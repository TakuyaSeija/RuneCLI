//! What tests observe of Windows, and the situations they set up — never
//! part of a build that ships (`test-support`, or this crate's own tests).
//! The same names and meanings as every OS's fixture.

use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, DENY_ACCESS, EXPLICIT_ACCESS_W,
    NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DELETE_CHILD, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::security::current_user;
use super::wide::{check, from_wide, owned, wide_path, win32, Local};
use crate::platform::{FileIdentity, Handed};

/// Where a process stands, as the kernel says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Running: alive.
    Live,
    /// Ended and not yet reaped by Rune, which keeps it by pid.
    Zombie,
}

/// `pid`'s state; `None` once it is gone (reaped, or never Rune's and
/// ended).
pub fn process_state(pid: i32) -> Option<ProcessState> {
    match super::process::kept_ended(pid) {
        Some(true) => Some(ProcessState::Zombie),
        Some(false) => Some(ProcessState::Live),
        None => super::process::alive(pid).then_some(ProcessState::Live),
    }
}

/// The process `pid`'s parent; `None` once it is gone.
pub fn parent_of(pid: i32) -> Option<u32> {
    let snapshot = owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }).ok()?;
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return None;
    }
    loop {
        if entry.th32ProcessID == pid as u32 {
            return super::process::alive(pid).then_some(entry.th32ParentProcessID);
        }
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 {
            return None;
        }
    }
}

/// Whether `pid` runs.
pub fn running(pid: i32) -> bool {
    process_state(pid) == Some(ProcessState::Live)
}

/// The program file process `pid` runs.
pub fn program_of(pid: u32) -> io::Result<FileIdentity> {
    let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
    let mut name = vec![0u16; 32_768];
    let mut len = name.len() as u32;
    check(unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            name.as_mut_ptr(),
            &mut len,
        )
    })?;
    FileIdentity::of_path(Path::new(&from_wide(&name[..len as usize])))
}

/// A symbolic link at `link` pointing to `target` — a folder link when the
/// target is a folder (Windows tells the two apart).
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let target = target.as_ref();
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.as_ref()
            .parent()
            .map_or(target.to_path_buf(), |parent| parent.join(target))
    };
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// The number `file` has open, as a handover record names it.
pub fn handed_number(file: &std::fs::File) -> Handed {
    Handed(file.as_raw_handle() as i64)
}

/// Whether `handed` would be inherited.
pub fn kept_across_exec(handed: Handed) -> bool {
    let mut flags = 0u32;
    let asked = unsafe { GetHandleInformation(handed.0 as _, &mut flags) };
    asked != 0 && flags & HANDLE_FLAG_INHERIT != 0
}

/// Access to a path taken away for a test — reading when `mode` has no read
/// bit, writing (and deleting what it holds) when it has no write bit — by an
/// entry denying this user, given back when this is dropped, a panic part way
/// included.
#[must_use = "access comes back when the guard is dropped"]
pub struct Restricted {
    path: std::path::PathBuf,
    descriptor: Local<std::ffi::c_void>,
    dacl: *mut ACL,
}

impl Restricted {
    pub fn new(path: &Path, mode: u32) -> io::Result<Restricted> {
        let name = wide_path(path)?;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        win32(unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        })?;
        let had = Restricted {
            path: path.to_path_buf(),
            descriptor: Local(descriptor),
            dacl,
        };
        let mut denied = 0u32;
        if mode & 0o222 == 0 {
            denied |= (FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD) & !FILE_GENERIC_READ;
        }
        if mode & 0o444 == 0 {
            denied |= FILE_GENERIC_READ & !windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        }
        if denied != 0 {
            let user = current_user()?;
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: denied,
                grfAccessMode: DENY_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: user.as_psid() as windows_sys::core::PWSTR,
                },
            };
            let mut updated: *mut ACL = std::ptr::null_mut();
            win32(unsafe { SetEntriesInAclW(1, &entry, had.dacl, &mut updated) })?;
            let updated = Local(updated);
            win32(unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    updated.0,
                    std::ptr::null(),
                )
            })?;
        }
        Ok(had)
    }
}

impl Drop for Restricted {
    fn drop(&mut self) {
        let Ok(name) = wide_path(&self.path) else {
            return;
        };
        unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.dacl,
                std::ptr::null(),
            )
        };
        let _ = &self.descriptor;
    }
}

/// Let every user of this machine read `path`: a file someone loosened by
/// hand (an entry for Users, as Windows's own sharing dialog writes).
pub fn share_with_others(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Security::WinBuiltinUsersSid;
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
    let users = super::security::Sid::well_known(WinBuiltinUsersSid)?;
    let folder = std::fs::metadata(path)?.is_dir();
    super::security::grant(
        path,
        &users,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        folder,
    )
    .map(drop)
}

/// Set when `path` — a file or a folder — was last modified. A folder is
/// opened as one (`FILE_FLAG_BACKUP_SEMANTICS`), which `File::open` cannot.
pub fn set_modified(path: &Path, time: std::time::SystemTime) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
    };
    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .set_modified(time)
}

/// A POSIX shell a test can run scripts in: Git for Windows's bash, where
/// it is installed (found beside the `git` on `PATH`, else where its
/// installer puts it); `None` on a Windows without it.
pub fn posix_shell() -> Option<std::path::PathBuf> {
    let beside_git = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| dir.join("git.exe").is_file())
        .filter_map(|dir| dir.parent().map(|git| git.join(r"bin\bash.exe")));
    let installed = ["ProgramFiles", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|root| std::path::PathBuf::from(root).join(r"Git\bin\bash.exe"));
    beside_git.chain(installed).find(|bash| bash.is_file())
}

/// A file every write to fails, as on a full disk: one open for reading only.
pub fn failing_file() -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rune-failing-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, b"")?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

/// Windows sends no signal when a console's window changes size: a console
/// is resized by its owner (`Pty::resize`), which its programs then read.
pub fn signal_resize(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows has no resize signal; resize the console instead",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_test_is_its_own_program() {
        let me = program_of(std::process::id()).unwrap();
        super::super::process::at_start();
        assert_eq!(me, crate::platform::running_program().unwrap());
    }

    #[test]
    fn every_write_to_a_failing_file_fails() {
        use std::io::Write;
        let mut file = failing_file().unwrap();
        assert!(file.write_all(b"x").is_err());
    }

    #[test]
    fn restricted_access_comes_back_even_after_a_panic() {
        let dir = std::env::temp_dir().join(format!("rune-restricted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let failed = std::panic::catch_unwind(|| {
            let _held = Restricted::new(&dir, 0o500).unwrap();
            assert!(std::fs::write(dir.join("new"), "x").is_err(), "no writing");
            panic!("the test failed part way");
        });
        assert!(failed.is_err());
        std::fs::write(dir.join("new"), "x").expect("writable again");
        std::fs::remove_dir_all(&dir).expect("removable again");
    }
}
