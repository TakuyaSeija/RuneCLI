//! Files a child of this process must never hold, not even for the moment
//! between its `fork` and its `exec`.
//!
//! A lock on a whole file (`flock`) belongs to the open file, and `fork`
//! hands the child a copy of every descriptor: until the child closes its
//! copy the lock stays taken, whatever this process does with its own.
//! Close-on-exec closes it only at the `exec`, and a confined command does
//! real work before that — Landlock, seccomp, and on Linux a wait for Rune
//! to take its listener (`services::Handoff`, up to a minute). Meanwhile a
//! session this process let go still read as open elsewhere (its scratch
//! kept: macOS CI, 2026-10-18), and the configuration, credential or drafts
//! lock another window waits for was held by a command still starting.
//!
//! So every such descriptor is noted here while it is open, and the first
//! thing a child of Rune's does after `fork` is to close the ones noted
//! ([`close_in_child`]). The note is a fixed table of atomics: a child of a
//! threaded process may run only async-signal-safe code, so no lock and no
//! allocation is taken there. A descriptor leaves the table before it is
//! closed, so a number in the table always names the file it was noted for.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// How many descriptors can be noted at once: a process holds one lease per
/// open session and a few locks for a moment. One past it is not noted, and
/// stays the child's until its `exec` as before.
const SLOTS: usize = 1024;

static NOTED: [AtomicI32; SLOTS] = [const { AtomicI32::new(-1) }; SLOTS];

/// The note of one descriptor, taken back when dropped — which must happen
/// before the descriptor is closed.
#[derive(Debug)]
pub struct KeptFromChildren(Option<usize>);

/// Note `file`'s descriptor as one no child may hold.
pub fn keep_from_children(file: &impl AsRawFd) -> KeptFromChildren {
    let fd = file.as_raw_fd();
    for (slot, noted) in NOTED.iter().enumerate() {
        if noted
            .compare_exchange(-1, fd, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return KeptFromChildren(Some(slot));
        }
    }
    KeptFromChildren(None)
}

impl Drop for KeptFromChildren {
    fn drop(&mut self) {
        if let Some(slot) = self.0 {
            NOTED[slot].store(-1, Ordering::Release);
        }
    }
}

/// In a child, right after `fork`: close every descriptor noted. Only
/// atomics and `close`, both safe there.
pub(in crate::platform) fn close_in_child() {
    for noted in &NOTED {
        let fd: RawFd = noted.load(Ordering::Acquire);
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn lock_file(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-kept-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("held.lock")
    }

    /// A child that has not reached its `exec` yet: forked, then (when
    /// `closing`) doing what every child of Rune's does first, then waiting.
    fn child_still_starting(closing: bool) -> libc::pid_t {
        let (read, write) = {
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            (fds[0], fds[1])
        };
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            if closing {
                close_in_child();
            }
            unsafe {
                libc::write(write, b"x".as_ptr().cast(), 1);
                libc::sleep(5);
                libc::_exit(0)
            }
        }
        let mut byte = 0u8;
        unsafe {
            libc::read(read, (&mut byte as *mut u8).cast(), 1);
            libc::close(read);
            libc::close(write);
        }
        pid
    }

    fn end(pid: libc::pid_t) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }

    /// The control: a child that kept its copy holds the lock after this
    /// process let go of it — the session read as open elsewhere.
    #[test]
    fn a_child_that_keeps_its_copy_holds_a_lock_its_parent_let_go() {
        let path = lock_file("control");
        let held = File::create(&path).unwrap();
        held.lock().unwrap();
        let child = child_still_starting(false);
        drop(held);
        let busy = File::open(&path).unwrap().try_lock().is_err();
        end(child);
        assert!(busy, "the child's copy holds the lock");
    }

    /// A noted lock is closed in the child at once: let go here, it is free.
    #[test]
    fn a_noted_lock_is_free_once_its_holder_lets_go_whatever_a_child_is_doing() {
        let path = lock_file("noted");
        let held = File::create(&path).unwrap();
        held.lock().unwrap();
        let note = keep_from_children(&held);
        let child = child_still_starting(true);
        drop(note);
        drop(held);
        let free = File::open(&path).unwrap().try_lock().is_ok();
        end(child);
        assert!(free, "a child of Rune's held a lock its parent let go");
    }

    /// A note taken back leaves its number to whatever opens it next: a
    /// child never closes a descriptor that is no longer the noted file.
    #[test]
    fn a_note_taken_back_is_no_longer_closed() {
        let path = lock_file("forgotten");
        let file = File::create(&path).unwrap();
        let fd = file.as_raw_fd();
        drop(keep_from_children(&file));
        assert!(NOTED.iter().all(|n| n.load(Ordering::Acquire) != fd));
    }
}
