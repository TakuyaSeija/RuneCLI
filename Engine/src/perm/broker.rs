//! Typed waiting room shared by permissions and clarification forms.
//!
//! A turn runs on its own thread. When it needs consent it parks here until the
//! frontend answers, the user cancels, or the wait times out. The frontend never
//! blocks: it receives a request event and replies whenever it likes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::panic;
use crate::proto::Decision;

/// An unanswered question waits this long before it is treated as a refusal.
/// Long enough for someone to come back from a coffee; short enough that a
/// forgotten prompt does not hold a thread forever.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, PartialEq, Eq)]
pub enum Answer<T = Decision> {
    Decided(T),
    /// The turn was cancelled while waiting.
    Cancelled,
    /// Nobody answered in time. Treated as a refusal by the caller.
    TimedOut,
}

pub trait Response {
    type Request;
    fn valid(&self, _request: &Self::Request) -> bool {
        true
    }
}

impl Response for Decision {
    type Request = ();
}

struct Inner<T: Response> {
    answers: HashMap<String, T>,
    /// Requests that have been asked and not yet answered.
    open: HashMap<String, T::Request>,
}

pub struct Broker<T: Response = Decision> {
    inner: Mutex<Inner<T>>,
    signal: Condvar,
    counter: AtomicU64,
}

impl<T: Response> Default for Broker<T> {
    fn default() -> Broker<T> {
        Broker::new()
    }
}

impl<T: Response> Broker<T> {
    pub fn new() -> Broker<T> {
        Broker {
            inner: Mutex::new(Inner {
                answers: HashMap::new(),
                open: HashMap::new(),
            }),
            signal: Condvar::new(),
            counter: AtomicU64::new(1),
        }
    }

    /// Register a question and get its id. The id is what the frontend echoes
    /// back in the corresponding reply command.
    pub fn open(&self, request: T::Request) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("p{n}");
        panic::lock(&self.inner).open.insert(id.clone(), request);
        id
    }

    /// Whether a question is waiting for its answer now.
    pub fn asking(&self) -> bool {
        !panic::lock(&self.inner).open.is_empty()
    }

    /// Block until this question is answered, the turn is cancelled, or the
    /// timeout expires (if a positive timeout is provided).
    pub fn wait(&self, req: &str, cancel: &Arc<AtomicBool>, timeout: Duration) -> Answer<T> {
        let deadline = if timeout.is_zero() {
            None
        } else {
            Some(Instant::now() + timeout)
        };
        let mut guard = panic::lock(&self.inner);
        loop {
            if let Some(d) = guard.answers.remove(req) {
                guard.open.remove(req);
                return Answer::Decided(d);
            }
            if cancel.load(Ordering::Relaxed) || !guard.open.contains_key(req) {
                guard.open.remove(req);
                return Answer::Cancelled;
            }
            let now = Instant::now();
            if let Some(dl) = deadline {
                if now >= dl {
                    guard.open.remove(req);
                    return Answer::TimedOut;
                }
            }
            // Wake regularly even without a reply, so cancellation is noticed
            // promptly rather than at the timeout.
            let slice = match deadline {
                Some(dl) => std::cmp::min(
                    dl.saturating_duration_since(now),
                    Duration::from_millis(100),
                ),
                None => Duration::from_millis(100),
            };
            let (g, _) = panic::wait_timeout(&self.signal, guard, slice);
            guard = g;
        }
    }

    /// Accept a valid answer once. Invalid, duplicate and abandoned replies
    /// leave the request unchanged and return false.
    pub fn reply(&self, req: &str, decision: T) -> bool {
        let mut guard = panic::lock(&self.inner);
        if guard.answers.contains_key(req)
            || !guard
                .open
                .get(req)
                .is_some_and(|request| decision.valid(request))
        {
            return false;
        }
        guard.answers.insert(req.to_string(), decision);
        drop(guard);
        self.signal.notify_all();
        true
    }

    /// Wake waiters to recheck their own cancellation flags without abandoning
    /// unrelated sessions' requests in this shared broker.
    pub fn wake(&self) {
        let guard = panic::lock(&self.inner);
        self.signal.notify_all();
        drop(guard);
    }

    /// Abandon every open question — used when the entire engine is torn down.
    pub fn clear(&self) {
        let mut guard = panic::lock(&self.inner);
        guard.open.clear();
        guard.answers.clear();
        drop(guard);
        self.signal.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn a_reply_unblocks_the_waiter() {
        let b = Arc::new(Broker::<Decision>::new());
        let id = b.open(());
        let b2 = Arc::clone(&b);
        let id2 = id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            assert!(b2.reply(&id2, Decision::AllowOnce));
        });
        assert_eq!(
            b.wait(&id, &flag(), Duration::from_secs(2)),
            Answer::Decided(Decision::AllowOnce)
        );
    }

    #[test]
    fn cancellation_beats_waiting() {
        let b = Arc::new(Broker::<Decision>::new());
        let id = b.open(());
        let cancel = flag();
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            c2.store(true, Ordering::Relaxed);
        });
        assert_eq!(
            b.wait(&id, &cancel, Duration::from_secs(2)),
            Answer::Cancelled
        );
    }

    #[test]
    fn unanswered_questions_time_out() {
        let b = Broker::<Decision>::new();
        let id = b.open(());
        let start = Instant::now();
        assert_eq!(
            b.wait(&id, &flag(), Duration::from_millis(150)),
            Answer::TimedOut
        );
        assert!(start.elapsed() >= Duration::from_millis(140));
    }

    #[test]
    fn ids_are_unique_and_replies_are_matched_to_them() {
        let b = Broker::<Decision>::new();
        let a = b.open(());
        let c = b.open(());
        assert_ne!(a, c);
        assert!(b.reply(&c, Decision::Deny));
        assert_eq!(
            b.wait(&c, &flag(), Duration::from_millis(200)),
            Answer::Decided(Decision::Deny)
        );
        // `a` is still open and unanswered.
        assert_eq!(
            b.wait(&a, &flag(), Duration::from_millis(50)),
            Answer::TimedOut
        );
    }

    #[test]
    fn a_reply_to_an_unknown_question_is_refused_not_stored() {
        let b = Broker::<Decision>::new();
        assert!(!b.reply("p999", Decision::AllowAlways));
    }

    #[test]
    fn waking_cancelled_session_preserves_another_sessions_question() {
        let broker = Broker::<Decision>::new();
        let first = broker.open(());
        let second = broker.open(());
        let cancel = flag();
        cancel.store(true, Ordering::Relaxed);
        broker.wake();
        assert_eq!(
            broker.wait(&first, &cancel, Duration::ZERO),
            Answer::Cancelled
        );
        assert!(broker.reply(&second, Decision::AllowOnce));
        assert_eq!(
            broker.wait(&second, &flag(), Duration::ZERO),
            Answer::Decided(Decision::AllowOnce)
        );
    }

    #[test]
    fn clear_abandons_open_questions() {
        let b = Broker::<Decision>::new();
        let id = b.open(());
        b.clear();
        assert!(!b.reply(&id, Decision::AllowOnce));
    }
}
