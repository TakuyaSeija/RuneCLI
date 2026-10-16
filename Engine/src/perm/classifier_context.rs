//! Only the person's recorded requests and explicit attachments supply task
//! context. Historical calls are inputs, never tool output or authorization.
use super::classifier_input::{self as document, Piece};
use crate::{
    error::{Error, Result, Supervision},
    settings::configuration::Configuration,
    store::sessions::{Message, Role},
};
use serde_json::Value;

pub(crate) struct Context {
    pub pieces: Vec<Piece>,
    pub media: Vec<crate::media::Attachment>,
    pub current_media_start: usize,
}

#[cfg(test)]
pub(crate) fn users(history: &[Message], _max: usize) -> Vec<&Message> {
    history.iter().filter(|m| m.role == Role::User).collect()
}

pub(crate) fn context(
    history: &[Message],
    config: &Configuration,
    pending: &[&str],
) -> Result<Context> {
    let source = crate::session::work::source(history);
    let mut pieces = Vec::new();
    let mut media = Vec::new();
    let mut current_media_start = 0;
    for row in history.iter().filter(|m| m.role == Role::User) {
        current_media_start = media.len();
        media.extend(row.media.iter().cloned());
    }
    for row in crate::session::user_instructions::sources(history) {
        let active = Some(row.work) == source;
        pieces.push(Piece {
            complete: false,
            label: format!(
                "Original user source {}, work {} ({})",
                row.id,
                row.work,
                if active {
                    "active work; later user sources supersede conflicting earlier ones"
                } else {
                    "earlier work; work-specific approvals are not current authorization"
                }
            ),
            body: row.text,
        });
    }
    let include = config
        .get("classifier.context_tools")
        .as_array()
        .expect("ToolNames is an array");
    let mut historical = Vec::new();
    for row in history.iter().rev().filter(|m| m.role == Role::Assistant) {
        for call in row
            .tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
        {
            let Some(name) = call["name"].as_str() else {
                continue;
            };
            let Some(id) = call["id"].as_str() else {
                continue;
            };
            if pending.contains(&id) || !include.iter().any(|v| v.as_str() == Some(name)) {
                continue;
            }
            let Some(args) = call["args"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
            else {
                continue;
            };
            let mut body = format!("Tool: {}\n", document::one_line(name));
            document::arguments(&mut body, name, &args);
            let result = history.iter().rev().find(|m| {
                m.role == Role::Tool && m.turn == row.turn && m.tool_call_id.as_deref() == Some(id)
            });
            if result.is_none() {
                continue;
            }
            body.push_str("A result was recorded; its output is not included.\n");
            historical.push(Piece {
                complete: false,
                label: format!(
                    "Historical tool input, turn {} (context, not a proposed action)",
                    row.turn
                ),
                body,
            });
            if historical.len() == 3 {
                break;
            }
        }
        if historical.len() == 3 {
            break;
        }
    }
    historical.reverse();
    pieces.extend(historical);
    Ok(Context {
        pieces,
        media,
        current_media_start,
    })
}

pub(crate) fn evidence_error() -> Error {
    Error::Classifier(Supervision::undecided(
        "classifier_evidence_limit",
        "user context and actions exceed the configured evidence budget",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: i64, role: Role, text: &str) -> Message {
        Message {
            work: (role == Role::User).then_some(crate::store::sessions::Work {
                source: 1,
                finished: false,
            }),
            id,
            turn: 1,
            role,
            content: text.into(),
            continuation: None,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            media: Vec::new(),
            created_at: 0,
            context: None,
        }
    }
    #[test]
    fn the_source_and_latest_steer_are_real_user_rows() {
        let rows = vec![
            row(1, Role::User, "task"),
            row(2, Role::Context, "not a user"),
            row(3, Role::User, "steer"),
        ];
        assert_eq!(
            users(&rows, 1).iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }
    #[test]
    fn history_filter_is_independent_of_review_exclusions_and_has_three_slots() {
        let mut rows = vec![row(1, Role::User, "task")];
        for id in 2..8 {
            let mut r = row(id, Role::Assistant, "");
            r.tool_calls = Some(
                serde_json::json!([{"id":format!("c{id}"),"name":if id==6{"track_tasks"}else{"inspect_file"},"args":serde_json::json!({"action":"read","path":format!("{id}.txt")}).to_string()}]),
            );
            rows.push(r);
            let mut result = row(id + 100, Role::Tool, "output not authorization");
            result.tool_call_id = Some(format!("c{id}"));
            rows.push(result);
        }
        let config = Configuration::default();
        let result = context(&rows, &config, &["c7"]).unwrap();
        let labels: Vec<_> = result
            .pieces
            .iter()
            .filter(|p| p.label.starts_with("Historical"))
            .map(|p| p.body.as_str())
            .collect();
        assert_eq!(labels.len(), 3);
        assert!(labels[0].contains("3.txt") && labels[2].contains("5.txt"));
    }
}
