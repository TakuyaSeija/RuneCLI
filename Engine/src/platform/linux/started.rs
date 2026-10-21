//! How this process's program was started: by its own path, or by naming it
//! to a dynamic loader.
//!
//! A program built for a newer C library than a system has is run there by
//! starting a newer library's loader on it — `ld-linux-x86-64.so.2
//! --library-path <dir> ./rune` (a SeaDrive container on Debian 12,
//! 2026-10-20). The kernel then runs the loader: `/proc/self/exe` names the
//! loader, and every place that starts Rune again (a session host, a newer
//! build, the upgrade check, the way back) started the loader with Rune's
//! arguments, which it took for a program to load — exit 127 and a session
//! that never opened.
//!
//! So the program is found by what the kernel mapped this very code from
//! (`/proc/self/maps`, the mapping holding [`remember`]), never by the
//! loader's name, and the loader's own options are read from the command
//! line as it was typed (`/proc/self/cmdline` keeps the loader's part; the
//! program's `argv` starts after it). Rune is started again the same way:
//! the same loader, the same options, then the program (`program_launch`).
//! A program started by its own path is left exactly as before.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::platform::{FileIdentity, Launch};

/// A program started through a loader.
pub(super) struct Through {
    /// The loader the kernel ran.
    pub(super) loader: PathBuf,
    /// What was given to the loader before the program's path — its own
    /// options (`--library-path <dir>`), kept as typed.
    pub(super) lead: Vec<OsString>,
    /// The program's path — where a release puts the next build.
    pub(super) program: PathBuf,
    /// The file the kernel mapped this code from.
    pub(super) identity: FileIdentity,
    /// That file, opened as this process started while it was still the one
    /// at its path: the way back to this build ([`super::procfs::own_program`]).
    /// `None` when a release had already replaced it.
    pub(super) file: Option<File>,
}

static STARTED: OnceLock<Option<Through>> = OnceLock::new();

/// Read how this process was started, as it starts (`platform::at_start`):
/// before a release can put another file at the program's path.
pub(super) fn remember() {
    let _ = through();
}

/// The loader this process was started through, if it was.
pub(super) fn through() -> Option<&'static Through> {
    STARTED.get_or_init(detect).as_ref()
}

/// How to start the Rune program at `program` the way this one was started.
pub fn program_launch(program: &Path) -> Launch {
    match through() {
        Some(through) => Launch::new(&through.loader)
            .args(&through.lead)
            .arg(program),
        None => Launch::new(program),
    }
}

fn detect() -> Option<Through> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let mapped = mapping_of(&maps, remember as *const () as usize)?;
    let identity = FileIdentity::of_device(libc::makedev(mapped.major, mapped.minor), mapped.inode);
    // Started by its own path: the kernel's program is this code's file.
    if FileIdentity::of_path(Path::new("/proc/self/exe")).ok()? == identity {
        return None;
    }
    let loader = PathBuf::from(
        std::fs::read_link("/proc/self/exe")
            .ok()?
            .as_os_str()
            .to_string_lossy()
            .trim_end_matches(" (deleted)"),
    );
    let cmdline = std::fs::read("/proc/self/cmdline").ok()?;
    let argv: Vec<OsString> = cmdline
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| OsStr::from_bytes(arg).to_owned())
        .collect();
    let at = program_index(&argv, |arg| {
        FileIdentity::of_path(Path::new(arg)).is_ok_and(|file| file == identity)
    })?;
    let file = File::open(&mapped.path)
        .ok()
        .filter(|file| FileIdentity::of_file(file).is_ok_and(|file| file == identity));
    Some(Through {
        loader,
        lead: argv[1..at].to_vec(),
        program: mapped.path,
        identity,
        file,
    })
}

/// A file mapping of `/proc/self/maps`.
#[derive(Debug, PartialEq)]
struct Mapped {
    major: u32,
    minor: u32,
    inode: u64,
    path: PathBuf,
}

/// The file mapped where `address` is: `start-end perms offset major:minor
/// inode path`, numbers in hex but the inode. A file replaced since is named
/// with " (deleted)" after it, which is no part of the path.
fn mapping_of(maps: &str, address: usize) -> Option<Mapped> {
    maps.lines().find_map(|line| {
        let mut fields = line.splitn(6, ' ');
        let (start, end) = fields.next()?.split_once('-')?;
        let (start, end) = (
            usize::from_str_radix(start, 16).ok()?,
            usize::from_str_radix(end, 16).ok()?,
        );
        if !(start..end).contains(&address) {
            return None;
        }
        let _perms = fields.next()?;
        let _offset = fields.next()?;
        let (major, minor) = fields.next()?.split_once(':')?;
        let inode: u64 = fields.next()?.parse().ok()?;
        let path = fields.next()?.trim_start();
        let (major, minor) = (
            u32::from_str_radix(major, 16).ok()?,
            u32::from_str_radix(minor, 16).ok()?,
        );
        (inode != 0 && path.starts_with('/')).then(|| Mapped {
            major,
            minor,
            inode,
            path: PathBuf::from(path.trim_end_matches(" (deleted)")),
        })
    })
}

/// Where in the loader's command line the program is: the first argument
/// after the loader's own name that names the program's file. The loader's
/// options before it are whatever that loader takes — never parsed, so a
/// loader with options of its own (glibc's `--argv0`, musl's `--preload`) is
/// started again as it was.
fn program_index(argv: &[OsString], is_program: impl Fn(&OsStr) -> bool) -> Option<usize> {
    (1..argv.len()).find(|&at| is_program(&argv[at]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
5581e2a00000-5581e2a10000 r--p 00000000 08:02 1048 /home/user/rune
5581e2a10000-5581e2b20000 r-xp 00010000 08:02 1048 /home/user/rune (deleted)
7f0000000000-7f0000001000 rw-p 00000000 00:00 0
7f1000000000-7f1000020000 r-xp 00001000 fd:01 77 /home/user/.local/glibc-2.39/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2
7ffc00000000-7ffc00021000 rw-p 00000000 00:00 0                          [stack]
";

    #[test]
    fn the_file_mapped_at_an_address_is_read_without_its_deleted_mark() {
        assert_eq!(
            mapping_of(MAPS, 0x5581e2a10010),
            Some(Mapped {
                major: 8,
                minor: 2,
                inode: 1048,
                path: PathBuf::from("/home/user/rune"),
            })
        );
        assert_eq!(
            mapping_of(MAPS, 0x7f1000000100).map(|m| (m.major, m.minor, m.inode)),
            Some((0xfd, 1, 77))
        );
        assert_eq!(mapping_of(MAPS, 0x7f0000000010), None, "anonymous memory");
        assert_eq!(mapping_of(MAPS, 0x7ffc00000010), None, "the stack");
        assert_eq!(mapping_of(MAPS, 0x1000), None, "nothing mapped");
    }

    /// The loader's options are kept as typed, whatever they are; the
    /// program is the first argument naming its file.
    #[test]
    fn the_program_is_found_after_whatever_options_its_loader_took() {
        let argv = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        let is_rune = |arg: &OsStr| arg == "./rune";
        let typed = argv(&[
            "/g/ld.so",
            "--library-path",
            "/g/lib",
            "./rune",
            "-resume",
            "4",
        ]);
        assert_eq!(program_index(&typed, is_rune), Some(3));
        assert_eq!(
            program_index(&argv(&["/g/ld.so", "./rune"]), is_rune),
            Some(1)
        );
        let argv0 = argv(&["/g/ld.so", "--argv0", "rune", "--inhibit-cache", "./rune"]);
        assert_eq!(program_index(&argv0, is_rune), Some(4));
        assert_eq!(
            program_index(&argv(&["./rune"]), is_rune),
            None,
            "the loader is first"
        );
        assert_eq!(program_index(&argv(&["/g/ld.so", "--list"]), is_rune), None);
    }

    /// A test binary is started by its own path: nothing changes for it.
    #[test]
    fn a_program_started_by_its_own_path_is_started_again_by_it() {
        assert!(through().is_none());
        let launch = program_launch(Path::new("/opt/rune"));
        assert_eq!(launch.program, PathBuf::from("/opt/rune"));
        assert!(launch.args.is_empty());
    }
}
