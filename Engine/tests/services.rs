//! End-to-end: the connections that serve the web tools (owner, 2026-10-14).
//!
//! A real engine, a scripted model, and vendors played by servers on this
//! machine speaking each vendor's own shapes. Nothing below the provider is
//! stubbed: a connection is proven by a real request before it is saved, the
//! model is told who serves through its status, a search goes out over HTTP,
//! and what happened is written to the session's log.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use runeengine::engine::{Config, Engine};
use runeengine::proto::{Command, Decision, Event, LogEntry, ServiceRoutes, ServiceUse};
use runeengine::provider::scripted::Scripted;
use serde_json::{json, Value};

struct Harness {
    engine: Engine,
    events: std::sync::mpsc::Receiver<Event>,
    session: i64,
    data: PathBuf,
    model: Arc<Scripted>,
    base: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn harness(name: &str, responses: Value) -> Harness {
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-services-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let data = base.join("data");
    // Retries wait a second, not three: the curve is the owner's, its step a setting.
    for (key, value) in [
        ("sanitizer.enabled", json!(false)),
        ("retry.initial_delay", json!("1s")),
        ("retry.max_attempts", json!(1)),
    ] {
        runeengine::settings::configuration::Configuration::edit(
            &data,
            Some(key),
            Some(value),
            false,
            None,
        )
        .unwrap();
    }
    let model = Arc::new(
        Scripted::from_value(&json!({ "responses": responses }), "test".into())
            .unwrap()
            .keeping(8),
    );
    let cfg = Config::at(base.join("rune.db"), data.clone(), model.clone()).unwrap();
    let (mut engine, events) = Engine::start(cfg).unwrap();
    let session = engine.open(project.to_str().unwrap()).unwrap().id();
    Harness {
        engine,
        events,
        session,
        data,
        model,
        base,
    }
}

impl Harness {
    /// Send `command` and wait for its answer.
    fn ask(&mut self, command: Command) -> Result<Value, String> {
        let id = command.request_id();
        self.engine.handle(command);
        loop {
            match self
                .events
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("the engine went quiet")
            {
                Event::Ack {
                    id: got,
                    ok,
                    error,
                    data,
                } if got == id => {
                    return if ok {
                        Ok(data.unwrap_or(Value::Null))
                    } else {
                        Err(error.unwrap_or_default())
                    }
                }
                _ => {}
            }
        }
    }

    /// One prompt to its end; every event it produced.
    fn prompt(&mut self, text: &str) -> Vec<Event> {
        self.engine.handle(Command::Prompt {
            id: 1,
            session: self.session,
            text: text.into(),
            attach: Vec::new(),
        });
        let mut seen = Vec::new();
        loop {
            let event = self
                .events
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the engine went quiet");
            if let Event::PermissionRequest { req, .. } = &event {
                self.engine.handle(Command::PermissionReply {
                    id: 99,
                    req: req.clone(),
                    decision: Decision::AllowOnce,
                });
            }
            let done = matches!(event, Event::Done { .. });
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    fn save(
        &mut self,
        id: u64,
        name: &str,
        base_url: &str,
        search: bool,
        fetch: bool,
        key: &str,
    ) -> Result<Value, String> {
        self.ask(Command::ServiceSave {
            id,
            name: name.into(),
            api: None,
            base_url: Some(base_url.into()),
            fetch_url: None,
            search: search.then(ServiceUse::default),
            fetch: fetch.then(ServiceUse::default),
            routes: None,
            zone: None,
            key_name: None,
            key: Some(key.into()),
        })
    }

    /// The 9router connection as the screen sends it: the route each service
    /// uses (none = off) and the routes it keeps.
    fn router(
        &mut self,
        id: u64,
        base: &str,
        search: Option<&str>,
        fetch: Option<&str>,
        kept: Option<(&[&str], &[&str])>,
    ) -> Result<Value, String> {
        let using = |route: Option<&str>| {
            route.map(|r| ServiceUse {
                route: Some(r.into()),
            })
        };
        self.ask(Command::ServiceSave {
            id,
            name: "9router".into(),
            api: None,
            base_url: Some(base.into()),
            fetch_url: None,
            search: using(search),
            fetch: using(fetch),
            routes: kept.map(|(search, fetch)| ServiceRoutes {
                search: search.iter().map(|r| r.to_string()).collect(),
                fetch: fetch.iter().map(|r| r.to_string()).collect(),
            }),
            zone: None,
            key_name: None,
            key: None,
        })
    }

    fn rune_toml(&self) -> String {
        std::fs::read_to_string(self.data.join("rune.toml")).unwrap_or_default()
    }
}

/// A vendor on this machine: each request is answered with the next of
/// `answers` (status, body), and what it was asked is kept.
struct Vendor {
    url: String,
    asked: Arc<Mutex<Vec<String>>>,
}

fn vendor(answers: Vec<(u16, Value)>) -> Vendor {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    std::thread::spawn(move || {
        for (status, body) in answers {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let request = read_request(&stream);
            log.lock().unwrap().push(request);
            respond(stream, status, &body);
        }
    });
    Vendor { url, asked }
}

/// One HTTP request as it arrived: head lines, then the body.
fn read_request(stream: &std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" {
            break;
        }
        request.push_str(&line);
    }
    let mut sent = vec![0; length];
    let _ = reader.read_exact(&mut sent);
    request.push_str(&String::from_utf8_lossy(&sent));
    request
}

fn respond(mut stream: std::net::TcpStream, status: u16, body: &Value) {
    let text = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// The JSON body of a request [`read_request`] kept.
fn body_of(request: &str) -> Value {
    serde_json::from_str(&request[request.find('{').unwrap()..]).unwrap()
}

/// A gateway that answers as 9router 0.5.99 does: it lists each vendor's
/// services as `<vendor>/search` and `<vendor>/fetch` (`GET /v1/models/web`),
/// and its search and fetch doors serve a vendor it knows by its id alone —
/// anything else, `exa/search` included, is `400 Unknown provider`. A vendor
/// it serves without listing (`ag`, a chat model's search) is in `unlisted`.
/// `rawhtml` answers a fetch with the page's HTML while its `format` says
/// markdown — 9router names the format it was asked for, not what came back.
fn nine_router(listed: &'static [&'static str], unlisted: &'static [&'static str]) -> Vendor {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                return;
            };
            let request = read_request(&stream);
            log.lock().unwrap().push(request.clone());
            let (status, body) = if request.starts_with("GET /v1/models/web") {
                let data: Vec<Value> = listed
                    .iter()
                    .flat_map(|v| {
                        [
                            json!({"id": format!("{v}/search"), "kind": "webSearch", "owned_by": v}),
                            json!({"id": format!("{v}/fetch"), "kind": "webFetch", "owned_by": v}),
                        ]
                    })
                    .collect();
                (200, json!({"object": "list", "data": data}))
            } else {
                let asked = body_of(&request);
                let vendor = asked["provider"]
                    .as_str()
                    .or(asked["model"].as_str())
                    .unwrap_or("")
                    .to_string();
                if !listed.contains(&vendor.as_str()) && !unlisted.contains(&vendor.as_str()) {
                    (
                        400,
                        json!({"error": {"message": format!("Unknown provider: {vendor}"), "code": 400}}),
                    )
                } else if request.starts_with("POST /v1/search") {
                    (
                        200,
                        json!({"provider": vendor, "query": asked["query"], "results": [
                            {"title": format!("found by {vendor}"), "url": "https://example.com/r",
                             "snippet": "a <strong>result</strong>"}
                        ], "answer": null}),
                    )
                } else if vendor == "rawhtml" {
                    (
                        200,
                        json!({"provider": vendor, "url": asked["url"], "content": {"format": "markdown",
                            "text": "<!DOCTYPE html><html><head><title>Raw page</title></head><body><nav>menu</nav><main><p>Hello <b>world</b> from the page.</p></main></body></html>"}}),
                    )
                } else {
                    (
                        200,
                        json!({"provider": vendor, "url": asked["url"], "title": "Read",
                            "content": {"format": "markdown", "text": format!("read by {vendor}")}}),
                    )
                }
            };
            respond(stream, status, &body);
        }
    });
    Vendor { url, asked }
}

#[test]
fn account_access_saves_without_request_and_tools_prove_exact_gateway_targets() {
    let router = nine_router(&["exa", "glm"], &["ag"]);
    let mut h = harness(
        "account-tools",
        json!([
            {"chunks":[search_call("s1")]}, {"chunks":[{"text":"done"}]}
        ]),
    );
    let base = format!("{}/v1", router.url);
    let saved = h
        .ask(Command::ServiceAccount {
            id: 20,
            name: "9router".into(),
            api: Some("9router".into()),
            base_url: Some(base.clone()),
            fetch_url: None,
            key_name: Some("Primary".into()),
            key: Some("fixture-key".into()),
        })
        .unwrap();
    assert_eq!(saved["checked"], json!([]));
    assert!(
        router.asked.lock().unwrap().is_empty(),
        "saving access sends no request"
    );
    assert!(!h.rune_toml().contains("fixture-key"));
    assert!(saved["registry"]["web"]["search"]
        .as_str()
        .unwrap()
        .starts_with("unavailable"));
    h.router(21, &base, None, None, Some((&["ag", "exa"], &[])))
        .unwrap();
    let asked = router.asked.lock().unwrap().clone();
    assert_eq!(
        asked.len(),
        2,
        "each addition proves exactly its own route, no catalog"
    );
    assert!(asked.iter().all(|r| r.starts_with("POST /v1/search")));
    assert!(
        !h.rune_toml().contains("[web_search]"),
        "adding keeps, not selects"
    );
    let selected = h
        .ask(Command::ServiceSelect {
            id: 22,
            service: "search".into(),
            target: Some("9router:exa".into()),
        })
        .unwrap();
    assert_eq!(selected["selected"], "9router:exa");
    assert_eq!(
        body_of(router.asked.lock().unwrap().last().unwrap())["model"],
        "exa"
    );
    let previous = h.rune_toml();
    assert!(h
        .ask(Command::ServiceSelect {
            id: 23,
            service: "search".into(),
            target: Some("9router:missing".into()),
        })
        .is_err());
    assert_eq!(h.rune_toml(), previous);
    let events = h.prompt("search");
    assert!(result(&events, "s1").0);
    assert_eq!(
        body_of(router.asked.lock().unwrap().last().unwrap())["model"],
        "exa"
    );
    assert!(status_sent(&h).contains("web search: 9router (exa)"));
    h.router(24, &base, None, None, Some((&[], &[]))).unwrap();
    assert!(
        h.rune_toml().contains("name = \"9router\""),
        "last provider removed keeps account"
    );
}

#[test]
fn a_failed_selection_keeps_the_previous_pin_and_sends_only_the_requested_target() {
    let vendor = vendor(vec![
        (200, json!({"results":[]})),
        (200, json!({"results":[]})),
        (401, json!({"error":{"message":"rejected"}})),
    ]);
    let mut h = harness("select-failure", json!([]));
    h.router(
        30,
        &format!("{}/v1", vendor.url),
        None,
        None,
        Some((&["ag", "exa"], &[])),
    )
    .unwrap();
    // A raw file choice is permitted, but the Settings selection must prove it.
    runeengine::settings::configuration::Configuration::edit(
        &h.data,
        Some("web_search.provider"),
        Some(json!("9router:ag")),
        false,
        None,
    )
    .unwrap();
    let before = h.rune_toml();
    assert!(h
        .ask(Command::ServiceSelect {
            id: 31,
            service: "search".into(),
            target: Some("9router:exa".into()),
        })
        .is_err());
    assert_eq!(h.rune_toml(), before);
    assert_eq!(
        body_of(vendor.asked.lock().unwrap().last().unwrap())["model"],
        "exa"
    );
}

fn tavily_found(title: &str) -> Value {
    json!({"query":"q","results":[
        {"title": title, "url": "https://example.com/found", "content": "a result", "score": 0.9}
    ]})
}

fn search_call(id: &str) -> Value {
    json!({ "tool_call": { "id": id, "name": "web_search",
                           "args": { "action": "look it up", "query": "rust landlock" } } })
}

fn result(events: &[Event], id: &str) -> (bool, String) {
    events
        .iter()
        .find_map(|e| match e {
            Event::ToolResult {
                id: got,
                ok,
                content,
                ..
            } if got == id => Some((*ok, content.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

fn logs(events: &[Event]) -> Vec<LogEntry> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Log { entry, .. } => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

/// The status block the model was handed at the opening of its context.
fn status_sent(h: &Harness) -> String {
    h.model
        .sent()
        .iter()
        .flat_map(|s| s.messages.clone())
        .filter(|m| m.contains("<status>") || m.contains("<status_update>"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_search_connection_is_proven_before_it_is_saved_and_then_serves_the_model() {
    let tavily = vendor(vec![
        (200, tavily_found("the check")),
        (200, tavily_found("Landlock docs")),
    ]);
    let mut h = harness(
        "serves",
        json!([
            { "chunks": [ search_call("s1") ] },
            { "chunks": [ { "text": "Found it." } ] }
        ]),
    );
    let saved = h
        .save(10, "tavily", &tavily.url, true, false, "tvly-test")
        .unwrap();
    assert_eq!(saved["checked"][0]["service"], "search", "{saved}");
    {
        let asked = tavily.asked.lock().unwrap();
        assert_eq!(asked.len(), 1, "one real request proved it");
        assert!(asked[0].starts_with("POST /search"), "{}", asked[0]);
        assert!(asked[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer tvly-test"));
        assert!(asked[0].contains("rune connection check"));
    }
    assert!(
        h.rune_toml().contains("[[services.list]]"),
        "{}",
        h.rune_toml()
    );

    let events = h.prompt("search for landlock");
    let (ok, content) = result(&events, "s1");
    assert!(ok, "{content}");
    assert!(
        content.contains("- Landlock docs\n  https://example.com/found"),
        "{content}"
    );
    assert!(
        status_sent(&h).contains("web search: tavily"),
        "the model is told who serves: {}",
        status_sent(&h)
    );
    let logged = logs(&events);
    assert!(
        logged.iter().any(|l| l.source == "web_search"
            && l.code == "search"
            && l.message.starts_with("tavily · 1 result(s)")),
        "{logged:?}"
    );
}

#[test]
fn a_refused_check_saves_nothing_and_puts_the_ring_back() {
    let tavily = vendor(vec![(
        401,
        json!({"detail":{"error":"Unauthorized: missing or invalid API key."}}),
    )]);
    let mut h = harness("refused", json!([]));
    let error = h
        .save(10, "tavily", &tavily.url, true, false, "tvly-wrong")
        .unwrap_err();
    assert!(error.contains("tavily search check failed"), "{error}");
    assert!(
        error.contains("401") || error.contains("API key"),
        "{error}"
    );
    assert!(!h.rune_toml().contains("services"), "{}", h.rune_toml());
    assert!(
        h.ask(Command::ProviderKeys {
            id: 11,
            name: "tavily".into()
        })
        .is_err(),
        "the key given for the failed check is not kept"
    );
}

#[test]
fn with_no_search_connection_the_model_is_told_and_a_call_is_not_executed() {
    let mut h = harness(
        "none",
        json!([
            { "chunks": [ search_call("s1") ] },
            { "chunks": [ { "text": "No search." } ] }
        ]),
    );
    let events = h.prompt("search something");
    let (ok, content) = result(&events, "s1");
    assert!(!ok);
    assert!(
        content.contains("not_executed") || content.contains("not executed"),
        "{content}"
    );
    assert!(content.contains("Settings › Connections"), "{content}");
    assert!(
        content.contains("Settings › Tools › Web search › Provider"),
        "{content}"
    );
    assert!(
        status_sent(&h).contains("web search: unavailable — no search provider"),
        "{}",
        status_sent(&h)
    );
    assert!(
        status_sent(&h).contains("web fetch: built-in reader"),
        "{}",
        status_sent(&h)
    );
    assert!(logs(&events)
        .iter()
        .any(|l| l.source == "web_search" && l.code == "search_unavailable"));
}

#[test]
fn a_connection_added_mid_session_reaches_the_model_as_a_status_update() {
    let tavily = vendor(vec![(200, tavily_found("check"))]);
    let mut h = harness(
        "update",
        json!([
            { "chunks": [ { "text": "first" } ] },
            { "chunks": [ { "text": "second" } ] }
        ]),
    );
    h.prompt("hello");
    h.save(10, "tavily", &tavily.url, true, false, "tvly-test")
        .unwrap();
    h.prompt("again");
    let last = h.model.last_sent().unwrap();
    let update = last
        .messages
        .iter()
        .find(|m| m.contains("<status_update>"))
        .unwrap_or_else(|| panic!("{:?}", last.messages));
    assert!(update.contains("web search"), "{update}");
    assert!(update.contains("tavily"), "{update}");
}

#[test]
fn a_failing_search_is_retried_on_the_curve_and_every_step_is_logged() {
    let tavily = vendor(vec![
        (200, tavily_found("check")),
        (500, json!({"detail":"upstream down"})),
        (200, tavily_found("Back")),
    ]);
    let mut h = harness(
        "retry",
        json!([
            { "chunks": [ search_call("s1") ] },
            { "chunks": [ { "text": "ok" } ] }
        ]),
    );
    h.save(10, "tavily", &tavily.url, true, false, "tvly-test")
        .unwrap();
    let events = h.prompt("search");
    let (ok, content) = result(&events, "s1");
    assert!(ok, "{content}");
    let logged = logs(&events);
    let retry = logged
        .iter()
        .find(|l| l.code == "search_retry")
        .unwrap_or_else(|| panic!("{logged:?}"));
    assert!(
        retry.message.contains("tavily · Server error (HTTP 500)"),
        "{}",
        retry.message
    );
    assert_eq!(
        retry.detail.as_ref().unwrap()["host"],
        tavily.url.trim_start_matches("http://")
    );
}

#[test]
fn fetch_through_a_connection_refuses_private_pages_first_and_logs_what_it_read() {
    let tavily = vendor(vec![
        (
            200,
            json!({"results":[{"url":"https://example.com/","raw_content":"Example Domain"}]}),
        ),
        (
            200,
            json!({"results":[{"url":"https://93.184.215.14/","raw_content":"Read through Tavily"}]}),
        ),
    ]);
    let mut h = harness(
        "fetch",
        json!([
            { "chunks": [
                { "tool_call": { "id": "f1", "name": "web_fetch",
                                 "args": { "action": "read", "url": "http://127.0.0.1:9/private" } } },
                { "tool_call": { "id": "f2", "name": "web_fetch",
                                 "args": { "action": "read", "url": "https://93.184.215.14/" } } }
            ] },
            { "chunks": [ { "text": "done" } ] }
        ]),
    );
    h.save(10, "tavily", &tavily.url, false, true, "tvly-test")
        .unwrap();
    runeengine::settings::configuration::Configuration::edit(
        &h.data,
        Some("web_fetch.provider"),
        Some(json!("tavily")),
        false,
        None,
    )
    .unwrap();
    let events = h.prompt("read these");
    let (ok, content) = result(&events, "f1");
    assert!(!ok && content.contains("internal/private"), "{content}");
    let (ok, content) = result(&events, "f2");
    assert!(ok, "{content}");
    assert_eq!(content.trim(), "Read through Tavily");
    assert_eq!(
        tavily.asked.lock().unwrap().len(),
        2,
        "the private page never reached the service"
    );
    assert!(
        status_sent(&h).contains("web fetch: tavily"),
        "{}",
        status_sent(&h)
    );
    assert!(logs(&events)
        .iter()
        .any(|l| l.source == "web_fetch" && l.code == "fetch" && l.message.starts_with("tavily")));
}

#[test]
fn a_pinned_connection_that_is_removed_is_unavailable_not_replaced() {
    let tavily = vendor(vec![(200, tavily_found("check"))]);
    let exa = vendor(vec![(
        200,
        json!({"results":[{"title":"t","url":"https://u","highlights":["h"]}]}),
    )]);
    let mut h = harness(
        "pin",
        json!([
            { "chunks": [ search_call("s1") ] },
            { "chunks": [ { "text": "ok" } ] }
        ]),
    );
    h.save(10, "tavily", &tavily.url, true, false, "tvly-test")
        .unwrap();
    h.save(11, "exa", &exa.url, true, false, "exa-test")
        .unwrap();
    runeengine::settings::configuration::Configuration::edit(
        &h.data,
        Some("web_search.provider"),
        Some(json!("tavily")),
        false,
        None,
    )
    .unwrap();
    h.ask(Command::ServiceRemove {
        id: 12,
        name: "tavily".into(),
    })
    .unwrap();
    let events = h.prompt("search");
    let (ok, content) = result(&events, "s1");
    assert!(!ok, "exa must not serve in tavily's place: {content}");
    assert!(
        content.contains("web_search.provider names tavily"),
        "{content}"
    );
    assert_eq!(exa.asked.lock().unwrap().len(), 1, "only exa's own check");
    // The key stays for a connection re-added later.
    assert!(h
        .ask(Command::ProviderKeys {
            id: 13,
            name: "tavily".into()
        })
        .is_ok());
}

#[test]
fn a_gateway_lists_routes_as_its_doors_take_them_and_a_copied_spelling_is_kept_as_one() {
    let router = nine_router(&["exa", "glm"], &[]);
    let mut h = harness("router", json!([]));
    let base = format!("{}/v1", router.url);
    let options = h
        .ask(Command::ServiceOptions {
            id: 10,
            name: "9router".into(),
            api: None,
            base_url: Some(base.clone()),
            key: Some("r-key".into()),
        })
        .unwrap();
    assert_eq!(options["routes"]["search"], json!(["exa", "glm"]));
    assert_eq!(options["routes"]["fetch"], json!(["exa", "glm"]));
    let saved = h.router(11, &base, Some("exa/search"), None, None);
    assert!(saved.is_ok(), "{saved:?}");
    assert_eq!(saved.unwrap()["checked"][0]["route"], "exa");
    let asked = router.asked.lock().unwrap().clone();
    assert!(asked[0].starts_with("GET /v1/models/web"), "{}", asked[0]);
    assert!(asked[1].starts_with("POST /v1/search"), "{}", asked[1]);
    assert_eq!(body_of(&asked[1])["model"], "exa", "{}", asked[1]);
    let toml = h.rune_toml();
    assert!(
        toml.contains("route = \"exa\"") && toml.contains("search = [\"exa\"]"),
        "kept as the router takes it: {toml}"
    );
}

#[test]
fn routes_added_are_kept_to_switch_back_to_and_each_step_is_proven_once() {
    let router = nine_router(&["exa", "glm"], &["ag"]);
    let mut h = harness(
        "kept",
        json!([
            { "chunks": [ search_call("s1") ] },
            { "chunks": [ { "text": "ok" } ] }
        ]),
    );
    let base = format!("{}/v1", router.url);
    let count = |router: &Vendor| router.asked.lock().unwrap().len();
    h.router(10, &base, Some("exa"), None, None).unwrap();
    assert_eq!(count(&router), 1, "switched on: one search through exa");

    // Added is only kept: proven through itself, the route in use stays.
    let added = h
        .router(11, &base, Some("exa"), None, Some((&["exa", "ag"], &[])))
        .unwrap();
    assert_eq!(added["checked"].as_array().unwrap().len(), 1, "{added}");
    assert_eq!(added["checked"][0]["route"], "ag");
    assert_eq!(count(&router), 2);
    assert_eq!(
        body_of(&router.asked.lock().unwrap()[1])["model"],
        "ag",
        "a route the router serves without listing is proven like any other"
    );
    let toml = h.rune_toml();
    assert!(
        toml.contains("route = \"exa\"") && toml.contains("search = [\"exa\", \"ag\"]"),
        "{toml}"
    );

    // A route the router refuses is not kept, and says which one failed.
    let refused = h
        .router(
            12,
            &base,
            Some("exa"),
            None,
            Some((&["exa", "ag", "nope/search"], &[])),
        )
        .unwrap_err();
    assert!(
        refused.contains("through nope") && refused.contains("Unknown provider: nope"),
        "{refused}"
    );
    assert!(!h.rune_toml().contains("nope"), "{}", h.rune_toml());

    // Using a kept route is a step of its own, proven again.
    let switched = h.router(13, &base, Some("ag"), None, None).unwrap();
    assert_eq!(switched["checked"][0]["route"], "ag", "{switched}");
    assert_eq!(count(&router), 4);
    let toml = h.rune_toml();
    assert!(
        toml.contains("route = \"ag\"") && toml.contains("search = [\"exa\", \"ag\"]"),
        "the route used before is still kept: {toml}"
    );

    // Taking a route off the list proves nothing.
    let removed = h
        .router(14, &base, Some("ag"), None, Some((&["ag"], &[])))
        .unwrap();
    assert!(
        removed["checked"].as_array().unwrap().is_empty(),
        "{removed}"
    );
    assert_eq!(count(&router), 4);
    assert!(!h.rune_toml().contains("\"exa\""), "{}", h.rune_toml());

    // The model searches through the route in use.
    let events = h.prompt("search");
    let (ok, content) = result(&events, "s1");
    assert!(ok, "{content}");
    assert!(content.contains("- found by ag"), "{content}");
    assert!(
        content.contains("a result") && !content.contains("<strong>"),
        "a vendor's markup reaches the model as words: {content}"
    );
    assert!(
        status_sent(&h).contains("web search: 9router (ag)"),
        "{}",
        status_sent(&h)
    );

    // Switched off, the kept routes stay the person's.
    h.router(15, &base, None, None, None).unwrap();
    let toml = h.rune_toml();
    assert!(
        toml.contains("search = [\"ag\"]") && !toml.contains("[services.list.search]"),
        "{toml}"
    );
}

#[test]
fn a_fetch_connection_switched_on_reads_nothing_until_tools_names_it() {
    let router = nine_router(&["exa"], &["rawhtml"]);
    let mut h = harness(
        "fetchpin",
        json!([
            { "chunks": [ { "text": "first" } ] },
            { "chunks": [
                { "tool_call": { "id": "f1", "name": "web_fetch",
                                 "args": { "action": "read", "url": "https://93.184.215.14/" } } }
            ] },
            { "chunks": [ { "text": "done" } ] }
        ]),
    );
    let base = format!("{}/v1", router.url);
    h.router(10, &base, None, Some("rawhtml"), None).unwrap();
    h.prompt("hello");
    assert!(
        status_sent(&h).contains("web fetch: built-in reader"),
        "switched on is not chosen: {}",
        status_sent(&h)
    );
    runeengine::settings::configuration::Configuration::edit(
        &h.data,
        Some("web_fetch.provider"),
        Some(json!("9router:rawhtml")),
        false,
        None,
    )
    .unwrap();
    let before = router.asked.lock().unwrap().len();
    let events = h.prompt("read it");
    let (ok, content) = result(&events, "f1");
    assert!(ok, "{content}");
    assert_eq!(router.asked.lock().unwrap().len(), before + 1);
    assert!(
        content.contains("Hello **world** from the page.")
            || content.contains("Hello world from the page."),
        "a page the vendor handed back as HTML is read by Rune's own parser: {content}"
    );
    assert!(
        !content.contains("<p>") && !content.contains("<b>"),
        "{content}"
    );
    assert!(content.starts_with("# Raw page"), "{content}");
    assert!(
        status_sent(&h).contains("web fetch: 9router (rawhtml)"),
        "{}",
        status_sent(&h)
    );
}

#[test]
fn a_key_is_never_saved_for_plain_http_to_another_machine() {
    let mut h = harness("clear", json!([]));
    let error = h
        .save(
            10,
            "tavily",
            "http://search.example",
            true,
            false,
            "tvly-test",
        )
        .unwrap_err();
    assert!(error.contains("plain http://"), "{error}");
    assert!(!h.rune_toml().contains("services"));
}

#[test]
fn a_model_connections_address_is_proven_before_it_changes() {
    let first = vendor(vec![
        (200, json!({"data":[{"id":"m1"}]})),
        (200, json!({"data":[{"id":"m1"}]})),
    ]);
    let mut h = harness("baseurl", json!([]));
    h.ask(Command::ProviderAdd {
        id: 10,
        name: "local".into(),
        preset: None,
        base_url: Some(format!("{}/v1", first.url)),
        key_name: None,
        key: Some("k".into()),
        model: None,
    })
    .unwrap();
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/v1", l.local_addr().unwrap())
    };
    let error = h
        .ask(Command::ProviderBaseUrl {
            id: 11,
            name: "local".into(),
            base_url: Some(dead.clone()),
        })
        .unwrap_err();
    assert!(!error.is_empty());
    assert!(
        !h.rune_toml().contains(&dead),
        "a refused address is not saved"
    );
    let moved = h
        .ask(Command::ProviderBaseUrl {
            id: 12,
            name: "local".into(),
            base_url: Some(format!("{}/v1/", first.url)),
        })
        .unwrap();
    assert_eq!(moved["base_url"], format!("{}/v1", first.url));
}
