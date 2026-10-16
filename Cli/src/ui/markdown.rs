//! Markdown, rendered for a terminal.
//!
//! # Why spans, and not a regular expression over the finished line
//!
//! The obvious implementation styles the text first and wraps the result. It is
//! wrong for one reason that shows up immediately: a bold phrase that straddles
//! a wrap point loses its closing code on the first line and opens on a line
//! that never closes it, so the rest of the screen goes bold. Colour has to
//! survive being cut, and the only way it does is if the cutting happens to
//! *spans* — text plus intent — and the escape codes are emitted per finished
//! line, opened and closed inside it.
//!
//! That is also what makes the width arithmetic honest: [`super::ansi::width`]
//! never has to measure a string that has already been dressed.
//!
//! # What is supported, and what is deliberately not
//!
//! Headings, bold, italic, inline code, links, fenced code blocks with a
//! language label, blockquotes, ordered and unordered lists with nesting, tables,
//! horizontal rules, and maths.
//!
//! Maths is the one that needs saying. A terminal cannot set an integral sign,
//! so `$…$` is **not** faked into a picture of an equation: the source is kept,
//! coloured so it reads as one object rather than as prose, and the symbols that
//! do have Unicode forms — `\alpha`, `\times`, `^2`, `_1` — are substituted. The
//! reader gets something they can read *and* copy back into LaTeX. A renderer
//! that half-draws a fraction gives them neither.

use super::ansi::{self, ACCENT, ADDED, CODE, MATH, MUTED, QUOTE, REMOVED, RULE, TEXT};
use super::highlight;

/// How a run of text should be drawn.
#[derive(Debug, Clone, PartialEq)]
struct Style {
    colour: ansi::Ink,
    bold: bool,
    italic: bool,
    /// A URL, when this run is a link label.
    href: Option<String>,
}

impl Style {
    fn plain(colour: ansi::Ink) -> Style {
        Style {
            colour,
            bold: false,
            italic: false,
            href: None,
        }
    }

    fn open(&self) -> String {
        let mut out = String::new();
        if self.bold {
            out.push_str(ansi::BOLD);
        }
        if self.italic {
            out.push_str(ansi::ITALIC);
        }
        out.push_str(&self.colour.to_string());
        out
    }
}

#[derive(Debug, Clone)]
struct Span {
    text: String,
    style: Style,
}

/// Render markdown to styled, wrapped terminal lines.
///
/// `width` is the number of columns the caller has; every returned line fits.
pub fn render(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut lines = text.split('\n').peekable();
    let mut table: Vec<String> = Vec::new();

    while let Some(raw) = lines.next() {
        // A table is the one construct that needs several lines at once.
        if is_table_row(raw) {
            table.push(raw.to_string());
            if lines.peek().is_some_and(|n| is_table_row(n)) {
                continue;
            }
            out.extend(self::table(&std::mem::take(&mut table), width));
            continue;
        }

        if let Some(fence) = fence_of(raw) {
            let mut body: Vec<String> = Vec::new();
            for line in lines.by_ref() {
                if fence_of(line).is_some() {
                    break;
                }
                body.push(line.to_string());
            }
            out.extend(code_block(&fence, &body, width));
            continue;
        }

        if raw.trim() == "$$" {
            let mut body: Vec<String> = Vec::new();
            for line in lines.by_ref() {
                if line.trim() == "$$" {
                    break;
                }
                body.push(line.to_string());
            }
            for line in body {
                out.push(format!("  {MATH}{}{}", maths(&line), ansi::RESET));
            }
            continue;
        }

        out.extend(block(raw, width));
    }
    if !table.is_empty() {
        out.extend(self::table(&table, width));
    }
    out
}

/// One line that is not part of a multi-line construct.
fn block(raw: &str, width: usize) -> Vec<String> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();

    if trimmed.is_empty() {
        return vec![String::new()];
    }

    if is_rule(trimmed) {
        return vec![format!(
            "{RULE}{}{}",
            "─".repeat(width.min(60)),
            ansi::RESET
        )];
    }

    if let Some(rest) = heading(trimmed) {
        let (level, text) = rest;
        // One visual weight per level and no more: three sizes of shouting in a
        // terminal is just shouting.
        let style = match level {
            1 => Style {
                colour: ACCENT,
                bold: true,
                italic: false,
                href: None,
            },
            2 => Style {
                colour: TEXT,
                bold: true,
                italic: false,
                href: None,
            },
            _ => Style {
                colour: MUTED,
                bold: true,
                italic: false,
                href: None,
            },
        };
        let mut lines = lay_out(&inline(text, style), width, "", "");
        if level == 1 {
            lines.push(format!(
                "{RULE}{}{}",
                "─".repeat(width.min(60)),
                ansi::RESET
            ));
        }
        return lines;
    }

    if let Some((depth, text)) = quoted(trimmed) {
        // One bar per level. A quote inside a quote is a different claim from
        // the one containing it — most often someone being cited disagreeing —
        // and rendering both at one level makes the two speakers one.
        let bar = format!("{QUOTE}│{} ", ansi::RESET).repeat(depth);
        return lay_out(
            &inline(text, Style::plain(MUTED)),
            width.saturating_sub(2 * depth),
            &bar,
            &bar,
        );
    }

    let pad = " ".repeat(indent);
    if let Some((marker, text)) = list_item(trimmed) {
        let first = format!("{pad}{ACCENT}{marker}{} ", ansi::RESET);
        let cont = " ".repeat(indent + marker.chars().count() + 1);
        let room = width.saturating_sub(indent + marker.chars().count() + 1);
        return lay_out(&inline(text, Style::plain(TEXT)), room, &first, &cont);
    }

    lay_out(
        &inline(trimmed, Style::plain(TEXT)),
        width.saturating_sub(indent),
        &pad,
        &pad,
    )
}

/// How deeply a line is quoted, and what is left once the markers are removed.
///
/// `>>text`, `> > text` and `> >text` are the same two levels; markdown does not
/// require the spaces and people do not write them.
fn quoted(line: &str) -> Option<(usize, &str)> {
    let mut rest = line.strip_prefix('>')?;
    let mut depth = 1;
    loop {
        let next = rest.strip_prefix(' ').unwrap_or(rest);
        match next.strip_prefix('>') {
            Some(inner) => {
                depth += 1;
                rest = inner;
            }
            None => return Some((depth, next)),
        }
    }
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    Some((hashes, rest))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

/// The marker and the text of a list item, for both kinds of list.
fn list_item(line: &str) -> Option<(String, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return Some(("•".to_string(), rest));
        }
    }
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(text) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &line[..digits]), text));
        }
    }
    None
}

fn fence_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    for fence in ["```", "~~~"] {
        if let Some(rest) = t.strip_prefix(fence) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// A fenced code block: never wrapped, never re-coloured, and fully framed.
///
/// Wrapping code is worse than cutting it — a broken line reads as a different
/// program — so a long line is truncated with a marker and the whole block keeps
/// the shape the author gave it.
///
/// # Why the frame is closed
///
/// It used to be a `┌─ label`, a column of `│`, and a lone `└` — open on the
/// right and open at the bottom. Drawn in the faintest ink beside a wide block
/// of output, that is not a frame at all: the owner photographed one and asked
/// what the stray `[` on the screen was. It was the whole of the code block's
/// left edge.
///
/// It is now the same closed frame, in the same ink, as a table — because a
/// fenced block and a table are the same idea, a rectangle of content that is
/// not prose, and a screen with two kinds of rectangle on it has one kind too
/// many. The language goes in the top edge where a table's heading would be.
pub fn code_block(language: &str, body: &[String], width: usize) -> Vec<String> {
    let label = if language.is_empty() {
        "code"
    } else {
        language
    };
    let max_avail = width.saturating_sub(6).max(12);
    let label_w = ansi::width(label) + 4;
    let max_content_w = body
        .iter()
        .map(|l| ansi::width(&ansi::tame(l)))
        .max()
        .unwrap_or(0);
    let inner = max_content_w.clamp(label_w, max_avail);
    let span = inner + 2;

    let rest = span.saturating_sub(ansi::width(label) + 4);
    let mut out = vec![format!(
        "  {RULE}╭─ {MUTED}{label}{RULE} {}{}",
        "─".repeat(rest),
        ansi::RESET
    )];
    let diff = language.eq_ignore_ascii_case("diff") || language.eq_ignore_ascii_case("patch");
    for line in body {
        let tamed = ansi::tame(line);
        // `ansi::wrap` only ever splits a line, so the rows it returns concatenate
        // back to the line it was given. That is what lets a row be coloured as a
        // *window* onto runs measured once against the whole line, instead of
        // being re-lexed in a fragment where a half-open string or a comment cut
        // in half would be lexed as something it is not.
        let runs = (!diff).then(|| highlight::pieces(language, &tamed, highlight::BASE));
        let wrapped = ansi::wrap(&tamed, inner);
        let mut offset = 0usize;
        for sub in wrapped {
            let body = match &runs {
                Some(runs) => highlight::paint(&tamed, runs, offset..offset + sub.len()),
                None => format!("{}{sub}{}", diff_ink(line), ansi::RESET),
            };
            offset += sub.len();
            out.push(format!("  {RULE}│{}  {body}{}", ansi::RESET, ansi::RESET));
        }
    }
    out.push(format!(
        "  {RULE}╰─{}{}",
        "─".repeat(rest.max(4)),
        ansi::RESET
    ));
    out
}

/// What one line of a diff means, by the character diff itself puts there.
///
/// The colour is a second reading of a distinction the text already carries —
/// `+` and `-` are in the content, not added by us — so the block still says
/// which lines are which under `NO_COLOR`, on a sixteen-colour terminal, and to
/// anyone who does not separate red from green. Colour that is the *only* carrier
/// of a meaning is the one thing every accessibility guide agrees to refuse.
fn diff_ink(line: &str) -> ansi::Ink {
    match line.as_bytes().first() {
        // `+++`/`---` are the file header, not a change to one line of it.
        Some(b'+') if !line.starts_with("+++") => ADDED,
        Some(b'-') if !line.starts_with("---") => REMOVED,
        Some(b'@') => ACCENT,
        _ => MUTED,
    }
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.len() > 1
}

/// A markdown table, with every column as wide as its widest cell.
///
/// Measured with [`ansi::width`], not `len`, so a table of Indonesian prose with
/// a tick in it lines up the same as one of ASCII.
///
/// # Why it is fully framed now
///
/// The first version drew only the verticals between the columns and one rule
/// under the header, in the dimmest ink on the screen. On a terminal with
/// something behind it, that is a handful of disconnected marks — the owner's
/// words were "too thin and unclear". A table earns its columns by being read as
/// a grid, and a grid needs a closed frame, a header that looks like a header,
/// and borders in an ink chosen to be visible ([`ansi::RULE`], which the theme
/// solves for a real contrast ratio) rather than the faintest one available.
fn table(rows: &[String], width: usize) -> Vec<String> {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.trim()
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect();
    // The `|---|---|` separator is layout, not data.
    let is_divider = |row: &Vec<String>| {
        row.iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
    };
    let body: Vec<&Vec<String>> = cells.iter().filter(|r| !is_divider(r)).collect();
    if body.is_empty() {
        return Vec::new();
    }

    let columns = body.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in &body {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(ansi::width(cell));
        }
    }
    // The two-column indent, both outer edges, and three cells of furniture per
    // column all count. Reduce the widest column first: subtracting the same
    // amount from every column crushed short labels while a prose column still
    // dominated the table.
    while widths.iter().sum::<usize>() + 3 * columns + 3 > width {
        let Some((at, widest)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if *widest <= 3 {
            break;
        }
        widths[at] -= 1;
    }

    // One place that knows how a horizontal edge is built, so the top, the
    // header rule and the bottom can never disagree about where a column ends.
    let edge = |left: &str, join: &str, right: &str| {
        let spans: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("  {RULE}{left}{}{right}{}", spans.join(join), ansi::RESET)
    };

    let mut out: Vec<String> = vec![edge("┌", "┬", "┐")];
    for (n, row) in body.iter().enumerate() {
        let rendered: Vec<Vec<String>> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let cell = row.get(i).cloned().unwrap_or_default();
                let style = if n == 0 {
                    Style {
                        colour: ACCENT,
                        bold: true,
                        italic: false,
                        href: None,
                    }
                } else {
                    Style::plain(TEXT)
                };
                lay_out(&inline(&cell, style), *w, "", "")
            })
            .collect();
        let height = rendered.iter().map(Vec::len).max().unwrap_or(1);
        for line_at in 0..height {
            let mut line = format!("  {RULE}│{}", ansi::RESET);
            for (i, w) in widths.iter().enumerate() {
                let drawn = rendered[i].get(line_at).cloned().unwrap_or_default();
                line.push_str(&format!(
                    " {} {RULE}│{}",
                    ansi::pad(&drawn, *w),
                    ansi::RESET
                ));
            }
            out.push(line);
        }
        if n == 0 {
            out.push(edge("├", "┼", "┤"));
        }
    }
    out.push(edge("└", "┴", "┘"));
    // The squeeze is best-effort when a terminal is narrower than three columns
    // per cell plus its borders. Ordinary tables wrap their cells and keep every
    // word; only an impossible grid is cut as a final frame-safety measure.
    out.into_iter().map(|l| ansi::truncate(&l, width)).collect()
}

// ─── Inline ───────────────────────────────────────────────────────────────────

/// Split one line into styled runs. `base` is the style prose gets here.
fn inline(text: &str, base: Style) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span>, style: &Style| {
        if !buf.is_empty() {
            spans.push(Span {
                text: std::mem::take(buf),
                style: style.clone(),
            });
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // Escapes come first: `\*` is an asterisk, not the start of emphasis.
        if c == '\\' && i + 1 < chars.len() {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }

        if c == '`' {
            if let Some(end) = find(&chars, i + 1, '`') {
                flush(&mut buf, &mut spans, &base);
                let body: String = chars[i + 1..end].iter().collect();
                // One span per run, so `` `fn` `` and `` `String` `` are not the
                // same colour — the same answer the fenced block gives, from the
                // same lexer. No language is named for an inline snippet, so the
                // lexer is given none and colours only what is the same in every
                // language: strings, numbers, and the call/case shapes.
                for piece in highlight::pieces("", &body, CODE) {
                    spans.push(Span {
                        text: body[piece.start..piece.end].to_string(),
                        style: Style::plain(piece.ink),
                    });
                }
                i = end + 1;
                continue;
            }
        }

        if c == '$' {
            let double = chars.get(i + 1) == Some(&'$');
            let open = if double { i + 2 } else { i + 1 };
            if let Some(end) = find_str(&chars, open, if double { "$$" } else { "$" }) {
                flush(&mut buf, &mut spans, &base);
                let body: String = chars[open..end].iter().collect();
                spans.push(Span {
                    text: maths(&body),
                    style: Style::plain(MATH),
                });
                i = end + if double { 2 } else { 1 };
                continue;
            }
        }

        if c == '[' {
            if let Some(close) = find(&chars, i + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(&chars, close + 2, ')') {
                        flush(&mut buf, &mut spans, &base);
                        let label: String = chars[i + 1..close].iter().collect();
                        let href: String = chars[close + 2..end].iter().collect();
                        spans.push(Span {
                            text: label,
                            style: Style {
                                colour: ACCENT,
                                bold: false,
                                italic: false,
                                href: Some(href),
                            },
                        });
                        i = end + 1;
                        continue;
                    }
                }
            }
        }

        // Emphasis: longest marker first, so `***` is not read as `*` twice.
        let mut matched = false;
        for (marker, bold, italic) in [
            ("***", true, true),
            ("**", true, false),
            ("__", true, false),
            ("*", false, true),
            ("_", false, true),
        ] {
            if !starts_with(&chars, i, marker) {
                continue;
            }
            let Some(end) = find_str(&chars, i + marker.len(), marker) else {
                continue;
            };
            if end == i + marker.len() {
                continue; // `**` with nothing between it is not emphasis.
            }
            // Markdown's own rule, and the reason `2 * 3 * 4` is arithmetic
            // rather than an italic three: a delimiter that opens on whitespace
            // or closes on it is not a delimiter.
            let opens_on_space = chars[i + marker.len()].is_whitespace();
            let closes_on_space = chars[end - 1].is_whitespace();
            if opens_on_space || closes_on_space {
                continue;
            }
            // An underscore inside a word is part of the word. Markdown says so,
            // and a file listing proves why: `Dickinson_Sample_Slides.pptx` came
            // out as `DickinsonSampleSlides.pptx` in italics — the two
            // underscores were read as emphasis and the name was silently
            // rewritten. Asterisks keep the looser rule they have in Markdown.
            if marker.starts_with('_') {
                let before = i.checked_sub(1).and_then(|n| chars.get(n));
                let after = chars.get(end + marker.len());
                if before.is_some_and(|c| c.is_alphanumeric())
                    || after.is_some_and(|c| c.is_alphanumeric())
                {
                    continue;
                }
            }
            flush(&mut buf, &mut spans, &base);
            let body: String = chars[i + marker.len()..end].iter().collect();
            let mut style = base.clone();
            style.bold |= bold;
            style.italic |= italic;
            // Nested markup inside emphasis is rare, and the recursion is
            // bounded by a strictly shorter slice.
            spans.extend(inline(&body, style));
            i = end + marker.len();
            matched = true;
            break;
        }
        if matched {
            continue;
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans, &base);
    spans
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(n, c)| chars.get(at + n) == Some(&c))
}

fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == needle)
}

fn find_str(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| starts_with(chars, i, needle))
}

/// LaTeX that has a Unicode form, in that form. Everything else is left exactly
/// as the author wrote it — a half-translated formula is worse than the source.
fn maths(text: &str) -> String {
    const SYMBOLS: &[(&str, &str)] = &[
        ("\\alpha", "α"),
        ("\\beta", "β"),
        ("\\gamma", "γ"),
        ("\\delta", "δ"),
        ("\\epsilon", "ε"),
        ("\\theta", "θ"),
        ("\\lambda", "λ"),
        ("\\mu", "μ"),
        ("\\pi", "π"),
        ("\\sigma", "σ"),
        ("\\phi", "φ"),
        ("\\omega", "ω"),
        ("\\Delta", "Δ"),
        ("\\Sigma", "Σ"),
        ("\\Omega", "Ω"),
        ("\\times", "×"),
        ("\\div", "÷"),
        ("\\pm", "±"),
        ("\\cdot", "·"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\neq", "≠"),
        ("\\approx", "≈"),
        ("\\infty", "∞"),
        ("\\sum", "∑"),
        ("\\prod", "∏"),
        ("\\int", "∫"),
        ("\\sqrt", "√"),
        ("\\in", "∈"),
        ("\\rightarrow", "→"),
        ("\\to", "→"),
        ("\\leftarrow", "←"),
        ("\\Rightarrow", "⇒"),
        ("\\partial", "∂"),
    ];
    const SUPERS: &[(char, char)] = &[
        ('0', '⁰'),
        ('1', '¹'),
        ('2', '²'),
        ('3', '³'),
        ('4', '⁴'),
        ('5', '⁵'),
        ('6', '⁶'),
        ('7', '⁷'),
        ('8', '⁸'),
        ('9', '⁹'),
        ('n', 'ⁿ'),
    ];
    const SUBS: &[(char, char)] = &[
        ('0', '₀'),
        ('1', '₁'),
        ('2', '₂'),
        ('3', '₃'),
        ('4', '₄'),
        ('5', '₅'),
        ('6', '₆'),
        ('7', '₇'),
        ('8', '₈'),
        ('9', '₉'),
        ('n', 'ₙ'),
    ];

    let mut out = text.to_string();
    for (from, to) in SYMBOLS {
        out = out.replace(from, to);
    }
    // Single-character scripts only: `x^{2n}` stays as written, because a
    // sequence of Unicode superscripts is not what the author drew.
    let chars: Vec<char> = out.chars().collect();
    let mut result = String::with_capacity(out.len());
    let mut i = 0;
    while i < chars.len() {
        let table = match chars[i] {
            '^' => Some(SUPERS),
            '_' => Some(SUBS),
            _ => None,
        };
        match table.and_then(|t| {
            let next = *chars.get(i + 1)?;
            t.iter().find(|(from, _)| *from == next).map(|(_, to)| *to)
        }) {
            Some(replacement) => {
                result.push(replacement);
                i += 2;
            }
            None => {
                result.push(chars[i]);
                i += 1;
            }
        }
    }
    result
}

// ─── Laying spans out ─────────────────────────────────────────────────────────

/// Wrap spans to `width`, prefixing the first line with `first` and the rest
/// with `rest`.
///
/// Every emitted line opens and closes its own styling, which is what makes a
/// bold phrase that crosses a line break safe.
fn lay_out(spans: &[Span], width: usize, first: &str, rest: &str) -> Vec<String> {
    let width = width.max(4);
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut used = 0;

    for span in spans {
        for word in span.text.split_inclusive(' ') {
            let w = ansi::width(word);
            let trimmed = ansi::width(word.trim_end());
            // A trailing space at a wrap point is not worth a new line for.
            if used + trimmed > width && used > 0 {
                lines.push(Vec::new());
                used = 0;
                if word.trim().is_empty() {
                    continue;
                }
            }
            if w > width {
                // One unbreakable run — a path, a hash — chopped at the margin.
                for chunk in chop(word, width - used.min(width - 1)) {
                    push_word(&mut lines, &mut used, &chunk, span, width);
                }
                continue;
            }
            let line = lines.last_mut().expect("at least one line");
            match line.last_mut() {
                Some(last) if last.style == span.style => last.text.push_str(word),
                _ => line.push(Span {
                    text: word.to_string(),
                    style: span.style.clone(),
                }),
            }
            used += w;
        }
    }

    lines
        .into_iter()
        .enumerate()
        .map(|(i, mut spans)| {
            // The space that separated two words is invisible in the middle of a
            // line and one column wide at the end of one. Drawn, it made every
            // wrapped paragraph exactly one column too wide.
            if let Some(last) = spans.last_mut() {
                let trimmed = last.text.trim_end().to_string();
                last.text = trimmed;
            }
            let prefix = if i == 0 { first } else { rest };
            format!("{prefix}{}", join(&spans))
        })
        .collect()
}

fn push_word(lines: &mut Vec<Vec<Span>>, used: &mut usize, word: &str, span: &Span, width: usize) {
    if *used + ansi::width(word) > width && *used > 0 {
        lines.push(Vec::new());
        *used = 0;
    }
    lines.last_mut().expect("at least one line").push(Span {
        text: word.to_string(),
        style: span.style.clone(),
    });
    *used += ansi::width(word);
}

fn chop(word: &str, first: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut room = first.max(1);
    for c in word.chars() {
        if ansi::width(&cur) + 1 > room {
            out.push(std::mem::take(&mut cur));
            room = usize::MAX;
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Spans to one printable string, each carrying its own open and close.
fn join(spans: &[Span]) -> String {
    let mut out = String::new();
    for span in spans {
        let body = match &span.style.href {
            Some(url) => ansi::link(url, &span.text),
            None => span.text.clone(),
        };
        out.push_str(&span.style.open());
        out.push_str(&body);
        out.push_str(ansi::RESET);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[String]) -> String {
        lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_are_marked_and_the_top_one_gets_a_rule() {
        let out = render("# Title\n## Second\n### Third", 40);
        let text = text_of(&out);
        assert!(text.contains("Title"));
        assert!(
            text.contains('─'),
            "a top-level heading is separated from what follows"
        );
        assert!(
            !text.contains('#'),
            "the marker is styling, not content: {text}"
        );
    }

    #[test]
    fn bold_and_italic_survive_and_their_markers_do_not() {
        let out = render("plain **bold** and *italic* done", 60);
        let text = text_of(&out);
        assert_eq!(text.trim(), "plain bold and italic done");
        assert!(out[0].contains(ansi::BOLD));
        assert!(out[0].contains(ansi::ITALIC));
    }

    /// The reason this file is span-based. Styled-then-wrapped leaves the first
    /// line opened and never closed, and the rest of the screen goes bold.
    #[test]
    fn emphasis_across_a_wrap_point_closes_on_every_line() {
        let out = render("**one two three four five six seven eight**", 14);
        assert!(out.len() > 1, "the fixture must actually wrap");
        for line in &out {
            assert!(
                line.ends_with(ansi::RESET),
                "a line left styling open: {line:?}"
            );
            assert!(ansi::width(line) <= 14, "{}", ansi::width(line));
        }
    }

    /// The whole point of the lexer: a fenced block is no longer one ink. Two
    /// different tokens in one line must come out in two different colours, or
    /// nothing was actually sorted and the block is still a wall.
    #[test]
    fn a_fenced_block_sorts_its_tokens_instead_of_being_one_ink() {
        let out = render("```rust\nlet s = \"text\"; // note\n```", 80);
        let line = out
            .iter()
            .find(|l| ansi::plain(l).contains("let s"))
            .expect("the code line must be on screen");
        assert!(
            line.contains(&ansi::CODE_KEYWORD.to_string()),
            "no keyword: {line:?}"
        );
        assert!(
            line.contains(&ansi::CODE_STRING.to_string()),
            "no string: {line:?}"
        );
        assert!(
            line.contains(&ansi::CODE_COMMENT.to_string()),
            "no comment: {line:?}"
        );
        assert_eq!(
            ansi::plain(line).split("│").nth(1).map(str::trim),
            Some(r#"let s = "text"; // note"#),
            "colouring changed the code"
        );
    }

    /// A block in a language nobody named is still a block of code — but it is
    /// not given a keyword list, so nothing is claimed about a grammar that was
    /// never stated.
    #[test]
    fn an_unlabelled_block_is_still_code_but_guesses_no_keywords() {
        let out = render("```\nlet s = \"text\";\n```", 80);
        let line = out
            .iter()
            .find(|l| ansi::plain(l).contains("let s"))
            .expect("the code line must be on screen");
        assert!(
            line.contains(&ansi::CODE_STRING.to_string()),
            "a string is a string in every language: {line:?}"
        );
        assert!(
            !line.contains(&ansi::CODE_KEYWORD.to_string()),
            "it guessed a keyword list for a language nobody named: {line:?}"
        );
    }

    #[test]
    fn inline_code_is_coloured_and_keeps_its_contents_verbatim() {
        let out = render("call `foo(**a**)` now", 60);
        let text = text_of(&out);
        assert!(text.contains("foo(**a**)"), "code is not markdown: {text}");
    }

    /// The frame used to be open on the right and open at the bottom — a
    /// corner, a column of bars, and one `└`. Beside a wide block of output that
    /// does not read as a frame at all: the owner photographed one and asked what
    /// the stray `[` on their screen was.
    #[test]
    fn a_fenced_block_is_framed_labelled_and_never_wrapped() {
        for width in [40usize, 60, 100] {
            let out = render("```rust\nfn main() {}\nlet x = 1;\n```", width);
            let text = text_of(&out);
            assert!(text.contains("rust"), "{text}");
            assert!(text.contains("fn main() {}"));
            assert!(!text.contains("```"), "the fence is a frame, not content");

            // Closed on all four sides, and every row the same width.
            let plain: Vec<String> = out
                .iter()
                .map(|l| text_of(std::slice::from_ref(l)))
                .collect();
            assert!(
                plain[0].trim_start().starts_with("╭─ rust"),
                "{:?}",
                plain[0]
            );
            assert!(
                plain.last().unwrap().trim_start().starts_with("╰─"),
                "{:?}",
                plain.last()
            );
            for row in &out {
                assert!(
                    ansi::width(row) <= width,
                    "line width exceeded {width}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_code_line_wraps_across_lines_rather_than_being_cut() {
        let long = "x".repeat(200);
        let out = render(&format!("```\n{long}\n```"), 40);
        for line in &out {
            assert!(ansi::width(line) <= 40, "{}", ansi::width(line));
        }
        let text = text_of(&out);
        assert!(
            !text.contains('…'),
            "no cut marker; long code line wraps: {text}"
        );
        assert_eq!(text.chars().filter(|&c| c == 'x').count(), 200);
    }

    #[test]
    fn lists_get_a_bullet_and_their_continuation_lines_line_up() {
        let out = render("- first item that is quite long indeed here\n- second", 24);
        let text = text_of(&out);
        assert!(text.contains('•'));
        assert!(
            !text.contains("- first"),
            "the dash is replaced by the bullet"
        );
        // The wrapped remainder is indented past the bullet.
        let wrapped = out
            .iter()
            .map(|l| ansi::plain(l))
            .find(|l| l.contains("indeed"));
        assert!(
            wrapped.is_some_and(|l| l.starts_with("  ")),
            "continuation is not aligned"
        );
    }

    #[test]
    fn numbered_lists_keep_their_numbers() {
        let text = text_of(&render("1. one\n2. two", 40));
        assert!(text.contains("1. one"), "{text}");
        assert!(text.contains("2. two"), "{text}");
    }

    #[test]
    fn a_blockquote_gets_a_bar_and_the_marker_disappears() {
        let text = text_of(&render("> quoted words", 40));
        assert!(text.contains('│'));
        assert!(!text.contains('>'));
        assert!(text.contains("quoted words"));
    }

    #[test]
    fn a_table_lines_up_even_when_the_cells_contain_wide_characters() {
        let md = "| name | ok |\n|---|---|\n| plain | yes |\n| ✅ wide | no |";
        let out = render(md, 60);
        let widths: Vec<usize> = out.iter().map(|l| ansi::width(l)).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "ragged table: {widths:?}"
        );
        let text = text_of(&out);
        assert!(text.contains("name"));
        assert!(!text.contains("---"), "the divider row is layout, not data");
    }

    #[test]
    fn a_table_wider_than_the_screen_is_squeezed_rather_than_overflowing() {
        let md = "| a | b |\n|---|---|\n| a very long cell indeed | another long one |";
        let rendered = render(md, 30);
        for line in &rendered {
            assert!(ansi::width(line) <= 30, "{}", ansi::width(line));
        }
        let text = text_of(&rendered);
        for word in ["very", "long", "indeed", "another"] {
            assert!(text.contains(word), "the wrapped cell lost {word}:\n{text}");
        }
        assert!(
            !text.contains('…'),
            "a normal table cell was truncated:\n{text}"
        );
        let widths: Vec<usize> = rendered.iter().map(|line| ansi::width(line)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged table: {widths:?}"
        );
    }

    #[test]
    fn links_carry_their_url_and_show_only_the_label() {
        let out = render("see [the docs](https://example.com/x) now", 60);
        assert!(
            out[0].contains("https://example.com/x"),
            "the URL must be in the escape"
        );
        // OSC-8 payload is not visible text, so the label is all that is measured.
        assert_eq!(ansi::width(&out[0]), "see the docs now".len());
    }

    #[test]
    fn a_horizontal_rule_becomes_a_rule() {
        let text = text_of(&render("above\n---\nbelow", 20));
        assert!(text.contains('─'));
        assert!(!text.contains("---"));
    }

    #[test]
    fn escaped_markers_are_printed_rather_than_interpreted() {
        let text = text_of(&render("a \\*literal\\* star", 40));
        assert!(text.contains("*literal*"), "{text}");
    }

    #[test]
    fn an_unclosed_marker_is_left_alone_instead_of_eating_the_line() {
        let text = text_of(&render("2 * 3 * 4 is not emphasis", 40));
        assert!(text.contains("2 * 3 * 4"), "{text}");
    }

    /// A file listing is the case that made this matter: a name is data, and a
    /// renderer that quietly deletes characters from it is worse than one that
    /// cannot do emphasis at all.
    #[test]
    fn an_underscore_inside_a_word_is_part_of_the_word() {
        let text = render(
            "| Dickinson_Sample_Slides.pptx | 632 KB |\n|---|---|\n| a | b |",
            80,
        )
        .join("\n");
        assert!(
            text.contains("Dickinson_Sample_Slides.pptx"),
            "the name was rewritten: {text}"
        );
        // The ordinary use still works.
        let emphasised = render("a _word_ here", 40).join("\n");
        assert!(emphasised.contains("word"));
        assert!(!emphasised.contains("_word_"));
    }

    // ── Maths ────────────────────────────────────────────────────────────────

    #[test]
    fn inline_maths_keeps_the_source_and_substitutes_what_it_can() {
        let text = text_of(&render("energy is $E = mc^2$ exactly", 60));
        assert!(text.contains("E = mc²"), "{text}");
        assert!(!text.contains('$'), "the delimiters are styling: {text}");
    }

    #[test]
    fn greek_and_operators_are_substituted_and_the_rest_is_untouched() {
        assert_eq!(maths("\\alpha \\times \\beta"), "α × β");
        assert_eq!(maths("x_1 + y^3"), "x₁ + y³");
        // A multi-character script has no honest single-glyph form.
        assert_eq!(maths("x^{2n}"), "x^{2n}");
        assert_eq!(
            maths("\\frac{a}{b}"),
            "\\frac{a}{b}",
            "what cannot be drawn is left readable"
        );
    }

    #[test]
    fn a_display_block_is_rendered_on_its_own_lines() {
        let text = text_of(&render("before\n$$\n\\sum x_1\n$$\nafter", 40));
        assert!(text.contains("∑ x₁"), "{text}");
        assert!(text.contains("before") && text.contains("after"));
    }

    // ── The contract every caller depends on ─────────────────────────────────

    #[test]
    fn no_rendered_line_is_ever_wider_than_it_was_asked_for() {
        let md = "# Heading\n\nSome **bold** prose with `code` and a [link](https://x.example/y).\n\n\
                  - a list item with rather a lot of words in it to force wrapping\n\
                  - ✅ another with an emoji that measures two columns\n\n\
                  | col | value |\n|---|---|\n| one | two |\n\n\
                  ```sh\necho hello world\n```\n\n> a quotation that is long enough to wrap around\n\n\
                  $$\n\\int_0^1 x^2\n$$";
        for width in [20usize, 32, 48, 80, 120] {
            for line in render(md, width) {
                assert!(
                    ansi::width(&line) <= width,
                    "width {width}: line measured {} — {line:?}",
                    ansi::width(&line)
                );
            }
        }
    }

    #[test]
    fn plain_prose_survives_a_round_trip_unchanged() {
        let text = text_of(&render("just some ordinary words here", 60));
        assert_eq!(text.trim(), "just some ordinary words here");
    }

    #[test]
    fn an_empty_input_renders_to_an_empty_line_not_a_panic() {
        assert_eq!(render("", 40).len(), 1);
        assert_eq!(ansi::plain(&render("", 40)[0]), "");
    }

    /// A quote inside a quote is a second speaker. Rendered at one level, the
    /// two become one — and the inner `>` is printed as literal text.
    #[test]
    fn a_nested_quote_gets_a_bar_for_every_level() {
        assert_eq!(quoted("> one"), Some((1, "one")));
        assert_eq!(quoted("> > two"), Some((2, "two")));
        assert_eq!(
            quoted(">>two"),
            Some((2, "two")),
            "markdown does not require the spaces"
        );
        assert_eq!(quoted("not a quote"), None);

        let out = render("> > cited", 40);
        let line = out.first().expect("a quote must render");
        assert_eq!(line.matches('│').count(), 2, "expected two bars: {line:?}");
        assert!(
            !line.contains('>'),
            "the inner marker was printed as text: {line:?}"
        );
    }

    /// The `+` and the `-` are diff's own; the colour only reads them back. That
    /// is what keeps the block legible under NO_COLOR.
    #[test]
    fn a_diff_block_colours_by_the_sign_the_text_already_carries() {
        assert_eq!(diff_ink("+added line"), ADDED);
        assert_eq!(diff_ink("-removed line"), REMOVED);
        assert_eq!(diff_ink("@@ -1,4 +1,6 @@"), ACCENT);
        assert_eq!(diff_ink(" context"), MUTED);
        // File headers are not one-line changes, whatever they start with.
        assert_eq!(diff_ink("+++ b/main.rs"), MUTED);
        assert_eq!(diff_ink("--- a/main.rs"), MUTED);

        let out = render("```diff\n+one\n-two\n```", 40);
        let body: String = out.join("\n");
        assert!(
            body.contains("+one") && body.contains("-two"),
            "the signs must survive: {body}"
        );
        assert!(
            body.contains("diff"),
            "the language label belongs in the top edge"
        );
    }

    /// Every other language keeps one ink, or a stray `-` in a shell script
    /// would read as a deletion.
    #[test]
    fn only_a_diff_block_is_read_for_signs() {
        let out = render("```bash\n-rf something\n```", 40);
        let body = out.join("\n");
        assert!(
            body.contains("-rf something"),
            "the shell flag was rewritten: {body}"
        );
    }
}
