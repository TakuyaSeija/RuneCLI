//! Format 20 introduced Rune's official help. It is installed — and replaced
//! whenever its copy differs from the build — by every process that discovers
//! skills (`capabilities::product::ensure`), so the capability switch is
//! honoured and a damaged copy never stops an upgrade or a start (owner,
//! 2026-10-16). Nothing in the home changes form here.
use super::Plan;
use crate::Result;
use std::path::Path;

pub(super) fn apply(_settings: &Path, _backup: &Path, _plan: &mut Plan) -> Result<()> {
    Ok(())
}
