//! Unix pseudo-terminals (Linux and macOS).
//!
//! A pty rather than pipes because half of what a developer runs behaves
//! differently without one: progress bars, colour, `less`, anything that calls
//! `isatty`. An agent that sees different output from the user is an agent that
//! reasons about a different machine.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use super::super::os;
use super::files::{close_on_exec, read_timeout, Handed};
use crate::platform::InputWait;

/// The master side. Dropping it closes the terminal, which sends the child
/// `SIGHUP` — that is the intended cleanup path.
pub struct Pty {
    master: OwnedFd,
}

/// The other end of a [`Pty`]: the command's terminal, handed to it by
/// [`super::spawn_on_terminal`] and closed in this process once the command
/// started.
pub struct PtySlave(pub(in crate::platform) OwnedFd);

/// Open a pty pair sized `rows` × `cols`.
///
/// Returns the master (kept by the engine) and the slave (handed to the child as
/// its stdin, stdout and stderr, then dropped by the parent).
///
/// Both ends close on `exec`. A descriptor left open is inherited by every
/// program this process starts afterwards — another command, an MCP server —
/// and a terminal's master is a way into whatever runs on it: read its
/// output, type into it. The kernel sandbox cannot help there, because it
/// governs what a process opens, not what it was handed already open. The
/// child gets its terminal through stdin/stdout/stderr, which `dup2` makes
/// without the flag; a master handed to a newer build of Rune on purpose is
/// [`Pty::keep_across_exec`].
pub fn open_pty(rows: u16, cols: u16) -> io::Result<(Pty, PtySlave)> {
    let master = os::open_master()?;
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let slave_path = os::slave_name(master.as_raw_fd())?;
    let slave = unsafe {
        libc::open(
            slave_path.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };

    let pty = Pty { master };
    pty.resize(rows, cols)?;
    Ok((pty, PtySlave(slave)))
}

impl Pty {
    /// Wait up to `timeout_ms` for output.
    ///
    /// `Ok(None)`     nothing arrived in the window — the caller can check for
    ///                cancellation and come back
    /// `Ok(Some(0))`  the child closed the terminal
    /// `Ok(Some(n))`  `n` bytes
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
        match read_timeout(self.master.as_raw_fd(), buf, timeout_ms) {
            // The kernel reports a hung-up pty as EIO. That is not a failure —
            // it is how "the child is gone" arrives on this fd.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(Some(0)),
            other => other,
        }
    }

    /// Send input to the running command — a password, a `y`, a `\x03` for
    /// Ctrl-C. Without this, anything that prompts hangs until its timeout and
    /// the agent learns nothing about why.
    pub fn write_all(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            bytes = &bytes[n as usize..];
        }
        Ok(())
    }

    /// Whether the program in front of this terminal is blocked reading it —
    /// waiting for someone to type — and how the terminal reads, as facts:
    /// the terminal's own mode (`tcgetattr` on the master: `ECHO` off is a
    /// hidden entry, `ICANON` a line read at Enter) and what the kernel says
    /// of each process of the terminal's foreground group. How much the
    /// kernel says is the OS's ([`super::super::INPUT_WAIT_DETECTION`]).
    /// `None`: not waiting, or not known to be. Nothing printed is ever
    /// parsed.
    pub fn input_wait(&self) -> Option<InputWait> {
        let fd = self.master.as_raw_fd();
        let (hidden, line) = terminal_mode(fd)?;
        let mut group: libc::pid_t = 0;
        if unsafe { libc::ioctl(fd, libc::TIOCGPGRP, &mut group) } != 0 || group <= 0 {
            return None;
        }
        os::input_wait(fd, group, InputWait { hidden, line })
    }

    /// Whether this terminal hands its reader a line at Enter (canonical
    /// mode) rather than every key as pressed; `None` when it cannot be read.
    pub fn line_mode(&self) -> Option<bool> {
        terminal_mode(self.master.as_raw_fd()).map(|(_, line)| line)
    }

    /// Whether this master stays open across the `exec` this process is
    /// about to do — kept for the program it becomes ([`Pty::into_handed`]),
    /// or closing on exec again when that handover is called off. Nothing is
    /// given up on failure.
    pub fn keep_across_exec(&self, keep: bool) -> io::Result<()> {
        close_on_exec(self.master.as_raw_fd(), !keep)
    }

    /// This master, for the program this process `exec`s next — which takes
    /// it over by [`Pty::adopt`].
    pub fn into_handed(self) -> Handed {
        Handed(self.master.into_raw_fd())
    }

    /// Take over a master a previous program of this process handed on. It
    /// must be a terminal master, or it is refused — a stray number is never
    /// mistaken for one. It closes on `exec` again from here on.
    pub fn adopt(handed: Handed) -> io::Result<Pty> {
        let fd: RawFd = handed.0;
        if os::slave_name(fd).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("descriptor {fd} is not a terminal master"),
            ));
        }
        close_on_exec(fd, true)?;
        Ok(Pty {
            master: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((size.ws_row, size.ws_col))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A terminal's mode as `(hidden, line)`: echo off, canonical reads.
fn terminal_mode(fd: RawFd) -> Option<(bool, bool)> {
    let mut mode: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut mode) } != 0 {
        return None;
    }
    Some((
        mode.c_lflag & libc::ECHO == 0,
        mode.c_lflag & libc::ICANON != 0,
    ))
}

#[cfg(test)]
pub(in crate::platform) mod tests {
    use super::*;
    use crate::platform::{Launch, Placement};

    #[test]
    fn pty_opens_and_resizes() {
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
        let start = std::time::Instant::now();
        let r = pty.read_timeout(&mut buf, 100).unwrap();
        assert!(r.is_none());
        assert!(start.elapsed() >= std::time::Duration::from_millis(80));
    }

    #[test]
    fn what_is_written_to_the_slave_arrives_on_the_master() {
        let (pty, slave) = open_pty(24, 80).unwrap();
        let mut file = std::fs::File::from(slave.0);
        use std::io::Write;
        file.write_all(b"hello\n").unwrap();
        let mut buf = [0u8; 64];
        let n = pty.read_timeout(&mut buf, 1000).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("hello"));
        drop(file);
    }

    #[test]
    fn input_written_to_the_master_reaches_the_command() {
        use std::io::Read;
        let (pty, slave) = open_pty(24, 80).unwrap();
        pty.write_all(b"hello-input\n").unwrap();
        let mut file = std::fs::File::from(slave.0);
        let mut buf = [0u8; 64];
        let n = file.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("hello-input"));
    }

    #[test]
    fn closed_slave_reports_eof_not_an_error() {
        let (pty, slave) = open_pty(24, 80).unwrap();
        drop(slave);
        let mut buf = [0u8; 8];
        // Either EOF now, or a timeout then EOF — both are "no child left".
        let r = pty.read_timeout(&mut buf, 200).unwrap();
        assert!(matches!(r, Some(0) | None));
    }

    /// A program of a terminal, started the way `terminal_use` starts one,
    /// and its terminal.
    pub(in crate::platform) fn on_terminal(
        shell: &str,
        script: &str,
    ) -> (Pty, super::super::process::Process) {
        let (pty, slave) = open_pty(24, 80).unwrap();
        let child = super::super::process::spawn_on_terminal(
            &Launch::new(shell).args(["-c", script]),
            &slave,
            Placement::default(),
        )
        .unwrap();
        drop(slave);
        (pty, child)
    }

    #[test]
    fn a_real_child_runs_on_the_pty() {
        let (pty, mut child) = on_terminal("/bin/sh", "printf hello-from-pty");
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match pty.read_timeout(&mut buf, 200).unwrap() {
                Some(0) => break,
                Some(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                None => {}
            }
            if seen.contains("hello-from-pty") {
                break;
            }
        }
        let _ = child.wait();
        assert!(seen.contains("hello-from-pty"), "got {seen:?}");
    }

    /// The terminal is the command's own: its controlling terminal, its
    /// session led by it — so the group it leads is the terminal's
    /// foreground group, and the whole tree can be stopped through it.
    #[test]
    fn the_terminal_is_the_commands_controlling_terminal() {
        let (pty, mut child) = on_terminal("/bin/sh", "sleep 30");
        let mut group: libc::pid_t = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while group != child.id() as libc::pid_t {
            assert!(
                std::time::Instant::now() < deadline,
                "never the foreground group"
            );
            unsafe { libc::ioctl(pty.master.as_raw_fd(), libc::TIOCGPGRP, &mut group) };
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = super::super::process::kill_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
    }

    /// The output of `pty` until it has been quiet a while, then whether its
    /// program waits for input.
    pub(in crate::platform) fn settled(pty: &Pty) -> Option<InputWait> {
        let mut buf = [0u8; 4096];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut quiet = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            if let Ok(Some(n)) = pty.read_timeout(&mut buf, 50) {
                if n > 0 {
                    quiet = std::time::Instant::now();
                }
            }
            if quiet.elapsed() > std::time::Duration::from_millis(400) {
                break;
            }
        }
        pty.input_wait()
    }

    /// Stop what `on_terminal` started.
    pub(in crate::platform) fn end(mut child: super::super::process::Process) {
        let _ = super::super::process::kill_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
    }

    /// A terminal's master is never inherited by another program: not by a
    /// command started while a background task runs, sandboxed or not.
    #[test]
    fn a_terminal_never_leaks_into_another_program() {
        let (pty, slave) = open_pty(24, 80).unwrap();
        let open = [pty.master.as_raw_fd(), slave.0.as_raw_fd()];
        // `/dev/fd` lists the descriptors of the program reading it, which
        // inherited the shell's — every Unix has it.
        let seen = std::process::Command::new("/bin/sh")
            .args(["-c", "ls /dev/fd/"])
            .output()
            .unwrap();
        let fds: Vec<i32> = String::from_utf8_lossy(&seen.stdout)
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect();
        assert!(fds.contains(&0), "the listing is real: {fds:?}");
        for fd in open {
            assert!(
                !fds.contains(&fd),
                "descriptor {fd} leaked into a child: {fds:?}"
            );
        }
    }

    /// A master handed across an exec is taken back only as a terminal
    /// master; a stray number is refused.
    #[test]
    fn only_a_terminal_master_is_adopted() {
        let (pty, _slave) = open_pty(24, 80).unwrap();
        pty.keep_across_exec(true).unwrap();
        let handed = pty.into_handed();
        let flags = unsafe { libc::fcntl(handed.0, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "open across the exec");
        let adopted = Pty::adopt(handed).unwrap();
        let flags = unsafe { libc::fcntl(adopted.master.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "closes on exec again");
        let file = std::fs::File::open("/dev/null").unwrap();
        let stray = Handed(file.as_raw_fd());
        assert_eq!(
            Pty::adopt(stray).err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
    }
}
