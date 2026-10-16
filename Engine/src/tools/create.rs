//! `create_file` — write a file.
//!
//! Overwrites an existing file if it exists, capturing previous state in the
//! checkpoint engine before writing.

use serde_json::{json, Value};

use super::{field_opt_str, field_str, flat_schema, plural, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};

pub struct CreateFile(pub std::sync::Arc<crate::limits::Limits>);
pub type CreateFiles = CreateFile;

impl Tool for CreateFile {
    fn name(&self) -> &'static str {
        "create_file"
    }

    fn mutates(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "create_file".into(),
            description: "Create a file with the given content. WARNING: Overwrites the file if it already exists."
                .into(),
            parameters: flat_schema(
                json!({
                    "path": {
                        "type": "string",
                        "description": "The file: relative to the workspace, ~/..., absolute, or @memory/..."
                    },
                    "text": {
                        "type": "string",
                        "description": "The file's complete content."
                    }
                }),
                &["path", "text"],
            ),
        }
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        super::outside_paths(args, &["path"])
    }

    fn preview(&self, args: &Value) -> String {
        let path = field_opt_str(args, "path").unwrap_or("?");
        let n = field_opt_str(args, "text")
            .map(|t| t.lines().count())
            .unwrap_or(0);
        format!("create {path} ({})", plural(n, "line"))
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let raw = field_str(args, "path")?;
        let text = field_str(args, "text")?;
        let (rel, path, existed, removed) = write_one(ctx, raw, text)?;
        let added = text.lines().count();
        ctx.record_write(&path, added as u64, removed as u64)?;
        let summary = "Success";
        // One line: the write is the result, and it was made.
        let report = if existed {
            format!(
                "Overwrote {rel}: {} replaced with {}.",
                super::plural(removed, "line"),
                super::plural(added, "line")
            )
        } else {
            format!("Created {rel} ({}).", super::plural(added, "line"))
        };
        Ok(Outcome::new(summary, report).with_detail(json!({
            "path": rel,
            "added": added,
            "removed": removed,
            "overwritten": existed,
        })))
    }
}

fn write_one(
    ctx: &ToolCtx<'_>,
    raw: &str,
    text: &str,
) -> Result<(String, std::path::PathBuf, bool, usize)> {
    let path = ctx.jail.resolve(raw)?;
    // Held before the existence check, so two sessions creating one note
    // cannot both find it missing.
    let held = ctx.hold(&path)?;
    let existed = path.exists();
    let removed = if existed {
        std::fs::read(&path)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).lines().count())
            .unwrap_or(0)
    } else {
        0
    };

    // Undo before change, always. The capture records "this file did not exist"
    // or previous state, which is what makes the restore work.
    ctx.write_guard(&path)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    held.write(&path, text.as_bytes())?;
    Ok((ctx.jail.display(&path), path, existed, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_file_declaration_warns_about_overwriting() {
        let tool = CreateFile(std::sync::Arc::new(crate::limits::Limits::default()));
        let decl = tool.declaration();
        assert!(decl
            .description
            .contains("WARNING: Overwrites the file if it already exists"));
        let path_prop = &decl.parameters["properties"]["path"];
        assert_eq!(path_prop["type"], "string");
        assert!(path_prop["description"]
            .as_str()
            .unwrap()
            .contains("The file: relative to the workspace"));
    }
}
