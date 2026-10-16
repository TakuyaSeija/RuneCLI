//! Windows security as Rune uses it: who this process runs as, the SIDs Rune
//! derives for its own grants, a descriptor that lets only this user in, and
//! ACL entries added to a folder and taken back.

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SDDL_REVISION_1, SET_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    CopySid, CreateWellKnownSid, EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorControl,
    GetTokenInformation, TokenElevation, TokenGroups, TokenIntegrityLevel, TokenUser,
    ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, INHERITED_ACE, NO_INHERITANCE,
    OBJECT_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_ACCESS_MASK, TOKEN_ELEVATION, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SE_GROUP_LOGON_ID};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::wide::{check, from_wide, owned, wide, wide_path, win32, Local};

/// A security identifier, owned.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct Sid(Vec<u8>);

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string().unwrap_or_else(|_| "S-?".into()))
    }
}

impl Sid {
    pub(super) fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// A copy of the SID at `psid`.
    pub(super) fn copy(psid: PSID) -> io::Result<Sid> {
        let len = unsafe { GetLengthSid(psid) };
        let mut buffer = vec![0u8; len as usize];
        check(unsafe { CopySid(len, buffer.as_mut_ptr().cast(), psid) })?;
        Ok(Sid(buffer))
    }

    /// One of the SIDs Windows names (Everyone, Users, …).
    pub(super) fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Sid> {
        let mut buffer = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = SECURITY_MAX_SID_SIZE;
        check(unsafe {
            CreateWellKnownSid(
                kind,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        })?;
        buffer.truncate(size as usize);
        Ok(Sid(buffer))
    }

    /// A SID of Rune's own for `name`, the way Windows derives a capability's
    /// SID from its name: `S-1-15-3-` and eight numbers from a SHA-256 of the
    /// name. No account or group has it; only a token Rune builds carries it,
    /// so an entry granting it lets in Rune's confined commands and nothing
    /// else.
    pub(super) fn derived(name: &str) -> Sid {
        use sha2::Digest;
        let hash = sha2::Sha256::digest(format!("rune:{name}").as_bytes());
        // Revision 1, nine sub-authorities, authority 15 (app packages):
        // 3 (capability base), then the hash.
        let mut sid = vec![1u8, 9, 0, 0, 0, 0, 0, 15];
        sid.extend_from_slice(&3u32.to_le_bytes());
        sid.extend_from_slice(&hash[..32]);
        Sid(sid)
    }

    /// Its `S-1-…` spelling.
    pub(super) fn string(&self) -> io::Result<String> {
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        check(unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) })?;
        let local = Local(text);
        let mut len = 0;
        while unsafe { *local.0.add(len) } != 0 {
            len += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(local.0, len) };
        Ok(from_wide(slice).to_string_lossy().into_owned())
    }

    pub(super) fn equals(&self, psid: PSID) -> bool {
        !psid.is_null() && unsafe { EqualSid(self.as_psid(), psid) } != 0
    }
}

/// A process or thread token.
pub(super) struct Token(pub(super) OwnedHandle);

impl Token {
    /// This process's own token, opened for `access`.
    pub(super) fn of_this_process(access: TOKEN_ACCESS_MASK) -> io::Result<Token> {
        let mut handle: HANDLE = std::ptr::null_mut();
        check(unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) })?;
        Ok(Token(owned(handle)?))
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// What `GetTokenInformation` says for `class`, in memory aligned for
    /// the structure it holds.
    pub(super) fn info(&self, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
        let mut size = 0u32;
        unsafe { GetTokenInformation(self.raw(), class, std::ptr::null_mut(), 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        check(unsafe {
            GetTokenInformation(
                self.raw(),
                class,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut size,
            )
        })?;
        Ok(buffer)
    }

    /// The user the token is.
    pub(super) fn user(&self) -> io::Result<Sid> {
        let info = self.info(TokenUser)?;
        let user = unsafe { &*(info.as_ptr() as *const TOKEN_USER) };
        Sid::copy(user.User.Sid)
    }

    /// The SID of the logon session, which the window station and desktop
    /// grant — a confined process needs it to start at all.
    pub(super) fn logon_sid(&self) -> Option<Sid> {
        let info = self.info(TokenGroups).ok()?;
        let groups = unsafe { &*(info.as_ptr() as *const TOKEN_GROUPS) };
        let all = unsafe {
            std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
        };
        all.iter()
            .find(|group| group.Attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32)
            .and_then(|group| Sid::copy(group.Sid).ok())
    }

    /// Whether the token is elevated (an Administrator's full rights).
    pub(super) fn elevated(&self) -> bool {
        self.info(TokenElevation)
            .map(
                |info| unsafe { (*(info.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated } != 0,
            )
            .unwrap_or(false)
    }

    /// The token's integrity level: the last number of its label SID
    /// (0x2000 medium, 0x3000 high, 0x4000 system).
    pub(super) fn integrity(&self) -> Option<u32> {
        let info = self.info(TokenIntegrityLevel).ok()?;
        let label = unsafe { &*(info.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
        let sid = Sid::copy(label.Label.Sid).ok()?;
        let count = *sid.0.get(1)? as usize;
        let at = 8 + 4 * count.checked_sub(1)?;
        Some(u32::from_le_bytes(sid.0.get(at..at + 4)?.try_into().ok()?))
    }
}

/// The user this process runs as.
pub(super) fn current_user() -> io::Result<&'static Sid> {
    static USER: OnceLock<Option<Sid>> = OnceLock::new();
    USER.get_or_init(|| {
        Token::of_this_process(TOKEN_QUERY)
            .and_then(|token| token.user())
            .ok()
    })
    .as_ref()
    .ok_or_else(|| io::Error::other("Windows did not say which user this process runs as"))
}

/// Security attributes that let only this user in — for a file Rune creates
/// that is this user's alone: a key, a lock, a host's log. Built once and
/// kept for the life of the process: an `OpenOptions` holds a pointer to them
/// until it opens.
pub(super) fn private_attributes() -> io::Result<*mut SECURITY_ATTRIBUTES> {
    struct Private(SECURITY_ATTRIBUTES);
    // SAFETY: built once, never written again; the descriptor it points to
    // is never freed.
    unsafe impl Send for Private {}
    unsafe impl Sync for Private {}
    static PRIVATE: OnceLock<Option<Box<Private>>> = OnceLock::new();
    let private = PRIVATE.get_or_init(|| {
        let user = current_user().ok()?.string().ok()?;
        // Owned by the user, a protected DACL: the user, everything.
        let sddl = wide(format!("O:{user}D:P(A;OICI;FA;;;{user})").as_ref()).ok()?;
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then(|| {
            Box::new(Private(SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            }))
        })
    });
    match private {
        Some(private) => Ok(&private.0 as *const SECURITY_ATTRIBUTES as *mut SECURITY_ATTRIBUTES),
        None => Err(io::Error::other(
            "no security descriptor could be built for a private file",
        )),
    }
}

/// A path's DACL, as `GetNamedSecurityInfoW` read it.
pub(super) struct Dacl {
    descriptor: Local<c_void>,
    acl: *mut ACL,
}

impl Dacl {
    pub(super) fn of(path: &Path) -> io::Result<Dacl> {
        let name = wide_path(path)?;
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        win32(unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        })?;
        Ok(Dacl {
            descriptor: Local(descriptor),
            acl,
        })
    }

    /// Every allowing entry: its SID, its access mask, and whether it was
    /// handed down from a folder above.
    fn allowed(&self) -> Vec<(PSID, u32, bool)> {
        let mut out = Vec::new();
        if self.acl.is_null() {
            return out;
        }
        let count = unsafe { (*self.acl).AceCount };
        for index in 0..count as u32 {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if unsafe { GetAce(self.acl, index, &mut ace) } == 0 || ace.is_null() {
                continue;
            }
            let allowed = ace as *const ACCESS_ALLOWED_ACE;
            if unsafe { (*allowed).Header.AceType } as u32 != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let sid = unsafe { std::ptr::addr_of!((*allowed).SidStart) } as PSID;
            let inherited = u32::from(unsafe { (*allowed).Header.AceFlags }) & INHERITED_ACE != 0;
            out.push((sid, unsafe { (*allowed).Mask }, inherited));
        }
        out
    }

    /// Whether an entry allows `sid` at least `mask`.
    pub(super) fn grants(&self, sid: &Sid, mask: u32) -> bool {
        self.allowed()
            .into_iter()
            .any(|(entry, granted, _)| sid.equals(entry) && granted & mask == mask)
    }

    /// Whether an entry of the path's own — not one handed down — allows
    /// `sid` anything: one [`revoke`] would take off.
    pub(super) fn names(&self, sid: &Sid) -> bool {
        self.allowed()
            .into_iter()
            .any(|(entry, _, inherited)| !inherited && sid.equals(entry))
    }

    /// Whether only `sid` is let in, and nothing is inherited from above.
    pub(super) fn only(&self, sid: &Sid) -> bool {
        let mut control = 0u16;
        let mut revision = 0u32;
        let protected =
            unsafe { GetSecurityDescriptorControl(self.descriptor.0, &mut control, &mut revision) }
                != 0
                && control & SE_DACL_PROTECTED != 0;
        protected
            && !self.acl.is_null()
            && self
                .allowed()
                .into_iter()
                .all(|(entry, _, _)| sid.equals(entry))
    }
}

/// An entry for `sid` in `path`'s DACL — `mask` to it, and, for a folder,
/// to everything in it (inherited) — unless one already grants it. Windows
/// carries an inheritable entry down to what the folder already holds, once.
/// `Ok(true)` when it was added.
pub(super) fn grant(path: &Path, sid: &Sid, mask: u32, folder: bool) -> io::Result<bool> {
    let current = Dacl::of(path)?;
    if current.grants(sid, mask) {
        return Ok(false);
    }
    let inheritance = if folder {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    edit(path, &current, sid, GRANT_ACCESS, mask, inheritance, 0)?;
    Ok(true)
}

/// Take every entry for `sid` out of `path`'s DACL.
pub(super) fn revoke(path: &Path, sid: &Sid) -> io::Result<()> {
    let current = Dacl::of(path)?;
    edit(path, &current, sid, REVOKE_ACCESS, 0, NO_INHERITANCE, 0)
}

/// Make `path` this user's alone: a protected DACL letting the user, and
/// nobody else, do everything — for a folder, to what it holds too.
pub(super) fn set_private(path: &Path, folder: bool) -> io::Result<()> {
    let user = current_user()?;
    let inheritance = if folder {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    let empty = Dacl {
        descriptor: Local::null(),
        acl: std::ptr::null_mut(),
    };
    edit(
        path,
        &empty,
        user,
        SET_ACCESS,
        FILE_ALL_ACCESS,
        inheritance,
        PROTECTED_DACL_SECURITY_INFORMATION,
    )
}

fn edit(
    path: &Path,
    current: &Dacl,
    sid: &Sid,
    mode: windows_sys::Win32::Security::Authorization::ACCESS_MODE,
    mask: u32,
    inheritance: windows_sys::Win32::Security::ACE_FLAGS,
    extra: OBJECT_SECURITY_INFORMATION,
) -> io::Result<()> {
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.as_psid() as windows_sys::core::PWSTR,
        },
    };
    let mut updated: *mut ACL = std::ptr::null_mut();
    win32(unsafe { SetEntriesInAclW(1, &entry, current.acl, &mut updated) })?;
    let updated = Local(updated);
    let name = wide_path(path)?;
    win32(unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | extra,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            updated.0,
            std::ptr::null(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_derived_sid_is_stable_and_names_nothing_else() {
        let a = Sid::derived("project:C:\\work\\one");
        assert_eq!(a, Sid::derived("project:C:\\work\\one"));
        assert_ne!(a, Sid::derived("project:C:\\work\\two"));
        let text = a.string().unwrap();
        assert!(text.starts_with("S-1-15-3-"), "{text}");
        assert_eq!(text.matches('-').count(), 11, "{text}");
    }

    #[test]
    fn this_process_knows_its_user() {
        let user = current_user().unwrap();
        assert!(user.string().unwrap().starts_with("S-1-5-"));
        let token = Token::of_this_process(TOKEN_QUERY).unwrap();
        assert_eq!(&token.user().unwrap(), user);
        assert!(token.integrity().is_some());
    }

    #[test]
    fn a_granted_entry_is_seen_and_taken_back() {
        let dir = std::env::temp_dir().join(format!("rune-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inside")).unwrap();
        let sid = Sid::derived(&format!("test:{}", dir.display()));
        assert!(!Dacl::of(&dir).unwrap().grants(&sid, FILE_ALL_ACCESS));
        assert!(grant(&dir, &sid, FILE_ALL_ACCESS, true).unwrap());
        assert!(!grant(&dir, &sid, FILE_ALL_ACCESS, true).unwrap(), "once");
        assert!(Dacl::of(&dir).unwrap().grants(&sid, FILE_ALL_ACCESS));
        assert!(Dacl::of(&dir).unwrap().names(&sid));
        let inside = Dacl::of(&dir.join("inside")).unwrap();
        assert!(
            inside.grants(&sid, FILE_ALL_ACCESS),
            "what the folder holds inherits it"
        );
        assert!(!inside.names(&sid), "handed down, not its own");
        revoke(&dir, &sid).unwrap();
        assert!(!Dacl::of(&dir).unwrap().grants(&sid, FILE_ALL_ACCESS));
        assert!(
            !Dacl::of(&dir.join("inside"))
                .unwrap()
                .grants(&sid, FILE_ALL_ACCESS),
            "what was handed down goes with it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_private_file_lets_only_its_user_in() {
        let dir = std::env::temp_dir().join(format!("rune-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key");
        std::fs::write(&file, "k").unwrap();
        let user = current_user().unwrap();
        assert!(
            !Dacl::of(&file).unwrap().only(user),
            "inherits from its folder at first"
        );
        set_private(&file, false).unwrap();
        assert!(Dacl::of(&file).unwrap().only(user));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "k",
            "its user still reads it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
