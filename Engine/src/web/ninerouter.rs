//! 9router: a gateway on this machine that routes searches and fetches to the
//! vendors connected in its dashboard. A route says which one serves: a
//! vendor's id or alias (`exa`, `ag`) or a combo's name.
//!
//! Its list of routes (`/v1/models/web`) names what has an active connection
//! there, not what works: a key that ran out is still listed, and a vendor that
//! searches through a chat model (`ag`, `gemini`) is not listed at all (both
//! seen against 9router 0.5.99, 2026-10-14). So the list is offered, a route
//! can be typed, and a connection check — one real search — is what tells.
//!
//! The list spells a vendor's services `exa/search` and `exa/fetch`, but the
//! doors that search and fetch resolve the vendor alone — `exa/search` is
//! refused as `Unknown provider` (0.5.99: `/api/v1/search` and
//! `/api/v1/web/fetch` look the name up by id or alias, combos by name; seen by
//! the owner, 2026-10-15). So a route is kept and sent as the vendor
//! ([`canonical`]), whichever spelling it came in.
//!
//! Request and answer shapes read from 9router 0.5.99's own routes
//! (`/api/v1/search`, `/api/v1/web/fetch`, `/api/v1/models/[...model]`).

use serde_json::{json, Value};

use super::{
    bearer, first_text, items, json as body, Adapter, Call, FetchApi, Found, Hit, Hosts, Key, Page,
    Read, Request, RouteApi, Routes, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::{Http, Response};
use crate::settings::services::Service;

pub(super) const ADAPTER: Adapter = Adapter {
    id: "9router",
    title: "9router",
    site: "the 9router dashboard (Endpoint & Key)",
    hosts: Hosts::Shared(Some("http://localhost:20128/v1")),
    key: Key::Optional,
    key_hint: "9router API key — leave empty if 9router does not require one",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: Some(FetchApi {
        build: fetch,
        read: page,
    }),
    max_results: 100,
    routes: Some(RouteApi {
        list: routes,
        canonical,
    }),
    zone: false,
    shares_model_base: true,
    refusal: None,
};

fn route<'a>(call: &Call<'a>) -> Result<&'a str> {
    call.route.filter(|r| !r.trim().is_empty()).ok_or_else(|| {
        Error::invalid("9router needs a route: a vendor's id such as exa, or a combo's name")
    })
}

/// A route as 9router's search and fetch doors take it: the vendor alone,
/// never the `<vendor>/search` or `<vendor>/fetch` its list shows. Only the
/// suffix of the service asked for goes — `exa/fetch` given for search stays
/// as it is and is refused by the check, rather than silently becoming
/// another service of the same vendor. A combo's name has no `/` (9router
/// tells combos from vendor models by it), so a combo passes unchanged.
fn canonical(route: &str, service: Service) -> String {
    let route = route.trim();
    let suffix = match service {
        Service::Search => "/search",
        Service::Fetch => "/fetch",
    };
    route
        .strip_suffix(suffix)
        .filter(|vendor| !vendor.is_empty())
        .unwrap_or(route)
        .to_string()
}

fn search(call: &Call, query: &str, count: u64) -> Result<Request> {
    Ok(Request::post(
        format!("{}/search", call.base),
        bearer(call.key),
        json!({ "model": route(call)?, "query": query, "max_results": count }),
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
                snippet: first_text(item, &["/snippet", "/content/text"])
                    .map(|s| clip(&s))
                    .unwrap_or_default(),
                date: first_text(item, &["/published_at"]),
            })
        })
        .collect();
    Ok(Read::Found(Found {
        // A route that searches through a chat model answers in words.
        answer: first_text(data, &["/answer/text"]),
        about: None,
        hits,
    }))
}

/// A snippet stays a snippet: some routes put a whole page where it goes.
fn clip(text: &str) -> String {
    const MOST: usize = 600;
    if text.chars().count() <= MOST {
        return text.to_string();
    }
    let cut: String = text.chars().take(MOST).collect();
    format!("{}…", cut.trim_end())
}

fn fetch(call: &Call, url: &str) -> Result<Request> {
    Ok(Request::post(
        format!("{}/web/fetch", call.base),
        bearer(call.key),
        json!({ "model": route(call)?, "url": url, "format": "markdown" }),
    ))
}

fn page(response: &Response) -> Result<Page> {
    let data = body(response)?;
    Ok(Page {
        title: first_text(data, &["/title"]),
        text: first_text(data, &["/content/text"]).unwrap_or_default(),
    })
}

/// The search and fetch routes 9router lists — its connected vendors and
/// combos of either kind — spelled as its doors take them.
fn routes(http: &Http, call: &Call) -> Result<Routes> {
    let response = super::send(
        http,
        &ADAPTER,
        &Request::get(format!("{}/models/web", call.base), bearer(call.key)),
    )?;
    Ok(listed(body(&response)?))
}

fn listed(data: &Value) -> Routes {
    let mut routes = Routes::default();
    for item in items(data, "/data") {
        let Some(id) = first_text(item, &["/id"]) else {
            continue;
        };
        let (list, service) = match item.get("kind").and_then(Value::as_str) {
            Some("webSearch") => (&mut routes.search, Service::Search),
            Some("webFetch") => (&mut routes.fetch, Service::Fetch),
            _ => continue,
        };
        let route = canonical(&id, service);
        if !list.contains(&route) {
            list.push(route);
        }
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: Value) -> Response {
        Response {
            status: 200,
            text: body.to_string(),
            body,
            content_type: None,
            location: None,
            set_cookies: Vec::new(),
        }
    }

    #[test]
    fn a_search_names_its_route_and_reads_the_normalised_answer() {
        let call = Call {
            base: "http://localhost:20128/v1",
            key: Some("k"),
            route: Some("exa"),
            zone: None,
        };
        let request = search(&call, "rust", 3).unwrap();
        assert_eq!(request.url, "http://localhost:20128/v1/search");
        assert_eq!(
            request.body.unwrap(),
            json!({"model":"exa","query":"rust","max_results":3})
        );
        assert!(request
            .headers
            .contains(&("Authorization".into(), "Bearer k".into())));
        let read = found(&response(json!({
            "provider": "exa",
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "snippet": "A language", "published_at": "2026-01-01"},
                {"title": "No URL"},
                {"url": "https://x.dev", "content": {"text": "page text"}}
            ],
            "answer": {"text": "  "}
        })))
        .unwrap();
        let Read::Found(found) = read else { panic!() };
        assert_eq!(found.answer, None);
        assert_eq!(found.hits.len(), 2);
        assert_eq!(found.hits[0].date.as_deref(), Some("2026-01-01"));
        assert_eq!(found.hits[1].title, "(untitled)");
        assert_eq!(found.hits[1].snippet, "page text");
    }

    #[test]
    fn a_route_is_required_and_no_key_sends_no_authorization() {
        let call = Call {
            base: "http://localhost:20128/v1",
            key: None,
            route: Some("  "),
            zone: None,
        };
        assert!(search(&call, "q", 1).is_err());
        let call = Call {
            route: Some("tavily"),
            ..call
        };
        let request = fetch(&call, "https://example.com/").unwrap();
        assert_eq!(request.url, "http://localhost:20128/v1/web/fetch");
        assert!(request.headers.iter().all(|(h, _)| h != "Authorization"));
    }

    #[test]
    fn a_chat_route_answers_in_words_and_a_page_reads_its_content() {
        let Read::Found(found) = found(&response(json!({
            "results": [], "answer": {"text": "It is 42."}
        })))
        .unwrap() else {
            panic!()
        };
        assert_eq!(found.answer.as_deref(), Some("It is 42."));
        let page = page(&response(json!({
            "title": "Example", "content": {"format": "markdown", "text": "# Example"}
        })))
        .unwrap();
        assert_eq!(page.title.as_deref(), Some("Example"));
        assert_eq!(page.text, "# Example");
    }

    #[test]
    fn the_listing_is_offered_as_the_routes_its_doors_take() {
        // As 9router 0.5.99 lists them on the owner's machine (2026-10-15):
        // `exa/search` there, while `/v1/search` answers `Unknown provider:
        // exa/search` and serves `exa`.
        let routes = listed(&json!({"object":"list","data":[
            {"id":"ollama/fetch","kind":"webFetch","owned_by":"ollama"},
            {"id":"glm/search","kind":"webSearch","owned_by":"glm"},
            {"id":"exa/search","kind":"webSearch","owned_by":"exa"},
            {"id":"exa/fetch","kind":"webFetch","owned_by":"exa"},
            {"id":"my-combo","kind":"webSearch","owned_by":"combo"},
            {"id":"gpt-5","owned_by":"openai"}
        ]}));
        assert_eq!(routes.search, ["glm", "exa", "my-combo"]);
        assert_eq!(routes.fetch, ["ollama", "exa"]);
    }

    #[test]
    fn a_route_copied_as_the_list_spells_it_becomes_the_vendor() {
        assert_eq!(canonical("exa/search", Service::Search), "exa");
        assert_eq!(canonical(" ag/search ", Service::Search), "ag");
        assert_eq!(canonical("exa/fetch", Service::Fetch), "exa");
        assert_eq!(canonical("ag", Service::Search), "ag");
        assert_eq!(canonical("my-combo", Service::Fetch), "my-combo");
        assert_eq!(
            canonical("exa/fetch", Service::Search),
            "exa/fetch",
            "another service's spelling is left for the check to refuse"
        );
        assert_eq!(canonical("/search", Service::Search), "/search");
        assert_eq!(
            ADAPTER.route(Service::Search, "  exa/search "),
            Some("exa".into())
        );
        assert_eq!(ADAPTER.route(Service::Search, "  "), None);
    }

    #[test]
    fn a_long_snippet_is_clipped() {
        let long = "word ".repeat(400);
        let clipped = clip(&long);
        assert!(clipped.chars().count() <= 601);
        assert!(clipped.ends_with('…'));
    }
}
