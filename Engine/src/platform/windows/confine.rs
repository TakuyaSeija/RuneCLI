//! Confinement of the commands Rune starts on Windows: a restricted token
//! and ACL entries.
//!
//! Windows has no Landlock: what a process may open is decided by each
//! object's ACL against the process's token. A confined command runs with a
//! restricted token — every privilege dropped, Administrators deny-only even
//! in an Administrator terminal, medium integrity at most — whose access is
//! checked twice: once as the user, once against its restricting SIDs. So an
//! object is reached only when the user may reach it AND an entry allows one
//! of those SIDs.
//!
//! Each granted path has a SID of its own, derived from the path and the
//! access (`write:<path>`, `read:<path>`), and an inherited entry for that SID
//! on the path. A confinement's token carries exactly the SIDs of its grants,
//! as a Landlock ruleset holds exactly its rules: project B's commands never
//! carry project A's SID, so an entry left on A after a session ends lets in
//! nobody but a later confinement granted A by the same decision. The entries
//! are written the first time a command is started under the confinement —
//! building one (`Sandbox::probe` reports what would apply) leaves nothing on
//! disk — and Windows hands an inherited entry down to what a folder holds,
//! once. Each is recorded in the ledger before it is written (`ledger.rs`),
//! so `rune -sandbox reset` takes every one back; a confinement given no
//! ledger writes none.
//!
//! Windows's own folders get no entry, nor does anything in them or holding
//! them — a drive's root, `C:\Users` ([`windows_own`]): an Administrator's
//! Rune could write one, and an elevated terminal starts in `System32`. What
//! a command needs of them it reads through the other restricting SIDs, as
//! every program of the user does.
//!
//! What the token's other restricting SIDs open stays open, as it does for any
//! program of the user: the system folders and Program Files (readable by
//! every user), what Windows lets every user read or write
//! (Everyone, Users: ProgramData, Public, the Windows temp folder), the
//! desktop of the logon session, the user's registry hive (RESTRICTED). Not
//! the user's own files: a profile grants none of those SIDs.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, GetLengthSid, SetTokenInformation, TokenDefaultDacl,
    TokenIntegrityLevel, WinAuthenticatedUserSid, WinBuiltinAdministratorsSid,
    WinBuiltinAnyPackageSid, WinBuiltinUsersSid, WinLocalSystemSid, WinMediumLabelSid,
    WinRestrictedCodeSid, WinWorldSid, ACL, DISABLE_MAX_PRIVILEGE, NO_INHERITANCE,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::SystemServices::{SE_GROUP_ENABLED, SE_GROUP_INTEGRITY};

use super::security::{grant, revoke, Dacl, Sid, Token};
use super::wide::{check, owned, wide_path, win32, Local};
use crate::platform::ledger::{Forgotten, Ledger, RecordedGrant, Taken};

/// A restricted token ready to start any number of confined commands, and
/// the entries it needs on disk. Cloning shares it.
#[derive(Clone)]
pub struct Confinement {
    inner: Arc<Inner>,
}

struct Inner {
    /// What its commands start as: the restricted token; or, in an
    /// AppContainer, the token the container is made from (privileges
    /// dropped, Administrators deny-only, no restricting SID). `None` when
    /// commands start inside the sandbox-account runner.
    token: Option<OwnedHandle>,
    mechanism: Mechanism,
    /// The AppContainer its commands run in, for [`Mechanism::AppContainer`].
    container: Option<Container>,
    /// The dedicated local sandbox account and its staging folder, for
    /// [`Mechanism::Account`].
    account: Option<(super::account::SandboxAccount, PathBuf)>,
    /// Unique per-session key (the session's canonical scratch folder) so
    /// shared project and reach grants stay on disk while another open
    /// session still holds them.
    session_key: Option<PathBuf>,
    /// Each granted path, its SID, its access, and whether it is a folder;
    /// written on disk at the first start (`Grants written`).
    grants: Vec<Grant>,
    /// Where each entry is recorded before it is written; `None`: none is.
    ledger: Option<PathBuf>,
    /// [`Grants::local_services`], as it was asked for.
    local_services: bool,
    written: Mutex<bool>,
}

struct Grant {
    path: PathBuf,
    sid: Sid,
    access: u32,
    /// Writing granted, not only reading.
    write: bool,
    folder: bool,
    /// The project: a confinement whose project cannot be granted is none.
    required: bool,
    /// Revoked when the session ends (`project`, `scratch`, `reach`).
    session_scoped: bool,
}

/// How a confined command is kept to its grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mechanism {
    /// A restricted token whose restricting SIDs are the grants' and the
    /// few every program needs (Everyone, Users, RESTRICTED, the logon
    /// session). Named objects and pipes are the user's own namespace.
    RestrictedToken,
    /// No restricting SID at all — one access check, as the account — with
    /// Administrators and Authenticated Users deny-only: what Windows lets
    /// every signed-in user change (a folder made at a drive's root) is not
    /// this token's. For an account of Rune's own, whose grants are entries
    /// for that account.
    DenyOnlyToken,
    /// A dedicated local user account (`rune-sbx-<8hex>`) running commands
    /// through the persistent runner under a deny-only token, with ACL
    /// grants naming that account's SID.
    Account,
    /// An AppContainer: a package SID and the grants as capabilities. Named
    /// objects live in the container's own namespace; the user's pipes and
    /// local servers are out of reach unless granted. `profile`: whether its
    /// profile (a folder and a registry key of the user's) is made first;
    /// `filtered`: whether it is made from a token whose privileges and
    /// Administrators are dropped first.
    AppContainer { profile: bool, filtered: bool },
}

/// An AppContainer a confinement starts its commands in.
struct Container {
    name: String,
    package: Sid,
    /// The grants' SIDs and the network's, each enabled.
    capabilities: Vec<Sid>,
    profile: bool,
}

/// What `CreateProcess` (or the sandbox-account runner) is given to start a
/// confined command; what it points at is the confinement's own and outlives
/// the call.
pub(super) struct Start {
    /// The token to start as; `None`: this process's own or the runner's.
    pub(super) token: Option<HANDLE>,
    /// The container to start in, for `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`.
    pub(super) container: Option<(SECURITY_CAPABILITIES, Vec<SID_AND_ATTRIBUTES>)>,
    /// The sandbox account and staging home when the command runs through
    /// the persistent runner.
    pub(super) account: Option<(super::account::SandboxAccount, PathBuf)>,
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
    /// Where this OS records what a confinement leaves on disk, so a person
    /// can take it back (`forget_grants`): Windows's ACL entries, each
    /// recorded before it is written. `None`: nothing may be left, and a
    /// confinement that would have to write an entry starts nothing.
    pub ledger: Option<&'a Path>,
    /// Full rights, each answered for by the person by name; a file among
    /// them is granted as a file.
    pub reach: &'a [PathBuf],
    /// Whether a confined command may reach programs already running on
    /// this machine (Unix sockets elsewhere). Recorded for the contract: a
    /// restricted token is refused every pipe or socket file whose ACL names
    /// only the user, as programs of the user make theirs.
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
    /// Landlock's ABI on Linux; Windows has none, so 0.
    pub abi: i32,
    pub reason: String,
}

/// Where Windows keeps its devices (`\\.\NUL`, `\\.\CON`): nothing Rune
/// grants by an ACL entry — a command reaches a device through the handles
/// it is given and the devices' own security — but named so a policy can say
/// a command already reaches them.
pub const DEVICES: &str = r"\\.\";

/// The canonical spelling of an existing path, which names one SID whatever
/// case or link it was given in; `None` when it does not exist.
fn canonical(path: &Path) -> Option<PathBuf> {
    super::canonical(path).ok()
}

/// The SID Rune grants `path` under, for writing or only reading: the same
/// SID for the same path however it is spelt — Windows names compare
/// without case.
fn sid_for(mechanism: Mechanism, write: bool, path: &Path) -> Sid {
    let access = if write { "write" } else { "read" };
    let name = format!(
        "{access}:{}",
        path.as_os_str().to_string_lossy().to_uppercase()
    );
    match mechanism {
        Mechanism::RestrictedToken | Mechanism::DenyOnlyToken | Mechanism::Account => {
            Sid::derived(&name)
        }
        Mechanism::AppContainer { .. } => Sid::capability(&name),
    }
}

/// Why `path` is Windows's own and gets no entry from Rune: the root of a
/// drive; the Windows folder, Program Files or ProgramData, or anything in
/// them; or a folder holding one of them or every user's profile
/// (`C:\Users`). An entry handed down from any of these reaches every file
/// below it — the whole system, the whole drive, other people's homes.
/// `None` for a path of the person's own.
fn windows_own(path: &Path) -> Option<String> {
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86,
        FOLDERID_UserProfiles, FOLDERID_Windows,
    };
    if path.parent().is_none() {
        return Some(format!("{} is the root of a drive", path.display()));
    }
    // (folder, whether what it holds is Windows's too) — where Windows keeps
    // them does not change while Rune runs.
    static FOLDERS: OnceLock<Vec<(PathBuf, bool)>> = OnceLock::new();
    let folders = FOLDERS.get_or_init(|| {
        [
            (&FOLDERID_Windows, true),
            (&FOLDERID_ProgramFiles, true),
            (&FOLDERID_ProgramFilesX86, true),
            (&FOLDERID_ProgramData, true),
            (&FOLDERID_UserProfiles, false),
        ]
        .into_iter()
        .filter_map(|(id, holds_too)| {
            let folder = super::home::known_folder(id).and_then(|f| canonical(&f))?;
            Some((folder, holds_too))
        })
        .collect()
    });
    for (folder, holds_too) in folders {
        if within(folder, path) || (*holds_too && within(path, folder)) {
            return Some(format!(
                "{} is Windows's own or holds what is ({})",
                path.display(),
                folder.display()
            ));
        }
    }
    None
}

/// Whether `path` is `folder` or inside it, Windows's way: without case.
fn within(path: &Path, folder: &Path) -> bool {
    let fold = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    };
    let (path, folder) = (fold(path), fold(folder));
    path.len() >= folder.len() && path[..folder.len()] == folder[..]
}

impl Confinement {
    /// Build the confinement `grants` describe: using the dedicated local
    /// sandbox account when available, or falling back to a restricted token
    /// of the current user. Nothing is written on the granted paths until the
    /// first command starts.
    pub fn build(grants: Grants<'_>) -> Result<Confinement, Unconfined> {
        if let Some((account, staging_home)) = super::account::resolve_for_confinement() {
            return Confinement::build_with_account(
                grants,
                Mechanism::Account,
                Some((account, staging_home)),
            );
        }
        Confinement::build_as(grants, Mechanism::RestrictedToken)
    }

    /// [`Confinement::build`] by `mechanism`.
    pub(super) fn build_as(
        grants: Grants<'_>,
        mechanism: Mechanism,
    ) -> Result<Confinement, Unconfined> {
        Confinement::build_with_account(grants, mechanism, None)
    }

    fn build_with_account(
        grants: Grants<'_>,
        mechanism: Mechanism,
        account: Option<(super::account::SandboxAccount, PathBuf)>,
    ) -> Result<Confinement, Unconfined> {
        let unconfined = |reason: String| Unconfined { abi: 0, reason };
        let project = canonical(grants.project)
            .ok_or_else(|| unconfined("the project directory could not be granted".into()))?;
        if let Some(why) = windows_own(&project) {
            return Err(unconfined(format!(
                "the project directory could not be granted: {why}, and Rune writes no access entry there"
            )));
        }
        may_change_access(&project).map_err(|e| {
            unconfined(format!(
                "the project directory could not be granted: Windows lets this user not change who may reach it ({e})"
            ))
        })?;
        let session_key = grants
            .writes
            .first()
            .and_then(|p| canonical(p))
            .or_else(|| Some(project.clone()));
        let grant_sid = |write: bool, path: &Path| -> Sid {
            match &account {
                Some((acct, _)) => acct.sid.clone(),
                None => sid_for(mechanism, write, path),
            }
        };
        let mut planned = vec![Grant {
            sid: grant_sid(true, &project),
            path: project,
            access: FILE_ALL_ACCESS,
            write: true,
            folder: true,
            required: true,
            session_scoped: true,
        }];
        let add = |planned: &mut Vec<Grant>, path: &Path, write: bool, session_scoped: bool| {
            if path.as_os_str() == DEVICES {
                return;
            }
            let Some(path) = canonical(path) else {
                return;
            };
            if windows_own(&path).is_some() {
                return;
            }
            if planned
                .iter()
                .any(|grant: &Grant| grant.path == path && (grant.write || !write))
            {
                return;
            }
            let sid = grant_sid(write, &path);
            planned.push(Grant {
                sid,
                folder: path.is_dir(),
                path,
                access: if write {
                    FILE_ALL_ACCESS
                } else {
                    FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
                },
                write,
                required: false,
                session_scoped,
            });
        };
        for (idx, path) in grants.writes.iter().enumerate() {
            add(&mut planned, path, true, idx == 0);
        }
        for path in grants.reach {
            add(&mut planned, path, true, true);
        }
        for path in grants.reads {
            add(&mut planned, path, false, false);
        }
        let sids: Vec<Sid> = planned.iter().map(|grant| grant.sid.clone()).collect();
        let (token, container) = match mechanism {
            Mechanism::Account => (None, None),
            Mechanism::RestrictedToken | Mechanism::DenyOnlyToken => (
                Some(
                    restricted_token(&sids, None, mechanism == Mechanism::DenyOnlyToken).map_err(
                        |e| unconfined(format!("Windows refused a restricted token: {e}")),
                    )?,
                ),
                None,
            ),
            Mechanism::AppContainer { profile, filtered } => {
                let container = container(&planned[0].path, sids, profile)
                    .map_err(|e| unconfined(format!("Windows refused an AppContainer: {e}")))?;
                let token = if filtered {
                    restricted_token(&[], Some(&container.package), false)
                } else {
                    own_token()
                }
                .map_err(|e| unconfined(format!("Windows refused a token: {e}")))?;
                (Some(token), Some(container))
            }
        };
        Ok(Confinement {
            inner: Arc::new(Inner {
                token,
                mechanism,
                container,
                account,
                session_key,
                grants: planned,
                ledger: grants.ledger.map(Path::to_path_buf),
                local_services: grants.local_services,
                written: Mutex::new(false),
            }),
        })
    }

    /// The mechanism's name in the sandbox report.
    pub fn kind(&self) -> &'static str {
        match self.inner.mechanism {
            Mechanism::Account => "sandbox-account",
            Mechanism::RestrictedToken => "restricted-token",
            Mechanism::DenyOnlyToken => "deny-only-token",
            Mechanism::AppContainer { .. } => "appcontainer",
        }
    }

    /// Landlock's ABI on Linux; Windows has none.
    pub fn abi(&self) -> i32 {
        0
    }

    /// What the Windows confinement leaves reachable (CI probe, 2026-10-18):
    /// `Users` and `Everyone` remain enabled, so what Windows lets every user
    /// read stays readable, and `ProgramData` — where `Users` may create —
    /// writable.
    /// A pipe another program made with Windows's default security admits
    /// `Everyone` to read (CI measurement, 2026-10-19: a confined command
    /// opened one to read and was refused to write) — no restricting SID
    /// can take that away without breaking Git Bash and Windows's HTTPS.
    /// Under the restricted token, also why Rune's own account is not in
    /// use: never a silent fallback.
    pub fn gaps(&self) -> Vec<crate::platform::Gap> {
        let readable = crate::platform::Gap {
            id: "readable_by_every_user",
            detail: "A confined command can still read what Windows lets every user \
                     read — folders at a drive's root, other drives, ProgramData — and \
                     create files in ProgramData. Your profile folder and your other \
                     programs stay out of its reach."
                .into(),
            fix: "Keep what no command may read inside your profile folder, or give \
                  such a folder an access list that names only you.",
        };
        let pipes = crate::platform::Gap {
            id: "pipes_readable_by_every_user",
            detail: "A confined command can open, to read only, a named pipe another \
                     program made with Windows's default security, and receive what that \
                     program sends on it. Writing to such a pipe is refused, and Rune's own \
                     pipes admit only you."
                .into(),
            fix: "A program whose pipe must stay private gives it an access list that \
                  names only its user.",
        };
        match self.inner.mechanism {
            Mechanism::Account => vec![readable, pipes],
            Mechanism::RestrictedToken => {
                let mut gaps = vec![readable, pipes];
                if let Some(why) = super::account::not_in_use() {
                    gaps.push(crate::platform::Gap {
                        id: "sandbox_account_not_in_use",
                        detail: format!(
                            "Commands run under a restricted token of your own account, not \
                             Rune's sandbox account: {why}. Git Bash and Windows's own HTTPS \
                             cannot run confined this way."
                        ),
                        fix: "Run `rune -sandbox setup` and confirm the administrator prompt.",
                    });
                }
                gaps
            }
            Mechanism::DenyOnlyToken | Mechanism::AppContainer { .. } => Vec::new(),
        }
    }

    /// Whether a command it confines may reach local services
    /// ([`Grants::local_services`]).
    pub fn local_services(&self) -> bool {
        self.inner.local_services
    }

    /// Revoke the sandbox account's access entries on `paths` when a one-call
    /// reach grant (`narrow`) ends, unless another open session still holds
    /// the path.
    pub fn revoke_paths(&self, paths: &[PathBuf]) {
        let Some((acct, staging_home)) = &self.inner.account else {
            return;
        };
        revoke_held_paths(
            self.inner.session_key.as_deref(),
            paths,
            &acct.sid,
            staging_home,
        );
    }

    /// Revoke the session-scoped access entries (`project`, `scratch`,
    /// `reach`) written for the sandbox account when a session closes,
    /// keeping any entry still held by another open session.
    pub fn revoke_session_grants(&self) {
        let Some((acct, staging_home)) = &self.inner.account else {
            return;
        };
        let paths: Vec<PathBuf> = self
            .inner
            .grants
            .iter()
            .filter(|g| g.session_scoped)
            .map(|g| g.path.clone())
            .collect();
        release_session_holds(
            self.inner.session_key.as_deref(),
            &paths,
            &acct.sid,
            staging_home,
        );
    }

    /// Ensure this confinement's grants are recorded and written on disk
    /// before its first command starts (or restored if an external reset
    /// removed the required project grant).
    pub(super) fn ensure_grants_written(&self) -> io::Result<()> {
        let mut written = crate::panic::lock(&self.inner.written);
        let needs_write = if !*written {
            true
        } else if self.inner.account.is_some() {
            self.inner.grants.iter().any(|g| {
                g.required
                    && g.path.exists()
                    && !Dacl::of(&g.path).is_ok_and(|dacl| dacl.grants(&g.sid, g.access))
            })
        } else {
            false
        };
        if needs_write {
            if let Some(container) = self.inner.container.as_ref().filter(|c| c.profile) {
                make_profile(container)?;
            }
            if let (Some(_), Some((acct, staging_home)), Some(session_key)) = (
                &self.inner.ledger,
                &self.inner.account,
                &self.inner.session_key,
            ) {
                acquire_session_holds(session_key, &acct.sid, staging_home, &self.inner.grants);
            }
            self.write_grants()?;
            *written = true;
        }
        Ok(())
    }

    /// The token a confined command starts with — its grants written on disk
    /// first, once: the project's must be, or nothing starts confined.
    #[cfg(test)]
    pub(super) fn token_for_start(&self) -> io::Result<HANDLE> {
        self.ensure_grants_written()?;
        Ok(self
            .inner
            .token
            .as_ref()
            .map_or(std::ptr::null_mut(), OwnedHandle::as_raw_handle))
    }

    /// Take back the AppContainer profile [`Mechanism::AppContainer`] made,
    /// if it made one.
    #[cfg(test)]
    pub(super) fn forget_profile(&self) {
        use windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile;
        if let Some(container) = self.inner.container.as_ref().filter(|c| c.profile) {
            if let Ok(name) = super::wide::wide(std::ffi::OsStr::new(&container.name)) {
                unsafe { DeleteAppContainerProfile(name.as_ptr()) };
            }
        }
    }

    /// What a confined command is started with ([`Start`]), its grants
    /// written on disk first ([`Confinement::ensure_grants_written`]).
    pub(super) fn for_start(&self) -> io::Result<Start> {
        self.ensure_grants_written()?;
        if let Some(account) = &self.inner.account {
            return Ok(Start {
                token: None,
                container: None,
                account: Some(account.clone()),
            });
        }
        let token = self.inner.token.as_ref().map(OwnedHandle::as_raw_handle);
        let Some(container) = &self.inner.container else {
            return Ok(Start {
                token,
                container: None,
                account: None,
            });
        };
        let mut capabilities: Vec<SID_AND_ATTRIBUTES> = container
            .capabilities
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: SE_GROUP_ENABLED as u32,
            })
            .collect();
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: container.package.as_psid(),
            Capabilities: capabilities.as_mut_ptr(),
            CapabilityCount: capabilities.len() as u32,
            Reserved: 0,
        };
        let filtered = matches!(
            self.inner.mechanism,
            Mechanism::AppContainer { filtered: true, .. }
        );
        Ok(Start {
            token: filtered.then_some(token).flatten(),
            container: Some((security, capabilities)),
            account: None,
        })
    }

    /// Every planned entry on disk, each recorded in the ledger first — and
    /// one found there unrecorded recorded now. One the project needs that
    /// cannot be written is an error; any other is let go, like a path that
    /// does not exist: not granted, and the command finds it closed.
    fn write_grants(&self) -> io::Result<()> {
        let mut ledger = self.inner.ledger.as_deref().map(Ledger::hold);
        for planned in &self.inner.grants {
            match write_grant(planned, ledger.as_mut()) {
                Ok(()) => {}
                Err(error) if planned.required => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "Rune could not let its confined commands into {}: {error}",
                            planned.path.display()
                        ),
                    ))
                }
                Err(_) => {}
            }
        }
        Ok(())
    }
}

fn write_grant(planned: &Grant, ledger: Option<&mut io::Result<Ledger>>) -> io::Result<()> {
    if !planned.path.exists() {
        return Ok(());
    }
    let present = Dacl::of(&planned.path)?.grants(&planned.sid, planned.access);
    let recorded = RecordedGrant {
        path: planned.path.clone(),
        write: planned.write,
    };
    match ledger {
        Some(Ok(ledger)) => ledger.note(&recorded)?,
        Some(Err(error)) => {
            return Err(io::Error::new(
                error.kind(),
                format!("the record of Rune's grants could not be opened: {error}"),
            ))
        }
        None if present => {}
        None => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no record is kept of what this confinement would grant, so it grants nothing",
            ))
        }
    }
    if !present {
        grant(&planned.path, &planned.sid, planned.access, planned.folder)?;
    }
    Ok(())
}

/// The runtimes that give every process of theirs one shared memory section
/// made by the first of them (MSYS2's, Git Bash's own; Cygwin's): a command
/// whose program loads one cannot start under a same-user restricted token
/// once a process of the person's runs it (CI probe, 2026-10-18:
/// `CreateFileMapping` refused, Git Bash exit 256). Under the dedicated
/// sandbox account (`Mechanism::Account`), the command runs as another user
/// with no restricting SIDs and starts normally.
const SHARED_RUNTIMES: [&str; 2] = ["msys-2.0.dll", "cygwin1.dll"];

/// Why a command started by `program` cannot run under this confinement, or
/// `None` when it can.
pub fn unconfinable(program: &Path) -> Option<String> {
    if super::account::is_active() {
        return None;
    }
    shared_runtime_unconfinable(program)
}

fn shared_runtime_unconfinable(program: &Path) -> Option<String> {
    let folder = program.parent()?;
    let mut folders = vec![folder.to_path_buf()];
    if let Some(root) = folder.parent() {
        folders.push(root.join("usr").join("bin"));
    }
    let runtime = folders.iter().find_map(|folder| {
        SHARED_RUNTIMES
            .iter()
            .find(|name| folder.join(name).is_file())
    })?;
    Some(format!(
        "{} runs on {runtime}, whose shared memory a confined process cannot open",
        program
            .file_name()
            .unwrap_or(program.as_os_str())
            .to_string_lossy()
    ))
}

/// Take back every entry the ledger at `ledger` records, from the path it is
/// on; what could not be taken back stays recorded, with why. A Rune still
/// running writes again what its next command needs.
pub fn forget_grants(ledger: &Path) -> io::Result<Forgotten> {
    crate::panic::lock(session_holds()).clear();
    if !ledger.exists() {
        return Ok(Forgotten::default());
    }
    let account_sid = super::account::owner_sandbox_sid();
    Ledger::hold(ledger)?.forget(|entry| {
        let dacl = Dacl::of(&entry.path)?;
        let mut taken = Taken::Absent;
        for mechanism in [
            Mechanism::RestrictedToken,
            Mechanism::AppContainer {
                profile: false,
                filtered: false,
            },
        ] {
            let sid = sid_for(mechanism, entry.write, &entry.path);
            if dacl.names(&sid) {
                revoke(&entry.path, &sid)?;
                taken = Taken::Removed;
            }
        }
        if let Some(ref acct_sid) = account_sid {
            if dacl.names(acct_sid) {
                revoke(&entry.path, acct_sid)?;
                taken = Taken::Removed;
            }
        }
        Ok(taken)
    })
}

/// Per-session OS shared file locks held for each `session_scoped` grant so
/// closing one session or ending a one-call grant never revokes an ACL entry
/// on a project or reach path while another open session (in this process or
/// in another Rune process) still holds it.
struct SessionHold {
    holds_dir: PathBuf,
    paths: HashMap<PathBuf, File>,
}

static SESSION_HOLDS: OnceLock<Mutex<HashMap<PathBuf, SessionHold>>> = OnceLock::new();

fn session_holds() -> &'static Mutex<HashMap<PathBuf, SessionHold>> {
    SESSION_HOLDS.get_or_init(Default::default)
}

fn normalize_session_key(session_key: &Path) -> PathBuf {
    canonical(session_key).unwrap_or_else(|| session_key.to_path_buf())
}

fn hold_lock_path(holds_dir: &Path, target: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        target
            .as_os_str()
            .to_string_lossy()
            .to_uppercase()
            .as_bytes(),
    );
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    holds_dir.join(format!("{hex}.lock"))
}

fn acquire_session_holds(session_key: &Path, _sid: &Sid, staging_home: &Path, grants: &[Grant]) {
    let key = normalize_session_key(session_key);
    let holds_dir = staging_home.join("sandbox").join("holds");
    let _ = std::fs::create_dir_all(&holds_dir);
    let mut map = crate::panic::lock(session_holds());
    let hold = map.entry(key).or_insert_with(|| SessionHold {
        holds_dir: holds_dir.clone(),
        paths: HashMap::new(),
    });
    hold.holds_dir = holds_dir.clone();
    for grant in grants {
        if !grant.session_scoped || hold.paths.contains_key(&grant.path) {
            continue;
        }
        let lock_path = hold_lock_path(&holds_dir, &grant.path);
        if let Ok(file) = super::open_private(&lock_path, crate::platform::Private::Lock) {
            if file.lock_shared().is_ok() {
                hold.paths.insert(grant.path.clone(), file);
            }
        }
    }
}

fn revoke_target_if_unheld(
    map: &HashMap<PathBuf, SessionHold>,
    holds_dir: &Path,
    target: &Path,
    sid: &Sid,
) {
    if windows_own(target).is_some() {
        return;
    }
    if map.values().any(|h| h.paths.contains_key(target)) {
        return;
    }
    let lock_path = hold_lock_path(holds_dir, target);
    if lock_path.exists() {
        let Ok(probe) = super::open_private(&lock_path, crate::platform::Private::Lock) else {
            return;
        };
        if probe.try_lock().is_err() {
            // Another Rune process still holds a shared lock on this path.
            return;
        }
        if Dacl::of(target).is_ok_and(|dacl| dacl.names(sid)) {
            let _ = revoke(target, sid);
        }
        // The lock file stays: a session of another process may have opened
        // it already and lock it next; removed, a later release would find no
        // file and take that session's entry back while it still holds it.
        drop(probe);
    } else if Dacl::of(target).is_ok_and(|dacl| dacl.names(sid)) {
        let _ = revoke(target, sid);
    }
}

fn revoke_held_paths(
    session_key: Option<&Path>,
    paths: &[PathBuf],
    sid: &Sid,
    staging_home: &Path,
) {
    let key = session_key.map(normalize_session_key);
    let mut map = crate::panic::lock(session_holds());
    let holds_dir = key
        .as_ref()
        .and_then(|k| map.get(k).map(|h| h.holds_dir.clone()))
        .unwrap_or_else(|| staging_home.join("sandbox").join("holds"));
    for path in paths {
        let Some(target) = canonical(path) else {
            continue;
        };
        if let Some(k) = &key {
            if let Some(hold) = map.get_mut(k) {
                hold.paths.remove(&target);
            }
        }
        revoke_target_if_unheld(&map, &holds_dir, &target, sid);
    }
    if let Some(k) = &key {
        if map.get(k).is_some_and(|h| h.paths.is_empty()) {
            map.remove(k);
        }
    }
}

fn release_session_holds(
    session_key: Option<&Path>,
    paths: &[PathBuf],
    sid: &Sid,
    staging_home: &Path,
) {
    let key = session_key.map(normalize_session_key);
    let mut map = crate::panic::lock(session_holds());
    let mut all_targets: Vec<PathBuf> = paths.iter().filter_map(|p| canonical(p)).collect();
    let mut holds_dir = staging_home.join("sandbox").join("holds");
    if let Some(k) = &key {
        if let Some(removed) = map.remove(k) {
            holds_dir = removed.holds_dir;
            for (held_path, lock_file) in removed.paths {
                drop(lock_file);
                if !all_targets.contains(&held_path) {
                    all_targets.push(held_path);
                }
            }
        }
    }
    for target in &all_targets {
        revoke_target_if_unheld(&map, &holds_dir, target, sid);
    }
}

/// Revoke the sandbox account's access entries on `paths` held by the session
/// at `session_scratch`, keeping any entry still held by another open session.
pub fn revoke_sandbox_paths(session_scratch: &Path, paths: &[PathBuf]) {
    let Some((acct, staging_home)) = super::account::active_account() else {
        return;
    };
    revoke_held_paths(Some(session_scratch), paths, &acct.sid, &staging_home);
}

/// Whether this user may change who reaches `path` (`WRITE_DAC`): asked by
/// opening it for that right, which writes nothing.
fn may_change_access(path: &Path) -> io::Result<()> {
    const WRITE_DAC: u32 = 0x0004_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    let name = wide_path(path)?;
    owned(unsafe {
        CreateFileW(
            name.as_ptr(),
            WRITE_DAC | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    })
    .map(drop)
}

/// The AppContainer for `project`'s commands: named after the project, so
/// two projects' commands share no named object; `grants` its capabilities,
/// with the network's (out, served, and the private network's).
fn container(project: &Path, grants: Vec<Sid>, profile: bool) -> io::Result<Container> {
    use sha2::Digest;
    use windows_sys::Win32::Security::{
        WinCapabilityInternetClientServerSid, WinCapabilityInternetClientSid,
        WinCapabilityPrivateNetworkClientServerSid,
    };
    let hash = sha2::Sha256::digest(
        project
            .as_os_str()
            .to_string_lossy()
            .to_uppercase()
            .as_bytes(),
    );
    let name = format!(
        "Rune.Confined.{}",
        hash[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let mut capabilities = grants;
    for network in [
        WinCapabilityInternetClientSid,
        WinCapabilityInternetClientServerSid,
        WinCapabilityPrivateNetworkClientServerSid,
    ] {
        capabilities.push(Sid::well_known(network)?);
    }
    Ok(Container {
        package: Sid::container(&name)?,
        name,
        capabilities,
        profile,
    })
}

/// The container's profile, made unless it is there already.
fn make_profile(container: &Container) -> io::Result<()> {
    use windows_sys::Win32::Security::FreeSid;
    use windows_sys::Win32::Security::Isolation::CreateAppContainerProfile;
    const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;
    let name = super::wide::wide(std::ffi::OsStr::new(&container.name))?;
    let mut psid = std::ptr::null_mut();
    let result = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            name.as_ptr(),
            std::ptr::null(),
            0,
            &mut psid,
        )
    };
    if result == ALREADY_EXISTS {
        return Ok(());
    }
    if result < 0 {
        return Err(io::Error::other(format!(
            "CreateAppContainerProfile: HRESULT {result:#010x}"
        )));
    }
    unsafe { FreeSid(psid) };
    Ok(())
}

/// This process's own token.
fn own_token() -> io::Result<OwnedHandle> {
    Ok(Token::of_this_process(TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY)?.0)
}

/// A deny-only token for the sandbox-account runner: privileges dropped,
/// Administrators and Authenticated Users deny-only, medium integrity at
/// most, and `owner_sid` included in `TokenDefaultDacl` so the owner's Rune
/// process can inspect or stop child processes created by the runner.
pub(super) fn runner_deny_only_token(owner_sid: &Sid) -> io::Result<OwnedHandle> {
    restricted_token_with_owner(&[], None, true, Some(owner_sid))
}

/// This process's token, restricted: no privilege but bypassing traverse
/// checks, Administrators deny-only, checked against `grants` and the SIDs
/// every program of the user needs; medium integrity at most; and a default
/// ACL its own processes and threads can be opened under. For `package`, the
/// token an AppContainer is made from: no restricting SID — the container's
/// capabilities are its second check — and a default ACL the container's
/// processes can open again.
fn restricted_token(
    grants: &[Sid],
    package: Option<&Sid>,
    deny_only: bool,
) -> io::Result<OwnedHandle> {
    restricted_token_with_owner(grants, package, deny_only, None)
}

fn restricted_token_with_owner(
    grants: &[Sid],
    package: Option<&Sid>,
    deny_only: bool,
    extra_owner: Option<&Sid>,
) -> io::Result<OwnedHandle> {
    let own = Token::of_this_process(
        TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
    )?;
    let user = own.user()?;
    let logon = own.logon_sid();
    let administrators = Sid::well_known(WinBuiltinAdministratorsSid)?;
    let mut restricting = Vec::new();
    if package.is_none() && !deny_only {
        restricting.extend([
            Sid::well_known(WinWorldSid)?,
            Sid::well_known(WinBuiltinUsersSid)?,
            Sid::well_known(WinRestrictedCodeSid)?,
        ]);
        restricting.extend(logon.iter().cloned());
    }
    if !deny_only {
        for grant in grants {
            if !restricting.contains(grant) {
                restricting.push(grant.clone());
            }
        }
    }
    let mut denied = vec![administrators];
    if deny_only {
        denied.push(Sid::well_known(WinAuthenticatedUserSid)?);
    }
    let disable: Vec<SID_AND_ATTRIBUTES> = denied
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: 0,
        })
        .collect();
    let restrict: Vec<SID_AND_ATTRIBUTES> = restricting
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: 0,
        })
        .collect();
    let mut restricted: HANDLE = std::ptr::null_mut();
    check(unsafe {
        CreateRestrictedToken(
            own.raw(),
            DISABLE_MAX_PRIVILEGE,
            disable.len() as u32,
            disable.as_ptr(),
            0,
            std::ptr::null(),
            restrict.len() as u32,
            restrict.as_ptr(),
            &mut restricted,
        )
    })
    .map_err(failed("CreateRestrictedToken"))?;
    let restricted = owned(restricted)?;

    // An elevated (high) or system token is lowered to medium: a confined
    // command never drives an elevated window.
    if own.integrity().is_some_and(|level| level > 0x2000) {
        let medium = Sid::well_known(WinMediumLabelSid)?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: medium.as_psid(),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        check(unsafe {
            SetTokenInformation(
                restricted.as_raw_handle(),
                TokenIntegrityLevel,
                (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32)
                    + GetLengthSid(medium.as_psid()),
            )
        })
        .map_err(failed("SetTokenInformation(TokenIntegrityLevel)"))?;
    }

    // What a confined process creates without saying — its own threads, the
    // process itself — must let the restricted process in again: the user,
    // SYSTEM, the logon session, and RESTRICTED, which passes the second
    // check.
    let mut owners = vec![
        user,
        Sid::well_known(WinLocalSystemSid)?,
        Sid::well_known(WinRestrictedCodeSid)?,
    ];
    owners.extend(logon);
    if let Some(owner) = extra_owner {
        if !owners.contains(owner) {
            owners.push(owner.clone());
        }
    }
    if let Some(package) = package {
        owners.push(package.clone());
        owners.push(Sid::well_known(WinBuiltinAnyPackageSid)?);
    }
    let entries: Vec<EXPLICIT_ACCESS_W> = owners
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid.as_psid() as windows_sys::core::PWSTR,
            },
        })
        .collect();
    let mut acl: *mut ACL = std::ptr::null_mut();
    win32(unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null(),
            &mut acl,
        )
    })
    .map_err(failed("SetEntriesInAclW"))?;
    let acl = Local(acl);
    let default = TOKEN_DEFAULT_DACL { DefaultDacl: acl.0 };
    check(unsafe {
        SetTokenInformation(
            restricted.as_raw_handle(),
            TokenDefaultDacl,
            (&default as *const TOKEN_DEFAULT_DACL).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    })
    .map_err(failed("SetTokenInformation(TokenDefaultDacl)"))?;
    Ok(restricted)
}

/// The error of a Windows call, said with the call's name — which of the
/// steps of a token refused is what tells a person (and a port) what to fix.
fn failed(call: &'static str) -> impl Fn(io::Error) -> io::Error {
    move |error| io::Error::new(error.kind(), format!("{call}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One SID per path and access, whatever the spelling's case.
    #[test]
    fn a_grant_has_one_sid_per_path_and_access() {
        let a = sid_for(Mechanism::RestrictedToken, true, Path::new(r"C:\Work\One"));
        assert_eq!(
            a,
            sid_for(Mechanism::RestrictedToken, true, Path::new(r"c:\work\one"))
        );
        assert_ne!(
            a,
            sid_for(Mechanism::RestrictedToken, false, Path::new(r"C:\Work\One"))
        );
        assert_ne!(
            a,
            sid_for(Mechanism::RestrictedToken, true, Path::new(r"C:\Work\Two"))
        );
    }

    /// A program beside an MSYS2 or Cygwin runtime — Git Bash's launcher
    /// included — is said to be beyond a restricted-token confinement; any
    /// other is not.
    #[test]
    fn a_program_on_a_shared_runtime_is_unconfinable() {
        let base = std::env::temp_dir().join(format!("rune-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for folder in ["Git\\bin", "Git\\usr\\bin", "cygwin\\bin", "plain"] {
            std::fs::create_dir_all(base.join(folder)).unwrap();
        }
        std::fs::write(base.join(r"Git\usr\bin\msys-2.0.dll"), "").unwrap();
        std::fs::write(base.join(r"cygwin\bin\cygwin1.dll"), "").unwrap();
        let launcher = shared_runtime_unconfinable(&base.join(r"Git\bin\bash.exe")).unwrap();
        assert!(
            launcher.contains("bash.exe runs on msys-2.0.dll"),
            "{launcher}"
        );
        assert!(shared_runtime_unconfinable(&base.join(r"Git\usr\bin\sh.exe")).is_some());
        assert!(
            shared_runtime_unconfinable(&base.join(r"cygwin\bin\bash.exe"))
                .unwrap()
                .contains("cygwin1.dll")
        );
        assert_eq!(
            shared_runtime_unconfinable(&base.join(r"plain\pwsh.exe")),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Building a confinement writes nothing on disk; its first start records
    /// each entry, then writes it; a reset takes every one back.
    #[test]
    fn a_confinement_is_granted_at_its_first_start_recorded_and_taken_back() {
        let base = std::env::temp_dir().join(format!("rune-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        std::fs::create_dir_all(base.join("cache")).unwrap();
        let project = canonical(&base.join("project")).unwrap();
        let cache = canonical(&base.join("cache")).unwrap();
        let ledger = base.join("sandbox-grants.txt");
        let confinement = Confinement::build_as(
            Grants {
                project: &project,
                ledger: Some(&ledger),
                reads: &[],
                writes: std::slice::from_ref(&cache),
                reach: &[],
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            Mechanism::RestrictedToken,
        )
        .unwrap_or_else(|why| panic!("{}", why.reason));
        let sid = sid_for(Mechanism::RestrictedToken, true, &project);
        assert!(!Dacl::of(&project).unwrap().grants(&sid, FILE_ALL_ACCESS));
        assert!(!ledger.exists(), "building records nothing");
        confinement.token_for_start().unwrap();
        assert!(Dacl::of(&project).unwrap().grants(&sid, FILE_ALL_ACCESS));
        assert_eq!(
            crate::platform::recorded_grants(&ledger).unwrap(),
            [
                RecordedGrant {
                    path: project.clone(),
                    write: true
                },
                RecordedGrant {
                    path: cache.clone(),
                    write: true
                }
            ]
        );
        assert_eq!(confinement.kind(), "restricted-token");
        let forgotten = forget_grants(&ledger).unwrap();
        assert_eq!((forgotten.removed, forgotten.kept.len()), (2, 0));
        assert!(!Dacl::of(&project).unwrap().names(&sid));
        assert_eq!(crate::platform::recorded_grants(&ledger).unwrap(), []);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Without a ledger nothing is written: the project's entry is missing,
    /// so nothing starts confined.
    #[test]
    fn without_a_ledger_no_entry_is_written() {
        let base = std::env::temp_dir().join(format!("rune-unrecorded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let project = canonical(&base.join("project")).unwrap();
        let confinement = Confinement::build_as(
            Grants {
                project: &project,
                ledger: None,
                reads: &[],
                writes: &[],
                reach: &[],
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            Mechanism::RestrictedToken,
        )
        .unwrap_or_else(|why| panic!("{}", why.reason));
        assert!(confinement.token_for_start().is_err());
        assert!(!Dacl::of(&project).unwrap().names(&sid_for(
            Mechanism::RestrictedToken,
            true,
            &project
        )));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Windows's own folders, what they hold, and what holds them get no
    /// entry; a folder of the person's own does.
    #[test]
    fn windows_own_folders_get_no_entry() {
        let system = canonical(
            &std::env::var_os("SystemRoot")
                .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from),
        )
        .unwrap();
        assert!(windows_own(&system).is_some());
        assert!(windows_own(&system.join("System32")).is_some());
        assert!(windows_own(system.ancestors().last().unwrap()).is_some());
        let home = canonical(&super::super::home_dir().unwrap()).unwrap();
        assert!(windows_own(home.parent().unwrap()).is_some(), "C:\\Users");
        assert!(windows_own(&home).is_none());
        assert!(within(
            Path::new(r"C:\Users\Me\Work"),
            Path::new(r"c:\users\me")
        ));
        assert!(!within(
            Path::new(r"C:\Users\Mel"),
            Path::new(r"C:\Users\Me")
        ));
        let refused = Confinement::build(Grants {
            project: &system.join("System32"),
            ledger: None,
            reads: &[],
            writes: &[],
            reach: &[],
            local_services: false,
            sockets: &[],
            ask_service: None,
        })
        .err()
        .expect("never a confinement that writes in System32");
        assert!(refused.reason.contains("Rune writes no access entry there"));
    }

    #[test]
    fn a_project_that_cannot_be_granted_is_no_confinement() {
        let unconfined = Confinement::build(Grants {
            project: Path::new(r"C:\nonexistent-rune-project"),
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

    /// A release that finds nobody holding a path keeps its lock file: a
    /// session of another process that opened the file before that release
    /// and locked it after still holds the path for every later release.
    #[test]
    fn a_hold_opened_before_a_release_keeps_its_entry_through_a_later_release() {
        let base = std::env::temp_dir().join(format!("rune-hold-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let staging = base.join("home");
        std::fs::create_dir_all(base.join("project")).unwrap();
        let project = canonical(&base.join("project")).unwrap();
        let sid = Sid::derived(&format!("hold-race:{}", project.display()));
        let holds_dir = staging.join("sandbox").join("holds");
        std::fs::create_dir_all(&holds_dir).unwrap();

        // Another process's session opens the hold file, not yet locked.
        let other = super::super::open_private(
            &hold_lock_path(&holds_dir, &project),
            crate::platform::Private::Lock,
        )
        .unwrap();
        // A session that holds nothing ends: nobody is locked, so it takes
        // the entry back.
        grant(&project, &sid, FILE_GENERIC_READ, true).unwrap();
        release_session_holds(
            Some(&base.join("first")),
            std::slice::from_ref(&project),
            &sid,
            &staging,
        );
        assert!(!Dacl::of(&project).unwrap().names(&sid));
        // The other session locks and writes its entry.
        other.lock_shared().unwrap();
        grant(&project, &sid, FILE_GENERIC_READ, true).unwrap();
        // A third session ending must see that hold.
        release_session_holds(
            Some(&base.join("third")),
            std::slice::from_ref(&project),
            &sid,
            &staging,
        );
        assert!(
            Dacl::of(&project).unwrap().names(&sid),
            "an entry a live session holds is never taken back"
        );
        drop(other);
        release_session_holds(
            Some(&base.join("third")),
            std::slice::from_ref(&project),
            &sid,
            &staging,
        );
        assert!(!Dacl::of(&project).unwrap().names(&sid));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn closing_one_session_keeps_shared_project_grant_for_another_session_until_last_closes() {
        let base = std::env::temp_dir().join(format!("rune-multi-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for dir in [
            "project",
            "scratch-a",
            "scratch-b",
            "reach-shared",
            "reach-only-a",
            "home",
        ] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        let project = canonical(&base.join("project")).unwrap();
        let scratch_a = canonical(&base.join("scratch-a")).unwrap();
        let scratch_b = canonical(&base.join("scratch-b")).unwrap();
        let reach_shared = canonical(&base.join("reach-shared")).unwrap();
        let reach_only_a = canonical(&base.join("reach-only-a")).unwrap();
        let staging_home = canonical(&base.join("home")).unwrap();
        let ledger = staging_home.join("sandbox-grants.txt");

        let sid = Sid::derived("test-multi-session-sandbox-account");
        let acct = super::super::account::SandboxAccount {
            name: "rune-sbx-test".into(),
            password: "pw".into(),
            sid: sid.clone(),
        };

        let reach_a = vec![reach_shared.clone(), reach_only_a.clone()];
        let reach_b = vec![reach_shared.clone()];

        let sess_a = Confinement::build_with_account(
            Grants {
                project: &project,
                ledger: Some(&ledger),
                reads: &[],
                writes: std::slice::from_ref(&scratch_a),
                reach: &reach_a,
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            Mechanism::Account,
            Some((acct.clone(), staging_home.clone())),
        )
        .unwrap_or_else(|why| panic!("{}", why.reason));

        let sess_b = Confinement::build_with_account(
            Grants {
                project: &project,
                ledger: Some(&ledger),
                reads: &[],
                writes: std::slice::from_ref(&scratch_b),
                reach: &reach_b,
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            Mechanism::Account,
            Some((acct, staging_home)),
        )
        .unwrap_or_else(|why| panic!("{}", why.reason));

        sess_a.ensure_grants_written().unwrap();
        sess_b.ensure_grants_written().unwrap();

        for path in [
            &project,
            &scratch_a,
            &scratch_b,
            &reach_shared,
            &reach_only_a,
        ] {
            assert!(Dacl::of(path).unwrap().grants(&sid, FILE_ALL_ACCESS));
        }

        // Narrowing session A's reach revokes `reach_only_a` but keeps `reach_shared`.
        sess_a.revoke_paths(&[reach_only_a.clone(), reach_shared.clone()]);
        assert!(!Dacl::of(&reach_only_a).unwrap().names(&sid));
        assert!(
            Dacl::of(&reach_shared)
                .unwrap()
                .grants(&sid, FILE_ALL_ACCESS),
            "reach_shared is still held by session B"
        );

        // Closing session A revokes `scratch_a` but keeps `project` for session B.
        sess_a.revoke_session_grants();
        assert!(!Dacl::of(&scratch_a).unwrap().names(&sid));
        assert!(
            Dacl::of(&project).unwrap().grants(&sid, FILE_ALL_ACCESS),
            "project must remain granted while session B is open"
        );
        assert!(Dacl::of(&scratch_b).unwrap().grants(&sid, FILE_ALL_ACCESS));

        // Closing session B revokes the remaining grants on `project`, `scratch_b`, and `reach_shared`.
        sess_b.revoke_session_grants();
        for path in [&project, &scratch_b, &reach_shared] {
            assert!(!Dacl::of(path).unwrap().names(&sid));
        }

        let _ = std::fs::remove_dir_all(&base);
    }
}
