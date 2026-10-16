//! Poll the underlying transport between idle reads without abandoning workers.
//! TLS remains ureq/rustls-owned; only its underlying read wait is interrupted.
//!
//! Two things end a wait here besides data: the person's Stop, and a server
//! that has gone silent. A connection that stays open and sends nothing never
//! fails by itself — before this limit a turn waited on one, spinner running,
//! until somebody pressed Stop (measured in session 546: 25 s, 8 min and 14 min
//! with no byte and no retry). So no byte for `network.stream_idle_timeout` is
//! a failure of its own, [`Stalled`], which the retry policy repeats like any
//! other.
use crate::provider::RequestControl;
use std::time::{Duration, Instant};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};
pub struct Interrupt {
    pub control: RequestControl,
    /// The live configuration, never a copy: the stall limit
    /// (`network.stream_idle_timeout`) is read at every wait, so a change made
    /// in Settings reaches a request that is already waiting.
    pub config: crate::settings::configuration::Shared,
}
impl std::fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interrupt").finish_non_exhaustive()
    }
}
pub struct Controlled<T> {
    inner: T,
    control: RequestControl,
    config: crate::settings::configuration::Shared,
}
impl<T: std::fmt::Debug> std::fmt::Debug for Controlled<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controlled")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

/// The server sent nothing for this long, and the wait was given up.
///
/// Carried inside the `io::Error` the transport fails with, so whoever reads
/// the failure — the request, or the body reader of a stream half-way through
/// — can tell a silent server from a broken connection ([`super::stalled`]).
#[derive(Debug)]
pub struct Stalled(pub Duration);

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server sent nothing for {}s",
            self.0.as_secs().max(1)
        )
    }
}

impl std::error::Error for Stalled {}

impl<T: Transport> Connector<T> for Interrupt {
    type Out = Controlled<T>;
    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| Controlled {
            inner,
            control: self.control.clone(),
            config: self.config.clone(),
        }))
    }
}
impl<T: Transport> Transport for Controlled<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        if self.control.cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "request cancelled",
            )
            .into());
        }
        self.inner.transmit_output(amount, timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let started = Instant::now();
        // Whichever limit comes first ends the wait: the phase's own (a call's
        // deadline, when it has one) or the stall limit. The stall limit is
        // measured per wait, so it is the silence between two bytes — a long
        // answer that keeps arriving is never cut by it.
        let phase = *timeout.after;
        loop {
            if self.control.cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "request cancelled",
                )
                .into());
            }
            let waited = started.elapsed();
            if waited >= phase {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let stall = self.config.duration("network.stream_idle_timeout");
            if waited >= stall {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::TimedOut, Stalled(stall)).into(),
                );
            }
            let slice = (phase - waited)
                .min(stall - waited)
                .min(Duration::from_millis(100));
            match self.inner.await_input(NextTimeout {
                after: slice.into(),
                reason: timeout.reason,
            }) {
                Err(ureq::Error::Timeout(_)) => continue,
                result => return result,
            }
        }
    }
    fn is_open(&mut self) -> bool {
        !self.control.cancelled() && self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A configuration whose stall limit is short enough for a test to wait it out.
    fn quick(stall: &str) -> crate::settings::configuration::Shared {
        crate::settings::configuration::Shared::new(
            crate::settings::configuration::Configuration::from_values(
                serde_json::json!({"network": {"stream_idle_timeout": stall}}),
            )
            .unwrap(),
        )
    }

    #[test]
    fn cancellation_interrupts_an_idle_sse_read_without_a_detached_worker() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
            tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
        });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancel.clone();
        let worker = std::thread::spawn(move || {
            let config = crate::settings::configuration::Shared::default();
            let http = crate::net::Http::configured(
                &config,
                Some(Duration::from_secs(10)),
                RequestControl {
                    cancel: Some(flag),
                    ..Default::default()
                },
            );
            let mut events = http
                .post_sse(
                    &format!("http://{address}/"),
                    &[],
                    &serde_json::json!({}),
                    None,
                )
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e.to_string())
                })?;
            events.next_event()
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(worker.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = release_tx.send(());
        server.join().unwrap();
    }

    /// A server that takes the request and never answers it: the call ends as
    /// a stall of its own, at the configured limit, with no deadline and no
    /// Stop involved — the model request of a turn has neither.
    #[test]
    fn a_server_that_never_answers_ends_the_call_as_a_stall() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            // Say nothing until the client has given up.
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });
        let http = crate::net::Http::configured(&quick("300ms"), None, RequestControl::default());
        let started = Instant::now();
        let result = http.post_sse(
            &format!("http://{address}/"),
            &[],
            &serde_json::json!({}),
            None,
        );
        let took = started.elapsed();
        let _ = release_tx.send(());
        server.join().unwrap();
        match result {
            Err(crate::error::Error::Stalled(silence)) => {
                assert_eq!(silence, Duration::from_millis(300))
            }
            Err(other) => panic!("expected a stall, got {other}"),
            Ok(_) => panic!("expected a stall, got a stream"),
        }
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    /// The limit is read at every wait: lowering it in Settings reaches a
    /// request that is already waiting, without a relaunch and without the
    /// request having to be sent again.
    #[test]
    fn a_new_limit_reaches_a_request_that_is_already_waiting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
        });
        let config = quick("10m");
        let live = config.clone();
        let changer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            live.store(
                crate::settings::configuration::Configuration::from_values(
                    serde_json::json!({"network": {"stream_idle_timeout": "400ms"}}),
                )
                .unwrap(),
            );
        });
        let http = crate::net::Http::configured(&config, None, RequestControl::default());
        let started = Instant::now();
        let result = http.post_sse(
            &format!("http://{address}/"),
            &[],
            &serde_json::json!({}),
            None,
        );
        let took = started.elapsed();
        let _ = release_tx.send(());
        changer.join().unwrap();
        server.join().unwrap();
        assert!(
            matches!(result, Err(crate::error::Error::Stalled(_))),
            "the waiting request must give up under the new limit"
        );
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// A stream that starts and then goes quiet: the read that waits on the
    /// silence fails as a stall, readable as one through the `io::Error` the
    /// body reader returns — not as a broken connection.
    #[test]
    fn a_stream_that_goes_quiet_midway_fails_as_a_stall() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"a\":1}\n\n",
                )
                .unwrap();
            stream.flush().unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
        });
        let http = crate::net::Http::configured(&quick("300ms"), None, RequestControl::default());
        let mut events = http
            .post_sse(
                &format!("http://{address}/"),
                &[],
                &serde_json::json!({}),
                None,
            )
            .unwrap();
        assert!(events.next_event().unwrap().is_some(), "the first event");
        let silent = events.next_event().unwrap_err();
        let _ = release_tx.send(());
        server.join().unwrap();
        assert_eq!(
            crate::net::stalled(&silent),
            Some(Duration::from_millis(300)),
            "{silent}"
        );
    }

    /// The limit is the silence between two bytes, not the length of the
    /// answer: a stream that keeps arriving runs well past it.
    #[test]
    fn a_long_answer_that_keeps_arriving_is_never_cut_by_the_stall_limit() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            for i in 0..8 {
                std::thread::sleep(Duration::from_millis(150));
                let _ = stream.write_all(format!("data: {{\"n\":{i}}}\n\n").as_bytes());
                let _ = stream.flush();
            }
        });
        let http = crate::net::Http::configured(&quick("400ms"), None, RequestControl::default());
        let mut events = http
            .post_sse(
                &format!("http://{address}/"),
                &[],
                &serde_json::json!({}),
                None,
            )
            .unwrap();
        let started = Instant::now();
        let mut seen = 0;
        while let Some(_event) = events.next_event().unwrap() {
            seen += 1;
        }
        server.join().unwrap();
        assert_eq!(seen, 8);
        assert!(started.elapsed() > Duration::from_millis(1000));
    }
}
