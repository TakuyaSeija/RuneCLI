//! Format 8 → 9: finished tool audit rows may carry the Error variant's code.
//! It is an additive JSONL field, so there is no historical rewrite. A code
//! that was never recorded cannot be reconstructed from prose; old rows remain
//! unknown. The format marker still advances through the normal upgrade barrier
//! so old and new writers never lay incompatible audit state side by side.

use std::path::Path;

use super::Plan;
use crate::error::Result;

pub(super) fn apply(_store: &Path, _backup: &Path, _plan: &mut Plan) -> Result<()> {
    Ok(())
}
