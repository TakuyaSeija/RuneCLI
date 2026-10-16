//! Serper: Google results as JSON (`POST /search`, `X-API-KEY`). The connection
//! Rune searched with before search connections existed; format 17 carries
//! its key over as this connection.

use serde_json::json;

use super::{
    first_text, header, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key,
    Read, Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "serper",
    title: "Serper",
    site: "serper.dev",
    hosts: Hosts::Shared(Some("https://google.serper.dev")),
    key: Key::Required,
    key_hint: "serper.dev API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 100,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/search", call.base),
        header("X-API-KEY", Some(need_key(call)?)),
        json!({ "q": query, "num": count }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: first_text(data, &["/answerBox/answer", "/answerBox/snippet"]),
        about: first_text(data, &["/knowledgeGraph/description"]),
        hits: hits(
            items(data, "/organic"),
            &["/title"],
            &["/link"],
            &["/snippet", "/description"],
            &["/date"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn a_direct_answer_and_the_knowledge_panel_come_with_the_results() {
        let call = Call {
            base: "https://google.serper.dev",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 8).unwrap();
        assert_eq!(r.url, "https://google.serper.dev/search");
        assert_eq!(r.body.unwrap(), json!({"q":"rust","num":8}));
        assert!(r.headers.contains(&("X-API-KEY".into(), "k".into())));
        let Read::Found(f) = found(&reply(json!({
            "answerBox": {"snippet": "5.13"},
            "knowledgeGraph": {"description": "A language"},
            "organic": [{"title":"Landlock","link":"https://landlock.io","snippet":"LSM","date":"2026"}, {}]
        })))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.answer.as_deref(), Some("5.13"));
        assert_eq!(f.about.as_deref(), Some("A language"));
        assert_eq!(f.hits.len(), 1, "a result with no link is no result");
    }
}
