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
                        "description": "The file to patch."
                    },
                    "old_str": {
                        "type": "string",
                        "description": "The exact text to replace, spaces and line breaks included. Copy it from the most recent read or patch in this turn, since text recalled from memory rarely matches exactly, and include enough surrounding lines for it to appear exactly once."
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
            return Err(Error::tool(format!(
                "Exact match not found; file unchanged.{} Re-read the target and verify whitespace before retrying.",
                nearest(&text, old)
            )));
        }
        1 => {}
        n => {
            return Err(Error::tool(format!(
                "old_str appears {n} times (lines {}) — include more surrounding context so it is unique",
                occurrence_lines(&text, old).join(", ")
            )));
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

/// The file line that comes closest to the first line of `old`.
///
/// A patch that misses by whitespace is the most common failed call (audit
/// 2026-11-01: 65 in one month, none of them acted on), and the plain "exact
/// match not found" leaves a blind retry as the only next step. Naming the
/// nearest line — and the line number, which every later call refers to — turns
/// that into one correction. A line too different to be the same text says
/// nothing: a misleading hint costs more than no hint.
fn nearest(text: &str, old: &str) -> String {
    let want = old.lines().next().unwrap_or("").trim();
    if want.is_empty() {
        return String::new();
    }
    let mut best: Option<(usize, usize, &str)> = None;
    for (index, line) in text.lines().enumerate() {
        let have = line.trim();
        if have.is_empty() {
            continue;
        }
        let score = want
            .chars()
            .zip(have.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if best.is_none_or(|(best_score, _, _)| score > best_score) {
            best = Some((score, index + 1, line));
        }
    }
    let Some((score, at, line)) = best else {
        return String::new();
    };
    // Fewer than half the opening characters in common: another line entirely.
    if score * 2 < want.chars().count() {
        return String::new();
    }
    let shown: String = line.trim().chars().take(120).collect();
    format!(
        "\nNearest match is line {at}: {}",
        crate::net::redact::scrub(&shown)
    )
}

/// The lines an ambiguous `old_str` starts on — the one thing a call that names
/// no context can act on (measured 2026-11-01: 36 such calls in one month, each
/// answered with "appears N times" and nothing else).
fn occurrence_lines(text: &str, old: &str) -> Vec<String> {
    text.match_indices(old)
        .map(|(at, _)| (text[..at].matches('\n').count() + 1).to_string())
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miss names the closest line, with the number every later call refers
    /// to; a line too different to be the same text says nothing, because a
    /// misleading hint costs more than none.
    #[test]
    fn a_miss_names_the_closest_line_and_never_a_far_one() {
        let file = "fn main() {}\n\nfn helper() -> u8 {\n    1\n}\n";
        let hint = nearest(file, "fn helper() -> u16 {");
        assert!(hint.contains("line 3"), "{hint}");
        assert!(hint.contains("fn helper"), "{hint}");
        assert_eq!(nearest(file, "completely different words here"), "");
        // Whitespace is the usual cause: the hint shows the line as the file has it.
        assert!(nearest(file, "    fn main() {}").contains("fn main() {}"));
    }

    #[test]
    fn every_occurrence_is_named_so_the_next_call_can_be_specific() {
        let file = "a\nsame\nb\nsame\nc\nsame\n";
        assert_eq!(occurrence_lines(file, "same"), vec!["2", "4", "6"]);
    }
}
