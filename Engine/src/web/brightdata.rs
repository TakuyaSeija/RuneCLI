//! Bright Data SERP API: Google through the account's SERP zone (`POST
//! /request` with the zone and the Google URL, `brd_json=1` for parsed JSON).
//! Shapes from Bright Data's docs (2026-10): parsed results under `organic`
//! with `link` and `description`; the account's zones from `GET
//! /zone/get_active_zones`, a SERP zone being `type: serp`. The zone is picked
//! from that list, never typed: its name is whatever the account calls it.

use serde_json::{json, Value};

use super::{
    bearer, enc, first_text, hits, items, json as body, need_key, Adapter, Call, Found, Hosts, Key,
    Read, Request, SearchApi,
};
use crate::error::{Error, Result};
use crate::net::{Http, Response};

pub(super) const ADAPTER: Adapter = Adapter {
    id: "brightdata",
    title: "Bright Data SERP",
    site: "brightdata.com/cp/setting/users (API key)",
    hosts: Hosts::Shared(Some("https://api.brightdata.com")),
    key: Key::Required,
    key_hint: "Bright Data API key",
    search: Some(SearchApi {
        build: search,
        read: found,
    }),
    fetch: None,
    max_results: 10,
    routes: None,
    zone: true,
    shares_model_base: false,
    refusal: None,
};

fn search(call: &Call, query: &str, _count: u64) -> Result<Request> {
    let zone = call
        .zone
        .filter(|z| !z.trim().is_empty())
        .ok_or_else(|| Error::invalid("Bright Data needs a SERP zone"))?;
    Ok(Request::post(
        format!("{}/request", call.base),
        bearer(Some(need_key(call)?)),
        json!({
            "zone": zone,
            "url": format!("https://www.google.com/search?q={}&brd_json=1", enc(query)),
            "format": "raw"
        }),
    ))
}

fn found(response: &Response) -> Result<Read> {
    let data = body(response)?;
    Ok(Read::Found(Found {
        answer: None,
        about: None,
        hits: hits(
            items(data, "/organic"),
            &["/title"],
            &["/link"],
            &["/description"],
            &["/last_modified_date"],
        ),
    }))
}

/// The account's SERP zones, by name.
pub(super) fn list_zones(http: &Http, call: &Call) -> Result<Vec<String>> {
    let response = super::send(
        http,
        &ADAPTER,
        &Request::get(
            format!("{}/zone/get_active_zones", call.base),
            bearer(Some(need_key(call)?)),
        ),
    )?;
    Ok(serp_zones(body(&response)?))
}

fn serp_zones(data: &Value) -> Vec<String> {
    data.as_array()
        .into_iter()
        .flatten()
        .filter(|z| z.get("type").and_then(Value::as_str) == Some("serp"))
        .filter_map(|z| first_text(z, &["/name"]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::tests::reply;

    #[test]
    fn a_search_goes_through_the_zone_and_only_serp_zones_are_offered() {
        let call = Call {
            base: "https://api.brightdata.com",
            key: Some("k"),
            route: None,
            zone: Some("serp_api1"),
        };
        let r = search(&call, "rust lang", 10).unwrap();
        assert_eq!(r.url, "https://api.brightdata.com/request");
        assert_eq!(
            r.body.unwrap(),
            json!({"zone":"serp_api1",
                "url":"https://www.google.com/search?q=rust%20lang&brd_json=1","format":"raw"})
        );
        assert!(search(&Call { zone: None, ..call }, "q", 1).is_err());
        let Read::Found(f) = found(&reply(json!({"organic":[
            {"link":"https://u","title":"T","description":"d","global_rank":1}
        ]})))
        .unwrap() else {
            panic!()
        };
        assert_eq!(f.hits[0].snippet, "d");
        assert_eq!(
            serp_zones(&json!([
                {"name":"serp_api1","type":"serp"},
                {"name":"residential","type":"res_rotating"}
            ])),
            ["serp_api1"]
        );
    }
}
