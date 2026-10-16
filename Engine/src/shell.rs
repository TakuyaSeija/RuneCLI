//! Which language a command line is written in, and how it is handed to the
//! shell that reads it.
//!
//! The shell is the person's choice (`terminal.shell`) or the platform's
//! default (bash or `/bin/sh`; on Windows PowerShell 7, else Windows
//! PowerShell, else `cmd.exe`), and the model is told which in `<status>`.
//! Everything that reads a command line before it runs — the risk rules
//! (`perm::denylist`), the paths it names (`perm::reach`) — reads it in the
//! language of the shell that will run it, never as bash by default.

use std::path::Path;

use crate::platform::Launch;

/// The language a command line is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// `sh`, `bash`, `zsh`, `dash`, `ksh`, `fish` — and Git Bash on Windows.
    Posix,
    /// PowerShell 7 (`pwsh`) or Windows PowerShell (`powershell`).
    PowerShell,
    /// `cmd.exe`.
    Cmd,
}

impl ShellKind {
    /// The language the shell at `program` reads, by its name.
    pub fn of(program: &Path) -> ShellKind {
        // The last name, after either separator: a Windows path read on any
        // machine names the same shell.
        let text = program.to_string_lossy().to_lowercase();
        let file = text.rsplit(['/', '\\']).next().unwrap_or(&text);
        let name = file.strip_suffix(".exe").unwrap_or(file);
        match name {
            "pwsh" | "powershell" | "pwsh-preview" => ShellKind::PowerShell,
            "cmd" => ShellKind::Cmd,
            _ => ShellKind::Posix,
        }
    }

    /// Its name, as the person and the model know it.
    pub fn name(self) -> &'static str {
        match self {
            ShellKind::Posix => "POSIX shell",
            ShellKind::PowerShell => "PowerShell",
            ShellKind::Cmd => "cmd",
        }
    }
}

/// What PowerShell runs around a command: output in UTF-8 (a console's own
/// code page would garble every name that is not ASCII), the last exit code
/// cleared — then the command — then its exit code reported the way a POSIX
/// shell reports it: the native program's own number when the last statement
/// was one that failed, 1 for any other failure, and nothing when it
/// succeeded (PowerShell would say 1 for every failure).
fn powershell_script(command: &str) -> String {
    format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
         $OutputEncoding = [Console]::OutputEncoding\n\
         $global:LASTEXITCODE = $null\n\
         {command}\n\
         if (-not $?) {{ if ($LASTEXITCODE -is [int] -and $LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; exit 1 }}\n"
    )
}

/// The longest command line Windows starts a program with, in UTF-16 units.
const WINDOWS_COMMAND_LINE: usize = 32_767;

/// `command` handed to the shell at `program`, as that shell takes one:
///
/// - a POSIX shell: `-c <command>`;
/// - PowerShell: `-NoLogo -NoProfile -EncodedCommand <base64>` — the script
///   encoded whole, so no quoting of the command line can change it; no
///   profile, as bash reads no rc file for `-c`; prompts it asks stay
///   answerable, as on Linux;
/// - cmd: `/D /S /C "<command>"` written out as is — cmd reads its own
///   command line, so no argument quoting may touch it; `/D` skips the
///   AutoRun commands of the registry, `/S` takes the outer quotes off. A
///   command of several lines runs them one after another (`&`), as a batch
///   file would.
pub fn launch(program: &Path, command: &str) -> crate::Result<Launch> {
    let launch = Launch::new(program);
    Ok(match ShellKind::of(program) {
        ShellKind::Posix => launch.arg("-c").arg(command),
        ShellKind::PowerShell => {
            use base64::Engine;
            let utf16: Vec<u8> = powershell_script(command)
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
            if encoded.len() + 64 > WINDOWS_COMMAND_LINE {
                return Err(crate::Error::invalid(format!(
                    "this command is too long for PowerShell's command line ({} characters); \
                     write it to a script file in the workspace and run that",
                    command.chars().count()
                )));
            }
            launch
                .args(["-NoLogo", "-NoProfile", "-EncodedCommand"])
                .arg(encoded)
        }
        ShellKind::Cmd => {
            let joined = command
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" & ");
            if joined.chars().count() + 32 > WINDOWS_COMMAND_LINE {
                return Err(crate::Error::invalid(
                    "this command is too long for cmd's command line; write it to a batch \
                     file in the workspace and run that",
                ));
            }
            launch
                .args(["/D", "/S", "/C"])
                .verbatim_tail(format!("\"{joined}\""))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn a_shell_is_known_by_its_name() {
        for (path, kind) in [
            ("/bin/bash", ShellKind::Posix),
            ("/usr/bin/zsh", ShellKind::Posix),
            (r"C:\Program Files\Git\bin\bash.exe", ShellKind::Posix),
            (
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                ShellKind::PowerShell,
            ),
            ("/usr/local/bin/pwsh", ShellKind::PowerShell),
            (
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                ShellKind::PowerShell,
            ),
            (r"C:\Windows\System32\cmd.exe", ShellKind::Cmd),
            (r"C:\Windows\System32\CMD.EXE", ShellKind::Cmd),
        ] {
            assert_eq!(ShellKind::of(Path::new(path)), kind, "{path}");
        }
    }

    #[test]
    fn a_posix_shell_takes_the_command_after_dash_c() {
        let launch = launch(Path::new("/bin/bash"), "echo hi && ls").unwrap();
        assert_eq!(launch.args, ["-c", "echo hi && ls"].map(OsString::from));
        assert_eq!(launch.verbatim_tail, None);
    }

    /// PowerShell gets the command whole, encoded, with the preamble and the
    /// exit-code epilogue around it — nothing a quote in it could change.
    #[test]
    fn powershell_gets_the_command_encoded_whole() {
        let command = "Get-ChildItem \"a b\" | Select-Object -First 3; git status";
        let launch = launch(Path::new(r"C:\pwsh\pwsh.exe"), command).unwrap();
        assert_eq!(
            launch.args[..3],
            ["-NoLogo", "-NoProfile", "-EncodedCommand"].map(OsString::from)
        );
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(launch.args[3].to_str().unwrap())
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(script.contains(&format!("\n{command}\n")), "{script}");
        assert!(script.starts_with("[Console]::OutputEncoding"));
        assert!(script.trim_end().ends_with("exit 1 }"));
        let long = "x".repeat(20_000);
        assert!(super::launch(Path::new(r"C:\pwsh\pwsh.exe"), &long).is_err());
    }

    /// cmd gets its own command line, written out, lines run in order.
    #[test]
    fn cmd_gets_its_command_line_written_out() {
        let launch = launch(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            "cd sub\r\n\r\ndir /b \"a & b\"",
        )
        .unwrap();
        assert_eq!(launch.args, ["/D", "/S", "/C"].map(OsString::from));
        assert_eq!(
            launch.verbatim_tail,
            Some(OsString::from("\"cd sub & dir /b \"a & b\"\""))
        );
    }
}
