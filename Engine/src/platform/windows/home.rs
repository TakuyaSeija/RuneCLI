//! Where this machine keeps the person's things, and the shell commands run
//! in when the person chose none.

use std::path::{Path, PathBuf};

use windows_sys::core::GUID;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Profile, SHGetKnownFolderPath};

/// The person's home folder: `%USERPROFILE%` when it names an absolute
/// path, else the profile folder Windows keeps for the user. An empty or
/// relative value is no home: joined with `.rune` it would name a folder
/// under wherever Rune was started.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| known_folder(&FOLDERID_Profile))
}

/// Where Windows keeps the folder `id` names (`FOLDERID_…`), as Windows
/// says — never read from a variable anyone may set.
pub(super) fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut path: windows_sys::core::PWSTR = std::ptr::null_mut();
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    if result < 0 || path.is_null() {
        if !path.is_null() {
            unsafe { CoTaskMemFree(path.cast()) };
        }
        return None;
    }
    let mut len = 0;
    while unsafe { *path.add(len) } != 0 {
        len += 1;
    }
    let text = super::wide::from_wide(unsafe { std::slice::from_raw_parts(path, len) });
    unsafe { CoTaskMemFree(path.cast()) };
    Some(PathBuf::from(text)).filter(|p| p.is_absolute())
}

/// No folder of the system's own keeps this user's tool caches beyond what
/// the sandbox names under the profile.
pub fn system_caches() -> Vec<PathBuf> {
    Vec::new()
}

/// The shell commands run in when the person chose none (owner, 2026-10-17):
/// PowerShell 7 where this machine has it on its `PATH`, else the Windows
/// PowerShell every Windows has, else `cmd.exe`.
pub fn default_shell() -> PathBuf {
    shell_on(std::env::var_os("PATH"), &system_root())
}

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn shell_on(path: Option<std::ffi::OsString>, root: &Path) -> PathBuf {
    let pwsh = path
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("pwsh.exe"))
        .find(|candidate| candidate.is_file());
    if let Some(pwsh) = pwsh {
        return pwsh;
    }
    let windows_powershell = root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    if windows_powershell.is_file() {
        return windows_powershell;
    }
    root.join(r"System32\cmd.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_shell_is_powershell_7_on_path_else_windows_powershell() {
        let dir = std::env::temp_dir().join(format!("rune-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ps7")).unwrap();
        std::fs::write(dir.join("ps7").join("pwsh.exe"), "").unwrap();
        let root = system_root();
        let path = std::env::join_paths([dir.join("empty"), dir.join("ps7")]).unwrap();
        assert_eq!(
            shell_on(Some(path), &root),
            dir.join("ps7").join("pwsh.exe")
        );
        let fallback = shell_on(Some("relative".into()), &root);
        assert!(
            fallback.ends_with(r"WindowsPowerShell\v1.0\powershell.exe")
                || fallback.ends_with(r"System32\cmd.exe"),
            "{}",
            fallback.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_home_is_an_absolute_folder() {
        let home = home_dir().expect("every Windows user has a profile");
        assert!(home.is_absolute());
    }
}
