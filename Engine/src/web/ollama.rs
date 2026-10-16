//! Ollama's web search and web fetch, on ollama.com with an ollama.com key —
//! not the local model server, whose address a model connection named
//! `ollama` usually holds. Shapes from Ollama's API docs (`/api/web_search`,
//! `/api/web_fetch`), as 9router 0.5.99 sends them.

use serde_json::json;

use super::{
    bearer, first_text, hits, items, json as body, need_key, Adapter, Call, FetchApi, Found, Hosts,
    Key, Page, Read, Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "ollama",
    title: "Ollama Web Search",
    site: "ollama.com/settings/keys",
    hosts: Hosts::Shared(Some("https://ollama.com")),
    key: Key::Required,
    key_hint: "ollama.com API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: Some(FetchApi {
        build: fetch,
        read: page,
    }),
    max_results: 10,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/api/web_search", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "query": query, "max_results": count }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/results"),
            &["/title"],
            &["/url"],
            &["/content", "/snippet"],
            &["/published_at"],
        ),
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/api/web_fetch", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "url": url }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    Ok(Page {
        title: first_text(data, &["/title"]),
        text: first_text(data, &["/content"]).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_and_fetch_reach_ollama_com() {
        let call = Call {
            base: "https://ollama.com",
            key: Some("k"),
            route: None,
            zone: None,
        };
        assert_eq!(
            search(&call, "q", 2).unwrap().url,
            "https://ollama.com/api/web_search"
        );
        assert_eq!(
            fetch(&call, "https://example.com/").unwrap().url,
            "https://ollama.com/api/web_fetch"
        );
        let Read::Found(f) = found(&reply(
            json!({"results":[{"title":"T","url":"https://u","content":"c"}]}),
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "c");
        let p = page(&reply(json!({"title":"E","content":"body","links":[]}))).unwrap();
        assert_eq!(p.text, "body");
    }
}
