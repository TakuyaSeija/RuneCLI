//! The engine's limits: named reads of the one hot-reloadable configuration.
//!
//! Every value — its default included — lives in
//! [`crate::settings::configuration::FIELDS`]. This is a handle over the shared
//! configuration that gives the reads the engine makes most a name; it never
//! holds a copy of a value, so there is nothing here to fall back to and
//! nothing that can go stale.
use std::path::Path;

use crate::error::Result;
use crate::settings::configuration::Shared;

/// Safe buffer ceiling (8 MiB) for tool JSON argument accumulation & host stdin.
pub const MAX_TOOL_ARG_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// The one hot-reloadable configuration, shared with `crate::settings::Settings`.
    /// Reads through this field always see the latest value swapped in by
    /// `SettingsSet` or a reload; it is never a frozen startup snapshot.
    pub config: Shared,
}

impl Limits {
    /// The configuration in `home`, already validated field by field.
    pub fn load(home: &Path) -> Result<Limits> {
        Ok(Limits::over(Shared::load(home)?))
    }

    /// A view over an existing shared configuration, so the engine hands the
    /// *same* handle to Settings and Limits rather than two snapshots.
    pub fn over(config: Shared) -> Limits {
        Limits { config }
    }

    /// Default timeout for one shell command, in milliseconds.
    pub fn command_timeout_ms(&self) -> u64 {
        self.config.number("terminal.command_timeout")
    }

    /// Maximum tool calls the model may place in one response.
    pub fn max_tool_requests_per_response(&self) -> usize {
        self.config.number("tools.max_actions_per_response") as usize
    }

    /// Transcript size, in estimated tokens, at which older turns are
    /// summarised when the model's own context window is unknown. See
    /// [`crate::session::compact`].
    pub fn compact_at_tokens(&self) -> usize {
        self.config.number("compact.fallback_context_tokens") as usize
    }

    /// How many recent turns survive a compaction verbatim.
    pub fn keep_recent_turns(&self) -> usize {
        self.config.number("compact.keep_recent_turns") as usize
    }

    /// Say something when a session's spend passes this many US dollars; zero
    /// says nothing.
    ///
    /// **A warning, never a block.** A hard ceiling that stops a turn halfway
    /// through has already paid for the tokens it is refusing to use, and the
    /// user is left with a half-finished edit and a bill for it. What counts as
    /// a lot of money is the user's to know, so the shipped default is off.
    pub fn warn_spend_usd(&self) -> f64 {
        self.config
            .get("usage.warn_spend_usd")
            .as_f64()
            .expect("validated money field")
    }

    /// Whether the screen may ask the terminal to report the mouse. Nothing in
    /// the engine reads it; the screen does.
    pub fn mouse(&self) -> bool {
        self.config.flag("ui.mouse")
    }

    /// The visual theme: violet, cyan or amber.
    pub fn theme(&self) -> String {
        self.config.text("ui.theme")
    }

    /// Whether the conversation side panel is visible by default.
    pub fn sidebar(&self) -> bool {
        self.config.flag("ui.sidebar")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::configuration::{Configuration, FILE};

    fn home(name: &str, contents: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("rune-test").join(format!(
            "runecfg-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(c) = contents {
            std::fs::write(dir.join(FILE), c).unwrap();
        }
        dir
    }

    #[test]
    fn no_file_means_the_defaults() {
        let l = Limits::load(&home("none", None)).unwrap();
        assert_eq!(
            l.command_timeout_ms(),
            Limits::default().command_timeout_ms()
        );
        assert_eq!(Limits::default().max_tool_requests_per_response(), 20);
    }

    #[test]
    fn a_partial_file_overrides_only_what_it_names() {
        let dir = home("partial", Some("[tools]\nmax_actions_per_response = 5\n"));
        let l = Limits::load(&dir).unwrap();
        assert_eq!(l.max_tool_requests_per_response(), 5);
        assert_eq!(
            l.command_timeout_ms(),
            Limits::default().command_timeout_ms()
        );
    }

    #[test]
    fn a_misspelled_key_is_an_error_rather_than_a_setting_that_never_applied() {
        let dir = home("typo", Some("[checkpoint]\nmax_file_sise = \"1MiB\"\n"));
        let why = Limits::load(&dir).unwrap_err().to_string();
        assert!(why.contains("checkpoint.max_file_sise"), "{why}");
    }

    #[test]
    fn values_outside_their_range_are_refused_with_the_reason() {
        for bad in [
            "[checkpoint.diff]\ncontext = 1000\n",
            "[scan]\nmax_depth = 0\n",
            "[terminal]\ncommand_timeout = \"10ms\"\n",
            "[tools]\nmax_actions_per_response = 0\n",
            "[usage]\nwarn_spend_usd = -1.0\n",
            "[ui]\ntheme = \"neon-pink\"\n",
        ] {
            let dir = home("range", Some(bad));
            let why = Limits::load(&dir).unwrap_err().to_string();
            assert!(
                why.contains("must"),
                "{bad} was accepted, or refused oddly: {why}"
            );
        }
    }

    /// Zero is how the spending warning is switched off, so it must be legal —
    /// the opposite of almost every other limit, where zero is a broken engine.
    #[test]
    fn a_spending_mark_of_zero_is_off_and_any_other_is_kept() {
        assert_eq!(
            Limits::default().warn_spend_usd(),
            0.0,
            "no figure is guessed for someone else's money"
        );
        let dir = home("spend", Some("[usage]\nwarn_spend_usd = 12.5\n"));
        assert_eq!(Limits::load(&dir).unwrap().warn_spend_usd(), 12.5);
    }

    #[test]
    fn an_edited_theme_is_what_the_next_load_reads() {
        assert_eq!(Limits::default().theme(), "amber");
        let dir = home("theme-update", None);
        Configuration::edit(
            &dir,
            Some("ui.theme"),
            Some(serde_json::json!("cyan")),
            false,
            None,
        )
        .unwrap();
        assert_eq!(Limits::load(&dir).unwrap().theme(), "cyan");
    }
}
