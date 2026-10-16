//! Format 17 → 18: a gateway connection keeps the routes the person added
//! (owner, 2026-10-15).
//!
//! A 9router connection used to hold one route per service, the one in use,
//! so a route the person had typed was gone the moment they chose another. Now
//! the connection keeps every route added (`routes.search`, `routes.fetch`) and
//! the one in use is always among them — so each route in use today becomes the
//! first route kept for its service. Nothing else about a connection changes.
//!
//! The same file carries a sentence an earlier upgrade put in the wrong place:
//! format 8 seeded `web_fetch.timeout` from `web_search.timeout`, comment and
//! all, so a fetch timeout chosen then still reads `# Search request timeout.`
//! That comment was Rune's own (its old template wrote it), never the
//! person's, and it says the wrong thing; it now says what the key is.

use std::path::Path;

use super::Plan;
use crate::error::{Error, Result};
use crate::settings::configuration::FILE;

/// The comment format 8 copied from `web_search.timeout`.
const COPIED: &str = "# Search request timeout.";

/// What `web_fetch.timeout` is, as its field describes it.
const FETCH_TIMEOUT: &str = "# Public page request timeout.";

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

fn config(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    let mut changed = keep_routes_in_use(&mut doc);
    changed |= correct_fetch_timeout_comment(&mut doc);
    if !changed {
        return Ok(text.to_string());
    }
    crate::settings::configuration::laid_out(&doc.to_string())
}

/// `text` with every route in use kept, for judging an older format's file by
/// this build's rules (`super::validate`): a route in use that is not kept is
/// refused now, and this step is what keeps it — `-upgrade-check` asked of a
/// format 17 home must not refuse what the upgrade itself puts right.
pub(super) fn seed(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    Ok(if keep_routes_in_use(&mut doc) {
        doc.to_string()
    } else {
        text.to_string()
    })
}

/// Every route a routed connection uses, kept for its service.
fn keep_routes_in_use(doc: &mut toml_edit::DocumentMut) -> bool {
    let Some(list) = doc
        .get_mut(crate::settings::services::SECTION)
        .and_then(|s| s.get_mut("list"))
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    else {
        return false;
    };
    let mut changed = false;
    for entry in list.iter_mut() {
        let routed = entry
            .get("api")
            .and_then(toml_edit::Item::as_str)
            .and_then(crate::web::adapter)
            .is_some_and(crate::web::Adapter::routed);
        if !routed {
            continue;
        }
        for service in ["search", "fetch"] {
            let Some(route) = entry
                .get(service)
                .and_then(|u| u.get("route"))
                .and_then(toml_edit::Item::as_str)
                .map(String::from)
            else {
                continue;
            };
            if !entry.contains_key("routes") {
                entry.insert("routes", toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let Some(routes) = entry
                .get_mut("routes")
                .and_then(toml_edit::Item::as_table_like_mut)
            else {
                continue;
            };
            if !routes.contains_key(service) {
                routes.insert(service, toml_edit::value(toml_edit::Array::new()));
            }
            let Some(kept) = routes
                .get_mut(service)
                .and_then(toml_edit::Item::as_array_mut)
            else {
                continue;
            };
            if !kept.iter().any(|r| r.as_str() == Some(route.as_str())) {
                kept.insert(0, route.as_str());
                changed = true;
            }
        }
    }
    changed
}

/// `web_fetch.timeout`'s comment, when it is the one format 8 copied.
fn correct_fetch_timeout_comment(doc: &mut toml_edit::DocumentMut) -> bool {
    let Some(value) = doc
        .get_mut("web_fetch")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|t| t.get_mut("timeout"))
        .and_then(toml_edit::Item::as_value_mut)
    else {
        return false;
    };
    let decor = value.decor_mut();
    let Some(suffix) = decor.suffix().and_then(|s| s.as_str()).map(String::from) else {
        return false;
    };
    let Some(before) = suffix.trim_end().strip_suffix(COPIED) else {
        return false;
    };
    decor.set_suffix(format!("{before}{FETCH_TIMEOUT}"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::configuration::Configuration;

    fn load(text: &str) -> Configuration {
        Configuration::from_values(toml_edit::de::from_str(text).unwrap()).unwrap()
    }

    /// The owner's file as 0.5.6 left it (2026-10-15), trimmed to what moves.
    const OWNER: &str = "\
[web_search]
provider = \"9router\"

[web_fetch]
timeout = \"120s\"                                 # Search request timeout.

[services]

[[services.list]]
name = \"serper\"
api = \"serper\"

[services.list.search]
added = 0

[[services.list]]
name = \"9router\"
api = \"9router\"

[services.list.search]
added = 1792012796873
route = \"ag\"

[services.list.fetch]
added = 1792011293226
route = \"exa\"
";

    #[test]
    fn each_route_in_use_becomes_the_first_route_kept() {
        let next = config(OWNER).unwrap();
        let c = load(&next);
        let services: crate::settings::Services =
            serde_json::from_value(c.get("services").clone()).unwrap();
        let nine = services.get("9router").unwrap();
        assert_eq!(nine.routes.search, ["ag"]);
        assert_eq!(nine.routes.fetch, ["exa"]);
        assert_eq!(nine.search.as_ref().unwrap().route.as_deref(), Some("ag"));
        assert_eq!(nine.search.as_ref().unwrap().added, 1792012796873);
        let serper = services.get("serper").unwrap();
        assert!(
            serper.routes.is_empty(),
            "a vendor without routes keeps none"
        );
        assert!(
            next.contains("[services.list.routes]\nsearch = [\"ag\"]\nfetch = [\"exa\"]\n"),
            "laid out for a person to read: {next}"
        );
        assert_eq!(c.connection("web_search.provider"), Some("9router"));
        assert_eq!(config(&next).unwrap(), next, "a second run changes nothing");
    }

    #[test]
    fn the_comment_format_8_copied_now_says_what_the_fetch_timeout_is() {
        let next = config(OWNER).unwrap();
        assert!(
            next.contains(
                "[web_fetch]\ntimeout = \"120s\"                                 # Public page request timeout.\n"
            ),
            "{next}"
        );
        assert!(
            next.contains("[web_search]\nprovider = \"9router\"\n"),
            "{next}"
        );
        let mine = "[web_fetch]\ntimeout = \"120s\" # my own words\n";
        assert_eq!(config(mine).unwrap(), mine, "the person's comment stays");
        let search = "[web_search]\ntimeout = \"60s\" # Search request timeout.\n";
        assert_eq!(config(search).unwrap(), search, "it is right where it is");
    }

    #[test]
    fn a_route_already_kept_is_not_kept_twice_and_nothing_else_moves() {
        let text = "[[services.list]]\nname = \"9router\"\napi = \"9router\"\n\n\
                    [services.list.search]\nadded = 1\nroute = \"glm\"\n\n\
                    [services.list.routes]\nsearch = [\"ag\", \"glm\"]\n";
        assert_eq!(config(text).unwrap(), text);
        let plain = "[ui]\ntheme = \"amber\"\n";
        assert_eq!(config(plain).unwrap(), plain);
    }
}
