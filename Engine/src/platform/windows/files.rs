//! Files as Windows sees them: identity (a volume and a 128-bit file id —
//! ReFS needs every bit), permissions (an ACL; the read-only attribute is
//! what an undo puts back), handles, and opening with no link or junction
//! anywhere on the way.

use std::fs::File;
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf};

use windows_sys::Win32::Foundation::{
    SetHandleInformation, ERROR_PIPE_BUSY, HANDLE, HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileIdInfo, GetDiskFreeSpaceExW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFileType, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
    FILE_WRITE_DATA, OPEN_EXISTING,
};

use super::security::{current_user, set_private, Dacl};
use super::wide::{check, owned, wide_path};

/// Open `path` to ask about it — no read or write — following a link at its
/// end or not; folders too (`FILE_FLAG_BACKUP_SEMANTICS`).
fn open_to_ask(path: &Path, follow: bool, share: u32) -> io::Result<OwnedHandle> {
    let name = wide_path(path)?;
    let flags = FILE_FLAG_BACKUP_SEMANTICS
        | if follow {
            0
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
    owned(unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    })
}

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Which file a path reaches: two paths, or a path and an open file, name the
/// same file exactly when their identities are equal — a hard link, or the
/// same name spelt in another case, alike. An identity says nothing once the
/// file is gone; it is compared, never kept across processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    volume: u64,
    file: [u8; 16],
}

impl FileIdentity {
    fn of_handle(handle: HANDLE) -> io::Result<FileIdentity> {
        let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut id as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok != 0 {
            return Ok(FileIdentity {
                volume: id.VolumeSerialNumber,
                file: id.FileId.Identifier,
            });
        }
        // Before the 128-bit id (or a filesystem that does not give it):
        // the 64-bit index.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        check(unsafe { GetFileInformationByHandle(handle, &mut info) })?;
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        let mut file = [0u8; 16];
        file[..8].copy_from_slice(&index.to_le_bytes());
        Ok(FileIdentity {
            volume: u64::from(info.dwVolumeSerialNumber),
            file,
        })
    }

    /// The file `path` reaches, links followed.
    pub fn of_path(path: &Path) -> io::Result<FileIdentity> {
        let handle = open_to_ask(path, true, SHARE_ALL)?;
        Self::of_handle(handle.as_raw_handle())
    }

    /// The entry at `path` itself — a link is its own entry, not its target.
    pub fn of_entry(path: &Path) -> io::Result<FileIdentity> {
        let handle = open_to_ask(path, false, SHARE_ALL)?;
        Self::of_handle(handle.as_raw_handle())
    }

    /// The file `file` has open.
    pub fn of_file(file: &File) -> io::Result<FileIdentity> {
        Self::of_handle(file.as_raw_handle())
    }
}

/// `path` with every link resolved, as the volume names it — spelt without
/// the `\\?\` that `GetFinalPathNameByHandle` puts first wherever the
/// path reads the same without it, so a path Rune shows, compares or starts a
/// command in (`cmd.exe` takes no `\\?\` folder) is the person's spelling.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    let verbatim = std::fs::canonicalize(path)?;
    Ok(simplified(&verbatim).unwrap_or(verbatim))
}

/// `\\?\C:\x` as `C:\x` and `\\?\UNC\server\share\x` as
/// `\\server\share\x` — only when every name in it is plain
/// (`windows_name_trouble`) and the whole fits in `MAX_PATH`; `None` when it
/// must keep its prefix to mean the same file.
fn simplified(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let (lead, rest) = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        (r"\\", rest)
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        let drive = rest.as_bytes();
        if drive.len() < 3
            || !drive[0].is_ascii_alphabetic()
            || drive[1] != b':'
            || drive[2] != b'\\'
        {
            return None;
        }
        ("", rest)
    } else {
        return Some(path.to_path_buf());
    };
    let names = rest.split('\\').skip(if lead.is_empty() { 1 } else { 2 });
    for name in names {
        if crate::platform::windows_name_trouble(name).is_some() {
            return None;
        }
    }
    let simple = format!("{lead}{rest}");
    (simple.encode_utf16().count() < 260).then(|| PathBuf::from(simple))
}

/// Nothing to do on Windows: a folder takes no flush, and NTFS logs the names
/// a folder holds with the file they name, flushed with it (`sync_all`).
pub fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Put `from` in the place of `to` in one step — a reader finds the old file
/// or the new one, never part of either; one holding the old file keeps it
/// (the standard library renames with POSIX semantics) — on disk when this
/// returns: the file is flushed under its new name, NTFS's log and the
/// rename in it with it.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)?;
    std::fs::OpenOptions::new().write(true).open(to)?.sync_all()
}

/// Whether nobody but this user may reach `path`: a protected ACL that
/// lets only this user in.
pub fn is_private(path: &Path) -> io::Result<bool> {
    Ok(Dacl::of(path)?.only(current_user()?))
}

/// Whether two paths reach one file — the same name spelt in another case, or
/// two links to it.
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!(
        (FileIdentity::of_path(a), FileIdentity::of_path(b)),
        (Ok(a), Ok(b)) if a == b
    )
}

/// Bytes this user may still write on the volume holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let name = wide_path(path)?;
    let mut free = 0u64;
    check(unsafe {
        GetDiskFreeSpaceExW(
            name.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    })?;
    Ok(free)
}

/// A file's permission bits as Windows keeps what Unix calls them: whether it
/// is read-only — what an undo puts back beside the content (`checkpoint`).
/// Who may reach a file is its ACL, which an undo never changes.
pub fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

/// Give `path` the read-only state [`file_mode`] read: read-only when no
/// write bit is set.
pub fn set_file_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(path, permissions)
}

/// Open the file at `path` as `how` says, created this user's alone when it
/// is made — owned by the user, a protected ACL letting only the user in,
/// whatever the folder would hand down: a key, a lock, a host's log. An
/// existing file keeps the ACL it has. No file at all when the descriptor
/// cannot be built: a key written where anyone may read it is the failure
/// this exists to prevent.
pub fn open_private(path: &Path, how: crate::platform::Private) -> io::Result<File> {
    use crate::platform::Private;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, OPEN_ALWAYS,
    };
    let attributes = super::security::private_attributes()?;
    let (access, disposition) = match how {
        Private::Lock => (GENERIC_READ | GENERIC_WRITE, OPEN_ALWAYS),
        Private::New => (GENERIC_WRITE, CREATE_NEW),
        // Appending only: every write lands at the end, as `O_APPEND` does.
        Private::Append => (FILE_GENERIC_WRITE & !FILE_WRITE_DATA, OPEN_ALWAYS),
    };
    let name = wide_path(path)?;
    let handle = owned(unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            SHARE_ALL,
            attributes,
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    })?;
    Ok(File::from(handle))
}

/// Make `path` this user's alone — a protected ACL letting only the user in,
/// for a folder handed down to what it holds — when anyone else may reach it;
/// one already private is left as it is. A person who loosened it by hand is
/// protected without knowing the rule.
pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let user = current_user()?;
    if Dacl::of(path)?.only(user) {
        return Ok(());
    }
    set_private(path, std::fs::metadata(path)?.is_dir())
}

/// Remove whatever is at `path` without following a link: a folder with
/// everything in it, a file, a link as a link — never what it points at.
/// Windows removes a link to a folder (a symbolic link or a junction) as a
/// folder: deleted as a file it answers "Access is denied" (CI, 2026-10-17:
/// help repair failed on a revision folder turned into a link). Nothing there
/// is no failure.
pub fn remove_entry(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink_dir() => std::fs::remove_dir(path),
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether this process may write to `path` — for a named pipe, whether it
/// may talk to whatever serves it (one busy now is still reachable).
/// Existence alone proves nothing.
pub fn writable(path: &Path) -> bool {
    let Ok(name) = wide_path(path) else {
        return false;
    };
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_WRITE_DATA,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    match owned(handle) {
        Ok(_) => true,
        Err(error) => error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32),
    }
}

/// Open an untrusted path for reading — only when it is a file on a disk: a
/// device (`CON`, `NUL`, `COM1`) or a pipe is refused before anything is read
/// from it, as a FIFO is never waited on elsewhere.
pub fn open_read_nonblocking(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(path)?;
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a device or a pipe, not a file", path.display()),
        ));
    }
    Ok(file)
}

/// Whether the entry `handle` has open is a link or junction (any reparse
/// point), and whether it is a folder.
fn attributes(handle: HANDLE) -> io::Result<(bool, bool)> {
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    check(unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    })?;
    Ok((
        tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
    ))
}

fn a_link_is_on_the_way(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} is a link or junction: a path read without links never follows one",
        path.display()
    ))
}

/// A folder opened with no link or junction anywhere in its path, and what is
/// read beneath it opened the same way — so one planted or swapped in at any
/// point is refused, never followed.
///
/// Each folder on the way, from the volume's root, is opened as itself
/// (`FILE_FLAG_OPEN_REPARSE_POINT`), refused when it is a reparse point, and
/// held open without letting anyone delete or rename it: while it is held,
/// no folder on the way can be swapped for a link.
pub struct LinkFreeDir {
    path: PathBuf,
    _held: Vec<OwnedHandle>,
}

/// Held folders may be read and written by others, never renamed or deleted.
const SHARE_NO_DELETE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

impl LinkFreeDir {
    /// The folder at absolute `path`, reached through no link or junction.
    pub fn open(path: &Path) -> io::Result<LinkFreeDir> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a folder opened without links is named by an absolute path",
            ));
        }
        let mut here = PathBuf::new();
        let mut held = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    here.push(component.as_os_str());
                    if matches!(component, Component::RootDir) {
                        held.push(Self::hold(&here)?);
                    }
                }
                Component::Normal(_) => {
                    here.push(component.as_os_str());
                    held.push(Self::hold(&here)?);
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a path read without links names folders and files only",
                    ))
                }
            }
        }
        Ok(LinkFreeDir {
            path: here,
            _held: held,
        })
    }

    /// The folder at `path` itself, refused when it is a link or not a folder.
    fn hold(path: &Path) -> io::Result<OwnedHandle> {
        let handle = open_to_ask(path, false, SHARE_NO_DELETE)?;
        let (link, folder) = attributes(handle.as_raw_handle())?;
        if link {
            return Err(a_link_is_on_the_way(path));
        }
        if !folder {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", path.display()),
            ));
        }
        Ok(handle)
    }

    /// The entry at `relative` beneath this folder, opened for reading with
    /// no link on the way (the caller checks what it opened). `relative`
    /// names folders and a file only: no `..`, no root, no drive.
    pub fn open_file(&self, relative: &Path) -> io::Result<File> {
        let mut parts: Vec<Component<'_>> = relative.components().collect();
        let Some(Component::Normal(name)) = parts.pop() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a path read without links names a file",
            ));
        };
        let mut here = self.path.clone();
        let mut held = Vec::new();
        for part in parts {
            let Component::Normal(folder) = part else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a path read without links names folders and files only",
                ));
            };
            here.push(folder);
            held.push(Self::hold(&here)?);
        }
        here.push(name);
        let wide_name = wide_path(&here)?;
        let handle = owned(unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                FILE_GENERIC_READ,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        })?;
        let (link, _) = attributes(handle.as_raw_handle())?;
        if link {
            return Err(a_link_is_on_the_way(&here));
        }
        drop(held);
        Ok(File::from(handle))
    }
}

/// Something this process keeps open for a successor — on Unix across an
/// `exec`. Windows has no `exec` (`REPLACES_IN_PLACE` is false), so nothing
/// is ever handed on there; the type is the record's shape, a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Handed(pub(in crate::platform) i64);

impl Handed {
    /// Whether a child would inherit it.
    pub fn keep_across_exec(self, keep: bool) -> io::Result<()> {
        check(unsafe {
            SetHandleInformation(
                self.0 as HANDLE,
                HANDLE_FLAG_INHERIT,
                if keep { HANDLE_FLAG_INHERIT } else { 0 },
            )
        })
    }

    /// Close what was handed on.
    pub fn close(self) {
        if self.0 != 0 && self.0 != -1 {
            drop(unsafe { OwnedHandle::from_raw_handle(self.0 as HANDLE) });
        }
    }
}

/// Mark `file` to be handed on, under the number the returned [`Handed`]
/// names.
pub fn hand_on(file: File) -> io::Result<Handed> {
    let handle = file.into_raw_handle();
    let handed = Handed(handle as i64);
    if let Err(error) = handed.keep_across_exec(true) {
        handed.close();
        return Err(error);
    }
    Ok(handed)
}

/// Take over a file handed on — only when it is the file at `expected` (the
/// same file, not merely the same name). Any other number is left as it was.
pub fn take_handed_file(handed: Handed, expected: &Path) -> Option<File> {
    let held = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(handed.0 as HANDLE) });
    let same = matches!(
        (FileIdentity::of_file(&held), FileIdentity::of_path(expected)),
        (Ok(a), Ok(b)) if a == b
    );
    if !same {
        return None;
    }
    let file = std::mem::ManuallyDrop::into_inner(held);
    let _ = Handed(file.as_raw_handle() as i64).keep_across_exec(false);
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path past `MAX_PATH` — a project deep in folders — is opened by
    /// Rune's own calls as by the standard library's: made private, its ACL
    /// read, told private.
    #[test]
    fn a_path_past_max_path_is_reached_by_rune_s_own_calls() {
        let mut deep = std::env::temp_dir().join(format!("rune-long-{}", std::process::id()));
        let base = deep.clone();
        while deep.as_os_str().len() < 300 {
            deep = deep.join("a-folder-name-that-is-forty-characters-long");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("key");
        drop(open_private(&file, crate::platform::Private::New).unwrap());
        assert!(file.as_os_str().len() > 260);
        assert!(is_private(&file).unwrap());
        assert!(FileIdentity::of_path(&file).is_ok());
        let _ = std::fs::remove_dir_all(&base);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn identities_name_files_not_spellings() {
        let dir = scratch("identity");
        let a = dir.join("a");
        std::fs::write(&a, "x").unwrap();
        std::fs::hard_link(&a, dir.join("b")).unwrap();
        assert!(same_file(&a, &dir.join("b")));
        assert!(
            same_file(&a, &dir.join("A")),
            "the same name in another case"
        );
        let open = File::open(&a).unwrap();
        assert_eq!(
            FileIdentity::of_file(&open).unwrap(),
            FileIdentity::of_path(&a).unwrap()
        );
        assert!(!same_file(&a, &dir.join("missing")));
        drop(open);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_reads_only_from_a_link_free_path() {
        let dir = scratch("linkfree");
        std::fs::create_dir_all(dir.join("pkg/docs")).unwrap();
        std::fs::write(dir.join("pkg/docs/guide.md"), "guide").unwrap();
        let pkg = LinkFreeDir::open(&dir.join("pkg")).unwrap();
        let mut text = String::new();
        use std::io::Read;
        pkg.open_file(Path::new("docs/guide.md"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "guide");
        assert_eq!(
            pkg.open_file(Path::new("docs/none.md")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for outside in [
            "..\\secret",
            "C:\\Windows\\win.ini",
            "docs\\..\\guide.md",
            "",
        ] {
            assert_eq!(
                pkg.open_file(Path::new(outside)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{outside}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_canonical_path_is_spelt_without_its_prefix_where_it_reads_the_same() {
        assert_eq!(
            simplified(Path::new(r"\\?\C:\Users\me\proj")),
            Some(PathBuf::from(r"C:\Users\me\proj"))
        );
        assert_eq!(
            simplified(Path::new(r"\\?\UNC\server\share\x")),
            Some(PathBuf::from(r"\\server\share\x"))
        );
        assert_eq!(simplified(Path::new(r"\\?\C:\dir\nul")), None);
        assert_eq!(simplified(Path::new(r"\\?\C:\dir\name.")), None);
        assert_eq!(simplified(Path::new(r"\\?\GLOBALROOT\Device\x")), None);
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(simplified(Path::new(&long)), None);
        let dir = scratch("canonical");
        let found = canonical(&dir).unwrap();
        assert!(
            !found.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            found.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_device_is_never_opened_as_a_file() {
        for device in ["NUL", "CON"] {
            assert!(
                open_read_nonblocking(Path::new(device)).is_err(),
                "{device}"
            );
        }
    }

    #[test]
    fn a_private_file_is_created_private() {
        let dir = scratch("created");
        let file = dir.join("key");
        open_private(&file, crate::platform::Private::New).unwrap();
        assert!(Dacl::of(&file).unwrap().only(current_user().unwrap()));
        let other = dir.join("loose");
        std::fs::write(&other, "x").unwrap();
        restrict_to_owner(&other).unwrap();
        assert!(Dacl::of(&other).unwrap().only(current_user().unwrap()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_read_only_file_is_read_only_again_after_an_undo() {
        let dir = scratch("mode");
        let file = dir.join("f");
        std::fs::write(&file, "x").unwrap();
        set_file_mode(&file, 0o444).unwrap();
        assert_eq!(file_mode(&std::fs::metadata(&file).unwrap()), 0o444);
        set_file_mode(&file, 0o644).unwrap();
        assert_eq!(file_mode(&std::fs::metadata(&file).unwrap()), 0o666);
        let _ = std::fs::remove_dir_all(dir);
    }
}
