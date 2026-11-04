//! The model repeating itself, caught in the middle of its own stream.
//!
//! A model can fall into a loop that repeats one short unit forever. Through a
//! gateway it usually starts the same way: the raw tool syntax that belongs in
//! the protocol leaks into the text (`to=functions.patch_file  *json`), the
//! arguments are written once, and then nothing produces the stop token — the
//! next thousands of bytes are `}"}"}"...`. Left alone that is not merely ugly:
//! it is paid for token by token, it fills the transcript with bytes nobody
//! wrote on purpose, and one 134,018-byte row of it took Rune's screen down
//! (session 698, 2026-11-02: 66,818 repeats of `}"`).
//!
//! # What is detected, and what is deliberately left alone
//!
//! One thing is measured: an **exact** unit repeated immediately at the end of
//! the stream, covering at least [`REPETITION_MIN_BYTES`]. That is what
//! separates a loop from everything a person actually asks a model to write:
//!
//! * Prose repeats words on purpose, and paragraphs breathe with blank lines,
//!   but no legitimate answer repeats one exact unit sixteen times without
//!   interruption across four kilobytes.
//! * A base64 data URI, a minified script, a long path, a hash or a JWT is one
//!   unbroken run of tens of kilobytes — and it is **not** detected, because its
//!   characters vary: there is no repeating unit. A ceiling on how long a single
//!   token may be would have cut exactly that legitimate output; this rule has
//!   no such ceiling.
//! * A Markdown rule, a table separator or a deeply indented block repeats one
//!   character, and stays far below the floor.
//! * A run of blank lines or of indentation gets its own, higher floor: those
//!   repeat on purpose far longer than anything else.
//! * A repetition that has already *ended* is not a loop. Only the end of the
//!   stream is examined, so a finished rule, a finished table and a finished
//!   fixture are never touched.
//!
//! Nothing here reads meaning, counts tokens or estimates capacity: the finding
//! is a byte fact about the stream, so a live window, the stored conversation
//! and a replayed one agree.

use crate::limits::{
    REPETITION_MAX_UNIT, REPETITION_MIN_BYTES, REPETITION_MIN_REPEATS,
    REPETITION_MIN_WHITESPACE_BYTES,
};

/// Which stream the repetition was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The answer the person reads.
    Answer,
    /// The reasoning shown above it.
    Thinking,
}

/// What the guard found: the repeating unit, and where the repeating run began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub part: Part,
    /// Byte offset, in that stream as it has been fed so far, where the
    /// repeating run starts. Everything before it was written once.
    pub starts_at: u64,
    /// The repeating unit, printable and cut short, for a person to recognise.
    pub unit: String,
    /// Bytes the run had covered when it was caught.
    pub bytes: u64,
    /// How many times the unit had repeated.
    pub repeats: u64,
}

impl Hit {
    /// The longest prefix of `text` that is what the model wrote once, before it
    /// began repeating. Always a character boundary: a unit may cut a multi-byte
    /// character in half.
    pub fn kept<'a>(&self, text: &'a str) -> &'a str {
        &text[..at_boundary(text, self.starts_at as usize)]
    }
}

/// `at`, brought back to the nearest character boundary at or below it.
pub fn at_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// As much of the stream as a run that qualifies can occupy, plus the unit. A
/// run is caught the moment it reaches its floor, so it never needs to be
/// measured past this.
const WINDOW: usize = REPETITION_MIN_WHITESPACE_BYTES + REPETITION_MAX_UNIT;

/// The tail of one stream, kept only as long as a qualifying run could be.
#[derive(Debug, Default)]
struct Guard {
    tail: Vec<u8>,
    total: u64,
}

impl Guard {
    /// Nothing has been seen: a retried attempt starts over.
    fn clear(&mut self) {
        self.tail.clear();
        self.total = 0;
    }

    /// Feed one streamed piece; the repeating unit's length and where the run
    /// begins, when the end of this stream has become a loop.
    fn push(&mut self, text: &str) -> Option<(usize, usize)> {
        if text.is_empty() {
            return None;
        }
        self.total = self.total.saturating_add(text.len() as u64);
        self.tail.extend_from_slice(text.as_bytes());
        if self.tail.len() > WINDOW {
            let cut = self.tail.len() - WINDOW;
            self.tail.drain(..cut);
        }
        detect(&self.tail)
    }
}

/// The exact repeating unit at the end of `tail`, with its length and where the
/// periodic run it belongs to begins (an index into `tail`). Shorter units are
/// examined first, so a run that can be read either way is reported by the
/// smallest unit it is made of.
fn detect(tail: &[u8]) -> Option<(usize, usize)> {
    let longest = REPETITION_MAX_UNIT.min(tail.len() / 2);
    for period in 1..=longest {
        let unit = &tail[tail.len() - period..];
        let floor = if unit.iter().all(u8::is_ascii_whitespace) {
            REPETITION_MIN_WHITESPACE_BYTES
        } else {
            REPETITION_MIN_BYTES
        };
        // The fewest repeats that could reach the floor, never fewer than the
        // rule's own minimum: below it there is nothing to report.
        let need = (floor.div_ceil(period) as u64).max(REPETITION_MIN_REPEATS);
        if tail.len() as u64 / (period as u64) < need {
            continue;
        }
        // The whole run is counted, not just enough of it to qualify: where the
        // repetition *began* is what the person keeps, so it has to be exact.
        // The scan is bounded by the window and stops at the first byte that
        // differs, so ordinary text costs one comparison per period examined.
        let mut repeats = 1u64;
        let mut at = tail.len() - period;
        while at >= period {
            if &tail[at - period..at] != unit {
                break;
            }
            repeats += 1;
            at -= period;
        }
        if repeats >= need {
            let mut start = tail.len() - period * repeats as usize;
            // A stream splits wherever it likes, so the counted alignment is
            // arbitrary: the run is the longest *periodic* suffix, and bytes
            // before the alignment still belong to it. Without this, a loop
            // caught mid-unit left a fragment of its own unit behind as if the
            // model had written it.
            while start > 0 && tail[start - 1] == tail[start - 1 + period] {
                start -= 1;
            }
            return Some((period, start));
        }
    }
    None
}

/// The unit as a person reading a log line can recognise it, with every control
/// character escaped so one line stays one line.
fn printable(unit: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(unit);
    let mut out = String::new();
    for c in lossy.chars().take(40) {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Both streams of one attempt, watched for a repetition.
#[derive(Debug, Default)]
pub struct Watch {
    answer: Guard,
    thinking: Guard,
    hit: Option<Hit>,
}

impl Watch {
    pub fn new() -> Watch {
        Watch::default()
    }

    /// This attempt's output is void (a retry starts it again): nothing watched
    /// so far belongs to the response the person will read.
    pub fn clear(&mut self) {
        self.answer.clear();
        self.thinking.clear();
        self.hit = None;
    }

    /// Feed the piece the model just streamed. `Some` once one of the streams
    /// has become a loop — from then on this attempt should be stopped.
    pub fn push(&mut self, part: Part, delta: &str) -> Option<&Hit> {
        if self.hit.is_some() {
            return None;
        }
        let guard = match part {
            Part::Answer => &mut self.answer,
            Part::Thinking => &mut self.thinking,
        };
        let (period, start) = guard.push(delta)?;
        let bytes = (guard.tail.len() - start) as u64;
        self.hit = Some(Hit {
            part,
            starts_at: guard.total - bytes,
            unit: printable(&guard.tail[guard.tail.len() - period..]),
            bytes,
            repeats: bytes / period as u64,
        });
        self.hit.as_ref()
    }

    /// What was found, if anything, once the stream has been stopped.
    pub fn hit(&self) -> Option<&Hit> {
        self.hit.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `text` in pieces of at most `step` bytes, as a stream delivers it,
    /// never cutting a character in half.
    fn fed(text: &str, step: usize) -> Option<Hit> {
        let mut watch = Watch::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + step).min(text.len());
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            if watch.push(Part::Answer, &text[start..end]).is_some() {
                return watch.hit().cloned();
            }
            start = end;
        }
        watch.hit().cloned()
    }

    fn detects(text: &str) -> bool {
        fed(text, 4096).is_some()
    }

    // ── What must never be mistaken for a loop ────────────────────────────────

    #[test]
    fn ordinary_prose_is_never_a_loop() {
        let prose = "The stage keeps the two windows from covering each other. \
                     The stage keeps the two windows from covering each other, \
                     and the measurement is what decides it, not the guess.";
        assert!(!detects(prose));
    }

    #[test]
    fn paragraphs_breathe_with_blank_lines() {
        let mut text = String::new();
        for n in 0..40 {
            text.push_str(&format!("Paragraph {n} ends here.\n\n\n"));
        }
        assert!(!detects(&text), "blank lines are not a repetition");
    }

    #[test]
    fn a_long_run_of_blank_lines_still_needs_the_whitespace_floor() {
        assert!(!detects(&"\n".repeat(16_000)));
        assert!(
            detects(&"\n".repeat(16_384)),
            "a stream of newlines is a loop"
        );
    }

    #[test]
    fn markdown_rules_and_table_separators_stay_under_the_floor() {
        for rule in [
            "---".repeat(500 / 3 + 1),
            "=".repeat(2_000),
            "*".repeat(1_000),
            "_".repeat(1_000),
            "|---|".repeat(60),
            "-".repeat(4_000),
        ] {
            assert!(!detects(&rule), "a {}-byte rule became a loop", rule.len());
        }
    }

    #[test]
    fn indentation_has_its_own_floor() {
        assert!(!detects(&" ".repeat(4_000)), "4000 spaces are indentation");
        assert!(!detects(&"\t".repeat(4_000)));
        assert!(detects(&" ".repeat(16_384)), "a padding loop is a loop");
    }

    #[test]
    fn a_base64_data_uri_is_not_a_loop_however_long_it_is() {
        // One unbroken run of 120 KB: the shape a ceiling on token length would
        // have destroyed.
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut uri = String::from("background-image:url(\"data:image/png;base64,");
        let mut seed = 7u32;
        for _ in 0..120_000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            uri.push(alphabet.as_bytes()[(seed >> 16) as usize % alphabet.len()] as char);
        }
        uri.push_str("\")");
        assert!(!detects(&uri));
    }

    #[test]
    fn a_minified_script_on_one_line_is_not_a_loop() {
        let mut code = String::from("function a(b){return b?b.c.d(1,2,3):e(f(4,5),6)}");
        let mut seed = 11u32;
        for _ in 0..30_000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let n = (seed >> 16) % 26;
            code.push((b'a' + n as u8) as char);
            if code.len() % 7 == 0 {
                code.push('(');
            }
        }
        assert!(!detects(&code));
    }

    #[test]
    fn a_long_path_hash_or_token_is_not_a_loop() {
        let hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(!detects(&hash.repeat(3)));
        let path = "/home/noire/Project/Atoryn/Studio/Engine/src/session/degeneration.rs";
        assert!(!detects(&path.repeat(20)));
    }

    #[test]
    fn a_repetition_that_already_ended_is_not_a_loop() {
        let mut text = "|".to_string();
        text.push_str(&"-".repeat(5_000));
        text.push_str("|\nand then the answer goes on, normally, in words.");
        assert!(!detects(&text));
    }

    #[test]
    fn a_short_run_below_the_floor_is_not_a_loop() {
        assert!(!detects(&"}".repeat(60)));
        assert!(!detects(&"\"}".repeat(2_000)), "4000 bytes is the floor");
        assert!(!detects(&"x".repeat(4_095)));
    }

    #[test]
    fn code_that_repeats_a_line_a_few_times_is_not_a_loop() {
        let mut code = String::new();
        for _ in 0..10 {
            code.push_str("    let value = read(&path)?;\n");
        }
        assert!(!detects(&code));
    }

    #[test]
    fn json_with_escaped_quotes_is_not_a_loop() {
        let mut json = String::from("[");
        for n in 0..50 {
            json.push_str(&format!(
                "{{\"id\":{n},\"text\":\"a \\\"quoted\\\" word\"}},"
            ));
        }
        json.push(']');
        assert!(!detects(&json));
    }

    #[test]
    fn unicode_prose_and_emoji_are_not_a_loop() {
        let mut text = String::new();
        for n in 0..300 {
            text.push_str(&format!("Kalimat {n}: teks dengan é dan ü, lalu panah →. "));
        }
        assert!(!detects(&text));
        assert!(!detects(&"→ ".repeat(500)));
    }

    /// The other side of the same coin: a sentence repeated word for word is
    /// what degeneration looks like once it is longer than one character, so a
    /// model that says the same thing two hundred times is stopped as well.
    #[test]
    fn the_same_sentence_two_hundred_times_is_a_loop() {
        let hit = fed(&"The stage keeps both windows. ".repeat(200), 4096).expect("a loop");
        assert_eq!(hit.unit, "The stage keeps both windows. ");
        assert_eq!(hit.starts_at, 0);
    }

    // ── What must be caught, and where it began ───────────────────────────────

    #[test]
    fn the_row_that_took_the_screen_down_is_caught() {
        // Session 698, byte for byte until the loop: the model's leaked tool
        // syntax, then the run of `}"`.
        let mut text = String::from(
            " to=functions.patch_file  *json\n{\"action\":\"Memastikan 404px\",\
             \"path\":\"Website/atoryn.css\"}",
        );
        let prose = text.len();
        text.push_str(&"\"}".repeat(3_000));
        let hit = fed(&text, 4096).expect("a loop");
        assert_eq!(hit.part, Part::Answer);
        assert_eq!(hit.unit, "\"}");
        // The unit also ends the prose itself (`..."}`), so the run is one unit
        // older than the prose: nothing of the loop is kept, and no more than
        // one unit of real text goes with it.
        assert!(prose - hit.starts_at as usize <= hit.unit.len());
        assert!(text.starts_with(hit.kept(&text)));
    }

    #[test]
    fn one_character_repeating_is_caught() {
        let hit = fed(&"\"".repeat(8_000), 97).expect("a loop");
        assert_eq!(hit.unit, "\"");
        assert_eq!(hit.starts_at, 0);
    }

    #[test]
    fn a_repeated_word_is_caught() {
        let hit = fed(&"hello ".repeat(900), 4096).expect("a loop");
        assert_eq!(hit.unit, "hello ");
        assert_eq!(hit.starts_at, 0);
    }

    #[test]
    fn a_three_byte_unit_is_caught() {
        assert!(detects(&"abc".repeat(1_400)));
    }

    #[test]
    fn the_loop_is_caught_early_rather_than_at_the_end() {
        // 200 KB of repetition: the guard must stop a few kilobytes in, because
        // that is where the money and the screen go.
        let text = &"\"}".repeat(100_000);
        let hit = fed(text, 4096).expect("a loop");
        assert!(
            hit.bytes <= REPETITION_MIN_BYTES as u64 + 8,
            "caught after {} bytes",
            hit.bytes
        );
    }

    #[test]
    fn one_byte_at_a_time_is_caught_too() {
        let hit = fed(&"\"}".repeat(3_000), 1).expect("a loop");
        assert_eq!(hit.unit, "\"}");
    }

    #[test]
    fn a_loop_in_the_reasoning_is_named_as_the_reasoning() {
        let mut watch = Watch::new();
        let mut hit = None;
        for _ in 0..3_000 {
            if let Some(found) = watch.push(Part::Thinking, "\"}\"") {
                hit = Some(found.clone());
                break;
            }
        }
        assert_eq!(hit.expect("a loop").part, Part::Thinking);
    }

    #[test]
    fn the_unit_is_printable_in_a_log_line() {
        let hit = fed(&"\n".repeat(20_000), 512).expect("a loop");
        assert_eq!(hit.unit, "\\n");
        let hit = fed(&"\t".repeat(20_000), 512).expect("a loop");
        assert_eq!(hit.unit, "\\t");
    }

    #[test]
    fn a_hit_is_reported_once_and_a_retry_starts_over() {
        let mut watch = Watch::new();
        assert!(watch.push(Part::Answer, &"\"}".repeat(3_000)).is_some());
        assert!(
            watch.push(Part::Answer, "\"}").is_none(),
            "a stopped stream is not reported again"
        );
        watch.clear();
        assert!(watch.hit().is_none());
        assert!(watch.push(Part::Answer, "an ordinary answer").is_none());
    }

    #[test]
    fn the_kept_prefix_never_cuts_a_character_in_half() {
        // `→` is three bytes, so a period of two can start inside one.
        let text = format!("{}{}", "→".repeat(10), "\"\"".repeat(4_000));
        let hit = fed(&text, 4096).expect("a loop");
        let kept = hit.kept(&text);
        assert!(text.starts_with(kept));
        assert_eq!(kept, "→".repeat(10));
    }
}
