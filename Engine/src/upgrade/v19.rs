//! Tools chooses an exact gateway provider, not a mutable account default.
//! Resolve old account-only pins once, preserving the route they actually used;
//! an unavailable old pin stays unavailable rather than choosing anything else.

use super::Plan;
use crate::error::{Error, Result};
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

pub(super) fn config(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    let mut changed = false;
    for (section, service) in [("web_search", "search"), ("web_fetch", "fetch")] {
        let Some(pin) = doc
            .get(section)
            .and_then(|s| s.get("provider"))
            .and_then(toml_edit::Item::as_str)
            .map(String::from)
        else {
            continue;
        };
        if pin.contains(':') {
            continue;
        }
        let route = doc
            .get("services")
            .and_then(|s| s.get("list"))
            .and_then(toml_edit::Item::as_array_of_tables)
            .and_then(|list| {
                list.iter()
                    .find(|c| c.get("name").and_then(toml_edit::Item::as_str) == Some(&pin))
            })
            .filter(|c| {
                c.get("api")
                    .and_then(toml_edit::Item::as_str)
                    .and_then(crate::web::adapter)
                    .is_some_and(crate::web::Adapter::routed)
            })
            .and_then(|c| c.get(service))
            .and_then(|u| u.get("route"))
            .and_then(toml_edit::Item::as_str)
            .map(String::from);
        if let Some(route) = route {
            let value = doc
                .get_mut(section)
                .and_then(|s| s.get_mut("provider"))
                .and_then(toml_edit::Item::as_value_mut)
                .expect("pin read above");
            let decor = value.decor().clone();
            *value = toml_edit::Value::from(format!("{pin}:{route}"));
            *value.decor_mut() = decor;
            changed = true;
        }
    }
    Ok(if changed {
        doc.to_string()
    } else {
        text.into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_gateway_pins_follow_their_exact_route_without_dropping_choices() {
        let text = "[web_search]\nprovider = \"9router\" # my choice\n\
                    [web_fetch]\nprovider = \"tavily\"\n\
                    [[services.list]]\nname = \"9router\"\napi = \"9router\"\n\
                    [services.list.search]\nadded = 10\nroute = \"ag\"\n\
                    [services.list.routes]\nsearch = [\"ag\", \"exa\"]\n";
        let next = config(text).unwrap();
        assert_eq!(
            next,
            text.replacen("provider = \"9router\"", "provider = \"9router:ag\"", 1)
        );
        assert_eq!(config(&next).unwrap(), next);
    }

    #[test]
    fn an_unavailable_pin_and_an_unpinned_fetch_are_not_reinterpreted() {
        let text = "[web_search]\nprovider = \"gone\"\n[web_fetch]\ntimeout = \"60s\"\n";
        assert_eq!(config(text).unwrap(), text);
    }
}
