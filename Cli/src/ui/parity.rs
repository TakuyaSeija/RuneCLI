//! Two windows on one session must show the same.
//!
//! A window that has watched a turn from its first word, and one that opened
//! while the turn was half done and was handed the engine's view, are the two
//! screens of one session. Whatever one shows and the other does not is a bug of
//! the kind that cost the attachments, the timers, the waiting questions and the
//! queued messages of an earlier release — each forgotten by a different path.
//!
//! The test below does not look for those by name. It takes a recorded stream
//! of everything the engine can say in one run and, at **every** point of it,
//! builds the view the engine would have given a window joining right there
//! (`session::view::overlay` over the same reducer the engine runs), applies it
//! to a fresh window, and demands that window equal the one that watched — both
//! at that instant and after the two have heard the rest of the stream. A field
//! a view forgets, or a handler that treats a replayed event differently from a
//! live one, fails here at the first point it matters.

use super::*;
use runeengine::proto::{Decision, StopReason};
use runeengine::session::live::Bus;
use runeengine::store::composer::{Composer, PendingPrompt};
use serde_json::json;

const SESSION: i64 = 1;

/// Everything a person could tell apart on a screen, without the clocks.
fn signature(app: &App) -> Vec<String> {
    let mut out = Vec::new();
    for block in &app.blocks {
        out.push(match block {
            Block::User { text, media, .. } => format!(
                "user|{text}|{}",
                media
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Block::Text { text } => format!("text|{text}"),
            Block::Thinking { text, .. } => format!("thinking|{text}"),
            Block::Tool(t) => format!(
                "tool|{}|{}|{}|{:?}|{}|{}|{}",
                t.id,
                t.name,
                t.action,
                t.state,
                t.summary,
                t.content,
                // Whether it ended in a way the engine labelled, not the numbers
                // the live event carries beside it.
                t.detail
                    .as_ref()
                    .and_then(|d| d["status"].as_str())
                    .unwrap_or_default()
            ),
            Block::Notice { text, bad } => format!("notice|{bad}|{text}"),
            Block::Notification { text, .. } => format!("notification|{text}"),
            Block::Divider { text } => format!("divider|{text}"),
        });
    }
    // And what is drawn: the transcript as a person reads it, without what
    // the clock decides — the times of the user's rules, and how long a step
    // has been running, which each window counts from the moment it saw the
    // step begin (a stamp this stream gives as 0): a window that joined a
    // second later, or replayed 15 000 rows a second slower, is not wrong.
    let drawn = super::draw::transcript_into(None, app, 100, true, true).sink;
    let clock = |line: &str| {
        let chars: Vec<char> = ansi::plain(line).chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let digit = |k: usize| chars.get(k).is_some_and(char::is_ascii_digit);
            if digit(i)
                && digit(i + 1)
                && chars.get(i + 2) == Some(&':')
                && digit(i + 3)
                && digit(i + 4)
            {
                out.push_str("hh:mm");
                i += 5;
                continue;
            }
            // `7s`, `1m 05s`, `2h 01m`: every part of a duration.
            if digit(i) && (i == 0 || !chars[i - 1].is_alphanumeric()) {
                let mut j = i;
                while digit(j) {
                    j += 1;
                }
                if matches!(chars.get(j), Some('s' | 'm' | 'h'))
                    && chars.get(j + 1).is_none_or(|c| !c.is_alphanumeric())
                {
                    out.push_str("<t>");
                    i = j + 1;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    };
    out.push(format!(
        "drawn|{}",
        drawn
            .lines
            .iter()
            .map(|l| clock(l))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    out.push(format!("busy={} label={:?}", app.busy, app.label));
    out.push(format!("retry={:?}", app.retry_status));
    out.push(format!(
        "ask={:?} questions={} mcp={}",
        app.pending.as_ref().map(|a| a.req.clone()),
        app.questions
            .as_ref()
            .map(|q| q.req.clone())
            .unwrap_or_default(),
        app.mcp_input.is_some()
    ));
    out.push(format!(
        "usage in={} out={} cache={} used={} cost={:.4} unpriced={}",
        app.tok_in, app.tok_out, app.cache_in, app.context_used, app.cost, app.unpriced_calls
    ));
    out.push(format!(
        "counts steps={} turns={} title={:?} mode={}",
        app.steps, app.turns, app.title, app.mode
    ));
    out.push(format!(
        "tasks={}",
        serde_json::Value::Array(app.tasks.clone())
    ));
    out.push(format!(
        "queued={}",
        app.queued_prompt
            .as_ref()
            .map(|q| q
                .steers
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
                .join("|"))
            .unwrap_or_default()
    ));
    out.push(format!("classifier={}", app.settings["classifier_status"]));
    out
}

fn window() -> App {
    let mut app = App::for_test();
    app.session = Some(SESSION);
    app
}

/// The view the engine gives a window at a moment when nothing is running:
/// one finished exchange in history.
fn base_view() -> Value {
    json!({
        "session": SESSION,
        "mode": "ask",
        "classifier": {"enabled": true, "policy": false, "active": false, "reason": "Disabled in Ask mode"},
        "title": null,
        "created_at": 500,
        "active_seconds": 12,
        "settings": null,
        "usage": {"unpriced_calls": 0, "used": 90, "tok_in": 90, "tok_out": 9, "cache_in": 0, "spend": 0.5},
        "tasks": [],
        "file_changes": [],
        "composer": {},
        "history": [
            {"id": 1, "turn": 1, "role": "user", "content": "hello", "created_at": 1000, "media": []},
            {"id": 2, "turn": 1, "role": "assistant", "content": "hi there", "created_at": 1001, "media": []}
        ],
        "toolcalls": [],
        "live": {"first_turn": null, "events": []}
    })
}

/// Every kind of thing a run can say, in an order a real run could say it:
/// a message with a file, thinking and text in pieces, a call written and made,
/// a permission asked and answered, a command's output, a failed attempt and its
/// repeat, a question left waiting, a message queued by another window and then
/// taken, usage, an error, a notice of compaction, the end.
fn stream() -> Vec<Event> {
    let s = SESSION;
    let file = runeengine::media::Attachment {
        name: "shot.png".into(),
        mime: "image/png".into(),
        path: "/tmp/shot.png".into(),
        size: 10,
        ..Default::default()
    };
    let ask = |req: &str| Event::PermissionRequest {
        session: s,
        req: req.into(),
        tool: "terminal_use".into(),
        action: "run it".into(),
        preview: "$ echo hi".into(),
        remember_as: None,
        reach: Vec::new(),
        local_services: false,
        full_access: false,
        reason: None,
    };
    let questions = serde_json::from_value(json!([{
        "id": "q0", "title": "Which?", "question": "Choose one.", "type": "single",
        "options": [{"id": "q0o0", "label": "A", "description": "a."}, {"id": "q0o1", "label": "B", "description": "b."}]
    }]))
    .unwrap();
    let composer = |pending: Vec<PendingPrompt>| Event::Composer {
        session: s,
        request: 0,
        composer: Box::new(Composer {
            pending,
            ..Default::default()
        }),
    };
    let waiting = PendingPrompt {
        id: 5,
        message_id: None,
        text: "also check b".into(),
        media: Vec::new(),
    };
    vec![
        Event::TurnStart {
            session: s,
            turn: 2,
            text: Some("build it".into()),
            notification: false,
            media: vec![file],
            at: 10_000,
        },
        Event::Title {
            session: s,
            title: "Build it".into(),
        },
        Event::Thinking { session: s, delta: "plan ".into() },
        Event::Thinking { session: s, delta: "the work".into() },
        Event::Text { session: s, delta: "Let me ".into() },
        Event::Text { session: s, delta: "look.".into() },
        Event::ToolArgs {
            session: s,
            id: "c1".into(),
            index: 0,
            name: "inspect_file".into(),
            delta: "{\"pa".into(),
        },
        Event::ToolArgs {
            session: s,
            id: "c1".into(),
            index: 0,
            name: "inspect_file".into(),
            delta: "th\":\"a.rs\"}".into(),
        },
        Event::ToolCall {
            session: s,
            id: "c1".into(),
            name: "inspect_file".into(),
            action: "read a".into(),
            args: json!({"action": "read a", "path": "a.rs"}),
            at: 11_000,
        },
        Event::ToolResult {
            session: s,
            id: "c1".into(),
            ok: true,
            summary: "read".into(),
            content: "=== a.rs (1 lines) ===\n     1| x\n".into(),
            detail: None,
            at: 11_100,
        },
        Event::Usage {
            session: s,
            input: 100,
            output: 10,
            cached: 20,
            cost: Some(0.01),
            details: Default::default(),
        },
        Event::ToolCall {
            session: s,
            id: "c2".into(),
            name: "terminal_use".into(),
            action: "run it".into(),
            args: json!({"action": "run it", "command": "echo hi"}),
            at: 12_000,
        },
        ask("p1"),
        composer(vec![waiting.clone()]),
        Event::PermissionResolved {
            session: s,
            req: "p1".into(),
            decision: Decision::AllowOnce,
        },
        Event::ToolOutput {
            session: s,
            id: "c2".into(),
            chunk: "line1\r\n".into(),
            rows: 10,
            cols: 80,
        },
        Event::ToolOutput {
            session: s,
            id: "c2".into(),
            chunk: "line2\r\n".into(),
            rows: 10,
            cols: 80,
        },
        Event::ToolResult {
            session: s,
            id: "c2".into(),
            ok: true,
            summary: "exit 1".into(),
            content: "<tool_notice>Exit code 1</tool_notice>\n\nline1\nline2".into(),
            detail: Some(json!({"status": null, "exit_code": 1})),
            at: 12_500,
        },
        Event::Retrying {
            session: s,
            attempt: 1,
            of: 5,
            wait_ms: 3000,
            rotated: false,
            reason: "overloaded".into(),
        },
        Event::Text { session: s, delta: "partial answer".into() },
        Event::StreamReset { session: s },
        Event::Text { session: s, delta: "Now b.".into() },
        // The engine stores the message and says so, then rewrites the composer
        // without it — in that order, inside one step (`persist_prompt`).
        Event::TurnStart {
            session: s,
            turn: 3,
            text: Some("also check b".into()),
            notification: false,
            media: Vec::new(),
            at: 13_000,
        },
        composer(Vec::new()),
        Event::ToolCall {
            session: s,
            id: "c3".into(),
            name: "ask_user_questions".into(),
            action: "ask".into(),
            args: json!({"action": "ask", "questions": []}),
            at: 13_100,
        },
        Event::QuestionsRequest {
            session: s,
            req: "q1".into(),
            id: "c3".into(),
            action: "ask".into(),
            questions,
        },
        Event::QuestionsResolved {
            session: s,
            req: "q1".into(),
        },
        Event::ToolResult {
            session: s,
            id: "c3".into(),
            ok: false,
            summary: "stopped by the user".into(),
            content: "<tool_notice>The user stopped this call; it may not have finished, so do not assume it did.</tool_notice>".into(),
            detail: Some(json!({"status": "cancelled"})),
            at: 13_200,
        },
        Event::AuxiliaryUsage {
            session: s,
            role: "title".into(),
            input: 5,
            output: 2,
            cached: 0,
            cost: Some(0.001),
        },
        Event::Usage {
            session: s,
            input: 150,
            output: 20,
            cached: 100,
            cost: Some(0.02),
            details: Default::default(),
        },
        Event::Tasks {
            session: s,
            tasks: Vec::new(),
        },
        Event::Session {
            session: s,
            project: "/p".into(),
            mode: "full".into(),
            confined: false,
            sandbox: runeengine::proto::SandboxReport {
                kind: "none".into(),
                abi: 0,
                enforced: false,
                reason: None,
                escapes: Vec::new(),
            },
            classifier: json!({"enabled": true, "policy": false, "active": false, "reason": "Inactive: no CLASSIFIER.md"}),
        },
        Event::Error {
            session: Some(s),
            code: "provider".into(),
            message: "the provider said no".into(),
        },
        Event::Done {
            session: s,
            turn: 3,
            stop: StopReason::End,
        },
    ]
}

/// `assert_eq!` that says WHICH line differs rather than printing two screens.
fn assert_same(joined: &[String], watched: &[String], context: &str) {
    if joined == watched {
        return;
    }
    for (a, b) in joined.iter().zip(watched.iter()) {
        if a != b {
            let (al, bl): (Vec<_>, Vec<_>) = (a.lines().collect(), b.lines().collect());
            let at = al
                .iter()
                .zip(bl.iter())
                .position(|(x, y)| x != y)
                .unwrap_or(al.len().min(bl.len()));
            panic!(
                "{context}\n  joined : {:?}\n  watched: {:?}",
                al.get(at).copied().unwrap_or("<nothing>"),
                bl.get(at).copied().unwrap_or("<nothing>")
            );
        }
    }
    panic!(
        "{context}: {} items against {}",
        joined.len(),
        watched.len()
    );
}

/// Join at every point of the stream; the joiner and the watcher must agree.
fn assert_parity(base: &Value, events: &[Event]) {
    let mut watcher = window();
    watcher.apply_view(base);
    let mut watched = vec![signature(&watcher)];
    for event in events {
        watcher.on_event(event.clone());
        watched.push(signature(&watcher));
    }
    // Every point while the run is in flight. (After `done` the run is on disk
    // and a view carries it as history; the engine's own test of that —
    // `a_view_taken_during_a_turn…` and `the_view_of_a_finished_run…` — reads it
    // from a real store.)
    for cut in 0..events.len() {
        let bus = Bus::detached();
        for event in &events[..cut] {
            let _ = bus.send(event.clone());
        }
        let mut view = base.clone();
        runeengine::session::view::overlay(&mut view, &bus.account_of(SESSION)).unwrap();
        let mut joiner = window();
        joiner.apply_view(&view);
        let last = cut
            .checked_sub(1)
            .map(|i| format!("{:?}", events[i]))
            .unwrap_or_default();
        let mut now = signature(&joiner);
        assert_same(
            &now,
            &watched[cut],
            &format!("a window that joined after {cut} events (the last: {last}) does not show what one that watched shows"),
        );
        for event in &events[cut..] {
            joiner.on_event(event.clone());
        }
        now = signature(&joiner);
        assert_same(
            &now,
            watched.last().unwrap(),
            &format!("joined after {cut} events and heard the rest, it differs from the one that watched"),
        );
    }
}

#[test]
fn a_window_that_joins_at_any_point_shows_what_one_that_watched_shows() {
    assert_parity(&base_view(), &stream());
}

#[test]
fn a_joining_window_replays_large_terminal_streams_and_resize_segments_without_losing_cursor_state()
{
    let session = SESSION;
    let mut events = vec![
        Event::TurnStart {
            session,
            turn: 2,
            text: Some("run".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        },
        Event::ToolCall {
            session,
            id: "vt".into(),
            name: "terminal_use".into(),
            action: "render progress".into(),
            args: json!({"command":"build"}),
            at: 0,
        },
    ];
    for (rows, cols, chunk) in [
        (10, 72, "row\r\n".repeat(15000)),
        (10, 72, "⠋\x08⠙\x1b[".into()),
        (10, 72, "2K\rready".into()),
        (8, 50, "\r\x1b[2Kresized".into()),
        (10, 72, "\r\x1b[2Kfinished".into()),
    ] {
        events.push(Event::ToolOutput {
            session,
            id: "vt".into(),
            chunk,
            rows,
            cols,
        });
    }
    assert_parity(&base_view(), &events);
}

#[test]
fn what_the_stream_carries_is_what_is_shown() {
    // A guard on the guard: the recorded stream really exercises what the
    // comparison is about, so a signature that went blind would be noticed.
    let mut watcher = window();
    watcher.apply_view(&base_view());
    let before_run = watcher.blocks.len();
    let events = stream();
    for event in &events[..14] {
        watcher.on_event(event.clone());
    }
    let sig = signature(&watcher).join("\n");
    assert!(
        sig.contains("user|build it|shot.png"),
        "the attachment is shown:\n{sig}"
    );
    assert!(sig.contains("tool|c1|inspect_file"), "{sig}");
    assert!(
        sig.contains("ask=Some(\"p1\")"),
        "the waiting question is shown:\n{sig}"
    );
    assert!(
        sig.contains("queued=also check b"),
        "another window's message is shown:\n{sig}"
    );
    assert!(watcher.blocks.len() > before_run);
}

/// The same demand against a real engine and a real store: a window that
/// watched a whole run, and one that opens the session after it, show the same —
/// the run having gone from the events to the history on disk and nothing having
/// changed on the way.
#[test]
fn a_window_that_opens_after_a_real_run_shows_what_one_that_watched_it_shows() {
    let base_dir = std::env::temp_dir().join(format!("rune-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    let project = base_dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(project.join("note.txt"), "remember this").unwrap();
    let fixture = base_dir.join("fixture.json");
    std::fs::write(
        &fixture,
        json!({ "responses": [
            { "chunks": [
                { "thinking": "I should read it" },
                { "text": "Looking. " },
                { "tool_call": { "id": "t1", "name": "inspect_file",
                                 "args": { "action": "read a", "path": "a.txt" } } },
                { "tool_call": { "id": "t2", "name": "terminal_use",
                                 "args": { "action": "list it", "command": "ls" } } },
                { "usage": { "input": 120, "output": 15 } }
            ]},
            { "chunks": [ { "text": "It has two lines." }, { "usage": { "input": 200, "output": 8 } } ] }
        ]})
        .to_string(),
    )
    .unwrap();

    let mut d = Driver::start(crate::driver::Options {
        db: Some(base_dir.join("rune.db")),
        read_only: false,
        script: Some(fixture),
        request_next: None,
    })
    .expect("the engine starts");
    let canonical = runeengine::platform::canonical(&project).unwrap();
    let opened = d
        .call(|id| Command::SessionOpen {
            id,
            project: canonical.to_string_lossy().into_owned(),
            fresh: true,
        })
        .unwrap();
    let session = opened["session"].as_i64().unwrap();
    d.call(|id| Command::SessionMode {
        id,
        session,
        mode: "full".into(),
    })
    .unwrap();
    d.pending_events.clear();
    let base = d.view(session).unwrap();

    let mut watcher = window();
    watcher.session = Some(session);
    watcher.apply_view(&base);

    d.handle(Command::Prompt {
        id: 90,
        session,
        text: "read a.txt".into(),
        attach: vec![canonical.join("note.txt").to_string_lossy().into_owned()],
    });
    loop {
        let event = d
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the run finishes");
        let done = matches!(event, Event::Done { .. });
        if event.session() == Some(session) {
            watcher.on_event(event);
        }
        if done {
            break;
        }
    }

    let after = d.view(session).unwrap();
    assert!(
        after["live"]["events"].as_array().unwrap().is_empty(),
        "nothing is running"
    );
    let mut opened_after = window();
    opened_after.session = Some(session);
    opened_after.apply_view(&after);
    let (watched, joined) = (signature(&watcher), signature(&opened_after));
    assert_eq!(
        watched, joined,
        "the window that watched the run and the one that opened the session after it must agree"
    );
    let text = watched.join("\n");
    assert!(
        text.contains("user|read a.txt|note.txt"),
        "the attachment is on both:\n{text}"
    );
    assert!(text.contains("tool|t1|inspect_file"), "{text}");
    assert!(text.contains("thinking|I should read it"), "{text}");
    assert!(text.contains("text|It has two lines."), "{text}");
    d.shutdown();
    let _ = std::fs::remove_dir_all(&base_dir);
}

/// A turn Rune opens itself (a background task ended), a command left running in
/// the background, and an MCP server asking the user for input.
/// A background task that ends while a command runs is shown at once and
/// handed to the model at the next request without a second box; a status
/// update Rune told the model is shown where it was told. A window that joins
/// at any point shows the same.
#[test]
fn a_notification_shown_at_once_and_a_status_update_are_shown_alike() {
    let s = SESSION;
    let wake = "<system_notification>Background task bg_9 finished: Completed. Call manage_process with process_id: bg_9 for its result.</system_notification>";
    let events = vec![
        Event::TurnStart {
            session: s,
            turn: 2,
            text: Some("run the tests".into()),
            notification: false,
            media: Vec::new(),
            at: 30_000,
        },
        Event::ToolCall {
            session: s,
            id: "t1".into(),
            name: "terminal_use".into(),
            action: "run the tests".into(),
            args: json!({"action": "run the tests", "command": "cargo test"}),
            at: 30_100,
        },
        Event::Notification {
            session: s,
            text: wake.into(),
            at: 30_200,
        },
        Event::ToolResult {
            session: s,
            id: "t1".into(),
            ok: true,
            summary: "command ok".into(),
            content: "test result: ok".into(),
            detail: Some(json!({"status": "success", "exit_code": 0})),
            at: 30_300,
        },
        Event::Context {
            session: s,
            turn: 2,
            kind: "status".into(),
            note: "model other-model (was test)".into(),
            at: 30_350,
        },
        Event::TurnStart {
            session: s,
            turn: 3,
            text: Some(wake.into()),
            notification: true,
            media: Vec::new(),
            at: 30_400,
        },
        Event::Text {
            session: s,
            delta: "The background tests passed.".into(),
        },
    ];
    assert_parity(&base_view(), &events);
    // Alike, and right: one box for the one notification, none for the update.
    let mut watcher = window();
    watcher.apply_view(&base_view());
    for event in &events {
        watcher.on_event(event.clone());
    }
    let notifications = watcher
        .blocks
        .iter()
        .filter(|b| matches!(b, Block::Notification { .. }))
        .count();
    assert_eq!(
        notifications, 1,
        "the handed-over notification was drawn twice"
    );
    assert!(
        !signature(&watcher).join("\n").contains("other-model"),
        "a status update is told to the model, not drawn in the conversation"
    );
}

#[test]
fn write_titles_and_patch_failure_details_survive_finished_history_replay() {
    let cases = [
        (
            "create_file",
            json!({"action":"write","path":"new.rs","text":"one\ntwo"}),
            true,
            json!({"added":2,"removed":0,"overwritten":false}),
            "+2",
        ),
        (
            "create_file",
            json!({"action":"write","path":"old.rs","text":"one\ntwo"}),
            true,
            json!({"added":2,"removed":1919,"overwritten":true}),
            "+2",
        ),
        (
            "create_file",
            json!({"action":"write","path":"empty.rs","text":""}),
            true,
            json!({"added":0,"removed":30,"overwritten":true}),
            "+0",
        ),
        (
            "create_file",
            json!({"action":"write","path":"stop.rs","text":"one\ntwo"}),
            false,
            json!({"status":"cancelled","code":"cancelled"}),
            "+0",
        ),
        (
            "patch_file",
            json!({"action":"patch","path":"a.rs","old_str":"a\nb","new_str":"c"}),
            true,
            json!({"removed":2,"added":1}),
            "−2 +1",
        ),
        (
            "patch_file",
            json!({"action":"patch","path":"a.rs","old_str":"a\nb","new_str":"c"}),
            false,
            json!({"status":"failed","code":"tool"}),
            "−2 +1",
        ),
        (
            "patch_file",
            json!({"action":"patch","path":"a.rs","old_str":"a\nb","new_str":"c"}),
            false,
            json!({"status":"not_executed","code":"invalid"}),
            "−0 +0",
        ),
        (
            "patch_file",
            json!({"action":"patch","path":"a.rs","old_str":"a\nb","new_str":"c"}),
            false,
            json!({"status":"cancelled","code":"cancelled"}),
            "−0 +0",
        ),
    ];
    for (name, args, ok, detail, expected) in cases {
        let mut live = window();
        live.on_event(Event::ToolCall {
            session: SESSION,
            id: "c".into(),
            name: name.into(),
            action: args["action"].as_str().unwrap().into(),
            args: args.clone(),
            at: 1000,
        });
        live.on_event(Event::ToolResult {
            session: SESSION,
            id: "c".into(),
            ok,
            summary: "result".into(),
            content: "result".into(),
            detail: Some(detail.clone()),
            at: 1100,
        });
        let mut replay = window();
        replay.load_history(&[
            json!({"role":"assistant","content":"","turn":1,"tool_calls":[{"id":"c","name":name,"args":args}]}),
            json!({"role":"tool","content":"result","turn":1,"tool_call_id":"c"}),
        ], &[
            json!({"id":"c","turn":1,"ok":ok,"summary":"result","status":detail["status"],"code":detail["code"],"started_at":1000,"ended_at":1100}),
        ], &json!([]));
        for app in [&mut live, &mut replay] {
            let tool = app.tool_by_id("c").unwrap();
            assert_eq!(steps::bullets(tool)[0].note, expected, "{name}: {detail}");
            tool.open = true;
        }
        // Opened/closed rendering must agree too, not only the counters.
        let draw = |app: &App| {
            super::draw::transcript_into(None, app, 100, true, true)
                .sink
                .lines
                .iter()
                .map(|l| ansi::plain(l))
                .collect::<Vec<_>>()
        };
        assert_eq!(draw(&live), draw(&replay), "{name}: {detail}");
    }
}

/// A notification raised while no turn runs (a direct tool call held the
/// session) waits in the view for the turn that hands it over.
#[test]
fn a_notification_raised_between_turns_waits_for_the_turn_that_reads_it() {
    let s = SESSION;
    let wake = "<system_notification>Background task bg_8 finished: Completed. Call manage_process with process_id: bg_8 for its result.</system_notification>";
    let events = vec![
        Event::Notification {
            session: s,
            text: wake.into(),
            at: 40_000,
        },
        Event::TurnStart {
            session: s,
            turn: 2,
            text: Some(wake.into()),
            notification: true,
            media: Vec::new(),
            at: 40_100,
        },
        Event::Text {
            session: s,
            delta: "Noted.".into(),
        },
    ];
    assert_parity(&base_view(), &events);
}

#[test]
fn a_notification_a_background_command_and_an_mcp_request_are_shown_alike() {
    let s = SESSION;
    let events = vec![
        Event::TurnStart {
            session: s,
            turn: 2,
            text: Some("<system_notification>Background task bg_1 finished.</system_notification>".into()),
            notification: true,
            media: Vec::new(),
            at: 20_000,
        },
        Event::ToolCall {
            session: s,
            id: "b1".into(),
            name: "terminal_use".into(),
            action: "build".into(),
            args: json!({"action": "build", "command": "cargo build", "run_in_background": true}),
            at: 20_100,
        },
        Event::ToolResult {
            session: s,
            id: "b1".into(),
            ok: true,
            summary: "started in background".into(),
            content: "<tool_notice>Running in the background as bg_2 (PID 1); output is being written to /tmp/x.txt. You will receive a <system_notification> when it ends.</tool_notice>".into(),
            detail: Some(json!({"status": "backgrounded", "exit_code": 0})),
            at: 20_200,
        },
        Event::McpInputRequest {
            session: s,
            id: "m1".into(),
            req: "r1".into(),
            server_id: "srv".into(),
            request: json!({"mode": "form", "message": "Token?", "requestedSchema": {"type": "object", "properties": {}}}),
        },
        Event::McpInputResolved {
            session: s,
            req: "r1".into(),
        },
        Event::Text { session: s, delta: "Started the build.".into() },
    ];
    assert_parity(&base_view(), &events);
}
