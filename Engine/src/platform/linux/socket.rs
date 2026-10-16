//! Unix sockets the Linux way: an address too long for the kernel spelt
//! through `/proc/self/fd`, the peer named by `SO_PEERCRED`, and a blocked
//! `accept` woken by shutting the listening socket.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Listen at `path`, however long.
pub(in crate::platform) fn bind(path: &Path) -> io::Result<UnixListener> {
    addressed(path, |address| UnixListener::bind(address))
}

/// Connect to `path`, however long.
pub(in crate::platform) fn connect(path: &Path) -> io::Result<UnixStream> {
    addressed(path, |address| UnixStream::connect(address))
}

/// The user the process at the other end ran as when the connection was
/// made (`SO_PEERCRED`), as the kernel recorded it.
pub(in crate::platform) fn peer_uid(stream: &UnixStream) -> Option<libc::uid_t> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// How a thread blocked in `accept` is woken: on Linux `shutdown` of the
/// listening socket ends its `accept` at once, and every one after it.
pub(in crate::platform) struct ListenerWake;

impl ListenerWake {
    pub(in crate::platform) fn new() -> io::Result<ListenerWake> {
        Ok(ListenerWake)
    }

    /// Nothing to wait for first: `accept` itself is woken.
    pub(in crate::platform) fn before_accept(&self, _socket: &UnixListener) -> io::Result<()> {
        Ok(())
    }

    pub(in crate::platform) fn wake(&self, socket: &UnixListener) -> io::Result<()> {
        // SAFETY: a descriptor the listener owns and keeps open for the call.
        if unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Hand `act` an address for `path` the kernel accepts: `path` itself when it
/// fits, otherwise the socket's name inside its directory reached through an
/// open descriptor (`/proc/self/fd/<n>/<name>`), which resolves to the same
/// directory the long spelling names.
fn addressed<T>(path: &Path, act: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    /// `sun_path` holds 108 bytes, its terminating NUL included.
    const SUN_PATH: usize = 108;
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
    if !Path::new("/proc/self/fd").is_dir() {
        return Err(too_long("and /proc is not mounted to spell it short"));
    }
    use std::os::unix::fs::OpenOptionsExt;
    // A path handle: it names the directory without reading it, closes on
    // exec like every descriptor Rune opens, and lives until `act` returns.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(dir)?;
    let short =
        std::path::PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd())).join(name);
    if short.as_os_str().len() >= SUN_PATH {
        return Err(too_long("and so is its file name alone"));
    }
    act(&short)
}
