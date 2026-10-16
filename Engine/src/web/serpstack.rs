//! serpstack (APILayer): Google results as JSON (`GET /search`, the key as
//! `access_key`). It refuses with HTTP 200 and `success: false` — read here
//! and given the status it means, so a refused key still rotates. Rune reaches
//! it over https only: a plan that allows only plain http (reportedly the free
//! one) is refused by serpstack with its own sentence, never by sending the
//! key in clear.

use super::{
    enc, first_text, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read,
    Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "serpstack",
    title: "serpstack",
    site: "serpstack.com/dashboard",
    hosts: Hosts::Shared(Some("https://api.serpstack.com")),
    key: Key::Required,
    key_hint: "serpstack access key",
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
        format!(
            "{}/search?access_key={}&query={}&num={count}",
            call.base,
            enc(need_key(call)?),
            enc(query)
        ),
        vec![("Accept".into(), "application/json".into())],
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    if data.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        let code = data
            .pointer("/error/code")
            .and_then(serde_json::Value::as_u64);
        let why = first_text(data, &["/error/info", "/error/type"])
            .unwrap_or_else(|| "the request was refused".into());
        // APILayer's codes: 101/102 a key that is wrong or inactive, 104 the
        // plan's monthly limit, 105 a feature (https) the plan lacks.
        let status = match code {
            Some(101 | 102) => 401,
            Some(104) => 429,
            Some(105) => 403,
            _ => 400,
        };
        return Err(Error::Rejected {
            status,
            message: format!("serpstack: {why}"),
        });
    }
    Ok(Read::Found(Found {
        answer: first_text(data, &["/answer_box/answer"]),
        about: first_text(data, &["/knowledge_graph/description"]),
        hits: hits(
            items(data, "/organic_results"),
            &["/title"],
            &["/url", "/link"],
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
    fn a_refusal_in_a_200_is_read_as_the_status_it_means() {
        let call = Call {
            base: "https://api.serpstack.com",
            key: Some("k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust", 10).unwrap();
        assert_eq!(
            r.url,
            "https://api.serpstack.com/search?access_key=k&query=rust&num=10"
        );
        let refused = |code| {
            found(&reply(json!({"success":false,
                "error":{"code":code,"type":"t","info":"why"}})))
            .unwrap_err()
        };
        assert!(matches!(refused(101), Error::Rejected { status: 401, .. }));
        assert!(matches!(refused(104), Error::Rejected { status: 429, .. }));
        assert!(matches!(refused(105), Error::Rejected { status: 403, .. }));
        let Read::Found(f) = found(&reply(json!({"organic_results":[
            {"position":1,"title":"T","url":"https://u","snippet":"s"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].url, "https://u");
    }
}
