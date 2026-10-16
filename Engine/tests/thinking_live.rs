//! Owner-approved live check (2026-10-14): what a person chooses for thinking
//! reaches a router's upstream, and the thinking that comes back reaches Rune.
//! The request is Rune's own (`ModelSettings::body` through `Live`), the answer
//! is read by Rune's own stream parser; the key is copied into a throwaway
//! home through the vault API and never printed.
//!
//! RUNE_LIVE_SETTINGS_HOME=~/.rune RUNE_LIVE_PROVIDER=9router \
//! RUNE_LIVE_THINKING_CASES='ag/gemini-3.8-flash|high|text;cc/claude-opus-5||text' \
//! cargo test -p runeengine --test thinking_live -- --ignored --nocapture
//!
//! A case is `model|level|expect|sent`: level empty = thinking on with no level
//! of its own, `off` = thinking off; expect `text` (thinking text arrives),
//! `quiet` (no thinking text may arrive) or `any` (the route decides — e.g. an
//! "off" that stops thinking between tool calls only); `sent`, when given, is
//! the `reasoning_effort` Rune must have sent, `-` that it sent none (a
//! gateway route on its own level, "Route default", 2026-10-15). Each row
//! reports the reasoning tokens the router counted, so a route default can be
//! compared with the levels. `RUNE_LIVE_THINKING_UNCONFIGURED=1` reads every
//! model as the person has configured nothing for its thinking (no Alt+C
//! options, levels or default), as a fresh home holds it.
use runeengine::provider::{live::Live, Chunk, Flow, Message, Provider, Request};
use runeengine::settings::{configuration::Configuration, ModelSettings, Settings};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const QUESTION: &str = "A train leaves at 09:17 and travels 283 km at 74 km/h, then waits 13 minutes, then travels 196 km at 88 km/h. Work out the exact arrival time to the second, checking your arithmetic twice. Reply with only the time (HH:MM:SS).";

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

#[test]
#[ignore = "requires explicit owner-approved model and credentials"]
fn thinking_choices_reach_the_router_and_its_thinking_comes_back() {
    let owner = PathBuf::from(std::env::var_os("RUNE_LIVE_SETTINGS_HOME").unwrap());
    let provider = std::env::var("RUNE_LIVE_PROVIDER").unwrap();
    let cases = std::env::var("RUNE_LIVE_THINKING_CASES").unwrap();
    let root = std::env::temp_dir().join(format!("RuneThinkingLive-{}", std::process::id()));
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let source = Settings::load(&owner, &owner.join("rune.db")).unwrap();
    let mut cfg = source.providers.get(&provider).unwrap().clone();
    if std::env::var_os("RUNE_LIVE_THINKING_UNCONFIGURED").is_some() {
        for model in &mut cfg.models {
            model.reasoning_enabled = None;
            model.reasoning_options = None;
            model.default_effort = None;
            model.efforts.clear();
            model.thinking_required = false;
            model.manual_fields.retain(|field| {
                !matches!(
                    field.as_str(),
                    "thinking_required" | "efforts" | "default_effort"
                )
            });
        }
    }
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
    Configuration::edit(
        &home,
        Some("retry.max_attempts"),
        Some(json!(1)),
        false,
        None,
    )
    .unwrap();
    // The throwaway home's own database: the owner's is only read, above.
    let settings = Arc::new(Mutex::new(
        Settings::load(&home, &home.join("rune.db")).unwrap(),
    ));
    let live = Live::new(settings.clone());
    let routed = settings
        .lock()
        .unwrap()
        .providers
        .get(&provider)
        .unwrap()
        .clone();

    let mut report = Vec::new();
    for case in cases.split(';').filter(|c| !c.trim().is_empty()) {
        let parts: Vec<&str> = case.split('|').collect();
        let (model, level, expect) = (parts[0], parts[1], parts[2]);
        let must_send = parts.get(3).filter(|v| !v.is_empty());
        let mut chosen = ModelSettings::for_provider(&routed);
        chosen.model = model.into();
        chosen.thinking = level != "off";
        chosen.effort = (!level.is_empty() && level != "off").then(|| level.to_string());
        chosen.normalize_for_model(&routed);
        let body = chosen.body(&routed);
        let sent = body.get("reasoning_effort").cloned().unwrap_or(Value::Null);
        assert!(!body.contains_key("reasoning"), "{model}: {body:?}");
        let request = Request {
            control: Default::default(),
            system: String::new(),
            messages: vec![Message::user(QUESTION)],
            tools: Vec::new(),
            settings: chosen.clone(),
        };
        let (mut thought, mut answer) = (String::new(), String::new());
        let mut reasoning_tokens = None;
        let outcome = live.stream(&request, &mut |chunk| {
            match chunk {
                Chunk::Thinking(t) => thought.push_str(&t),
                Chunk::Text(t) => answer.push_str(&t),
                Chunk::Usage { details, .. } => {
                    reasoning_tokens = details.reasoning.or(reasoning_tokens)
                }
                _ => {}
            }
            Flow::Continue
        });
        let passed = outcome.is_ok()
            && !answer.trim().is_empty()
            && must_send.is_none_or(|v| {
                if **v == *"-" {
                    sent.is_null()
                } else {
                    sent == json!(v)
                }
            })
            && match expect {
                "text" => !thought.trim().is_empty(),
                "quiet" => thought.trim().is_empty(),
                _ => true,
            };
        let row = json!({
            "model": model, "level": level, "sent": sent, "expect": expect,
            "thinking_chars": thought.chars().count(), "reasoning_tokens": reasoning_tokens,
            "answer": answer.trim(),
            "error": outcome.err().map(|e| e.to_string()), "passed": passed,
        });
        println!("THINKING_LIVE {row}");
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
        "THINKING_LIVE_REPORT {}",
        root.join("results.json").display()
    );
    assert!(
        report.iter().all(|r| r["passed"] == true),
        "a live case failed; see the report"
    );
}
