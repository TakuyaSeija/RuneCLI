//! macOS: what the Unix half of the platform (`platform/unix/`) leaves to the
//! kernel it runs on — Seatbelt for a confined command, what `libproc` and
//! `sysctl` tell of processes, the file this program runs from (no
//! `/proc/self/exe`), and a socket path longer than a socket address.

mod confine;
mod input;
mod procinfo;
mod socket;

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

use std::borrow::Cow;
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{Launch, Os, Placement};

pub use confine::{Confinement, Grants, Unconfined, DEVICES};
pub(in crate::platform) use input::input_wait;
pub use input::INPUT_WAIT_DETECTION;
pub(in crate::platform) use procinfo::tree_members;
pub use procinfo::{
    boot_time, installed_program, own_program, process_started, program_at, running_program,
};
pub(in crate::platform) use socket::{bind, connect, peer_uid, ListenerWake};

/// This build runs on macOS.
pub const OS: Os = Os::MacOs;

/// A process becomes a newer build in place (`exec`): its pid, its terminal
/// and what it hands on stay.
pub const REPLACES_IN_PLACE: bool = true;

/// Hold the file this program runs from while it is still the one at its
/// path, and remove a way-back copy it runs from.
pub(in crate::platform) fn at_start() {
    procinfo::remember_own_program();
}

/// The folders macOS keeps for this user's caches and temporary files
/// (`_CS_DARWIN_USER_CACHE_DIR`, `_CS_DARWIN_USER_TEMP_DIR`): the compilers
/// write their module caches there whatever `TMPDIR` says.
pub fn system_caches() -> Vec<std::path::PathBuf> {
    [
        libc::_CS_DARWIN_USER_CACHE_DIR,
        libc::_CS_DARWIN_USER_TEMP_DIR,
    ]
    .into_iter()
    .filter_map(|name| darwin_folder(name).ok())
    .collect()
}

/// Put `text` on the pasteboard through `/usr/bin/pbcopy`, which every macOS
/// has: Terminal.app, the terminal macOS comes with, ignores the terminal
/// clipboard sequence (OSC 52). Read as UTF-8 whatever the locale says.
pub fn set_clipboard(text: &str) -> bool {
    use std::io::Write;
    let child = std::process::Command::new("/usr/bin/pbcopy")
        .env("LC_CTYPE", "UTF-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && written
}

/// A folder macOS names for this user by `confstr`.
pub(super) fn darwin_folder(name: libc::c_int) -> io::Result<std::path::PathBuf> {
    let mut buffer = vec![0u8; 1024];
    let n = unsafe { libc::confstr(name, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n == 0 || n > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "macOS named no such folder for this user",
        ));
    }
    buffer.truncate(n - 1);
    use std::os::unix::ffi::OsStringExt;
    Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(
        buffer,
    )))
}

/// `fd` closes on exec from here on.
fn close_on_exec(fd: &OwnedFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A terminal master. macOS takes no `O_CLOEXEC` in `posix_openpt`, so the
/// flag is set at once after.
pub(in crate::platform) fn open_master() -> io::Result<OwnedFd> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    close_on_exec(&master)?;
    Ok(master)
}

/// The path of the terminal whose master is `fd` (`TIOCPTYGNAME`, the
/// thread-safe way macOS names it); an error for a descriptor that is no
/// terminal master.
pub(in crate::platform) fn slave_name(fd: libc::c_int) -> io::Result<CString> {
    let mut name = [0 as libc::c_char; 128];
    if unsafe { libc::ioctl(fd, libc::TIOCPTYGNAME as libc::c_ulong, name.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned())
}

/// A pipe, both ends non-blocking and closing on exec. macOS has no `pipe2`:
/// the flags follow the pipe at once.
pub(in crate::platform) fn nonblocking_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: two ints for the kernel to fill.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just opened both, and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for end in [&read, &write] {
        close_on_exec(end)?;
        let flags = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

/// macOS confines a command by starting it inside Seatbelt
/// (`/usr/bin/sandbox-exec`), which applies the profile to itself and then
/// becomes the command: the same pid, session and terminal.
pub(in crate::platform) fn confined_launch<'a>(
    launch: &'a Launch,
    placement: &Placement<'_>,
) -> io::Result<Cow<'a, Launch>> {
    confine::wrap(launch, placement)
}

/// Make stdin — the terminal by now — the calling process's controlling
/// terminal. In the child between `fork` and `exec`: one syscall.
pub(in crate::platform) fn claim_terminal() -> io::Result<()> {
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Nothing is applied in the child on macOS: the sandbox is the program it
/// starts ([`confined_launch`]).
pub(in crate::platform) struct ChildSteps;

/// Nothing to watch once it started: Seatbelt holds its sockets itself.
pub(in crate::platform) struct Watch;

impl ChildSteps {
    pub(in crate::platform) fn new(_placement: &Placement<'_>) -> io::Result<(ChildSteps, Watch)> {
        Ok((ChildSteps, Watch))
    }

    pub(in crate::platform) fn run(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Watch {
    pub(in crate::platform) fn started(self, _root: i32) {}
}
