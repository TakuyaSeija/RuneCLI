//! TinyFish: search (free, keyed) and fetch, each on its own host. Shapes from
//! TinyFish's docs (`GET api.search.tinyfish.ai?query=`, `POST
//! api.fetch.tinyfish.ai`), as 9router 0.5.99 sends them.

use serde_json::json;

use super::{
    enc, first_text, header, hits, items, json as body, need_key, Adapter, Call, FetchApi, Found,
    Hosts, Key, Page, Read, Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "tinyfish",
    title: "TinyFish",
    site: "agent.tinyfish.ai/api-keys",
    hosts: Hosts::Split {
        search: "https://api.search.tinyfish.ai",
        fetch: "https://api.fetch.tinyfish.ai",
    },
    key: Key::Required,
    key_hint: "TinyFish API key",
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

fn search(call: &Call, query: &str, _count: u64) -> Result<Request> {
    Ok(Request::get(
        format!("{}?query={}", call.base, enc(query)),
        header("X-API-Key", Some(need_key(call)?)),
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
            &["/date"],
        ),
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        call.base.to_string(),
        header("X-API-Key", Some(need_key(call)?)),
        json!({ "urls": [url], "format": "markdown" }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    if let Some(why) = first_text(data, &["/errors/0/error"]) {
        return Err(Error::Provider(format!(
            "TinyFish could not read the page: {why}"
        )));
    }
    Ok(Page {
        title: first_text(data, &["/results/0/title"]),
        text: first_text(data, &["/results/0/text"]).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_and_fetch_go_to_their_own_hosts() {
        let call = Call {
            base: "https://api.search.tinyfish.ai",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 3).unwrap();
        assert_eq!(r.url, "https://api.search.tinyfish.ai?query=rust");
        let Read::Found(f) = found(&reply(
            json!({"results":[{"position":1,"title":"T","url":"https://u","snippet":"s"}]}),
        ))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits.len(), 1);
        let call = Call {
            base: "https://api.fetch.tinyfish.ai",
            ..call
        };
        let r = fetch(&call, "https://example.com/").unwrap();
        assert_eq!(r.url, "https://api.fetch.tinyfish.ai");
        assert_eq!(
            page(&reply(json!({"results":[{"title":"E","text":"body"}]})))
                .unwrap()
                .text,
            "body"
        );
        assert!(page(&reply(
            json!({"errors":[{"status":404,"error":"not found"}]})
        ))
        .is_err());
    }
}
