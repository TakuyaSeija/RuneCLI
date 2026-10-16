//! `patch_file` — replace an exact string inside a file.
//!
//! `old_str` must appear **exactly once**. Not "the first occurrence": exactly
//! once. An agent that means one of three identical lines and gets the wrong one
//! produces a change nobody asked for, and the failure is silent. Requiring
//! uniqueness turns that into an error the model can fix by quoting more context.

use serde_json::{json, Value};

use super::{field_opt_str, field_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};

pub struct PatchFile(pub std::sync::Arc<crate::limits::Limits>);
pub type PatchFiles = PatchFile;

impl Tool for PatchFile {
    fn name(&self) -> &'static str {
        "patch_file"
    }

    fn mutates(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "patch_file".into(),
            description: "Replace one exact piece of text in a file.".into(),
            parameters: flat_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "The file: relative to the workspace, ~/..., absolute, or @memory/..."
                    },
                    "old_str": {
                        "type": "string",
                        "description": "The text to replace, exactly as in the file, spaces and line breaks included. It must appear exactly once."
                    },
                    "new_str": {
                        "type": "string",
                        "description": "The text that replaces it. An empty string deletes old_str."
                    }
                }),
                &["path", "old_str", "new_str"],
            ),
        }
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        super::outside_paths(args, &["path"])
    }

    fn preview(&self, args: &Value) -> String {
        let path = field_opt_str(args, "path").unwrap_or("?");
        let old = field_opt_str(args, "old_str").unwrap_or("");
        let new = field_opt_str(args, "new_str").unwrap_or("");
        let removed = old.lines().count();
        let added = if new.is_empty() {
            0
        } else {
            new.lines().count()
        };
        format!("patch {path} (-{removed} +{added})")
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let raw = field_str(args, "path")?;
        let old = field_str(args, "old_str")?;
        let new = field_str(args, "new_str")?;

        let (rel, path) = patch_one(ctx, raw, old, new)?;
        let unchanged = old == new;
        let removed = if unchanged { 0 } else { old.lines().count() };
        let added = if unchanged { 0 } else { new.lines().count() };
        if !unchanged {
            ctx.record_write(&path, added as u64, removed as u64)?;
        }
        let summary = "Success";
        // One line: the write is the result, and it was made.
        let report = if unchanged {
            format!("Unchanged {rel}: the replacement is the same as the text it replaces.")
        } else {
            format!(
                "Patched {rel}: {} replaced with {}.",
                super::plural(removed, "line"),
                super::plural(added, "line")
            )
        };
        Ok(Outcome::new(summary, report).with_detail(json!({
            "path": rel,
            "removed": removed,
            "added": added,
        })))
    }
}

fn patch_one(
    ctx: &ToolCtx<'_>,
    raw: &str,
    old: &str,
    new: &str,
) -> Result<(String, std::path::PathBuf)> {
    if old.is_empty() {
        return Err(Error::invalid("old_str must not be empty"));
    }
    let path = ctx.jail.resolve(raw)?;
    let rel = ctx.jail.display(&path);
    // Held from the read to the write: a memory file another session changes
    // in between would otherwise lose that change to this one.
    let held = ctx.hold(&path)?;
    if !path.exists() {
        return Err(Error::NotFound(format!(
            "{rel} — use create_file for a new file"
        )));
    }

    let max = ctx.limits.config.number("checkpoint.max_file_size");
    let meta = std::fs::metadata(&path)?;
    if meta.len() > max {
        return Err(Error::invalid(format!(
            "file size ({} bytes) exceeds snapshot limit ({max} bytes)",
            meta.len()
        )));
    }
    let bytes = crate::settings::configuration::read_bounded(&path, max)?;
    let text =
        String::from_utf8(bytes).map_err(|_| Error::tool(format!("{rel} is not UTF-8 text")))?;

    let count = text.matches(old).count();
    match count {
        0 => {
            return Err(Error::tool(
                "Exact match not found; file unchanged. Re-read the target and verify whitespace before retrying.",
            ));
        }
        1 => {}
        n => {
            return Err(Error::tool(format!(
                "old_str appears {n} times — include more surrounding context so it is unique"
            )))
        }
    }

    if old == new {
        return Ok((rel, path));
    }
    ctx.write_guard(&path)?;
    let patched = text.replacen(old, new, 1);
    held.write(&path, patched.as_bytes())?;
    Ok((rel, path))
}
