//! Terminals of commands on Windows: ConPTY (Windows 10 1809 and later).
//!
//! A pseudoconsole rather than pipes for the same reason as a Unix pty:
//! colour, progress bars and anything that asks whether it runs in a console
//! behave as they do for the person. The pseudoconsole reads keys from one
//! pipe and writes what the console shows, as UTF-8 text with VT sequences,
//! to another; a command started on it (`spawn_on_terminal`) has it as its
//! console.
//!
//! Two things differ from a Unix pty, and the callers already allow for
//! both: the output pipe stays open after the command ends (the console
//! lives until it is closed), so an end is seen by the process ending; and
//! nothing tells whether the command waits to read (`INPUT_WAIT_DETECTION`).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, Condvar, Mutex};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;

use super::files::Handed;
use super::wide::check;
use crate::platform::InputWait;

/// A pseudoconsole, closed when the last of its terminal and its slave lets
/// go of it.
pub(super) struct PseudoConsole {
    handle: HPCON,
    size: Mutex<(u16, u16)>,
}

// SAFETY: an HPCON is a handle any thread may use.
unsafe impl Send for PseudoConsole {}
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    pub(super) fn handle(&self) -> HPCON {
        self.handle
    }
}

impl Drop for PseudoConsole {
    /// Closing ends the console, and every program still attached to it ends
    /// as a Unix command does when its terminal hangs up. The output pipe is
    /// drained meanwhile by its reader, which a Windows 10 console waits for
    /// before it closes.
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.handle) };
    }
}

/// What the console has shown and Rune has not read yet: filled by the
/// reader thread, read by [`Pty::read_timeout`].
struct Shown {
    state: Mutex<ShownState>,
    ready: Condvar,
}

struct ShownState {
    bytes: VecDeque<u8>,
    ended: bool,
    /// Whether a [`Pty`] still reads; once not, what arrives is let go.
    wanted: bool,
}

/// How much shown output waits for a reader before the reader thread waits
/// too — as a Unix pty's buffer holds a writer back.
const SHOWN_LIMIT: usize = 1 << 20;

/// The terminal side Rune keeps: keys go in, what the console shows comes out.
pub struct Pty {
    console: Arc<PseudoConsole>,
    input: Mutex<File>,
    shown: Arc<Shown>,
}

/// The console a command starts on ([`super::process::spawn_on_terminal`]),
/// released once it started.
pub struct PtySlave(pub(super) Arc<PseudoConsole>);

/// An anonymous pipe: (read end, write end), neither inherited.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    check(unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) })?;
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

/// A Windows result code as an I/O error.
fn hresult(code: i32) -> io::Result<()> {
    if code >= 0 {
        return Ok(());
    }
    // HRESULT_FROM_WIN32 carries the Win32 error in its low word.
    let facility = (code as u32 >> 16) & 0x1fff;
    Err(if facility == 7 {
        io::Error::from_raw_os_error((code as u32 & 0xffff) as i32)
    } else {
        io::Error::other(format!("Windows answered {:#010x}", code as u32))
    })
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD {
        X: cols.clamp(1, i16::MAX as u16) as i16,
        Y: rows.clamp(1, i16::MAX as u16) as i16,
    }
}

/// Open a pseudoconsole sized `rows` × `cols`.
///
/// Returns the terminal side (kept by the engine) and the console a command
/// starts on (dropped by the engine once the command started). Nothing of it
/// is ever inherited by another program: the command reaches the console
/// through the attribute it is started with, never through a handle.
pub fn open_pty(rows: u16, cols: u16) -> io::Result<(Pty, PtySlave)> {
    let (keys_read, keys_write) = pipe()?;
    let (shown_read, shown_write) = pipe()?;
    let mut handle: HPCON = 0;
    hresult(unsafe {
        CreatePseudoConsole(
            coord(rows, cols),
            keys_read.as_raw_handle(),
            shown_write.as_raw_handle(),
            0,
            &mut handle,
        )
    })?;
    // The console holds its own copies of its ends.
    drop((keys_read, shown_write));
    let console = Arc::new(PseudoConsole {
        handle,
        size: Mutex::new((rows, cols)),
    });
    let shown = Arc::new(Shown {
        state: Mutex::new(ShownState {
            bytes: VecDeque::new(),
            ended: false,
            wanted: true,
        }),
        ready: Condvar::new(),
    });
    spawn_reader(File::from(shown_read), Arc::clone(&shown))?;
    Ok((
        Pty {
            console: Arc::clone(&console),
            input: Mutex::new(File::from(keys_write)),
            shown,
        },
        PtySlave(console),
    ))
}

/// Read what the console shows until it closes — for as long as a [`Pty`]
/// wants it, then only to let the console close (it waits for its output to
/// be read).
fn spawn_reader(mut output: File, shown: Arc<Shown>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("rune-conpty-output".into())
        .spawn(move || {
            let mut chunk = vec![0u8; 16 * 1024];
            loop {
                let n = match output.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut state = crate::panic::lock(&shown.state);
                while state.wanted && state.bytes.len() >= SHOWN_LIMIT {
                    state = shown
                        .ready
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if state.wanted {
                    state.bytes.extend(&chunk[..n]);
                    shown.ready.notify_all();
                }
            }
            crate::panic::lock(&shown.state).ended = true;
            shown.ready.notify_all();
        })
        .map(drop)
}

impl Pty {
    /// Wait up to `timeout_ms` for output.
    ///
    /// `Ok(None)`     nothing arrived in the window
    /// `Ok(Some(0))`  the console closed
    /// `Ok(Some(n))`  `n` bytes
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        let mut state = crate::panic::lock(&self.shown.state);
        loop {
            if !state.bytes.is_empty() {
                let n = buf.len().min(state.bytes.len());
                for (slot, byte) in buf.iter_mut().zip(state.bytes.drain(..n)) {
                    *slot = byte;
                }
                self.shown.ready.notify_all();
                return Ok(Some(n));
            }
            if state.ended {
                return Ok(Some(0));
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            state = self
                .shown
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Send input to the running command, as keys: text, `\r` for Enter,
    /// `\x03` for Ctrl+C (the console sends the command its Ctrl+C).
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut input = crate::panic::lock(&self.input);
        input.write_all(bytes)?;
        input.flush()
    }

    /// Never known on Windows: no public interface tells that a console
    /// program waits to read (`INPUT_WAIT_DETECTION` is `Never`).
    pub fn input_wait(&self) -> Option<InputWait> {
        None
    }

    /// Not known on Windows: a console's input mode belongs to the programs
    /// attached to it.
    pub fn line_mode(&self) -> Option<bool> {
        None
    }

    /// A console cannot outlive this process on Windows, which never becomes
    /// another program in place (`REPLACES_IN_PLACE` is false).
    pub fn keep_across_exec(&self, _keep: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Windows has no exec: a console is never handed to a newer build",
        ))
    }

    /// This terminal, kept in this process under a number [`Pty::adopt`]
    /// takes it back by.
    pub fn into_handed(self) -> Handed {
        let mut kept = crate::panic::lock(kept());
        let number = kept.keys().max().map_or(1, |n| n + 1);
        kept.insert(number, self);
        Handed(number)
    }

    /// Take back a terminal [`Pty::into_handed`] kept; any other number is
    /// refused.
    pub fn adopt(handed: Handed) -> io::Result<Pty> {
        crate::panic::lock(kept()).remove(&handed.0).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a console Rune kept", handed.0),
            )
        })
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        Ok(*crate::panic::lock(&self.console.size))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        hresult(unsafe { ResizePseudoConsole(self.console.handle, coord(rows, cols)) })?;
        *crate::panic::lock(&self.console.size) = (rows, cols);
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // What the console shows from now on is only drained.
        let mut state = crate::panic::lock(&self.shown.state);
        state.wanted = false;
        state.bytes.clear();
        self.shown.ready.notify_all();
    }
}

fn kept() -> &'static Mutex<std::collections::HashMap<i64, Pty>> {
    static KEPT: std::sync::OnceLock<Mutex<std::collections::HashMap<i64, Pty>>> =
        std::sync::OnceLock::new();
    KEPT.get_or_init(Default::default)
}

#[cfg(test)]
pub(in crate::platform) mod tests {
    use super::*;
    use crate::platform::{Launch, Placement};

    #[test]
    fn a_console_opens_and_resizes() {
        let (pty, slave) = open_pty(24, 80).unwrap();
        pty.resize(40, 120).unwrap();
        assert_eq!(pty.size().unwrap(), (40, 120));
        drop(slave);
        drop(pty);
    }

    #[test]
    fn read_times_out_without_blocking_forever() {
        let (pty, _slave) = open_pty(24, 80).unwrap();
        let mut buf = [0u8; 32];
        // A console says what it is when it opens; read past it.
        while pty
            .read_timeout(&mut buf, 300)
            .unwrap()
            .is_some_and(|n| n > 0)
        {}
        let start = std::time::Instant::now();
        assert!(pty.read_timeout(&mut buf, 100).unwrap().is_none());
        assert!(start.elapsed() >= std::time::Duration::from_millis(80));
    }

    /// A program of a console, started the way `terminal_use` starts one.
    pub(in crate::platform) fn on_terminal(
        program: &str,
        args: &[&str],
    ) -> (Pty, super::super::process::Process) {
        let (pty, slave) = open_pty(24, 80).unwrap();
        let child = super::super::process::spawn_on_terminal(
            &Launch::new(program).args(args),
            &slave,
            Placement::default(),
        )
        .unwrap();
        drop(slave);
        (pty, child)
    }

    /// Everything `pty` shows until `until` appears or five seconds pass.
    pub(in crate::platform) fn shows(pty: &Pty, until: &str) -> String {
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !seen.contains(until) {
            if let Some(n) = pty.read_timeout(&mut buf, 100).unwrap() {
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }
        seen
    }

    #[test]
    fn a_real_program_runs_on_the_console() {
        let (pty, mut child) = on_terminal("cmd.exe", &["/D", "/C", "echo hello-from-conpty"]);
        let seen = shows(&pty, "hello-from-conpty");
        assert_eq!(child.wait().unwrap(), 0);
        assert!(seen.contains("hello-from-conpty"), "got {seen:?}");
    }

    #[test]
    fn keys_typed_reach_the_program() {
        // `!ANSWER!` with delayed expansion: `%ANSWER%` is expanded when
        // the line is read, before `set /p` has run.
        let (pty, mut child) = on_terminal(
            "cmd.exe",
            &["/D", "/V:ON", "/C", "set /p ANSWER=? & echo got-!ANSWER!"],
        );
        shows(&pty, "?");
        pty.write_all(b"yes\r").unwrap();
        let seen = shows(&pty, "got-yes");
        assert!(seen.contains("got-yes"), "got {seen:?}");
        let _ = child.wait();
    }

    #[test]
    fn a_kept_console_is_taken_back_only_by_its_number() {
        let (pty, _slave) = open_pty(24, 80).unwrap();
        assert!(pty.keep_across_exec(true).is_err());
        let handed = pty.into_handed();
        assert!(Pty::adopt(Handed(handed.0 + 1000)).is_err());
        assert!(Pty::adopt(handed).is_ok());
    }
}
