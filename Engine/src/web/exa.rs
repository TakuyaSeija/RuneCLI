//! Exa: neural search for agents, and page contents. Shapes from Exa's API
//! reference (2026-10): `/search` takes its content options in a `contents`
//! object; `/contents` takes `urls` and a top-level `text`.

use serde_json::json;

use super::{
    first_text, header, items, json as body, need_key, Adapter, Call, FetchApi, Found, Hit, Hosts,
    Key, Page, Read, Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "exa",
    title: "Exa",
    site: "dashboard.exa.ai",
    hosts: Hosts::Shared(Some("https://api.exa.ai")),
    key: Key::Required,
    key_hint: "Exa API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: Some(FetchApi {
        build: fetch,
        read: page,
    }),
    max_results: 100,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/search", call.base),
        header("x-api-key", Some(need_key(call)?)),
        json!({
            "query": query,
            "numResults": count,
            "type": "auto",
            "contents": { "highlights": true }
        }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    let hits = items(data, "/results")
        .iter()
        .filter_map(|item| {
            Some(Hit {
                title: first_text(item, &["/title"]).unwrap_or_else(|| "(untitled)".into()),
                url: first_text(item, &["/url"])?,
                snippet: item
                    .get("highlights")
                    .and_then(serde_json::Value::as_array)
                    .map(|h| {
                        h.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" … ")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| first_text(item, &["/summary"]))
                    .unwrap_or_default(),
                date: first_text(item, &["/publishedDate"]),
            })
        })
        .collect();
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits,
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/contents", call.base),
        header("x-api-key", Some(need_key(call)?)),
        json!({ "urls": [url], "text": true }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    if data.pointer("/statuses/0/status").and_then(|s| s.as_str()) == Some("error") {
        let why = first_text(data, &["/statuses/0/error/tag"]).unwrap_or_else(|| "error".into());
        return Err(Error::Provider(format!(
            "Exa could not read the page: {why}"
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
    fn search_asks_for_highlights_and_contents_reads_the_text() {
        let call = Call {
            base: "https://api.exa.ai",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 4).unwrap();
        let b = r.body.unwrap();
        assert_eq!(b["numResults"], 4);
        assert_eq!(b["contents"]["highlights"], true);
        assert!(r.headers.contains(&("x-api-key".into(), "k".into())));
        let Read::Found(f) = found(&reply(json!({"results":[
            {"title":"T","url":"https://u","highlights":["one","two"],"publishedDate":"2026-01-01"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "one … two");
        let r = fetch(&call, "https://example.com/").unwrap();
        assert_eq!(
            r.body.unwrap(),
            json!({"urls":["https://example.com/"],"text":true})
        );
        let p = page(&reply(json!({"results":[{"title":"E","text":"body"}],
            "statuses":[{"id":"https://example.com/","status":"success"}]})))
        .unwrap();
        assert_eq!((p.title.as_deref(), p.text.as_str()), (Some("E"), "body"));
        assert!(page(&reply(json!({"results":[],
            "statuses":[{"status":"error","error":{"tag":"CRAWL_NOT_FOUND"}}]})))
        .is_err());
    }
}
