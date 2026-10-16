//! What macOS tells of processes — through `sysctl` and `libproc`, its
//! kernel's word as `/proc` is Linux's — and of the file this program runs
//! from, which macOS names by path only (no `/proc/self/exe`).

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::platform::{hand_on, FileIdentity, Handed};

/// Every process id the kernel lists now.
pub(super) fn all_pids() -> Vec<libc::pid_t> {
    listed(|buffer, size| unsafe { libc::proc_listallpids(buffer, size) })
}

/// The processes of process group `group`.
pub(super) fn group_members(group: libc::pid_t) -> Vec<libc::pid_t> {
    /// `PROC_PGRP_ONLY` from `<sys/proc_info.h>`: the pids of one group.
    const PROC_PGRP_ONLY: u32 = 2;
    listed(|buffer, size| unsafe {
        libc::proc_listpids(PROC_PGRP_ONLY, group as u32, buffer, size)
    })
}

/// A pid list from `list(buffer, bytes)`, which answers how many pids (or
/// bytes, for `proc_listpids`) it wrote — asked again with room to spare when
/// processes started meanwhile.
fn listed(list: impl Fn(*mut c_void, libc::c_int) -> libc::c_int) -> Vec<libc::pid_t> {
    let mut room = 1024usize;
    loop {
        let mut pids = vec![0 as libc::pid_t; room];
        let bytes = (room * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let answer = list(pids.as_mut_ptr().cast(), bytes);
        if answer <= 0 {
            return Vec::new();
        }
        // `proc_listallpids` counts pids, `proc_listpids` bytes: both are
        // read as "at most this many pids", the zeros dropped.
        let written = (answer as usize).min(room);
        if written < room {
            pids.truncate(written);
            pids.retain(|pid| *pid > 0);
            return pids;
        }
        room *= 2;
    }
}

/// Every live process whose environment holds `entry` (`NAME=value`) —
/// readable for this user's own processes (`KERN_PROCARGS2`) — never this
/// process itself. A process that has ended lists no environment.
pub(in crate::platform) fn tree_members(entry: &str) -> Vec<libc::pid_t> {
    let me = unsafe { libc::getpid() };
    let mut buffer = vec![0u8; argument_room()];
    all_pids()
        .into_iter()
        .filter(|pid| *pid != me)
        .filter(|pid| environment_holds(*pid, entry.as_bytes(), &mut buffer))
        .collect()
}

/// How many bytes a process's arguments and environment may take.
fn argument_room() -> usize {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut room: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut room as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && room > 0 {
        room as usize
    } else {
        1 << 20
    }
}

/// Whether `pid`'s environment holds `entry`. The kernel's record is `argc`,
/// the executable's path, padding, `argc` arguments, then the environment —
/// each NUL-terminated.
fn environment_holds(pid: libc::pid_t, entry: &[u8], buffer: &mut [u8]) -> bool {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = buffer.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return false;
    }
    environment(&buffer[..size]).any(|held| held == entry)
}

/// The environment entries of a `KERN_PROCARGS2` record.
fn environment(record: &[u8]) -> impl Iterator<Item = &[u8]> {
    let argc = record
        .get(..4)
        .map(|n| i32::from_ne_bytes([n[0], n[1], n[2], n[3]]).max(0) as usize)
        .unwrap_or(0);
    let mut rest = record.get(4..).unwrap_or(&[]);
    // The executable's path, then the NULs that pad it.
    let path_end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    rest = &rest[path_end..];
    let start = rest.iter().position(|b| *b != 0).unwrap_or(rest.len());
    rest = &rest[start..];
    let mut strings = rest.split(|b| *b == 0);
    for _ in 0..argc {
        strings.next();
    }
    strings.take_while(|s| !s.is_empty())
}

/// What `proc_pidinfo` tells of `pid` (`PROC_PIDTBSDINFO`); `None` once it is
/// gone or when it is not this user's to ask.
pub(super) fn bsd_info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size).then_some(info)
}

/// When this machine started: nothing a process of an earlier boot left can
/// still be in use. `None` when the system does not say.
pub fn boot_time() -> Option<std::time::SystemTime> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut when: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::timeval>();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut when as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && when.tv_sec > 0).then(|| {
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(when.tv_sec as u64)
            + std::time::Duration::from_micros(when.tv_usec.max(0) as u64)
    })
}

/// When process `pid` began, in microseconds since 1970: with the pid, the
/// process's identity — a number the kernel hands out again later never
/// comes back with the same start. `None` once it is gone.
pub fn process_started(pid: u32) -> Option<u64> {
    let info = bsd_info(libc::pid_t::try_from(pid).ok()?)?;
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// The file this process was started from, opened when it started
/// ([`remember_own_program`]): macOS names a running program only by path,
/// and a release may put another file at that path while it runs.
static OWN_PROGRAM: OnceLock<Option<OwnedFd>> = OnceLock::new();

fn own() -> Option<&'static OwnedFd> {
    OWN_PROGRAM
        .get_or_init(|| {
            File::open(std::env::current_exe().ok()?)
                .ok()
                .map(OwnedFd::from)
        })
        .as_ref()
}

/// Called as this process starts (`platform::at_start`): open the file it
/// runs from while that is still the one at its path, and remove the copy it
/// may run from — the way back an earlier handover wrote ([`program_at`]).
pub(super) fn remember_own_program() {
    let _ = own();
    if let Ok(exe) = std::env::current_exe() {
        if is_way_back_copy(&exe) {
            let _ = std::fs::remove_file(exe);
        }
    }
}

/// A descriptor on the program this process runs, open across `exec`: the
/// way back to this build for a successor that cannot take the home over.
pub fn own_program() -> io::Result<Handed> {
    let own = own().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "this program's own file was never opened",
        )
    })?;
    hand_on(File::from(own.try_clone()?))
}

/// The path that runs the program `handed` holds ([`own_program`]), when it
/// is one. macOS cannot run a program from a descriptor: it is copied, only
/// now that it is needed, to a file of this user's alone in this user's
/// private temporary folder — which the program removes as it starts
/// ([`remember_own_program`]); a signed program stays signed, the signature
/// being inside the file.
pub fn program_at(handed: Handed) -> io::Result<PathBuf> {
    let held = std::mem::ManuallyDrop::new(unsafe {
        <File as std::os::fd::FromRawFd>::from_raw_fd(handed.0)
    });
    let meta = held.metadata().map_err(|_| {
        io::Error::new(io::ErrorKind::NotFound, "no previous program was handed on")
    })?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "what was handed on is not a program",
        ));
    }
    let path =
        way_back_folder()?.join(format!("{WAY_BACK}{}-{:x}", std::process::id(), meta.ino()));
    let _ = std::fs::remove_file(&path);
    let mut copy = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)?;
    let source = held.try_clone()?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    loop {
        let n = unsafe {
            libc::pread(
                source.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                offset as libc::off_t,
            )
        };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        if n == 0 {
            break;
        }
        copy.write_all(&buffer[..n as usize])?;
        offset += n as u64;
    }
    copy.sync_all()?;
    drop(source);
    Ok(path)
}

/// The name a way-back copy begins with.
const WAY_BACK: &str = "rune-way-back-";

/// This user's private temporary folder (`_CS_DARWIN_USER_TEMP_DIR`), which
/// no other user may enter — never the shared `/tmp`.
fn way_back_folder() -> io::Result<PathBuf> {
    let mut buffer = vec![0u8; 1024];
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if n == 0 || n > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "macOS named no private temporary folder for this user",
        ));
    }
    buffer.truncate(n - 1);
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

fn is_way_back_copy(exe: &Path) -> bool {
    exe.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(WAY_BACK))
        && way_back_folder()
            .ok()
            .and_then(|folder| folder.canonicalize().ok())
            .zip(exe.parent().and_then(|p| p.canonicalize().ok()))
            .is_some_and(|(folder, parent)| folder == parent)
}

/// The file this process was started from, whatever is installed at its
/// path now — compared with [`FileIdentity::of_path`] of the installed path,
/// it tells that a release replaced it.
pub fn running_program() -> io::Result<FileIdentity> {
    match own() {
        Some(own) => FileIdentity::of_file(&File::from(own.try_clone()?)),
        None => FileIdentity::of_path(&std::env::current_exe()?),
    }
}

/// The path this process's program is installed at — where a release puts
/// the next build.
pub fn installed_program() -> io::Result<PathBuf> {
    std::env::current_exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_program_is_the_installed_one_until_replaced() {
        remember_own_program();
        let running = running_program().unwrap();
        let installed = installed_program().unwrap();
        assert_eq!(FileIdentity::of_path(&installed).unwrap(), running);
    }

    /// The environment of a `KERN_PROCARGS2` record is read past the path,
    /// its padding and the arguments — never an argument mistaken for it.
    #[test]
    fn a_process_record_is_read_to_its_environment() {
        let mut record = 2i32.to_ne_bytes().to_vec();
        record.extend_from_slice(
            b"/bin/sh\0\0\0\0sh\0RUNE_PROCESS_TREE=decoy\0A=1\0RUNE_PROCESS_TREE=m\0\0rest",
        );
        let env: Vec<&[u8]> = environment(&record).collect();
        assert_eq!(env, vec![&b"A=1"[..], &b"RUNE_PROCESS_TREE=m"[..]]);
    }

    /// A process this user started is found by its mark; a process carrying
    /// another mark is not.
    #[test]
    fn a_process_is_found_by_its_mark() {
        let mut marked = std::process::Command::new("/bin/sleep")
            .arg("30")
            .env("RUNE_PROCESS_TREE", "rune-test-mark")
            .spawn()
            .unwrap();
        let mut other = std::process::Command::new("/bin/sleep")
            .arg("30")
            .env("RUNE_PROCESS_TREE", "someone-else")
            .spawn()
            .unwrap();
        let found = tree_members("RUNE_PROCESS_TREE=rune-test-mark");
        assert_eq!(found, vec![marked.id() as libc::pid_t]);
        assert!(process_started(marked.id()).is_some());
        assert!(boot_time().is_some());
        let _ = marked.kill();
        let _ = other.kill();
        let _ = marked.wait();
        let _ = other.wait();
    }

    /// The way back to a build is a private copy of exactly its bytes.
    #[test]
    fn the_way_back_is_a_private_copy_of_the_program() {
        remember_own_program();
        let handed = own_program().unwrap();
        let path = program_at(handed).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let copied = std::fs::read(&path).unwrap();
        let original = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(copied == original, "the copy is the program, byte for byte");
        assert!(is_way_back_copy(&path));
        std::fs::remove_file(&path).unwrap();
        handed.close();
    }
}
