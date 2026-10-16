//! JSONL and per-session directory storage — the codex-style source of truth.
//!
//! Structure:
//! ~/.rune/
//!   └── projects/
//!         └── {project folder}/          (see `crate::project`)
//!               └── sessions/
//!                     └── {session_id}/
//!                           ├── meta.json
//!                           ├── session.jsonl
//!                           ├── logs.jsonl
//!                           └── tools_result/
//!                                 └── {tool_name}/
//!                                       └── {id}.txt
//!
//! A session lives in the folder of the project it belongs to, so a project's
//! data is one folder. There is no central `sessions_index.json`: the
//! per-session `meta.json` files are the index, derived on read by
//! [`JsonlStore::read_all_meta`]. One corrupt session can never take down the
//! list, and the index can never drift from the per-session truth.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::sessions::{Message, Session, ToolCall};
use super::tasks::Task;
use crate::error::Result;

/// A session's composer — what is typed in its windows, shared between them.
pub const COMPOSER: &str = "composer.json";

/// A whole-file record as a person reads it: indented, ending in a newline.
/// The `.jsonl` files keep one record per line; a file Rune writes whole
/// (`meta.json`, `composer.json`, the drafts) is one object laid out.
pub fn pretty(value: &impl Serialize) -> crate::error::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// One compaction: a summary standing in for every message up to and including
/// `through_id`, and the context the model is handed back beside it.
///
/// `context` is read from the world once, when the summary is written, and
/// stored: every request after the compaction replays the same bytes, so the
/// prefix it starts stays cacheable. A record written before this field existed
/// has it empty; the session's next turn then opens with fresh context instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub through_id: i64,
    pub summary: String,
    pub created_at: i64,
    #[serde(default)]
    pub context: String,
    /// The status values `context` told the model, by name.
    #[serde(default)]
    pub status: std::collections::BTreeMap<String, String>,
    /// The fingerprints of the file-born parts of `context`, by name.
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, String>,
}

/// One history attachment dropped for the payload budget: the `index`-th file
/// of message `message`. Permanent — see `media::evictions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaEviction {
    pub message: i64,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: i64,
    pub project: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub mode: String,
    pub active_seconds: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub settings: Option<String>,
}

impl From<&Session> for SessionMeta {
    fn from(s: &Session) -> Self {
        SessionMeta {
            id: s.id,
            project: s.project.clone(),
            title: s.title.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            mode: s.mode.clone(),
            active_seconds: s.active_seconds,
            model: s.model.clone(),
            settings: None,
        }
    }
}

impl From<SessionMeta> for Session {
    fn from(m: SessionMeta) -> Self {
        Session {
            id: m.id,
            project: m.project,
            title: m.title,
            created_at: m.created_at,
            updated_at: m.updated_at,
            mode: m.mode,
            active_seconds: m.active_seconds,
            model: m.model,
            size_bytes: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JsonlStore {
    root: PathBuf,
    /// Parsed transcripts, shared by every clone of this store.
    transcripts: Arc<Mutex<Transcripts>>,
    /// Where each session seen so far lives. A session another process made is
    /// found by looking ([`JsonlStore::locate`]) and remembered after.
    dirs: Arc<Mutex<HashMap<i64, PathBuf>>>,
}

/// The project folder a session no project folder holds would sit in. Never
/// created: a read there finds nothing, and a write is refused.
const UNFILED: &str = ".unfiled";

/// How many sessions keep a parsed transcript. The turn that is running reads
/// its own on every round; a host serving more windows than this re-parses the
/// least recent one from disk, which is only as slow as before the cache.
const CACHED_TRANSCRIPTS: usize = 8;

/// The parsed `session.jsonl` of recently read sessions.
///
/// The file is append-only — every writer appends a line, or writes the whole
/// file atomically while it is still empty — so a later read parses only the
/// bytes written since the last one. A turn reads its transcript every round,
/// and re-parsing the whole of it each time grew with the session (measured:
/// 13 ms a round at 400 messages, twice per round). A file that was replaced
/// (another inode) or is shorter than what was parsed is read again in full.
#[derive(Debug, Default)]
struct Transcripts {
    tick: u64,
    by_session: HashMap<i64, Transcript>,
}

#[derive(Debug, Default)]
struct Transcript {
    /// Which file was read: an atomic replace is a new file.
    file: Option<crate::platform::FileIdentity>,
    offset: u64,
    /// Shared with readers: a reader holding the previous round's transcript
    /// costs nothing, and once it is dropped the next append extends this in
    /// place instead of copying it.
    messages: Arc<Vec<Message>>,
    used: u64,
}

impl JsonlStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            transcripts: Arc::default(),
            dirs: Arc::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// File `session_id` under `project`'s folder (creating both as needed),
    /// and remember where. A session is filed before its first write.
    pub fn register(&self, session_id: i64, project: &str) -> Result<PathBuf> {
        if let Some(dir) = self.locate(session_id) {
            return Ok(dir);
        }
        let folder = crate::project::open(&self.root, Path::new(project))?;
        let dir = folder.join("sessions").join(session_id.to_string());
        fs::create_dir_all(&dir)?;
        crate::panic::lock(&self.dirs).insert(session_id, dir.clone());
        Ok(dir)
    }

    /// Where `session_id` lives, if any project folder holds it.
    fn locate(&self, session_id: i64) -> Option<PathBuf> {
        if let Some(dir) = crate::panic::lock(&self.dirs).get(&session_id) {
            return Some(dir.clone());
        }
        let found = fs::read_dir(self.root.join("projects"))
            .ok()?
            .flatten()
            .map(|entry| entry.path().join("sessions").join(session_id.to_string()))
            .find(|dir| dir.is_dir())?;
        crate::panic::lock(&self.dirs).insert(session_id, found.clone());
        Some(found)
    }

    /// Where a session's files live: inside its project's folder. A session no
    /// project folder holds answers with a path that does not exist, so a read
    /// there finds nothing and [`Self::ensure_session_dir`] refuses to write.
    pub fn session_dir(&self, session_id: i64) -> PathBuf {
        self.locate(session_id).unwrap_or_else(|| {
            self.root
                .join("projects")
                .join(UNFILED)
                .join("sessions")
                .join(session_id.to_string())
        })
    }

    pub fn session_socket_path(&self, session_id: i64) -> PathBuf {
        super::run_files::socket(&self.root, session_id)
    }

    pub fn tools_result_dir(&self, session_id: i64) -> PathBuf {
        self.session_dir(session_id).join("tools_result")
    }

    pub fn ensure_session_dir(&self, session_id: i64) -> Result<PathBuf> {
        let dir = self.locate(session_id).ok_or_else(|| {
            crate::error::Error::NotFound(format!(
                "session {session_id} is not filed under a project"
            ))
        })?;
        fs::create_dir_all(dir.join("tools_result"))?;
        Ok(dir)
    }

    pub fn write_meta(&self, meta: &SessionMeta) -> Result<()> {
        self.register(meta.id, &meta.project)?;
        let dir = self.ensure_session_dir(meta.id)?;
        let path = dir.join("meta.json");
        let content = serde_json::to_string_pretty(meta)?;
        crate::settings::secrets::atomic_write(&path, content.as_bytes())?;
        // The per-session meta.json is the whole record; the index is derived by
        // read_all_meta, so there is no separate index to keep in sync here.
        Ok(())
    }

    pub fn read_meta(&self, session_id: i64) -> Result<Option<SessionMeta>> {
        let path = self.session_dir(session_id).join("meta.json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(path)?;
        let meta: SessionMeta = serde_json::from_str(&data)?;
        Ok(Some(meta))
    }

    /// A per-session snapshot, not an append-only edit log; the host
    /// serializes composer writes.
    pub fn composer(&self, session_id: i64) -> Result<Option<super::composer::Composer>> {
        let path = self.session_dir(session_id).join(COMPOSER);
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write_composer(
        &self,
        session_id: i64,
        composer: &super::composer::Composer,
    ) -> Result<()> {
        let dir = self.ensure_session_dir(session_id)?;
        crate::settings::secrets::atomic_write(&dir.join(COMPOSER), &pretty(composer)?)
    }

    pub fn append_message(&self, session_id: i64, message: &Message) -> Result<()> {
        self.append_line(
            session_id,
            "session.jsonl",
            &serde_json::to_string(message)?,
        )
    }

    /// Append one JSON line to a per-session log, creating the session dir as needed.
    ///
    /// The record and its newline go out in one write: `writeln!` on a `File`
    /// writes them in two, and a crash between the two left a record with no
    /// newline, onto which the next append glued its own — one line no reader
    /// could parse, so both records vanished for every reader that came after.
    /// For the same reason a file that does not end in a newline (a record a
    /// crash cut short) is closed off first: what follows starts a line of its
    /// own, and only the record that was never whole is lost.
    fn append_line(&self, session_id: i64, file: &str, line: &str) -> Result<()> {
        let dir = self.ensure_session_dir(session_id)?;
        let path = dir.join(file);
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut record = Vec::with_capacity(line.len() + 2);
        if !ends_a_line(&mut f)? {
            record.push(b'\n');
        }
        record.extend_from_slice(line.as_bytes());
        record.push(b'\n');
        f.write_all(&record)?;
        f.sync_all()?;
        crate::platform::sync_dir(&dir)?;
        Ok(())
    }

    /// Read every parseable line of a per-session JSONL file. A line that fails to
    /// parse is skipped, never fatal: one corrupt record must not take down the rest
    /// of the session's history.
    fn read_lines<T: for<'de> Deserialize<'de>>(
        &self,
        session_id: i64,
        file: &str,
    ) -> Result<Vec<T>> {
        let path = self.session_dir(session_id).join(file);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(path)?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<T>(&line) {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Every message of the session, in the order written, as an owned copy.
    pub fn read_messages(&self, session_id: i64) -> Result<Vec<Message>> {
        Ok(Arc::unwrap_or_clone(self.read_messages_shared(session_id)?))
    }

    /// Every message of the session, in the order written, without a copy.
    /// Parses only what was appended since the last read (see [`Transcripts`]);
    /// one unparseable line is skipped, never fatal, exactly as
    /// [`Self::read_lines`] does.
    pub fn read_messages_shared(&self, session_id: i64) -> Result<Arc<Vec<Message>>> {
        let path = self.session_dir(session_id).join("session.jsonl");
        let mut cache = crate::panic::lock(&self.transcripts);
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cache.by_session.remove(&session_id);
                return Ok(Arc::default());
            }
            Err(e) => return Err(e.into()),
        };
        cache.tick += 1;
        let tick = cache.tick;
        let entry = cache.by_session.entry(session_id).or_default();
        let file = crate::platform::FileIdentity::of_path(&path).ok();
        if entry.file != file || meta.len() < entry.offset {
            *entry = Transcript {
                file,
                ..Transcript::default()
            };
        }
        entry.used = tick;
        if meta.len() > entry.offset {
            let mut f = File::open(&path)?;
            f.seek(SeekFrom::Start(entry.offset))?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            entry.offset += parse_appended(&bytes, Arc::make_mut(&mut entry.messages)) as u64;
        }
        let messages = Arc::clone(&entry.messages);
        if cache.by_session.len() > CACHED_TRANSCRIPTS {
            if let Some(oldest) = cache
                .by_session
                .iter()
                .min_by_key(|(_, t)| t.used)
                .map(|(id, _)| *id)
            {
                cache.by_session.remove(&oldest);
            }
        }
        Ok(messages)
    }

    // ── Tool calls (audit trail per session) ────────────────────────────────

    pub fn append_toolcall(&self, session_id: i64, call: &ToolCall) -> Result<()> {
        let line = serde_json::to_string(call)?;
        self.append_line(session_id, "toolcalls.jsonl", &line)
    }

    pub fn read_toolcalls(&self, session_id: i64) -> Result<Vec<ToolCall>> {
        self.read_lines(session_id, "toolcalls.jsonl")
    }

    // ── The session's log (`/logs`) ──────────────────────────────────────────

    /// A file of its own, never `session.jsonl`: the turn reads the transcript
    /// every round, and a log line there would be parsed by every request.
    pub fn append_log(&self, session_id: i64, entry: &crate::proto::LogEntry) -> Result<()> {
        let line = serde_json::to_string(entry)?;
        self.append_line(session_id, "logs.jsonl", &line)
    }

    /// The newest `limit` lines of the log, oldest first.
    pub fn read_logs(&self, session_id: i64, limit: usize) -> Result<Vec<crate::proto::LogEntry>> {
        let mut all: Vec<crate::proto::LogEntry> = self.read_lines(session_id, "logs.jsonl")?;
        let skip = all.len().saturating_sub(limit);
        Ok(all.split_off(skip))
    }

    pub(crate) fn append_sanitizer(
        &self,
        session_id: i64,
        record: &crate::session::sanitizer::Record,
    ) -> Result<()> {
        self.append_line(
            session_id,
            "sanitizer.jsonl",
            &serde_json::to_string(record)?,
        )
    }

    pub(crate) fn read_sanitizer(
        &self,
        session_id: i64,
    ) -> Result<Vec<crate::session::sanitizer::Record>> {
        self.read_lines(session_id, "sanitizer.jsonl")
    }

    // ── Compactions (summary standing in for older history) ─────────────────

    pub fn append_compaction(&self, session_id: i64, record: &CompactionRecord) -> Result<()> {
        let line = serde_json::to_string(record)?;
        self.append_line(session_id, "compactions.jsonl", &line)
    }

    /// Newest compaction first, matching the DB's `ORDER BY through_id DESC`.
    pub fn read_compactions(&self, session_id: i64) -> Result<Vec<CompactionRecord>> {
        let mut out: Vec<CompactionRecord> = self.read_lines(session_id, "compactions.jsonl")?;
        out.sort_by_key(|b| std::cmp::Reverse(b.through_id));
        Ok(out)
    }

    // ── Background tasks (what the model was told of their ends) ────────────

    pub fn append_background(
        &self,
        session_id: i64,
        record: &crate::session::background::BackgroundRecord,
    ) -> Result<()> {
        let line = serde_json::to_string(record)?;
        self.append_line(session_id, "background.jsonl", &line)
    }

    pub fn read_background(
        &self,
        session_id: i64,
    ) -> Result<Vec<crate::session::background::BackgroundRecord>> {
        self.read_lines(session_id, "background.jsonl")
    }

    pub fn append_eviction(&self, session_id: i64, eviction: &MediaEviction) -> Result<()> {
        let line = serde_json::to_string(eviction)?;
        self.append_line(session_id, "media.jsonl", &line)
    }

    pub fn read_evictions(&self, session_id: i64) -> Result<Vec<MediaEviction>> {
        self.read_lines(session_id, "media.jsonl")
    }

    /// Where attachment snapshots of a session live — inside the session, so
    /// they go when it goes and no object-store sweep can collect them.
    pub fn attachments_dir(&self, session_id: i64) -> PathBuf {
        self.session_dir(session_id).join("attachments")
    }

    // ── Tasks (the plan; one list per session, replaced whole) ──────────────

    /// Replace a session's plan with `tasks`, in the order given — the same
    /// whole-list-replace semantics the DB version has (a rewrite, never a merge).
    pub fn write_tasks(&self, session_id: i64, tasks: &[Task]) -> Result<()> {
        let dir = self.ensure_session_dir(session_id)?;
        let path = dir.join("tasks.json");
        let content = serde_json::to_string_pretty(tasks)?;
        fs::write(path, content)?;
        Ok(())
    }

    pub fn read_tasks(&self, session_id: i64) -> Result<Vec<Task>> {
        let path = self.session_dir(session_id).join("tasks.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let data = fs::read_to_string(path)?;
        let mut tasks: Vec<Task> = serde_json::from_str(&data).unwrap_or_default();
        tasks.sort_by_key(|t| t.position);
        Ok(tasks)
    }

    /// Every session's metadata, read straight from the per-session `meta.json`
    /// files — newest first. This is the codex-style index: the per-session files
    /// ARE the index, so there is no separate `sessions_index.json` to drift out of
    /// sync or become a single point of failure.
    pub fn read_all_meta(&self) -> Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        for folder in crate::project::all(&self.root) {
            let Ok(entries) = fs::read_dir(folder.path.join("sessions")) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path().join("meta.json");
                if !path.is_file() {
                    continue;
                }
                if let Ok(data) = fs::read_to_string(&path) {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&data) {
                        crate::panic::lock(&self.dirs).insert(meta.id, entry.path());
                        out.push(meta);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    pub fn delete_session(&self, session_id: i64) -> Result<bool> {
        crate::panic::lock(&self.transcripts)
            .by_session
            .remove(&session_id);
        let dir = self.session_dir(session_id);
        crate::panic::lock(&self.dirs).remove(&session_id);
        let exists = dir.exists();
        if exists {
            let _ = fs::remove_dir_all(dir);
        }
        // No central index to update: removing the session's own directory is the
        // whole deletion. The next read_all_meta simply no longer sees it.
        Ok(exists)
    }

    pub fn session_size_bytes(&self, session_id: i64) -> u64 {
        let dir = self.session_dir(session_id);
        Self::dir_size_bytes(&dir)
    }

    pub fn dir_size_bytes(dir: &Path) -> u64 {
        let mut total: u64 = 0;
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = fs::symlink_metadata(entry.path()) {
                    if meta.is_file() {
                        total = total.saturating_add(meta.len());
                    } else if meta.is_dir() {
                        total = total.saturating_add(Self::dir_size_bytes(&entry.path()));
                    }
                }
            }
        }
        total
    }

    pub fn list_sessions(&self, project: Option<&str>, limit: usize) -> Result<Vec<Session>> {
        let filtered = self
            .read_all_meta()?
            .into_iter()
            .filter(|m| project.is_none() || Some(m.project.as_str()) == project)
            .take(limit)
            .map(|m| {
                let size_bytes = self.session_size_bytes(m.id);
                let mut s = Session::from(m);
                s.size_bytes = size_bytes;
                s
            })
            .collect();
        Ok(filtered)
    }
}

/// Whether `file` is empty or ends with a newline — whether a record appended
/// now starts a line of its own. Appends always land at the end whatever the
/// read position, so reading the last byte moves nothing a write depends on.
fn ends_a_line(file: &mut File) -> std::io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    let mut last = [0u8];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Parse the lines in `bytes` (what was appended since the last read) into
/// `out`, returning how many bytes were consumed.
///
/// Whole lines only: a line a concurrent writer has not finished is left for
/// the next read. A final line without its newline is taken when it is already
/// a complete record — a partial JSON object never parses — so a record whose
/// newline a crash lost is not hidden, and the next append starts after it.
fn parse_appended(bytes: &[u8], out: &mut Vec<Message>) -> usize {
    let mut consumed = 0;
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        let complete = line.ends_with(b"\n");
        let text = line.trim_ascii();
        if text.is_empty() {
            consumed += line.len();
            continue;
        }
        match serde_json::from_slice::<Message>(text) {
            Ok(message) => out.push(message),
            Err(_) if !complete => break,
            Err(_) => {}
        }
        consumed += line.len();
    }
    consumed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with sessions 1 and 2 filed under one project, as the engine
    /// files a session before its first write.
    fn store(tag: &str) -> (JsonlStore, PathBuf) {
        let root = std::env::temp_dir()
            .join("rune-jsonl")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("run")).unwrap();
        let store = JsonlStore::new(&root);
        store.register(1, "/fixture/project").unwrap();
        store.register(2, "/fixture/project").unwrap();
        (store, root)
    }

    fn message(id: i64, content: &str) -> Message {
        Message {
            work: None,
            continuation: None,
            id,
            turn: 1,
            role: super::super::sessions::Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            media: Vec::new(),
            created_at: 0,
            context: None,
        }
    }

    fn contents(store: &JsonlStore) -> Vec<String> {
        store
            .read_messages(1)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect()
    }

    fn raw_append(store: &JsonlStore, bytes: &[u8]) {
        let path = store.session_dir(1).join("session.jsonl");
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// A read after an append returns everything — including lines another
    /// process appended — while parsing only what is new.
    #[test]
    fn the_transcript_is_read_incrementally_and_misses_nothing() {
        let (store, root) = store("tail-append");
        store.append_message(1, &message(1, "a")).unwrap();
        assert_eq!(contents(&store), ["a"]);
        store.append_message(1, &message(2, "b")).unwrap();
        let line = serde_json::to_string(&message(3, "from another process")).unwrap();
        raw_append(&store, format!("{line}\n").as_bytes());
        assert_eq!(contents(&store), ["a", "b", "from another process"]);
        assert_eq!(contents(&store), ["a", "b", "from another process"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// A line still being written is not read until it is whole; a complete
    /// record whose newline was lost is read, and so is the next one after it.
    #[test]
    fn a_half_written_line_waits_and_a_lost_newline_hides_nothing() {
        let (store, root) = store("tail-partial");
        store.append_message(1, &message(1, "a")).unwrap();
        let line = serde_json::to_string(&message(2, "b")).unwrap();
        let (head, tail) = line.split_at(line.len() / 2);
        raw_append(&store, head.as_bytes());
        assert_eq!(contents(&store), ["a"]);
        raw_append(&store, tail.as_bytes());
        assert_eq!(
            contents(&store),
            ["a", "b"],
            "complete, newline not yet written"
        );
        store.append_message(1, &message(3, "c")).unwrap();
        assert_eq!(contents(&store), ["a", "b", "c"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// What a crash left at the end of a transcript costs at most the record it
    /// cut short — for every reader, including one that never saw the file
    /// before (another process, the next start), not only one whose cache had
    /// already stepped past the damage.
    #[test]
    fn a_crash_mid_append_costs_no_record_written_after_it() {
        let (store, root) = store("tail-crash");
        store.append_message(1, &message(1, "a")).unwrap();
        // Whole, but its newline never written.
        raw_append(
            &store,
            serde_json::to_string(&message(2, "b")).unwrap().as_bytes(),
        );
        store.append_message(1, &message(3, "c")).unwrap();
        // Cut short half way.
        let line = serde_json::to_string(&message(4, "d")).unwrap();
        raw_append(&store, &line.as_bytes()[..line.len() / 2]);
        store.append_message(1, &message(5, "e")).unwrap();
        let fresh = JsonlStore::new(&root);
        assert_eq!(contents(&fresh), ["a", "b", "c", "e"]);
        assert_eq!(contents(&store), ["a", "b", "c", "e"]);
        let _ = fs::remove_dir_all(&root);
    }

    /// A transcript that was replaced or shortened is read again from the start,
    /// never stitched onto what an older file held.
    #[test]
    fn a_replaced_or_shorter_transcript_is_read_again_in_full() {
        let (store, root) = store("tail-replace");
        store.append_message(1, &message(1, "old one")).unwrap();
        store.append_message(1, &message(2, "old two")).unwrap();
        assert_eq!(contents(&store).len(), 2);
        let path = store.session_dir(1).join("session.jsonl");
        let fresh = format!("{}\n", serde_json::to_string(&message(9, "fresh")).unwrap());
        crate::settings::secrets::atomic_write(&path, fresh.as_bytes()).unwrap();
        assert_eq!(contents(&store), ["fresh"], "replaced: another inode");
        fs::write(&path, b"").unwrap();
        assert!(contents(&store).is_empty(), "shortened in place");
        fs::remove_file(&path).unwrap();
        assert!(contents(&store).is_empty(), "gone");
        let _ = fs::remove_dir_all(&root);
    }

    fn meta(id: i64, project: &str, updated: i64) -> SessionMeta {
        SessionMeta {
            id,
            project: project.into(),
            title: Some(format!("session {id}")),
            created_at: updated - 1000,
            updated_at: updated,
            mode: "full".into(),
            active_seconds: 0,
            model: Some("m".into()),
            settings: None,
        }
    }

    /// The codex-style contract: the session list is derived from the per-session
    /// meta.json files alone — there is NO sessions_index.json to read, and the
    /// list is complete even though no central index was ever written.
    #[test]
    fn the_session_list_is_derived_from_meta_files_with_no_central_index() {
        let (s, root) = store("derive");
        for (id, updated) in [(1, 100), (2, 300), (3, 200)] {
            s.write_meta(&meta(id, "/p", updated)).unwrap();
        }

        // No central index file was created anywhere.
        assert!(
            !root.join("sessions_index.json").exists(),
            "a central index must never be written"
        );

        // The list is derived, newest first (2@300, 3@200, 1@100), all three present.
        let listed = s.list_sessions(None, 50).unwrap();
        let ids: Vec<i64> = listed.iter().map(|x| x.id).collect();
        assert_eq!(
            ids,
            vec![2, 3, 1],
            "newest first, derived from meta.json, not an index file"
        );

        // Project filter and limit still apply on the derived list.
        assert_eq!(s.list_sessions(Some("/p"), 50).unwrap().len(), 3);
        assert_eq!(s.list_sessions(Some("/other"), 50).unwrap().len(), 0);
        assert_eq!(
            s.list_sessions(None, 1).unwrap()[0].id,
            2,
            "limit keeps the newest"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Deleting a session removes only its own directory; the derived index simply
    /// no longer sees it — no index file to keep in sync.
    #[test]
    fn deleting_a_session_removes_it_from_the_derived_list() {
        let (s, root) = store("delete");
        s.write_meta(&meta(1, "/p", 100)).unwrap();
        s.write_meta(&meta(2, "/p", 200)).unwrap();
        assert_eq!(s.list_sessions(None, 50).unwrap().len(), 2);
        let dir = s.session_dir(1);
        assert!(dir.is_dir());

        assert!(s.delete_session(1).unwrap());
        let ids: Vec<i64> = s
            .list_sessions(None, 50)
            .unwrap()
            .iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(ids, vec![2], "only session 2 remains after deleting 1");
        assert!(!dir.exists(), "the session dir is gone");

        let _ = fs::remove_dir_all(&root);
    }

    /// A corrupt or non-session entry in sessions/ must not break the whole list —
    /// the resilience the per-session design exists for.
    #[test]
    fn one_corrupt_session_does_not_take_down_the_list() {
        let (s, root) = store("corrupt");
        s.write_meta(&meta(1, "/p", 100)).unwrap();
        s.write_meta(&meta(2, "/p", 200)).unwrap();

        // A garbage dir and a corrupt meta.json sit alongside the good sessions.
        let sessions = s.session_dir(1).parent().unwrap().to_path_buf();
        fs::create_dir_all(sessions.join("junk")).unwrap();
        fs::create_dir_all(sessions.join("3")).unwrap();
        fs::write(sessions.join("3").join("meta.json"), "{ not json").unwrap();

        let ids: Vec<i64> = s
            .list_sessions(None, 50)
            .unwrap()
            .iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(
            ids,
            vec![2, 1],
            "the two good sessions still list, the corrupt one is skipped"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn toolcalls_round_trip_per_session() {
        let (s, root) = store("toolcalls");
        let call = ToolCall {
            id: "c1".into(),
            turn: 1,
            name: "terminal_use".into(),
            action: "run ls".into(),
            ok: Some(true),
            status: None,
            code: Some("synthetic_code".into()),
            summary: Some("listed".into()),
            started_at: 100,
            ended_at: Some(150),
        };
        s.append_toolcall(1, &call).unwrap();
        let got = s.read_toolcalls(1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "terminal_use");
        assert_eq!(got[0].ok, Some(true));
        assert_eq!(got[0].ended_at, Some(150));
        assert_eq!(got[0].code.as_deref(), Some("synthetic_code"));
        assert!(
            s.read_toolcalls(999).unwrap().is_empty(),
            "another session has none"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compactions_round_trip_newest_first() {
        let (s, root) = store("compactions");
        s.append_compaction(
            1,
            &CompactionRecord {
                through_id: 10,
                summary: "first".into(),
                created_at: 100,
                context: String::new(),
                status: Default::default(),
                sources: Default::default(),
            },
        )
        .unwrap();
        s.append_compaction(
            1,
            &CompactionRecord {
                through_id: 40,
                summary: "second".into(),
                created_at: 200,
                context: String::new(),
                status: Default::default(),
                sources: Default::default(),
            },
        )
        .unwrap();
        let got = s.read_compactions(1).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].through_id, 40, "newest compaction first");
        assert_eq!(got[0].summary, "second");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tasks_replace_whole_per_session() {
        let (s, root) = store("tasks");
        let mk = |text: &str, pos: i64, status: super::super::tasks::Status| Task {
            position: pos,
            text: text.into(),
            status,
        };
        s.write_tasks(
            1,
            &[
                mk("a", 0, super::super::tasks::Status::Done),
                mk("b", 1, super::super::tasks::Status::Doing),
            ],
        )
        .unwrap();
        let got = s.read_tasks(1).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].text, "a");
        // Whole-list replace: a second write replaces, never merges.
        s.write_tasks(1, &[mk("c", 0, super::super::tasks::Status::Todo)])
            .unwrap();
        let got = s.read_tasks(1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "c");
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn session_size_includes_tool_results_but_never_follows_symlink_loops() {
        let (store, root) = store("size-symlink");
        let dir = store.ensure_session_dir(1).unwrap();
        fs::write(dir.join("session.jsonl"), "12345").unwrap();
        fs::write(dir.join("tools_result/result.txt"), "123456789").unwrap();
        crate::platform::fixture::symlink(&dir, dir.join("tools_result/loop")).unwrap();
        assert_eq!(store.session_size_bytes(1), 14);
        fs::remove_dir_all(root).unwrap();
    }
}
