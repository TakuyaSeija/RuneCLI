//! The persisted allow-list.
//!
//! Two rules this file exists to enforce structurally rather than by discipline:
//!
//!  1. **Never global.** Every row carries the project it was granted in, and
//!     there is no lookup that ignores it. An "allow always" in one project
//!     grants nothing anywhere else.
//!  2. **Storage only.** Whether something *may* be remembered at all is the
//!     policy's call, in `perm::policy`. This module does not decide.

use rusqlite::params;

use super::{now_ms, Db};
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Grant {
    pub id: i64,
    pub tool: String,
    pub pattern: String,
    pub created_at: i64,
}

impl Db {
    /// Remember a decision for this project. Repeating it is not an error.
    pub fn grant(&self, project: &str, tool: &str, pattern: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO permissions (project, tool, pattern, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![project, tool, pattern, now_ms()],
        )?;
        Ok(())
    }

    pub fn is_granted(&self, project: &str, tool: &str, pattern: &str) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM permissions WHERE project = ?1 AND tool = ?2 AND pattern = ?3",
            params![project, tool, pattern],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn grants(&self, project: &str) -> Result<Vec<Grant>> {
        let mut st = self.conn().prepare(
            "SELECT id, tool, pattern, created_at FROM permissions
             WHERE project = ?1 ORDER BY tool, pattern",
        )?;
        let rows = st.query_map(params![project], |r| {
            Ok(Grant {
                id: r.get(0)?,
                tool: r.get(1)?,
                pattern: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for g in rows {
            out.push(g?);
        }
        Ok(out)
    }

    /// Drop one grant. Returns whether a row was actually removed.
    pub fn revoke(&self, project: &str, tool: &str, pattern: &str) -> Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM permissions WHERE project = ?1 AND tool = ?2 AND pattern = ?3",
            params![project, tool, pattern],
        )?;
        Ok(n > 0)
    }

    /// Drop every grant of one tool in a project.
    pub fn revoke_tool(&self, project: &str, tool: &str) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM permissions WHERE project = ?1 AND tool = ?2",
            params![project, tool],
        )?)
    }

    /// Drop every grant for a project.
    pub fn revoke_all(&self, project: &str) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM permissions WHERE project = ?1",
            params![project],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_is_scoped_to_its_project() {
        let db = Db::memory().unwrap();
        db.grant("/a", "terminal_use", "cargo test").unwrap();
        assert!(db.is_granted("/a", "terminal_use", "cargo test").unwrap());
        // Same tool, same pattern, different project — must not carry over.
        assert!(!db.is_granted("/b", "terminal_use", "cargo test").unwrap());
    }

    #[test]
    fn grant_is_scoped_to_its_tool_and_pattern() {
        let db = Db::memory().unwrap();
        db.grant("/a", "terminal_use", "cargo test").unwrap();
        assert!(!db.is_granted("/a", "patch_file", "cargo test").unwrap());
        assert!(!db
            .is_granted("/a", "terminal_use", "cargo publish")
            .unwrap());
    }

    #[test]
    fn granting_twice_is_harmless() {
        let db = Db::memory().unwrap();
        db.grant("/a", "t", "p").unwrap();
        db.grant("/a", "t", "p").unwrap();
        assert_eq!(db.grants("/a").unwrap().len(), 1);
    }

    #[test]
    fn revoking_a_tool_takes_all_of_its_patterns_and_nothing_else() {
        let db = Db::memory().unwrap();
        db.grant("/a", "terminal_use", "cargo").unwrap();
        db.grant("/a", "terminal_use", "npm").unwrap();
        db.grant("/a", "patch_file", "patch_file").unwrap();
        assert_eq!(db.revoke_tool("/a", "terminal_use").unwrap(), 2);
        assert_eq!(db.grants("/a").unwrap().len(), 1);
    }

    #[test]
    fn revoke_removes_only_the_named_grant() {
        let db = Db::memory().unwrap();
        db.grant("/a", "t", "p1").unwrap();
        db.grant("/a", "t", "p2").unwrap();
        assert!(db.revoke("/a", "t", "p1").unwrap());
        assert!(!db.revoke("/a", "t", "p1").unwrap());
        assert_eq!(db.grants("/a").unwrap().len(), 1);
        assert_eq!(db.revoke_all("/a").unwrap(), 1);
        assert!(db.grants("/a").unwrap().is_empty());
    }
}
