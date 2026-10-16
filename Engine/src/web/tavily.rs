//! Tavily: search built for agents, and page extraction. Shapes from Tavily's
//! API reference (`/search`, `/extract`), as 9router 0.5.99 sends them.

use serde_json::json;

use super::{
    bearer, first_text, hits, items, json as body, need_key, Adapter, Call, FetchApi, Found, Hosts,
    Key, Page, Read, Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "tavily",
    title: "Tavily",
    site: "app.tavily.com",
    hosts: Hosts::Shared(Some("https://api.tavily.com")),
    key: Key::Required,
    key_hint: "Tavily API key (tvly-…)",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: Some(FetchApi {
        build: fetch,
        read: page,
    }),
    max_results: 20,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/search", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "query": query, "max_results": count, "topic": "general" }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: first_text(data, &["/answer"]),
        about: None,
        hits: hits(
            items(data, "/results"),
            &["/title"],
            &["/url"],
            &["/content"],
            &["/published_date"],
        ),
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/extract", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "urls": [url], "extract_depth": "basic" }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    if let Some(text) = first_text(data, &["/results/0/raw_content"]) {
        return Ok(Page { title: None, text });
    }
    Err(Error::Provider(
        first_text(data, &["/failed_results/0/error"])
            .map(|why| format!("Tavily could not read the page: {why}"))
            .unwrap_or_else(|| "Tavily returned no content for the page".into()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_and_extract_speak_tavilys_shapes() {
        let call = Call {
            base: "https://api.tavily.com",
            key: Some("tvly-k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 5).unwrap();
        assert_eq!(r.url, "https://api.tavily.com/search");
        assert_eq!(r.body.unwrap()["max_results"], 5);
        let Read::Found(f) = found(&reply(json!({"results":[
            {"title":"Rust","url":"https://rust-lang.org","content":"lang","published_date":"2026"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "lang");
        let r = fetch(&call, "https://example.com/").unwrap();
        assert_eq!(r.body.unwrap()["urls"], json!(["https://example.com/"]));
        assert_eq!(
            page(&reply(json!({"results":[{"raw_content":"Hello"}]})))
                .unwrap()
                .text,
            "Hello"
        );
        let e = page(&reply(
            json!({"results":[],"failed_results":[{"url":"u","error":"blocked"}]}),
        ))
        .unwrap_err();
        assert!(e.to_string().contains("blocked"), "{e}");
        assert!(search(&Call { key: None, ..call }, "q", 1).is_err());
    }
}
