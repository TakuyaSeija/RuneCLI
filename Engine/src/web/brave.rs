//! Brave Search API: an independent web index. Shape from Brave's API
//! reference (`/res/v1/web/search`, `X-Subscription-Token`), as 9router
//! 0.5.99 sends it.

use super::{
    enc, header, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key, Read,
    Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "brave",
    title: "Brave Search",
    site: "api-dashboard.search.brave.com",
    hosts: Hosts::Shared(Some("https://api.search.brave.com/res/v1")),
    key: Key::Required,
    key_hint: "Brave Search API subscription token",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 20,
    routes: None,
    zone: false,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::get(
        format!("{}/web/search?q={}&count={count}", call.base, enc(query)),
        header("X-Subscription-Token", Some(need_key(call)?)),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/web/results"),
            &["/title"],
            &["/url"],
            &["/description"],
            &["/page_age", "/age"],
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;
    use serde_json::json;

    #[test]
    fn a_search_is_a_get_with_the_subscription_token() {
        let call = Call {
            base: "https://api.search.brave.com/res/v1",
            key: Some("tok"),
            route: None,
            zone: None,
        };
        let r = search(&call, "a b&c", 5).unwrap();
        assert!(r.get);
        assert_eq!(
            r.url,
            "https://api.search.brave.com/res/v1/web/search?q=a%20b%26c&count=5"
        );
        assert!(r
            .headers
            .contains(&("X-Subscription-Token".into(), "tok".into())));
        let Read::Found(f) = found(&reply(json!({"web":{"results":[
            {"title":"T","url":"https://u","description":"d","age":"2 days ago"}
        ]}})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].date.as_deref(), Some("2 days ago"));
    }
}
