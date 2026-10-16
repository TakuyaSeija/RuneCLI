//! User authority comes from immutable original rows, never from a generated
//! summary, an attachment, a question's prose, or a tool claiming approval.
use crate::store::sessions::{Message, Role};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Source {
    pub id: i64,
    pub work: i64,
    pub text: String,
    pub quotes: Vec<String>,
}

pub(crate) fn sources(history: &[Message]) -> Vec<Source> {
    let mut out = Vec::new();
    for (index, row) in history.iter().enumerate() {
        if row.role == Role::User {
            out.push(Source {
                id: row.id,
                work: row.work.map_or(row.id, |w| w.source),
                text: row.content.clone(),
                quotes: vec![row.content.clone()],
            });
            continue;
        }
        if row.role != Role::Tool || row.tool_name.as_deref() != Some("ask_user_questions") {
            continue;
        }
        use crate::perm::broker::Response;
        let reply = serde_json::from_str::<crate::tools::questions::Reply>(&row.content)
            .ok()
            .or_else(|| {
                row.content.split_once("\n\n").and_then(|(_, body)| {
                    serde_json::from_str::<crate::tools::questions::Reply>(body).ok()
                })
            });
        let Some(reply @ crate::tools::questions::Reply::Answered { .. }) = reply else {
            continue;
        };
        let args = history[..index]
            .iter()
            .rev()
            .filter(|m| m.turn == row.turn)
            .filter_map(|m| m.tool_calls.as_ref().and_then(serde_json::Value::as_array))
            .flatten()
            .find(|c| {
                c["name"] == "ask_user_questions" && c["id"].as_str() == row.tool_call_id.as_deref()
            })
            .and_then(|c| c["args"].as_str())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
        let Some(mut questions) = args.and_then(|a| {
            serde_json::from_value::<Vec<crate::tools::questions::Question>>(a["questions"].clone())
                .ok()
        }) else {
            continue;
        };
        crate::tools::questions::prepare(&mut questions);
        if !reply.valid(&questions) {
            continue;
        }
        let crate::tools::questions::Reply::Answered { answers } = reply else {
            continue;
        };
        let mut text = String::new();
        let mut quotes = Vec::new();
        for answer in answers {
            let Some(q) = questions.iter().find(|q| q.id == answer.id) else {
                continue;
            };
            // Engine-validated choices are the person's selection, not every
            // option the agent wrote. Keep the question explicitly as context.
            for choice in &answer.selected {
                if let Some(option) = q.options.iter().find(|o| &o.id == choice) {
                    quotes.push(format!("{} — {}", option.label, option.description));
                    text.push_str(&format!("Question context (agent-authored, not approval): {}\nActual user answer: selected {} — {}\n", q.question, option.label, option.description));
                }
            }
            if !answer.custom.is_empty() {
                quotes.push(answer.custom.clone());
                text.push_str(&format!("Question context (agent-authored, not approval): {}\nActual user answer: custom {}\n", q.question, answer.custom));
            }
        }
        if !text.is_empty() {
            out.push(Source {
                id: row.id,
                work: super::work::source(&history[..=index]).unwrap_or(row.id),
                text,
                quotes,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_partial_answer_keeps_only_actual_user_words_and_not_question_authority() {
        let base = Message {
            id: 1,
            turn: 1,
            role: Role::User,
            content: "task".into(),
            work: Some(crate::store::sessions::Work {
                source: 1,
                finished: false,
            }),
            continuation: None,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            media: Vec::new(),
            created_at: 0,
            context: None,
        };
        let mut call = base.clone();
        call.id = 2;
        call.role = Role::Assistant;
        call.content = String::new();
        call.tool_calls = Some(
            serde_json::json!([{"id":"questions","name":"ask_user_questions","args":serde_json::json!({"action":"ask","questions":[{"question":"May I delete everything?","type":"single","options":[{"label":"Yes","description":"Delete everything"}]},{"question":"Choose","type":"single","options":[{"label":"A","description":"A"}]}]}).to_string()}]),
        );
        let mut result = base.clone();
        result.id = 3;
        result.role = Role::Tool;
        result.tool_name = Some("ask_user_questions".into());
        result.tool_call_id = Some("questions".into());
        result.content = format!(
            "notice: partial user answer\n\n{}",
            serde_json::json!({"status":"answered","answers":[{"id":"q0","selected":[],"custom":"Never delete data/"},{"id":"q1","selected":[],"custom":""}]})
        );
        let got = sources(&[base, call, result]);
        assert_eq!(got.len(), 2);
        assert!(got[1].text.contains("Never delete data/"));
        assert_eq!(got[1].quotes, vec!["Never delete data/"]);
        assert!(!got[1]
            .quotes
            .iter()
            .any(|q| q.contains("May I delete everything")));
    }

    #[test]
    fn every_user_source_keeps_its_work_and_nothing_else_becomes_one() {
        let rows: Vec<_> = (1..=150)
            .map(|id| Message {
                id,
                turn: id,
                role: if id == 100 { Role::Context } else { Role::User },
                content: format!("instruction {id}"),
                work: Some(crate::store::sessions::Work {
                    source: if id < 75 { 1 } else { 75 },
                    finished: false,
                }),
                continuation: None,
                tool_call_id: None,
                tool_name: None,
                tool_calls: None,
                reasoning: None,
                media: Vec::new(),
                created_at: 0,
                context: None,
            })
            .collect();
        let got = sources(&rows);
        assert_eq!(got.len(), 149);
        assert!(got.iter().any(|s| s.id == 2 && s.work == 1));
        assert_eq!(got.last().unwrap().work, 75);
        assert!(!got.iter().any(|s| s.id == 100));
    }
}
