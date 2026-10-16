//! Serpex: search for agents (`POST /api/search`, Bearer). Shape from
//! Serpex's docs (2026-10): `results` with `title`, `url`, `snippet`.

use serde_json::json;

use super::{
    bearer, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read, Request,
    SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "serpex",
    title: "Serpex",
    site: "serpex.dev/dashboard",
    hosts: Hosts::Shared(Some("https://api.serpex.dev")),
    key: Key::Required,
    key_hint: "Serpex API key (sk_…)",
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
    Ok(Request::post(
        format!("{}/api/search", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "q": query }),
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
            &["/published_date", "/date"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_reads_results() {
        let call = Call {
            base: "https://api.serpex.dev",
            key: Some("sk_k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 10).unwrap();
        assert_eq!(r.url, "https://api.serpex.dev/api/search");
        assert_eq!(r.body.unwrap(), json!({"q":"rust"}));
        let Read::Found(f) = found(&reply(json!({"results":[
            {"title":"T","url":"https://u","snippet":"s","position":1,"engine":"google"}
        ],"metadata":{"credits_used":1}})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits.len(), 1);
    }
}
