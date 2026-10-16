//! Format 13 → 14 retires count-limited user evidence: `classifier.max_user_messages`
//! is no longer a key. Nothing else is touched — new defaults for recent turns,
//! compact output and review exclusions reach a file only where it holds a
//! default, which format 16 tells apart ([`super::defaults`]); a value the person
//! chose stays theirs (owner, 2026-10-14: this step used to overwrite them).
use super::Plan;
use crate::error::{Error, Result};
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

pub(super) fn retire(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    if let Some(table) = doc
        .get_mut("classifier")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        table.remove("max_user_messages");
    }
    Ok(doc.to_string())
}

fn config(text: &str) -> Result<String> {
    crate::settings::configuration::laid_out(&retire(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_removes_only_the_retired_key_and_keeps_every_choice() {
        let text="[classifier]\nmax_user_messages = 8\nmax_evidence_size = \"512KiB\"\nexcluded_tools = [\"manage_process\", \"inspect_file\"]\n[compact]\nkeep_recent_turns = 7\nmax_output_tokens = 32000\nmessage_chars = 50000\n";
        let next = config(text).unwrap();
        let c = crate::settings::configuration::Configuration::from_values(
            toml_edit::de::from_str(&next).unwrap(),
        )
        .unwrap();
        assert_eq!(c.number("compact.keep_recent_turns"), 7);
        assert_eq!(c.number("compact.max_output_tokens"), 32000);
        assert_eq!(c.number("compact.message_chars"), 50000);
        assert_eq!(c.number("classifier.max_evidence_size"), 512 * 1024);
        assert_eq!(
            c.get("classifier.excluded_tools"),
            &serde_json::json!(["manage_process", "inspect_file"])
        );
        assert!(!next.contains("max_user_messages"));
        assert_eq!(config(&next).unwrap(), next);
        assert!(crate::settings::configuration::Configuration::parse_value(
            "classifier.max_user_messages",
            "1"
        )
        .is_err());
    }
}
