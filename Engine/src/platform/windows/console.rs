//! The console this process was started in: its modes, its size, and word
//! that its window changed. Mechanism only — when a window takes the console
//! and gives it back is the CLI's (`ui/term.rs`).
//!
//! Raw on Windows is the console's virtual-terminal modes: keys arrive as
//! the same sequences a Unix terminal sends (`ENABLE_VIRTUAL_TERMINAL_INPUT`),
//! output is read as VT (`ENABLE_VIRTUAL_TERMINAL_PROCESSING`), nothing is
//! echoed or held to a line, Ctrl+C is a key, and Quick Edit is off so the
//! mouse reaches the program. Windows sends no signal when the window is
//! resized; its size is compared instead.
//!
//! The mouse is the program's or the console's ([`console_mouse`]). A
//! terminal that reads VT — Windows Terminal, a newer console — is told by
//! the sequences the program writes; the console window itself is told by
//! its input mode: the program's, its records arrive (`ENABLE_MOUSE_INPUT`,
//! turned into the sequences a VT terminal sends by `stdio.rs`); the
//! console's, Quick Edit selects as the person had it.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    FlushConsoleInputBuffer, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle,
    SetConsoleMode, CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN,
    ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

use super::wide::check;

fn input() -> HANDLE {
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
}

pub(super) fn output() -> HANDLE {
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
}

/// Whether the program has the mouse ([`console_mouse`]): yes until told
/// otherwise, as Rune's own setting starts.
pub(super) static MOUSE: AtomicBool = AtomicBool::new(true);

/// Whether Quick Edit was on in the console Rune was started in: what the
/// console gets back with the mouse. Windows turns it on by default.
static QUICK_EDIT_AT_START: OnceLock<bool> = OnceLock::new();

/// The input mode's mouse bits for whoever has the mouse.
fn mouse_bits(program: bool) -> CONSOLE_MODE {
    if program {
        ENABLE_MOUSE_INPUT
    } else if *QUICK_EDIT_AT_START.get().unwrap_or(&true) {
        ENABLE_QUICK_EDIT_MODE
    } else {
        0
    }
}

/// Give the mouse to the program (`true`) or to the console (`false`) —
/// Rune's mouse setting, said to the console window by its input mode while
/// it is raw; a VT terminal is told by what the program writes.
pub fn console_mouse(on: bool) {
    MOUSE.store(on, Ordering::Relaxed);
    let mut mode = 0;
    if unsafe { GetConsoleMode(input(), &mut mode) } == 0
        || mode & ENABLE_VIRTUAL_TERMINAL_INPUT == 0
    {
        return;
    }
    let mode = (mode & !(ENABLE_QUICK_EDIT_MODE | ENABLE_MOUSE_INPUT))
        | ENABLE_EXTENDED_FLAGS
        | mouse_bits(on);
    unsafe { SetConsoleMode(input(), mode) };
}

/// The console's input and output modes, as read from it or as they are to
/// be set.
#[derive(Clone, Copy)]
pub struct ConsoleMode {
    input: CONSOLE_MODE,
    output: CONSOLE_MODE,
}

/// What happens to keys typed and not yet read when a mode is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// They stay, for whoever reads next.
    Keep,
    /// They are thrown away: the program is leaving, and they were typed
    /// to it, not to the shell it hands the console back to.
    Discard,
}

impl ConsoleMode {
    /// The modes this process's console has now.
    pub fn current() -> io::Result<ConsoleMode> {
        let mut input_mode = 0;
        let mut output_mode = 0;
        check(unsafe { GetConsoleMode(input(), &mut input_mode) })?;
        check(unsafe { GetConsoleMode(output(), &mut output_mode) })?;
        QUICK_EDIT_AT_START.get_or_init(|| input_mode & ENABLE_QUICK_EDIT_MODE != 0);
        Ok(ConsoleMode {
            input: input_mode,
            output: output_mode,
        })
    }

    /// These modes made raw: every key as it is pressed, as VT sequences,
    /// nothing echoed, Ctrl+C a key, the mouse whoever has it
    /// ([`console_mouse`]); output read as VT, placed by the program alone.
    pub fn raw(&self) -> ConsoleMode {
        ConsoleMode {
            input: (self.input
                & !(ENABLE_LINE_INPUT
                    | ENABLE_ECHO_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_QUICK_EDIT_MODE
                    | ENABLE_MOUSE_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
                | ENABLE_WINDOW_INPUT
                | ENABLE_EXTENDED_FLAGS
                | mouse_bits(MOUSE.load(Ordering::Relaxed)),
            output: self.output
                | ENABLE_PROCESSED_OUTPUT
                | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                | DISABLE_NEWLINE_AUTO_RETURN,
        }
    }

    /// Set these modes; `typed` says what becomes of keys typed and not yet
    /// read.
    pub fn apply(&self, typed: Typed) -> io::Result<()> {
        if typed == Typed::Discard {
            unsafe { FlushConsoleInputBuffer(input()) };
        }
        check(unsafe { SetConsoleMode(input(), self.input) })?;
        check(unsafe { SetConsoleMode(output(), self.output) })
    }
}

/// Rows and columns of the console's window; `None` when there is none.
pub fn console_size() -> Option<(u16, u16)> {
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(output(), &mut info) } == 0 {
        return None;
    }
    let window = info.srWindow;
    let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
    let cols = i32::from(window.Right) - i32::from(window.Left) + 1;
    (rows > 0 && cols > 0).then_some((rows as u16, cols as u16))
}

/// The size last told ([`take_console_resized`]), `None` before watching.
static SEEN: Mutex<Option<(u16, u16)>> = Mutex::new(None);

/// Hear about the console's window changing size from now on.
pub fn watch_console_resize() {
    *crate::panic::lock(&SEEN) = Some(console_size().unwrap_or((0, 0)));
}

/// Whether the window changed size since [`take_console_resized`] last said
/// so — only looking.
pub fn console_resized() -> bool {
    match *crate::panic::lock(&SEEN) {
        Some(seen) => console_size().is_some_and(|now| now != seen),
        None => false,
    }
}

/// Whether the window changed size since this was last asked, forgetting it.
pub fn take_console_resized() -> bool {
    let mut seen = crate::panic::lock(&SEEN);
    let Some(before) = *seen else {
        return false;
    };
    match console_size() {
        Some(now) if now != before => {
            *seen = Some(now);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw keeps nothing a key could be lost to: no echo, no line, Ctrl+C a
    /// key, keys and output as VT.
    #[test]
    fn raw_takes_every_key_as_it_comes() {
        let cooked = ConsoleMode {
            input: ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE,
            output: ENABLE_PROCESSED_OUTPUT,
        };
        let raw = cooked.raw();
        assert_eq!(
            raw.input
                & (ENABLE_LINE_INPUT
                    | ENABLE_ECHO_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_QUICK_EDIT_MODE),
            0
        );
        assert_ne!(raw.input & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(raw.output & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
    }

    /// The program's mouse: its records come, Quick Edit is off. The
    /// console's: no records, Quick Edit as the person had it.
    #[test]
    fn the_mouse_is_the_programs_or_the_consoles() {
        assert_eq!(mouse_bits(true), ENABLE_MOUSE_INPUT);
        assert_eq!(mouse_bits(true) & ENABLE_QUICK_EDIT_MODE, 0);
        assert_eq!(mouse_bits(false) & ENABLE_MOUSE_INPUT, 0);
    }
}
