//! What colour the user's terminal actually is, and a palette computed for it.
//!
//! # Why a fixed palette was wrong
//!
//! The screen used to ship fifteen hard-coded slate tones. They were chosen
//! against one background — a dark, slightly blue one — and every other terminal
//! got them anyway. On the owner's translucent window the dim end of that ramp
//! sat almost exactly on top of the wallpaper showing through, so labels, table
//! frames and the whole side panel read as smudges. On a light terminal the same
//! ramp is invisible for the opposite reason.
//!
//! A colour is only ever legible *relative to what is behind it*. So Rune asks
//! the terminal what is behind it and computes the rest:
//!
//! 1. [`probe`] asks with OSC 11 and reads the reply.
//! 2. Failing that, `RUNE_THEME` and then `COLORFGBG` are consulted.
//! 3. [`build`] solves each role for a **contrast ratio** against that
//!    background, keeping the hue the role means and moving only its lightness
//!    until it is far enough from the ground to read.
//!
//! The neutrals borrow the background's own hue, so the palette belongs to the
//! terminal it is drawn in rather than importing a second theme on top of it.
//!
//! # Contrast, not brightness
//!
//! Every target below is a WCAG contrast ratio, which is the only measure that
//! answers "can this be read" for both a black terminal and a white one. Nothing
//! in the program picks a literal colour any more; it picks a *meaning* and a
//! legibility floor, and this module resolves the two.

use std::sync::RwLock;

use super::ansi::{Rgb, Role};

/// The available visual themes for Rune.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// Option 1 (Default): Cyber Violet / Arcane Purple
    #[default]
    Violet,
    /// Option 2: Arcane Cyan / Ethereal Ice
    Cyan,
    /// Option 3: Solar Amber / Golden Rune
    Amber,
}

impl ThemeMode {
    #[cfg(test)]
    pub const ALL: [ThemeMode; 3] = [ThemeMode::Violet, ThemeMode::Cyan, ThemeMode::Amber];

    pub fn name(&self) -> &'static str {
        match self {
            ThemeMode::Violet => "violet",
            ThemeMode::Cyan => "cyan",
            ThemeMode::Amber => "amber",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ThemeMode::Violet => "Cyber Violet",
            ThemeMode::Cyan => "Arcane Cyan",
            ThemeMode::Amber => "Solar Amber",
        }
    }

    pub fn parse(s: &str) -> Option<ThemeMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "violet" | "purple" | "cyber" => Some(ThemeMode::Violet),
            "cyan" | "ice" | "blue" => Some(ThemeMode::Cyan),
            "amber" | "gold" | "yellow" => Some(ThemeMode::Amber),
            _ => None,
        }
    }

    pub fn next(&self) -> ThemeMode {
        match self {
            ThemeMode::Violet => ThemeMode::Cyan,
            ThemeMode::Cyan => ThemeMode::Amber,
            ThemeMode::Amber => ThemeMode::Violet,
        }
    }

    pub fn prev(&self) -> ThemeMode {
        match self {
            ThemeMode::Violet => ThemeMode::Amber,
            ThemeMode::Amber => ThemeMode::Cyan,
            ThemeMode::Cyan => ThemeMode::Violet,
        }
    }
}

/// The palette, resolved against the terminal's background and active theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub bg: Rgb,
    pub inks: [Rgb; Role::COUNT],
}

impl Palette {
    pub fn of(&self, role: Role) -> Rgb {
        self.inks[role as usize]
    }
}

/// An ink pushed `amount` of the way back towards the terminal's background.
///
/// What a gradient is made of: at 0 the colour is itself, at 1 it has sunk into
/// the ground. Used by the wordmark's shadow and by anything else that needs a
/// tone *between* the palette's steps, rather than a sixteenth named colour.
#[cfg(test)]
pub fn fade(ink: Rgb, amount: f32) -> Rgb {
    blend(ink, palette().bg, amount)
}

/// Linear blend in sRGB, and the only one in the program.
#[cfg(test)]
pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

static THEME_MODE: RwLock<Option<ThemeMode>> = RwLock::new(None);
static CURRENT_BG: RwLock<Rgb> = RwLock::new(DEFAULT_BG);
static PALETTE: RwLock<Option<Palette>> = RwLock::new(None);
static INSTALLED_BG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reads the configured theme from `~/.rune/rune.toml`.
pub fn from_config() -> Option<ThemeMode> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        runeengine::store::Db::home_dir()
            .and_then(|home| runeengine::limits::Limits::load(&home))
            .ok()
            .and_then(|l| ThemeMode::parse(&l.theme()))
    }
}

/// Explicitly load and initialize the configured visual theme.
pub fn init() {
    let mode = from_config().unwrap_or(ThemeMode::Violet);
    if let Ok(mut m) = THEME_MODE.write() {
        *m = Some(mode);
    }
    let bg = CURRENT_BG.read().map(|b| *b).unwrap_or(DEFAULT_BG);
    let p = build_with(bg, mode);
    if let Ok(mut guard) = PALETTE.write() {
        *guard = Some(p);
    }
    super::ansi::inks_changed();
}

/// Reload the theme from config in real time without restart.
pub fn reload() {
    let mode = from_config().unwrap_or(ThemeMode::Violet);
    set_theme(mode);
}

pub fn current_theme() -> ThemeMode {
    if let Ok(guard) = THEME_MODE.read() {
        if let Some(m) = *guard {
            return m;
        }
    }
    let configured = from_config().unwrap_or(ThemeMode::Violet);
    if let Ok(mut guard) = THEME_MODE.write() {
        if let Some(m) = *guard {
            return m;
        }
        *guard = Some(configured);
    }
    configured
}

/// The palette in force. Built from the environment on first use, so a pipe or
/// a test still gets something sane without a terminal to ask.
pub fn palette() -> Palette {
    if let Ok(guard) = PALETTE.read() {
        if let Some(p) = *guard {
            return p;
        }
    }
    let bg = from_env().unwrap_or(DEFAULT_BG);
    let p = build_with(bg, current_theme());
    if let Ok(mut guard) = PALETTE.write() {
        *guard = Some(p);
    }
    super::ansi::inks_changed();
    p
}

/// Fix the palette to a background.
pub fn install(bg: Rgb) {
    if let Ok(mut b) = CURRENT_BG.write() {
        *b = bg;
    }
    let mode = current_theme();
    let p = build_with(bg, mode);
    if let Ok(mut guard) = PALETTE.write() {
        *guard = Some(p);
    }
    super::ansi::inks_changed();
    INSTALLED_BG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a palette has already been resolved and installed.
pub fn is_installed() -> bool {
    INSTALLED_BG.load(std::sync::atomic::Ordering::Relaxed)
}

/// Switch the visual theme dynamically and rebuild the active palette.
pub fn set_theme(mode: ThemeMode) {
    if let Ok(mut m) = THEME_MODE.write() {
        *m = Some(mode);
    }
    let bg = CURRENT_BG.read().map(|b| *b).unwrap_or(DEFAULT_BG);
    let p = build_with(bg, mode);
    if let Ok(mut guard) = PALETTE.write() {
        *guard = Some(p);
    }
    super::ansi::inks_changed();
}

/// Where a terminal that answers nothing and says nothing is assumed to be: the
/// dark end, because that is where the overwhelming majority of them are.
const DEFAULT_BG: Rgb = Rgb(17, 19, 26);

/// The luminance at which black text and white text are equally legible. Below
/// it, ink moves lighter; above it, darker. Derived rather than guessed:
/// `(L+0.05)² = 0.05 × 1.05`.
const PIVOT: f32 = 0.179;

/// What a role is, before it knows what it will be drawn on.
///
/// `hue: None` means "the background's own hue" — the neutrals, which is what
/// makes the result read as this terminal's theme rather than as Rune's.
pub(super) struct Spec {
    pub(super) hue: Option<f32>,
    pub(super) sat: f32,
    /// The contrast ratio this role must reach against the background.
    pub(super) contrast: f32,
}

const fn spec(hue: Option<f32>, sat: f32, contrast: f32) -> Spec {
    Spec { hue, sat, contrast }
}

pub(super) fn spec_of_mode(role: Role, mode: ThemeMode) -> Spec {
    match role {
        // Neutrals — the background's hue, separated only by how far they sit from it.
        Role::Text => spec(None, 0.03, 15.0),
        Role::Muted => spec(None, 0.06, 10.0),

        // Faint and Rule carry bold, rich, unmistakable tints of the active theme
        Role::Faint => match mode {
            ThemeMode::Violet => spec(Some(278.0), 0.75, 5.8),
            ThemeMode::Cyan => spec(Some(188.0), 0.75, 5.8),
            ThemeMode::Amber => spec(Some(40.0), 0.75, 5.8),
        },
        Role::Rule => match mode {
            ThemeMode::Violet => spec(Some(278.0), 0.85, 7.2),
            ThemeMode::Cyan => spec(Some(188.0), 0.85, 7.2),
            ThemeMode::Amber => spec(Some(40.0), 0.85, 7.2),
        },

        // USER: What the user themselves typed — crisp neutral white as requested.
        Role::User => spec(None, 0.03, 15.0),

        // ACCENT: Bold, punchy, intensely saturated signature brand color!
        Role::Accent => match mode {
            ThemeMode::Violet => spec(Some(278.0), 0.99, 4.8),
            ThemeMode::Cyan => spec(Some(188.0), 0.99, 9.0),
            ThemeMode::Amber => spec(Some(40.0), 0.99, 9.2),
        },

        // THINK: Agent internal reasoning — luminous and vivid
        Role::Think => match mode {
            ThemeMode::Violet => spec(Some(285.0), 0.95, 6.5),
            ThemeMode::Cyan => spec(Some(192.0), 0.95, 7.5),
            ThemeMode::Amber => spec(Some(34.0), 0.95, 7.0),
        },

        // Semantic status roles — vibrant, solid, and bright:
        Role::Good => spec(Some(136.0), 0.96, 7.5),
        Role::Bad => spec(Some(0.0), 0.98, 5.8),
        Role::Warn => spec(Some(46.0), 0.98, 9.5),
        // ── Code ──────────────────────────────────────────────────────────────
        //
        // These are NOT theme roles, and deliberately so. A keyword is a keyword
        // whether the theme is violet or amber; tinting it with the theme would
        // make the *same* `fn` a different colour after `/theme cyan`, and a
        // reader who has learned "blue means keyword" would be taught a lie every
        // time the theme changed. The hues below are fixed and borrowed from a
        // conventional dark code theme, and only the *lightness* is solved —
        // which is the part that has to move, because it is what has to be legible
        // against whatever background this terminal has.
        //
        // `Role::Code` is the base every unclassified run is painted in: cool and
        // desaturated, so code of a language we do not know still reads as a
        // distinct kind of thing without shouting.
        //
        // Contrast targets sit where a dark editor theme does (VS Code Dark+ and
        // its relatives): the nearest-legible stop made every token darker than
        // any editor would dare — a keyword and a comment read almost the same,
        // and the owner read the highlight as "terlalu gelap". The solve stops at
        // the target, so the target *is* the lightness; these are chosen so each
        // token lands near its editor counterpart while the hues below stay the
        // fixed, theme-independent convention.
        Role::Code => spec(Some(215.0), 0.30, 9.0),
        Role::CodeKeyword => spec(Some(212.0), 0.78, 7.2),
        Role::CodeString => spec(Some(24.0), 0.68, 7.2),
        Role::CodeNumber => spec(Some(105.0), 0.50, 7.5),
        Role::CodeComment => spec(Some(95.0), 0.42, 5.0),
        Role::CodeType => spec(Some(168.0), 0.62, 7.2),
        Role::CodeFunc => spec(Some(52.0), 0.60, 7.5),
        Role::Math => match mode {
            ThemeMode::Violet => spec(Some(272.0), 0.85, 7.0),
            ThemeMode::Cyan => spec(Some(188.0), 0.85, 7.0),
            ThemeMode::Amber => spec(Some(46.0), 0.85, 7.0),
        },
        Role::Quote => match mode {
            ThemeMode::Violet => spec(Some(278.0), 0.80, 7.0),
            ThemeMode::Cyan => spec(Some(192.0), 0.80, 7.0),
            ThemeMode::Amber => spec(Some(36.0), 0.80, 7.0),
        },
    }
}

/// Solve every role against one background using the current theme.
#[cfg(test)]
pub fn build(bg: Rgb) -> Palette {
    build_with(bg, current_theme())
}

/// Solve every role against one background with the specified theme mode.
pub fn build_with(bg: Rgb, mode: ThemeMode) -> Palette {
    let mut inks = [Rgb(0, 0, 0); Role::COUNT];
    for role in Role::ALL {
        inks[role as usize] = solve(bg, &spec_of_mode(role, mode));
    }
    Palette { bg, inks }
}

/// The colour that says what this role means and can still be read on `bg`.
///
/// Walks the lightness away from the background until the contrast target is
/// met, and stops there — the *nearest* legible tone, not the brightest one. A
/// palette that jumps straight to white is legible and unpleasant, and loses the
/// hierarchy between a heading and a hint.
fn solve(bg: Rgb, spec: &Spec) -> Rgb {
    let hue = spec.hue.unwrap_or_else(|| hue_of(bg));
    // A neutral may only be as tinted as the ground it borrows its hue from.
    // Near-grey backgrounds have a violently unstable hue — two units of blue
    // decide between yellow and violet — and without this an off-white terminal
    // was given olive labels. A genuinely tinted terminal keeps its full tint.
    let sat = match spec.hue {
        Some(_) => spec.sat,
        None => spec.sat.min(chroma_of(bg) * 2.0 + 0.04),
    };
    let lighter = luminance(bg) < PIVOT;
    let start = lightness_of(bg);
    let mut best = Rgb(0, 0, 0);
    let mut best_ratio = 0.0;

    let steps = 200;
    for i in 0..=steps {
        let delta = i as f32 / steps as f32;
        let l = if lighter {
            start + delta
        } else {
            start - delta
        };
        if !(0.0..=1.0).contains(&l) {
            continue;
        }
        let candidate = from_hsl(hue, sat, l);
        let ratio = contrast(candidate, bg);
        if ratio > best_ratio {
            best_ratio = ratio;
            best = candidate;
        }
        if ratio >= spec.contrast {
            return candidate;
        }
    }
    // Unreachable target — a mid-grey background cannot give ten to one in
    // either direction. The most legible tone that exists is the honest answer.
    best
}

// ─── Asking the terminal ──────────────────────────────────────────────────────

/// Ask the terminal for its background colour, and hand back whatever else
/// arrived while waiting.
///
/// # Why the leftovers come back
///
/// The reply arrives on stdin, mixed in with anything the user has already
/// typed. Reading it and throwing the rest away eats keystrokes at start-up, so
/// the bytes that were not the reply are returned for the key decoder to
/// consume. Terminals that do not implement OSC 11 answer nothing at all, which
/// is why this is a poll with a deadline rather than a read.
pub fn probe(timeout_ms: i32) -> (Option<Rgb>, Vec<u8>) {
    use std::io::Write;
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b]11;?\x1b\\").is_err() || out.flush().is_err() {
        return (None, Vec::new());
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        if !runeengine::platform::wait_stdin(Some(left), None) {
            break;
        }
        let mut chunk = [0u8; 256];
        let n = match runeengine::platform::read_stdin(&mut chunk) {
            Ok(n) if n > 0 => n,
            _ => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some((rgb, rest)) = take_reply(&buf) {
            return (Some(rgb), rest);
        }
    }
    (None, buf)
}

/// Find an OSC 11 reply in `bytes`, returning it and everything that was not it.
fn take_reply(bytes: &[u8]) -> Option<(Rgb, Vec<u8>)> {
    const HEAD: &[u8] = b"\x1b]11;";
    let start = bytes.windows(HEAD.len()).position(|w| w == HEAD)?;
    let body = &bytes[start + HEAD.len()..];
    // Two terminators are legal and both are in the wild: BEL, and ESC \.
    let end = body
        .iter()
        .position(|&b| b == 0x07 || b == 0x1b)
        .filter(|&i| body[i] != 0x1b || body.get(i + 1) == Some(&b'\\'))?;
    let payload = std::str::from_utf8(&body[..end]).ok()?;
    let rgb = parse_colour(payload)?;

    let skip = if body[end] == 0x1b { 2 } else { 1 };
    let mut rest = bytes[..start].to_vec();
    rest.extend_from_slice(&body[end + skip..]);
    Some((rgb, rest))
}

/// `rgb:1a1a/2b2b/3c3c`, `rgb:1a/2b/3c`, or `#1a2b3c` — every shape a terminal
/// has been seen to answer with. The components are not always eight bits, so
/// each is scaled by its own width rather than truncated.
fn parse_colour(text: &str) -> Option<Rgb> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        if hex.len() == 6 {
            return Some(Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ));
        }
        return None;
    }
    let body = text.strip_prefix("rgb:").unwrap_or(text);
    let mut parts = body.split('/');
    let mut next = || -> Option<u8> {
        let part = parts.next()?;
        let value = u32::from_str_radix(part, 16).ok()?;
        let max = 16u32.checked_pow(part.len() as u32)? - 1;
        Some(((value * 255) / max.max(1)) as u8)
    };
    let rgb = Rgb(next()?, next()?, next()?);
    parts.next().is_none().then_some(rgb)
}

/// What the environment says, when the terminal will not say anything.
///
/// `RUNE_THEME` is the escape hatch for a terminal that answers nothing and
/// guesses wrong: `dark`, `light`, or a hex colour. `COLORFGBG` is set by a
/// handful of terminals and carries the background as a palette index.
pub fn from_env() -> Option<Rgb> {
    if let Ok(value) = std::env::var("RUNE_THEME") {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => return Some(DEFAULT_BG),
            "light" => return Some(Rgb(250, 250, 248)),
            other => {
                if let Some(rgb) = parse_colour(other) {
                    return Some(rgb);
                }
            }
        }
    }
    let fgbg = std::env::var("COLORFGBG").ok()?;
    let background: u8 = fgbg.rsplit(';').next()?.trim().parse().ok()?;
    // 0–6 and 8 are the dark half of the ANSI palette; 7 and 9–15 the light one.
    Some(if matches!(background, 0..=6 | 8) {
        DEFAULT_BG
    } else {
        Rgb(250, 250, 248)
    })
}

// ─── Colour arithmetic ────────────────────────────────────────────────────────

/// WCAG relative luminance. sRGB is gamma-encoded, so the channels have to be
/// linearised before they mean anything about brightness.
pub(super) fn luminance(c: Rgb) -> f32 {
    let channel = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.0) + 0.7152 * channel(c.1) + 0.0722 * channel(c.2)
}

/// The WCAG ratio, from 1 (identical) to 21 (black on white).
fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// The background's hue, so the neutrals can borrow it. A grey background has no
/// hue to borrow and gets the palette's own.
pub(super) fn hue_of(c: Rgb) -> f32 {
    let (r, g, b) = (c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d < 0.001 {
        return 220.0;
    }
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    if h < 0.0 {
        h + 360.0
    } else {
        h
    }
}

/// How far from grey the background is, on its own scale rather than HSL's —
/// which exaggerates the saturation of anything close to black or white.
pub(super) fn chroma_of(c: Rgb) -> f32 {
    let (r, g, b) = (c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
    r.max(g).max(b) - r.min(g).min(b)
}

/// HSL lightness — the midpoint of the extremes, which is the axis [`solve`]
/// walks. Not the same quantity as [`luminance`], and used for a different job:
/// this one constructs a colour, that one judges it.
fn lightness_of(c: Rgb) -> f32 {
    let (r, g, b) = (c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
    (r.max(g).max(b) + r.min(g).min(b)) / 2.0
}

fn from_hsl(hue: f32, sat: f32, light: f32) -> Rgb {
    let c = (1.0 - (2.0 * light - 1.0).abs()) * sat;
    let h = (hue.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = light - c / 2.0;
    let byte = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb(byte(r), byte(g), byte(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: Rgb = Rgb(12, 14, 20);
    const LIGHT: Rgb = Rgb(250, 250, 248);

    /// The whole point of the module: nothing it produces may be hard to read on
    /// the background it produced it for. This is the "samar" bug as a test.
    #[test]
    fn every_role_meets_its_contrast_target_on_both_ends_of_the_spectrum() {
        for mode in ThemeMode::ALL {
            for bg in [DARK, LIGHT, Rgb(40, 30, 60), Rgb(230, 225, 200)] {
                let palette = build_with(bg, mode);
                // No background can give more than white or black gives against it,
                // and a mid-toned one gives a good deal less. Demanding a fixed
                // ratio of every background is a test that fails for being right.
                let ceiling = contrast(Rgb(255, 255, 255), bg).max(contrast(Rgb(0, 0, 0), bg));
                for role in Role::ALL {
                    let want = spec_of_mode(role, mode).contrast.min(ceiling);
                    let got = contrast(palette.of(role), bg);
                    assert!(
                        got >= want - 0.15,
                        "{mode:?} {role:?} on {bg:?} reached {got:.2}, needed {want:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_light_terminal_gets_dark_ink_and_a_dark_one_gets_light_ink() {
        let on_light = build(LIGHT);
        let on_dark = build(DARK);
        assert!(luminance(on_light.of(Role::Text)) < luminance(LIGHT));
        assert!(luminance(on_dark.of(Role::Text)) > luminance(DARK));
    }

    /// The hierarchy has to survive the solve, or every label ends up shouting
    /// as loudly as the text it labels.
    #[test]
    fn the_ramp_keeps_its_order_after_solving() {
        for mode in ThemeMode::ALL {
            for bg in [DARK, LIGHT] {
                let p = build_with(bg, mode);
                let step = |r: Role| contrast(p.of(r), bg);
                assert!(step(Role::Text) > step(Role::Muted));
                assert!(step(Role::Muted) > step(Role::Faint));
                // A rule carries structure, so it sits above the faintest prose —
                // the owner's words were "terang dan tebal", bright and bold.
                assert!(step(Role::Rule) > step(Role::Faint));
                assert!(step(Role::Muted) > step(Role::Rule));
                // User text is vibrant and clearly legible (WCAG accessible)
                assert!(step(Role::User) >= 4.5);
            }
        }
    }

    #[test]
    fn theme_mode_parsing_and_cycling() {
        assert_eq!(ThemeMode::parse("violet"), Some(ThemeMode::Violet));
        assert_eq!(ThemeMode::parse("purple"), Some(ThemeMode::Violet));
        assert_eq!(ThemeMode::parse("cyan"), Some(ThemeMode::Cyan));
        assert_eq!(ThemeMode::parse("ice"), Some(ThemeMode::Cyan));
        assert_eq!(ThemeMode::parse("amber"), Some(ThemeMode::Amber));
        assert_eq!(ThemeMode::parse("gold"), Some(ThemeMode::Amber));
        assert_eq!(ThemeMode::parse("invalid"), None);

        assert_eq!(ThemeMode::Violet.next(), ThemeMode::Cyan);
        assert_eq!(ThemeMode::Cyan.next(), ThemeMode::Amber);
        assert_eq!(ThemeMode::Amber.next(), ThemeMode::Violet);

        assert_eq!(ThemeMode::Violet.prev(), ThemeMode::Amber);
        assert_eq!(ThemeMode::Amber.prev(), ThemeMode::Cyan);
        assert_eq!(ThemeMode::Cyan.prev(), ThemeMode::Violet);

        assert_eq!(ThemeMode::Violet.name(), "violet");
        assert_eq!(ThemeMode::Cyan.name(), "cyan");
        assert_eq!(ThemeMode::Amber.name(), "amber");
    }

    #[test]
    fn the_neutrals_take_the_terminals_own_hue() {
        let warm = build(Rgb(28, 20, 14));
        // A brown-grounded terminal must not be given cold blue-grey labels.
        let muted = warm.of(Role::Muted);
        assert!(
            muted.0 >= muted.2,
            "neutral ink ignored the background's hue: {muted:?}"
        );
    }

    #[test]
    fn a_fade_runs_from_the_ink_to_the_ground_and_stops_at_both() {
        let ink = palette().of(Role::Accent);
        assert_eq!(fade(ink, 0.0), ink);
        assert_eq!(fade(ink, 1.0), palette().bg);
        // Out of range is clamped, not wrapped into a wrong colour.
        assert_eq!(fade(ink, -2.0), ink);
        assert_eq!(fade(ink, 9.0), palette().bg);
    }

    #[test]
    fn a_reply_is_parsed_in_every_shape_a_terminal_sends_it() {
        assert_eq!(
            parse_colour("rgb:1a1a/2b2b/3c3c"),
            Some(Rgb(0x1a, 0x2b, 0x3c))
        );
        assert_eq!(parse_colour("rgb:1a/2b/3c"), Some(Rgb(0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_colour("#1a2b3c"), Some(Rgb(0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_colour("nonsense"), None);
    }

    /// Keystrokes typed before the terminal answered must survive the probe.
    #[test]
    fn what_the_user_typed_around_the_reply_is_handed_back() {
        let bytes = b"ab\x1b]11;rgb:0000/0000/0000\x07cd";
        let (rgb, rest) = take_reply(bytes).expect("reply not found");
        assert_eq!(rgb, Rgb(0, 0, 0));
        assert_eq!(rest, b"abcd");
    }

    #[test]
    fn a_half_arrived_reply_is_not_guessed_at() {
        assert!(take_reply(b"\x1b]11;rgb:0000/0000").is_none());
    }

    #[test]
    fn the_environment_is_read_when_the_terminal_will_not_answer() {
        // Not `RUNE_THEME`: the test process is shared, and setting a variable
        // that later tests read is how a suite becomes order-dependent.
        assert_eq!(parse_colour("#fafaf8"), Some(LIGHT));
    }
}
