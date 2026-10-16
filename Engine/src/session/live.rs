//! One ordered stream, and what the turn in flight has said so far.
//!
//! # Why the engine keeps it
//!
//! Every window of a session, every frontend, must show the same thing: a
//! window opened while a turn is running has to look exactly like one that
//! watched it from the start. That cannot be left to each frontend to
//! reconstruct from three different sources (the live events, a snapshot a host
//! assembled, the history on disk) — each forgot something different, and
//! attachments, timers, queued messages and waiting questions each went missing
//! in one window or another.
//!
//! So the engine, which is the only thing that knows, keeps the one account:
//! [`Bus`] is the only way an event leaves it, and it holds one lock while it
//! (1) folds the event into the account of the running turn and (2) puts it on
//! the stream. A frontend that wants to join asks for a view
//! (`session.view`): the engine builds it under *that same lock* and sends it
//! on the same stream as the answer. The answer is therefore at a definite place
//! in the stream — everything before it is inside the view, everything after it
//! is not — and the frontend drops what came before and applies what follows. No
//! sequence numbers and no overlap checks are needed: the order is the order.
//!
//! # What the account holds
//!
//! The *run* — from the first message that started it to its `done`: every
//! event that adds something to the transcript, merged so a long answer is one
//! text event, not ten thousand; a question that was asked and answered is
//! gone. Everything else a window shows (title, mode, plan, file tally) is state
//! the view reads as it stands, because the latest value is the whole story.
//!
//! The history on disk is read **as of before the run began** (rows of earlier
//! turns only): the run's own rows are in the account, and counting them twice
//! is the bug this arrangement exists to make impossible.

use std::collections::HashMap;
use std::sync::mpsc::{SendError, Sender};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::error::Result;
use crate::proto::Event;

/// What a session's running turn has said, in the order it said it.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// The turn that began the run: history from here on belongs to the account.
    pub first_turn: i64,
    pub events: Vec<Event>,
}

/// What the engine has said about one session: the run in flight, and the
/// latest word on each piece of state a window shows as it stands (title, plan,
/// file tally, mode, settings). The latest event is the truth about state, and
/// it is the one thing a view can take from the stream without ever being a
/// moment behind it.
#[derive(Debug, Clone, Default)]
pub struct Account {
    pub run: Option<Run>,
    last: HashMap<&'static str, Event>,
    /// Classifier facts arrive in both mode and settings events; their stream
    /// order, not the category a view overlays last, decides which one wins.
    classifier: Option<serde_json::Value>,
    /// Notifications raised while no turn ran (the session held by a direct
    /// tool call, say), shown and not yet read by the model: they open the
    /// next run's events, in the order they came.
    pub waiting: Vec<Event>,
}

impl Account {
    /// The newest event of a kind of state: `title`, `tasks`, `file_changes`,
    /// `session`, `settings`, `composer`.
    pub fn last(&self, kind: &str) -> Option<&Event> {
        self.last.get(kind)
    }

    pub fn classifier(&self) -> Option<&serde_json::Value> {
        self.classifier.as_ref()
    }
}

/// The kind of state an event is the latest word on, if it is one.
fn state_kind(event: &Event) -> Option<&'static str> {
    match event {
        Event::Title { .. } => Some("title"),
        Event::Tasks { .. } => Some("tasks"),
        Event::FileChanges { .. } => Some("file_changes"),
        Event::Session { .. } => Some("session"),
        Event::Settings { .. } => Some("settings"),
        Event::Composer { .. } => Some("composer"),
        _ => None,
    }
}

struct Inner {
    tx: Sender<Event>,
    accounts: Mutex<HashMap<i64, Account>>,
}

/// The engine's end of the event stream. Cheap to clone.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

impl Bus {
    pub fn new(tx: Sender<Event>) -> Bus {
        Bus {
            inner: Arc::new(Inner {
                tx,
                accounts: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// A bus nobody listens to, for code under test that only needs to be able
    /// to say things.
    pub fn detached() -> Bus {
        Bus::new(std::sync::mpsc::channel().0)
    }

    /// Fold the event into the account of its session, and put it on the
    /// stream — one critical section, so the account and the stream cannot
    /// disagree about the order.
    pub fn send(&self, event: Event) -> std::result::Result<(), Box<SendError<Event>>> {
        let mut accounts = crate::panic::lock(&self.inner.accounts);
        if let Some(session) = event.session() {
            fold(&mut accounts, session, &event);
        }
        self.inner.tx.send(event).map_err(Box::new)
    }

    /// Build a view of `session` and answer request `id` with it, in one
    /// critical section with [`Bus::send`]: the answer's place in the stream is
    /// exactly where the account stood when `build` read it.
    ///
    /// Lock order: a caller that needs the database takes it **before** this
    /// (every emitter that writes the database and then speaks does the same);
    /// `build` must not take it again, nor any lock an emitter holds while it
    /// speaks.
    pub fn answer(&self, id: u64, session: i64, build: impl FnOnce(&Account) -> Result<Value>) {
        let accounts = crate::panic::lock(&self.inner.accounts);
        let empty = Account::default();
        let result = build(accounts.get(&session).unwrap_or(&empty));
        let event = match result {
            Ok(data) => Event::Ack {
                id,
                ok: true,
                error: None,
                data: Some(data),
            },
            Err(e) => Event::Ack {
                id,
                ok: false,
                error: Some(e.to_string()),
                data: None,
            },
        };
        let _ = self.inner.tx.send(event);
    }

    /// Everything the engine has said about `session` so far, as a view would
    /// read it: the run in flight and the latest word on each piece of state.
    pub fn account_of(&self, session: i64) -> Account {
        crate::panic::lock(&self.inner.accounts)
            .get(&session)
            .cloned()
            .unwrap_or_default()
    }

    /// The account of `session`'s run as it stands now.
    pub fn run_of(&self, session: i64) -> Option<Run> {
        crate::panic::lock(&self.inner.accounts)
            .get(&session)
            .and_then(|account| account.run.clone())
    }
}

/// Fold one event into the account it belongs to.
fn fold(accounts: &mut HashMap<i64, Account>, session: i64, event: &Event) {
    let account = accounts.entry(session).or_default();
    let classifier = match event {
        Event::Session { classifier, .. } => Some(classifier),
        Event::Settings { settings, .. } => settings.get("classifier_status"),
        _ => None,
    };
    if let Some(classifier) = classifier.filter(|value| !value.is_null()) {
        account.classifier = Some(classifier.clone());
    }
    if let Some(kind) = state_kind(event) {
        account.last.insert(kind, event.clone());
        return;
    }
    match event {
        // Shown at once, read by the model later: part of the run in flight,
        // or waiting for the next one.
        Event::Notification { .. } => match account.run.as_mut() {
            Some(run) => run.events.push(event.clone()),
            None => account.waiting.push(event.clone()),
        },
        Event::TurnStart { turn, .. } => {
            let waiting = std::mem::take(&mut account.waiting);
            let run = account.run.get_or_insert_with(|| Run {
                first_turn: *turn,
                events: waiting,
            });
            run.events.push(event.clone());
        }
        // The run is over: everything it said is on disk now.
        Event::Done { .. } => account.run = None,
        _ => {
            if let Some(run) = account.run.as_mut() {
                fold_into(run, event);
            }
        }
    }
}

fn fold_into(run: &mut Run, event: &Event) {
    let events = &mut run.events;
    match event {
        // What the model is saying: one event per stretch of speech.
        Event::Thinking { delta, .. } => {
            clear_retry(events);
            match events.last_mut() {
                Some(Event::Thinking { delta: held, .. }) => held.push_str(delta),
                _ => events.push(event.clone()),
            }
        }
        Event::Text { delta, .. } => {
            clear_retry(events);
            match events.last_mut() {
                Some(Event::Text { delta: held, .. }) => held.push_str(delta),
                _ => events.push(event.clone()),
            }
        }
        // A call being written: progress, superseded by the call itself.
        Event::ToolArgs {
            id, index, delta, ..
        } => match events.iter_mut().rev().find_map(|held| match held {
            Event::ToolArgs {
                id: held_id,
                index: held_index,
                delta: held_delta,
                ..
            } if held_id == id && held_index == index => Some(held_delta),
            _ => None,
        }) {
            Some(held) => held.push_str(delta),
            None => events.push(event.clone()),
        },
        Event::ToolCall { id, .. } => {
            events.retain(|held| !matches!(held, Event::ToolArgs { id: held_id, .. } if held_id == id));
            events.push(event.clone());
        }
        // Cursor state depends on the entire byte stream. Cutting an arbitrary
        // tail loses saved cursors, partial escapes and rewrites of older rows.
        // The tool's configured max_output_size already bounds this stream.
        // A resize starts a new segment: applying one geometry to both halves
        // would make a joining window draw differently from the watcher.
        Event::ToolOutput { id, chunk, rows, cols, .. } => {
            let held = events.iter_mut().rev().find(|held| {
                matches!(held, Event::ToolOutput { id: held_id, .. } if held_id == id)
            });
            match held {
                Some(Event::ToolOutput { chunk: held, rows: held_rows, cols: held_cols, .. })
                    if held_rows == rows && held_cols == cols => held.push_str(chunk),
                _ => events.push(event.clone()),
            }
        }
        // The attempt that streamed the trailing text failed and is repeated:
        // what it said is gone, as it is on every screen.
        Event::StreamReset { .. } => {
            while matches!(
                events.last(),
                Some(Event::Text { .. } | Event::Thinking { .. } | Event::ToolArgs { .. })
            ) {
                events.pop();
            }
            clear_retry(events);
        }
        // A wait for a reply to a request: kept while it is open.
        Event::Retrying { .. } => {
            clear_retry(events);
            events.push(event.clone());
        }
        Event::PermissionResolved { req, .. } => {
            events.retain(|held| !matches!(held, Event::PermissionRequest { req: held_req, .. } if held_req == req))
        }
        Event::QuestionsResolved { req, .. } => {
            events.retain(|held| !matches!(held, Event::QuestionsRequest { req: held_req, .. } if held_req == req))
        }
        Event::McpInputResolved { req, .. } => {
            events.retain(|held| !matches!(held, Event::McpInputRequest { req: held_req, .. } if held_req == req))
        }
        // What adds to the transcript, or to a counter the transcript's sidebar
        // adds up, and is said once.
        Event::ToolResult { id, .. } => {
            events.retain(|held| !matches!(held, Event::ToolOutput { id: held_id, .. } if held_id == id));
            events.push(event.clone());
        }
        Event::PermissionRequest { .. }
        | Event::QuestionsRequest { .. }
        | Event::McpInputRequest { .. }
        | Event::Context { .. }
        | Event::Usage { .. }
        | Event::AuxiliaryUsage { .. }
        | Event::Compacted { .. }
        | Event::Error { .. } => events.push(event.clone()),
        // State the view reads as it stands (title, mode, plan, files, the
        // composer, settings), or not about the transcript at all.
        _ => {}
    }
}

/// A retry banner stands until the stream speaks again.
fn clear_retry(events: &mut Vec<Event>) {
    events.retain(|held| !matches!(held, Event::Retrying { .. }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::StopReason;

    fn text(delta: &str) -> Event {
        Event::Text {
            session: 1,
            delta: delta.into(),
        }
    }

    fn turn(n: i64) -> Event {
        Event::TurnStart {
            session: 1,
            turn: n,
            text: Some("go".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        }
    }

    fn bus() -> (Bus, std::sync::mpsc::Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Bus::new(tx), rx)
    }

    #[test]
    fn a_run_starts_with_its_first_turn_and_ends_with_done() {
        let (bus, _rx) = bus();
        assert!(bus.run_of(1).is_none(), "nothing runs before a turn starts");
        bus.send(turn(7)).unwrap();
        bus.send(turn(9)).unwrap();
        let run = bus.run_of(1).unwrap();
        assert_eq!(run.first_turn, 7, "a second message joins the run");
        assert_eq!(run.events.len(), 2);
        bus.send(Event::Done {
            session: 1,
            turn: 9,
            stop: StopReason::End,
        })
        .unwrap();
        assert!(bus.run_of(1).is_none(), "its words are on disk now");
    }

    #[test]
    fn another_sessions_events_are_its_own_account() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        bus.send(Event::Text {
            session: 2,
            delta: "elsewhere".into(),
        })
        .unwrap();
        assert!(bus.run_of(2).is_none());
        assert_eq!(bus.run_of(1).unwrap().events.len(), 1);
    }

    #[test]
    fn speech_is_one_event_per_stretch_and_a_reset_takes_the_failed_attempt_away() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        for part in ["Hel", "lo ", "there"] {
            bus.send(text(part)).unwrap();
        }
        let run = bus.run_of(1).unwrap();
        assert_eq!(run.events.len(), 2);
        assert!(matches!(&run.events[1], Event::Text { delta, .. } if delta == "Hello there"));
        bus.send(Event::StreamReset { session: 1 }).unwrap();
        assert_eq!(
            bus.run_of(1).unwrap().events.len(),
            1,
            "the TurnStart stays"
        );
    }

    #[test]
    fn a_call_being_written_is_replaced_by_the_call() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        for delta in ["{\"pa", "th\":\"a\"}"] {
            bus.send(Event::ToolArgs {
                session: 1,
                id: "c1".into(),
                index: 0,
                name: "create_file".into(),
                delta: delta.into(),
            })
            .unwrap();
        }
        let run = bus.run_of(1).unwrap();
        assert!(
            matches!(&run.events[1], Event::ToolArgs { delta, .. } if delta == "{\"path\":\"a\"}")
        );
        bus.send(Event::ToolCall {
            session: 1,
            id: "c1".into(),
            name: "create_file".into(),
            action: "write a".into(),
            args: serde_json::json!({}),
            at: 0,
        })
        .unwrap();
        let events = bus.run_of(1).unwrap().events;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], Event::ToolCall { .. }));
    }

    #[test]
    fn a_question_that_was_answered_is_gone_and_one_that_waits_stays() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        let ask = |req: &str| Event::PermissionRequest {
            session: 1,
            req: req.into(),
            tool: "terminal_use".into(),
            action: "run".into(),
            preview: "$ ls".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        };
        bus.send(ask("p1")).unwrap();
        bus.send(ask("p2")).unwrap();
        bus.send(Event::PermissionResolved {
            session: 1,
            req: "p1".into(),
            decision: crate::proto::Decision::AllowOnce,
        })
        .unwrap();
        let events = bus.run_of(1).unwrap().events;
        assert_eq!(
            events.len(),
            2,
            "the TurnStart and the question still waiting"
        );
        assert!(matches!(&events[1], Event::PermissionRequest { req, .. } if req == "p2"));
    }

    #[test]
    fn a_long_output_keeps_its_cursor_state_in_one_geometry_segment() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        let line = "x".repeat(1024) + "\n";
        for _ in 0..100 {
            bus.send(Event::ToolOutput {
                session: 1,
                id: "c1".into(),
                chunk: line.clone(),
                rows: 40,
                cols: 200,
            })
            .unwrap();
        }
        let events = bus.run_of(1).unwrap().events;
        assert_eq!(events.len(), 2);
        let Event::ToolOutput { chunk, .. } = &events[1] else {
            panic!("an output event")
        };
        assert_eq!(chunk, &line.repeat(100));
        assert!(chunk.ends_with("\n"));
    }

    #[test]
    fn output_geometry_segments_replay_in_order_and_finished_streams_are_released() {
        let (bus, _rx) = bus();
        bus.send(turn(1)).unwrap();
        let chunks = [(10, 72, "old"), (12, 50, "\r\x1b[2Knew"), (10, 72, "\x08!")];
        let mut watcher = crate::terminal_screen::Screen::new(10, 72, 10000);
        for (rows, cols, chunk) in chunks {
            watcher.resize(rows, cols);
            watcher.feed(chunk);
            bus.send(Event::ToolOutput {
                session: 1,
                id: "c".into(),
                chunk: chunk.into(),
                rows,
                cols,
            })
            .unwrap();
        }
        let mut joiner = crate::terminal_screen::Screen::new(10, 72, 10000);
        for event in bus.run_of(1).unwrap().events {
            if let Event::ToolOutput {
                chunk, rows, cols, ..
            } = event
            {
                joiner.resize(rows, cols);
                joiner.feed(&chunk);
            }
        }
        assert_eq!(watcher.text(), joiner.text());
        bus.send(Event::ToolResult {
            session: 1,
            id: "c".into(),
            ok: true,
            summary: "ok".into(),
            content: watcher.text(),
            detail: None,
            at: 0,
        })
        .unwrap();
        assert!(!bus
            .run_of(1)
            .unwrap()
            .events
            .iter()
            .any(|event| matches!(event, Event::ToolOutput { .. })));
    }

    /// The latest word on a piece of state is what a view reads, never older.
    #[test]
    fn the_latest_word_on_state_is_what_a_view_reads() {
        let (bus, _rx) = bus();
        for title in ["First", "Second"] {
            bus.send(Event::Title {
                session: 1,
                title: title.into(),
            })
            .unwrap();
        }
        let mut seen = String::new();
        bus.answer(1, 1, |account| {
            if let Some(Event::Title { title, .. }) = account.last("title") {
                seen = title.clone();
            }
            Ok(Value::Null)
        });
        assert_eq!(seen, "Second");
        assert!(bus.run_of(1).is_none(), "state is not a run");
    }

    /// The answer is in the stream where the account stood: events sent before
    /// it are in the view, events after are not.
    #[test]
    fn an_answer_sits_in_the_stream_where_the_account_stood() {
        let (bus, rx) = bus();
        bus.send(turn(1)).unwrap();
        bus.send(text("before")).unwrap();
        bus.answer(5, 1, |account| {
            Ok(serde_json::json!({ "events": account.run.as_ref().map_or(0, |r| r.events.len()) }))
        });
        bus.send(text(" after")).unwrap();
        let stream: Vec<Event> = rx.try_iter().collect();
        assert_eq!(stream.len(), 4);
        let Event::Ack { id, data, .. } = &stream[2] else {
            panic!("the answer is third: {stream:?}")
        };
        assert_eq!(*id, 5);
        assert_eq!(
            data.as_ref().unwrap()["events"],
            2,
            "the view held TurnStart and 'before'"
        );
        assert!(matches!(&stream[3], Event::Text { delta, .. } if delta == " after"));
    }
}
