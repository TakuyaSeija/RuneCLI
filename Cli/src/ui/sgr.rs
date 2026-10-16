//! The style a terminal is holding, sent only when it changes.
//!
//! A frame is composed a span at a time, and every span opens with its own
//! colour and closes with a reset: a row of same-coloured words says the same
//! colour again after every one of them. Measured on a scroll through a long
//! session, 63% of what Rune sent were colour codes — and a terminal that reads
//! slowly pays for every byte (Konsole reads about 180 KB/s, so a frame cost it
//! 25 ms). This pass rewrites each run of colour codes (`ESC [ … m`) as the
//! shortest one that takes the terminal from the style it holds to the style
//! the run leaves it in, and drops the run when that is the style it already
//! holds.
//!
//! It never guesses. A parameter it does not model, or anything else that can
//! change the style behind its back (a saved cursor restored, a reset), makes
//! the held style unknown, and runs are then sent exactly as they came until a
//! reset makes it known again. So at every byte the terminal holds the style
//! the unrewritten stream would have given it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Colour {
    #[default]
    Default,
    /// One of the sixteen, as its own parameter (30–37, 90–97 for the
    /// foreground; 40–47, 100–107 for the background).
    Basic(u16),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Style {
    fg: Colour,
    bg: Colour,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    reverse: bool,
    hidden: bool,
    strike: bool,
}

impl Style {
    /// What an erase leaves in a cell under this style: its background, and
    /// nothing else of it — for the tests that read a stream back as cells.
    #[cfg(test)]
    pub(super) fn erasing(self) -> Style {
        Style {
            bg: self.bg,
            ..Style::default()
        }
    }
}

/// Whether a style has one flag on.
type Flag = fn(&Style) -> bool;

/// The flags with their own on and off parameters (bold and dim share an off).
const FLAGS: [(Flag, u16, u16); 6] = [
    (|s| s.italic, 3, 23),
    (|s| s.underline, 4, 24),
    (|s| s.blink, 5, 25),
    (|s| s.reverse, 7, 27),
    (|s| s.hidden, 8, 28),
    (|s| s.strike, 9, 29),
];

/// Apply one `ESC [ params m` to `style`. `None`: a parameter this does not
/// model, so what the terminal holds afterwards is not known here.
pub(super) fn apply(style: Style, params: &str) -> Option<Style> {
    let mut style = style;
    if params.is_empty() {
        return Some(Style::default());
    }
    let mut numbers = Vec::new();
    for part in params.split(';') {
        if part.is_empty() {
            numbers.push(0);
        } else {
            numbers.push(part.parse::<u16>().ok()?);
        }
    }
    let mut at = 0;
    while at < numbers.len() {
        let n = numbers[at];
        at += 1;
        match n {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            5 => style.blink = true,
            7 => style.reverse = true,
            8 => style.hidden = true,
            9 => style.strike = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            25 => style.blink = false,
            27 => style.reverse = false,
            28 => style.hidden = false,
            29 => style.strike = false,
            30..=37 | 90..=97 => style.fg = Colour::Basic(n),
            39 => style.fg = Colour::Default,
            40..=47 | 100..=107 => style.bg = Colour::Basic(n),
            49 => style.bg = Colour::Default,
            38 | 48 => {
                let colour = match numbers.get(at)? {
                    5 => {
                        let index = u8::try_from(*numbers.get(at + 1)?).ok()?;
                        at += 2;
                        Colour::Indexed(index)
                    }
                    2 => {
                        let channel = |k: usize| u8::try_from(*numbers.get(at + k)?).ok();
                        let rgb = Colour::Rgb(channel(1)?, channel(2)?, channel(3)?);
                        at += 4;
                        rgb
                    }
                    _ => return None,
                };
                if n == 38 {
                    style.fg = colour;
                } else {
                    style.bg = colour;
                }
            }
            _ => return None,
        }
    }
    Some(style)
}

fn colour(out: &mut Vec<String>, colour: Colour, background: bool) {
    let (default, extended) = if background { (49, 48) } else { (39, 38) };
    out.push(match colour {
        Colour::Default => default.to_string(),
        Colour::Basic(n) => n.to_string(),
        Colour::Indexed(i) => format!("{extended};5;{i}"),
        Colour::Rgb(r, g, b) => format!("{extended};2;{r};{g};{b}"),
    });
}

/// The parameters that set `to` from a reset.
fn from_reset(to: &Style) -> Vec<String> {
    let mut out = vec!["0".to_string()];
    if to.bold {
        out.push("1".into());
    }
    if to.dim {
        out.push("2".into());
    }
    for (on, set, _) in FLAGS {
        if on(to) {
            out.push(set.to_string());
        }
    }
    if to.fg != Colour::Default {
        colour(&mut out, to.fg, false);
    }
    if to.bg != Colour::Default {
        colour(&mut out, to.bg, true);
    }
    out
}

/// The parameters that change `from` into `to`, one attribute at a time.
fn between(from: &Style, to: &Style) -> Vec<String> {
    let mut out = Vec::new();
    if (from.bold && !to.bold) || (from.dim && !to.dim) {
        out.push("22".into());
        if to.bold {
            out.push("1".into());
        }
        if to.dim {
            out.push("2".into());
        }
    } else {
        if to.bold && !from.bold {
            out.push("1".into());
        }
        if to.dim && !from.dim {
            out.push("2".into());
        }
    }
    for (on, set, unset) in FLAGS {
        if on(from) != on(to) {
            out.push(if on(to) { set } else { unset }.to_string());
        }
    }
    if from.fg != to.fg {
        colour(&mut out, to.fg, false);
    }
    if from.bg != to.bg {
        colour(&mut out, to.bg, true);
    }
    out
}

/// The shortest `ESC [ … m` that turns `from` into `to`; nothing when they are
/// the same style.
fn transition(from: &Style, to: &Style, out: &mut String) {
    if from == to {
        return;
    }
    let step = between(from, to).join(";");
    let reset = from_reset(to).join(";");
    out.push_str("\x1b[");
    out.push_str(if step.len() <= reset.len() {
        &step
    } else {
        &reset
    });
    out.push('m');
}

/// Whether an escape sequence other than a colour code can change the style
/// the terminal holds: restoring a saved cursor brings back the style saved
/// with it (`ESC 8`, `CSI u`, leaving the alternate screen), and a full reset
/// clears it.
fn touches_style(sequence: &str) -> bool {
    if let Some(csi) = sequence.strip_prefix("\x1b[") {
        let last = csi.chars().last().unwrap_or(' ');
        return matches!(last, 's' | 'u')
            || ((last == 'h' || last == 'l')
                && csi.starts_with('?')
                && csi[1..csi.len() - 1]
                    .split(';')
                    .any(|mode| matches!(mode, "47" | "1047" | "1048" | "1049")));
    }
    if sequence.starts_with("\x1b]") {
        return false;
    }
    true
}

/// The length of the escape sequence at the start of `s` (which begins with
/// ESC): a CSI to its final byte, an OSC to its terminator, else ESC and one
/// character.
fn sequence_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.get(1) {
        Some(b'[') => {
            let mut end = 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            (end + 1).min(bytes.len())
        }
        Some(b']') => {
            let mut end = 2;
            while end < bytes.len() {
                if bytes[end] == 0x07 {
                    return end + 1;
                }
                if bytes[end] == 0x1b && bytes.get(end + 1) == Some(&b'\\') {
                    return end + 2;
                }
                end += 1;
            }
            bytes.len()
        }
        Some(_) => 1 + s[1..].chars().next().map_or(0, char::len_utf8),
        None => 1,
    }
}

/// Whether a colour code begins from a reset: `ESC [ m`, `ESC [ 0 … m`, or
/// an empty first parameter, which means 0.
fn resets(params: &str) -> bool {
    params.is_empty() || params == "0" || params.starts_with("0;") || params.starts_with(';')
}

/// `out` with every run of colour codes rewritten as the least that keeps the
/// terminal's style what the run would have made it. What the terminal holds
/// before `out` is taken as unknown, so the first run goes as it came unless
/// it begins with a reset.
pub fn minimal(out: &str) -> String {
    let mut sent = String::with_capacity(out.len());
    let mut held: Option<Style> = None;
    let mut rest = out;
    while let Some(at) = rest.find('\x1b') {
        sent.push_str(&rest[..at]);
        rest = &rest[at..];
        // A run: colour codes one after another, nothing between them.
        let mut run_end = 0;
        let mut style = held;
        let mut understood = held.is_some();
        while rest[run_end..].starts_with("\x1b[") {
            let len = sequence_len(&rest[run_end..]);
            let sequence = &rest[run_end..run_end + len];
            if !sequence.ends_with('m') || len < 3 {
                break;
            }
            let params = &sequence[2..len - 1];
            style = match (style, resets(params)) {
                (Some(s), _) => apply(s, params),
                (None, true) => apply(Style::default(), params),
                (None, false) => None,
            };
            understood &= style.is_some();
            run_end += len;
        }
        if run_end > 0 {
            match (held, style, understood) {
                (Some(from), Some(to), true) => transition(&from, &to, &mut sent),
                _ => sent.push_str(&rest[..run_end]),
            }
            held = style;
            rest = &rest[run_end..];
            continue;
        }
        let len = sequence_len(rest);
        if touches_style(&rest[..len]) {
            held = None;
        }
        sent.push_str(&rest[..len]);
        rest = &rest[len..];
    }
    sent.push_str(rest);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each printed character with the style the terminal held when it was
    /// printed — `None` where the style is not modelled. Escape sequences that
    /// are not colour codes are kept as markers, so a sequence moved past a
    /// character would show.
    fn styled(stream: &str) -> Vec<(String, Option<Style>)> {
        let mut out = Vec::new();
        let mut style = Some(Style::default());
        let mut rest = stream;
        while !rest.is_empty() {
            if rest.starts_with('\x1b') {
                let len = sequence_len(rest);
                let sequence = &rest[..len];
                if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
                    let params = &sequence[2..len - 1];
                    style = style.and_then(|s| apply(s, params));
                    if style.is_none() && resets(params) {
                        style = apply(Style::default(), params);
                    }
                } else {
                    if touches_style(sequence) {
                        style = None;
                    }
                    out.push((sequence.to_string(), None));
                }
                rest = &rest[len..];
            } else {
                let c = rest.chars().next().unwrap();
                out.push((c.to_string(), style));
                rest = &rest[c.len_utf8()..];
            }
        }
        out
    }

    fn same_picture(original: &str) -> String {
        // The terminal starts plain here, as it does after the reset that
        // opens every row Rune writes.
        let original = format!("\x1b[0m{original}");
        let min = minimal(&original);
        assert_eq!(styled(&original), styled(&min), "{original:?}\n{min:?}");
        min
    }

    #[test]
    fn a_colour_said_again_is_not_sent_again() {
        let fg = "\x1b[38;2;231;231;233m";
        let line = format!("{fg}line one: \x1b[0m{fg}more words\x1b[0m{fg} and more\x1b[0m");
        let min = same_picture(&line);
        assert_eq!(min.matches("38;2;231;231;233").count(), 1, "{min:?}");
        assert!(min.len() < line.len() / 2 + 20, "{min:?}");
    }

    #[test]
    fn a_change_is_sent_as_the_least_that_makes_it() {
        let fg = "\x1b[38;2;231;231;233m";
        // Bold then plain again in the same colour: `22`, not reset and colour.
        let min = same_picture(&format!("{fg}a \x1b[0m\x1b[1m{fg}bold\x1b[0m{fg} b"));
        assert!(min.contains("\x1b[22m"), "{min:?}");
        // To plain: a reset.
        let min = same_picture(&format!("{fg}\x1b[1;3;4ma\x1b[0mb"));
        assert!(min.ends_with("\x1b[0mb"), "{min:?}");
    }

    #[test]
    fn what_it_does_not_model_goes_as_it_came_until_a_reset() {
        // A curly underline (colon form) and an underline colour: not modelled.
        let line = "\x1b[4:3mcurly\x1b[58;5;1mred\x1b[24mplain\x1b[0m\x1b[0mx\x1b[31my\x1b[31mz";
        let min = same_picture(line);
        assert!(
            min.contains("\x1b[4:3m") && min.contains("\x1b[58;5;1m") && min.contains("\x1b[24m")
        );
        assert_eq!(
            min.matches("\x1b[31m").count(),
            1,
            "known again after the reset: {min:?}"
        );
    }

    #[test]
    fn a_restored_cursor_forgets_the_style() {
        let line = "\x1b[31ma\x1b8\x1b[31mb";
        let min = same_picture(line);
        assert_eq!(min.matches("\x1b[31m").count(), 2, "{min:?}");
    }

    #[test]
    fn cursor_moves_and_links_keep_their_place() {
        let line =
            "\x1b[31ma\x1b[5;9H\x1b[31mb\x1b]8;;http://x\x1b\\\x1b[31mc\x1b]8;;\x1b\\\x1b[K\x1b[0m";
        let min = same_picture(line);
        assert_eq!(min.matches("\x1b[31m").count(), 1, "{min:?}");
        assert!(min.contains("\x1b]8;;http://x\x1b\\") && min.contains("\x1b[5;9H"));
    }

    /// Real frames — the wordmark, markdown, code, the side panel, the theme's
    /// colours — show the same picture with far fewer bytes.
    #[test]
    fn real_frames_keep_their_picture_and_lose_their_repeats() {
        use crate::ui::blocks::Block;
        let mut app = crate::ui::App::for_test();
        app.sidebar = true;
        for n in 0..80 {
            app.blocks
                .push(Block::user(format!("question {n}: why does it **fail**?")));
            app.blocks.push(Block::Text {
                text: format!(
                    "Answer {n}: the `parser` reads **two** fields.\n\n- first item with `code`\n- second item\n\n```rust\nfn demo() {{ let x = {n}; }}\n```\n"
                ),
            });
        }
        let (mut raw_total, mut sent_total) = (0, 0);
        for scroll in [0, 3, 40, 200] {
            app.scroll = scroll;
            app.transcript_dirty = true;
            let frame = crate::ui::draw::compose_for_test(&mut app, 45, 170).join("\r\n");
            let sent = same_picture(&frame);
            raw_total += frame.len();
            sent_total += sent.len();
        }
        // Composed rows alone; the painter's own resets around each row write
        // add more repeats (a real scroll stream lost 14.7%).
        assert!(
            sent_total * 100 < raw_total * 98,
            "{sent_total} bytes sent for {raw_total} composed"
        );
    }

    #[test]
    fn colour_forms_are_read_whole() {
        for line in [
            "\x1b[38;5;208ma\x1b[38;5;208mb\x1b[48;2;1;2;3mc\x1b[49md",
            "\x1b[91;104ma\x1b[39;49mb\x1b[2;1mc\x1b[22md\x1b[7;9me\x1b[27;29mf",
            "\x1b[ma\x1b[;31mb\x1b[38;2;300;0;0mc",
        ] {
            same_picture(line);
        }
    }
}
