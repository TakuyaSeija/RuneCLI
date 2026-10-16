//! Unsent composition data. Never a transcript message or an implicit steer.
//! Cursor offsets are UTF-8 byte boundaries; presentation stays frontend-local.

use crate::error::{Error, Result};
use crate::settings::ModelSettings;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingPrompt {
    pub id: i64,
    #[serde(default)]
    pub message_id: Option<i64>,
    pub text: String,
    pub media: Vec<crate::media::Attachment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Composer {
    pub revision: u64,
    pub operations: std::collections::BTreeMap<String, u64>,
    pub pending: Vec<PendingPrompt>,
    pub text: String,
    pub cursor: usize,
    pub attachments: Vec<String>,
    pub settings: ModelSettings,
    pub mode: String,
    /// Defaults alone do not make an empty project visit a saved composition.
    pub preference_changed: bool,
    pub cleared: bool,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            revision: 0,
            operations: std::collections::BTreeMap::new(),
            pending: Vec::new(),
            text: String::new(),
            cursor: 0,
            attachments: Vec::new(),
            settings: ModelSettings::default(),
            mode: "ask".into(),
            preference_changed: false,
            cleared: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    Splice {
        start: usize,
        end: usize,
        text: String,
    },
    Attach {
        path: String,
    },
    Remove {
        path: String,
    },
}

impl Composer {
    pub fn validate(&self) -> Result<()> {
        if self.text.len() > crate::limits::MAX_TOOL_ARG_BYTES
            || self.attachments.len() > 10000
            || self
                .attachments
                .iter()
                .any(|path| path.len() > 4096 || path.contains('\0'))
            || crate::perm::Mode::parse(&self.mode).is_none()
        {
            return Err(Error::invalid(
                "Composer state is invalid or exceeds the supported input size.",
            ));
        }
        Ok(())
    }

    pub fn has_user_state(&self) -> bool {
        !self.text.is_empty() || !self.attachments.is_empty() || self.preference_changed
    }

    /// Nothing a person put here: no text, no file, no model or mode they
    /// chose, no message waiting. Such a composition is not kept.
    pub fn holds_nothing(&self) -> bool {
        !self.has_user_state() && self.pending.is_empty()
    }

    /// Revalidate stored paths without capturing bytes. The caller persists any
    /// change before returning restored state, so invalid paths cannot resurrect.
    pub fn restore(&mut self, project: &Path) -> bool {
        let before = self.clone();
        self.cursor = self.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
        let mut paths = Vec::new();
        for path in &self.attachments {
            let named = Path::new(path);
            let absolute = if named.is_absolute() {
                named.to_path_buf()
            } else {
                project.join(named)
            };
            if let Ok(canonical) = crate::platform::canonical(&absolute) {
                if canonical.is_file() {
                    let canonical = canonical.to_string_lossy().into_owned();
                    if !paths.contains(&canonical) {
                        paths.push(canonical);
                    }
                }
            }
        }
        self.attachments = paths;
        *self != before
    }

    /// Apply operations only to the exact acknowledged base. A stale client gets
    /// the authoritative snapshot, never permission to replace somebody's edits.
    pub fn edit(&self, base: u64, changes: &[Edit], project: &Path) -> Result<Self> {
        if self.revision != base {
            return Err(Error::invalid(
                "Composer changed in another window; rebase this edit on the latest revision.",
            ));
        }
        let mut next = self.clone();
        for change in changes {
            match change {
                Edit::Splice { start, end, text } => {
                    if start > end
                        || *end > next.text.len()
                        || !next.text.is_char_boundary(*start)
                        || !next.text.is_char_boundary(*end)
                    {
                        return Err(Error::invalid(
                            "Composer edit does not address valid text boundaries.",
                        ));
                    }
                    next.text.replace_range(*start..*end, text);
                    next.cursor = start + text.len();
                }
                Edit::Attach { path } => {
                    let named = Path::new(path);
                    let absolute = if named.is_absolute() {
                        named.to_path_buf()
                    } else {
                        project.join(named)
                    };
                    let attachment = crate::media::Attachment::at(&absolute)?;
                    if !next.attachments.contains(&attachment.path) {
                        next.attachments.push(attachment.path);
                    }
                }
                Edit::Remove { path } => next.attachments.retain(|stored| stored != path),
            }
        }
        if next.text.len() > crate::limits::MAX_TOOL_ARG_BYTES {
            return Err(Error::invalid(
                "Composer input exceeds the supported input size.",
            ));
        }
        next.restore(project);
        next.validate()?;
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::Internal("Composer revision exhausted".into()))?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_defaults_are_not_a_saved_visit_and_stale_edits_cannot_overwrite() {
        let initial = Composer::default();
        assert!(!initial.has_user_state());
        let accepted = initial
            .edit(
                0,
                &[Edit::Splice {
                    start: 0,
                    end: 0,
                    text: "a".into(),
                }],
                Path::new("/tmp"),
            )
            .unwrap();
        assert_eq!(accepted.revision, 1);
        assert!(accepted.has_user_state());
        assert!(accepted
            .edit(
                0,
                &[Edit::Splice {
                    start: 0,
                    end: 1,
                    text: "b".into()
                }],
                Path::new("/tmp")
            )
            .is_err());
        assert_eq!(accepted.text, "a");
    }

    #[test]
    fn restore_clamps_utf8_cursor_and_removes_nonexistent_paths() {
        let mut state = Composer {
            text: "é🙂".into(),
            cursor: 3,
            attachments: vec!["/nonexistent-rune-composer-fixture".into()],
            ..Default::default()
        };
        assert!(state.restore(Path::new("/tmp")));
        assert_eq!(state.cursor, 2);
        assert!(state.attachments.is_empty());
        assert!(!state.restore(Path::new("/tmp")));
    }
}
