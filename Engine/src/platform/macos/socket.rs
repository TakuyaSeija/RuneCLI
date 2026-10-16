//! Unix sockets the macOS way: an address longer than the kernel takes is
//! reached through a short link in this user's private temporary folder,
//! the peer is named by `getpeereid`, and a blocked `accept` is woken
//! through a pipe — `shutdown` of a listening socket does not wake an
//! `accept` on macOS as it does on Linux.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// `sun_path` holds 104 bytes on macOS, its terminating NUL included.
const SUN_PATH: usize = 104;

/// Listen at `path`, however long.
pub(in crate::platform) fn bind(path: &Path) -> io::Result<UnixListener> {
    addressed(path, |address| UnixListener::bind(address))
}

/// Connect to `path`, however long.
pub(in crate::platform) fn connect(path: &Path) -> io::Result<UnixStream> {
    addressed(path, |address| UnixStream::connect(address))
}

/// The user the process at the other end ran as when the connection was
/// made (`getpeereid`), as the kernel recorded it.
pub(in crate::platform) fn peer_uid(stream: &UnixStream) -> Option<libc::uid_t> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    (unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

/// How a thread blocked in `accept` is woken: it waits on the socket and a
/// pipe together, and a wake makes the pipe readable for good — that accept
/// and every one after it fail at once.
pub(in crate::platform) struct ListenerWake {
    read: OwnedFd,
    write: OwnedFd,
}

impl ListenerWake {
    pub(in crate::platform) fn new() -> io::Result<ListenerWake> {
        let (read, write) = super::nonblocking_pipe()?;
        Ok(ListenerWake { read, write })
    }

    /// Wait until a connection waits on `socket` — or a wake came, which is
    /// an error.
    pub(in crate::platform) fn before_accept(&self, socket: &UnixListener) -> io::Result<()> {
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: socket.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: descriptors this listener owns and keeps open.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds[1].revents != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the listener was woken to stop",
                ));
            }
            if fds[0].revents != 0 {
                return Ok(());
            }
        }
    }

    pub(in crate::platform) fn wake(&self, _socket: &UnixListener) -> io::Result<()> {
        // One byte, never read: the pipe stays readable for every accept.
        // SAFETY: one byte to a pipe this listener owns.
        let n = unsafe { libc::write(self.write.as_raw_fd(), b"w".as_ptr().cast(), 1) };
        if n == 1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A pipe already full of wakes is woken already.
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        Err(error)
    }
}

/// Hand `act` an address for `path` the kernel accepts: `path` itself when it
/// fits; otherwise the socket's name inside a short link to its directory,
/// made for the call in this user's private temporary folder — which no
/// other user may enter, so nobody can swap the link — and removed after.
fn addressed<T>(path: &Path, act: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    if path.as_os_str().len() < SUN_PATH {
        return act(path);
    }
    let too_long = |why: &str| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is {} bytes, over the {} a socket address holds, {why}",
                path.display(),
                path.as_os_str().len(),
                SUN_PATH - 1
            ),
        )
    };
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(too_long("and it names no file in a directory"));
    };
    let private = super::darwin_folder(libc::_CS_DARWIN_USER_TEMP_DIR)
        .map_err(|_| too_long("and no private folder spells it short"))?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let link = private.join(format!(
        "rs{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let short = link.join(name);
    if short.as_os_str().len() >= SUN_PATH {
        return Err(too_long("and so is its short spelling"));
    }
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(dir, &link)?;
    let result = act(&short);
    let _ = std::fs::remove_file(&link);
    result
}
