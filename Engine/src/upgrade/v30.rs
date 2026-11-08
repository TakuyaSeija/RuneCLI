//! Retire the values that became Rune's own constants (owner, 2026-11-08).
//!
//! A person may have set one of them while the program still let them:
//! `ui.escape_timeout`, `terminal.rows`, `storage.busy_timeout` and the rest of
//! [`crate::settings::configuration::FIXED`]. From this format on, the value is
//! the program's own and nothing may write it, so the file is cleaned of it
//! here instead of being refused: nobody is locked out of Rune by a value they
//! can no longer set. Only those keys are touched — every other choice, its
//! comment and the file's order stay exactly as the person left them.
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
    let mut removed = false;
    for key in crate::settings::configuration::FIXED {
        removed |= crate::settings::configuration::remove_key(&mut document, key);
    }
    if !removed {
        return Ok(text.into());
    }
    // A table that held nothing but retired values would otherwise stay behind
    // as an empty header.
    prune_empty_tables(document.as_table_mut());
    Ok(document.to_string())
}

fn prune_empty_tables(table: &mut toml_edit::Table) {
    let mut drop = Vec::new();
    for (name, item) in table.iter() {
        let Some(child) = item.as_table() else {
            continue;
        };
        if !child.is_empty() {
            continue;
        }
        // A header the person wrote something above keeps its words with it:
        // the retired value goes, the note stays. Only a bare table that the
        // retired values left behind is dropped.
        let speaks = child
            .decor()
            .prefix()
            .and_then(|prefix| prefix.as_str())
            .is_some_and(|prefix| !prefix.trim().is_empty());
        if !speaks {
            drop.push(name.to_string());
        }
    }
    for name in drop {
        table.remove(&name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_keeping_a_retired_value_is_cleaned_and_everything_else_stays() {
        let text = "# my settings\n[ui]\ntheme = \"cyan\" # chosen\nescape_timeout = \"50ms\"\nspinner_frames = 6\n\n[terminal]\ncommand_timeout = \"2m\"\nrows = 40\n";
        let next = config(text).unwrap();
        assert!(!next.contains("escape_timeout"));
        assert!(!next.contains("spinner_frames"));
        assert!(!next.contains("rows"));
        assert!(next.contains("theme = \"cyan\" # chosen"));
        assert!(next.contains("command_timeout = \"2m\""));
        assert_eq!(config(&next).unwrap(), next);
    }

    #[test]
    fn a_table_left_with_nothing_is_dropped_instead_of_left_empty() {
        let text = "[ui]\nframe_interval = \"16ms\"\n";
        let next = config(text).unwrap();
        assert!(!next.contains("[ui]"), "{next:?}");
        assert_eq!(next.trim(), "");
    }

    #[test]
    fn a_missing_or_damaged_shape_never_fails_the_upgrade() {
        assert_eq!(config("").unwrap(), "");
        assert_eq!(config("version = 1\n").unwrap(), "version = 1\n");
        // A value of the wrong shape is still just a key to remove.
        let next = config("[ui]\ncolor = 5\n").unwrap();
        assert!(!next.contains("color"));
    }

    #[test]
    fn the_cleaned_file_loads_and_the_value_is_the_programs_own() {
        let home = super::super::tests::temp("fixed-values-v30");
        super::super::write_marker(&home, 29).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[ui]\nescape_timeout = \"900ms\"\ntheme = \"violet\"\n",
        )
        .unwrap();
        super::super::prepare(&home, &home, &[], &mut |_| {}).unwrap();
        let configuration = crate::settings::configuration::Configuration::load(&home).unwrap();
        assert_eq!(
            configuration.text("ui.escape_timeout"),
            "50ms",
            "the program's own value, not the retired choice"
        );
        assert_eq!(configuration.text("ui.theme"), "violet");
        let _ = std::fs::remove_dir_all(home);
    }
}
