//! Firecrawl: page scraping to Markdown, and search. Shapes from Firecrawl's
//! v2 API reference (2026-10): `POST /v2/scrape`, `POST /v2/search` (results
//! under `data.web`; an older account answering `data` as a list is read too).

use serde_json::json;

use super::{
    bearer, first_text, hits, items, json as body, need_key, Adapter, Call, FetchApi, Found, Hosts,
    Key, Page, Read, Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "firecrawl",
    title: "Firecrawl",
    site: "firecrawl.dev/app/api-keys",
    hosts: Hosts::Shared(Some("https://api.firecrawl.dev/v2")),
    key: Key::Required,
    key_hint: "Firecrawl API key (fc-…)",
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
        bearer(Some(need_key(call)?)),
        json!({ "query": query, "limit": count }),
    ))
}

fn refused(data: &serde_json::Value) -> Option<Error> {
    (data.get("success").and_then(serde_json::Value::as_bool) == Some(false)).then(|| {
        Error::Provider(format!(
            "Firecrawl: {}",
            first_text(data, &["/error"]).unwrap_or_else(|| "the request failed".into())
        ))
    })
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    if let Some(e) = refused(data) {
        return Err(e);
    }
    let list = match data.pointer("/data") {
        Some(serde_json::Value::Array(_)) => items(data, "/data"),
        _ => items(data, "/data/web"),
    };
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(list, &["/title"], &["/url"], &["/description"], &[]),
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/scrape", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "url": url, "formats": ["markdown"], "onlyMainContent": true }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    if let Some(e) = refused(data) {
        return Err(e);
    }
    if let Some(why) = first_text(data, &["/data/metadata/error"]) {
        return Err(Error::Provider(format!(
            "Firecrawl could not read the page: {why}"
        )));
    }
    Ok(Page {
        title: first_text(data, &["/data/metadata/title"]),
        text: first_text(data, &["/data/markdown"]).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn scrape_and_search_read_v2_and_a_refusal_is_an_error() {
        let call = Call {
            base: "https://api.firecrawl.dev/v2",
            key: Some("fc-k"),
            route: None,
            zone: None,
        };
        assert_eq!(search(&call, "q", 3).unwrap().body.unwrap()["limit"], 3);
        let Read::Found(f) = found(&reply(json!({"success":true,"data":{"web":[
            {"url":"https://u","title":"T","description":"d"}
        ]}})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "d");
        let Read::Found(f) = found(&reply(json!({"success":true,"data":[
            {"url":"https://v","title":"V"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].url, "https://v");
        let p = page(&reply(json!({"success":true,"data":{"markdown":"# E",
            "metadata":{"title":"E","statusCode":200}}})))
        .unwrap();
        assert_eq!(p.text, "# E");
        assert!(page(&reply(json!({"success":false,"error":"Payment required"}))).is_err());
    }
}
