//! The terminal this process was started in: its line discipline, its size,
//! and word that its window changed. Mechanism only — when a window takes the
//! terminal and gives it back is the CLI's (`ui/term.rs`).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// The terminal's line discipline, as read from it or as it is to be set.
#[derive(Clone, Copy)]
pub struct ConsoleMode(libc::termios);

/// What happens to keys typed and not yet read when a mode is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typed {
    /// They stay, for whoever reads next — across a handover's `exec` too.
    Keep,
    /// They are thrown away: the program is leaving, and they were typed
    /// to it, not to the shell it hands the terminal back to.
    Discard,
}

impl ConsoleMode {
    /// The mode stdin's terminal has now.
    pub fn current() -> io::Result<ConsoleMode> {
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut mode) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ConsoleMode(mode))
    }

    /// This mode made raw: every key as it is pressed, nothing echoed,
    /// nothing a signal, no output processing.
    pub fn raw(&self) -> ConsoleMode {
        let mut raw = self.0;
        // ICRNL off so Enter arrives as \r and can be told from Ctrl-J.
        // IXON off is what makes Ctrl-S available at all: with flow control on,
        // the terminal swallows it and freezes the screen instead.
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        // OPOST off: every character is placed by the program.
        raw.c_oflag &= !libc::OPOST;
        // No echo, no line buffering, no signal keys — Ctrl-C is a keystroke the
        // program answers, not a signal that kills a turn mid-write.
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_cflag |= libc::CS8;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        ConsoleMode(raw)
    }

    /// Set this mode on stdin's terminal, once what was written has been
    /// sent; `typed` says what becomes of keys typed and not yet read.
    pub fn apply(&self, typed: Typed) -> io::Result<()> {
        let when = match typed {
            Typed::Keep => libc::TCSADRAIN,
            Typed::Discard => libc::TCSAFLUSH,
        };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, when, &self.0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Rows and columns of stdin's terminal; `None` when it does not say.
pub fn console_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0)
        .then_some((ws.ws_row, ws.ws_col))
}

/// Set by the `SIGWINCH` handler. Nothing else may happen in a signal handler —
/// this is one atomic store, which is async-signal-safe; anything more is not.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

/// Hear about the terminal's window changing size from now on
/// ([`console_resized`], [`take_console_resized`]).
pub fn watch_console_resize() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_winch as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

/// Whether the window changed size since [`take_console_resized`] last said
/// so — only looking, the word stays.
pub fn console_resized() -> bool {
    RESIZED.load(Ordering::Relaxed)
}

/// Whether the window changed size since this was last asked, forgetting it.
pub fn take_console_resized() -> bool {
    RESIZED.swap(false, Ordering::Relaxed)
}

/// Give the mouse to the program or to the terminal. A Unix terminal is told
/// by the sequences the program writes (`?1002h`, `?1007h`); there is no
/// mode of the terminal's own to set.
pub fn console_mouse(_on: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw keeps nothing a key could be lost to: no echo, no line, no signal.
    #[test]
    fn raw_takes_every_key_as_it_comes() {
        let cooked: libc::termios = unsafe { std::mem::zeroed() };
        let mut cooked = cooked;
        cooked.c_lflag = libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
        cooked.c_iflag = libc::IXON | libc::ICRNL;
        cooked.c_oflag = libc::OPOST;
        let raw = ConsoleMode(cooked).raw().0;
        assert_eq!(
            raw.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN),
            0
        );
        assert_eq!(raw.c_iflag & (libc::IXON | libc::ICRNL), 0);
        assert_eq!(raw.c_oflag & libc::OPOST, 0);
        assert_eq!((raw.c_cc[libc::VMIN], raw.c_cc[libc::VTIME]), (1, 0));
    }

    #[test]
    fn a_resize_is_heard_once_taken() {
        watch_console_resize();
        unsafe { libc::raise(libc::SIGWINCH) };
        assert!(console_resized(), "looking does not forget it");
        assert!(take_console_resized());
        assert!(!take_console_resized(), "taken once");
    }
}
