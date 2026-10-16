//! Format 11 → 12 lays out the checkpoint refresh setting through the same
//! configuration writer; this release adds no runtime legacy fallback.
use super::Plan;
use crate::error::Result;
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(
        settings,
        backup,
        plan,
        crate::settings::configuration::laid_out,
    )
}
