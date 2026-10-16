//! One locked snapshot for unsent project compositions. Frontends never share
//! their visible draft. The registry orders background history and promotion.

use super::composer::Composer;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub token: String,
    pub order: u64,
    pub composer: Composer,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectDrafts {
    pub project: String,
    pub generation: u64,
    pub entries: Vec<Entry>,
    /// The order at which a composition was last emptied: a new window that
    /// restores the newest composition restores none when nothing was written
    /// after it.
    ///
    /// # Why not an empty entry
    ///
    /// It was one — the emptied composition kept as a tombstone, so an older,
    /// non-empty one was not resurrected in its place. Every window closed
    /// without a word left one behind, and `drafts.json` filled with entries
    /// holding nothing (owner, 2026-10-09: an entry with no data is not kept).
    /// One number says what the tombstones said.
    #[serde(default)]
    pub cleared: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub clock: u64,
    pub projects: BTreeMap<String, ProjectDrafts>,
}

impl Registry {
    /// Hold the rule that no entry holds nothing, whoever wrote the file last.
    ///
    /// The format-5 upgrade takes them out once, but a window of the previous
    /// build that is open on a draft holds no session, so the upgrade does not
    /// wait for it: it still writes its emptied composition as an entry on its
    /// way to `exec` the new build (measured: the tombstone was back 0.6 s
    /// after the upgrade). Every write here folds such an entry into the
    /// project's `cleared`, exactly as the upgrade does.
    fn drop_empty(&mut self) {
        for stored in self.projects.values_mut() {
            let newest = stored
                .entries
                .iter()
                .filter(|entry| entry.composer.holds_nothing())
                .map(|entry| entry.order)
                .max();
            if let Some(order) = newest {
                stored.cleared = stored.cleared.max(order);
                stored
                    .entries
                    .retain(|entry| !entry.composer.holds_nothing());
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub token: String,
    pub project: String,
    pub generation: u64,
    pub composer: Composer,
    pub history: Vec<Entry>,
}

impl Draft {
    pub fn validate(&self) -> Result<()> {
        if self.token.is_empty()
            || self.token.len() > 256
            || !self
                .token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(Error::invalid("Draft identity is invalid."));
        }
        let canonical = crate::platform::canonical(Path::new(&self.project))?;
        if !canonical.is_dir() || canonical.to_string_lossy() != self.project {
            return Err(Error::invalid(
                "Draft workspace must be a canonical project directory.",
            ));
        }
        self.composer.validate()
    }
}

pub struct Drafts {
    root: PathBuf,
}

impl Drafts {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// All read/modify/write operations share this lock and atomic writer. Lock
    /// inode lives outside the renamed snapshot and must never be deleted.
    pub fn locked<T>(&self, operation: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let _lock =
            crate::lock::exclusive(&self.root.join("run/Drafts.lock"), crate::lock::Wait::Yes)?;
        let path = self.root.join("drafts.json");
        let (existed, mut registry) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(registry) => (true, registry),
                // A registry something else damaged never stops a window from
                // opening (owner, 2026-10-16): it is kept aside — it may hold
                // words not yet sent — and a fresh one is started.
                Err(_) => {
                    let aside = self
                        .root
                        .join("backups")
                        .join(format!("Drafts-{}.json", super::now_ms()));
                    std::fs::create_dir_all(aside.parent().expect("backups folder"))?;
                    std::fs::rename(&path, &aside)?;
                    (false, Registry::default())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (false, Registry::default())
            }
            Err(error) => return Err(error.into()),
        };
        let before = serde_json::to_vec(&registry)?;
        registry.drop_empty();
        let result = operation(&mut registry)?;
        let after = serde_json::to_vec(&registry)?;
        if (!existed && !registry.projects.is_empty()) || (existed && before != after) {
            std::fs::create_dir_all(path.parent().unwrap())?;
            crate::settings::secrets::atomic_write(&path, &super::jsonl::pretty(&registry)?)?;
        }
        Ok(result)
    }

    /// Drop every unsent composition kept for `project`, a path recorded when
    /// it existed: the project is gone, and so is anything it would restore.
    pub fn forget(&self, project: &Path) -> Result<()> {
        let key = crate::project::project_key(project);
        self.locked(|registry| {
            registry.projects.remove(&key);
            Ok(())
        })?;
        // The generation mirror goes with the drafts it counts.
        let _ = std::fs::remove_file(self.root.join("run").join(format!("{key}.gen")));
        Ok(())
    }

    /// Visiting a project assigns only a frontend token, never a DB ID or an
    /// on-disk registry entry. Canonical aliases address the same history.
    pub fn open(&self, project: &Path, defaults: Composer, token: Option<String>) -> Result<Draft> {
        let project = crate::platform::canonical(project)?
            .to_string_lossy()
            .into_owned();
        let key = crate::capabilities::config::project_key(Path::new(&project));
        let restore_latest = token.is_none();
        let token = token.unwrap_or_else(|| {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            format!(
                "{}-{}-{}-{}",
                std::process::id(),
                super::now_ms(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                crate::tools::unique_persist_id(&self.root.join("run"), ".draft")
            )
        });
        self.locked(|registry| {
            let mut composer = defaults;
            let mut generation = 0;
            let mut history = Vec::new();
            if let Some(stored) = registry.projects.get_mut(&key) {
                if stored.project != project {
                    return Err(Error::invalid(
                        "Draft project identity does not match its canonical path.",
                    ));
                }
                generation = stored.generation;
                for entry in &mut stored.entries {
                    if entry.composer.restore(Path::new(&project)) {
                        entry.composer.revision = entry.composer.revision.saturating_add(1);
                    }
                }
                history = stored.entries.clone();
                history.sort_by_key(|entry| entry.order);
                // A composition emptied after the newest one is not permission
                // to resurrect that older one.
                let cleared = stored.cleared;
                if let Some(last) =
                    history
                        .iter()
                        .find(|entry| entry.token == token)
                        .or_else(|| {
                            restore_latest
                                .then(|| history.last())
                                .flatten()
                                .filter(|entry| entry.order > cleared)
                        })
                {
                    composer = last.composer.clone();
                    if last.token != token {
                        composer.revision = 0;
                    }
                }
            }
            Ok(Draft {
                token,
                project,
                generation,
                composer,
                history,
            })
        })
    }

    /// Coalesce all saves by one frontend token. A stale generation is returned
    /// as consumed without inserting old text into the next generation.
    pub fn save(&self, draft: &Draft, base: u64, mut composer: Composer) -> Result<Draft> {
        draft.validate()?;
        composer.validate()?;
        let key = crate::capabilities::config::project_key(Path::new(&draft.project));
        self.locked(|registry| {
            let stored_generation = registry
                .projects
                .get(&key)
                .map_or(0, |stored| stored.generation);
            if stored_generation != draft.generation {
                return Ok(Draft {
                    generation: stored_generation,
                    composer: Composer {
                        settings: composer.settings,
                        mode: composer.mode,
                        ..Default::default()
                    },
                    history: Vec::new(),
                    ..draft.clone()
                });
            }
            if composer.holds_nothing() {
                return forget_entry(registry, &key, draft, base, composer);
            }
            let stored = registry
                .projects
                .entry(key)
                .or_insert_with(|| ProjectDrafts {
                    project: draft.project.clone(),
                    ..Default::default()
                });
            if stored.project != draft.project {
                return Err(Error::invalid("Draft project identity mismatch."));
            }
            let previous = stored
                .entries
                .iter()
                .position(|entry| entry.token == draft.token);
            let revision = previous.map_or(0, |index| stored.entries[index].composer.revision);
            if revision != base {
                return Err(Error::invalid(
                    "Draft revision changed; the saved composition was preserved.",
                ));
            }
            composer.restore(Path::new(&draft.project));
            composer.revision = revision
                .checked_add(1)
                .ok_or_else(|| Error::Internal("Draft revision exhausted".into()))?;
            registry.clock = registry
                .clock
                .checked_add(1)
                .ok_or_else(|| Error::Internal("Draft order exhausted".into()))?;
            let entry = Entry {
                token: draft.token.clone(),
                order: registry.clock,
                composer: composer.clone(),
            };
            if let Some(index) = previous {
                stored.entries[index] = entry;
            } else {
                stored.entries.push(entry);
            }
            Ok(Draft {
                composer,
                history: stored.entries.clone(),
                ..draft.clone()
            })
        })
    }
}

/// Save a composition that holds nothing: its window's entry goes, and the
/// project remembers that the newest composition is now an empty one.
///
/// Nothing is written for a window that never kept anything and is not
/// clearing anything — a visit is not a composition.
fn forget_entry(
    registry: &mut Registry,
    key: &str,
    draft: &Draft,
    base: u64,
    mut composer: Composer,
) -> Result<Draft> {
    // A project with nothing kept has nothing older to resurrect, so a clear
    // there has nothing to remember either.
    let Some(stored) = registry.projects.get_mut(key) else {
        composer.revision = 0;
        return Ok(Draft {
            composer,
            history: Vec::new(),
            ..draft.clone()
        });
    };
    if stored.project != draft.project {
        return Err(Error::invalid("Draft project identity mismatch."));
    }
    let previous = stored
        .entries
        .iter()
        .position(|entry| entry.token == draft.token);
    let revision = previous.map_or(0, |index| stored.entries[index].composer.revision);
    if revision != base {
        return Err(Error::invalid(
            "Draft revision changed; the saved composition was preserved.",
        ));
    }
    if previous.is_some() || composer.cleared {
        registry.clock = registry
            .clock
            .checked_add(1)
            .ok_or_else(|| Error::Internal("Draft order exhausted".into()))?;
        let stored = registry.projects.get_mut(key).expect("looked up above");
        stored.cleared = registry.clock;
        if let Some(index) = previous {
            stored.entries.remove(index);
        }
    }
    composer.revision = 0;
    Ok(Draft {
        composer,
        history: registry
            .projects
            .get(key)
            .map(|stored| stored.entries.clone())
            .unwrap_or_default(),
        ..draft.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_tokens_validate_and_invalid_tokens_cannot_write() {
        let db = super::super::Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let mut draft = drafts
            .open(
                &std::env::temp_dir(),
                Composer::default(),
                Some("frontend_token-1".into()),
            )
            .unwrap();
        assert!(draft.validate().is_ok());
        draft.token = "../other".into();
        assert!(drafts
            .save(
                &draft,
                0,
                Composer {
                    text: "invalid".into(),
                    ..Default::default()
                }
            )
            .is_err());
    }

    #[test]
    fn visits_are_lazy_edits_coalesce_and_clear_does_not_resurrect_history() {
        let db = super::super::Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let project = std::env::temp_dir();
        let mut first = drafts.open(&project, Composer::default(), None).unwrap();
        assert!(!db.jsonl.root().join("sessions").exists());
        first = drafts
            .save(
                &first,
                0,
                Composer {
                    text: "a".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        first = drafts
            .save(
                &first,
                1,
                Composer {
                    text: "aa".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(first.history.len(), 1);
        let restored = drafts.open(&project, Composer::default(), None).unwrap();
        drafts
            .save(
                &restored,
                0,
                Composer {
                    cleared: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(drafts
            .open(&project, Composer::default(), None)
            .unwrap()
            .composer
            .text
            .is_empty());
        let mut second = drafts.open(&project, Composer::default(), None).unwrap();
        assert_ne!(first.token, second.token);
        second = drafts
            .save(
                &second,
                0,
                Composer {
                    text: "b".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        // The cleared composition kept no entry: "aa" and "b" are all there is.
        assert_eq!(second.history.len(), 2);
        assert_eq!(first.composer.text, "aa");
        let before_clear = second.clone();
        second = drafts.save(&second, 1, Composer::default()).unwrap();
        let restored = drafts.open(&project, Composer::default(), None).unwrap();
        assert!(restored.composer.text.is_empty());
        assert_eq!(
            restored.history.len(),
            1,
            "the emptied window's entry is gone"
        );
        // A save still based on the revision before the clear is stale.
        assert!(drafts
            .save(
                &before_clear,
                1,
                Composer {
                    text: "stale".into(),
                    ..Default::default()
                }
            )
            .is_err());
        // The emptied window starts again from nothing.
        assert_eq!(second.composer.revision, 0);
        assert_eq!(
            db.conn()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    /// Owner 2026-10-09: an entry with no data — no text, no file, no chosen
    /// model or mode, nothing waiting — is not kept in `drafts.json`, and its
    /// going still keeps an older composition from being resurrected.
    #[test]
    fn a_composition_holding_nothing_is_not_kept() {
        let db = super::super::Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let project = std::env::temp_dir();
        let stored = |drafts: &Drafts| {
            drafts
                .locked(|registry| {
                    Ok(registry
                        .projects
                        .values()
                        .map(|p| p.entries.len())
                        .sum::<usize>())
                })
                .unwrap()
        };
        // A visit, a clear with nothing kept, and an empty save write nothing.
        let visit = drafts.open(&project, Composer::default(), None).unwrap();
        drafts.save(&visit, 0, Composer::default()).unwrap();
        drafts
            .save(
                &visit,
                0,
                Composer {
                    cleared: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(stored(&drafts), 0);
        assert!(!db.jsonl.root().join("drafts.json").exists() || stored(&drafts) == 0);

        // Text kept, then emptied: the entry goes, and a new window does not
        // get the older text back.
        let older = drafts.open(&project, Composer::default(), None).unwrap();
        drafts
            .save(
                &older,
                0,
                Composer {
                    text: "older".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let newer = drafts
            .open(&project, Composer::default(), Some("newer-window".into()))
            .unwrap();
        let newer = drafts
            .save(
                &newer,
                0,
                Composer {
                    text: "newer".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(stored(&drafts), 2);
        drafts.save(&newer, 1, Composer::default()).unwrap();
        assert_eq!(stored(&drafts), 1, "the emptied entry is gone");
        let fresh = drafts.open(&project, Composer::default(), None).unwrap();
        assert!(fresh.composer.text.is_empty(), "the older text came back");

        // A chosen model alone is data, and is kept.
        let chosen = drafts
            .open(&project, Composer::default(), Some("chosen".into()))
            .unwrap();
        drafts
            .save(
                &chosen,
                0,
                Composer {
                    preference_changed: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(stored(&drafts), 2);
        // Written after the clear, it is the newest composition and is
        // restored.
        let fresh = drafts.open(&project, Composer::default(), None).unwrap();
        assert!(fresh.composer.preference_changed);
    }

    /// Owner, 2026-10-16: a damaged `drafts.json` refused every window.
    #[test]
    fn a_damaged_registry_is_kept_aside_and_a_window_still_opens() {
        let db = super::super::Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        std::fs::create_dir_all(db.jsonl.root()).unwrap();
        std::fs::write(db.jsonl.root().join("drafts.json"), "{\"clock\": 4").unwrap();
        let opened = drafts
            .open(&std::env::temp_dir(), Composer::default(), None)
            .unwrap();
        assert!(opened.composer.text.is_empty());
        let kept: Vec<_> = std::fs::read_dir(db.jsonl.root().join("backups"))
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        assert_eq!(
            kept,
            vec!["{\"clock\": 4".to_string()],
            "the damaged words are kept"
        );
    }

    /// A window of the previous build writes an emptied composition as an
    /// entry even after the upgrade took them out; the next write here folds it
    /// away again, and the older words stay unresurrected.
    #[test]
    fn an_empty_entry_written_by_an_older_build_is_folded_at_the_next_write() {
        let db = super::super::Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let project = crate::platform::canonical(&std::env::temp_dir()).unwrap();
        let key = crate::capabilities::config::project_key(&project);
        let older = serde_json::json!({
            "clock": 9,
            "projects": { key.clone(): {
                "project": project.to_string_lossy(),
                "generation": 0,
                "entries": [
                    {"token": "words", "order": 3, "composer": {"text": "older words"}},
                    {"token": "gone", "order": 7, "composer": {"text": "", "cleared": true}}
                ]
            }}
        });
        std::fs::create_dir_all(db.jsonl.root()).unwrap();
        std::fs::write(
            db.jsonl.root().join("drafts.json"),
            serde_json::to_vec(&older).unwrap(),
        )
        .unwrap();
        let opened = drafts.open(&project, Composer::default(), None).unwrap();
        assert!(opened.composer.text.is_empty(), "the older words came back");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(db.jsonl.root().join("drafts.json")).unwrap())
                .unwrap();
        let entries = stored["projects"][&key]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["token"], "words");
        assert_eq!(stored["projects"][&key]["cleared"], 7);
    }
}
