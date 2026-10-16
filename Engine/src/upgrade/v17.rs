//! Format 16 → 17: web search is served by connections (owner, 2026-10-14).
//!
//! Before, search had one vendor built in — Serper, its key the vault's
//! `serper` ring, its address `web_search.endpoint`. Now a connection serves it
//! (`[services]`, [`crate::settings::services`]), and `web_search.endpoint` is
//! no longer a key: every vendor knows its own address, and a connection holds
//! one the person chose.
//!
//! So a home whose vault holds a Serper key gets the connection that key was
//! serving: `serper`, searching, added before anything else so it keeps
//! serving until the person chooses otherwise; an endpoint the person had set
//! becomes its address (one the new rules refuse — a key sent over plain http
//! to another machine — is not carried; the file as it was stays in the
//! upgrade's backup). A home without that key had no search, and gets none.
//! The key itself stays where it is: the ring is the connection's name.
//!
//! A home with that key but no `rune.toml` is given an empty one first, so an
//! undo by any build — the previous one included — leaves a file every build
//! reads, never one holding a section the previous build does not know.

use std::path::Path;

use super::Plan;
use crate::error::{Error, Result};
use crate::settings::configuration::FILE;

/// What `web_search.endpoint` held unless the person changed it.
const DEFAULT_ENDPOINT: &str = "https://google.serper.dev/search";

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let serper = match has_serper_key(settings)? {
        Some(held) => held,
        // A store that cannot be opened is never guessed at: no connection is
        // made, and the person is told what to set up if they used Serper.
        None => {
            plan.skip(
                &settings.join(crate::settings::secrets::VAULT_FILE),
                "the credential store cannot be opened, so whether it held a Serper search key is unknown — if you used Serper, set it up in Settings › Tools › Web search",
            );
            false
        }
    };
    let path = settings.join(FILE);
    if serper && !path.exists() {
        crate::settings::secrets::atomic_write(&path, b"")?;
    }
    super::rewrite_configuration(settings, backup, plan, |text| config(text, serper))
}

/// Whether the vault holds a Serper key; `None` when its store cannot be
/// opened. Read without creating or changing anything.
fn has_serper_key(settings: &Path) -> Result<Option<bool>> {
    let secrets = crate::settings::secrets::Secrets::load(settings)?;
    Ok(match secrets.store() {
        crate::settings::secrets::Store::Unavailable(_) => None,
        _ => Some(secrets.get("serper").is_some()),
    })
}

/// `web_search.endpoint` taken out, for judging an older step's output by
/// this build's keys.
pub(super) fn retire(text: &str) -> Result<String> {
    let mut doc = parse(text)?;
    take_endpoint(&mut doc);
    Ok(doc.to_string())
}

fn parse(text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))
}

/// Remove `web_search.endpoint`, returning what the person had set there.
fn take_endpoint(doc: &mut toml_edit::DocumentMut) -> Option<String> {
    let table = doc
        .get_mut("web_search")
        .and_then(toml_edit::Item::as_table_like_mut)?;
    let old = table.remove("endpoint")?;
    old.as_str()
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != DEFAULT_ENDPOINT)
        .map(String::from)
}

fn config(text: &str, serper: bool) -> Result<String> {
    let mut doc = parse(text)?;
    let endpoint = take_endpoint(&mut doc);
    if serper && doc.get(crate::settings::services::SECTION).is_none() {
        let mut entry = toml_edit::Table::new();
        entry.insert("name", toml_edit::value("serper"));
        entry.insert("api", toml_edit::value("serper"));
        // The old endpoint named the search call itself; the connection
        // holds the vendor's address, which Rune adds `/search` to.
        let base = endpoint.map(|e| {
            let e = e.trim_end_matches('/');
            e.strip_suffix("/search").unwrap_or(e).to_string()
        });
        if let Some(base) = base
            .filter(|b| crate::settings::services::check_url(b, crate::web::Key::Required).is_ok())
        {
            entry.insert("base_url", toml_edit::value(base));
        }
        let mut search = toml_edit::Table::new();
        search.insert("added", toml_edit::value(0));
        entry.insert("search", toml_edit::Item::Table(search));
        let mut list = toml_edit::ArrayOfTables::new();
        list.push(entry);
        let mut section = toml_edit::Table::new();
        section.insert("list", toml_edit::Item::ArrayOfTables(list));
        doc.insert(
            crate::settings::services::SECTION,
            toml_edit::Item::Table(section),
        );
    }
    crate::settings::configuration::laid_out(&doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::configuration::Configuration;
    use crate::settings::services::Service;

    fn load(text: &str) -> Configuration {
        Configuration::from_values(toml_edit::de::from_str(text).unwrap()).unwrap()
    }

    #[test]
    fn a_serper_key_becomes_the_connection_that_serves_first() {
        let next = config("[web_search]\nresults = 5\n", true).unwrap();
        let c = load(&next);
        assert_eq!(c.number("web_search.results"), 5, "a choice stays");
        let services: crate::settings::Services =
            serde_json::from_value(c.get("services").clone()).unwrap();
        let serper = services.first(Service::Search).unwrap();
        assert_eq!(
            (serper.name.as_str(), serper.api.as_str()),
            ("serper", "serper")
        );
        assert_eq!(serper.search.as_ref().unwrap().added, 0);
        assert_eq!(serper.base_url, None);
        assert_eq!(
            config(&next, true).unwrap(),
            next,
            "a second run changes nothing"
        );
        assert!(
            next.ends_with(
                "[[services.list]]\nname = \"serper\"\napi = \"serper\"\n\n[services.list.search]\nadded = 0\n"
            ),
            "laid out for a person to read: {next:?}"
        );
    }

    #[test]
    fn a_chosen_endpoint_becomes_the_address_and_the_default_one_goes() {
        let next = config(
            "[web_search]\nendpoint = \"https://proxy.example/serper/search\"\n",
            true,
        )
        .unwrap();
        assert!(!next.contains("endpoint"), "{next}");
        assert!(
            next.contains("base_url = \"https://proxy.example/serper\""),
            "{next}"
        );
        load(&next);
        let next = config(
            &format!("[web_search]\nendpoint = \"{DEFAULT_ENDPOINT}\"\n"),
            true,
        )
        .unwrap();
        assert!(!next.contains("base_url"), "{next}");
        let next = config(
            "[web_search]\nendpoint = \"http://search.example/search\"\n",
            true,
        )
        .unwrap();
        assert!(
            !next.contains("base_url"),
            "a key is never sent in clear: {next}"
        );
        load(&next);
    }

    #[test]
    fn without_a_serper_key_there_is_no_connection_and_no_endpoint() {
        let next = config(
            "[web_search]\nendpoint = \"https://x.example/search\"\ntimeout = \"9s\"\n",
            false,
        )
        .unwrap();
        assert!(
            !next.contains("endpoint") && !next.contains("services"),
            "{next}"
        );
        assert_eq!(load(&next).duration("web_search.timeout").as_secs(), 9);
        assert!(Configuration::parse_value("web_search.endpoint", "\"x\"").is_err());
    }

    #[test]
    fn the_vault_is_read_only_when_it_exists() {
        let dir = std::env::temp_dir().join(format!("rune-v17-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(has_serper_key(&dir).unwrap(), Some(false));
        assert!(!dir.join("master.key").exists(), "nothing is created");
        let mut secrets = crate::settings::Secrets::load(&dir).unwrap();
        secrets.set("serper", "k").unwrap();
        assert_eq!(has_serper_key(&dir).unwrap(), Some(true));
        std::fs::remove_file(dir.join("master.key")).unwrap();
        // Unknown, never guessed — and still nothing created.
        assert_eq!(has_serper_key(&dir).unwrap(), None);
        assert!(!dir.join("master.key").exists());
    }
}
