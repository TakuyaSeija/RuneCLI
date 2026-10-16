//! One complete clarification form, answered by the host during a running turn.
use super::{action_field, Outcome, Tool, ToolCtx, ToolDecl};
use crate::{
    error::{Error, Result},
    perm::broker::{Answer, Response},
    proto::Event,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const MAX_CUSTOM_CHARS: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Single,
    Multiple,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionItem {
    /// Assigned by the engine, never sent by the model — the model writes
    /// prose, the system names things so an answer can reference its choice
    /// verbatim.
    #[serde(default)]
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// See [`OptionItem::id`].
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub question: String,
    #[serde(rename = "type")]
    pub selection: Selection,
    pub options: Vec<OptionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserAnswer {
    pub id: String,
    pub selected: Vec<String>,
    pub custom: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Answered { answers: Vec<UserAnswer> },
    Dismissed { answers: Vec<UserAnswer> },
    Cancelled,
    Unavailable,
}

impl Response for Reply {
    type Request = Vec<Question>;
    fn valid(&self, questions: &Self::Request) -> bool {
        let (answers, partial) = match self {
            Self::Answered { answers } => (answers, false),
            Self::Dismissed { answers } => (answers, true),
            _ => return true,
        };
        let mut seen = HashSet::new();
        (partial || answers.len() == questions.len())
            && answers.iter().all(|answer| {
                let Some(question) = questions.iter().find(|q| q.id == answer.id) else {
                    return false;
                };
                let mut selected = HashSet::new();
                let count = answer.selected.len() + usize::from(!answer.custom.trim().is_empty());
                seen.insert(&answer.id)
                    && (question.selection == Selection::Multiple || count <= 1)
                    && answer.custom.chars().count() <= MAX_CUSTOM_CHARS
                    && !answer
                        .custom
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                    && (answer.custom.is_empty() || !answer.custom.trim().is_empty())
                    && answer.selected.iter().all(|id| {
                        selected.insert(id) && question.options.iter().any(|o| &o.id == id)
                    })
            })
    }
}

pub struct AskUserQuestions;

impl Tool for AskUserQuestions {
    fn name(&self) -> &'static str {
        "ask_user_questions"
    }
    fn mutates(&self) -> bool {
        false
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: self.name().into(),
            description: "Ask the user for a decision or a missing requirement. Put every question in one call: the user answers them together. The user can always write an answer of their own.".into(),
            parameters: json!({
                "type":"object","additionalProperties":false,
                "required":["action","questions"],
                "properties":{
                    "action":action_field(),
                    "questions":{"type":"array","minItems":1,"description":"The questions, in the order they are shown.","items":{
                        "type":"object","additionalProperties":false,
                        "required":["question","type","options"],
                        "properties":{
                            "title":{"type":"string","minLength":1,"description":"Optional short label naming the decision, not a sentence."},
                            "question":{"type":"string","minLength":1,"description":"One direct question."},
                            "type":{"type":"string","enum":["single","multiple"],"description":"single: one choice. multiple: any number of choices."},
                            "options":{"type":"array","minItems":1,"description":"The choices, most recommended first. The label of a choice you recommend ends with (Recommended).","items":{
                                "type":"object","additionalProperties":false,"required":["label","description"],
                                "properties":{
                                    "label":{"type":"string","minLength":1,"description":"A few words naming the choice."},
                                    "description":{"type":"string","minLength":1,"description":"One line: what choosing it means."}
                                }
                            }}
                        }
                    }}
                }
            }),
        }
    }

    fn preview(&self, args: &Value) -> String {
        super::action_of(args).into()
    }

    fn validate(&self, args: &Value) -> Result<()> {
        super::validate_builtin(&self.declaration().parameters, args)?;
        let mut questions: Vec<Question> = serde_json::from_value(args["questions"].clone())
            .map_err(|_| Error::invalid("Invalid question form"))?;
        prepare(&mut questions);
        for q in &questions {
            let mut labels = HashSet::new();
            if q.options
                .iter()
                .any(|o| !labels.insert(o.label.trim().to_lowercase()))
            {
                return Err(Error::invalid(
                    "Options within a question need distinct labels",
                ));
            }
            for text in [&q.title, &q.question]
                .into_iter()
                .chain(q.options.iter().flat_map(|o| [&o.label, &o.description]))
                .map(String::as_str)
                .chain(std::iter::once(args["action"].as_str().unwrap()))
            {
                if text.trim().is_empty() || text.chars().any(char::is_control) {
                    return Err(Error::invalid(
                        "Question fields must be nonempty text without control characters",
                    ));
                }
            }
        }
        Ok(())
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        let mut questions: Vec<Question> = serde_json::from_value(args["questions"].clone())
            .map_err(|_| Error::invalid("Invalid question form"))?;
        prepare(&mut questions);
        let req = ctx.questions.open(questions.clone());
        let total_q = questions.len();
        (ctx.emit)(Event::QuestionsRequest {
            session: ctx.session,
            id: ctx.call_id.into(),
            req: req.clone(),
            action: super::action_of(args).into(),
            questions,
        });
        let answer = ctx.questions.wait(
            &req,
            ctx.cancel,
            ctx.limits.config.duration("questions.timeout"),
        );
        (ctx.emit)(Event::QuestionsResolved {
            session: ctx.session,
            req,
        });
        let reply = match answer {
            Answer::Decided(reply) => reply,
            Answer::Cancelled => return Err(Error::Cancelled),
            Answer::TimedOut => {
                return Err(Error::tool("User input timed out; no answer was submitted"))
            }
        };
        if matches!(reply, Reply::Cancelled) {
            return Err(Error::Cancelled);
        }
        let (summary, content) = match &reply {
            Reply::Answered { answers } => {
                let filled = answers
                    .iter()
                    .filter(|a| !a.selected.is_empty() || !a.custom.trim().is_empty())
                    .count();
                let summary = if filled == total_q {
                    format!("{total_q} questions answered")
                } else {
                    format!("{filled} of {total_q} questions answered")
                };
                let detail_json = serde_json::to_string(&reply).unwrap_or_default();
                let content = if filled < total_q {
                    format!("{}\n\n{detail_json}", partial_notice(filled, total_q))
                } else {
                    detail_json
                };
                (summary, content)
            }
            Reply::Dismissed { answers } => {
                let filled = answers.len();
                let detail_json = serde_json::to_string(&reply).unwrap_or_default();
                (
                    format!("{filled} of {total_q} questions answered"),
                    format!("{}\n\n{detail_json}", partial_notice(filled, total_q)),
                )
            }
            Reply::Unavailable => (
                "User input unavailable".into(),
                format!(
                    "{}\n\n{}",
                    super::notice::failure(
                        super::notice::status::SERVICE_UNAVAILABLE,
                        "the user input channel is unavailable",
                    ),
                    serde_json::to_string(&reply).unwrap_or_default()
                ),
            ),
            Reply::Cancelled => unreachable!(),
        };
        let detail = serde_json::to_value(&reply)
            .map_err(|_| Error::tool("Could not encode user answers"))?;
        Ok(Outcome::new(summary, content).with_detail(detail))
    }
}

/// Everything the engine owes a form before anyone sees it: the model's
/// optional title filled in, and every question and option named by the
/// system. The ids are deterministic per position, so `validate` and `run`
/// agree without sharing state, and an answer references a choice verbatim.
/// Public because a frontend handed a form straight from the model's JSON —
/// as tests do — is holding an unnamed form: naming it is the engine's job,
/// and this is the one door for that.
pub fn prepare(questions: &mut [Question]) {
    for (idx, q) in questions.iter_mut().enumerate() {
        normalize_title(q, idx);
        assign_ids(q, idx);
    }
}

fn assign_ids(q: &mut Question, idx: usize) {
    q.id = format!("q{idx}");
    for (oi, o) in q.options.iter_mut().enumerate() {
        o.id = format!("q{idx}o{oi}");
    }
}

fn normalize_title(q: &mut Question, idx: usize) {
    if q.title.trim().is_empty() {
        let stop_words = [
            "apa",
            "apakah",
            "bagaimana",
            "dimana",
            "di",
            "mana",
            "siapa",
            "mengapa",
            "kenapa",
            "berapa",
            "yang",
            "kamu",
            "anda",
            "ingin",
            "saya",
            "untuk",
            "what",
            "which",
            "how",
            "where",
            "who",
            "why",
            "when",
            "is",
            "are",
            "do",
            "does",
            "can",
            "could",
            "would",
            "should",
            "your",
            "the",
            "a",
            "an",
        ];
        let filtered: Vec<&str> = q
            .question
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|w| !w.is_empty() && !stop_words.contains(&w.to_lowercase().as_str()))
            .collect();
        q.title = if !filtered.is_empty() {
            let count = filtered.len().min(3);
            filtered[..count].join(" ")
        } else {
            format!("Question {}", idx + 1)
        };
    }
}

/// The report for a form the user submitted with some answers left blank: a
/// fact about the answers, which the model reads before the answers themselves.
fn partial_notice(filled: usize, total: usize) -> String {
    super::notice::inline(&format!(
        "Answered {filled} of {total} questions; the user left the rest blank."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_validation_rejects_malformed_forms_and_names_things_itself() {
        let form: Value =
            serde_json::from_str(include_str!("../../../Cli/tests/Questions.json")).unwrap();
        let tool = AskUserQuestions;
        tool.validate(&form).unwrap();
        assert_eq!(tool.request_count(&form), 1);

        // The system owns the names: a validated form comes out named, and the
        // model has no way to send one.
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        prepare(&mut questions);
        assert_eq!(questions[0].id, "q0");
        assert_eq!(questions[0].options[2].id, "q0o2");
        assert_eq!(questions[1].options[0].id, "q1o0");

        // The form may grow: question and option counts are the model's to
        // judge now — only the shape is policed.
        let mut many = form.clone();
        many["questions"] = Value::Array((0..7).map(|_| form["questions"][0].clone()).collect());
        tool.validate(&many).unwrap();

        for (pointer, value) in [
            ("/questions", json!([])),
            ("/questions/0/question", json!("\u{1b}[2JInjected")),
            ("/questions/0/type", json!("rating")),
            ("/questions/0/options", json!([])),
            ("/questions/0/options/0/description", json!(" ")),
            ("/questions/0/options/1/label", json!("AUTOMATIC SYNC")),
        ] {
            let mut bad = form.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(tool.validate(&bad).is_err(), "{pointer}: {bad}");
        }

        // The model has no way to name things: an id anywhere in the form is
        // an unknown field, and the whole call is refused.
        let mut bad = form.clone();
        bad["questions"][0]
            .as_object_mut()
            .unwrap()
            .insert("id".into(), json!("Bad ID"));
        assert!(tool.validate(&bad).is_err(), "question id: {bad}");
        let mut bad = form.clone();
        bad["questions"][0]["options"][1]
            .as_object_mut()
            .unwrap()
            .insert("id".into(), json!("automatic"));
        assert!(tool.validate(&bad).is_err(), "option id: {bad}");

        let mut bad = form;
        bad["answers"] = json!([]);
        assert!(tool.validate(&bad).is_err());
    }

    #[test]
    fn replies_are_bound_to_complete_questions_and_never_overwrite_an_answer() {
        let form: Value =
            serde_json::from_str(include_str!("../../../Cli/tests/Questions.json")).unwrap();
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        prepare(&mut questions);
        let good = Reply::Answered {
            answers: vec![
                UserAnswer {
                    id: "q0".into(),
                    selected: vec!["q0o0".into(), "q0o2".into()],
                    custom: "Only on Wi-Fi".into(),
                },
                UserAnswer {
                    id: "q1".into(),
                    selected: vec![],
                    custom: "A private NAS".into(),
                },
            ],
        };
        assert!(good.valid(&questions));
        let partial = serde_json::to_value(&good).unwrap()["answers"][0].clone();
        let partial: UserAnswer = serde_json::from_value(partial).unwrap();
        assert!(Reply::Dismissed {
            answers: vec![partial.clone()]
        }
        .valid(&questions));
        assert!(Reply::Dismissed { answers: vec![] }.valid(&questions));
        let mut invalid_partial = partial;
        invalid_partial.id = "unknown".into();
        assert!(!Reply::Dismissed {
            answers: vec![invalid_partial]
        }
        .valid(&questions));
        for (pointer, value) in [
            ("/answers/0/id", json!("q1")),
            ("/answers/0/selected", json!(["unknown"])),
            ("/answers/0/selected", json!(["q0o0", "q0o0"])),
            ("/answers/1/selected", json!(["q0o0"])),
            ("/answers/1/custom", json!(" ")),
            ("/answers/1/custom", json!("x".repeat(MAX_CUSTOM_CHARS + 1))),
            ("/answers/1/custom", json!("\u{1b}]52;c;bad\u{7}")),
            ("/answers", json!([])),
        ] {
            let mut bad = serde_json::to_value(&good).unwrap();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(
                !serde_json::from_value::<Reply>(bad)
                    .unwrap()
                    .valid(&questions),
                "{pointer}"
            );
        }
        let broker = crate::perm::broker::Broker::<Reply>::new();
        let req = broker.open(questions);
        assert!(!broker.reply(&req, Reply::Answered { answers: vec![] }));
        assert!(broker.reply(&req, good));
        assert!(!broker.reply(&req, Reply::Cancelled));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(matches!(
            broker.wait(&req, &cancel, std::time::Duration::from_secs(1)),
            Answer::Decided(Reply::Answered { .. })
        ));
        assert!(!broker.reply(&req, Reply::Unavailable));
    }

    #[test]
    fn options_carry_the_recommended_prefix_and_counts_are_free() {
        let form: Value =
            serde_json::from_str(include_str!("../../../Cli/tests/Questions.json")).unwrap();
        let tool = AskUserQuestions;

        let mut two_opts = form.clone();
        two_opts["questions"][0]["options"] = json!([
            {"label":"Fast sync (Recommended)", "description":"Sync faster."},
            {"label":"Slow sync", "description":"Sync slower."}
        ]);
        assert!(tool.validate(&two_opts).is_ok());

        // Four options, the recommendation either way the label carries it.
        let mut four_opts = form.clone();
        four_opts["questions"][0]["options"] = json!([
            {"label":"Opt 1 (Recommended)", "description":"Desc 1."},
            {"label":"Opt 2", "description":"Desc 2."},
            {"label":"Opt 3", "description":"Desc 3."},
            {"label":"Opt 4", "description":"Desc 4."}
        ]);
        assert!(tool.validate(&four_opts).is_ok());

        // Question with title omitted is valid (auto-derived from question)
        let mut no_title = form.clone();
        no_title["questions"][0]
            .as_object_mut()
            .unwrap()
            .remove("title");
        assert!(tool.validate(&no_title).is_ok());

        // One option or six — the counts are the model's to judge, not ours.
        let mut one_opt = form.clone();
        one_opt["questions"][0]["options"] = json!([
            {"label":"Only one", "description":"Desc 1."}
        ]);
        assert!(tool.validate(&one_opt).is_ok());

        let mut six_opts = form.clone();
        six_opts["questions"][0]["options"] = json!([
            {"label":"Opt 1", "description":"Desc 1."},
            {"label":"Opt 2", "description":"Desc 2."},
            {"label":"Opt 3", "description":"Desc 3."},
            {"label":"Opt 4", "description":"Desc 4."},
            {"label":"Opt 5", "description":"Desc 5."},
            {"label":"Opt 6", "description":"Desc 6."}
        ]);
        assert!(tool.validate(&six_opts).is_ok());

        // What stays policed: siblings that read as one choice.
        let mut twins = form.clone();
        twins["questions"][0]["options"] = json!([
            {"label":"Automatic sync", "description":"One."},
            {"label":"automatic SYNC", "description":"Two."}
        ]);
        assert!(tool.validate(&twins).is_err());
    }

    #[test]
    fn broker_wait_zero_timeout_does_not_timeout() {
        let form: Value =
            serde_json::from_str(include_str!("../../../Cli/tests/Questions.json")).unwrap();
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        prepare(&mut questions);
        let broker = std::sync::Arc::new(crate::perm::broker::Broker::<Reply>::new());
        let req = broker.open(questions);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let b2 = broker.clone();
        let r2 = req.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            b2.reply(
                &r2,
                Reply::Answered {
                    answers: vec![
                        UserAnswer {
                            id: "q0".into(),
                            selected: vec!["q0o0".into()],
                            custom: "".into(),
                        },
                        UserAnswer {
                            id: "q1".into(),
                            selected: vec!["q1o0".into()],
                            custom: "".into(),
                        },
                    ],
                },
            );
        });

        // With zero timeout (unlimited), it waits until replied
        let answer = broker.wait(&req, &cancel, std::time::Duration::ZERO);
        assert!(matches!(answer, Answer::Decided(Reply::Answered { .. })));
        handle.join().unwrap();
    }
}
