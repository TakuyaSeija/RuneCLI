use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::*;
use crate::settings::configuration::{Configuration, Shared};
use crate::settings::services::Services;

/// A 200 answer carrying `body`.
pub(crate) fn reply(body: Value) -> Response {
    Response {
        status: 200,
        text: body.to_string(),
        body,
        content_type: None,
        location: None,
        set_cookies: Vec::new(),
    }
}

/// Settings holding `services`, pinned as `config` says, with a key for each
/// name in `keys`.
fn settings(config: Value, services: Value, keys: &[&str]) -> Settings {
    let mut s = Settings::empty();
    s.config = Shared::new(Configuration::from_values(config).unwrap());
    s.services = serde_json::from_value::<Services>(services).unwrap();
    for name in keys {
        s.secrets.keys_for_test(name, "test-key");
    }
    s
}

fn ready(choice: Choice) -> Route {
    match choice {
        Choice::Ready(route) => route,
        other => panic!("{other:?}"),
    }
}

fn unavailable(choice: Choice) -> String {
    match choice {
        Choice::Unavailable(why) => why,
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_adapter_is_listed_once_and_serves_what_it_says() {
    let mut ids = std::collections::HashSet::new();
    for a in ADAPTERS {
        assert!(ids.insert(a.id), "{} twice", a.id);
        assert!(
            a.search.is_some() || a.fetch.is_some(),
            "{} serves nothing",
            a.id
        );
        assert!(a.max_results >= 1, "{}", a.id);
        for service in [Service::Search, Service::Fetch] {
            if a.serves(service) && a.key != Key::None {
                let base = a
                    .default_base(service)
                    .expect("a keyed vendor has an address");
                assert!(
                    base.starts_with("https://") || a.id == "9router",
                    "{} {base}",
                    a.id
                );
            }
        }
    }
    assert!(adapter("serpdive").is_none(), "owner, 2026-10-14: left out");
}

#[test]
fn search_goes_to_the_first_added_unless_one_is_pinned() {
    let services = json!({"list":[
        {"name":"tavily","api":"tavily","search":{"added":20}},
        {"name":"exa","api":"exa","search":{"added":10}}
    ]});
    let s = settings(json!({}), services.clone(), &["tavily", "exa"]);
    assert_eq!(ready(choose(&s, Service::Search)).name, "exa");
    let s = settings(
        json!({"web_search":{"provider":"tavily"}}),
        services.clone(),
        &["tavily", "exa"],
    );
    let route = ready(choose(&s, Service::Search));
    assert_eq!(route.name, "tavily");
    assert_eq!(route.base, "https://api.tavily.com");
    assert_eq!(route.secret.as_deref(), Some("tavily"));
    assert_eq!(status(&s, Service::Search), "tavily");
}

#[test]
fn a_pin_to_a_connection_that_is_gone_is_unavailable_never_another() {
    let s = settings(
        json!({"web_search":{"provider":"brave"}}),
        json!({"list":[{"name":"exa","api":"exa","search":{"added":1}}]}),
        &["exa"],
    );
    let why = unavailable(choose(&s, Service::Search));
    assert!(why.contains("web_search.provider names brave"), "{why}");
    assert!(status(&s, Service::Search).starts_with("unavailable — "));
}

#[test]
fn with_nothing_configured_search_says_how_and_fetch_reads_itself() {
    let s = settings(json!({}), json!({}), &[]);
    let why = unavailable(choose(&s, Service::Search));
    assert!(why.contains("Settings › Connections"), "{why}");
    assert!(matches!(choose(&s, Service::Fetch), Choice::Builtin));
    assert_eq!(status(&s, Service::Fetch), "built-in reader");
}

#[test]
fn a_fetch_connection_reads_pages_only_once_it_is_pinned() {
    // Owner, 2026-10-15: switching a connection's fetch on makes it one to
    // choose; the built-in reader serves until Tools › Web fetch names it.
    let services = json!({"list":[
        {"name":"tavily","api":"tavily","search":{"added":1},"fetch":{"added":2}}
    ]});
    let s = settings(json!({}), services.clone(), &["tavily"]);
    assert!(matches!(choose(&s, Service::Fetch), Choice::Builtin));
    assert_eq!(status(&s, Service::Fetch), "built-in reader");
    assert_eq!(ready(choose(&s, Service::Search)).name, "tavily");
    let s = settings(
        json!({"web_fetch":{"provider":"tavily"}}),
        services,
        &["tavily"],
    );
    assert_eq!(ready(choose(&s, Service::Fetch)).name, "tavily");
    assert_eq!(status(&s, Service::Fetch), "tavily");
}

#[test]
fn a_keyed_connection_without_a_key_is_unavailable_and_says_where_to_add_one() {
    let s = settings(
        json!({}),
        json!({"list":[{"name":"exa","api":"exa","search":{"added":1}}]}),
        &[],
    );
    let why = unavailable(choose(&s, Service::Search));
    assert!(
        why.contains("Key missing") && why.contains("`exa`"),
        "{why}"
    );
    assert!(why.contains("Settings › Connections › exa"), "{why}");
}

#[test]
fn nine_router_reaches_the_model_connections_address_with_its_one_key() {
    let mut s = settings(
        json!({}),
        json!({"list":[{"name":"9router","api":"9router",
            "search":{"added":1,"route":"exa"},"fetch":{"added":2,"route":"tavily"},
            "routes":{"search":["exa"],"fetch":["tavily"]}}]}),
        &["9router"],
    );
    let mut model = crate::settings::providers::preset("9router")
        .unwrap()
        .build("");
    model.base_url = "http://127.0.0.1:30000/v1".into();
    s.providers.list.push(model);
    let route = ready(choose(&s, Service::Search));
    assert_eq!(route.base, "http://127.0.0.1:30000/v1");
    assert_eq!(route.route.as_deref(), Some("exa"));
    assert_eq!(route.secret.as_deref(), Some("9router"));
    assert_eq!(status(&s, Service::Search), "9router (exa)");
    let s = settings(
        json!({"web_fetch":{"provider":"9router:tavily"}}),
        json!({"list":[{"name":"9router","api":"9router",
            "fetch":{"added":2,"route":"tavily"},"routes":{"fetch":["tavily"]}}]}),
        &[],
    );
    let route = ready(choose(&s, Service::Fetch));
    assert_eq!(
        route.base, "http://localhost:20128/v1",
        "the vendor's own address"
    );
    assert_eq!(
        route.secret, None,
        "9router without authentication needs no key"
    );
}

#[test]
fn split_vendors_keep_search_and_fetch_addresses_apart() {
    let s = settings(
        json!({"web_fetch":{"provider":"jina"}}),
        json!({"list":[{"name":"jina","api":"jina","fetch_url":"https://reader.example",
            "search":{"added":1},"fetch":{"added":1}}]}),
        &["jina"],
    );
    assert_eq!(ready(choose(&s, Service::Search)).base, "https://s.jina.ai");
    assert_eq!(
        ready(choose(&s, Service::Fetch)).base,
        "https://reader.example"
    );
}

#[test]
fn a_key_is_never_sent_in_clear_to_another_machine() {
    let s = settings(
        json!({}),
        json!({"list":[{"name":"9router","api":"9router","base_url":"http://192.168.1.9:20128/v1",
            "search":{"added":1,"route":"exa"},"routes":{"search":["exa"]}}]}),
        &["9router"],
    );
    let why = unavailable(choose(&s, Service::Search));
    assert!(why.contains("plain http://"), "{why}");
    assert!(key_may_travel("https://x.example"));
    assert!(key_may_travel("http://localhost:1"));
    assert!(!key_may_travel("http://x.example"));
}

#[test]
fn the_rendering_is_one_shape_whoever_answered() {
    let lines = render(&Found {
        answer: Some("42".into()),
        about: Some("A number".into()),
        hits: vec![
            Hit {
                title: "T".into(),
                url: "https://u".into(),
                snippet: "s".into(),
                date: Some("2026".into()),
            },
            Hit {
                title: "(untitled)".into(),
                url: "https://v".into(),
                ..Hit::default()
            },
        ],
    });
    assert_eq!(
        lines,
        [
            "ANSWER: 42",
            "ABOUT: A number",
            "- T\n  https://u\n  (2026)\n  s",
            "- (untitled)\n  https://v"
        ]
    );
}

#[test]
fn a_refusal_names_the_next_step_for_this_vendor() {
    let tavily = adapter("tavily").unwrap();
    let e = rejected(tavily, 401, r#"{"detail":{"error":"Unauthorized"}}"#).to_string();
    assert!(e.contains("check this connection's API key"), "{e}");
    let searxng = adapter("searxng").unwrap();
    let e = rejected(searxng, 403, "<html><title>403 Forbidden</title></html>").to_string();
    assert!(e.contains("search: formats:"), "{e}");
    assert!(matches!(
        rejected(tavily, 429, ""),
        Error::Rejected { status: 429, .. }
    ));
}

/// A server on this machine answering each request with the next of
/// `answers` (status, body), and handing back what it was asked.
fn server(answers: Vec<(u16, Value)>) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in answers {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            head.push_str(&String::from_utf8_lossy(&sent));
            let _ = tx.send(head);
            let text = body.to_string();
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (url, rx)
}

fn local(api: &str, base: &str) -> Route {
    Route {
        name: api.into(),
        adapter: adapter(api).unwrap(),
        service: Service::Search,
        base: base.into(),
        secret: Some(api.into()),
        route: None,
        zone: None,
    }
}

#[test]
fn a_search_finished_later_is_asked_after_at_the_same_host_until_it_answers() {
    let (base, asked) = server(vec![
        (
            202,
            json!({"status":"accepted","data":{"status_url":"/v1/serp/tasks/t1"}}),
        ),
        (
            202,
            json!({"status":"accepted","data":{"status":"processing","status_url":"/v1/serp/tasks/t1"}}),
        ),
        (
            200,
            json!({"status":"ok","data":{"items":[
                {"type":"organic","title":"T","url":"https://u","description":"d"}
            ]}}),
        ),
    ]);
    let http = Http::new(Duration::from_secs(10));
    let found = search_once(
        &http,
        &local("serplify", &base),
        Some("live_k"),
        "rust",
        5,
        Instant::now() + Duration::from_secs(30),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(found.hits.len(), 1);
    let first = asked.recv().unwrap();
    assert!(first.starts_with("POST /v1/serp/search"), "{first}");
    assert!(first
        .to_ascii_lowercase()
        .contains("authorization: bearer live_k"));
    for _ in 0..2 {
        let poll = asked.recv().unwrap();
        assert!(poll.starts_with("GET /v1/serp/tasks/t1"), "{poll}");
    }
}

#[test]
fn a_poll_address_on_another_host_is_refused_before_the_key_goes_there() {
    let (base, _) = server(vec![(
        202,
        json!({"data":{"status_url":"https://elsewhere.example/v1/serp/tasks/t1"}}),
    )]);
    let e = search_once(
        &Http::new(Duration::from_secs(10)),
        &local("serplify", &base),
        Some("k"),
        "q",
        5,
        Instant::now() + Duration::from_secs(30),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(e.to_string().contains("another host"), "{e}");
}

#[test]
fn a_refused_search_is_a_rejection_with_its_status_and_results_are_capped() {
    let many: Vec<Value> = (0..30)
        .map(|i| json!({"title": i.to_string(), "link": format!("https://u/{i}")}))
        .collect();
    let (base, _) = server(vec![
        (401, json!({"message":"Unauthorized."})),
        (200, json!({ "organic": many })),
    ]);
    let http = Http::new(Duration::from_secs(10));
    let route = local("serper", &base);
    let deadline = Instant::now() + Duration::from_secs(30);
    let stop = AtomicBool::new(false);
    let e = search_once(&http, &route, Some("k"), "q", 5, deadline, &stop).unwrap_err();
    assert!(matches!(e, Error::Rejected { status: 401, .. }), "{e}");
    let found = search_once(&http, &route, Some("k"), "q", 5, deadline, &stop).unwrap();
    assert_eq!(found.hits.len(), 5);
}

#[test]
fn a_fetched_page_with_nothing_in_it_is_an_error_not_an_empty_success() {
    let (base, _) = server(vec![(200, json!({"results":[{"raw_content":"  "}]}))]);
    let route = Route {
        service: Service::Fetch,
        ..local("tavily", &base)
    };
    let e = fetch_once(
        &Http::new(Duration::from_secs(10)),
        &route,
        Some("k"),
        CHECK_PAGE,
    )
    .unwrap_err();
    assert!(e.to_string().contains("no content") || e.to_string().contains("no readable"));
}

#[test]
fn page_markup_in_what_a_vendor_found_reaches_the_model_as_words() {
    // Brave marks the matched words (`<strong>`), as its API answers and as
    // 9router's Brave route passes on; a page's escapes come along too.
    let (base, _) = server(vec![(
        200,
        json!({"web":{"results":[{
            "title": "The <strong>Rust</strong> Book &amp; more",
            "url": "https://doc.rust-lang.org/book/",
            "description": "Learn <strong>Rust</strong>:\n<em>ownership</em>, Vec&lt;T&gt; &#39;n all",
            "age": " 2 days ago "
        }]}}),
    )]);
    let found = search_once(
        &Http::new(Duration::from_secs(10)),
        &local("brave", &base),
        Some("k"),
        "rust",
        5,
        Instant::now() + Duration::from_secs(30),
        &AtomicBool::new(false),
    )
    .unwrap();
    let hit = &found.hits[0];
    assert_eq!(hit.title, "The Rust Book & more");
    assert_eq!(hit.snippet, "Learn Rust: ownership, Vec<T> 'n all");
    assert_eq!(hit.date.as_deref(), Some("2 days ago"));
    assert_eq!(hit.url, "https://doc.rust-lang.org/book/");
}
