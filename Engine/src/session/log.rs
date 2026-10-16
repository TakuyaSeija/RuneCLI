//! The session's log: what happened behind the conversation.
//!
//! Every event a user may want to know about and the turn does not depend on —
//! the context Rune told the model, a status update, a retry, a cache miss, a
//! compaction, a title that could not be made, what a request cost — is written
//! here once: appended to the session's `logs.jsonl` and sent to the frontend as
//! `Event::Log`. The conversation itself only ever shows the conversation.
//!
//! A log line is shown to the user and kept on disk, so its message passes
//! [`crate::net::redact`] first: a provider's error can quote a key.

use crate::proto::{Event, LogEntry, LogLevel};

use super::Runtime;

/// Write one line to `session`'s log and tell the frontend.
pub fn record(
    rt: &Runtime,
    session: i64,
    level: LogLevel,
    source: &str,
    code: &str,
    message: impl Into<String>,
    detail: Option<serde_json::Value>,
) {
    let message = message.into();
    let entry = LogEntry {
        at: crate::store::now_ms(),
        level,
        source: source.into(),
        code: code.into(),
        message: crate::net::redact::scrub(&message).into_owned(),
        turn: None,
        detail,
    };
    // A log that cannot be written is still shown: losing the line on disk is
    // no reason to also hide it from the person watching.
    let _ = crate::panic::lock(&rt.db).jsonl.append_log(session, &entry);
    let _ = rt.events.send(Event::Log {
        session: Some(session),
        entry,
    });
}

pub fn info(rt: &Runtime, session: i64, source: &str, code: &str, message: impl Into<String>) {
    record(rt, session, LogLevel::Info, source, code, message, None);
}

pub fn warn(rt: &Runtime, session: i64, source: &str, code: &str, message: impl Into<String>) {
    record(rt, session, LogLevel::Warn, source, code, message, None);
}

/// One request's cost: what the provider counted, and what it charged when the
/// price is known. One line per request, agent or auxiliary alike, so the log
/// adds up to the bill.
#[allow(clippy::too_many_arguments)]
pub fn usage(
    rt: &Runtime,
    session: i64,
    role: &str,
    model: &str,
    input: u64,
    output: u64,
    cached: u64,
    cost: Option<f64>,
) {
    let human = |n: u64| crate::media::human_num(n as usize);
    let cached = if cached > 0 {
        format!(" ({} from cache)", human(cached))
    } else {
        String::new()
    };
    let cost_text = cost.map(|c| format!(", ${c:.4}")).unwrap_or_default();
    record(
        rt,
        session,
        LogLevel::Info,
        "usage",
        role,
        format!(
            "{role} request on {model}: {} tokens in{cached}, {} out{cost_text}.",
            human(input),
            human(output)
        ),
        Some(serde_json::json!({"input": input, "output": output, "cost": cost})),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A provider's error can quote a key; the log line on disk and on screen
    /// carries the redaction, never the key.
    #[test]
    fn a_secret_in_a_log_line_reaches_neither_disk_nor_screen() {
        let settings = Arc::new(Mutex::new(crate::settings::Settings::empty()));
        let db = Arc::new(Mutex::new(crate::store::Db::memory().unwrap()));
        let session = crate::panic::lock(&db)
            .create_session("/fixture")
            .unwrap()
            .id;
        let limits = Arc::new(crate::limits::Limits::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = Runtime {
            mcp: crate::capabilities::mcp::Runtime::new().unwrap(),
            registry: Arc::new(crate::tools::Registry::read_only(
                limits.clone(),
                settings.clone(),
            )),
            limits,
            settings,
            db: Arc::clone(&db),
            provider: Arc::new(crate::provider::scripted::Scripted::new(Vec::new())),
            events: crate::session::live::Bus::new(tx),
        };
        crate::net::redact::register("sk-log-test-secret-0123456789");
        warn(
            &rt,
            session,
            "retry",
            "retrying",
            "401 from provider: invalid key sk-log-test-secret-0123456789",
        );
        let Ok(Event::Log { entry, .. }) = rx.try_recv() else {
            panic!("the line was not told to the frontend");
        };
        assert!(
            !entry.message.contains("sk-log-test-secret"),
            "{}",
            entry.message
        );
        let stored = crate::panic::lock(&db)
            .jsonl
            .read_logs(session, 10)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert!(!stored[0].message.contains("sk-log-test-secret"));
        assert_eq!(stored[0].level, LogLevel::Warn);
    }
}
