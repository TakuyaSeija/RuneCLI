//! Format 26 → 27: `approved_skills` leaves a project's capability entry.
//!
//! It was accepted on reading and never written or used — the runtime
//! fallback a retired key must not have. A home that still holds it has it
//! removed once here; every other choice in the entry stays as written.
use super::Plan;
use crate::{Error, Result};
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

/// `capabilities.projects.*.approved_skills` taken out, for this step and for
/// judging an older step's output by this build's keys.
pub(super) fn config(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    let Some(projects) = document
        .get_mut("capabilities")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|capabilities| capabilities.get_mut("projects"))
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return Ok(text.into());
    };
    let mut removed = false;
    for (_, project) in projects.iter_mut() {
        // An entry that is not a table is left for the configuration's own
        // check to name; it never stops the others from being tidied.
        if let Some(project) = project.as_table_like_mut() {
            removed |= project.remove("approved_skills").is_some();
        }
    }
    Ok(if removed {
        document.to_string()
    } else {
        text.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retired_property_goes_and_every_choice_beside_it_stays() {
        let text = "[capabilities]\nenabled = false # mine\n\n[capabilities.projects.a]\nroot = \"/work/a\"\napproved_skills = [\"x\"]\ndisabled_skills = [\"team-style:0123\"]\n\n[capabilities.projects.b]\nroot = \"/work/b\"\napproved_skills = { old = true }\n\n[capabilities.projects.b.approved_servers]\ndocs = \"digest\"\n";
        let next = config(text).unwrap();
        assert!(!next.contains("approved_skills"), "{next}");
        for kept in [
            "enabled = false # mine",
            "root = \"/work/a\"",
            "disabled_skills = [\"team-style:0123\"]",
            "root = \"/work/b\"",
            "docs = \"digest\"",
        ] {
            assert!(next.contains(kept), "{kept}: {next}");
        }
        assert_eq!(config(&next).unwrap(), next, "done once");
        let inline =
            "[capabilities]\nprojects = { a = { root = \"/work/a\", approved_skills = [] } }\n";
        let next = config(inline).unwrap();
        assert!(!next.contains("approved_skills") && next.contains("root = \"/work/a\""));
        next.parse::<toml_edit::DocumentMut>().unwrap();
    }

    #[test]
    fn a_home_without_it_or_with_a_damaged_entry_is_left_as_written() {
        for text in [
            "",
            "[ui]\nstatus_lifetime = '11s'\n",
            "[capabilities]\nprojects = 7\n",
            "[capabilities.projects]\na = \"damaged\"\n",
        ] {
            assert_eq!(config(text).unwrap(), text);
        }
        let mixed = "[capabilities.projects]\na = \"damaged\"\n\n[capabilities.projects.b]\nroot = \"/work/b\"\napproved_skills = []\n";
        let next = config(mixed).unwrap();
        assert!(next.contains("a = \"damaged\"") && !next.contains("approved_skills"));
    }

    #[test]
    fn an_older_home_holding_it_reaches_this_build_and_loads() {
        let home = super::super::tests::temp("approved-skills-v27");
        super::super::write_marker(&home, 26).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[capabilities.projects.a]\nroot = \"/work/a\"\napproved_skills = [\"x\"]\ndisabled_skills = [\"team-style:0123\"]\n",
        )
        .unwrap();
        let session = home.join("projects/example/sessions/1");
        std::fs::create_dir_all(&session).unwrap();
        let history = b"{\"role\":\"user\",\"content\":\"unchanged\"}\n{damaged\n";
        std::fs::write(session.join("session.jsonl"), history).unwrap();
        super::super::prepare(&home, &home, &[], &mut |_| {}).unwrap();
        assert_eq!(
            super::super::marked(&home).unwrap(),
            Some(super::super::VERSION)
        );
        let text = std::fs::read_to_string(home.join("rune.toml")).unwrap();
        assert!(!text.contains("approved_skills") && text.contains("team-style:0123"));
        let configuration = crate::settings::configuration::Configuration::load(&home).unwrap();
        let settings = crate::capabilities::config::Settings::from_config(&configuration).unwrap();
        assert_eq!(settings.projects["a"].disabled_skills, ["team-style:0123"]);
        assert_eq!(
            std::fs::read(session.join("session.jsonl")).unwrap(),
            history
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
