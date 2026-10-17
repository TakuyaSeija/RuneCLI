//! What one Rune process hands the next when it `exec`s a newer build of
//! itself — the window carrying what was being typed, the host carrying the
//! sessions it held open.
//!
//! # One record, with its format named
//!
//! The process writing the record is the older build and the one reading it
//! the newer, so the two never share code: the record is their whole contract.
//! It travels as one environment variable ([`VAR`]) holding a JSON object that
//! names its format first. This module is the only code that writes or reads
//! it, so the contract is in one place and its tests pin it.
//!
//! A reader accepts its own format and the one the release before it wrote —
//! that release is the one that hands over to this one. Rune 0.1.5 wrote no
//! record, only a set of `RUNE_HANDOVER_*` variables ([`FORMAT`] − 1); they are
//! read here, once, into the same record. Any other format is refused rather
//! than half-read: the process starts as if nothing were handed over and says
//! so, because a guessed field is worse than a missing one.
//!
//! # Why the environment
//!
//! `exec` keeps the process — its pid, its terminal, its open descriptors — and
//! replaces only the program. Arguments say what to run; the environment is the
//! one channel that also reaches a build going back to the one that handed it
//! over (`build::go_back` execs the program in [`Handover::program`] with the
//! same environment), so a failed upgrade hands the previous build exactly
//! what it would have read.

use serde::{Deserialize, Serialize};

/// The variable the record travels in.
pub const VAR: &str = "RUNE_HANDOVER";

/// The format this build writes.
pub const FORMAT: u32 = 2;

/// Everything one process hands the next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Handover {
    /// The window's state, when a window handed over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
    /// The sessions a host held open, the one it was started for included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<i64>,
    /// The background tasks of those sessions, the running ones with their
    /// terminals open across the exec — the successor takes them over, still
    /// running (`runeengine::session::background`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<runeengine::session::background::SessionTasks>,
    /// What `rune -host` had read of its stdin and not yet run — the start of
    /// a command still arriving. Its successor reads on from there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<u8>,
    /// The ownership lock of every session a host held, open across the exec:
    /// the successor takes each over (`session_ipc::adopt_ownership`), so the
    /// session is never without an owner while its host changes build — a
    /// second host started meanwhile steps aside instead of taking it. Handed
    /// only to a build that says it adopts them (`build::readiness`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<Owner>,
    /// The program of the build that handed over, open across the exec
    /// (`platform::own_program`): where this process goes back when it cannot
    /// bring the home up to its own build, so a session never loses its host
    /// — on any machine, with no copy of an older build kept anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<runeengine::platform::Handed>,
}

/// A session's ownership lock, open across the exec. The field keeps the name
/// every build since 0.2.3 wrote and read: the record is the two builds' only
/// shared contract.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    pub session: i64,
    pub fd: runeengine::platform::Handed,
}

/// What a window was showing and holding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// The unsent draft of a window with no session yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    /// The next request id, so the successor's ids never repeat one a host
    /// may still be about to acknowledge.
    pub request_next: u64,
    pub input: String,
    /// Byte offset of the cursor in `input`.
    pub cursor: usize,
    /// Paths of the files attached to `input`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attached: Vec<String>,
    #[serde(default)]
    pub scroll: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<Panel>,
    /// Handovers in a row before this one, and when the last happened: a build
    /// that keeps handing over to itself is stopped (see `ui`).
    #[serde(default)]
    pub chain: Chain,
    /// Session hosts this window started that may still be running. They stay
    /// its children across the exec; the successor waits for them, so one that
    /// ends is collected rather than left a zombie (`Driver::adopt_children`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<u32>,
}

/// The panel a window had open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "panel")]
pub enum Panel {
    /// The settings page (as the window's own type serialises it) and row.
    Settings {
        page: serde_json::Value,
        row: usize,
    },
    Tasks,
    Help,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Chain {
    pub count: usize,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// A record that was handed over and could not be read.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused(pub String);

impl Handover {
    /// The record as the variable's value.
    pub fn encode(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        value["format"] = FORMAT.into();
        value.to_string()
    }

    /// Hand `self` to the program `command` will run: the record set, and every
    /// variable an older format used removed, so nothing stale rides along.
    pub fn apply(&self, command: &mut std::process::Command) {
        for name in OLD {
            command.env_remove(name);
        }
        command.env(VAR, self.encode());
    }
}

/// The variables Rune 0.1.5 handed over (format 1).
const OLD: [&str; 10] = [
    "RUNE_HANDOVER_DRAFT",
    "RUNE_HANDOVER_REQUEST_NEXT",
    "RUNE_HANDOVER_INPUT",
    "RUNE_HANDOVER_CURSOR",
    "RUNE_HANDOVER_ATTACH",
    "RUNE_HANDOVER_SCROLL",
    "RUNE_HANDOVER_PANEL",
    "RUNE_HANDOVER_PAGE",
    "RUNE_HANDOVER_ROW",
    "RUNE_HANDOVER_SESSIONS",
];

/// The window's loop guard of format 1, set on every window exec.
const OLD_CHAIN: [&str; 3] = [
    "RUNE_CLI_HANDOVER_ACTIVE",
    "RUNE_CLI_HANDOVER_COUNT",
    "RUNE_CLI_HANDOVER_TIME",
];

/// Take what this process was handed — and clear it from the environment, so
/// nothing this process starts inherits it. Every terminal handed on closes
/// on exec again before this returns, so no program this process starts
/// before they are taken over can inherit one.
pub fn take() -> Result<Option<Handover>, Refused> {
    let read = read(|name| std::env::var(name).ok());
    for name in std::iter::once(VAR).chain(OLD).chain(OLD_CHAIN) {
        std::env::remove_var(name);
    }
    if let Ok(Some(handed)) = &read {
        for kept in handed
            .tasks
            .iter()
            .flat_map(|session| &session.tasks)
            .filter_map(|task| task.pty)
            .chain(handed.owners.iter().map(|owner| owner.fd))
        {
            let _ = kept.keep_across_exec(false);
        }
        // The home is this build's now: the way back is not needed.
        if let Some(program) = handed.program {
            program.close();
        }
    }
    read
}

/// The program of the build that handed this process over, read without
/// taking the record — asked when the home could not be brought up to this
/// build, before anything else was taken.
pub fn way_home() -> Option<runeengine::platform::Handed> {
    match read(|name| std::env::var(name).ok()) {
        Ok(Some(handed)) => handed.program,
        _ => None,
    }
}

/// Become the program `command` names, handing it `record` and the way back
/// to this build. The one way a Rune process `exec`s a newer build of itself
/// (a window, a session host, the picker). Returns only when the exec failed.
pub fn exec(mut record: Handover, command: &mut std::process::Command) -> std::io::Error {
    record.program = runeengine::platform::own_program().ok();
    record.apply(command);
    let error = runeengine::platform::replace_process(command, None);
    if let Some(program) = record.program {
        program.close();
    }
    error
}

/// The sessions whose ownership lock this process holds from the build that
/// `exec`d it, read without taking the record: the home is brought up to this
/// build before the record is taken (`main`), and the upgrade must not wait
/// for locks this very process holds. A record that does not read names none.
pub fn owned_sessions() -> Vec<i64> {
    match read(|name| std::env::var(name).ok()) {
        Ok(Some(handed)) => handed.owners.iter().map(|owner| owner.session).collect(),
        _ => Vec::new(),
    }
}

/// The record in an environment seen through `get`.
fn read(get: impl Fn(&str) -> Option<String>) -> Result<Option<Handover>, Refused> {
    match get(VAR) {
        Some(text) => current(&text).map(Some),
        None => Ok(format_1(get)),
    }
}

fn current(text: &str) -> Result<Handover, Refused> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| Refused(format!("the handed-over record is not JSON: {e}")))?;
    match value.get("format").and_then(serde_json::Value::as_u64) {
        Some(format) if format == u64::from(FORMAT) => serde_json::from_value(value)
            .map_err(|e| Refused(format!("the handed-over record does not read: {e}"))),
        Some(format) => Err(Refused(format!(
            "the handed-over record is format {format}; this build reads format {FORMAT}"
        ))),
        None => Err(Refused("the handed-over record names no format".into())),
    }
}

/// What Rune 0.1.5 handed over, read into a record. Its values were written by
/// that build's own code, so a value that does not parse is left out — exactly
/// as 0.1.5 itself read them.
fn format_1(get: impl Fn(&str) -> Option<String>) -> Option<Handover> {
    let sessions: Vec<i64> = get("RUNE_HANDOVER_SESSIONS")
        .map(|list| list.split(',').filter_map(|id| id.parse().ok()).collect())
        .unwrap_or_default();
    let input = get("RUNE_HANDOVER_INPUT");
    let panel = get("RUNE_HANDOVER_PANEL");
    let draft = get("RUNE_HANDOVER_DRAFT");
    let request_next = get("RUNE_HANDOVER_REQUEST_NEXT").and_then(|n| n.parse().ok());
    let window = (input.is_some() || panel.is_some() || draft.is_some() || request_next.is_some())
        .then(|| Window {
            draft,
            request_next: request_next.unwrap_or(0),
            cursor: get("RUNE_HANDOVER_CURSOR")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            input: input.unwrap_or_default(),
            attached: get("RUNE_HANDOVER_ATTACH")
                .map(|paths| paths.split(":::").map(String::from).collect())
                .unwrap_or_default(),
            scroll: get("RUNE_HANDOVER_SCROLL")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            panel: panel.and_then(|panel| match panel.as_str() {
                "settings" => Some(Panel::Settings {
                    page: get("RUNE_HANDOVER_PAGE")
                        .and_then(|page| serde_json::from_str(&page).ok())
                        .unwrap_or(serde_json::Value::Null),
                    row: get("RUNE_HANDOVER_ROW")
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0),
                }),
                "tasks" => Some(Panel::Tasks),
                "help" => Some(Panel::Help),
                _ => None,
            }),
            chain: Chain {
                count: get("RUNE_CLI_HANDOVER_COUNT")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
                at: get("RUNE_CLI_HANDOVER_TIME")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
            },
            children: Vec::new(),
        });
    (window.is_some() || !sessions.is_empty()).then_some(Handover {
        window,
        sessions,
        ..Handover::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn window() -> Window {
        Window {
            draft: None,
            request_next: (42u64 << 32) | 7,
            input: "half a ✓ prompt".into(),
            cursor: 7,
            attached: vec!["/w/a.png".into(), "/w/b.rs".into()],
            scroll: 12,
            panel: Some(Panel::Settings {
                page: serde_json::json!("Connections"),
                row: 3,
            }),
            chain: Chain {
                count: 1,
                at: 1_791_000_000,
            },
            children: Vec::new(),
        }
    }

    /// What Rune 0.1.5 set on a window exec, byte for byte — the handover this
    /// build receives when it is installed over 0.1.5.
    #[test]
    fn a_window_handed_over_by_0_1_5_reads_whole() {
        let handed = read(env(&[
            (
                "RUNE_HANDOVER_REQUEST_NEXT",
                &((42u64 << 32) | 7).to_string(),
            ),
            ("RUNE_HANDOVER_INPUT", "half a ✓ prompt"),
            ("RUNE_HANDOVER_CURSOR", "7"),
            ("RUNE_HANDOVER_ATTACH", "/w/a.png:::/w/b.rs"),
            ("RUNE_HANDOVER_SCROLL", "12"),
            ("RUNE_HANDOVER_PANEL", "settings"),
            ("RUNE_HANDOVER_PAGE", "\"Connections\""),
            ("RUNE_HANDOVER_ROW", "3"),
            ("RUNE_CLI_HANDOVER_ACTIVE", "1"),
            ("RUNE_CLI_HANDOVER_COUNT", "1"),
            ("RUNE_CLI_HANDOVER_TIME", "1791000000"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            handed,
            Handover {
                window: Some(window()),
                ..Handover::default()
            }
        );
    }

    /// What Rune 0.1.5 set on a session host's exec.
    #[test]
    fn a_host_handed_over_by_0_1_5_reads_whole() {
        let handed = read(env(&[("RUNE_HANDOVER_SESSIONS", "535,536,x")]))
            .unwrap()
            .unwrap();
        assert_eq!(handed.sessions, [535, 536]);
        assert_eq!(handed.window, None);
        assert_eq!(read(env(&[])).unwrap(), None, "nothing handed over");
    }

    /// A descriptor number as the record carries it.
    fn handed(number: i32) -> runeengine::platform::Handed {
        serde_json::from_value(serde_json::json!(number)).unwrap()
    }

    /// The record this build writes is the record it reads, field for field,
    /// and it names its format.
    #[test]
    fn the_record_round_trips_and_names_its_format() {
        let record = Handover {
            window: Some(Window {
                children: vec![4242],
                ..window()
            }),
            sessions: vec![1, 9],
            tasks: vec![runeengine::session::background::SessionTasks {
                session: 9,
                tasks: vec![runeengine::session::background::HandedTask {
                    info: runeengine::session::background::BackgroundTaskInfo {
                        task_id: "bg_abc".into(),
                        pid: 4242,
                        command: "npm run dev".into(),
                        out_file: "/home/me/.rune/projects/p/sessions/9/tools_result/terminal_use/bg_abc.txt".into(),
                        started_at: 1,
                        status: runeengine::session::background::BackgroundTaskStatus::Running,
                        surfaced: false,
                        answered: false,
                        wake_text: None,
                        capture_note: None,
                        tail: String::new(),
                        screen: None,
                        reported: None,
                        waiting: None,
                        tree: None,
                    },
                    pty: Some(handed(9)),
                    written: 1234,
                    services: None,
                }],
            }],
            input: br#"{"cmd":"pro"#.to_vec(),
            owners: vec![Owner {
                session: 9,
                fd: handed(11),
            }],
            program: Some(handed(7)),
        };
        let text = record.encode();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["format"], FORMAT);
        // What is open across the exec travels as the bare number, as every
        // earlier build of this format wrote and reads it.
        assert_eq!(value["program"], 7);
        assert_eq!(
            value["owners"],
            serde_json::json!([{ "session": 9, "fd": 11 }])
        );
        assert_eq!(value["tasks"][0]["tasks"][0]["pty"], 9);
        assert_eq!(read(env(&[(VAR, &text)])).unwrap(), Some(record));
    }

    /// The record the release before this one wrote — the same format, without
    /// the ownership locks and the window's children — reads whole, with none
    /// of either: that release is the one handing over to this one.
    #[test]
    fn the_previous_releases_record_reads_without_locks_or_children() {
        let previous = r#"{"format":2,"sessions":[460],"window":{"request_next":7,"input":"half","cursor":4,"chain":{"count":1,"at":5}}}"#;
        let handed = read(env(&[(VAR, previous)])).unwrap().unwrap();
        assert_eq!(handed.sessions, [460]);
        assert!(handed.owners.is_empty());
        let window = handed.window.unwrap();
        assert_eq!(window.input, "half");
        assert!(window.children.is_empty());
    }

    /// The record wins over stray variables of the old format, and a record
    /// in a format this build does not read is refused whole, never half-read.
    #[test]
    fn an_unknown_or_broken_record_is_refused_and_the_old_variables_never_mix_in() {
        let mine = Handover {
            sessions: vec![3],
            ..Handover::default()
        }
        .encode();
        let handed = read(env(&[(VAR, &mine), ("RUNE_HANDOVER_SESSIONS", "4")]))
            .unwrap()
            .unwrap();
        assert_eq!(handed.sessions, [3]);

        for text in [
            r#"{"format":99,"sessions":[1]}"#,
            r#"{"sessions":[1]}"#,
            r#"{"format":2,"sessions":"one"}"#,
            "not json",
        ] {
            assert!(read(env(&[(VAR, text)])).is_err(), "{text}");
        }
    }

    /// Writing removes every variable of the old format from what the next
    /// program inherits.
    #[test]
    fn writing_the_record_drops_the_old_variables() {
        let mut command = std::process::Command::new("true");
        command.env("RUNE_HANDOVER_INPUT", "stale");
        Handover::default().apply(&mut command);
        let set: HashMap<_, _> = command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert_eq!(set.get("RUNE_HANDOVER_INPUT"), Some(&None), "removed");
        assert!(set.get(VAR).is_some_and(Option::is_some));
    }
}
