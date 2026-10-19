//! Local persistence. Everything the engine remembers lives here and nowhere
//! else — there is no server copy of any of it, by design.
//!
//! One SQLite file per machine under `~/.rune/`, holding every project's
//! sessions, permission grants and undo manifests.
//!
//! All of it lives in the engine's home rather than in the project, and that is
//! a security decision, not a filing preference: the agent runs shell commands
//! *inside* the project. Anything it can write, it can also destroy.
//!
//! The folder splits by subject, not by SQL: [`sessions`] owns conversation
//! rows, [`perms`] owns the per-project allow-list, [`checkpoints`] owns undo
//! manifests. `mod.rs` owns only the connection and the schema.

pub mod catalogue;
pub mod checkpoints;
#[path = "Composer.rs"]
pub mod composer;
#[path = "Drafts.rs"]
pub mod drafts;
pub mod jsonl;
pub mod perms;
#[path = "Promotion.rs"]
pub mod promotion;
pub mod run_files;
pub mod sessions;
pub mod storage;
pub mod tasks;
pub mod usage;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Bumped whenever the schema below changes. `migrate` walks from whatever the
/// file has to this.
pub(crate) const SCHEMA_VERSION: i64 = 22;

/// An in-flight tool call: the data `begin_toolcall` captured that `end_toolcall`
/// needs to complete the per-session JSONL record. Held in memory (not the DB)
/// because a pending call is runtime state, not history — it only becomes history
/// (a finished toolcalls.jsonl line) when the call ends.
#[derive(Debug, Clone)]
pub(crate) struct PendingCall {
    pub turn: i64,
    pub name: String,
    pub args: String,
    pub started_at: i64,
}

pub struct Db {
    conn: Connection,
    pub jsonl: jsonl::JsonlStore,
    pub(crate) session_leases: std::sync::Mutex<std::collections::HashMap<i64, crate::lock::Lock>>,
    /// In-flight tool calls keyed by (session_id, wire_id). The wire id can repeat
    /// across rounds, so a newer begin replaces an older pending entry for the same
    /// key — matching the old "most recent open row" semantics of end_toolcall.
    pub(crate) pending_toolcalls:
        std::sync::Mutex<std::collections::HashMap<(i64, String), PendingCall>>,
    /// The home of an in-memory database ([`Db::memory`]): made for it alone,
    /// so it goes with it. A real home is never removed here.
    ephemeral: Option<PathBuf>,
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Some(home) = self.ephemeral.take() {
            let _ = std::fs::remove_dir_all(home);
        }
    }
}

impl Db {
    /// Open (creating if needed) the engine database at `path`.
    pub fn open(path: &Path, timeout: std::time::Duration) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let home = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let conn = Connection::open(path)?;
        Db::prepare(conn, timeout, home)
    }

    /// In-memory database, for tests.
    pub fn memory() -> Result<Db> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "rune_test_{}_{}_{}",
            std::process::id(),
            now_ms(),
            id
        ));
        let mut db = Db::prepare(
            Connection::open_in_memory()?,
            crate::settings::configuration::Configuration::default()
                .duration("storage.busy_timeout"),
            home.clone(),
        )?;
        db.ephemeral = Some(home);
        Ok(db)
    }

    fn prepare(conn: Connection, timeout: std::time::Duration, home: PathBuf) -> Result<Db> {
        Db::configure(&conn, timeout)?;
        let mut db = Db {
            conn,
            jsonl: jsonl::JsonlStore::new(home),
            session_leases: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending_toolcalls: std::sync::Mutex::new(std::collections::HashMap::new()),
            ephemeral: None,
        };
        db.migrate()?;
        db.recover_promotions()?;
        Ok(db)
    }

    /// The connection settings every door into this database shares — the
    /// store's own connection and [`catalogue`]'s. Only the settings: no
    /// migration and no recovery, which are the store's start-up work.
    fn configure(conn: &Connection, timeout: std::time::Duration) -> Result<()> {
        // The timeout goes on *first*, and that ordering is the whole reason a
        // second Rune process survives opening this database. `journal_mode = WAL`
        // needs a lock of its own, and a connection with no busy handler yet is
        // refused instantly rather than waited on — so the very first pragma,
        // before anything else, is the one that could not wait. Two processes
        // launched together hit exactly that.
        conn.busy_timeout(timeout)?;
        // WAL keeps a long-running turn from blocking a frontend that is reading
        // history at the same time.
        //
        // Two facts make this one line the most dangerous in the file. Converting
        // the journal is refused outright while another connection is doing it,
        // and SQLite does **not** consult the busy handler for it — so a second
        // Rune process starting alongside the first is refused instantly, and no
        // amount of `busy_timeout` helps. And re-issuing the pragma against a file
        // that is already WAL still takes that same lock, so asking every time is
        // what made it fail. So: ask what the file already is, convert only if it
        // is not, and wait the conversion out when there is something to wait for.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap_or_default();
        if !mode.eq_ignore_ascii_case("wal") {
            while_busy(|| Ok(conn.pragma_update(None, "journal_mode", "WAL")?))?;
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    /// Where the engine keeps its state: `$RUNE_HOME`, else `~/.rune/`.
    ///
    /// A visible dotfile directory rather than `~/.local/share/`, because this is
    /// a developer tool people back up, inspect and delete by hand — the same
    /// reason `~/.cargo` and `~/.ssh` sit where they do.
    ///
    /// It is deliberately somewhere the agent cannot reach: `$HOME` is not a
    /// write root in the sandbox, so a command cannot touch the sessions, the
    /// permission grants, or the undo history. Project-local state would be
    /// inside the one directory the agent *can* write.
    ///
    /// Spelt as the filesystem names it (`jail::canonical_spelling`), whether
    /// it came from `$RUNE_HOME` or `$HOME`: a path derived from it — a
    /// session's media, a package root — compares equal to what the jail
    /// resolves. A `~/.rune` named through a link under `$HOME` refused every
    /// MCP attachment read (bug of 0.6.7).
    pub fn home_dir() -> Result<PathBuf> {
        home_from(
            std::env::var_os("RUNE_HOME"),
            crate::platform::home_dir(),
            std::env::current_dir,
        )
    }

    pub fn default_path() -> Result<PathBuf> {
        Ok(Db::home_dir()?.join("rune.db"))
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A copy of the database taken before a change that cannot be undone —
    /// a schema migration — kept in `backups/` under the store's root, once
    /// per change.
    pub(crate) fn backup_before(&self, name: &str) -> Result<()> {
        if !self.conn.path().is_some_and(|path| !path.is_empty()) {
            return Ok(());
        }
        let directory = self.jsonl.root().join("backups");
        std::fs::create_dir_all(&directory)?;
        let backup = directory.join(format!("{name}.db"));
        if !backup.exists() {
            let temporary = directory.join(format!("{name}.{}.tmp", std::process::id()));
            self.conn.execute(
                "VACUUM INTO ?1",
                rusqlite::params![temporary.to_string_lossy()],
            )?;
            std::fs::OpenOptions::new()
                .write(true)
                .open(&temporary)?
                .sync_all()?;
            crate::platform::replace(&temporary, &backup)?;
        }
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        let _migration = crate::lock::exclusive(
            &self.jsonl.root().join("run/SchemaMigration.lock"),
            crate::lock::Wait::Yes,
        )?;
        let previous: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if previous > 0
            && previous < SCHEMA_VERSION
            && self.conn.path().is_some_and(|path| !path.is_empty())
        {
            self.backup_before(&format!("BeforeSchema{SCHEMA_VERSION}"))?;
        }
        // `IMMEDIATE`, and the version is read *inside* it — both parts matter.
        //
        // A deferred transaction takes the write lock only at its first write, so
        // a second process is refused there with a busy that never waited: the
        // snapshot it read no longer matches. And reading the version before
        // taking any lock is worse still — two Rune processes launched together
        // both read the old version, both run `ALTER TABLE ... ADD COLUMN`, and
        // the loser dies on "duplicate column name". Under `IMMEDIATE` the second
        // waits, then reads the version the first just committed and does nothing.
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if current >= SCHEMA_VERSION {
            transaction.rollback()?;
            return Ok(());
        }
        if current < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        if current < 2 {
            // A session remembers how much it was trusted. Defaulting to `ask`
            // means an old database opens in the safe mode, never in a looser one.
            self.conn.execute_batch(SCHEMA_V2)?;
        }
        if current < 3 {
            self.conn.execute_batch(SCHEMA_V3)?;
        }
        if current < 4 {
            self.conn.execute_batch(SCHEMA_V4)?;
        }
        if current < 5 {
            self.conn.execute_batch(SCHEMA_V5)?;
        }
        if current < 6 {
            self.conn.execute_batch(SCHEMA_V6)?;
        }
        if current < 7 {
            self.conn.execute_batch(SCHEMA_V7)?;
        }
        if current < 8 {
            self.conn.execute_batch(SCHEMA_V8)?;
        }
        if current < 9 {
            self.conn.execute_batch(SCHEMA_V9)?;
        }
        if current < 10 {
            self.conn.execute_batch(SCHEMA_V10)?;
        }
        if current < 11 {
            self.conn.execute_batch(SCHEMA_V11)?;
        }
        if current < 12 {
            // SQLite has no `ADD COLUMN IF NOT EXISTS`, and a previous run can
            // have added the column yet died before `user_version` was written —
            // leaving the database with the column present but the version old.
            // Adding it again would then fail with "duplicate column name",
            // every start, forever. Check first, so re-running is always safe.
            if !self.column_exists("token_usage", "estimated_in")? {
                self.conn.execute_batch(SCHEMA_V12)?;
            }
        }
        if current < 13 {
            self.conn.execute_batch(SCHEMA_V13)?;
            // Seed the message id counter past every id already used, so a jsonl-only
            // writer never reissues an id the historical record already carries.
            self.seed_message_counter()?;
        }
        if current < 15 {
            self.conn.execute_batch(SCHEMA_V15)?;
        }
        if current < 16 {
            if !self.column_exists("session_files", "added")? {
                self.conn.execute_batch(
                    "ALTER TABLE session_files ADD COLUMN added INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if !self.column_exists("session_files", "removed")? {
                self.conn.execute_batch(
                    "ALTER TABLE session_files ADD COLUMN removed INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if !self.column_exists("session_files", "change")? {
                self.conn.execute_batch(
                    "ALTER TABLE session_files ADD COLUMN change TEXT NOT NULL DEFAULT 'modified';",
                )?;
            }
        }
        if current < 18 {
            self.conn.execute_batch("CREATE TABLE IF NOT EXISTS promotions (token TEXT PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, journal TEXT NOT NULL, complete INTEGER NOT NULL DEFAULT 0, started INTEGER NOT NULL DEFAULT 0);")?;
        }
        if current < 19 {
            self.conn.execute_batch("DROP TABLE IF EXISTS ninerouter_effort_jobs; DROP TABLE IF EXISTS ninerouter_efforts;")?;
        }
        if current < 20 {
            // Conversation history, tool calls, summaries and plans live in each
            // session's JSONL files. These tables only ever held copies kept for
            // sessions written before that move, every one of them already in
            // its JSONL; nothing reads them any more.
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS toolcalls; \
                 DROP TABLE IF EXISTS compactions; DROP TABLE IF EXISTS tasks;",
            )?;
        }
        if current < 21 && self.table_exists("session_files")? {
            // Undo is per file (owner, 2026-10-09): each row says when the
            // session last changed its file, and the mode to give it back.
            // Rows written before take both from the turn's checkpoint while
            // its manifest is still kept; the rest stay unknown.
            if !self.column_exists("session_files", "last_at")? {
                self.conn.execute_batch(
                    "ALTER TABLE session_files ADD COLUMN last_at INTEGER;
                     UPDATE session_files SET last_at = (
                         SELECT c.created_at FROM checkpoints c
                         WHERE c.session_id = session_files.session_id
                           AND c.turn = session_files.last_turn);",
                )?;
            }
            if !self.column_exists("session_files", "first_mode")? {
                self.conn.execute_batch(
                    "ALTER TABLE session_files ADD COLUMN first_mode INTEGER;
                     UPDATE session_files SET first_mode = (
                         SELECT json_extract(f.value, '$.mode')
                         FROM checkpoints c, json_each(c.manifest) f
                         WHERE c.session_id = session_files.session_id
                           AND c.turn = session_files.first_turn
                           AND json_extract(f.value, '$.path') = session_files.path);",
                )?;
            }
        }
        if current < 22 && self.table_exists("session_files")? {
            // Redo (owner, 2026-10-09): what the last undo of a file
            // overwrote, kept until it is put back.
            for (column, kind) in [
                ("redo_blob", "TEXT"),
                ("redo_mode", "INTEGER"),
                ("redo_at", "INTEGER"),
            ] {
                if !self.column_exists("session_files", column)? {
                    self.conn.execute_batch(&format!(
                        "ALTER TABLE session_files ADD COLUMN {column} {kind};"
                    ))?;
                }
            }
        }
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    }

    /// Whether `table` already has a column called `column`.
    ///
    /// Used by migrations that must tolerate a previous run having applied part
    /// of their work — SQLite's `ALTER TABLE ADD COLUMN` has no `IF NOT EXISTS`,
    /// so without this a half-applied migration is fatal on every later start.
    /// Whether the database has a table called `table`.
    fn table_exists(&self, table: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?)
    }

    fn column_exists(&self, table: &str, column: &str) -> Result<bool> {
        let mut statement = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Seed the `message` counter past the highest id ever issued. The AUTOINCREMENT
    /// ledger `sqlite_sequence.messages` is the authoritative high-water mark of every
    /// id the old table ever minted — including the ids the historical JSONL still
    /// carries — so the counter starts there, never below it.
    fn seed_message_counter(&self) -> Result<()> {
        let high_water: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='messages'),
                                  (SELECT MAX(id) FROM messages), 0)",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO counters (name, value) VALUES ('message', ?1)
             ON CONFLICT(name) DO UPDATE SET value = MAX(value, excluded.value)",
            rusqlite::params![high_water],
        )?;
        Ok(())
    }

    /// The next global message id: atomically increment the counter and return the
    /// new value. Monotonic across sessions and across restarts, with no `messages`
    /// table needed. Compaction's `through_id` relies on this staying ever-increasing.
    pub fn next_message_id(&self) -> Result<i64> {
        while_busy(|| {
            // Ensure the row exists even on a database created before this counter.
            self.conn.execute(
                "INSERT OR IGNORE INTO counters (name, value) VALUES ('message', 0)",
                [],
            )?;
            let id: i64 = self.conn.query_row(
                "UPDATE counters SET value = value + 1 WHERE name = 'message' RETURNING value",
                [],
                |r| r.get(0),
            )?;
            Ok(id)
        })
    }
}

/// A connection to the database at `path` with the store's own settings
/// ([`Db::configure`]) and nothing else — for [`catalogue`], which shares the
/// table set but not the store's start-up work.
pub(crate) fn connect(path: &Path, timeout: std::time::Duration) -> Result<Connection> {
    let conn = Connection::open(path)?;
    Db::configure(&conn, timeout)?;
    Ok(conn)
}

/// Repeat a write while the database says "not now" rather than "never".
///
/// `busy_timeout` covers contention it is consulted about. It is *not* consulted
/// for a snapshot conflict: a connection that took a read lock and only asked for
/// the write lock afterwards is refused immediately, with no waiting, because the
/// snapshot it read no longer matches what is on disk. Two Rune processes over one
/// database produce exactly that shape. A short bounded wait with jitter is the
/// whole difference between a turn that failed and a turn that took a moment
/// longer — and a genuine failure is still a genuine failure, reported untouched.
pub(crate) fn while_busy<T>(mut op: impl FnMut() -> Result<T>) -> Result<T> {
    const ATTEMPTS: u32 = 4;
    let mut waited = 0;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if waited < ATTEMPTS && is_contention(&e) => {
                waited += 1;
                std::thread::sleep(std::time::Duration::from_millis(
                    15 * waited as u64 + now_ms().unsigned_abs() % 23,
                ));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Whether this is the database asking for a moment, as opposed to refusing.
pub(crate) fn is_contention(e: &Error) -> bool {
    match e {
        Error::Db(message) => {
            let message = message.to_lowercase();
            message.contains("locked") || message.contains("busy")
        }
        _ => false,
    }
}

/// Unix milliseconds. One clock for every row the engine writes.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// The file the agent touched first, and the state it had **before the session
// ever reached it**. Per-turn manifests are no longer trimmed on their own,
// but a `checkpoint.prune` with an explicit `keep` still deletes the oldest
// captures — the ones holding a file's pre-session content — and sweeps their
// blobs. This one row per touched file is what keeps that first state alive:
// the side panel's FILE CHANGE totals are `diff(first state, file now)`, and
// they must stay true for a session of any length.
//
// `first_*` is written once, on the session's first touch of the path, and
// never updated; `last_turn` rides along so the list can also say when the
// file was last touched. The row dies with its session (`ON DELETE CASCADE`).
const SCHEMA_V15: &str = r#"
CREATE TABLE IF NOT EXISTS session_files (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    path        TEXT    NOT NULL,
    first_blob  TEXT,
    first_turn  INTEGER NOT NULL,
    last_turn   INTEGER NOT NULL,
    added       INTEGER NOT NULL DEFAULT 0,
    removed     INTEGER NOT NULL DEFAULT 0,
    change      TEXT    NOT NULL DEFAULT 'modified',
    PRIMARY KEY (session_id, path)
);
"#;

const SCHEMA_V12: &str = r#"
ALTER TABLE token_usage ADD COLUMN estimated_in INTEGER NOT NULL DEFAULT 0;
"#;

/// Monotonic id counters living in the DB as engine *core state* (not history).
/// With messages/toolcalls moved to per-session JSONL, the DB can no longer mint
/// message ids from `messages` AUTOINCREMENT — but compaction's `through_id` still
/// needs a global, ever-increasing message id. One row per counter; `message` is
/// the only one today.
const SCHEMA_V13: &str = r#"
CREATE TABLE IF NOT EXISTS counters (
    name    TEXT PRIMARY KEY,
    value   INTEGER NOT NULL
);
"#;

const SCHEMA_V11: &str = r#"
CREATE TABLE IF NOT EXISTS provider_models (
    provider    TEXT NOT NULL,
    id          TEXT NOT NULL,
    info        TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);
"#;

const SCHEMA_V10: &str = r#"
CREATE TABLE IF NOT EXISTS model_settings (
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    settings    TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, model)
);
"#;

const SCHEMA_V8: &str = "ALTER TABLE token_usage ADD COLUMN role TEXT NOT NULL DEFAULT 'agent'; ALTER TABLE toolcalls ADD COLUMN wire_id TEXT; UPDATE toolcalls SET wire_id=id;";

const SCHEMA_V9: &str = "ALTER TABLE messages ADD COLUMN continuation TEXT; ALTER TABLE token_usage ADD COLUMN details TEXT; ALTER TABLE token_usage ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 0;";

const SCHEMA_V7: &str = r#"
ALTER TABLE sessions ADD COLUMN active_seconds INTEGER NOT NULL DEFAULT 0;
"#;

const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS token_usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn          INTEGER NOT NULL,
    timestamp     INTEGER NOT NULL,
    tok_in        INTEGER NOT NULL,
    tok_out       INTEGER NOT NULL,
    cache_in      INTEGER NOT NULL,
    context_used  INTEGER NOT NULL,
    cost          REAL    NOT NULL,
    provider      TEXT,
    model         TEXT
);
CREATE INDEX IF NOT EXISTS idx_token_usage_session ON token_usage(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_token_usage_time ON token_usage(timestamp);
"#;

const SCHEMA_V2: &str = r#"
ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'ask';
"#;

// The agent's plan for the turn. Keyed by position rather than an id: the list is
// replaced whole on every update, so a stable row identity would be a lie.
const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    text        TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    PRIMARY KEY (session_id, position)
);
"#;

// What the model actually said, rather than a readable trace of it. A tool-using
// assistant turn is not text: it is text *plus* the calls it made, and a
// transcript missing the calls cannot be replayed to any provider — the tool
// results that follow have nothing to attach to. `reasoning` is kept for the
// same reason, on the providers that ask for it back.
//
// `compactions` is how a long conversation stays sendable: one summary standing
// in for every message up to `through_id`, written once and reused, never
// recomputed. Storing it rather than summarising on the fly is what keeps a
// reload from producing a different history than the turn before it.
// Files the user attached to a question, as a JSON array of
// `{name, mime, path}` — **paths, never bytes**.
//
// The bytes are read from disk when the request is built, once per round. That
// is the difference between this and the Clode server's staging table, and it is
// a difference of situation rather than of taste: there, a file arrived over the
// network and the server was the only place it existed, so it had to be stored.
// Here the file is on the same machine, and a base64 copy in the database would
// be a second copy of something the user already has — bloating a file people
// back up, and going stale the moment they edit the original.
//
// A file that has since been deleted becomes a line saying so, which is more
// useful to the model than a stale copy of what it used to hold.
const SCHEMA_V5: &str = r#"
ALTER TABLE messages ADD COLUMN media TEXT;
"#;

const SCHEMA_V4: &str = r#"
ALTER TABLE messages ADD COLUMN tool_calls TEXT;
ALTER TABLE messages ADD COLUMN reasoning  TEXT;
ALTER TABLE sessions ADD COLUMN settings   TEXT;

CREATE TABLE IF NOT EXISTS compactions (
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    through_id  INTEGER NOT NULL,
    summary     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, through_id)
);
"#;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT    NOT NULL,
    title       TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn          INTEGER NOT NULL,
    role          TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    tool_call_id  TEXT,
    tool_name     TEXT,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

CREATE TABLE IF NOT EXISTS toolcalls (
    id          TEXT    PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn        INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    args        TEXT    NOT NULL,
    ok          INTEGER,
    summary     TEXT,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_toolcalls_session ON toolcalls(session_id, started_at DESC);

-- Permissions are keyed by project on purpose: an allow-list is never global.
CREATE TABLE IF NOT EXISTS permissions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT    NOT NULL,
    tool        TEXT    NOT NULL,
    pattern     TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE(project, tool, pattern)
);

CREATE TABLE IF NOT EXISTS checkpoints (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn        INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    manifest    TEXT    NOT NULL,
    UNIQUE(session_id, turn)
);
"#;

/// Where the engine keeps its state, from what the environment says:
/// `rune_home` when set (relative to `cwd`), else `.rune` in the person's
/// home — always spelt canonically.
fn home_from(
    rune_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(value) = rune_home.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            cwd()?.join(path)
        };
        return Ok(crate::jail::canonical_spelling(&path));
    }
    let home = home.ok_or_else(|| Error::invalid("HOME is not set; pass an explicit db path"))?;
    Ok(crate::jail::canonical_spelling(&home.join(".rune")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_migrates() {
        let db = Db::memory().unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("runedb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("rune.db");
        {
            let _db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        }
        let db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real-world failure this guards: a previous run added `estimated_in`
    /// but died before `user_version` was written, so the column exists while the
    /// version is old. Re-running must detect the column and skip the `ALTER`
    /// instead of failing with "duplicate column name" forever.
    #[test]
    fn a_half_applied_v12_migration_recovers_instead_of_failing_forever() {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("rune-half-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rune.db");
        {
            // Bring the database fully up to date first...
            let _db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        }
        {
            // ...then simulate the half-applied state: column present, version 11.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 11).unwrap();
        }
        // Opening must succeed (not "duplicate column name") and finish at v12.
        let db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let has = db.column_exists("token_usage", "estimated_in").unwrap();
        assert!(has, "the column must still be present exactly once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_drops_obsolete_effort_probe_tables() {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("rune-v17-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rune.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE ninerouter_effort_jobs (provider TEXT, model TEXT);
                 CREATE TABLE ninerouter_efforts (provider TEXT, model TEXT);
                 PRAGMA user_version = 16;",
            )
            .unwrap();
        }
        let db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let jobs_exists: bool = db
            .conn()
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='ninerouter_effort_jobs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let efforts_exists: bool = db
            .conn()
            .query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='ninerouter_efforts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!jobs_exists);
        assert!(!efforts_exists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A canonical scratch folder of its own for one test.
    fn base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-home-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::canonical(&dir).unwrap()
    }

    /// Read from what the environment says, never by changing this test
    /// process's own environment: tests run in parallel, and one that set
    /// `RUNE_HOME` sent every other test reading the home elsewhere.
    #[test]
    fn the_engine_home_is_overridable_and_defaults_to_a_dotfile_dir() {
        let base = base("default");
        let home = base.join("dev");
        std::fs::create_dir_all(&home).unwrap();
        let cwd = || Ok(base.clone());
        let custom = base.join("custom-home");
        assert_eq!(
            home_from(Some(custom.clone().into()), Some(home.clone()), cwd).unwrap(),
            custom
        );
        assert_eq!(
            home_from(None, Some(home.clone()), cwd).unwrap(),
            home.join(".rune")
        );
        assert_eq!(
            home_from(Some("".into()), Some(home.clone()), cwd).unwrap(),
            home.join(".rune"),
            "an empty RUNE_HOME names none"
        );
        assert_eq!(
            home_from(Some("relative".into()), None, cwd).unwrap(),
            base.join("relative"),
            "a relative RUNE_HOME is the working directory's"
        );
        assert!(home_from(None, None, cwd).is_err(), "no home, no default");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Bug of 0.6.7: a home reached through a link (`/home` → `/var/home`, as
    /// on Fedora Atomic) was used as spelt, so a path derived from it never
    /// matched what the jail resolved, and every MCP attachment read was
    /// refused as "following a symbolic link". It is spelt as the filesystem
    /// names it — before `~/.rune` exists, and once it does.
    #[test]
    fn a_home_reached_through_a_link_is_spelt_as_the_filesystem_names_it() {
        let base = base("linked");
        let real = base.join("var/home/dev");
        std::fs::create_dir_all(&real).unwrap();
        crate::platform::fixture::symlink(base.join("var/home"), base.join("home")).unwrap();
        let linked = base.join("home/dev");
        let cwd = || Ok(base.clone());
        assert_eq!(
            home_from(None, Some(linked.clone()), cwd).unwrap(),
            real.join(".rune"),
            "before it exists"
        );
        std::fs::create_dir_all(real.join(".rune")).unwrap();
        assert_eq!(
            home_from(None, Some(linked.clone()), cwd).unwrap(),
            real.join(".rune"),
            "once it exists"
        );
        assert_eq!(
            home_from(Some(linked.join(".rune").into()), None, cwd).unwrap(),
            real.join(".rune"),
            "a RUNE_HOME spelt through the link"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn foreign_keys_are_on() {
        let db = Db::memory().unwrap();
        let on: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("runedb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("rune.db")
    }

    /// The counter is written on every message, so it is the write two Rune
    /// processes collide on most. A lock the busy handler was never told about
    /// used to surface as `database is locked` and take the turn down with it.
    #[test]
    fn a_locked_counter_is_waited_for_rather_than_reported() {
        let path = temp_db("busy");
        let db = Db::open(&path, std::time::Duration::from_millis(50)).unwrap();

        // A second connection holding the write lock, which is what another Rune
        // process looks like from here.
        let squatter = Connection::open(&path).unwrap();
        squatter
            .execute_batch("CREATE TABLE IF NOT EXISTS hold (i INTEGER)")
            .unwrap();
        squatter.execute_batch("BEGIN IMMEDIATE").unwrap();
        squatter.execute("INSERT INTO hold VALUES (1)", []).unwrap();

        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            let _ = squatter.execute_batch("COMMIT");
        });

        let id = db.next_message_id().expect("the counter gave up on a lock");
        assert!(id > 0);
        release.join().unwrap();
    }

    /// Only "come back later" is retried. A missing table will still be missing.
    #[test]
    fn contention_is_told_apart_from_a_fault() {
        assert!(is_contention(&Error::Db("database is locked".into())));
        assert!(is_contention(&Error::Db(
            "database table is busy: counters".into()
        )));
        assert!(!is_contention(&Error::Db("no such table: nope".into())));
        assert!(!is_contention(&Error::invalid("action is required")));
    }

    #[test]
    fn while_busy_gives_up_on_an_error_that_is_not_contention() {
        let mut calls = 0;
        let e = while_busy(|| {
            calls += 1;
            Err::<(), _>(Error::Db("no such table: nope".into()))
        })
        .unwrap_err();
        assert_eq!(calls, 1, "a real failure must not be retried");
        assert!(e.to_string().contains("no such table"));
    }

    #[test]
    fn while_busy_stops_repeating_a_turn_that_will_never_succeed() {
        let mut calls = 0;
        let _ = while_busy(|| {
            calls += 1;
            Err::<(), _>(Error::Db("database is locked".into()))
        });
        assert_eq!(calls, 5, "four retries then the original failure");
    }

    #[test]
    fn while_busy_returns_the_first_success_untouched() {
        let mut calls = 0;
        let out = while_busy(|| {
            calls += 1;
            if calls < 3 {
                return Err(Error::Db("database is locked".into()));
            }
            Ok(calls)
        })
        .unwrap();
        assert_eq!(out, 3);
    }

    /// Two processes opening one database at the same instant is the reported
    /// situation: both migrate, both claim the write lock.
    #[test]
    fn two_connections_racing_one_file_both_get_through() {
        // Repeated on purpose. One pass proves nothing about a race — the bug
        // behind this failed roughly three runs in four and passed the fourth, so
        // a single green run is luck, not evidence. Twenty-five fresh files, each
        // opened by two threads at once.
        for round in 0..25 {
            let path = temp_db(&format!("race{round}"));
            let dir = path.parent().unwrap().to_path_buf();
            std::fs::create_dir_all(&dir).unwrap();
            std::thread::scope(|scope| {
                for _ in 0..2 {
                    let p = path.clone();
                    scope.spawn(move || Db::open(&p, std::time::Duration::from_secs(5)).unwrap());
                }
            });
            let db = Db::open(&path, std::time::Duration::from_secs(5)).unwrap();
            let version: i64 = db
                .conn()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION, "round {round}");
            // Whoever lost the race must not have left the file downgraded out of
            // WAL, which is the mode that keeps a reader from blocking a turn.
            let mode: String = db
                .conn()
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                mode.to_lowercase(),
                "wal",
                "round {round} left the file in {mode}"
            );
        }
    }
}
