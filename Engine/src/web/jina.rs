//! Jina: Search (`s.jina.ai`) and Reader (`r.jina.ai`), on two hosts. Both
//! asked for JSON (`Accept: application/json`); search without page contents
//! (`X-Respond-With: no-content`) so a search stays a list of links. Reader is
//! asked the way 9router 0.5.99 asks it: `POST /` with the URL in the body.

use serde_json::{json, Value};

use super::{
    bearer, enc, first_text, hits, items, need_key, Adapter, Call, FetchApi, Found, Hosts, Key,
    Page, Read, Request, SearchApi,
};
use crate::error::Result;
use crate::net::Response;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "jina",
    title: "Jina AI",
    site: "jina.ai/api-dashboard",
    hosts: Hosts::Split {
        search: "https://s.jina.ai",
        fetch: "https://r.jina.ai",
    },
    key: Key::Required,
    key_hint: "Jina API key (jina_…)",
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
    let mut headers = bearer(Some(need_key(call)?));
    headers.push(("X-Respond-With".into(), "no-content".into()));
    Ok(Request::get(
        format!("{}/?q={}", call.base, enc(query)),
        headers,
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = super::json(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/data"),
            &["/title"],
            &["/url"],
            &["/description", "/content"],
            &["/date", "/publishedTime"],
        ),
    }))
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/", call.base),
        bearer(Some(need_key(call)?)),
        json!({ "url": url }),
    ))
}

/// JSON when Reader honoured `Accept`, its Markdown text when it did not.
fn page(response: &Response) -> Result<Page> {
    if response.body.is_object() {
        let data = response.body.get("data").unwrap_or(&Value::Null);
        return Ok(Page {
            title: first_text(data, &["/title"]),
            text: first_text(data, &["/content"]).unwrap_or_default(),
        });
    }
    let title = response
        .text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Title:"))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Ok(Page {
        title,
        text: response.text.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn search_lists_links_and_reader_takes_json_or_text() {
        let call = Call {
            base: "https://s.jina.ai",
            key: Some("jina_k"),
            route: None,
            zone: None,
        };
        let r = search(&call, "rust lang", 5).unwrap();
        assert_eq!(r.url, "https://s.jina.ai/?q=rust%20lang");
        assert!(r
            .headers
            .contains(&("X-Respond-With".into(), "no-content".into())));
        let Read::Found(f) = found(&reply(json!({"code":200,"data":[
            {"title":"T","url":"https://u","description":"d"}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "d");
        let p = page(&reply(
            json!({"code":200,"data":{"title":"E","content":"# E"}}),
        ))
        .unwrap();
        assert_eq!((p.title.as_deref(), p.text.as_str()), (Some("E"), "# E"));
        let text = Response {
            status: 200,
            body: Value::Null,
            text: "Title: Example\nURL Source: https://example.com/\n\nMarkdown Content:\nHi"
                .into(),
            content_type: None,
            location: None,
            set_cookies: Vec::new(),
        };
        let p = page(&text).unwrap();
        assert_eq!(p.title.as_deref(), Some("Example"));
        assert!(p.text.ends_with("Hi"));
    }
}
