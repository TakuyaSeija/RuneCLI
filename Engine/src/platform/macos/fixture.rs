//! What tests observe of macOS, and the situations they set up — never part
//! of a build that ships (`test-support`, or this crate's own tests). The
//! same names and meanings as every OS's fixture.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::platform::{FileIdentity, Handed};

/// Where a process stands, as the kernel says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Running or asleep: alive.
    Live,
    /// Ended and not yet collected by its parent.
    Zombie,
    /// Its program ended, the OS still ending it: macOS holds a process
    /// whose terminal still has output nobody read until it is read
    /// (`PROC_FLAG_INEXIT`; CI, 2026-10-18: `ps` state `E`). Linux and
    /// Windows end it at once and never say this.
    Exiting,
}

/// `pid`'s state; `None` once it is gone (collected). An ended child of this
/// process not yet collected is seen without collecting it (`WNOWAIT`).
pub fn process_state(pid: i32) -> Option<ProcessState> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let ended = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if ended == 0 && info.si_pid == pid {
        return Some(ProcessState::Zombie);
    }
    match super::procinfo::bsd_info(pid) {
        // SZOMB: ended, its parent (not this process) yet to collect it.
        Some(info) if info.pbi_status == 5 => Some(ProcessState::Zombie),
        // PROC_FLAG_INEXIT
        Some(info) if info.pbi_flags & 4 != 0 => Some(ProcessState::Exiting),
        Some(_) => Some(ProcessState::Live),
        None => (unsafe { libc::kill(pid, 0) } == 0).then_some(ProcessState::Live),
    }
}

/// The process `pid`'s parent; `None` once it is gone.
pub fn parent_of(pid: i32) -> Option<u32> {
    super::procinfo::bsd_info(pid).map(|info| info.pbi_ppid)
}

/// Whether `pid` runs: alive and not a zombie.
pub fn running(pid: i32) -> bool {
    process_state(pid) == Some(ProcessState::Live)
}

/// The program file process `pid` runs — compared with
/// [`FileIdentity::of_path`] of an installed build, it tells which build runs.
pub fn program_of(pid: u32) -> io::Result<FileIdentity> {
    let mut path = vec![0u8; 4 * libc::MAXPATHLEN as usize];
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            path.as_mut_ptr().cast(),
            path.len() as u32,
        )
    };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    path.truncate(n as usize);
    use std::os::unix::ffi::OsStringExt;
    FileIdentity::of_path(Path::new(&std::ffi::OsString::from_vec(path)))
}

/// A symbolic link at `link` pointing to `target`.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// The number `file` has open, as a handover record names it — without
/// taking it from `file`, which still owns and closes it.
pub fn handed_number(file: &std::fs::File) -> Handed {
    Handed(file.as_raw_fd())
}

/// Whether `handed` stays open across this process's next exec.
pub fn kept_across_exec(handed: Handed) -> bool {
    let flags = unsafe { libc::fcntl(handed.0, libc::F_GETFD) };
    flags >= 0 && flags & libc::FD_CLOEXEC == 0
}

/// Access to a path taken away for a test, given back when this is dropped —
/// a panic part way included.
#[must_use = "access comes back when the guard is dropped"]
pub struct Restricted {
    path: std::path::PathBuf,
    mode: u32,
}

impl Restricted {
    /// `path`'s permission bits set to `mode` until the guard is dropped,
    /// when the bits it had come back. A path gone by then (moved by the
    /// code under test) is left to whoever moved it.
    pub fn new(path: &Path, mode: u32) -> io::Result<Restricted> {
        let had = crate::platform::file_mode(&std::fs::metadata(path)?);
        crate::platform::set_file_mode(path, mode)?;
        Ok(Restricted {
            path: path.to_path_buf(),
            mode: had,
        })
    }
}

impl Drop for Restricted {
    fn drop(&mut self) {
        let _ = crate::platform::set_file_mode(&self.path, self.mode);
    }
}

/// Let the group and others read `path` (and enter it, a folder): a file
/// someone loosened by hand.
pub fn share_with_others(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if std::fs::metadata(path)?.is_dir() {
        0o755
    } else {
        0o644
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Set when `path` — a file or a folder — was last modified.
pub fn set_modified(path: &Path, time: std::time::SystemTime) -> io::Result<()> {
    std::fs::File::open(path)?.set_modified(time)
}

/// A POSIX shell a test can run scripts in.
pub fn posix_shell() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from("/bin/sh"))
}

/// A file every write to fails, as on a full disk. macOS has no `/dev/full`:
/// the write end of a pipe whose reader is gone fails every write.
pub fn failing_file() -> io::Result<std::fs::File> {
    let (read, write) = super::nonblocking_pipe()?;
    drop(read);
    Ok(std::fs::File::from(write))
}

/// Tell process `pid` its terminal's window changed size, as a terminal
/// emulator does when its window is resized.
pub fn signal_resize(pid: u32) -> io::Result<()> {
    let pid =
        libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    if unsafe { libc::kill(pid, libc::SIGWINCH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_is_live_then_a_zombie_then_gone() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.1"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        assert!(running(pid));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_state(pid) != Some(ProcessState::Zombie) {
            assert!(std::time::Instant::now() < deadline, "never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!running(pid));
        child.wait().unwrap();
        assert_eq!(process_state(pid), None);
    }

    #[test]
    fn a_child_names_its_parent() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 1"])
            .spawn()
            .unwrap();
        assert_eq!(parent_of(child.id() as i32), Some(std::process::id()));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(parent_of(child.id() as i32), None);
    }

    #[test]
    fn a_running_test_is_its_own_program() {
        let me = program_of(std::process::id()).unwrap();
        assert_eq!(me, crate::platform::running_program().unwrap());
    }

    #[test]
    fn every_write_to_a_failing_file_fails() {
        use std::io::Write;
        let mut file = failing_file().unwrap();
        assert!(file.write_all(b"x").is_err());
    }
}
