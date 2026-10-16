//! Format 4 → 5 (Rune 0.3.0).
//!
//! `drafts.json` keeps no composition that holds nothing (owner, 2026-10-09).
//! An emptied composition used to stay as an entry — a tombstone, so a new
//! window did not resurrect an older one — and every window closed without a
//! word left one behind. A project now remembers the order at which its
//! newest composition was emptied (`cleared`), which says what the tombstones
//! said; the tombstones themselves go.
//!
//! Read and written as JSON, not as the store's types: a step describes the
//! format it was written for, whatever the store's types become later.

use std::path::Path;

use serde_json::Value;

use super::Plan;
use crate::error::Result;

const DRAFTS: &str = "drafts.json";
const DRAFTS_LOCK: &str = "run/Drafts.lock";

/// Upgrade `store` from format 4, writing every change into `plan` before
/// making it.
pub(super) fn apply(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let path = store.join(DRAFTS);
    // The same lock every draft write takes: a window on a draft holds no
    // session, so the upgrade's barrier does not wait for it.
    let _lock = crate::lock::exclusive(&store.join(DRAFTS_LOCK), crate::lock::Wait::Yes)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // A registry a power cut left unreadable is left as it was: the upgrade
    // of the rest of the home goes on.
    let Ok(mut registry) = serde_json::from_slice::<Value>(&bytes) else {
        plan.skip(&path, "the drafts registry does not read");
        return Ok(());
    };
    if !fold(&mut registry) {
        return Ok(());
    }
    let saved = backup.join(DRAFTS);
    std::fs::write(&saved, &bytes)?;
    plan.files.push((path.clone(), saved));
    plan.save(backup)?;
    crate::settings::secrets::atomic_write(&path, &crate::store::jsonl::pretty(&registry)?)
}

/// Take every entry that holds nothing out of `registry`, remembering the
/// newest one's order as the project's `cleared`. Whether anything changed.
pub(super) fn fold(registry: &mut Value) -> bool {
    let Some(projects) = registry.get_mut("projects").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for project in projects.values_mut() {
        let Some(project) = project.as_object_mut() else {
            continue;
        };
        let mut cleared = project.get("cleared").and_then(Value::as_u64).unwrap_or(0);
        let Some(entries) = project.get_mut("entries").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| {
            if holds_nothing(&entry["composer"]) {
                cleared = cleared.max(entry["order"].as_u64().unwrap_or(0));
                false
            } else {
                true
            }
        });
        if entries.len() != before {
            project.insert("cleared".into(), cleared.into());
            changed = true;
        }
    }
    changed
}

/// No text, no file, no model or mode chosen, no message waiting — the
/// format-4 spelling of `Composer::holds_nothing`.
fn holds_nothing(composer: &Value) -> bool {
    let empty = |key: &str| match &composer[key] {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    };
    empty("text")
        && empty("attachments")
        && empty("pending")
        && !composer["preference_changed"].as_bool().unwrap_or(false)
}
