//! What tests share across the workspace — the engine's own, its
//! integration tests and the CLI's: how their POSIX scripts run on every OS,
//! and the programs their fixtures need. Never part of a build that ships
//! (`test-support`, or this crate's own tests).

/// The scripts these tests run are POSIX. On Windows, where `auto` is
/// PowerShell, they run in Git Bash (`platform::fixture::posix_shell`): what
/// is tested is Rune's terminal on Windows — ConPTY, jobs, confinement — not
/// PowerShell reading bash. The Windows shells have tests of their own.
pub fn posix_shell_for_tests(data: &std::path::Path) {
    if crate::platform::OS != crate::platform::Os::Windows {
        return;
    }
    if let Some(shell) = crate::platform::fixture::posix_shell() {
        crate::settings::configuration::Configuration::edit(
            data,
            Some("terminal.shell"),
            Some(serde_json::json!(shell.to_string_lossy())),
            false,
            None,
        )
        .unwrap();
    }
}

/// A POSIX command starting `command` outside its own process group, as a
/// daemon does: `setsid` on Linux, Perl's `POSIX::setsid` on macOS (no
/// `setsid` command there; every macOS has `/usr/bin/perl`). `None` on
/// Windows, which has no groups to leave — a job holds the whole tree, and
/// the Windows platform's own tests stop it.
pub fn leaving_its_group(command: &str) -> Option<String> {
    use crate::platform::Os;
    match crate::platform::OS {
        Os::Linux => Some(format!("setsid {command}")),
        Os::MacOs => Some(format!(
            "perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' {command}"
        )),
        Os::Windows => None,
    }
}

/// An absolute Python for the MCP fixture (`McpFixture.py`): `/usr/bin/python3`
/// where it is, else the first `python3` (Windows: `python.exe`) on `PATH` —
/// never Windows's Store alias in `WindowsApps`, a stub that opens the Store.
pub fn python() -> std::path::PathBuf {
    let windows = crate::platform::OS == crate::platform::Os::Windows;
    if !windows {
        let usual = std::path::PathBuf::from("/usr/bin/python3");
        if usual.is_file() {
            return usual;
        }
    }
    let names: &[&str] = if windows {
        &["python.exe", "python3.exe"]
    } else {
        &["python3"]
    };
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| dir.is_absolute() && !dir.to_string_lossy().contains("WindowsApps"))
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
        .expect("a Python on this machine for the MCP fixture")
}
