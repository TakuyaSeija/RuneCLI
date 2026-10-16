//! Raw Landlock bindings.
//!
//! Three syscalls, two structs, no crate. The `landlock` crate wraps exactly
//! this with a builder API and a compatibility matrix; we need one fixed policy
//! shape, so the wrapper would be more code than the syscalls.
//!
//! Constants are transcribed from `/usr/include/linux/landlock.h` and are ABI,
//! not implementation detail — they cannot change without breaking every
//! Landlock user on earth. What a confined command may reach is decided above
//! the platform (`sandbox::Policy`); `confine.rs` turns it into these rules.

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

/// Scopes, ABI 6 and up. These are not filesystem rights: they stop a sandboxed
/// process from reaching *out* of its domain by other means.
pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
pub const SCOPE_SIGNAL: u64 = 1 << 1;

pub const FS_EXECUTE: u64 = 1 << 0;
pub const FS_WRITE_FILE: u64 = 1 << 1;
pub const FS_READ_FILE: u64 = 1 << 2;
pub const FS_READ_DIR: u64 = 1 << 3;
pub const FS_REMOVE_DIR: u64 = 1 << 4;
pub const FS_REMOVE_FILE: u64 = 1 << 5;
pub const FS_MAKE_CHAR: u64 = 1 << 6;
pub const FS_MAKE_DIR: u64 = 1 << 7;
pub const FS_MAKE_REG: u64 = 1 << 8;
pub const FS_MAKE_SOCK: u64 = 1 << 9;
pub const FS_MAKE_FIFO: u64 = 1 << 10;
pub const FS_MAKE_BLOCK: u64 = 1 << 11;
pub const FS_MAKE_SYM: u64 = 1 << 12;
pub const FS_REFER: u64 = 1 << 13;
pub const FS_TRUNCATE: u64 = 1 << 14;
pub const FS_IOCTL_DEV: u64 = 1 << 15;

/// The kernel reads as many fields as the size we pass, and zero-fills the rest.
/// So one struct serves every ABI: send 8 bytes on an old kernel (filesystem
/// only) and 24 on ABI 6+ (filesystem plus scopes).
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// Size to send for this ABI level. Sending a field the kernel does not know is
/// an `EINVAL`, not a stricter policy.
fn attr_size(abi: i32) -> usize {
    if abi >= 6 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        std::mem::size_of::<u64>()
    }
}

/// Scopes this ABI supports. Zero below ABI 6.
pub fn all_scopes(abi: i32) -> u64 {
    if abi >= 6 {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    }
}

/// The rights that mean anything on a **file**.
///
/// A rule anchored at a regular file may only carry file access rights; asking
/// for `FS_MAKE_DIR` on one is `EINVAL`, and the whole rule is then dropped. The
/// failure is silent — `add_rule` is best-effort everywhere else in this file —
/// so the path simply stays unreachable and the user, who allowed it a moment
/// ago, gets `Permission denied`. That is the same class of bug as the one the
/// reach machinery exists to fix, one layer down.
pub fn file_rights(abi: i32) -> u64 {
    let mut r = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE;
    if abi >= 3 {
        r |= FS_TRUNCATE;
    }
    if abi >= 5 {
        r |= FS_IOCTL_DEV;
    }
    r
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Highest ABI the running kernel supports, or 0 when Landlock is unavailable.
pub fn abi() -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc < 0 {
        0
    } else {
        rc as i32
    }
}

/// Every filesystem right this ABI level knows about. Rights the kernel does not
/// know are left out — asking for them is an `EINVAL`, not a stricter policy.
pub fn all_fs_rights(abi: i32) -> u64 {
    let mut r = FS_EXECUTE
        | FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM;
    if abi >= 2 {
        r |= FS_REFER;
    }
    if abi >= 3 {
        r |= FS_TRUNCATE;
    }
    if abi >= 5 {
        r |= FS_IOCTL_DEV;
    }
    r
}

pub struct Ruleset {
    fd: OwnedFd,
}

impl Ruleset {
    /// `abi` decides how much of the attribute the kernel is told about.
    pub fn new(abi: i32, handled_fs: u64, scoped: u64) -> std::io::Result<Ruleset> {
        let attr = RulesetAttr {
            handled_access_fs: handled_fs,
            handled_access_net: 0,
            scoped,
        };
        let fd = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const RulesetAttr,
                attr_size(abi),
                0u32,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Ruleset {
            fd: unsafe { OwnedFd::from_raw_fd(fd as RawFd) },
        })
    }

    /// Grant `access` on everything beneath `path`. A path that does not exist is
    /// skipped rather than failing the whole policy — `~/.cargo` simply may not
    /// be there yet.
    pub fn allow(&self, path: &Path, access: u64) -> std::io::Result<bool> {
        let c = match CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        let dir_fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if dir_fd < 0 {
            return Ok(false);
        }
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: dir_fd,
        };
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                self.fd_raw(),
                LANDLOCK_RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        unsafe { libc::close(dir_fd) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(true)
    }

    pub fn fd_raw(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

/// Apply a prepared ruleset to the **calling thread**, permanently.
///
/// Called from the child side of a spawn, after `fork` and before `exec`, so the
/// engine itself is never restricted — only what the agent runs is. Everything
/// here is a bare syscall: no allocation, no locks, nothing that is unsafe to do
/// between fork and exec.
pub fn restrict_calling_thread(ruleset_fd: RawFd) -> std::io::Result<()> {
    // Landlock requires this, and it is worth having on its own: it stops a
    // setuid binary from handing back the privileges we just dropped.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_is_reported_not_guessed() {
        // Any kernel answer is valid; what matters is that we never claim a level
        // we did not ask for.
        let v = abi();
        assert!(v >= 0);
    }

    #[test]
    fn rights_grow_with_abi_and_never_shrink() {
        assert!(all_fs_rights(1) & FS_REFER == 0);
        assert!(all_fs_rights(2) & FS_REFER != 0);
        assert!(all_fs_rights(3) & FS_TRUNCATE != 0);
        assert!(all_fs_rights(5) & FS_IOCTL_DEV != 0);
        assert!(all_fs_rights(1) & all_fs_rights(5) == all_fs_rights(1));
    }

    #[test]
    fn scopes_appear_only_on_abi_six() {
        assert_eq!(all_scopes(5), 0);
        assert_eq!(all_scopes(6), SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL);
    }

    #[test]
    fn attribute_size_tracks_the_abi() {
        assert_eq!(attr_size(1), 8);
        assert_eq!(attr_size(5), 8);
        assert_eq!(attr_size(6), 24);
    }

    #[test]
    fn ruleset_builds_when_supported() {
        let v = abi();
        if v == 0 {
            return;
        }
        let rs = Ruleset::new(v, all_fs_rights(v), all_scopes(v)).unwrap();
        assert!(rs.allow(Path::new("/tmp"), all_fs_rights(v)).unwrap());
        // A path that does not exist is skipped, not fatal.
        assert!(!rs
            .allow(Path::new("/nonexistent-rune-test"), FS_READ_FILE)
            .unwrap());
    }
}
