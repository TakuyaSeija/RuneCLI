//! Kernel confinement of the commands Rune starts: Landlock.
//!
//! What a confined command may reach is the sandbox's decision
//! (`sandbox::Policy`); this file is how Linux holds it — a ruleset built in
//! the parent once, applied in each child between `fork` and `exec`
//! (`ChildSteps`). The engine process itself is never restricted.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::landlock;

/// A ruleset ready to confine any number of children. Cloning shares it.
#[derive(Clone)]
pub struct Confinement {
    ruleset: Arc<OwnedFd>,
    abi: i32,
}

/// What one confinement grants. Read and write are different questions: a
/// compiler reads the system's headers, and nothing outside the project and
/// its caches is written.
pub struct Grants<'a> {
    /// Full rights; a confinement that cannot grant it is no confinement.
    pub project: &'a Path,
    /// Read and execute only.
    pub reads: &'a [PathBuf],
    /// Full rights: scratch space and package caches.
    pub writes: &'a [PathBuf],
    /// Where an OS records what a confinement leaves on disk, so a person can
    /// take it back (`forget_grants`): Windows's ACL entries. This kernel's
    /// confinement leaves nothing there, so nothing is recorded.
    pub ledger: Option<&'a Path>,
    /// Full rights, each answered for by the person by name; a file among
    /// them gets the rights a file can carry.
    pub reach: &'a [PathBuf],
}

/// Why no confinement holds, said to the person (`SandboxReport.reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfined {
    /// The kernel's Landlock ABI, 0 when it has none.
    pub abi: i32,
    pub reason: String,
}

impl Confinement {
    /// Build the confinement `grants` describe. A path that does not exist is
    /// skipped — `~/.cargo` may simply not be there yet — except the project,
    /// which every confined command needs.
    pub fn build(grants: Grants<'_>) -> Result<Confinement, Unconfined> {
        let abi = landlock::abi();
        let unconfined = |reason: String| Unconfined { abi, reason };
        if abi <= 0 {
            return Err(Unconfined {
                abi: 0,
                reason: "this kernel has no Landlock support".into(),
            });
        }

        let all = landlock::all_fs_rights(abi);
        let read_only = landlock::FS_EXECUTE | landlock::FS_READ_FILE | landlock::FS_READ_DIR;

        // Scopes close two doors that no filesystem right covers: signalling
        // processes outside the sandbox, and connecting to abstract UNIX sockets
        // — which is how a process reaches X11 and could log every keystroke.
        let ruleset = landlock::Ruleset::new(abi, all, landlock::all_scopes(abi))
            .map_err(|e| unconfined(format!("ruleset could not be created: {e}")))?;

        // Terminals need their device: /dev/null, /dev/tty and the pty pair.
        let dev =
            read_only | landlock::FS_WRITE_FILE | if abi >= 5 { landlock::FS_IOCTL_DEV } else { 0 };
        let _ = ruleset.allow(Path::new(DEVICES), dev);

        // A rule anchored at a file may carry file rights only (the kernel
        // refuses a directory right there, and the refusal is silent): every
        // file among the reads (`~/.gitconfig`, the resolver's file) was
        // never readable until it got its own rights (CI, 2026-10-17: no
        // name could be resolved on Ubuntu).
        let file_read = landlock::FS_EXECUTE | landlock::FS_READ_FILE;
        for r in grants.reads {
            let rights = if r.is_dir() { read_only } else { file_read };
            let _ = ruleset.allow(r, rights);
        }

        match ruleset.allow(grants.project, all) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return Err(unconfined(
                    "the project directory could not be granted".into(),
                ))
            }
        }
        for w in grants.writes {
            let rights = if w.is_dir() {
                all
            } else {
                landlock::file_rights(abi)
            };
            let _ = ruleset.allow(w, rights);
        }
        // Answered for by a person, by name, one prompt each. A rule anchored at
        // a file may only carry file rights — see `landlock::file_rights`.
        for r in grants.reach {
            let rights = if r.is_dir() {
                all
            } else {
                landlock::file_rights(abi)
            };
            let _ = ruleset.allow(r, rights);
        }

        Ok(Confinement {
            ruleset: Arc::new(ruleset.into_fd()),
            abi,
        })
    }

    /// The mechanism's name in the sandbox report.
    pub fn kind(&self) -> &'static str {
        "landlock"
    }

    /// The kernel's Landlock ABI this confinement was built for.
    pub fn abi(&self) -> i32 {
        self.abi
    }

    /// Apply this confinement to the calling thread, permanently: in a
    /// child, between `fork` and `exec`. Bare syscalls only.
    pub(super) fn restrict_calling_thread(&self) -> io::Result<()> {
        landlock::restrict_calling_thread(self.ruleset.as_raw_fd())
    }
}

/// Where this OS keeps the devices a terminal needs (`/dev/null`, `/dev/tty`,
/// the pty pair): granted with every confinement, so a command never asks
/// for a path every command on the machine uses.
pub const DEVICES: &str = "/dev";

#[cfg(test)]
mod tests {
    use super::*;

    /// A confined command reaches what it was granted, and nothing else:
    /// the project is written, a granted folder read, the rest refused.
    #[test]
    fn a_confined_command_reaches_what_it_was_granted_and_nothing_else() {
        if landlock::abi() <= 0 {
            eprintln!("note: this kernel has no Landlock; nothing to confine");
            return;
        }
        let base = std::env::temp_dir().join(format!("rune-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for dir in ["project", "granted", "hidden"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        std::fs::write(base.join("granted/read.txt"), "ok").unwrap();
        std::fs::write(base.join("hidden/secret.txt"), "no").unwrap();
        // A file granted on its own, in a folder that is not: a dotfile.
        std::fs::write(base.join("hidden/granted-file.txt"), "ok").unwrap();
        let project = base.join("project");
        let reads = vec![
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/etc"),
            base.join("granted"),
            base.join("hidden/granted-file.txt"),
        ];
        let confinement = Confinement::build(Grants {
            project: &project,
            ledger: None,
            reads: &reads,
            writes: &[],
            reach: &[],
        })
        .unwrap();
        assert_eq!(confinement.kind(), "landlock");
        assert!(confinement.abi() >= 1);
        let run = |script: String| {
            let (pty, slave) = crate::platform::open_pty(24, 80).unwrap();
            let mut child = crate::platform::spawn_on_terminal(
                &crate::platform::Launch::new("/bin/sh").args(["-c", &script]),
                &slave,
                crate::platform::Placement {
                    confinement: Some(&confinement),
                    ..crate::platform::Placement::default()
                },
            )
            .unwrap();
            drop(slave);
            let ok = child.wait().unwrap() == 0;
            drop(pty);
            ok
        };
        assert!(run(format!("echo hi > {}/out.txt", project.display())));
        assert!(run(format!("cat {}/granted/read.txt", base.display())));
        assert!(
            run(format!("cat {}/hidden/granted-file.txt", base.display())),
            "a file granted to read is read"
        );
        assert!(!run(format!("cat {}/hidden/secret.txt", base.display())));
        assert!(!run(format!(
            "echo no > {}/granted/new.txt",
            base.display()
        )));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_project_that_cannot_be_granted_is_no_confinement() {
        let unconfined = Confinement::build(Grants {
            project: Path::new("/nonexistent-rune-project"),
            ledger: None,
            reads: &[],
            writes: &[],
            reach: &[],
        })
        .err()
        .expect("never a confinement without its project");
        if unconfined.abi > 0 {
            assert_eq!(
                unconfined.reason,
                "the project directory could not be granted"
            );
        } else {
            assert_eq!(unconfined.reason, "this kernel has no Landlock support");
        }
    }
}
