//! Account access and tool request choices have separate command contracts.
//! Connections can keep endpoint/key without sending a request; that says
//! nothing about a provider working. Tools proves each provider added and each
//! exact selection with one real request before saving it, through the same net
//! path and vault ring a real call uses. Failure keeps the previous choice.
//!
//! A gateway's catalog is never proof or an automatic Add candidate. Typed
//! routes are kept in the spelling its doors take ([`web::Adapter::route`]).
//! Pin and service route move in one configuration transaction, held to the
//! revisions seen before the check so a second window cannot be overwritten.

use std::time::Instant;

use serde_json::{json, Value};

use super::manage::Shared;
use super::secrets::SecretItem;
use super::services::{self, Connection, SavedRoutes, Service, Use};
use super::Settings;
use crate::error::{Error, Result};
use crate::panic;
use crate::proto::{ServiceRoutes, ServiceUse};
use crate::web::{self, Choice, Key};

/// What `service.save` asks for.
pub struct Save {
    pub name: String,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub fetch_url: Option<String>,
    pub search: Option<ServiceUse>,
    pub fetch: Option<ServiceUse>,
    /// The routes to keep; `None` keeps the ones kept now.
    pub routes: Option<ServiceRoutes>,
    pub zone: Option<String>,
    pub key_name: Option<String>,
    pub key: Option<String>,
}

fn given(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// `service.account` has no request payload, service switch or route field.
pub struct Account {
    pub name: String,
    pub api: Option<String>,
    pub base_url: Option<String>,
    pub fetch_url: Option<String>,
    pub key_name: Option<String>,
    pub key: Option<String>,
}

/// Account access is independent of the routes proved in Tools. Existing
/// tool choices survive an address/key edit; an account saved is not a proof.
pub fn account(settings: &Shared, request: Account) -> Result<Value> {
    let name = request.name.trim().to_string();
    let existing = panic::lock(settings).services.get(&name).cloned();
    let api = given(request.api)
        .or_else(|| existing.as_ref().map(|c| c.api.clone()))
        .unwrap_or_else(|| name.clone());
    if existing.as_ref().is_some_and(|c| c.api != api) {
        return Err(Error::invalid("an account's adapter cannot be replaced"));
    }
    let mut candidate = existing.unwrap_or(Connection {
        name: name.clone(),
        api: api.clone(),
        base_url: None,
        fetch_url: None,
        search: None,
        fetch: None,
        routes: SavedRoutes::default(),
        zone: None,
    });
    candidate.base_url = given(request.base_url);
    candidate.fetch_url = given(request.fetch_url);
    candidate.check()?;
    let adapter = web::adapter(&api).ok_or_else(|| Error::invalid("unknown web adapter"))?;
    let key = given(request.key);
    if key.is_some() && adapter.key == Key::None {
        return Err(Error::invalid("this account takes no API key"));
    }
    let previous = panic::lock(settings).secrets.raw_items(&name);
    if let Some(key) = &key {
        let item = match given(request.key_name) {
            Some(label) => SecretItem::Named {
                name: label,
                key: key.clone(),
            },
            None => SecretItem::Plain(key.clone()),
        };
        panic::lock(settings).secrets.restore(&name, vec![item])?;
    }
    let stored = panic::lock(settings).services.put(candidate);
    if let Err(e) = stored {
        if key.is_some() {
            panic::lock(settings).secrets.restore(&name, previous)?;
        }
        return Err(e);
    }
    Ok(json!({"saved":name,"checked":[],"account":true}))
}

/// A pin is written only after its exact saved target works; failure retains
/// the previous choice. Choosing the default never sends a third-party fetch.
pub fn select(settings: &Shared, service: &str, target: Option<String>) -> Result<Value> {
    let service = Service::parse(service)?;
    let target = given(target);
    let (previous_section, previous_pin, mut section) = {
        let s = panic::lock(settings);
        let section = s.config.get(services::SECTION);
        (
            section.clone(),
            s.config.get(web::pin_key(service)),
            section,
        )
    };
    let mut checked = Value::Null;
    if let Some(text) = &target {
        let target = services::Target::parse(text)?;
        let (mut connection, selected) = {
            let s = panic::lock(settings);
            let connection = s
                .services
                .get(&target.name)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("account {}", target.name)))?;
            (connection, web::target_route(&s, service, text))
        };
        if let Choice::Unavailable(why) = selected {
            return Err(Error::invalid(why));
        }
        let added = connection
            .uses(service)
            .map_or_else(services::now, |u| u.added);
        *connection.uses_mut(service) = Some(Use {
            added,
            route: target.route,
        });
        checked = check(settings, &connection, service)?;
        let entry = section["list"]
            .as_array_mut()
            .and_then(|list| {
                list.iter_mut()
                    .find(|c| c["name"].as_str() == Some(&connection.name))
            })
            .ok_or_else(|| Error::invalid("the chosen account changed; reload and choose again"))?;
        *entry = serde_json::to_value(connection)?;
    }
    let home = panic::lock(settings).home.clone();
    let mut changes = vec![(web::pin_key(service), target.as_ref().map(|s| json!(s)))];
    if target.is_some() {
        changes.insert(0, (services::SECTION, Some(section)));
    }
    super::configuration::Configuration::edit_many(
        &home,
        &changes,
        false,
        &[
            (services::SECTION, &previous_section),
            (web::pin_key(service), &previous_pin),
        ],
    )?;
    {
        let mut s = panic::lock(settings);
        s.config.reload()?;
        s.follow_services();
    }
    Ok(json!({"selected":target,"checked":checked}))
}

/// Add or change a service connection, after proving what changed.
pub fn save(settings: &Shared, request: Save) -> Result<Value> {
    let name = request.name.trim().to_string();
    let api = given(request.api).unwrap_or_else(|| name.clone());
    let adapter = web::adapter(&api).ok_or_else(|| {
        Error::invalid(format!("{api} is not a search or fetch service Rune knows"))
    })?;
    let existing = panic::lock(settings).services.get(&name).cloned();
    if existing.as_ref().is_some_and(|e| e.api != api) {
        return Err(Error::invalid(format!(
            "{name} is already a {} connection",
            existing.as_ref().map(|e| e.api.as_str()).unwrap_or("")
        )));
    }
    let now = services::now();
    let wanted = |service: Service, asked: Option<ServiceUse>| {
        asked.map(|u| Use {
            added: existing
                .as_ref()
                .and_then(|e| e.uses(service))
                .map_or(now, |e| e.added),
            route: u.route.as_deref().and_then(|r| adapter.route(service, r)),
        })
    };
    let search = wanted(Service::Search, request.search);
    let fetch = wanted(Service::Fetch, request.fetch);
    let routes = kept(
        adapter,
        existing.as_ref(),
        request.routes,
        [(Service::Search, &search), (Service::Fetch, &fetch)],
    );
    let candidate = Connection {
        name: name.clone(),
        api,
        base_url: given(request.base_url),
        fetch_url: given(request.fetch_url),
        search,
        fetch,
        zone: given(request.zone),
        routes,
    };
    candidate.check()?;

    let key = given(request.key);
    if key.is_some() && adapter.key == Key::None {
        return Err(Error::invalid(format!("{} takes no key", adapter.title)));
    }
    let to_check = proofs(existing.as_ref(), &candidate, key.is_some());

    // The key goes in first — the check reads it exactly as a real call does.
    let previous = panic::lock(settings).secrets.raw_items(&name);
    if let Some(key) = &key {
        let item = match given(request.key_name.clone()) {
            Some(key_name) => SecretItem::Named {
                name: key_name,
                key: key.clone(),
            },
            None => SecretItem::Plain(key.clone()),
        };
        panic::lock(settings).secrets.restore(&name, vec![item])?;
    }
    let undo = |settings: &Shared| {
        if key.is_some() {
            let _ = panic::lock(settings)
                .secrets
                .restore(&name, previous.clone());
        }
    };

    let mut checked = Vec::new();
    for (service, route) in to_check {
        match check(settings, &probe(&candidate, service, route), service) {
            Ok(result) => checked.push(result),
            Err(e) => {
                undo(settings);
                return Err(e);
            }
        }
    }
    if let Err(e) = panic::lock(settings).services.put(candidate) {
        undo(settings);
        return Err(e);
    }
    Ok(json!({ "saved": name, "checked": checked }))
}

/// The routes a connection keeps once saved: those the request names — or,
/// naming none, those kept now — in the gateway's spelling and each once, with
/// every route in use among them.
fn kept(
    adapter: &web::Adapter,
    existing: Option<&Connection>,
    asked: Option<ServiceRoutes>,
    uses: [(Service, &Option<Use>); 2],
) -> SavedRoutes {
    let mut routes = SavedRoutes::default();
    for (service, used) in uses {
        let named = match &asked {
            Some(asked) => match service {
                Service::Search => asked.search.clone(),
                Service::Fetch => asked.fetch.clone(),
            },
            None => existing
                .map(|e| e.saved(service).to_vec())
                .unwrap_or_default(),
        };
        let list = routes.of_mut(service);
        for route in named
            .iter()
            .filter_map(|r| adapter.route(service, r))
            .chain(used.as_ref().and_then(|u| u.route.clone()))
        {
            if !list.contains(&route) {
                list.push(route);
            }
        }
    }
    routes
}

/// What must be proven before `candidate` is saved, as (service, route): a
/// service switched on, or whose route, address or key changed, through the
/// route it will use; every route kept for the first time, through itself;
/// and a new address or key on a connection with nothing switched on, through
/// the first route it keeps. A route only taken off the list proves nothing.
fn proofs(
    existing: Option<&Connection>,
    candidate: &Connection,
    new_key: bool,
) -> Vec<(Service, Option<String>)> {
    let moved = existing.is_none_or(|e| {
        e.base_url != candidate.base_url
            || e.fetch_url != candidate.fetch_url
            || e.zone != candidate.zone
    });
    let mut out: Vec<(Service, Option<String>)> = Vec::new();
    for service in [Service::Search, Service::Fetch] {
        let before = existing.and_then(|e| e.uses(service));
        if let Some(used) = candidate.uses(service) {
            if new_key || moved || before.map(|b| &b.route) != Some(&used.route) {
                out.push((service, used.route.clone()));
            }
        }
        let kept_before = existing.map(|e| e.saved(service)).unwrap_or(&[]);
        for route in candidate.saved(service) {
            let proven = out
                .iter()
                .any(|(s, r)| *s == service && r.as_deref() == Some(route.as_str()));
            if !proven && !kept_before.contains(route) {
                out.push((service, Some(route.clone())));
            }
        }
    }
    if out.is_empty() && (new_key || moved) {
        for service in [Service::Search, Service::Fetch] {
            if let Some(route) = candidate.saved(service).first() {
                out.push((service, Some(route.clone())));
            }
        }
    }
    out
}

/// `candidate` reading `service` through `route` — what one check sends.
fn probe(candidate: &Connection, service: Service, route: Option<String>) -> Connection {
    let mut probe = candidate.clone();
    *probe.uses_mut(service) = Some(Use { added: 0, route });
    probe
}

/// One real request through a saved connection.
pub fn test(settings: &Shared, name: &str, service: &str) -> Result<Value> {
    let service = Service::parse(service)?;
    let connection = panic::lock(settings)
        .services
        .get(name)
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("service connection {name}")))?;
    check(settings, &connection, service)
}

/// Prove `connection` serves `service`: one search, or one page read.
fn check(settings: &Shared, connection: &Connection, service: Service) -> Result<Value> {
    let (route, key, timeout) = {
        let s = panic::lock(settings);
        let route = match web::route(&s, connection, service) {
            Choice::Ready(route) => route,
            Choice::Unavailable(why) => return Err(Error::invalid(why)),
            Choice::Builtin => return Err(Error::invalid("nothing to check")),
        };
        let key = route
            .secret
            .as_deref()
            .and_then(|n| s.secrets.get(n))
            .map(String::from);
        (route, key, s.config.duration("network.probe_timeout"))
    };
    let http = web::client(settings, timeout, None);
    let started = Instant::now();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = match service {
        Service::Search => {
            let count = 3.min(route.adapter.max_results);
            web::search_once(
                &http,
                &route,
                key.as_deref(),
                web::CHECK_QUERY,
                count,
                started + timeout,
                &stop,
            )
            .map(|found| json!({ "results": found.hits.len() }))
        }
        Service::Fetch => web::fetch_once(&http, &route, key.as_deref(), web::CHECK_PAGE)
            .map(|page| json!({ "characters": page.text.chars().count() })),
    };
    let through = route
        .route
        .as_deref()
        .map(|r| format!(" through {r}"))
        .unwrap_or_default();
    let mut result = outcome.map_err(|e| {
        let why = e.to_string();
        Error::Provider(format!(
            "{} {} check{through} failed: {}",
            connection.name,
            service.noun(),
            why.strip_prefix("provider: ").unwrap_or(&why)
        ))
    })?;
    result["name"] = json!(connection.name);
    result["service"] = json!(service.noun());
    if let Some(r) = &route.route {
        result["route"] = json!(r);
    }
    result["ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(result)
}

/// Remove a service connection; its keys stay (a model connection of the same
/// name may use them, and re-adding should not mean finding the key again).
pub fn remove(settings: &Shared, name: &str) -> Result<Value> {
    if !panic::lock(settings).services.remove(name)? {
        return Err(Error::NotFound(format!("service connection {name}")));
    }
    Ok(json!({ "removed": name, "key_kept": true }))
}

/// What a vendor offers to choose from: a gateway's routes, an account's SERP
/// zones.
pub fn options(
    settings: &Shared,
    name: &str,
    api: Option<String>,
    base_url: Option<String>,
    key: Option<String>,
) -> Result<Value> {
    let (adapter, base, key, timeout) = {
        let s = panic::lock(settings);
        let existing = s.services.get(name);
        let api = given(api)
            .or_else(|| existing.map(|e| e.api.clone()))
            .unwrap_or_else(|| name.to_string());
        let adapter = web::adapter(&api)
            .ok_or_else(|| Error::invalid(format!("{api} is not a service Rune knows")))?;
        let probe = Connection {
            name: name.to_string(),
            api,
            base_url: given(base_url).or_else(|| existing.and_then(|e| e.base_url.clone())),
            fetch_url: None,
            search: None,
            fetch: None,
            zone: None,
            routes: SavedRoutes::default(),
        };
        let base = web::base_of(&s, &probe, adapter, Service::Search).ok_or_else(|| {
            Error::invalid(format!(
                "{} needs the address of your server",
                adapter.title
            ))
        })?;
        let key = given(key).or_else(|| s.secrets.get(name).map(String::from));
        let held = s.secrets.key_state(name);
        if key.is_none()
            && !matches!(adapter.key, web::Key::None)
            && matches!(held, crate::settings::secrets::KeyState::Unavailable(_))
        {
            return Err(Error::invalid(held.note(name)));
        }
        if key.is_some() {
            services::check_url(&base, adapter.key)?;
            if !web::key_may_travel(&base) {
                return Err(Error::invalid(
                    "plain http:// is only allowed for a server on this machine — use https://",
                ));
            }
        }
        (
            adapter,
            base,
            key,
            s.config.duration("network.probe_timeout"),
        )
    };
    let http = web::client(settings, timeout, None);
    let routes = if adapter.routed() {
        web::routes(&base, key.as_deref(), adapter, &http)?
    } else {
        web::Routes::default()
    };
    let zones = if adapter.zone {
        let key = key.as_deref().ok_or_else(|| {
            Error::invalid(format!("{} needs its API key to list zones", adapter.title))
        })?;
        web::zones(&base, key, &http)?
    } else {
        Vec::new()
    };
    Ok(json!({
        "name": name,
        "routes": { "search": routes.search, "fetch": routes.fetch },
        "zones": zones,
    }))
}

/// The service connections, the vendors on offer and who serves each tool,
/// for the provider screen. Key values never leave the vault: counts only.
pub fn list(s: &Settings) -> Value {
    let connections: Vec<Value> = s
        .services
        .list
        .iter()
        .map(|c| {
            let adapter = web::adapter(&c.api);
            let base = |service| adapter.and_then(|a| web::base_of(s, c, a, service));
            let (keys, ready) = s.secrets.health(&c.name);
            let used = |u: Option<&Use>| u.map(|u| json!({ "added": u.added, "route": u.route }));
            let keyless = adapter.is_none_or(|a| matches!(a.key, Key::None));
            let state = (!keyless).then(|| s.secrets.key_state(&c.name));
            json!({
                "name": c.name,
                "api": c.api,
                "title": adapter.map(|a| a.title),
                "base_url": base(Service::Search),
                "fetch_url": adapter.filter(|a| a.split()).and_then(|_| base(Service::Fetch)),
                "base_url_set": c.base_url,
                "fetch_url_set": c.fetch_url,
                "search": used(c.search.as_ref()),
                "fetch": used(c.fetch.as_ref()),
                "routes": { "search": c.routes.search, "fetch": c.routes.fetch },
                "zone": c.zone,
                "keys": keys,
                "ready": ready,
                "keys_list": s.secrets.keys_metadata(&c.name),
                "key_state": state.as_ref().map_or("none", |st| st.code()),
                "key_note": state.map(|st| st.note(&c.name)),
            })
        })
        .collect();
    let adapters: Vec<Value> = web::ADAPTERS
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "title": a.title,
                "site": a.site,
                "key": match a.key {
                    Key::Required => "required",
                    Key::Optional => "optional",
                    Key::None => "none",
                },
                "key_hint": a.key_hint,
                // Keys the vault already holds under the vendor's name — a
                // connection removed keeps them, so setting it up again needs
                // none (counts only; values never leave the vault).
                "keys": s.secrets.health(a.id).0,
                "search": a.search.is_some(),
                "fetch": a.fetch.is_some(),
                "routes": a.routed(),
                "zone": a.zone,
                "split": a.split(),
                "base_url": a.default_base(Service::Search),
                "fetch_url": a.default_base(Service::Fetch),
                "shares_model_base": a.shares_model_base,
                "max_results": a.max_results,
            })
        })
        .collect();
    json!({
        "services": connections,
        "adapters": adapters,
        "web": {
            "search": web::status(s, Service::Search),
            "fetch": web::status(s, Service::Fetch),
            "search_pin": s.config.connection(web::pin_key(Service::Search)),
            "fetch_pin": s.config.connection(web::pin_key(Service::Fetch)),
        },
    })
}
