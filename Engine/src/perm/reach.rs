//! Paths a call names that lie **outside** the workspace.
//!
//! # The bug this file exists to close
//!
//! Confinement had two independent gates and only one of them could be answered.
//! [`super::Gate`] asks a human; [`crate::sandbox`] asks the kernel. A user who
//! allowed `ls ~/Downloads` at the prompt watched it fail anyway with
//! `Permission denied`, because an allow moved the first gate and the second one
//! had never heard of the question. An allow that does not widen what it was
//! asked about is not an allow, it is a lie with a keystroke.
//!
//! So a call now declares the outside paths it names ([`crate::tools::Tool::reaches`]),
//! the gate asks about **those paths by name**, and an allow widens the jail and
//! the kernel ruleset together for exactly as long as the answer covers.
//!
//! # It is a scanner, not a shell
//!
//! For `terminal_use` the input is a command line, and nothing short of running
//! bash knows every path it will open. This reads the tokens that *look* like
//! paths. Being wrong in one direction offers the user a grant the command did
//! not need — visible, and refusable. Being wrong in the other leaves the
//! command failing exactly as it does today, which is no worse than before.
//! Neither direction can widen anything without a person saying yes.

use crate::jail::normalize;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The `tool` column that persisted path grants are filed under.
///
/// `@`-prefixed so it can never collide with a real tool name, present or
/// future — the table is keyed `(project, tool, pattern)` and a tool called
/// `reach` would otherwise share rows with these.
pub const GRANT_TOOL: &str = "@reach";

/// Shell punctuation that rides on a token when a command is split by
/// whitespace alone. `~/Downloads;` is the same path as `~/Downloads`.
const EDGE: &[char] = &[';', '&', '|', '(', ')', '<', '>', ',', '\'', '"', '`'];

/// Path-shaped tokens in a command line written for a shell of `kind`:
/// bash and its kin as below, PowerShell and cmd as they read a line
/// (`windows_shells::paths`: a backslash is a separator there).
pub fn in_command_for(command: &str, kind: crate::shell::ShellKind) -> Vec<String> {
    match kind {
        crate::shell::ShellKind::Posix => in_command(command),
        other => super::windows_shells::paths(command, other),
    }
}

/// Path-shaped tokens in one bash command line.
///
/// Deliberately generous about what counts and strict about what it returns:
/// anything absolute or `~`-rooted is a candidate, everything else is
/// workspace-relative by construction and the jail already anchors it.
pub fn in_command(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tokens in super::denylist::segment_tokens(command) {
        let program = awk_program_operand(&tokens);
        for (index, raw) in tokens.iter().enumerate() {
            // awk's first operand is a program, not a filename. In particular
            // `/MemTotal/ { printf ... }` must not ask to lift the jail at `/`.
            // Real files (-f, inputs, output redirection) are still scanned.
            if Some(index) == program {
                continue;
            }
            let token = raw.trim_matches(EDGE);
            if !is_path_shaped(token) {
                continue;
            }
            let token = token.to_string();
            if !out.contains(&token) {
                out.push(token);
            }
        }
    }
    out
}

fn awk_program_operand(tokens: &[String]) -> Option<usize> {
    let executable = tokens.iter().position(|token| {
        !(token.contains('=') && !token.starts_with('-') && !token.contains('/'))
    })?;
    if !matches!(
        super::denylist::program_name(&tokens[executable]).as_str(),
        "awk" | "gawk" | "mawk" | "nawk"
    ) {
        return None;
    }
    let mut index = executable + 1;
    let mut from_file = false;
    while let Some(token) = tokens.get(index) {
        match token.as_str() {
            "-f" | "--file" => {
                from_file = true;
                index += 2;
            }
            "-e" | "--source" => return Some(index + 1).filter(|i| *i < tokens.len()),
            "-v" | "-F" | "--field-separator" | "--assign" => index += 2,
            "--" => {
                return (!from_file)
                    .then_some(index + 1)
                    .filter(|i| *i < tokens.len())
            }
            _ if token.starts_with("-f") || token.starts_with("--file=") => {
                from_file = true;
                index += 1;
            }
            _ if token.starts_with('-') => index += 1,
            _ => return (!from_file).then_some(index),
        }
    }
    None
}

/// One path-shaped argument, e.g. a tool's `path` field. Relative paths are the
/// workspace's own and are never candidates.
pub fn in_argument(value: &str) -> Option<String> {
    let value = value.trim();
    is_path_shaped(value).then(|| value.to_string())
}

fn is_path_shaped(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // A flag that happens to carry a path (`--out=/tmp/x`) is left alone: the
    // value is picked up when it appears as its own token, and splitting flags
    // here would mean guessing which ones take values.
    token.starts_with('/')
        || token == "~"
        || token.starts_with("~/")
        || (crate::platform::OS == crate::platform::Os::Windows
            && super::windows_shells::path_shaped(token))
}

// ─── What no answer can open ──────────────────────────────────────────────────

/// Paths under `$HOME` that a permission prompt must never be able to open.
///
/// The distinction this list draws is the whole reason it exists. "Allow this
/// command" and "allow this command to read `~/.ssh`" are different questions,
/// and a user answering the first at speed — which is what a user does, on the
/// twentieth prompt of an afternoon — must not find they have answered the
/// second. So these are refusals in confined Ask/Auto sessions: no prompt or
/// stored grant reaches them. Full deliberately bypasses confinement entirely.
///
/// Two kinds, and both are about what happens *after* the session ends:
///
///   * **credentials** — reading one is exfiltration, and the agent has network
///     access. A key read today is a key used against the user tomorrow.
///   * **startup files** — writing one makes the user run the agent's code later,
///     outside any sandbox, without knowing they are doing it.
const FORBIDDEN_IN_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".pgpass",
    ".config/gh",
    ".config/gcloud",
    ".config/op",
    ".password-store",
    // Browser profiles: session cookies are credentials with a friendlier name.
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    // Anything that runs on next login, or on the next build.
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_aliases",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".inputrc",
    ".config/fish",
    ".config/systemd/user",
    ".config/autostart",
    ".local/share/systemd/user",
    ".local/bin",
    ".cargo/config.toml",
];

/// The paths no reach grant may cover, on this machine.
#[derive(Debug, Clone, Default)]
pub struct Forbidden {
    roots: Vec<PathBuf>,
}

impl Forbidden {
    pub fn standard(home: Option<&Path>, _engine_dirs: &[PathBuf]) -> Forbidden {
        let mut roots: Vec<PathBuf> = super::denylist::protected_prefixes(crate::platform::OS);
        if let Some(home) = home {
            roots.extend(
                forbidden_in_home(crate::platform::OS)
                    .iter()
                    .map(|p| home.join(p)),
            );
        }
        Forbidden { roots }
    }

    /// The forbidden root this path collides with, if any.
    ///
    /// Collision runs **both ways** on purpose. `~/.ssh/id_ed25519` is inside a
    /// forbidden root; `~/.config` merely contains several — and granting it
    /// would hand over every one of them. A one-directional check would let the
    /// second through.
    pub fn blocks(&self, path: &Path) -> Option<&Path> {
        let os = crate::platform::OS;
        self.roots
            .iter()
            .find(|root| {
                crate::jail::starts_with_folded(path, root, os)
                    || crate::jail::starts_with_folded(root, path, os)
            })
            .map(|p| p.as_path())
    }
}

/// A path the engine is willing to put to the user.
///
/// Nothing here refuses. What used to be two rejections — `$HOME` itself, and
/// anything colliding with a root the machine runs on (`~/.ssh`, `.bashrc`,
/// `/boot`) — is now an annotation: the gate still asks, the card carries the
/// reason, and no answer may be remembered against it. The owner's rule,
/// 2026-09-28: the system never says no on its own; the one autonomous denier
/// is the thinking classifier, and only where a CLASSIFIER.md policy says so.
///
/// What remains is a mechanical fact rather than a judgement: a path whose
/// nearest existing ancestor is the filesystem root, `$HOME` itself, or a
/// shared directory (`/tmp`, `/home`) cannot be anchored as a narrow Landlock
/// rule, so allowing the call means dropping confinement for this call only.
/// The card says so on screen: "allow /" must never disguise itself as an
/// ordinary directory grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// As the user will read it: the path the call actually named.
    pub named: PathBuf,
    /// Where the rule is anchored — `named` itself, or its nearest existing
    /// ancestor when it does not exist yet.
    pub anchor: PathBuf,
    /// This path is too broad for a Landlock reach rule, so allowing the call
    /// means dropping confinement for this call only. Kept explicit all the
    /// way to the permission card.
    pub full_access: bool,
    /// Why the path needs the owner's eyes — credentials, startup files,
    /// system prefixes. The gate asks rather than refusing, and consent is
    /// never stored for a sensitive candidate.
    pub sensitive: Option<String>,
}

/// Directories that are shared with everything else the user runs, or that hold
/// every user on the machine. A rule can be anchored at a **file** inside one —
/// that is narrow and answerable — but never at the directory itself.
///
/// The distinction is what separates "read `/tmp/build.log`" from "write
/// anywhere in the world-writable directory every other program also uses".
const SHARED_ANCHORS: &[&str] = &[
    "/tmp", "/var/tmp", "/dev", "/dev/shm", "/home", "/root", "/mnt", "/media", "/srv",
];

/// The same on `os`: macOS's temporary folders, the folder of every user's
/// home and the mounted volumes; Windows's folder of every user's profile,
/// its public profile and its temporary folders.
fn shared_anchors(os: crate::platform::Os) -> Vec<PathBuf> {
    use crate::platform::Os;
    match os {
        Os::Linux => SHARED_ANCHORS.iter().map(PathBuf::from).collect(),
        Os::MacOs => [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
            "/dev",
            "/Users",
            "/Users/Shared",
            "/Volumes",
        ]
        .iter()
        .map(PathBuf::from)
        .collect(),
        Os::Windows => {
            let here = os == crate::platform::OS;
            let var = |name: &str, usual: &str| {
                here.then(|| std::env::var_os(name))
                    .flatten()
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| PathBuf::from(usual))
            };
            let drive = var("SystemDrive", "C:");
            let mut root = drive.into_os_string();
            root.push("\\");
            let root = PathBuf::from(root);
            vec![
                root.join("Users"),
                var("PUBLIC", r"C:\Users\Public"),
                var("SystemRoot", r"C:\Windows").join("Temp"),
                var("TEMP", r"C:\Windows\Temp"),
            ]
        }
    }
}

/// What under the home no reach may open on `os` ([`FORBIDDEN_IN_HOME`]
/// on Linux): credentials, browser profiles, and what runs at the next
/// login or build — macOS's keychains and launch agents, Windows's saved
/// credentials and data-protection keys, its Startup folder and the
/// profiles PowerShell runs at every start.
fn forbidden_in_home(os: crate::platform::Os) -> &'static [&'static str] {
    use crate::platform::Os;
    const MACOS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".netrc",
        ".pgpass",
        ".config/gh",
        ".config/gcloud",
        ".config/op",
        ".password-store",
        "Library/Keychains",
        "Library/Cookies",
        "Library/Safari",
        "Library/Application Support/Google/Chrome",
        "Library/Application Support/Firefox",
        "Library/Application Support/BraveSoftware",
        "Library/Application Support/Microsoft Edge",
        "Library/Application Support/com.apple.TCC",
        "Library/LaunchAgents",
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".profile",
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".zlogin",
        ".config/fish",
        ".local/bin",
        ".cargo/config.toml",
    ];
    const WINDOWS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".netrc",
        ".git-credentials",
        ".config/gh",
        ".config/gcloud",
        "AppData/Roaming/GitHub CLI",
        "AppData/Roaming/gcloud",
        "AppData/Roaming/gnupg",
        "AppData/Roaming/Microsoft/Credentials",
        "AppData/Local/Microsoft/Credentials",
        "AppData/Roaming/Microsoft/Protect",
        "AppData/Roaming/Microsoft/Crypto",
        "AppData/Roaming/Microsoft/SystemCertificates",
        "AppData/Local/Google/Chrome/User Data",
        "AppData/Local/Microsoft/Edge/User Data",
        "AppData/Local/BraveSoftware",
        "AppData/Roaming/Mozilla",
        "AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup",
        "Documents/PowerShell",
        "Documents/WindowsPowerShell",
        ".cargo/config.toml",
        ".bashrc",
        ".bash_profile",
        ".profile",
    ];
    match os {
        Os::Linux => FORBIDDEN_IN_HOME,
        Os::MacOs => MACOS,
        Os::Windows => WINDOWS,
    }
}

/// Expand `~` and resolve `.`/`..`, without judging anything.
///
/// Split from [`candidate`] so a caller can ask "is this already reachable?"
/// before asking "may this be granted?". Getting that order wrong means
/// refusing `/usr/bin/python3` for being in a protected prefix when every
/// command on the machine already reads it.
///
/// `home` is passed in rather than read here so the tests do not depend on the
/// machine they run on.
pub fn resolve(token: &str, home: Option<&Path>) -> Result<PathBuf> {
    Ok(normalize(&expand(token, home, crate::platform::OS)?))
}

/// Judge an already-resolved path, and work out where a rule for it can be
/// anchored. Never an error and never a refusal — a path the machine runs on
/// its own comes back flagged, and the wizard takes it from there.
pub fn candidate(named: PathBuf, home: Option<&Path>, forbidden: &Forbidden) -> Candidate {
    if let Some(home) = home {
        if named == *home {
            return Candidate {
                named: named.clone(),
                anchor: named,
                full_access: true,
                sensitive: Some(
                    "the whole home directory — allowing it is full access under another name; \
                     name the directory you actually need, or switch the session to Full"
                        .into(),
                ),
            };
        }
    }
    if named.parent().is_none() {
        return Candidate {
            named: named.clone(),
            anchor: named,
            full_access: true,
            sensitive: None,
        };
    }

    // Flagged, not refused: the wizard still puts it to the user, the card
    // says why, and no answer may be remembered.
    let mut sensitive = forbidden.blocks(&named).map(|root| {
        format!(
            "{} holds credentials or files the user's machine runs on its own — an answer \
             given in the middle of other work must not open it",
            root.display()
        )
    });

    let anchor = match nearest_existing(&named) {
        Some(anchor) => anchor,
        // Not even `/` exists above it, so no rule can be anchored anywhere.
        // The only honest shape left is a one-call full access.
        None => {
            return Candidate {
                named: named.clone(),
                anchor: named,
                full_access: true,
                sensitive,
            }
        }
    };
    let os = crate::platform::OS;
    let same = |a: &Path, b: &Path| {
        crate::jail::starts_with_folded(a, b, os) && crate::jail::starts_with_folded(b, a, os)
    };
    if anchor.parent().is_none()
        || home.is_some_and(|h| crate::jail::starts_with_folded(h, &anchor, os))
        || shared_anchors(os).iter().any(|s| same(&anchor, s))
    {
        return Candidate {
            named,
            anchor,
            full_access: true,
            sensitive,
        };
    }
    // A canonical ancestor can cross into forbidden ground that the named path
    // hides behind a symlink, so the anchor is judged too — first reason wins.
    if sensitive.is_none() {
        if let Some(root) = forbidden.blocks(&anchor) {
            sensitive = Some(format!(
                "the closest existing directory to {} is {}, which holds credentials or \
                 protected files — create the directory first, or name a path that exists",
                named.display(),
                root.display()
            ));
        }
    }
    Candidate {
        named,
        anchor,
        full_access: sensitive.is_some(),
        sensitive,
    }
}

fn expand(token: &str, home: Option<&Path>, os: crate::platform::Os) -> Result<PathBuf> {
    let windows = os == crate::platform::Os::Windows;
    // What names the home: `~`, and on Windows also how PowerShell and cmd
    // spell it.
    let lower = token.to_lowercase();
    let names: &[&str] = if windows {
        &["~", "$home", "$env:userprofile", "%userprofile%"]
    } else {
        &["~"]
    };
    for name in names {
        let rest = if lower == *name {
            Some("")
        } else if lower.starts_with(&format!("{name}/"))
            || (windows && lower.starts_with(&format!("{name}\\")))
        {
            Some(&token[name.len() + 1..])
        } else {
            None
        };
        if let Some(rest) = rest {
            let home = home.ok_or_else(|| {
                Error::Denied(format!(
                    "{token} cannot be resolved: this process has no HOME"
                ))
            })?;
            return Ok(if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            });
        }
    }
    // Git Bash on Windows names a drive as `/c/…`.
    if windows {
        let b = token.as_bytes();
        if b.len() >= 2
            && b[0] == b'/'
            && b[1].is_ascii_alphabetic()
            && (b.len() == 2 || b[2] == b'/')
        {
            let drive = format!("{}:\\", (b[1] as char).to_ascii_uppercase());
            return Ok(PathBuf::from(drive).join(token.get(3..).unwrap_or("")));
        }
    }
    Ok(PathBuf::from(token))
}

/// The deepest ancestor of `path` — itself included — that exists on disk.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            // Canonicalised so a symlinked ancestor cannot anchor a rule
            // somewhere other than where the user read that it would.
            return crate::platform::canonical(cur)
                .ok()
                .or_else(|| Some(cur.to_path_buf()));
        }
        cur = cur.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/dev")
    }

    /// The cases that name POSIX paths (`/home/dev`, `/tmp`, `/a/b`) mean them
    /// as Linux and macOS read them. Windows reads `/a/…` as Git Bash's drive
    /// A: and `/tmp` as a folder of the current drive, so there they are left
    /// out and `windows_paths_are_judged_as_windows_reads_them` says the same
    /// with Windows's paths.
    fn posix_paths() -> bool {
        crate::platform::OS != crate::platform::Os::Windows
    }

    #[test]
    fn windows_paths_are_judged_as_windows_reads_them() {
        if posix_paths() {
            return;
        }
        let home = PathBuf::from(r"C:\Users\dev");
        let f = Forbidden::standard(Some(&home), &[]);
        for path in [
            r"C:\Users\dev\.ssh\id_ed25519",
            r"c:\users\DEV\.SSH",
            r"C:\Users\dev\AppData\Roaming\Microsoft\Protect",
            r"C:\Users\dev\Documents\PowerShell\profile.ps1",
            r"C:\Windows\System32\drivers\etc\hosts",
        ] {
            assert!(
                f.blocks(Path::new(path)).is_some(),
                "{path} must never be granted quietly"
            );
        }
        assert!(f.blocks(Path::new(r"C:\Users\dev\Downloads")).is_none());
        assert!(
            f.blocks(Path::new(r"C:\Users\dev")).is_some(),
            "the profile holds what is forbidden"
        );
        assert_eq!(
            resolve(r"C:\a\b\..\c", None).unwrap(),
            PathBuf::from(r"C:\a\c")
        );
        let c = judge(r"~\.ssh\id_rsa", Some(&home)).unwrap();
        assert!(c.sensitive.as_deref().unwrap_or("").contains(".ssh"));
    }

    /// On Windows the home has PowerShell's and cmd's names too, and Git
    /// Bash's `/c/…` is drive C:; elsewhere they are what they say.
    #[test]
    fn the_home_and_a_drive_are_named_as_each_shell_names_them() {
        use crate::platform::Os;
        let home = PathBuf::from("/h");
        for token in ["$HOME/x", "$env:USERPROFILE/x", "%USERPROFILE%/x", "~/x"] {
            assert_eq!(
                expand(token, Some(&home), Os::Windows).unwrap(),
                home.join("x"),
                "{token}"
            );
        }
        assert_eq!(
            expand("$HOME/x", Some(&home), Os::Linux).unwrap(),
            PathBuf::from("$HOME/x"),
            "bash expands $HOME itself before Rune sees a path"
        );
        assert_eq!(
            expand("/c/Users/me", Some(&home), Os::Windows).unwrap(),
            PathBuf::from("C:\\").join("Users/me")
        );
        assert_eq!(
            expand("/c/Users/me", Some(&home), Os::Linux).unwrap(),
            PathBuf::from("/c/Users/me")
        );
    }

    /// Each OS keeps its own credentials and startup files from a reach
    /// answer, and its own shared folders from being an anchor.
    #[test]
    fn every_os_keeps_its_own_credentials_and_shared_folders() {
        use crate::platform::Os;
        assert!(forbidden_in_home(Os::Windows).contains(&"Documents/PowerShell"));
        assert!(forbidden_in_home(Os::Windows).contains(&"AppData/Roaming/Microsoft/Protect"));
        assert!(forbidden_in_home(Os::MacOs).contains(&"Library/Keychains"));
        assert!(forbidden_in_home(Os::MacOs).contains(&"Library/LaunchAgents"));
        assert_eq!(forbidden_in_home(Os::Linux), FORBIDDEN_IN_HOME);
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert!(forbidden_in_home(os).contains(&".ssh"), "{os:?}");
        }
        assert!(shared_anchors(Os::MacOs).contains(&PathBuf::from("/Users")));
        assert!(shared_anchors(Os::Windows)
            .iter()
            .any(|p| p.ends_with("Users")));
    }

    /// The list as it is on a machine with a home directory and no engine of
    /// its own — the shape every case below needs unless it says otherwise.
    fn forbidden() -> Forbidden {
        Forbidden::standard(Some(&home()), &[])
    }

    fn judge(token: &str, home: Option<&Path>) -> Result<Candidate> {
        Ok(candidate(
            resolve(token, home)?,
            home,
            &Forbidden::standard(home, &[]),
        ))
    }

    #[test]
    fn absolute_and_tilde_tokens_are_candidates_and_relative_ones_are_not() {
        let found = in_command("cp ~/Downloads/a.txt src/b.txt /tmp/c");
        assert_eq!(found, vec!["~/Downloads/a.txt", "/tmp/c"]);
    }

    #[test]
    fn punctuation_that_rides_on_a_token_is_not_part_of_the_path() {
        assert_eq!(in_command("ls ~/Downloads; echo done"), vec!["~/Downloads"]);
        assert_eq!(in_command("cat < /etc/hosts"), vec!["/etc/hosts"]);
        assert_eq!(in_command("echo hi > ~/out.txt"), vec!["~/out.txt"]);
    }

    #[test]
    fn a_quoted_path_with_a_space_survives_tokenising() {
        assert_eq!(
            in_command("ls \"/home/dev/My Files\""),
            vec!["/home/dev/My Files"]
        );
    }

    #[test]
    fn awk_regex_programs_are_not_paths_but_real_operands_and_redirects_remain_paths() {
        assert_eq!(
            in_command("awk '/MemTotal/ {printf \"RAM: %.2f\\n\", $2/1024}' /proc/meminfo"),
            vec!["/proc/meminfo"]
        );
        assert_eq!(
            in_command("echo audit && awk '/a/ {print $0}' /tmp/in | cat /tmp/out"),
            vec!["/tmp/in", "/tmp/out"]
        );
        assert_eq!(
            in_command("awk -F : -v count=1 '/Users/ {print}' /tmp/data > /tmp/result"),
            vec!["/tmp/data", "/tmp/result"]
        );
        assert_eq!(
            in_command("awk -f /tmp/script.awk /tmp/data"),
            vec!["/tmp/script.awk", "/tmp/data"]
        );
        assert_eq!(
            in_command("awk -- '/pattern/ {print}' /tmp/data"),
            vec!["/tmp/data"]
        );
        assert_eq!(
            in_command("cat '/a path/with spaces'"),
            vec!["/a path/with spaces"]
        );
        assert_eq!(in_command("cat /MemTotal/ /"), vec!["/MemTotal/", "/"]);
    }

    #[test]
    fn the_same_path_twice_is_offered_once() {
        assert_eq!(in_command("diff /tmp/a /tmp/a"), vec!["/tmp/a"]);
    }

    #[test]
    fn an_argument_is_only_a_candidate_when_it_leaves_the_workspace() {
        assert_eq!(in_argument("src/main.rs"), None);
        assert_eq!(in_argument("  /etc/hosts "), Some("/etc/hosts".to_string()));
        assert_eq!(in_argument("~/notes.md"), Some("~/notes.md".to_string()));
    }

    #[test]
    fn tilde_expands_against_the_home_it_is_given() {
        assert_eq!(
            resolve("~/Downloads", Some(&home())).unwrap(),
            home().join("Downloads")
        );
        assert_eq!(resolve("~", Some(&home())).unwrap(), home());
    }

    #[test]
    fn dot_dot_is_resolved_before_the_path_is_judged() {
        if !posix_paths() {
            return;
        }
        assert_eq!(resolve("/a/b/../c", None).unwrap(), PathBuf::from("/a/c"));
        // A `..` must not smuggle home past the judge — it comes back flagged,
        // not refused.
        let c = judge("~/x/..", Some(&home())).unwrap();
        assert!(c
            .sensitive
            .as_deref()
            .unwrap_or("")
            .contains("whole home directory"));
    }

    #[test]
    fn the_home_directory_is_a_sensitive_full_access_candidate_and_the_root_too() {
        if !posix_paths() {
            return;
        }
        // The old behaviour refused `$HOME` outright; the owner's rule is that
        // the wizard decides, so it is now a question with the reason on it.
        let c = judge("~", Some(&home())).unwrap();
        assert!(c.full_access);
        assert!(c
            .sensitive
            .as_deref()
            .unwrap_or("")
            .contains("whole home directory"));
        assert!(judge("/", None).unwrap().full_access);
        assert!(judge("/", None).unwrap().sensitive.is_none());
    }

    #[test]
    fn an_absent_home_anchors_with_full_access() {
        if !posix_paths() {
            return;
        }
        // `/home/dev` does not exist on the machine running this, so the nearest
        // existing ancestor of `~/Downloads` is `/home` — answerable via authorization card.
        let c = judge("~/Downloads", Some(&home())).unwrap();
        assert!(c.full_access);
    }

    #[test]
    fn a_path_that_does_not_exist_yet_anchors_on_its_nearest_existing_parent() {
        let dir = std::env::temp_dir().join(format!("rune-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("not-yet/file.txt");
        let c = judge(target.to_str().unwrap(), Some(&home())).unwrap();
        assert_eq!(c.named, target);
        assert_eq!(c.anchor, crate::platform::canonical(&dir).unwrap());
        assert!(!c.full_access);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_file_directly_in_a_shared_directory_requires_full_access() {
        if !posix_paths() {
            return;
        }
        // Anchoring at /tmp grants full access for the single call upon user
        // confirmation. Name /tmp literally: the behaviour under test is the
        // shared anchor, and std::env::temp_dir() follows TMPDIR — under a host
        // that redirects TMPDIR (e.g. into a scratch dir) the anchor stops being
        // /tmp and this test would judge the wrong directory entirely.
        let target = Path::new("/tmp").join(format!("rune-absent-{}", std::process::id()));
        let c = judge(target.to_str().unwrap(), Some(&home())).unwrap();
        assert!(
            c.full_access,
            "a new file directly under /tmp must anchor at the shared /tmp and require full access"
        );
        assert_eq!(
            c.anchor,
            Path::new("/tmp"),
            "the anchor is the shared /tmp itself"
        );
    }

    // ── What only the owner may open ──────────────────────────────────────

    #[test]
    fn credentials_and_startup_files_are_flagged_however_they_are_reached() {
        if !posix_paths() {
            return;
        }
        let f = forbidden();
        for path in [
            "/home/dev/.ssh",
            "/home/dev/.ssh/id_ed25519",
            "/home/dev/.aws/credentials",
            "/home/dev/.gnupg",
            "/home/dev/.bashrc",
            "/home/dev/.zshrc",
            "/home/dev/.config/systemd/user/evil.service",
            "/home/dev/.local/bin/rune",
            "/home/dev/.cargo/config.toml",
            "/etc/passwd",
        ] {
            assert!(
                f.blocks(Path::new(path)).is_some(),
                "{path} must never be granted quietly"
            );
        }
    }

    #[test]
    fn a_directory_that_merely_contains_a_forbidden_one_is_flagged_too() {
        if !posix_paths() {
            return;
        }
        // Granting ~/.config would hand over ~/.config/gh with it. The check has
        // to run both ways, or the parent is the way in.
        let f = forbidden();
        assert!(f.blocks(Path::new("/home/dev/.config")).is_some());
        assert!(f.blocks(Path::new("/home/dev")).is_some());
    }

    #[test]
    fn ordinary_directories_are_not_on_the_list() {
        if !posix_paths() {
            return;
        }
        let f = forbidden();
        for path in ["/home/dev/Downloads", "/home/dev/Project/other", "/var/log"] {
            assert!(
                f.blocks(Path::new(path)).is_none(),
                "{path} should be answerable"
            );
        }
    }

    #[test]
    fn the_engines_own_directories_are_allowed() {
        if !posix_paths() {
            return;
        }
        // ~/.rune is permitted without exception
        let engine = vec![
            PathBuf::from("/home/dev/.rune"),
            PathBuf::from("/data/rune"),
        ];
        let f = Forbidden::standard(Some(&home()), &engine);
        assert!(f
            .blocks(Path::new("/home/dev/.rune/secrets.json"))
            .is_none());
    }

    #[test]
    fn the_sensitive_reason_names_the_root_that_caused_it_rather_than_just_saying_no() {
        let c = judge("~/.ssh/id_rsa", Some(&home())).unwrap();
        let why = c.sensitive.as_deref().unwrap_or("");
        assert!(why.contains(".ssh"), "{why}");
        assert!(why.contains("credentials"), "{why}");
    }

    #[test]
    fn an_ordinary_reach_stays_rememberable_and_unflagged() {
        // A real directory, not the fixture home: on this machine `/home/dev`
        // does not exist, and an absent home anchors at `/home` — shared, and
        // therefore full access. The behaviour under test needs a plain,
        // existing, non-shared directory.
        let dir = std::env::temp_dir().join(format!("rune-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = judge(dir.join("file.txt").to_str().unwrap(), Some(&home())).unwrap();
        assert!(c.sensitive.is_none());
        assert!(!c.full_access);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
