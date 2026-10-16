//! The one answer to "what is this session showing?".
//!
//! A frontend that opens a session — or finds its connection to the session's
//! host gone and opens it again — asks once and is told everything a screen is
//! made of, as the engine holds it at one definite place in the event stream
//! (see [`super::live`]): the conversation, the audit of every call, the
//! counters, the plan, the file tally, the composer with the messages waiting
//! in it, the settings, the mode and what the classifier is doing, the requests
//! that wait for an answer — and the events of the turn in flight, merged.
//!
//! Everything that is *history* is read as of before the running turn began,
//! because that turn's own rows are in the events and counting them in both
//! places is the bug this arrangement exists to make impossible. Everything that
//! is *state* is the latest event the engine sent about it, so the view is never
//! a moment behind the stream it joins.

use serde_json::{json, Value};

use super::live::Account;
use super::SessionState;
use crate::error::Result;
use crate::proto::Event;
use crate::store::Db;

/// What is worked out before the stream's lock is taken, because working it out
/// needs the database or the file system. It is only the fallback: any newer
/// word the stream has said about the same thing wins.
pub struct Standing {
    pub settings: Value,
    pub file_changes: Value,
    pub mode: String,
    pub classifier: Value,
}

/// The view, read from `db` (held by the caller, before the stream's lock) and
/// from the account of the session, under the stream's lock.
pub fn build(db: &Db, st: &SessionState, account: &Account, standing: Standing) -> Result<Value> {
    let first = account.run.as_ref().map(|run| run.first_turn);
    let mut view = from_store(db, st, first, standing)?;
    overlay(&mut view, account)?;
    Ok(view)
}

/// What the stores hold, with history as of before turn `first` (everything
/// when no turn runs).
pub fn from_store(
    db: &Db,
    st: &SessionState,
    first: Option<i64>,
    standing: Standing,
) -> Result<Value> {
    let session = st.id;
    let earlier = |turn: i64| first.is_none_or(|first| turn < first);
    let rows = db.history_shared(session)?;
    let history: Vec<_> = rows.iter().filter(|m| earlier(m.turn)).collect();
    let toolcalls: Vec<_> = db
        .jsonl
        .read_toolcalls(session)?
        .into_iter()
        .filter(|call| earlier(call.turn))
        .collect();
    let row = db.get_session(session)?;
    Ok(json!({
        "session": session,
        "version": crate::VERSION,
        "protocol": crate::proto::PROTOCOL,
        "project": st.project,
        "mode": standing.mode,
        "classifier": standing.classifier,
        "title": row.title,
        "created_at": row.created_at,
        // As of before the running turn: the window adds that turn's time from
        // the moment it began (`live`'s first event says when).
        "active_seconds": row.active_seconds,
        "settings": standing.settings,
        "usage": db.session_usage_before(session, first)?,
        "tasks": db.tasks(session)?,
        "file_changes": standing.file_changes,
        "composer": db.jsonl.composer(session)?,
        "history": history,
        "toolcalls": toolcalls,
        "live": { "first_turn": null, "events": [] },
    }))
}

/// Lay what the stream has said over a view read from the stores: the latest
/// word on each piece of state, and the turn in flight. Pure, so a test can
/// build the view a frontend would have been given at any point of a recorded
/// stream.
pub fn overlay(view: &mut Value, account: &Account) -> Result<()> {
    if let Some(Event::Title { title, .. }) = account.last("title") {
        view["title"] = json!(title);
    }
    if let Some(Event::Tasks { tasks, .. }) = account.last("tasks") {
        view["tasks"] = serde_json::to_value(tasks)?;
    }
    if let Some(Event::FileChanges { changes, .. }) = account.last("file_changes") {
        view["file_changes"] = serde_json::to_value(changes)?;
    }
    if let Some(Event::Session { mode, .. }) = account.last("session") {
        view["mode"] = json!(mode);
    }
    if let Some(Event::Settings { settings, .. }) = account.last("settings") {
        view["settings"] = settings.clone();
    }
    if let Some(classifier) = account.classifier() {
        view["classifier"] = classifier.clone();
    }
    if !view["classifier"].is_null() {
        view["settings"]["classifier_status"] = view["classifier"].clone();
    }
    if let Some(Event::Composer { composer, .. }) = account.last("composer") {
        view["composer"] = serde_json::to_value(composer)?;
    }
    view["live"] = match &account.run {
        Some(run) => json!({
            "first_turn": run.first_turn,
            "events": serde_json::to_value(&run.events)?,
        }),
        None => json!({
            "first_turn": null,
            "events": serde_json::to_value(&account.waiting)?,
        }),
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::live::Bus;

    fn session(classifier: Value) -> Event {
        Event::Session {
            session: 1,
            project: "/p".into(),
            mode: "full".into(),
            confined: false,
            sandbox: crate::proto::SandboxReport {
                kind: "none".into(),
                abi: 0,
                enforced: false,
                reason: None,
                escapes: Vec::new(),
            },
            classifier,
        }
    }

    #[test]
    fn classifier_facts_in_a_view_follow_stream_order_across_event_kinds() {
        let old = json!({"enabled":true,"policy":true,"active":true,"reason":"old"});
        let new = json!({"enabled":true,"policy":false,"active":false,"reason":"new"});
        for order in [
            vec![
                session(old.clone()),
                Event::Settings {
                    session: 1,
                    settings: json!({"classifier_status":new}),
                },
            ],
            vec![
                Event::Settings {
                    session: 1,
                    settings: json!({"classifier_status":old}),
                },
                session(new.clone()),
            ],
        ] {
            let (tx, _rx) = std::sync::mpsc::channel();
            let bus = Bus::new(tx);
            for event in order {
                bus.send(event).unwrap();
            }
            // A later model/knob update is not a classifier announcement.
            bus.send(Event::Settings {
                session: 1,
                settings: json!({"settings":{"model":"test"}}),
            })
            .unwrap();
            let mut view = json!({"classifier":old,"settings":{},"mode":"ask"});
            overlay(&mut view, &bus.account_of(1)).unwrap();
            assert_eq!(view["classifier"], new);
            assert_eq!(view["settings"]["classifier_status"], new);
        }
    }
}
