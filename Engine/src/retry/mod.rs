//! When a failed call may be repeated, and with which key.
//!
//! # One loop, around the whole attempt
//!
//! This is the only retry loop in the engine (audit RT4: the stream used to
//! have a second one of its own, with its own curve and its own rules). An
//! attempt is the whole call — opening it *and* reading its stream — so a
//! provider that drops a stream, injects an error frame into it, or answers
//! with nothing is retried exactly like one that refused to connect: up to
//! `retry.max_attempts` times, on the stepped curve of [`backoff`], every error
//! except the user's own cancel (the owner's contract, 2026-09-11; the curve's
//! shape, 2026-10-06).
//!
//! Repeating after part of an answer streamed costs those tokens twice, and the
//! owner chose that over a transcript that disagrees with the screen: the caller
//! tells its frontend to drop the partial output before the next attempt. It
//! never repeats a *tool*: tools run only after a stream has completed, outside
//! this loop.
//!
//! What is not repeated is what cannot change by repeating: a cut-off answer
//! (`Incomplete`), a request longer than the context (`ContextOverflow` — the
//! turn compacts instead), and Rune's own refusals of a request it could not
//! even build (`Invalid`, `NotFound`).
//!
//! # The server's own `Retry-After` is not read
//!
//! Rune's curve alone decides every wait (owner, 2026-10-08). It used to win
//! whenever a server asked for longer, and a gateway answering 503 with
//! `Retry-After: 1800` then froze a turn for thirty minutes on its first retry
//! — twice in one day, ended only by switching the model by hand. A key a
//! server refused rests on the same curve.
//!
//! # Why the ring lives in `settings::secrets` and the policy lives here
//!
//! Rotating a key is a fact about credentials — a web-search key runs out
//! exactly the way a model key does — so the ring belongs next to the file that
//! stores them. Deciding *that* a failure was the key's fault needs HTTP status
//! codes, which belong to [`crate::net`]. This module is the one place both are
//! in scope, and it is deliberately the only place either of them is interpreted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::panic;
use crate::settings::Settings;

/// What the caller is told before a wait, so a retry is never a silent hang.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub attempt: u32,
    pub of: u32,
    pub wait: Duration,
    /// Whether the next attempt uses a different key.
    pub rotated: bool,
    /// Human-readable reason for the retry (e.g. rate limit, bad request, connection failure).
    pub reason: String,
}

/// What a failure means for the next attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The same key may still work.
    Wait,
    /// This key is the problem: rest it, move it to the back, use the next.
    Rotate { dead: bool, rest: Duration },
    /// Repeating cannot help.
    Fatal,
}

/// The caller's answer before (and during) a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Go {
    On,
    /// The user stopped the turn.
    Cancel,
    /// The request's own deadline passed: report the failure that caused the
    /// wait, never a cancellation the user did not ask for (audit RT6).
    GiveUp,
}

/// Run `attempt` until it succeeds, a failure is final, or the attempts run out.
///
/// `secret` names the ring in `secrets.json` — `None` for an endpoint that needs
/// no credential, such as a local Ollama. `announce` is told before and during
/// every wait. `attempt` reads the active key itself, so each one picks up
/// whatever rotation left at the front of the ring.
pub fn run<T>(
    settings: &Arc<Mutex<Settings>>,
    secret: Option<&str>,
    announce: &mut dyn FnMut(Waiting) -> Go,
    attempt: &mut dyn FnMut() -> Result<T>,
) -> Result<T> {
    let config = panic::lock(settings).config.clone();
    let max_retries = config.number("retry.max_attempts") as u32;
    let mut retries = 0u32;
    loop {
        let used =
            secret.and_then(|name| panic::lock(settings).secrets.get(name).map(String::from));
        let error = match attempt() {
            Ok(value) => return Ok(value),
            Err(e) => e,
        };
        retries += 1;
        if retries > max_retries {
            return Err(error);
        }

        let reason = reason_for(&error);
        let backoff = backoff(
            retries,
            config.duration("retry.initial_delay"),
            config.duration("retry.max_delay"),
        );
        let (wait, rotated) = match fault(&error, backoff, config.duration("retry.credit_cooldown"))
        {
            Fault::Fatal => return Err(error),
            Fault::Wait => (backoff, false),
            Fault::Rotate { dead, rest } => {
                if let (Some(name), Some(key)) = (secret, used.as_deref()) {
                    let mut held = panic::lock(settings);
                    let (total, _) = held.secrets.health(name);
                    if total < 2 {
                        // Nothing to rotate to: wait on the curve.
                        drop(held);
                        (backoff, false)
                    } else {
                        held.secrets.demote(name, key, rest, dead);
                        drop(held);
                        // The next key is fresh, so there is nothing to wait out.
                        (Duration::ZERO, true)
                    }
                } else {
                    (backoff, false)
                }
            }
        };

        let mut tell = |wait: Duration, rotated: bool| -> Option<Error> {
            match announce(Waiting {
                attempt: retries,
                of: max_retries,
                wait,
                rotated,
                reason: reason.clone(),
            }) {
                Go::On => None,
                Go::Cancel => Some(Error::Cancelled),
                Go::GiveUp => Some(Error::Provider(format!(
                    "the request deadline passed while waiting to retry: {error}"
                ))),
            }
        };
        if let Some(stop) = tell(wait, rotated) {
            return Err(stop);
        }
        if !wait.is_zero() {
            let started = std::time::Instant::now();
            while started.elapsed() < wait {
                std::thread::sleep(
                    (wait - started.elapsed().min(wait)).min(Duration::from_millis(100)),
                );
                if let Some(stop) = tell(wait.saturating_sub(started.elapsed()), false) {
                    return Err(stop);
                }
            }
        }
    }
}

/// Describe the specific failure condition for user status display across all error types.
pub fn reason_for(e: &Error) -> String {
    match e {
        Error::Rejected { status: 429, .. } => "Rate limited (HTTP 429)".into(),
        Error::Rejected {
            status: 400,
            message,
            ..
        } => {
            let lower = message.to_lowercase();
            if lower.contains("model") {
                "Invalid model ID (HTTP 400)".into()
            } else {
                "Provider rejected (HTTP 400)".into()
            }
        }
        Error::Rejected { status: 401, .. } => "Unauthorized (HTTP 401)".into(),
        Error::Rejected { status: 403, .. } => "Forbidden (HTTP 403)".into(),
        Error::Rejected { status: 402, .. } => "Payment required (HTTP 402)".into(),
        Error::Rejected { status: 404, .. } => "Endpoint not found (HTTP 404)".into(),
        Error::Rejected { status: 408, .. } => "Request timeout (HTTP 408)".into(),
        Error::Rejected { status: 422, .. } => "Unprocessable entity (HTTP 422)".into(),
        Error::Rejected { status, .. } if *status >= 500 => format!("Server error (HTTP {status})"),
        Error::Rejected { status, .. } => format!("Provider error (HTTP {status})"),
        Error::Unreachable(_) => "Connection failed".into(),
        Error::Stalled(silence) => {
            format!(
                "Stream stalled ({}s without data)",
                silence.as_secs().max(1)
            )
        }
        Error::Provider(msg) => {
            if msg.contains("closed prematurely") || msg.contains("ended badly") {
                "Abnormal end stream".into()
            } else {
                "Provider error".into()
            }
        }
        _ => "Provider error".into(),
    }
}

/// Read the failure. Every provider and transport error (including 400, 401,
/// 403, 404, 429, 500, a dropped or empty stream) is retried; see the module
/// docs for the few that repeating cannot change. Out of credit does not clear
/// in seconds: that key rests for `credit` (`retry.credit_cooldown`) rather
/// than walking back into the same refusal on the next attempt.
fn fault(e: &Error, backoff: Duration, credit: Duration) -> Fault {
    match e {
        Error::Cancelled
        | Error::Incomplete(_)
        | Error::ContextOverflow(_)
        | Error::Invalid(_)
        | Error::NotFound(_) => Fault::Fatal,
        Error::Rejected { status, .. } => match status {
            429 | 401 | 403 => Fault::Rotate {
                dead: false,
                rest: backoff,
            },
            402 => Fault::Rotate {
                dead: false,
                rest: credit,
            },
            _ => Fault::Wait,
        },
        _ => Fault::Wait,
    }
}

/// The wait before retry `attempt` (1-based): `step × attempt`, and a wait
/// that would pass `cap` starts the series again from `step` (owner,
/// 2026-10-06). With the defaults (3 s, 1 m): 3, 6, …, 57, 60, then 3, 6, …
/// again — so five retries wait 3/6/9/12/15 s, and any number of retries never
/// waits longer than the cap. Both values come from the configuration as it
/// is now; the loader refuses a zero step and a step above the cap.
fn backoff(attempt: u32, step: Duration, cap: Duration) -> Duration {
    let (step, cap) = (step.as_nanos(), cap.as_nanos());
    if step == 0 {
        return Duration::ZERO;
    }
    let per_series = (cap / step).max(1);
    let k = u128::from(attempt.max(1) - 1) % per_series + 1;
    Duration::from_nanos(u64::try_from(step * k).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backoff value handed to [`fault`]; any value works, it is only echoed.
    const STEP: Duration = Duration::from_secs(3);
    /// A credit cooldown handed to [`fault`].
    const CREDIT: Duration = Duration::from_secs(900);

    /// The retries the shipped configuration allows.
    fn attempts() -> u32 {
        crate::settings::configuration::Configuration::default().number("retry.max_attempts") as u32
    }

    /// A home whose curve is milliseconds, so a test that lets the loop wait
    /// spends no real time on it; the policy under test is the same.
    fn home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-retry-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rune.toml"),
            "[retry]\ninitial_delay = \"1ms\"\nmax_delay = \"3ms\"\n",
        )
        .unwrap();
        dir
    }

    fn shared_in(dir: &std::path::Path, secrets: &str) -> Arc<Mutex<Settings>> {
        crate::settings::secrets::write_store(dir, secrets);
        Arc::new(Mutex::new(
            Settings::load(dir, &dir.join("rune.db")).unwrap(),
        ))
    }

    fn shared(tag: &str, secrets: &str) -> Arc<Mutex<Settings>> {
        shared_in(&home(tag), secrets)
    }

    /// What the whole feature is for: the first key is limited, and the call
    /// succeeds on the second without the user doing anything.
    #[test]
    fn a_limited_key_is_demoted_and_the_next_one_answers() {
        let s = shared("rotate", r#"{"p":["k1","k2"]}"#);
        let mut seen: Vec<String> = Vec::new();
        let mut announced = 0;
        let out = run(
            &s,
            Some("p"),
            &mut |w| {
                announced += 1;
                assert!(
                    w.rotated,
                    "a rate limit must move to the next key, not sleep on this one"
                );
                assert!(w.wait.is_zero(), "a fresh key has nothing to wait out");
                Go::On
            },
            &mut || {
                let key = panic::lock(&s).secrets.get("p").unwrap().to_string();
                seen.push(key.clone());
                if key == "k1" {
                    Err(rejected(429))
                } else {
                    Ok(key)
                }
            },
        )
        .unwrap();

        assert_eq!(out, "k2");
        assert_eq!(seen, vec!["k1", "k2"]);
        assert_eq!(announced, 1);
        assert_eq!(panic::lock(&s).secrets.ring("p"), vec!["k2", "k1"]);
    }

    /// Writing the new order down is an optimisation for the *next* launch —
    /// the rotation the caller asked for has already happened, in memory. A home
    /// that cannot be written must not turn a rate limit the ring just recovered
    /// from into a hard failure.
    #[test]
    fn a_rotation_answers_even_when_the_new_order_cannot_be_written_down() {
        let dir = home("readonly");
        let s = shared_in(&dir, r#"{"p":["k1","k2"]}"#);
        let read_only = crate::platform::fixture::Restricted::new(&dir, 0o555).unwrap();
        let out = run(&s, Some("p"), &mut |_| Go::On, &mut || {
            let key = panic::lock(&s).secrets.get("p").unwrap().to_string();
            if key == "k1" {
                Err(rejected(429))
            } else {
                Ok(key)
            }
        });

        // Given back before asserting (and by the guard if `run` panics), or a
        // failure leaves an undeletable directory.
        drop(read_only);
        assert_eq!(out.unwrap(), "k2", "the second key was never reached");
        assert_eq!(
            panic::lock(&s).secrets.ring("p"),
            vec!["k2", "k1"],
            "the ring did not rotate"
        );
    }

    #[test]
    fn a_request_that_fails_with_400_is_retried_up_to_max_attempts() {
        let s = shared("retry_400", r#"{"p":["k1","k2"]}"#);
        let mut tries = 0;
        let e = run(&s, Some("p"), &mut |_| Go::On, &mut || {
            tries += 1;
            Err::<(), _>(rejected(400))
        })
        .unwrap_err();
        assert_eq!(tries, (attempts() + 1) as usize);
        assert!(e.to_string().contains("400"));
    }

    #[test]
    fn attempts_are_bounded_and_the_last_failure_is_the_one_reported() {
        let s = shared("bounded", r#"{"p":["k1","k2","k3","k4"]}"#);
        let mut tries = 0;
        let e = run(&s, Some("p"), &mut |_| Go::On, &mut || {
            tries += 1;
            Err::<(), _>(rejected(429))
        })
        .unwrap_err();
        assert_eq!(tries, (attempts() + 1) as usize);
        assert!(e.to_string().contains("429"));
    }

    #[test]
    fn saying_no_to_a_wait_ends_it_as_a_cancellation_not_a_provider_failure() {
        let s = shared("cancel", r#"{"p":["k1","k2"]}"#);
        let e = run(&s, Some("p"), &mut |_| Go::Cancel, &mut || {
            Err::<(), _>(rejected(429))
        })
        .unwrap_err();
        assert_eq!(e.code(), "cancelled");
    }

    /// A keyless endpoint — a local Ollama — has no ring to rotate, and a
    /// transport hiccup must still be retried.
    #[test]
    fn a_keyless_endpoint_retries_without_a_ring() {
        let s = shared("keyless", "{}");
        let mut tries = 0;
        let out = run(
            &s,
            None,
            &mut |w| {
                assert!(!w.rotated);
                Go::On
            },
            &mut || {
                tries += 1;
                if tries < 2 {
                    Err(Error::Unreachable("dns".into()))
                } else {
                    Ok(tries)
                }
            },
        )
        .unwrap();
        assert_eq!(out, 2);
    }

    /// A server that went silent is repeated like any other failure, and the
    /// wait says that it was the silence — the footer is where a person learns
    /// why the spinner stopped answering.
    #[test]
    fn a_stalled_stream_is_repeated_and_the_wait_says_why() {
        let s = shared("stalled", "{}");
        let mut reasons = Vec::new();
        let mut tries = 0;
        let out = run(
            &s,
            None,
            &mut |w| {
                reasons.push(w.reason.clone());
                Go::On
            },
            &mut || {
                tries += 1;
                if tries < 2 {
                    Err(Error::Stalled(Duration::from_secs(300)))
                } else {
                    Ok(tries)
                }
            },
        )
        .unwrap();
        assert_eq!(out, 2);
        assert!(
            reasons
                .iter()
                .all(|reason| reason == "Stream stalled (300s without data)"),
            "{reasons:?}"
        );
        assert_eq!(
            fault(&Error::Stalled(Duration::from_secs(1)), STEP, CREDIT),
            Fault::Wait
        );
    }

    fn rejected(status: u16) -> Error {
        Error::Rejected {
            status,
            message: "no".into(),
        }
    }

    #[test]
    fn all_rejected_statuses_are_retried_and_only_what_cannot_change_is_fatal() {
        for status in [400, 404, 422, 500, 502, 503] {
            assert_eq!(fault(&rejected(status), STEP, CREDIT), Fault::Wait);
        }
        // A dropped stream, an injected error frame, an empty answer.
        assert_eq!(
            fault(
                &Error::Provider("the stream ended badly".into()),
                STEP,
                CREDIT
            ),
            Fault::Wait
        );
        for fatal in [
            Error::Cancelled,
            Error::Incomplete("length".into()),
            Error::ContextOverflow("too long".into()),
            Error::invalid("no model chosen"),
            Error::NotFound("provider".into()),
        ] {
            assert_eq!(fault(&fatal, STEP, CREDIT), Fault::Fatal, "{fatal}");
        }
    }

    /// Audit RT6: a deadline that passes during a wait ends with the failure
    /// that caused it, not with a cancellation the user never asked for.
    #[test]
    fn giving_up_reports_the_failure_not_a_cancellation() {
        let s = shared("giveup", "{}");
        let e = run(&s, None, &mut |_| Go::GiveUp, &mut || {
            Err::<(), _>(Error::Unreachable("offline".into()))
        })
        .unwrap_err();
        assert_ne!(e.code(), "cancelled");
        assert!(e.to_string().contains("offline"), "{e}");
    }

    /// What froze a turn for thirty minutes (session 546, 2026-10-08): a real
    /// server answering 503 with `Retry-After: 1800`, through the real client.
    /// The wait is the curve's first step, whatever the server asked for.
    #[test]
    fn a_servers_retry_after_is_not_what_is_waited_out() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 8192];
            let _ = stream.read(&mut bytes);
            let _ = stream.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1800\r\nContent-Type: application/json\r\nContent-Length: 34\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"overloaded\"}}",
            );
        });
        let s = shared("retry_after", "{}");
        let config = panic::lock(&s).config.clone();
        let http = crate::net::Http::configured(&config, None, Default::default());
        let mut announced = None;
        let e = run(
            &s,
            None,
            &mut |w| {
                // The first word on the wait is what the retry would sleep.
                announced.get_or_insert(w.wait);
                Go::Cancel
            },
            &mut || {
                let response =
                    http.post_json(&format!("http://{address}/"), &[], &serde_json::json!({}))?;
                Err::<(), _>(crate::net::rejected(response.status, &response.text))
            },
        )
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(e.code(), "cancelled");
        // The test home's curve: a 1 ms step, whatever the server asked for.
        assert_eq!(announced, Some(Duration::from_millis(1)));
    }

    #[test]
    fn a_call_that_never_landed_is_free_to_repeat() {
        assert_eq!(
            fault(&Error::Unreachable("dns".into()), STEP, CREDIT),
            Fault::Wait
        );
        for status in [500, 502, 503, 504, 408] {
            assert_eq!(fault(&rejected(status), STEP, CREDIT), Fault::Wait);
        }
    }

    #[test]
    fn the_statuses_that_are_the_keys_fault_rotate_and_retry() {
        assert_eq!(
            fault(&rejected(429), STEP, CREDIT),
            Fault::Rotate {
                dead: false,
                rest: STEP
            }
        );
        assert_eq!(
            fault(&rejected(401), STEP, CREDIT),
            Fault::Rotate {
                dead: false,
                rest: STEP
            }
        );
        assert_eq!(
            fault(&rejected(403), STEP, CREDIT),
            Fault::Rotate {
                dead: false,
                rest: STEP
            }
        );
        assert_eq!(
            fault(&rejected(402), STEP, CREDIT),
            Fault::Rotate {
                dead: false,
                rest: CREDIT
            }
        );
    }

    fn secs(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|s| Duration::from_secs(*s)).collect()
    }

    fn curve(retries: u32, step: u64, cap: u64) -> Vec<Duration> {
        (1..=retries)
            .map(|k| backoff(k, Duration::from_secs(step), Duration::from_secs(cap)))
            .collect()
    }

    /// The owner's curve (2026-10-06): N retries wait step × k, and a wait that
    /// would pass the cap starts the series again from the step.
    #[test]
    fn the_curve_steps_up_to_the_cap_and_then_starts_again() {
        assert_eq!(curve(5, 3, 60), secs(&[3, 6, 9, 12, 15]));
        assert_eq!(curve(4, 3, 60), secs(&[3, 6, 9, 12]));
        let mut long: Vec<u64> = (1..=20).map(|k| 3 * k).collect();
        long.extend([3, 6, 9, 12, 15]);
        assert_eq!(
            curve(25, 3, 60),
            secs(&long),
            "60 s is reached, 63 s is not"
        );
        // A step that does not divide the cap stops below it.
        assert_eq!(
            curve(10, 7, 60),
            secs(&[7, 14, 21, 28, 35, 42, 49, 56, 7, 14])
        );
        // A step equal to the cap waits the cap every time.
        assert_eq!(curve(3, 60, 60), secs(&[60, 60, 60]));
        assert!(
            (1..=10_000)
                .all(|k| backoff(k, STEP, Duration::from_secs(60)) <= Duration::from_secs(60)),
            "no retry ever waits longer than the cap"
        );
    }

    /// The shipped defaults are the owner's: five retries on 3/6/9/12/15 s and a
    /// one-minute cap — there is no floor or ceiling of the code's own.
    #[test]
    fn the_defaults_are_the_owners_curve_and_nothing_overrides_the_configuration() {
        let config = crate::settings::configuration::Configuration::default();
        assert_eq!(
            config.duration("retry.initial_delay"),
            Duration::from_secs(3)
        );
        assert_eq!(config.duration("retry.max_delay"), Duration::from_secs(60));
        assert_eq!(attempts(), 5);
        // A configured step shorter than the old 3 s floor is used as written.
        assert_eq!(
            backoff(2, Duration::from_millis(500), Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }

    /// The loop reads the curve from the configuration it is given, live, and
    /// announces every wait it then actually takes.
    #[test]
    fn the_loop_waits_the_configured_curve() {
        let s = shared("curve", "{}");
        let mut waits = Vec::new();
        let _ = run(
            &s,
            None,
            &mut |w| {
                if w.attempt as usize > waits.len() {
                    waits.push(w.wait);
                }
                Go::On
            },
            &mut || Err::<(), _>(Error::Unreachable("offline".into())),
        );
        let ms = |v: &[u64]| {
            v.iter()
                .map(|m| Duration::from_millis(*m))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            waits,
            ms(&[1, 2, 3, 1, 2]),
            "1 ms step, 3 ms cap, five retries"
        );
    }

    /// Out of credit rests the key for `retry.credit_cooldown`, read from the
    /// configuration rather than from a constant of this module.
    #[test]
    fn out_of_credit_rests_the_key_for_the_configured_cooldown() {
        assert_eq!(
            fault(&rejected(402), STEP, Duration::from_secs(120)),
            Fault::Rotate {
                dead: false,
                rest: Duration::from_secs(120)
            }
        );
    }
}
