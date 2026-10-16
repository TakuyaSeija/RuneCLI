//! Format 10 → 11: five limits retired, four made the person's (owner,
//! 2026-10-11).
//!
//! Retired, so a `rune.toml` that names them would be refused (a retired key
//! is an unknown key): `tools.max_parallel` (one response's calls are bounded
//! by `tools.max_actions_per_response` alone), `scan.respect_ignore` and
//! `scan.hidden` (a search is shown every file, save `.git` and `.rune`),
//! `checkpoint.keep_turns` (undo history is never trimmed) and
//! `attachments.snapshot_max_size` (an attachment is read from its path and
//! copied only once it changes). Whatever value the person had set for them
//! is dropped: the behaviour they chose between is gone.
//!
//! Made user-facing — shown in Settings and written to `rune.toml` with their
//! defaults: `terminal.max_background_log_size`, `scan.max_entries`,
//! `scan.max_depth` and `scan.regex_steps`.
//!
//! Attachment rows may now carry the file's modification time beside its
//! hash; an older row has none and is simply hashed again. Nothing the
//! session store holds is rewritten.

use std::path::Path;

use super::Plan;
use crate::error::{Error, Result};
use crate::settings::configuration::FILE;

/// The keys this format retires, as `(table, key)`.
pub(super) const RETIRED: &[(&str, &str)] = &[
    ("tools", "max_parallel"),
    ("scan", "respect_ignore"),
    ("scan", "hidden"),
    ("checkpoint", "keep_turns"),
    ("attachments", "snapshot_max_size"),
];

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

/// `text` without the retired keys, every other value and comment kept —
/// however the person wrote the table: `[scan]`, `scan.hidden = …` or
/// `scan = { hidden = … }`.
pub(super) fn retire(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    let mut changed = false;
    for (table, key) in RETIRED {
        if let Some(table) = document
            .get_mut(table)
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            changed |= table.remove(key).is_some();
        }
    }
    Ok(if changed {
        document.to_string()
    } else {
        text.to_string()
    })
}

/// The configuration in this format: retired keys gone, the keys that became
/// user-facing written with their defaults.
pub(super) fn config(text: &str) -> Result<String> {
    crate::settings::configuration::laid_out(&retire(text)?)
}
