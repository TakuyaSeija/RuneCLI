//! Kernel confinement of the commands Rune starts on macOS: Seatbelt.
//!
//! What a confined command may reach is the sandbox's decision
//! (`sandbox::Policy`); this file is how macOS holds it — a Seatbelt profile
//! that denies everything first, then grants what the policy grants, applied
//! by `/usr/bin/sandbox-exec`, which confines itself and then becomes the
//! command: the same pid, session and terminal. Its children inherit the
//! profile and cannot leave it. The engine process itself is never
//! restricted.
//!
//! The same grants as Linux's Landlock: the project, scratch, caches and
//! answered paths read and written, system folders read and run, `/dev` for a
//! terminal, every path's metadata (Landlock does not judge `stat` either),
//! network open. Seatbelt judges the path a file is reached by after links
//! are resolved, so every grant is named canonically (`/tmp` is
//! `/private/tmp` here). What Codex and Chrome learned on real macOS is kept:
//! no XPC service may be started (it would run outside the profile), no
//! `fcntl` that changes a file through a read-only descriptor, no writable
//! root removed itself.

use std::borrow::Cow;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::platform::{Launch, Placement};

/// The only `sandbox-exec` trusted: never one found on a `PATH` someone else
/// may have written.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A profile ready to confine any number of commands. Cloning shares it.
#[derive(Clone)]
pub struct Confinement {
    /// The profile without its socket rules, which each placement adds.
    profile: Arc<str>,
    /// `-D` definitions: every granted path is a parameter of the profile,
    /// never text inside it, so no path can change what the profile says.
    params: Arc<Vec<(String, PathBuf)>>,
    /// Unix sockets granted ([`Grants::local_services`]).
    local_services: bool,
    /// The rules that open the command's own sockets (`Grants::sockets`)
    /// when other programs' stay closed.
    own_sockets: Arc<str>,
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
    /// them is granted as a file.
    pub reach: &'a [PathBuf],
    /// Whether a confined command may reach programs already running on
    /// this machine through Unix-domain sockets; `false`: the profile denies
    /// every Unix socket, name resolution and logging excepted.
    pub local_services: bool,
    /// Folders whose Unix sockets are the command's own — the project, its
    /// scratch, what the person granted: reached without a question.
    pub sockets: &'a [PathBuf],
    /// Who is asked when a command reaches any other program's socket;
    /// `None`: nobody, the connection is refused.
    pub ask_service: Option<crate::platform::ServiceAsk>,
}

/// Why no confinement holds, said to the person (`SandboxReport.reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconfined {
    /// Landlock's ABI on Linux; Seatbelt has none, so 0.
    pub abi: i32,
    pub reason: String,
}

/// Where this OS keeps the devices a terminal needs (`/dev/null`, `/dev/tty`,
/// the pty pair): granted with every confinement.
pub const DEVICES: &str = "/dev";

impl Confinement {
    /// Build the confinement `grants` describe, and prove it on this Mac: a
    /// profile this macOS refuses would fail every command, so it is tried
    /// once (`sandbox-exec … /usr/bin/true`) and, refused, is no confinement
    /// — the gate then asks before every command, and the report says why.
    /// A path that does not exist is skipped — `~/.cargo` may simply not be
    /// there yet — except the project, which every confined command needs.
    pub fn build(grants: Grants<'_>) -> Result<Confinement, Unconfined> {
        let unconfined = |reason: String| Unconfined { abi: 0, reason };
        if !Path::new(SANDBOX_EXEC).is_file() {
            return Err(unconfined(format!("this macOS has no {SANDBOX_EXEC}")));
        }
        let Some(project) = canonical(grants.project) else {
            return Err(unconfined(
                "the project directory could not be granted".into(),
            ));
        };
        let mut params = vec![("PROJECT".to_string(), project)];
        let mut reads = String::new();
        let mut writes = String::from("    (subpath (param \"PROJECT\"))\n");
        let mut roots = vec!["PROJECT".to_string()];
        for (index, path) in grants.reads.iter().filter_map(|p| canonical(p)).enumerate() {
            let key = format!("READ_{index}");
            reads.push_str(&format!("    (subpath (param \"{key}\"))\n"));
            params.push((key, path));
        }
        let granted = grants
            .writes
            .iter()
            .chain(grants.reach)
            .filter_map(|p| canonical(p));
        for (index, path) in granted.enumerate() {
            let key = format!("WRITE_{index}");
            if path.is_dir() {
                writes.push_str(&format!("    (subpath (param \"{key}\"))\n"));
                roots.push(key.clone());
            } else {
                writes.push_str(&format!("    (literal (param \"{key}\"))\n"));
            }
            params.push((key, path));
        }
        // A command's own sockets — in its project, its scratch, what the
        // person granted — are reached and served there; another program's
        // are not (Seatbelt has no way to ask at the moment of `connect`).
        let mut own_sockets = String::from("(allow network-inbound (local unix-socket))\n");
        for (index, path) in grants
            .sockets
            .iter()
            .filter_map(|p| canonical(p))
            .enumerate()
        {
            let key = format!("SOCKETS_{index}");
            own_sockets.push_str(&format!(
                "(allow network-outbound (subpath (param \"{key}\")))\n(allow network-bind (subpath (param \"{key}\")))\n"
            ));
            params.push((key, path));
        }
        let profile = profile(&reads, &writes, &roots);
        let confinement = Confinement {
            profile: profile.into(),
            params: Arc::new(params),
            local_services: grants.local_services,
            own_sockets: own_sockets.into(),
        };
        confinement.prove().map_err(unconfined)?;
        Ok(confinement)
    }

    /// The mechanism's name in the sandbox report.
    pub fn kind(&self) -> &'static str {
        "seatbelt"
    }

    /// Landlock's ABI on Linux; Seatbelt has none.
    pub fn abi(&self) -> i32 {
        0
    }

    /// Whether a command it confines may reach local services
    /// ([`Grants::local_services`]).
    pub fn local_services(&self) -> bool {
        self.local_services
    }

    /// The whole profile for a command that may (`true`) or may not create
    /// and reach Unix sockets.
    fn full(&self, unix_sockets: bool) -> String {
        if unix_sockets {
            format!("{}{UNIX_SOCKETS}", self.profile)
        } else {
            format!("{}{SYSTEM_SOCKETS_ONLY}{}", self.profile, self.own_sockets)
        }
    }

    /// Run `/usr/bin/true` under the profile: `Err` with what Seatbelt said
    /// when it refuses it.
    fn prove(&self) -> Result<(), String> {
        let launch = wrapped(
            &Launch::new("/usr/bin/true"),
            &self.full(false),
            &self.params,
        );
        let output = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{SANDBOX_EXEC} could not start: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let said = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "this macOS refused Rune's Seatbelt profile: {}",
                said.trim()
            ))
        }
    }
}

/// `launch` as macOS starts it under `placement`: inside Seatbelt when it is
/// confined, with Unix sockets as its confinement grants them; as asked
/// otherwise.
pub(super) fn wrap<'a>(
    launch: &'a Launch,
    placement: &Placement<'_>,
) -> io::Result<Cow<'a, Launch>> {
    Ok(match placement.confinement {
        Some(confinement) => Cow::Owned(wrapped(
            launch,
            &confinement.full(confinement.local_services),
            &confinement.params,
        )),
        None => Cow::Borrowed(launch),
    })
}

/// `sandbox-exec -p <profile> -DKEY=path… -- <program> <args…>`, in the
/// folder and with the environment `launch` asked for: `sandbox-exec`
/// confines itself and then becomes the program.
fn wrapped(launch: &Launch, profile: &str, params: &[(String, PathBuf)]) -> Launch {
    let mut wrapped = Launch::new(SANDBOX_EXEC).arg("-p").arg(profile);
    for (key, path) in params {
        let mut definition = OsString::from(format!("-D{key}="));
        definition.push(path.as_os_str());
        wrapped = wrapped.arg(definition);
    }
    wrapped = wrapped.arg("--").arg(&launch.program).args(&launch.args);
    wrapped.dir = launch.dir.clone();
    wrapped.clear_env = launch.clear_env;
    wrapped.env = launch.env.clone();
    wrapped
}

/// The path Seatbelt sees for `path` — every link resolved, `/tmp` as
/// `/private/tmp` — or `None` when it does not exist.
fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// The profile for `reads` (read and run) and `writes` (read, write and
/// run) — lists of path filters naming parameters — and the parameters of
/// the writable `roots`, which may never be removed themselves.
fn profile(reads: &str, writes: &str, roots: &[String]) -> String {
    let reads = if reads.is_empty() {
        String::new()
    } else {
        format!("(allow file-read* file-map-executable\n{reads})\n")
    };
    let roots: String = roots
        .iter()
        .map(|key| {
            format!(
                "(deny file-write-unlink (require-all (vnode-type DIRECTORY) (literal (param \"{key}\"))))\n"
            )
        })
        .collect();
    format!(
        r#"(version 1)
(deny default)

; Processes: what a shell and a build do; signals and process facts only
; within this sandbox.
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow sysctl-write (sysctl-name "kern.grade_cputype"))
(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))
(allow ipc-posix-sem)
(allow ipc-posix-shm-read* (ipc-posix-name "apple.shm.notification_center"))
(allow ipc-posix-shm-read-data ipc-posix-shm-write-create ipc-posix-shm-write-unlink
    (ipc-posix-name-regex #"^/__KMP_REGISTERED_LIB_[0-9]+$"))
(allow system-mac-syscall (mac-policy-name "vnguard"))
(allow system-mac-syscall
    (require-all (mac-policy-name "Sandbox") (mac-syscall-number 67)))

; The system services ordinary programs ask at start: who the user is,
; logging, notifications, certificates and trust for TLS, name resolution,
; network configuration.
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.opendirectoryd.membership")
    (global-name "com.apple.system.DirectoryService.libinfo_v1")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.logd")
    (global-name "com.apple.logd.events")
    (global-name "com.apple.diagnosticd")
    (global-name "com.apple.analyticsd")
    (global-name "com.apple.analyticsd.messagetracer")
    (global-name "com.apple.bsd.dirhelper")
    (global-name "com.apple.secinitd")
    (global-name "com.apple.trustd")
    (global-name "com.apple.trustd.agent")
    (global-name "com.apple.TrustEvaluationAgent")
    (global-name "com.apple.SecurityServer")
    (global-name "com.apple.ocspd")
    (global-name "com.apple.networkd")
    (global-name "com.apple.dnssd.service")
    (global-name "com.apple.SystemConfiguration.DNSConfiguration")
    (global-name "com.apple.SystemConfiguration.configd")
    (global-name "com.apple.PowerManagement.control")
    (global-name "com.apple.xpc.activity.unmanaged"))

; A terminal: its devices, and new ones a command may open itself.
(allow pseudo-tty)
(allow file-read* file-write-data file-ioctl (subpath "/dev"))

; Every path's metadata, as Landlock leaves it; the root folder itself.
(allow file-read-metadata file-test-existence)
(allow file-read* (literal "/"))

; Read and run.
{reads}
; Read, write and run: the project, scratch, caches, answered paths.
(allow file-read* file-write* file-map-executable
{writes})

; Network as on Linux: open.
(allow network-outbound (remote ip "*:*"))
(allow network-inbound (local ip "*:*"))
(allow network-bind (local ip "*:*"))
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow network-outbound (literal "/private/var/run/syslog"))

; Last, so no grant above reopens them: a writable root is never removed
; itself, no fcntl changes a file through a read-only descriptor
; (F_MAKECOMPRESSED 80, F_TRANSFEREXTENTS 110), and no XPC service is
; started — it would run outside this profile.
{roots}(deny system-fcntl (fcntl-command 80 110))
(deny mach-lookup (xpc-service-name-prefix ""))
"#
    )
}

/// Unix sockets, for a command allowed them (every terminal command, as on
/// Linux; an MCP server the person allowed).
const UNIX_SOCKETS: &str = r#"
(allow network-outbound (remote unix-socket))
(allow network-inbound (local unix-socket))
(allow network-bind (local unix-socket))
"#;

/// Nothing more for a command refused Unix sockets: the profile denies them,
/// name resolution and logging excepted.
const SYSTEM_SOCKETS_ONLY: &str = "";

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-seatbelt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// A confined command reaches what it was granted, and nothing else:
    /// the project is written, a granted folder read, the rest refused.
    #[test]
    fn a_confined_command_reaches_what_it_was_granted_and_nothing_else() {
        let base = scratch("reach");
        for dir in ["project", "granted", "hidden"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        std::fs::write(base.join("granted/read.txt"), "ok").unwrap();
        std::fs::write(base.join("hidden/secret.txt"), "no").unwrap();
        let project = base.join("project");
        let reads = [
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/private/etc",
            "/private/var/db",
            "/private/var/select",
        ]
        .map(PathBuf::from)
        .into_iter()
        .chain([base.join("granted")])
        .collect::<Vec<_>>();
        let confinement = Confinement::build(Grants {
            project: &project,
            ledger: None,
            reads: &reads,
            writes: &[],
            reach: &[],
            local_services: false,
            sockets: &[],
            ask_service: None,
        })
        .unwrap_or_else(|why| panic!("{}", why.reason));
        assert_eq!(confinement.kind(), "seatbelt");
        let run = |script: String| {
            let (pty, slave) = crate::platform::open_pty(24, 80).unwrap();
            let mut child = crate::platform::spawn_on_terminal(
                &Launch::new("/bin/sh").args(["-c", &script]),
                &slave,
                Placement {
                    confinement: Some(&confinement),
                    ..Placement::default()
                },
            )
            .unwrap();
            drop(slave);
            // What it prints is read while it runs — a terminal nobody reads
            // fills, and its writer waits — and a command that does not end
            // in time is stopped and named, never waited on for ever.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut shown = Vec::new();
            let code = loop {
                let mut buf = [0u8; 4096];
                if let Ok(Some(n)) = pty.read_timeout(&mut buf, 50) {
                    shown.extend_from_slice(&buf[..n]);
                }
                if let Some(code) = child.try_wait().unwrap() {
                    break code;
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{script:?} did not end under the sandbox; it showed {:?}",
                        String::from_utf8_lossy(&shown)
                    );
                }
            };
            drop(pty);
            code == 0
        };
        assert!(run(format!("echo hi > '{}/out.txt'", project.display())));
        assert!(run(format!("cat '{}/granted/read.txt'", base.display())));
        assert!(!run(format!("cat '{}/hidden/secret.txt'", base.display())));
        assert!(!run(format!(
            "echo no > '{}/granted/new.txt'",
            base.display()
        )));
        assert!(
            !run(format!("rmdir '{}'", project.display())),
            "a writable root is never removed itself"
        );
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
            local_services: false,
            sockets: &[],
            ask_service: None,
        })
        .err()
        .expect("never a confinement without its project");
        assert_eq!(
            unconfined.reason,
            "the project directory could not be granted"
        );
    }

    /// A granted path is a parameter, never text of the profile: a quote or
    /// a parenthesis in a folder's name changes nothing the profile says.
    #[test]
    fn a_granted_path_never_becomes_profile_text() {
        let base = scratch("quote");
        let odd = base.join("a\") (allow default) (\"");
        std::fs::create_dir_all(&odd).unwrap();
        let confinement = Confinement::build(Grants {
            project: &odd,
            ledger: None,
            reads: &[PathBuf::from("/usr"), PathBuf::from("/System")],
            writes: &[],
            reach: &[],
            local_services: false,
            sockets: &[],
            ask_service: None,
        })
        .unwrap_or_else(|why| panic!("{}", why.reason));
        assert!(!confinement.profile.contains("(allow default)"));
        assert!(confinement
            .params
            .iter()
            .any(|(key, path)| key == "PROJECT" && path == &odd));
        let _ = std::fs::remove_dir_all(&base);
    }
}
