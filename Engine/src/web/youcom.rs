//! You.com Search API. Shape from You.com's API reference
//! (`GET ydc-index.io/v1/search`, `X-API-Key`), as 9router 0.5.99 sends it.

use serde_json::Value;

use super::{
    enc, first_text, header, items, json as body, need_key, Adapter, Call, Found, Hit, Hosts, Key,
    Read, Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "youcom",
    title: "You.com",
    site: "you.com/platform/api-keys",
    hosts: Hosts::Shared(Some("https://ydc-index.io/v1")),
    key: Key::Required,
    key_hint: "You.com API key",
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
    Ok(Request::get(
        format!("{}/search?query={}&count={count}", call.base, enc(query)),
        header("X-API-Key", Some(need_key(call)?)),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    let hits = items(data, "/results/web")
        .iter()
        .filter_map(|item| {
            let snippet = item
                .get("snippets")
                .and_then(Value::as_array)
                .and_then(|s| s.iter().find_map(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| first_text(item, &["/description"]));
            Some(Hit {
                title: first_text(item, &["/title"]).unwrap_or_else(|| "(untitled)".into()),
                url: first_text(item, &["/url"])?,
                snippet: snippet.unwrap_or_default(),
                date: first_text(item, &["/page_age"]),
            })
        })
        .collect();
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;
    use serde_json::json;

    #[test]
    fn web_results_prefer_a_snippet_over_the_description() {
        let call = Call {
            base: "https://ydc-index.io/v1",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust lang", 3).unwrap();
        assert_eq!(
            r.url,
            "https://ydc-index.io/v1/search?query=rust%20lang&count=3"
        );
        assert!(r.headers.contains(&("X-API-Key".into(), "k".into())));
        let Read::Found(f) = found(&reply(json!({"results":{"web":[
            {"title":"A","url":"https://a","description":"desc","snippets":["snip"]},
            {"title":"B","url":"https://b","description":"only desc"}
        ],"news":[]}})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "snip");
        assert_eq!(f.hits[1].snippet, "only desc");
    }
}
