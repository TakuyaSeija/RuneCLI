//! What a session keeps in `run/` while it is open — every name spelt here
//! once: its lease (`session_<id>.lock`), its host's ownership lock
//! (`session_<id>.Host.lock`), the host's socket and key, the host's stderr
//! log, and — while a host carries the ownership lock across an exec and waits
//! for the home's upgrade — the note saying which process carries it.
//!
//! The locks keep the names earlier builds gave them: two builds hold them side
//! by side while one hands over to the other, and a lock spelt two ways locks
//! nothing.
//!
//! # What is left behind, and who clears it
//!
//! A deleted session takes its files with it ([`forget`]). A host that ended
//! without tidying — killed, or a machine switched off — leaves its socket,
//! its key and its log behind; nothing of an earlier boot is alive, so what an
//! earlier boot left is cleared when Rune starts ([`sweep`]). A file a process
//! of this boot may hold is never removed on a guess.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

fn run(root: &Path) -> PathBuf {
    root.join("run")
}

/// The lease of session `id`: held by every process that has it open.
pub fn lease(root: &Path, id: i64) -> PathBuf {
    run(root).join(format!("session_{id}.lock"))
}

/// The ownership lock of session `id`'s host.
pub fn host_lock(root: &Path, id: i64) -> PathBuf {
    run(root).join(format!("session_{id}.Host.lock"))
}

/// The socket session `id`'s host listens on.
pub fn socket(root: &Path, id: i64) -> PathBuf {
    run(root).join(format!("session_{id}.sock"))
}

/// The key session `id`'s host issued (`crate::access`).
pub fn key(root: &Path, id: i64) -> PathBuf {
    crate::access::key_path(&socket(root, id))
}

/// Session `id`'s host's stderr: what it said when it could not start, or
/// failed on its way out. Removed once its host ended with nothing to say.
pub fn host_log(root: &Path, id: i64) -> PathBuf {
    run(root).join(format!("session_{id}.host.log"))
}

/// Which process carries session `id`'s ownership lock across an exec while
/// it waits for the home's upgrade (`crate::upgrade`).
pub fn carried(root: &Path, id: i64) -> PathBuf {
    run(root).join(format!("session_{id}.carried"))
}

/// Clear what session `id` — deleted — left in `run/`. Its host lock is taken
/// first: a host still serving it keeps everything until it ends.
pub fn forget(root: &Path, id: i64) {
    let Ok(_host) = crate::lock::exclusive(&host_lock(root, id), crate::lock::Wait::No) else {
        return;
    };
    forget_held(root, id);
}

/// [`forget`], by a caller that already holds the session's lease and host
/// lock: nothing will open them again, so they go too.
pub fn forget_held(root: &Path, id: i64) {
    for path in [
        socket(root, id),
        key(root, id),
        host_log(root, id),
        carried(root, id),
        lease(root, id),
        host_lock(root, id),
    ] {
        let _ = std::fs::remove_file(path);
    }
}

/// The files of `run/`, by the session they belong to.
fn by_session(root: &Path) -> BTreeMap<i64, Vec<PathBuf>> {
    let mut out: BTreeMap<i64, Vec<PathBuf>> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(run(root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let id = name
            .strip_prefix("session_")
            .and_then(|rest| rest.split('.').next())
            .and_then(|id| id.parse::<i64>().ok());
        if let Some(id) = id {
            out.entry(id).or_default().push(entry.path());
        }
    }
    out
}

/// Clear `run/` of what nothing can use any more, as Rune starts: everything
/// of a session that no longer exists (`exists` says which do) once nobody
/// holds its locks, and of a session that does, the socket, key and empty log
/// a host of an earlier boot (`boot`, when the platform knows it) left — the
/// note of a lock it carried across an exec included.
pub fn sweep(root: &Path, exists: &HashSet<i64>, boot: Option<std::time::SystemTime>) {
    for (id, files) in by_session(root) {
        if !exists.contains(&id) {
            let Ok(_lease) = crate::lock::exclusive(&lease(root, id), crate::lock::Wait::No) else {
                continue;
            };
            forget(root, id);
            // Anything else spelt with its number is as dead.
            for file in files {
                let _ = std::fs::remove_file(file);
            }
            continue;
        }
        let Some(boot) = boot else {
            continue;
        };
        let older = |path: &Path| {
            std::fs::symlink_metadata(path)
                .and_then(|meta| meta.modified())
                .is_ok_and(|at| at < boot)
        };
        for path in [socket(root, id), key(root, id), carried(root, id)] {
            if older(&path) {
                let _ = std::fs::remove_file(path);
            }
        }
        let log = host_log(root, id);
        if older(&log) && std::fs::metadata(&log).is_ok_and(|meta| meta.len() == 0) {
            let _ = std::fs::remove_file(log);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("rune-test")
            .join(format!("run-files-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("run")).unwrap();
        root
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"").unwrap();
    }

    /// A deleted session leaves nothing in `run/` — unless a host still holds
    /// it, which keeps every file until it ends.
    #[test]
    fn a_deleted_session_leaves_nothing_behind_unless_its_host_still_runs() {
        let root = home("forget");
        for path in [
            lease(&root, 4),
            host_lock(&root, 4),
            socket(&root, 4),
            key(&root, 4),
            host_log(&root, 4),
        ] {
            touch(&path);
        }
        let host = crate::lock::exclusive(&host_lock(&root, 4), crate::lock::Wait::No).unwrap();
        forget(&root, 4);
        assert!(socket(&root, 4).exists(), "a running host keeps its files");
        drop(host);
        forget(&root, 4);
        assert!(std::fs::read_dir(root.join("run"))
            .unwrap()
            .next()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The sweep clears every file of a session that no longer exists, leaves
    /// a live session's locks alone, and clears only what an earlier boot left
    /// of its host — never the files a host of this boot may be using.
    #[test]
    fn the_sweep_clears_the_dead_and_leaves_the_living() {
        let root = home("sweep");
        // Session 9 was deleted; 987654 a test's stray; 5 exists.
        for path in [
            lease(&root, 9),
            host_lock(&root, 9),
            socket(&root, 9),
            key(&root, 9),
        ] {
            touch(&path);
        }
        touch(&socket(&root, 987654));
        for path in [
            lease(&root, 5),
            host_lock(&root, 5),
            socket(&root, 5),
            key(&root, 5),
            host_log(&root, 5),
        ] {
            touch(&path);
        }
        let exists: HashSet<i64> = [5].into();
        // Booted before the files were written: the host may be alive.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        sweep(&root, &exists, Some(long_ago));
        let left: HashSet<String> = std::fs::read_dir(root.join("run"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            [
                "session_5.lock",
                "session_5.Host.lock",
                "session_5.sock",
                "session_5.key",
                "session_5.host.log"
            ]
            .map(String::from)
            .into(),
        );
        // Booted after: what the host of an earlier boot left is dead.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        sweep(&root, &exists, Some(later));
        assert!(!socket(&root, 5).exists() && !key(&root, 5).exists());
        assert!(!host_log(&root, 5).exists(), "an empty log of a dead host");
        assert!(
            lease(&root, 5).exists() && host_lock(&root, 5).exists(),
            "locks stay"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A deleted session's files stay while a process still holds its lease.
    #[test]
    fn the_sweep_leaves_a_deleted_session_someone_still_holds() {
        let root = home("held");
        touch(&lease(&root, 3));
        touch(&socket(&root, 3));
        let held = crate::lock::exclusive(&lease(&root, 3), crate::lock::Wait::No).unwrap();
        sweep(&root, &HashSet::new(), None);
        assert!(socket(&root, 3).exists());
        drop(held);
        sweep(&root, &HashSet::new(), None);
        assert!(!socket(&root, 3).exists() && !lease(&root, 3).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
