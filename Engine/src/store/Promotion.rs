//! Recoverable cross-store first-message commit. The SQLite journal reserves a
//! permanent identity once; files are durable before consuming project history.

use super::{
    composer::Composer,
    drafts::{Draft, Drafts, ProjectDrafts, Registry},
    jsonl::SessionMeta,
    sessions::{Message, Role},
    Db,
};
use crate::error::{Error, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Promotion {
    pub token: String,
    pub generation: u64,
    pub meta: SessionMeta,
    pub message: Message,
}

impl Db {
    /// Called only after engine input/model/attachment validation. Holding the
    /// registry lock orders every competing save before or after this commit.
    pub fn promote(
        &self,
        draft: &Draft,
        media: Vec<crate::media::Attachment>,
    ) -> Result<Promotion> {
        draft.validate()?;
        let submission = format!("{}:{}", draft.token, draft.generation);
        Drafts::new(self.jsonl.root()).locked(|registry| {
            let saved: Option<String> = self.conn().query_row("SELECT journal FROM promotions WHERE token=?1", params![submission], |row| row.get(0)).optional()?;
            let promotion = if let Some(saved) = saved {
                let saved: Promotion = serde_json::from_str(&saved)?;
                if saved.meta.project != draft.project { return Err(Error::invalid("Draft promotion belongs to another project.")); }
                saved
            } else {
                let key = crate::project::project_key(Path::new(&draft.project));
                let generation = registry.projects.get(&key).map_or(0, |stored| stored.generation);
                if generation != draft.generation { return Err(Error::invalid("Project drafts were consumed; start a new composition before sending.")); }
                let transaction = rusqlite::Transaction::new_unchecked(self.conn(), rusqlite::TransactionBehavior::Immediate)?;
                let now = super::now_ms();
                let title = super::sessions::title_of(&draft.composer.text);
                let settings = serde_json::to_string(&draft.composer.settings)?;
                self.conn().execute("INSERT INTO sessions(project,title,created_at,updated_at,mode,settings,active_seconds) VALUES(?1,?2,?3,?3,?4,?5,0)", params![draft.project, title, now, draft.composer.mode, settings])?;
                let id = self.conn().last_insert_rowid();
                let message_id = self.next_message_id()?;
                let promotion = Promotion {
                    token: submission.clone(), generation,
                    meta: SessionMeta { id, project: draft.project.clone(), title: Some(title), created_at: now, updated_at: now, mode: draft.composer.mode.clone(), active_seconds: 0, model: Some(draft.composer.settings.model.clone()), settings: Some(settings) },
                    message: Message { work: Some(super::sessions::Work { source: message_id, finished: false }), id: message_id, turn: 1, role: Role::User, content: draft.composer.text.clone(), tool_call_id: None, tool_name: None, tool_calls: None, reasoning: None, media, created_at: now, continuation: None, context: None },
                };
                self.conn().execute("INSERT INTO promotions(token,session_id,journal,complete) VALUES(?1,?2,?3,0)", params![promotion.token, id, serde_json::to_string(&promotion)?])?;
                transaction.commit()?;
                promotion
            };
            self.finish_promotion(&promotion, registry)?;
            Ok(promotion)
        }).and_then(|promotion| {
            // Completion is marked only after the atomic registry rename. Crash
            // here replays the journal without consuming the next generation.
            self.conn().execute("UPDATE promotions SET complete=1 WHERE token=?1", params![promotion.token])?;
            Ok(promotion)
        })
    }

    fn finish_promotion(&self, promotion: &Promotion, registry: &mut Registry) -> Result<()> {
        // Filed under its project before anything is written for it.
        let path = self
            .jsonl
            .register(promotion.meta.id, &promotion.meta.project)?
            .join("session.jsonl");
        let messages = self.jsonl.read_messages(promotion.meta.id)?;
        if !messages
            .iter()
            .any(|message| message.id == promotion.message.id)
        {
            if !messages.is_empty()
                || std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0)
            {
                return Err(Error::invalid("Promotion history conflicts with its first message; existing data was preserved."));
            }
            self.jsonl.ensure_session_dir(promotion.meta.id)?;
            crate::settings::secrets::atomic_write(
                &path,
                format!("{}\n", serde_json::to_string(&promotion.message)?).as_bytes(),
            )?;
        }
        if self.jsonl.read_meta(promotion.meta.id)?.is_none() {
            self.jsonl.write_meta(&promotion.meta)?;
        }
        if self.jsonl.composer(promotion.meta.id)?.is_none() {
            self.jsonl.write_composer(
                promotion.meta.id,
                &Composer {
                    settings: promotion
                        .meta
                        .settings
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .unwrap_or_default(),
                    mode: promotion.meta.mode.clone(),
                    ..Default::default()
                },
            )?;
        }
        let key = crate::project::project_key(Path::new(&promotion.meta.project));
        let stored = registry
            .projects
            .entry(key.clone())
            .or_insert_with(|| ProjectDrafts {
                project: promotion.meta.project.clone(),
                ..Default::default()
            });
        if stored.project != promotion.meta.project {
            return Err(Error::invalid("Promotion project identity mismatch."));
        }
        if stored.generation == promotion.generation {
            stored.generation = stored
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::Internal("Draft generation exhausted".into()))?;
            stored.entries.clear();
            let _ = std::fs::write(
                self.jsonl.root().join(format!("run/{key}.gen")),
                stored.generation.to_le_bytes(),
            );
        }
        Ok(())
    }

    pub fn recover_promotions(&self) -> Result<()> {
        let pending = {
            let mut statement = self
                .conn()
                .prepare("SELECT journal FROM promotions WHERE complete=0 ORDER BY session_id")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for journal in pending {
            let promotion: Promotion = serde_json::from_str(&journal)?;
            Drafts::new(self.jsonl.root())
                .locked(|registry| self.finish_promotion(&promotion, registry))?;
            self.conn().execute(
                "UPDATE promotions SET complete=1 WHERE token=?1",
                params![promotion.token],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_file_commit_preserves_draft_and_recovery_retries_the_same_identity() {
        let db = Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let draft = drafts
            .open(&std::env::temp_dir(), Composer::default(), None)
            .unwrap();
        let draft = drafts
            .save(
                &draft,
                0,
                Composer {
                    text: "preserve me".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        // A file where session 1's folder must go: the commit cannot land.
        let folder = crate::project::open(db.jsonl.root(), &std::env::temp_dir()).unwrap();
        std::fs::create_dir_all(folder.join("sessions")).unwrap();
        let blocked = folder.join("sessions/1");
        std::fs::write(&blocked, "blocked fixture").unwrap();
        assert!(db.promote(&draft, Vec::new()).is_err());
        assert_eq!(
            drafts
                .open(&std::env::temp_dir(), Composer::default(), None)
                .unwrap()
                .composer
                .text,
            "preserve me"
        );
        std::fs::remove_file(blocked).unwrap();
        db.recover_promotions().unwrap();
        let committed = db.promote(&draft, Vec::new()).unwrap();
        assert_eq!(committed.meta.id, 1);
        assert_eq!(db.jsonl.read_messages(1).unwrap().len(), 1);
    }

    #[test]
    fn recovery_finishes_reserved_row_without_reconsuming_new_generation() {
        for phase in 0..3 {
            let db = Db::memory().unwrap();
            let drafts = Drafts::new(db.jsonl.root());
            let draft = drafts
                .open(&std::env::temp_dir(), Composer::default(), None)
                .unwrap();
            let draft = drafts
                .save(
                    &draft,
                    0,
                    Composer {
                        text: "first".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let promotion = db.promote(&draft, Vec::new()).unwrap();
            db.conn()
                .execute(
                    "UPDATE promotions SET complete=0 WHERE token=?1",
                    params![promotion.token],
                )
                .unwrap();
            if phase == 0 {
                std::fs::remove_dir_all(db.jsonl.session_dir(promotion.meta.id)).unwrap();
                drafts
                    .locked(|registry| {
                        let project = registry.projects.values_mut().next().unwrap();
                        project.generation = 0;
                        project.entries = draft.history.clone();
                        Ok(())
                    })
                    .unwrap();
            } else if phase == 1 {
                std::fs::remove_file(db.jsonl.session_dir(promotion.meta.id).join("meta.json"))
                    .unwrap();
            }
            let next = drafts
                .open(&std::env::temp_dir(), Composer::default(), None)
                .unwrap();
            let next = if phase == 0 {
                next
            } else {
                drafts
                    .save(
                        &next,
                        0,
                        Composer {
                            text: "after".into(),
                            ..Default::default()
                        },
                    )
                    .unwrap()
            };
            db.recover_promotions().unwrap();
            db.recover_promotions().unwrap();
            assert_eq!(db.jsonl.read_messages(promotion.meta.id).unwrap().len(), 1);
            assert!(db.jsonl.read_meta(promotion.meta.id).unwrap().is_some());
            if phase > 0 {
                assert_eq!(
                    drafts
                        .open(&std::env::temp_dir(), Composer::default(), None)
                        .unwrap()
                        .composer
                        .text,
                    next.composer.text
                );
            }
            assert_eq!(
                db.promote(&draft, Vec::new()).unwrap().meta.id,
                promotion.meta.id
            );
        }
    }

    #[test]
    fn promotion_is_idempotent_consumes_peers_and_preserves_next_generation() {
        let db = Db::memory().unwrap();
        let drafts = Drafts::new(db.jsonl.root());
        let project = std::env::temp_dir();
        let first = drafts.open(&project, Composer::default(), None).unwrap();
        let first = drafts
            .save(
                &first,
                0,
                Composer {
                    text: "a".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let peer = drafts.open(&project, Composer::default(), None).unwrap();
        let peer = drafts
            .save(
                &peer,
                0,
                Composer {
                    text: "b".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let committed = db.promote(&peer, Vec::new()).unwrap();
        assert_eq!(committed.message.content, "b");
        let consumed = drafts
            .save(&first, first.composer.revision, first.composer.clone())
            .unwrap();
        assert!(consumed.composer.text.is_empty());
        assert!(consumed.history.is_empty());
        let next = drafts.open(&project, Composer::default(), None).unwrap();
        let next = drafts
            .save(
                &next,
                0,
                Composer {
                    text: "after".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            db.promote(&peer, Vec::new()).unwrap().meta.id,
            committed.meta.id
        );
        assert_eq!(
            drafts
                .open(&project, Composer::default(), None)
                .unwrap()
                .composer
                .text,
            next.composer.text
        );
        assert_eq!(db.jsonl.read_messages(committed.meta.id).unwrap().len(), 1);
        let mut reused = consumed;
        reused.composer.text = "next generation".into();
        let later = db.promote(&reused, Vec::new()).unwrap();
        assert_ne!(later.meta.id, committed.meta.id);
        assert_eq!(
            db.conn()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
}
