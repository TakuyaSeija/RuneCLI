//! Where a project's data lives under the engine's home.
//!
//! # One folder per project, named so a person can find it
//!
//! Everything Rune keeps about a project — its memory, its undo blobs, its
//! sessions and their scratch space — lives in one folder,
//! `projects/<name>/`. Deleting a project's data is deleting that folder.
//!
//! The name is the project's path with every character that is not a letter or
//! a digit turned into `-`, the rule Claude Code uses:
//! `/home/me/Project/App` is `-home-me-Project-App`. That rule is lossy —
//! `/a/web-app` and `/a/web/app` both become `-a-web-app` — so the name is
//! only ever a label. The identity is the path recorded inside the folder
//! (`project.json`), and when a second project's name is already taken by the
//! first, or a path is too long to name a folder on every filesystem, the
//! second one's name carries a short digest of its path: `-a-web-app-1f3c9a2b`.
//! Two projects therefore never share a folder, and almost every folder still
//! reads as the path it belongs to.
//!
//! # The internal key
//!
//! [`project_key`] — a digest of the canonical path — still names a project
//! where no person reads it: the draft registry, the generation file a
//! frontend watches, the vault entries that hold an MCP server's login. It is
//! an identity, not a place.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// A digest of the canonical path, for the places that need a stable identity
/// and no person reads.
pub fn project_key(root: &Path) -> String {
    crate::checkpoint::objects::hash_of(root.as_os_str().as_encoded_bytes())[..16].to_owned()
}

/// The path a project is known by: its canonical form when it exists, so a
/// symlink and its target are one project, and its normalised form when it
/// does not (a session can outlive its project directory).
pub fn canonical(project: &Path) -> PathBuf {
    crate::platform::canonical(project).unwrap_or_else(|_| crate::jail::normalize(project))
}

/// Longest name a folder is given from its path. Well under every filesystem's
/// limit for one name, and short enough that a session's deepest file stays a
/// path every operating system opens.
const NAME_CHARS: usize = 120;

/// The name a project's folder is given from its path when nothing else
/// claims it: every character that is not an ASCII letter or digit turned into
/// `-`. A path too long for that is cut from the front — the end is the part
/// that tells projects apart — and marked with its digest, because a cut name is
/// no longer the path's own.
pub fn folder_name(project: &Path) -> String {
    let slug: String = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if slug.len() <= NAME_CHARS {
        return slug;
    }
    let tail = &slug[slug.len() - (NAME_CHARS - 9)..];
    format!("{tail}-{}", &project_key(project)[..8])
}

/// The name a project takes when [`folder_name`] already belongs to another
/// project.
fn disambiguated(project: &Path) -> String {
    let name = folder_name(project);
    if name.len() > NAME_CHARS - 9 && name.ends_with(&project_key(project)[..8]) {
        return name;
    }
    format!("{name}-{}", &project_key(project)[..8])
}

/// What a project's folder records about itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct Identity {
    project: String,
}

const IDENTITY: &str = "project.json";

/// One project folder: where it is, and the project path it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub project: PathBuf,
}

/// The path recorded in `folder`, when it holds a readable one.
fn recorded(folder: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(folder.join(IDENTITY)).ok()?;
    let identity: Identity = serde_json::from_slice(&bytes).ok()?;
    Some(PathBuf::from(identity.project))
}

/// The folder that holds `project`, if it has one yet.
pub fn find(home: &Path, project: &Path) -> Option<PathBuf> {
    let project = canonical(project);
    let root = home.join("projects");
    [folder_name(&project), disambiguated(&project)]
        .into_iter()
        .map(|name| root.join(name))
        .find(|folder| recorded(folder).as_deref() == Some(project.as_path()))
}

/// The folder that holds `project`, created with its identity when the project
/// has none yet. Creation is serialised across processes, so two sessions
/// opening one new project at once share one folder.
pub fn open(home: &Path, project: &Path) -> Result<PathBuf> {
    if let Some(folder) = find(home, project) {
        return Ok(folder);
    }
    let project = canonical(project);
    let _lock = crate::lock::exclusive(&home.join("run/Projects.lock"), crate::lock::Wait::Yes)?;
    if let Some(folder) = find(home, &project) {
        return Ok(folder);
    }
    let root = home.join("projects");
    let preferred = root.join(folder_name(&project));
    let folder = if preferred.exists() {
        root.join(disambiguated(&project))
    } else {
        preferred
    };
    if folder.exists() {
        return Err(Error::invalid(format!(
            "Project folder {} belongs to another project; its data was left untouched.",
            folder.display()
        )));
    }
    std::fs::create_dir_all(&folder)?;
    crate::settings::secrets::atomic_write(
        &folder.join(IDENTITY),
        &serde_json::to_vec_pretty(&Identity {
            project: project.to_string_lossy().into_owned(),
        })?,
    )?;
    Ok(folder)
}

/// The project folder called `name` under `home`: a real directory directly
/// inside `projects/` that records its project. A name is never a path, so
/// nothing outside `projects/` can be reached through one.
pub fn named(home: &Path, name: &str) -> Option<Folder> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return None;
    }
    let path = home.join("projects").join(name);
    if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let project = recorded(&path)?;
    Some(Folder { path, project })
}

/// Remove a project's folder, its identity last: a removal cut short still
/// leaves a folder that names its project, so it is found and finished later
/// instead of lingering as a folder nobody owns.
pub fn remove(folder: &Path) -> Result<()> {
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        if entry.file_name() == IDENTITY {
            continue;
        }
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
    }
    match std::fs::remove_file(folder.join(IDENTITY)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    std::fs::remove_dir(folder)?;
    Ok(())
}

/// Every project folder under `home`, with the path it holds. A folder with no
/// readable identity is not a project's and is left out.
pub fn all(home: &Path) -> Vec<Folder> {
    let Ok(entries) = std::fs::read_dir(home.join("projects")) else {
        return Vec::new();
    };
    let mut folders: Vec<Folder> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let path = entry.path();
            recorded(&path).map(|project| Folder { path, project })
        })
        .collect();
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("projects-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        dir
    }

    #[test]
    fn a_folder_is_named_after_its_path() {
        assert_eq!(
            folder_name(Path::new("/home/me/Project/App")),
            "-home-me-Project-App"
        );
        assert_eq!(folder_name(Path::new("/a/my_app.v2")), "-a-my-app-v2");
    }

    /// The owner's case: two paths that differ only in a separator name the
    /// same folder; the first keeps the name and the second is told apart by
    /// its digest — never sharing memory, undo or logins.
    #[test]
    fn two_projects_that_name_one_folder_never_share_it() {
        let home = home("collide");
        let work = home.join("work");
        let flat = work.join("web-app");
        let nested = work.join("web/app");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            folder_name(&canonical(&flat)),
            folder_name(&canonical(&nested))
        );

        let first = open(&home, &flat).unwrap();
        let second = open(&home, &nested).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            first.file_name().unwrap().to_string_lossy(),
            folder_name(&canonical(&flat))
        );
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&project_key(&canonical(&nested))[..8]));
        assert_eq!(
            open(&home, &flat).unwrap(),
            first,
            "found again, not created"
        );
        assert_eq!(open(&home, &nested).unwrap(), second);
        assert_eq!(all(&home).len(), 2);
    }

    #[test]
    fn a_symlink_and_its_target_are_one_project() {
        let home = home("alias");
        let real = home.join("work/real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = home.join("work/alias");
        crate::platform::fixture::symlink(&real, &alias).unwrap();
        assert_eq!(open(&home, &real).unwrap(), open(&home, &alias).unwrap());
    }

    /// A path too long to name a folder everywhere keeps its end — the part
    /// that tells projects apart — and its digest.
    #[test]
    fn a_long_path_keeps_its_end_and_is_marked() {
        let long = PathBuf::from(format!("/{}/end-of-path", "deep/".repeat(60)));
        let name = folder_name(&long);
        assert!(name.len() <= NAME_CHARS, "{}", name.len());
        assert!(name.contains("end-of-path"), "{name}");
        assert!(name.ends_with(&project_key(&long)[..8]));
    }

    /// Only a real folder directly under `projects/` that records a project
    /// answers to a name; a removal takes everything and the identity last.
    #[test]
    fn a_folder_is_named_only_from_inside_projects_and_removed_whole() {
        let home = home("named");
        let project = home.join("work/p");
        std::fs::create_dir_all(&project).unwrap();
        let folder = open(&home, &project).unwrap();
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(named(&home, &name).unwrap().path, folder);
        for bad in ["", ".", "..", "../run", "a/b", "nope"] {
            assert!(named(&home, bad).is_none(), "{bad:?}");
        }
        let elsewhere = home.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(IDENTITY), br#"{"project":"/x"}"#).unwrap();
        crate::platform::fixture::symlink(&elsewhere, home.join("projects/linked")).unwrap();
        assert!(
            named(&home, "linked").is_none(),
            "a symlink is not a folder"
        );

        std::fs::create_dir_all(folder.join("sessions/1")).unwrap();
        std::fs::write(folder.join("sessions/1/session.jsonl"), b"{}\n").unwrap();
        std::fs::create_dir_all(folder.join("memory")).unwrap();
        std::fs::write(folder.join("memory.lock"), b"").unwrap();
        remove(&folder).unwrap();
        assert!(!folder.exists());
        assert!(
            elsewhere.join(IDENTITY).exists(),
            "nothing outside was touched"
        );
    }

    /// A folder whose identity names another path is never adopted.
    #[test]
    fn a_folder_holding_another_path_is_left_alone() {
        let home = home("foreign");
        let project = home.join("work/p");
        std::fs::create_dir_all(&project).unwrap();
        let name = folder_name(&canonical(&project));
        let squatter = home.join("projects").join(&name);
        std::fs::create_dir_all(&squatter).unwrap();
        std::fs::write(squatter.join(IDENTITY), br#"{"project":"/elsewhere"}"#).unwrap();
        let folder = open(&home, &project).unwrap();
        assert_ne!(folder, squatter);
        assert_eq!(
            std::fs::read(squatter.join(IDENTITY)).unwrap(),
            br#"{"project":"/elsewhere"}"#
        );
    }
}
