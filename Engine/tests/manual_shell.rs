//! Human shell submissions use real tools, PTYs, admission and storage. Only
//! the provider is scripted: no account, network request or live home is used.
use runeengine::{
    engine::{Config, Engine},
    proto::{Decision, StopReason},
    provider::{Chunk, Flow, Provider, Request},
    Command, Event,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{
        mpsc::{channel, Receiver},
        Arc, Mutex,
    },
    time::Duration,
};

struct Probe {
    seen: Mutex<Vec<Request>>,
    tool: bool,
}
impl Provider for Probe {
    fn name(&self) -> &str {
        "manual-probe"
    }
    fn stream(
        &self,
        request: &Request,
        sink: &mut dyn FnMut(Chunk) -> Flow,
    ) -> runeengine::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        let index = seen.len();
        seen.push(request.clone());
        drop(seen);
        if self.tool && index == 0 {
            sink(Chunk::ToolCall {
                id: "model-after-manual".into(),
                name: "terminal_use".into(),
                args: json!({"action":"model must still ask", "command":"echo model-only"}),
            });
        } else {
            sink(Chunk::Text("I have observed the command result.".into()));
        }
        Ok(())
    }
}
struct DenyingTerminal {
    requests: Mutex<Vec<Request>>,
}
impl Provider for DenyingTerminal {
    fn name(&self) -> &str {
        "manual-denial-fixture"
    }
    fn stream(
        &self,
        request: &Request,
        sink: &mut dyn FnMut(Chunk) -> Flow,
    ) -> runeengine::Result<()> {
        let mut requests = self.requests.lock().unwrap();
        let main_before = requests
            .iter()
            .filter(|held| held.control.role.is_none())
            .count();
        requests.push(request.clone());
        drop(requests);
        if request.control.role == Some("classifier") {
            let ids: Vec<_> = request.messages[0]
                .content
                .lines()
                .filter_map(|line| line.strip_prefix("Action "))
                .collect();
            let denied: Vec<_> = ids.into_iter().map(|id| json!({"id":id,"reason":"Project policy prohibits terminal commands","rule":"Never run terminal commands"})).collect();
            sink(Chunk::ToolCall {
                id: "deny".into(),
                name: "submit_decisions".into(),
                args: json!({"allow":[],"deny":denied}),
            });
        } else if main_before == 0 {
            sink(Chunk::ToolCall {
                id: "policy-refusal".into(),
                name: "terminal_use".into(),
                args: json!({"action":"try command","command":"echo must-not-run"}),
            });
        } else {
            sink(Chunk::Text(
                "I explain the refusal and manual option without executing it.".into(),
            ));
        }
        Ok(())
    }
}

struct Harness {
    engine: Engine,
    events: Receiver<Event>,
    session: i64,
    root: PathBuf,
    project: PathBuf,
    probe: Arc<Probe>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.engine.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn harness(label: &str, tool: bool) -> Harness {
    let root = std::env::temp_dir().join(format!("manual-shell-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("project")).unwrap();
    let project = runeengine::platform::canonical(&root.join("project")).unwrap();
    let home = root.join("home");
    runeengine::testing::posix_shell_for_tests(&home);
    runeengine::settings::configuration::Configuration::edit(
        &home,
        Some("sanitizer.enabled"),
        Some(json!(false)),
        false,
        None,
    )
    .unwrap();
    let probe = Arc::new(Probe {
        seen: Mutex::new(Vec::new()),
        tool,
    });
    let config = Config::at(root.join("rune.db"), home, probe.clone()).unwrap();
    let (mut engine, events) = Engine::start(config).unwrap();
    let session = engine.open(project.to_str().unwrap()).unwrap().id();
    Harness {
        engine,
        events,
        session,
        root,
        project,
        probe,
    }
}
fn until_done(h: &mut Harness) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = h
            .events
            .recv_timeout(Duration::from_secs(60))
            .expect("engine went quiet");
        if let Event::PermissionRequest { req, .. } = &event {
            h.engine.handle(Command::PermissionReply {
                id: 999,
                req: req.clone(),
                decision: Decision::Deny,
            });
        }
        let done = matches!(event, Event::Done { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}
fn submit(h: &mut Harness, id: u64, text: String) -> Vec<Event> {
    h.engine.handle(Command::ShellSubmit {
        id,
        session: h.session,
        text,
        submission: format!("fixture-manual-token-{id}"),
    });
    until_done(h)
}
fn ack(h: &mut Harness, id: u64) -> Value {
    loop {
        match h.events.recv_timeout(Duration::from_secs(30)).unwrap() {
            Event::Ack {
                id: got,
                ok,
                data,
                error,
            } if got == id => {
                assert!(ok, "{error:?}");
                return data.unwrap_or(Value::Null);
            }
            _ => {}
        }
    }
}

#[test]
fn manual_shell_runs_outside_workspace_once_and_ai_sees_the_result_after_execution() {
    let mut h = harness("full-once", false);
    std::fs::write(
        h.project.join("CLASSIFIER.md"),
        "Never permit any terminal command.",
    )
    .unwrap();
    let outside = h.root.join("outside.txt");
    let text = format!(
        "! printf 'manual-output\\n'\nprintf 'once\\n' >> '{}'",
        outside.display()
    );
    let events = submit(&mut h, 101, text.clone());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::PermissionRequest { .. })),
        "manual command asked again"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "once\n");
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::ToolShell { .. })));
    let seen = h.probe.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let messages = seen[0]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(messages.contains("manual-output"), "{messages}");
    assert!(seen[0].system.contains("not authorization for later calls"));
    assert!(!messages.contains("do not retry it"));
    assert!(!messages.contains("Never execute it again"));
    drop(seen);
    h.engine.handle(Command::ShellSubmit {
        id: 101,
        session: h.session,
        text,
        submission: "fixture-manual-token-101".into(),
    });
    let receipt = ack(&mut h, 101);
    assert_eq!(receipt["already_accepted"], true);
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "once\n");
    h.engine.handle(Command::SessionView {
        id: 102,
        session: h.session,
    });
    let view = ack(&mut h, 102);
    assert_eq!(view["mode"], "ask");
    assert_eq!(view["toolcalls"].as_array().unwrap().len(), 1);
    assert!(view["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["manual_shell"] == true));
    assert!(view["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["context"]["kind"] == "manual_shell"));
}

#[test]
fn manual_multiline_executes_each_line_and_exposes_the_same_script_and_result_to_ai() {
    let mut h = harness("literal-newlines", false);
    let text = "! echo tes ; echo tes2\necho tes3";
    let events = submit(&mut h, 901, text.into());
    let args = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCall { args, .. } => Some(args),
            _ => None,
        })
        .unwrap();
    assert_eq!(args["command"], "echo tes ; echo tes2\necho tes3");
    let result = events
        .iter()
        .find_map(|event| match event {
            Event::ToolResult { ok, content, .. } => Some((*ok, content)),
            _ => None,
        })
        .unwrap();
    assert!(result.0, "{}", result.1);
    let body = runeengine::tools::notice::split(result.1).rest;
    assert_eq!(body.lines().collect::<Vec<_>>(), ["tes", "tes2", "tes3"]);
    let seen = h.probe.seen.lock().unwrap();
    let transcript = seen[0]
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(transcript.contains("tes2\\necho tes3"));
    assert!(transcript.contains("tes\\ntes2\\ntes3"));
}

#[test]
fn terminal_aliases_share_the_same_pinned_shell_execution_without_manual_authority() {
    let mut h = harness("terminal-alias", false);
    h.engine.handle(Command::ToolInvoke {
        id: 801,
        session: h.session,
        name: "bash".into(),
        args: json!({"action":"alias command", "command":"echo alias-output"}),
    });
    let mut shell = false;
    let mut asked = false;
    loop {
        match h.events.recv_timeout(Duration::from_secs(30)).unwrap() {
            Event::PermissionRequest { req, .. } => {
                asked = true;
                h.engine.handle(Command::PermissionReply {
                    id: 802,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::ToolShell { .. } => shell = true,
            Event::ToolResult { ok, content, .. } => {
                assert!(ok, "{content}");
                assert!(content.contains("alias-output"));
                break;
            }
            _ => {}
        }
    }
    assert!(shell && asked);
    assert!(h.probe.seen.lock().unwrap().is_empty());
}

#[test]
fn a_frontend_terminal_alias_refused_by_the_user_receives_the_manual_option_once() {
    let mut h = harness("direct-user-refusal", false);
    h.engine.handle(Command::ToolInvoke {
        id: 911,
        session: h.session,
        name: "bash".into(),
        args: json!({"action":"check denial", "command":"echo should-not-run"}),
    });
    loop {
        match h.events.recv_timeout(Duration::from_secs(30)).unwrap() {
            Event::PermissionRequest { req, .. } => {
                h.engine.handle(Command::PermissionReply {
                    id: 912,
                    req,
                    decision: Decision::Deny,
                });
            }
            Event::ToolResult {
                ok,
                content,
                detail,
                ..
            } => {
                assert!(!ok);
                assert_eq!(detail.unwrap()["code"], "denied");
                assert!(content.contains("The user refused this call"));
                assert_eq!(content.matches("submit !").count(), 1, "{content}");
                assert!(!content.contains("should-not-run"));
                break;
            }
            _ => {}
        }
    }
    assert!(h.probe.seen.lock().unwrap().is_empty());
}

#[test]
fn manual_submission_receipts_reject_different_scripts_and_bad_tokens() {
    let mut h = harness("receipt-mismatch", false);
    submit(&mut h, 701, "! echo first".into());
    for (id, text, token) in [
        (702, "! echo second", "fixture-manual-token-701"),
        (703, "! echo bad", ""),
        (704, "!", "fixture-manual-token-704"),
        (
            705,
            " ! echo not-a-manual-prefix",
            "fixture-manual-token-705",
        ),
    ] {
        h.engine.handle(Command::ShellSubmit {
            id,
            session: h.session,
            text: text.into(),
            submission: token.into(),
        });
        loop {
            if let Event::Ack { id: got, ok, .. } =
                h.events.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                if got == id {
                    assert!(!ok);
                    break;
                }
            }
        }
    }
    assert_eq!(h.probe.seen.lock().unwrap().len(), 1);
}

#[test]
fn manual_full_access_never_authorizes_the_models_next_terminal_call() {
    let mut h = harness("scope", true);
    let events = submit(&mut h, 201, "! echo manual-before-model".into());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::PermissionRequest { .. }))
            .count(),
        1
    );
    let failed = events
        .iter()
        .find_map(|event| match event {
            Event::ToolResult {
                id, ok, content, ..
            } if id == "model-after-manual" => Some((*ok, content)),
            _ => None,
        })
        .unwrap();
    assert!(!failed.0);
    assert!(failed.1.contains("submit !"), "{}", failed.1);
    let seen = h.probe.seen.lock().unwrap();
    let last = seen.last().unwrap();
    assert!(last.system.contains("do not repeat or bypass that refusal"));
    assert!(!last
        .messages
        .iter()
        .any(|row| row.content.contains("Do not retry or bypass")));
}

#[test]
fn classifier_terminal_denial_is_explained_to_ai_with_manual_option_and_no_execution() {
    let mut h = harness("classifier-refusal", false);
    h.engine.shutdown();
    std::fs::write(
        h.project.join("CLASSIFIER.md"),
        "Never run terminal commands.",
    )
    .unwrap();
    let provider = Arc::new(DenyingTerminal {
        requests: Mutex::new(Vec::new()),
    });
    let config = Config::at(
        h.root.join("classifier.db"),
        h.root.join("home"),
        provider.clone(),
    )
    .unwrap();
    let (engine, events) = Engine::start(config).unwrap();
    h.engine = engine;
    h.events = events;
    h.session = h.engine.open(h.project.to_str().unwrap()).unwrap().id();
    h.engine.handle(Command::SessionMode {
        id: 921,
        session: h.session,
        mode: "full".into(),
    });
    ack(&mut h, 921);
    h.engine.handle(Command::Prompt {
        id: 922,
        session: h.session,
        text: "try the proposed command".into(),
        attach: Vec::new(),
    });
    let events = until_done(&mut h);
    let denied = events
        .iter()
        .find_map(|event| match event {
            Event::ToolResult {
                id,
                ok,
                content,
                detail,
                ..
            } if id == "policy-refusal" => Some((*ok, content, detail)),
            _ => None,
        })
        .unwrap();
    assert!(!denied.0);
    assert_eq!(denied.2.as_ref().unwrap()["code"], "classifier_denied");
    assert_eq!(denied.1.matches("submit !").count(), 1);
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::ToolOutput { .. })));
    let requests = provider.requests.lock().unwrap();
    let classifier = requests
        .iter()
        .find(|request| request.control.role == Some("classifier"))
        .unwrap();
    assert!(classifier
        .system
        .contains("A ! prefix cannot authorize a proposed AI action"));
    assert!(requests
        .iter()
        .rev()
        .find(|request| request.control.role.is_none())
        .unwrap()
        .messages
        .iter()
        .any(|message| message.content.contains("submit !")));
}

#[test]
fn recovery_of_pending_manual_submission_stores_it_without_executing() {
    let mut h = harness("recover-no-execution", false);
    let marker = h.project.join("must-not-exist.txt");
    let database =
        runeengine::store::Db::open(&h.root.join("rune.db"), Duration::from_secs(30)).unwrap();
    let composer = runeengine::store::composer::Composer {
        pending: vec![runeengine::store::composer::PendingPrompt {
            manual_shell: true,
            id: 777,
            message_id: None,
            text: format!("! echo no > '{}'", marker.display()),
            media: Vec::new(),
        }],
        ..Default::default()
    };
    database.jsonl.write_composer(h.session, &composer).unwrap();
    drop(database);
    h.engine.recover_session(h.session).unwrap();
    assert!(!marker.exists());
    assert!(h.probe.seen.lock().unwrap().is_empty());
    let database =
        runeengine::store::Db::open(&h.root.join("rune.db"), Duration::from_secs(30)).unwrap();
    let history = database.history(h.session).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].manual_shell);
    assert!(database
        .jsonl
        .composer(h.session)
        .unwrap()
        .unwrap()
        .pending
        .is_empty());
}

#[test]
fn ordinary_prompts_containing_bang_never_execute_as_manual_commands() {
    for (index, text) in [
        "! echo should-not-run",
        "hello ! echo should-not-run",
        "hello\n! echo should-not-run",
        " ! echo should-not-run",
    ]
    .into_iter()
    .enumerate()
    {
        let mut h = harness(&format!("prompt-{index}"), false);
        h.engine.handle(Command::Prompt {
            id: 301,
            session: h.session,
            text: text.into(),
            attach: Vec::new(),
        });
        let events = until_done(&mut h);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ToolCall { .. })),
            "{text}"
        );
        assert!(h.probe.seen.lock().unwrap()[0]
            .messages
            .iter()
            .any(|row| row.content == text));
    }
}

#[test]
fn a_manual_command_yielding_to_background_is_not_relaunched_before_the_ai_responds() {
    let mut h = harness("background", false);
    runeengine::settings::configuration::Configuration::edit(
        &h.root.join("home"),
        Some("terminal.command_timeout"),
        Some(json!("1s")),
        false,
        None,
    )
    .unwrap();
    let output = h.project.join("launched.txt");
    let events = submit(
        &mut h,
        401,
        format!("! printf 'once\\n' >> '{}'\nsleep 5", output.display()),
    );
    assert!(events.iter().any(|event|matches!(event,Event::ToolResult{detail:Some(detail),..} if detail["status"]=="backgrounded")),"{events:?}");
    assert_eq!(std::fs::read_to_string(output).unwrap(), "once\n");
    assert!(h.probe.seen.lock().unwrap()[0]
        .messages
        .iter()
        .any(|row| row.content.contains("Moved to the background")));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Done {
            stop: StopReason::End,
            ..
        }
    )));
}

#[test]
fn a_manual_submission_waits_behind_an_active_turn_and_preserves_later_prompt_order() {
    struct Held {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<Receiver<()>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Provider for Held {
        fn name(&self) -> &str {
            "held"
        }
        fn stream(
            &self,
            _: &Request,
            sink: &mut dyn FnMut(Chunk) -> Flow,
        ) -> runeengine::Result<()> {
            let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if index == 0 {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            }
            sink(Chunk::Text(format!("answer-{index}")));
            Ok(())
        }
    }
    let mut h = harness("queue", false);
    h.engine.shutdown();
    let (entered_tx, entered) = channel();
    let (release, release_rx) = channel();
    let provider = Arc::new(Held {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        calls: 0.into(),
    });
    let config = Config::at(h.root.join("queue.db"), h.root.join("queue-home"), provider).unwrap();
    let (engine, events) = Engine::start(config).unwrap();
    h.engine = engine;
    h.events = events;
    h.session = h.engine.open(h.project.to_str().unwrap()).unwrap().id();
    h.engine.handle(Command::Prompt {
        id: 501,
        session: h.session,
        text: "first".into(),
        attach: Vec::new(),
    });
    entered.recv_timeout(Duration::from_secs(30)).unwrap();
    let marker = h.project.join("queued.txt");
    h.engine.handle(Command::ShellSubmit {
        id: 502,
        session: h.session,
        text: format!("! echo queued > '{}'", marker.display()),
        submission: "fixture-manual-token-502".into(),
    });
    h.engine.handle(Command::Prompt {
        id: 503,
        session: h.session,
        text: "after-manual".into(),
        attach: Vec::new(),
    });
    assert!(!marker.exists());
    release.send(()).unwrap();
    let events = until_done(&mut h);
    assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), "queued");
    let words = events
        .iter()
        .filter_map(|event| {
            if let Event::Text { delta, .. } = event {
                Some(delta.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(words, ["answer-0", "answer-1", "answer-2"]);
    let tool = events
        .iter()
        .position(|event| matches!(event, Event::ToolCall { .. }))
        .unwrap();
    let final_prompt = events
        .iter()
        .position(
            |event| matches!(event,Event::TurnStart{text:Some(text),..} if text=="after-manual"),
        )
        .unwrap();
    assert!(tool < final_prompt);
}
