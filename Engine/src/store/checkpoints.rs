//! Checkpoint manifests.
//!
//! A manifest is the list of files a turn was about to change, with the content
//! hash each had *before* the change (or `None` when the file did not exist).
//! The blobs themselves are not in the database — they are content-addressed
//! files in the project folder's `objects/` (`~/.rune/projects/<name>/`, see
//! `crate::checkpoint::objects`), so a large snapshot never bloats the database
//! and no command run inside the project can delete the undo history.

use std::collections::HashSet;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Db};
use crate::error::Result;

/// One file's pre-turn state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    /// Path relative to the project root.
    pub path: String,
    /// Content hash of the blob holding the previous content. `None` means the
    /// file did not exist and restoring means deleting it.
    pub blob: Option<String>,
    /// Unix permission bits, when the file existed.
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Checkpoint {
    pub id: i64,
    pub turn: i64,
    pub created_at: i64,
    pub files: Vec<Snapshot>,
}

impl Db {
    /// Create the (empty) checkpoint for a turn, or return the existing one.
    /// A turn has exactly one checkpoint however many files it later touches.
    pub fn open_checkpoint(&self, session: i64, turn: i64) -> Result<i64> {
        if let Some(id) = self.checkpoint_id(session, turn)? {
            return Ok(id);
        }
        self.conn().execute(
            "INSERT INTO checkpoints (session_id, turn, created_at, manifest)
             VALUES (?1, ?2, ?3, '[]')",
            params![session, turn, now_ms()],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn checkpoint_id(&self, session: i64, turn: i64) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id FROM checkpoints WHERE session_id = ?1 AND turn = ?2",
                params![session, turn],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Add one file's pre-state. The first record for a path wins: later writes
    /// in the same turn must not overwrite the original state.
    pub fn record_snapshot(&self, session: i64, turn: i64, snap: Snapshot) -> Result<bool> {
        self.open_checkpoint(session, turn)?;
        let mut files = self
            .checkpoint(session, turn)?
            .map(|c| c.files)
            .unwrap_or_default();
        if files.iter().any(|f| f.path == snap.path) {
            return Ok(false);
        }
        files.push(snap);
        let json = serde_json::to_string(&files)?;
        self.conn().execute(
            "UPDATE checkpoints SET manifest = ?3 WHERE session_id = ?1 AND turn = ?2",
            params![session, turn, json],
        )?;
        Ok(true)
    }

    pub fn checkpoint(&self, session: i64, turn: i64) -> Result<Option<Checkpoint>> {
        let row: Option<(i64, i64, String)> = self
            .conn()
            .query_row(
                "SELECT id, created_at, manifest FROM checkpoints
                 WHERE session_id = ?1 AND turn = ?2",
                params![session, turn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((id, created_at, manifest)) => Ok(Some(Checkpoint {
                id,
                turn,
                created_at,
                files: serde_json::from_str(&manifest)?,
            })),
        }
    }

    /// Newest first.
    pub fn checkpoints(&self, session: i64) -> Result<Vec<Checkpoint>> {
        let mut st = self.conn().prepare(
            "SELECT id, turn, created_at, manifest FROM checkpoints
             WHERE session_id = ?1 ORDER BY turn DESC",
        )?;
        let rows = st.query_map(params![session], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, turn, created_at, manifest) = row?;
            out.push(Checkpoint {
                id,
                turn,
                created_at,
                files: serde_json::from_str(&manifest)?,
            });
        }
        Ok(out)
    }
}

/// One file a session has touched, with the state it had **before the session
/// first reached it** — the row [`Checkpoints::capture`] keeps alive precisely
/// because `checkpoints` prunes its oldest manifests away. It is what the side
/// panel's FILE CHANGE measures against and what undo puts back. Ordered by
/// first-touch; `last_turn` lives in the ledger for the stamp query.
#[derive(Debug, Clone, Serialize)]
pub struct SessionFile {
    /// Path as the jail displays it — project-relative, `@memory/...` under the
    /// alias, or absolute for a file outside the project.
    pub path: String,
    /// Content hash of the pre-first-touch blob. `None`: the file did not exist
    /// before the session, so the session's whole content is one big plus.
    pub first_blob: Option<String>,
    /// Unix permission bits the file had before the first touch, when it
    /// existed and the ledger knew them (rows from before schema 21 may not).
    pub first_mode: Option<u32>,
    /// When the session last changed the file (milliseconds since the epoch);
    /// `None` for a row older than schema 21 whose turn left no checkpoint.
    pub last_at: Option<i64>,
    pub added: u64,
    pub removed: u64,
    pub change: String,
    /// What the last undo of the file overwrote — what redo puts back —
    /// while `redo_at` says there is one: its blob (`None`: there was no
    /// file), its mode, and when the undo was.
    pub redo_blob: Option<String>,
    pub redo_mode: Option<u32>,
    pub redo_at: Option<i64>,
}

/// The columns [`session_file_of`] reads, in its order.
const SESSION_FILE: &str =
    "path, first_blob, first_mode, last_at, added, removed, change, redo_blob, redo_mode, redo_at";

fn session_file_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionFile> {
    Ok(SessionFile {
        path: r.get(0)?,
        first_blob: r.get(1)?,
        first_mode: r.get(2)?,
        last_at: r.get(3)?,
        added: r.get(4)?,
        removed: r.get(5)?,
        change: r.get(6)?,
        redo_blob: r.get(7)?,
        redo_mode: r.get(8)?,
        redo_at: r.get(9)?,
    })
}

impl Db {
    /// Record the session's touch of `path`, holding the pre-touch state.
    ///
    /// The first write wins and is never rewritten — that is the state FILE
    /// CHANGE measures against and undo puts back; a later touch of the same
    /// path only moves `last_turn`. `last_at` is set by the first touch and
    /// moved by every write after it ([`Db::record_session_file_change`],
    /// [`Db::touch_session_file`]), not by a capture that may not be followed
    /// by one.
    pub fn record_session_file(
        &self,
        session: i64,
        path: &str,
        blob: Option<&str>,
        mode: Option<u32>,
        turn: i64,
    ) -> Result<()> {
        let change = if blob.is_some() {
            "modified"
        } else {
            "created"
        };
        self.conn().execute(
            "INSERT INTO session_files (session_id, path, first_blob, first_mode, first_turn, last_turn, change, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)
             ON CONFLICT(session_id, path) DO UPDATE SET
                last_turn = excluded.last_turn,
                last_at = excluded.last_at",
            params![session, path, blob, mode, turn, change, now_ms()],
        )?;
        Ok(())
    }

    /// The session changed `path` at `turn` outside a tool's write — a
    /// restore: its turn and time move, its tallies do not.
    pub fn touch_session_file(&self, session: i64, path: &str, turn: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE session_files SET last_turn = ?3, last_at = ?4
             WHERE session_id = ?1 AND path = ?2",
            params![session, path, turn, now_ms()],
        )?;
        Ok(())
    }

    /// An undo of `path` overwrote `blob` (`None`: no file) with `mode`:
    /// what redo puts back, replacing what an earlier undo left.
    pub fn set_redo(
        &self,
        session: i64,
        path: &str,
        blob: Option<&str>,
        mode: Option<u32>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE session_files SET redo_blob = ?3, redo_mode = ?4, redo_at = ?5
             WHERE session_id = ?1 AND path = ?2",
            params![session, path, blob, mode, now_ms()],
        )?;
        Ok(())
    }

    /// The redo of `path` was used: there is none until the next undo.
    pub fn clear_redo(&self, session: i64, path: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE session_files SET redo_blob = NULL, redo_mode = NULL, redo_at = NULL
             WHERE session_id = ?1 AND path = ?2",
            params![session, path],
        )?;
        Ok(())
    }

    /// One file of the session's ledger, by the path the jail displays.
    pub fn session_file(&self, session: i64, path: &str) -> Result<Option<SessionFile>> {
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT {SESSION_FILE} FROM session_files WHERE session_id = ?1 AND path = ?2"
                ),
                params![session, path],
                session_file_of,
            )
            .optional()?)
    }

    /// Update the session's cumulative line delta for `path` using the running balance.
    pub fn record_session_file_change(
        &self,
        session: i64,
        path: &str,
        delta_add: u64,
        delta_rem: u64,
        turn: i64,
    ) -> Result<()> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT first_blob, added, removed, change FROM session_files
             WHERE session_id = ?1 AND path = ?2",
        )?;
        let row = st.query_row(params![session, path], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, String>(3)?,
            ))
        });

        let (first_blob, mut added, mut removed, mut change_str) = match row {
            Ok(tuple) => tuple,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let change = if delta_rem == 0 {
                    "created"
                } else {
                    "modified"
                };
                (None, 0, 0, change.to_string())
            }
            Err(e) => return Err(e.into()),
        };

        let is_created = first_blob.is_none() && change_str == "created";
        if is_created {
            added = added.saturating_sub(delta_rem) + delta_add;
            removed = 0;
            change_str = "created".into();
        } else if added == 0 && removed == 0 && change_str != "unchanged" {
            added = delta_add;
            removed = delta_rem;
            change_str = "modified".into();
        } else {
            if delta_rem <= added {
                added -= delta_rem;
            } else {
                removed += delta_rem - added;
                added = 0;
            }
            if delta_add <= removed {
                removed -= delta_add;
            } else {
                added += delta_add - removed;
                removed = 0;
            }
            change_str = if added == 0 && removed == 0 {
                "unchanged".into()
            } else {
                "modified".into()
            };
        }

        conn.execute(
            "INSERT INTO session_files (session_id, path, first_blob, first_turn, last_turn, added, removed, change, last_at)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id, path) DO UPDATE SET
                last_turn = excluded.last_turn,
                added = excluded.added,
                removed = excluded.removed,
                change = excluded.change,
                last_at = excluded.last_at",
            params![session, path, first_blob, turn, added, removed, change_str, now_ms()],
        )?;
        Ok(())
    }

    /// Every file the session has ever touched, in first-touch order.
    pub fn session_files(&self, session: i64) -> Result<Vec<SessionFile>> {
        let mut st = self.conn().prepare(&format!(
            "SELECT {SESSION_FILE} FROM session_files
             WHERE session_id = ?1
             ORDER BY first_turn, rowid"
        ))?;
        let rows = st.query_map(params![session], session_file_of)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every blob still named by any manifest of this project.
    ///
    /// Project-wide rather than per-session on purpose: two sessions in the same
    /// project share one object store, and deleting one session must not take
    /// content the other still needs to undo with.
    pub fn referenced_blobs(&self, project: &str) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        {
            let mut st = self.conn().prepare(
                "SELECT c.manifest FROM checkpoints c
                 JOIN sessions s ON s.id = c.session_id
                 WHERE s.project = ?1",
            )?;
            let rows = st.query_map(params![project], |r| r.get::<_, String>(0))?;
            for manifest in rows {
                let files: Vec<Snapshot> = serde_json::from_str(&manifest?)?;
                for f in files {
                    if let Some(b) = f.blob {
                        out.insert(b);
                    }
                }
            }
        }
        // A session file's pre-first-touch blob is referenced by `session_files`
        // alone once its manifest is pruned — without this the sweep would eat
        // exactly the state FILE CHANGE measures against.
        let mut st = self.conn().prepare(
            "SELECT s.first_blob FROM session_files s
             JOIN sessions ON sessions.id = s.session_id
             WHERE sessions.project = ?1 AND s.first_blob IS NOT NULL",
        )?;
        let rows = st.query_map(params![project], |r| r.get::<_, String>(0))?;
        for blob in rows {
            out.insert(blob?);
        }
        drop(st);
        // What a redo puts back lives as long as the redo: past the manifest
        // that captured it.
        let mut st = self.conn().prepare(
            "SELECT s.redo_blob FROM session_files s
             JOIN sessions ON sessions.id = s.session_id
             WHERE sessions.project = ?1 AND s.redo_blob IS NOT NULL",
        )?;
        let rows = st.query_map(params![project], |r| r.get::<_, String>(0))?;
        for blob in rows {
            out.insert(blob?);
        }
        Ok(out)
    }

    /// Drop all but the `keep` newest checkpoints of a session. Returns how many
    /// manifests were removed.
    pub fn prune_checkpoints(&self, session: i64, keep: usize) -> Result<usize> {
        let n = self.conn().execute(
            "DELETE FROM checkpoints WHERE session_id = ?1 AND turn NOT IN (
                 SELECT turn FROM checkpoints WHERE session_id = ?1
                 ORDER BY turn DESC LIMIT ?2
             )",
            params![session, keep as i64],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(path: &str, blob: Option<&str>) -> Snapshot {
        Snapshot {
            path: path.into(),
            blob: blob.map(|b| b.into()),
            mode: Some(0o644),
        }
    }

    #[test]
    fn one_checkpoint_per_turn() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        let a = db.open_checkpoint(s.id, 1).unwrap();
        let b = db.open_checkpoint(s.id, 1).unwrap();
        assert_eq!(a, b);
        assert_eq!(db.checkpoints(s.id).unwrap().len(), 1);
    }

    #[test]
    fn first_snapshot_of_a_path_wins() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        assert!(db
            .record_snapshot(s.id, 1, snap("a.rs", Some("hash1")))
            .unwrap());
        // Second write in the same turn must not clobber the original state.
        assert!(!db
            .record_snapshot(s.id, 1, snap("a.rs", Some("hash2")))
            .unwrap());
        let c = db.checkpoint(s.id, 1).unwrap().unwrap();
        assert_eq!(c.files.len(), 1);
        assert_eq!(c.files[0].blob.as_deref(), Some("hash1"));
    }

    #[test]
    fn absent_file_is_recorded_as_none() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_snapshot(s.id, 2, snap("new.rs", None)).unwrap();
        let c = db.checkpoint(s.id, 2).unwrap().unwrap();
        assert_eq!(c.files[0].blob, None);
    }

    #[test]
    fn referenced_blobs_span_every_session_of_the_project() {
        let db = Db::memory().unwrap();
        let a = db.create_session("/p").unwrap();
        let b = db.create_session("/p").unwrap();
        let other = db.create_session("/elsewhere").unwrap();
        db.record_snapshot(a.id, 1, snap("a.rs", Some("hash-a")))
            .unwrap();
        db.record_snapshot(b.id, 1, snap("b.rs", Some("hash-b")))
            .unwrap();
        db.record_snapshot(other.id, 1, snap("c.rs", Some("hash-c")))
            .unwrap();

        let refs = db.referenced_blobs("/p").unwrap();
        assert!(refs.contains("hash-a") && refs.contains("hash-b"));
        assert!(
            !refs.contains("hash-c"),
            "another project's blobs are not ours to keep"
        );
    }

    #[test]
    fn the_first_touch_of_a_path_is_the_one_the_ledger_keeps() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_session_file(s.id, "a.rs", Some("first"), None, 1)
            .unwrap();
        db.record_session_file(s.id, "a.rs", Some("later"), None, 4)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        assert_eq!(files.len(), 1, "one row per path, however many touches");
        assert_eq!(
            files[0].first_blob.as_deref(),
            Some("first"),
            "the first touch holds the pre-session state"
        );
        // That the later touch moved the ledger's notion of "last touched" is
        // the stamp query's business — see `the_ledger_counts_as_the_change_detector`.
    }

    #[test]
    fn the_ledger_walks_in_first_touch_order() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_session_file(s.id, "b.rs", None, None, 1).unwrap();
        db.record_session_file(s.id, "a.rs", Some("h"), None, 2)
            .unwrap();
        db.record_session_file(s.id, "b.rs", None, None, 3).unwrap();
        let files = db.session_files(s.id).unwrap();
        let order: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(order, vec!["b.rs", "a.rs"]);
    }

    #[test]
    fn ledger_blobs_are_referenced_so_the_sweep_spares_them() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_session_file(s.id, "a.rs", Some("ledger-only"), None, 1)
            .unwrap();
        assert!(db.referenced_blobs("/p").unwrap().contains("ledger-only"));
        // …and not for another project's ledger.
        let other = db.create_session("/elsewhere").unwrap();
        db.record_session_file(other.id, "b.rs", Some("theirs"), None, 1)
            .unwrap();
        assert!(!db.referenced_blobs("/p").unwrap().contains("theirs"));
    }

    #[test]
    fn a_deleted_session_drops_its_ledger_and_its_blobs() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_session_file(s.id, "a.rs", Some("mine"), None, 1)
            .unwrap();
        db.delete_session(s.id).unwrap();
        assert!(!db.referenced_blobs("/p").unwrap().contains("mine"));
    }

    #[test]
    fn pruning_keeps_the_newest_turns() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        for turn in 1..=5 {
            db.record_snapshot(s.id, turn, snap(&format!("f{turn}.rs"), Some("h")))
                .unwrap();
        }
        assert_eq!(db.prune_checkpoints(s.id, 2).unwrap(), 3);
        let left: Vec<i64> = db
            .checkpoints(s.id)
            .unwrap()
            .iter()
            .map(|c| c.turn)
            .collect();
        assert_eq!(left, vec![5, 4]);
    }

    #[test]
    fn pruning_below_the_limit_removes_nothing() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.record_snapshot(s.id, 1, snap("a.rs", Some("h")))
            .unwrap();
        assert_eq!(db.prune_checkpoints(s.id, 10).unwrap(), 0);
    }

    #[test]
    fn checkpoints_list_newest_first() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        db.open_checkpoint(s.id, 1).unwrap();
        db.open_checkpoint(s.id, 2).unwrap();
        let list = db.checkpoints(s.id).unwrap();
        assert_eq!(list.iter().map(|c| c.turn).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn a_turn_with_no_checkpoint_reports_none() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();
        assert!(db.checkpoint(s.id, 9).unwrap().is_none());
    }

    #[test]
    fn running_balance_tallies_deltas_correctly_across_patches() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/p").unwrap();

        // 1. User's exact scenario: patch 1 (+9, -5), patch 2 (+4, -3) -> +6, -1
        db.record_session_file(s.id, "calc.rs", Some("orig_hash"), None, 1)
            .unwrap();
        db.record_session_file_change(s.id, "calc.rs", 9, 5, 1)
            .unwrap();
        db.record_session_file_change(s.id, "calc.rs", 4, 3, 2)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        assert_eq!((files[0].added, files[0].removed), (6, 1));
        assert_eq!(files[0].change, "modified");

        // 2. Scenario -5 then +10 -> +5 -0
        db.record_session_file(s.id, "second.rs", Some("orig"), None, 1)
            .unwrap();
        db.record_session_file_change(s.id, "second.rs", 0, 5, 1)
            .unwrap();
        db.record_session_file_change(s.id, "second.rs", 10, 0, 2)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        let second = files.iter().find(|f| f.path == "second.rs").unwrap();
        assert_eq!((second.added, second.removed), (5, 0));

        // 3. Scenario +5 then -10 -> +0 -5
        db.record_session_file(s.id, "third.rs", Some("orig"), None, 1)
            .unwrap();
        db.record_session_file_change(s.id, "third.rs", 5, 0, 1)
            .unwrap();
        db.record_session_file_change(s.id, "third.rs", 0, 10, 2)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        let third = files.iter().find(|f| f.path == "third.rs").unwrap();
        assert_eq!((third.added, third.removed), (0, 5));

        // 4. Scenario revert back to original: +10 -10 then +10 -10 opposite -> +0 -0 unchanged
        db.record_session_file(s.id, "revert.rs", Some("orig"), None, 1)
            .unwrap();
        db.record_session_file_change(s.id, "revert.rs", 10, 10, 1)
            .unwrap();
        db.record_session_file_change(s.id, "revert.rs", 10, 10, 2)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        let rev = files.iter().find(|f| f.path == "revert.rs").unwrap();
        assert_eq!((rev.added, rev.removed), (0, 0));
        assert_eq!(rev.change, "unchanged");

        // 5. Scenario created file: +50 then -10 +2 -> +42 -0 created
        db.record_session_file(s.id, "new.rs", None, None, 1)
            .unwrap();
        db.record_session_file_change(s.id, "new.rs", 50, 0, 1)
            .unwrap();
        db.record_session_file_change(s.id, "new.rs", 2, 10, 2)
            .unwrap();
        let files = db.session_files(s.id).unwrap();
        let new_file = files.iter().find(|f| f.path == "new.rs").unwrap();
        assert_eq!((new_file.added, new_file.removed), (42, 0));
        assert_eq!(new_file.change, "created");
    }
}
