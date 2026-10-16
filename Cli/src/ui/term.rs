//! The terminal itself: raw mode, the alternate screen, and putting both back.
//!
//! When the terminal is taken and given back is decided here; how is the
//! platform's (`platform::ConsoleMode`, `console_size`, the resize watch) —
//! four `ioctl`s and a `termios` struct on Linux, hand-written for the same
//! reason the rest of this workspace is: a crate for it would bring a
//! dependency tree to save fifty lines.
//!
//! # Why the alternate screen
//!
//! A split view — transcript on the left, the selected step's detail on the
//! right — cannot be drawn inline: the right column would have to be re-drawn
//! next to scrollback the program no longer owns. So Rune takes the whole screen
//! and keeps its own scrollback, and gives the terminal back exactly as it found
//! it on the way out. The cost is honest and known: the conversation is not left
//! in the terminal's buffer afterwards, so the last lines are re-printed on exit.
//!
//! # Restoration is a `Drop`, not a line at the end
//!
//! A terminal left in raw mode with the cursor hidden is a broken shell, and the
//! ways out of a program are many: a return, a `?`, a panic. `Drop` covers all
//! of them. The panic hook covers the one case `Drop` cannot — a panic while the
//! screen is mid-frame, where the message would otherwise be painted over.

use std::io::Write;
use std::sync::atomic::{AtomicU16, Ordering};

use runeengine::platform::{self, ConsoleMode, Typed};

static COLS: AtomicU16 = AtomicU16::new(80);
static ROWS: AtomicU16 = AtomicU16::new(24);

/// How long the terminal is given to answer the background-colour query.
/// Long enough for a slow ssh hop, short enough that a terminal which will never
/// answer does not hold the first frame back noticeably.
/// Where a terminal that answers nothing and has no `COLORFGBG` is assumed to
/// be. Dark, because nearly all of them are.
const FALLBACK_BG: super::ansi::Rgb = super::ansi::Rgb(17, 19, 26);

pub struct Term {
    original: ConsoleMode,
}

/// The line discipline the user's shell had, kept where a panic hook can
/// reach it: the hook runs before the unwind that would drop the [`Term`],
/// and a message written to a terminal still in raw mode (`OPOST` off)
/// staircases across the screen.
static SAVED: std::sync::Mutex<Option<ConsoleMode>> = std::sync::Mutex::new(None);

fn saved() -> std::sync::MutexGuard<'static, Option<ConsoleMode>> {
    SAVED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Term {
    /// Take the terminal: raw mode, alternate screen, paste and — unless the
    /// user turned it off — mouse reporting.
    pub fn enter(mouse: bool) -> std::io::Result<Term> {
        let original = ConsoleMode::current()?;
        // Every key as it is pressed, Ctrl-C and Ctrl-S included: keystrokes
        // this program answers. `Typed::Keep` (TCSADRAIN), never `Discard`:
        // what was typed before this point — ahead of a start, or across the
        // `exec` of a handover — is the person's, and the reader takes it like
        // any other key.
        original.raw().apply(Typed::Keep)?;

        platform::watch_console_resize();
        refresh_size();

        // Before a single byte is drawn, and while this is the only thing
        // reading stdin: the answer decides every colour on the screen, and a
        // palette that arrives mid-session would repaint half a frame. Anything
        // the user typed while the terminal was thinking comes back with it and
        // is handed to the key decoder rather than eaten.
        if !super::theme::is_installed() {
            let (background, typed) = super::theme::probe(
                super::runtime_configuration().number("ui.color_probe_timeout") as i32,
            );
            super::theme::install(
                background
                    .or_else(super::theme::from_env)
                    .unwrap_or(FALLBACK_BG),
            );
            super::input::feed(typed);
        }

        let mut out = std::io::stdout();
        // Alternate screen, cursor hidden, bracketed paste, auto-wrap off.
        out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[?7l")?;
        out.flush()?;
        set_mouse(mouse);
        *saved() = Some(original);
        Ok(Term { original })
    }

    /// Rows and columns, refreshed if the window changed since the last check.
    pub fn size(&self) -> (u16, u16) {
        if platform::take_console_resized() {
            refresh_size();
        }
        (ROWS.load(Ordering::Relaxed), COLS.load(Ordering::Relaxed))
    }

    /// Whether the window changed, without consuming the flag's effect on size.
    pub fn resized(&self) -> bool {
        platform::console_resized()
    }
}

/// Currently detected terminal columns.
pub fn cols() -> usize {
    COLS.load(Ordering::Relaxed) as usize
}

/// Ask for the mouse, or hand it back.
///
/// # The two states, and what each one costs
///
/// **On** is `?1002h` (press, release, and motion while a button is held) and
/// `?1006h` (coordinates beyond column 223). Motion without a pressed button is
/// not requested. Rune owns drag selection in this state, so a drag can select
/// while an ordinary click still opens a step.
///
/// **Off** is `?1007h`, alternate scroll, which asks the terminal to send arrow
/// keys when the wheel turns. The wheel still scrolls, nothing else is
/// reported, and selection works with a plain drag.
///
/// Called at start-up and again whenever the setting is changed, so the choice
/// takes effect in the session that made it rather than the next one.
pub fn set_mouse(on: bool) {
    // The console window of Windows is told by its own mode, not by these.
    runeengine::platform::console_mouse(on);
    let mut out = std::io::stdout();
    let _ = out.write_all(if on {
        b"\x1b[?1007l\x1b[?1000l\x1b[?1002h\x1b[?1006h" as &[u8]
    } else {
        b"\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1007h"
    });
    let _ = out.flush();
}

/// Everything this program asked the terminal for, switched back off.
///
/// One constant, used by the ordinary exit and by the panic hook, because a
/// crash that leaves the mouse reporting turns the user's shell into a stream of
/// escape codes — the worst possible moment to have got this list wrong twice.
const RELEASE: &[u8] =
    b"\x1b[0 q\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1007l\x1b[?2004l\x1b[?2026l\x1b[?25h\x1b[?7h\x1b[?1049l";

/// Put selected text on the clipboard: the system's own where a terminal
/// program may set it itself (Windows, macOS — the console window and
/// Terminal.app ignore OSC 52), and the terminal's through OSC 52 too, which
/// reaches the clipboard of the machine the person sits at over SSH.
pub fn copy(text: &str) {
    if text.is_empty() {
        return;
    }
    #[cfg(test)]
    return;
    #[cfg(not(test))]
    runeengine::platform::set_clipboard(text);
    #[cfg(not(test))]
    let encoded = runeengine::media::base64(text.as_bytes());
    #[cfg(not(test))]
    let mut out = std::io::stdout();
    #[cfg(not(test))]
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    #[cfg(not(test))]
    let _ = out.flush();
}

impl Term {
    /// Give the terminal to the process this one is about to `exec` into: the
    /// line discipline goes back to how the user's shell left it (the next
    /// process records it as its own original), but the alternate screen stays
    /// up, so the shell's scrollback never flashes between the two frames.
    #[cfg(not(test))]
    pub fn hand_over(self) {
        *saved() = None;
        // Keys typed and not yet read stay in the terminal for the next build
        // (TCSADRAIN keeps them; TCSAFLUSH threw them away).
        let _ = self.original.apply(Typed::Keep);
        std::mem::forget(self);
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        *saved() = None;
        let mut out = std::io::stdout();
        let _ = out.write_all(RELEASE);
        let _ = out.flush();
        let _ = self.original.apply(Typed::Discard);
    }
}

fn refresh_size() {
    if let Some((rows, cols)) = platform::console_size() {
        COLS.store(cols, Ordering::Relaxed);
        ROWS.store(rows.max(1), Ordering::Relaxed);
    }
}

/// Whether a panic should give the terminal back: only one that is going to end
/// the process, which is an uncaught panic on the thread that owns the screen.
/// A panic the engine catches costs one failed tool call and the window carries
/// on — tearing the screen down under it would leave a running program on a
/// dead terminal; and a panic on a helper thread leaves the screen standing.
fn ends_the_process(contained: bool, on_main_thread: bool) -> bool {
    on_main_thread && !contained
}

/// Put the terminal back before a panic message is printed.
///
/// Without this a crash paints its backtrace onto the alternate screen, which is
/// then torn down — so the user sees a shell that has gone strange and no reason
/// why. And with the line discipline still raw, the message itself would
/// staircase. Installed once, at start.
pub fn guard_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let main = std::thread::current().name() == Some("main");
        if ends_the_process(runeengine::panic::contained(), main) {
            let mut out = std::io::stdout();
            let _ = out.write_all(RELEASE);
            let _ = out.flush();
            if let Some(original) = saved().take() {
                let _ = original.apply(Typed::Discard);
            }
        }
        previous(info);
    }));
}

/// Take the terminal, or say plainly why it could not be taken.
pub fn enter_or_report(mouse: bool) -> Result<Term, String> {
    Term::enter(mouse).map_err(|e| format!("could not take the terminal: {e}"))
}

/// Whether stdin and stdout are both a terminal. A pipe gets the protocol host,
/// never a screen full of escape codes.
pub fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The person's clock now, as seconds since 1970.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Current local time formatted as "HH:MM".
pub fn now_hm() -> String {
    platform::local_time(now_secs())
        .map(|t| format!("{:02}:{:02}", t.hour, t.minute))
        .unwrap_or_default()
}

/// Convert epoch milliseconds to local "HH:MM:SS".
pub fn epoch_hms(epoch_ms: i64) -> String {
    platform::local_time(epoch_ms / 1000)
        .map(|t| format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second))
        .unwrap_or_default()
}

/// Convert epoch milliseconds to local "HH:MM".
pub fn epoch_hm(epoch_ms: i64) -> String {
    platform::local_time(epoch_ms / 1000)
        .map(|t| format!("{:02}:{:02}", t.hour, t.minute))
        .unwrap_or_default()
}

/// Convert epoch milliseconds to local "YYYY-MM-DD HH:MM" (year, month, day, hour, minute).
pub fn epoch_ymd_hm(epoch_ms: i64) -> String {
    platform::local_time(epoch_ms / 1000)
        .map(|t| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                t.year, t.month, t.day, t.hour, t.minute
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_panic_that_ends_the_process_gives_the_terminal_back() {
        assert!(ends_the_process(false, true));
        assert!(
            !ends_the_process(true, true),
            "a caught panic: the window goes on"
        );
        assert!(
            !ends_the_process(false, false),
            "a helper thread: the screen stands"
        );
        assert!(!ends_the_process(true, false));
    }
}
