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
}
