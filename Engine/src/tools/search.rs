//! `search_text` — find content inside files.
//!
//! Line-oriented, like `grep`, because that is the shape an agent can act on:
//! `path:line: text` is enough to open the right place next.

use serde_json::{json, Value};

use super::{
    field_bool, field_opt_str, field_opt_u64, field_str, flat_schema, plural, Outcome, Tool,
    ToolCtx, ToolDecl,
};
use crate::error::Result;
use crate::scan::glob::Glob;
use crate::scan::rx::Regex;
use crate::scan::walk::{self, Options};

pub struct SearchText(pub std::sync::Arc<crate::limits::Limits>);

impl Tool for SearchText {
    fn name(&self) -> &'static str {
        "search_text"
    }

    fn mutates(&self) -> bool {
        false
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "search_text".into(),
            description: "Search file contents. Returns path:line: text for each matching line. Every file is searched, ignored and hidden ones too; only .git and .rune are skipped."
                .into(),
            parameters: flat_schema(
                json!({
                    "pattern": {
                        "type": "string",
                        "description": "The text to find, or a regular expression when regex is true. In both, | separates alternatives."
                    },
                    "path": {
                        "type": "string",
                        "description": "A file or directory to search: relative to the workspace, ~/..., or absolute. Default: the workspace root."
                    },
                    "regex": {
                        "type": "boolean",
                        "description": "Treat pattern as a regular expression. Default false."
                    },
                    "case_sensitive": {
                        "type": "boolean",
                        "description": "Match letter case exactly. Default true."
                    },
                    "include": {
                        "type": "string",
                        "description": "Search only files whose name matches this glob, e.g. *.rs."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum matching lines. Default and ceiling: search_text results in <status>."
                    }
                }),
                &["pattern"],
            ),
        }
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        super::outside_paths(args, &["path"])
    }

    fn preview(&self, args: &Value) -> String {
        let pat = field_opt_str(args, "pattern").unwrap_or("");
        if pat.is_empty() {
            "search file contents".into()
        } else {
            format!("search for \"{pat}\"")
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(crate::Error::Cancelled);
        }
        let pattern = field_str(args, "pattern")?;
        let as_regex = field_bool(args, "regex", false);
        let case_sensitive = field_bool(args, "case_sensitive", true);
        let max_matches = self.0.config.number("search_text.matches") as usize;
        let limit = field_opt_u64(args, "limit")
            .map(|l| (l as usize).min(max_matches))
            .unwrap_or(max_matches);
        let include = super::field_given_str(args, "include").map(Glob::new);

        let needle = needle_of(pattern, as_regex, case_sensitive)?;

        let needle = needle.with_step_budget(self.0.config.number("scan.regex_steps") as u32);
        let root = match super::field_given_str(args, "path") {
            Some(p) => ctx.jail.resolve(p)?,
            None => ctx.jail.root().to_path_buf(),
        };

        let mut hits: Vec<String> = Vec::new();
        let mut notices = String::new();
        let mut files_hit = 0usize;
        // (relative path, line number, why) for the lines that could not be
        // searched. Reported, never fatal.
        let mut skipped: Vec<(String, usize, String)> = Vec::new();

        if root.is_file() {
            let mut here = Vec::new();
            files_hit += scan_file(
                &root,
                &ctx.jail.display(&root),
                &needle,
                limit,
                &mut hits,
                &mut here,
                &mut notices,
                &self.0.config,
            )?;
            skipped.extend(
                here.into_iter()
                    .map(|(n, why)| (ctx.jail.display(&root).to_string(), n, why)),
            );
        } else {
            let mut err: Option<crate::error::Error> = None;
            walk::walk(&root, &Options::configured(&self.0.config), |e| {
                if let Some(g) = &include {
                    let name = e.rel.rsplit('/').next().unwrap_or(&e.rel);
                    if !(g.matches(&e.rel) || g.matches(name)) {
                        return true;
                    }
                }
                // Named as every tool names a workspace path: with `/` on
                // every OS (`Jail::display`), never `build\Out.txt` beside
                // `find_files`' `build/Out.txt`.
                let rel = match e.path.strip_prefix(ctx.jail.root()) {
                    Ok(_) => ctx.jail.display(&e.path),
                    Err(_) => e.rel.clone(),
                };
                let mut here: Vec<(usize, String)> = Vec::new();
                match scan_file(
                    &e.path,
                    &rel,
                    &needle,
                    limit,
                    &mut hits,
                    &mut here,
                    &mut notices,
                    &self.0.config,
                ) {
                    Ok(n) => files_hit += n,
                    Err(e) => {
                        err = Some(e);
                        return false;
                    }
                }
                skipped.extend(here.into_iter().map(|(n, why)| (rel.clone(), n, why)));
                hits.len() < limit && !ctx.cancelled()
            })?;
            if let Some(e) = err {
                return Err(e);
            }
        }

        let mut out = format!(
            "=== \"{pattern}\" — {} match(es) in {files_hit} file(s) ===\n",
            hits.len()
        );
        for h in &hits {
            out.push_str(h);
            out.push('\n');
        }
        if hits.len() >= limit {
            notices.push_str(&super::notice::inline(&format!(
                "The search stopped at {limit} matching lines."
            )));
            notices.push('\n');
        }
        // The gap is named, so it can be searched by hand. A search that skips
        // a line without saying so is a search that lies about being complete.
        if !skipped.is_empty() {
            let lines = skipped
                .iter()
                .map(|(rel, n, _)| format!("{rel}:{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let reason = &skipped[0].2;
            notices.push_str(&super::notice::inline(&format!(
                "{} could not be searched by this pattern ({}; {}).",
                plural(skipped.len(), "line"),
                super::notice::quote(&lines),
                super::notice::reason(reason)
            )));
            notices.push('\n');
        }

        let max_chars = self.0.config.number("search_text.max_output_chars") as usize;
        let final_out = super::cap(out, max_chars);

        Ok(
            Outcome::new(plural(hits.len(), "match"), format!("{notices}{final_out}"))
                .with_detail(json!({ "matches": hits.len() })),
        )
    }
}

/// The pattern as the matcher it asks for. An invalid regular expression is
/// said so the next call is right: what is wrong, where, and the two ways out.
/// It is never quietly searched as text instead: `.` or `*` would then mean
/// something other than what was asked.
fn needle_of(pattern: &str, as_regex: bool, case_sensitive: bool) -> crate::Result<Regex> {
    if !as_regex {
        return Regex::new(&literal_alternation(pattern), !case_sensitive);
    }
    Regex::new(pattern, !case_sensitive).map_err(|error| match error {
        crate::Error::Invalid(why) => crate::Error::invalid(format!(
            "the pattern is not a valid regular expression: {why}. Put \\ before a character to match it as text, or search with regex: false, where | still separates alternatives"
        )),
        other => other,
    })
}

/// Turn a literal into a regex that matches exactly it.
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Turn a literal pattern into a regex source that keeps `|` as alternation.
///
/// In literal mode the pattern was escaped wholesale — which disarmed `|`
/// with it, so the natural way to write a multi-needle search (`foo|bar`)
/// searched for the literal string `foo|bar` and found nothing anywhere. Each
/// branch is escaped on its own and the alternatives rejoined, so `|` splits
/// literals while every other metacharacter stays literal. A `|` that must
/// match itself belongs to a regex search (`\|`) — the same trade every
/// literal/regex toggle makes.
fn literal_alternation(pattern: &str) -> String {
    let branches: Vec<&str> = pattern.split('|').filter(|b| !b.is_empty()).collect();
    if branches.len() <= 1 {
        return escape_literal(pattern);
    }
    branches
        .iter()
        .map(|b| escape_literal(b))
        .collect::<Vec<_>>()
        .join("|")
}

#[allow(clippy::too_many_arguments)]
fn scan_file(
    path: &std::path::Path,
    rel: &str,
    needle: &Regex,
    limit: usize,
    hits: &mut Vec<String>,
    out_skipped: &mut Vec<(usize, String)>,
    notices: &mut String,
    config: &crate::settings::configuration::Shared,
) -> Result<usize> {
    let max_file_size = config.number("inspect_file.max_file_size");
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > max_file_size {
            return Ok(0);
        }
    }
    let bytes = match crate::settings::configuration::read_bounded(path, max_file_size) {
        Ok(b) => b,
        Err(_) => return Ok(0),
    };
    // Searched when it is text, by the same decision `inspect_file` reads it by.
    if !crate::media::is_text_mime(crate::media::kind_of(&bytes, path)) {
        return Ok(0);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut found = 0;
    let mut skipped: Vec<(usize, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if hits.len() >= limit {
            break;
        }
        // One expensive line must not throw away the whole search.
        //
        // This used to be `needle.find(line)?`. A single line the pattern could
        // not afford took every other match in the file with it — the results
        // already found, in this file and in every file walked after it — and
        // reported the whole thing as a failure. The search tool's job is to
        // find things; losing a hundred good matches to one hard line is the
        // opposite of that.
        //
        // So the line is skipped, remembered, and reported at the end. A
        // reader can then see *where* the gap is and search there by hand,
        // which is an answer; a bare failure is not.
        let hit = match needle.find(line) {
            Ok(hit) => hit,
            Err(e) => {
                skipped.push((i + 1, e.to_string()));
                continue;
            }
        };
        // An over-long line is still searched, just not printed whole.
        if hit.is_some() {
            let max_line_chars = config.number("search_text.max_line_chars") as usize;
            let total_chars = line.chars().count();
            let shown: String = if total_chars > max_line_chars {
                let head: String = line.chars().take(max_line_chars).collect();
                notices.push_str(&super::notice::inline(&format!(
                    "{}:{} is a long line, cut at {max_line_chars} of {total_chars} characters.",
                    super::notice::quote(rel),
                    i + 1
                )));
                notices.push('\n');
                format!("{head}...")
            } else {
                line.to_string()
            };
            hits.push(format!("{rel}:{}: {}", i + 1, shown.trim_end()));
            found += 1;
        }
    }
    // The lines that cost too much to search, carried back to the caller so the
    // result can name them rather than pretend they were not there.
    if !skipped.is_empty() {
        *out_skipped = skipped;
    }
    Ok(if found > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_text_defaults_to_regex_false_in_declaration() {
        let tool = SearchText(std::sync::Arc::new(crate::limits::Limits::default()));
        let decl = tool.declaration();
        let schema = &decl.parameters["properties"]["regex"];
        assert!(schema["description"]
            .as_str()
            .unwrap()
            .contains("Default false"));
    }

    /// Session 570: `suspend fun call|fun call(` with regex true came back as
    /// "invalid: unclosed `(`" — not that it was the regex, not where, not
    /// what to do. The next call guessed right; the message makes it certain.
    #[test]
    fn an_invalid_regex_says_where_and_how_to_fix_it() {
        let error = needle_of("suspend fun call|fun call(", true, true)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("not a valid regular expression"), "{error}");
        assert!(
            error.contains("`(` at character 26 is never closed"),
            "{error}"
        );
        assert!(error.contains("Put \\ before a character"), "{error}");
        assert!(error.contains("regex: false"), "{error}");
        // The same text without regex is a search, and finds what it says.
        let literal = needle_of("suspend fun call|fun call(", false, true).unwrap();
        assert!(literal
            .is_match("    suspend fun call(path: String)")
            .unwrap());
        assert!(needle_of("[abc", true, true)
            .err()
            .unwrap()
            .to_string()
            .contains("`[` at character 1 is never closed"));
    }

    #[test]
    fn literal_mode_alternates_on_the_pipe_instead_of_searching_for_one() {
        // `alpha|omega` in literal mode was escaped wholesale and searched for
        // the string `alpha|omega` — found nowhere, ever. Each branch is
        // literal; the pipe is what joins them.
        let needle = Regex::new(&literal_alternation("alpha|omega"), false).unwrap();
        assert!(needle.is_match("call alpha()").unwrap());
        assert!(needle.is_match("the omega end").unwrap());
        assert!(!needle.is_match("just beta here").unwrap());

        // No pipe: exactly what it always was, metacharacters still literal.
        let plain = Regex::new(&literal_alternation("a.b"), false).unwrap();
        assert!(plain.is_match("a.b").unwrap());
        assert!(
            !plain.is_match("axb").unwrap(),
            "a literal dot is not any character"
        );

        // A trailing `|` has no second branch, so the whole pattern stays
        // literal — the escape hatch for a pipe that must match itself.
        let trailing = Regex::new(&literal_alternation("needle|"), false).unwrap();
        assert!(trailing.is_match("uses a needle| here").unwrap());
        assert!(
            !trailing.is_match("nothing here").unwrap(),
            "an unmeant pattern must not match the world"
        );
    }
}
