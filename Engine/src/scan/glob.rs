//! Path glob matching.
//!
//! `*` and `?` stop at a `/`; `**` is the only thing that crosses directory
//! boundaries. Character classes work as in a shell: `[abc]`, `[a-z]`, `[!abc]`.
//!
//! Written here rather than pulled in because the whole job is one recursive
//! function over path segments, and both the `find_files` tool and the ignore
//! rules in `walk` need exactly this and nothing more.

/// A compiled pattern. Compiling is cheap; it exists so a walk does not re-split
/// the same pattern once per file.
#[derive(Debug, Clone)]
pub struct Glob {
    segs: Vec<Seg>,
    /// Pattern started with `/` — anchored to the base directory.
    pub anchored: bool,
    /// Pattern ended with `/` — matches directories only.
    pub dir_only: bool,
}

#[derive(Debug, Clone)]
enum Seg {
    /// `**`
    AnyDepth,
    Pattern(Vec<char>),
}

impl Glob {
    pub fn new(pattern: &str) -> Glob {
        let anchored = pattern.starts_with('/');
        let dir_only = pattern.ends_with('/');
        let core = pattern.trim_start_matches('/').trim_end_matches('/');
        let segs = core
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| {
                if s == "**" {
                    Seg::AnyDepth
                } else {
                    Seg::Pattern(s.chars().collect())
                }
            })
            .collect();
        Glob {
            segs,
            anchored,
            dir_only,
        }
    }

    /// True when the pattern has no `/` at all — those match a bare name at any
    /// depth, which is what `.gitignore` means by `node_modules`.
    pub fn is_bare_name(&self) -> bool {
        !self.anchored && self.segs.len() == 1 && !matches!(self.segs[0], Seg::AnyDepth)
    }

    /// Match a `/`-separated relative path.
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        segs_match(&self.segs, &parts)
    }
}

/// A path pattern split where a walk can start: the folders it names before
/// its first wildcard, and the rest, which is matched below them —
/// `src/**/*.rs` is (`src`, `**/*.rs`), `/etc/*.conf` is (`/etc`, `*.conf`),
/// `*.rs` is (``, `*.rs`). The last name is always part of the rest, since it
/// names what is looked for: `src/main.rs` starts in `src`.
///
/// A walk that starts there sees what a walk of the whole workspace filtered
/// by the pattern would, without going through every other folder first —
/// the difference between a search that answers and one that runs out of its
/// entry budget in `node_modules` before it reaches the folder it was asked
/// about.
pub fn split_walk_root(pattern: &str) -> (&str, &str) {
    let mut start = 0;
    let mut root_end = None;
    for (at, _) in pattern.match_indices('/') {
        let segment = &pattern[start..at];
        if is_wild(segment) {
            break;
        }
        // A leading `/` is the filesystem's root, itself a place to start.
        root_end = Some(if at == 0 { 1 } else { at });
        start = at + 1;
    }
    match root_end {
        None => ("", pattern),
        Some(end) => {
            let rest = pattern[end..].trim_start_matches('/');
            (&pattern[..end], rest)
        }
    }
}

/// Whether a segment holds anything [`Glob`] reads as other than itself.
fn is_wild(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '\\', '{', '}', '|'])
}

/// Whether a whole pattern is a path with no wildcard in it.
pub fn is_literal(pattern: &str) -> bool {
    !is_wild(pattern)
}

/// Maximum number of branches a brace pattern may expand to, guarding against
/// combinatorial denial of service.
pub const MAX_EXPANSIONS: usize = 256;

/// Expand bash/zsh-style brace expansions `{a,b,c}`, nested `{a,{b,c}}`, and
/// pipe alternations `a|b`.
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let mut initial = Vec::new();
    split_top_level_pipe(pattern, &mut initial);
    if initial.is_empty() {
        initial.push(pattern.to_string());
    }

    let mut results = Vec::new();
    for pat in initial {
        expand_one(&pat, &mut results);
    }
    dedup_preserve_order(results)
}

fn split_top_level_pipe(s: &str, out: &mut Vec<String>) {
    let mut depth = 0;
    let mut in_escape = false;
    let mut start = 0;
    let mut has_pipe = false;

    for (i, c) in s.char_indices() {
        if in_escape {
            in_escape = false;
            continue;
        }
        if c == '\\' {
            in_escape = true;
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' && depth > 0 {
            depth -= 1;
        } else if c == '|' && depth == 0 {
            has_pipe = true;
            let part = s[start..i].trim();
            if !part.is_empty() {
                out.push(part.to_string());
            }
            start = i + 1;
        }
    }
    if has_pipe {
        let part = s[start..].trim();
        if !part.is_empty() {
            out.push(part.to_string());
        }
    }
}

fn expand_one(s: &str, out: &mut Vec<String>) {
    if out.len() >= MAX_EXPANSIONS {
        return;
    }
    if let Some((open, close, options)) = find_first_brace(s) {
        let prefix = &s[..open];
        let suffix = &s[close + 1..];
        for opt in options {
            if out.len() >= MAX_EXPANSIONS {
                break;
            }
            let mut sub_opts = Vec::new();
            expand_one(&opt, &mut sub_opts);
            if sub_opts.is_empty() {
                sub_opts.push(opt);
            }
            for sub in sub_opts {
                if out.len() >= MAX_EXPANSIONS {
                    break;
                }
                let combined = format!("{prefix}{sub}{suffix}");
                expand_one(&combined, out);
            }
        }
    } else {
        out.push(s.to_string());
    }
}

fn find_first_brace(s: &str) -> Option<(usize, usize, Vec<String>)> {
    let mut in_escape = false;
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let (open_idx, c) = chars[i];
        if in_escape {
            in_escape = false;
            i += 1;
            continue;
        }
        if c == '\\' {
            in_escape = true;
            i += 1;
            continue;
        }

        if c == '{' {
            let mut depth = 1;
            let mut j = i + 1;
            let mut inner_in_escape = false;
            let mut options = Vec::new();
            let mut current = String::new();
            let mut has_sep = false;
            let mut closed_idx = None;

            while j < chars.len() {
                let (curr_idx, ch) = chars[j];
                if inner_in_escape {
                    inner_in_escape = false;
                    current.push(ch);
                    j += 1;
                    continue;
                }
                if ch == '\\' {
                    inner_in_escape = true;
                    current.push(ch);
                    j += 1;
                    continue;
                }

                if ch == '{' {
                    depth += 1;
                    current.push(ch);
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        options.push(current);
                        closed_idx = Some(curr_idx);
                        break;
                    } else {
                        current.push(ch);
                    }
                } else if (ch == ',' || ch == '|') && depth == 1 {
                    has_sep = true;
                    options.push(current);
                    current = String::new();
                } else {
                    current.push(ch);
                }
                j += 1;
            }

            if let Some(close_idx) = closed_idx {
                if has_sep {
                    return Some((open_idx, close_idx, options));
                }
                // No comma or pipe was found; this `{foo}` is treated as literal text.
                i = j;
            }
        }
        i += 1;
    }
    None
}

fn dedup_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// A set of compiled glob patterns. Matches if any glob in the set matches.
#[derive(Debug, Clone)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    pub fn new<I, S>(patterns: I) -> GlobSet
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let globs = patterns
            .into_iter()
            .map(|p| Glob::new(p.as_ref()))
            .collect();
        GlobSet { globs }
    }

    pub fn single(glob: Glob) -> GlobSet {
        GlobSet { globs: vec![glob] }
    }

    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|g| g.matches(path))
    }

    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.globs.len()
    }
}

/// Find the common directory prefix among multiple path patterns split where wildcards begin.
///
/// Returns `(common_root, relative_patterns)` where each relative pattern is the part of
/// the corresponding pattern that follows `common_root`.
pub fn common_walk_root(patterns: &[String]) -> (String, Vec<String>) {
    if patterns.is_empty() {
        return (String::new(), Vec::new());
    }
    if patterns.len() == 1 {
        let (root, rest) = split_walk_root(&patterns[0]);
        return (root.to_string(), vec![rest.to_string()]);
    }

    let mut root_segments: Vec<Vec<&str>> = Vec::with_capacity(patterns.len());
    let mut is_absolute = None;

    for pat in patterns {
        let (root, _) = split_walk_root(pat);
        let abs = root.starts_with('/');
        if let Some(prev_abs) = is_absolute {
            if prev_abs != abs {
                return (String::new(), patterns.to_vec());
            }
        } else {
            is_absolute = Some(abs);
        }

        let segs: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
        root_segments.push(segs);
    }

    let mut common_segs: Vec<&str> = Vec::new();
    if let Some(first) = root_segments.first() {
        for (idx, seg) in first.iter().enumerate() {
            if root_segments.iter().all(|r| r.get(idx) == Some(seg)) {
                common_segs.push(seg);
            } else {
                break;
            }
        }
    }

    let common_root = if common_segs.is_empty() {
        if is_absolute == Some(true) {
            "/".to_string()
        } else {
            String::new()
        }
    } else if is_absolute == Some(true) {
        format!("/{}", common_segs.join("/"))
    } else {
        common_segs.join("/")
    };

    let mut relative_patterns = Vec::with_capacity(patterns.len());
    for pat in patterns {
        let rel = if common_root.is_empty() || common_root == "/" {
            pat.trim_start_matches('/').to_string()
        } else if pat.starts_with(&common_root) {
            let after = &pat[common_root.len()..];
            after.trim_start_matches('/').to_string()
        } else {
            pat.clone()
        };
        relative_patterns.push(rel);
    }

    (common_root, relative_patterns)
}

fn segs_match(pat: &[Seg], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(Seg::AnyDepth) => {
            // `**` swallows zero or more segments.
            for skip in 0..=path.len() {
                if segs_match(&pat[1..], &path[skip..]) {
                    return true;
                }
            }
            false
        }
        Some(Seg::Pattern(p)) => {
            if path.is_empty() {
                return false;
            }
            let seg: Vec<char> = path[0].chars().collect();
            one_match(p, 0, &seg, 0) && segs_match(&pat[1..], &path[1..])
        }
    }
}

/// Wildcard match inside a single path segment.
fn one_match(pat: &[char], pi: usize, seg: &[char], si: usize) -> bool {
    if pi == pat.len() {
        return si == seg.len();
    }
    match pat[pi] {
        '*' => {
            for skip in si..=seg.len() {
                if one_match(pat, pi + 1, seg, skip) {
                    return true;
                }
            }
            false
        }
        '?' => si < seg.len() && one_match(pat, pi + 1, seg, si + 1),
        '[' => {
            if si >= seg.len() {
                return false;
            }
            match class_end(pat, pi) {
                None => seg.get(si) == Some(&'[') && one_match(pat, pi + 1, seg, si + 1),
                Some(end) => {
                    if class_hit(&pat[pi + 1..end], seg[si]) {
                        one_match(pat, end + 1, seg, si + 1)
                    } else {
                        false
                    }
                }
            }
        }
        '\\' if pi + 1 < pat.len() => {
            si < seg.len() && seg[si] == pat[pi + 1] && one_match(pat, pi + 2, seg, si + 1)
        }
        c => si < seg.len() && seg[si] == c && one_match(pat, pi + 1, seg, si + 1),
    }
}

fn class_end(pat: &[char], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if pat.get(i) == Some(&'!') || pat.get(i) == Some(&'^') {
        i += 1;
    }
    if pat.get(i) == Some(&']') {
        i += 1;
    }
    while i < pat.len() {
        if pat[i] == ']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn class_hit(body: &[char], c: char) -> bool {
    let (negated, body) = match body.first() {
        Some('!') | Some('^') => (true, &body[1..]),
        _ => (false, body),
    };
    let mut i = 0;
    let mut hit = false;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            if c >= body[i] && c <= body[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if body[i] == c {
                hit = true;
            }
            i += 1;
        }
    }
    hit != negated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str, path: &str) -> bool {
        Glob::new(p).matches(path)
    }

    #[test]
    fn star_stays_inside_one_segment() {
        assert!(g("*.rs", "main.rs"));
        assert!(!g("*.rs", "src/main.rs"));
        assert!(g("src/*.rs", "src/main.rs"));
        assert!(!g("src/*.rs", "src/a/main.rs"));
    }

    #[test]
    fn doublestar_crosses_segments() {
        assert!(g("**/*.rs", "src/deep/main.rs"));
        assert!(g("**/*.rs", "main.rs"));
        assert!(g("src/**/mod.rs", "src/a/b/mod.rs"));
        assert!(g("src/**/mod.rs", "src/mod.rs"));
        assert!(!g("src/**/mod.rs", "other/a/mod.rs"));
    }

    #[test]
    fn question_and_classes() {
        assert!(g("?.rs", "a.rs"));
        assert!(!g("?.rs", "ab.rs"));
        assert!(g("[abc]*.rs", "b1.rs"));
        assert!(!g("[abc]*.rs", "d1.rs"));
        assert!(g("[!abc]*.rs", "d1.rs"));
        assert!(g("[a-c]x", "bx"));
    }

    #[test]
    fn flags_are_parsed() {
        let anchored = Glob::new("/target/");
        assert!(anchored.anchored);
        assert!(anchored.dir_only);
        assert!(anchored.matches("target"));

        let bare = Glob::new("node_modules");
        assert!(bare.is_bare_name());
        assert!(!Glob::new("a/b").is_bare_name());
        assert!(!Glob::new("**").is_bare_name());
    }

    #[test]
    fn escaped_metachar_is_literal() {
        assert!(g("a\\*b", "a*b"));
        assert!(!g("a\\*b", "axb"));
    }

    #[test]
    fn empty_segments_are_ignored() {
        assert!(g("src//main.rs", "src/main.rs"));
        assert!(g("./main.rs", "main.rs"));
    }

    /// A walk starts in the folders a pattern names before its first
    /// wildcard; the name it looks for is always matched, never walked into.
    #[test]
    fn a_pattern_starts_its_walk_in_the_folders_it_names() {
        for (pattern, root, rest) in [
            ("*.rs", "", "*.rs"),
            ("Cargo.toml", "", "Cargo.toml"),
            ("src/main.rs", "src", "main.rs"),
            ("src/**/*.rs", "src", "**/*.rs"),
            ("Engine/src/tools/*.rs", "Engine/src/tools", "*.rs"),
            ("src/*/mod.rs", "src", "*/mod.rs"),
            ("**/mod.rs", "", "**/mod.rs"),
            ("/etc/*.conf", "/etc", "*.conf"),
            ("/*.conf", "/", "*.conf"),
            ("/home/me/proj/src/**/x.rs", "/home/me/proj/src", "**/x.rs"),
            ("~/notes/**/*.md", "~/notes", "**/*.md"),
            ("~/x.md", "~", "x.md"),
            (".rune/skills/**/SKILL.md", ".rune/skills", "**/SKILL.md"),
            ("a[bc]/d.rs", "", "a[bc]/d.rs"),
            ("src//main.rs", "src/", "main.rs"),
        ] {
            assert_eq!(split_walk_root(pattern), (root, rest), "{pattern}");
        }
        assert!(is_literal("src/main.rs"));
        assert!(!is_literal("src/*.rs"));
        assert!(!is_literal("a\\*b"));
        assert!(!is_literal("src/{a,b}.rs"));
        assert!(!is_literal("a|b"));
    }

    #[test]
    fn brace_expansion_simple_and_nested() {
        assert_eq!(expand_braces("*.{ts,tsx}"), vec!["*.ts", "*.tsx"]);
        assert_eq!(expand_braces("{a,{b,c}}"), vec!["a", "b", "c"]);
        assert_eq!(
            expand_braces("src/{client,server/{api,db}}/**/*.rs"),
            vec![
                "src/client/**/*.rs",
                "src/server/api/**/*.rs",
                "src/server/db/**/*.rs"
            ]
        );
        assert_eq!(expand_braces("foo{,.bak}"), vec!["foo", "foo.bak"]);
        assert_eq!(expand_braces("{single}"), vec!["{single}"]);
        assert_eq!(expand_braces("unmatched{brace"), vec!["unmatched{brace"]);
    }

    #[test]
    fn brace_expansion_cartesian_and_real_session_714_patterns() {
        // Pattern 7
        assert_eq!(
            expand_braces("{WebsiteDev.sh,Studio/WebsiteDev.sh,Cloud/SeaDrive/WebsiteDev.sh}"),
            vec![
                "WebsiteDev.sh",
                "Studio/WebsiteDev.sh",
                "Cloud/SeaDrive/WebsiteDev.sh"
            ]
        );

        // Pattern 11: 3 x 4 = 12 combinations
        let p11 = expand_braces(
            "Cloud/SeaDrive/docs/Current/{Desktop,Mobile,Backend}/**/*{Deploy,Port,Build,Release}*.md",
        );
        assert_eq!(p11.len(), 12);
        assert_eq!(p11[0], "Cloud/SeaDrive/docs/Current/Desktop/**/*Deploy*.md");
        assert_eq!(
            p11[3],
            "Cloud/SeaDrive/docs/Current/Desktop/**/*Release*.md"
        );
        assert_eq!(p11[4], "Cloud/SeaDrive/docs/Current/Mobile/**/*Deploy*.md");
        assert_eq!(
            p11[11],
            "Cloud/SeaDrive/docs/Current/Backend/**/*Release*.md"
        );

        // Pattern 12: heterogeneous paths
        let p12 = expand_braces(
            "Cloud/SeaDrive/{DeploySeaDrive.sh,scripts/*.sh,Application/Desktop/**/*.sh,Application/Mobile/**/*.sh}",
        );
        assert_eq!(
            p12,
            vec![
                "Cloud/SeaDrive/DeploySeaDrive.sh",
                "Cloud/SeaDrive/scripts/*.sh",
                "Cloud/SeaDrive/Application/Desktop/**/*.sh",
                "Cloud/SeaDrive/Application/Mobile/**/*.sh"
            ]
        );

        // Pipe alternations
        assert_eq!(expand_braces("*.ts|*.js"), vec!["*.ts", "*.js"]);
        assert_eq!(expand_braces("*.{ts|tsx}"), vec!["*.ts", "*.tsx"]);
    }

    #[test]
    fn glob_set_matches_any_branch() {
        let set = GlobSet::new(["*.ts", "src/**/*.rs", "package.json"]);
        assert!(set.matches("index.ts"));
        assert!(set.matches("src/main.rs"));
        assert!(set.matches("package.json"));
        assert!(!set.matches("index.js"));
        assert!(!set.matches("other/main.rs"));
    }

    #[test]
    fn common_walk_root_calculation() {
        let p1 = vec![
            "Cloud/SeaDrive/docs/Current/Desktop/**/*.md".to_string(),
            "Cloud/SeaDrive/docs/Current/Mobile/**/*.md".to_string(),
            "Cloud/SeaDrive/docs/Current/Backend/**/*.md".to_string(),
        ];
        let (root, rels) = common_walk_root(&p1);
        assert_eq!(root, "Cloud/SeaDrive/docs/Current");
        assert_eq!(
            rels,
            vec!["Desktop/**/*.md", "Mobile/**/*.md", "Backend/**/*.md"]
        );

        let p2 = vec![
            "WebsiteDev.sh".to_string(),
            "Studio/WebsiteDev.sh".to_string(),
            "Cloud/SeaDrive/WebsiteDev.sh".to_string(),
        ];
        let (root, rels) = common_walk_root(&p2);
        assert_eq!(root, "");
        assert_eq!(
            rels,
            vec![
                "WebsiteDev.sh",
                "Studio/WebsiteDev.sh",
                "Cloud/SeaDrive/WebsiteDev.sh"
            ]
        );
    }

    #[test]
    fn brace_expansion_stress_and_edge_cases() {
        // Deep nesting (4 levels)
        assert_eq!(
            expand_braces("{a,{b,{c,{d,e}}}}"),
            vec!["a", "b", "c", "d", "e"]
        );

        // Cartesian product stress: 2 * 2 * 3 * 2 = 24 branches
        let stress = expand_braces("{Engine,Cli}/{src,tests}/{tools,scan,ui}/*.{rs,toml}");
        assert_eq!(stress.len(), 24);
        assert!(stress.contains(&"Engine/src/tools/*.rs".to_string()));
        assert!(stress.contains(&"Cli/tests/ui/*.toml".to_string()));

        // DoS protection: {0,1}^10 would be 1024 branches; capped at MAX_EXPANSIONS (256)
        let dos = expand_braces("{0,1}{0,1}{0,1}{0,1}{0,1}{0,1}{0,1}{0,1}{0,1}{0,1}");
        assert_eq!(dos.len(), MAX_EXPANSIONS);

        // Complex extensions and symbols
        assert_eq!(
            expand_braces("archive.{tar.gz,tar.bz2,zip,7z}"),
            vec![
                "archive.tar.gz",
                "archive.tar.bz2",
                "archive.zip",
                "archive.7z"
            ]
        );

        // Empty options in middle, start, and end
        assert_eq!(
            expand_braces("test{,-old,.bak}"),
            vec!["test", "test-old", "test.bak"]
        );
        assert_eq!(expand_braces("{,pre-}hook"), vec!["hook", "pre-hook"]);
        assert_eq!(
            expand_braces("a{,,b}c"),
            vec!["ac", "abc"] // deduped
        );

        // Absolute path with braces
        let abs = expand_braces("/{etc,var/log}/**/*.{conf,log}");
        assert_eq!(
            abs,
            vec![
                "/etc/**/*.conf",
                "/etc/**/*.log",
                "/var/log/**/*.conf",
                "/var/log/**/*.log"
            ]
        );
        let (abs_root, abs_rels) = common_walk_root(&abs);
        assert_eq!(abs_root, "/");
        assert_eq!(
            abs_rels,
            vec![
                "etc/**/*.conf",
                "etc/**/*.log",
                "var/log/**/*.conf",
                "var/log/**/*.log"
            ]
        );

        // Multi-branch GlobSet matching against real paths
        let set = GlobSet::new(abs_rels);
        assert!(set.matches("etc/nginx/nginx.conf"));
        assert!(set.matches("etc/systemd/system.conf"));
        assert!(set.matches("var/log/syslog.log"));
        assert!(!set.matches("usr/bin/bash"));
        assert!(!set.matches("etc/nginx/nginx.txt"));
    }
}
