//! Files as a Unix kernel sees them (Linux and macOS alike): identity,
//! permissions, descriptors kept across `exec`, and opening without following
//! links.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

/// Wait up to `timeout_ms` for `fd` to be readable, then read what is there.
///
/// `Ok(None)`     nothing arrived in the window (or a signal interrupted it)
/// `Ok(Some(0))`  the other end closed
/// `Ok(Some(n))`  `n` bytes
pub(in crate::platform) fn read_timeout(
    fd: RawFd,
    buf: &mut [u8],
    timeout_ms: i32,
) -> io::Result<Option<usize>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(e);
    }
    if rc == 0 {
        return Ok(None);
    }
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(Some(n as usize))
}

/// Whether `fd` closes when this process `exec`s.
pub(in crate::platform) fn close_on_exec(fd: RawFd, close: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if close {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Something this process keeps open across the `exec` of a newer build —
/// a session's ownership lock, a background task's terminal, the program of
/// the build that handed over — and that the program it becomes takes over.
///
/// The handover record names it by its descriptor number, and its JSON is
/// that number alone, exactly as every build since 0.1.6 wrote it: the build
/// handing over and the one taking over never share code, only the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Handed(pub(in crate::platform) RawFd);

impl Handed {
    /// Whether it stays open across this process's next `exec`. Taken over
    /// (or taken back from a handover that did not happen), it closes on
    /// exec again, like every descriptor Rune opens.
    pub fn keep_across_exec(self, keep: bool) -> io::Result<()> {
        close_on_exec(self.0, !keep)
    }

    /// Close what was handed on and is not needed here — the previous
    /// build's program once this one took the home over.
    pub fn close(self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

/// Hand `file` to the program this process `exec`s next: it stays open
/// across the exec, under the number the returned [`Handed`] names. Done last
/// before the exec, so no program started meanwhile can inherit it.
pub fn hand_on(file: File) -> io::Result<Handed> {
    let fd = file.into_raw_fd();
    if let Err(error) = close_on_exec(fd, false) {
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        return Err(error);
    }
    Ok(Handed(fd))
}

/// Take over a file the program this process was before handed on — only
/// when it is the regular file at `expected` (the same file, not merely the
/// same name). Any other number is left exactly as it was, neither taken nor
/// closed: it may be something else this process holds. It closes on exec
/// again from here on.
pub fn take_handed_file(handed: Handed, expected: &Path) -> Option<File> {
    let held = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(handed.0) });
    let same = match (held.metadata(), std::fs::metadata(expected)) {
        (Ok(held), Ok(named)) => {
            held.is_file() && (held.dev(), held.ino()) == (named.dev(), named.ino())
        }
        _ => false,
    };
    if !same {
        return None;
    }
    let file = std::mem::ManuallyDrop::into_inner(held);
    let _ = close_on_exec(file.as_raw_fd(), true);
    Some(file)
}

/// Opening an untrusted path must not wait for a FIFO peer before its metadata
/// can be checked. Nonblocking changes nothing for a regular file.
pub fn open_read_nonblocking(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// Which file a path reaches: two paths, or a path and an open file, name the
/// same file exactly when their identities are equal — a hard link, or the
/// same name spelt in another case where the filesystem ignores case, alike.
/// An identity says nothing once the file is gone; it is compared, never kept
/// across processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> FileIdentity {
        FileIdentity {
            device: meta.dev(),
            inode: meta.ino(),
        }
    }

    /// The file `path` reaches, links followed.
    pub fn of_path(path: &Path) -> io::Result<FileIdentity> {
        std::fs::metadata(path).map(|meta| Self::of(&meta))
    }

    /// The entry at `path` itself — a link is its own entry, not its target.
    pub fn of_entry(path: &Path) -> io::Result<FileIdentity> {
        std::fs::symlink_metadata(path).map(|meta| Self::of(&meta))
    }

    /// The file `file` has open.
    pub fn of_file(file: &File) -> io::Result<FileIdentity> {
        file.metadata().map(|meta| Self::of(&meta))
    }
}

/// `path` with every link resolved, as the filesystem names it.
pub fn canonical(path: &Path) -> io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

/// Make the names `dir` holds — a file just created or renamed in it —
/// survive a crash: the folder itself is flushed.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Put `from` in the place of `to` in one step — a reader finds the old file
/// or the new one, never part of either — on disk when this returns.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)?;
    match to.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => sync_dir(dir),
        _ => sync_dir(Path::new(".")),
    }
}

/// Whether nobody but this user may reach `path`: no permission for the
/// group or others.
pub fn is_private(path: &Path) -> io::Result<bool> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o077 == 0)
}

/// Whether two paths reach one file — the same name spelt in another case on a
/// filesystem that ignores case, or two links to it.
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!(
        (FileIdentity::of_path(a), FileIdentity::of_path(b)),
        (Ok(a), Ok(b)) if a == b
    )
}

/// Bytes this user may still write on the filesystem holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL"))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// A file's permission bits as this OS keeps them — what an undo puts back
/// beside the content (`checkpoint`), so a script stays executable.
pub fn file_mode(meta: &std::fs::Metadata) -> u32 {
    meta.permissions().mode()
}

/// Give `path` the permission bits [`file_mode`] read.
pub fn set_file_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Open the file at `path` as `how` says, created this user's alone
/// (`0600`) when it is made: a key, a lock, a host's log. An existing file
/// keeps the permissions it has.
pub fn open_private(path: &Path, how: crate::platform::Private) -> io::Result<File> {
    use crate::platform::Private;
    let mut options = std::fs::OpenOptions::new();
    match how {
        Private::Lock => options.read(true).write(true).create(true).truncate(false),
        Private::New => options.write(true).create_new(true),
        Private::Append => options.create(true).append(true),
    };
    options.mode(0o600).open(path)
}

/// Make `path` this user's alone — `0600` for a file, `0700` for a folder —
/// when anyone else may reach it; one already private is left as it is. A
/// person who loosened it by hand is protected without knowing the rule.
pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let meta = std::fs::metadata(path)?;
    if meta.permissions().mode() & 0o077 == 0 {
        return Ok(());
    }
    let private = if meta.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(private))
}

/// Remove whatever is at `path` without following a link: a folder with
/// everything in it, a file, a link as a link — never what it points at.
/// Nothing there is no failure.
pub fn remove_entry(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether this process may write to `path` — for a socket, whether it may
/// talk to whatever listens there. Existence alone proves nothing.
pub fn writable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_bytes())
        .is_ok_and(|name| unsafe { libc::access(name.as_ptr(), libc::W_OK) } == 0)
}

/// A folder opened with no symbolic link anywhere in its path, and what is
/// read beneath it opened the same way — so a link planted or swapped in at
/// any point is refused, never followed (a package root replaced by a link to
/// `~/.ssh` after it was found, a resource that points out of its package).
///
/// One path at a time, each component opened from the folder before it with
/// `O_NOFOLLOW` (the kernel refuses a link as the last component of an open,
/// magic `/proc` links included), so it holds on every kernel — it needs no
/// `openat2`, which kernels before 5.6 and many container profiles refuse.
pub struct LinkFreeDir(OwnedFd);

impl LinkFreeDir {
    /// The folder at absolute `path`, reached through no link.
    pub fn open(path: &Path) -> io::Result<LinkFreeDir> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a folder opened without links is named by an absolute path",
            ));
        }
        let mut dir = LinkFreeDir(open_at(
            libc::AT_FDCWD,
            Path::new("/").as_os_str(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )?);
        for component in components {
            dir = dir.child(component)?;
        }
        Ok(dir)
    }

    fn child(&self, component: Component<'_>) -> io::Result<LinkFreeDir> {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a path read without links names folders and files only",
            ));
        };
        Ok(LinkFreeDir(open_at(
            self.0.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?))
    }

    /// The entry at `relative` beneath this folder, opened for reading with
    /// no link on the way and without waiting on a FIFO's peer (the caller
    /// checks what it opened). `relative` names folders and a file only: no
    /// `..`, no root.
    pub fn open_file(&self, relative: &Path) -> io::Result<File> {
        let mut parts: Vec<Component<'_>> = relative.components().collect();
        let Some(Component::Normal(name)) = parts.pop() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a path read without links names a file",
            ));
        };
        let mut folder: Option<LinkFreeDir> = None;
        for part in parts {
            let next = folder.as_ref().unwrap_or(self).child(part)?;
            folder = Some(next);
        }
        let fd = open_at(
            folder.as_ref().unwrap_or(self).0.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )?;
        Ok(File::from(fd))
    }
}

fn open_at(dir: RawFd, name: &std::ffi::OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL"))?;
    loop {
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags) };
        if fd >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// What is beneath a folder is read without following a link anywhere:
    /// not in the folder's own path, not in the folders below it, not as the
    /// file itself.
    #[test]
    fn a_link_anywhere_on_the_way_is_refused_never_followed() {
        let dir = scratch("links");
        std::fs::create_dir_all(dir.join("pkg/docs")).unwrap();
        std::fs::write(dir.join("pkg/docs/guide.md"), "guide").unwrap();
        std::fs::write(dir.join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.join("pkg"), dir.join("alias")).unwrap();
        std::os::unix::fs::symlink(dir.join("pkg/docs"), dir.join("pkg/linked")).unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("pkg/docs/out.md")).unwrap();

        let pkg = LinkFreeDir::open(&dir.join("pkg")).unwrap();
        let mut text = String::new();
        use std::io::Read;
        pkg.open_file(Path::new("docs/guide.md"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "guide");

        // A link where a folder is expected is "not a folder" (ENOTDIR), one
        // where the file is expected is a loop (ELOOP): refused either way.
        let refused = |e: io::Error| matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR));
        assert!(
            LinkFreeDir::open(&dir.join("alias")).is_err_and(refused),
            "the folder's own path"
        );
        assert!(
            pkg.open_file(Path::new("linked/guide.md"))
                .is_err_and(refused),
            "a folder below it"
        );
        assert!(
            pkg.open_file(Path::new("docs/out.md")).is_err_and(refused),
            "the file itself"
        );
        assert_eq!(
            pkg.open_file(Path::new("docs/none.md")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for outside in ["../secret", "/etc/hostname", "docs/../guide.md", ""] {
            assert_eq!(
                pkg.open_file(Path::new(outside)).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{outside}"
            );
        }
        assert_eq!(
            LinkFreeDir::open(Path::new("relative/pkg"))
                .err()
                .map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file handed on is taken over only when it is the file expected;
    /// any other number is left open and untouched.
    #[test]
    fn a_handed_file_is_taken_only_when_it_is_the_one_expected() {
        let dir = scratch("handed");
        let lock = dir.join("session.lock");
        let other = dir.join("other");
        let handed = hand_on(std::fs::File::create(&lock).unwrap()).unwrap();
        let flags = unsafe { libc::fcntl(handed.0, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "kept across the exec");

        let stray = std::fs::File::create(&other).unwrap();
        assert!(take_handed_file(Handed(stray.as_raw_fd()), &lock).is_none());
        use std::io::Write;
        assert!((&stray).write_all(b"still open").is_ok(), "never closed");

        let taken = take_handed_file(handed, &lock).expect("the lock itself");
        let flags = unsafe { libc::fcntl(taken.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "closes on exec again");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file only its owner may reach is left alone; one anyone else may
    /// reach is made private — a folder keeps what its owner needs to enter it.
    #[test]
    fn restricting_to_the_owner_tightens_only_what_is_loose() {
        let dir = scratch("private");
        let file = dir.join("key");
        std::fs::write(&file, "k").unwrap();
        set_file_mode(&file, 0o644).unwrap();
        restrict_to_owner(&file).unwrap();
        assert_eq!(file_mode(&std::fs::metadata(&file).unwrap()) & 0o777, 0o600);
        set_file_mode(&file, 0o400).unwrap();
        restrict_to_owner(&file).unwrap();
        assert_eq!(file_mode(&std::fs::metadata(&file).unwrap()) & 0o777, 0o400);
        let folder = dir.join("aside");
        std::fs::create_dir(&folder).unwrap();
        set_file_mode(&folder, 0o755).unwrap();
        restrict_to_owner(&folder).unwrap();
        assert_eq!(
            file_mode(&std::fs::metadata(&folder).unwrap()) & 0o777,
            0o700
        );
        let created = dir.join("created");
        open_private(&created, crate::platform::Private::New).unwrap();
        assert_eq!(
            file_mode(&std::fs::metadata(&created).unwrap()) & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn identities_name_files_not_spellings() {
        let dir = scratch("identity");
        let a = dir.join("a");
        std::fs::write(&a, "x").unwrap();
        std::fs::hard_link(&a, dir.join("b")).unwrap();
        std::os::unix::fs::symlink(&a, dir.join("c")).unwrap();
        assert!(same_file(&a, &dir.join("b")));
        assert!(same_file(&a, &dir.join("c")), "a link is followed");
        assert_ne!(
            FileIdentity::of_entry(&dir.join("c")).unwrap(),
            FileIdentity::of_path(&a).unwrap(),
            "the link's own entry is not its target"
        );
        let open = std::fs::File::open(&a).unwrap();
        assert_eq!(
            FileIdentity::of_file(&open).unwrap(),
            FileIdentity::of_path(&a).unwrap()
        );
        assert!(!same_file(&a, &dir.join("missing")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
