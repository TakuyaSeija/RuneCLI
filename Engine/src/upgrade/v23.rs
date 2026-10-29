//! Old prompts never acquire shell authorization from their contents. Mark
//! their provenance once; old shell identity stays unknown, never guessed.
use super::Plan;
use crate::Result;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn apply(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_sessions(
        store,
        backup,
        plan,
        &["session.jsonl", "composer.json", "toolcalls.jsonl"],
        |name, bytes| {
            if name == "composer.json" {
                let Ok(mut row) = serde_json::from_slice::<Value>(bytes) else {
                    return bytes.to_vec();
                };
                if mark(name, &mut row) {
                    serde_json::to_vec(&row).unwrap_or_else(|_| bytes.to_vec())
                } else {
                    bytes.to_vec()
                }
            } else {
                super::rewrite_lines(bytes, |row| mark(name, row))
            }
        },
    )
}

fn mark(name: &str, row: &mut Value) -> bool {
    let Some(object) = row.as_object_mut() else {
        return false;
    };
    match name {
        "session.jsonl" => {
            if object.contains_key("manual_shell") {
                return false;
            }
            object.insert("manual_shell".into(), json!(false));
            true
        }
        "toolcalls.jsonl" => false,
        "composer.json" => {
            let mut changed = false;
            if let Some(pending) = object.get_mut("pending").and_then(Value::as_array_mut) {
                for entry in pending {
                    if let Some(entry) = entry.as_object_mut() {
                        if !entry.contains_key("manual_shell") {
                            entry.insert("manual_shell".into(), json!(false));
                            changed = true;
                        }
                    }
                }
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_bang_text_never_becomes_authorization_and_damaged_rows_survive() {
        let old = b"{\"role\":\"user\",\"content\":\"! echo old\"}\n{damaged\n";
        let next = super::super::rewrite_lines(old, |row| mark("session.jsonl", row));
        let first: Value =
            serde_json::from_slice(next.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(first["manual_shell"], false);
        assert_eq!(first["content"], "! echo old");
        assert!(next.ends_with(b"{damaged\n"));
        assert_eq!(
            super::super::rewrite_lines(&next, |row| mark("session.jsonl", row)),
            next
        );
        let mut pending = json!({"pending":[{"text":"! echo old"}]});
        assert!(mark("composer.json", &mut pending));
        assert_eq!(pending["pending"][0]["manual_shell"], false);
        assert!(pending.get("shell_receipts").is_none());
    }
}
