//! `find_files` — locate files by path pattern.
//!
//! A pattern with no `/` is matched at any depth, because that is what someone
//! means by `*.rs`. Anything with a `/` — relative or absolute — is matched
//! against the path as written, where `**` is the only wildcard that crosses
//! directories; the walk starts in the folders it names (`tools::Scope`).

use serde_json::{json, Value};

use super::{
    field_opt_str, field_opt_u64, field_str, flat_schema, plural, Outcome, Tool, ToolCtx, ToolDecl,
};
use crate::error::Result;
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
            description:
                "Find files by path pattern. *.py matches at any depth; src/**/*.py only under src."
                    .into(),
            parameters: flat_schema(
                json!({
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern from the workspace root, e.g. *.ts, docs/**/*.md, package.json."
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
        super::outside_paths(args, &["pattern"])
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

        // One root, the workspace (owner, 2026-11-03): a directory is part of
        // the pattern (`src/**/*.rs`), which is how a model already spells one;
        // the old `path` argument was read as a file's path instead. The walk
        // starts in the folder the pattern names, and every match is named as
        // every tool names a path (`Jail::display`): from the workspace root,
        // or whole when it lies outside it.
        let scope = super::Scope::of(ctx.jail, raw)?;

        let mut hits: Vec<String> = Vec::new();
        let mut stats = None;
        match &scope.file {
            Some(file) => hits.push(ctx.jail.display(file)),
            None => {
                stats = Some(walk::walk(
                    &scope.dir,
                    &Options::configured(&self.0.config),
                    |e| {
                        if scope.glob.as_ref().is_none_or(|glob| glob.matches(&e.rel)) {
                            hits.push(ctx.jail.display(&e.path));
                        }
                        hits.len() < limit && !ctx.cancelled()
                    },
                )?);
            }
        }

        let mut out = format!("=== {raw} — {} match(es) ===\n", hits.len());
        for h in &hits {
            out.push_str(h);
            out.push('\n');
        }
        let mut notices: Vec<String> = Vec::new();
        if hits.len() == limit {
            notices.push(super::notice::inline(&format!(
                "The search stopped at {limit} matches."
            )));
        }
        // A walk that ran out of its bounds found less than there is, and says
        // so: a silent stop reads as "no such file".
        if let Some(cut) = stats.as_ref().and_then(|stats| {
            super::walk_stopped(
                &self.0.config,
                stats,
                "name a folder in the pattern, as in src/**/*.rs, to look only there.",
            )
        }) {
            notices.push(cut);
        }
        let notice = if notices.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", notices.join("\n"))
        };

        let max_chars = self.0.config.number("find_files.max_output_chars") as usize;
        let content = super::cap(out, max_chars);

        let summary = format!("found {}", plural(hits.len(), "match"));
        Ok(Outcome::new(summary, format!("{notice}{content}"))
            .with_detail(json!({ "matches": hits.len() })))
    }
}
