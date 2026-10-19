//! Decoding what the terminal sends into keys a program can reason about.
//!
//! # What a terminal can actually send
//!
//! Less than people expect, and the keymap had to be designed around it. A
//! terminal transmits control **characters**, so `Ctrl` plus a letter arrives as
//! one byte (`Ctrl-O` = 0x0F) and there are only 26 of them. `Ctrl-,`, `Ctrl-1`
//! and `Ctrl-Shift-anything` send nothing at all unless the terminal speaks a
//! newer keyboard protocol, which most do not. Every shortcut in this program is
//! therefore a control letter, an `Alt` letter (ESC-prefixed), a function key,
//! or `Shift-Tab` — the ones that genuinely arrive everywhere.
//!
//! `Ctrl-S` is the one to be careful with: it is XOFF, and only reaches a
//! program because [`super::term`] turns off flow control. It is deliberately
//! *not* used as a shortcut — muscle memory that freezes every other terminal is
//! not a feature — and `Ctrl-O` carries the job instead.
//!
//! # The mouse, and the trade it costs
//!
//! A program that turns on mouse reporting owns button one, so native terminal
//! selection cannot coexist with clickable steps. Rune replaces the part it
//! takes: button-held motion updates an in-app selection, release copies it,
//! and release without motion remains a click. OpenCode uses the same guard — a
//! selected range prevents the mouse-up action under it from firing.
//!
//! `mouse` remains a setting. Off asks for no pointer events, restores alternate
//! scroll, and hands selection back to the terminal.
//!
//! # Paste
//!
//! Bracketed paste is not a nicety. Without it, pasting twenty lines of code
//! looks like twenty Enter presses, and the first line is sent as a prompt while
//! the rest lands in an empty composer. The terminal wraps a paste in
//! `ESC[200~ … ESC[201~`, and that whole span becomes one [`Key::Paste`].

use std::sync::mpsc::Sender;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    AltLeft,
    AltRight,
    Home,
    End,
    PageUp,
    PageDown,
    /// `↓` with Ctrl held. Read on its own because it has a meaning of its own
    /// (back to the newest row of the conversation); with the modifier
    /// dropped it was a plain `↓`, which moves the composer's caret.
    CtrlDown,
    /// A modified motion is never silently demoted to an unmodified arrow.
    Motion {
        direction: Direction,
        ctrl: bool,
        shift: bool,
    },
    /// A whole pasted span, delivered at once.
    Paste(String),
    /// A newline inside the composer rather than a send. `Alt+Enter` is the
    /// gesture every terminal can produce; `Shift+Enter` and `Ctrl+Enter` reach
    /// here too where the terminal speaks the newer keyboard protocol.
    NewLine,
    /// Something the pointer did, in **screen** coordinates, one-based, exactly
    /// as the terminal counts them.
    Mouse(Mouse),
    /// The terminal answering what the painter asked it — never a keystroke.
    Terminal(Report),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// What a terminal says about itself on the input stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// `ESC [ 0 n`: it has read everything sent before the question, so a
    /// frame written then has reached it (see `draw::Screen::paint`).
    Ready,
    /// `ESC [ ? 2026 ; n $ y`: whether it shows a frame between the
    /// synchronized-update marks whole, never half-drawn.
    Synchronized(bool),
}

/// What the pointer did and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: Click,
    /// One-based screen row, as the terminal reports it.
    pub row: u16,
    /// One-based screen column.
    pub col: u16,
    /// The keys held with it, as far as the terminal says.
    pub modifiers: Modifiers,
}

/// Ctrl and Alt held during a pointer report.
///
/// Shift is not here: terminals keep it for themselves (Shift bypasses an
/// application's mouse mode so the person can select text), and none reports
/// it reliably. What a terminal takes for its own — Konsole zooms on
/// Ctrl+wheel, Qt turns Alt+wheel sideways — never arrives at all, so a
/// gesture read from these is always an extra, never the only way to do
/// something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
}

impl Modifiers {
    pub(super) fn wheel_multiplier(self) -> usize {
        if self.ctrl || self.alt {
            super::FAST_WHEEL
        } else {
            1
        }
    }

    #[cfg(test)]
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
    };

    /// The bits every mouse encoding carries in its button field: 8 for
    /// Alt (Meta), 16 for Ctrl.
    fn of_button(button: u16) -> Modifiers {
        Modifiers {
            ctrl: button & 16 != 0,
            alt: button & 8 != 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    /// Button one went down. The only one that activates anything.
    Press,
    /// Button one came up. Kept distinct so a press is never counted twice.
    Release,
    /// Button one is held while the pointer moves. Rune uses it for in-app text
    /// selection; a release with no Drag remains an ordinary click target.
    Drag,
    WheelUp,
    WheelDown,
}

/// Bytes that arrived on stdin before this thread existed.
///
/// [`super::term`] reads stdin directly at start-up to ask the terminal what
/// colour it is, and a user typing during that moment would otherwise lose the
/// keystrokes to that read. What was not the terminal's answer is put here and
/// decoded as if it had arrived normally.
fn typed_early() -> &'static std::sync::Mutex<Vec<u8>> {
    static EARLY: std::sync::OnceLock<std::sync::Mutex<Vec<u8>>> = std::sync::OnceLock::new();
    EARLY.get_or_init(Default::default)
}

/// Hand over what was read before the decoder started.
pub fn feed(bytes: Vec<u8>) {
    if !bytes.is_empty() {
        runeengine::panic::lock(typed_early()).extend(bytes);
    }
}

/// Who the reader hands keys to now — the picker, then the window it opens
/// — and which taker that is, counted up at every change of hands. `None`
/// between two takers: keys read then are held for the next ([`HELD`]).
static ACTIVE_SENDER: std::sync::Mutex<Option<(Sender<Key>, u64)>> = std::sync::Mutex::new(None);
static SPAWN_ONCE: std::sync::Once = std::sync::Once::new();

/// Which taker had the reader last, so the next one is told apart.
static TAKERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Keys read while no taker had the reader — between the picker choosing a
/// session and the window it opens taking the reader, which can be seconds
/// while the session's host starts. The reader of a fresh process does not
/// run yet then and keys wait in the terminal; the picker's does, and those
/// keys went to the picker gone and were lost (measured 2026-10-09). Handed to
/// the next taker, in order, as it takes the reader.
static HELD: std::sync::Mutex<Vec<Key>> = std::sync::Mutex::new(Vec::new());

/// Keys the reader has handed to the taker it has now — a terminal's report
/// is not one ([`Key::Terminal`]). The taker counts the keys it applied the
/// same way, from zero when it took the reader; while the two differ, a key
/// is on its way and an `exec` would lose it. Keys an earlier taker was given
/// are not counted: a window opened from the picker would otherwise never
/// match, and never take a release over (measured 2026-10-09 on 0.3.8).
static SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Keys handed to the taker that has the reader now (see [`SENT`]).
pub fn sent() -> u64 {
    SENT.load(std::sync::atomic::Ordering::Acquire)
}

/// Make `tx` the one the reader hands keys to, its count starting at zero,
/// and hand it first what was held while nobody had the reader. Returns
/// which taker it is.
fn take_reader(tx: Sender<Key>) -> u64 {
    let mut active = runeengine::panic::lock(&ACTIVE_SENDER);
    let taker = TAKERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
    SENT.store(0, std::sync::atomic::Ordering::Release);
    let held = std::mem::take(&mut *runeengine::panic::lock(&HELD));
    *active = Some((tx, taker));
    for key in held {
        deliver(&mut active, key);
    }
    taker
}

/// The taker that has the reader lets it go: what it was handed and has not
/// taken out of `rx` yet is held for the next taker, ahead of anything read
/// after.
pub fn release(rx: &std::sync::mpsc::Receiver<Key>) {
    let mut active = runeengine::panic::lock(&ACTIVE_SENDER);
    *active = None;
    let unread: Vec<Key> = rx.try_iter().collect();
    let mut held = runeengine::panic::lock(&HELD);
    held.splice(0..0, unread);
}

/// Hand `key` to the taker that has the reader — counted when typed — or
/// hold it when there is none, or the one there was is gone. Under the lock
/// that changes hands, so no key is counted for, or lost between, two takers.
fn deliver(active: &mut Option<(Sender<Key>, u64)>, key: Key) {
    let typed = is_typed(&key);
    let Some((tx, _)) = active.as_ref() else {
        runeengine::panic::lock(&HELD).push(key);
        return;
    };
    match tx.send(key) {
        Ok(()) => {
            if typed {
                SENT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        Err(std::sync::mpsc::SendError(key)) => {
            *active = None;
            runeengine::panic::lock(&HELD).push(key);
        }
    }
}

/// Whether `key` is one the window counts as typed.
pub fn is_typed(key: &Key) -> bool {
    !matches!(key, Key::Terminal(_))
}

/// What the window asks of the reader between two reads: to stop reading, so
/// the process can `exec` a newer build without a byte read and not yet
/// handed on ([`park`]).
#[derive(Default)]
struct Gate {
    park: bool,
    parked: bool,
}

static GATE: std::sync::Mutex<Gate> = std::sync::Mutex::new(Gate {
    park: false,
    parked: false,
});
static GATE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// What wakes the reader out of its wait for stdin (`platform::InputWaker`).
fn wake() -> Option<&'static runeengine::platform::InputWaker> {
    static WAKE: std::sync::OnceLock<Option<runeengine::platform::InputWaker>> =
        std::sync::OnceLock::new();
    WAKE.get_or_init(|| runeengine::platform::InputWaker::new().ok())
        .as_ref()
}

/// The reader, stopped: it reads nothing until this is dropped. Kept alive
/// across an `exec` — the process image is gone then, and what was typed
/// meanwhile waits in the terminal for the next build to read.
pub struct Parked(());

impl Drop for Parked {
    fn drop(&mut self) {
        runeengine::panic::lock(&GATE).park = false;
        GATE_MOVED.notify_all();
    }
}

/// Stop the reader between two reads, with nothing it read still held back
/// (no half a key sequence waiting for its end). `None` when it does not stop
/// within `within` — it holds half a sequence, or no reader runs — and then it
/// reads on as before. Whatever it read before stopping has been handed to the
/// window: compare [`sent`] with what the window applied.
pub fn park(within: std::time::Duration) -> Option<Parked> {
    let waker = wake()?;
    runeengine::panic::lock(&GATE).park = true;
    waker.wake();
    let deadline = std::time::Instant::now() + within;
    let mut gate = runeengine::panic::lock(&GATE);
    while !gate.parked {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            gate.park = false;
            drop(gate);
            GATE_MOVED.notify_all();
            return None;
        }
        gate = GATE_MOVED
            .wait_timeout(gate, left)
            .map(|(gate, _)| gate)
            .unwrap_or_else(|error| error.into_inner().0);
    }
    Some(Parked(()))
}

/// Wait until stdin has bytes or the window wakes the reader. `true` when
/// stdin can be read.
fn wait_for_input() -> bool {
    runeengine::platform::wait_stdin(None, wake())
}

/// Stop here while the window asks, unless half a key is held (`pending`).
fn honour_gate(pending: &[u8]) {
    let mut gate = runeengine::panic::lock(&GATE);
    if !gate.park || !pending.is_empty() {
        return;
    }
    gate.parked = true;
    GATE_MOVED.notify_all();
    while gate.park {
        gate = GATE_MOVED
            .wait(gate)
            .unwrap_or_else(|error| error.into_inner());
    }
    gate.parked = false;
}

fn send(key: Key) {
    #[cfg(debug_assertions)]
    trace(&format!("key {key:?}"));
    deliver(&mut runeengine::panic::lock(&ACTIVE_SENDER), key);
}

/// TEMPORARY diagnosis (Windows PTY `~` in the MCP form): what the reader
/// read and decoded, to the file `RUNE_DEBUG_KEYS` names. Debug builds only;
/// removed once the cause is known.
#[cfg(debug_assertions)]
fn trace(line: &str) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<
        Option<(std::time::Instant, std::sync::Mutex<std::fs::File>)>,
    > = std::sync::OnceLock::new();
    let Some((began, file)) = FILE
        .get_or_init(|| {
            let path = std::env::var_os("RUNE_DEBUG_KEYS")?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some((std::time::Instant::now(), std::sync::Mutex::new(file)))
        })
        .as_ref()
    else {
        return;
    };
    let ms = began.elapsed().as_millis();
    let _ = writeln!(runeengine::panic::lock(file), "{ms} {line}");
}

/// Read stdin forever, decoding as it goes.
///
/// Its own thread because the alternative is a main loop that cannot animate
/// while waiting for a key, and because a blocking read must never be the thing
/// that stops a model's output from being drawn.
///
/// Read straight from the descriptor, never through `std::io::Stdin`: its
/// buffer can hold bytes the kernel no longer reports, and the wait before
/// each read ([`wait_for_input`]) would then sleep on keys already typed.
pub fn spawn(tx: Sender<Key>) {
    take_reader(tx);
    SPAWN_ONCE.call_once(|| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut pending: Vec<u8> = std::mem::take(&mut *runeengine::panic::lock(typed_early()));
            loop {
                // A lone `ESC` is the start of every arrow key and also a key in
                // its own right, so it cannot be decoded until either the rest of a
                // sequence arrives or enough time passes that nothing will. Without
                // the wait, `Esc` did nothing at all until the *next* keystroke —
                // and that keystroke was then swallowed as the tail of it, so
                // pressing `Esc` and then typing lost a letter.
                //
                // `ESCAPE_MS` is the same figure vim and readline settled on: long
                // enough that a terminal sending `ESC [ A` in three packets is never
                // split, short enough that `Esc` feels like a key.
                if (pending == [0x1b] || pending == [0x18])
                    && !waiting(super::runtime_configuration().number("ui.escape_timeout") as i32)
                {
                    let escape = pending == [0x1b];
                    #[cfg(debug_assertions)]
                    trace("escape timeout");
                    pending.clear();
                    if escape {
                        send(Key::Esc);
                    }
                    continue;
                }
                honour_gate(&pending);
                if !wait_for_input() {
                    continue;
                }
                // The window may have asked to stop while this waited.
                honour_gate(&pending);
                // Stdin was just reported readable: this read does not wait.
                match runeengine::platform::read_stdin(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        #[cfg(debug_assertions)]
                        trace(&format!("read {:?}", String::from_utf8_lossy(&buf[..n])));
                        pending.extend_from_slice(&buf[..n]);
                        // Decode by offset and drop the consumed bytes once: a
                        // burst of keys used to shift the whole buffer per key
                        // and take the sender lock per key (audit T10).
                        let mut at = 0;
                        while at < pending.len() {
                            match decode(&pending[at..]) {
                                Decoded::Need => break,
                                Decoded::Skip(used) => at += used,
                                Decoded::Key(key, used) => {
                                    at += used;
                                    send(key);
                                }
                            }
                        }
                        pending.drain(..at);
                    }
                    Err(error) => {
                        if error.kind() != std::io::ErrorKind::Interrupted {
                            break;
                        }
                    }
                }
            }
        });
    });
}

/// How long an unresolved `ESC` waits for the rest of its sequence.
/// Whether more input arrived within `ms`. `false` means the terminal has
/// stopped talking and whatever is pending is all there is.
fn waiting(ms: i32) -> bool {
    let ms = u64::try_from(ms).unwrap_or(0);
    runeengine::platform::wait_stdin(Some(std::time::Duration::from_millis(ms)), None)
}

pub enum Decoded {
    /// A key, and how many bytes it consumed.
    Key(Key, usize),
    /// Bytes that mean nothing to us; drop them.
    Skip(usize),
    /// An incomplete sequence — wait for more.
    Need,
}

pub fn decode(bytes: &[u8]) -> Decoded {
    let Some(&first) = bytes.first() else {
        return Decoded::Need;
    };
    match first {
        0x1b => decode_escape(bytes),
        b'\r' if bytes.starts_with(b"\r\n") => Decoded::Key(Key::NewLine, 2),
        b'\r' => Decoded::Key(Key::Enter, 1),
        b'\n' => Decoded::Key(Key::NewLine, 1),
        b'\t' => Decoded::Key(Key::Tab, 1),
        0x7f | 0x08 => Decoded::Key(Key::Backspace, 1),
        0x1f => Decoded::Key(Key::Ctrl('_'), 1),
        // The unsupported Readline prefix is consumed as a sequence, never a
        // stray Ctrl-U that deletes text after somebody asked for undo.
        0x18 if bytes.len() == 1 => Decoded::Need,
        0x18 if bytes[1] == 0x15 => Decoded::Skip(2),
        // Control letters. 0x00 is Ctrl-Space and 0x1f Ctrl-/, neither of which
        // is a letter, so the range is walked rather than mapped blindly.
        c @ 0x01..=0x1a => Decoded::Key(Key::Ctrl((b'a' + c - 1) as char), 1),
        c if c < 0x20 => Decoded::Skip(1),
        _ => decode_utf8(bytes),
    }
}

fn decode_escape(bytes: &[u8]) -> Decoded {
    if bytes.len() == 1 {
        // A lone ESC is ambiguous until the next byte arrives, and waiting
        // forever would make Esc unusable. The caller resolves it by timeout.
        return Decoded::Need;
    }
    match bytes[1] {
        b'[' => decode_csi(bytes),
        b'O' if bytes.len() >= 3 => {
            let key = match bytes[2] {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                // rxvt's spelling of Ctrl+↓.
                b'b' => Key::CtrlDown,
                _ => return Decoded::Skip(3),
            };
            Decoded::Key(key, 3)
        }
        b'O' => Decoded::Need,
        b']' => decode_osc(bytes),
        b'_' | b'P' | b'^' => decode_report_string(bytes),
        // `Alt+Enter` on every terminal that does not speak the newer keyboard
        // protocol: the ESC prefix, then the Return the key already sends.
        b'\r' | b'\n' => Decoded::Key(Key::NewLine, 2),
        // ESC + letter is Alt + that letter.
        c if c.is_ascii_alphabetic() => Decoded::Key(Key::Alt(c.to_ascii_lowercase() as char), 2),
        0x1b => Decoded::Key(Key::Esc, 1),
        _ => Decoded::Skip(2),
    }
}

/// The longest operating-system reply read as one: past it, what looked like
/// one is dropped two bytes at a time like any unknown escape.
const OSC_MAX: usize = 512;

/// `ESC ] Ps ; Pt` ended by BEL or `ESC \` — the terminal answering a question
/// (the background colour, `ESC ] 11 ; rgb:… `), never keys. The colour probe
/// at start waits only so long (`ui.color_probe_timeout`): an answer that comes
/// later — a busy machine, a slow terminal, a build just `exec`ed — reaches the
/// key reader, and was typed into the picker's search as `11;rgb:1010/…`
/// (measured 2026-10-09). Only a numbered one is taken for a reply, so Alt+]
/// followed by typing is never swallowed.
fn decode_osc(bytes: &[u8]) -> Decoded {
    let body = &bytes[2..];
    let digits = body.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == body.len() {
        return if digits < 6 {
            Decoded::Need
        } else {
            Decoded::Skip(2)
        };
    }
    if digits == 0 || body[digits] != b';' {
        return Decoded::Skip(2);
    }
    for (at, &byte) in body.iter().enumerate().skip(digits + 1) {
        if at > OSC_MAX {
            return Decoded::Skip(2);
        }
        match byte {
            0x07 => return Decoded::Skip(2 + at + 1),
            0x1b => {
                return match body.get(at + 1) {
                    Some(b'\\') => Decoded::Skip(2 + at + 2),
                    // Cut short by another sequence: the reply ends here.
                    Some(_) => Decoded::Skip(2 + at),
                    None => Decoded::Need,
                };
            }
            _ => {}
        }
    }
    if body.len() > OSC_MAX {
        Decoded::Skip(2)
    } else {
        Decoded::Need
    }
}

fn decode_report_string(bytes: &[u8]) -> Decoded {
    for at in 2..bytes.len() {
        if bytes[at] == 0x07 {
            return Decoded::Skip(at + 1);
        }
        if bytes[at] == 0x1b && bytes.get(at + 1) == Some(&b'\\') {
            return Decoded::Skip(at + 2);
        }
    }
    if bytes.len() > 4096 {
        Decoded::Skip(bytes.len())
    } else {
        Decoded::Need
    }
}

fn decode_csi(bytes: &[u8]) -> Decoded {
    // Paste comes first: its terminator is a full sequence away, and every byte
    // between the markers is content, not keys.
    const START: &[u8] = b"\x1b[200~";
    if bytes.starts_with(START) {
        const END: &[u8] = b"\x1b[201~";
        return match find(&bytes[START.len()..], END) {
            Some(at) => {
                let body = &bytes[START.len()..START.len() + at];
                let text = String::from_utf8_lossy(body).into_owned();
                Decoded::Key(Key::Paste(text), START.len() + at + END.len())
            }
            None => Decoded::Need,
        };
    }

    // The old X10 mouse encoding: `ESC [ M` and then three raw bytes, which are
    // *not* CSI parameters and would be decoded as keystrokes by the loop below.
    // A terminal that does not understand `?1006h` answers in this instead, so
    // it is read rather than assumed away.
    if bytes.len() >= 3 && bytes[2] == b'M' {
        if bytes.len() < 6 {
            return Decoded::Need;
        }
        return match x10_mouse(bytes[3], bytes[4], bytes[5]) {
            Some(m) => Decoded::Key(Key::Mouse(m), 6),
            None => Decoded::Skip(6),
        };
    }

    // Everything else: ESC [ params final-byte.
    let mut end = 2;
    while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
        end += 1;
    }
    if end >= bytes.len() {
        return Decoded::Need;
    }
    let params = String::from_utf8_lossy(&bytes[2..end]).into_owned();
    let final_byte = bytes[end];
    let used = end + 1;

    // SGR mouse: `ESC [ < button ; col ; row M` for a press, `m` for a release.
    // A report this program did not ask for is dropped rather than decoded — a
    // stray `M` must never reach the composer as a keystroke.
    if final_byte == b'M' || final_byte == b'm' {
        return match sgr_mouse(&params, final_byte == b'M') {
            Some(m) => Decoded::Key(Key::Mouse(m), used),
            None => Decoded::Skip(used),
        };
    }

    // The terminal's answers to the painter's questions. A status report and a
    // mode report are not keys anyone can type.
    if final_byte == b'n' && params == "0" {
        return Decoded::Key(Key::Terminal(Report::Ready), used);
    }
    if final_byte == b'y' {
        if let Some(state) = params
            .strip_prefix("?2026;")
            .and_then(|rest| rest.strip_suffix('$'))
        {
            // 1 set, 2 reset: recognised. 0 unknown, 4 permanently reset.
            let honoured = matches!(state, "1" | "2");
            return Decoded::Key(Key::Terminal(Report::Synchronized(honoured)), used);
        }
        return Decoded::Skip(used);
    }

    if final_byte == b'u' {
        let mut parts = params.split(';');
        let code = parts
            .next()
            .and_then(|p| p.split(':').next()?.parse::<u32>().ok());
        let modifier = parts.next().unwrap_or("1");
        let mut bits = modifier.split(':');
        let flags = bits
            .next()
            .and_then(|m| m.parse::<u8>().ok())
            .unwrap_or(1)
            .saturating_sub(1);
        if bits.next() == Some("3") {
            return Decoded::Skip(used);
        }
        return code
            .and_then(|c| encoded_key(c, flags))
            .map_or(Decoded::Skip(used), |key| Decoded::Key(key, used));
    }
    if final_byte == b'~' && params.starts_with("27;") {
        let parts: Vec<_> = params.split(';').collect();
        let key = parts
            .get(1)
            .and_then(|m| m.parse::<u8>().ok())
            .zip(parts.get(2).and_then(|c| c.parse::<u32>().ok()))
            .and_then(|(m, c)| encoded_key(c, m.saturating_sub(1)));
        return key.map_or(Decoded::Skip(used), |key| Decoded::Key(key, used));
    }
    if let Some(direction) = match final_byte {
        b'A' => Some(Direction::Up),
        b'B' => Some(Direction::Down),
        b'C' => Some(Direction::Right),
        b'D' => Some(Direction::Left),
        b'H' => Some(Direction::Home),
        b'F' => Some(Direction::End),
        _ => None,
    } {
        return motion_key(direction, modifier_of(&params))
            .map_or(Decoded::Skip(used), |key| Decoded::Key(key, used));
    }
    let key = match (final_byte, params.as_str()) {
        (b'Z', _) => Key::ShiftTab,
        (b'~', "3") => Key::Delete,
        (b'~', "5") => Key::PageUp,
        (b'~', "6") => Key::PageDown,
        (b'~', "1") | (b'~', "7") => Key::Home,
        (b'~', "4") | (b'~', "8") => Key::End,
        _ => return Decoded::Skip(used),
    };
    Decoded::Key(key, used)
}

fn motion_key(direction: Direction, flags: u8) -> Option<Key> {
    if flags == 2 {
        return match direction {
            Direction::Left => Some(Key::AltLeft),
            Direction::Right => Some(Key::AltRight),
            _ => None,
        };
    }
    // Other Alt/Super combinations are not aliases of the chosen motions.
    if flags & !5 != 0 {
        return None;
    }
    if direction == Direction::Down && flags == 4 {
        return Some(Key::CtrlDown);
    }
    if flags != 0 {
        return Some(Key::Motion {
            direction,
            ctrl: flags & 4 != 0,
            shift: flags & 1 != 0,
        });
    }
    Some(match direction {
        Direction::Left => Key::Left,
        Direction::Right => Key::Right,
        Direction::Up => Key::Up,
        Direction::Down => Key::Down,
        Direction::Home => Key::Home,
        Direction::End => Key::End,
    })
}

fn encoded_key(code: u32, flags: u8) -> Option<Key> {
    if code == 13 {
        return Some(if flags == 0 { Key::Enter } else { Key::NewLine });
    }
    if let Some(direction) = match code {
        57350 => Some(Direction::Left),
        57351 => Some(Direction::Right),
        57352 => Some(Direction::Up),
        57353 => Some(Direction::Down),
        57356 => Some(Direction::Home),
        57357 => Some(Direction::End),
        _ => None,
    } {
        return motion_key(direction, flags);
    }
    let c = char::from_u32(code)?;
    if flags == 4 || flags == 5 && c == '_' {
        return if c == '_' {
            Some(Key::Ctrl('_'))
        } else if c.is_ascii_alphabetic() {
            Some(Key::Ctrl(c.to_ascii_lowercase()))
        } else {
            None
        };
    }
    if flags == 2 && c.is_ascii_alphabetic() {
        return Some(Key::Alt(c.to_ascii_lowercase()));
    }
    if flags > 1 {
        return None;
    }
    Some(match code {
        9 if flags == 1 => Key::ShiftTab,
        9 => Key::Tab,
        27 if flags == 0 => Key::Esc,
        127 | 8 if flags == 0 => Key::Backspace,
        57349 if flags == 0 => Key::Delete,
        57354 if flags == 0 => Key::PageUp,
        57355 if flags == 0 => Key::PageDown,
        _ if code >= 57344 => return None,
        _ if !c.is_control() => Key::Char(c),
        _ => return None,
    })
}

/// The xterm modifier bits of a key's parameters (`1;5` → 4, Ctrl), or 0
/// when there is none.
fn modifier_of(params: &str) -> u8 {
    params
        .split(';')
        .nth(1)
        .and_then(|m| m.parse::<u8>().ok())
        .map_or(0, |m| m.saturating_sub(1))
}

/// One SGR mouse report: `<button;col;row` and whether it was a press.
///
/// Only button one and the wheel are returned. Motion with button one held is
/// Rune's text selection; motion without a held button remains ignored.
fn sgr_mouse(params: &str, press: bool) -> Option<Mouse> {
    let rest = params.strip_prefix('<')?;
    let mut parts = rest.split(';');
    let button: u16 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // Bit 5 (32) marks motion, and motion is never asked for. Bit 6 (64) marks
    // the wheel. The low two bits are the button.
    let kind = match (button & 64 != 0, button & 32 != 0, button & 3) {
        (true, _, 0) => Click::WheelUp,
        (true, _, 1) => Click::WheelDown,
        (true, ..) => return None,
        (false, true, 0) => Click::Drag,
        (false, true, _) => return None,
        (false, false, 0) => {
            if press {
                Click::Press
            } else {
                Click::Release
            }
        }
        // Buttons two and three belong to the terminal: a right-click is how a
        // person reaches their own context menu, and taking it would be taking
        // paste away.
        (false, false, _) => return None,
    };
    Some(Mouse {
        kind,
        row: row.max(1),
        col: col.max(1),
        modifiers: Modifiers::of_button(button),
    })
}

/// The same report in the encoding that predates SGR: each field is a byte with
/// 32 added, which is also why it cannot count past column 223.
fn x10_mouse(button: u8, col: u8, row: u8) -> Option<Mouse> {
    // Every field carries the same offset, the button included — so the flags
    // cannot be read until it is taken back off.
    let button = button.wrapping_sub(32);
    let kind = match (button & 64 != 0, button & 32 != 0, button & 3) {
        (true, _, 0) => Click::WheelUp,
        (true, _, 1) => Click::WheelDown,
        (true, ..) => return None,
        (false, true, 0) => Click::Drag,
        (false, true, _) => return None,
        // This encoding has one release report for every button, so a release
        // cannot be told from the button it belongs to. Press is the one this
        // program acts on, and 3 is the release.
        (false, false, 3) => Click::Release,
        (false, false, 0) => Click::Press,
        (false, false, _) => return None,
    };
    Some(Mouse {
        kind,
        row: (row.saturating_sub(32) as u16).max(1),
        col: (col.saturating_sub(32) as u16).max(1),
        modifiers: Modifiers::of_button(button as u16),
    })
}

fn decode_utf8(bytes: &[u8]) -> Decoded {
    let len = utf8_len(bytes[0]);
    if bytes.len() < len {
        return Decoded::Need;
    }
    match std::str::from_utf8(&bytes[..len]) {
        Ok(s) => match s.chars().next() {
            Some(c) => Decoded::Key(Key::Char(c), len),
            None => Decoded::Skip(len),
        },
        Err(_) => Decoded::Skip(1),
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(bytes: &[u8]) -> Key {
        match decode(bytes) {
            Decoded::Key(k, _) => k,
            other => panic!(
                "expected a key, got {}",
                match other {
                    Decoded::Need => "need more",
                    Decoded::Skip(_) => "skip",
                    Decoded::Key(..) => unreachable!(),
                }
            ),
        }
    }

    /// The painter's questions are answered on the input stream: the answers
    /// are reports, whole or waited for, and never reach the composer.
    #[test]
    fn the_terminals_answers_are_reports_not_keys() {
        assert_eq!(key(b"\x1b[0n"), Key::Terminal(Report::Ready));
        assert_eq!(
            key(b"\x1b[?2026;2$y"),
            Key::Terminal(Report::Synchronized(true))
        );
        assert_eq!(
            key(b"\x1b[?2026;1$y"),
            Key::Terminal(Report::Synchronized(true))
        );
        assert_eq!(
            key(b"\x1b[?2026;4$y"),
            Key::Terminal(Report::Synchronized(false))
        );
        assert_eq!(
            key(b"\x1b[?2026;0$y"),
            Key::Terminal(Report::Synchronized(false))
        );
        assert!(matches!(decode(b"\x1b[?2026;2$"), Decoded::Need));
        assert!(matches!(decode(b"\x1b[?69;2$y"), Decoded::Skip(9)));
        // An answer in the middle of typing takes only its own bytes.
        assert!(matches!(
            decode(b"\x1b[0nab"),
            Decoded::Key(Key::Terminal(Report::Ready), 4)
        ));
    }

    #[test]
    fn editing_modifiers_are_exact_and_never_demoted_to_plain_arrows() {
        for (seq, expected) in [
            (
                &b"\x1b[1;5D"[..],
                Key::Motion {
                    direction: Direction::Left,
                    ctrl: true,
                    shift: false,
                },
            ),
            (
                &b"\x1b[1;5C"[..],
                Key::Motion {
                    direction: Direction::Right,
                    ctrl: true,
                    shift: false,
                },
            ),
            (
                &b"\x1b[1;2D"[..],
                Key::Motion {
                    direction: Direction::Left,
                    ctrl: false,
                    shift: true,
                },
            ),
            (
                &b"\x1b[1;6C"[..],
                Key::Motion {
                    direction: Direction::Right,
                    ctrl: true,
                    shift: true,
                },
            ),
            (
                &b"\x1b[1;2A"[..],
                Key::Motion {
                    direction: Direction::Up,
                    ctrl: false,
                    shift: true,
                },
            ),
            (&b"\x1b[1;3D"[..], Key::AltLeft),
            (&b"\x1b[1;3C"[..], Key::AltRight),
            (&b"\x1b[57350;3u"[..], Key::AltLeft),
            (&b"\x1b[57351;3u"[..], Key::AltRight),
            (&b"\x1f"[..], Key::Ctrl('_')),
            (&b"\x1b[95;5u"[..], Key::Ctrl('_')),
            (&b"\x1b[107;5u"[..], Key::Ctrl('k')),
            (&b"\x1b[27;2;13~"[..], Key::NewLine),
            (
                &b"\x1b[57350;5u"[..],
                Key::Motion {
                    direction: Direction::Left,
                    ctrl: true,
                    shift: false,
                },
            ),
        ] {
            assert_eq!(key(seq), expected);
        }
        for seq in [
            &b"\x1b[1;4D"[..],
            &b"\x1b[3;5~"[..],
            &b"\x1b\x7f"[..],
            &b"\x1b[107;5:3u"[..],
            &b"\x18\x15"[..],
        ] {
            assert!(
                matches!(decode(seq),Decoded::Skip(n) if n==seq.len()),
                "{seq:?}"
            );
        }
        assert!(matches!(decode(b"\x18"), Decoded::Need));
    }

    #[test]
    fn modern_keyboard_keeps_shift_tab_and_does_not_alias_unbound_modified_keys() {
        assert_eq!(key(b"\x1b[9;2u"), Key::ShiftTab);
        assert_eq!(key(b"\x1b[27;2;9~"), Key::ShiftTab);
        for seq in [
            &b"\x1b[107;6u"[..],
            &b"\x1b[127;2u"[..],
            &b"\x1b[57349;2u"[..],
            &b"\x1b[57354;2u"[..],
        ] {
            assert!(
                matches!(decode(seq),Decoded::Skip(n) if n==seq.len()),
                "{seq:?}"
            );
        }
    }

    #[test]
    fn terminal_string_replies_are_whole_reports_never_composer_characters() {
        for seq in [
            &b"\x1b_Gi=31;OK\x1b\\"[..],
            &b"\x1bP0;1;0qDATA\x1b\\"[..],
            &b"\x1b^private\x07"[..],
        ] {
            for split in 2..seq.len() - 1 {
                assert!(matches!(decode(&seq[..split]), Decoded::Need));
            }
            assert!(matches!(decode(seq),Decoded::Skip(n) if n==seq.len()));
            let mut with_text = seq.to_vec();
            with_text.extend_from_slice(b"X");
            assert!(matches!(decode(&with_text),Decoded::Skip(n) if n==seq.len()));
        }
        let overlong = [&b"\x1b_"[..], &vec![b'X'; 4096]].concat();
        assert!(matches!(decode(&overlong),Decoded::Skip(n) if n==overlong.len()));
    }

    #[test]
    fn control_letters_decode_to_the_letter_they_are() {
        assert_eq!(key(&[0x0f]), Key::Ctrl('o'));
        assert_eq!(key(&[0x12]), Key::Ctrl('r'));
        assert_eq!(key(&[0x03]), Key::Ctrl('c'));
        // Ctrl-S reaches us at all only because flow control is off.
        assert_eq!(key(&[0x13]), Key::Ctrl('s'));
    }

    #[test]
    fn enter_tab_and_backspace_are_not_control_letters() {
        assert_eq!(key(b"\r"), Key::Enter);
        assert_eq!(key(b"\n"), Key::NewLine);
        assert_eq!(key(b"\r\n"), Key::NewLine);
        assert_eq!(key(b"\t"), Key::Tab);
        assert_eq!(key(&[0x7f]), Key::Backspace);
    }

    #[test]
    fn shift_tab_arrives_as_its_own_sequence() {
        assert_eq!(key(b"\x1b[Z"), Key::ShiftTab);
    }

    #[test]
    fn arrows_decode_in_both_the_forms_terminals_send() {
        assert_eq!(key(b"\x1b[A"), Key::Up);
        assert_eq!(key(b"\x1bOB"), Key::Down);
        assert_eq!(key(b"\x1b[5~"), Key::PageUp);
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        assert_eq!(key(b"\x1bx"), Key::Alt('x'));
    }

    #[test]
    fn a_paste_is_one_key_however_many_newlines_it_holds() {
        let seq = b"\x1b[200~line one\nline two\n\x1b[201~";
        match decode(seq) {
            Decoded::Key(Key::Paste(text), used) => {
                assert_eq!(text, "line one\nline two\n");
                assert_eq!(used, seq.len());
            }
            _ => panic!("a paste must not decode as keystrokes"),
        }
    }

    #[test]
    fn an_unterminated_paste_waits_rather_than_sending_half_of_it() {
        assert!(matches!(decode(b"\x1b[200~half"), Decoded::Need));
    }

    #[test]
    fn utf8_survives_arriving_one_byte_at_a_time() {
        let bytes = "é".as_bytes();
        assert!(matches!(decode(&bytes[..1]), Decoded::Need));
        assert_eq!(key(bytes), Key::Char('é'));
    }

    #[test]
    fn a_click_carries_the_cell_it_landed_on() {
        // `<0;12;7M` is button one down at column 12, row 7 — and the report
        // says column first, which is the transposition this decoder exists to
        // get right once rather than at every call site.
        assert_eq!(
            key(b"\x1b[<0;12;7M"),
            Key::Mouse(Mouse {
                kind: Click::Press,
                row: 7,
                col: 12,
                modifiers: Modifiers::NONE,
            })
        );
        assert_eq!(
            key(b"\x1b[<0;12;7m"),
            Key::Mouse(Mouse {
                kind: Click::Release,
                row: 7,
                col: 12,
                modifiers: Modifiers::NONE,
            })
        );
    }

    /// Ctrl+↓ is its own key in every spelling a terminal uses for it — with
    /// Alt or Shift held too — and every other modified ↓ stays a ↓.
    #[test]
    fn ctrl_down_is_read_as_its_own_key() {
        assert_eq!(key(b"\x1b[1;5B"), Key::CtrlDown);
        assert!(
            matches!(decode(b"\x1b[1;7B"), Decoded::Skip(_)),
            "Ctrl+Alt is not a duplicate jump chord"
        );
        assert_eq!(
            key(b"\x1b[1;6B"),
            Key::Motion {
                direction: Direction::Down,
                ctrl: true,
                shift: true
            },
            "Ctrl+Shift is not jump"
        );
        assert_eq!(
            key(b"\x1bOb"),
            Key::CtrlDown,
            "rxvt encoding of the same physical key"
        );
        assert!(
            matches!(decode(b"\x1b[1;3B"), Decoded::Skip(_)),
            "Alt is not a plain arrow"
        );
        assert_eq!(key(b"\x1b[B"), Key::Down);
        assert_eq!(key(b"\x1bOB"), Key::Down);
    }

    /// The wheel reports which of Ctrl and Alt were held, in both encodings.
    #[test]
    fn a_wheel_report_says_whether_ctrl_or_alt_was_held() {
        let held = |bytes: &[u8]| match key(bytes) {
            Key::Mouse(m) => (m.kind, m.modifiers),
            other => panic!("not a mouse report: {other:?}"),
        };
        let ctrl = Modifiers {
            ctrl: true,
            alt: false,
        };
        let alt = Modifiers {
            ctrl: false,
            alt: true,
        };
        assert_eq!(held(b"\x1b[<64;10;5M"), (Click::WheelUp, Modifiers::NONE));
        assert_eq!(held(b"\x1b[<80;10;5M"), (Click::WheelUp, ctrl));
        assert_eq!(held(b"\x1b[<73;10;5M"), (Click::WheelDown, alt));
        assert_eq!(
            held(&[0x1b, b'[', b'M', 32 + 64 + 16, 32 + 10, 32 + 5]),
            (Click::WheelUp, ctrl),
            "the X10 encoding carries the same bits"
        );
    }

    #[test]
    fn the_wheel_is_a_report_rather_than_an_arrow_key() {
        assert!(matches!(
            key(b"\x1b[<64;10;5M"),
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                ..
            })
        ));
        assert!(matches!(
            key(b"\x1b[<65;10;5M"),
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                ..
            })
        ));
    }

    /// Buttons two and three stay the terminal's: a right-click is how a person
    /// reaches their own paste menu, and a program that swallows it has taken
    /// something away without offering anything back.
    #[test]
    fn the_other_buttons_are_left_alone_and_left_drag_is_selection() {
        assert!(matches!(decode(b"\x1b[<2;10;5M"), Decoded::Skip(_)));
        assert_eq!(
            key(b"\x1b[<32;10;5M"),
            Key::Mouse(Mouse {
                kind: Click::Drag,
                row: 5,
                col: 10,
                modifiers: Modifiers::NONE,
            })
        );
    }

    /// A terminal that does not understand `?1006h` answers in the encoding that
    /// predates it, where the coordinates are raw bytes rather than digits.
    #[test]
    fn the_old_mouse_encoding_is_read_rather_than_typed() {
        assert!(matches!(decode(b"\x1b[M"), Decoded::Need));
        assert_eq!(
            key(&[0x1b, b'[', b'M', 32, 32 + 12, 32 + 7]),
            Key::Mouse(Mouse {
                kind: Click::Press,
                row: 7,
                col: 12,
                modifiers: Modifiers::NONE,
            })
        );
    }

    /// Enter sends. Enter with any modifier breaks the line — which is the
    /// gesture every other composer on the machine uses.
    #[test]
    fn enter_with_a_modifier_is_a_line_break_in_both_forms() {
        assert_eq!(key(b"\x1b\r"), Key::NewLine);
        assert_eq!(key(b"\x1b[13;3u"), Key::NewLine);
        assert_eq!(key(b"\x1b[13;2u"), Key::NewLine);
        assert_eq!(key(b"\r"), Key::Enter);
    }

    #[test]
    fn a_partial_escape_sequence_asks_for_more_instead_of_guessing() {
        assert!(matches!(decode(b"\x1b"), Decoded::Need));
        assert!(matches!(decode(b"\x1b["), Decoded::Need));
        assert!(matches!(decode(b"\x1b[1"), Decoded::Need));
    }

    /// The owner's window opened from `rune -session` never took 0.3.8 over:
    /// the keys the picker was handed counted toward the window's total, so
    /// "every key read was applied" never held. Each taker counts from zero.
    #[test]
    fn a_new_taker_of_the_reader_counts_its_own_keys_only() {
        let (picker, picked) = std::sync::mpsc::channel();
        take_reader(picker);
        send(Key::Char('a'));
        send(Key::Char('b'));
        send(Key::Terminal(Report::Ready));
        assert_eq!(sent(), 2, "a terminal's report is not typed");
        assert_eq!(picked.recv().unwrap(), Key::Char('a'));
        // The picker chose a session: what it did not take, and what is
        // typed before the window takes the reader, waits for the window.
        release(&picked);
        drop(picked);
        send(Key::Char('c'));
        let (window, typed) = std::sync::mpsc::channel();
        take_reader(window);
        assert_eq!(sent(), 2, "the picker's keys are not the window's: b and c");
        let got: Vec<Key> = typed.try_iter().collect();
        assert_eq!(
            got,
            vec![Key::Char('b'), Key::Terminal(Report::Ready), Key::Char('c')]
        );
        // A taker gone without letting go: its keys are held, not lost.
        drop(typed);
        send(Key::Char('d'));
        let (next, rx) = std::sync::mpsc::channel();
        take_reader(next);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![Key::Char('d')]);
        assert_eq!(sent(), 1);
    }

    /// A background-colour answer that arrived after the start-up probe gave
    /// up is the terminal talking, not typing: nothing of it reaches a field.
    #[test]
    fn a_late_colour_reply_is_never_typed() {
        for reply in [
            &b"\x1b]11;rgb:1010/1010/1010\x1b\\"[..],
            &b"\x1b]11;rgb:1010/1010/1010\x07"[..],
        ] {
            assert!(matches!(decode(reply), Decoded::Skip(n) if n == reply.len()));
            let mut with_key = reply.to_vec();
            with_key.push(b'a');
            assert!(matches!(decode(&with_key), Decoded::Skip(n) if n == reply.len()));
            assert!(matches!(
                decode(&with_key[reply.len()..]),
                Decoded::Key(Key::Char('a'), 1)
            ));
        }
        // Still arriving: wait for the rest rather than type its start.
        assert!(matches!(decode(b"\x1b]11;rgb:10"), Decoded::Need));
        assert!(matches!(decode(b"\x1b]1"), Decoded::Need));
        // Alt+] then typing is not swallowed.
        assert!(matches!(decode(b"\x1b]x"), Decoded::Skip(2)));
    }
}
