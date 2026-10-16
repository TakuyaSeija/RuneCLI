//! Format 7 → 8: the former search tool also fetched pages. The new tools own
//! separate limits, seeded once from what the owner set for that combined
//! tool. A classifier exclusion of the old tool covered fetches too, so its
//! new name inherits that exclusion, without changing a review-all policy.

use std::path::Path;

use super::Plan;
use crate::error::{Error, Result};
use crate::settings::configuration::FILE;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

/// A previous format's layout step already writes today's defaults. Seed the
/// split before that happens so a default never masks the owner's old limit.
pub(super) fn split(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    for key in ["timeout", "max_output_chars"] {
        let old = document
            .get("web_search")
            .and_then(|table| table.get(key))
            .cloned();
        let missing = document
            .get("web_fetch")
            .and_then(|table| table.get(key))
            .is_none();
        if let Some(mut old) = old.filter(|_| missing) {
            // The value is seeded, not its comment: a comment beside the
            // search key describes search (format 18 corrects what this step
            // copied before it knew).
            if let Some(value) = old.as_value_mut() {
                value.decor_mut().clear();
            }
            if document.get("web_fetch").is_none() {
                document.insert("web_fetch", toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let table = document
                .get_mut("web_fetch")
                .and_then(toml_edit::Item::as_table_like_mut)
                .ok_or_else(|| Error::invalid("web_fetch must be a table"))?;
            table.insert(key, old);
        }
    }
    if let Some(excluded) = document
        .get_mut("classifier")
        .and_then(|table| table.get_mut("excluded_tools"))
        .and_then(toml_edit::Item::as_array_mut)
    {
        let has = |name| excluded.iter().any(|v| v.as_str() == Some(name));
        if has("web_search") && !has("web_fetch") {
            excluded.push("web_fetch");
        }
    }
    Ok(document.to_string())
}

pub(super) fn config(text: &str) -> Result<String> {
    crate::settings::configuration::laid_out(&split(text)?)
}

/// Validate the whole configuration path without touching the home. Earlier
/// format steps rename retired keys before the split is applied.
pub(super) fn check(settings: &Path, from: u32) -> Result<()> {
    let mut text = match std::fs::read_to_string(settings.join(FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if from < 2 {
        text = super::v2::config(&text)?;
    }
    if from < 3 {
        text = super::v3::config(&text)?;
    }
    if from < 4 {
        text = super::v4::config(&text)?;
    }
    super::validate(&config(&super::v14::retire(&text)?)?)
}
