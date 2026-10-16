//! Format 15 → 16, two things an older home holds that this build reads
//! differently (owner, 2026-10-14):
//!
//! * `rune.toml` keeps choices only. Defaults an earlier save wrote into it are
//!   taken out ([`super::defaults`]), so the newest default reaches everyone who
//!   never chose, and every choice stays as written.
//! * A session opened before Rune fingerprinted the file-born parts of its
//!   context (`AGENTS.md`, the memory index, the skill list) holds them with no
//!   fingerprint, and nothing could tell its model they changed. Those parts
//!   are marked [`crate::session::context::UNKNOWN_SOURCE`]: what the model was
//!   handed is unknown, so the next request tells it once to read them again,
//!   as any session is told — then it is fingerprinted like the rest. Bodies
//!   never change; the original files stay in the upgrade's backup.
use super::Plan;
use crate::error::Result;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, super::defaults::forget)?;
    super::rewrite_sessions(
        store,
        backup,
        plan,
        &["session.jsonl", "compactions.jsonl"],
        |name, old| match name {
            "session.jsonl" => marked(old, opening),
            _ => marked(old, compaction),
        },
    )
}

/// Every part a context row holds, held as unknown.
fn unknown() -> Value {
    let parts = crate::session::context::SOURCE_PARTS;
    Value::Object(
        parts
            .iter()
            .map(|part| {
                (
                    part.to_string(),
                    json!(crate::session::context::UNKNOWN_SOURCE),
                )
            })
            .collect(),
    )
}

fn holds_none(sources: Option<&Value>) -> bool {
    sources.is_none_or(|s| s.as_object().is_none_or(serde_json::Map::is_empty))
}

/// An opening context row with no fingerprints.
fn opening(row: &mut Value) -> bool {
    let Some(meta) = row.get_mut("context").filter(|m| m.is_object()) else {
        return false;
    };
    if meta["kind"] != "session" || !holds_none(meta.get("sources")) {
        return false;
    }
    meta["sources"] = unknown();
    true
}

/// A compaction that handed a context back with no fingerprints.
fn compaction(record: &mut Value) -> bool {
    let carries = record
        .get("context")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty());
    if !carries || !holds_none(record.get("sources")) {
        return false;
    }
    record["sources"] = unknown();
    true
}

/// The file with each line `mark` changes written anew; every other line —
/// one that does not parse included — kept byte for byte.
fn marked(bytes: &[u8], mark: fn(&mut Value) -> bool) -> Vec<u8> {
    super::rewrite_lines(bytes, mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_contexts_without_fingerprints_are_marked_and_bodies_never_change() {
        let rows = [
            json!({"id":1,"turn":1,"role":"context","content":"opening","context":{"kind":"session","status":{"date":"x"}}}),
            json!({"id":2,"turn":1,"role":"user","content":"hello"}),
            json!({"id":3,"turn":2,"role":"context","content":"later","context":{"kind":"session","sources":{"AGENTS.md":"abc"}}}),
        ];
        let mut text = rows
            .iter()
            .map(|row| row.to_string() + "\n")
            .collect::<String>();
        text.push_str("{not json\n");
        let next = String::from_utf8(marked(text.as_bytes(), opening)).unwrap();
        let lines: Vec<&str> = next.lines().collect();
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["content"], "opening");
        assert_eq!(first["context"]["status"]["date"], "x");
        for part in crate::session::context::SOURCE_PARTS {
            assert_eq!(
                first["context"]["sources"][part],
                crate::session::context::UNKNOWN_SOURCE
            );
        }
        let rest: String = text.lines().skip(1).map(|l| l.to_string() + "\n").collect();
        let kept: String = next.lines().skip(1).map(|l| l.to_string() + "\n").collect();
        assert_eq!(kept, rest, "every other line byte for byte");
        assert_eq!(marked(next.as_bytes(), opening), next.as_bytes(), "once");

        let mut record = json!({"through_id":9,"summary":"s","created_at":1,"context":"held"});
        assert!(compaction(&mut record));
        assert_eq!(record["sources"]["AGENTS.md"], "unknown");
        let mut empty = json!({"through_id":9,"summary":"s","created_at":1});
        assert!(
            !compaction(&mut empty),
            "no context handed back, nothing held"
        );
    }

    #[test]
    fn upgrade_marks_a_legacy_session_and_forgets_written_defaults() {
        let root = std::env::temp_dir().join(format!("rune-v16-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = root.join("projects/p/sessions/7");
        std::fs::create_dir_all(&session).unwrap();
        let backup = root.join("backups/Upgrade16");
        std::fs::create_dir_all(&backup).unwrap();
        let opening =
            json!({"id":1,"turn":1,"role":"context","content":"o","context":{"kind":"session"}});
        std::fs::write(session.join("session.jsonl"), opening.to_string() + "\n").unwrap();
        let original = "[compact]\nkeep_recent_turns = 5\nmax_output_tokens = 32000\n";
        std::fs::write(root.join("rune.toml"), original).unwrap();
        let mut plan = Plan::default();
        apply(&root, &root, &backup, &mut plan).unwrap();
        let config = std::fs::read_to_string(root.join("rune.toml")).unwrap();
        assert!(!config.contains("keep_recent_turns"), "{config}");
        assert!(config.contains("max_output_tokens = 32000"), "{config}");
        assert_eq!(
            std::fs::read_to_string(plan.config.as_ref().unwrap()).unwrap(),
            original,
            "the file as it was is in the backup"
        );
        let row: Value = serde_json::from_str(
            std::fs::read_to_string(session.join("session.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["context"]["sources"]["memory index"], "unknown");
        assert_eq!(plan.files.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
