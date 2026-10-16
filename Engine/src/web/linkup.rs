//! Linkup: search for agents. Shape from Linkup's API reference (`/v1/search`
//! with `outputType: searchResults`), as 9router 0.5.99 sends it.

use serde_json::json;

use super::{
    bearer, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read, Request,
    SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "linkup",
    title: "Linkup",
    site: "app.linkup.so",
    hosts: Hosts::Shared(Some("https://api.linkup.so/v1")),
    key: Key::Required,
    key_hint: "Linkup API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 50,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/search", call.base),
        bearer(Some(need_key(call)?)),
        json!({
            "q": query,
            "depth": "standard",
            "outputType": "searchResults",
            "maxResults": count
        }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/results"),
            &["/name", "/title"],
            &["/url"],
            &["/content", "/snippet"],
            &[],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_results_are_named_and_carry_their_content() {
        let call = Call {
            base: "https://api.linkup.so/v1",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 7).unwrap();
        assert_eq!(r.url, "https://api.linkup.so/v1/search");
        assert_eq!(r.body.unwrap()["outputType"], "searchResults");
        let Read::Found(f) = found(&reply(
            json!({"results":[{"type":"text","name":"Rust","url":"https://u","content":"c"}]}),
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (f.hits[0].title.as_str(), f.hits[0].snippet.as_str()),
            ("Rust", "c")
        );
    }
}
