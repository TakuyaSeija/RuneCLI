//! Format 9 → 10: background tasks keep more of what they are.
//!
//! Additive only, so nothing is rewritten: a `background.jsonl` start record
//! may carry the process's identity (`identity`: boot and start time) and its
//! tree's mark (`tree`), an end may be `Lost` (with why), and a configuration
//! may name `terminal.max_background_log_size` and `terminal.input_wait_delay`.
//! Records an earlier build wrote stay byte for byte; a start without an
//! identity, if lost, is lost for an unknown reason — never guessed. The marker
//! still advances through the barrier, so a build that cannot read a `Lost` end
//! or the new keys never opens a home that holds them.

use std::path::Path;

use super::Plan;
use crate::error::Result;

pub(super) fn apply(_store: &Path, _backup: &Path, _plan: &mut Plan) -> Result<()> {
    Ok(())
}
