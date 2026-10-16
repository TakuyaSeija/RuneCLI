//! Serplify: Google SERPs (`POST /v1/serp/search`, Bearer). Shapes from
//! Serplify's docs (2026-10): results under `data.items`, organic ones typed
//! `organic`; a live search that cannot finish in time answers 202 with a
//! `status_url` that is asked until it answers 200 — never sent again, which
//! would pay for a second search.
//!
//! Serplify requires a location and a language. Rune sends the same ones
//! every time, compiled in (owner, 2026-10-14: no locale in the tool's
//! schema, no setting): the United States in English on a desktop — Google's
//! largest index, where the documentation a coding agent looks for mostly is,
//! and what the vendors that need no location do when none is sent. A query
//! in another language still finds pages in that language.

use serde_json::json;

use super::{
    bearer, first_text, json as body, need_key, Adapter, Call, Found, Hit, Hosts, Key, Read,
    Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

/// Google's location code for the United States.
const LOCATION: u64 = 2840;
const LANGUAGE: &str = "en";
const DEVICE: &str = "desktop";

pub(super) const ADAPTER: Adapter = Adapter {
    id: "serplify",
    title: "Serplify",
    site: "serplify.io (API keys)",
    hosts: Hosts::Shared(Some("https://api.serplify.io")),
    key: Key::Required,
    key_hint: "Serplify API key (live_…)",
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
        format!("{}/v1/serp/search", call.base),
        bearer(Some(need_key(call)?)),
        json!({
            "keyword": query,
            "location": { "code": LOCATION },
            "language": { "code": LANGUAGE },
            "device": DEVICE,
            // Serplify crawls at least ten and at most a hundred.
            "depth": count.clamp(10, 100),
            "format": "standard"
        }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    if response.status == 202 {
        let later = first_text(data, &["/data/status_url"]).ok_or_else(|| {
            crate::error::Error::Provider(
                "Serplify moved the search to a task without saying where to ask for it".into(),
            )
        })?;
        return Ok(Read::Later(later));
    }
    let hits = data
        .pointer("/data/items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("organic"))
        .filter_map(|item| {
            Some(Hit {
                title: first_text(item, &["/title"]).unwrap_or_else(|| "(untitled)".into()),
                url: first_text(item, &["/url"])?,
                snippet: first_text(item, &["/description"]).unwrap_or_default(),
                date: first_text(item, &["/timestamp"]),
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

    #[test]
    fn the_compiled_locale_is_sent_and_a_task_is_asked_after() {
        let call = Call {
            base: "https://api.serplify.io",
            key: Some("live_k"),
            route: None,
            zone: None,
        };
        let b = search(&call, "rust", 3).unwrap().body.unwrap();
        assert_eq!(b["location"]["code"], 2840);
        assert_eq!(b["language"]["code"], "en");
        assert_eq!(b["device"], "desktop");
        assert_eq!(b["depth"], 10);
        let Read::Found(f) = found(&reply(json!({"status":"ok","data":{"items":[
            {"type":"paid","title":"Ad","url":"https://ad"},
            {"type":"organic","title":"T","url":"https://u","description":"d"}
        ]}})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits.len(), 1);
        assert_eq!(f.hits[0].url, "https://u");
        let mut later = reply(json!({"status":"accepted",
            "data":{"task_id":"t","status_url":"/v1/serp/tasks/t"}}));
        later.status = 202;
        assert_eq!(
            found(&later).unwrap(),
            Read::Later("/v1/serp/tasks/t".into())
        );
    }
}
