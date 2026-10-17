//! What the integration tests share: how their POSIX scripts run on every
//! OS. A test file uses what it needs of it.
#![allow(dead_code)]

/// The scripts these tests run are POSIX. On Windows, where `auto` is
/// PowerShell, they run in Git Bash (`platform::fixture::posix_shell`): what
/// is tested is Rune's terminal on Windows — ConPTY, jobs, confinement — not
/// PowerShell reading bash. The Windows shells have tests of their own.
pub fn posix_shell_for_tests(data: &std::path::Path) {
    if runeengine::platform::OS != runeengine::platform::Os::Windows {
        return;
    }
    if let Some(shell) = runeengine::platform::fixture::posix_shell() {
        runeengine::settings::configuration::Configuration::edit(
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
    use runeengine::platform::Os;
    match runeengine::platform::OS {
        Os::Linux => Some(format!("setsid {command}")),
        Os::MacOs => Some(format!(
            "perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' {command}"
        )),
        Os::Windows => None,
    }
}
