//! Workspace jail — every path a tool touches passes through here first.
//!
//! Two escapes have to be closed, and they are different problems:
//!
//!  * **Lexical** — `../../etc/passwd`. Closed by normalising `.` and `..` in the
//!    path string *before* touching the filesystem, so a path that does not exist
//!    yet is still checked.
//!  * **Symlink** — `link/passwd`, where `link` is a symlink out of the project.
//!    Closed by canonicalising the deepest ancestor that actually exists and
//!    re-checking. A purely lexical check cannot see this.
//!
//! Note the scope of this file honestly: it guards paths **the engine's own tools**
//! open. It does nothing about a shell command the agent runs — that is the
//! kernel sandbox's job (see `sandbox`).

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::memory;

/// A project root plus the rules for staying inside it.
///
/// `confined` is what the session's permission mode turns off: in full-access
/// mode the root still anchors relative paths, but leaving it is allowed. The
/// flag lives here rather than in every caller so there is exactly one place
/// that decides whether a path may escape.
///
/// `annex` is a **second** permitted root: the session's memory directory, which
/// by construction lives outside the project (see [`crate::memory`]). It is a
/// full root, not a special case inside [`Jail::resolve`] — the lexical check,
/// the symlink check and the post-join check all consult it identically, so a
/// `..` cannot ride out of it any more than it can out of the project.
///
/// `reach` is every path the **user** has allowed this session to leave the
/// workspace for (see [`crate::perm::reach`]). It is rebuilt from the session
/// rather than stored, so a grant that lasts one call disappears with it. These
/// go through the identical `permits` check for the same reason the annex does:
/// a special case is a place for the three passes of [`Jail::resolve`] to drift
/// apart.
#[derive(Debug, Clone)]
pub struct Jail {
    root: PathBuf,
    annex: Option<PathBuf>,
    reach: Vec<PathBuf>,
    confined: bool,
}

impl Jail {
    /// `root` must exist and be a directory. It is canonicalised once, here, so
    /// every later comparison is against a symlink-free absolute path.
    pub fn new<P: AsRef<Path>>(root: P) -> Result<Jail> {
        let root = root.as_ref();
        let meta = std::fs::metadata(root)
            .map_err(|_| Error::NotFound(format!("project directory {}", root.display())))?;
        if !meta.is_dir() {
            return Err(Error::invalid(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        Ok(Jail {
            root: crate::platform::canonical(root)?,
            annex: None,
            reach: Vec::new(),
            confined: true,
        })
    }

    /// Add the memory directory as a second permitted root.
    ///
    /// `dir` must already exist and be canonical — [`crate::memory::Memory::ensure`]
    /// returns exactly that. Passing an uncanonicalised path would let a symlink
    /// inside it point anywhere and still satisfy the prefix check.
    pub fn with_annex(mut self, dir: PathBuf) -> Jail {
        self.annex = Some(dir);
        self
    }

    /// The same root, but paths outside it are permitted. Only full-access mode
    /// builds one of these, and the session event says so out loud.
    pub fn unconfined(self) -> Jail {
        Jail {
            confined: false,
            ..self
        }
    }

    /// Permit the paths the user has allowed this session to reach.
    ///
    /// Each must already be absolute and canonical — [`crate::perm::reach::candidate`]
    /// returns exactly that. An uncanonicalised entry would let a symlink inside
    /// it point anywhere and still satisfy the prefix check, which is the same
    /// hole the annex is documented against.
    pub fn with_reach(mut self, paths: Vec<PathBuf>) -> Jail {
        self.reach = paths;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Is `path` inside one of the roots this jail permits?
    ///
    /// An unconfined jail permits everything, which is what full-access mode
    /// means. One function, consulted by every check below, so the three passes
    /// of `resolve` cannot drift apart. `spelt` is how names compare: exactly
    /// for a path the filesystem spelt (canonical), and — where the OS
    /// compares names without case (Windows, macOS) — without case for a path
    /// as the model or person wrote it, which the canonical passes then check
    /// exactly: `C:\PROJ\x` is the project `C:\proj` there, and a sibling
    /// folder differing only in case on a case-sensitive volume is still
    /// refused once its real spelling is read.
    fn permits(&self, path: &Path, spelt: Spelt) -> bool {
        if !self.confined {
            return true;
        }
        let inside = |root: &Path| match spelt {
            Spelt::Canonical => path.starts_with(root),
            Spelt::Written => starts_with_folded(path, root, crate::platform::OS),
        };
        inside(&self.root)
            || self.annex.as_ref().is_some_and(|a| inside(a))
            || self.reach.iter().any(|r| inside(r))
    }

    /// Translate `@memory` / `@memory/...` into the annex. `None` when the input
    /// is not the alias — including `@memoryfoo`, which is an ordinary filename.
    fn dealias(&self, input: &str) -> Option<PathBuf> {
        let annex = self.annex.as_ref()?;
        let rest = input.strip_prefix(memory::ALIAS)?;
        match rest.strip_prefix('/') {
            Some(tail) => Some(annex.join(tail)),
            None if rest.is_empty() => Some(annex.clone()),
            None => None,
        }
    }

    /// Resolve a model- or user-supplied path to an absolute path proven to be
    /// inside the jail. The target need not exist — creating a new file is a
    /// normal case — but every existing component of it is checked for real.
    pub fn resolve(&self, input: &str) -> Result<PathBuf> {
        if input.trim().is_empty() {
            return Err(Error::invalid("empty path"));
        }
        if input.contains('\0') {
            return Err(Error::invalid("path contains a NUL byte"));
        }

        let windows = crate::platform::OS == crate::platform::Os::Windows;
        let home_relative = input
            .strip_prefix("~/")
            .or_else(|| windows.then(|| input.strip_prefix("~\\")).flatten());
        let joined = match self.dealias(input) {
            Some(p) => p,
            None => {
                if input == "~" || home_relative.is_some() {
                    let home = crate::platform::home_dir();
                    match home {
                        Some(h) => match home_relative {
                            Some(rest) => h.join(rest),
                            None => h,
                        },
                        None => PathBuf::from(input),
                    }
                } else {
                    let raw = Path::new(input);
                    if raw.is_absolute() {
                        raw.to_path_buf()
                    } else {
                        self.root.join(raw)
                    }
                }
            }
        };
        let lexical = normalize(&joined);

        // Where Windows reads the path, a name that is a device (`NUL`), a
        // stream of a file (`a:b`) or a name it trims (`.git.`) is not the
        // file it looks like: refused before anything is opened.
        if windows {
            for component in lexical.components() {
                if let Component::Normal(name) = component {
                    if let Some(why) =
                        crate::platform::windows_name_trouble(&name.to_string_lossy())
                    {
                        return Err(Error::invalid(format!("{input}: {why}")));
                    }
                }
            }
        }

        // As written, then as the filesystem spells it; the second decides.
        // A name written through a link to a granted folder (`/etc/hosts` on
        // macOS, where `/etc` is `/private/etc` and the grant names the real
        // folder) is the granted file; a name inside whose link leads out is
        // not.
        let written = self.permits(&lexical, Spelt::Written);

        // Symlink pass: canonicalise the longest existing prefix, then re-check.
        let (existing, rest) = split_existing(&lexical);
        let canon = crate::platform::canonical(&existing)
            .map_err(|e| Error::Jail(format!("{input}: {e}")))?;
        if !self.permits(&canon, Spelt::Canonical) {
            return Err(Error::Jail(if written {
                format!("{input} (symlink leaves the workspace)")
            } else {
                input.to_string()
            }));
        }

        let resolved = if rest.as_os_str().is_empty() {
            canon
        } else {
            canon.join(&rest)
        };
        if !self.permits(&resolved, Spelt::Canonical) {
            return Err(Error::Jail(input.to_string()));
        }
        Ok(resolved)
    }

    /// Display form: path relative to the root, for events and model output.
    /// Never leaks the absolute location of the project.
    ///
    /// Memory paths come back under the alias rather than the real directory —
    /// the same name the model uses to address them, so a checkpoint recorded
    /// from a memory write resolves back to the same file.
    pub fn display(&self, abs: &Path) -> String {
        if let Some(annex) = &self.annex {
            if let Ok(rel) = abs.strip_prefix(annex) {
                let s = slashed(rel);
                return if s.is_empty() {
                    memory::ALIAS.to_string()
                } else {
                    format!("{}/{s}", memory::ALIAS)
                };
            }
        }
        abs.strip_prefix(&self.root)
            .map(|p| {
                let s = slashed(p);
                if s.is_empty() {
                    ".".to_string()
                } else {
                    s
                }
            })
            .unwrap_or_else(|_| abs.to_string_lossy().to_string())
    }

    /// True when `path` is inside this jail. Used by the checkpoint engine, which
    /// already holds absolute paths and must never snapshot somewhere the jail
    /// would not have let a tool write in the first place.
    pub fn contains(&self, path: &Path) -> bool {
        self.permits(&normalize(path), Spelt::Canonical)
    }
}

/// How a path being checked was spelt ([`Jail::permits`]).
#[derive(Clone, Copy)]
enum Spelt {
    /// By the filesystem: every existing name as it is on disk.
    Canonical,
    /// As written by the model or the person.
    Written,
}

/// A path inside the workspace as Rune shows it on every OS: its names
/// joined by `/`, as the file walk names them (`scan::walk`) — never
/// `sub\file.txt` beside `sub/file.txt`, nor `@memory/a\b`.
fn slashed(relative: &Path) -> String {
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `path` lies under `root`, names compared without case where the
/// OS compares them so (Windows, macOS) and exactly elsewhere.
pub(crate) fn starts_with_folded(path: &Path, root: &Path, os: crate::platform::Os) -> bool {
    use crate::platform::Os;
    if !matches!(os, Os::Windows | Os::MacOs) {
        return path.starts_with(root);
    }
    let mut names = path.components();
    root.components().all(|wanted| {
        names.next().is_some_and(|name| {
            name == wanted
                || name.as_os_str().to_string_lossy().to_lowercase()
                    == wanted.as_os_str().to_string_lossy().to_lowercase()
        })
    })
}

/// Remove `.` and `..` without touching the filesystem. `..` past the root is
/// clamped rather than escaping.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    // Already at the root of the path; stay there.
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

/// `path` spelt as the filesystem names it — every link resolved — even
/// before it exists: its longest existing ancestor canonicalised, the rest
/// joined on. `path` itself when not even an ancestor can be read.
///
/// A folder Rune keeps things in is named this way once, where it is chosen
/// (`store::Db::home_dir`), so every path derived from it compares equal to
/// what the jail and the kernel resolve — a home reached through a link
/// (`/home` → `/var/home` on Fedora Atomic) is no second spelling.
pub(crate) fn canonical_spelling(path: &Path) -> PathBuf {
    if let Ok(canonical) = crate::platform::canonical(path) {
        return canonical;
    }
    let (existing, rest) = split_existing(path);
    match crate::platform::canonical(&existing) {
        Ok(canonical) if rest.as_os_str().is_empty() => canonical,
        Ok(canonical) => canonical.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// Split `path` into (longest existing ancestor, remainder).
fn split_existing(path: &Path) -> (PathBuf, PathBuf) {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            let mut tail = PathBuf::new();
            for seg in rest.iter().rev() {
                tail.push(seg);
            }
            return (existing, tail);
        }
        match existing.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                if !existing.pop() {
                    return (PathBuf::from("/"), PathBuf::new());
                }
            }
            None => return (existing, PathBuf::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A written path is inside a root whatever the case of its names where
    /// the OS compares them without case, and only exactly elsewhere.
    #[test]
    fn written_names_compare_as_the_os_compares_them() {
        use crate::platform::Os;
        let root = Path::new("/work/Proj");
        for (os, folded) in [(Os::Linux, false), (Os::MacOs, true), (Os::Windows, true)] {
            assert!(starts_with_folded(Path::new("/work/Proj/a"), root, os));
            assert_eq!(
                starts_with_folded(Path::new("/WORK/proj/a"), root, os),
                folded,
                "{os:?}"
            );
            assert!(!starts_with_folded(Path::new("/work/Project/a"), root, os));
            assert!(!starts_with_folded(Path::new("/work"), root, os));
        }
    }

    /// Where Windows reads the path, a device, a stream or a trimmed name is
    /// refused before anything is opened; elsewhere they are plain names.
    #[test]
    fn a_name_windows_reads_as_something_else_is_refused_there() {
        let dir = scratch("windows-names");
        let j = Jail::new(&dir).unwrap();
        let windows = crate::platform::OS == crate::platform::Os::Windows;
        for name in [
            "NUL",
            "sub/con.txt",
            "sub/file.txt:hidden",
            ".git./config",
            "com1",
        ] {
            assert_eq!(j.resolve(name).is_err(), windows, "{name}");
        }
        assert!(j.resolve("sub/console.txt").is_ok());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("rune-test").join(format!(
            "runejail-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/file.txt"), b"hello").unwrap();
        dir
    }

    /// A grant names the real folder; the file written through a link to it
    /// is the granted file (CI macOS, 2026-10-18: `/etc/hosts` was refused
    /// after the person allowed it, `/etc` being `/private/etc`) — while a
    /// link that leads out of every granted folder is refused as before.
    #[test]
    fn a_granted_folder_reached_through_a_link_is_granted() {
        let dir = scratch("granted-link");
        let project = dir.join("project");
        let real = dir.join("real");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("hosts"), "x").unwrap();
        if crate::platform::fixture::symlink(&real, dir.join("etc")).is_err() {
            eprintln!("note: no links here");
            return;
        }
        let real = crate::platform::canonical(&real).unwrap();
        let j = Jail::new(&project).unwrap().with_reach(vec![real.clone()]);
        let written = dir.join("etc").join("hosts");
        assert_eq!(
            j.resolve(written.to_str().unwrap()).unwrap(),
            real.join("hosts")
        );
        let narrow = Jail::new(&project).unwrap();
        assert!(narrow.resolve(written.to_str().unwrap()).is_err());
    }

    #[test]
    fn resolves_relative_inside() {
        let dir = scratch("rel");
        let j = Jail::new(&dir).unwrap();
        let p = j.resolve("sub/file.txt").unwrap();
        assert!(p.ends_with("sub/file.txt"));
        assert_eq!(j.display(&p), "sub/file.txt");
    }

    #[test]
    fn allows_paths_that_do_not_exist_yet() {
        let dir = scratch("new");
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve("sub/deeper/new.txt").is_ok());
    }

    #[test]
    fn rejects_dotdot_escape() {
        let dir = scratch("dotdot");
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve("../../etc/passwd").is_err());
        assert!(j.resolve("sub/../../outside.txt").is_err());
    }

    #[test]
    fn rejects_absolute_outside() {
        let dir = scratch("abs");
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve("/etc/passwd").is_err());
    }

    #[test]
    fn accepts_absolute_inside() {
        let dir = scratch("absin");
        let j = Jail::new(&dir).unwrap();
        let inside = j.root().join("sub/file.txt");
        assert!(j.resolve(inside.to_str().unwrap()).is_ok());
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = scratch("symlink");
        // A real folder outside the root, on every OS (`/etc` names none on
        // Windows, where a link to it would lead nowhere).
        let outside = scratch("symlink-outside");
        std::fs::write(outside.join("passwd"), "root\n").unwrap();
        crate::platform::fixture::symlink(&outside, dir.join("escape")).unwrap();
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve("escape/passwd").is_err());
        assert!(j.resolve("escape").is_err());
    }

    #[test]
    fn an_unconfined_jail_still_anchors_relative_paths_but_permits_escape() {
        let dir = scratch("unconfined");
        let j = Jail::new(&dir).unwrap().unconfined();
        // Relative paths still resolve against the project.
        assert!(j.resolve("sub/file.txt").unwrap().starts_with(j.root()));
        // And leaving is allowed — this is what full-access mode means.
        assert!(j.resolve("/etc/hostname").is_ok());
        assert!(j.resolve("../../etc").is_ok());
        assert!(j.contains(std::path::Path::new("/etc")));
        if let Some(home) = crate::platform::home_dir() {
            assert_eq!(j.resolve("~/somefile").unwrap(), home.join("somefile"));
        }
    }

    #[test]
    fn rejects_empty_and_nul() {
        let dir = scratch("bad");
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve("").is_err());
        assert!(j.resolve("   ").is_err());
        assert!(j.resolve("a\0b").is_err());
    }

    /// A jail with a memory annex, plus the annex's *sibling* — the directory a
    /// different project's memory would occupy.
    fn annexed(name: &str) -> (Jail, PathBuf, PathBuf) {
        let dir = scratch(name);
        let store = dir
            .parent()
            .unwrap()
            .join(format!("store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let mine = store.join("memory/aaaa");
        let theirs = store.join("memory/bbbb");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(store.join("rune.db"), b"pretend").unwrap();
        let annex = crate::platform::canonical(&mine).unwrap();
        (
            Jail::new(&dir).unwrap().with_annex(annex.clone()),
            annex,
            store,
        )
    }

    #[test]
    fn the_alias_resolves_into_the_annex() {
        let (j, annex, _) = annexed("alias");
        assert_eq!(j.resolve("@memory").unwrap(), annex);
        assert_eq!(
            j.resolve("@memory/MEMORY.md").unwrap(),
            annex.join("MEMORY.md")
        );
        assert_eq!(
            j.resolve("@memory/notes/build.md").unwrap(),
            annex.join("notes/build.md")
        );
    }

    #[test]
    fn a_name_that_merely_starts_with_the_alias_is_an_ordinary_file() {
        let (j, _, _) = annexed("aliasish");
        let p = j.resolve("@memoryfoo").unwrap();
        assert!(
            p.starts_with(j.root()),
            "@memoryfoo must stay in the project: {p:?}"
        );
    }

    #[test]
    fn dotdot_cannot_ride_out_of_the_annex() {
        let (j, _, _) = annexed("annexdotdot");
        assert!(j.resolve("@memory/../bbbb/MEMORY.md").is_err());
        assert!(j.resolve("@memory/../../rune.db").is_err());
        assert!(j.resolve("@memory/../..").is_err());
    }

    #[test]
    fn another_projects_memory_and_the_engine_store_stay_unreachable() {
        let (j, _, store) = annexed("annexsibling");
        assert!(j
            .resolve(store.join("memory/bbbb").to_str().unwrap())
            .is_err());
        assert!(j.resolve(store.join("rune.db").to_str().unwrap()).is_err());
        assert!(j.resolve(store.to_str().unwrap()).is_err());
    }

    #[test]
    fn a_symlink_out_of_the_annex_is_refused() {
        let (j, annex, store) = annexed("annexsymlink");
        crate::platform::fixture::symlink(&store, annex.join("out")).unwrap();
        assert!(j.resolve("@memory/out/rune.db").is_err());
        assert!(j.resolve("@memory/out").is_err());
    }

    #[test]
    fn a_memory_path_displays_under_the_alias_and_round_trips() {
        let (j, annex, _) = annexed("annexdisplay");
        let abs = j.resolve("@memory/MEMORY.md").unwrap();
        assert_eq!(j.display(&abs), "@memory/MEMORY.md");
        // display → resolve is the checkpoint round trip: capture stores the
        // display form, restore resolves it back.
        assert_eq!(j.resolve(&j.display(&abs)).unwrap(), abs);
        assert_eq!(j.display(&annex), "@memory");
    }

    #[test]
    fn the_checkpoint_engine_may_snapshot_memory_but_nothing_beside_it() {
        let (j, annex, store) = annexed("annexcontains");
        assert!(j.contains(&annex.join("MEMORY.md")));
        assert!(j.contains(&j.root().join("src/main.rs")));
        assert!(!j.contains(&store.join("rune.db")));
        assert!(!j.contains(&store.join("memory/bbbb/MEMORY.md")));
    }

    #[test]
    fn without_an_annex_the_alias_is_just_a_filename() {
        let dir = scratch("noannex");
        let j = Jail::new(&dir).unwrap();
        assert!(j
            .resolve("@memory/MEMORY.md")
            .unwrap()
            .starts_with(j.root()));
    }

    #[test]
    fn sibling_directory_is_not_inside() {
        let dir = scratch("sibling");
        let sibling = dir.parent().unwrap().join(format!(
            "{}-evil",
            dir.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        let j = Jail::new(&dir).unwrap();
        assert!(j.resolve(sibling.to_str().unwrap()).is_err());
    }
}
