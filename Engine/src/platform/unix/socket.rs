//! Local sockets: how a window reaches its session's host. On Unix a Unix
//! socket at a path in the home's `run/`. What differs between Unix kernels —
//! an address longer than a socket address holds, who the peer is, how a
//! blocked `accept` is woken — is the OS's (`os::bind`, `os::connect`,
//! `os::peer_uid`, `os::ListenerWake`).

use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::super::os;

/// A local socket a session host listens on.
pub struct LocalListener {
    socket: UnixListener,
    wake: Arc<os::ListenerWake>,
}

/// One connection between a window and a host.
pub struct LocalStream(UnixStream);

impl LocalListener {
    /// Listen at `path`, however long the path is.
    ///
    /// The kernel takes a socket address of at most ~100 bytes, and a
    /// session's socket lives in the home (`<home>/run/`), whose length is the
    /// user's to choose — a `RUNE_HOME` a little deep put every session host
    /// past it, and no window could start one. The socket stays at `path`,
    /// with the same owner and the same permissions; only the address handed
    /// to the kernel is spelt short.
    pub fn bind(path: &Path) -> io::Result<LocalListener> {
        Ok(LocalListener {
            socket: os::bind(path)?,
            wake: Arc::new(os::ListenerWake::new()?),
        })
    }

    /// The next connection.
    pub fn accept(&self) -> io::Result<LocalStream> {
        self.wake.before_accept(&self.socket)?;
        self.socket.accept().map(|(stream, _)| LocalStream(stream))
    }

    /// Every connection, as it comes; ends once [`LocalListener::wake`] was
    /// called (each accept after it fails).
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<LocalStream>> + '_ {
        std::iter::repeat_with(move || self.accept())
    }

    /// Another handle on the same socket — kept to wake the accept loop by.
    pub fn try_clone(&self) -> io::Result<LocalListener> {
        Ok(LocalListener {
            socket: self.socket.try_clone()?,
            wake: Arc::clone(&self.wake),
        })
    }

    /// Wake a thread blocked accepting on this socket: it returns at once,
    /// with an error, and so does every accept after.
    ///
    /// Through the socket itself, not its path: a host used to wake its own
    /// accept loop by connecting to its socket's file, and when that file was
    /// gone — a home removed, a `run/` cleared by hand — the connect failed,
    /// the loop slept on and the host hung in its shutdown forever (measured:
    /// a test host still alive 29 minutes after its last window).
    pub fn wake(&self) -> io::Result<()> {
        self.wake.wake(&self.socket)
    }
}

impl LocalStream {
    /// Connect to the socket at `path`, however long the path is (see
    /// [`LocalListener::bind`]).
    pub fn connect(path: &Path) -> io::Result<LocalStream> {
        os::connect(path).map(LocalStream)
    }

    /// Two ends connected to each other — for tests of what a connection
    /// carries.
    pub fn pair() -> io::Result<(LocalStream, LocalStream)> {
        UnixStream::pair().map(|(a, b)| (LocalStream(a), LocalStream(b)))
    }

    /// Another handle on the same connection: a reader beside a writer.
    pub fn try_clone(&self) -> io::Result<LocalStream> {
        self.0.try_clone().map(LocalStream)
    }

    /// How long a read waits; `None` for as long as it takes.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    /// How long a write waits; `None` for as long as it takes.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    /// End the connection both ways: every handle on it reads its end.
    pub fn shutdown(&self) -> io::Result<()> {
        self.0.shutdown(std::net::Shutdown::Both)
    }

    /// Whether the process at the other end runs as this process's user, as
    /// the kernel recorded it when the connection was made — not anything
    /// the peer says about itself.
    pub fn peer_is_same_user(&self) -> bool {
        os::peer_uid(&self.0).is_some_and(|uid| uid == unsafe { libc::geteuid() })
    }
}

impl Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Read for &LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Write for &LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}

/// Whether the entry at `path` is a local socket (not followed if a link).
pub fn is_socket(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket whose path is longer than a socket address holds is bound and
    /// reached at that path, and lands where the long spelling names.
    #[test]
    fn a_socket_path_past_the_address_limit_still_binds_and_connects() {
        let dir = std::env::temp_dir()
            .join(format!("rune-longsock-{}", std::process::id()))
            .join("d".repeat(120))
            .join("run");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Session1Host.sock");
        assert!(path.as_os_str().len() > 107);
        let listener = LocalListener::bind(&path).unwrap();
        assert!(is_socket(&path), "not at its path");
        let mut client = LocalStream::connect(&path).unwrap();
        client.write_all(b"ping").unwrap();
        drop(client);
        let mut accepted = listener.accept().unwrap();
        let mut got = String::new();
        accepted.read_to_string(&mut got).unwrap();
        assert_eq!(got, "ping");
        let short = std::env::temp_dir().join(format!("rune-sock-{}", std::process::id()));
        let _ = std::fs::remove_file(&short);
        drop(LocalListener::bind(&short).unwrap());
        assert!(
            LocalStream::connect(&short).is_err(),
            "nobody listens any more"
        );
        let _ = std::fs::remove_file(&short);
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("rune-longsock-{}", std::process::id())),
        );
    }

    /// A wake ends a blocked accept at once, whatever became of the path —
    /// and every accept after it fails too.
    #[test]
    fn a_wake_ends_an_accept_even_when_the_path_is_gone() {
        let path = std::env::temp_dir().join(format!("rune-wake-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = LocalListener::bind(&path).unwrap();
        let waker = listener.try_clone().unwrap();
        let accepting = std::thread::spawn(move || {
            let first = listener.accept().is_err();
            (first, listener.accept().is_err())
        });
        std::thread::sleep(Duration::from_millis(50));
        std::fs::remove_file(&path).unwrap();
        waker.wake().unwrap();
        assert_eq!(
            accepting.join().unwrap(),
            (true, true),
            "accept ended with an error, and so did the next"
        );
    }

    /// Both ends of a connection this user made run as this user.
    #[test]
    fn a_peer_of_this_user_is_this_user() {
        let (a, b) = LocalStream::pair().unwrap();
        assert!(a.peer_is_same_user());
        assert!(b.peer_is_same_user());
        assert!(!is_socket(&std::env::temp_dir()), "a folder is no socket");
    }
}
