//! Where this machine keeps the person's things.

use std::path::PathBuf;

/// The person's home folder: `$HOME` when it names an absolute path.
///
/// An empty or relative `$HOME` is no home: joined with `.rune` it named a
/// folder under whatever directory Rune was started in — possibly a project
/// the agent may write, holding the grants and keys of the very process it
/// should be kept out of.
pub fn home_dir() -> Option<PathBuf> {
    home_from(std::env::var_os("HOME"))
}

fn home_from(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// The shell commands run in when the person chose none: bash where this
/// machine has it on its `PATH` (NixOS keeps it outside `/bin`), else the
/// POSIX shell every Linux has.
pub fn default_shell() -> PathBuf {
    shell_on(std::env::var_os("PATH"))
}

fn shell_on(path: Option<std::ffi::OsString>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    path.map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("bash"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner, 2026-10-16: `/bin/bash` is not on every Linux.
    #[test]
    fn the_default_shell_is_bash_on_path_else_the_posix_shell() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rune-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nix/bin")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        let bash = dir.join("nix/bin/bash");
        std::fs::write(&bash, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bash, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([dir.join("empty"), dir.join("nix/bin")]).unwrap();
        assert_eq!(shell_on(Some(path)), bash);
        let none = std::env::join_paths([dir.join("empty"), "relative".into()]).unwrap();
        assert_eq!(shell_on(Some(none)), PathBuf::from("/bin/sh"));
        assert_eq!(shell_on(None), PathBuf::from("/bin/sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty or relative `$HOME` names no home — never a folder under
    /// the directory Rune happened to start in.
    #[test]
    fn only_an_absolute_home_is_a_home() {
        assert_eq!(
            home_from(Some("/home/dev".into())),
            Some(PathBuf::from("/home/dev"))
        );
        assert_eq!(home_from(Some("".into())), None);
        assert_eq!(home_from(Some("dev".into())), None);
        assert_eq!(home_from(None), None);
    }
}
