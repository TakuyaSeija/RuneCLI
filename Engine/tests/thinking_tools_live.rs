//! Owner-approved live check (2026-10-14): with thinking on through a router,
//! a turn that calls tools and the turn after it run end to end in the engine —
//! the thinking reaches the frontend (`Event::Thinking`), is kept in the
//! transcript, and replaying the history with the tool rounds is accepted.
//! Thinking that really reaches a Claude route for the first time (0.5.5) is
//! exactly where a provider insists on its thinking being sent back.
//!
//! RUNE_LIVE_SETTINGS_HOME=~/.rune RUNE_LIVE_PROVIDER=9router \
//! RUNE_LIVE_THINKING_CASES='cc/claude-opus-5||text;ag/gemini-3.8-flash|high|text' \
//! cargo test -p runeengine --test thinking_tools_live -- --ignored --nocapture
//!
//! A case is `model|level|expect` as in `thinking_live.rs`.
use runeengine::{
    engine::{Config, Engine},
    proto::{Command, Decision, Event, StopReason},
    provider::live::Live,
    settings::{configuration::Configuration, Settings},
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

/// The answer to the puzzle in notes.txt: a task worth thinking about, so a
/// model that thinks adaptively does think (a trivial one is answered unthought,
/// and a router returns no thinking text for a few dozen thinking tokens).
const SECRET: &str = "15:33:06";
const PUZZLE: &str = "A train leaves at 09:17 and travels 283 km at 74 km/h, then waits 13 minutes, then travels 196 km at 88 km/h. At what exact time, to the second, does it arrive?";

fn clean(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, clean(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(clean).collect()),
        x => x,
    }
}

struct Turn {
    stop: Option<StopReason>,
    errors: Vec<String>,
    tools: usize,
    thinking: usize,
    answer: String,
}

fn run(
    engine: &mut Engine,
    events: &std::sync::mpsc::Receiver<Event>,
    session: i64,
    id: u64,
    text: &str,
) -> Turn {
    engine.handle(Command::Prompt {
        id,
        session,
        text: text.into(),
        attach: Vec::new(),
    });
    let mut turn = Turn {
        stop: None,
        errors: Vec::new(),
        tools: 0,
        thinking: 0,
        answer: String::new(),
    };
    loop {
        match events
            .recv_timeout(Duration::from_secs(300))
            .expect("live model went quiet")
        {
            Event::PermissionRequest { req, .. } => {
                engine.handle(Command::PermissionReply {
                    id: 900,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::QuestionsRequest { req, .. } => {
                engine.handle(Command::QuestionsReply {
                    id: 901,
                    session,
                    req,
                    reply: runeengine::tools::questions::Reply::Unavailable,
                });
            }
            Event::Thinking { delta, .. } => turn.thinking += delta.chars().count(),
            Event::Text { delta, .. } => turn.answer.push_str(&delta),
            Event::ToolResult { .. } => turn.tools += 1,
            Event::Error { message, .. } => turn.errors.push(message),
            Event::Done { stop, .. } => {
                turn.stop = Some(stop);
                return turn;
            }
            _ => {}
        }
    }
}

#[test]
#[ignore = "requires explicit owner-approved model and credentials"]
fn thinking_survives_tool_rounds_and_the_next_turn_through_the_engine() {
    let owner = PathBuf::from(std::env::var_os("RUNE_LIVE_SETTINGS_HOME").unwrap());
    let provider = std::env::var("RUNE_LIVE_PROVIDER").unwrap();
    let cases = std::env::var("RUNE_LIVE_THINKING_CASES").unwrap();
    let root = std::env::temp_dir().join(format!("RuneThinkingToolsLive-{}", std::process::id()));
    let home = root.join("home");
    let project = root.join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("notes.txt"), format!("{PUZZLE}\n")).unwrap();
    std::fs::write(project.join("other.txt"), "Nothing here.\n").unwrap();
    let source = Settings::load(&owner, &owner.join("rune.db")).unwrap();
    let mut cfg = source.providers.get(&provider).unwrap().clone();
    // An audit proxy in between, when asked for (never logs the key).
    if let Ok(url) = std::env::var("RUNE_LIVE_BASE_URL") {
        cfg.base_url = url;
    }
    Configuration::edit(
        &home,
        Some("providers"),
        Some(json!({"default": provider, "list": [clean(serde_json::to_value(&cfg).unwrap())]})),
        false,
        None,
    )
    .unwrap();
    if let Some(secret) = &cfg.secret {
        runeengine::settings::Secrets::load(&home)
            .unwrap()
            .append(secret, source.secrets.get(secret).unwrap())
            .unwrap();
    }
    // Only the main model is under test: no reviewer, no extractor.
    for (key, value) in [
        ("retry.max_attempts", json!(1)),
        ("classifier.enabled", json!(false)),
        ("sanitizer.enabled", json!(false)),
    ] {
        Configuration::edit(&home, Some(key), Some(value), false, None).unwrap();
    }
    let settings = Arc::new(Mutex::new(
        Settings::load(&home, &home.join("rune.db")).unwrap(),
    ));
    let (mut engine, events) = Engine::start(Config {
        db_path: home.join("rune.db"),
        data_dir: home.clone(),
        settings: settings.clone(),
        provider: Arc::new(Live::new(settings)),
        read_only: false,
    })
    .unwrap();

    let mut report = Vec::new();
    let mut id = 10;
    for case in cases.split(';').filter(|c| !c.trim().is_empty()) {
        let parts: Vec<&str> = case.split('|').collect();
        let (model, level, expect) = (parts[0], parts[1], parts[2]);
        let session = engine.open(project.to_str().unwrap()).unwrap().id();
        let mut set = |key: &str, value: String| {
            id += 1;
            engine.handle(Command::SettingsSet {
                id,
                session,
                key: key.into(),
                value,
            });
        };
        set("model", format!("{provider}:{model}"));
        set("thinking", (level != "off").to_string());
        if !level.is_empty() && level != "off" {
            set("effort", level.into());
        }
        engine.handle(Command::SessionMode {
            id: 1,
            session,
            mode: "full".into(),
        });
        let first = run(
            &mut engine,
            &events,
            session,
            100 + id,
            "Use find_files to list the .txt files in this workspace, then use inspect_file to read notes.txt, solve the puzzle in it checking your arithmetic twice, and reply with only the arrival time (HH:MM:SS).",
        );
        let second = run(
            &mut engine,
            &events,
            session,
            200 + id,
            "Which tools did you just use, and what was the arrival time? Answer in one short line.",
        );
        // What the session kept: assistant rows with reasoning.
        let transcript = std::fs::read_dir(home.join("projects"))
            .unwrap()
            .flatten()
            .map(|p| p.path().join(format!("sessions/{session}/session.jsonl")))
            .find(|p| p.is_file())
            .map(|p| std::fs::read_to_string(p).unwrap())
            .unwrap_or_default();
        let kept = transcript
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|row| {
                row["role"] == "assistant"
                    && row["reasoning"].as_str().is_some_and(|r| !r.is_empty())
            })
            .count();
        let thinking = first.thinking + second.thinking;
        let passed = first.errors.is_empty()
            && second.errors.is_empty()
            && first.stop == Some(StopReason::End)
            && second.stop == Some(StopReason::End)
            && first.tools >= 2
            // Whether the sum comes out right is the model's, not Rune's (the
            // same model answers it right on one run and a second off on the
            // next); it is reported, never the verdict.
            && match expect {
                "text" => thinking > 0 && kept > 0,
                "quiet" => thinking == 0,
                _ => true,
            };
        let correct = first.answer.contains(SECRET) && second.answer.contains(SECRET);
        let row = json!({
            "model": model, "level": level, "expect": expect, "passed": passed,
            "answer_correct": correct,
            "tools": first.tools, "thinking_chars": thinking, "reasoning_rows_kept": kept,
            "first": {"stop": format!("{:?}", first.stop), "answer": first.answer.trim(), "errors": first.errors},
            "second": {"stop": format!("{:?}", second.stop), "answer": second.answer.trim(), "errors": second.errors},
        });
        println!("THINKING_TOOLS_LIVE {row}");
        report.push(row);
    }
    std::fs::write(
        root.join("results.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
    for name in ["master.key", "secrets.vault", "secrets.lock"] {
        let _ = std::fs::remove_file(home.join(name));
    }
    println!(
        "THINKING_TOOLS_LIVE_REPORT {}",
        root.join("results.json").display()
    );
    assert!(
        report.iter().all(|r| r["passed"] == true),
        "a live case failed; see the report"
    );
}
