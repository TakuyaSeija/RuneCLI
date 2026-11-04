//! Source size and native/display thinking are not provider token counts.
//! The remote endpoint alone is replaced; admission, store, media and compaction
//! still run through the real engine.
use runeengine::{
    engine::{Config, Engine},
    proto::{Command, Event, StopReason},
    provider::{Chunk, Continuation, Flow, Provider, Request},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Endpoint {
    requests: Mutex<Vec<Request>>,
    reject_main: bool,
}
impl Provider for Endpoint {
    fn name(&self) -> &str {
        "context-truth"
    }
    fn stream(
        &self,
        request: &Request,
        sink: &mut dyn FnMut(Chunk) -> Flow,
    ) -> runeengine::Result<()> {
        let mut sent = self.requests.lock().unwrap();
        let main = sent.iter().filter(|r| r.control.role.is_none()).count();
        sent.push(request.clone());
        if request.control.role == Some("compact") {
            sink(Chunk::Text("summary from original sources".into()));
        } else if self.reject_main && main > 0 {
            return Err(runeengine::Error::ContextOverflow(
                "fixture provider context rejection".into(),
            ));
        } else if main == 0 {
            // A large reasoning body, varied line by line: this fixture is about
            // a thinking block far bigger than any context budget, never about a
            // model repeating itself — an identical sentence forty thousand times
            // is exactly the loop `session::degeneration` stops (`REPETITION_*`
            // in `limits.rs`), and stopping it here would hang this test waiting
            // for an answer nobody gives.
            let thought: String = (0..40_000)
                .map(|n| format!("Generated fixture reasoning line {n}, for size only. "))
                .collect();
            sink(Chunk::Thinking(thought.clone()));
            sink(Chunk::Continuation(Continuation {
                provider: request.settings.provider.clone(),
                model: request.settings.model.clone(),
                data: json!({"reasoning_content":thought}),
            }));
            sink(Chunk::Text("first completed".into()));
            sink(Chunk::Usage {
                input: 1234,
                output: 12,
                cached: 1000,
                details: runeengine::provider::UsageDetails {
                    measured: true,
                    input_known: true,
                    output_known: true,
                    ..Default::default()
                },
            });
        } else {
            sink(Chunk::Text("provider reached again".into()));
        }
        Ok(())
    }
}

fn drive(
    engine: &mut Engine,
    events: &std::sync::mpsc::Receiver<Event>,
    id: i64,
    attach: Vec<String>,
) -> Vec<Event> {
    engine.handle(Command::Prompt {
        id: 1,
        session: id,
        text: "Continue original work".into(),
        attach,
    });
    let mut seen = Vec::new();
    loop {
        let event = events
            .recv_timeout(Duration::from_secs(30))
            .expect("engine went quiet");
        if let Event::PermissionRequest { req, .. } = &event {
            engine.handle(Command::PermissionReply {
                id: 9,
                req: req.clone(),
                decision: runeengine::proto::Decision::AllowOnce,
            });
        }
        let done = matches!(event, Event::Done { .. });
        seen.push(event);
        if done {
            return seen;
        }
    }
}

#[test]
fn identical_native_and_display_reasoning_never_invents_context_with_or_without_image() {
    for image in [false, true] {
        let root =
            std::env::temp_dir().join(format!("context-truth-{}-{image}", std::process::id()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[compact]\nfallback_context_tokens=10000\n",
        )
        .unwrap();
        let mut connection = runeengine::settings::providers::preset("9router")
            .unwrap()
            .build("");
        connection.secret = None;
        let mut model = runeengine::settings::providers::ModelInfo::named("vision-fixture", false);
        model.context = Some(10_000);
        model.max_output = Some(1024);
        model.modalities = vec!["text".into(), "image".into()];
        let mut other = model.clone();
        other.id = "other-fixture".into();
        connection.models = vec![model, other];
        runeengine::settings::configuration::Configuration::edit(
            &home,
            Some("providers"),
            Some(json!({"default":connection.name,"list":[connection]})),
            false,
            None,
        )
        .unwrap();
        let provider = Arc::new(Endpoint {
            requests: Mutex::new(Vec::new()),
            reject_main: false,
        });
        let (mut engine, events) = Engine::start(
            Config::at(home.join("rune.db"), home.clone(), provider.clone()).unwrap(),
        )
        .unwrap();
        let session = engine
            .open(root.join("project").to_str().unwrap())
            .unwrap()
            .id();
        let attach = if image {
            let path = root.join("project/image.png");
            std::fs::write(&path, [137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 0]).unwrap();
            vec![path.to_string_lossy().into_owned()]
        } else {
            Vec::new()
        };
        let first = drive(&mut engine, &events, session, attach);
        assert!(first.iter().any(|e| matches!(
            e,
            Event::Done {
                stop: StopReason::End,
                ..
            }
        )));
        let second = drive(&mut engine, &events, session, Vec::new());
        assert!(!second
            .iter()
            .any(|e| matches!(e, Event::Error { .. } | Event::Compacted { .. })));
        assert!(second.iter().any(|e| matches!(
            e,
            Event::Done {
                stop: StopReason::End,
                ..
            }
        )));
        let sent = provider.requests.lock().unwrap();
        let main: Vec<_> = sent.iter().filter(|r| r.control.role.is_none()).collect();
        assert_eq!(main.len(), 2);
        let thought = main[1]
            .messages
            .iter()
            .find(|m| m.reasoning.is_some())
            .unwrap();
        assert_eq!(
            thought
                .native(&main[1].settings.provider, &main[1].settings.model)
                .unwrap()["reasoning_content"],
            thought.reasoning.as_ref().unwrap().as_str()
        );
        if image {
            assert!(main[1].messages.iter().any(|m| !m.media.is_empty()));
        }
        drop(sent);
        engine.handle(Command::SettingsSet {
            id: 77,
            session,
            key: "model".into(),
            value: "other-fixture".into(),
        });
        loop {
            if let Event::Ack { id: 77, ok, .. } =
                events.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                assert!(ok);
                break;
            }
        }
        let switched = drive(&mut engine, &events, session, Vec::new());
        assert!(!switched
            .iter()
            .any(|e| matches!(e, Event::Error { .. } | Event::Compacted { .. })));
        let sent = provider.requests.lock().unwrap();
        let last = sent.last().unwrap();
        assert_eq!(last.settings.model, "other-fixture");
        assert!(last.messages.iter().all(|m| m
            .native(&last.settings.provider, &last.settings.model)
            .is_none()));
        drop(sent);
        let db =
            runeengine::store::Db::open(&home.join("rune.db"), Duration::from_secs(5)).unwrap();
        let usage = db.session_usage(session, &Default::default()).unwrap();
        assert_eq!(
            usage.used, 1234,
            "only the actual provider input is context usage"
        );
        drop(db);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn actual_overflow_compacts_once_then_returns_the_original_provider_failure() {
    let root = std::env::temp_dir().join(format!("context-truth-overflow-{}", std::process::id()));
    std::fs::create_dir_all(root.join("project")).unwrap();
    let home = root.join("home");
    let provider = Arc::new(Endpoint {
        requests: Mutex::new(Vec::new()),
        reject_main: true,
    });
    let (mut engine, events) =
        Engine::start(Config::at(home.join("rune.db"), home, provider.clone()).unwrap()).unwrap();
    let session = engine
        .open(root.join("project").to_str().unwrap())
        .unwrap()
        .id();
    drive(&mut engine, &events, session, Vec::new());
    let result = drive(&mut engine, &events, session, Vec::new());
    assert_eq!(
        result
            .iter()
            .filter(|e| matches!(e, Event::Compacted { .. }))
            .count(),
        1
    );
    assert!(result.iter().any(|e| matches!(e, Event::Error { message, .. } if message.contains("fixture provider context rejection"))));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.iter().filter(|r| r.control.role.is_none()).count(),
        3
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.control.role == Some("compact"))
            .count(),
        1
    );
    drop(requests);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(root);
}
