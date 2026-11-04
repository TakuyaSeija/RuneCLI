//! `inspect_file` — read files.
//!
//! Text comes back with line numbers because every later tool call refers to
//! lines. Binary files (images, audio, video, documents) are attached for
//! multimodal models up to 8MB.

use serde_json::{json, Value};

use super::{field_opt_str, field_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::Result;

/// Reading more than this from one file is almost always a mistake; the caller
/// gets a clear cut and a line count rather than a silent half-file.
pub struct InspectFile;
pub type InspectFiles = InspectFile;

impl Tool for InspectFile {
    fn name(&self) -> &'static str {
        "inspect_file"
    }

    fn mutates(&self) -> bool {
        false
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "inspect_file".into(),
            description: "Read one file. Text comes back with line numbers; an image, audio, video or document is attached when the model can take it."
                .into(),
            parameters: flat_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "The exact path of the file to read. A path the user or an earlier result has not given is best found first with find_files or search_text: a guessed path usually misses."
                    },
                    "view_range": {
                        "type": "array",
                        "minItems": 2,
                        "maxItems": 2,
                        "items": { "type": "integer" },
                        "description": "[first, last] line to read, 1-based and inclusive. Omit to read the whole file; for a large file, read the lines you need."
                    }
                }),
                &["path"],
            ),
        }
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        super::outside_paths(args, &["path"])
    }

    fn preview(&self, args: &Value) -> String {
        let path = field_opt_str(args, "path").unwrap_or("");
        if path.is_empty() {
            "read file".into()
        } else {
            format!("read {path}")
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(crate::Error::Cancelled);
        }
        let raw = field_str(args, "path")?;
        let (content, maybe_att) = one(ctx, args, raw)?;
        let summary = format!("read {raw}");
        let mut outcome = Outcome::new(summary, content);
        if let Some(att) = maybe_att {
            outcome.media.push(att);
        }
        Ok(outcome)
    }
}

fn one(
    ctx: &ToolCtx<'_>,
    item: &Value,
    raw: &str,
) -> Result<(String, Option<crate::media::Attachment>)> {
    // A registered SKILL.md reads like any other file; only its bytes come from
    // the integrity check, so the instructions shown are the ones Rune vetted.
    let skill = ctx
        .capabilities
        .read_skill(raw, item.get("view_range").is_none())?;
    let package = ctx.capabilities.package_root(raw);
    let path = if let Some(root) = &package {
        crate::jail::Jail::new(root)?.resolve(raw)?
    } else {
        ctx.jail.resolve(raw)?
    };
    let rel = ctx.jail.display(&path);
    let max = ctx.limits.config.number("inspect_file.max_file_size");
    let too_large = |len: u64| {
        crate::Error::invalid(format!(
            "file size ({len} bytes) exceeds inspect_file.max_file_size ({max} bytes)"
        ))
    };
    // Official help is read from the build itself, never from its copy on
    // disk, so a damaged copy cannot fail or alter a reading.
    let bytes = if let Some(text) = ctx.capabilities.official_text(&path)? {
        if text.len() as u64 > max {
            return Err(too_large(text.len() as u64));
        }
        text.as_bytes().to_vec()
    } else {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(crate::Error::NotFound(format!(
                    "{rel} — check the path or search with find_files{}",
                    nearby(ctx, &path)
                )));
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            return Err(crate::Error::invalid(format!(
                "{rel} is a directory; list its files with find_files"
            )));
        }
        if meta.len() > max {
            return Err(too_large(meta.len()));
        }
        if let Some(bytes) = skill {
            bytes
        } else if let Some(root) = &package {
            crate::capabilities::skills::read_package_file(root, &path, max as usize)?
        } else {
            crate::settings::configuration::read_bounded(&path, max)?
        }
    };
    // What the file is comes from `media`, the one decision every reader of a
    // file asks: a text file is read here, line-numbered, whatever stray bytes
    // it holds; only what is not text is attached.
    if !crate::media::is_text_mime(crate::media::kind_of(&bytes, &path)) {
        let file_len = bytes.len() as u64;
        match crate::media::Attachment::at(&path) {
            Ok(att) => {
                let notice = super::notice::inline(&format!(
                    "Attached {} ({}, {}).",
                    super::notice::quote(&rel),
                    att.mime,
                    crate::media::human(file_len)
                ));
                return Ok((notice, Some(att)));
            }
            Err(e) => {
                return Err(crate::Error::tool(format!(
                    "Binary file could not be attached: {e}"
                )));
            }
        }
    }

    // A NUL is shown as the symbol for one: it is in the file, and a raw NUL
    // is a byte some providers refuse in a request.
    let text = String::from_utf8_lossy(&bytes).replace('\0', "\u{2400}");
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let (start, end) = match item.get("view_range").and_then(|v| v.as_array()) {
        Some(r) if r.len() == 2 => {
            let s = r[0].as_i64().unwrap_or(1).max(1) as usize;
            let e = r[1].as_i64().unwrap_or(total as i64);
            let e = if e < 0 {
                total
            } else {
                (e as usize).min(total)
            };
            (s, e.max(s))
        }
        _ => (1, total),
    };

    let max_text_chars = ctx.limits.config.number("inspect_file.max_output_chars") as usize;
    let mut body = String::new();
    // Counted as it grows: counting the whole body again for every line made a
    // long read quadratic.
    let mut body_chars = 0usize;
    let mut notice = String::new();
    for (i, line) in lines.iter().enumerate().skip(start.saturating_sub(1)) {
        let n = i + 1;
        if n > end {
            break;
        }
        let prefix = format!("{n:>6}| ");
        let prefix_chars = prefix.chars().count();
        let line_chars = prefix_chars + line.chars().count() + 1;
        if body_chars + line_chars > max_text_chars {
            let available = max_text_chars.saturating_sub(body_chars + prefix_chars + 1);
            let truncated_line: String = line.chars().take(available).collect();
            body.push_str(&format!("{prefix}{truncated_line}\n"));
            let remaining = total.saturating_sub(n);
            let next_start = n;
            notice.push_str(&format!(
                "{}\n",
                super::notice::inline(&format!(
                    "Line {n} is cut and {remaining} later lines are not shown (output limit {max_text_chars} characters). Continue with view_range [{next_start}, {end}]; a single line longer than the limit is easier to read with terminal_use."
                ))
            ));
            break;
        } else {
            body.push_str(&format!("{prefix}{line}\n"));
            body_chars += line_chars;
        }
    }

    let header = if start == 1 && end == total {
        format!("=== {rel} ({total} lines) ===")
    } else {
        format!("=== {rel} (lines {start}-{end} of {total}) ===")
    };
    Ok((format!("{notice}{header}\n{body}\n"), None))
}

/// What the nearest existing folder holds, for a path that is not there.
///
/// Most missing reads name a root the project does not have (`Application/
/// Studio/...` inside the folder itself, measured 2026-11-01). One listing of
/// the closest folder that does exist answers that in the same step, with the
/// person's own project files — the same names `find_files` would show — so the
/// model can correct the path instead of guessing again.
fn nearby(ctx: &ToolCtx<'_>, path: &std::path::Path) -> String {
    let Some((dir, shown, more)) = nearest_folder(path) else {
        return String::new();
    };
    format!(
        "\nThe nearest folder that exists, {}, holds: {shown}{}",
        ctx.jail.display(&dir),
        if more { ", and more" } else { "" }
    )
}

/// The closest ancestor of `path` that exists, with its entries: folders first
/// marked with `/`, hidden ones left out, at most [`SHOWN`].
fn nearest_folder(path: &std::path::Path) -> Option<(std::path::PathBuf, String, bool)> {
    /// Enough to show the shape of a folder without turning an error into a listing.
    const SHOWN: usize = 12;
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current.is_dir() {
            let entries = std::fs::read_dir(current).ok()?;
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') {
                        return None;
                    }
                    Some(if entry.path().is_dir() {
                        format!("{name}/")
                    } else {
                        name
                    })
                })
                .collect();
            if names.is_empty() {
                return None;
            }
            names.sort();
            let more = names.len() > SHOWN;
            names.truncate(SHOWN);
            return Some((current.to_path_buf(), names.join(", "), more));
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing path is answered with what the closest real folder holds, so a
    /// wrong root (`Application/Studio/...`) is corrected in the same step.
    #[test]
    fn a_missing_path_is_answered_with_the_nearest_folder_that_exists() {
        let base = std::env::temp_dir().join(format!("rune-nearby-{}", std::process::id()));
        let project = base.join("project");
        std::fs::create_dir_all(project.join("Engine/src")).unwrap();
        std::fs::write(project.join("Cargo.toml"), "").unwrap();
        std::fs::write(project.join(".hidden"), "").unwrap();
        let (dir, shown, _) = nearest_folder(&project.join("Application/Studio/x.rs")).unwrap();
        assert_eq!(dir, project);
        assert!(shown.contains("Engine/"), "{shown}");
        assert!(shown.contains("Cargo.toml"), "{shown}");
        assert!(!shown.contains(".hidden"), "{shown}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
