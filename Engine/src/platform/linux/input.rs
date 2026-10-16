//! Whether a terminal's program waits for someone to type, as Linux tells
//! it: each process of the foreground group, `/proc/<pid>/syscall` — blocked
//! in a read whose descriptor is this terminal or `/dev/tty`.

use std::ffi::CStr;

use crate::platform::{InputWait, InputWaitDetection};

/// Linux sees every wait: the kernel names the syscall a process is blocked
/// in and the descriptor it reads.
pub const INPUT_WAIT_DETECTION: InputWaitDetection = InputWaitDetection::Every;

/// `mode` is the terminal's own (echo off: hidden; canonical: a line). A
/// process this user may not inspect (a set-user-ID `sudo`, or a kernel that
/// forbids it) is counted only when the terminal is set for a hidden line,
/// the mode a password prompt sets. `None`: not waiting, or not known to be
/// — a process of the group still running, or a read of something else.
pub(in crate::platform) fn input_wait(
    master: libc::c_int,
    group: libc::pid_t,
    mode: InputWait,
) -> Option<InputWait> {
    let terminal = super::slave_name(master).ok()?;
    let terminal = terminal.to_string_lossy().into_owned();
    let mut reading = false;
    let mut uninspectable = false;
    let mut members = 0;
    for pid in super::procfs::group_members(group) {
        members += 1;
        let syscall = match std::fs::read_to_string(format!("/proc/{pid}/syscall")) {
            Ok(text) => text,
            Err(_) => {
                uninspectable = true;
                continue;
            }
        };
        let mut fields = syscall.split_whitespace();
        let Some(number) = fields.next().and_then(|n| n.parse::<libc::c_long>().ok()) else {
            // "running" (or a state the kernel does not detail): busy.
            return None;
        };
        if ![
            libc::SYS_read,
            libc::SYS_readv,
            libc::SYS_pread64,
            libc::SYS_preadv,
        ]
        .contains(&number)
        {
            continue;
        }
        let Some(read_fd) = fields
            .next()
            .and_then(|a| i64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
        else {
            continue;
        };
        let target = std::fs::read_link(format!("/proc/{pid}/fd/{read_fd}"));
        if target.is_ok_and(|t| t.as_os_str() == terminal.as_str() || t.as_os_str() == "/dev/tty") {
            reading = true;
        }
    }
    if members == 0 {
        return None;
    }
    if reading || (uninspectable && mode.hidden && mode.line) {
        Some(mode)
    } else {
        None
    }
}

/// The name of the terminal whose master is `fd` — `None` for a descriptor
/// that is no terminal master.
pub(super) fn ptsname(fd: libc::c_int) -> std::io::Result<std::ffi::CString> {
    let mut name = [0 as libc::c_char; 128];
    if unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned())
}

#[cfg(test)]
mod tests {
    use crate::platform::unix::pty::tests::{end, on_terminal, settled};
    use crate::platform::InputWait;

    /// The kernel and the terminal say when a program waits for someone to
    /// type, and how: a line read, a hidden entry, a single key — never
    /// guessed from what it printed (it printed nothing that says so here).
    #[test]
    fn a_program_blocked_reading_its_terminal_is_seen_waiting_and_how() {
        for (script, hidden, line) in [
            ("printf 'Continue? [Y/n] '; read answer", false, true),
            ("printf 'Password: '; read -s secret", true, true),
            ("read -n 1 key", false, false),
            ("cat", false, true),
            ("sleep 30 & wait; read x", false, true),
        ] {
            let (pty, child) = on_terminal("/bin/bash", script);
            let seen = if script.starts_with("sleep") {
                // Busy first (waiting on its child, not the terminal).
                let busy = settled(&pty);
                assert_eq!(busy, None, "{script}: nothing reads the terminal yet");
                end(child);
                continue;
            } else {
                settled(&pty)
            };
            assert_eq!(seen, Some(InputWait { hidden, line }), "{script}");
            assert_eq!(pty.line_mode(), Some(line), "{script}");
            end(child);
        }
    }

    /// Work that reads nothing, or reads something other than the terminal,
    /// is not waiting for input.
    #[test]
    fn work_that_does_not_read_the_terminal_is_not_waiting() {
        for script in [
            "sleep 30",
            "while true; do :; done",
            "mkfifo /proc/self/fd/0 2>/dev/null; read x < <(sleep 30)",
        ] {
            let (pty, child) = on_terminal("/bin/bash", script);
            assert_eq!(settled(&pty), None, "{script}");
            end(child);
        }
    }
}
