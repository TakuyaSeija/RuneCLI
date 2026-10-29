//! Format 20 → 21: one 9router connection.
//!
//! Rune 0.8.2 and 0.8.3 reached Claude Code's models on 9router through a
//! second connection of the same router speaking its Anthropic door, with a
//! key of its own. The one connection now sends each model through the door
//! that counts it whole (`ProviderConfig::dialect_for`; owner, 2026-10-23),
//! so such a connection is folded into the 9router connection at the same
//! address: what named it — the provider default, a model chosen for a job,
//! a session's settings, a model's saved settings, a draft — names that one,
//! and the catalogue kept for it goes. One that is the router's only
//! connection is turned back to its OpenAI door, under its own name. The
//! keys it held stay in the vault, and what it cost stays recorded under its
//! name: neither is the connection.
use super::Plan;
use crate::error::{Error, Result};
use serde_json::Value;
use std::path::Path;

pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let path = settings.join(crate::settings::configuration::FILE);
    let renames = match std::fs::read_to_string(&path) {
        Ok(text) => folds(&parse(&text)?)
            .into_iter()
            .filter_map(|fold| Some((fold.name, fold.into?)))
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    super::rewrite_configuration(settings, backup, plan, config)?;
    if renames.is_empty() {
        return Ok(());
    }
    database(store, backup, plan, &renames)?;
    super::rewrite_sessions(
        store,
        backup,
        plan,
        &["meta.json", "composer.json"],
        |_, old| renamed_file(old, &renames),
    )?;
    drafts(store, backup, plan, &renames)
}

/// One 9router connection speaking the Anthropic door: folded `into` the
/// router's connection at the same address, or, with none, turned back to
/// the OpenAI door under its own name.
struct Fold {
    name: String,
    into: Option<String>,
}

fn parse(text: &str) -> Result<toml_edit::DocumentMut> {
    text.parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))
}

fn folds(doc: &toml_edit::DocumentMut) -> Vec<Fold> {
    let Some(list) = doc
        .get("providers")
        .and_then(|p| p.get("list"))
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        return Vec::new();
    };
    let text = |t: &toml_edit::Table, key: &str| {
        t.get(key)
            .and_then(toml_edit::Item::as_str)
            .map(String::from)
    };
    let address =
        |t: &toml_edit::Table| text(t, "base_url").map(|url| url.trim_end_matches('/').to_string());
    let router = |t: &toml_edit::Table| text(t, "api").as_deref() == Some("9router");
    let anthropic = |t: &toml_edit::Table| text(t, "dialect").as_deref() == Some("anthropic");
    list.iter()
        .filter(|t| router(t) && anthropic(t))
        .filter_map(|door| {
            let name = text(door, "name")?;
            let into = list
                .iter()
                .find(|t| router(t) && !anthropic(t) && address(t) == address(door))
                .and_then(|t| text(t, "name"));
            Some(Fold { name, into })
        })
        .collect()
}

/// `rune.toml` with every Anthropic-door 9router connection folded.
pub(super) fn config(text: &str) -> Result<String> {
    let mut doc = parse(text)?;
    let folds = folds(&doc);
    if folds.is_empty() {
        return Ok(text.to_string());
    }
    let renames: Vec<(String, String)> = folds
        .iter()
        .filter_map(|f| Some((f.name.clone(), f.into.clone()?)))
        .collect();
    let gone = |name: &str| renames.iter().any(|(old, _)| old == name);
    if let Some(providers) = doc.get_mut("providers").and_then(|p| p.as_table_like_mut()) {
        if let Some(list) = providers
            .get_mut("list")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
        {
            list.retain(|t| {
                !t.get("name")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(gone)
            });
            for t in list.iter_mut() {
                let lone = t
                    .get("name")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(|name| folds.iter().any(|f| f.name == name && f.into.is_none()));
                if lone {
                    t["dialect"] = toml_edit::value("open_ai");
                }
            }
        }
        if let Some(default) = providers
            .get_mut("default")
            .and_then(toml_edit::Item::as_value_mut)
        {
            rename_value(default, &renames);
        }
        if let Some(excluded) = providers
            .get_mut("excluded_models")
            .and_then(|e| e.as_table_like_mut())
        {
            for (old, _) in &renames {
                excluded.remove(old);
            }
        }
    }
    for item in doc.iter_mut().map(|(_, item)| item) {
        rename_item(item, &renames);
    }
    Ok(doc.to_string())
}

/// Every `provider = <old>` under `item` names the connection it was folded
/// into: each model chosen for a job, as a table or inline.
fn rename_item(item: &mut toml_edit::Item, renames: &[(String, String)]) {
    match item {
        toml_edit::Item::Table(table) => {
            for (key, child) in table.iter_mut() {
                if key == "provider" {
                    if let Some(value) = child.as_value_mut() {
                        rename_value(value, renames);
                    }
                } else {
                    rename_item(child, renames);
                }
            }
        }
        toml_edit::Item::Value(value) => rename_inline(value, renames),
        toml_edit::Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                for (key, child) in table.iter_mut() {
                    // A connection's own entry is not a reference to one.
                    if key != "name" && key != "secret" {
                        rename_item(child, renames);
                    }
                }
            }
        }
        toml_edit::Item::None => {}
    }
}

fn rename_inline(value: &mut toml_edit::Value, renames: &[(String, String)]) {
    match value {
        toml_edit::Value::InlineTable(table) => {
            for (key, child) in table.iter_mut() {
                if key == "provider" {
                    rename_value(child, renames);
                } else {
                    rename_inline(child, renames);
                }
            }
        }
        toml_edit::Value::Array(items) => {
            for child in items.iter_mut() {
                rename_inline(child, renames);
            }
        }
        _ => {}
    }
}

fn rename_value(value: &mut toml_edit::Value, renames: &[(String, String)]) {
    let Some(new) = value
        .as_str()
        .and_then(|name| renames.iter().find(|(old, _)| old == name))
        .map(|(_, new)| new.clone())
    else {
        return;
    };
    let decor = value.decor().clone();
    *value = toml_edit::Value::from(new);
    *value.decor_mut() = decor;
}

/// Whether `value` named a folded connection, now renamed: an object's
/// `provider`, at any depth, and JSON kept inside a string (a session's
/// settings are stored that way).
fn renamed_json(value: &mut Value, renames: &[(String, String)]) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            for (key, child) in map.iter_mut() {
                if key == "provider" {
                    if let Some(new) = child
                        .as_str()
                        .and_then(|name| renames.iter().find(|(old, _)| old == name))
                    {
                        *child = Value::String(new.1.clone());
                        changed = true;
                        continue;
                    }
                }
                changed |= renamed_json(child, renames);
            }
            changed
        }
        Value::Array(items) => {
            // Every item, not the first that changed.
            let mut changed = false;
            for child in items.iter_mut() {
                changed |= renamed_json(child, renames);
            }
            changed
        }
        Value::String(text) if text.trim_start().starts_with('{') => {
            let Ok(mut inner) = serde_json::from_str::<Value>(text) else {
                return false;
            };
            if !renamed_json(&mut inner, renames) {
                return false;
            }
            match serde_json::to_string(&inner) {
                Ok(again) => {
                    *text = again;
                    true
                }
                Err(_) => false,
            }
        }
        _ => false,
    }
}

/// A JSON file with its folded connections renamed, written the way it was
/// (indented or not); one that does not read is left byte for byte.
fn renamed_file(bytes: &[u8], renames: &[(String, String)]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.to_vec();
    };
    if !renamed_json(&mut value, renames) {
        return bytes.to_vec();
    }
    let indented = bytes.windows(2).any(|w| w == b"\n ");
    let mut out = if indented {
        serde_json::to_vec_pretty(&value)
    } else {
        serde_json::to_vec(&value)
    }
    .unwrap_or_else(|_| bytes.to_vec());
    if bytes.ends_with(b"\n") && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

/// Sessions' settings and each model's saved settings name the connection
/// folded into; the catalogue kept for the folded one goes. The database is
/// copied whole into the backup first, so an undo puts it back.
fn database(
    store: &Path,
    backup: &Path,
    plan: &mut Plan,
    renames: &[(String, String)],
) -> Result<()> {
    let path = store.join("rune.db");
    if !path.is_file() {
        return Ok(());
    }
    let mut db = crate::store::connect(&path, std::time::Duration::from_secs(30))?;
    let has = |db: &rusqlite::Connection, table: &str| -> Result<bool> {
        Ok(db.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )?)
    };
    let mut sessions = Vec::new();
    if has(&db, "sessions")? {
        let mut query =
            db.prepare("SELECT id, settings FROM sessions WHERE settings IS NOT NULL")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, text) = row?;
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if renamed_json(&mut value, renames) {
                sessions.push((id, serde_json::to_string(&value)?));
            }
        }
    }
    let olds: Vec<&str> = renames.iter().map(|(old, _)| old.as_str()).collect();
    let count = |db: &rusqlite::Connection, table: &str| -> Result<i64> {
        if !has(db, table)? {
            return Ok(0);
        }
        let mut total = 0;
        for old in &olds {
            total += db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE provider=?1"),
                [old],
                |row| row.get::<_, i64>(0),
            )?;
        }
        Ok(total)
    };
    let saved_settings = count(&db, "model_settings")?;
    let catalogue = count(&db, "provider_models")?;
    if sessions.is_empty() && saved_settings == 0 && catalogue == 0 {
        return Ok(());
    }
    let saved = backup.join("BeforeOneRouter.db");
    db.execute("VACUUM INTO ?1", [saved.to_string_lossy().as_ref()])?;
    plan.database = Some(saved);
    plan.save(backup)?;
    let transaction = db.transaction()?;
    for (id, settings) in sessions {
        transaction.execute(
            "UPDATE sessions SET settings=?2 WHERE id=?1",
            rusqlite::params![id, settings],
        )?;
    }
    for (old, new) in renames {
        if saved_settings > 0 {
            // A model saved under both keeps the one connection's settings.
            transaction.execute(
                "DELETE FROM model_settings WHERE provider=?1 AND model IN \
                 (SELECT model FROM model_settings WHERE provider=?2)",
                rusqlite::params![old, new],
            )?;
            transaction.execute(
                "UPDATE model_settings SET provider=?2 WHERE provider=?1",
                rusqlite::params![old, new],
            )?;
        }
        if catalogue > 0 {
            transaction.execute("DELETE FROM provider_models WHERE provider=?1", [old])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// The drafts of every project name the connection folded into.
fn drafts(
    store: &Path,
    backup: &Path,
    plan: &mut Plan,
    renames: &[(String, String)],
) -> Result<()> {
    let path = store.join("drafts.json");
    let old = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            plan.skip(&path, error);
            return Ok(());
        }
    };
    let next = renamed_file(&old, renames);
    if next == old {
        return Ok(());
    }
    let saved = backup.join("drafts.json");
    crate::settings::secrets::atomic_write(&saved, &old)?;
    plan.files.push((path.clone(), saved));
    plan.save(backup)?;
    crate::settings::secrets::atomic_write(&path, &next)
}
