//! Compaction keeps source identity before selecting excerpts. A raw JSON-array
//! cut could erase a whole middle action; a body cut could erase a user's rule.
use crate::{
    settings::configuration::Configuration,
    store::sessions::{Message, Role},
};
use serde_json::Value;

pub(crate) struct Document {
    pub label: String,
    pub body: String,
}

pub(crate) fn excerpt(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let head = max / 2;
    let tail = max - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start}\n… [{} characters omitted] …\n{end}", count - max)
}

pub(crate) fn documents(rows: &[&Message], config: &Configuration) -> Vec<Document> {
    let mut out = Vec::new();
    let message = config.number("compact.message_chars") as usize;
    let call = config.number("compact.tool_chars") as usize;
    let owned: Vec<_> = rows.iter().map(|r| (*r).clone()).collect();
    let sources = super::user_instructions::sources(&owned);
    for row in rows {
        let clarification = sources
            .iter()
            .find(|s| s.id == row.id && row.role == Role::Tool);
        let who = if clarification.is_some() {
            "USER original clarification"
        } else {
            match row.role {
                Role::User if row.manual_shell => "USER manual shell script",
                Role::User => "USER original instruction",
                Role::Assistant => "ASSISTANT",
                Role::Tool => "TOOL RESULT",
                Role::Error => continue,
                Role::Context => {
                    use crate::store::sessions::ContextKind;
                    if row.context.as_ref().is_some_and(|meta| {
                        matches!(
                            meta.kind,
                            ContextKind::Session
                                | ContextKind::Status
                                | ContextKind::Memory
                                | ContextKind::ContextUpdate
                        )
                    }) {
                        continue;
                    }
                    "RUNE"
                }
            }
        };
        let body = if let Some(source) = clarification {
            source.text.clone()
        } else if row.role == Role::User {
            row.content.clone()
        } else {
            excerpt(&row.content, message)
        };
        let mut label_parts = vec![format!("{who} (turn {})", row.turn)];
        if let Some(tool) = &row.tool_name {
            label_parts.push(format!("tool {tool}"));
        }
        if let Some(call_id) = &row.tool_call_id {
            label_parts.push(format!("call {call_id}"));
        }
        out.push(Document {
            label: label_parts.join(" · "),
            body,
        });
        for action in row
            .tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let name = action["name"].as_str().unwrap_or("unknown");
            let id = action["id"].as_str().unwrap_or("unknown");
            let args = action["args"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| action["args"].to_string());
            let parsed = serde_json::from_str::<Value>(&args).ok();
            let mut selected = String::new();
            let mut left = call;
            if let Some(map) = parsed.as_ref().and_then(Value::as_object) {
                let mut keys = Vec::new();
                for key in ["path", "process_id", "server_id", "tool", "command"] {
                    if map.contains_key(key) {
                        keys.push(key);
                    }
                }
                keys.extend(map.keys().map(String::as_str).filter(|key| {
                    !["path", "process_id", "server_id", "tool", "command"].contains(key)
                }));
                let both_edits = map.contains_key("old_str") && map.contains_key("new_str");
                let (old_quota, new_quota) = if both_edits {
                    let old_c = map["old_str"].as_str().map_or_else(
                        || map["old_str"].to_string().chars().count(),
                        |s| s.chars().count(),
                    );
                    let new_c = map["new_str"].as_str().map_or_else(
                        || map["new_str"].to_string().chars().count(),
                        |s| s.chars().count(),
                    );
                    let total_edit_budget = left.saturating_sub(
                        keys.iter()
                            .filter(|&&k| !matches!(k, "old_str" | "new_str"))
                            .map(|&k| {
                                map[k].as_str().map_or_else(
                                    || map[k].to_string().chars().count(),
                                    |s| s.chars().count(),
                                )
                            })
                            .sum(),
                    );
                    let half = total_edit_budget / 2;
                    let (oq, nq) = if old_c <= half {
                        (old_c, total_edit_budget.saturating_sub(old_c).min(new_c))
                    } else if new_c <= half {
                        (total_edit_budget.saturating_sub(new_c).min(old_c), new_c)
                    } else {
                        (half, total_edit_budget - half)
                    };
                    (oq.max(100), nq.max(100))
                } else {
                    (left, left)
                };
                for key in keys {
                    let value = &map[key];
                    let text = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    let count = text.chars().count();
                    let keep = if both_edits && key == "old_str" {
                        count.min(old_quota)
                    } else if both_edits && key == "new_str" {
                        count.min(new_quota)
                    } else {
                        count.min(left)
                    };
                    selected.push_str(&format!("{key}: {}\n", excerpt(&text, keep)));
                    left = left.saturating_sub(keep);
                }
            } else {
                selected = excerpt(&args, call);
            }
            out.push(Document {
                label: format!(
                    "ASSISTANT TOOL CALL {}; tool {}",
                    crate::perm::classifier_input::one_line(id),
                    crate::perm::classifier_input::one_line(name)
                ),
                body:format!("Argument fields (target first; total retained values <= {call} characters):\n{selected}"),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: i64, role: Role, text: &str) -> Message {
        Message {
            manual_shell: false,
            id,
            turn: 1,
            role,
            content: text.into(),
            work: None,
            continuation: None,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            thinking_tokens: None,
            media: Vec::new(),
            created_at: 0,
            context: None,
        }
    }
    #[test]
    fn unicode_counts_characters_and_middle_calls_never_disappear() {
        let config = Configuration::default();
        assert_eq!(excerpt(&"🦀".repeat(2000), 2000), "🦀".repeat(2000));
        let mut assistant = row(2, Role::Assistant, "explain");
        assistant.tool_calls = Some(serde_json::json!([
            {"id":"a","name":"create_file","args":serde_json::json!({"path":"a.rs","text":"x".repeat(10000)}).to_string()},
            {"id":"b","name":"patch_file","args":"{\"path\":\"middle.rs\"}"},
            {"id":"c","name":"terminal_use","args":"{\"command\":\"cargo test\"}"}
        ]));
        let user = row(
            1,
            Role::User,
            &format!(
                "{}\nDo not change public API\n{}",
                "u".repeat(40000),
                "v".repeat(40000)
            ),
        );
        let result = row(3, Role::Tool, &"r".repeat(50000));
        let docs = documents(&[&user, &assistant, &result], &config);
        assert_eq!(docs.len(), 6);
        assert_eq!(docs[0].body, user.content);
        assert!(docs
            .iter()
            .any(|d| d.label.contains("patch_file") && d.body.contains("middle.rs")));
        assert!(docs
            .last()
            .unwrap()
            .body
            .contains("18000 characters omitted"));
    }

    #[test]
    fn clean_labels_and_balanced_patch_excerpts() {
        let config = Configuration::default();
        let user = row(1, Role::User, "Please update");
        let mut assistant = row(2, Role::Assistant, "Updating");
        assistant.tool_calls = Some(serde_json::json!([{
            "id": "call_1",
            "name": "patch_file",
            "args": serde_json::json!({
                "path": "src/lib.rs",
                "old_str": "fn old() {}",
                "new_str": "y".repeat(3000)
            }).to_string()
        }]));
        let docs = documents(&[&user, &assistant], &config);
        assert_eq!(docs[0].label, "USER original instruction (turn 1)");
        assert!(!docs[0].label.contains("None"));
        assert!(!docs[0].label.contains("Some("));
        // Short old_str + long new_str must give the remaining budget to new_str
        // instead of wasting half of `compact.tool_chars` (2000).
        let patch_doc = docs
            .iter()
            .find(|d| d.label.contains("patch_file"))
            .unwrap();
        assert!(patch_doc.body.contains("fn old() {}"));
        assert!(patch_doc.body.contains("1900") || patch_doc.body.chars().count() > 1500);
    }
}
