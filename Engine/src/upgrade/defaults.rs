//! Tells a default in `rune.toml` apart from a choice.
//!
//! Until format 16 every save wrote the default of each user-facing key into
//! the file (`layout_keeping`), so a file read as if the person had chosen
//! every value. A later default then never reached them, and an upgrade that
//! wanted it to could only overwrite — choices included (format 14 did; owner,
//! 2026-10-14: "only the default changes, never a custom value"). The file now
//! holds choices only; this is how an older file is brought there once.
//!
//! A value is a default when it means the same as this build's default or as
//! a default an earlier build wrote ([`PAST`]): Rune put it there, nobody chose
//! it apart from the default. Meaning, not spelling: `"300s"` is `"5m"`, a tool
//! list is a set. Anything else is a choice and stays as written.
//!
//! Format 16 only. A file a format-16 build saved holds choices and nothing
//! else, so a value in it equal to some old default was chosen; a later step
//! that changes a default has nothing to forget and must not run this again.
use crate::error::{Error, Result};
use crate::settings::configuration::{parse_duration_sum, parse_size_sum, Kind, FIELDS};
use serde_json::Value;

/// Defaults earlier builds had for keys whose default has since changed, as
/// their `FIELDS` wrote them (read from every source kept in RuneBackups,
/// 2026-10-04 → 0.5.4). A `null` default was never written, so it is not here.
pub(super) const PAST: &[(&str, &[&str])] = &[
    (
        "classifier.excluded_tools",
        &[
            r#"["ask_user_questions","track_tasks","manage_process","web_search","find_files","inspect_file","read_mcp_resource","discover_capabilities","search_text"]"#,
            r#"["ask_user_questions","track_tasks","manage_process","web_search","find_files","inspect_files","read_mcp_resource","discover_capabilities","search_text"]"#,
            r#"["ask_user_questions","track_tasks","manage_process","web_search","web_fetch","find_files","inspect_file","read_mcp_resource","discover_capabilities","search_text"]"#,
        ],
    ),
    ("classifier.max_evidence_size", &[r#""128KiB""#]),
    ("compact.fallback_context_tokens", &["60000"]),
    ("compact.keep_recent_turns", &["4"]),
    ("compact.max_output_tokens", &["8192"]),
    ("compact.message_chars", &["4000"]),
    ("compact.tool_chars", &["500"]),
    ("retry.max_delay", &[r#""15s""#]),
    ("sanitizer.enabled", &["false"]),
    // Until 0.6.4 (owner, 2026-10-16: not every Linux has it).
    ("terminal.shell", &[r#""/bin/bash""#]),
    ("ui.scroll_lines", &["3"]),
];

/// `text` with every key that holds a default taken out; a section left with
/// nothing goes with its last key. Comments and choices are kept as written.
pub(super) fn forget(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    let raw: Value =
        toml_edit::de::from_str(text).map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    for field in FIELDS {
        let Some(present) = field.key.split('.').try_fold(&raw, |v, k| v.get(k)) else {
            continue;
        };
        let current = std::iter::once(field.default);
        let past = PAST
            .iter()
            .filter(|(key, _)| *key == field.key)
            .flat_map(|(_, values)| values.iter().copied());
        let is_default = current.chain(past).any(|written| {
            serde_json::from_str::<Value>(written)
                .is_ok_and(|default| same(field.kind, present, &default))
        });
        if is_default {
            remove(&mut document, field.key);
        }
    }
    crate::settings::configuration::laid_out(&document.to_string())
}

/// Whether two values of a field's kind mean the same setting.
pub(super) fn same(kind: Kind, a: &Value, b: &Value) -> bool {
    match kind {
        // A model, a generation table, capabilities and a connection are
        // always choices: their defaults are absences, never written.
        Kind::Model | Kind::Generation | Kind::Capabilities | Kind::Connection => false,
        Kind::ToolNames => {
            fn set(v: &Value) -> Option<std::collections::BTreeSet<&str>> {
                v.as_array()
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
            }
            set(a).is_some() && set(a) == set(b)
        }
        Kind::Duration => match (a.as_str(), b.as_str()) {
            (Some(a), Some(b)) => {
                parse_duration_sum(a).is_some() && parse_duration_sum(a) == parse_duration_sum(b)
            }
            _ => a == b,
        },
        Kind::Size => match (a.as_str(), b.as_str()) {
            (Some(a), Some(b)) => {
                parse_size_sum(a).is_some() && parse_size_sum(a) == parse_size_sum(b)
            }
            _ => a == b,
        },
        Kind::Count(..) | Kind::Money => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        },
        Kind::Bool | Kind::Text => a == b,
    }
}

fn remove(document: &mut toml_edit::DocumentMut, key: &str) {
    let parts: Vec<&str> = key.split('.').collect();
    let (last, path) = parts.split_last().expect("a field key has a name");
    let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
    for part in path {
        match table
            .get_mut(part)
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            Some(inner) => table = inner,
            None => return,
        }
    }
    table.remove(last);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_current_and_past_go_and_every_choice_stays() {
        let text = "# my notes\n[classifier]\nenabled = true\nexcluded_tools = [\"search_text\", \"ask_user_questions\", \"track_tasks\", \"web_search\", \"find_files\", \"inspect_file\", \"read_mcp_resource\", \"discover_capabilities\", \"web_fetch\"]\n[compact]\nkeep_recent_turns = 4\nmax_output_tokens = 32000\n[retry]\nmax_attempts = 100000\nmax_delay = \"300s\"\ninitial_delay = \"3000ms\"\n[ui]\ntheme = \"amber\"\nscroll_lines = 7\n[sanitizer]\nenabled = false\ntimeout = \"2m\"\n[title]\nmodel = { provider = \"openrouter\", id = \"free\" }\n";
        let next = forget(text).unwrap();
        let value: Value = toml_edit::de::from_str(&next).unwrap();
        // Defaults — this build's, an earlier build's, however spelt — go.
        for gone in [
            "/classifier/enabled",
            "/classifier/excluded_tools",
            "/compact/keep_recent_turns",
            "/retry/initial_delay",
            "/ui/theme",
            "/sanitizer",
        ] {
            assert!(value.pointer(gone).is_none(), "{gone} kept:\n{next}");
        }
        // Choices stay exactly as written.
        assert_eq!(value["compact"]["max_output_tokens"], 32000);
        assert_eq!(value["retry"]["max_attempts"], 100000);
        assert_eq!(value["retry"]["max_delay"], "300s");
        assert_eq!(value["ui"]["scroll_lines"], 7);
        assert_eq!(value["title"]["model"]["provider"], "openrouter");
        assert!(next.starts_with("# my notes"), "{next}");
        // Once is enough.
        assert_eq!(forget(&next).unwrap(), next);
        // What the engine reads is unchanged for every default that went.
        let config = crate::settings::configuration::Configuration::from_values(value).unwrap();
        assert_eq!(config.number("compact.keep_recent_turns"), 5);
        assert!(config.flag("sanitizer.enabled"));
    }

    #[test]
    fn meaning_not_spelling_decides() {
        let tools = Kind::ToolNames;
        assert!(same(
            tools,
            &serde_json::json!(["a", "b"]),
            &serde_json::json!(["b", "a"])
        ));
        assert!(!same(
            tools,
            &serde_json::json!(["a"]),
            &serde_json::json!(["a", "b"])
        ));
        assert!(same(
            Kind::Duration,
            &serde_json::json!("300s"),
            &serde_json::json!("5m")
        ));
        assert!(same(
            Kind::Size,
            &serde_json::json!("262144B"),
            &serde_json::json!("256KiB")
        ));
        assert!(!same(
            Kind::Model,
            &serde_json::Value::Null,
            &serde_json::Value::Null
        ));
    }
}
