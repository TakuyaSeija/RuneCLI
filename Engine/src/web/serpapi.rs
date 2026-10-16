//! SerpApi: Google results as JSON (`GET /search.json?engine=google`, the key
//! as `api_key` in the query — so a log line names the host, never the URL).
//! Google pages hold ten results; more are not asked for.

use super::{
    enc, first_text, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read,
    Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "serpapi",
    title: "SerpApi",
    site: "serpapi.com/manage-api-key",
    hosts: Hosts::Shared(Some("https://serpapi.com")),
    key: Key::Required,
    key_hint: "SerpApi private API key",
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
            "{}/search.json?engine=google&q={}&api_key={}",
            call.base,
            enc(query),
            enc(need_key(call)?)
        ),
        vec![("Accept".into(), "application/json".into())],
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    if let Some(why) = first_text(data, &["/error"]) {
        // "Google hasn't returned any results for this query." is an answer.
        if why
            .to_ascii_lowercase()
            .contains("hasn't returned any results")
        {
            return Ok(Read::Found(Found::default()));
        }
        return Err(Error::Provider(format!("SerpApi: {why}")));
    }
    Ok(Read::Found(Found {
        answer: first_text(data, &["/answer_box/answer", "/answer_box/snippet"]),
        about: first_text(data, &["/knowledge_graph/description"]),
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
    fn the_key_rides_in_the_query_encoded_and_an_empty_search_is_no_error() {
        let call = Call {
            base: "https://serpapi.com",
            key: Some("a+b"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust lang", 10).unwrap();
        assert_eq!(
            r.url,
            "https://serpapi.com/search.json?engine=google&q=rust%20lang&api_key=a%2Bb"
        );
        let Read::Found(f) = found(&reply(json!({"organic_results":[
            {"position":1,"title":"T","link":"https://u","snippet":"s"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].url, "https://u");
        assert!(matches!(
            found(&reply(json!({"error":"Google hasn't returned any results for this query."}))),
            Ok(Read::Found(Found { hits, .. })) if hits.is_empty()
        ));
        assert!(found(&reply(json!({"error":"Invalid API key."}))).is_err());
    }
}
