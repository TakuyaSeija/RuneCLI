//! Format 6 → 7 (Rune 0.3.8).
//!
//! Redo (owner, 2026-10-09): the session's file ledger keeps what the last
//! undo of each file overwrote, so the database schema moves to 22. The
//! migration runs here, under the upgrade's lock with every process of the
//! previous build gone, and its copy is named in the plan so an undo puts the
//! database back.

use std::path::Path;

use super::Plan;
use crate::error::Result;

/// Upgrade `store` from format 6, writing every change into `plan` before
/// making it.
pub(super) fn apply(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::v2::database(store, backup, plan)
}
