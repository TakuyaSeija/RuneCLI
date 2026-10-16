//! The services the web tools reach through: who answers a search, who reads
//! a page when the built-in reader is not the one chosen.
//!
//! # A connection is the account, a service is what it does
//!
//! One account can do several things — 9router routes models, searches and
//! fetches behind one key and one address — so a connection is stored once
//! and the services it serves are switched on beside it
//! (`[services]` in `rune.toml`, [`crate::settings::services`]). The key ring
//! is the connection's name in the vault, the same ring a model connection of
//! that name uses: a key added once serves every service.
//!
//! # Every vendor is data plus two functions
//!
//! An [`Adapter`] says where the vendor lives, how a key is presented, how
//! many results one call can return, and how to build a request and read an
//! answer. Everything a vendor sends comes back as one [`Found`] or [`Page`],
//! and the tools render those one way, so the model reads the same shape
//! whichever vendor answered. Response fields are read defensively: a vendor
//! that renames an optional field degrades a result, it does not fail a turn.
//!
//! # What stays with the engine
//!
//! Every request goes through [`crate::net`] (the one door to the network) and
//! [`crate::retry`] (the one retry loop); a key travels only to the address its
//! connection names, never over plain `http://` to another machine
//! ([`key_may_travel`]).

mod brave;
mod brightdata;
mod exa;
mod firecrawl;
mod jina;
mod linkup;
mod ninerouter;
mod ollama;
mod perplexity;
mod scrapingdog;
mod searxng;
mod serpapi;
mod serper;
mod serpex;
mod serplify;
mod serpstack;
mod tavily;
pub(crate) mod text;
mod tinyfish;
mod youcom;

use crate::settings::secrets::KeyState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::net::{Http, Response};
use crate::settings::services::{Connection, Service};
use crate::settings::Settings;

/// How a vendor wants its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Every request carries one.
    Required,
    /// A gateway that may or may not have authentication switched on.
    Optional,
    /// A server of the person's own that knows no keys (SearXNG).
    None,
}

/// Where a vendor's services live. Most answer both services on one address;
/// a few answer search and fetch on two hosts, and those two are configured
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosts {
    /// One address for every service. `None`: the person names it (their own
    /// server), there is no vendor default.
    Shared(Option<&'static str>),
    /// Search and fetch on their own hosts.
    Split {
        search: &'static str,
        fetch: &'static str,
    },
}

/// What one call needs to build its request.
pub struct Call<'a> {
    /// The address this service is reached at, without a trailing slash.
    pub base: &'a str,
    pub key: Option<&'a str>,
    /// The gateway's route (9router: `exa`, `ag`, a combo's name), for an
    /// adapter that routes.
    pub route: Option<&'a str>,
    /// Bright Data's SERP zone.
    pub zone: Option<&'a str>,
}

/// One HTTP request, ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub get: bool,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl Request {
    fn get(url: String, headers: Vec<(String, String)>) -> Request {
        Request {
            get: true,
            url,
            headers,
            body: None,
        }
    }

    fn post(url: String, headers: Vec<(String, String)>, body: Value) -> Request {
        Request {
            get: false,
            url,
            headers,
            body: Some(body),
        }
    }
}

/// A search result, whichever vendor found it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub date: Option<String>,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    /// A direct answer, when the vendor gave one: often the reason for the
    /// search, so it is shown first.
    pub answer: Option<String>,
    /// What the vendor knows about the subject itself (a knowledge panel).
    pub about: Option<String>,
    pub hits: Vec<Hit>,
}

/// How a search answer was read.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    Found(Found),
    /// The vendor finishes the search later; ask this address until it has.
    Later(String),
}

/// A page another service read. Its text is what the vendor extracted —
/// unless the vendor handed back the page itself, which the fetch tool tells
/// from the text (it opens as an HTML document: no vendor's own word about the
/// format is reliable — 9router 0.5.99 names the format it was asked for, not
/// the one that came back) and reads with the built-in reader's parser.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub title: Option<String>,
    pub text: String,
}

pub struct SearchApi {
    pub build: fn(&Call, &str, u64) -> Result<Request>,
    pub read: fn(&Response) -> Result<Read>,
}

pub struct FetchApi {
    pub build: fn(&Call, &str) -> Result<Request>,
    pub read: fn(&Response) -> Result<Page>,
}

/// The routes a gateway lists, per service, spelled as it takes them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Routes {
    pub search: Vec<String>,
    pub fetch: Vec<String>,
}

/// A gateway that routes each request to one of the vendors behind it.
pub struct RouteApi {
    /// The routes it lists. An offer, never proof: a gateway may list what no
    /// longer works and leave out what does.
    pub list: fn(&Http, &Call) -> Result<Routes>,
    /// `text` as the gateway takes it for `service`. A person copies a route
    /// from wherever the gateway shows one, and a gateway may show it in a
    /// spelling its own doors refuse.
    pub canonical: fn(&str, Service) -> String,
}

/// One vendor.
pub struct Adapter {
    /// The connection name and `api` it is configured under.
    pub id: &'static str,
    pub title: &'static str,
    /// Where the person gets a key.
    pub site: &'static str,
    pub hosts: Hosts,
    pub key: Key,
    pub key_hint: &'static str,
    pub search: Option<SearchApi>,
    pub fetch: Option<FetchApi>,
    /// The most results one search returns.
    pub max_results: u64,
    /// A route names which of the gateway's vendors serves (9router).
    pub routes: Option<RouteApi>,
    /// A SERP zone of the account names where searches run (Bright Data).
    pub zone: bool,
    /// Its address is the model connection's of the same name, when there is
    /// one (9router serves models and the web on one address).
    pub shares_model_base: bool,
    /// What a refusal (401/403) means for this vendor, when it is not a key.
    pub refusal: Option<&'static str>,
}

impl Adapter {
    pub fn serves(&self, service: Service) -> bool {
        match service {
            Service::Search => self.search.is_some(),
            Service::Fetch => self.fetch.is_some(),
        }
    }

    /// The vendor's own address for `service`, if it has one.
    pub fn default_base(&self, service: Service) -> Option<&'static str> {
        match (self.hosts, service) {
            (Hosts::Shared(base), _) => base,
            (Hosts::Split { search, .. }, Service::Search) => Some(search),
            (Hosts::Split { fetch, .. }, Service::Fetch) => Some(fetch),
        }
    }

    pub fn split(&self) -> bool {
        matches!(self.hosts, Hosts::Split { .. })
    }

    /// Whether a route names which of the vendors behind it serves.
    pub fn routed(&self) -> bool {
        self.routes.is_some()
    }

    /// `text` as a route this vendor takes for `service`: trimmed, in the
    /// gateway's own spelling; `None` when blank.
    pub fn route(&self, service: Service, text: &str) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(match &self.routes {
            Some(api) => (api.canonical)(text, service),
            None => text.to_string(),
        })
    }
}

/// Every vendor Rune can reach, in the order the person is offered them.
pub static ADAPTERS: &[Adapter] = &[
    ninerouter::ADAPTER,
    tavily::ADAPTER,
    exa::ADAPTER,
    brave::ADAPTER,
    linkup::ADAPTER,
    perplexity::ADAPTER,
    youcom::ADAPTER,
    tinyfish::ADAPTER,
    ollama::ADAPTER,
    jina::ADAPTER,
    firecrawl::ADAPTER,
    serper::ADAPTER,
    serpapi::ADAPTER,
    serpstack::ADAPTER,
    scrapingdog::ADAPTER,
    brightdata::ADAPTER,
    serpex::ADAPTER,
    serplify::ADAPTER,
    searxng::ADAPTER,
];

/// The vendor configured as `api`.
pub fn adapter(api: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.id == api)
}

/// A connection ready to serve one service.
#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub adapter: &'static Adapter,
    pub service: Service,
    pub base: String,
    /// The vault ring this connection's key is read from, when it sends one.
    pub secret: Option<String>,
    pub route: Option<String>,
    pub zone: Option<String>,
}

impl std::fmt::Debug for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id)
    }
}

impl Route {
    /// The host a request goes to — what a log line names. Never the whole
    /// URL: some vendors take the key in the query.
    pub fn host(&self) -> String {
        host_of(&self.base)
    }
}

/// Who serves `service` now.
#[derive(Debug, Clone)]
pub enum Choice {
    Ready(Route),
    /// Fetch with nothing chosen: the engine's own reader.
    Builtin,
    /// Nothing can serve; the sentence says why and what the person does.
    Unavailable(String),
}

/// The configuration key that pins `service` to one connection.
pub fn pin_key(service: Service) -> &'static str {
    match service {
        Service::Search => "web_search.provider",
        Service::Fetch => "web_fetch.provider",
    }
}

/// Who serves `service`: the connection the person pinned; unpinned, search
/// goes to the first connection whose search was switched on and fetch to the
/// built-in reader (owner, 2026-10-15: a fetch connection reads pages only
/// once Tools › Web fetch › Provider names it — a page handed to a third party
/// is a choice, never a side effect of switching a service on).
pub fn choose(settings: &Settings, service: Service) -> Choice {
    let pinned = settings.config.connection(pin_key(service));
    if let Some(target) = pinned.as_deref() {
        return target_route(settings, service, target);
    }
    if service == Service::Fetch {
        return Choice::Builtin;
    }
    let connection = match settings.services.first(service) {
        Some(c) => c,
        None => {
            return Choice::Unavailable(
                "no search provider is configured; the person adds one in Settings › Tools › \
                     Web search › Provider, using an account from Settings › Connections"
                    .into(),
            )
        }
    };
    route(settings, connection, service)
}

/// A saved Tools choice, held to its exact account and kept route even when
/// that account has no default service switched on.
pub fn target_route(settings: &Settings, service: Service, text: &str) -> Choice {
    let unavailable = || {
        Choice::Unavailable(format!(
            "{} names {text}, which is not a saved {} provider; the person adds or chooses it \
         in Settings › Tools › {} › Provider",
            pin_key(service),
            service.noun(),
            service.noun()
        ))
    };
    let Ok(target) = crate::settings::services::Target::parse(text) else {
        return unavailable();
    };
    let Some(connection) = settings.services.get(&target.name) else {
        return unavailable();
    };
    let Some(adapter) = adapter(&connection.api) else {
        return unavailable();
    };
    if !adapter.serves(service) {
        return unavailable();
    }
    match &target.route {
        Some(route) if adapter.routed() && connection.saved(service).contains(route) => {}
        None if !adapter.routed() && connection.uses(service).is_some() => {}
        _ => return unavailable(),
    }
    let mut chosen = connection.clone();
    *chosen.uses_mut(service) = Some(crate::settings::services::Use {
        added: 0,
        route: target.route,
    });
    route(settings, &chosen, service)
}

/// `connection` ready to serve `service`, or why it cannot.
pub fn route(settings: &Settings, connection: &Connection, service: Service) -> Choice {
    let Some(adapter) = adapter(&connection.api) else {
        return Choice::Unavailable(format!(
            "{} names an unknown service vendor {}",
            connection.name, connection.api
        ));
    };
    let Some(used) = connection.uses(service) else {
        return Choice::Unavailable(format!(
            "{} does not serve {}",
            connection.name,
            service.noun()
        ));
    };
    let Some(base) = base_of(settings, connection, adapter, service) else {
        return Choice::Unavailable(format!(
            "{} has no address; the person sets its base URL in Settings › Connections",
            connection.name
        ));
    };
    let state = settings.secrets.key_state(&connection.name);
    let has_key = matches!(state, KeyState::Present { .. });
    // A key that may be there but cannot be read is never taken for none:
    // an optional key would otherwise be left out without a word.
    if matches!(state, KeyState::Unavailable(_)) && !matches!(adapter.key, Key::None) {
        return Choice::Unavailable(state.note(&connection.name));
    }
    let secret = match adapter.key {
        Key::Required if !has_key => {
            return Choice::Unavailable(format!(
                "{}; the person adds one in Settings › Connections › {}",
                state.note(&connection.name),
                connection.name
            ))
        }
        Key::Required | Key::Optional if has_key => Some(connection.name.clone()),
        _ => None,
    };
    if secret.is_some() && !key_may_travel(&base) {
        return Choice::Unavailable(format!(
            "{} would send its key over plain http:// to another machine; the person sets an \
             https:// base URL in Settings › Connections",
            connection.name
        ));
    }
    Choice::Ready(Route {
        name: connection.name.clone(),
        adapter,
        service,
        base,
        secret,
        route: used.route.clone(),
        zone: connection.zone.clone(),
    })
}

/// The address `connection` reaches `service` at: what the person set, else
/// the model connection's of the same name for a vendor that shares it, else
/// the vendor's own.
pub fn base_of(
    settings: &Settings,
    connection: &Connection,
    adapter: &Adapter,
    service: Service,
) -> Option<String> {
    let chosen = match service {
        Service::Fetch if adapter.split() => connection.fetch_url.as_deref(),
        _ => connection.base_url.as_deref(),
    };
    let shared = adapter
        .shares_model_base
        .then(|| settings.providers.get(&connection.name))
        .flatten()
        .map(|p| p.base_url.as_str());
    chosen
        .or(shared)
        .or(adapter.default_base(service))
        .map(|b| b.trim_end_matches('/').to_string())
}

/// What the status tells the model about `service`.
pub fn status(settings: &Settings, service: Service) -> String {
    match choose(settings, service) {
        Choice::Ready(route) => match &route.route {
            Some(r) => format!("{} ({r})", route.name),
            None => route.name,
        },
        Choice::Builtin => "built-in reader".into(),
        Choice::Unavailable(why) => format!("unavailable — {why}"),
    }
}

/// Whether a key may be sent to `base`: always over https, over plain http
/// only to this machine (the rule model connections keep, `ProviderConfig`).
pub fn key_may_travel(base: &str) -> bool {
    match base.split_once("://") {
        Some(("https", _)) => true,
        Some(("http", _)) => crate::settings::providers::is_loopback(base),
        _ => false,
    }
}

fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_string()
}

/// One search request (and, for a vendor that finishes later, the asking
/// after it) with the key at the front of the ring — no retry here: the tool
/// runs this inside [`crate::retry`], a connection check runs it once.
pub fn search_once(
    http: &Http,
    route: &Route,
    key: Option<&str>,
    query: &str,
    count: u64,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Found> {
    let api = route
        .adapter
        .search
        .as_ref()
        .ok_or_else(|| Error::invalid(format!("{} does not search", route.name)))?;
    let call = Call {
        base: &route.base,
        key,
        route: route.route.as_deref(),
        zone: route.zone.as_deref(),
    };
    let request = (api.build)(&call, query, count)?;
    let mut answer = (api.read)(&send(http, route.adapter, &request)?)?;
    loop {
        match answer {
            Read::Found(mut found) => {
                found.hits.truncate(count as usize);
                return Ok(plain(found));
            }
            Read::Later(path) => {
                let url = if path.starts_with("http://") || path.starts_with("https://") {
                    path
                } else {
                    format!("{}/{}", route.base, path.trim_start_matches('/'))
                };
                // Asked at the vendor's own address only: a poll URL pointing
                // elsewhere would carry the key to a host nobody configured.
                if host_of(&url) != route.host() {
                    return Err(Error::Provider(format!(
                        "{} asked to be polled at another host",
                        route.name
                    )));
                }
                wait(POLL_EVERY, deadline, cancel)?;
                let poll = Request::get(url, request.headers.clone());
                answer = (api.read)(&send(http, route.adapter, &poll)?)?;
            }
        }
    }
}

/// What a search found, its page-born text made plain ([`text`]) whichever
/// vendor answered. A direct answer is left as written: a route that searches
/// through a chat model answers in Markdown, which is meant for the reader.
fn plain(mut found: Found) -> Found {
    found.about = found
        .about
        .map(|a| text::plain_line(&a))
        .filter(|a| !a.is_empty());
    for hit in &mut found.hits {
        hit.title = text::plain_line(&hit.title);
        if hit.title.is_empty() {
            hit.title = "(untitled)".into();
        }
        hit.snippet = text::plain_line(&hit.snippet);
        hit.date = hit
            .date
            .take()
            .map(|d| text::plain_line(&d))
            .filter(|d| !d.is_empty());
    }
    found
}

/// How often a search a vendor finishes later is asked after.
const POLL_EVERY: Duration = Duration::from_secs(2);

fn wait(step: Duration, deadline: Instant, cancel: &AtomicBool) -> Result<()> {
    let until = Instant::now() + step;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(Error::Unreachable(
                "the search did not finish within web_search.timeout".into(),
            ));
        }
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(Duration::from_millis(100)));
    }
}

/// One fetch request with the key at the front of the ring.
pub fn fetch_once(http: &Http, route: &Route, key: Option<&str>, url: &str) -> Result<Page> {
    let api = route
        .adapter
        .fetch
        .as_ref()
        .ok_or_else(|| Error::invalid(format!("{} does not fetch pages", route.name)))?;
    let call = Call {
        base: &route.base,
        key,
        route: route.route.as_deref(),
        zone: route.zone.as_deref(),
    };
    let page = (api.read)(&send(http, route.adapter, &(api.build)(&call, url)?)?)?;
    if page.text.trim().is_empty() {
        return Err(Error::Provider(format!(
            "{} returned no readable content for this page",
            route.name
        )));
    }
    Ok(page)
}

/// What a gateway lists, asked with the key the person is about to save.
pub fn routes(base: &str, key: Option<&str>, adapter: &Adapter, http: &Http) -> Result<Routes> {
    let Some(api) = &adapter.routes else {
        return Ok(Routes::default());
    };
    (api.list)(
        http,
        &Call {
            base,
            key,
            route: None,
            zone: None,
        },
    )
}

/// The SERP zones of a Bright Data account.
pub fn zones(base: &str, key: &str, http: &Http) -> Result<Vec<String>> {
    brightdata::list_zones(
        http,
        &Call {
            base,
            key: Some(key),
            route: None,
            zone: None,
        },
    )
}

/// Send `request`; a non-2xx answer is the vendor refusing, read for its reason.
fn send(http: &Http, adapter: &Adapter, request: &Request) -> Result<Response> {
    let response = if request.get {
        http.get(&request.url, &request.headers)?
    } else {
        http.post_json(
            &request.url,
            &request.headers,
            request.body.as_ref().unwrap_or(&Value::Null),
        )?
    };
    if !response.ok() {
        return Err(rejected(adapter, response.status, &response.text));
    }
    Ok(response)
}

/// A vendor's refusal, with what to do next. The status decides the retry
/// (a refused key rotates, `retry::fault`); the sentence is for the person.
pub fn rejected(adapter: &Adapter, status: u16, text: &str) -> Error {
    let message = crate::net::rejection_message(text);
    let refusal = adapter.refusal.map(|r| format!(" — {r}"));
    let hint = match status {
        401 | 403 if refusal.is_some() => refusal.as_deref().unwrap_or(""),
        401 | 403 => " — check this connection's API key",
        402 => " — the account is out of credit",
        404 => " — check this connection's base URL",
        429 => " — rate limited; wait and try again",
        500..=599 => " — the service is having trouble; this is not your request",
        _ => "",
    };
    Error::Rejected {
        status,
        message: format!("{message}{hint}"),
    }
}

/// The JSON an answer carries; text that is not JSON is the vendor saying
/// something else went wrong (an HTML error page from a proxy, most often).
fn json(response: &Response) -> Result<&Value> {
    if response.body.is_null() {
        return Err(Error::Provider(format!(
            "the service answered with something that is not JSON: {}",
            crate::net::rejection_message(&response.text)
        )));
    }
    Ok(&response.body)
}

/// Text at `pointer`, trimmed; empty is absent.
fn text(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// The first of `pointers` that holds text.
fn first_text(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|p| text(value, p))
}

/// The items at `pointer`, or none.
fn items<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Hits read from `items` with the field names one vendor uses.
fn hits(
    items: &[Value],
    title: &[&str],
    url: &[&str],
    snippet: &[&str],
    date: &[&str],
) -> Vec<Hit> {
    items
        .iter()
        .filter_map(|item| {
            let url = first_text(item, url)?;
            Some(Hit {
                title: first_text(item, title).unwrap_or_else(|| "(untitled)".into()),
                url,
                snippet: first_text(item, snippet).unwrap_or_default(),
                date: first_text(item, date),
            })
        })
        .collect()
}

fn bearer(key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
    if let Some(key) = key {
        headers.push(("Authorization".into(), format!("Bearer {key}")));
    }
    headers
}

fn header(name: &str, key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
    if let Some(key) = key {
        headers.push((name.into(), key.into()));
    }
    headers
}

fn need_key<'a>(call: &Call<'a>) -> Result<&'a str> {
    call.key
        .ok_or_else(|| Error::invalid("this connection needs an API key"))
}

/// A query component, encoded so separators keep their meaning.
fn enc(value: &str) -> String {
    crate::net::encode_component(value)
}

/// The model-visible text of a search: a direct answer first, then one block
/// per result. The same shape from every vendor.
pub fn render(found: &Found) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(answer) = &found.answer {
        out.push(format!("ANSWER: {answer}"));
    }
    if let Some(about) = &found.about {
        out.push(format!("ABOUT: {about}"));
    }
    for hit in &found.hits {
        let mut line = format!("- {}\n  {}", hit.title, hit.url);
        if let Some(date) = &hit.date {
            line.push_str(&format!("\n  ({date})"));
        }
        if !hit.snippet.is_empty() {
            line.push_str(&format!("\n  {}", hit.snippet));
        }
        out.push(line);
    }
    out
}

/// The query a connection check sends: harmless, cheap, and recognisable in a
/// vendor's usage log.
pub const CHECK_QUERY: &str = "rune connection check";

/// The page a connection check reads.
pub const CHECK_PAGE: &str = "https://example.com/";

/// A client for a connection the person configured: it reaches the address
/// they named, however local (their own SearXNG, 9router), which the model's
/// own fetches never may.
pub fn client(
    settings: &Arc<Mutex<Settings>>,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Http {
    let config = crate::panic::lock(settings).config.clone();
    Http::configured(
        &config,
        Some(timeout),
        crate::provider::RequestControl {
            cancel,
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests;
