//! Pre-write snapshots — the agent's only undo.
//!
//! The owner does not use Git. That single fact is why this module is a gate
//! rather than a convenience: **no write tool may run before the file it is
//! about to change has been captured here.** If capture fails, the write fails.
//!
//! Shape of one snapshot: for every path a turn is about to modify, the content
//! it had *before* — or a marker that it did not exist, in which case undoing
//! means deleting it. One checkpoint per turn, however many files that turn ends
//! up touching.
//!
//! Undo is per file (owner, 2026-10-09): a file is put back the way it was
//! before the session first touched it, from the session's file ledger
//! (`session_files`), which outlives the pruning of the per-turn manifests.
//! The manifests stay what a capture writes — a restore included, so what it
//! overwrote is kept like any other write.

pub mod diff;
pub mod objects;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::jail::Jail;
use crate::panic;
use crate::store::checkpoints::{Checkpoint, Snapshot};
use crate::store::Db;
use objects::{Objects, Swept};

/// How one file stands now, compared with the checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    /// The turn created it: nothing was there before.
    Created,
    /// It existed before the turn and is gone now.
    Deleted,
    Modified,
    /// Captured, but what is on disk matches the snapshot — the turn touched it
    /// and put it back, or a later restore already undid it.
    Unchanged,
    /// Not text on one side or the other. There is nothing to show line by line.
    Binary,
    /// The state before the session is no longer in the object store (removed
    /// by hand): nothing to diff against or put back. Its counts are the write
    /// ledger's running tally, the only numbers left.
    Missing,
}

/// One file's difference between a checkpoint and the working tree.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileDiff {
    /// The path as the jail displays it — project-relative, or under the memory
    /// alias. Never the absolute location.
    pub path: String,
    pub change: Change,
    /// Empty for every change a frontend cannot render line by line.
    pub hunks: Vec<diff::Hunk>,
}

/// One file a session has changed, with its session-long totals.
///
/// Totals are measured against the file's state **before the session first
/// touched it**, so a file edited many times is one row whose counts are the
/// session's net — never a sum of overlapping per-turn diffs. [`Change::Created`]
/// carries every line as `added`; [`Change::Deleted`] every line as `removed`;
/// [`Change::Missing`] carries the write ledger's own running tally for the
/// file, the only numbers left. [`Change::Binary`] reports `0/0`: line
/// arithmetic has nothing to say about bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    /// The path as the jail displays it — project-relative, under the memory
    /// alias, or absolute for a file outside the project.
    pub path: String,
    pub change: Change,
    pub added: u64,
    pub removed: u64,
    /// When the session last changed the file, in milliseconds since the
    /// epoch; `None` when the ledger row predates the record of it.
    #[serde(default)]
    pub last_at: Option<i64>,
}

/// What a prune reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pruned {
    pub manifests: usize,
    pub blobs: usize,
    pub bytes: u64,
}

pub struct Checkpoints {
    db: Arc<Mutex<Db>>,
    objects: Objects,
    config: crate::settings::configuration::Shared,
    tallies: Mutex<std::collections::BTreeMap<(i64, String), Tally>>,
}

struct Tally {
    before: Option<String>,
    after: Option<String>,
    budget: u64,
    change: Change,
    added: u64,
    removed: u64,
}

impl Checkpoints {
    /// `objects` is the project's own blob folder (see `crate::project`).
    pub fn new(
        db: Arc<Mutex<Db>>,
        objects: PathBuf,
        config: &crate::settings::configuration::Shared,
    ) -> Checkpoints {
        Checkpoints {
            db,
            objects: Objects::new(objects),
            config: config.clone(),
            tallies: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Capture `path` as it is right now, before a tool changes it.
    ///
    /// Returns `true` when this call created the record, `false` when the turn
    /// had already captured that path — the first capture is the one that
    /// matters, because it holds the state from before the turn began.
    /// `jail` is the one in force for the session right now — which is the whole
    /// point of passing it: in full-access mode a captured path may legitimately
    /// sit outside the project, and the checkpoint must still hold it.
    pub fn capture(&self, session: i64, turn: i64, path: &Path, jail: &Jail) -> Result<bool> {
        if !jail.contains(path) {
            return Err(Error::Jail(path.display().to_string()));
        }
        let rel = jail.display(path);

        let snap = match std::fs::symlink_metadata(path) {
            Err(_) => Snapshot {
                path: rel.clone(),
                blob: None,
                mode: None,
            },
            Ok(meta) => {
                if meta.is_dir() {
                    return Err(Error::invalid(format!("{rel} is a directory")));
                }
                let bytes = crate::settings::configuration::read_bounded(
                    path,
                    self.config.number("checkpoint.max_file_size"),
                )?;
                let blob = self.objects.put(&bytes)?;
                Snapshot {
                    path: rel.clone(),
                    blob: Some(blob),
                    mode: Some(mode_of(&meta)),
                }
            }
        };

        let (first_blob, first_mode) = (snap.blob.clone(), snap.mode);
        let db = panic::lock(&self.db);
        let created = db.record_snapshot(session, turn, snap)?;
        if created {
            // The session ledger, one row per path: the first touch holds the
            // file's pre-session state — what [`Checkpoints::session_changes`]
            // measures against and [`Checkpoints::restore_file`] puts back —
            // and a later touch only moves `last_turn`.
            db.record_session_file(session, &rel, first_blob.as_deref(), first_mode, turn)?;
        }
        Ok(created)
    }

    /// Stop before touching `path` unless its previous state is safely captured.
    ///
    /// Every write in the engine goes through here. That is the whole of "write
    /// tools stay disabled until the checkpoint engine exists": there is no path
    /// to a write that does not pass this line.
    pub fn record_change(
        &self,
        session: i64,
        turn: i64,
        path: &Path,
        jail: &Jail,
        delta_add: u64,
        delta_rem: u64,
    ) -> Result<()> {
        let rel = jail.display(path);
        let db = panic::lock(&self.db);
        db.record_session_file_change(session, &rel, delta_add, delta_rem, turn)
    }

    /// One file of the session's ledger, by the path the jail displays it
    /// under, and its content before the session first touched it.
    fn first_state(
        &self,
        session: i64,
        path: &str,
    ) -> Result<(crate::store::checkpoints::SessionFile, Option<Vec<u8>>)> {
        let file = panic::lock(&self.db)
            .session_file(session, path)?
            .ok_or_else(|| {
                Error::NotFound(format!("{path} among the files this session changed"))
            })?;
        let before = match &file.first_blob {
            None => None,
            Some(hash) => match self.objects.get(hash) {
                Ok(bytes) => Some(bytes),
                Err(Error::NotFound(_)) => {
                    return Err(Error::invalid(format!(
                        "{path} cannot be put back: its content from before this session is no longer kept"
                    )))
                }
                Err(error) => return Err(error),
            },
        };
        Ok((file, before))
    }

    /// The file at `abs` as it is now; `None` when there is no file there.
    fn current(&self, abs: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::symlink_metadata(abs) {
            Ok(meta) if meta.is_file() => Ok(Some(crate::settings::configuration::read_bounded(
                abs,
                self.config.number("checkpoint.max_file_size"),
            )?)),
            Ok(_) => Err(Error::invalid(
                "tracked checkpoint path is no longer a regular file",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// What [`Checkpoints::restore_file`] would undo: `path` before the
    /// session first touched it against the file as it is now — every later
    /// change of it together, the session's and anyone else's.
    pub fn file_diff(&self, session: i64, path: &str, jail: &Jail) -> Result<FileDiff> {
        let (file, before) = self.first_state(session, path)?;
        let after = self.current(&jail.resolve(&file.path)?)?;
        Ok(FileDiff {
            path: file.path,
            ..compare(before.as_deref(), after.as_deref(), &self.config.snapshot())
        })
    }

    /// Put `path` back the way it was before the session first touched it:
    /// its content and mode then, or no file when it did not exist. Whether
    /// anything was written — a file already as it was is left alone.
    ///
    /// What the file holds now is captured under `turn` first, as for any
    /// write, and kept as the file's redo — what [`Checkpoints::redo_file`]
    /// puts back — replacing an earlier undo's. The file is written the way a
    /// tool writes it — held through `memory` (see
    /// [`crate::memory::Memory::hold`]) — so undoing a note never lands
    /// halfway through another session's change to the same index.
    pub fn restore_file(
        &self,
        session: i64,
        path: &str,
        jail: &Jail,
        memory: &crate::memory::Memory,
        turn: i64,
    ) -> Result<bool> {
        let (file, before) = self.first_state(session, path)?;
        let abs = jail.resolve(&file.path)?;
        let now = self.current(&abs)?;
        if before == now {
            return Ok(false);
        }
        self.capture(session, turn, &abs, jail)?;
        let kept = match &now {
            Some(bytes) => Some(self.objects.put(bytes)?),
            None => None,
        };
        let mode = std::fs::symlink_metadata(&abs)
            .ok()
            .map(|meta| mode_of(&meta));
        self.put_back(&abs, before.as_deref(), file.first_mode, memory)?;
        let db = panic::lock(&self.db);
        db.set_redo(session, &file.path, kept.as_deref(), mode)?;
        db.touch_session_file(session, &file.path, turn)?;
        Ok(true)
    }

    /// Put back what the last undo of `path` overwrote — the file as it was
    /// the moment it was undone, whatever happened to it since — and use the
    /// redo up: there is none until the next undo. Whether anything was
    /// written. What the file holds now is captured under `turn` first, as
    /// for any write.
    pub fn redo_file(
        &self,
        session: i64,
        path: &str,
        jail: &Jail,
        memory: &crate::memory::Memory,
        turn: i64,
    ) -> Result<bool> {
        let (file, undone) = self.redo_state(session, path)?;
        let abs = jail.resolve(&file.path)?;
        let wrote = undone != self.current(&abs)?;
        if wrote {
            self.capture(session, turn, &abs, jail)?;
            self.put_back(&abs, undone.as_deref(), file.redo_mode, memory)?;
        }
        let db = panic::lock(&self.db);
        db.clear_redo(session, &file.path)?;
        if wrote {
            db.touch_session_file(session, &file.path, turn)?;
        }
        Ok(wrote)
    }

    /// What [`Checkpoints::redo_file`] would change: the file as it is now
    /// against the file as the last undo found it.
    pub fn redo_diff(&self, session: i64, path: &str, jail: &Jail) -> Result<FileDiff> {
        let (file, undone) = self.redo_state(session, path)?;
        let now = self.current(&jail.resolve(&file.path)?)?;
        Ok(FileDiff {
            path: file.path,
            ..compare(now.as_deref(), undone.as_deref(), &self.config.snapshot())
        })
    }

    /// Every file of the session that has a redo, in first-touch order: its
    /// `+N −N` is what redoing it would change, `last_at` when it was undone.
    pub fn redo_changes(&self, session: i64, jail: &Jail) -> Result<Vec<FileChange>> {
        let files = panic::lock(&self.db).session_files(session)?;
        let mut out = Vec::new();
        for file in files.into_iter().filter(|f| f.redo_at.is_some()) {
            let Ok(abs) = jail.resolve(&file.path) else {
                continue;
            };
            let undone = match &file.redo_blob {
                Some(hash) => match self.objects.get(hash) {
                    Ok(bytes) => Some(bytes),
                    Err(Error::NotFound(_)) => {
                        out.push(FileChange {
                            path: file.path,
                            change: Change::Missing,
                            added: 0,
                            removed: 0,
                            last_at: file.redo_at,
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            };
            let d = compare(
                self.current(&abs)?.as_deref(),
                undone.as_deref(),
                &self.config.snapshot(),
            );
            let (added, removed) = counts(&d);
            out.push(FileChange {
                path: file.path,
                change: d.change,
                added,
                removed,
                last_at: file.redo_at,
            });
        }
        Ok(out)
    }

    /// One file's redo: its ledger row and what the last undo overwrote.
    fn redo_state(
        &self,
        session: i64,
        path: &str,
    ) -> Result<(crate::store::checkpoints::SessionFile, Option<Vec<u8>>)> {
        let file = panic::lock(&self.db)
            .session_file(session, path)?
            .filter(|file| file.redo_at.is_some())
            .ok_or_else(|| {
                Error::NotFound(format!("{path} among the files there is a redo for"))
            })?;
        let undone = match &file.redo_blob {
            None => None,
            Some(hash) => match self.objects.get(hash) {
                Ok(bytes) => Some(bytes),
                Err(Error::NotFound(_)) => {
                    return Err(Error::invalid(format!(
                        "{path} cannot be redone: what the undo replaced is no longer kept"
                    )))
                }
                Err(error) => return Err(error),
            },
        };
        Ok((file, undone))
    }

    /// Write `content` (or remove the file, for `None`) at `abs` with `mode`,
    /// held through `memory` like any tool's write.
    fn put_back(
        &self,
        abs: &Path,
        content: Option<&[u8]>,
        mode: Option<u32>,
        memory: &crate::memory::Memory,
    ) -> Result<()> {
        let held = memory.hold(abs)?;
        match content {
            Some(bytes) => {
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                held.write(abs, bytes)?;
                if let Some(mode) = mode {
                    set_mode(abs, mode)?;
                }
            }
            None => {
                if abs.exists() {
                    std::fs::remove_file(abs)?;
                }
            }
        }
        Ok(())
    }

    /// What this session has done to every file it ever touched, measured
    /// against the state each file had **before the session first reached it**.
    ///
    /// Adding up what each turn did would double-count: turn 2 deletes what
    /// turn 1 added, and both describe it. This is the session's own answer,
    /// from the ledger [`Checkpoints::capture`] keeps: one row per touched
    /// path, in first-touch order, its `+N −N` the net of the whole session —
    /// what [`Checkpoints::restore_file`] would undo. A file put back reports
    /// [`Change::Unchanged`] with nothing to count — what a restore did is a
    /// fact about the past, not about the tree.
    pub fn session_changes(&self, session: i64, jail: &Jail) -> Result<Vec<FileChange>> {
        let files = {
            let db = panic::lock(&self.db);
            db.session_files(session)?
        };

        let mut out = Vec::with_capacity(files.len());
        for file in files {
            let abs = match jail.resolve(&file.path) {
                // A path the session is no longer allowed to reach — a one-call
                // grant left behind by an earlier run, say — cannot be tallied
                // right now. Its ledger row survives and it reappears the moment
                // it is reachable again; the tally must never jam the resume
                // door on it.
                Err(_) => continue,
                Ok(abs) => abs,
            };

            let before = match &file.first_blob {
                Some(hash) => match self.objects.get(hash) {
                    Ok(bytes) => Some(bytes),
                    // Its state before the session is gone: the write ledger's
                    // tally is the only count left, and nothing can be put back.
                    Err(Error::NotFound(_)) => {
                        out.push(FileChange {
                            path: file.path,
                            change: Change::Missing,
                            added: file.added,
                            removed: file.removed,
                            last_at: file.last_at,
                        });
                        continue;
                    }
                    Err(error) => return Err(error),
                },
                None => None,
            };
            let after = self.current(&abs)?;
            let after_hash = after.as_deref().map(objects::hash_of);
            let config = self.config.snapshot();
            let budget = config.number("checkpoint.diff.max_cells");
            let key = (session, file.path.clone());
            let mut tallies = panic::lock(&self.tallies);
            let reused = tallies.get(&key).filter(|tally| {
                tally.before == file.first_blob
                    && tally.after == after_hash
                    && tally.budget == budget
            });
            let (change, added, removed) = if let Some(tally) = reused {
                (tally.change, tally.added, tally.removed)
            } else {
                let d = compare(before.as_deref(), after.as_deref(), &config);
                let (added, removed) = counts(&d);
                tallies.insert(
                    key,
                    Tally {
                        before: file.first_blob,
                        after: after_hash,
                        budget,
                        change: d.change,
                        added,
                        removed,
                    },
                );
                (d.change, added, removed)
            };
            out.push(FileChange {
                path: file.path,
                change,
                added,
                removed,
                last_at: file.last_at,
            });
        }
        Ok(out)
    }

    /// Keep the `keep` newest checkpoints of a session, then delete every blob
    /// no manifest of this project still names.
    ///
    /// Called automatically at the end of a turn, so undo history stays bounded
    /// without anyone remembering to prune it.
    pub fn prune(&self, session: i64, project: &str, keep: usize) -> Result<Pruned> {
        let (manifests, referenced) = {
            let db = panic::lock(&self.db);
            let removed = db.prune_checkpoints(session, keep)?;
            (removed, db.referenced_blobs(project)?)
        };
        let swept = self.objects.sweep(&referenced)?;
        Ok(Pruned {
            manifests,
            blobs: swept.blobs,
            bytes: swept.bytes,
        })
    }

    /// Blob collection with no manifest pruning — for when a whole session was
    /// deleted and its manifests are already gone.
    pub fn collect(&self, project: &str) -> Result<Swept> {
        let referenced = {
            let db = panic::lock(&self.db);
            db.referenced_blobs(project)?
        };
        self.objects.sweep(&referenced)
    }

    /// The content whose SHA-256 is `hash`, when a capture of this project
    /// holds it — what a file was before Rune's own tools changed it, and so
    /// what an attachment of it was when it was sent.
    pub fn original(&self, hash: &str) -> Option<Vec<u8>> {
        self.objects.get(hash).ok()
    }

    pub fn list(&self, session: i64) -> Result<Vec<Checkpoint>> {
        let db = panic::lock(&self.db);
        db.checkpoints(session)
    }
}

/// Lines a diff adds and removes.
fn counts(d: &FileDiff) -> (u64, u64) {
    let mut out = (0u64, 0u64);
    for line in d.hunks.iter().flat_map(|hunk| &hunk.lines) {
        match line.op {
            diff::Op::Add => out.0 += 1,
            diff::Op::Del => out.1 += 1,
            diff::Op::Same => {}
        }
    }
    out
}

/// Classify one file's before/after, and diff it when that is meaningful.
///
/// `path` is filled in by the caller — this half is pure, so every branch of it
/// is testable without a filesystem, a database, or a jail.
fn compare(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    config: &crate::settings::configuration::Configuration,
) -> FileDiff {
    let blank = |change| FileDiff {
        path: String::new(),
        change,
        hunks: Vec::new(),
    };

    let (b, a) = match (before, after) {
        (None, None) => return blank(Change::Unchanged),
        (Some(b), None) => (b, &b""[..]),
        (None, Some(a)) => (&b""[..], a),
        (Some(b), Some(a)) => {
            if b == a {
                return blank(Change::Unchanged);
            }
            (b, a)
        }
    };

    let change = match (before.is_some(), after.is_some()) {
        (false, true) => Change::Created,
        (true, false) => Change::Deleted,
        _ => Change::Modified,
    };

    let (Ok(b), Ok(a)) = (std::str::from_utf8(b), std::str::from_utf8(a)) else {
        return blank(Change::Binary);
    };
    FileDiff {
        path: String::new(),
        change,
        hunks: diff::hunks(
            b,
            a,
            config.number("checkpoint.diff.max_cells") as usize,
            config.number("checkpoint.diff.context") as usize,
        ),
    }
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    crate::platform::file_mode(meta)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    crate::platform::set_file_mode(path, mode)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_classifies_every_before_and_after_pair() {
        assert_eq!(
            compare(
                None,
                None,
                &crate::settings::configuration::Configuration::default()
            )
            .change,
            Change::Unchanged
        );
        assert_eq!(
            compare(
                Some(b"same"),
                Some(b"same"),
                &crate::settings::configuration::Configuration::default()
            )
            .change,
            Change::Unchanged
        );
        assert_eq!(
            compare(
                None,
                Some(b"new\n"),
                &crate::settings::configuration::Configuration::default()
            )
            .change,
            Change::Created
        );
        assert_eq!(
            compare(
                Some(b"gone\n"),
                None,
                &crate::settings::configuration::Configuration::default()
            )
            .change,
            Change::Deleted
        );
        assert_eq!(
            compare(
                Some(b"a\n"),
                Some(b"b\n"),
                &crate::settings::configuration::Configuration::default()
            )
            .change,
            Change::Modified
        );
    }

    #[test]
    fn a_created_file_shows_all_of_its_content_as_added() {
        let d = compare(
            None,
            Some(b"one\ntwo\n"),
            &crate::settings::configuration::Configuration::default(),
        );
        assert_eq!(d.change, Change::Created);
        let texts: Vec<&str> = d.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two"]);
        assert!(d.hunks[0].lines.iter().all(|l| l.op == diff::Op::Add));
    }

    #[test]
    fn a_deleted_file_shows_all_of_its_content_as_removed() {
        let d = compare(
            Some(b"one\ntwo\n"),
            None,
            &crate::settings::configuration::Configuration::default(),
        );
        assert_eq!(d.change, Change::Deleted);
        assert!(d.hunks[0].lines.iter().all(|l| l.op == diff::Op::Del));
    }

    #[test]
    fn bytes_that_are_not_text_are_reported_without_hunks() {
        let d = compare(
            Some(&[0xff, 0xfe, 0x00]),
            Some(&[0x00, 0x01]),
            &crate::settings::configuration::Configuration::default(),
        );
        assert_eq!(d.change, Change::Binary);
        assert!(d.hunks.is_empty(), "there is nothing to show line by line");
    }

    /// A rewrite of every line used to be refused as "too large" and counted
    /// from the write ledger instead; it is diffed like any other change.
    #[test]
    fn a_full_rewrite_is_diffed_line_by_line() {
        let before: Vec<u8> = (0..2000)
            .map(|n| format!("old {n}\n"))
            .collect::<String>()
            .into();
        let after: Vec<u8> = (0..2000)
            .map(|n| format!("new {n}\n"))
            .collect::<String>()
            .into();
        let d = compare(
            Some(&before),
            Some(&after),
            &crate::settings::configuration::Configuration::default(),
        );
        assert_eq!(d.change, Change::Modified);
        let lines = d.hunks.iter().flat_map(|h| &h.lines);
        assert_eq!(
            lines.clone().filter(|l| l.op == diff::Op::Add).count(),
            2000
        );
        assert_eq!(lines.filter(|l| l.op == diff::Op::Del).count(), 2000);
    }

    struct Fixture {
        cp: Checkpoints,
        dir: PathBuf,
        session: i64,
        jail: Jail,
        /// Beside the project, as in an install: no fixture file is a note.
        memory: crate::memory::Memory,
        db: Arc<Mutex<Db>>,
    }

    fn fixture(name: &str) -> Fixture {
        let base = std::env::temp_dir().join("rune-test").join(format!(
            "runecp-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("project");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"original\n").unwrap();

        let db = Arc::new(Mutex::new(Db::memory().unwrap()));
        let session = db
            .lock()
            .unwrap()
            .create_session(dir.to_str().unwrap())
            .unwrap()
            .id;
        let jail = Jail::new(&dir).unwrap();
        let cp = Checkpoints::new(
            Arc::clone(&db),
            base.join("objects"),
            &crate::settings::configuration::Shared::default(),
        );
        Fixture {
            cp,
            dir,
            session,
            jail,
            memory: crate::memory::Memory::new(base.join("memory")),
            db,
        }
    }

    #[test]
    fn missing_first_blob_uses_ledger_tally_without_blocking_resume() {
        let f = fixture("missing-ledger-blob");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        let hash = objects::hash_of(b"original\n");
        std::fs::remove_file(f.cp.objects.root().join(&hash[..2]).join(&hash[2..])).unwrap();
        f.db.lock()
            .unwrap()
            .conn()
            .execute(
                "UPDATE session_files SET added=7,removed=3 WHERE session_id=?1",
                rusqlite::params![f.session],
            )
            .unwrap();
        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!((changes[0].added, changes[0].removed), (7, 3));
        assert_eq!(changes[0].change, Change::Missing);
        let refused =
            f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
                .unwrap_err()
                .to_string();
        assert!(refused.contains("no longer kept"), "{refused}");
    }

    #[test]
    fn existing_file_is_captured_and_restored() {
        let f = fixture("revert");
        let path = f.dir.join("src/main.rs");
        assert!(f.cp.capture(f.session, 1, &path, &f.jail).unwrap());

        std::fs::write(&path, b"agent rewrote this\n").unwrap();
        assert!(f
            .cp
            .restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    #[test]
    fn a_file_the_session_created_is_removed_on_restore() {
        let f = fixture("created");
        let path = f.dir.join("src/new.rs");
        assert!(f.cp.capture(f.session, 1, &path, &f.jail).unwrap());
        std::fs::write(&path, b"brand new\n").unwrap();

        assert!(f
            .cp
            .restore_file(f.session, "src/new.rs", &f.jail, &f.memory, 2)
            .unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn only_the_first_capture_in_a_turn_counts() {
        let f = fixture("first");
        let path = f.dir.join("src/main.rs");
        assert!(f.cp.capture(f.session, 1, &path, &f.jail).unwrap());
        std::fs::write(&path, b"halfway\n").unwrap();
        // A second tool touches the same file in the same turn.
        assert!(!f.cp.capture(f.session, 1, &path, &f.jail).unwrap());
        std::fs::write(&path, b"final\n").unwrap();

        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    /// The owner's rule (2026-10-09): however many turns changed a file, its
    /// undo puts back what it was before the session first touched it — even
    /// once the turns that held that state were pruned.
    #[test]
    fn a_file_goes_back_to_before_the_session_whatever_the_turns_did() {
        let f = fixture("multi");
        let path = f.dir.join("src/main.rs");
        for turn in 1..=4 {
            f.cp.capture(f.session, turn, &path, &f.jail).unwrap();
            std::fs::write(&path, format!("after turn {turn}\n")).unwrap();
        }
        let project = f.dir.to_string_lossy().to_string();
        f.cp.prune(f.session, &project, 1).unwrap();

        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 5)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    /// One file is put back alone: another file of the same turns stays.
    #[test]
    fn one_file_is_restored_alone() {
        let f = fixture("one-file");
        let a = f.dir.join("src/main.rs");
        let b = f.dir.join("src/b.rs");
        f.cp.capture(f.session, 10, &a, &f.jail).unwrap();
        f.cp.capture(f.session, 10, &b, &f.jail).unwrap();
        std::fs::write(&a, b"after turn 10\n").unwrap();
        std::fs::write(&b, b"b from turn 10\n").unwrap();

        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 11)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "original\n");
        assert_eq!(
            std::fs::read_to_string(&b).unwrap(),
            "b from turn 10\n",
            "a file not chosen was touched"
        );
    }

    /// What a restore overwrites is captured under its own turn first, like
    /// any write, and the file's last change moves to that turn.
    #[test]
    fn a_restore_keeps_what_it_overwrote() {
        let f = fixture("redo");
        let a = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &a, &f.jail).unwrap();
        std::fs::write(&a, b"after turn 1\n").unwrap();
        f.db.lock()
            .unwrap()
            .conn()
            .execute(
                "UPDATE session_files SET last_at = 5 WHERE session_id = ?1",
                rusqlite::params![f.session],
            )
            .unwrap();

        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 3)
            .unwrap();
        let kept = f.cp.list(f.session).unwrap();
        let redo = kept
            .iter()
            .find(|c| c.turn == 3)
            .expect("the restore's turn");
        let blob = redo.files[0].blob.as_deref().unwrap();
        assert_eq!(f.cp.objects.get(blob).unwrap(), b"after turn 1\n");
        let row = &f.cp.session_changes(f.session, &f.jail).unwrap()[0];
        assert!(
            row.last_at.unwrap() > 5,
            "the restore is the file's last change"
        );
    }

    /// A file already as it was is not rewritten, captured or reported.
    #[test]
    fn a_tracked_path_that_became_a_directory_is_not_reported_as_a_deletion() {
        let f = fixture("tracked-directory");
        let a = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &a, &f.jail).unwrap();
        std::fs::remove_file(&a).unwrap();
        std::fs::create_dir(&a).unwrap();
        assert!(matches!(
            f.cp.session_changes(f.session, &f.jail),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn a_file_already_as_it_was_is_left_alone() {
        let f = fixture("noop");
        let a = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &a, &f.jail).unwrap();
        assert!(!f
            .cp
            .restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap());
        assert!(f.cp.list(f.session).unwrap().iter().all(|c| c.turn != 2));
    }

    /// The diff is what the restore undoes: every later change of the file
    /// together, against the file as it is now.
    #[test]
    fn the_file_diff_shows_every_later_change_together() {
        let f = fixture("preview");
        let a = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &a, &f.jail).unwrap();
        std::fs::write(&a, b"original\nfrom turn 1\n").unwrap();
        f.cp.capture(f.session, 2, &a, &f.jail).unwrap();
        std::fs::write(&a, b"original\nfrom turn 1\nfrom turn 2\n").unwrap();
        let diff = f.cp.file_diff(f.session, "src/main.rs", &f.jail).unwrap();
        assert_eq!(diff.path, "src/main.rs");
        let added: Vec<&str> = diff.hunks[0]
            .lines
            .iter()
            .filter(|l| l.op == diff::Op::Add)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(added, vec!["from turn 1", "from turn 2"]);
    }

    #[test]
    fn a_file_the_session_never_changed_is_not_found() {
        let f = fixture("unknown");
        assert!(matches!(
            f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            f.cp.file_diff(f.session, "src/main.rs", &f.jail),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn file_permissions_survive_a_restore() {
        // What a mode says on each OS: the execute bit on Unix; on Windows
        // only whether the file is read-only.
        let (kept, changed) = match crate::platform::OS {
            crate::platform::Os::Windows => (0o444, 0o666),
            _ => (0o755, 0o644),
        };
        let f = fixture("mode");
        let script = f.dir.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        crate::platform::set_file_mode(&script, kept).unwrap();

        f.cp.capture(f.session, 1, &script, &f.jail).unwrap();
        crate::platform::set_file_mode(&script, changed).unwrap();
        std::fs::write(&script, b"clobbered\n").unwrap();

        f.cp.restore_file(f.session, "run.sh", &f.jail, &f.memory, 2)
            .unwrap();
        let mode = crate::platform::file_mode(&std::fs::metadata(&script).unwrap());
        assert_eq!(mode & 0o777, kept);

        // Deleted, then put back: the mode comes from the ledger.
        crate::platform::set_file_mode(&script, changed).unwrap();
        std::fs::remove_file(&script).unwrap();
        f.cp.restore_file(f.session, "run.sh", &f.jail, &f.memory, 3)
            .unwrap();
        let mode = crate::platform::file_mode(&std::fs::metadata(&script).unwrap());
        assert_eq!(mode & 0o777, kept);
    }

    #[test]
    fn pruning_bounds_the_history_and_frees_the_blobs_behind_it() {
        let f = fixture("prune");
        let path = f.dir.join("src/main.rs");
        for turn in 1..=6 {
            f.cp.capture(f.session, turn, &path, &f.jail).unwrap();
            std::fs::write(&path, format!("version {turn}\n")).unwrap();
        }

        let project = f.dir.to_string_lossy().to_string();
        let pruned = f.cp.prune(f.session, &project, 2).unwrap();

        assert_eq!(pruned.manifests, 4);
        assert!(pruned.blobs >= 1, "orphaned content should have been freed");
        assert_eq!(f.cp.list(f.session).unwrap().len(), 2);

        // Undo must still work — pruning that breaks it is worse than a full
        // disk: the ledger keeps the state from before the session.
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 7)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    #[test]
    fn collect_frees_everything_once_the_sessions_are_gone() {
        let f = fixture("collect");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        let project = f.dir.to_string_lossy().to_string();

        // Simulate the session having been deleted.
        {
            let db = f.db.lock().unwrap();
            db.delete_session(f.session).unwrap();
        }
        let swept = f.cp.collect(&project).unwrap();
        assert_eq!(swept.blobs, 1);
    }

    #[test]
    fn checkpoints_are_listed_newest_first() {
        let f = fixture("list");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        f.cp.capture(f.session, 2, &path, &f.jail).unwrap();
        let list = f.cp.list(f.session).unwrap();
        assert_eq!(list.iter().map(|c| c.turn).collect::<Vec<_>>(), vec![2, 1]);
    }

    // ─── Session-wide file changes (the side panel's FILE CHANGE) ───────────────

    #[test]
    fn a_file_edited_many_times_is_one_row_with_the_session_net() {
        let f = fixture("aggregate");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "one\n").unwrap();
        f.cp.capture(f.session, 2, &path, &f.jail).unwrap();
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes.len(), 1, "one path, however many turns touched it");
        assert_eq!(changes[0].change, Change::Modified);
        // Against the pre-session state ("original"): one line gone, three in.
        assert_eq!((changes[0].added, changes[0].removed), (3, 1));
    }

    #[test]
    fn a_created_file_counts_every_line_as_added_and_nothing_as_removed() {
        let f = fixture("created-tally");
        let path = f.dir.join("src/new.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "alpha\nbeta\n").unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, Change::Created);
        assert_eq!((changes[0].added, changes[0].removed), (2, 0));
    }

    #[test]
    fn a_deleted_file_counts_every_line_as_removed() {
        let f = fixture("deleted");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::remove_file(&path).unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes[0].change, Change::Deleted);
        assert_eq!((changes[0].added, changes[0].removed), (0, 1));
    }

    #[test]
    fn a_restored_file_reports_unchanged_with_nothing_to_count() {
        let f = fixture("reverted");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "agent rewrote this\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes[0].change, Change::Unchanged);
        assert_eq!((changes[0].added, changes[0].removed), (0, 0));
    }

    /// A rewrite wider than Myers' budget used to be refused and counted from
    /// the write ledger, which sums every write rather than netting them. It
    /// is counted from a real diff now, whatever the ledger says.
    #[test]
    fn a_wide_rewrite_counts_from_the_diff_not_the_ledger() {
        let f = fixture("wide-rewrite");
        let path = f.dir.join("src/big.rs");
        let before: String = (0..2000).map(|i| format!("before line {i}\n")).collect();
        std::fs::write(&path, &before).unwrap();
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        f.db.lock()
            .unwrap()
            .record_session_file_change(f.session, "src/big.rs", 9000, 9000, 2)
            .unwrap();
        let after: String = (0..2000).map(|i| format!("after line {i}\n")).collect();
        std::fs::write(&path, &after).unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        let row = changes
            .iter()
            .find(|c| c.path == "src/big.rs")
            .expect("the rewritten file has its row");
        assert_eq!(row.change, Change::Modified);
        assert_eq!((row.added, row.removed), (2000, 2000));
    }

    #[test]
    fn rows_arrive_in_first_touch_order() {
        let f = fixture("order");
        let first = f.dir.join("src/main.rs");
        let second = f.dir.join("src/extra.rs");
        f.cp.capture(f.session, 1, &first, &f.jail).unwrap();
        std::fs::write(&first, "changed\n").unwrap();
        f.cp.capture(f.session, 2, &second, &f.jail).unwrap();
        std::fs::write(&second, "added\n").unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["src/main.rs", "src/extra.rs"]);
    }

    #[test]
    fn pruning_the_history_never_loses_the_first_touch() {
        let f = fixture("prune-survives");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        for turn in 2..=4 {
            f.cp.capture(f.session, turn, &path, &f.jail).unwrap();
            std::fs::write(&path, format!("version {turn}\n")).unwrap();
        }
        // The pre-session manifest is long gone; its blob survives because the
        // session ledger still names it.
        f.cp.prune(f.session, &f.dir.to_string_lossy(), 1).unwrap();
        assert_eq!(f.cp.list(f.session).unwrap().len(), 1);

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes[0].change, Change::Modified);
        assert_eq!(
            (changes[0].added, changes[0].removed),
            (1, 1),
            "the total must stay true after pruning"
        );
    }

    #[test]
    fn a_session_file_outside_the_project_is_still_tallied() {
        let f = fixture("outside-in");
        // Full-access mode: the jail the session hands out permits everything,
        // so a capture — and the tally — must follow the file wherever it is.
        // One directory over from the project, inside the fixture's own scratch.
        let jail = f.jail.unconfined();
        let outside = f
            .dir
            .parent()
            .unwrap()
            .join("elsewhere")
            .join("elsewhere.txt");

        // Captured while it does not exist: the session's whole content is one
        // plus, wherever the file happens to live.
        f.cp.capture(f.session, 1, &outside, &jail).unwrap();
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "created here\nand edited\n").unwrap();

        let changes = f.cp.session_changes(f.session, &jail).unwrap();
        assert_eq!(
            changes.len(),
            1,
            "an outside file is one row like any other"
        );
        // The path the jail displays cannot be made relative to this project,
        // so it arrives in the one honest form it has.
        assert_eq!(changes[0].path, outside.to_string_lossy());
        assert_eq!(changes[0].change, Change::Created);
        assert_eq!((changes[0].added, changes[0].removed), (2, 0));
    }

    #[test]
    fn bytes_that_cannot_be_counted_report_the_change_without_counts() {
        let f = fixture("binary");
        let path = f.dir.join("src/blob.bin");
        std::fs::write(&path, [0xffu8, 0xfe, 0x01]).unwrap();
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, [0x00u8, 0x01, 0x02, 0x03]).unwrap();

        let changes = f.cp.session_changes(f.session, &f.jail).unwrap();
        assert_eq!(changes[0].change, Change::Binary);
        assert_eq!((changes[0].added, changes[0].removed), (0, 0));
    }

    #[test]
    fn a_session_with_no_touches_tallies_nothing() {
        let f = fixture("empty");
        assert!(f.cp.session_changes(f.session, &f.jail).unwrap().is_empty());
    }

    #[test]
    fn another_sessions_tally_never_leaks_into_this_one() {
        let f = fixture("isolation");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "changed\n").unwrap();
        let other =
            f.db.lock()
                .unwrap()
                .create_session(f.dir.to_str().unwrap())
                .unwrap()
                .id;

        assert!(f.cp.session_changes(other, &f.jail).unwrap().is_empty());
        assert_eq!(f.cp.session_changes(f.session, &f.jail).unwrap().len(), 1);
    }

    // ─── Redo ───────────────────────────────────────────────────────────────────

    /// The owner's model (2026-10-09): A is the file before the undo, B after
    /// it. Nothing to redo before an undo; redo puts A back and is used up.
    #[test]
    fn redo_puts_back_what_the_undo_replaced_and_is_used_up() {
        let f = fixture("redo-basic");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "A\n").unwrap();
        assert!(
            f.cp.redo_changes(f.session, &f.jail).unwrap().is_empty(),
            "no undo, no redo"
        );
        assert!(matches!(
            f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 2),
            Err(Error::NotFound(_))
        ));

        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        let redos = f.cp.redo_changes(f.session, &f.jail).unwrap();
        assert_eq!(redos.len(), 1);
        assert_eq!((redos[0].added, redos[0].removed), (1, 1));
        assert!(redos[0].last_at.is_some(), "when it was undone");

        assert!(f
            .cp
            .redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 3)
            .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\n");
        assert!(
            f.cp.redo_changes(f.session, &f.jail).unwrap().is_empty(),
            "used up"
        );
        // And the file can be undone again.
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 4)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    }

    /// B edited after the undo: the redo stays, its diff is measured from the
    /// file as it is now, and it still puts A back (owner's choice).
    #[test]
    fn a_redo_survives_a_later_edit_and_still_puts_back_what_was_undone() {
        let f = fixture("redo-edited");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "A\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        std::fs::write(&path, "original\nedited after the undo\n").unwrap();

        let d = f.cp.redo_diff(f.session, "src/main.rs", &f.jail).unwrap();
        let removed: Vec<&str> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.op == diff::Op::Del)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(removed, ["original", "edited after the undo"]);
        f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 3)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\n");
        // What the redo overwrote was captured like any write.
        let kept = f.cp.list(f.session).unwrap();
        let blob = kept.iter().find(|c| c.turn == 3).unwrap().files[0]
            .blob
            .clone()
            .unwrap();
        assert_eq!(
            f.cp.objects.get(&blob).unwrap(),
            b"original\nedited after the undo\n"
        );
    }

    #[test]
    fn a_second_undo_keeps_the_latest_file_for_redo() {
        let f = fixture("redo-latest");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "first\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        f.cp.capture(f.session, 3, &path, &f.jail).unwrap();
        std::fs::write(&path, "second\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 4)
            .unwrap();
        f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 5)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
    }

    /// The redo outlives the pruning of the turn that captured it, and a file
    /// the agent had deleted is deleted again.
    #[test]
    fn a_redo_survives_pruning_and_can_mean_no_file() {
        let f = fixture("redo-pruned");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::remove_file(&path).unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        assert!(path.exists());
        let other = f.dir.join("src/other.rs");
        for turn in 3..=8 {
            f.cp.capture(f.session, turn, &other, &f.jail).unwrap();
            std::fs::write(&other, format!("{turn}\n")).unwrap();
        }
        let project = f.dir.to_string_lossy().to_string();
        f.cp.prune(f.session, &project, 1).unwrap();
        f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 9)
            .unwrap();
        assert!(!path.exists(), "the redo deleted it again");

        // A redo whose content is kept past pruning.
        std::fs::write(&path, "back\n").unwrap();
        f.cp.capture(f.session, 10, &path, &f.jail).unwrap();
        std::fs::write(&path, "kept for redo\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 11)
            .unwrap();
        for turn in 12..=18 {
            f.cp.capture(f.session, turn, &other, &f.jail).unwrap();
            std::fs::write(&other, format!("{turn}\n")).unwrap();
        }
        f.cp.prune(f.session, &project, 1).unwrap();
        f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 19)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept for redo\n");
    }

    /// What the undo replaced was removed from `objects/` by hand: the redo
    /// is listed as missing and refused with the reason, never a wrong file.
    #[test]
    fn a_redo_whose_content_is_gone_is_refused_with_the_reason() {
        let f = fixture("redo-missing");
        let path = f.dir.join("src/main.rs");
        f.cp.capture(f.session, 1, &path, &f.jail).unwrap();
        std::fs::write(&path, "kept for redo\n").unwrap();
        f.cp.restore_file(f.session, "src/main.rs", &f.jail, &f.memory, 2)
            .unwrap();
        let hash = objects::hash_of(b"kept for redo\n");
        std::fs::remove_file(f.cp.objects.root().join(&hash[..2]).join(&hash[2..])).unwrap();

        let redos = f.cp.redo_changes(f.session, &f.jail).unwrap();
        assert_eq!(redos[0].change, Change::Missing);
        let refused =
            f.cp.redo_file(f.session, "src/main.rs", &f.jail, &f.memory, 3)
                .unwrap_err()
                .to_string();
        assert!(refused.contains("no longer kept"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original\n",
            "untouched"
        );
        assert!(f.cp.redo_diff(f.session, "src/main.rs", &f.jail).is_err());
    }

    /// A rewrite of a large file is diffed within a bounded time, and the
    /// script still rebuilds it.
    #[test]
    fn a_large_rewrite_is_diffed_in_bounded_time() {
        let before: String = (0..60_000).map(|i| format!("before {i}\n")).collect();
        let after: String = (0..60_000)
            .map(|i| {
                if i % 3 == 0 {
                    format!("before {i}\n")
                } else {
                    format!("after {i}\n")
                }
            })
            .collect();
        let started = std::time::Instant::now();
        let d = compare(
            Some(before.as_bytes()),
            Some(after.as_bytes()),
            &crate::settings::configuration::Configuration::default(),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let (added, removed) = counts(&d);
        assert_eq!((added, removed), (40_000, 40_000));
    }
}
