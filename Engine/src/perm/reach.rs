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

/// Where a POSIX shell running on Windows (Git Bash, MSYS2, Cygwin) has its
/// `/`: the folder holding the `bin` (or `usr\bin`) its program is in —
/// `C:\Program Files\Git` for Git Bash. `None` for a shell laid out
/// otherwise.
pub fn posix_root_of(shell: &Path) -> Option<PathBuf> {
    let named = |path: &Path, name: &str| {
        path.file_name()
            .is_some_and(|file| file.eq_ignore_ascii_case(name))
    };
    let bin = shell.parent().filter(|bin| named(bin, "bin"))?;
    let above = bin.parent()?;
    if named(above, "usr") {
        above.parent().map(Path::to_path_buf)
    } else {
        Some(above.to_path_buf())
    }
}

/// A path-shaped token as such a shell reads it: rooted at `/`, it lies
/// under the shell's own root (`/usr/bin/env` is Git's), unless it names a
/// drive (`/c/…`, left for [`resolve`]) or a share (`//server`). Anything else
/// is returned as it was.
pub fn under_posix_root(token: &str, root: &Path) -> String {
    let b = token.as_bytes();
    let drive = b.len() >= 2 && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/');
    if !token.starts_with('/') || token.starts_with("//") || drive {
        return token.to_string();
    }
    root.join(token.trim_start_matches('/').replace('/', "\\"))
        .display()
        .to_string()
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

// ─── What an answer can open ───────────────────────────────────────────────

/// A path the engine is willing to put to the user.
///
/// Nothing here refuses, and no folder is special (owner, 2026-10-17: the
/// modes are the only restriction, the same on every OS — `~/.ssh` or
/// `.bashrc` is a path outside the workspace like any other, asked in Ask
/// and Auto with "always" on offer, so the agent can debug what the person
/// asks it to; the classifier still reviews what its policy covers). The
/// owner's rule, 2026-09-28: the system never says no on its own.
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

/// Expand `~` and resolve `.`/`..`, without judging anything.
///
/// Split from [`candidate`] so a caller can ask "is this already reachable?"
/// before putting it to the person: `/usr/bin/python3` names a path outside
/// the workspace that every command on the machine already reads.
///
/// `home` is passed in rather than read here so the tests do not depend on the
/// machine they run on.
pub fn resolve(token: &str, home: Option<&Path>) -> Result<PathBuf> {
    Ok(normalize(&expand(token, home, crate::platform::OS)?))
}

/// Work out where a rule for an already-resolved path can be anchored. Never
/// an error and never a refusal.
pub fn candidate(named: PathBuf, home: Option<&Path>) -> Candidate {
    let broad = |named: PathBuf, anchor: PathBuf| Candidate {
        named,
        anchor,
        full_access: true,
    };
    // The whole home, or the filesystem's root: allowing it is full access
    // under another name, said so on the card.
    if home.is_some_and(|home| named == *home) || named.parent().is_none() {
        return broad(named.clone(), named);
    }
    let anchor = match nearest_existing(&named) {
        Some(anchor) => anchor,
        // Not even `/` exists above it, so no rule can be anchored anywhere.
        // The only honest shape left is a one-call full access.
        None => return broad(named.clone(), named),
    };
    let os = crate::platform::OS;
    let same = |a: &Path, b: &Path| {
        crate::jail::starts_with_folded(a, b, os) && crate::jail::starts_with_folded(b, a, os)
    };
    // The anchor is canonical, so the home and the shared folders are read in
    // both spellings: `/home` is a link on macOS (`/System/Volumes/Data/home`)
    // and on image-based Linux (`/var/home`), and a broad folder reached
    // through a link is as broad.
    let spellings = |path: &Path| {
        let mut both = vec![path.to_path_buf()];
        if let Ok(real) = crate::platform::canonical(path) {
            if real != path {
                both.push(real);
            }
        }
        both
    };
    if anchor.parent().is_none()
        || home.is_some_and(|h| {
            h.ancestors()
                .any(|above| spellings(above).iter().any(|s| same(&anchor, s)))
        })
        || shared_anchors(os)
            .iter()
            .any(|s| spellings(s).iter().any(|s| same(&anchor, s)))
    {
        return broad(named, anchor);
    }
    Candidate {
        named,
        anchor,
        full_access: false,
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
        assert_eq!(
            resolve(r"C:\a\b\..\c", None).unwrap(),
            PathBuf::from(r"C:\a\c")
        );
        assert_eq!(
            resolve(r"~\.ssh\id_rsa", Some(&home)).unwrap(),
            home.join(r".ssh\id_rsa")
        );
        assert!(
            judge(r"~", Some(&home)).unwrap().full_access,
            "the whole profile"
        );
    }

    /// Git Bash's `/` is Git's own folder: `/usr/bin/env` is read there, a
    /// drive or a share as it says.
    #[test]
    fn a_posix_shell_on_windows_roots_its_paths_in_its_own_folder() {
        let git = PathBuf::from("C:/Program Files/Git");
        for shell in [
            "C:/Program Files/Git/bin/bash.exe",
            "C:/Program Files/Git/usr/bin/bash.exe",
        ] {
            assert_eq!(
                posix_root_of(Path::new(shell)),
                Some(git.clone()),
                "{shell}"
            );
        }
        assert_eq!(posix_root_of(Path::new("C:/tools/bash.exe")), None);
        assert_eq!(
            under_posix_root("/usr/bin/env", &git),
            git.join("usr\\bin\\env").display().to_string()
        );
        for token in ["/c/Users/me", "/d", "//server/share", "~/x", "relative"] {
            assert_eq!(under_posix_root(token, &git), token);
        }
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

    /// Each OS keeps its own shared folders from being an anchor.
    #[test]
    fn every_os_keeps_its_own_shared_folders_from_being_an_anchor() {
        use crate::platform::Os;
        assert!(shared_anchors(Os::MacOs).contains(&PathBuf::from("/Users")));
        assert!(shared_anchors(Os::Windows)
            .iter()
            .any(|p| p.ends_with("Users")));
        assert!(shared_anchors(Os::Linux).contains(&PathBuf::from("/tmp")));
    }

    fn judge(token: &str, home: Option<&Path>) -> Result<Candidate> {
        Ok(candidate(resolve(token, home)?, home))
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
        // A `..` must not smuggle the whole home past the judge.
        assert!(judge("~/x/..", Some(&home())).unwrap().full_access);
    }

    #[test]
    fn the_home_directory_and_the_root_are_full_access_candidates() {
        if !posix_paths() {
            return;
        }
        assert!(judge("~", Some(&home())).unwrap().full_access);
        assert!(judge("/", None).unwrap().full_access);
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

    /// The folder of every home, reached through a link (`/home` on macOS
    /// and on image-based Linux): its real path is as broad (CI macOS,
    /// 2026-10-18: the canonical anchor was compared with the link's name).
    #[test]
    fn a_home_folder_behind_a_link_is_as_broad_as_its_name() {
        if !posix_paths() {
            return;
        }
        let base = std::env::temp_dir().join(format!("rune-reach-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real")).unwrap();
        if crate::platform::fixture::symlink(base.join("real"), base.join("homes")).is_err() {
            eprintln!("note: no links here");
            return;
        }
        let home = base.join("homes").join("dev");
        let c = judge("~/Downloads", Some(&home)).unwrap();
        assert!(c.full_access, "{c:?}");
        let _ = std::fs::remove_dir_all(base);
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
        // `/tmp` as the filesystem names it (`/private/tmp` on macOS).
        assert_eq!(
            c.anchor,
            crate::platform::canonical(Path::new("/tmp")).unwrap(),
            "the anchor is the shared /tmp itself"
        );
    }

    /// Owner, 2026-10-17: no folder is special. A folder that used to be on
    /// a list of credentials and startup files is anchored like any other —
    /// a narrow grant the person may keep — on every OS.
    #[test]
    fn no_folder_in_the_home_is_special() {
        let dir = std::env::temp_dir().join(format!("rune-reach-dot-{}", std::process::id()));
        for name in [".ssh", ".aws", ".config/gh", ".bashrc.d"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            let c = judge(dir.join(name).join("x").to_str().unwrap(), Some(&home())).unwrap();
            assert!(!c.full_access, "{name}");
            assert_eq!(
                c.anchor,
                crate::platform::canonical(&dir.join(name)).unwrap()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(!c.full_access);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
