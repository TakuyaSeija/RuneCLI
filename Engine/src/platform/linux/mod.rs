//! Linux: what the Unix half of the platform (`platform/unix/`) leaves to the
//! kernel it runs on — Landlock and seccomp for a confined command, what
//! `/proc` tells of processes and of this program, and a socket address
//! spelt through `/proc/self/fd`.

mod confine;
mod input;
mod landlock;
mod procfs;
mod seccomp;
mod socket;

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

use std::borrow::Cow;
use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};

use super::{Launch, Os, Placement};

pub use confine::{Confinement, Grants, Unconfined, DEVICES};
pub(in crate::platform) use input::input_wait;
pub use input::INPUT_WAIT_DETECTION;
pub(in crate::platform) use procfs::tree_members;
pub use procfs::{
    boot_time, installed_program, own_program, process_started, program_at, running_program,
};
pub(in crate::platform) use socket::{bind, connect, peer_uid, ListenerWake};

/// This build runs on Linux.
pub const OS: Os = Os::Linux;

/// A process becomes a newer build in place (`exec`): its pid, its terminal
/// and what it hands on stay.
pub const REPLACES_IN_PLACE: bool = true;

/// Nothing to take at start on Linux: `/proc/self/exe` names the running
/// program for as long as it runs.
pub(in crate::platform) fn at_start() {}

/// No folder of the system's own keeps this user's tool caches on Linux:
/// they live under the home (`~/.cache`), which the sandbox names.
pub fn system_caches() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// A terminal program on Linux reaches no clipboard of its own — X11 and
/// Wayland each need a connection to the display, which a program in a
/// terminal may not have: the terminal is asked instead (OSC 52).
pub fn set_clipboard(_text: &str) -> bool {
    false
}

/// A terminal master, closing on exec from the moment it exists.
pub(in crate::platform) fn open_master() -> io::Result<OwnedFd> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(master) })
}

/// The path of the terminal whose master is `fd`; an error for a descriptor
/// that is no terminal master.
pub(in crate::platform) fn slave_name(fd: libc::c_int) -> io::Result<CString> {
    input::ptsname(fd)
}

/// A pipe, both ends non-blocking and closing on exec from the moment they
/// exist.
pub(in crate::platform) fn nonblocking_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: two ints for the kernel to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just opened both, and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Linux confines in the child (`ChildSteps`): the program started is the
/// one asked for.
pub(in crate::platform) fn confined_launch<'a>(
    launch: &'a Launch,
    _placement: &Placement<'_>,
) -> io::Result<Cow<'a, Launch>> {
    Ok(Cow::Borrowed(launch))
}

/// Make stdin — the terminal by now — the calling process's controlling
/// terminal. In the child between `fork` and `exec`: one syscall.
pub(in crate::platform) fn claim_terminal() -> io::Result<()> {
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a placed child does to itself between `fork` and `exec`, after it
/// left its session: Landlock, then seccomp's refusal of Unix sockets. Bare
/// syscalls only.
pub(in crate::platform) struct ChildSteps {
    confinement: Option<Confinement>,
    refuse_unix_sockets: bool,
}

impl ChildSteps {
    pub(in crate::platform) fn new(placement: &Placement<'_>) -> io::Result<ChildSteps> {
        Ok(ChildSteps {
            confinement: placement.confinement.cloned(),
            refuse_unix_sockets: !placement.unix_sockets,
        })
    }

    pub(in crate::platform) fn run(&self) -> io::Result<()> {
        if let Some(confinement) = &self.confinement {
            confinement.restrict_calling_thread()?;
        }
        if self.refuse_unix_sockets {
            seccomp::refuse_unix_sockets()?;
        }
        Ok(())
    }
}
