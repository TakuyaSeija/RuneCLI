//! The connections that serve Rune's tools rather than its model: who answers
//! `web_search`, who reads a page for `web_fetch` when the built-in reader is
//! not the one chosen (owner, 2026-10-14).
//!
//! `[services]` in `rune.toml`, beside `[providers]` and written the same way
//! (`Configuration::edit` with the section as it was read, so a save never
//! overwrites what another process wrote meanwhile). An entry is one account
//! at one vendor ([`crate::web::adapter`]) with the services switched on for
//! it; its keys are the vault ring of its name — the ring a model connection
//! of the same name uses, so 9router's one key serves its models, its search
//! and its fetch.
//!
//! # The first added serves
//!
//! While `web_search.provider` names nobody, search goes to the connection
//! whose search was switched on first — kept as the moment it was (`added`),
//! because the list is shown in alphabetical order and an order of addition
//! must not depend on how anything is displayed.
//!
//! # A gateway keeps the routes the person added
//!
//! A gateway (9router) serves each service through one of the vendors behind
//! it — a route. Every route the person adds is proven by one real request and
//! kept (`routes`), so they can switch back to it later; the one in use is
//! the service's `route`, always one of the kept ones (owner, 2026-10-15). A
//! route added is only kept; using it is a step of its own, proven again. The
//! kept routes outlive the service being switched off.
//!
//! An account may have no tool provider: keeping access is not a health claim.
//! Tools owns proved provider additions and exact pins; only choices are written.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::web::{self, Key};

/// The `rune.toml` section.
pub const SECTION: &str = "services";

/// What a connection can do for the tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Search,
    Fetch,
}

impl Service {
    pub fn noun(self) -> &'static str {
        match self {
            Service::Search => "search",
            Service::Fetch => "fetch",
        }
    }

    pub fn parse(text: &str) -> Result<Service> {
        match text {
            "search" => Ok(Service::Search),
            "fetch" => Ok(Service::Fetch),
            other => Err(Error::invalid(format!(
                "unknown service {other:?}: search or fetch"
            ))),
        }
    }
}

/// One service switched on for a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Use {
    /// When it was switched on, in milliseconds since the epoch.
    pub added: u64,
    /// Which of a gateway's vendors serves it (9router: `exa`, `ag`, a
    /// combo's name) — one of the connection's kept routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// The most routes a connection keeps for one service.
pub const MAX_ROUTES: usize = 32;

/// The routes a gateway connection keeps, per service: every one the person
/// added, in the order they added it, each proven when it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRoutes {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<String>,
}

impl SavedRoutes {
    pub fn is_empty(&self) -> bool {
        self.search.is_empty() && self.fetch.is_empty()
    }

    pub fn of(&self, service: Service) -> &[String] {
        match service {
            Service::Search => &self.search,
            Service::Fetch => &self.fetch,
        }
    }

    pub fn of_mut(&mut self, service: Service) -> &mut Vec<String> {
        match service {
            Service::Search => &mut self.search,
            Service::Fetch => &mut self.fetch,
        }
    }
}

/// One account at one vendor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// The connection's name: its vault ring, and how the person and
    /// `web_search.provider` call it.
    pub name: String,
    /// The vendor ([`crate::web::adapter`]).
    pub api: String,
    /// Where the vendor is reached, when the person chose an address of their
    /// own (search's address, for a vendor with two hosts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Fetch's address, chosen by the person, for a vendor whose fetch lives on
    /// a host of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<Use>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<Use>,
    /// Bright Data's SERP zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// A gateway's kept routes, per service.
    #[serde(default, skip_serializing_if = "SavedRoutes::is_empty")]
    pub routes: SavedRoutes,
}

impl Connection {
    pub fn uses(&self, service: Service) -> Option<&Use> {
        match service {
            Service::Search => self.search.as_ref(),
            Service::Fetch => self.fetch.as_ref(),
        }
    }

    pub fn uses_mut(&mut self, service: Service) -> &mut Option<Use> {
        match service {
            Service::Search => &mut self.search,
            Service::Fetch => &mut self.fetch,
        }
    }

    /// The routes kept for `service`.
    pub fn saved(&self, service: Service) -> &[String] {
        self.routes.of(service)
    }

    /// Whether this entry is something a person could have saved.
    pub fn check(&self) -> Result<()> {
        if !valid_name(&self.name) {
            return Err(Error::invalid(format!(
                "a service connection's name is lowercase letters, digits, '.', '_' or '-' \
                 (at most 64): {:?}",
                self.name
            )));
        }
        let adapter = web::adapter(&self.api).ok_or_else(|| {
            Error::invalid(format!(
                "services: {} names an unknown vendor {:?}",
                self.name, self.api
            ))
        })?;
        for service in [Service::Search, Service::Fetch] {
            let saved = self.saved(service);
            let used = self.uses(service);
            if (used.is_some() || !saved.is_empty()) && !adapter.serves(service) {
                return Err(Error::invalid(format!(
                    "services: {} ({}) does not offer {}",
                    self.name,
                    adapter.title,
                    service.noun()
                )));
            }
            if !adapter.routed() {
                if used.is_some_and(|u| u.route.is_some()) || !saved.is_empty() {
                    return Err(Error::invalid(format!(
                        "services: {} takes no route",
                        self.name
                    )));
                }
                continue;
            }
            if saved.len() > MAX_ROUTES {
                return Err(Error::invalid(format!(
                    "services: {} keeps at most {MAX_ROUTES} {} routes",
                    self.name,
                    service.noun()
                )));
            }
            for (at, route) in saved.iter().enumerate() {
                if !valid_text(route) {
                    return Err(Error::invalid(format!(
                        "services: {}'s {} routes hold an empty or unreadable one",
                        self.name,
                        service.noun()
                    )));
                }
                if saved[..at].contains(route) {
                    return Err(Error::invalid(format!(
                        "services: {}'s {} route {route} is kept twice",
                        self.name,
                        service.noun()
                    )));
                }
            }
            let Some(used) = used else {
                continue;
            };
            match &used.route {
                Some(route) if valid_text(route) && saved.contains(route) => {}
                Some(route) if valid_text(route) => {
                    return Err(Error::invalid(format!(
                        "services: {}'s {} route {route} is not one of its kept routes \
                         (routes.{})",
                        self.name,
                        service.noun(),
                        service.noun()
                    )))
                }
                _ => {
                    return Err(Error::invalid(format!(
                        "services: {}'s {} needs a provider route, added in Settings › Tools",
                        self.name,
                        service.noun()
                    )))
                }
            }
        }
        match (&self.zone, adapter.zone) {
            (Some(zone), true) if valid_text(zone) => {}
            (None, true) if self.search.is_none() && self.fetch.is_none() => {}
            (_, true) => {
                return Err(Error::invalid(format!(
                    "services: {} needs a SERP zone in Settings › Tools",
                    self.name
                )))
            }
            (Some(_), false) => {
                return Err(Error::invalid(format!(
                    "services: {} takes no zone",
                    self.name
                )))
            }
            (None, false) => {}
        }
        if self.fetch_url.is_some() && !adapter.split() {
            return Err(Error::invalid(format!(
                "services: {} reaches fetch at its base_url; fetch_url is for a vendor whose \
                 fetch has a host of its own",
                self.name
            )));
        }
        if adapter.default_base(Service::Search).is_none()
            && adapter.default_base(Service::Fetch).is_none()
            && self.base_url.is_none()
        {
            return Err(Error::invalid(format!(
                "services: {} needs the address of your server (base_url)",
                self.name
            )));
        }
        for url in [&self.base_url, &self.fetch_url].into_iter().flatten() {
            check_url(url, adapter.key)?;
        }
        Ok(())
    }
}

/// An address a connection may be saved with. A key never travels in clear to
/// another machine: plain `http://` is for this machine — or, for a server
/// that takes no key at all (the person's own SearXNG), their network.
pub fn check_url(url: &str, key: Key) -> Result<()> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| Error::invalid("the base URL must be a valid http(s) address"))?;
    if uri.host().is_none_or(str::is_empty)
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
    {
        return Err(Error::invalid(
            "the base URL needs a host and cannot contain credentials",
        ));
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if key == Key::None || web::key_may_travel(url) => Ok(()),
        Some("http") => Err(Error::invalid(
            "plain http:// is only allowed for a server on this machine — a key sent over \
             http can be read in transit; use https://",
        )),
        _ => Err(Error::invalid(
            "the base URL must start with http:// or https://",
        )),
    }
}

/// A Tools choice names the account and, for a gateway, its exact route.
/// Parsing never looks up an alias or substitutes another account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub route: Option<String>,
}

impl Target {
    pub fn parse(text: &str) -> Result<Self> {
        let (name, route) = match text.split_once(':') {
            Some((name, route)) => (name, Some(route)),
            None => (text, None),
        };
        if !valid_name(name) || route.is_some_and(|r| !valid_text(r) || r.trim() != r) {
            return Err(Error::invalid(
                "name a connection or connection:route, without blank or control characters",
            ));
        }
        Ok(Self {
            name: name.into(),
            route: route.map(String::from),
        })
    }

    pub fn label(&self) -> String {
        match &self.route {
            Some(route) => format!("{}:{route}", self.name),
            None => self.name.clone(),
        }
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
}

/// Every service connection of a home.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Services {
    #[serde(default)]
    pub list: Vec<Connection>,
    /// `rune.toml`'s path; empty for a configuration that cannot be saved.
    #[serde(skip)]
    path: PathBuf,
    /// The section as it was read, so a save can tell another process wrote it.
    #[serde(skip)]
    revision: Value,
    /// The configuration generation this list was read at.
    #[serde(skip)]
    seen: u64,
}

impl PartialEq for Services {
    fn eq(&self, other: &Services) -> bool {
        self.list == other.list
    }
}

impl Services {
    /// The service connections in `home`'s `rune.toml`.
    pub fn load(home: &Path) -> Result<Services> {
        let config = super::configuration::Configuration::load(home)?;
        Self::from_section(home, config.get(SECTION))
    }

    fn from_section(home: &Path, section: &Value) -> Result<Services> {
        let mut services: Services = if section.is_null() {
            Services::default()
        } else {
            serde_json::from_value(section.clone())
                .map_err(|e| Error::invalid(format!("services: {e}")))?
        };
        services.check()?;
        services.path = home.join(super::configuration::FILE);
        services.revision = section.clone();
        Ok(services)
    }

    /// Validate a whole section, as `rune.toml` holds it.
    pub fn validate(section: &Value) -> Result<()> {
        let services: Services = serde_json::from_value(section.clone())
            .map_err(|e| Error::invalid(format!("services: {e}")))?;
        services.check()
    }

    fn check(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for connection in &self.list {
            connection.check()?;
            if !names.insert(connection.name.as_str()) {
                return Err(Error::invalid(format!(
                    "services: {} is listed twice",
                    connection.name
                )));
            }
        }
        Ok(())
    }

    /// The configuration generation this list was last brought up to.
    pub fn seen(&self) -> u64 {
        self.seen
    }

    pub fn get(&self, name: &str) -> Option<&Connection> {
        self.list.iter().find(|c| c.name == name)
    }

    /// The connection whose `service` was switched on first.
    pub fn first(&self, service: Service) -> Option<&Connection> {
        self.list
            .iter()
            .filter_map(|c| c.uses(service).map(|u| (u.added, c)))
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)))
            .map(|(_, c)| c)
    }

    /// Re-read when the configuration moved since this list was read — a
    /// connection another process saved serves the next search, not the next
    /// restart. A file that does not read leaves the list as it was.
    pub fn follow(&mut self, config: &super::configuration::Shared) {
        let generation = config.generation();
        if generation == self.seen || self.path.as_os_str().is_empty() {
            self.seen = generation;
            return;
        }
        let home = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        if let Ok(mut fresh) = Services::from_section(&home, &config.get(SECTION)) {
            fresh.seen = generation;
            *self = fresh;
        }
    }

    /// Add `connection`, or replace the one of its name, and save.
    pub fn put(&mut self, connection: Connection) -> Result<()> {
        connection.check()?;
        let previous = self.list.clone();
        match self.list.iter_mut().find(|c| c.name == connection.name) {
            Some(existing) => *existing = connection,
            None => self.list.push(connection),
        }
        if let Err(e) = self.save() {
            self.list = previous;
            return Err(e);
        }
        Ok(())
    }

    /// Remove the connection named `name`. Its keys stay in the vault: they
    /// may serve a model connection of the same name, and re-adding the
    /// connection should not mean finding the key again.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let previous = self.list.clone();
        self.list.retain(|c| c.name != name);
        if self.list.len() == previous.len() {
            return Ok(false);
        }
        if let Err(e) = self.save() {
            self.list = previous;
            return Err(e);
        }
        Ok(true)
    }

    fn save(&mut self) -> Result<()> {
        let home = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| Error::invalid("this configuration has nowhere to be saved"))?
            .to_path_buf();
        let section = if self.list.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&*self)?)
        };
        super::configuration::Configuration::edit(
            &home,
            Some(SECTION),
            section.clone(),
            false,
            Some(&self.revision),
        )?;
        self.revision = section.unwrap_or(Value::Null);
        Ok(())
    }

    /// For tests: a list that saves into `home`.
    #[cfg(test)]
    pub fn at(home: &Path) -> Services {
        Services {
            path: home.join(super::configuration::FILE),
            ..Services::default()
        }
    }
}

/// Now, in the unit `added` is kept in.
pub fn now() -> u64 {
    crate::store::now_ms().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-services-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn connection(value: Value) -> Result<Connection> {
        let c: Connection = serde_json::from_value(value)?;
        c.check().map(|()| c)
    }

    #[test]
    fn an_entry_is_checked_against_what_its_vendor_offers() {
        connection(
            json!({"name":"tavily","api":"tavily","search":{"added":1},"fetch":{"added":2}}),
        )
        .unwrap();
        for (bad, why) in [
            (
                json!({"name":"brave","api":"brave","fetch":{"added":1}}),
                "does not offer fetch",
            ),
            (
                json!({"name":"x","api":"nobody","search":{"added":1}}),
                "unknown vendor",
            ),
            (
                json!({"name":"9router","api":"9router","search":{"added":1}}),
                "needs a provider route, added in Settings › Tools",
            ),
            (
                json!({"name":"tavily","api":"tavily","search":{"added":1,"route":"a"}}),
                "takes no route",
            ),
            (
                json!({"name":"brightdata","api":"brightdata","search":{"added":1}}),
                "SERP zone",
            ),
            (
                json!({"name":"searxng","api":"searxng","search":{"added":1}}),
                "address of your server",
            ),
            (
                json!({"name":"tavily","api":"tavily","fetch_url":"https://x","fetch":{"added":1}}),
                "fetch_url",
            ),
            (
                json!({"name":"Tavily","api":"tavily","search":{"added":1}}),
                "lowercase",
            ),
            (
                json!({"name":"tavily","api":"tavily","search":{"added":1},"extra":1}),
                "unknown field",
            ),
        ] {
            let e = connection(bad.clone()).unwrap_err().to_string();
            assert!(e.contains(why), "{bad} → {e}");
        }
    }

    #[test]
    fn account_access_is_valid_without_any_tool_provider() {
        for api in ["tavily", "9router", "brightdata"] {
            connection(json!({"name":api,"api":api})).unwrap();
        }
        assert!(connection(json!({"name":"searxng","api":"searxng"})).is_err());
        assert!(connection(
            json!({"name":"tavily","api":"tavily","base_url":"http://public.example"})
        )
        .is_err());
    }

    #[test]
    fn a_tools_target_has_an_exact_account_and_route_without_label_controls() {
        for label in [
            "tavily",
            "9router:ag",
            "9router:web-combo",
            "my-gateway:model:variant",
        ] {
            assert_eq!(Target::parse(label).unwrap().label(), label);
        }
        for label in [
            "9router:",
            "9router: ",
            "9router:ag\n",
            "9router: ag",
            "Bad:ag",
            ":ag",
        ] {
            assert!(Target::parse(label).is_err(), "{label:?}");
        }
    }

    #[test]
    fn a_gateway_keeps_its_routes_and_uses_one_of_them() {
        connection(json!({"name":"9router","api":"9router",
            "search":{"added":1,"route":"ag"},
            "routes":{"search":["ag","glm"],"fetch":["exa"]}}))
        .expect("a route kept for fetch while fetch is off stays kept");
        connection(json!({"name":"9router","api":"9router",
            "routes":{"search":["glm"]}}))
        .expect("every service off, the kept routes are still the person's");
        for (bad, why) in [
            (
                json!({"name":"9router","api":"9router","search":{"added":1,"route":"ag"}}),
                "not one of its kept routes",
            ),
            (
                json!({"name":"9router","api":"9router","search":{"added":1,"route":"ag"},
                    "routes":{"search":["ag","ag"]}}),
                "kept twice",
            ),
            (
                json!({"name":"9router","api":"9router","routes":{"search":[" "]}}),
                "empty or unreadable",
            ),
            (
                json!({"name":"tavily","api":"tavily","search":{"added":1},
                    "routes":{"search":["x"]}}),
                "takes no route",
            ),
            (
                json!({"name":"brave","api":"brave","search":{"added":1},
                    "routes":{"fetch":["x"]}}),
                "does not offer fetch",
            ),
            (
                json!({"name":"9router","api":"9router","routes":{"search":[],"images":["x"]}}),
                "unknown field",
            ),
        ] {
            let e = connection(bad.clone()).unwrap_err().to_string();
            assert!(e.contains(why), "{bad} → {e}");
        }
        let many: Vec<String> = (0..=MAX_ROUTES).map(|n| format!("r{n}")).collect();
        let e = connection(json!({"name":"9router","api":"9router","routes":{"search":many}}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("at most"), "{e}");
    }

    #[test]
    fn kept_routes_are_laid_out_for_a_person_to_read() {
        let home = home("routes");
        let mut services = Services::at(&home);
        services
            .put(
                connection(json!({"name":"9router","api":"9router",
                    "search":{"added":5,"route":"ag"},
                    "routes":{"search":["ag","glm"]}}))
                .unwrap(),
            )
            .unwrap();
        let text = std::fs::read_to_string(home.join("rune.toml")).unwrap();
        assert!(
            text.contains("[services.list.search]\nadded = 5\nroute = \"ag\"\n")
                && text.contains("[services.list.routes]\nsearch = [\"ag\", \"glm\"]\n"),
            "{text}"
        );
        assert_eq!(Services::load(&home).unwrap().list, services.list);
    }

    #[test]
    fn a_key_never_travels_in_clear_to_another_machine() {
        for (api, url, ok) in [
            ("tavily", "https://proxy.example/tavily", true),
            ("tavily", "http://localhost:9000", true),
            ("tavily", "http://127.0.0.1:9000", true),
            ("tavily", "http://search.example", false),
            ("9router", "http://192.168.1.5:20128/v1", false),
            ("searxng", "http://192.168.1.5:8888", true),
            ("searxng", "ftp://x", false),
            ("searxng", "https://user:pw@x", false),
        ] {
            let adapter = web::adapter(api).unwrap();
            assert_eq!(check_url(url, adapter.key).is_ok(), ok, "{api} {url}");
        }
    }

    #[test]
    fn the_first_switched_on_serves_whatever_the_list_order() {
        let services: Services = serde_json::from_value(json!({"list":[
            {"name":"tavily","api":"tavily","search":{"added":30}},
            {"name":"exa","api":"exa","search":{"added":10},"fetch":{"added":40}},
            {"name":"jina","api":"jina","fetch":{"added":20}}
        ]}))
        .unwrap();
        assert_eq!(services.first(Service::Search).unwrap().name, "exa");
        assert_eq!(services.first(Service::Fetch).unwrap().name, "jina");
    }

    #[test]
    fn a_section_saves_whole_and_a_removal_keeps_nothing_behind() {
        let home = home("save");
        let mut services = Services::at(&home);
        services
            .put(connection(json!({"name":"exa","api":"exa","search":{"added":5}})).unwrap())
            .unwrap();
        let read = Services::load(&home).unwrap();
        assert_eq!(read.list, services.list);
        let text = std::fs::read_to_string(home.join("rune.toml")).unwrap();
        assert!(text.contains("[[services.list]]"), "{text}");
        assert!(
            text.contains("\n[services.list.search]\nadded = 5\n") && !text.contains(" ]"),
            "laid out for a person to read: {text:?}"
        );
        assert!(services.remove("exa").unwrap());
        assert!(!services.remove("exa").unwrap());
        assert!(Services::load(&home).unwrap().list.is_empty());
        let text = std::fs::read_to_string(home.join("rune.toml")).unwrap_or_default();
        assert!(!text.contains("services"), "{text}");
    }

    #[test]
    fn a_save_over_a_section_another_process_changed_is_refused() {
        let home = home("race");
        let mut mine = Services::at(&home);
        mine.put(connection(json!({"name":"exa","api":"exa","search":{"added":5}})).unwrap())
            .unwrap();
        let mut theirs = Services::load(&home).unwrap();
        theirs
            .put(connection(json!({"name":"tavily","api":"tavily","search":{"added":6}})).unwrap())
            .unwrap();
        let e = mine
            .put(connection(json!({"name":"brave","api":"brave","search":{"added":7}})).unwrap())
            .unwrap_err();
        assert!(e.to_string().contains("changed externally"), "{e}");
        assert_eq!(
            mine.list.len(),
            1,
            "a refused save leaves the list as it was"
        );
    }
}
