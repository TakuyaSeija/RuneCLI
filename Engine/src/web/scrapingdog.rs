//! Scrapingdog's Google Search API (`GET /google/`, the key as `api_key`).
//! Shape from Scrapingdog's documentation (2026-10): ten results a page
//! (`results` no longer has any effect), `organic_results` with `link`.

use super::{
    enc, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read, Request,
    SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "scrapingdog",
    title: "Scrapingdog",
    site: "api.scrapingdog.com (dashboard)",
    hosts: Hosts::Shared(Some("https://api.scrapingdog.com")),
    key: Key::Required,
    key_hint: "Scrapingdog API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 10,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, _count: u64) -> Result<Request> {
    Ok(Request::get(
        format!(
            "{}/google/?api_key={}&query={}",
            call.base,
            enc(need_key(call)?),
            enc(query)
        ),
        vec![("Accept".into(), "application/json".into())],
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/organic_results"),
            &["/title"],
            &["/link"],
            &["/snippet"],
            &["/date"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;
    use serde_json::json;

    #[test]
    fn organic_results_are_read_by_link() {
        let call = Call {
            base: "https://api.scrapingdog.com",
            key: Some("k"),
            route: None,
            zone: None,
        };
        assert_eq!(
            search(&call, "foot ball", 10).unwrap().url,
            "https://api.scrapingdog.com/google/?api_key=k&query=foot%20ball"
        );
        let Read::Found(f) = found(&reply(json!({"organic_results":[
            {"title":"Football","link":"https://en.wikipedia.org/wiki/Football","snippet":"s","rank":1}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].title, "Football");
    }
}
