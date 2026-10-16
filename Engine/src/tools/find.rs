//! `find_files` — locate files by path pattern.
//!
//! A pattern with no `/` is matched at any depth, because that is what someone
//! means by `*.rs`. Anything with a `/` is matched against the path as written,
//! where `**` is the only wildcard that crosses directories.

use serde_json::{json, Value};

use super::{
    field_opt_str, field_opt_u64, field_str, flat_schema, plural, Outcome, Tool, ToolCtx, ToolDecl,
};
use crate::error::Result;
use crate::scan::glob::Glob;
use crate::scan::walk::{self, Options};

pub struct FindFiles(pub std::sync::Arc<crate::limits::Limits>);

impl Tool for FindFiles {
    fn name(&self) -> &'static str {
        "find_files"
    }

    fn mutates(&self) -> bool {
        false
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "find_files".into(),
            description: "Find files by path pattern. *.rs matches at any depth; src/**/*.rs only under src. Every file is searched, ignored and hidden ones too; only .git and .rune are skipped."
                .into(),
            parameters: flat_schema(
                json!({
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern, e.g. *.rs, src/**/mod.rs, Cargo.toml."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search: relative to the workspace, ~/..., or absolute. Default: the workspace root."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum matches. Default and ceiling: find_files results in <status>."
                    }
                }),
                &["pattern"],
            ),
        }
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        super::outside_paths(args, &["pattern", "path"])
    }

    fn preview(&self, args: &Value) -> String {
        let pattern = field_opt_str(args, "pattern").unwrap_or("");
        if pattern.is_empty() {
            "find files".into()
        } else {
            format!("find {pattern}")
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(crate::Error::Cancelled);
        }
        let raw = field_str(args, "pattern")?;
        let max_matches = self.0.config.number("find_files.matches") as usize;
        let limit = field_opt_u64(args, "limit")
            .map(|l| (l as usize).min(max_matches))
            .unwrap_or(max_matches);

        let (search_dir, pattern_str) = match super::field_given_str(args, "path") {
            Some(p) => (
                ctx.jail.resolve(p)?,
                if raw.contains('/') {
                    raw.to_string()
                } else {
                    format!("**/{raw}")
                },
            ),
            None => {
                if raw.starts_with('/') || raw.starts_with('~') {
                    if let Some(last_slash) = raw.rfind('/') {
                        let dir_part = &raw[..last_slash];
                        let file_part = &raw[last_slash + 1..];
                        let resolved_dir =
                            ctx.jail
                                .resolve(if dir_part.is_empty() { "/" } else { dir_part })?;
                        (
                            resolved_dir,
                            if file_part.is_empty() {
                                "**/*".to_string()
                            } else if file_part.contains('/') {
                                file_part.to_string()
                            } else {
                                format!("**/{file_part}")
                            },
                        )
                    } else {
                        (ctx.jail.root().to_path_buf(), format!("**/{raw}"))
                    }
                } else {
                    (
                        ctx.jail.root().to_path_buf(),
                        if raw.contains('/') {
                            raw.to_string()
                        } else {
                            format!("**/{raw}")
                        },
                    )
                }
            }
        };
        let glob = Glob::new(&pattern_str);

        let mut hits: Vec<String> = Vec::new();
        walk::walk(&search_dir, &Options::configured(&self.0.config), |e| {
            if glob.matches(&e.rel) {
                hits.push(e.rel);
            }
            hits.len() < limit && !ctx.cancelled()
        })?;

        let mut out = format!("=== {raw} — {} match(es) ===\n", hits.len());
        for h in &hits {
            out.push_str(h);
            out.push('\n');
        }
        let notice = if hits.len() == limit {
            format!(
                "{}\n\n",
                super::notice::inline(&format!("The search stopped at {limit} matches."))
            )
        } else {
            String::new()
        };

        let max_chars = self.0.config.number("find_files.max_output_chars") as usize;
        let content = super::cap(out, max_chars);

        let summary = format!("found {}", plural(hits.len(), "match"));
        Ok(Outcome::new(summary, format!("{notice}{content}"))
            .with_detail(json!({ "matches": hits.len() })))
    }
}
