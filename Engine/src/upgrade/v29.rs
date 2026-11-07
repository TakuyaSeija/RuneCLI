//! Format 28 → 29: `retry.credit_cooldown` retires.
//!
//! A key no longer rests after a failure. Rotation is decided per call by one
//! owner: a key that has spent its own allowance (`retry.attempts_per_key`)
//! hands over to the next key in the ring, and the ring starts over once every
//! key has spent its own (owner, 2026-11-07).
//!
//! The key is removed rather than left unread: a configuration key that does
//! nothing is worse than no key at all, because a person tuning it would expect
//! an effect. A `[retry]` that is not a table is left exactly as it was written,
//! for the configuration's own check to name.
use super::Plan;
use crate::{Error, Result};
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

pub(super) fn config(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    let mut changed = false;
    if let Some(retry) = document
        .get_mut("retry")
        .and_then(|t| t.as_table_like_mut())
    {
        if retry.remove("credit_cooldown").is_some() {
            changed = true;
        }
    }
    Ok(if changed {
        document.to_string()
    } else {
        text.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The old cooldown leaves; every other retry choice stays exactly as the
    /// person wrote it.
    #[test]
    fn the_credit_cooldown_is_removed_and_nothing_else_moves() {
        let before =
            "[retry]\nmax_attempts = 100000\ncredit_cooldown = \"15m\"\nmax_delay = \"15s\"\n";
        let after = config(before).unwrap();
        assert!(!after.contains("credit_cooldown"), "{after}");
        assert!(after.contains("max_attempts = 100000"), "{after}");
        assert!(after.contains("max_delay = \"15s\""), "{after}");
        // Twice is the same as once: a step that already ran changes nothing.
        assert_eq!(config(&after).unwrap(), after);
    }

    /// A file that never had the key is returned untouched.
    #[test]
    fn a_file_without_the_key_is_left_alone() {
        let text = "[retry]\nmax_attempts = 5\n";
        assert_eq!(config(text).unwrap(), text);
    }
}
