//! Portable skill metadata, package integrity and approved activation content.
use super::config::Settings;
use crate::checkpoint::objects::hash_of;
use crate::scan::walk::{self, Options};
use crate::settings::configuration::read_bounded;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    pub scope: String,
    pub root: PathBuf,
    pub revision: String,
    pub instruction_revision: String,
    pub disabled: bool,
    pub shadowed: bool,
}

#[derive(Deserialize)]
pub(super) struct Metadata {
    name: String,
    description: String,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

pub fn discover(
    settings: &Settings,
    home: &Path,
    project: &Path,
) -> Result<(Vec<Skill>, Vec<Value>)> {
    let mut roots: Vec<(PathBuf, &str, &str)> = vec![
        (project.join(".rune/skills"), "rune", "project"),
        (project.join(".agents/skills"), "portable", "project"),
    ];
    if let Some(p) = settings.project(project) {
        roots.extend(
            p.skill_roots
                .iter()
                .cloned()
                .map(|p| (p, "configured", "project")),
        );
    }
    if settings.external_skills_compatibility {
        roots.push((
            project.join(".agents/external_skills"),
            "external",
            "project",
        ));
    }
    // An isolated Rune home must not discover the real user's skills by accident.
    let actual_home = crate::platform::home_dir();
    if let Some(user) = actual_home.as_ref().filter(|user| {
        settings
            .portable_user_skills
            .unwrap_or_else(|| home == user.join(".rune"))
    }) {
        roots.push((user.join(".agents/skills"), "portable", "user"));
        if settings.external_skills_compatibility {
            roots.push((user.join(".agents/external_skills"), "external", "user"));
        }
    }
    roots.push((home.join("skills"), "rune", "user"));
    roots.extend(
        settings
            .skill_roots
            .iter()
            .cloned()
            .map(|p| (p, "configured", "user")),
    );
    roots.extend(
        settings
            .product_skill_roots
            .iter()
            .cloned()
            .map(|p| (p, "rune-product", "product")),
    );
    let mut found = Vec::new();
    let mut diagnostics = Vec::new();
    // No process-global or real-user home fallback: an isolated install gets
    // its own compiled guide, and a no-home configuration creates nothing.
    // Help that cannot be provided now (a full disk) is said, never fatal:
    // every other skill and the turn itself go on.
    let builtin = if home.as_os_str().is_empty() {
        None
    } else {
        match super::product::ensure(home) {
            Ok(root) => {
                roots.push((root.clone(), "rune-product", "product"));
                Some(root.join(super::product::NAME))
            }
            Err(error) => {
                diagnostics.push(json!({"source":"rune-product","scope":"product","error":format!("Rune's official help is unavailable now: {error}")}));
                None
            }
        }
    };
    let mut paths = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (root, source, scope) in roots {
        if found.len() >= settings.max_entries {
            diagnostics.push(json!({"error":"Skill catalog limit reached"}));
            break;
        }
        let root = match crate::platform::canonical(&root) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                diagnostics.push(
                    json!({"source":source,"scope":scope,"error":"Skill root is unavailable"}),
                );
                continue;
            }
        };
        let opts = Options {
            max_depth: 4,
            max_entries: settings.max_entries,
            respect_ignore: false,
            hidden: false,
            ..Options::default()
        };
        let stats = walk::walk(&root, &opts, |entry| {
            if entry.path.file_name().is_none_or(|n| n != "SKILL.md") {
                return true;
            }
            if found.len() >= settings.max_entries {
                return false;
            }
            let package = entry
                .path
                .parent()
                .expect("walked file has a parent")
                .to_path_buf();
            if !paths.insert(package.clone()) {
                return true;
            }
            match metadata(&entry.path, settings.max_skill_bytes, true) {
                Ok((meta, instruction_revision)) => {
                    let revision = match package_revision(&package, settings) {
                        Ok(v) => v,
                        Err(_) => {
                            diagnostics.push(json!({"path":entry.path,"error":"Skill package exceeds bounds or contains unsafe files"}));
                            return true;
                        }
                    };
                    let identity = format!(
                        "{}:{}:{}",
                        scope,
                        source,
                        hash_of(package.to_string_lossy().as_bytes())
                    );
                    let id = if builtin.as_ref() == Some(&package) {
                        super::product::ID.to_string()
                    } else {
                        format!("{}:{}", meta.name, &hash_of(identity.as_bytes())[..16])
                    };
                    let disabled = settings
                        .project(project)
                        .is_some_and(|p| p.disabled_skills.contains(&id));
                    let shadowed = !disabled && !names.insert(meta.name.clone());
                    found.push(Skill {
                        id,
                        name: meta.name,
                        description: meta.description,
                        source: source.into(),
                        scope: scope.into(),
                        root: package,
                        revision,
                        instruction_revision,
                        disabled,
                        shadowed,
                    });
                }
                Err(_) => diagnostics
                    .push(json!({"path":entry.path,"error":"Invalid or oversized SKILL.md"})),
            }
            true
        })?;
        if stats.truncated {
            diagnostics
                .push(json!({"root":root,"error":"Skill scan reached its configured bound"}));
        }
    }
    Ok((found, diagnostics))
}

pub(super) fn metadata(
    path: &Path,
    max: usize,
    match_directory: bool,
) -> Result<(Metadata, String)> {
    let bytes = read_bounded(path, max as u64)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::invalid("SKILL.md must be UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().is_none_or(|s| s.trim_end() != "---") {
        return Err(Error::invalid("SKILL.md requires YAML frontmatter"));
    }
    let mut front = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        front.push_str(line);
    }
    if !closed || lines.collect::<String>().trim().is_empty() {
        return Err(Error::invalid("Skill instructions are missing"));
    }
    let opts = serde_saphyr::options! {budget:serde_saphyr::budget!{max_events:4096,max_aliases:0,max_anchors:0}};
    let meta: Metadata = serde_saphyr::from_str_with_options(&front, opts)
        .map_err(|_| Error::invalid("Invalid skill metadata"))?;
    if meta.name.is_empty()
        || meta.name.len() > 64
        || meta.name.starts_with('-')
        || meta.name.ends_with('-')
        || meta.name.contains("--")
        || !meta
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || (match_directory
            && path
                .parent()
                .and_then(Path::file_name)
                .is_none_or(|n| n != meta.name.as_str()))
    {
        return Err(Error::invalid(
            "Skill name must match its directory and Agent Skills naming rules",
        ));
    }
    if meta.description.trim().is_empty()
        || meta.description.chars().count() > 1024
        || meta
            .description
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        || meta
            .compatibility
            .as_ref()
            .is_some_and(|v| v.chars().count() > 500)
        || meta.metadata.len() > 64
    {
        return Err(Error::invalid("Invalid skill description or metadata"));
    }
    Ok((meta, hash_of(&bytes)))
}

impl Skill {
    pub fn entry(&self) -> Value {
        json!({"id":self.id,"kind":"skill","name":self.name,"description":self.description,"source":self.source,"scope":self.scope,"revision":self.revision,"path":self.root.join("SKILL.md"),"package_root":self.root,"disabled":self.disabled,"shadowed":self.shadowed})
    }

    pub fn instructions(&self, settings: &Settings) -> Result<String> {
        if self.disabled {
            return Err(Error::Denied("Skill is disabled".into()));
        }
        // Official help is the build's own text, whatever its copy on disk
        // holds now (a changed copy is replaced at the next discovery).
        if self.id == super::product::ID {
            return Ok(super::product::document("SKILL.md")
                .expect("bundled entrypoint")
                .to_string());
        }
        let path = self.root.join("SKILL.md");
        let (_, revision) = metadata(&path, settings.max_skill_bytes, true)?;
        if revision != self.instruction_revision
            || package_revision(&self.root, settings)? != self.revision
        {
            return Err(Error::Denied(
                "Skill changed; reload to use the new revision".into(),
            ));
        }
        let bytes = read_package_file(&self.root, &path, settings.max_skill_bytes)?;
        if hash_of(&bytes) != self.instruction_revision {
            return Err(Error::Denied("Skill changed while loading".into()));
        }
        String::from_utf8(bytes).map_err(|_| Error::invalid("Skill is not UTF-8"))
    }
}

/// Read `path`, a file of the package at `root`, at most `max` bytes.
///
/// Resolved with the jail first, then opened with no symbolic link anywhere
/// on the way — `root`'s own path included, so a package folder swapped for
/// a link after it was found is refused as surely as a link inside it — and
/// never waiting on a FIFO: what was opened must be a regular file
/// (`platform::LinkFreeDir`, which needs no `openat2` and so reads on every
/// kernel and container profile).
pub fn read_package_file(root: &Path, path: &Path, max: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let jail = crate::jail::Jail::new(root)?;
    let resolved = jail.resolve(&path.to_string_lossy())?;
    let base = crate::platform::LinkFreeDir::open(root)
        .map_err(|_| Error::Denied("Package root changed or follows a symbolic link".into()))?;
    let relative = resolved
        .strip_prefix(root)
        .map_err(|_| Error::invalid("Skill resource escapes its package"))?;
    let file = base
        .open_file(relative)
        .map_err(|error| match error.kind() {
            // Gone since it was found: a missing file, as every reader says it.
            std::io::ErrorKind::NotFound => Error::Io(error),
            _ => Error::Denied("Skill resource could not be opened without following links".into()),
        })?;
    if !file.metadata()?.is_file() {
        return Err(Error::invalid("Skill resource must be a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error::invalid("Skill resource exceeds its size limit"));
    }
    Ok(bytes)
}

pub fn package_files(root: &Path, settings: &Settings) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let root = crate::platform::canonical(root)?;
    let mut files = Vec::new();
    let mut size = 0;
    let mut error = None;
    let opts = Options {
        max_depth: 8,
        max_entries: settings.max_entries,
        respect_ignore: false,
        strict: true,
        ..Options::default()
    };
    let stats = walk::walk(&root, &opts, |e| {
        match read_package_file(
            &root,
            &e.path,
            settings.max_package_bytes.saturating_sub(size),
        ) {
            Ok(bytes) => {
                size += bytes.len();
                files.push((PathBuf::from(e.rel), bytes));
            }
            Err(e) => {
                error = Some(e);
                return false;
            }
        }
        true
    })?;
    if let Some(e) = error {
        return Err(e);
    }
    if stats.truncated {
        return Err(Error::invalid("Skill package exceeds traversal limits"));
    }
    Ok(files)
}

/// The same manifest covers scripts, instructions and resources, so a changed executable invalidates approval.
pub fn package_revision(root: &Path, settings: &Settings) -> Result<String> {
    let files = package_files(root, settings)?;
    let manifest: Vec<_> = files
        .iter()
        .map(|(path, bytes)| (path, hash_of(bytes)))
        .collect();
    Ok(hash_of(&serde_json::to_vec(&manifest)?))
}

/// Installs a complete verified package atomically; updates retain the previous directory as a backup.
pub fn install(
    source: &Path,
    destination_root: &Path,
    settings: &Settings,
    update: bool,
) -> Result<Value> {
    use std::io::Read;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    std::fs::create_dir_all(destination_root)?;
    let destination_root = crate::platform::canonical(destination_root)?;
    let staging = destination_root.join(format!(".RuneInstall{nonce}"));
    std::fs::create_dir(&staging)?;
    let result = (|| {
        let source = crate::platform::canonical(source)?;
        let package = if source.is_dir() {
            source.clone()
        } else {
            let bytes = read_bounded(&source, settings.max_package_bytes as u64)?;
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
                .map_err(|_| Error::invalid("Skill archive must be a supported ZIP file"))?;
            if archive.len() > settings.max_entries {
                return Err(Error::invalid("Too many files in skill archive"));
            }
            let unpack = staging.join("Unpack");
            std::fs::create_dir(&unpack)?;
            let mut seen = BTreeSet::new();
            let mut total = 0usize;
            for index in 0..archive.len() {
                let mut file = archive
                    .by_index(index)
                    .map_err(|_| Error::invalid("Invalid skill archive entry"))?;
                let relative = file
                    .enclosed_name()
                    .ok_or_else(|| Error::invalid("Skill archive path escapes its root"))?;
                if relative.components().count() > 10
                    || file.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
                {
                    return Err(Error::invalid(
                        "Skill archives cannot contain symbolic links or deeply nested paths",
                    ));
                }
                if !seen.insert(relative.clone()) {
                    return Err(Error::invalid("Skill archive has duplicate paths"));
                }
                let target = unpack.join(&relative);
                if file.is_dir() {
                    std::fs::create_dir_all(&target)?;
                    continue;
                }
                if !file.is_file() {
                    return Err(Error::invalid("Skill archive contains a special file"));
                }
                let mut bytes = Vec::new();
                file.by_ref()
                    .take(settings.max_package_bytes.saturating_sub(total) as u64 + 1)
                    .read_to_end(&mut bytes)?;
                total = total.saturating_add(bytes.len());
                if total > settings.max_package_bytes {
                    return Err(Error::invalid(
                        "Expanded skill archive exceeds its byte budget",
                    ));
                }
                std::fs::create_dir_all(target.parent().expect("archive parent"))?;
                crate::settings::secrets::atomic_write(&target, &bytes)?;
            }
            let mut candidates = Vec::new();
            walk::walk(
                &unpack,
                &Options {
                    max_depth: 2,
                    max_entries: settings.max_entries,
                    respect_ignore: false,
                    strict: true,
                    ..Options::default()
                },
                |e| {
                    if e.path.file_name().is_some_and(|n| n == "SKILL.md") {
                        candidates.push(e.path.parent().unwrap().to_path_buf());
                    }
                    true
                },
            )?;
            if candidates.len() != 1 {
                return Err(Error::invalid(
                    "A skill archive must contain exactly one skill package",
                ));
            }
            let package = candidates.remove(0);
            if package == unpack {
                let (meta, _) =
                    metadata(&package.join("SKILL.md"), settings.max_skill_bytes, false)?;
                let renamed = staging.join("Package").join(meta.name);
                std::fs::create_dir_all(renamed.parent().unwrap())?;
                std::fs::rename(&package, &renamed)?;
                renamed
            } else {
                package
            }
        };
        let (meta, _) = metadata(&package.join("SKILL.md"), settings.max_skill_bytes, true)?;
        let files = package_files(&package, settings)?;
        let target = destination_root.join(&meta.name);
        if target.exists() && !update {
            return Err(Error::invalid(
                "Skill already exists; use update explicitly",
            ));
        }
        if !target.exists() && update {
            return Err(Error::invalid(
                "Skill is not installed in the selected scope",
            ));
        }
        if target
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(Error::invalid("Cannot replace a symbolic skill directory"));
        }
        let ready = staging.join("Ready");
        std::fs::create_dir(&ready)?;
        for (relative, bytes) in &files {
            let output = ready.join(relative);
            std::fs::create_dir_all(output.parent().expect("package parent"))?;
            crate::settings::secrets::atomic_write(&output, bytes)?;
        }
        let backup = destination_root.join(format!(".RuneBackup{nonce}"));
        if update {
            std::fs::rename(&target, &backup)?;
        }
        if let Err(error) = std::fs::rename(&ready, &target) {
            if update {
                let _ = std::fs::rename(&backup, &target);
            }
            return Err(error.into());
        }
        let manifest: Vec<_> = files
            .iter()
            .map(|(path, bytes)| (path, hash_of(bytes)))
            .collect();
        Ok(
            json!({"installed":target,"revision":hash_of(&serde_json::to_vec(&manifest)?),"source":source,"backup":update.then_some(backup),"approval_required":true}),
        )
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}
