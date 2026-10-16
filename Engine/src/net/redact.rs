//! Secret scrubbing, applied at the one place every error message passes.
//!
//! # Why this exists at all
//!
//! This engine is meant to be read. Its source is public, so nothing here may
//! depend on an attacker not knowing how it works — and the flip side of that
//! bargain is that the *user's* secrets must never travel anywhere a reader
//! could end up looking: an error event, a log line, a bug report, a terminal
//! recording pasted into an issue.
//!
//! An API key reaches a message by accident, never on purpose. A provider echoes
//! the request it rejected, a URL carries a token in a query string, an `io`
//! error quotes the header it failed to send. Catching those one at a time is a
//! game nobody wins, so every secret the engine loads is registered here and
//! [`crate::error::Error`]'s `Display` — the single funnel through which every
//! user-visible error text is built — scrubs it out.
//!
//! Registration is one-way and process-wide by design: there is no `forget`,
//! because a key that was live a moment ago is still a key.

use std::sync::Mutex;

/// Shorter than this and a "secret" is more likely to be a common word whose
/// redaction would mangle ordinary text than an actual credential.
const MIN_LEN: usize = 12;

pub const MASK: &str = "[redacted]";

static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Remember `secret` so it can never appear in an error message again.
///
/// Idempotent. Values too short to be credentials are ignored rather than
/// registered, so an empty or placeholder key cannot blank out unrelated text.
pub fn register(secret: &str) {
    let s = secret.trim();
    if s.len() < MIN_LEN {
        return;
    }
    let mut guard = lock();
    if !guard.iter().any(|k| k == s) {
        guard.push(s.to_string());
    }
}

/// Replace every registered secret in `text` with [`MASK`].
///
/// Returns the input untouched — and allocates nothing — in the overwhelmingly
/// common case where it holds no secret at all.
pub fn scrub(text: &str) -> std::borrow::Cow<'_, str> {
    let guard = lock();
    if guard.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out: Option<String> = None;
    for secret in guard.iter() {
        let current: &str = out.as_deref().unwrap_or(text);
        if current.contains(secret.as_str()) {
            out = Some(current.replace(secret.as_str(), MASK));
        }
    }
    match out {
        Some(s) => std::borrow::Cow::Owned(s),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// A poisoned lock here must not take the process down: the whole point of this
/// module is to still be working on the unhappy path.
fn lock() -> std::sync::MutexGuard<'static, Vec<String>> {
    SECRETS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_a_registered_key_anywhere_in_the_text() {
        register("sk-abcdefghijklmnop");
        let out = scrub("POST failed with api-key: sk-abcdefghijklmnop (401)");
        assert!(!out.contains("sk-abcdefghijklmnop"));
        assert!(out.contains(MASK));
    }

    #[test]
    fn leaves_ordinary_text_untouched_and_unallocated() {
        let text = "nothing secret here";
        assert!(matches!(scrub(text), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn refuses_to_register_something_too_short_to_be_a_key() {
        register("yes");
        assert!(
            !scrub("yes").contains(MASK),
            "a 3-letter word must not become a mask"
        );
    }

    #[test]
    fn registration_is_idempotent() {
        register("tp-0123456789abcdef");
        let before = lock().len();
        register("tp-0123456789abcdef");
        assert_eq!(lock().len(), before);
    }
}
