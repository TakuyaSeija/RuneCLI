//! Kernel confinement for commands the agent runs.
//!
//! The jail in `jail` guards paths the engine's own tools open. It cannot guard
//! a shell: once `bash` is running, `> ~/.ssh/authorized_keys` is between the
//! shell and the kernel, and no amount of path checking in Rust sees it.
//!
//! So the confinement has to be the kernel's. This module decides what a
//! confined command may reach ([`Policy`]); the kernel's mechanism is the
//! platform's (`platform::Confinement` — on Linux, Landlock: a ruleset built in
//! the parent, applied in the child between `fork` and `exec`). The engine
//! process itself is never restricted — only what it spawns.
//!
//! **Best-effort, always reported.** On a kernel without Landlock the engine
//! still runs, the permission gate still puts every question to the person, and
//! the `ready` event says `enforced: false` with the reason. A silent downgrade
//! is how someone's home directory gets rewritten by a command they thought was
//! contained.

use std::path::{Path, PathBuf};

use crate::proto::{Escape, SandboxReport};

/// What a spawned command is allowed to reach.
///
/// Two lists, because read and write are different questions. The first version
/// of this file granted the whole filesystem readable, on the reasoning that a
/// compiler needs its headers. True — and it also meant an approved `cat` could
/// read `~/.ssh`, every other project on the disk, and anything else the user
/// owns, which combined with an approved `curl` is exfiltration. Reading is now
/// narrowed to the system directories that builds actually need.
#[derive(Debug, Clone)]
pub struct Policy {
    /// The opened project. Full read and write.
    pub project: PathBuf,
    /// This session's private scratch directory, exported as `TMPDIR`.
    pub scratch: PathBuf,
    /// Writable: scratch space and package caches, without which ordinary
    /// developer commands fail for reasons nobody will connect to us.
    pub writes: Vec<PathBuf>,
    /// Readable and executable, nothing more.
    pub reads: Vec<PathBuf>,
    /// Paths outside the workspace that the **user** has allowed this session to
    /// reach (see [`crate::perm::reach`]). Full rights, like the project: an
    /// allow that quietly granted less than the command needs is the bug this
    /// whole path exists to close — the command would fail after the user had
    /// already said yes, which is indistinguishable from not having asked.
    ///
    /// Empty in every session where nobody has answered a reach prompt.
    pub reach: Vec<PathBuf>,
    /// Whether a confined command may reach programs already running on this
    /// machine through Unix sockets — a container engine, the user's service
    /// manager, a terminal multiplexer, an agent holding keys. Such a program
    /// is outside the workspace and acts with the user's own rights, so the
    /// mode's rule for outside the workspace holds: closed in Ask and Auto
    /// until the person allows it ([`crate::perm::Allowed::local_services`],
    /// or an MCP server's `allow_host_sockets`), open in Full, where nothing
    /// is confined.
    pub local_services: bool,
    /// Where the confinement records each entry it leaves on disk before
    /// writing it — Windows's access entries — so `rune -sandbox reset` can
    /// take every one back ([`grants_ledger`]). `None`: nothing may be left
    /// on disk, and where the OS would have to (Windows), nothing starts
    /// confined. A probe, which starts nothing, needs none.
    pub ledger: Option<PathBuf>,
    /// Who decides, at the moment a confined command connects, about a
    /// program running outside the workspace (`platform::Grants::ask_service`):
    /// the session's permission gate. `None`: refused.
    pub ask_service: Option<AskService>,
}

/// [`Policy::ask_service`]: the session's question, carried with the policy.
#[derive(Clone)]
pub struct AskService(pub crate::platform::ServiceAsk);

impl std::fmt::Debug for AskService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AskService")
    }
}

use crate::platform::Os;

/// Where the access entries a confinement leaves on disk are recorded —
/// Windows's, on the folders it grants — in Rune's `home`, which no confined
/// command reaches: one file for every session and MCP server, so
/// `rune -sandbox reset` takes every one back.
pub fn grants_ledger(home: &Path) -> PathBuf {
    home.join("sandbox-grants.txt")
}

/// Container daemon sockets on Linux. Reaching one of these is root on the
/// host — the daemon does the work, so the kernel never sees the sandboxed
/// process do it.
///
/// Landlock cannot help here: it mediates opening files, not `connect()` on a
/// pathname UNIX socket. Verified on this machine, not assumed — with `/run`
/// ungranted, `docker ps` still worked.
const DAEMON_SOCKETS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/podman/podman.sock",
    "/run/containerd/containerd.sock",
];

/// The container daemons' sockets a confined command could still reach on
/// `os`. macOS's sandbox leaves a terminal command its Unix sockets, as
/// Linux does, and the desktop engines put theirs under the home. None on
/// Windows: a confined command's restricted token is refused the Docker
/// engine's pipe, whose ACL names administrators and its own group.
fn daemon_sockets(os: Os, home: Option<&Path>) -> Vec<PathBuf> {
    match os {
        Os::Linux => DAEMON_SOCKETS.iter().map(PathBuf::from).collect(),
        Os::MacOs => {
            let mut all = vec![PathBuf::from("/var/run/docker.sock")];
            if let Some(home) = home {
                all.extend(
                    [
                        ".docker/run/docker.sock",
                        ".colima/default/docker.sock",
                        ".orbstack/run/docker.sock",
                        ".rd/docker.sock",
                    ]
                    .iter()
                    .map(|p| home.join(p)),
                );
            }
            all
        }
        Os::Windows => Vec::new(),
    }
}

/// Sockets this process can actually write to. An existing socket the user has
/// no access to is not an escape.
fn reachable_daemons() -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in daemon_sockets(crate::platform::OS, crate::platform::home_dir().as_deref()) {
        // Can we talk to it? Existence alone proves nothing.
        if !crate::platform::writable(&path) {
            continue;
        }
        // `/var/run` is a symlink to `/run` on most distributions, so the
        // same socket appears twice. Report the thing, not the spellings.
        let canonical = crate::platform::canonical(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        found.push(path.display().to_string());
    }
    found
}

fn known_escapes() -> Vec<Escape> {
    let mut escapes: Vec<Escape> = reachable_daemons()
        .into_iter()
        .map(|sock| Escape {
            id: "container_daemon".into(),
            detail: format!(
                "{sock} is reachable from this user. A container daemon runs as root, so a \
                 command that talks to it can read and write the whole host — the kernel sandbox \
                 cannot see it happen."
            ),
            fix: "Run the engine as a user that is not in the docker/podman group, or stop the \
                  daemon while using Rune. The gate asks about the obvious shapes, but it is a \
                  tripwire, not a boundary."
                .into(),
        })
        .collect();
    if crate::platform::running_elevated() {
        let who = match crate::platform::OS {
            Os::Windows => "as Administrator",
            Os::Linux | Os::MacOs => "as root",
        };
        escapes.push(Escape {
            id: "elevated".into(),
            detail: format!(
                "Rune runs {who}: a command it runs unconfined — in Full mode, or wherever \
                 the sandbox is off — has the machine's highest rights."
            ),
            fix: "Start Rune from a terminal that is not elevated (not root, not \"Run as \
                  administrator\")."
                .into(),
        });
    }
    escapes
}

/// System directories a build genuinely needs to read, on Linux.
///
/// `/run` is deliberately absent: it holds daemon sockets — `docker.sock` above
/// all — and a process that can reach the Docker socket is root on the host, no
/// matter what any sandbox says. Where `/etc/resolv.conf` links into `/run`,
/// its target alone is read ([`resolver_files`]).
const SYSTEM_READS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32", "/etc", "/opt", "/snap",
    "/proc", "/sys", "/var",
];

/// What a confined command reads and runs of the system on `os`. macOS: the
/// system, its libraries and frameworks, Homebrew (`/opt`, `/usr/local`),
/// the applications (Xcode's toolchains), `/bin/sh`'s choice of shell
/// (`/private/var/select`) and the time zones and name service it needs.
/// Windows: the Windows folder and Program Files, which every application
/// package may read already — named so a command is never asked about them.
fn system_reads(os: Os) -> Vec<PathBuf> {
    match os {
        Os::Linux => SYSTEM_READS.iter().map(PathBuf::from).collect(),
        Os::MacOs => [
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/Applications",
            "/opt",
            "/etc",
            "/private/etc",
            "/var/db",
            "/private/var/db",
            "/private/var/select",
            "/private/var/run/resolv.conf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect(),
        Os::Windows => crate::perm::denylist::protected_prefixes(Os::Windows),
    }
}

/// The file the name service is configured by, where it really is: on a
/// system whose `/etc/resolv.conf` is a link into `/run` — systemd-resolved,
/// NetworkManager, resolvconf (Ubuntu, Fedora) — the link's target, granted
/// alone, so no confined command could resolve a name there (CI, 2026-10-17:
/// `curl` exit 6 on Ubuntu). `/run` and its daemons' sockets stay closed: a
/// regular file is granted, never a folder. Found on this machine, never
/// assumed from a distribution; nothing where the file is not a link (Windows
/// keeps no such file).
pub fn resolver_files(os: Os) -> Vec<PathBuf> {
    let link: &[&str] = match os {
        Os::Linux => &["/etc/resolv.conf"],
        Os::MacOs => &["/etc/resolv.conf", "/private/etc/resolv.conf"],
        Os::Windows => &[],
    };
    let mut found: Vec<PathBuf> = Vec::new();
    for path in link {
        let path = Path::new(path);
        let Ok(real) = crate::platform::canonical(path) else {
            continue;
        };
        if real != path && real.is_file() && !found.contains(&real) {
            found.push(real);
        }
    }
    found
}

/// Package stores of systems that keep every program and its libraries in one
/// read-only, content-addressed tree instead of `/usr` (NixOS, Guix, and Nix
/// on macOS). Granted read and execute only where this machine has one —
/// found, never assumed from a distribution's name (owner, 2026-10-16: on
/// NixOS no command could run). Nothing else under them is writable, and
/// `/run`, where those systems link into the store from, stays closed: the
/// kernel judges the file reached.
const PACKAGE_STORES: &[&str] = &["/nix/store", "/gnu/store"];

/// The package stores this machine has.
pub fn package_stores() -> Vec<PathBuf> {
    match crate::platform::OS {
        Os::Windows => Vec::new(),
        Os::Linux | Os::MacOs => present(PACKAGE_STORES),
    }
}

fn present(candidates: &[&str]) -> Vec<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .filter(|path| std::fs::metadata(path).is_ok_and(|m| m.is_dir()))
        .collect()
}

/// System folders an MCP server may read and run programs from, on Linux.
const MCP_SYSTEM_READS: &[&str] = &["/usr", "/bin", "/lib", "/lib64"];

/// The same on `os`. macOS: its programs in `/usr/bin` (`python3`, `git`)
/// are `xcrun` shims that read the chosen developer folder from `/var/db`
/// and run the tool from Xcode under `/Applications` or the command line
/// tools under `/Library` — as a terminal command reads them.
fn mcp_system_reads(os: Os) -> Vec<PathBuf> {
    match os {
        Os::Linux => MCP_SYSTEM_READS.iter().map(PathBuf::from).collect(),
        Os::MacOs => [
            "/usr",
            "/bin",
            "/System",
            "/Library",
            "/opt",
            "/Applications",
            "/private/var/db",
            "/private/var/select",
        ]
        .iter()
        .map(PathBuf::from)
        .collect(),
        Os::Windows => crate::perm::denylist::protected_prefixes(Os::Windows),
    }
}

/// What else an MCP server reads to reach the network, on `os`: the
/// certificates and the name service's files (Windows keeps both in its own
/// stores, which every program reaches).
fn mcp_network_reads(os: Os) -> Vec<PathBuf> {
    let files: &[&str] = match os {
        Os::Linux => &[
            "/etc/ssl",
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
        ],
        Os::MacOs => &[
            "/private/etc/ssl",
            "/private/etc/hosts",
            "/private/var/run/resolv.conf",
        ],
        Os::Windows => &[],
    };
    files.iter().map(PathBuf::from).collect()
}

/// The `PATH` an MCP server is started with: the person's own, kept to the
/// folders its confinement lets it run programs from — so `node` or `npx` is
/// found wherever this system keeps it (`/run/current-system/sw/bin` on
/// NixOS reaches the store), and a folder it could not run from anyway (a
/// `~/bin`) is left out. The plain system folders when none is left.
pub fn mcp_search_path() -> std::ffi::OsString {
    let roots: Vec<PathBuf> = mcp_system_reads(crate::platform::OS)
        .into_iter()
        .chain(package_stores())
        .collect();
    search_path_within(std::env::var_os("PATH"), &roots, crate::platform::OS)
}

/// The system's own program folders, when nothing of the person's `PATH` is
/// left.
fn plain_search_path(os: Os) -> std::ffi::OsString {
    match os {
        Os::Linux => "/usr/local/bin:/usr/bin:/bin".into(),
        Os::MacOs => "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".into(),
        Os::Windows => {
            let root = crate::perm::denylist::protected_prefixes(Os::Windows)
                .into_iter()
                .next()
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            // Windows's own separator, whichever machine computes it.
            let mut path = std::ffi::OsString::new();
            for (i, folder) in [
                root.join("System32"),
                root.clone(),
                root.join(r"System32\WindowsPowerShell\v1.0"),
            ]
            .iter()
            .enumerate()
            {
                if i > 0 {
                    path.push(";");
                }
                path.push(folder);
            }
            path
        }
    }
}

fn search_path_within(
    path: Option<std::ffi::OsString>,
    roots: &[PathBuf],
    os: Os,
) -> std::ffi::OsString {
    let kept: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| {
            dir.is_absolute()
                && crate::platform::canonical(dir).is_ok_and(|real| {
                    roots
                        .iter()
                        .any(|root| crate::jail::starts_with_folded(&real, root, os))
                })
        })
        .collect();
    if kept.is_empty() {
        return plain_search_path(os);
    }
    std::env::join_paths(kept).unwrap_or_else(|_| plain_search_path(os))
}

/// The environment an MCP server starts with: its `PATH` and its own
/// folder for home and temporary files — never the person's — and on
/// Windows what every program needs to start (`SystemRoot`: no network
/// without it), its home folders pointed at that folder too.
pub fn mcp_environment(scratch: &Path) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        vec![("PATH".into(), mcp_search_path())];
    match crate::platform::OS {
        Os::Linux | Os::MacOs => {
            env.push(("LANG".into(), "C.UTF-8".into()));
            env.push(("HOME".into(), scratch.into()));
            env.push(("TMPDIR".into(), scratch.into()));
        }
        Os::Windows => {
            for key in [
                "PATHEXT",
                "SystemRoot",
                "SystemDrive",
                "windir",
                "ComSpec",
                "OS",
                "NUMBER_OF_PROCESSORS",
                "PROCESSOR_ARCHITECTURE",
                "ProgramData",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "CommonProgramFiles",
                "CommonProgramFiles(x86)",
                "CommonProgramW6432",
            ] {
                if let Some(value) = std::env::var_os(key) {
                    env.push((key.into(), value));
                }
            }
            for key in ["USERPROFILE", "TEMP", "TMP"] {
                env.push((key.into(), scratch.into()));
            }
            env.push(("APPDATA".into(), scratch.join(r"AppData\Roaming").into()));
            env.push(("LOCALAPPDATA".into(), scratch.join(r"AppData\Local").into()));
        }
    }
    env
}

/// Dotfiles and toolchains under `$HOME` that build tools read, on Linux.
/// Everything else in the home directory stays invisible.
const HOME_READS: &[&str] = &[
    ".rustup",
    ".cargo",
    ".local/bin",
    ".gitconfig",
    ".npmrc",
    ".config/git",
    ".nvm",
];

/// The same on `os`: on Windows also where a person's own installs live —
/// npm's global commands, Scoop, WinGet, per-user programs (Python, Node),
/// nvm-windows, .NET tools.
fn home_reads(os: Os) -> &'static [&'static str] {
    const WINDOWS: &[&str] = &[
        ".rustup",
        ".cargo",
        ".gitconfig",
        ".npmrc",
        ".config/git",
        "AppData/Roaming/npm",
        "AppData/Roaming/nvm",
        "AppData/Local/Programs",
        "AppData/Local/Microsoft/WinGet/Packages",
        "scoop",
        ".dotnet/tools",
    ];
    match os {
        Os::Linux | Os::MacOs => HOME_READS,
        Os::Windows => WINDOWS,
    }
}

/// Cache directories under `$HOME` that build tools *write*, on Linux. Narrow
/// on purpose: `~/.cargo` as a whole would include `config.toml`, and whoever
/// writes that chooses the linker for every future build the user runs —
/// outside any sandbox.
const HOME_WRITES: &[&str] = &[
    ".cache",
    ".cargo/registry",
    ".cargo/git",
    ".cargo/.package-cache",
    ".npm/_cacache",
    ".local/share/pnpm/store",
];

/// The same on `os`: macOS's per-user caches (`Library/Caches`: pip, Go,
/// Yarn, Homebrew); Windows's npm, pip, NuGet and Go caches.
fn home_writes(os: Os) -> &'static [&'static str] {
    const MACOS: &[&str] = &[
        ".cache",
        ".cargo/registry",
        ".cargo/git",
        ".cargo/.package-cache",
        ".npm/_cacache",
        ".local/share/pnpm/store",
        "Library/Caches",
    ];
    const WINDOWS: &[&str] = &[
        ".cache",
        ".cargo/registry",
        ".cargo/git",
        ".cargo/.package-cache",
        "AppData/Local/npm-cache",
        "AppData/Local/pip/Cache",
        "AppData/Local/pnpm/store",
        "AppData/Local/go-build",
        "go/pkg/mod",
        ".nuget/packages",
    ];
    match os {
        Os::Linux => HOME_WRITES,
        Os::MacOs => MACOS,
        Os::Windows => WINDOWS,
    }
}

impl Policy {
    /// `scratch` is this session's own temp directory. `/tmp` itself is **not**
    /// granted: it is world-writable and shared with everything else the user
    /// runs, and a project opened under `/tmp` would otherwise have its whole
    /// parent tree exposed. Commands get `TMPDIR` pointed here instead.
    pub fn for_project(project: &Path, scratch: &Path) -> Policy {
        let os = crate::platform::OS;
        let mut writes = vec![scratch.to_path_buf()];
        let mut reads: Vec<PathBuf> = system_reads(os);

        // Granted for real by every confinement (`platform::Confinement`),
        // with the extra rights a terminal needs. Recorded here as well so
        // `reachable` gives the same answer the kernel will: without this,
        // `curl -o /dev/null` raised a permission prompt for a path every
        // command on the machine already uses.
        reads.push(PathBuf::from(crate::platform::DEVICES));
        reads.extend(package_stores());
        reads.extend(resolver_files(os));

        if let Some(home) = crate::platform::home_dir() {
            writes.extend(home_writes(os).iter().map(|p| home.join(p)));
            reads.extend(home_reads(os).iter().map(|p| home.join(p)));
        }
        // Where the system keeps this user's caches and temporary files for
        // its own tools (macOS: a compiler's module cache), besides `TMPDIR`.
        writes.extend(crate::platform::system_caches());

        Policy {
            project: project.to_path_buf(),
            scratch: scratch.to_path_buf(),
            writes,
            reads,
            reach: Vec::new(),
            ledger: None,
            local_services: false,
            ask_service: None,
        }
    }

    /// The same policy, its confinement recording what it leaves on disk in
    /// `ledger` ([`grants_ledger`]).
    pub fn recorded_in(mut self, ledger: PathBuf) -> Policy {
        self.ledger = Some(ledger);
        self
    }

    /// The same policy, plus the paths the user has allowed. Consumed and
    /// returned rather than mutated so a caller cannot hold a policy that is
    /// half-widened while a ruleset is being built from it.
    pub fn with_reach(mut self, reach: Vec<PathBuf>) -> Policy {
        self.reach = reach;
        self
    }

    /// The same policy, its commands allowed local services or not.
    pub fn with_local_services(mut self, allowed: bool) -> Policy {
        self.local_services = allowed;
        self
    }

    /// The same policy, asking `ask` about other programs' sockets.
    pub fn asking_services(mut self, ask: Option<AskService>) -> Policy {
        self.ask_service = ask;
        self
    }

    /// Could a spawned command already open `path` under this policy?
    ///
    /// Asked before a permission prompt is built, so the user is never shown a
    /// grant they do not need. `/usr/bin/python3` names a path outside the
    /// workspace and is readable by every command already — prompting for it
    /// would put a dialog in front of half the commands an agent runs, and
    /// dialogs nobody needs are how a person learns to press `y` without
    /// reading.
    ///
    /// Read and write are deliberately not distinguished: the system
    /// directories are granted read-only, so a *write* into one of them is
    /// already covered by a rule and the kernel — not the gate — has the last
    /// word. Nothing here is ever widened by asking.
    ///
    /// Compared as the OS compares names: a Windows path in another case is
    /// the same path (`jail::starts_with_folded`).
    pub fn reachable(&self, path: &Path) -> bool {
        let os = crate::platform::OS;
        let under = |root: &Path| crate::jail::starts_with_folded(path, root, os);
        [self.project.as_path(), self.scratch.as_path()]
            .into_iter()
            .chain(self.writes.iter().map(PathBuf::as_path))
            .chain(self.reads.iter().map(PathBuf::as_path))
            .chain(self.reach.iter().map(PathBuf::as_path))
            .any(under)
    }
}

/// A prepared ruleset, ready to be applied to child processes. Build once per
/// session; one ruleset can confine any number of spawns.
pub struct Sandbox {
    /// The kernel's hold on a spawned command; `None` when nothing holds it,
    /// and the report says why.
    confinement: Option<crate::platform::Confinement>,
    report: SandboxReport,
    scratch: PathBuf,
    /// The policy this was built from, kept so the permission gate can ask what
    /// is *already* reachable without rebuilding it and getting a different
    /// answer.
    policy: Policy,
}

/// Whether an MCP server's root collides — either way — with what no root may
/// hold: the system's own folders (`denylist::protected_prefixes`) and Rune's
/// home, whose `run/` keys drive a session host. No other folder is special
/// (owner, 2026-10-17).
fn mcp_root_refused(root: &Path, rune_home: &Path) -> bool {
    let os = crate::platform::OS;
    crate::perm::denylist::protected_prefixes(os)
        .iter()
        .map(PathBuf::as_path)
        .chain([rune_home])
        .any(|held| {
            crate::jail::starts_with_folded(root, held, os)
                || crate::jail::starts_with_folded(held, root, os)
        })
}

impl Sandbox {
    /// Persistent integrations receive an explicit profile, independent of the conversation mode.
    pub fn for_mcp(
        project: &Path,
        scratch: &Path,
        home: &Path,
        server: &crate::capabilities::config::Server,
    ) -> crate::Result<std::sync::Arc<Self>> {
        use std::sync::Arc;
        for root in server.read_roots.iter().chain(&server.write_roots) {
            if mcp_root_refused(root, home) {
                return Err(crate::Error::Denied(
                    "MCP profile includes a protected host path".into(),
                ));
            }
        }
        if home.starts_with(project) {
            return Err(crate::Error::Denied(
                "MCP workspace contains Rune's protected state".into(),
            ));
        }
        let mut policy = Policy::for_project(scratch, scratch).recorded_in(grants_ledger(home));
        let os = crate::platform::OS;
        policy.reads = mcp_system_reads(os)
            .into_iter()
            .chain(mcp_network_reads(os))
            .chain(resolver_files(os))
            .chain(package_stores())
            .collect();
        policy.reads.push(project.to_path_buf());
        if let Some(executable) = &server.command {
            policy.reads.push(executable.clone());
        }
        policy.reads.extend(server.read_roots.iter().cloned());
        policy.writes = server.write_roots.clone();
        if server.project_write {
            policy.writes.push(project.to_path_buf());
        }
        policy.local_services = server.allow_host_sockets;
        let sandbox = Self::prepare(&policy);
        if !sandbox.enforced() {
            return Err(crate::Error::Denied(
                "MCP process confinement is unavailable; the server was not started".into(),
            ));
        }
        Ok(Arc::new(sandbox))
    }
    /// Never fails. Either it is enforcing, or it says why it is not.
    pub fn prepare(policy: &Policy) -> Sandbox {
        // The scratch directory has to exist before it can be granted.
        let _ = std::fs::create_dir_all(&policy.scratch);
        Self::probe(policy)
    }

    /// What confinement `policy` would get, built without creating anything:
    /// for a frontend that has no session yet and only reports what would
    /// apply. Its report is the one [`Sandbox::prepare`] would give — a write
    /// root that does not exist is skipped either way — and it leaves nothing
    /// on disk (0.1.5's probe left `scratch/probe` at the top of the home).
    pub fn probe(policy: &Policy) -> Sandbox {
        // A command's own sockets: in its project, its scratch, and what the
        // person let it reach.
        let sockets: Vec<PathBuf> = [policy.project.clone(), policy.scratch.clone()]
            .into_iter()
            .chain(policy.reach.iter().cloned())
            .collect();
        let built = crate::platform::Confinement::build(crate::platform::Grants {
            project: &policy.project,
            ledger: policy.ledger.as_deref(),
            reads: &policy.reads,
            writes: &policy.writes,
            reach: &policy.reach,
            local_services: policy.local_services,
            sockets: &sockets,
            ask_service: policy.ask_service.as_ref().map(|ask| ask.0.clone()),
        });
        match built {
            Ok(confinement) => Sandbox {
                report: SandboxReport {
                    kind: confinement.kind().into(),
                    abi: confinement.abi(),
                    enforced: true,
                    reason: None,
                    escapes: {
                        let mut escapes = known_escapes();
                        escapes.extend(confinement.gaps().into_iter().map(|gap| Escape {
                            id: gap.id.into(),
                            detail: gap.detail,
                            fix: gap.fix.into(),
                        }));
                        escapes
                    },
                },
                confinement: Some(confinement),
                scratch: policy.scratch.clone(),
                policy: policy.clone(),
            },
            Err(unconfined) => Sandbox::off(unconfined.abi, &unconfined.reason, policy),
        }
    }

    /// Not enforcing, and saying why. Never silent.
    fn off(abi: i32, reason: &str, policy: &Policy) -> Sandbox {
        Sandbox {
            confinement: None,
            report: SandboxReport {
                kind: "none".into(),
                abi,
                enforced: false,
                reason: Some(reason.to_string()),
                escapes: known_escapes(),
            },
            scratch: policy.scratch.clone(),
            policy: policy.clone(),
        }
    }

    /// Confinement deliberately switched off, because the session is in
    /// full-access mode. Reported exactly like any other unenforced sandbox —
    /// the frontend must be able to show the user that nothing is holding the
    /// agent back, and the reason says it was a choice, not a failure.
    pub fn full_access(policy: &Policy) -> Sandbox {
        let _ = std::fs::create_dir_all(&policy.scratch);
        Sandbox::off(
            0,
            "full-access mode: the workspace jail and the kernel sandbox are both off by choice",
            policy,
        )
    }

    /// Where a command should put its temporary files.
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    pub fn report(&self) -> &SandboxReport {
        &self.report
    }

    pub fn enforced(&self) -> bool {
        self.report.enforced
    }

    /// Whether a command spawned under this sandbox could already reach `path`.
    /// Always true when nothing is being enforced.
    pub fn reachable(&self, path: &Path) -> bool {
        !self.report.enforced || self.policy.reachable(path)
    }

    /// Whether a command spawned under this sandbox may reach local services:
    /// always where nothing confines it.
    pub fn local_services(&self) -> bool {
        self.confinement
            .as_ref()
            .is_none_or(|confinement| confinement.local_services())
    }

    /// What confines a command spawned under this sandbox, for
    /// `platform::place`; `None` when nothing is being enforced.
    pub fn confinement(&self) -> Option<&crate::platform::Confinement> {
        self.confinement.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owner, 2026-10-16: on NixOS (and Guix) every program and library lives
    /// in a read-only package store, and no confined command could run. A
    /// store is granted only where the machine has one.
    #[test]
    fn package_stores_are_granted_only_where_they_exist() {
        let base = std::env::temp_dir().join(format!("rune-stores-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("nix/store")).unwrap();
        let there = base.join("nix/store");
        let absent = base.join("gnu/store");
        assert_eq!(
            present(&[there.to_str().unwrap(), absent.to_str().unwrap()]),
            vec![there.clone()]
        );
        let policy = Policy::for_project(&base, &base);
        for store in package_stores() {
            assert!(policy.reads.contains(&store));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An MCP server finds its programs wherever this system keeps them — a
    /// link into a package store included — and never through a folder its
    /// confinement would not let it run from.
    #[test]
    fn an_mcp_search_path_keeps_only_folders_the_server_may_run_from() {
        let base = std::env::temp_dir().join(format!("rune-mcp-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store = base.join("store/abc-system/bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(base.join("home/bin")).unwrap();
        std::fs::create_dir_all(base.join("run")).unwrap();
        crate::platform::fixture::symlink(&store, base.join("run/sw-bin")).unwrap();
        let roots = vec![crate::platform::canonical(&base.join("store")).unwrap()];
        let path = std::env::join_paths([
            base.join("home/bin"),
            base.join("run/sw-bin"),
            "relative/bin".into(),
            base.join("missing"),
        ])
        .unwrap();
        let os = crate::platform::OS;
        assert_eq!(
            search_path_within(Some(path), &roots, os),
            std::ffi::OsString::from(base.join("run/sw-bin"))
        );
        assert_eq!(search_path_within(None, &roots, os), plain_search_path(os));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn policy(project: &str) -> Policy {
        Policy::for_project(Path::new(project), Path::new("/tmp/rune-test/scratch"))
    }

    #[test]
    fn policy_grants_scratch_but_not_the_whole_home() {
        let p = policy("/tmp/whatever");
        assert_eq!(p.project, Path::new("/tmp/whatever"));
        assert!(p
            .writes
            .iter()
            .any(|w| w == Path::new("/tmp/rune-test/scratch")));
        assert!(
            !p.writes.iter().any(|w| w == Path::new("/tmp")),
            "shared /tmp must not be granted"
        );

        let home = crate::platform::home_dir().unwrap();
        assert!(
            !p.writes.contains(&home),
            "the home directory must never be writable"
        );
        assert!(
            !p.reads.contains(&home),
            "the home directory must never be readable wholesale"
        );
        // The cache is writable; the config that sits beside it is not.
        assert!(p.writes.iter().any(|w| w.ends_with(".cargo/registry")));
        assert!(!p.writes.iter().any(|w| w.ends_with(".cargo")));
    }

    /// Every OS has its own tables, and none grants a whole home, the root,
    /// or where its container daemons listen.
    #[test]
    fn every_os_keeps_its_own_system_and_home_tables() {
        let home = Path::new("/h");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert!(
                !system_reads(os).iter().any(|r| r.parent().is_none()),
                "{os:?}"
            );
            assert!(!home_writes(os).contains(&".cargo"), "{os:?}");
            assert!(!home_reads(os).is_empty() && !home_writes(os).is_empty());
            assert!(!plain_search_path(os).is_empty());
            for daemon in daemon_sockets(os, Some(home)) {
                assert!(!system_reads(os)
                    .iter()
                    .any(|r| daemon.starts_with(r) && r.ends_with("run")));
            }
        }
        assert!(system_reads(Os::MacOs).contains(&PathBuf::from("/private/var/select")));
        assert!(home_writes(Os::MacOs).contains(&"Library/Caches"));
        assert!(home_reads(Os::Windows).contains(&"AppData/Local/Programs"));
        assert!(daemon_sockets(Os::Windows, Some(home)).is_empty());
        assert!(
            daemon_sockets(Os::MacOs, Some(home)).contains(&home.join(".docker/run/docker.sock"))
        );
        assert!(mcp_network_reads(Os::Windows).is_empty());
    }

    /// Nothing under `/run` is granted but the resolver's own file, read
    /// only: never `/run` or a folder in it, where daemons keep their sockets.
    #[test]
    fn nothing_grants_run_where_daemon_sockets_live() {
        let p = policy("/tmp/x");
        for path in &p.writes {
            assert!(!path.starts_with("/run"), "{path:?} exposes daemon sockets");
        }
        for path in &p.reads {
            assert!(
                !path.starts_with("/run") || path.is_file(),
                "{path:?} exposes daemon sockets"
            );
        }
    }

    /// A resolver file reached through a link is granted as its target, a
    /// file; one that is no link adds nothing.
    #[test]
    fn the_resolver_is_read_where_its_link_points() {
        for path in resolver_files(crate::platform::OS) {
            assert!(path.is_file(), "{path:?}");
            assert!(path.is_absolute());
        }
        assert!(resolver_files(Os::Windows).is_empty());
        if let Ok(real) = crate::platform::canonical(Path::new("/etc/resolv.conf")) {
            let policy = policy("/tmp/x");
            assert!(
                policy.reachable(&real),
                "{real:?}: a confined command reads the file names are resolved by"
            );
        }
    }

    #[test]
    fn the_filesystem_root_is_never_granted() {
        let p = policy("/tmp/x");
        assert!(!p.reads.iter().any(|r| r == Path::new("/")));
        assert!(!p.writes.iter().any(|w| w == Path::new("/")));
    }

    #[test]
    fn a_reachable_container_daemon_is_reported_as_an_escape() {
        let dir = std::env::temp_dir();
        let s = Sandbox::prepare(&Policy::for_project(
            &dir,
            Path::new("/tmp/rune-test/scratch"),
        ));
        let daemons = reachable_daemons();
        let escapes: Vec<_> = s
            .report()
            .escapes
            .iter()
            .filter(|e| e.id == "container_daemon")
            .collect();
        assert_eq!(
            escapes.len(),
            daemons.len(),
            "every reachable daemon socket must be reported"
        );
        if matches!(
            s.report().kind.as_str(),
            "restricted-token" | "sandbox-account"
        ) {
            assert!(
                s.report()
                    .escapes
                    .iter()
                    .any(|e| e.id == "readable_by_every_user"),
                "what a Windows confinement leaves readable is reported"
            );
        }
        assert_eq!(
            s.report().escapes.iter().any(|e| e.id == "elevated"),
            crate::platform::running_elevated(),
            "running as root or Administrator is said"
        );
        for e in escapes {
            assert_eq!(e.id, "container_daemon");
            assert!(
                !e.fix.is_empty(),
                "an escape without a fix is just bad news"
            );
        }
        if daemons.is_empty() {
            eprintln!("note: no container daemon reachable here, nothing to report");
        }
    }

    #[test]
    fn prepare_always_returns_an_honest_report() {
        let dir = std::env::temp_dir();
        let s = Sandbox::prepare(&Policy::for_project(
            &dir,
            Path::new("/tmp/rune-test/scratch"),
        ));
        let r = s.report();
        if r.enforced {
            let (kinds, abi): (&[&str], i32) = match crate::platform::OS {
                Os::Linux => (&["landlock"], 1),
                Os::MacOs => (&["seatbelt"], 0),
                Os::Windows => (&["restricted-token", "sandbox-account"], 0),
            };
            assert!(
                kinds.contains(&r.kind.as_str()),
                "unexpected kind {}",
                r.kind
            );
            assert!(r.abi >= abi);
            assert!(r.reason.is_none());
        } else {
            // Not enforcing is allowed; being quiet about it is not.
            assert!(r.reason.is_some());
        }
    }

    #[test]
    fn missing_project_directory_disables_enforcement_loudly() {
        let s = Sandbox::prepare(&Policy::for_project(
            Path::new("/nonexistent-rune-project"),
            Path::new("/tmp/rune-test/scratch"),
        ));
        assert!(!s.enforced());
        assert!(s.report().reason.is_some());
    }
}
