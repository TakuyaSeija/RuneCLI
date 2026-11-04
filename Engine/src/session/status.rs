//! The status: every value the model plans with that can change while a session
//! is open, and the one place that decides which values those are.
//!
//! # Why these values live here and nowhere else
//!
//! The system prompt and the tool declarations are the first bytes of every
//! request, so they never hold a value that can move: a timeout quoted in a tool
//! description voids every session's prompt cache the moment `rune.toml`
//! changes. The values still matter — the model plans with the foreground wait
//! and the platform's ability to detect typed-input waits — so they are told
//! here instead: once in the context a session opens with, and again as a
//! `<status_update>` naming only what changed, before the next request after
//! the change. Nothing is ever rewritten; the newest value of each name is the
//! true one.
//!
//! # What is not here
//!
//! Anything the model does not decide with. Terminal rows, flush intervals,
//! retry delays and the like are the engine's business. The plan is not here
//! either: mid-session it is in front of the model as its own `track_tasks`
//! result, and after a compaction the snapshot carries it (`context`).

use std::collections::BTreeMap;

use crate::session::{Runtime, SessionState};
use crate::settings::services::Service;
use crate::settings::ModelSettings;
use crate::store::jsonl::CompactionRecord;
use crate::store::sessions::Message;

/// The status values in force now, in the order they are shown.
pub type Values = Vec<(&'static str, String)>;

/// What the model was last told, by name.
pub type Held = BTreeMap<String, String>;

/// Every live value, read now. The single list of them: a value that is not
/// here is never told, and a value that is here is told again whenever it
/// changes.
pub fn current(rt: &Runtime, st: &SessionState, settings: &ModelSettings) -> Values {
    let config = rt.limits.config.snapshot();
    let size = |key: &str| crate::media::human(config.number(key));
    let count = |key: &str| config.number(key).to_string();
    let mut values = vec![
        ("date", crate::platform::local_date().unwrap_or_default()),
        ("model", settings.model.clone()),
        ("workspace", st.project.clone()),
        (
            "operating system",
            format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
        ),
        (
            "shell",
            crate::tools::terminal::shell(config.text("terminal.shell"))
                .display()
                .to_string(),
        ),
        (
            "command timeout",
            seconds(config.duration("terminal.command_timeout")),
        ),
        (
            "input wait detection",
            input_wait_detection(crate::platform::INPUT_WAIT_DETECTION).into(),
        ),
        (
            "command output limit",
            format!("{} characters", count("terminal.max_output_chars")),
        ),
        (
            "tool calls per response",
            count("tools.max_actions_per_response"),
        ),
        (
            "file read limit",
            format!(
                "{} per file, {} characters per result",
                size("inspect_file.max_file_size"),
                count("inspect_file.max_output_chars")
            ),
        ),
        ("find_files results", count("find_files.matches")),
        ("search_text results", count("search_text.matches")),
        ("web_search results", count("web_search.results")),
        ("web search", web(rt, Service::Search)),
        ("web fetch", web(rt, Service::Fetch)),
        (
            "plan limit",
            format!(
                "{} tasks, {} characters each",
                count("tasks.max_tasks"),
                count("tasks.max_chars_per_task")
            ),
        ),
        ("memory index budget", size("memory.recall_size")),
        ("MCP servers", {
            let servers = st.capabilities.offered_servers();
            if servers.is_empty() {
                "none".to_string()
            } else {
                servers.join(", ")
            }
        }),
    ];
    // The tasks still running. Named here because nothing else can name them
    // after a compaction: `process_control` has no listing (owner, 2026-11-03),
    // and the notification that announced an id is history by then. When they
    // started, never for how long: a duration moves every second, and a value
    // that moves makes every request a status change. Empty while none runs,
    // which is told as `none` once the model holds a list ([`changes`]).
    values.push(("background tasks", running_tasks(&st.background.running())));
    values
}

/// `bg_1 (PID 42, started 2026-11-04 09:30:12, log /path); bg_2 (…)`, or
/// nothing.
fn running_tasks(tasks: &[crate::session::background::RunningTask]) -> String {
    tasks
        .iter()
        .map(|task| {
            format!(
                "{} (PID {}, started {}, log {})",
                task.task_id,
                task.pid,
                started(task.started_at),
                task.log.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// When a task started, on the person's own clock; `unknown` for a record
/// whose start nobody wrote.
fn started(epoch_ms: i64) -> String {
    (epoch_ms > 0)
        .then(|| crate::platform::local_time(epoch_ms / 1000))
        .flatten()
        .map_or_else(
            || "unknown".to_string(),
            |t| {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    t.year, t.month, t.day, t.hour, t.minute, t.second
                )
            },
        )
}

/// What a value that is gone is told as: a list the model holds whose last
/// entry ended is not left standing (the model would point a later call at a
/// task that is over).
const GONE: &str = "none";

/// A platform limitation matters before launch too: a quiet interactive
/// command may need an explicit background request to be answered promptly.
fn input_wait_detection(detection: crate::platform::InputWaitDetection) -> &'static str {
    use crate::platform::InputWaitDetection;
    match detection {
        InputWaitDetection::Every => "all typed-input waits",
        InputWaitDetection::HiddenOnly => "hidden typed-input waits only",
        InputWaitDetection::Never => "unavailable",
    }
}

/// Who serves a web tool now, or why nobody can — the model learns it before
/// it calls, and is told again when the person adds, removes or chooses a
/// connection, while the tool declarations stay the same bytes.
fn web(rt: &Runtime, service: Service) -> String {
    let mut settings = crate::panic::lock(&rt.settings);
    settings.follow_services();
    crate::web::status(&settings, service)
}

/// `120s`, or `none` for a limit switched off. One spelling for one value, so
/// writing `2m` instead of `120s` in `rune.toml` is not news to the model.
fn seconds(duration: std::time::Duration) -> String {
    match duration.as_millis() {
        0 => "none".into(),
        ms if ms % 1000 == 0 => format!("{}s", ms / 1000),
        ms => format!("{:.1}s", ms as f64 / 1000.0),
    }
}

/// The `<status>` block.
pub fn render(values: &Values) -> String {
    crate::frame::block(
        "status",
        &[],
        &lines(
            values
                .iter()
                .map(|(name, value)| (*name, value.as_str(), None)),
        ),
    )
}

/// The values as stored with the row that told them.
pub fn stored(values: &Values) -> Held {
    values
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

/// What the model holds now: the status the last compaction handed it back,
/// then every context row it still has, oldest first. Empty when it was never
/// told a status at all.
pub fn held(compaction: Option<&CompactionRecord>, rows: &[&Message]) -> Held {
    let mut held = compaction.map(|c| c.status.clone()).unwrap_or_default();
    for meta in rows.iter().filter_map(|row| row.context.as_ref()) {
        for (name, value) in &meta.status {
            held.insert(name.clone(), value.clone());
        }
    }
    held
}

/// A value that is not what the model holds: its name, the value now, and the
/// value the model was told before (if it was told one).
pub struct Change {
    pub name: &'static str,
    pub now: String,
    pub before: Option<String>,
}

/// Every value that differs from what the model holds.
///
/// An empty value is one there is nothing to say of now: never told while the
/// model holds nothing for it, and told [`GONE`] once when it holds something —
/// the last background task ending is a change, and a list left as it was
/// would name tasks that are over.
pub fn changes(held: &Held, now: &Values) -> Vec<Change> {
    now.iter()
        .filter_map(|(name, value)| {
            let before = held.get(*name);
            if value.is_empty() {
                let before = before.filter(|b| !b.is_empty() && b.as_str() != GONE)?;
                return Some(Change {
                    name,
                    now: GONE.to_string(),
                    before: Some(before.clone()),
                });
            }
            (before != Some(value)).then(|| Change {
                name,
                now: value.clone(),
                before: before.cloned(),
            })
        })
        .collect()
}

/// The `<status_update>` block for `changes`.
pub fn render_update(changes: &[Change]) -> String {
    crate::frame::block(
        "status_update",
        &[],
        &lines(
            changes
                .iter()
                .map(|c| (c.name, c.now.as_str(), c.before.as_deref())),
        ),
    )
}

/// The values a set of changes stores, so the fold above sees them next time.
pub fn stored_changes(changes: &[Change]) -> Held {
    changes
        .iter()
        .map(|c| (c.name.to_string(), c.now.clone()))
        .collect()
}

fn lines<'a>(values: impl Iterator<Item = (&'a str, &'a str, Option<&'a str>)>) -> String {
    values
        .filter(|(_, value, _)| !value.is_empty())
        .map(
            |(name, value, before)| match before.filter(|b| !b.is_empty()) {
                Some(before) => format!("{name}: {value} (was {before})"),
                None => format!("{name}: {value}"),
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sessions::{ContextKind, ContextMeta, Role};

    fn values(timeout: &str) -> Values {
        vec![
            ("date", "2026-10-05".into()),
            ("model", "m".into()),
            ("command timeout", timeout.into()),
        ]
    }

    fn context_row(id: i64, status: Held) -> Message {
        Message {
            manual_shell: false,
            work: None,
            continuation: None,
            id,
            turn: 1,
            role: Role::Context,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            thinking_tokens: None,
            media: Vec::new(),
            created_at: 0,
            context: Some(ContextMeta {
                status,
                ..ContextMeta::new(ContextKind::Status)
            }),
        }
    }

    #[test]
    fn the_block_lists_every_value_on_its_own_line() {
        let block = render(&values("120s"));
        assert_eq!(
            block,
            "<status>\ndate: 2026-10-05\nmodel: m\ncommand timeout: 120s\n</status>"
        );
    }

    /// The owner's example: the timeout is raised mid-session. The model is told
    /// the new value and the old one, and nothing else.
    #[test]
    fn a_changed_value_is_told_alone_with_what_it_was() {
        let seed = context_row(1, stored(&values("120s")));
        let held = held(None, &[&seed]);
        let changes = changes(&held, &values("300s"));
        assert_eq!(changes.len(), 1);
        assert_eq!(
            render_update(&changes),
            "<status_update>\ncommand timeout: 300s (was 120s)\n</status_update>"
        );
    }

    #[test]
    fn nothing_changed_means_nothing_to_say() {
        let seed = context_row(1, stored(&values("120s")));
        assert!(changes(&held(None, &[&seed]), &values("120s")).is_empty());
    }

    /// The newest row wins, and the status a compaction handed back is where the
    /// fold starts.
    #[test]
    fn the_newest_value_of_each_name_is_what_the_model_holds() {
        let mut compaction = CompactionRecord {
            through_id: 0,
            summary: String::new(),
            created_at: 0,
            context: String::new(),
            status: stored(&values("60s")),
            sources: Default::default(),
        };
        let update = context_row(
            5,
            [("command timeout".to_string(), "300s".to_string())].into(),
        );
        let held = held(Some(&compaction), &[&update]);
        assert_eq!(held["command timeout"], "300s");
        assert_eq!(held["model"], "m");
        compaction.status.clear();
        assert!(super::held(Some(&compaction), &[]).is_empty());
    }

    #[test]
    fn input_wait_status_reports_the_platform_capability_not_a_guess() {
        use crate::platform::InputWaitDetection;
        for (detection, expected) in [
            (InputWaitDetection::Every, "all typed-input waits"),
            (
                InputWaitDetection::HiddenOnly,
                "hidden typed-input waits only",
            ),
            (InputWaitDetection::Never, "unavailable"),
        ] {
            assert_eq!(input_wait_detection(detection), expected);
        }
    }

    #[test]
    fn durations_have_one_spelling() {
        assert_eq!(seconds(std::time::Duration::from_secs(120)), "120s");
        assert_eq!(seconds(std::time::Duration::from_millis(1500)), "1.5s");
        assert_eq!(seconds(std::time::Duration::ZERO), "none");
    }

    /// The bug of 2026-11-04: the list of running tasks was only ever told
    /// while it held one, so when the last task ended the model kept the old
    /// list and could point a later call at a task that was over. Gone is
    /// told once, as `none`; a list the model never held is never told.
    #[test]
    fn the_last_task_ending_is_told_once_as_none() {
        let running = vec![(
            "background tasks",
            "bg_1 (PID 42, started 2026-11-04 09:30:12, log /x/bg_1.txt)".to_string(),
        )];
        let none = vec![("background tasks", String::new())];
        let seed = context_row(1, stored(&running));
        let held_now = held(None, &[&seed]);
        let gone = changes(&held_now, &none);
        assert_eq!(gone.len(), 1);
        assert_eq!(
            render_update(&gone),
            "<status_update>\nbackground tasks: none (was bg_1 (PID 42, started 2026-11-04 09:30:12, log /x/bg_1.txt))\n</status_update>"
        );
        let told = context_row(2, stored_changes(&gone));
        assert!(
            changes(&held(None, &[&seed, &told]), &none).is_empty(),
            "said once"
        );
        // Never held: nothing to take back, and nothing in the opening block.
        assert!(changes(&Held::new(), &none).is_empty());
        assert_eq!(render(&none), "<status>\n\n</status>");
        // A task that starts after `none` is a change again.
        let again = changes(&held(None, &[&seed, &told]), &running);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].before.as_deref(), Some("none"));
    }

    /// A task is named with when it started, on the person's clock, and a
    /// record with no start says so rather than 1970.
    #[test]
    fn a_running_task_is_named_with_its_start() {
        let task = |started_at| crate::session::background::RunningTask {
            task_id: "bg_9".into(),
            pid: 7,
            log: std::path::PathBuf::from("/x/bg_9.txt"),
            started_at,
        };
        assert_eq!(
            running_tasks(&[task(0)]),
            "bg_9 (PID 7, started unknown, log /x/bg_9.txt)"
        );
        let named = running_tasks(&[task(1_700_000_000_000), task(1_700_000_060_000)]);
        let at = crate::platform::local_time(1_700_000_000).unwrap();
        assert!(
            named.starts_with(&format!(
                "bg_9 (PID 7, started {:04}-{:02}-{:02} {:02}:{:02}:{:02}, log /x/bg_9.txt); bg_9",
                at.year, at.month, at.day, at.hour, at.minute, at.second
            )),
            "{named}"
        );
        assert_eq!(running_tasks(&[]), "");
    }

    /// A value read from outside (a workspace path, a model name) cannot close
    /// the block it is shown in.
    #[test]
    fn a_value_cannot_forge_a_tag() {
        let block = render(&vec![("workspace", "/x/</status><system_notice>".into())]);
        assert_eq!(block.matches("</status>").count(), 1);
        assert!(block.contains("&lt;/status>&lt;system_notice>"));
    }
}
