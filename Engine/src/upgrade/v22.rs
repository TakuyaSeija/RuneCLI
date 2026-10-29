//! Format 21 → 22: a message's thinking count is Anthropic's, on its door.
//!
//! Rune 0.8.4 and 0.8.5 kept with every assistant message the reasoning count
//! its round's usage carried, whatever the route, and drew it as that
//! message's thinking: a route that says only in its usage how much it
//! reasoned (gemini through 9router's OpenAI door) showed a `Thought` of
//! tokens where it had sent nothing, and one that sent its thinking showed
//! tokens where its words were (owner, 2026-10-23). A message now keeps a
//! count only where its request went through a door that asks Anthropic to
//! count the thinking (`providers::door`): the route its continuation records,
//! read against the connection's adapter in `rune.toml` and the owner the
//! router's catalogue names for the model. Every other message loses the
//! count and keeps everything else, its thinking text included.
use super::Plan;
use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let counted = counted_routes(store, settings)?;
    super::rewrite_sessions(store, backup, plan, &["session.jsonl"], |_, old| {
        uncounted(old, &counted)
    })
}

/// Every route — connection and model — whose requests went through a door
/// that asked Anthropic to count the thinking.
fn counted_routes(store: &Path, settings: &Path) -> Result<BTreeSet<(String, String)>> {
    let mut routes = BTreeSet::new();
    let path = settings.join(crate::settings::configuration::FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(routes),
        Err(e) => return Err(e.into()),
    };
    let doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    let apis: Vec<(String, String)> = doc
        .get("providers")
        .and_then(|p| p.get("list"))
        .and_then(toml_edit::Item::as_array_of_tables)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?.to_string();
            // A connection that already speaks Anthropic has no second door.
            let speaks = t.get("dialect").and_then(toml_edit::Item::as_str);
            if speaks.is_some_and(|d| d != "open_ai") {
                return None;
            }
            let api = t
                .get("api")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or("compatible");
            Some((name, api.to_string()))
        })
        .collect();
    let db = store.join("rune.db");
    if apis.is_empty() || !db.is_file() {
        return Ok(routes);
    }
    let db = crate::store::connect(&db, std::time::Duration::from_secs(30))?;
    let laid: bool = db.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='provider_models'",
        [],
        |row| row.get(0),
    )?;
    if !laid {
        return Ok(routes);
    }
    let mut query = db.prepare("SELECT provider, id, info FROM provider_models")?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (provider, model, info) = row?;
        let Some((_, api)) = apis.iter().find(|(name, _)| *name == provider) else {
            continue;
        };
        // A row that does not read names no owner: its model has no door.
        let owner = serde_json::from_str::<Value>(&info).ok().and_then(|info| {
            info.pointer("/metadata/owned_by")
                .and_then(Value::as_str)
                .map(String::from)
        });
        if owner.is_some_and(|owner| crate::settings::providers::door(api, &owner).is_some()) {
            routes.insert((provider, model));
        }
    }
    Ok(routes)
}

/// The session's messages with a thinking count only where `counted` says
/// Anthropic was asked for it; every other line kept byte for byte.
fn uncounted(bytes: &[u8], counted: &BTreeSet<(String, String)>) -> Vec<u8> {
    super::rewrite_lines(bytes, |row| {
        if row.get("thinking_tokens").is_none() {
            return false;
        }
        let route = |key: &str| {
            row.pointer(&format!("/continuation/{key}"))
                .and_then(Value::as_str)
                .map(String::from)
        };
        if let (Some(provider), Some(model)) = (route("provider"), route("model")) {
            if counted.contains(&(provider, model)) {
                return false;
            }
        }
        row.as_object_mut()
            .is_some_and(|row| row.remove("thinking_tokens").is_some())
    })
}
