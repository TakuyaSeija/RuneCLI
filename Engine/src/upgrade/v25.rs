//! Retire a catalogue's estimated-token setting without reinterpreting it at
//! runtime. The one-time unit conversion preserves its former text allowance;
//! no conversation, measured usage, or historical estimate row is rewritten.
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
    let Some(capabilities) = document
        .get_mut("capabilities")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return Ok(text.into());
    };
    let Some(old) = capabilities.remove("catalog_tokens") else {
        return Ok(text.into());
    };
    if capabilities.get("catalog_bytes").is_none() {
        // These were the old setting's byte bounds, not a new token estimate.
        // An invalid item never prevents the rest of this home from upgrading.
        if let Some(tokens) = old.as_integer().filter(|n| (128..=32768).contains(n)) {
            let mut bytes = toml_edit::Value::from(tokens * 4);
            if let Some(value) = old.as_value() {
                *bytes.decor_mut() = value.decor().clone();
            }
            capabilities.insert("catalog_bytes", toml_edit::Item::Value(bytes));
        }
    }
    Ok(document.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_catalogue_allowance_moves_to_bytes_once() {
        let text = "[capabilities]\ncatalog_tokens = 4096 # my limit\nenabled = false\n[compact]\nfallback_context_tokens = 12345\n";
        let next = config(text).unwrap();
        assert!(next.contains("catalog_bytes = 16384 # my limit"));
        assert!(!next.contains("catalog_tokens"));
        assert!(next.contains("enabled = false"));
        assert!(next.contains("fallback_context_tokens = 12345"));
        assert_eq!(config(&next).unwrap(), next);
    }

    #[test]
    fn a_new_choice_wins_and_a_damaged_item_is_not_a_fatal_upgrade() {
        let text = "[capabilities]\ncatalog_tokens = 4096\ncatalog_bytes = 9000\n";
        assert!(config(text).unwrap().contains("catalog_bytes = 9000"));
        let damaged = "[capabilities]\ncatalog_tokens = \"damaged\"\npage_size = 7\n";
        let next = config(damaged).unwrap();
        assert!(!next.contains("catalog_tokens"));
        assert!(next.contains("page_size = 7"));
    }

    #[test]
    fn earlier_configuration_steps_validate_the_eventual_byte_catalogue() {
        let home = super::super::tests::temp("context-truth-early-v25");
        super::super::write_marker(&home, 18).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[capabilities]\ncatalog_tokens = 4096\n[web_search]\nprovider = \"gone\"\n",
        )
        .unwrap();
        super::super::prepare(&home, &home, &[], &mut |_| {}).unwrap();
        let configuration = crate::settings::configuration::Configuration::load(&home).unwrap();
        assert_eq!(configuration.get("capabilities")["catalog_bytes"], 16384);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn damaged_history_and_usage_are_not_rewritten_by_the_setting_upgrade() {
        let home = super::super::tests::temp("context-truth-v25");
        super::super::write_marker(&home, 24).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[capabilities]\ncatalog_tokens = 4096\n",
        )
        .unwrap();
        let session = home.join("projects/example/sessions/1");
        std::fs::create_dir_all(&session).unwrap();
        let history = b"{\"role\":\"assistant\",\"reasoning\":\"unchanged\"}\n{damaged\n";
        std::fs::write(session.join("session.jsonl"), history).unwrap();
        super::super::prepare(&home, &home, &[], &mut |_| {}).unwrap();
        assert_eq!(
            std::fs::read(session.join("session.jsonl")).unwrap(),
            history
        );
        assert_eq!(
            super::super::marked(&home).unwrap(),
            Some(super::super::VERSION)
        );
        assert!(std::fs::read_to_string(home.join("rune.toml"))
            .unwrap()
            .contains("catalog_bytes = 16384"));
        let _ = std::fs::remove_dir_all(home);
    }
}
