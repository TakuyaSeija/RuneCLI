//! This process's standard input, read straight from the descriptor.
//!
//! Never through `std::io::Stdin`: its buffer can hold bytes the kernel no
//! longer reports, and a wait before a read would then sleep on keys already
//! typed.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Duration;

/// Wakes a thread waiting on stdin ([`wait_stdin`]) from another thread — a
/// window parking its key reader before it `exec`s a newer build.
pub struct InputWaker {
    read: OwnedFd,
    write: OwnedFd,
}

impl InputWaker {
    /// A pipe, both ends closing on exec like every descriptor Rune opens.
    pub fn new() -> io::Result<InputWaker> {
        let (read, write) = super::super::os::nonblocking_pipe()?;
        Ok(InputWaker { read, write })
    }

    /// Wake whoever waits, now or at its next wait. A wake pending already
    /// is enough: a full pipe already wakes.
    pub fn wake(&self) {
        // SAFETY: one byte to a pipe this waker owns.
        unsafe { libc::write(self.write.as_raw_fd(), b"p".as_ptr().cast(), 1) };
    }

    fn drain(&self) {
        let mut sink = [0u8; 64];
        // SAFETY: non-blocking reads into a local buffer.
        while unsafe { libc::read(self.read.as_raw_fd(), sink.as_mut_ptr().cast(), sink.len()) } > 0
        {
        }
    }
}

/// Wait until stdin has something to say (bytes, or its end), `timeout`
/// passes (`None`: no limit) or `waker` is woken. `true` when stdin can be
/// read without waiting; a wake is used up by the wait it ends.
pub fn wait_stdin(timeout: Option<Duration>, waker: Option<&InputWaker>) -> bool {
    let mut fds = [
        libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            // A negative descriptor is ignored.
            fd: waker.map_or(-1, |w| w.read.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    // SAFETY: descriptors this process owns and a struct initialised above.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if let Some(waker) = waker.filter(|_| fds[1].revents != 0) {
        waker.drain();
    }
    ready > 0 && fds[0].revents != 0
}

/// Read what stdin holds into `buf`: `Ok(0)` at its end. A read a signal
/// interrupted is an `Interrupted` error, to be tried again.
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: a read into a local buffer from stdin.
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Wait up to `timeout_ms` for stdin, then read what is there — the way a
/// `-host` reads the commands a frontend writes it.
///
/// `Ok(None)`     nothing arrived in the window (or a signal interrupted it)
/// `Ok(Some(0))`  stdin ended
/// `Ok(Some(n))`  `n` bytes
pub fn read_stdin_timeout(buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
    super::files::read_timeout(libc::STDIN_FILENO, buf, timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wake ends a wait on stdin at once and is used up by it.
    #[test]
    fn a_wake_ends_the_wait_and_is_used_up() {
        let waker = InputWaker::new().unwrap();
        waker.wake();
        waker.wake();
        let started = std::time::Instant::now();
        wait_stdin(Some(Duration::from_secs(5)), Some(&waker));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the wake ended the wait"
        );
        let mut fds = libc::pollfd {
            fd: waker.read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut fds, 1, 0) },
            0,
            "every wake used up"
        );
    }
}
