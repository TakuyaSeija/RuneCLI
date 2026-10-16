//! Conversation rows: sessions, messages, and the record of every tool call.
//!
//! History is stored in the shape the loop needs to rebuild a request — role,
//! content, and the tool-call identity that ties a result back to its call.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{now_ms, Db};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: i64,
    pub project: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Permission mode, as stored. `ask` unless the user chose otherwise.
    pub mode: String,
    pub active_seconds: i64,
    pub model: Option<String>,
    #[serde(default)]
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    Error,
    /// Written by Rune, never by the user: the context a session opens with,
    /// a status update, a background notification. Sent to the model in the
    /// user's turn — no provider has another place for it mid-conversation —
    /// but never read as the user's words: the permission classifier and the
    /// history a frontend draws both tell it apart by this role.
    Context,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Error => "error",
            Role::Context => "context",
        }
    }

    pub fn parse(s: &str) -> Role {
        match s {
            "user" => Role::User,
            "tool" => Role::Tool,
            "error" => Role::Error,
            "context" => Role::Context,
            _ => Role::Assistant,
        }
    }
}

/// What a [`Role::Context`] row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    /// What the session opens with: the memory index, `AGENTS.md`, the skill
    /// catalogue and the status. Sent before the first request it was taken for.
    Session,
    /// Status values that changed since the model was last told them.
    Status,
    /// The memory index outgrew its recall budget, or fits it again.
    Memory,
    /// A background task ended.
    Notification,
    /// The user activated a skill through Rune.
    Skill,
    /// The user ran a prompt an MCP server offers.
    McpPrompt,
    /// What the session was handed from a file — `AGENTS.md`, the memory
    /// index, the skill list — changed since the model read it.
    ContextUpdate,
    /// The user put files back the way they were before a turn (undo).
    Restore,
}

/// The data a context row carries beside its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMeta {
    pub kind: ContextKind,
    /// The status values this row told the model, by name. The newest value of
    /// each name, folded over the rows the model still holds, is what it knows.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub status: std::collections::BTreeMap<String, String>,
    /// What the row is about, for a frontend that names it: a skill's id, an
    /// MCP server's id. Never read back by the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The fingerprint of each file-born part of the context this row handed
    /// the model (`AGENTS.md`, the memory index, the skill list), by name —
    /// folded over the rows the model holds exactly like `status`
    /// (`session::context::sources_held`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sources: std::collections::BTreeMap<String, String>,
    /// The row in a person's words, for the frontend that shows it in the
    /// conversation (a status or context update, a restore). Never sent to
    /// the model, never read back by the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ContextMeta {
    pub fn new(kind: ContextKind) -> ContextMeta {
        ContextMeta {
            kind,
            status: Default::default(),
            name: None,
            sources: Default::default(),
            note: None,
        }
    }

    pub fn named(kind: ContextKind, name: impl Into<String>) -> ContextMeta {
        ContextMeta {
            name: Some(name.into()),
            ..ContextMeta::new(kind)
        }
    }
}

/// The request that owns this work, not an authorization inferred from prose.
/// A final response closes it; failures and restarts keep its source available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    pub source: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub finished: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<Work>,
    pub continuation: Option<crate::provider::Continuation>,
    pub id: i64,
    pub turn: i64,
    pub role: Role,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    /// Calls this assistant message asked for. Present only on assistant rows,
    /// and only when the model asked for a tool.
    ///
    /// Parsed on the way out rather than handed over as text: a frontend
    /// rendering a transcript needs the fields, and making every reader parse
    /// the same string is how two readers end up disagreeing about it.
    pub tool_calls: Option<Value>,
    /// The model's reasoning for this message, where the provider exposed it.
    pub reasoning: Option<String>,
    /// Files the user attached to this turn. Empty on every other row.
    pub media: Vec<crate::media::Attachment>,
    pub created_at: i64,
    /// Present exactly on [`Role::Context`] rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextMeta>,
}

/// One entry in the audit trail: a tool the engine ran, and how it went.
///
/// `action` is the model's own one-line description, lifted from the pending
/// arguments. Full model calls live in the assistant's session.jsonl row, not
/// this bounded list; only result status/code and timing are duplicated here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub turn: i64,
    pub name: String,
    pub action: String,
    /// `None` while the call is still running.
    pub ok: Option<bool>,
    /// How it ended when that is more than ok or not — `denied`, `cancelled`,
    /// `timed_out`, `backgrounded`, … — as a label for a frontend. It is not
    /// part of what the model reads, and absent on rows written before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The Error variant's stable code, shared with the live result. Older
    /// rows did not record it: unknown stays unknown, never inferred from prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub summary: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

/// A tool call being recorded as it happens.
#[derive(Debug, Clone)]
pub struct NewToolCall<'a> {
    pub id: &'a str,
    pub turn: i64,
    pub name: &'a str,
    pub args: &'a str,
    pub started_at: i64,
}

/// What to write for one message. Separate from [`Message`] because the id and
/// timestamp are the store's to assign, not the caller's.
#[derive(Debug, Clone)]
pub struct NewMessage<'a> {
    pub work: Option<Work>,
    pub id: Option<i64>,
    pub continuation: Option<&'a str>,
    pub turn: i64,
    pub role: Role,
    pub content: &'a str,
    pub tool_call_id: Option<&'a str>,
    pub tool_name: Option<&'a str>,
    pub tool_calls: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    /// Files the user attached, as JSON — see the `media` column.
    pub media: Option<&'a str>,
    /// Required on a [`Role::Context`] row, absent on every other.
    pub context: Option<ContextMeta>,
}

impl<'a> NewMessage<'a> {
    /// An ordinary message: a role and its text.
    pub fn plain(turn: i64, role: Role, content: &'a str) -> NewMessage<'a> {
        NewMessage {
            work: None,
            id: None,
            continuation: None,
            turn,
            role,
            content,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            media: None,
            context: None,
        }
    }

    /// A row Rune writes for the model, of `kind`.
    pub fn context(turn: i64, content: &'a str, meta: ContextMeta) -> NewMessage<'a> {
        NewMessage {
            context: Some(meta),
            ..NewMessage::plain(turn, Role::Context, content)
        }
    }
}

impl Db {
    /// A shared OS lease protects sessions owned by any engine process.
    /// Lock files stay outside session directories so deletion cannot replace
    /// the inode another process has locked.
    pub fn lease_session(&self, id: i64) -> Result<()> {
        let mut held = crate::panic::lock(&self.session_leases);
        if let std::collections::hash_map::Entry::Vacant(entry) = held.entry(id) {
            let file = crate::lock::shared(
                &self
                    .jsonl
                    .root()
                    .join("run")
                    .join(format!("session_{id}.lock")),
                crate::lock::Wait::Yes,
            )?;
            entry.insert(file);
        }
        Ok(())
    }

    pub fn create_session(&self, project: &str) -> Result<Session> {
        let ts = now_ms();
        self.conn().execute(
            "INSERT INTO sessions (project, title, created_at, updated_at, active_seconds) VALUES (?1, NULL, ?2, ?2, 0)",
            params![project, ts],
        )?;
        let id = self.conn().last_insert_rowid();
        let session = Session {
            id,
            project: project.to_string(),
            title: None,
            created_at: ts,
            updated_at: ts,
            mode: "ask".into(),
            active_seconds: 0,
            model: None,
            size_bytes: 0,
        };
        let meta = super::jsonl::SessionMeta::from(&session);
        self.jsonl.write_meta(&meta)?;
        Ok(session)
    }

    /// The newest session in `project` that nobody has ever said anything in.
    ///
    /// # Why this exists
    ///
    /// Opening a workspace inserted a row every single time, and `/new` inserted
    /// another — so a day of starting the program, changing one's mind, and
    /// starting it again left a trail of sessions with nothing in them, and the
    /// resume list was mostly empty rows. An empty session is *indistinguishable*
    /// from a fresh one: there is no state in it to carry over and nothing in it
    /// to lose. So the fresh one is the empty one, and a session row is only ever
    /// spent on a conversation somebody actually had.
    ///
    /// Emptiness is measured in messages rather than in age or title, because a
    /// message is the only thing that makes a session worth keeping apart.
    pub fn empty_session(&self, project: &str) -> Result<Option<i64>> {
        // Emptiness is judged against each session's own transcript
        // (has_any_message); the rows only say which sessions exist.
        let mut st = self
            .conn()
            .prepare("SELECT s.id FROM sessions s WHERE s.project = ?1 ORDER BY s.id DESC")?;
        let ids: Vec<i64> = st
            .query_map(params![project], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            if !self.has_any_message(id) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Persist the session's permission mode so re-opening it does not silently
    /// drop back to a different level of trust than the user last chose.
    pub fn set_mode(&self, session: i64, mode: &str) -> Result<()> {
        let ts = now_ms();
        self.conn().execute(
            "UPDATE sessions SET mode = ?2, updated_at = ?3 WHERE id = ?1",
            params![session, mode, ts],
        )?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            meta.mode = mode.to_string();
            meta.updated_at = ts;
            self.jsonl.write_meta(&meta)?;
        }
        Ok(())
    }

    pub fn update_active_seconds(&self, session: i64, seconds: i64) -> Result<()> {
        let ts = now_ms();
        self.conn().execute(
            "UPDATE sessions SET active_seconds = ?2, updated_at = ?3 WHERE id = ?1",
            params![session, seconds, ts],
        )?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            meta.active_seconds = seconds;
            meta.updated_at = ts;
            self.jsonl.write_meta(&meta)?;
        }
        Ok(())
    }

    pub fn get_session(&self, id: i64) -> Result<Session> {
        if let Ok(Some(meta)) = self.jsonl.read_meta(id) {
            return Ok(Session::from(meta));
        }
        self.conn()
            .query_row(
                "SELECT s.id, s.project, s.title, s.created_at, s.updated_at, s.mode, s.active_seconds, s.settings,
                        (SELECT tu.model FROM token_usage tu WHERE tu.session_id = s.id AND tu.model IS NOT NULL AND tu.model != '' ORDER BY tu.id DESC LIMIT 1) AS last_model
                 FROM sessions s WHERE s.id = ?1",
                params![id],
                |r| {
                    let settings_str: Option<String> = r.get(7)?;
                    let last_model: Option<String> = r.get(8)?;
                    let model = settings_str
                        .as_deref()
                        .and_then(|raw| {
                            serde_json::from_str::<serde_json::Value>(raw)
                                .ok()
                                .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
                        })
                        .or(last_model);
                    Ok(Session {
                        id: r.get(0)?,
                        project: r.get(1)?,
                        title: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                        mode: r.get(5)?,
                        active_seconds: r.get(6)?,
                        model,
                        size_bytes: 0,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("session {id}")))
    }

    /// Newest first. `project` narrows to one workspace. Each session's own
    /// `meta.json` is the index; there is no other list to consult.
    pub fn list_sessions(&self, project: Option<&str>, limit: i64) -> Result<Vec<Session>> {
        self.jsonl.list_sessions(project, limit as usize)
    }

    /// First line of the first user message, used as a session label. Set once.
    pub fn set_title_if_unset(&self, session: i64, title: &str) -> Result<()> {
        let trimmed = title_of(title);
        if trimmed.is_empty() {
            return Ok(());
        }
        self.conn().execute(
            "UPDATE sessions SET title = ?2 WHERE id = ?1 AND (title IS NULL OR title = '')",
            params![session, trimmed],
        )?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            if meta.title.as_deref().unwrap_or("").is_empty() {
                meta.title = Some(trimmed.to_string());
                self.jsonl.write_meta(&meta)?;
            }
        }
        Ok(())
    }

    /// Replace a session's label, whatever it was.
    ///
    /// The counterpart to [`Db::set_title_if_unset`]: that one names a session
    /// from its first prompt and then never touches it again, which leaves a
    /// long conversation stuck with a label from its opening line. Returns
    /// whether a session was actually renamed.
    pub fn rename_session(&self, session: i64, title: &str) -> Result<bool> {
        let trimmed = title_of(title);
        if trimmed.is_empty() {
            return Err(Error::invalid("a session title cannot be empty"));
        }
        let ts = now_ms();
        let n = self.conn().execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![session, trimmed, ts],
        )?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            meta.title = Some(trimmed.to_string());
            meta.updated_at = ts;
            self.jsonl.write_meta(&meta)?;
        }
        Ok(n > 0)
    }

    /// The session's model settings, as stored JSON. `None` for a session that
    /// predates them, or one whose provider was never configured.
    pub fn settings_json(&self, session: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT settings FROM sessions WHERE id = ?1",
                params![session],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn set_settings_json(&self, session: i64, json: &str) -> Result<()> {
        let ts = now_ms();
        self.conn().execute(
            "UPDATE sessions SET settings = ?2, updated_at = ?3 WHERE id = ?1",
            params![session, json, ts],
        )?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            meta.settings = Some(json.to_string());
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(json) {
                if let Some(m) = val.get("model").and_then(|m| m.as_str()) {
                    meta.model = Some(m.to_string());
                }
            }
            meta.updated_at = ts;
            self.jsonl.write_meta(&meta)?;
        }
        Ok(())
    }

    /// Retrieve persistent per-model settings JSON.
    pub fn get_model_settings_json(&self, provider: &str, model: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT settings FROM model_settings WHERE provider = ?1 AND model = ?2",
                params![provider, model],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Save persistent per-model settings JSON.
    pub fn set_model_settings_json(&self, provider: &str, model: &str, json: &str) -> Result<()> {
        let ts = now_ms();
        self.conn().execute(
            "INSERT INTO model_settings (provider, model, settings, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (provider, model) DO UPDATE SET settings = excluded.settings, updated_at = excluded.updated_at",
            params![provider, model, json, ts],
        )?;
        Ok(())
    }

    /// Return the stored settings JSON of recent sessions, ordered newest first.
    pub fn recent_settings_json(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT settings FROM sessions WHERE settings IS NOT NULL AND trim(settings) != '' ORDER BY updated_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Record a summary standing in for every message up to and including its
    /// `through_id`. Written once and read back verbatim: a summary that is
    /// recomputed produces a different conversation on every reload.
    pub fn add_compaction(
        &self,
        session: i64,
        record: &crate::store::jsonl::CompactionRecord,
    ) -> Result<()> {
        self.jsonl.append_compaction(session, record)
    }

    /// The newest compaction of a session.
    pub fn compaction(
        &self,
        session: i64,
    ) -> Result<Option<crate::store::jsonl::CompactionRecord>> {
        Ok(self.jsonl.read_compactions(session)?.into_iter().next())
    }

    /// The turn number a new prompt should use: one past the highest stored.
    pub fn next_turn(&self, session: i64) -> Result<i64> {
        let messages = self.jsonl.read_messages_shared(session)?;
        Ok(messages.iter().map(|m| m.turn).max().unwrap_or(0) + 1)
    }

    /// True when the session has any recorded message. Used by the empty-session
    /// cleanup, so an unreadable transcript counts as a conversation: it is not
    /// evidence of an empty session.
    pub fn has_any_message(&self, session: i64) -> bool {
        let history = self.jsonl.session_dir(session).join("session.jsonl");
        if std::fs::metadata(&history).is_ok_and(|meta| meta.len() > 0) {
            return true;
        }
        !matches!(self.jsonl.read_messages(session), Ok(messages) if messages.is_empty())
    }

    pub fn append_message(&self, session: i64, msg: NewMessage<'_>) -> Result<i64> {
        let ts = now_ms();
        // JSONL-only: the transcript lives in the per-session session.jsonl. The id
        // still must be global and monotonic (compaction's through_id depends on it),
        // so it comes from the `counters` ledger, not a `messages` AUTOINCREMENT that
        // no longer exists as the store of record.
        let inserted_id = match msg.id {
            Some(id) => {
                if self
                    .jsonl
                    .read_messages(session)?
                    .iter()
                    .any(|message| message.id == id)
                {
                    return Ok(id);
                }
                id
            }
            None => self.next_message_id()?,
        };
        super::while_busy(|| {
            self.conn().execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session, ts],
            )?;
            Ok(())
        })?;

        let msg_obj = Message {
            work: msg.work,
            continuation: msg.continuation.and_then(|j| serde_json::from_str(j).ok()),
            id: inserted_id,
            turn: msg.turn,
            role: msg.role,
            content: msg.content.to_string(),
            tool_call_id: msg.tool_call_id.map(String::from),
            tool_name: msg.tool_name.map(String::from),
            tool_calls: msg.tool_calls.and_then(|j| serde_json::from_str(j).ok()),
            reasoning: msg.reasoning.map(String::from),
            media: msg
                .media
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default(),
            created_at: ts,
            context: msg.context,
        };
        self.jsonl.append_message(session, &msg_obj)?;
        if let Some(mut meta) = self.jsonl.read_meta(session)? {
            meta.updated_at = ts;
            self.jsonl.write_meta(&meta)?;
        }

        Ok(inserted_id)
    }

    /// Oldest first — the order the loop replays them in.
    pub fn history(&self, session: i64) -> Result<Vec<Message>> {
        Ok(std::sync::Arc::unwrap_or_clone(
            self.history_shared(session)?,
        ))
    }

    /// [`Self::history`] without a copy, for the turn that reads it every round.
    pub fn history_shared(&self, session: i64) -> Result<std::sync::Arc<Vec<Message>>> {
        let exists: bool = self
            .conn()
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(std::sync::Arc::default());
        }

        // An unreadable transcript is an error, never an empty conversation.
        self.jsonl.read_messages_shared(session)
    }

    /// Remove a session and everything hanging off it. The foreign keys carry
    /// messages, tool calls and checkpoint manifests with it; the blobs those
    /// manifests referenced are collected separately, because another session of
    /// the same project may still need them.
    pub fn delete_session(&self, id: i64) -> Result<bool> {
        let _ = self.jsonl.delete_session(id);
        let n = self
            .conn()
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Delete a session only if no messages have ever been posted to it.
    /// Returns true if the empty session was deleted.
    ///
    /// Text somebody typed into it and never sent moves to the project's
    /// drafts first: cleaning up an empty session never loses what a person
    /// wrote.
    pub fn delete_if_empty(&self, id: i64) -> Result<bool> {
        let lease = super::run_files::lease(self.jsonl.root(), id);
        let _lease = match crate::lock::exclusive(&lease, crate::lock::Wait::No) {
            Ok(file) => file,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(e),
        };
        let socket = self.jsonl.session_socket_path(id);
        let _host = match crate::lock::exclusive(
            &super::run_files::host_lock(self.jsonl.root(), id),
            crate::lock::Wait::No,
        ) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(false)
            }
            Err(error) => return Err(error),
        };
        if crate::platform::LocalStream::connect(&socket).is_ok()
            || self.has_any_message(id)
            || self
                .jsonl
                .composer(id)?
                .is_some_and(|composer| !composer.pending.is_empty())
        {
            return Ok(false);
        }
        if let Some(composer) = self
            .jsonl
            .composer(id)?
            .filter(|composer| composer.has_user_state())
        {
            let row = self.get_session(id)?;
            if let Ok(project) = crate::platform::canonical(std::path::Path::new(&row.project)) {
                let settings = self
                    .settings_json(id)?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default();
                let drafts = super::drafts::Drafts::new(self.jsonl.root());
                let draft = drafts.open(
                    &project,
                    super::composer::Composer {
                        settings,
                        mode: row.mode,
                        ..Default::default()
                    },
                    Some(format!("session-{id}")),
                )?;
                drafts.save(&draft, draft.composer.revision, composer)?;
            }
        }
        let n = self
            .conn()
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        if n > 0 {
            self.jsonl.delete_session(id)?;
            // Both its locks are held here: nothing will open them again.
            super::run_files::forget_held(self.jsonl.root(), id);
        }
        Ok(n > 0)
    }

    /// Every session the store files, by id — what `run/` may still hold
    /// files for (`run_files::sweep`).
    pub fn session_ids(&self) -> Result<std::collections::HashSet<i64>> {
        let mut st = self.conn().prepare("SELECT id FROM sessions")?;
        let ids = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// Sweep every empty session across all projects, except the ones named
    /// (the sessions this engine has open).
    pub fn prune_empty_sessions_except(&self, exclude: &[i64]) -> Result<usize> {
        let mut st = self.conn().prepare("SELECT id FROM sessions")?;
        let ids: Vec<i64> = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut removed = 0;
        for id in ids {
            if !exclude.contains(&id) && self.delete_if_empty(id)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn begin_toolcall(
        &self,
        id: &str,
        session: i64,
        turn: i64,
        name: &str,
        args: &str,
    ) -> Result<()> {
        // JSONL-only: a pending call is runtime state, held in memory until it ends.
        // Nothing is written to the audit trail yet — a call is not history until it
        // has an outcome.
        let pending = crate::store::PendingCall {
            turn,
            name: name.to_string(),
            args: args.to_string(),
            started_at: now_ms(),
        };
        self.pending_toolcalls
            .lock()
            .map_err(|_| Error::invalid("toolcall pending lock poisoned"))?
            .insert((session, id.to_string()), pending);
        Ok(())
    }

    pub fn end_toolcall(
        &self,
        session: i64,
        id: &str,
        ok: bool,
        status: Option<&str>,
        code: Option<&str>,
        summary: &str,
    ) -> Result<()> {
        let ended = now_ms();
        // Complete the pending call from memory and append the finished record to the
        // per-session JSONL audit trail. A missing pending entry (e.g. a call begun
        // before this refactor, or after a restart) is tolerated: the call is still
        // recorded with the data end_toolcall has, rather than dropped.
        let pending = self
            .pending_toolcalls
            .lock()
            .map_err(|_| Error::invalid("toolcall pending lock poisoned"))?
            .remove(&(session, id.to_string()));
        let (turn, name, action, started_at) = match pending {
            Some(p) => (p.turn, p.name, action_of(&p.args), p.started_at),
            None => (0, String::new(), String::new(), ended),
        };
        let call = ToolCall {
            id: id.to_string(),
            turn,
            name,
            action,
            ok: Some(ok),
            status: status.map(str::to_string),
            code: code.map(str::to_string),
            summary: Some(summary.to_string()),
            started_at,
            ended_at: Some(ended),
        };
        self.jsonl.append_toolcall(session, &call)
    }

    /// The newest `limit` tool calls of a session, newest first.
    ///
    /// Newest first because the question this answers is "what did it just do";
    /// bounded because a long session's trail is unbounded and a frontend asking
    /// for it should not be handed a hundred megabytes. Callers pair it with
    /// [`Db::count_toolcalls`] to say "showing 200 of 3,412".
    pub fn toolcalls(&self, session: i64, limit: usize) -> Result<Vec<ToolCall>> {
        // Newest first, bounded, from the session's own audit file. started_at
        // can tie within one millisecond and a wire id can repeat, so the append
        // order (the sequence the calls were recorded) is the final tie-breaker —
        // the most recently recorded call wins. That keeps the listing
        // deterministic no matter how fast the calls land.
        let calls = self.jsonl.read_toolcalls(session)?;
        let mut v: Vec<(usize, ToolCall)> = calls.into_iter().enumerate().collect();
        v.sort_by(|(ai, a), (bi, b)| {
            b.started_at
                .cmp(&a.started_at)
                .then(b.id.cmp(&a.id))
                .then(bi.cmp(ai))
        });
        let mut v: Vec<ToolCall> = v.into_iter().map(|(_, c)| c).collect();
        v.truncate(limit);
        Ok(v)
    }

    pub fn count_toolcalls(&self, session: i64) -> Result<i64> {
        Ok(self.jsonl.read_toolcalls(session)?.len() as i64)
    }

    /// Highest turn number reached in this session, or 0 if no messages exist.
    pub fn max_turn(&self, session: i64) -> Result<i64> {
        Ok(self.next_turn(session)? - 1)
    }

    /// Total assistant messages posted in this session.
    pub fn count_assistant_messages(&self, session: i64) -> Result<i64> {
        Ok(self
            .jsonl
            .read_messages_shared(session)?
            .iter()
            .filter(|m| matches!(m.role, Role::Assistant))
            .count() as i64)
    }
}

/// One line, bounded, printable, single-spaced — what a session label may be.
///
/// One home for the rule, because a title set from a prompt, by a model, or by
/// hand must obey the same one: a list that mixes 80-character labels with
/// multi-line ones is a list that cannot be rendered.
///
/// Printable because a title is text every frontend prints as it is, and one
/// written by a model that read untrusted text can be steered by that text. No
/// prompt makes a model immune to that, so the label is what is held: no
/// control character (an escape sequence would move the cursor or recolour the
/// screen) and no invisible or direction-reversing character (which would make
/// the label read as something it is not). What remains is plain words.
pub fn title_of(raw: &str) -> String {
    let line = raw.lines().next().unwrap_or("");
    let printable: String = line
        .chars()
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    printable
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

/// Zero-width and bidirectional-override characters: they occupy no column or
/// reorder the ones around them, so a label holding one does not read as what
/// it contains.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Pull the `action` line out of stored arguments.
///
/// Stored arguments are whatever the model sent. A model that skipped `action`,
/// or sent something that is not an object, is not an error worth failing a
/// listing over — the row still says which tool ran and how it ended.
pub(crate) fn action_of(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        assert_eq!(db.get_session(s.id).unwrap().project, "/tmp/proj");
        assert!(db.get_session(999).is_err());
        assert_eq!(db.list_sessions(Some("/tmp/proj"), 10).unwrap().len(), 1);
        assert_eq!(db.list_sessions(Some("/tmp/other"), 10).unwrap().len(), 0);
        assert_eq!(db.list_sessions(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn turns_increment_and_history_keeps_order() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        assert_eq!(db.next_turn(s.id).unwrap(), 1);

        db.append_message(s.id, NewMessage::plain(1, Role::User, "hi"))
            .unwrap();
        db.append_message(
            s.id,
            NewMessage {
                tool_call_id: Some("t1"),
                tool_name: Some("inspect_file"),
                ..NewMessage::plain(1, Role::Tool, "{}")
            },
        )
        .unwrap();

        assert_eq!(db.next_turn(s.id).unwrap(), 2);
        let h = db.history(s.id).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[1].tool_call_id.as_deref(), Some("t1"));
    }

    #[test]
    fn an_assistant_turn_keeps_its_tool_calls_and_reasoning() {
        // The regression this guards: history that stored only the text, so the
        // next request carried a tool result answering nothing.
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.append_message(
            s.id,
            NewMessage {
                tool_calls: Some(r#"[{"id":"c1","name":"create_file","args":"{}"}]"#),
                reasoning: Some("thought about it"),
                ..NewMessage::plain(1, Role::Assistant, "")
            },
        )
        .unwrap();
        let h = db.history(s.id).unwrap();
        assert_eq!(h[0].tool_calls.as_ref().unwrap()[0]["name"], "create_file");
        assert_eq!(h[0].reasoning.as_deref(), Some("thought about it"));
    }

    #[test]
    fn an_error_message_is_stored_and_retrieved_with_role_error() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.append_message(s.id, NewMessage::plain(1, Role::User, "tes"))
            .unwrap();
        db.append_message(
            s.id,
            NewMessage::plain(1, Role::Error, "provider: HTTP 400: invalid model ID"),
        )
        .unwrap();

        let h = db.history(s.id).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[0].content, "tes");
        assert_eq!(h[1].role, Role::Error);
        assert_eq!(h[1].content, "provider: HTTP 400: invalid model ID");
    }

    #[test]
    fn settings_and_compactions_round_trip_per_session() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        assert!(
            db.settings_json(s.id).unwrap().is_none(),
            "a new session has chosen nothing"
        );
        db.set_settings_json(s.id, r#"{"model":"m"}"#).unwrap();
        assert_eq!(
            db.settings_json(s.id).unwrap().as_deref(),
            Some(r#"{"model":"m"}"#)
        );

        assert!(db.compaction(s.id).unwrap().is_none());
        let record = |through_id, summary: &str| crate::store::jsonl::CompactionRecord {
            through_id,
            summary: summary.into(),
            created_at: 0,
            context: String::new(),
            status: Default::default(),
            sources: Default::default(),
        };
        db.add_compaction(s.id, &record(10, "older summary"))
            .unwrap();
        db.add_compaction(s.id, &record(42, "newer summary"))
            .unwrap();
        let newest = db.compaction(s.id).unwrap().unwrap();
        assert_eq!(
            (newest.through_id, newest.summary.as_str()),
            (42, "newer summary")
        );

        let s2 = db.create_session("/tmp/p").unwrap();
        db.set_settings_json(s2.id, r#"{"model":"m2"}"#).unwrap();
        let recents = db.recent_settings_json(10).unwrap();
        assert_eq!(recents.len(), 2);
        assert_eq!(
            recents[0], r#"{"model":"m2"}"#,
            "newest session settings first"
        );
        assert_eq!(recents[1], r#"{"model":"m"}"#);
    }

    #[test]
    fn a_new_session_starts_in_the_safe_mode_and_remembers_a_change() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        assert_eq!(s.mode, "ask");
        db.set_mode(s.id, "auto").unwrap();
        assert_eq!(db.get_session(s.id).unwrap().mode, "auto");
        assert_eq!(db.list_sessions(None, 5).unwrap()[0].mode, "auto");
    }

    /// A title can come from a model that read untrusted text. Whatever it
    /// carries, what is stored is plain words a frontend can print as they are:
    /// no escape sequence, no control character, no invisible or
    /// direction-reversing character.
    #[test]
    fn a_stored_title_is_plain_printable_text() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.rename_session(
            s.id,
            "\x1b[2J\x1b[31mClear\tthe\u{202E}screen\u{200B}now\x07  ",
        )
        .unwrap();
        let title = db.get_session(s.id).unwrap().title.unwrap();
        assert_eq!(title, "[2J [31mClear the screen now");
        assert!(!title
            .chars()
            .any(|c| c.is_control() || super::is_invisible(c)));
        assert!(
            db.rename_session(s.id, "\x1b\u{202E}\t").is_err(),
            "nothing printable left"
        );
    }

    #[test]
    fn title_is_set_once() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.set_title_if_unset(s.id, "first prompt\nsecond line")
            .unwrap();
        db.set_title_if_unset(s.id, "later prompt").unwrap();
        assert_eq!(
            db.get_session(s.id).unwrap().title.as_deref(),
            Some("first prompt")
        );
    }

    #[test]
    fn a_session_can_be_renamed_after_it_was_named_by_its_first_prompt() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        db.set_title_if_unset(s.id, "fix the login bug").unwrap();

        assert!(db
            .rename_session(s.id, "  auth rewrite\nignored second line  ")
            .unwrap());
        assert_eq!(
            db.get_session(s.id).unwrap().title.as_deref(),
            Some("auth rewrite")
        );

        // The automatic namer stays hands-off afterwards.
        db.set_title_if_unset(s.id, "some later prompt").unwrap();
        assert_eq!(
            db.get_session(s.id).unwrap().title.as_deref(),
            Some("auth rewrite")
        );

        assert!(
            db.rename_session(s.id, "   ").is_err(),
            "an empty title is not a title"
        );
        assert!(
            !db.rename_session(9999, "ghost").unwrap(),
            "an unknown session renames nothing"
        );
    }

    #[test]
    fn a_title_is_one_line_of_at_most_eighty_characters_however_it_was_set() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        let long = "x".repeat(200);
        db.rename_session(s.id, &long).unwrap();
        assert_eq!(
            db.get_session(s.id).unwrap().title.unwrap().chars().count(),
            80
        );
    }

    #[test]
    fn toolcalls_come_back_newest_first_bounded_and_counted() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        for n in 1..=5 {
            db.begin_toolcall(
                &format!("t{n}"),
                s.id,
                n,
                "find_files",
                &format!("{{\"action\":\"step {n}\",\"queries\":[]}}"),
            )
            .unwrap();
            db.end_toolcall(
                s.id,
                &format!("t{n}"),
                n % 2 == 1,
                None,
                None,
                &format!("did {n}"),
            )
            .unwrap();
        }

        let all = db.toolcalls(s.id, 100).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].turn, 5, "newest first");
        assert_eq!(all[0].action, "step 5");
        assert_eq!(all[0].ok, Some(true));
        assert_eq!(all[1].ok, Some(false));
        assert_eq!(all[0].summary.as_deref(), Some("did 5"));
        assert!(all[0].ended_at.is_some());

        assert_eq!(
            db.toolcalls(s.id, 2).unwrap().len(),
            2,
            "the limit is honoured"
        );
        assert_eq!(db.count_toolcalls(s.id).unwrap(), 5);
        assert_eq!(
            db.toolcalls(999, 10).unwrap().len(),
            0,
            "an unknown session is empty"
        );
    }

    #[test]
    fn a_call_still_running_reports_no_outcome_yet() {
        // With the audit trail in JSONL, a pending call is runtime state, not history:
        // it does not appear in the listing until it ends. This is the correct
        // semantics — a call with no outcome yet is not a record of anything.
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        db.begin_toolcall("t1", s.id, 1, "terminal_use", "{\"action\":\"build\"}")
            .unwrap();
        assert!(
            db.toolcalls(s.id, 10).unwrap().is_empty(),
            "a call still running is not yet in the historical audit trail"
        );
        // Once it ends, it is recorded with its outcome and parsed action.
        db.end_toolcall(s.id, "t1", true, None, None, "built")
            .unwrap();
        let row = &db.toolcalls(s.id, 10).unwrap()[0];
        assert_eq!(row.ok, Some(true));
        assert!(row.ended_at.is_some());
        assert_eq!(row.action, "build");
    }

    #[test]
    fn arguments_the_engine_cannot_parse_do_not_break_the_listing() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/proj").unwrap();
        db.begin_toolcall("t1", s.id, 1, "find_files", "not json at all")
            .unwrap();
        db.begin_toolcall("t2", s.id, 1, "find_files", "{\"queries\":[]}")
            .unwrap();
        db.end_toolcall(s.id, "t1", true, None, None, "done")
            .unwrap();
        db.end_toolcall(s.id, "t2", true, None, None, "done")
            .unwrap();
        let rows = db.toolcalls(s.id, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.action.is_empty()));
    }

    #[test]
    fn toolcall_records_outcome() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.begin_toolcall("t1", s.id, 1, "terminal_use", "{}")
            .unwrap();
        db.end_toolcall(s.id, "t1", false, Some("failed"), Some("io"), "exit 1")
            .unwrap();
        // The audit trail is the per-session JSONL, not the SQL table.
        let row = db
            .toolcalls(s.id, 10)
            .unwrap()
            .into_iter()
            .find(|c| c.id == "t1")
            .expect("the finished call is in the JSONL audit trail");
        assert_eq!(row.ok, Some(false));
        assert_eq!(row.summary.as_deref(), Some("exit 1"));
    }

    #[test]
    fn a_tool_audit_write_failure_is_not_reported_as_success() {
        let db = Db::memory().unwrap();
        let session = db.create_session("/tmp/audit-failure-fixture").unwrap();
        let path = db.jsonl.session_dir(session.id).join("toolcalls.jsonl");
        std::fs::create_dir_all(&path).unwrap();
        db.begin_toolcall("p", session.id, 1, "patch_file", "{}")
            .unwrap();
        assert!(db
            .end_toolcall(
                session.id,
                "p",
                false,
                Some("failed"),
                Some("tool"),
                "failed"
            )
            .is_err());
    }

    #[test]
    fn delete_session_reports_whether_it_removed_anything() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        assert!(db.delete_session(s.id).unwrap());
        assert!(!db.delete_session(s.id).unwrap());
        assert!(db.get_session(s.id).is_err());
    }

    #[test]
    fn deleting_a_session_takes_its_messages() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.append_message(s.id, NewMessage::plain(1, Role::User, "x"))
            .unwrap();
        db.conn()
            .execute("DELETE FROM sessions WHERE id = ?1", params![s.id])
            .unwrap();
        assert_eq!(db.history(s.id).unwrap().len(), 0);
    }

    #[test]
    fn delete_if_empty_only_deletes_unmessaged_sessions() {
        let db = Db::memory().unwrap();
        let empty = db.create_session("/tmp/p").unwrap();
        let filled = db.create_session("/tmp/p").unwrap();
        db.append_message(filled.id, NewMessage::plain(1, Role::User, "hi"))
            .unwrap();

        assert!(
            db.delete_if_empty(empty.id).unwrap(),
            "empty session is deleted"
        );
        assert!(
            !db.delete_if_empty(filled.id).unwrap(),
            "non-empty session is not deleted"
        );
        assert!(db.get_session(empty.id).is_err());
        assert!(db.get_session(filled.id).is_ok());
    }

    /// Unsent text in an empty session moves to the project's drafts when the
    /// session goes; a session open anywhere is never touched.
    #[test]
    fn an_empty_sessions_unsent_text_moves_to_the_drafts() {
        let db = Db::memory().unwrap();
        let project = db.jsonl.root().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let row = db.create_session(project.to_str().unwrap()).unwrap();
        db.jsonl
            .write_composer(
                row.id,
                &super::super::composer::Composer {
                    text: "unsent text".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        db.lease_session(row.id).unwrap();
        assert!(!db.delete_if_empty(row.id).unwrap());
        db.session_leases.lock().unwrap().remove(&row.id);
        let dir = db.jsonl.session_dir(row.id);
        assert!(db.delete_if_empty(row.id).unwrap());
        assert!(db.get_session(row.id).is_err());
        assert!(!dir.exists(), "nothing of the session is kept but its text");
        let draft = super::super::drafts::Drafts::new(db.jsonl.root())
            .open(&project, Default::default(), None)
            .unwrap();
        assert_eq!(draft.composer.text, "unsent text");
    }

    #[test]
    fn prune_empty_sessions_cleans_only_zero_message_rows() {
        let db = Db::memory().unwrap();
        let e1 = db.create_session("/tmp/p1").unwrap();
        let e2 = db.create_session("/tmp/p1").unwrap();
        let f1 = db.create_session("/tmp/p1").unwrap();
        db.append_message(f1.id, NewMessage::plain(1, Role::User, "work"))
            .unwrap();

        let pruned = db.prune_empty_sessions_except(&[e2.id]).unwrap();
        assert_eq!(pruned, 1);
        assert!(db.get_session(e1.id).is_err());
        assert!(db.get_session(e2.id).is_ok(), "an open session is excluded");
        assert!(db.get_session(f1.id).is_ok());
    }
    #[test]
    fn foreign_engine_lease_survives_list_pruning_and_resume() {
        let root = std::env::temp_dir().join(format!("RuneLease{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("rune.db");
        let owner = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        let first = owner.create_session("/fixture").unwrap();
        owner.lease_session(first.id).unwrap();
        let newest = owner.create_session("/fixture").unwrap();
        let reader = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(reader.prune_empty_sessions_except(&[]).unwrap(), 1);
        assert!(reader.get_session(first.id).is_ok(), "its lease keeps it");
        assert!(reader.get_session(newest.id).is_err());
        drop(owner);
        assert_eq!(reader.prune_empty_sessions_except(&[]).unwrap(), 1);
        assert!(reader.get_session(first.id).is_err());
        drop(reader);
        std::fs::remove_dir_all(root).unwrap();
    }
}
