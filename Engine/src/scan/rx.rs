//! A small backtracking regex engine.
//!
//! Why not the `regex` crate: it is excellent and it is large — a DFA/NFA hybrid
//! with SIMD literal scanning and its own dependency tree, built for scanning
//! gigabytes. `search_text` scans a project's source files line by line, and the
//! patterns an agent writes are simple. This is ~300 lines, has no dependencies,
//! and is bounded so a pathological pattern cannot hang a turn.
//!
//! Supported: literals, `.`, `[...]` with ranges and negation, `\d \w \s` (and
//! negations), escapes, `* + ? {m,n}` greedy or lazy, `|`, `(...)`, `^`, `$`,
//! and a case-insensitive mode.
//!
//! Not supported, on purpose: capture extraction, backreferences, lookaround.
//! A search tool needs to answer "where does this match", nothing more.
//!
//! # Why the matcher does not call itself
//!
//! This was a continuation-passing backtracker: `m()` called itself through
//! `concat()` and `repeat()`, and each repetition of a `*` added a call frame.
//! The budget guarded *work*, and said nothing about *depth* — so the stack was
//! the unguarded resource, and it is the one that cannot be caught.
//!
//! `\w+` — the most ordinary pattern there is — died at 500 characters on the
//! 2 MiB stack a turn actually runs on, and at 4.000 on an 8 MiB one.
//! `search_text` reads files up to 8 MiB and matches every line whole (the
//! `max_line_chars` cut applies to *printing*, not matching), so one minified
//! JavaScript line or base64 blob aborted the entire process. Not an error
//! message: `SIGABRT`, and the session with it. `panic::catch` cannot help —
//! a stack overflow is a signal, not a panic, so no handler of ours ever runs.
//!
//! So the pattern is now **compiled to instructions and executed by a flat
//! loop** over a heap stack. Depth is bounded by the *pattern* — a pattern is
//! one line the agent typed — instead of by the *line being searched*, which is
//! arbitrary and can be a megabyte. The same work is still counted against the
//! same `scan.regex_steps` budget, which is what it was always for.

use crate::error::{Error, Result};

/// The parsed shape of a pattern. Kept because the literal fast path reads it
/// directly, and because compiling it is a walk of a tree bounded by the
/// pattern's own length.
#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
    Start,
    End,
}

/// One instruction of the compiled pattern.
///
/// `Split` and `Jmp` are what make backtracking possible without recursion: a
/// `Split` records the alternative it did not take, and that record is how a
/// pattern backs up instead of calling itself.
#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
    Start,
    End,
    /// Try `first`; if that whole path fails, come back and try `second`.
    Split(usize, usize),
    Jmp(usize),
    /// The pattern matched here. The engine stops and reports this position.
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    max_steps: u32,
    icase: bool,
    /// Literal alternatives fast path — matched without the backtracker at all.
    literals: Option<Vec<Vec<char>>>,
}

impl Regex {
    pub fn new(pattern: &str, icase: bool) -> Result<Regex> {
        if pattern.is_empty() {
            return Err(Error::invalid("empty pattern"));
        }
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            icase,
        };
        let root = p.parse_alt()?;
        if p.pos != p.chars.len() {
            return Err(Error::invalid(format!(
                "unexpected `{}` at character {}",
                p.chars[p.pos],
                p.pos + 1
            )));
        }
        let literals = literals_of(&root, icase);
        let mut prog = Vec::new();
        root.compile(&mut prog);
        prog.push(Inst::Match);
        Ok(Regex {
            prog,
            icase,
            literals,
            max_steps: crate::settings::configuration::Configuration::default()
                .number("scan.regex_steps") as u32,
        })
    }

    pub fn with_step_budget(mut self, steps: u32) -> Self {
        self.max_steps = steps;
        self
    }

    /// A pattern with no metacharacters, ready for a plain substring scan.
    pub fn is_literal(&self) -> bool {
        self.literals.as_ref().is_some_and(|l| l.len() == 1)
    }

    /// First match in `hay`, as (start, end) **character** offsets.
    pub fn find(&self, hay: &str) -> Result<Option<(usize, usize)>> {
        let text: Vec<char> = if self.icase {
            hay.chars().flat_map(|c| c.to_lowercase()).collect()
        } else {
            hay.chars().collect()
        };

        if let Some(lits) = &self.literals {
            return Ok(find_any_literal(&text, lits));
        }

        // The budget is for **one attempt at one line**, not for the whole file.
        //
        // It used to be created once here and shared by every start position
        // below. A line of 1.000 characters then cost the pattern a quadratic
        // number of steps, all charged to a single allowance, which is how
        // `.*Exception` came to be rejected as "too complex" on a line that is
        // comfortably inside what `search_text` itself allows. One allowance
        // per start position is also the honest unit: it is what it takes to
        // answer "does this pattern match *here*".
        for start in 0..=text.len() {
            let mut left = self.max_steps;
            if let Some(end) = run(&self.prog, &text, start, &mut left)? {
                return Ok(Some((start, end)));
            }
        }
        Ok(None)
    }

    pub fn is_match(&self, hay: &str) -> Result<bool> {
        Ok(self.find(hay)?.is_some())
    }
}

fn find_any_literal(text: &[char], lits: &[Vec<char>]) -> Option<(usize, usize)> {
    if lits.is_empty() || text.is_empty() {
        return None;
    }
    for i in 0..=text.len() {
        for lit in lits {
            if !lit.is_empty()
                && lit.len() <= text.len()
                && i <= text.len() - lit.len()
                && text[i..i + lit.len()] == *lit
            {
                return Some((i, i + lit.len()));
            }
        }
    }
    None
}

fn literals_of(node: &Node, icase: bool) -> Option<Vec<Vec<char>>> {
    match node {
        Node::Alt(branches) => {
            let mut list = Vec::with_capacity(branches.len());
            for b in branches {
                let lit = literal_of(b, icase)?;
                if !lit.is_empty() {
                    list.push(lit);
                }
            }
            if list.is_empty() {
                None
            } else {
                Some(list)
            }
        }
        other => literal_of(other, icase).map(|l| vec![l]),
    }
}

fn literal_of(node: &Node, _icase: bool) -> Option<Vec<char>> {
    match node {
        Node::Char(c) => Some(vec![*c]),
        Node::Concat(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for p in parts {
                match p {
                    Node::Char(c) => out.push(*c),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Execute `prog` from `start` on a heap stack, returning where it matched.
///
/// # Why this is a loop and never a recursion
///
/// Each entry of `stack` is one *alternative still to be tried*, not one
/// nested call. Backtracking still happens — `Split` pushes the branch it
/// deferred and execution carries on with the other one — but nothing here is
/// nested inside anything else, so the memory this function needs grows with
/// the number of pending alternatives and never with the length of the input.
///
/// That is the whole point: the text being searched is arbitrary and can be a
/// megabyte on one line, while the pattern is one line the agent typed. Depth
/// belongs to the pattern. See the module header for what the alternative used
/// to cost.
fn run(prog: &[Inst], text: &[char], start: usize, left: &mut u32) -> Result<Option<usize>> {
    // (instruction index, index into text)
    let mut stack: Vec<(usize, usize)> = vec![(0, start)];
    while let Some((mut pc, mut at)) = stack.pop() {
        loop {
            if *left == 0 {
                // `Error::tool`, not `Error::invalid`: the pattern is valid and
                // parsed fine — it simply cost more than the line was worth.
                // Rendering that as "invalid" told the reader their own pattern
                // was malformed, which is the one conclusion they would not have
                // drawn and the one they could not act on.
                return Err(Error::tool(
                    "pattern is too expensive for this line: it used all \
                     scan.regex_steps steps backtracking. Simplify the \
                     pattern, or search a smaller file.",
                ));
            }
            *left -= 1;
            match prog.get(pc) {
                None => break,
                Some(Inst::Char(c)) => {
                    if at < text.len() && text[at] == *c {
                        pc += 1;
                        at += 1;
                    } else {
                        break;
                    }
                }
                Some(Inst::Any) => {
                    if at < text.len() && text[at] != '\n' {
                        pc += 1;
                        at += 1;
                    } else {
                        break;
                    }
                }
                Some(Inst::Class { ranges, negated }) => {
                    if at >= text.len() {
                        break;
                    }
                    let c = text[at];
                    let inside = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi);
                    if inside != *negated {
                        pc += 1;
                        at += 1;
                    } else {
                        break;
                    }
                }
                Some(Inst::Start) => {
                    if at != 0 {
                        break;
                    }
                    pc += 1;
                }
                Some(Inst::End) => {
                    if at != text.len() {
                        break;
                    }
                    pc += 1;
                }
                // `first` runs now, `second` is remembered. A repeat that
                // consumed nothing would otherwise defer the very position it
                // is standing on, forever, so an unchanged position is not
                // worth a second visit.
                Some(Inst::Split(first, second)) => {
                    if at != start_of_deferred(&stack) {
                        stack.push((*second, at));
                    }
                    pc = *first;
                }
                Some(Inst::Jmp(there)) => pc = *there,
                Some(Inst::Match) => return Ok(Some(at)),
            }
        }
    }
    Ok(None)
}

/// The position a deferred branch is waiting on, if the top of the stack is one.
///
/// There is a real ambiguity here that the old recursion resolved for free:
/// `(a*)*` can reach a position many ways and must not be retried from the
/// same spot, or it never finishes. This compares against the newest deferred
/// branch only, which is the one a zero-width repeat produces — the one that
/// has just failed and would be re-tried at exactly the same place.
fn start_of_deferred(stack: &[(usize, usize)]) -> usize {
    stack.last().map(|(_, p)| *p).unwrap_or(usize::MAX)
}

/// Flatten a parsed pattern into instructions.
///
/// A walk of a tree whose depth is the *pattern's* nesting — a handful of
/// frames for anything a person would type. This is the only recursion left in
/// the file, and it is bounded by the size of the pattern rather than by the
/// size of the file being searched.
impl Node {
    fn compile(&self, out: &mut Vec<Inst>) {
        match self {
            Node::Char(c) => out.push(Inst::Char(*c)),
            Node::Any => out.push(Inst::Any),
            Node::Class { ranges, negated } => out.push(Inst::Class {
                ranges: ranges.clone(),
                negated: *negated,
            }),
            Node::Start => out.push(Inst::Start),
            Node::End => out.push(Inst::End),
            Node::Concat(parts) => {
                for p in parts {
                    p.compile(out);
                }
            }
            Node::Alt(branches) => {
                // Each branch but the last jumps over the ones after it.
                let mut jumps = Vec::new();
                for (i, b) in branches.iter().enumerate() {
                    if i + 1 < branches.len() {
                        let split = out.len();
                        out.push(Inst::Split(0, 0)); // patched below
                        let body = out.len();
                        b.compile(out);
                        jumps.push(out.len());
                        out.push(Inst::Jmp(0)); // patched below
                        let next = out.len();
                        out[split] = Inst::Split(body, next);
                    } else {
                        b.compile(out);
                    }
                }
                let end = out.len();
                for j in jumps {
                    out[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => {
                for _ in 0..*min {
                    node.compile(out);
                }
                match max {
                    None => {
                        // `x*` / `x+` tail: try another copy, or fall out.
                        let split = out.len();
                        out.push(Inst::Split(0, 0));
                        let body = out.len();
                        node.compile(out);
                        out.push(Inst::Jmp(split));
                        let after = out.len();
                        // Greedy tries the body first; lazy skips it first.
                        out[split] = if *greedy {
                            Inst::Split(body, after)
                        } else {
                            Inst::Split(after, body)
                        };
                    }
                    Some(high) => {
                        // `x{m,n}`: m copies are required, the rest optional.
                        let mut splits = Vec::new();
                        for _ in *min..*high {
                            let split = out.len();
                            out.push(Inst::Split(0, 0));
                            let body = out.len();
                            node.compile(out);
                            splits.push((split, body));
                        }
                        let after = out.len();
                        // Each optional copy falls through to the next
                        // decision, and the last one to the end of the pattern.
                        for (i, (split, body)) in splits.iter().enumerate() {
                            let next = splits.get(i + 1).map(|(s, _)| *s).unwrap_or(after);
                            out[*split] = if *greedy {
                                Inst::Split(*body, next)
                            } else {
                                Inst::Split(next, *body)
                            };
                        }
                    }
                }
            }
        }
    }
}

// ─── Parser ───────────────────────────────────────────────────────────────────

struct Parser {
    chars: Vec<char>,
    pos: usize,
    icase: bool,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn fold(&self, c: char) -> char {
        if self.icase {
            c.to_lowercase().next().unwrap_or(c)
        } else {
            c
        }
    }

    fn parse_alt(&mut self) -> Result<Node> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.bump();
            branches.push(self.parse_concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Node::Alt(branches)
        })
    }

    fn parse_concat(&mut self) -> Result<Node> {
        let mut parts = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            parts.push(self.parse_repeat()?);
        }
        Ok(match parts.len() {
            0 => Node::Concat(Vec::new()),
            1 => parts.pop().unwrap(),
            _ => Node::Concat(parts),
        })
    }

    fn parse_repeat(&mut self) -> Result<Node> {
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.bump();
                (0, None)
            }
            Some('+') => {
                self.bump();
                (1, None)
            }
            Some('?') => {
                self.bump();
                (0, Some(1))
            }
            Some('{') => match self.parse_bounds()? {
                Some(b) => b,
                None => return Ok(atom),
            },
            _ => return Ok(atom),
        };
        let greedy = if self.peek() == Some('?') {
            self.bump();
            false
        } else {
            true
        };
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    /// `{m}`, `{m,}`, `{m,n}`. A `{` that is not a valid bound is a literal brace,
    /// which is what every editor's search box does too.
    fn parse_bounds(&mut self) -> Result<Option<(u32, Option<u32>)>> {
        let save = self.pos;
        self.bump(); // '{'
        let mut lo = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            lo.push(self.bump().unwrap());
        }
        if lo.is_empty() {
            self.pos = save;
            return Ok(None);
        }
        let min: u32 = lo
            .parse()
            .map_err(|_| Error::invalid("repeat count too large"))?;
        match self.peek() {
            Some('}') => {
                self.bump();
                Ok(Some((min, Some(min))))
            }
            Some(',') => {
                self.bump();
                let mut hi = String::new();
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    hi.push(self.bump().unwrap());
                }
                if self.peek() != Some('}') {
                    self.pos = save;
                    return Ok(None);
                }
                self.bump();
                let max = if hi.is_empty() {
                    None
                } else {
                    Some(
                        hi.parse()
                            .map_err(|_| Error::invalid("repeat count too large"))?,
                    )
                };
                if let Some(mx) = max {
                    if mx < min {
                        return Err(Error::invalid("repeat range is inverted"));
                    }
                }
                Ok(Some((min, max)))
            }
            _ => {
                self.pos = save;
                Ok(None)
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Node> {
        match self.bump() {
            None => Err(Error::invalid("pattern ends early")),
            Some('(') => {
                let open = self.pos;
                // `(?:` is accepted and means the same thing as `(` here — we
                // never capture.
                if self.peek() == Some('?') {
                    let save = self.pos;
                    self.bump();
                    if self.peek() == Some(':') {
                        self.bump();
                    } else {
                        self.pos = save;
                    }
                }
                let inner = self.parse_alt()?;
                if self.bump() != Some(')') {
                    return Err(Error::invalid(format!(
                        "`(` at character {open} is never closed"
                    )));
                }
                Ok(inner)
            }
            Some(')') => Err(Error::invalid(format!(
                "`)` at character {} closes nothing",
                self.pos
            ))),
            Some('[') => self.parse_class(),
            Some('.') => Ok(Node::Any),
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('\\') => {
                let c = self
                    .bump()
                    .ok_or_else(|| Error::invalid("pattern ends after `\\`"))?;
                Ok(escape_node(c, self.icase))
            }
            Some(c) => Ok(Node::Char(self.fold(c))),
        }
    }

    fn parse_class(&mut self) -> Result<Node> {
        let open = self.pos;
        let mut negated = false;
        if self.peek() == Some('^') {
            self.bump();
            negated = true;
        }
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let c = match self.bump() {
                None => {
                    return Err(Error::invalid(format!(
                        "`[` at character {open} is never closed"
                    )))
                }
                Some(']') if !first => break,
                Some(c) => c,
            };
            first = false;

            let lo = if c == '\\' {
                let e = self
                    .bump()
                    .ok_or_else(|| Error::invalid("pattern ends after `\\`"))?;
                match escape_node(e, self.icase) {
                    Node::Class {
                        ranges: mut r,
                        negated: false,
                    } => {
                        ranges.append(&mut r);
                        continue;
                    }
                    Node::Char(ch) => ch,
                    // `[\D]` and friends: fold the negated class in as its own
                    // complement is not expressible here, so treat conservatively
                    // as "any char" minus nothing.
                    _ => {
                        return Err(Error::invalid(
                            "negated class inside `[...]` is not supported",
                        ))
                    }
                }
            } else {
                c
            };

            if self.peek() == Some('-')
                && self
                    .chars
                    .get(self.pos + 1)
                    .copied()
                    .map(|n| n != ']')
                    .unwrap_or(false)
            {
                self.bump();
                let hi_raw = self.bump().unwrap();
                let hi = if hi_raw == '\\' {
                    self.bump()
                        .ok_or_else(|| Error::invalid("pattern ends after `\\`"))?
                } else {
                    hi_raw
                };
                if hi < lo {
                    return Err(Error::invalid("character range is inverted"));
                }
                ranges.push((self.fold(lo), self.fold(hi)));
                if self.icase {
                    ranges.push((lo, hi));
                }
            } else {
                let f = self.fold(lo);
                ranges.push((f, f));
                if self.icase && f != lo {
                    ranges.push((lo, lo));
                }
            }
        }
        if ranges.is_empty() {
            return Err(Error::invalid("empty character class"));
        }
        Ok(Node::Class { ranges, negated })
    }
}

fn escape_node(c: char, icase: bool) -> Node {
    match c {
        'd' => Node::Class {
            ranges: vec![('0', '9')],
            negated: false,
        },
        'D' => Node::Class {
            ranges: vec![('0', '9')],
            negated: true,
        },
        'w' => Node::Class {
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: false,
        },
        'W' => Node::Class {
            ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: true,
        },
        's' => Node::Class {
            ranges: vec![
                (' ', ' '),
                ('\t', '\t'),
                ('\n', '\n'),
                ('\r', '\r'),
                ('\x0b', '\x0c'),
            ],
            negated: false,
        },
        'S' => Node::Class {
            ranges: vec![
                (' ', ' '),
                ('\t', '\t'),
                ('\n', '\n'),
                ('\r', '\r'),
                ('\x0b', '\x0c'),
            ],
            negated: true,
        },
        'n' => Node::Char('\n'),
        't' => Node::Char('\t'),
        'r' => Node::Char('\r'),
        '0' => Node::Char('\0'),
        other => Node::Char(if icase {
            other.to_lowercase().next().unwrap_or(other)
        } else {
            other
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, hay: &str) -> bool {
        Regex::new(pat, false).unwrap().is_match(hay).unwrap()
    }

    #[test]
    fn literal_fast_path() {
        let r = Regex::new("hello", false).unwrap();
        assert!(r.is_literal());
        assert_eq!(r.find("say hello there").unwrap(), Some((4, 9)));
        assert_eq!(r.find("nope").unwrap(), None);
    }

    #[test]
    fn literal_alternation_fast_path() {
        let r = Regex::new("max_output_size|max_file_size", false).unwrap();
        assert_eq!(r.find("let max_file_size = 10;").unwrap(), Some((4, 17)));
        assert_eq!(r.find("let max_output_size = 20;").unwrap(), Some((4, 19)));
        assert_eq!(r.find("nothing here").unwrap(), None);

        // Case insensitive literal alternation
        let r_icase = Regex::new("Alpha|Beta", true).unwrap();
        assert_eq!(r_icase.find("hello beta world").unwrap(), Some((6, 10)));
        assert_eq!(r_icase.find("ALPHA is here").unwrap(), Some((0, 5)));
    }

    #[test]
    fn metachars_disable_literal_path() {
        assert!(!Regex::new("hel.o", false).unwrap().is_literal());
        assert!(!Regex::new("a+", false).unwrap().is_literal());
    }

    #[test]
    fn dot_plus_star_question() {
        assert!(m("a.c", "abc"));
        assert!(!m("a.c", "ac"));
        assert!(m("ab*c", "ac"));
        assert!(m("ab*c", "abbbc"));
        assert!(m("ab+c", "abc"));
        assert!(!m("ab+c", "ac"));
        assert!(m("ab?c", "ac"));
    }

    #[test]
    fn classes() {
        assert!(m("[abc]x", "bx"));
        assert!(!m("[abc]x", "dx"));
        assert!(m("[^abc]x", "dx"));
        assert!(m("[a-f0-9]+", "beef42"));
        assert!(m(r"\d{3}", "abc 456"));
        assert!(!m(r"^\d{3}$", "45"));
        assert!(m(r"\w+", "hello_9"));
        assert!(m(r"a\sb", "a b"));
    }

    #[test]
    fn anchors() {
        assert!(m("^fn ", "fn main()"));
        assert!(!m("^fn ", "pub fn main()"));
        assert!(m("\\}$", "}"));
    }

    #[test]
    fn alternation_and_groups() {
        assert!(m("cat|dog", "hotdog"));
        assert!(m("(ab)+c", "ababc"));
        assert!(m("(?:foo|bar)baz", "barbaz"));
        assert!(!m("(?:foo|bar)baz", "bazbaz"));
    }

    #[test]
    fn bounds() {
        assert!(m("a{2,3}b", "aab"));
        assert!(m("a{2,3}b", "aaab"));
        assert!(!m("^a{2,3}b$", "ab"));
        assert!(m("a{2}", "aa"));
        // A brace that is not a bound stays literal.
        assert!(m("x{y", "x{y"));
    }

    #[test]
    fn lazy_repeat_stops_early() {
        let r = Regex::new("<.+?>", false).unwrap();
        assert_eq!(r.find("<a><b>").unwrap(), Some((0, 3)));
        let g = Regex::new("<.+>", false).unwrap();
        assert_eq!(g.find("<a><b>").unwrap(), Some((0, 6)));
    }

    #[test]
    fn case_insensitive() {
        let r = Regex::new("Hello", true).unwrap();
        assert!(r.is_match("say HELLO").unwrap());
        let c = Regex::new("[a-z]+", true).unwrap();
        assert!(c.is_match("ABC").unwrap());
    }

    #[test]
    fn offsets_are_char_based() {
        let r = Regex::new("b", false).unwrap();
        assert_eq!(r.find("äb").unwrap(), Some((1, 2)));
    }

    #[test]
    fn catastrophic_pattern_is_bounded_not_hung() {
        let r = Regex::new("(a+)+$", false).unwrap();
        let hay = "a".repeat(40) + "!";
        assert!(r.is_match(&hay).is_err());
    }

    #[test]
    fn bad_patterns_are_rejected() {
        assert!(Regex::new("", false).is_err());
        assert!(Regex::new("(unclosed", false).is_err());
        assert!(Regex::new("[unclosed", false).is_err());
        assert!(Regex::new("a)", false).is_err());
        assert!(Regex::new("[z-a]", false).is_err());
    }

    #[test]
    fn zero_width_repeat_terminates() {
        let r = Regex::new("(a*)*b", false).unwrap();
        assert!(r.is_match("aab").unwrap());
        assert!(matches!(r.is_match("aac"), Ok(false) | Err(_)));
    }
}
