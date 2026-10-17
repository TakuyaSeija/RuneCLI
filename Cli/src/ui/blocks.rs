//! The transcript: what is on screen, and how a tool call's counters move while
//! the model is still writing it.
//!
//! # The scanner is the point
//!
//! A provider streams tool arguments as raw JSON text, in pieces, and the JSON
//! is not valid until the last piece. Waiting for it means the screen shows
//! nothing while a 400-line file is written, then everything at once. So the
//! partial text is scanned for the three things worth showing — which file, how
//! many lines are being added, how many removed — and those are shown as they
//! grow.
//!
//! It is a scanner, not a parser: it never builds a value, it cannot fail, and
//! it is only ever right about a prefix. The authoritative arguments arrive with
//! the `tool_call` event and replace whatever it guessed.
//!
//! Measured before it was written: with a single item at the top level a real
//! provider streams in ~8-character pieces, so these counters move dozens of
//! times per file. With a batch array the whole value arrives in one piece and
//! they jump straight to the final number — correct either way, and never faked.

use runeengine::proto::Decision;
use std::time::Instant;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolState {
    /// The model is still writing the arguments.
    Writing,
    /// Waiting for the user to allow it.
    Asking,
    Running,
    Ok,
    /// The foreground deadline elapsed; process/output ownership moved to background capture.
    Backgrounded,
    Failed,
    Stopped,
}

impl ToolState {
    pub fn finished(self) -> bool {
        matches!(
            self,
            ToolState::Ok | ToolState::Backgrounded | ToolState::Failed | ToolState::Stopped
        )
    }
}

pub struct Tool {
    pub id: String,
    pub name: String,
    pub action: String,
    /// Arguments as text, exactly as they streamed.
    pub raw: String,
    pub args: Option<Value>,
    pub state: ToolState,
    /// Plain live screen snapshot, never raw control sequences.
    pub output: String,
    pub terminal: Option<runeengine::terminal_screen::Screen>,
    pub output_revision: u64,
    pub summary: String,
    /// What the model was given back. The transcript would be a list of
    /// filenames without it.
    pub content: String,
    pub detail: Option<Value>,
    pub progress: Progress,
    /// Reads the streamed arguments once, as they arrive (audit T6).
    pub scanner: ArgScanner,
    pub(crate) counter: CounterRamp,
    /// Set only when Engine emits `ToolCall`, immediately before execution.
    /// Streamed arguments may describe a queued later call and must not age it.
    pub started: Option<Instant>,
    pub ended: Option<Instant>,
    /// Persisted duration when loaded from saved session history.
    pub duration: Option<std::time::Duration>,
    /// The permission question, while there is one.
    pub ask: Option<Ask>,
    /// Number of lines trimmed from live output buffer so the cut counter never jumps backwards.
    pub trimmed_lines: usize,
    /// Whether this step is showing everything it did, rather than its summary.
    ///
    /// Per step, not per screen: a person opening one call to read its output
    /// does not want the other forty opened with it. `/verbose` still opens
    /// everything at once for anybody working without a pointer.
    pub open: bool,
}

pub(crate) struct CounterRamp {
    shown_added: f64,
    shown_removed: f64,
    from_added: f64,
    from_removed: f64,
    target_added: f64,
    target_removed: f64,
    pub(crate) started: Instant,
    duration: std::time::Duration,
}

impl Default for CounterRamp {
    fn default() -> CounterRamp {
        CounterRamp {
            shown_added: 0.0,
            shown_removed: 0.0,
            from_added: 0.0,
            from_removed: 0.0,
            target_added: 0.0,
            target_removed: 0.0,
            started: Instant::now(),
            duration: std::time::Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Hash)]
pub struct Ask {
    pub req: String,
    /// The model's own one-line statement of intent — the same sentence that
    /// heads the step. The card leads with it, because "what is being asked"
    /// beats "which tool asked".
    pub action: String,
    pub preview: String,
    pub remember_as: Option<String>,
    /// Paths outside the workspace this answer would open. Almost always empty,
    /// and the part that must be read when it is not.
    pub reach: Vec<String>,
    /// The answer lets the call's commands reach programs already running on
    /// this machine through Unix sockets — services outside the workspace.
    pub local_services: bool,
    /// The answer drops confinement for this call only.
    pub full_access: bool,
    /// Why a mode that would not have asked does, in the engine's words.
    pub reason: Option<String>,
}

impl Ask {
    /// The answers on offer, in the order they are shown, with what each one
    /// means to the engine.
    ///
    /// One list, read by two places: the card draws it and the keyboard answers
    /// from it. The alternative — a row of labels here and a `match` on keys
    /// over there — is two lists that agree until the day one of them is edited,
    /// and the thing they would disagree about is which button grants access.
    ///
    /// "Always" is only offered when the engine said the grant can be stored;
    /// for everything else there is nothing to remember it *by*.
    pub fn answers(&self) -> Vec<(String, Decision)> {
        let once = if self.full_access {
            "Full access once"
        } else {
            "Allow once"
        };
        let mut out = vec![(once.to_string(), Decision::AllowOnce)];
        if let Some(key) = &self.remember_as {
            out.push((format!("Always allow {key}"), Decision::AllowAlways));
        }
        out.push(("Refuse".to_string(), Decision::Deny));
        out
    }
}

/// What the counters say right now.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Progress {
    pub path: Option<String>,
    pub added: usize,
    pub removed: usize,
    /// The file body or replacement text decoded from the argument prefix.
    pub new_text: String,
    /// The text a patch will replace, decoded from the argument prefix.
    pub old_text: String,
    /// The line range [start, end] if inspect_file specifies view_range.
    pub range: Option<(i64, i64)>,
}

impl Tool {
    pub fn new(id: String, name: String, action: String) -> Tool {
        Tool {
            id,
            name,
            action,
            raw: String::new(),
            args: None,
            state: ToolState::Writing,
            output: String::new(),
            terminal: None,
            output_revision: 0,
            summary: String::new(),
            content: String::new(),
            detail: None,
            progress: Progress::default(),
            scanner: ArgScanner::default(),
            counter: CounterRamp::default(),
            started: None,
            ended: None,
            duration: None,
            ask: None,
            trimmed_lines: 0,
            open: false,
        }
    }

    /// Fold in a piece of streamed argument text and re-read the counters.
    ///
    /// Only the new bytes are read: decoding the whole prefix again on every
    /// delta made a large file body cost the square of its length.
    pub fn absorb(&mut self, delta: &str) {
        self.raw.push_str(delta);
        self.scanner.feed(&self.name, &self.raw, &mut self.progress);
        if !self.scanner.action.trim().is_empty() {
            self.action.clone_from(&self.scanner.action);
        }
        self.retarget_counters();
    }

    /// The authoritative arguments. Replaces whatever the scanner inferred.
    pub fn settle(&mut self, args: &Value) {
        if let Some(act) = args.get("action").and_then(|v| v.as_str()) {
            if !act.trim().is_empty() {
                self.action = act.to_string();
            }
        }
        self.args = Some(args.clone());
        self.progress = scan_value(&self.name, args);
        self.retarget_counters();
        self.raw.clear();
        self.scanner = ArgScanner::default();
        if self.state == ToolState::Writing {
            self.state = ToolState::Running;
        }
        self.started.get_or_insert_with(Instant::now);
    }

    pub fn absorb_output(&mut self, chunk: &str, rows: u16, cols: u16) {
        let before_size = self.terminal.as_ref().map(|screen| screen.size());
        let before_peak = self
            .terminal
            .as_ref()
            .map_or(0, |screen| screen.occupied_rows_peak);
        let screen = self.terminal.get_or_insert_with(|| {
            runeengine::terminal_screen::Screen::with_prefix(
                rows,
                cols,
                500_000,
                super::viewport::MAIN_PREVIEW_ROWS,
            )
        });
        screen.resize(rows, cols);
        screen.feed(chunk);
        let output = screen.text();
        if output != self.output
            || before_size != Some(screen.size())
            || self.trimmed_lines != screen.trimmed_rows
            || before_peak != screen.occupied_rows_peak
        {
            self.output = output;
            self.output_revision = self.output_revision.wrapping_add(1);
        }
        self.trimmed_lines = screen.trimmed_rows;
    }

    pub fn elapsed(&self) -> std::time::Duration {
        if let Some(dur) = self.duration {
            return dur;
        }
        self.started
            .map(|started| {
                self.ended
                    .unwrap_or_else(Instant::now)
                    .duration_since(started)
            })
            .unwrap_or_default()
    }

    /// Advance the two displayed counters along the distance-based ramp:
    /// removed (-N) lines climb first, followed by added (+N) lines.
    pub fn advance_counters(&mut self, now: Instant) -> bool {
        if self.counter.duration.is_zero() {
            return false;
        }
        let before = self.displayed_counts();
        let elapsed = now.saturating_duration_since(self.counter.started);
        let progress = (elapsed.as_secs_f64() / self.counter.duration.as_secs_f64()).min(1.0);

        let is_fresh_dual = self.counter.from_removed < 1.0
            && self.counter.from_added < 1.0
            && (self.counter.target_removed - self.counter.from_removed) >= 1.0
            && (self.counter.target_added - self.counter.from_added) >= 1.0;

        if is_fresh_dual {
            // Sequential motion when both start together from zero: removed (-N) climbs first (0.0..0.5),
            // then added (+N) climbs second (0.35..1.0).
            let p_rem = (progress / 0.5).min(1.0);
            let eased_rem = 1.0 - (1.0 - p_rem).powi(2);
            self.counter.shown_removed = self.counter.from_removed
                + (self.counter.target_removed - self.counter.from_removed) * eased_rem;

            let p_add = ((progress - 0.35) / 0.65).clamp(0.0, 1.0);
            let eased_add = 1.0 - (1.0 - p_add).powi(2);
            self.counter.shown_added = self.counter.from_added
                + (self.counter.target_added - self.counter.from_added) * eased_add;
        } else {
            let eased = 1.0 - (1.0 - progress).powi(2);
            self.counter.shown_added = self.counter.from_added
                + (self.counter.target_added - self.counter.from_added) * eased;
            self.counter.shown_removed = self.counter.from_removed
                + (self.counter.target_removed - self.counter.from_removed) * eased;
        }

        if progress >= 1.0 {
            self.counter.shown_added = self.counter.target_added;
            self.counter.shown_removed = self.counter.target_removed;
            self.counter.duration = std::time::Duration::ZERO;
        }
        before != self.displayed_counts()
    }

    pub fn displayed_counts(&self) -> (usize, usize) {
        (
            self.counter.shown_added.round().max(0.0) as usize,
            self.counter.shown_removed.round().max(0.0) as usize,
        )
    }

    /// A fingerprint of everything about this call that its drawing reads —
    /// the same completeness rule as [`Block::stamp`], one level down.
    ///
    /// `raw` and `args` are both hashed even though the drawing reads one or
    /// the other: while arguments are streaming only `raw` is filled, and
    /// `settle` moves the value across, so hashing both is what makes the step
    /// redraw at the moment it becomes authoritative.
    pub fn stamp(&self, h: &mut impl std::hash::Hasher) {
        use std::hash::Hash;
        self.id.hash(h);
        self.name.hash(h);
        self.action.hash(h);
        self.raw.hash(h);
        self.args.hash(h);
        self.state.hash(h);
        self.output.hash(h);
        self.output_revision.hash(h);
        self.summary.hash(h);
        self.content.hash(h);
        self.detail.hash(h);
        // The decoded texts come from `raw` or `args`, both hashed above;
        // hashing the copies again only doubled the cost of a large body.
        self.progress.path.hash(h);
        self.progress.range.hash(h);
        self.displayed_counts().hash(h);
        self.trimmed_lines.hash(h);
        self.open.hash(h);
        self.ask.hash(h);
        self.duration.hash(h);
        // A live call's elapsed time is drawn as it runs, and a settled one's
        // is a fixed number of seconds — so the reading, not the instants, is
        // what the drawing depends on.
        self.elapsed().as_secs().hash(h);
    }

    pub fn snap_counters(&mut self) {
        self.counter.shown_added = self.progress.added as f64;
        self.counter.shown_removed = self.progress.removed as f64;
        self.counter.target_added = self.counter.shown_added;
        self.counter.target_removed = self.counter.shown_removed;
        self.counter.duration = std::time::Duration::ZERO;
    }

    fn retarget_counters(&mut self) {
        let now = Instant::now();
        let _ = self.advance_counters(now);
        self.counter.from_added = self.counter.shown_added;
        self.counter.from_removed = self.counter.shown_removed;
        self.counter.target_added = self.progress.added as f64;
        self.counter.target_removed = self.progress.removed as f64;
        let distance = (self.counter.target_added - self.counter.from_added)
            .abs()
            .max((self.counter.target_removed - self.counter.from_removed).abs());
        if distance < 1.0 {
            return;
        }
        let is_fresh_dual = self.counter.from_removed < 1.0
            && self.counter.from_added < 1.0
            && (self.counter.target_removed - self.counter.from_removed) >= 1.0
            && (self.counter.target_added - self.counter.from_added) >= 1.0;
        let min_duration = if is_fresh_dual {
            520.0
        } else {
            super::runtime_configuration().number("ui.counter_min_duration") as f64
        };
        let millis = (distance * 26.0).clamp(
            min_duration
                .min(super::runtime_configuration().number("ui.counter_max_duration") as f64),
            super::runtime_configuration().number("ui.counter_max_duration") as f64,
        ) as u64;
        self.counter.started = now;
        self.counter.duration = std::time::Duration::from_millis(millis);
    }
}

/// One thing on the screen.
///
/// The tool variant is boxed because it is an order of magnitude larger than
/// the others — it carries a command's whole transcript — and a transcript is a
/// `Vec` of these: without the box, every plain line of text would reserve the
/// space of a running terminal.
pub enum Block {
    User {
        text: String,
        time: Option<String>,
        media: Vec<runeengine::media::Attachment>,
        /// A display-only fold: the stored/model-visible message stays whole.
        open: bool,
    },
    Text {
        text: String,
    },
    /// What the model reasoned, and whether the reader has opened it.
    Thinking {
        text: String,
        open: bool,
    },
    Tool(Box<Tool>),
    Notice {
        text: String,
        bad: bool,
    },
    /// A notification of Rune's own — a background task ended — told apart from
    /// the user's words by the engine (a context row, or a turn that opens on
    /// one), never by what its text happens to say.
    Notification {
        text: String,
        time: Option<String>,
    },
    /// One thin line across the transcript: a mark, not a message.
    Divider {
        text: String,
    },
}

impl Block {
    #[cfg(test)]
    pub fn user(text: impl Into<String>) -> Block {
        Block::user_timed(text, None, Vec::new())
    }

    pub fn user_timed(
        text: impl Into<String>,
        time: Option<String>,
        media: Vec<runeengine::media::Attachment>,
    ) -> Block {
        Block::User {
            text: text.into(),
            time,
            media,
            open: false,
        }
    }

    #[cfg(test)]
    pub fn tool(&self) -> Option<&Tool> {
        match self {
            Block::Tool(t) => Some(t),
            _ => None,
        }
    }

    pub fn tool_mut(&mut self) -> Option<&mut Tool> {
        match self {
            Block::Tool(t) => Some(t),
            _ => None,
        }
    }

    /// A block holding this tool, ready to be pushed onto the transcript.
    pub fn of(tool: Tool) -> Block {
        Block::Tool(Box::new(tool))
    }

    /// A fingerprint of everything about this block that its drawing reads.
    ///
    /// The transcript keeps the lines it drew for each block and redraws only
    /// the ones this number has moved for — see `draw::Transcript`. The test
    /// for "has it changed" therefore has to be *complete*: a field the
    /// renderer reads and this omits is a block that stops updating on screen,
    /// which is the one failure a cache like this cannot be allowed to have.
    /// A new field on `Block` belongs in here in the same commit that adds it.
    pub fn stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Block::User {
                text,
                time,
                media,
                open,
            } => {
                0u8.hash(&mut h);
                text.hash(&mut h);
                time.hash(&mut h);
                open.hash(&mut h);
                for m in media {
                    m.name.hash(&mut h);
                    m.mime.hash(&mut h);
                    m.path.hash(&mut h);
                    m.size.hash(&mut h);
                }
            }
            Block::Text { text } => {
                1u8.hash(&mut h);
                text.hash(&mut h);
            }
            Block::Notification { text, time } => {
                5u8.hash(&mut h);
                text.hash(&mut h);
                time.hash(&mut h);
            }
            Block::Divider { text } => {
                6u8.hash(&mut h);
                text.hash(&mut h);
            }
            Block::Thinking { text, open } => {
                2u8.hash(&mut h);
                text.hash(&mut h);
                open.hash(&mut h);
            }
            Block::Tool(tool) => {
                3u8.hash(&mut h);
                tool.stamp(&mut h);
            }
            Block::Notice { text, bad } => {
                4u8.hash(&mut h);
                text.hash(&mut h);
                bad.hash(&mut h);
            }
        }
        h.finish()
    }
}

// ─── The scanner ──────────────────────────────────────────────────────────────

/// Read what can be known from a prefix of a tool call's arguments.
pub fn scan(tool: &str, raw: &str) -> Progress {
    let mut progress = Progress::default();
    ArgScanner::default().feed(tool, raw, &mut progress);
    progress
}

/// The keys a stream is decoded for. Any other string value is skipped.
const ACTION: usize = 0;
const PATH: usize = 1;
const TEXT: usize = 2;
const NEW_STR: usize = 3;
const OLD_STR: usize = 4;
const COMMAND: usize = 5;
const KEYS: [&str; 6] = ["action", "path", "text", "new_str", "old_str", "command"];

/// An incremental reader of a call's JSON arguments.
///
/// It keeps its place in the text and decodes only what was appended, writing
/// straight into the `Progress` the step draws from. The newest occurrence of a
/// key wins, as it should for a multi-file call whose current file is the
/// last one opened. Escapes are decoded in full — `\uXXXX` and surrogate pairs
/// included — so non-ASCII text streams as itself, not as `u00e9`.
#[derive(Debug, Default)]
pub struct ArgScanner {
    /// Bytes of the argument text already read.
    pos: usize,
    /// Inside a string: whether it is a key, a value being kept, or skipped.
    target: Option<Target>,
    escape: Escape,
    /// The first half of a surrogate pair, waiting for its second.
    high: Option<u16>,
    key: String,
    after_colon: bool,
    /// The `action` sentence so far.
    pub action: String,
    /// The `command` so far (terminal calls).
    pub command: String,
    path: String,
    new_lines: usize,
    old_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Key,
    Value(usize),
    Skip,
}

#[derive(Debug, Default, PartialEq)]
enum Escape {
    #[default]
    None,
    Backslash,
    Unicode(u16, u8),
}

impl ArgScanner {
    pub fn feed(&mut self, tool: &str, raw: &str, progress: &mut Progress) {
        let (new_key, old_key) = match tool {
            "create_file" => (Some(TEXT), None),
            "patch_file" => (Some(NEW_STR), Some(OLD_STR)),
            _ => (None, None),
        };
        for c in raw[self.pos..].chars() {
            match self.target {
                Some(target) => self.in_string(c, target, new_key, old_key, progress),
                None => match c {
                    '"' => {
                        let target = if self.after_colon {
                            match KEYS.iter().position(|k| *k == self.key) {
                                Some(field) => {
                                    self.open_value(field, new_key, old_key, progress);
                                    Target::Value(field)
                                }
                                None => Target::Skip,
                            }
                        } else {
                            self.key.clear();
                            Target::Key
                        };
                        self.after_colon = false;
                        self.target = Some(target);
                    }
                    ':' => self.after_colon = true,
                    c if c.is_whitespace() => {}
                    _ => self.after_colon = false,
                },
            }
        }
        self.pos = raw.len();
        progress.added = lines(&progress.new_text, self.new_lines);
        progress.removed = lines(&progress.old_text, self.old_lines);
        if matches!(tool, "inspect_file") {
            progress.range = range_after(raw);
        }
    }

    /// A value for a kept key begins: its newest occurrence replaces the last.
    fn open_value(
        &mut self,
        field: usize,
        new_key: Option<usize>,
        old_key: Option<usize>,
        progress: &mut Progress,
    ) {
        match field {
            ACTION => self.action.clear(),
            COMMAND => self.command.clear(),
            PATH => {
                self.path.clear();
                progress.path = None;
            }
            f if Some(f) == new_key => {
                progress.new_text.clear();
                self.new_lines = 0;
            }
            f if Some(f) == old_key => {
                progress.old_text.clear();
                self.old_lines = 0;
            }
            _ => {}
        }
    }

    fn in_string(
        &mut self,
        c: char,
        target: Target,
        new_key: Option<usize>,
        old_key: Option<usize>,
        progress: &mut Progress,
    ) {
        let decoded = match std::mem::take(&mut self.escape) {
            Escape::Backslash => match c {
                'n' => Some('\n'),
                't' => Some('\t'),
                'r' => Some('\r'),
                'b' => Some('\u{8}'),
                'f' => Some('\u{c}'),
                'u' => {
                    self.escape = Escape::Unicode(0, 0);
                    None
                }
                other => Some(other),
            },
            Escape::Unicode(value, digits) => {
                let value = value * 16 + c.to_digit(16).unwrap_or(0) as u16;
                if digits + 1 < 4 {
                    self.escape = Escape::Unicode(value, digits + 1);
                    None
                } else if (0xD800..0xDC00).contains(&value) {
                    self.high = Some(value);
                    None
                } else {
                    let code = match self.high.take() {
                        Some(high) if (0xDC00..0xE000).contains(&value) => {
                            0x10000
                                + ((u32::from(high) - 0xD800) << 10)
                                + (u32::from(value) - 0xDC00)
                        }
                        _ => u32::from(value),
                    };
                    Some(char::from_u32(code).unwrap_or('\u{FFFD}'))
                }
            }
            Escape::None => match c {
                '\\' => {
                    self.escape = Escape::Backslash;
                    None
                }
                '"' => {
                    self.target = None;
                    if target == Target::Value(PATH) {
                        progress.path = (!self.path.is_empty()).then(|| self.path.clone());
                    }
                    None
                }
                other => Some(other),
            },
        };
        let Some(c) = decoded else { return };
        match target {
            Target::Key => self.key.push(c),
            Target::Skip => {}
            Target::Value(ACTION) => self.action.push(c),
            Target::Value(COMMAND) => self.command.push(c),
            Target::Value(PATH) => self.path.push(c),
            Target::Value(f) if Some(f) == new_key => {
                progress.new_text.push(c);
                self.new_lines += usize::from(c == '\n');
            }
            Target::Value(f) if Some(f) == old_key => {
                progress.old_text.push(c);
                self.old_lines += usize::from(c == '\n');
            }
            Target::Value(_) => {}
        }
    }
}

/// `str::lines().count()` from a running newline count, without a pass over
/// the text.
fn lines(text: &str, newlines: usize) -> usize {
    newlines + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

fn range_after(raw: &str) -> Option<(i64, i64)> {
    let pos = raw.find("\"view_range\"")?;
    let slice = &raw[pos + "\"view_range\"".len()..];
    let bracket = slice.find('[')?;
    let closing = slice[bracket..].find(']')?;
    let content = &slice[bracket + 1..bracket + closing];
    let mut nums = content
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok());
    Some((nums.next()?, nums.next()?))
}

fn scan_value(tool: &str, args: &Value) -> Progress {
    match tool {
        "inspect_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).map(String::from);
            let range = args
                .get("view_range")
                .and_then(|v| v.as_array())
                .and_then(|arr| Some((arr.first()?.as_i64()?, arr.get(1)?.as_i64()?)));
            Progress {
                path,
                added: 0,
                removed: 0,
                new_text: String::new(),
                old_text: String::new(),
                range,
            }
        }
        "create_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).map(String::from);
            let new_text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let added = new_text.lines().count();
            Progress {
                path,
                added,
                removed: 0,
                new_text,
                old_text: String::new(),
                range: None,
            }
        }
        "patch_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).map(String::from);
            let old_text = args
                .get("old_str")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let new_text = args
                .get("new_str")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let removed = old_text.lines().count();
            let added = if new_text.is_empty() {
                0
            } else {
                new_text.lines().count()
            };
            Progress {
                path,
                added,
                removed,
                new_text,
                old_text,
                range: None,
            }
        }
        _ => scan(tool, &args.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Audit T6/U5: fed in any pieces, the scanner decodes exactly what one
    /// pass over the whole text would — escapes, non-ASCII and surrogate pairs
    /// included — and a quote inside a file body never moves the path.
    #[test]
    fn streamed_arguments_decode_the_same_however_they_are_split() {
        let raw = r#"{"action": "write it", "path": "src/a.rs", "text": "caf\u00e9 \ud83d\ude00\n\"path\": \"x\"\nend"}"#;
        let whole = scan("create_file", raw);
        assert_eq!(whole.path.as_deref(), Some("src/a.rs"));
        assert_eq!(whole.new_text, "café 😀\n\"path\": \"x\"\nend");
        assert_eq!(whole.added, 3);
        for split in 1..raw.len() {
            if !raw.is_char_boundary(split) {
                continue;
            }
            let mut tool = Tool::new("c".into(), "create_file".into(), String::new());
            tool.absorb(&raw[..split]);
            tool.absorb(&raw[split..]);
            assert_eq!(tool.progress, whole, "split at {split}");
            assert_eq!(tool.action, "write it");
        }
    }

    /// The cost of streaming a body is linear in its size: every byte is read
    /// once. Re-decoding the prefix per delta took seconds at this size.
    #[test]
    fn a_large_body_streams_in_linear_time() {
        let mut tool = Tool::new("c".into(), "create_file".into(), String::new());
        tool.absorb(r#"{"action":"w","path":"big.txt","text":""#);
        let started = Instant::now();
        for _ in 0..60_000 {
            tool.absorb("a");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(tool.progress.new_text.len(), 60_000);
    }

    #[test]
    fn the_path_appears_as_soon_as_it_is_complete_and_not_before() {
        assert_eq!(
            scan("create_file", "{\"action\": \"w\", \"path\": \"src/ma").path,
            None
        );
        assert_eq!(
            scan(
                "create_file",
                "{\"action\": \"w\", \"path\": \"src/main.rs\", \"text\": \""
            )
            .path,
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn the_added_counter_grows_with_every_line_that_arrives() {
        let mut raw = String::from("{\"path\": \"a.txt\", \"text\": \"one");
        assert_eq!(scan("create_file", &raw).added, 1);
        raw.push_str("\\ntwo");
        assert_eq!(scan("create_file", &raw).added, 2);
        raw.push_str("\\nthree\"}");
        assert_eq!(scan("create_file", &raw).added, 3);
    }

    #[test]
    fn a_patch_counts_both_directions() {
        let raw = r#"{"path": "a.rs", "old_str": "a\nb", "new_str": "a\nb\nc"}"#;
        let p = scan("patch_file", raw);
        assert_eq!((p.removed, p.added), (2, 3));
    }

    #[test]
    fn a_trailing_newline_does_not_invent_an_extra_line() {
        // What `str::lines()` does, and therefore what the tool will report.
        let raw = r#"{"path": "a.txt", "text": "one\ntwo\n"}"#;
        assert_eq!(scan("create_file", raw).added, 2);
        let no_trailer = r#"{"path": "a.txt", "text": "one\ntwo"}"#;
        assert_eq!(scan("create_file", no_trailer).added, 2);
    }

    #[test]
    fn an_empty_replacement_is_a_deletion_not_a_line() {
        let raw = r#"{"path": "a.rs", "old_str": "gone\nline", "new_str": ""}"#;
        let p = scan("patch_file", raw);
        assert_eq!((p.removed, p.added), (2, 0));
    }

    #[test]
    fn a_quote_inside_the_content_does_not_end_it_early() {
        let raw = r#"{"path": "a.rs", "text": "say \"hi\"\nand more"#;
        assert_eq!(scan("create_file", raw).added, 2);
    }

    #[test]
    fn the_batch_form_reveals_its_last_item_when_it_arrives() {
        let raw = r#"{"action":"two","files":[{"path":"a","text":"x"},{"path":"b","text":"y"}]}"#;
        let p = scan("create_file", raw);
        assert_eq!(p.path.as_deref(), Some("b"));
        assert_eq!(p.new_text, "y");
    }

    #[test]
    fn streamed_arguments_do_not_start_elapsed_time_before_execution() {
        let mut tool = Tool::new("queued".into(), "terminal_use".into(), String::new());
        tool.absorb(r#"{"action":"later","command":"sleep 70"}"#);
        assert!(
            tool.started.is_none(),
            "queued argument streaming must not start a tool timer"
        );
        assert!(tool.elapsed().is_zero());
        tool.settle(&json!({"action":"later","command":"sleep 70"}));
        assert!(
            tool.started.is_some(),
            "ToolCall settlement is the execution boundary"
        );
    }

    #[test]
    fn one_finished_tool_cannot_age_a_later_queued_tool() {
        let mut first = Tool::new("first".into(), "terminal_use".into(), "first".into());
        first.settle(&json!({"command":"sleep 300"}));
        let began = Instant::now() - std::time::Duration::from_secs(300);
        first.started = Some(began);
        first.ended = Some(Instant::now());
        assert!(first.elapsed() >= std::time::Duration::from_secs(299));

        let mut second = Tool::new("second".into(), "terminal_use".into(), String::new());
        second.absorb(r#"{"command":"sleep 60"}"#);
        assert!(second.started.is_none());
        assert!(
            second.elapsed().is_zero(),
            "a queued call must remain at 0s after another call finishes"
        );
        second.settle(&json!({"command":"sleep 60"}));
        assert!(second.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn settled_arguments_and_streamed_text_agree() {
        let mut tool = Tool::new("c1".into(), "create_file".into(), "write".into());
        tool.absorb(r#"{"path": "a.txt", "text": "one\ntwo"}"#);
        let streamed = tool.progress.clone();
        tool.settle(&json!({ "path": "a.txt", "text": "one\ntwo" }));
        assert_eq!(
            streamed, tool.progress,
            "the counter must not jump when it settles"
        );
        assert_eq!(tool.progress.added, 2);
    }

    #[test]
    fn nothing_in_the_arguments_yet_means_nothing_claimed() {
        let p = scan("create_file", "{\"acti");
        assert_eq!(p, Progress::default());
    }
}

#[cfg(test)]
mod recorded {
    use super::*;

    /// The argument deltas of one real `create_file` call, as Xiaomi MiMo
    /// actually streamed them. Recorded rather than imagined: the shapes that
    /// break a scanner — a key split across two chunks, an escaped quote inside
    /// the content — are the ones nobody thinks to write by hand.
    const DELTAS: &str = include_str!("../../tests/deltas.json");

    #[test]
    fn the_counter_never_goes_backwards_on_a_real_stream() {
        let deltas: Vec<String> = serde_json::from_str(DELTAS).unwrap();
        let mut tool = Tool::new("c1".into(), "create_file".into(), String::new());
        let mut highest = 0;
        let mut path_seen = false;
        for delta in &deltas {
            tool.absorb(delta);
            let added = tool.progress.added;
            assert!(
                added >= highest,
                "the line counter fell from {highest} to {added} — a number that goes backwards \
                 reads as work being undone"
            );
            highest = added;
            if tool.progress.path.is_some() {
                path_seen = true;
            }
        }
        assert!(
            path_seen,
            "the path must appear while the file is still being written"
        );
        assert!(
            highest > 5,
            "a 13-line file should have counted well past five"
        );
    }

    #[test]
    fn the_live_count_matches_what_the_tool_reports_when_it_finishes() {
        // The engine counts with `str::lines()`. If the scanner disagrees, the
        // number jumps at the last moment and the user sees the tool "correct"
        // a count that was right all along.
        let deltas: Vec<String> = serde_json::from_str(DELTAS).unwrap();
        let mut tool = Tool::new("c1".into(), "create_file".into(), String::new());
        for delta in &deltas {
            tool.absorb(delta);
        }
        let args: serde_json::Value = serde_json::from_str(&tool.raw).unwrap();
        let text = args["text"]
            .as_str()
            .expect("the recorded call writes a file");
        assert_eq!(tool.progress.added, text.lines().count());
        assert_eq!(tool.action, "Create greet.py with a greet function");
    }
}
