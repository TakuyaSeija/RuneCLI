//! Format 3 → 4 (Rune 0.2.8).
//!
//! The configuration: the stall limit (`network.stream_idle_timeout`) became
//! the person's to set (owner, 2026-10-08), so `rune.toml` is laid out the way
//! a save writes it — every value and comment already there is kept. (Until
//! format 16 a layout also wrote the default in; the file now holds choices
//! only.) The layout: the names of what Rune keeps follow one
//! spelling (owner, 2026-10-08) —
//!
//! * each session's `Composer.json` is `composer.json`, beside its `meta.json`
//!   and `tasks.json`;
//! * a session host's log `run/Session<id>Host.log` is
//!   `run/session_<id>.host.log`, beside that session's socket and key.
//!
//! The locks in `run/` keep their names: two builds hold them side by side
//! while one hands over to the other, and a lock spelled two ways locks
//! nothing.

use std::path::{Path, PathBuf};

use super::Plan;
use crate::error::Result;
use crate::settings::configuration::FILE;

/// Upgrade `store` (and the configuration in `settings`) from format 3,
/// writing every change into `plan` before making it.
pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)?;
    for project in entries(&store.join("projects")) {
        for session in entries(&project.join("sessions")) {
            let old = session.join("Composer.json");
            let new = session.join(crate::store::jsonl::COMPOSER);
            // A name on a filesystem that ignores case is already the new one.
            if std::fs::symlink_metadata(&old).is_err() || crate::platform::same_file(&old, &new) {
                continue;
            }
            // One already under the new name is never overwritten: the old
            // one is set aside.
            let to = if std::fs::symlink_metadata(&new).is_ok() {
                set_aside(store, backup, &old)
            } else {
                new
            };
            plan.moves.push((old, to));
        }
    }
    for log in entries(&store.join("run")) {
        let Some(id) = log
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("Session"))
            .and_then(|rest| rest.strip_suffix("Host.log"))
            .and_then(|id| id.parse::<i64>().ok())
        else {
            continue;
        };
        let new = crate::store::run_files::host_log(store, id);
        let to = if std::fs::symlink_metadata(&new).is_ok() {
            set_aside(store, backup, &log)
        } else {
            new
        };
        plan.moves.push((log, to));
    }
    plan.make_moves(backup)?;
    plan.save(backup)
}

/// What [`apply`] would do to the configuration, checked without doing it.
/// A home still at format `from` < 3 is checked through the steps before.
pub(super) fn check(settings: &Path, from: u32) -> Result<()> {
    let text = match std::fs::read_to_string(settings.join(FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let text = if from < 2 {
        super::v3::config(&super::v2::config(&text)?)?
    } else if from < 3 {
        super::v3::config(&text)?
    } else {
        text
    };
    super::validate(&config(&text)?)
}

/// `rune.toml` in format 4.
pub(super) fn config(text: &str) -> Result<String> {
    crate::settings::configuration::laid_out(&super::v8::split(text)?)
}

/// Where a file the new layout has no place for is kept: in the upgrade's
/// backup, at its path under the home.
fn set_aside(store: &Path, backup: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix(store).unwrap_or(path);
    backup.join("obsolete").join(relative)
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}
