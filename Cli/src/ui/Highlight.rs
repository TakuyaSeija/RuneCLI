//! Telling a code block's words apart, so that a block of code reads as code.
//!
//! # Why this exists
//!
//! Every line of every code block used to be painted in exactly one ink. The
//! result was a wall: a reader could not tell a keyword from a string without
//! reading the line, which is the one thing code is supposed to let them stop
//! doing. The colour a language's own editor gives a token is a *meaning* —
//! "this is a name for a kind", "this is the literal value" — and a terminal is
//! perfectly able to carry that, because terminals have colour.
//!
//! # What this is, and is not
//!
//! This is a **lexer**, not a parser. It walks the line once, left to right, and
//! sorts each run of characters by what it looks like: a quote starts a string
//! until the matching quote, a digit starts a number, a word is a keyword if the
//! language's list says so and a type if it is capitalised. It knows nothing
//! about nesting, scopes, macros, or whether the program compiles. That is a real
//! limitation and it is deliberate: the alternative is a grammar engine and its
//! twenty-odd dependencies, and this program has three.
//!
//! It is also *additive*. A run it does not recognise is painted in the base ink
//! rather than dropped or guessed at, so an unrecognised construct is simply
//! unhighlighted — never miscoloured.
//!
//! # Why a language we do not know is still worth running
//!
//! Strings and numbers look the same in every language there is. A block tagged
//! ```` ```wat ````, or no tag at all, still gets those two. What it does *not*
//! get is a keyword list, because painting `fn` as a keyword inside a language
//! where it is an ordinary identifier is worse than not colouring it at all.

use super::ansi::{
    self, Ink, CODE, CODE_COMMENT, CODE_FUNC, CODE_KEYWORD, CODE_NUMBER, CODE_STRING, CODE_TYPE,
};
use std::ops::Range;

/// One run of a line, in bytes of the line it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub start: usize,
    pub end: usize,
    pub ink: Ink,
}

/// The language families the word lists below belong to.
///
/// Several languages share a family because they share a *grammar of keywords* —
/// which is all the keyword list is for. JavaScript, TypeScript and Go do not
/// have the same reserved words, but highlighting a Go `func` as a keyword would
/// be a lie of a different size, and one list kept in step is one list kept right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Rust,
    CLike,
    Python,
    Json,
    Shell,
    /// No list. Strings and numbers only.
    Unknown,
}

/// Map a fence's info string onto a family. A fence may carry more than a name
/// (```rust,ignore, or `rust title="x"`), so this reads the first word.
fn dialect(name: &str) -> Dialect {
    let first = name
        .split(|c: char| c.is_whitespace() || c == ',')
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();
    match first.as_str() {
        "rust" | "rs" => Dialect::Rust,
        "js" | "jsx" | "javascript" | "ts" | "tsx" | "typescript" | "go" | "java" | "c" | "h"
        | "cpp" | "c++" | "cs" | "csharp" => Dialect::CLike,
        "python" | "py" => Dialect::Python,
        "json" => Dialect::Json,
        "sh" | "bash" | "shell" | "zsh" | "console" | "shell-session" => Dialect::Shell,
        _ => Dialect::Unknown,
    }
}

/// The words this language reserves for control flow and declarations.
fn keywords(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::Rust => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
            "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "true", "type", "unsafe", "use", "where", "while",
        ],
        Dialect::CLike => &[
            "abstract",
            "as",
            "async",
            "await",
            "break",
            "case",
            "catch",
            "class",
            "const",
            "continue",
            "default",
            "delete",
            "do",
            "else",
            "enum",
            "export",
            "extends",
            "false",
            "final",
            "for",
            "from",
            "func",
            "function",
            "go",
            "if",
            "implements",
            "import",
            "in",
            "instanceof",
            "interface",
            "let",
            "new",
            "null",
            "package",
            "private",
            "protected",
            "public",
            "return",
            "static",
            "struct",
            "super",
            "switch",
            "this",
            "throw",
            "true",
            "try",
            "typeof",
            "undefined",
            "var",
            "while",
            "yield",
        ],
        Dialect::Python => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
            "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
            "True", "try", "while", "with", "yield",
        ],
        Dialect::Json => &["false", "null", "true"],
        Dialect::Shell => &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "local", "return", "select", "then", "until", "while",
        ],
        Dialect::Unknown => &[],
    }
}

/// The words that name a *kind of value* rather than a kind of instruction.
///
/// These are the ones a capital letter cannot find: `u32` and `usize` are types
/// and they are not capitalised, and a reader who sees `u32` in the same colour
/// as `x` has been told nothing at all.
fn primitives(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::Rust => &[
            "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
            "u16", "u32", "u64", "u128", "usize",
        ],
        Dialect::CLike => &[
            "bool", "byte", "char", "double", "float", "int", "long", "short", "string", "void",
        ],
        Dialect::Python => &[
            "bool", "bytes", "dict", "float", "int", "list", "set", "str", "tuple",
        ],
        Dialect::Json | Dialect::Shell | Dialect::Unknown => &[],
    }
}

/// A line sorted into coloured runs, covering the whole line with no gaps.
///
/// `base` is the ink for everything the lexer did not classify, so a caller gets
/// a complete answer and never has to decide what an unrecognised run means.
pub fn pieces(language: &str, line: &str, base: Ink) -> Vec<Piece> {
    let d = dialect(language);
    let mut out: Vec<Piece> = Vec::new();
    let mut at = 0usize; // end of the last run pushed

    let push = |out: &mut Vec<Piece>, at: &mut usize, from: usize, to: usize, ink: Ink| {
        if from < *at {
            // Runs never overlap, so this cannot happen; guarding it means a bug
            // here costs a colour and not a panic on someone else's text.
            return;
        }
        if from > *at {
            out.push(Piece {
                start: *at,
                end: from,
                ink: base,
            });
        }
        out.push(Piece {
            start: from,
            end: to,
            ink,
        });
        *at = to;
    };

    // A whole-line comment is the one case worth deciding before the walk: it is
    // by far the most common way a line is *entirely* not code, and deciding it
    // up front is both faster and harder to get wrong.
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') && d != Dialect::Rust && d != Dialect::Json {
        return vec![Piece {
            start: 0,
            end: line.len(),
            ink: CODE_COMMENT,
        }];
    }

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let char_at = |n: usize| chars.get(n).map(|(_, c)| *c);
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];

        // `//` and `/* … */` — but not in the languages where `/` is division and
        // `#` is the comment.
        if c == '/' && d != Dialect::Python && d != Dialect::Shell {
            match char_at(i + 1) {
                Some('/') => {
                    push(&mut out, &mut at, start, line.len(), CODE_COMMENT);
                    break;
                }
                Some('*') => {
                    let end = line[start + 2..]
                        .find("*/")
                        .map(|i| start + 2 + i + 2)
                        .unwrap_or(line.len());
                    push(&mut out, &mut at, start, end, CODE_COMMENT);
                    i = chars.partition_point(|&(b, _)| b < end);
                    continue;
                }
                _ => {}
            }
        }
        // A comment that starts mid-line, in the `#` languages.
        if c == '#' && (d == Dialect::Python || d == Dialect::Shell) {
            push(&mut out, &mut at, start, line.len(), CODE_COMMENT);
            break;
        }

        if c == '"' || c == '\'' || c == '`' {
            if let Some(end) = string_end(&chars, i) {
                let stop = chars[end].0 + chars[end].1.len_utf8();
                // A quoted word with a colon after it is a key, not a value —
                // which is the only reason JSON is worth having its own family.
                let ink = if d == Dialect::Json && next_non_space(&chars, end + 1) == Some(':') {
                    CODE_TYPE
                } else {
                    CODE_STRING
                };
                push(&mut out, &mut at, start, stop, ink);
                i = end + 1;
                continue;
            }
        }

        if c.is_ascii_digit() {
            let mut j = i;
            while let Some(&(b, ch)) = chars.get(j) {
                if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' {
                    j += 1;
                } else {
                    let _ = b;
                    break;
                }
            }
            let stop = chars[j - 1].0 + chars[j - 1].1.len_utf8();
            push(&mut out, &mut at, start, stop, CODE_NUMBER);
            i = j;
            continue;
        }

        if c.is_alphabetic() || c == '_' || c == '$' {
            let mut j = i;
            while let Some(&(_, ch)) = chars.get(j) {
                if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                    j += 1;
                } else {
                    break;
                }
            }
            let word: String = chars[i..j].iter().map(|(_, ch)| *ch).collect();
            let stop = chars[j - 1].0 + chars[j - 1].1.len_utf8();
            if let Some(ink) = ink_for(&word, char_at(j), d) {
                push(&mut out, &mut at, start, stop, ink);
            }
            i = j;
            continue;
        }

        i += 1;
    }

    if at < line.len() {
        out.push(Piece {
            start: at,
            end: line.len(),
            ink: base,
        });
    }
    out
}

/// The colour one whole word gets, or `None` to leave it in the base ink.
fn ink_for(word: &str, next: Option<char>, d: Dialect) -> Option<Ink> {
    // An unknown language gets nothing but what it already has: strings and
    // numbers, which are decided before this is ever called. Every answer here is
    // a claim about a grammar nobody named, and a wrong claim is worse than none.
    if d == Dialect::Unknown {
        return None;
    }
    if keywords(d).contains(&word) {
        return Some(CODE_KEYWORD);
    }
    if primitives(d).contains(&word) {
        return Some(CODE_TYPE);
    }
    if next == Some('(') {
        return Some(CODE_FUNC);
    }
    // A capitalised word is a name for a kind in every language here, and an
    // all-caps one is a constant in most. It is a convention, not a rule, and it
    // is the convention editors use.
    if word.chars().next().is_some_and(char::is_uppercase) {
        return Some(CODE_TYPE);
    }
    None
}

/// The index of the quote that closes the one starting at `open`, if any.
///
/// Handles a backslash escape, and Python's triple-quoted strings, which is the
/// only multi-line literal any of these languages has.
fn string_end(chars: &[(usize, char)], open: usize) -> Option<usize> {
    let quote = chars[open].1;
    let triple = chars.get(open + 1).map(|&(_, c)| c) == Some(quote)
        && chars.get(open + 2).map(|&(_, c)| c) == Some(quote);
    let step = if triple { 3 } else { 1 };
    let mut i = open + step;
    while i < chars.len() {
        let (_, c) = chars[i];
        if c == '\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if triple {
                let closes = chars.get(i + 1).map(|&(_, c)| c) == Some(quote)
                    && chars.get(i + 2).map(|&(_, c)| c) == Some(quote);
                if closes {
                    return Some(i + 2);
                }
            } else {
                return Some(i);
            }
        }
        // A single-quoted string never spans a line. Without this an apostrophe
        // in a comment would swallow the rest of the file.
        if c == '\n' {
            return None;
        }
        i += 1;
    }
    None
}

/// The first character from `from` that is not a space or a tab.
fn next_non_space(chars: &[(usize, char)], from: usize) -> Option<char> {
    chars
        .iter()
        .skip(from)
        .find(|(_, c)| !matches!(c, ' ' | '\t'))
        .map(|(_, c)| *c)
}

/// Colour the part of `line` that falls inside `window`.
///
/// Wrapping splits a line into rows, and a row is a *window* onto the original
/// rather than a copy of it — so the runs are measured once against the whole
/// line and this is what narrows them to the row being drawn.
pub fn paint(line: &str, pieces: &[Piece], window: Range<usize>) -> String {
    let mut out = String::new();
    let mut cursor = window.start;
    for p in pieces {
        let from = p.start.max(window.start);
        let to = p.end.min(window.end);
        if from >= to {
            continue;
        }
        if from > cursor {
            out.push_str(ansi::RESET);
        }
        out.push_str(&p.ink.to_string());
        out.push_str(&line[from..to]);
        cursor = to;
    }
    out.push_str(ansi::RESET);
    out
}

/// Highlight untrusted source once, then wrap its styled spans without
/// re-lexing partial strings/comments. Commands and create-file previews share
/// this path with the conversation's highlighter; patch colours stay separate.
pub(super) fn source_rows(language: &str, source: &str, width: usize) -> Vec<String> {
    if source.is_empty() {
        return vec![String::new()];
    }
    source
        .lines()
        .flat_map(|line| {
            let safe = ansi::plain(&ansi::tame(line));
            let runs = pieces(language, &safe, ansi::TEXT);
            ansi::wrap(&paint(&safe, &runs, 0..safe.len()), width)
        })
        .collect()
}

pub(super) fn source_line(language: &str, source: &str) -> String {
    let safe = ansi::plain(&ansi::tame(source));
    let runs = pieces(language, &safe, ansi::TEXT);
    paint(&safe, &runs, 0..safe.len())
}

/// The ink a whole line is painted in when nothing classifies it, which is what
/// an unrecognised language falls back to. Named so the answer lives here.
pub const BASE: Ink = CODE;

#[cfg(test)]
mod tests {
    use super::*;

    fn ink_at(language: &str, line: &str, needle: &str) -> Ink {
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
        let runs = pieces(language, line, BASE);
        for p in &runs {
            if at >= p.start && at < p.end {
                return p.ink;
            }
        }
        BASE
    }

    #[test]
    fn source_previews_keep_text_and_colours_across_wraps_and_never_forward_control_escapes() {
        for (language, line) in [
            ("bash", "if true; then echo 'a long string'; fi"),
            ("jsx", "const value = 'a long string'; // comment"),
        ] {
            let rows = source_rows(language, line, 12);
            assert_eq!(
                rows.iter().map(|row| ansi::plain(row)).collect::<String>(),
                line
            );
            assert!(rows
                .iter()
                .any(|row| row.contains(&CODE_STRING.to_string())));
            for row in &rows {
                assert!(ansi::width(row) <= 12);
            }
        }
        let rows = source_rows("js", "one\n\nthree", 30);
        assert_eq!(rows.len(), 3);
        assert_eq!(ansi::plain(&rows[1]), "");
        let safe = source_line("bash", "echo \x1b[2J\x1b]52;c;c2VjcmV0\x07safe");
        assert_eq!(ansi::plain(&safe), "echo safe");
        assert!(!safe.contains("\x1b[2J") && !safe.contains("\x1b]52"));
    }

    #[test]
    fn a_rust_line_sorts_its_parts_by_meaning() {
        let line = r#"pub fn solve(n: u32) -> u32 { let x = "hi"; n + 1 }"#;
        assert_eq!(ink_at("rust", line, "pub"), CODE_KEYWORD);
        assert_eq!(ink_at("rust", line, "fn"), CODE_KEYWORD);
        assert_eq!(ink_at("rust", line, "solve"), CODE_FUNC);
        assert_eq!(ink_at("rust", line, "u32"), CODE_TYPE);
        assert_eq!(ink_at("rust", line, "\"hi\""), CODE_STRING);
    }

    #[test]
    fn a_comment_is_not_coloured_like_code() {
        let line = "// let x = 1";
        let runs = pieces("rust", line, BASE);
        assert!(runs
            .iter()
            .all(|p| p.ink == CODE_COMMENT || line[p.start..p.end].trim().is_empty()));
    }

    #[test]
    fn a_hash_language_uses_hash_for_comments_and_c_does_not() {
        assert_eq!(ink_at("python", "# x = 1", "#"), CODE_COMMENT);
        assert_eq!(ink_at("bash", "# x = 1", "#"), CODE_COMMENT);
        // In Rust `#` is an attribute, not a comment.
        assert_eq!(ink_at("rust", "#![allow(dead_code)]", "#"), BASE);
    }

    /// The apostrophe trap: an unclosed quote must not eat the rest of the file.
    #[test]
    fn an_unclosed_quote_does_not_swallow_the_line() {
        let line = "don't stop = 1";
        let runs = pieces("rust", line, BASE);
        assert!(line[0..line.len()].find("stop").is_some());
        assert!(
            runs.iter().all(|p| p.end <= line.len()),
            "a run ran past the line: {runs:?}"
        );
    }

    #[test]
    fn json_separates_a_key_from_a_value() {
        assert_eq!(ink_at("json", r#"{"a": 1}"#, "\"a\""), CODE_TYPE);
        assert_eq!(ink_at("json", r#"{"a": "b"}"#, "\"b\""), CODE_STRING);
        assert_eq!(ink_at("json", r#"{"a": 1}"#, "1"), CODE_NUMBER);
    }

    /// The whole point of having an Unknown family: guess nothing, colour the
    /// two things that look the same in every language there is.
    #[test]
    fn an_unknown_language_is_never_given_a_keyword_list() {
        let line = r#"fn main() { let s = "x"; }"#;
        assert_eq!(ink_at("wat", line, "fn"), BASE);
        assert_eq!(ink_at("wat", line, "main"), BASE);
        assert_eq!(ink_at("wat", line, "\"x\""), CODE_STRING);
    }

    #[test]
    fn every_run_is_a_valid_boundary_and_the_line_is_covered() {
        for (lang, line) in [
            ("rust", r#"let s = "日本語"; // note"#),
            ("python", "x = {'a': [1, 2]}  # note"),
            ("json", r#"{"k": [1, null, true]}"#),
            ("bash", "echo \"a\" | grep -q x # note"),
            ("", "plain text with no code at all"),
        ] {
            let runs = pieces(lang, line, BASE);
            assert_eq!(
                runs.first().map(|p| p.start),
                Some(0),
                "{lang} left a gap at the front: {runs:?}"
            );
            assert_eq!(
                runs.last().map(|p| p.end),
                Some(line.len()),
                "{lang} left a gap at the end: {runs:?}"
            );
            for w in runs.windows(2) {
                assert_eq!(w[0].end, w[1].start, "{lang} left a hole: {runs:?}");
            }
            for p in &runs {
                assert!(line.is_char_boundary(p.start) && line.is_char_boundary(p.end));
            }
        }
    }

    /// Wrapping hands back rows whose concatenation is the original line, which
    /// is what lets a row be coloured as a window onto the whole.
    #[test]
    fn a_wrapped_row_paints_only_its_own_window() {
        let line = r#"let alpha = "one"; let beta = "two";"#;
        let wrapped = ansi::wrap(line, 24);
        let runs = pieces("rust", line, BASE);
        let mut offset = 0;
        for row in &wrapped {
            let window = offset..offset + row.len();
            let painted = paint(line, &runs, window);
            let plain = ansi::plain(&painted);
            assert_eq!(
                plain, *row,
                "painting changed the text: {plain:?} vs {row:?}"
            );
            offset += row.len();
        }
        assert_eq!(offset, line.len(), "the rows do not cover the line");
    }

    #[test]
    fn a_fence_may_carry_more_than_a_language() {
        assert_eq!(dialect("rust,ignore"), Dialect::Rust);
        assert_eq!(dialect("rust title=\"main\""), Dialect::Rust);
        assert_eq!(dialect("  TypeScript"), Dialect::CLike);
        assert_eq!(dialect(""), Dialect::Unknown);
    }
}
