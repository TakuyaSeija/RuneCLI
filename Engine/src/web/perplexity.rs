//! Perplexity Search API: ranked web results. Shape from Perplexity's API
//! reference (`POST /search`, 2026-10): `max_results` 1–20 for web search.

use serde_json::json;

use super::{
    bearer, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read, Request,
    SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "perplexity",
    title: "Perplexity Search",
    site: "perplexity.ai/account/api",
    hosts: Hosts::Shared(Some("https://api.perplexity.ai")),
    key: Key::Required,
    key_hint: "Perplexity API key (pplx-…)",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
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
            &["/snippet"],
            &["/date", "/last_updated"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_reads_title_url_snippet_and_date() {
        let call = Call {
            base: "https://api.perplexity.ai",
            key: Some("pplx"),
            route: None,
            zone: None,
        };
        let r = search(&call, "q", 9).unwrap();
        assert_eq!(r.url, "https://api.perplexity.ai/search");
        assert_eq!(r.body.unwrap(), json!({"query":"q","max_results":9}));
        let Read::Found(f) = found(&reply(json!({"id":"x","results":[
            {"title":"T","url":"https://u","snippet":"s","date":null,"last_updated":"2026-09-01"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].date.as_deref(), Some("2026-09-01"));
    }
}
