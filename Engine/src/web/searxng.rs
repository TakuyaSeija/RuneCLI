//! SearXNG: a metasearch server of the person's own. It knows no keys, and it
//! has no address of its own — the person names theirs. Asked as its search
//! API documents (`GET /search?q=…&format=json`); JSON is served only when
//! `json` is listed under `search: formats:` in its `settings.yml` (403
//! otherwise), and its limiter, when on, can refuse clients that are not
//! browsers.

use super::{
    enc, first_text, hits, items, json as body, Adapter, Call, Found, Hosts, Key, Read, Request,
    SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "searxng",
    title: "SearXNG",
    site: "your own SearXNG server",
    hosts: Hosts::Shared(None),
    key: Key::None,
    key_hint: "SearXNG needs no key — give the address of your server",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 50,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: Some(
        "SearXNG answers JSON only when `json` is listed under `search: formats:` in its \
         settings.yml, and its limiter may refuse clients that are not browsers (turn \
         `server: limiter` off or pass this machine's address)",
    ),
};

fn search(call: &Call, query: &str, _count: u64) -> Result<Request> {
    let path = if call.base.ends_with("/search") {
        call.base.to_string()
    } else {
        format!("{}/search", call.base)
    };
    Ok(Request::get(
        format!("{path}?q={}&format=json&categories=general", enc(query)),
        vec![("Accept".into(), "application/json".into())],
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: data
            .get("answers")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| {
                a.iter().find_map(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| first_text(v, &["/answer"]))
                })
            })
            .filter(|a| !a.trim().is_empty()),
        about: None,
        hits: hits(
            items(data, "/results"),
            &["/title"],
            &["/url"],
            &["/content", "/snippet"],
            &["/publishedDate", "/published_date"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;
    use serde_json::json;

    #[test]
    fn the_search_path_is_added_once_and_answers_are_read() {
        for base in ["http://localhost:8888", "http://localhost:8888/search"] {
            let call = Call {
                base,
                key: None,
                route: None,
                zone: None,
            };
            assert_eq!(
                search(&call, "rust lang", 5).unwrap().url,
                "http://localhost:8888/search?q=rust%20lang&format=json&categories=general"
            );
        }
        let Read::Found(f) = found(&reply(json!({"query":"q","answers":[{"answer":"42"}],
            "results":[{"title":"T","url":"https://u","content":"c","engine":"duckduckgo"}]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.answer.as_deref(), Some("42"));
        assert_eq!(f.hits[0].snippet, "c");
    }
}
