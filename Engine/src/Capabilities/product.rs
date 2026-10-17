//! Product help belongs to the build, not to a person's editable skill folders.
//!
//! The compiled bundle is the authority. A copy on disk that differs from it
//! in any way — an edit, a lost file, an editor's swap file, a file manager's
//! note — is replaced whole by a fresh copy, so official help is always this
//! build's own text and a damaged copy never stops Rune (owner, 2026-10-16:
//! a changed package once refused every start). Each content revision lives
//! in its own folder, so builds coexist during a live handover; a revision no
//! running process holds is removed, so nothing piles up.
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/product_help.rs"));
pub const NAME: &str = "rune-settings-guide";
pub const ID: &str = "rune-settings-guide:builtin";
const DIRECTORY: &str = "product-skills";
/// Held exclusively while a revision is published, taken or removed.
const LOCK: &str = ".lock";
/// `<IN_USE><revision>` is held shared by every process that may read that
/// revision, for as long as it runs; a revision whose lock is free is unused.
const IN_USE: &str = ".in-use-";
const STAGE: &str = ".stage-";
const TRASH: &str = ".trash-";

pub fn document(path: &str) -> Option<&'static str> {
    DOCUMENTS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, text)| *text)
}

pub fn revision() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        crate::checkpoint::objects::hash_of(&serde_json::to_vec(DOCUMENTS).expect("help manifest"))
    })
}

#[cfg(test)]
fn root(home: &Path) -> PathBuf {
    home.join(DIRECTORY).join(revision())
}

fn plain_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Remove whatever is at `path` without following a link: a link goes, never
/// what it points at.
fn discard(path: &Path) -> std::io::Result<()> {
    let remove = || crate::platform::remove_entry(path);
    match remove() {
        // A read-only file (Windows refuses to delete one) or a folder that
        // lost its write bit: given back to its owner, then removed.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            writable_again(path);
            remove()
        }
        done => done,
    }
}

/// Everything under `path` this user's to change again — `0700` folders,
/// `0600` files (on Windows: not read-only) — links never followed.
fn writable_again(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    let _ = crate::platform::set_file_mode(path, mode);
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            writable_again(&entry.path());
        }
    }
}

/// Whether the package under `root` is exactly the compiled bundle: the one
/// package folder, every bundled file and nothing else (hidden files
/// included), no links, every byte the build's.
fn intact(root: &Path) -> bool {
    if !plain_directory(root) {
        return false;
    }
    let Ok(top) = std::fs::read_dir(root).and_then(|d| d.collect::<std::io::Result<Vec<_>>>())
    else {
        return false;
    };
    if top.len() != 1 || top[0].file_name() != NAME {
        return false;
    }
    let package = root.join(NAME);
    if !plain_directory(&package) {
        return false;
    }
    let mut pending = vec![PathBuf::new()];
    let mut files = 0;
    while let Some(relative) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(package.join(&relative)) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = relative.join(entry.file_name());
            let name = path.to_string_lossy().replace('\\', "/");
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_dir() {
                if !DOCUMENTS
                    .iter()
                    .any(|(p, _)| p.starts_with(&format!("{name}/")))
                {
                    return false;
                }
                pending.push(path);
                continue;
            }
            let Some(text) = document(&name).filter(|_| kind.is_file()) else {
                return false;
            };
            if std::fs::read(package.join(&path)).ok().as_deref() != Some(text.as_bytes()) {
                return false;
            }
            files += 1;
        }
    }
    files == DOCUMENTS.len()
}

/// The folder holding every revision, made a plain directory if something
/// else took its name.
fn parent(home: &Path) -> Result<PathBuf> {
    if home.as_os_str().is_empty() {
        return Err(Error::invalid("Official help needs an explicit Rune home"));
    }
    std::fs::create_dir_all(home)?;
    let parent = crate::platform::canonical(home)?.join(DIRECTORY);
    if !plain_directory(&parent) {
        discard(&parent)?;
        match std::fs::create_dir(&parent) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
            _ => {}
        }
    }
    Ok(parent)
}

fn lock(parent: &Path) -> Result<std::fs::File> {
    crate::lock::exclusive(&parent.join(LOCK), crate::lock::Wait::Yes)
}

/// The official package for this build, published when it is missing or
/// differs from the compiled bundle in any way. Offline; the person's own
/// skill folders are never touched.
pub(crate) fn ensure(home: &Path) -> Result<PathBuf> {
    let parent = parent(home)?;
    let destination = parent.join(revision());
    hold_in_use(&parent)?;
    if !intact(&destination) {
        let _lock = lock(&parent)?;
        // Another process may have published it while this one waited.
        if !intact(&destination) {
            publish(&parent, &destination)?;
        }
    }
    prune_once(&parent);
    Ok(destination)
}

/// Write a fresh copy beside the old one, then swap it in: no reader ever
/// sees half a package, and what was there before goes entirely. Only under
/// [`LOCK`], so a stage or trash folder seen here is one a crash left behind.
fn publish(parent: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(parent)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(STAGE) || name.starts_with(TRASH) {
            discard(&entry.path())?;
        }
    }
    let stage = parent.join(format!("{STAGE}{:032x}", rand::random::<u128>()));
    let written = (|| -> Result<()> {
        std::fs::create_dir(&stage)?;
        for (name, text) in DOCUMENTS {
            let output = stage.join(NAME).join(name);
            std::fs::create_dir_all(output.parent().expect("help parent"))?;
            crate::settings::secrets::atomic_write(&output, text.as_bytes())?;
        }
        if !intact(&stage) {
            return Err(Error::Internal(
                "Official help could not be written whole".into(),
            ));
        }
        Ok(())
    })();
    if let Err(error) = written {
        let _ = discard(&stage);
        return Err(error);
    }
    let trash = parent.join(format!("{TRASH}{:032x}", rand::random::<u128>()));
    let had_old = std::fs::symlink_metadata(destination).is_ok();
    if had_old {
        if let Err(error) = std::fs::rename(destination, &trash) {
            let _ = discard(&stage);
            return Err(error.into());
        }
    }
    if let Err(error) = std::fs::rename(&stage, destination) {
        if had_old {
            let _ = std::fs::rename(&trash, destination);
        }
        let _ = discard(&stage);
        return Err(error.into());
    }
    discard(&trash)?;
    Ok(())
}

/// Note, once per home for this process's life, that it may read this
/// revision. Taken under [`LOCK`], so a prune cannot remove the revision
/// between its check and this hold.
fn hold_in_use(parent: &Path) -> Result<()> {
    static HELD: std::sync::Mutex<BTreeMap<PathBuf, std::fs::File>> =
        std::sync::Mutex::new(BTreeMap::new());
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if held.contains_key(parent) {
        return Ok(());
    }
    let _lock = lock(parent)?;
    let file = crate::lock::shared(
        &parent.join(format!("{IN_USE}{}", revision())),
        crate::lock::Wait::Yes,
    )?;
    held.insert(parent.to_path_buf(), file);
    Ok(())
}

/// Remove, once per home for this process's life, every other revision no
/// running process holds, and the hold files of revisions already gone.
/// Never fatal: what cannot be removed now is tried by the next start.
fn prune_once(parent: &Path) {
    static DONE: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    if !DONE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(parent.to_path_buf())
    {
        return;
    }
    let _ = prune(parent);
}

fn is_revision(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn prune(parent: &Path) -> Result<()> {
    let _lock = lock(parent)?;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Publication happens only under the lock held here, so a stage or
        // trash folder seen now is one a crash left behind.
        if name.starts_with(STAGE) || name.starts_with(TRASH) {
            discard(&entry.path())?;
            continue;
        }
        let revision = name.strip_prefix(IN_USE).unwrap_or(&name);
        if !is_revision(revision) || revision == self::revision() {
            continue;
        }
        let hold = parent.join(format!("{IN_USE}{revision}"));
        match crate::lock::exclusive(&hold, crate::lock::Wait::No) {
            Ok(_free) => {
                discard(&parent.join(revision))?;
                discard(&hold)?;
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "product_tests.rs"]
mod tests;
