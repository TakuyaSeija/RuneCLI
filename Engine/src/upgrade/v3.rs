//! Format 2 → 3 (Rune 0.1.6).
//!
//! The changes, the cheapest to undo first. The configuration: the retry step
//! and cap became the person's to set (owner, 2026-10-06), so `rune.toml` is
//! laid out the way a save writes it — every value and comment already there
//! is kept. (Until format 16 a layout also wrote the defaults in; the file now
//! holds choices only.) The layout, in two parts:
//!
//! * the `scratch/` that 0.1.5's sandbox report left at the top of the home
//!   (`rune -host` with no session) is set aside;
//! * the long outputs and background logs 0.1.5 kept under
//!   `<settings>/sessions/<id>/tools_result/` move into their session's own
//!   folder, where format 2 already said they belong — what has no session
//!   folder any more, or would land on a file already there, is set aside.
//!
//! No format keeps a `scratch/` or a `sessions/` at the top of the home, and
//! both are the very shape by which an unmarked home is recognised as format 1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Plan;
use crate::error::Result;
use crate::settings::configuration::FILE;

/// Upgrade `store` (and the configuration in `settings`) from format 2,
/// writing every change into `plan` before making it.
pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)?;
    layout(store, settings, backup, plan)
}

/// What [`apply`] would do to the configuration, checked without doing it.
/// A home still at format `from` < 2 is checked through the step before.
pub(super) fn check(settings: &Path, from: u32) -> Result<()> {
    let text = match std::fs::read_to_string(settings.join(FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let text = if from < 2 {
        super::v2::config(&text)?
    } else {
        text
    };
    super::validate(&config(&text)?)
}

/// `rune.toml` in format 3.
pub(super) fn config(text: &str) -> Result<String> {
    crate::settings::configuration::laid_out(&super::v8::split(text)?)
}

fn layout(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let obsolete = backup.join("obsolete");
    let scratch = store.join("scratch");
    if std::fs::symlink_metadata(&scratch).is_ok() {
        plan.moves.push((scratch, obsolete.join("scratch")));
    }
    let stray = settings.join("sessions");
    let mut emptied = Vec::new();
    if stray.is_dir() && !stray.is_symlink() {
        let folders = session_folders(store);
        for (id, files) in files_by_session(&stray)? {
            for relative in files {
                let from = stray.join(&id).join(&relative);
                let to = match folders.get(&id).map(|folder| folder.join(&relative)) {
                    Some(to) if std::fs::symlink_metadata(&to).is_err() => to,
                    _ => obsolete.join("sessions").join(&id).join(&relative),
                };
                plan.moves.push((from, to));
            }
        }
        emptied = directories_deepest_first(&stray);
    }
    plan.make_moves(backup)?;
    // What is left of the stray folders is empty now.
    for dir in emptied {
        std::fs::remove_dir(&dir)?;
        plan.emptied.push(dir);
    }
    plan.save(backup)
}

/// Every session folder the home files, by session id.
fn session_folders(store: &Path) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for project in entries(&store.join("projects")) {
        for session in entries(&project.join("sessions")) {
            if let Some(id) = session.file_name().and_then(|n| n.to_str()) {
                out.insert(id.to_string(), session.clone());
            }
        }
    }
    out
}

/// The files under each `<stray>/<id>/`, relative to it.
fn files_by_session(stray: &Path) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for path in entries(dir) {
            if path.is_dir() && !path.is_symlink() {
                walk(root, &path, out)?;
            } else if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    for session in entries(stray) {
        let Some(id) = session.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mut files = Vec::new();
        if session.is_dir() && !session.is_symlink() {
            walk(&session, &session, &mut files)?;
        } else {
            // A file where a session folder would be: set aside as it is.
            files.push(PathBuf::new());
        }
        out.insert(id.to_string(), files);
    }
    Ok(out)
}

/// `root` and every folder below it, the deepest first, so each is empty by
/// the time it is removed.
fn directories_deepest_first(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for path in entries(dir) {
            if path.is_dir() && !path.is_symlink() {
                walk(&path, out);
            }
        }
        out.push(dir.to_path_buf());
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}
