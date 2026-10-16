//! Whether a terminal's program waits for someone to type, as far as macOS
//! tells it. The kernel gives each thread's state — running, waiting — but
//! not what a waiting thread waits on (no syscall, no wait channel: XNU's
//! `proc_threadinfo` has neither, and `kinfo_proc` leaves `p_wmesg` empty).
//! So a visible prompt cannot be told from a program waiting on the network
//! or a timer, and is not claimed; a hidden line — the mode a password
//! prompt sets — while every thread of the foreground group waits is, as
//! Linux counts a process it may not inspect.

use crate::platform::{InputWait, InputWaitDetection};

/// macOS sees a wait only for a hidden entry read by line.
pub const INPUT_WAIT_DETECTION: InputWaitDetection = InputWaitDetection::HiddenOnly;

/// `mode` is the terminal's own (echo off: hidden; canonical: a line).
pub(in crate::platform) fn input_wait(
    _master: libc::c_int,
    group: libc::pid_t,
    mode: InputWait,
) -> Option<InputWait> {
    if !(mode.hidden && mode.line) {
        return None;
    }
    let members = super::procinfo::group_members(group);
    if members.is_empty() {
        return None;
    }
    // A process this user may not inspect (a set-user-ID `sudo`) is counted
    // as waiting: the terminal is set for a hidden line, which is what it
    // sets to ask for a password.
    members
        .iter()
        .all(|pid| every_thread_waits(*pid) != Some(false))
        .then_some(mode)
}

/// Whether every thread of `pid` is waiting (none running); `None` when the
/// kernel does not say (gone, or not this user's to ask).
fn every_thread_waits(pid: libc::pid_t) -> Option<bool> {
    /// `PROC_PIDLISTTHREADS` from `<sys/proc_info.h>`: the thread ids.
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    let mut ids = vec![0u64; 256];
    let bytes = (ids.len() * std::mem::size_of::<u64>()) as libc::c_int;
    let got =
        unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, ids.as_mut_ptr().cast(), bytes) };
    if got <= 0 {
        return None;
    }
    ids.truncate(got as usize / std::mem::size_of::<u64>());
    let size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
    for id in ids {
        let mut info: libc::proc_threadinfo = unsafe { std::mem::zeroed() };
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTHREADINFO,
                id,
                (&mut info as *mut libc::proc_threadinfo).cast(),
                size,
            )
        };
        if got != size {
            // A thread that ended meanwhile says nothing.
            continue;
        }
        if info.pth_run_state == libc::TH_STATE_RUNNING {
            return Some(false);
        }
    }
    Some(true)
}

#[cfg(test)]
mod tests {
    use crate::platform::unix::pty::tests::{end, on_terminal, settled};
    use crate::platform::InputWait;

    /// A password prompt — a hidden line read — is seen waiting; a visible
    /// question is not claimed, nor is work that reads nothing.
    #[test]
    fn a_hidden_entry_is_seen_waiting_and_nothing_else_is_claimed() {
        let (pty, child) = on_terminal("/bin/bash", "printf 'Password: '; read -s secret");
        assert_eq!(
            settled(&pty),
            Some(InputWait {
                hidden: true,
                line: true
            })
        );
        end(child);
        for script in [
            "printf 'Continue? [Y/n] '; read answer",
            "sleep 30",
            "while true; do :; done",
        ] {
            let (pty, child) = on_terminal("/bin/bash", script);
            assert_eq!(settled(&pty), None, "{script}");
            end(child);
        }
    }
}
