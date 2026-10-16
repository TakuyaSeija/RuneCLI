//! What the engine keeps for each project, and the removal of what belonged to
//! a project that no longer exists.
//!
//! # Removed only on proof
//!
//! A project's folder holds the only copy of its sessions, its undo history and
//! its memory, so nothing is deleted on a name alone. A folder goes when the
//! path recorded inside it is reported missing by the filesystem — not
//! unreadable, not unchecked — and no session filed under it is open in any
//! process. Anything short of that keeps the folder and says why: a refusal
//! here costs disk space, never somebody's history.
//!
//! # Rows before the folder
//!
//! The sessions and grants go from the database first, then the project's MCP
//! logins from the vault, and the folder last, its identity last of all
//! ([`crate::project::remove`]). A prune cut short leaves a folder that still
//! names its project, which the next prune finds and finishes; the other order
//! would leave rows — or credentials — naming a project that exists nowhere.

use std::path::Path;
use std::sync::Mutex;

use super::jsonl::JsonlStore;
use super::Db;
use crate::error::{Error, Result};
use crate::proto::{StorageFolder, StorageKept, StoragePruned};

/// Every project folder under `root`, with its project and what it holds.
pub fn list(root: &Path) -> Vec<StorageFolder> {
    crate::project::all(root)
        .into_iter()
        .map(|folder| StorageFolder {
            folder: name_of(&folder.path),
            project: folder.project.to_string_lossy().into_owned(),
            present: present(&folder.project),
            bytes: JsonlStore::dir_size_bytes(&folder.path),
            sessions: filed(&folder.path).len(),
        })
        .collect()
}

/// Remove each named folder whose project is gone, with every session, grant,
/// unsent composition and MCP login (in the vault under `vault`) that belonged
/// to it. The database is held only while its rows go, never while a folder
/// is deleted.
pub fn prune(db: &Mutex<Db>, vault: &Path, folders: &[String]) -> Result<StoragePruned> {
    let root = crate::panic::lock(db).jsonl.root().to_path_buf();
    let mut out = StoragePruned::default();
    for name in folders {
        if out.removed.contains(name) || out.kept.iter().any(|kept| &kept.folder == name) {
            continue;
        }
        let kept = |reason: String| StorageKept {
            folder: name.clone(),
            reason,
        };
        match prune_one(db, &root, vault, name) {
            Ok(Verdict::Removed(bytes)) => {
                out.removed.push(name.clone());
                out.bytes_freed = out.bytes_freed.saturating_add(bytes);
            }
            Ok(Verdict::Kept(reason)) => out.kept.push(kept(reason)),
            Err(error) => out.kept.push(kept(format!("stopped partway: {error}"))),
        }
    }
    Ok(out)
}

enum Verdict {
    Removed(u64),
    Kept(String),
}

fn prune_one(db: &Mutex<Db>, root: &Path, vault: &Path, name: &str) -> Result<Verdict> {
    let Some(folder) = crate::project::named(root, name) else {
        return Ok(Verdict::Kept("it is not a project folder".into()));
    };
    if present(&folder.project) {
        return Ok(Verdict::Kept(format!(
            "{} still exists",
            folder.project.display()
        )));
    }
    // A project's MCP logins go with it. A credential store that cannot be
    // opened keeps the whole project — nothing of it half removed, and no
    // login left behind with no project to show it (owner, 2026-10-16).
    let mut secrets = crate::settings::secrets::Secrets::load(vault)?;
    if let crate::settings::secrets::Store::Unavailable(why) = secrets.store() {
        return Ok(Verdict::Kept(format!(
            "its MCP logins cannot be removed now: {}",
            why.message()
        )));
    }
    let project = folder.project.to_string_lossy().into_owned();
    let bytes = JsonlStore::dir_size_bytes(&folder.path);
    {
        let db = crate::panic::lock(db);
        let mut sessions = filed(&folder.path);
        {
            let mut rows = db
                .conn()
                .prepare("SELECT id FROM sessions WHERE project = ?1")?;
            for id in rows.query_map([&project], |row| row.get::<_, i64>(0))? {
                sessions.push(id?);
            }
        }
        sessions.sort_unstable();
        sessions.dedup();
        // Every lease, held while the rows go: a session open anywhere keeps
        // its folder.
        let mut leases = Vec::with_capacity(sessions.len());
        for id in &sessions {
            let lease = super::run_files::lease(root, *id);
            match crate::lock::exclusive(&lease, crate::lock::Wait::No) {
                Ok(file) => leases.push(file),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(Verdict::Kept(format!("session {id} is open")));
                }
                Err(error) => return Err(error),
            }
        }
        // First of what goes: a failure here leaves everything in place.
        secrets.remove_prefixed(&crate::capabilities::mcp::credential_prefix(
            &folder.project,
        ))?;
        for id in &sessions {
            db.delete_session(*id)?;
            super::run_files::forget(root, *id);
        }
        db.revoke_all(&project)?;
        super::drafts::Drafts::new(root).forget(&folder.project)?;
    }
    crate::project::remove(&folder.path)?;
    Ok(Verdict::Removed(bytes))
}

/// Whether `project` is still there: false only on the filesystem's own "not
/// found". Any other answer — no permission, an I/O error — counts as there,
/// so a path that cannot be checked never reads as a deleted project.
fn present(project: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(project),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// The sessions filed under a project folder.
fn filed(folder: &Path) -> Vec<i64> {
    let Ok(entries) = std::fs::read_dir(folder.join("sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect()
}

fn name_of(folder: &Path) -> String {
    folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Store {
        db: Mutex<Db>,
        root: std::path::PathBuf,
        work: std::path::PathBuf,
    }

    fn store(name: &str) -> Store {
        let base = std::env::temp_dir()
            .join("rune-test")
            .join(format!("storage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("home");
        std::fs::create_dir_all(&root).unwrap();
        let db = Db::open(&root.join("rune.db"), std::time::Duration::from_secs(5)).unwrap();
        Store {
            db: Mutex::new(db),
            root,
            work: base.join("work"),
        }
    }

    impl Store {
        /// A project with one session that has a line of history, a grant,
        /// and a note in its memory.
        fn project(&self, name: &str) -> (std::path::PathBuf, i64, String) {
            let project = self.work.join(name);
            std::fs::create_dir_all(&project).unwrap();
            let project = crate::platform::canonical(&project).unwrap();
            let text = project.to_string_lossy().into_owned();
            let db = crate::panic::lock(&self.db);
            let session = db.create_session(&text).unwrap();
            db.append_message(
                session.id,
                super::super::sessions::NewMessage::plain(
                    1,
                    super::super::sessions::Role::User,
                    "hello",
                ),
            )
            .unwrap();
            db.grant(&text, "terminal_use", "cargo build").unwrap();
            let folder = crate::project::find(&self.root, &project).unwrap();
            std::fs::create_dir_all(folder.join("memory")).unwrap();
            std::fs::write(folder.join("memory/MEMORY.md"), "- a note\n").unwrap();
            let name = name_of(&folder);
            (project, session.id, name)
        }
    }

    #[test]
    fn the_list_names_every_folder_its_project_and_whether_it_is_still_there() {
        let s = store("list");
        let (kept, _, kept_name) = s.project("kept");
        let (gone, _, gone_name) = s.project("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let listed = list(&s.root);
        let entry = |name: &str| listed.iter().find(|f| f.folder == name).unwrap().clone();
        assert!(entry(&kept_name).present);
        assert_eq!(entry(&kept_name).project, kept.to_string_lossy());
        assert_eq!(entry(&kept_name).sessions, 1);
        assert!(entry(&kept_name).bytes > 0);
        assert!(!entry(&gone_name).present);
    }

    /// The owner's case: a deleted project's folder goes with its sessions,
    /// its grants and its memory; a project that is still there is untouched
    /// whatever it is asked.
    #[test]
    fn only_a_gone_projects_folder_and_rows_are_pruned() {
        let s = store("prune");
        let (kept, kept_session, kept_name) = s.project("kept");
        let (gone, gone_session, gone_name) = s.project("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let gone_folder = s.root.join("projects").join(&gone_name);

        let pruned = prune(
            &s.db,
            &s.root,
            &[gone_name.clone(), kept_name.clone(), "../run".into()],
        )
        .unwrap();
        assert_eq!(pruned.removed, vec![gone_name.clone()]);
        assert!(pruned.bytes_freed > 0);
        let reasons: Vec<&str> = pruned.kept.iter().map(|k| k.reason.as_str()).collect();
        assert!(reasons[0].contains("still exists"), "{reasons:?}");
        assert!(reasons[1].contains("not a project folder"), "{reasons:?}");

        assert!(!gone_folder.exists());
        let db = crate::panic::lock(&s.db);
        assert!(
            db.get_session(gone_session).is_err(),
            "its session row is gone"
        );
        assert!(db.grants(&gone.to_string_lossy()).unwrap().is_empty());
        assert!(db.get_session(kept_session).is_ok());
        assert_eq!(db.grants(&kept.to_string_lossy()).unwrap().len(), 1);
        assert!(s
            .root
            .join("projects")
            .join(&kept_name)
            .join("memory/MEMORY.md")
            .exists());
    }

    /// A gone project's MCP logins leave the vault with it (owner, 2026-10-06);
    /// the logins of a project that is still there, and every other key, stay.
    #[test]
    fn a_pruned_projects_mcp_logins_leave_the_vault_and_nothing_else_does() {
        let s = store("vault");
        let (kept, _, kept_name) = s.project("kept");
        let (gone, _, gone_name) = s.project("gone");
        let login = |project: &Path, label: &str| {
            format!(
                "{}server:digest:{label}",
                crate::capabilities::mcp::credential_prefix(project)
            )
        };
        crate::settings::secrets::write_store(
            &s.root,
            &serde_json::json!({
                login(&gone, "oauth"): "gone-token",
                login(&gone, "api"): "gone-key",
                login(&kept, "oauth"): "kept-token",
                "openrouter": "provider-key",
            })
            .to_string(),
        );
        std::fs::remove_dir_all(&gone).unwrap();

        let pruned = prune(&s.db, &s.root, &[gone_name.clone(), kept_name]).unwrap();
        assert_eq!(pruned.removed, vec![gone_name]);
        let vault = crate::settings::secrets::Secrets::load(&s.root).unwrap();
        let mut names = vault.names();
        names.sort_unstable();
        let mut expected = vec![login(&kept, "oauth"), "openrouter".to_string()];
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    /// Owner, 2026-10-16: a store that cannot be opened left a pruned project
    /// half gone — its sessions deleted, its logins and folder still there.
    /// Now nothing of it moves, and the reason is said.
    #[test]
    fn a_credential_store_that_cannot_be_opened_keeps_the_whole_project() {
        let s = store("vault-unavailable");
        let (gone, gone_session, gone_name) = s.project("gone");
        crate::settings::secrets::write_store(
            &s.root,
            &serde_json::json!({ "openrouter": "provider-key" }).to_string(),
        );
        std::fs::remove_file(s.root.join("master.key")).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        let folder = s.root.join("projects").join(&gone_name);

        let pruned = prune(&s.db, &s.root, std::slice::from_ref(&gone_name)).unwrap();
        assert!(pruned.removed.is_empty());
        assert!(
            pruned.kept[0].reason.contains("master.key"),
            "{:?}",
            pruned.kept
        );
        assert!(folder.join("memory/MEMORY.md").exists());
        let db = crate::panic::lock(&s.db);
        assert!(db.get_session(gone_session).is_ok(), "no session went");
        assert_eq!(db.grants(&gone.to_string_lossy()).unwrap().len(), 1);
    }

    /// A session open in any process keeps its folder, however gone its
    /// project looks.
    #[test]
    fn an_open_session_keeps_its_folder() {
        let s = store("open");
        let (gone, session, name) = s.project("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let _lease = crate::lock::shared(
            &s.root.join("run").join(format!("session_{session}.lock")),
            crate::lock::Wait::Yes,
        )
        .unwrap();
        let pruned = prune(&s.db, &s.root, std::slice::from_ref(&name)).unwrap();
        assert!(pruned.removed.is_empty());
        assert!(pruned.kept[0].reason.contains("is open"), "{pruned:?}");
        assert!(s.root.join("projects").join(&name).exists());
        assert!(crate::panic::lock(&s.db).get_session(session).is_ok());
    }

    /// A prune cut short leaves a folder that still names its project, and the
    /// next prune finishes it.
    #[test]
    fn a_prune_cut_short_is_finished_by_the_next() {
        let s = store("resume");
        let (gone, session, name) = s.project("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        // As if the first prune stopped after the rows went.
        crate::panic::lock(&s.db).delete_session(session).unwrap();
        assert_eq!(list(&s.root).len(), 1, "still listed by its identity");
        let pruned = prune(&s.db, &s.root, std::slice::from_ref(&name)).unwrap();
        assert_eq!(pruned.removed, vec![name]);
        assert!(list(&s.root).is_empty());
    }
}
