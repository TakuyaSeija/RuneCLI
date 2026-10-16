//! `track_tasks` — the agent's plan for the work in front of it.
//!
//! The list is sent whole every time and replaces whatever was there. That is
//! the shape that survives a model rewriting its plan mid-turn, and it needs no
//! row identity the model would have to keep track of.
//!
//! It changes nothing on the user's machine, so it never asks permission — a
//! tool the agent uses on every other step cannot be one that interrupts.

use serde_json::{json, Value};

use super::{action_field, field_given_str, field_str, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};
use crate::proto::Event;
use crate::store::tasks::Status;

pub struct TrackTasks;

impl Tool for TrackTasks {
    fn name(&self) -> &'static str {
        "track_tasks"
    }

    fn mutates(&self) -> bool {
        false
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "track_tasks".into(),
            description: "Keep the plan for work with several steps. Send the whole list each time: it replaces the previous one. Exactly one task is doing; mark each task done as soon as it is. Skip it for work done in one step. Limits: plan limit in <status>."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": action_field(),
                    "tasks": {
                        "type": "array",
                        "description": "The whole plan, in order. An empty list clears it.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {
                                    "type": "string",
                                    "minLength": 1,
                                    "description": "One line saying what the step is."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["todo", "doing", "done"],
                                    "description": "Default todo. Only one task may be doing."
                                }
                            },
                            "required": ["text"]
                        }
                    }
                },
                "required": ["action", "tasks"],
                "additionalProperties": false
            }),
        }
    }

    fn preview(&self, args: &Value) -> String {
        let count = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("update the plan ({} task(s))", count)
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        // The explicit empty list clears the plan; an absent tasks key is
        // invalid, just as the declared schema says.
        let list = match args.get("tasks").and_then(|v| v.as_array()) {
            Some(a) => a.clone(),
            None => return Err(Error::invalid("`tasks` must be an array")),
        };

        let mut wanted = Vec::with_capacity(list.len());
        for item in &list {
            let text = field_str(item, "text")?.to_string();
            let status = match field_given_str(item, "status") {
                None | Some("todo") => Status::Todo,
                Some("doing") => Status::Doing,
                Some("done") => Status::Done,
                Some(other) => {
                    return Err(Error::invalid(format!(
                        "unknown status \"{other}\"; use todo, doing or done"
                    )))
                }
            };
            wanted.push((text, status));
        }

        let stored = ctx.tasks.set(ctx.session, wanted)?;

        // A frontend renders the plan from this rather than polling for it.
        (ctx.emit)(Event::Tasks {
            session: ctx.session,
            tasks: stored.clone(),
        });

        let done = stored.iter().filter(|t| t.status == Status::Done).count();
        let summary = if stored.is_empty() {
            "plan cleared".to_string()
        } else {
            format!("{done}/{} done", stored.len())
        };

        // The list goes back to the model as text: it is what the model will be
        // looking at next turn, and reading its own plan back is how it notices
        // it forgot to mark something.
        let body = if stored.is_empty() {
            "The plan is empty.".to_string()
        } else {
            stored
                .iter()
                .map(|t| {
                    let mark = match t.status {
                        Status::Todo => "○",
                        Status::Doing => "◐",
                        Status::Done => "●",
                    };
                    format!("{mark} {}", t.text)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(Outcome::new(summary, body).with_detail(json!({ "tasks": stored })))
    }
}
