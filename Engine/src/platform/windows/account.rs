//! The dedicated local Windows user account (`rune-sbx-<8hex>`) under which
//! Rune runs confined commands when available.
//!
//! A restricted token of the person's own account still carries the person's
//! user SID (`TokenUser`), and `CreateRestrictedToken` never disables or
//! deny-onlies `TokenUser`. Any file in the person's profile whose DACL names
//! the person directly — rather than `Administrators` or `Users` — remains
//! open to a same-user token unless `RESTRICT_TOKEN` adds restricting SIDs,
//! which in turn breaks schannel (`AcquireCredentialsHandleW` returns
//! `SEC_E_INTERNAL_ERROR`) and MSYS2's `usr\bin\bash.exe` heap startup
//! (measured on Windows Server 2025, 2026-10-18).
//!
//! A separate local user has a distinct `TokenUser` (`S-1-5-21-…-<new RID>`):
//! the person's profile (`C:\Users\<person>`) does not name it, so a token of
//! that account with `Administrators` and `Authenticated Users` marked
//! deny-only (and no restricting SIDs) is kept out of the person's profile by
//! the kernel while schannel TLS and Git for Windows work unchanged.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rand::RngCore;
use sha2::Digest;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MEMBER_IN_ALIAS, WAIT_OBJECT_0,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    NERR_GroupNotFound, NERR_Success, NERR_UserExists, NERR_UserNotFound, NetLocalGroupAddMembers,
    NetUserAdd, NetUserDel, NetUserSetInfo, LOCALGROUP_MEMBERS_INFO_0, UF_DONT_EXPIRE_PASSWD,
    UF_NORMAL_ACCOUNT, UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_INFO_1, USER_INFO_1003,
    USER_INFO_1008, USER_PRIV_USER,
};
use windows_sys::Win32::Security::{
    LookupAccountSidW, WinBuiltinAdministratorsSid, WinBuiltinUsersSid, SID_NAME_USE,
};
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

use super::security::{current_user, grant, set_private, Sid};
use super::wide::{check, owned, wide, wide_path};

/// The sandbox account for this person's Windows profile, its credentials, and
/// its resolved SID.
#[derive(Clone)]
pub(super) struct SandboxAccount {
    pub(super) name: String,
    pub(super) password: String,
    pub(super) sid: Sid,
}

/// Deterministic 17-character local account name for an owner SID (`S-1-5-…`).
pub(super) fn account_name_for_sid(sid_text: &str) -> String {
    let hash = sha2::Sha256::digest(format!("rune:sandbox-account:{sid_text}").as_bytes());
    format!(
        "rune-sbx-{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

/// Whether `name` has the exact `rune-sbx-<8 lowercase hex>` shape.
pub(super) fn valid_account_name(name: &str) -> bool {
    name.len() == 17
        && name
            .strip_prefix("rune-sbx-")
            .is_some_and(|hex| hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Where the owner's private sandbox account record lives under `home`.
pub(super) fn account_file(home: &Path) -> PathBuf {
    home.join("sandbox").join("account")
}

/// Where this Windows user's sandbox account record, staged runner and
/// session holds live (`%LOCALAPPDATA%\\Rune`, as `<this>\\sandbox\\…`): the
/// account is one per Windows user, so its record is too, whatever `RUNE_HOME`
/// a Rune runs with — two homes sharing the account share its password and
/// see each other's holds.
pub(super) fn user_home() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;
    Some(super::home::known_folder(&FOLDERID_LocalAppData)?.join("Rune"))
}

/// The account found in use by this process, with [`user_home`].
static ACTIVE: Mutex<Option<(SandboxAccount, PathBuf)>> = Mutex::new(None);

/// Why the last look found no account in use; `None` once one is.
static NOT_IN_USE: Mutex<Option<String>> = Mutex::new(None);

/// The active sandbox account and its home in this process, if any.
pub(super) fn active_account() -> Option<(SandboxAccount, PathBuf)> {
    crate::panic::lock(&ACTIVE).clone()
}

/// Whether the dedicated sandbox account is active in this process.
pub(super) fn is_active() -> bool {
    active_account().is_some()
}

/// Why confined commands do not run under the sandbox account, as the last
/// confinement found it; `None` when they do or none was built yet.
pub(super) fn not_in_use() -> Option<String> {
    crate::panic::lock(&NOT_IN_USE).clone()
}

/// The SID of the current user's sandbox account if that account exists on
/// this machine.
pub(super) fn owner_sandbox_sid() -> Option<Sid> {
    let owner = current_user().ok()?.string().ok()?;
    let name = account_name_for_sid(&owner);
    Sid::of_account(&name).ok()
}

/// The sandbox account a confinement starts its commands under. Only found,
/// never set up here: a confinement is built by a host or a one-shot run that
/// has nobody to confirm an administrator prompt ([`set_up`] does that). Once
/// found it is kept for the process; until then every confinement looks
/// again, so one set up meanwhile is taken at the next session.
pub(super) fn resolve_for_confinement() -> Option<(SandboxAccount, PathBuf)> {
    let mut active = crate::panic::lock(&ACTIVE);
    if let Some(found) = &*active {
        return Some(found.clone());
    }
    let found = user_home()
        .ok_or_else(|| "Windows names no local application data folder for this user".to_string())
        .and_then(|home| in_use(&home).map(|account| (account, home)));
    match found {
        Ok(pair) => {
            *active = Some(pair.clone());
            *crate::panic::lock(&NOT_IN_USE) = None;
            Some(pair)
        }
        Err(why) => {
            *crate::panic::lock(&NOT_IN_USE) = Some(why);
            None
        }
    }
}

/// This user's account as its record under `home` names it, or why not.
fn in_use(home: &Path) -> Result<SandboxAccount, String> {
    let owner = current_user()
        .and_then(|user| user.string())
        .map_err(|e| format!("this user's SID could not be read ({e})"))?;
    let expected = account_name_for_sid(&owner);
    let file = account_file(home);
    let (name, password) = match read_credentials(&file) {
        Ok(pair) => pair,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err("Rune's sandbox account is not set up for this user".into())
        }
        Err(e) => {
            return Err(format!(
                "the sandbox account record {} cannot be used ({e})",
                file.display()
            ))
        }
    };
    if name != expected {
        return Err(format!(
            "the sandbox account record names {name}, not this user's {expected}"
        ));
    }
    let sid = Sid::of_account(&name)
        .map_err(|e| format!("the sandbox account {name} is not on this computer ({e})"))?;
    Ok(SandboxAccount {
        name,
        password,
        sid,
    })
}

/// Why setting the account up did not happen.
pub(super) enum Refusal {
    /// This process may not add local accounts: an administrator must.
    NoRight,
    Failed(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NoRight => f.write_str("only an administrator may add a local account"),
            Refusal::Failed(why) => f.write_str(why),
        }
    }
}

/// Set this user's sandbox account up, unless it is already: directly when
/// this process may add local accounts, else through Windows's administrator
/// confirmation (`-sandbox account-setup <record>` run elevated). By
/// `rune -sandbox setup` and the first interactive start ([`set_up_once`]);
/// never by a host.
pub(super) fn set_up() -> Result<(), String> {
    let home = user_home().ok_or_else(|| {
        "Windows names no local application data folder for this user".to_string()
    })?;
    if in_use(&home).is_ok() {
        return Ok(());
    }
    let owner = current_user()
        .and_then(|user| user.string())
        .map_err(|e| format!("this user's SID could not be read ({e})"))?;
    let expected = account_name_for_sid(&owner);
    let file = account_file(&home);
    write_credentials(&file, &expected, &random_password())
        .map_err(|e| format!("write {}: {e}", file.display()))?;
    let provisioned = match setup_account_at(&file) {
        Err(Refusal::NoRight) => elevate_account_setup(&file),
        other => other.map_err(|refusal| refusal.to_string()),
    };
    if let Err(err) = provisioned {
        let _ = std::fs::remove_file(&file);
        return Err(err);
    }
    let _ = std::fs::remove_file(declined_file(&home));
    in_use(&home).map(drop)
}

/// Where a declined or failed automatic setup is remembered, so an
/// interactive start asks once, not at every start.
fn declined_file(home: &Path) -> PathBuf {
    home.join("sandbox").join("declined")
}

/// [`set_up`] at an interactive start, once: a setup declined or failed
/// before is not asked again (`rune -sandbox setup` asks again).
pub(super) fn set_up_once() {
    let Some(home) = user_home() else {
        return;
    };
    if in_use(&home).is_ok() || declined_file(&home).exists() {
        return;
    }
    if let Err(why) = set_up() {
        let _ = std::fs::create_dir_all(home.join("sandbox"));
        let _ = std::fs::write(declined_file(&home), why);
    }
}

/// Read `(name, password)` from `path` (or create it when invoked directly by
/// an administrator) and provision the local Windows account.
pub(super) fn setup_account_at(path: &Path) -> Result<(), Refusal> {
    let failed = Refusal::Failed;
    let (name, password) = match read_credentials_unchecked(path) {
        Ok(pair) => pair,
        Err(_) if !path.exists() => {
            let owner = current_user().map_err(|e| failed(format!("current_user: {e}")))?;
            let owner_sid = owner
                .string()
                .map_err(|e| failed(format!("owner SID: {e}")))?;
            let name = account_name_for_sid(&owner_sid);
            let password = random_password();
            write_credentials(path, &name, &password)
                .map_err(|e| failed(format!("write {}: {e}", path.display())))?;
            (name, password)
        }
        Err(e) => return Err(failed(format!("read {}: {e}", path.display()))),
    };
    if !valid_account_name(&name) {
        return Err(failed(format!(
            "refusing invalid sandbox account name: {name}"
        )));
    }
    provision_local_account(&name, &password)
}

/// Remove this user's sandbox account, then its record, staged runner and
/// holds. The account goes first: a record removed before an account that
/// could not be would leave an account nobody knows the password of.
pub(super) fn remove() -> Result<(), String> {
    super::runner::stop_runner();
    *crate::panic::lock(&ACTIVE) = None;
    let home = user_home().ok_or_else(|| {
        "Windows names no local application data folder for this user".to_string()
    })?;
    let file = account_file(&home);
    let name = read_credentials_unchecked(&file)
        .ok()
        .map(|(n, _)| n)
        .filter(|n| valid_account_name(n))
        .or_else(|| {
            current_user()
                .ok()
                .and_then(|u| u.string().ok())
                .map(|s| account_name_for_sid(&s))
        })
        .ok_or_else(|| "could not determine sandbox account name".to_string())?;

    let wide_name = wide(OsStr::new(&name)).map_err(|e| e.to_string())?;
    let status = unsafe { NetUserDel(std::ptr::null(), wide_name.as_ptr()) };
    if status != NERR_Success && status != NERR_UserNotFound {
        return Err(format!(
            "the sandbox account {name} could not be removed ({}); run the reset as an administrator",
            io::Error::from_raw_os_error(status as i32)
        ));
    }
    let _ = unhide_from_welcome_screen(&name);
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(declined_file(&home));
    let _ = std::fs::remove_dir_all(home.join("sandbox").join("runner"));
    Ok(())
}

fn random_password() -> String {
    let mut bytes = [0u8; 28];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(60);
    out.push_str("Rn1!");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn read_credentials(path: &Path) -> io::Result<(String, String)> {
    if !super::files::is_private(path)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sandbox account file is not private to the current user",
        ));
    }
    read_credentials_unchecked(path)
}

fn read_credentials_unchecked(path: &Path) -> io::Result<(String, String)> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let name = lines
        .next()
        .map(str::trim)
        .filter(|s| valid_account_name(s))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid account name"))?;
    let password = lines
        .next()
        .map(str::trim)
        .filter(|s| s.len() >= 32)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid account password"))?;
    Ok((name.to_string(), password.to_string()))
}

fn write_credentials(path: &Path, name: &str, password: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    let mut file = super::files::open_private(path, crate::platform::Private::New)?;
    writeln!(file, "{name}")?;
    writeln!(file, "{password}")?;
    file.sync_all()?;
    drop(file);
    set_private(path, false)
}

fn elevate_account_setup(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let admins = Sid::well_known(WinBuiltinAdministratorsSid).ok();
    if let Some(ref admins) = admins {
        if let Some(parent) = path.parent() {
            let _ = grant(parent, admins, FILE_GENERIC_READ, true);
        }
        let _ = grant(path, admins, FILE_GENERIC_READ, false);
    }

    let result = run_elevated_setup(&exe, path);

    let _ = set_private(path, false);
    if let (Some(ref admins), Some(parent)) = (&admins, path.parent()) {
        let _ = super::security::revoke(parent, admins);
    }
    result
}

fn run_elevated_setup(exe: &Path, path: &Path) -> Result<(), String> {
    let verb = wide(OsStr::new("runas")).map_err(|e| e.to_string())?;
    let file = wide_path(exe).map_err(|e| e.to_string())?;
    let params = wide(OsStr::new(&format!(
        "-sandbox account-setup \"{}\"",
        path.display()
    )))
    .map_err(|e| e.to_string())?;
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = 0; // SW_HIDE

    check(unsafe { ShellExecuteExW(&mut info) })
        .map_err(|e| format!("ShellExecuteExW(runas): {e}"))?;
    let process: OwnedHandle =
        owned(info.hProcess).map_err(|e| format!("elevated process handle: {e}"))?;
    let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), 60_000) };
    if waited != WAIT_OBJECT_0 {
        return Err("elevated account-setup timed out".into());
    }
    let mut code = 1u32;
    check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) })
        .map_err(|e| format!("GetExitCodeProcess: {e}"))?;
    if code != 0 {
        return Err(format!("elevated account-setup exited with status {code}"));
    }
    Ok(())
}

fn provision_local_account(name: &str, password: &str) -> Result<(), Refusal> {
    let mut wide_name = wide(OsStr::new(name)).map_err(|e| Refusal::Failed(e.to_string()))?;
    let mut wide_pass = wide(OsStr::new(password)).map_err(|e| Refusal::Failed(e.to_string()))?;
    let flags = UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE | UF_NORMAL_ACCOUNT;
    let info = USER_INFO_1 {
        usri1_name: wide_name.as_mut_ptr(),
        usri1_password: wide_pass.as_mut_ptr(),
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: std::ptr::null_mut(),
        usri1_comment: std::ptr::null_mut(),
        usri1_flags: flags,
        usri1_script_path: std::ptr::null_mut(),
    };
    let mut parm_err = 0u32;
    let status = unsafe {
        NetUserAdd(
            std::ptr::null(),
            1,
            (&info as *const USER_INFO_1).cast(),
            &mut parm_err,
        )
    };
    if status == ERROR_ACCESS_DENIED {
        return Err(Refusal::NoRight);
    }
    if status == NERR_UserExists {
        let pass_info = USER_INFO_1003 {
            usri1003_password: wide_pass.as_mut_ptr(),
        };
        let status = unsafe {
            NetUserSetInfo(
                std::ptr::null(),
                wide_name.as_ptr(),
                1003,
                (&pass_info as *const USER_INFO_1003).cast(),
                &mut parm_err,
            )
        };
        if status != NERR_Success {
            return Err(Refusal::Failed(format!(
                "NetUserSetInfo(password): {}",
                io::Error::from_raw_os_error(status as i32)
            )));
        }
        let flag_info = USER_INFO_1008 {
            usri1008_flags: flags,
        };
        let status = unsafe {
            NetUserSetInfo(
                std::ptr::null(),
                wide_name.as_ptr(),
                1008,
                (&flag_info as *const USER_INFO_1008).cast(),
                &mut parm_err,
            )
        };
        if status != NERR_Success {
            return Err(Refusal::Failed(format!(
                "NetUserSetInfo(flags): {}",
                io::Error::from_raw_os_error(status as i32)
            )));
        }
    } else if status != NERR_Success {
        return Err(Refusal::Failed(format!(
            "NetUserAdd({name}): {}",
            io::Error::from_raw_os_error(status as i32)
        )));
    }

    let sid = Sid::of_account(name)
        .map_err(|e| Refusal::Failed(format!("LookupAccountNameW({name}): {e}")))?;
    add_to_users_group(&sid).map_err(Refusal::Failed)?;
    let _ = hide_from_welcome_screen(name);
    Ok(())
}

fn add_to_users_group(account_sid: &Sid) -> Result<(), String> {
    let users_sid =
        Sid::well_known(WinBuiltinUsersSid).map_err(|e| format!("WinBuiltinUsersSid: {e}"))?;
    let mut group_name = vec![0u16; 256];
    let mut group_len = group_name.len() as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind: SID_NAME_USE = 0;
    check(unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            users_sid.as_psid(),
            group_name.as_mut_ptr(),
            &mut group_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    })
    .map_err(|e| format!("LookupAccountSidW(Users): {e}"))?;

    let member = LOCALGROUP_MEMBERS_INFO_0 {
        lgrmi0_sid: account_sid.as_psid(),
    };
    let status = unsafe {
        NetLocalGroupAddMembers(
            std::ptr::null(),
            group_name.as_ptr(),
            0,
            (&member as *const LOCALGROUP_MEMBERS_INFO_0).cast(),
            1,
        )
    };
    if status != NERR_Success && status != ERROR_MEMBER_IN_ALIAS && status != NERR_GroupNotFound {
        return Err(format!(
            "NetLocalGroupAddMembers(Users): {}",
            io::Error::from_raw_os_error(status as i32)
        ));
    }
    Ok(())
}

const SPECIAL_ACCOUNTS_SUBKEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

fn hide_from_welcome_screen(name: &str) -> io::Result<()> {
    let subkey = wide(OsStr::new(SPECIAL_ACCOUNTS_SUBKEY))?;
    let value_name = wide(OsStr::new(name))?;
    let mut key: HKEY = std::ptr::null_mut();
    super::wide::win32(unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    })?;
    let zero: u32 = 0;
    let result = super::wide::win32(unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_DWORD,
            (&zero as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    });
    unsafe { RegCloseKey(key) };
    result
}

fn unhide_from_welcome_screen(name: &str) -> io::Result<()> {
    let subkey = wide(OsStr::new(SPECIAL_ACCOUNTS_SUBKEY))?;
    let value_name = wide(OsStr::new(name))?;
    let status =
        unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), value_name.as_ptr()) };
    if status == 0 || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_is_deterministic_17_chars_and_valid() {
        let a = account_name_for_sid("S-1-5-21-111-222-333-1001");
        assert_eq!(a.len(), 17, "{a}");
        assert!(valid_account_name(&a), "{a}");
        assert_eq!(a, account_name_for_sid("S-1-5-21-111-222-333-1001"));
        assert_ne!(a, account_name_for_sid("S-1-5-21-111-222-333-1002"));
        assert!(!valid_account_name("Administrator"));
        assert!(!valid_account_name("rune-sbx-1234567"));
        assert!(!valid_account_name("rune-sbx-123456789"));
        assert!(!valid_account_name("rune-sbx-1234567G"));
    }

    #[test]
    fn private_credentials_round_trip_and_reject_tampered_names() {
        let dir = std::env::temp_dir().join(format!("rune-acct-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = account_file(&dir);
        let name = account_name_for_sid("S-1-5-21-10-20-30-1000");
        let pass = random_password();
        write_credentials(&file, &name, &pass).unwrap();
        assert!(super::super::files::is_private(&file).unwrap());
        let (read_name, read_pass) = read_credentials(&file).unwrap();
        assert_eq!(read_name, name);
        assert_eq!(read_pass, pass);

        write_credentials(&file, "Administrator", &pass).unwrap();
        assert!(read_credentials(&file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
