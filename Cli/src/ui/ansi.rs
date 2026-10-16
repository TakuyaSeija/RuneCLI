//! Colour, measurement, wrapping, and the small escape sequences the screen is
//! built from.
//!
//! # Measuring, not counting
//!
//! Terminal layout is in **columns**, and a column is not a `char`: a CJK
//! ideograph occupies two, a combining accent occupies none, and an escape
//! sequence occupies none while taking many bytes. Laying out with `len()` is
//! what produces a box whose right edge wanders. [`width`] is the one measuring
//! function; every layout decision in this program goes through it.
//!
//! # One palette
//!
//! Colours are named for what they *mean* — `ACCENT`, `MUTED`, `ADDED` — not for
//! what they look like, and they are not tones at all: an [`Ink`] resolves
//! against the palette [`super::theme`] computed for *this* terminal's
//! background, at the moment it is printed.
//!
//! # One palette, three encodings
//!
//! There is still exactly one palette. What changes with the terminal is only
//! how a tone is *written down*: truecolour where it is supported, the 256-colour
//! cube where it is not, the sixteen where even that is missing, and nothing at
//! all when [`NO_COLOR`](https://no-color.org) is set.
//!
//! This file used to say a fallback was "a second thing to keep in step", and
//! that objection is answered rather than ignored: the palette is computed in
//! RGB exactly as before and only [`Rgb`]'s `Display` differs, so there is one
//! set of colours and one quantiser, not two designs. The reason it is worth
//! doing at all is that this program is going to strangers' terminals, where
//! "truecolour or nothing" is a promise nobody made.

use super::theme;

pub const RESET: &str = "\x1b[0m";
pub const REVERSE: &str = "\x1b[7m";
pub const BOLD: &str = "\x1b[1m";
pub const ITALIC: &str = "\x1b[3m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl std::fmt::Display for Rgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&encode(*self, depth(), false))
    }
}

/// How a tone is written down at a given depth.
///
/// Split out from `Display` so all four can be tested in one process: the depth
/// itself is resolved once and fixed for the life of the program, which is right
/// for the program and impossible to test through.
pub fn encode(tone: Rgb, depth: Depth, background: bool) -> String {
    let plane = if background { 48 } else { 38 };
    match depth {
        Depth::None => String::new(),
        Depth::True => format!("\x1b[{plane};2;{};{};{}m", tone.0, tone.1, tone.2),
        Depth::Ansi256 => format!("\x1b[{plane};5;{}m", tone.to_256()),
        Depth::Ansi16 => format!("\x1b[{}m", tone.to_16() + if background { 10 } else { 0 }),
    }
}

/// How many colours this terminal can actually be told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// No colour at all — `NO_COLOR`, or a terminal that says it is `dumb`.
    None,
    Ansi16,
    Ansi256,
    True,
}

static DEPTH: std::sync::RwLock<Option<Depth>> = std::sync::RwLock::new(None);

/// What the environment says, resolved once.
///
/// Order matters and is the settled convention: a refusal beats a capability,
/// an explicit `COLORTERM` beats an inferred `TERM`, and the floor is sixteen
/// rather than none — a terminal that says nothing is far more likely to be a
/// normal one than a teletype.
pub fn depth() -> Depth {
    if let Ok(guard) = DEPTH.read() {
        if let Some(d) = *guard {
            return d;
        }
    }
    // Tests must not depend on the terminal they happen to run in: an empty
    // `TERM` made colour assertions fail on one machine and pass on another.
    #[cfg(test)]
    let detected = Depth::True;
    #[cfg(not(test))]
    let detected = {
        let var = |k: &str| std::env::var(k).unwrap_or_default().to_ascii_lowercase();
        let term = var("TERM");
        if !std::env::var("NO_COLOR").unwrap_or_default().is_empty() || term == "dumb" {
            Depth::None
        } else if super::runtime_configuration().text("ui.color") != "auto" {
            match super::runtime_configuration().text("ui.color").as_str() {
                "none" => Depth::None,
                "ansi16" => Depth::Ansi16,
                "ansi256" => Depth::Ansi256,
                _ => Depth::True,
            }
        } else if matches!(var("COLORTERM").as_str(), "truecolor" | "24bit") {
            Depth::True
        } else if term.contains("256color") || term.contains("direct") {
            Depth::Ansi256
        } else {
            Depth::Ansi16
        }
    };
    if let Ok(mut guard) = DEPTH.write() {
        *guard = Some(detected);
    }
    detected
}

/// Fix the depth, ignoring the environment. For `--no-color`, and for tests,
/// which must not depend on the machine they run on.
pub fn set_depth(d: Depth) {
    if let Ok(mut guard) = DEPTH.write() {
        *guard = Some(d);
    }
    inks_changed();
}

pub fn invalidate_depth() {
    if let Ok(mut guard) = DEPTH.write() {
        *guard = None;
    }
    inks_changed();
}

/// Bumped after every change to the palette or the depth: cached escapes from
/// an older generation are stale.
static INK_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Every role's escape, encoded once per generation.
static INKS: std::sync::RwLock<(u64, Vec<String>)> = std::sync::RwLock::new((0, Vec::new()));

/// Call after writing a new palette or depth.
pub fn inks_changed() {
    INK_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

impl Rgb {
    /// The nearest entry of the xterm 256-colour palette.
    ///
    /// The 6×6×6 cube and the 24-step grey ramp are searched separately and the
    /// closer of the two wins, because a near-neutral tone — which most of this
    /// palette is — lands far better on the greys than on the cube, and the cube
    /// alone would give the whole screen a faint colour cast.
    fn to_256(self) -> u8 {
        let Rgb(r, g, b) = self;
        let step = |v: u8| -> u8 {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            LEVELS
                .iter()
                .enumerate()
                .min_by_key(|(_, l)| (**l as i32 - v as i32).abs())
                .map(|(i, _)| i as u8)
                .unwrap_or(0)
        };
        let (ri, gi, bi) = (step(r), step(g), step(b));
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let cube = Rgb(
            LEVELS[ri as usize],
            LEVELS[gi as usize],
            LEVELS[bi as usize],
        );

        let mean = (r as u32 + g as u32 + b as u32) / 3;
        let grey_index = ((mean as i32 - 8) / 10).clamp(0, 23) as u8;
        let grey_value = (8 + grey_index as u32 * 10).min(238) as u8;
        let grey = Rgb(grey_value, grey_value, grey_value);

        if distance(self, grey) < distance(self, cube) {
            232 + grey_index
        } else {
            16 + 36 * ri + 6 * gi + bi
        }
    }

    /// The nearest of the sixteen, as an SGR foreground code.
    ///
    /// **Hue first, then brightness** — deliberately not the nearest tone by
    /// [`distance`]. Distance answers "which of these sixteen looks most like
    /// this colour", and at this depth that is the wrong question: the palette's
    /// whole job is to keep *meanings* apart, and a pale blue is arithmetically
    /// closer to grey than it is to blue.
    ///
    /// Measured before this was rewritten, which is how the fault was found:
    /// the accent `Rgb(125, 167, 239)` landed on **white**, losing its hue
    /// entirely, and `Role::Warn` landed on the same bright red as `Role::Bad` —
    /// so two of the three permission modes were drawn identically, which is the
    /// one thing three separate inks exist to prevent. Green is weighted four
    /// times in [`distance`], and a light blue carries a great deal of green.
    ///
    /// Approximate by construction either way: these sixteen slots are whatever
    /// the user themselves configured, so this can only aim at the conventional
    /// values.
    fn to_16(self) -> u16 {
        // Below this much chroma a tone carries no hue worth keeping and is a
        // neutral instead. The gap it sits in is wide: this palette builds its
        // neutrals at 0.02–0.09 saturation and its hues at 0.45 and up.
        const NEUTRAL: f32 = 0.15;
        // The four neutrals, with the luminance each conventionally sits at.
        const NEUTRALS: [(f32, u16); 4] = [(0.0, 30), (0.09, 90), (0.40, 37), (1.0, 97)];
        // Normal and bright, by hue family. The angle is the *bright* variant's:
        // xterm's "normal yellow" is a brown, and taking its hue would drag the
        // family away from the name it is chosen by.
        const FAMILIES: [(f32, u16, u16); 6] = [
            (0.0, 31, 91),
            (60.0, 33, 93),
            (120.0, 32, 92),
            (180.0, 36, 96),
            (240.0, 34, 94),
            (300.0, 35, 95),
        ];
        // The valley between the two rings, not a tuned number: the normal six
        // sit near 0.03–0.09 luminance and the bright six near 0.15–0.5.
        const RING: f32 = 0.22;

        let light = theme::luminance(self);
        if theme::chroma_of(self) < NEUTRAL {
            return NEUTRALS
                .iter()
                .min_by(|(a, _), (b, _)| (a - light).abs().total_cmp(&(b - light).abs()))
                .map(|(_, code)| *code)
                .unwrap_or(37);
        }
        let hue = theme::hue_of(self);
        let (_, dim, bright) = FAMILIES
            .iter()
            .min_by(|(a, ..), (b, ..)| arc(hue, *a).total_cmp(&arc(hue, *b)))
            .copied()
            .unwrap_or(FAMILIES[0]);
        if light < RING {
            dim
        } else {
            bright
        }
    }
}

/// The shorter way round the colour wheel, in degrees.
fn arc(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}

/// Squared distance in sRGB, weighted the way the eye is: green carries most of
/// the luminance, blue almost none. Enough for "which of sixteen", which is the
/// only question asked of it.
fn distance(a: Rgb, b: Rgb) -> i32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2);
    2 * d(a.0, b.0) + 4 * d(a.1, b.1) + d(a.2, b.2)
}

/// What a colour is *for*. The screen names these and never a literal tone, so
/// the same drawing code is legible on a black terminal, a white one, and the
/// translucent one this program is actually used in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Accent,
    Text,
    User,
    Muted,
    Faint,
    Rule,
    Good,
    Bad,
    Warn,
    Think,
    Code,
    // The six kinds of token inside a code block. A code block painted in one
    // colour is a wall, and the eye reads a wall as prose; these are what let it
    // read as code. See [`super::highlight`] for how a line is sorted into them.
    CodeKeyword,
    CodeType,
    CodeFunc,
    CodeString,
    CodeNumber,
    CodeComment,
    Math,
    Quote,
}

impl Role {
    pub const ALL: [Role; 19] = [
        Role::Accent,
        Role::Text,
        Role::User,
        Role::Muted,
        Role::Faint,
        Role::Rule,
        Role::Good,
        Role::Bad,
        Role::Warn,
        Role::Think,
        Role::Code,
        Role::CodeKeyword,
        Role::CodeType,
        Role::CodeFunc,
        Role::CodeString,
        Role::CodeNumber,
        Role::CodeComment,
        Role::Math,
        Role::Quote,
    ];
    pub const COUNT: usize = Role::ALL.len();
}

/// A role, ready to be written into a line.
///
/// # Why this is not an `Rgb` constant
///
/// The palette is not known until the terminal has been asked what colour it is
/// (see [`super::theme`]), and that answer arrives after this program is
/// compiled. `Ink` resolves at the moment it is printed, so every existing
/// `{MUTED}` in a format string keeps working and none of them can be a literal
/// tone chosen against somebody else's background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink(pub Role);

/// Thousands of inks are printed per relayout. Each used to take two locks,
/// copy the palette and allocate a fresh escape (audit T8); now it is one
/// generation check and a borrowed `&str`.
impl std::fmt::Display for Ink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let generation = INK_GENERATION.load(std::sync::atomic::Ordering::Acquire);
        if let Ok(cache) = INKS.read() {
            if cache.0 == generation {
                if let Some(escape) = cache.1.get(self.0 as usize) {
                    return f.write_str(escape);
                }
            }
        }
        let depth = depth();
        let palette = super::theme::palette();
        let encoded: Vec<String> = Role::ALL
            .iter()
            .map(|role| encode(palette.of(*role), depth, false))
            .collect();
        let escape = encoded[self.0 as usize].clone();
        if let Ok(mut cache) = INKS.write() {
            *cache = (generation, encoded);
        }
        f.write_str(&escape)
    }
}

// The design, as meanings. Two names sharing a role is deliberate: `ADDED` and
// `OK` are the same idea in two vocabularies, and giving them separate tones
// would be two things to keep in step for no gain.
pub const ACCENT: Ink = Ink(Role::Accent);
pub const TEXT: Ink = Ink(Role::Text);
/// What the user themselves typed — the strongest ink on the screen, because
/// telling one's own words from the model's is the first thing a reader does.
pub const USER: Ink = Ink(Role::User);
pub const MUTED: Ink = Ink(Role::Muted);
pub const FAINT: Ink = Ink(Role::Faint);
/// Rules, frames, and table borders.
pub const RULE: Ink = Ink(Role::Rule);
pub const ADDED: Ink = Ink(Role::Good);
pub const OK: Ink = Ink(Role::Good);
pub const REMOVED: Ink = Ink(Role::Bad);
pub const WARN: Ink = Ink(Role::Warn);
pub const THINK: Ink = Ink(Role::Think);
/// Inline code and fenced blocks: warm, so a snippet reads as a different kind
/// of thing from the prose around it without competing with the accent.
pub const CODE: Ink = Ink(Role::Code);
/// `fn`, `return`, `if` — the words the language itself owns.
pub const CODE_KEYWORD: Ink = Ink(Role::CodeKeyword);
/// `String`, `Config`, `SOME_CONSTANT` — a name for a kind.
pub const CODE_TYPE: Ink = Ink(Role::CodeType);
/// A name called with `(`.
pub const CODE_FUNC: Ink = Ink(Role::CodeFunc);
/// A literal string, in either quoting style.
pub const CODE_STRING: Ink = Ink(Role::CodeString);
/// A number, in any base the language spells.
pub const CODE_NUMBER: Ink = Ink(Role::CodeNumber);
/// A comment, which is not code at all and is dimmed accordingly.
pub const CODE_COMMENT: Ink = Ink(Role::CodeComment);
/// Maths, which is neither prose nor code and must not be mistaken for either.
pub const MATH: Ink = Ink(Role::Math);
pub const QUOTE: Ink = Ink(Role::Quote);

/// Columns this string occupies once printed.
///
/// # Why this is not `chars().count()`
///
/// Three things break that, and all three were on screen in a real session: an
/// escape sequence takes bytes and no columns, a CJK ideograph takes two, and an
/// emoji takes two while living outside the range most width tables remember.
/// The last one is what produced a visibly crooked frame — see [`char_width`].
pub fn width(s: &str) -> usize {
    let mut measure = Measure::default();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars, &mut None);
            continue;
        }
        measure.add(c);
    }
    measure.total
}

/// Columns, counted one character at a time — the one definition of width,
/// shared by [`width`] and the column slicers, so measuring a growing prefix
/// costs one step per character instead of a pass over the prefix (S6).
#[derive(Default)]
struct Measure {
    total: usize,
    previous_narrow: bool,
    after_zwj: bool,
    regional: bool,
}

fn regional(c: char) -> bool {
    matches!(c as u32, 0x1f1e6..=0x1f1ff)
}

impl Measure {
    /// Count one visible character; returns the total so far.
    fn add(&mut self, c: char) -> usize {
        // A zero-width joiner fuses what follows onto the previous glyph:
        // A base joined to a second pictograph is one two-column glyph, not two. A terminal that cannot draw the
        // sequence still draws it inside the columns the first piece claimed, so
        // counting the joined parts as zero is the reading that does not shove
        // the rest of the line sideways.
        if c == '\u{200d}' {
            self.after_zwj = true;
            return self.total;
        }
        if self.after_zwj {
            self.after_zwj = false;
            self.previous_narrow = false;
            return self.total;
        }
        // U+FE0F asks for the *emoji* form of the character before it, which is
        // two columns where the text form was one. It has no width of its own;
        // what it really does is widen its neighbour.
        if c == '\u{fe0f}' || c == '\u{20e3}' {
            if self.previous_narrow {
                self.total += 1;
                self.previous_narrow = false;
            }
            return self.total;
        }
        if regional(c) {
            if self.regional {
                self.regional = false;
                return self.total;
            }
            self.regional = true;
            self.previous_narrow = false;
            self.total += 2;
            return self.total;
        }
        if matches!(c as u32, 0x1f3fb..=0x1f3ff) {
            return self.total;
        }
        self.regional = false;
        let w = char_width(c);
        self.previous_narrow = w == 1;
        self.total += w;
        self.total
    }
}

/// One cell of a drawn row: the character in it and the colour codes in force
/// on it (everything said since the last reset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub style: String,
    pub ch: char,
    pub width: usize,
}

/// A row as the terminal will hold it, cell by cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cells {
    pub cells: Vec<Cell>,
    /// The colour codes in force after the last cell: what an erase to the end
    /// of the row paints with.
    pub end: String,
}

impl Cells {
    /// The columns the row takes.
    pub fn width(&self) -> usize {
        self.cells.iter().map(|cell| cell.width).sum()
    }
}

/// A row as cells, so two frames of it can be compared column by column and
/// only what changed sent. `None` for a row whose columns a terminal may count
/// otherwise than [`width`] does — a character joined from several (an emoji
/// sequence, an accent written apart), drawn in one column or two as the
/// terminal pleases — or that holds anything but colour codes (a link). Such a
/// row is sent whole, from its first column, so the terminal's own reading of
/// it places the rest of it.
pub fn cells(s: &str) -> Option<Cells> {
    let mut row = Cells::default();
    let mut measure = Measure::default();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut code = String::new();
            skip_escape(&mut chars, &mut Some(&mut code));
            if !(code.starts_with("\x1b[") && code.ends_with('m')) {
                return None;
            }
            if code == RESET {
                row.end.clear();
            } else {
                row.end.push_str(&code);
            }
            continue;
        }
        let before = measure.total;
        let width = measure.add(c) - before;
        // Joined onto the glyph before it, or widening it (U+FE0F).
        if width == 0 || c == '\u{fe0f}' {
            return None;
        }
        row.cells.push(Cell {
            style: row.end.clone(),
            ch: c,
            width,
        });
    }
    Some(row)
}

/// Cells back into what a terminal draws: colour codes where they change, and
/// a reset after the last cell, so nothing written next inherits them.
pub fn render_cells(cells: &[Cell]) -> String {
    let mut out = String::new();
    let mut current: Option<&str> = None;
    for cell in cells {
        if current != Some(cell.style.as_str()) {
            out.push_str(RESET);
            out.push_str(&cell.style);
            current = Some(cell.style.as_str());
        }
        out.push(cell.ch);
    }
    out.push_str(RESET);
    out
}

/// `top` drawn over `base` from column `from`: the columns it covers give way
/// to it, and every other column of `base` keeps its character and colour —
/// a label over a row, not a row replaced by a label. A wide glyph cut by an
/// edge leaves blanks in its columns outside `top`. A row whose cells cannot
/// be told apart (a link, a joined emoji) keeps its text, without colour.
pub fn overlay(base: &str, from: usize, top: &str) -> String {
    let to = from + width(top);
    let row = cells(base)
        .or_else(|| cells(&plain(base)))
        .unwrap_or_default();
    let blank = |style: &str| Cell {
        style: style.to_string(),
        ch: ' ',
        width: 1,
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut col = 0;
    for cell in row.cells {
        let end = col + cell.width;
        if end <= from {
            left.push(cell);
        } else if col >= to {
            right.push(cell);
        } else {
            left.extend((col..from).map(|_| blank(&cell.style)));
            right.extend((to.max(col)..end).map(|_| blank(&cell.style)));
        }
        col = end;
    }
    left.extend((col..from).map(|_| blank("")));
    format!(
        "{}{top}{RESET}{}",
        render_cells(&left),
        render_cells(&right)
    )
}

/// The same line with every escape sequence taken out of it.
///
/// One copy, for the tests of every module that draws: three of them had their
/// own, which is three chances for a rendering test to be measuring a slightly
/// different notion of "what is actually on the screen".
pub fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut sink: Option<&mut String> = None;
            skip_escape(&mut chars, &mut sink);
            continue;
        }
        out.push(c);
    }
    out
}

/// Consume one escape sequence, appending it to `keep` when the caller is
/// rebuilding the string.
///
/// The two kinds are terminated differently, and treating them alike is exactly
/// how a hyperlink's URL ends up counted as visible text: a CSI (`ESC [ … m`)
/// ends at a byte in `@`..`~`, while an OSC (`ESC ] … `) runs until a BEL or a
/// string terminator — and its payload is full of letters that look like the end
/// of a CSI but are not.
fn skip_escape<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    keep: &mut Option<&mut String>,
) {
    let Some(kind) = chars.next() else { return };
    if let Some(out) = keep.as_deref_mut() {
        out.push('\x1b');
        out.push(kind);
    }
    match kind {
        ']' => {
            // OSC: ends at BEL, or at ESC \.
            while let Some(c) = chars.next() {
                if let Some(out) = keep.as_deref_mut() {
                    out.push(c);
                }
                if c == '\u{7}' {
                    return;
                }
                if c == '\x1b' {
                    if let Some(next) = chars.next() {
                        if let Some(out) = keep.as_deref_mut() {
                            out.push(next);
                        }
                    }
                    return;
                }
            }
        }
        '[' | '(' | ')' | '#' => {
            for c in chars.by_ref() {
                if let Some(out) = keep.as_deref_mut() {
                    out.push(c);
                }
                let final_start = if kind == '[' { '\u{40}' } else { '\u{30}' };
                if (final_start..='\u{7e}').contains(&c) {
                    return;
                }
            }
        }
        // A two-character sequence: ESC \ and friends are already complete.
        _ => {}
    }
}

/// Columns one character occupies.
///
/// # The bug this table was rewritten for
///
/// The emoji ranges used to be one line — `0x1f300..=0x1f9ff` — which is where
/// the *pictographs* live and is not where the emoji people actually type live.
/// U+2705, U+274C and U+26A0 are all below U+2E80, and all take two
/// columns on screen, all counted as one.
///
/// In a split view every such character pushed the divider one column right, so
/// the vertical rule between the transcript and the detail panel bent around
/// exactly the lines that had a tick or a cross in them. It read as a broken
/// layout engine. It was an arithmetic error in this function.
///
/// The ranges below are Unicode's East Asian *Wide* and *Fullwidth* classes plus
/// the emoji-presentation ranges — not the whole of the tables, but every range a
/// terminal session actually meets, and short enough to read in one sitting.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || cp == 0x7f {
        return 0;
    }
    if cp < 0x80 {
        return 1;
    }
    // The table below is the only answer, on purpose: the C library's
    // `wcwidth` gave answers that depended on the user's locale (owner rule 8)
    // and needed a process-wide, thread-unsafe `setlocale` (audit H7).
    table_width(cp)
}

fn is_combining_or_zero_width(cp: u32) -> bool {
    matches!(
        cp,
        // Combining marks, diacritics, and format characters sit on, or between,
        // other glyphs and claim no column of their own in the terminal.
        0x0300..=0x036f
        | 0x0483..=0x0489
        | 0x0591..=0x05bd
        | 0x05bf
        | 0x05c1..=0x05c2
        | 0x05c4..=0x05c5
        | 0x05c7
        | 0x0610..=0x061a
        | 0x064b..=0x065f
        | 0x0670
        | 0x06d6..=0x06dc
        | 0x06df..=0x06e4
        | 0x06e7..=0x06e8
        | 0x06ea..=0x06ed
        | 0x0711
        | 0x0730..=0x074a
        | 0x07a6..=0x07b0
        | 0x08d3..=0x08e1
        | 0x08e3..=0x0903
        | 0x093a..=0x093c
        | 0x093e..=0x094f
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0981..=0x0983
        | 0x09bc
        | 0x09be..=0x09cd
        | 0x09d7
        | 0x09e2..=0x09e3
        | 0x0a01..=0x0a03
        | 0x0a3c
        | 0x0a3e..=0x0a42
        | 0x0a47..=0x0a48
        | 0x0a4b..=0x0a4d
        | 0x0a51
        | 0x0a70..=0x0a71
        | 0x0a75
        | 0x0a81..=0x0a83
        | 0x0abc
        | 0x0abe..=0x0acd
        | 0x0ae2..=0x0ae3
        | 0x0b01..=0x0b03
        | 0x0b3c
        | 0x0b3e..=0x0b4d
        | 0x0b56..=0x0b57
        | 0x0b62..=0x0b63
        | 0x0b82
        | 0x0bbe..=0x0bcd
        | 0x0bd7
        | 0x0c00..=0x0c04
        | 0x0c3e..=0x0c4d
        | 0x0c55..=0x0c56
        | 0x0c62..=0x0c63
        | 0x0c81..=0x0c83
        | 0x0cbc
        | 0x0cbe..=0x0ccd
        | 0x0cd5..=0x0cd6
        | 0x0ce2..=0x0ce3
        | 0x0d00..=0x0d03
        | 0x0d3b..=0x0d3c
        | 0x0d3e..=0x0d4d
        | 0x0d57
        | 0x0d62..=0x0d63
        | 0x0d82..=0x0d83
        | 0x0dca
        | 0x0dcf..=0x0ddf
        | 0x0df2..=0x0df3
        | 0x0e31
        | 0x0e34..=0x0e3a
        | 0x0e47..=0x0e4e
        | 0x0eb1
        | 0x0eb4..=0x0ebc
        | 0x0ec8..=0x0ecd
        | 0x0f18..=0x0f19
        | 0x0f35
        | 0x0f37
        | 0x0f39
        | 0x0f71..=0x0f84
        | 0x0f86..=0x0f87
        | 0x0f8d..=0x0fbc
        | 0x0fc6
        | 0x102b..=0x103e
        | 0x1056..=0x1060
        | 0x1062..=0x1064
        | 0x1067..=0x106d
        | 0x1071..=0x1074
        | 0x1082..=0x108d
        | 0x108f
        | 0x109a..=0x109d
        | 0x17b4..=0x17d3
        | 0x17dd
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x200b..=0x200f
        | 0x202a..=0x202e
        | 0x2060..=0x206f
        | 0x20d0..=0x20ff
        | 0xa9e5
        | 0xaa7b..=0xaa7d
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f
        | 0xe0001..=0xe007f
        | 0xe0100..=0xe01ef
    )
}

fn table_width(cp: u32) -> usize {
    if is_combining_or_zero_width(cp) {
        return 0;
    }
    match cp {

        // Emoji below the pictograph block — the half that was missing, and the
        // half a coding session is full of.
        0x231a..=0x231b         // watch hourglass
        | 0x23e9..=0x23ec       // fast-direction controls
        | 0x23f0                // alarm clock
        | 0x23f3                // hourglass
        | 0x25fd..=0x25fe       // medium-small squares
        | 0x2614..=0x2615       // umbrella hot beverage
        | 0x2648..=0x2653       // the zodiac
        | 0x267f                // wheelchair
        | 0x2693                // anchor
        | 0x26a1                // high voltage
        | 0x26aa..=0x26ab       // circle circle
        | 0x26bd..=0x26be       // ball ball
        | 0x26c4..=0x26c5       // snowman cloud
        | 0x26ce                // ophiuchus
        | 0x26d4                // no entry
        | 0x26ea                // building
        | 0x26f2..=0x26f3       // fountain flag
        | 0x26f5                // sailboat
        | 0x26fa                // tent
        | 0x26fd                // fuel pump
        | 0x2705                // check mark — the one that bent the divider
        | 0x270a..=0x270b       // raised fist raised hand
        | 0x2728                // sparkles
        | 0x274c                // cross mark
        | 0x274e                // cross mark
        | 0x2753..=0x2755       // question mark question mark exclamation
        | 0x2757                // exclamation
        | 0x2795..=0x2797       // plus minus division
        | 0x27b0                // curly loop
        | 0x27bf                // double loop
        | 0x2b1b..=0x2b1c       // large squares
        | 0x2b50                // star
        | 0x2b55 => 2,          // circle

        // East Asian Wide and Fullwidth.
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xa960..=0xa97f
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f004
        | 0x1f0cf
        | 0x1f18e
        | 0x1f191..=0x1f19a
        | 0x1f200..=0x1f2ff
        | 0x1f300..=0x1f9ff
        | 0x1fa70..=0x1faff
        | 0x20000..=0x2fffd
        | 0x30000..=0x3fffd => 2,

        _ => 1,
    }
}

/// Make a foreign string safe to lay out.
///
/// # The bug this exists for: a carriage return punctures the split
///
/// A command's output comes back through a **pty**, and a pty in its normal mode
/// turns every `\n` into `\r\n`. Splitting that on `\n` leaves a `\r` sitting at
/// the end of every line. [`width`] counts it as nothing, so the layout believes
/// the line fits — and then the terminal *acts* on it, throwing the cursor back
/// to column one, where everything printed after it (including the divider
/// between the transcript and the side panel) lands on top of the text. That is
/// exactly what the owner photographed: `ls` output running clean through the
/// divider, with `│` scattered at random columns to the right of it.
///
/// The rule this function enforces is the one the whole layout rests on: **what
/// is measured must be what is drawn.** Anything that moves the cursor without
/// occupying a column breaks that, so:
///
///   * a tab becomes four spaces. Its real width depends on the column it lands
///     in and on the terminal's tab stops, and a composed line knows neither;
///   * every other C0 control and `DEL` is dropped — `\r`, `\x08`, the bell;
///   * an escape sequence survives **only** if it is colour (`ESC [ … m`) or a
///     hyperlink (`OSC 8`). Cursor moves, screen clears and mode changes are
///     things a program prints and a frame cannot survive.
///
/// Newlines are left alone: they are how the callers split text into rows in the
/// first place, and none of them prints one.
pub fn tame(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                let mut kept = String::new();
                let mut sink = Some(&mut kept);
                skip_escape(&mut chars, &mut sink);
                // `ESC [ … m` is colour; `ESC ] 8 ; …` is a hyperlink. Both are
                // ours or harmless. Nothing else may reach the terminal.
                let colour = kept.starts_with("\x1b[") && kept.ends_with('m');
                let link = kept.starts_with("\x1b]8;");
                if colour || link {
                    out.push_str(&kept);
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => out.push(c),
        }
    }
    out
}

/// Plain editor text and its display-byte → source-byte boundaries. Tabs keep
/// their four displayed spaces; invisible controls/escapes cannot shift a click
/// into a different source character. Unlike output, input cannot supply styles.
pub fn editor_text(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut source = vec![0];
    let consumed = std::cell::Cell::new(0);
    let mut chars = s
        .char_indices()
        .map(|(at, c)| {
            consumed.set(at + c.len_utf8());
            c
        })
        .peekable();
    while let Some(c) = chars.next() {
        let before = consumed.get() - c.len_utf8();
        if c == '\x1b' {
            skip_escape(&mut chars, &mut None);
        }
        let end = consumed.get();
        match c {
            '\x1b' => *source.last_mut().unwrap() = end,
            '\t' => {
                out.push_str("    ");
                source.extend([before, before, before, end]);
            }
            c if c != '\n' && ((c as u32) < 0x20 || c as u32 == 0x7f) => {
                *source.last_mut().unwrap() = end;
            }
            c => {
                out.push(c);
                source.extend((1..=c.len_utf8()).map(|n| end - c.len_utf8() + n));
            }
        }
    }
    (out, source)
}

#[cfg(test)]
mod editor_projection_tests {
    use super::*;

    #[test]
    fn projection_is_plain_linear_and_preserves_original_source_boundaries() {
        let input = "a\té\x1b[31m界\x1b[0m\x07\nend";
        let (shown, source) = editor_text(input);
        assert_eq!(shown, "a    é界\nend");
        assert_eq!(source.len(), shown.len() + 1);
        assert!(source.windows(2).all(|pair| pair[0] <= pair[1]));
        for at in (0..=shown.len()).filter(|&at| shown.is_char_boundary(at)) {
            assert!(input.is_char_boundary(source[at]), "display byte {at}");
        }
        assert_eq!(source[5], 2);
        assert_eq!(source[shown.len()], input.len());
        assert_eq!(source[7], input.find('界').unwrap());
    }

    #[test]
    fn emoji_suffixes_flags_and_keycaps_share_one_editing_and_column_boundary() {
        for glyph in ["👍🏽", "🇮🇩", "1️⃣", "👩🏽\u{200d}💻"] {
            assert_eq!(editor_glyphs(glyph).count(), 1, "{glyph}");
            assert_eq!(width(glyph), 2, "{glyph}");
            let text = format!("{glyph}x");
            assert_eq!(byte_at_column(&text, 1), glyph.len(), "{glyph}");
            assert_eq!(byte_at_column(&text, 2), glyph.len(), "{glyph}");
        }
    }

    #[test]
    fn a_column_never_splits_combining_or_joined_glyphs() {
        assert_eq!(byte_at_column("1234", 2), 2);
        assert_eq!(byte_at_column("界x", 1), "界".len());
        assert_eq!(byte_at_column("e\u{301}x", 1), "e\u{301}".len());
        assert_eq!(byte_at_column("👨\u{200d}💻x", 2), "👨\u{200d}💻".len());
        assert_eq!(byte_at_column("short", 200), 5);
    }
}

/// Plain editor glyphs, borrowing their complete source bytes. A combining
/// suffix or ZWJ continuation is never a place to wrap. Width uses the same
/// Measure as terminal layout, not a second per-character sum.
pub fn editor_glyphs(text: &str) -> impl Iterator<Item = (usize, &str, usize)> {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, first) = chars.next()?;
        let mut end = start + first.len_utf8();
        let mut measure = Measure::default();
        measure.add(first);
        while let Some(&(offset, next)) = chars.peek() {
            let suffix =
                matches!(next as u32, 0x1f3fb..=0x1f3ff) || regional(next) && measure.regional;
            if first == '\n'
                || next == '\n'
                || (char_width(next) > 0 && !measure.after_zwj && !suffix)
            {
                break;
            }
            chars.next();
            end = offset + next.len_utf8();
            measure.add(next);
        }
        Some((start, &text[start..end], measure.total))
    })
}

/// A source-byte boundary at a displayed column, using the same Unicode
/// measure as `width`. Combining marks and joined emoji stay with their glyph.
pub fn byte_at_column(row: &str, column: usize) -> usize {
    let mut used = 0;
    for (offset, _, width) in editor_glyphs(row) {
        if used >= column {
            return offset;
        }
        used += width;
    }
    row.len()
}

/// Cut to `cols` columns, keeping escape sequences intact.
pub fn truncate(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut sink = Some(&mut out);
            skip_escape(&mut chars, &mut sink);
            continue;
        }
        let w = char_width(c);
        if used + w > cols.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(RESET);
    out
}

/// Pad to exactly `cols` columns.
pub fn pad(s: &str, cols: usize) -> String {
    let w = width(s);
    if w >= cols {
        let t = truncate(s, cols);
        let tw = width(&t);
        if tw < cols {
            return format!("{t}{}", " ".repeat(cols - tw));
        }
        return t;
    }
    format!("{s}{}", " ".repeat(cols - w))
}

/// Wrap text to `cols`, breaking on spaces where possible and mid-word when a
/// single word is longer than the line — which, in a coding tool, it often is.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut used = 0;
        for word in raw.split_inclusive(' ') {
            let w = width(word);
            if used + w > cols && used > 0 {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if w > cols {
                // A single unbreakable run: chop it at the margin.
                for c in word.chars() {
                    let cw = char_width(c);
                    if used + cw > cols {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                    }
                    line.push(c);
                    used += cw;
                }
                continue;
            }
            line.push_str(word);
            used += w;
        }
        out.push(line);
    }
    out
}

/// The byte offset inside one wrapped row at which a caret at `cursor` sits.
///
/// Wrapping hands back rows tagged with the byte offset they start at, so a
/// caret's position *within* a row is its own offset minus the row's. Two
/// clamps, both necessary: one for a caret that has moved past the end of the
/// row it is on, and one walking back to a character boundary, because an offset
/// in the middle of a multi-byte glyph cannot be drawn or sliced.
pub fn caret_in_row(row: &str, row_start: usize, cursor: usize) -> usize {
    let cut = cursor.saturating_sub(row_start).min(row.len());
    (0..=cut)
        .rev()
        .find(|&i| row.is_char_boundary(i))
        .unwrap_or(0)
}

/// The visible columns in `[from, to)`, with terminal styling removed.
///
/// Selection and layout share [`width`]'s definition of a column. Byte slicing
/// would split UTF-8; character counting would split a wide glyph across two
/// cells. This is the one inverse operation Rune needs.
pub fn slice(s: &str, from: usize, to: usize) -> String {
    if from >= to {
        return String::new();
    }
    let plain = plain(s);
    let mut out = String::new();
    let mut measure = Measure::default();
    for c in plain.chars() {
        let before = measure.total;
        let after = measure.add(c);
        if (after > from && before < to) || (after == before && before > from && before <= to) {
            out.push(c);
        }
    }
    out
}

/// Reverse the visible columns in `[from, to)` without losing the line's own
/// colours after the selected range.
pub fn select(s: &str, from: usize, to: usize) -> String {
    if from >= to {
        return s.to_string();
    }
    let mut out = String::new();
    let mut active = String::new();
    let mut measure = Measure::default();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let mut kept = String::new();
            let mut sink = Some(&mut kept);
            skip_escape(&mut chars, &mut sink);
            out.push_str(&kept);
            if kept == RESET {
                active.clear();
            } else if kept.starts_with("\x1b[") && kept.ends_with('m') {
                active.push_str(&kept);
            }
            continue;
        }
        let before = measure.total;
        let after = measure.add(c);
        let selected =
            (after > from && before < to) || (after == before && before > from && before <= to);
        if selected {
            out.push_str(REVERSE);
            out.push(c);
            out.push_str(RESET);
            out.push_str(&active);
        } else {
            out.push(c);
        }
    }
    out
}

/// A clickable link, where the terminal supports OSC 8. Terminals that do not
/// simply show the text, so there is no fallback to write.
pub fn link(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Move the cursor to a 1-based row and column.
pub fn at(row: u16, col: u16) -> String {
    format!("\x1b[{row};{col}H")
}

pub const CLEAR_LINE: &str = "\x1b[2K";
pub const CLEAR_SCREEN: &str = "\x1b[2J";

#[cfg(test)]
mod tests {

    #[test]
    fn an_overlay_covers_only_its_columns_and_keeps_the_colours_around_it() {
        let base = "\x1b[31mred text\x1b[0m plain 漢字 end".to_string();
        let out = overlay(&base, 2, "XY");
        assert_eq!(plain(&out), "reXYtext plain 漢字 end");
        assert!(out.starts_with(&format!("{RESET}\x1b[31mre")), "{out:?}");
        assert_eq!(width(&out), width(&base));
        // A wide glyph cut by an edge leaves blanks in its other column.
        let at = plain(&base).find('漢').unwrap();
        let col = width(&plain(&base)[..at]);
        assert_eq!(
            plain(&overlay(&base, col + 1, "Z")),
            "red text plain  Z字 end"
        );
        // Past the row's end: blanks up to it.
        assert_eq!(plain(&overlay("ab", 4, "Q")), "ab  Q");
    }

    use super::*;

    /// A row read as cells comes back out as the same cells, colours where
    /// they change; a row a terminal may count otherwise is not read at all.
    #[test]
    fn a_row_is_read_as_cells_and_written_back_the_same() {
        let row = "\x1b[31mab\x1b[0m c\x1b[1m中\x1b[44mx";
        let read = cells(row).expect("colour only");
        let shown: Vec<(char, usize, &str)> = read
            .cells
            .iter()
            .map(|cell| (cell.ch, cell.width, cell.style.as_str()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ('a', 1, "\x1b[31m"),
                ('b', 1, "\x1b[31m"),
                (' ', 1, ""),
                ('c', 1, ""),
                ('中', 2, "\x1b[1m"),
                ('x', 1, "\x1b[1m\x1b[44m"),
            ]
        );
        assert_eq!(read.width(), width(row));
        assert_eq!(
            read.end, "\x1b[1m\x1b[44m",
            "what an erase after it paints with"
        );
        let again = cells(&render_cells(&read.cells)).unwrap();
        assert_eq!(again.cells, read.cells);
        assert_eq!(again.end, "", "and nothing leaks past the last cell");
        // Columns a terminal may count its own way, and links: sent whole.
        for whole in [
            "a\u{2764}\u{fe0f}b",
            "e\u{301}",
            "\u{1f468}\u{200d}\u{1f4bb}",
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        ] {
            assert!(cells(whole).is_none(), "{whole:?}");
        }
    }

    /// The ink cache indexes escapes by `role as usize`; `ALL` must list the
    /// roles in exactly that order or every colour would be someone else's.
    #[test]
    fn every_role_sits_at_its_own_index() {
        for (i, role) in Role::ALL.iter().enumerate() {
            assert_eq!(*role as usize, i);
        }
        assert_eq!(
            MUTED.to_string(),
            encode(
                super::super::theme::palette().of(Role::Muted),
                depth(),
                false
            )
        );
    }

    #[test]
    fn a_hyperlink_is_only_as_wide_as_its_label() {
        // The URL is letters, and a naive escape skipper stops at the first of
        // them and counts the address as text.
        assert_eq!(width(&link("https://example.com/a/b", "docs")), 4);
        let cut = truncate(
            &format!("{ACCENT}{}", link("https://example.com", "a long label")),
            6,
        );
        assert!(width(&cut) <= 6);
    }

    #[test]
    fn escape_sequences_occupy_no_columns() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width(&format!("{ACCENT}abc{RESET}")), 3);
        assert_eq!(width(&link("https://x", "abc")), 3);
    }

    #[test]
    fn wide_characters_count_double_and_marks_count_nothing() {
        assert_eq!(width("日本"), 4);
        assert_eq!(width("e\u{0301}"), 1);
    }

    /// The regression that bent the split divider. Every character here sits
    /// below U+2E80, which is where a naive emoji range stops looking.
    #[test]
    fn emoji_below_the_pictograph_block_are_two_columns_wide() {
        for (glyph, name) in [
            ("✅", "check"),
            ("❌", "cross"),
            ("⭐", "star"),
            ("⚡", "bolt"),
        ] {
            assert_eq!(
                width(glyph),
                2,
                "{name} measured narrow, which shoves the frame right"
            );
        }
        // The exact line shape that was crooked on screen.
        assert_eq!(width("- ✅ Membaca"), 12);
    }

    #[test]
    fn a_variation_selector_widens_the_character_it_follows_and_nothing_else() {
        assert_eq!(width("⚔"), 1, "the text form is narrow");
        assert_eq!(width("⚔\u{fe0f}"), 2, "the emoji form is not");
        // Already wide: the selector is redundant and must not add a third column.
        assert_eq!(width("✅\u{fe0f}"), 2);
    }

    #[test]
    fn a_joined_emoji_sequence_occupies_one_glyph_not_three() {
        assert_eq!(width("👨\u{200d}💻"), 2);
        assert_eq!(width("a👨\u{200d}💻b"), 4);
    }

    /// The `\r` that punctured the divider. Everything a pty adds to a line has
    /// to be gone before anything measures it.
    #[test]
    fn what_a_command_prints_cannot_move_the_cursor_out_of_its_column() {
        // A pty's own line ending, which is what `ls` output really looks like.
        assert_eq!(tame("catur-pro\r"), "catur-pro");
        assert_eq!(tame("a\r\nb"), "a\nb");
        // Everything else that moves the cursor without taking a column.
        assert_eq!(tame("a\x08b\x07c"), "abc");
        assert_eq!(tame("a\tb"), "a    b");
        // A screen clear and a cursor jump are dropped; colour and links are not.
        assert_eq!(tame("\x1b[2Ja\x1b[5;9Hb"), "ab");
        assert_eq!(
            tame(&format!("{ACCENT}x{RESET}")),
            format!("{ACCENT}x{RESET}")
        );
        assert_eq!(tame(&link("https://x", "d")), link("https://x", "d"));
        // And what is measured is now what is drawn.
        let raw = "one\ttwo\r";
        assert_eq!(width(&tame(raw)), "one    two".chars().count());
    }

    #[test]
    fn padding_and_truncation_agree_with_the_new_measurement() {
        // The two functions the layout is built from must not disagree with
        // `width`, or the divider bends again one layer up.
        let line = "- ✅ done ❌ failed";
        assert_eq!(width(&pad(line, 40)), 40);
        assert!(width(&truncate(line, 10)) <= 10);
    }

    #[test]
    fn truncation_keeps_colour_intact_and_marks_the_cut() {
        let out = truncate(&format!("{ACCENT}abcdefghij{RESET}"), 5);
        assert!(out.contains('…'));
        assert!(out.ends_with(RESET));
        assert!(width(&out) <= 5);
    }

    #[test]
    fn padding_reaches_exactly_the_asked_for_width() {
        assert_eq!(width(&pad("ab", 6)), 6);
        assert_eq!(width(&pad(&format!("{MUTED}ab{RESET}"), 6)), 6);
        assert_eq!(width(&pad("abcdefgh", 4)), 4);
    }

    #[test]
    fn combining_marks_and_diacritics_claim_zero_width() {
        // Tamil: த (1) + ம (1) + ி (0) + ழ (1) + ் (0) = 3 visual cells.
        assert_eq!(width("தமிழ்"), 3);
        // Telugu: త (1) + ె (0) + ల (1) + ు (0) + గ (1) + ు (0) = 3 visual cells.
        assert_eq!(width("తెలుగు"), 3);
        // Thai: ไทย = 3 visual cells.
        assert_eq!(width("ไทย"), 3);
        // Arabic / Urdu: اردو = 4 visual cells.
        assert_eq!(width("اردو"), 4);
        // Bengali: বাংলা = 2 visual cells.
        assert_eq!(width("বাংলা"), 2);
        // Assamese: অসমীয়া = 4 visual cells.
        assert_eq!(width("অসমীয়া"), 4);
        // Pa'O Karen (Myanmar): ပအိုဝ်ႏဘာႏသာႏ = 5 visual cells.
        assert_eq!(width("ပအိုဝ်ႏဘာႏသာႏ"), 5);
        // Tibetan: བོད་ཡིག = 5 visual cells.
        assert_eq!(width("བོད་ཡིག"), 5);
    }

    #[test]
    fn wrapping_breaks_on_spaces_and_keeps_every_word() {
        let lines = wrap("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| width(l) <= 10));
        let rejoined: String = lines.join("");
        assert!(rejoined.contains("quick"));
        assert!(rejoined.contains("jumps"));
    }

    #[test]
    fn an_unbreakable_run_is_chopped_rather_than_overflowing() {
        let path = "/very/long/path/that/has/no/spaces/at/all/in/it";
        let lines = wrap(path, 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| width(l) <= 12));
    }

    #[test]
    fn blank_lines_survive_wrapping() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn slicing_and_selection_use_terminal_columns_not_bytes() {
        let line = format!("{ACCENT}a日本b{RESET}");
        assert_eq!(slice(&line, 1, 5), "日本");
        let marked = select(&line, 1, 5);
        assert!(marked.contains(REVERSE));
        assert_eq!(plain(&marked), "a日本b");
        assert_eq!(width(&marked), 6);
    }
}

#[cfg(test)]
mod colour_depth_tests {
    use super::*;

    /// The palette is one set of tones; only the encoding changes. These are the
    /// four sentences a terminal can be told, checked directly rather than
    /// through the environment, which the test process cannot own.
    #[test]
    fn a_tone_is_written_down_differently_at_each_depth() {
        let tone = Rgb(215, 95, 95);
        assert_eq!(tone.to_256(), 167, "the cube entry for this tone");
        assert_eq!(tone.to_16(), 91, "the nearest of the sixteen is bright red");
    }

    /// Most of this palette is near-neutral, and the grey ramp holds those far
    /// better than the cube does. Landing them on the cube tints the screen.
    #[test]
    fn a_near_neutral_tone_lands_on_the_grey_ramp_not_the_colour_cube() {
        let index = Rgb(122, 124, 126).to_256();
        assert!((232..=255).contains(&index), "expected a grey, got {index}");
    }

    /// The four sentences, end to end. `NO_COLOR` must produce **nothing** —
    /// not a reset, not an empty sequence — or a pipe still gets escape bytes.
    #[test]
    fn each_depth_writes_the_tone_the_way_that_terminal_understands() {
        let tone = Rgb(215, 95, 95);
        assert_eq!(encode(tone, Depth::True, false), "\x1b[38;2;215;95;95m");
        assert_eq!(encode(tone, Depth::Ansi256, false), "\x1b[38;5;167m");
        assert_eq!(encode(tone, Depth::Ansi16, false), "\x1b[91m");
        assert_eq!(encode(tone, Depth::None, false), "");
    }

    /// The fault this quantiser was rewritten for. Matching by weighted
    /// distance put the accent on **white** and `Warn` on `Bad`'s red, so two of
    /// the three permission modes were drawn identically. What has to survive
    /// the trip down to sixteen is the hue, not the arithmetic nearness.
    #[test]
    fn the_sixteen_keep_hues_apart_rather_than_taking_the_nearest_tone() {
        // The three tones the roles actually solve to on the owner's terminal.
        let accent = Rgb(125, 167, 239);
        let warn = Rgb(225, 151, 25);
        let bad = Rgb(231, 130, 134);
        assert_eq!(accent.to_16(), 94, "a pale accent must still read as blue");
        assert_eq!(warn.to_16(), 93, "warn is a yellow, not a second red");
        assert_eq!(bad.to_16(), 91, "bad keeps the red");
        let inks: std::collections::HashSet<u16> =
            [accent, warn, bad].iter().map(|c| c.to_16()).collect();
        assert_eq!(inks.len(), 3, "two roles collapsed onto one slot");
    }

    /// A dark terminal solves its roles light and a light one solves them dark.
    /// Which ring a tone lands in may change with that; its family may not.
    #[test]
    fn brightness_picks_the_ring_and_hue_picks_the_family() {
        assert_eq!(
            Rgb(0, 0, 140).to_16(),
            34,
            "a dark blue takes the normal slot"
        );
        assert_eq!(
            Rgb(150, 170, 255).to_16(),
            94,
            "a light one takes the bright slot"
        );
    }

    #[test]
    fn black_and_white_survive_the_round_trip_to_sixteen() {
        assert_eq!(Rgb(0, 0, 0).to_16(), 30);
        assert_eq!(Rgb(255, 255, 255).to_16(), 97);
    }
}
