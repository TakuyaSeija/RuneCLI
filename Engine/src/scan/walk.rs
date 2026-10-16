//! Directory walking with `.gitignore`-style filtering.
//!
//! Deliberately not the `ignore` crate: that brings parallel walking, git repo
//! discovery, global/system ignore files and a `regex` dependency. We need one
//! bounded, single-threaded, deterministic walk of one project directory.
//!
//! Semantics implemented, in `.gitignore`'s own order of precedence:
//!   * a bare name (`node_modules`) matches that name at any depth
//!   * a leading `/` anchors to the directory holding the ignore file
//!   * a trailing `/` matches directories only
//!   * `!` re-includes; the last matching rule wins
//!   * an ignored directory is not descended into
//!
//! `.git` and `.rune` are always skipped: one is not ours, the other is where
//! checkpoints live and would otherwise snapshot themselves.

use std::path::{Path, PathBuf};

use super::glob::Glob;
use crate::error::Result;

/// Hard stop so a walk of `/` by accident cannot run forever.
pub const DEFAULT_MAX_ENTRIES: usize = 200_000;
pub const DEFAULT_MAX_DEPTH: usize = 32;

#[derive(Debug, Clone)]
pub struct Options {
    pub max_depth: usize,
    pub max_entries: usize,
    /// Honour `.gitignore` files found along the way.
    pub respect_ignore: bool,
    /// Include entries whose name starts with `.`.
    pub hidden: bool,
    /// Emit directories as entries too (they are always descended regardless).
    pub emit_dirs: bool,
    /// Package inspection must report unreadable paths and links instead of silently skipping them.
    pub strict: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_depth: DEFAULT_MAX_DEPTH,
            max_entries: DEFAULT_MAX_ENTRIES,
            respect_ignore: true,
            hidden: true,
            emit_dirs: false,
            strict: false,
        }
    }
}

impl Options {
    /// The walk `find_files` and `search_text` make: everything in the
    /// project is shown to them — files an ignore file names and hidden ones
    /// alike (owner, 2026-10-11: nothing is hidden from a search) — save
    /// `.git` and `.rune`, which no walk enters. Depth and size stay the
    /// person's to set.
    pub fn configured(config: &crate::settings::configuration::Shared) -> Self {
        Self {
            max_depth: config.number("scan.max_depth") as usize,
            max_entries: config.number("scan.max_entries") as usize,
            respect_ignore: false,
            hidden: true,
            emit_dirs: false,
            strict: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Path relative to the walk root, `/`-separated.
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub seen: usize,
    pub emitted: usize,
    pub skipped_ignored: usize,
    /// True when the walk stopped because it hit `max_entries`.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct Rule {
    glob: Glob,
    negate: bool,
    /// Directory of the ignore file this rule came from, relative to the root.
    base: String,
    bare: bool,
}

/// Walk `root`, calling `visit` for every entry that survives filtering.
/// `visit` returns `false` to stop the walk early.
pub fn walk<F>(root: &Path, opts: &Options, mut visit: F) -> Result<Stats>
where
    F: FnMut(Entry) -> bool,
{
    let mut stats = Stats::default();
    let mut rules: Vec<Rule> = Vec::new();
    if opts.respect_ignore {
        load_rules(root, "", &mut rules);
    }
    descend(root, "", 0, opts, &mut rules, &mut stats, &mut visit)?;
    Ok(stats)
}

fn descend<F>(
    dir: &Path,
    rel_dir: &str,
    depth: usize,
    opts: &Options,
    rules: &mut Vec<Rule>,
    stats: &mut Stats,
    visit: &mut F,
) -> Result<bool>
where
    F: FnMut(Entry) -> bool,
{
    if depth > opts.max_depth {
        stats.truncated = true;
        return Ok(true);
    }

    let scan_limit = if opts.strict {
        opts.max_entries
    } else {
        opts.max_entries.saturating_mul(16).max(256)
    };
    let mut entries: Vec<(String, std::fs::DirEntry)> = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        // An unreadable directory is a fact about the project, not a failure of
        // the walk. Skip it and keep going.
        Err(e) if opts.strict => return Err(e.into()),
        Err(_) => return Ok(true),
    };
    for e in read {
        let e = match e {
            Ok(e) => e,
            Err(e) if opts.strict => return Err(e.into()),
            Err(_) => continue,
        };
        if entries.len().saturating_add(stats.seen) >= scan_limit {
            stats.truncated = true;
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        entries.push((name, e));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, entry) in entries {
        if stats.seen >= scan_limit {
            stats.truncated = true;
            return Ok(false);
        }
        if name == ".git" || name == ".rune" {
            continue;
        }
        if !opts.hidden && name.starts_with('.') {
            continue;
        }
        stats.seen += 1;

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) if opts.strict => return Err(e.into()),
            Err(_) => continue,
        };
        // Symlinks are never followed: a link out of the project would let a walk
        // read the whole disk, and a link loop would never end.
        if meta.file_type().is_symlink() {
            if opts.strict {
                return Err(crate::Error::invalid("Package contains a symbolic link"));
            }
            continue;
        }
        if opts.strict && !meta.is_file() && !meta.is_dir() {
            return Err(crate::Error::invalid("Package contains a special file"));
        }

        let is_dir = meta.is_dir();
        let rel = if rel_dir.is_empty() {
            name.clone()
        } else {
            format!("{rel_dir}/{name}")
        };

        if opts.respect_ignore && ignored(rules, &rel, is_dir) {
            stats.skipped_ignored += 1;
            continue;
        }

        if is_dir {
            if opts.emit_dirs {
                stats.emitted += 1;
                if !visit(Entry {
                    path: entry.path(),
                    rel: rel.clone(),
                    is_dir: true,
                    size: 0,
                }) {
                    return Ok(false);
                }
            }
            let added = if opts.respect_ignore {
                load_rules(&entry.path(), &rel, rules)
            } else {
                0
            };
            let keep_going = descend(&entry.path(), &rel, depth + 1, opts, rules, stats, visit)?;
            rules.truncate(rules.len() - added);
            if !keep_going {
                return Ok(false);
            }
        } else {
            if stats.emitted >= opts.max_entries {
                stats.truncated = true;
                return Ok(false);
            }
            stats.emitted += 1;
            if !visit(Entry {
                path: entry.path(),
                rel,
                is_dir: false,
                size: meta.len(),
            }) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Read `<dir>/.gitignore` and push its rules. Returns how many were pushed so
/// the caller can pop them on the way back up.
fn load_rules(dir: &Path, rel_dir: &str, rules: &mut Vec<Rule>) -> usize {
    let text = match std::fs::read_to_string(dir.join(".gitignore")) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let mut added = 0;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, body) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        if body.is_empty() {
            continue;
        }
        let glob = Glob::new(body);
        let bare = glob.is_bare_name();
        rules.push(Rule {
            glob,
            negate,
            base: rel_dir.to_string(),
            bare,
        });
        added += 1;
    }
    added
}

/// Last matching rule wins, exactly as git does it.
fn ignored(rules: &[Rule], rel: &str, is_dir: bool) -> bool {
    let mut verdict = false;
    for r in rules {
        if r.glob.dir_only && !is_dir {
            continue;
        }
        let subject = match strip_base(&r.base, rel) {
            Some(s) => s,
            None => continue,
        };
        let hit = if r.bare {
            subject.split('/').any(|seg| r.glob.matches(seg))
        } else {
            r.glob.matches(subject) || {
                // An unanchored pattern with a slash still matches at any depth.
                !r.glob.anchored
                    && subject
                        .match_indices('/')
                        .any(|(i, _)| r.glob.matches(&subject[i + 1..]))
            }
        };
        if hit {
            verdict = !r.negate;
        }
    }
    verdict
}

fn strip_base<'a>(base: &str, rel: &'a str) -> Option<&'a str> {
    if base.is_empty() {
        return Some(rel);
    }
    rel.strip_prefix(base).and_then(|r| r.strip_prefix('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("rune-test").join(format!(
            "runewalk-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("main.rs"), b"fn main(){}").unwrap();
        std::fs::write(dir.join("src/lib.rs"), b"// lib").unwrap();
        std::fs::write(dir.join("src/deep/mod.rs"), b"// deep").unwrap();
        std::fs::write(dir.join("target/debug/app"), b"binary").unwrap();
        std::fs::write(dir.join(".git/config"), b"x").unwrap();
        dir
    }

    fn collect(dir: &Path, opts: &Options) -> Vec<String> {
        let mut out = Vec::new();
        walk(dir, opts, |e| {
            out.push(e.rel);
            true
        })
        .unwrap();
        out.sort();
        out
    }

    #[test]
    fn walks_files_and_skips_git() {
        let dir = tree("basic");
        let got = collect(&dir, &Options::default());
        assert!(got.contains(&"main.rs".to_string()));
        assert!(got.contains(&"src/deep/mod.rs".to_string()));
        assert!(!got.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn gitignore_prunes_directory() {
        let dir = tree("ignore");
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let got = collect(&dir, &Options::default());
        assert!(!got.iter().any(|p| p.starts_with("target")));
        assert!(got.contains(&"src/lib.rs".to_string()));
    }

    #[test]
    fn negation_reincludes() {
        let dir = tree("negate");
        std::fs::write(dir.join(".gitignore"), "*.rs\n!lib.rs\n").unwrap();
        let got = collect(&dir, &Options::default());
        assert!(got.contains(&"src/lib.rs".to_string()));
        assert!(!got.contains(&"main.rs".to_string()));
    }

    #[test]
    fn nested_ignore_file_is_scoped_to_its_directory() {
        let dir = tree("nested");
        std::fs::write(dir.join("src/.gitignore"), "deep/\n").unwrap();
        let got = collect(&dir, &Options::default());
        assert!(!got.contains(&"src/deep/mod.rs".to_string()));
        assert!(got.contains(&"main.rs".to_string()));
    }

    #[test]
    fn ignore_can_be_disabled() {
        let dir = tree("noignore");
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let opts = Options {
            respect_ignore: false,
            ..Options::default()
        };
        let got = collect(&dir, &opts);
        assert!(got.iter().any(|p| p.starts_with("target/")));
    }

    #[test]
    fn symlinks_are_not_followed() {
        let dir = tree("symlink");
        crate::platform::fixture::symlink("/etc", dir.join("etc")).unwrap();
        let got = collect(&dir, &Options::default());
        assert!(!got.iter().any(|p| p.starts_with("etc")));
    }

    #[test]
    fn max_entries_truncates_instead_of_running_away() {
        let dir = tree("cap");
        let opts = Options {
            max_entries: 2,
            ..Options::default()
        };
        let mut n = 0;
        let stats = walk(&dir, &opts, |_| {
            n += 1;
            true
        })
        .unwrap();
        assert_eq!(n, 2);
        assert!(stats.truncated);
    }

    #[test]
    fn hidden_files_can_be_excluded() {
        let dir = tree("hidden");
        std::fs::write(dir.join(".env.sample"), b"x").unwrap();
        let all = collect(&dir, &Options::default());
        assert!(all.contains(&".env.sample".to_string()));
        let opts = Options {
            hidden: false,
            ..Options::default()
        };
        let visible = collect(&dir, &opts);
        assert!(!visible.contains(&".env.sample".to_string()));
    }
}
