//! Panic containment: catching one, and surviving a lock whose holder panicked.
//!
//! The workspace deliberately does not build with `panic = "abort"`, and that
//! choice only means something if a panic is actually caught. Without the two
//! functions here, a bug in one tool took the whole engine with it in three
//! separate ways:
//!
//! 1. the unwind skipped the code that clears the session's busy flag, so every
//!    later prompt was answered "this session is already running a turn" — and a
//!    `session.resume` hands back the very same in-memory session, so nothing
//!    short of restarting the host recovered it;
//! 2. `.lock().unwrap()` on a mutex whose holder panicked panics again, and the
//!    engine locks its database on the command dispatch path — one panic and
//!    every subsequent command died the same way;
//! 3. the frontend was never told. A turn simply stopped producing events.
//!
//! A panicking tool is a bug in this engine either way. The contract is only
//! that it costs one tool call, reported as a failed one, instead of the
//! session.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Condvar, Mutex, MutexGuard, WaitTimeoutResult};
use std::time::Duration;

use crate::error::{Error, Result};

/// How much of a panic message is carried into the error.
///
/// Enough to name the file, line and assertion; short enough that a panic
/// carrying a whole file's contents does not become the tool result.
const MAX_MESSAGE: usize = 300;

thread_local! {
    /// How many [`catch`] calls this thread is inside of.
    static CONTAINED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether a panic raised on this thread right now is going to be caught by
/// [`catch`] — and so costs one failed call, not the process. A panic hook asks
/// this before it does anything that only a dying process should do, such as
/// giving the terminal back.
pub fn contained() -> bool {
    CONTAINED.with(|depth| depth.get() > 0)
}

/// Run `f`, turning a panic into an ordinary [`Error::Internal`].
///
/// `what` names the thing that panicked — the tool, or the turn — because
/// "internal error" with no subject is a report nobody can act on. The default
/// panic hook has already written the message and any backtrace to stderr,
/// which is where they belong: stdout carries the protocol.
pub fn catch<T>(what: &str, f: impl FnOnce() -> T) -> Result<T> {
    CONTAINED.with(|depth| depth.set(depth.get() + 1));
    let caught = catch_unwind(AssertUnwindSafe(f));
    CONTAINED.with(|depth| depth.set(depth.get() - 1));
    match caught {
        Ok(value) => Ok(value),
        Err(payload) => Err(Error::Internal(format!(
            "{what} panicked: {}",
            describe(&payload)
        ))),
    }
}

/// Lock a mutex, taking it back from a thread that panicked while holding it.
///
/// Poisoning is sticky and these locks sit on the dispatch path, so `.unwrap()`
/// turns one panic into a dead engine. What is behind them — a database handle,
/// a mode, the pty of the command running now — is not left half-written by an
/// unwind in a way a second thread would notice, and a session that carries on
/// is strictly better than a host that answers nothing.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// [`Condvar::wait_timeout`] with the same treatment, for the same reason.
pub fn wait_timeout<'a, T>(
    signal: &Condvar,
    guard: MutexGuard<'a, T>,
    dur: Duration,
) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
    match signal.wait_timeout(guard, dur) {
        Ok(pair) => pair,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The panic payload as a single readable line.
///
/// `panic!("…")` and `assert!` produce a `String` or a `&str`; anything thrown
/// by `panic_any` may be neither, and there is nothing honest to print for it.
fn describe(payload: &Box<dyn std::any::Any + Send>) -> String {
    let text = if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else {
        "no message"
    };
    let line = text.lines().next().unwrap_or("no message");
    crate::tools::cap(line.to_string(), MAX_MESSAGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// What a panic hook asks: is this one going to be caught? True inside
    /// `catch` (nested too), false outside it, and false again after a panic
    /// that was caught.
    #[test]
    fn a_hook_can_tell_a_contained_panic_from_one_that_will_end_the_process() {
        assert!(!contained());
        catch("outer", || {
            assert!(contained());
            let _ = catch("inner", || assert!(contained()));
            assert!(contained(), "leaving the inner one leaves the outer one");
        })
        .unwrap();
        assert!(!contained());
        let _ = catch("failing", || {
            assert!(contained());
            panic!("on purpose");
        });
        assert!(
            !contained(),
            "a caught panic must not leave the thread marked"
        );
    }

    #[test]
    fn a_value_passes_straight_through() {
        assert_eq!(catch("thing", || 7).unwrap(), 7);
    }

    #[test]
    fn a_panic_becomes_an_error_that_names_what_panicked() {
        let err = catch("create_file", || panic!("index out of bounds")).unwrap_err();
        assert_eq!(err.code(), "internal");
        let text = err.to_string();
        assert!(text.contains("create_file panicked"), "{text}");
        assert!(text.contains("index out of bounds"), "{text}");
    }

    #[test]
    fn a_panic_with_no_string_payload_still_reports() {
        let err = catch("tool", || std::panic::panic_any(42u8)).unwrap_err();
        assert!(err.to_string().contains("no message"), "{err}");
    }

    #[test]
    fn only_the_first_line_and_only_so_much_of_it_is_carried() {
        let err = catch("tool", || {
            panic!("{}", format!("{}\nsecond line", "x".repeat(1000)))
        });
        let text = err.unwrap_err().to_string();
        assert!(!text.contains("second line"), "{text}");
        assert!(
            text.contains("only the first") || text.contains("<system_notice>"),
            "the cut says so: {text}"
        );
        assert!(
            text.chars().count() < MAX_MESSAGE + 150,
            "{}",
            text.chars().count()
        );
    }

    #[test]
    fn a_mutex_poisoned_by_a_panic_can_still_be_locked() {
        let m = Arc::new(Mutex::new(5));
        let clone = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("die while holding it");
        })
        .join();

        assert!(m.lock().is_err(), "the mutex really is poisoned");
        assert_eq!(*lock(&m), 5, "and we take it back anyway");
    }

    #[test]
    fn a_condvar_poisoned_by_a_panic_can_still_be_waited_on() {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let clone = Arc::clone(&pair);
        let _ = std::thread::spawn(move || {
            let _guard = clone.0.lock().unwrap();
            panic!("die while holding it");
        })
        .join();

        let (guard, timed) = wait_timeout(&pair.1, lock(&pair.0), Duration::from_millis(20));
        assert!(timed.timed_out());
        assert!(!*guard);
    }

    #[test]
    fn the_caller_of_catch_keeps_running_after_a_panic() {
        let reached = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reached);
        let _ = catch("tool", move || panic!("boom"));
        flag.store(true, Ordering::Relaxed);
        assert!(reached.load(Ordering::Relaxed));
    }
}
