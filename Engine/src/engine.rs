//! The facade a frontend drives: commands in, events out.
//!
//! Everything below this file is the engine proper; everything above it is a
//! frontend. `Engine` owns the sessions, dispatches [`Command`]s, and hands out
//! one [`Event`] stream. It does not know or care whether the frontend on the
//! other end is a terminal, a GUI, or a test.
//!
//! Commands are answered immediately with an `ack`. A prompt does **not** block
//! the answer — it runs on its own thread, so a frontend can cancel it or reply
//! to a permission question while it is still going.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::{json, Value};

use crate::checkpoint::Checkpoints;
use crate::error::{Error, Result};
use crate::jail::Jail;
use crate::memory::Memory;
use crate::panic;
use crate::perm::broker::Broker;
use crate::perm::Gate;
use crate::perm::Mode;
use crate::proto::{Command, Event};
use crate::provider::Provider;
use crate::sandbox::Sandbox;
use crate::session::{call, turn, NewSession, Runtime, Session, SessionState};
use crate::settings::{ModelSettings, Settings};
use crate::store::Db;
use crate::tools::Registry;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Config {
    /// Where sessions, permissions and checkpoint manifests live. Its
    /// directory is the store's root: every project's folder
    /// (`crate::project`) and the `run/` locks sit beside it.
    pub db_path: PathBuf,
    /// Where the user's settings and keys live.
    pub data_dir: PathBuf,
    pub provider: Arc<dyn Provider>,
    /// Providers and keys the user configured. Shared with the live provider, so
    /// adding an endpoint at runtime takes effect on the next call rather than
    /// on the next launch.
    pub settings: Arc<Mutex<Settings>>,
    /// Offer only the tools that cannot change anything.
    pub read_only: bool,
}

impl Config {
    /// An engine rooted at an explicit pair of paths.
    ///
    /// The user's providers and keys are read from `data_dir`, so a test or a
    /// second install pointed at its own directory is genuinely isolated — it
    /// cannot pick up the real machine's credentials by accident.
    pub fn at(db_path: PathBuf, data_dir: PathBuf, provider: Arc<dyn Provider>) -> Result<Config> {
        let settings = Arc::new(Mutex::new(Settings::load(&data_dir, &db_path)?));
        Ok(Config {
            db_path,
            data_dir,
            provider,
            settings,
            read_only: false,
        })
    }

    /// The standard local layout: `$RUNE_HOME`, else `~/.rune/`.
    pub fn local(provider: Arc<dyn Provider>) -> Result<Config> {
        let db_path = Db::default_path()?;
        let data_dir = db_path
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| Error::invalid("data directory has no parent"))?;
        Config::at(db_path, data_dir, provider)
    }
}

pub struct Engine {
    rt: Runtime,
    broker: Arc<Broker>,
    settings: Arc<Mutex<Settings>>,
    /// The store's root, where each project's folder lives: one root for the
    /// sessions the store files there and the memory, undo blobs and scratch
    /// the engine keeps beside them.
    home: PathBuf,
    /// Paths no permission prompt may open. Built here because two of its
    /// entries are this install's own directories, which only `Config` knows.
    forbidden: Arc<crate::perm::reach::Forbidden>,
    sessions: HashMap<i64, Session>,
    running: HashMap<i64, JoinHandle<()>>,
    catalogues: Vec<JoinHandle<()>>,
    provider_work: Arc<Mutex<()>>,
    provider_models: HashMap<(i64, String), String>,
    /// This engine is a session host and took over what the build before it
    /// handed on: from here, a session it opens tells the model of the
    /// background tasks it was never told ended ([`Engine::settle_background`]).
    background_settled: bool,
    /// The configuration generation whose providers and keys this engine read
    /// ([`Engine::follow_configuration`]).
    configuration_seen: u64,
}

impl Engine {
    /// Build an engine and the event stream it writes to.
    pub fn start(cfg: Config) -> Result<(Engine, Receiver<Event>)> {
        // A home an older build wrote is upgraded by the frontend before any
        // engine opens it (`crate::upgrade`). Opened as it is, new sessions
        // would be filed beside old ones this build cannot see.
        if let Some(store) = cfg.db_path.parent() {
            if crate::upgrade::pending(store)? {
                return Err(Error::invalid(
                    "this Rune home is still in an older format; it is upgraded before an engine opens it",
                ));
            }
        }
        // One shared configuration for the whole engine. Limits is a view over the
        // same handle Settings holds, so a SettingsSet reload reaches both — no
        // frozen startup snapshot anywhere downstream.
        let shared_config = panic::lock(&cfg.settings).config.clone();
        let limits = Arc::new(crate::limits::Limits::over(shared_config));
        let configuration_seen = limits.config.generation();
        let db = Db::open(&cfg.db_path, limits.config.duration("storage.busy_timeout"))?;
        let (tx, rx) = channel();

        // Read before anything else is built: a config the user got wrong should
        // stop the engine here, with the reason, rather than take effect halfway.
        let registry = Arc::new(if cfg.read_only {
            Registry::read_only(Arc::clone(&limits), Arc::clone(&cfg.settings))
        } else {
            Registry::new(Arc::clone(&limits), Arc::clone(&cfg.settings))
        });

        // Scratch directories outlive a crash. Sweep the old ones on the way up
        // rather than leaving a machine to fill with them.
        //
        // On a thread of its own, and this is the whole point: a machine
        // that has not opened Rune for a day can be holding a gigabyte of them
        // across a hundred thousand directories, and a launch that deletes
        // that much *before the first frame* is a launch that looks like a
        // hang. Nothing here is needed to answer the first request, so nothing
        // here may stand in front of it.
        let home = db.jsonl.root().to_path_buf();
        let sweep_home = home.clone();
        let sweep_after = limits.config.duration("storage.scratch_lifetime");
        let _ = std::thread::Builder::new()
            .name("rune-scratch-sweep".into())
            .spawn(move || sweep_stale_scratch(&sweep_home, sweep_after));
        // And `run/` of what no process can be using: a deleted session's
        // files, and what a host of an earlier boot left (`store::run_files`).
        let sessions: std::collections::HashSet<i64> = db.session_ids().unwrap_or_default();
        let run_home = home.clone();
        let _ = std::thread::Builder::new()
            .name("rune-run-sweep".into())
            .spawn(move || {
                crate::store::run_files::sweep(&run_home, &sessions, crate::platform::boot_time())
            });

        // Everything this install writes to, so no prompt can be answered into
        // its own database, its checkpoints, or `secrets.json`.
        let mut own: Vec<PathBuf> = vec![cfg.data_dir.clone()];
        if let Some(dir) = cfg.db_path.parent() {
            own.push(dir.to_path_buf());
        }
        if let Ok(home) = Db::home_dir() {
            own.push(home);
        }
        let forbidden = Arc::new(crate::perm::reach::Forbidden::standard(
            crate::platform::home_dir().as_deref(),
            &own,
        ));

        let engine = Engine {
            rt: Runtime {
                mcp: crate::capabilities::mcp::Runtime::new()?,
                db: Arc::new(Mutex::new(db)),
                registry,
                limits,
                provider: cfg.provider,
                settings: Arc::clone(&cfg.settings),
                events: crate::session::live::Bus::new(tx),
            },
            broker: Arc::new(Broker::new()),
            settings: cfg.settings,
            home,
            forbidden,
            sessions: HashMap::new(),
            running: HashMap::new(),
            catalogues: Vec::new(),
            provider_work: Arc::new(Mutex::new(())),
            provider_models: HashMap::new(),
            background_settled: false,
            configuration_seen,
        };
        Ok((engine, rx))
    }

    /// The first line a frontend should see: what this engine is and what it can
    /// actually enforce on this machine.
    pub fn announce(&self, sandbox: &Sandbox) {
        self.emit(Event::Ready {
            version: VERSION.to_string(),
            protocol: crate::proto::PROTOCOL,
            sandbox: sandbox.report().clone(),
        });
    }

    /// Handle one command. Returns `false` when the frontend asked to shut down.
    pub fn handle(&mut self, cmd: Command) -> bool {
        self.follow_configuration();
        let id = cmd.request_id();
        if matches!(cmd, Command::Shutdown { .. }) {
            self.shutdown();
            Self::ack(&self.rt.events, id, Ok(None));
            return false;
        }
        if let Command::CapabilityManage {
            session, request, ..
        } = &cmd
        {
            let state = match self.resume(*session) {
                Ok(s) => s.state,
                Err(e) => {
                    Self::ack(&self.rt.events, id, Err(e));
                    return true;
                }
            };
            if state.is_busy() {
                Self::ack(
                    &self.rt.events,
                    id,
                    Err(Error::invalid(
                        "Wait for the active operation before managing capabilities",
                    )),
                );
                return true;
            }
            let request = request.clone();
            let events = self.rt.events.clone();
            let database = self.rt.db.clone();
            let Some(guard) = state.begin_guarded() else {
                return true;
            };
            self.catalogues.retain(|h| !h.is_finished());
            if self.catalogues.len() >= 16 {
                drop(guard);
                Self::ack(
                    &self.rt.events,
                    id,
                    Err(Error::invalid("Capability operations are busy")),
                );
                return true;
            }
            self.catalogues.push(std::thread::spawn(move||{
                let result=panic::catch("capability management",||{
                    *panic::lock(&state.capabilities.input.operation)=Some(("host-capability".into(),state.cancel.clone()));
                    let result=crate::capabilities::manage::execute(&state.capabilities,&state.registry,&request,&state.cancel);
                    *panic::lock(&state.capabilities.input.operation)=None;
                    let result=result?;
                    // Both are Rune's report of what the user chose, carrying text
                    // from outside (a skill package, an MCP server): context rows,
                    // never the user's own words, so no classifier reads them as
                    // the user's authorization.
                    use crate::store::sessions::{ContextKind, ContextMeta, NewMessage};
                    if request.group=="skills" && request.operation=="activate" {
                        let content=result["content"].as_str().ok_or_else(||Error::invalid("Skill content is missing"))?;
                        let skill=result["skill"].as_str().unwrap_or_default();
                        let text=crate::frame::block("skill_activation",&[("id",skill),("note","the user activated this skill through Rune")],content);
                        let db=panic::lock(&database);
                        db.append_message(state.id,NewMessage::context(db.max_turn(state.id)?,&text,ContextMeta::named(ContextKind::Skill,skill)))?;
                    }
                    if request.group=="mcp" && request.operation=="prompt" {
                        let source=request.arguments.first().cloned().unwrap_or_default();
                        let text=crate::frame::block("mcp_prompt",&[("server",&source),("note","the user ran this MCP server prompt; its content comes from the server")],&serde_json::to_string(&result["messages"])?);
                        let db=panic::lock(&database);
                        db.append_message(state.id,NewMessage::context(db.max_turn(state.id)?,&text,ContextMeta::named(ContextKind::McpPrompt,&source)))?;
                    }
                    Ok(result)
                }).and_then(|r|r);
                drop(guard);
                Self::ack(&events,id,result.map(Some));
            }));
            return true;
        }
        // Serialize registry mutations away from the UI thread. A probe may
        // take seconds; navigation and turn cancellation must remain available.
        if matches!(
            cmd,
            Command::ProviderModels { .. }
                | Command::ProviderAdd { .. }
                | Command::ProviderTest { .. }
                | Command::ProviderKey { .. }
                | Command::ProviderRemove { .. }
                | Command::ProviderTimeout { .. }
                | Command::ProviderDropKey { .. }
                | Command::CredentialsNew { .. }
                | Command::ProviderDropModel { .. }
                | Command::ProviderDefault { .. }
                | Command::ProviderBaseUrl { .. }
                | Command::ServiceAccount { .. }
                | Command::ServiceSelect { .. }
                | Command::ServiceSave { .. }
                | Command::ServiceTest { .. }
                | Command::ServiceRemove { .. }
                | Command::ServiceOptions { .. }
        ) {
            self.catalogues.retain(|h| !h.is_finished());
            if self.catalogues.len() >= 16 {
                Self::ack(
                    &self.rt.events,
                    id,
                    Err(Error::invalid(
                        "Provider operations are busy. Try again shortly.",
                    )),
                );
                return true;
            }
            let settings = Arc::clone(&self.settings);
            let events = self.rt.events.clone();
            let serial = Arc::clone(&self.provider_work);
            self.catalogues.push(std::thread::spawn(move || {
                let _guard = panic::lock(&serial);
                let result = panic::catch("provider operation", || {
                    panic::lock(&settings).reload_providers();
                    panic::lock(&settings).reload_secrets();
                    crate::settings::manage::execute(&settings, cmd)
                })
                .and_then(|r| r);
                Self::ack(&events, id, result.map(Some));
            }));
            return true;
        }
        // Answered in the stream, not after it: see `session::view`.
        if let Command::SessionView { session, .. } = &cmd {
            self.answer_view(id, *session);
            return true;
        }
        let result = self.dispatch_local(cmd);
        Self::ack(&self.rt.events, id, result);
        true
    }

    /// Answer `session.view`: what needs the file system or a lock an emitter
    /// holds is worked out first, then the database is taken and the answer is
    /// built and sent in the one critical section of the event stream.
    fn answer_view(&mut self, id: u64, session: i64) {
        let s = match self.resume(session) {
            Ok(s) => s,
            Err(e) => {
                Self::ack(&self.rt.events, id, Err(e));
                return;
            }
        };
        let mode = s.state.mode();
        let standing = crate::session::view::Standing {
            settings: crate::settings::manage::describe(&self.settings, &s.state.settings(), true),
            file_changes: s
                .state
                .files
                .refresh(&self.rt, &s.state)
                .ok()
                .and_then(|changes| serde_json::to_value(changes).ok())
                .unwrap_or_else(|| json!([])),
            mode: mode.as_str().to_string(),
            classifier: crate::perm::classifier::session_status(&self.rt, &s.state)
                .unwrap_or(Value::Null),
        };
        let db = panic::lock(&self.rt.db);
        self.rt.events.answer(id, session, |account| {
            crate::session::view::build(&db, &s.state, account, standing)
        });
    }

    /// Every command meets the configuration as disk holds it now — whichever
    /// process saved it: a window still on its draft answers its Settings from
    /// this engine, a GUI's `-host` from its own (owner, 2026-10-14). Checked
    /// here, not left to the shared handle's own pace, so an answer is never
    /// older than the change that made the window ask; the provider list and
    /// keys, read apart from the fields, follow the same change.
    fn follow_configuration(&mut self) {
        let _ = self.rt.limits.config.reload_if_changed();
        let generation = self.rt.limits.config.generation();
        if generation == self.configuration_seen {
            return;
        }
        self.configuration_seen = generation;
        let mut settings = panic::lock(&self.settings);
        settings.reload_providers();
        settings.reload_secrets();
    }

    pub fn dispatch_local(&mut self, cmd: Command) -> Result<Option<Value>> {
        self.follow_configuration();
        match cmd {
            Command::DraftOpen { project, token, .. } => {
                let defaults = crate::store::composer::Composer {
                    settings: self.settings_for_fresh(None)?,
                    ..Default::default()
                };
                let root = panic::lock(&self.rt.db).jsonl.root().to_path_buf();
                let draft = crate::store::drafts::Drafts::new(&root).open(
                    std::path::Path::new(&project),
                    defaults,
                    token,
                )?;
                let settings = crate::settings::manage::describe(
                    &self.settings,
                    &draft.composer.settings,
                    false,
                );
                Ok(Some(json!({"draft": draft, "settings": settings})))
            }
            Command::DraftCheck { draft, .. } => {
                let key = crate::project::project_key(std::path::Path::new(&draft.project));
                let root = panic::lock(&self.rt.db).jsonl.root().to_path_buf();
                let gen_file = root.join(format!("run/{key}.gen"));
                let generation = match std::fs::read(&gen_file) {
                    Ok(bytes) if bytes.len() == 8 => u64::from_le_bytes(bytes.try_into().unwrap()),
                    _ => crate::store::drafts::Drafts::new(&root).locked(|registry| {
                        let gen = registry
                            .projects
                            .get(&key)
                            .map_or(0, |stored| stored.generation);
                        let _ = std::fs::create_dir_all(gen_file.parent().unwrap());
                        let _ = std::fs::write(&gen_file, gen.to_le_bytes());
                        Ok(gen)
                    })?,
                };
                Ok(Some(json!({"generation": generation})))
            }
            Command::DraftSave {
                draft,
                base,
                composer,
                ..
            } => {
                let root = panic::lock(&self.rt.db).jsonl.root().to_path_buf();
                let saved =
                    crate::store::drafts::Drafts::new(&root).save(&draft, base, *composer)?;
                Ok(Some(json!({"draft": saved})))
            }
            Command::DraftSettings {
                draft,
                key,
                value,
                provider,
                custom,
                ..
            } => {
                draft.validate()?;
                let mut settings = if let Some(key) = key {
                    self.set_settings(
                        None,
                        Some(draft.composer.settings.clone()),
                        &key,
                        &value,
                        provider.as_deref().map(|provider| (provider, custom)),
                    )?
                    .unwrap_or(Value::Null)
                } else {
                    crate::settings::manage::describe(
                        &self.settings,
                        &draft.composer.settings,
                        false,
                    )
                };
                settings["classifier_status"] = crate::perm::classifier::status(
                    &self.rt,
                    &draft.project,
                    Mode::parse(&draft.composer.mode).unwrap_or_default(),
                )?;
                Ok(Some(settings))
            }
            Command::DraftPromote { draft, .. } => {
                draft.validate()?;
                let submission = format!("{}:{}", draft.token, draft.generation);
                let saved = {
                    use rusqlite::OptionalExtension;
                    let db = panic::lock(&self.rt.db);
                    db.conn()
                        .query_row(
                            "SELECT journal FROM promotions WHERE token=?1",
                            rusqlite::params![submission],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                };
                if let Some(saved) = saved {
                    let promotion: crate::store::promotion::Promotion =
                        serde_json::from_str(&saved)?;
                    let committed =
                        panic::lock(&self.rt.db).promote(&draft, promotion.message.media)?;
                    return Ok(Some(
                        json!({"session": committed.meta.id, "promotion": committed.token}),
                    ));
                }
                let project = crate::platform::canonical(std::path::Path::new(&draft.project))?;
                if project.to_string_lossy() != draft.project
                    || crate::perm::Mode::parse(&draft.composer.mode).is_none()
                {
                    return Err(Error::invalid("Draft project or mode is invalid."));
                }
                if draft.composer.text.trim().is_empty() && draft.composer.attachments.is_empty() {
                    return Err(Error::invalid("prompt text is empty"));
                }
                let held = panic::lock(&self.settings);
                if held.providers.list.is_empty() && self.rt.provider.name() != "live" {
                    // Scripted fixtures do not need a configured remote model.
                } else if draft.composer.settings.model.is_empty()
                    || !held
                        .providers
                        .get(&draft.composer.settings.provider)
                        .is_some_and(|provider| {
                            provider.models.is_empty()
                                || provider.model(&draft.composer.settings.model).is_some()
                        })
                {
                    return Err(Error::invalid("Select a configured model before sending."));
                }
                drop(held);
                let media = draft
                    .composer
                    .attachments
                    .iter()
                    .map(|path| crate::media::Attachment::at(std::path::Path::new(path)))
                    .collect::<Result<Vec<_>>>()?;
                let committed = panic::lock(&self.rt.db).promote(&draft, media)?;
                Ok(Some(
                    json!({"session": committed.meta.id, "promotion": committed.token}),
                ))
            }
            Command::ComposerGet { session, .. } => {
                let state = self.resume(session)?;
                let db = panic::lock(&self.rt.db);
                let mut composer = db.jsonl.composer(session)?.unwrap_or_else(|| {
                    crate::store::composer::Composer {
                        settings: state.state.settings(),
                        mode: state.state.mode().as_str().into(),
                        ..Default::default()
                    }
                });
                if composer.restore(std::path::Path::new(&state.state.project)) {
                    composer.revision = composer
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Internal("Composer revision exhausted".into()))?;
                    db.jsonl.write_composer(session, &composer)?;
                }
                self.emit(Event::Composer {
                    session,
                    request: 0,
                    composer: Box::new(composer.clone()),
                });
                Ok(Some(json!({"composer": composer})))
            }
            Command::ComposerEdit {
                id,
                session,
                base,
                changes,
            } => {
                let state = self.resume(session)?;
                let db = panic::lock(&self.rt.db);
                let composer = db.jsonl.composer(session)?.unwrap_or_else(|| {
                    crate::store::composer::Composer {
                        settings: state.state.settings(),
                        mode: state.state.mode().as_str().into(),
                        ..Default::default()
                    }
                });
                let client = (id >> 32).to_string();
                if composer
                    .operations
                    .get(&client)
                    .is_some_and(|accepted| *accepted >= id)
                {
                    return Ok(Some(json!({"applied": true, "composer": composer})));
                }
                if base != composer.revision {
                    return Ok(Some(json!({"applied": false, "composer": composer})));
                }
                let mut next =
                    composer.edit(base, &changes, std::path::Path::new(&state.state.project))?;
                next.operations.insert(client, id);
                db.jsonl.write_composer(session, &next)?;
                self.emit(Event::Composer {
                    session,
                    request: id,
                    composer: Box::new(next.clone()),
                });
                Ok(Some(json!({"applied": true, "composer": next})))
            }
            Command::PromptCommitted { session, token, .. } => {
                let db = panic::lock(&self.rt.db);
                let (journal, started): (String, bool) = db.conn().query_row("SELECT journal,started FROM promotions WHERE token=?1 AND session_id=?2 AND complete=1", rusqlite::params![token, session], |row| Ok((row.get(0)?, row.get(1)?)))?;
                let promotion: crate::store::promotion::Promotion = serde_json::from_str(&journal)?;
                let already_started = started
                    || db.max_turn(session)? > 1
                    || db.count_assistant_messages(session)? > 0;
                drop(db);
                let state = self.resume(session)?;
                if already_started || state.state.is_busy() {
                    return Ok(None);
                }
                let busy = state
                    .state
                    .begin_guarded()
                    .ok_or_else(|| Error::invalid("this session is already running a turn"))?;
                let state = state.state;
                {
                    // Marked started and announced in one hold of the database:
                    // a view finds the turn before both or after both.
                    let db = panic::lock(&self.rt.db);
                    if db.conn().execute(
                        "UPDATE promotions SET started=1 WHERE token=?1 AND started=0",
                        rusqlite::params![token],
                    )? == 0
                    {
                        return Ok(None);
                    }
                    turn::announce_committed(&self.rt, &db, &state, &promotion.message);
                }
                let rt = self.rt.clone();
                let committed = promotion.message.turn;
                let handle = std::thread::spawn(move || {
                    let _busy = busy;
                    let _ = turn::run(
                        &rt,
                        &state,
                        &promotion.message.content,
                        promotion.message.media,
                        Some(committed),
                    );
                });
                self.running.insert(session, handle);
                Ok(None)
            }
            Command::McpInputReply {
                session,
                req,
                reply,
                ..
            } => {
                let state = self.resume(session)?.state;
                if !state.capabilities.input.broker.reply(&req, reply) {
                    return Err(Error::invalid(
                        "MCP input reply does not match the pending form",
                    ));
                }
                Ok(None)
            }
            Command::CapabilityManage { .. } => unreachable!("handled asynchronously"),
            Command::SessionOpen { project, fresh, .. } => {
                let s = self.open_with(&project, fresh)?;
                self.announce_session(&s);
                let (title, created_at, active_seconds) = {
                    let db = panic::lock(&self.rt.db);
                    let row = db.get_session(s.id())?;
                    (row.title, row.created_at, row.active_seconds)
                };
                // `/new` starts a session, and a session's permission card
                // reads the classifier's answer about *this* session. Without
                // it here the composer kept showing the previous session's
                // status until something else refreshed it — a stale word on
                // screen, not a wrong decision.
                let classifier_status =
                    crate::perm::classifier::session_status(&self.rt, &s.state).ok();
                let settings =
                    crate::settings::manage::describe(&self.settings, &s.state.settings(), true);
                Ok(Some(json!({
                    "session": s.id(),
                    "version": crate::VERSION,
                    "protocol": crate::proto::PROTOCOL,
                    "mode": s.state.mode().as_str(),
                    "title": title,
                    "created_at": created_at,
                    "steps": 0,
                    "turns": 0,
                    "active_seconds": active_seconds,
                    "classifier_status": classifier_status,
                    "settings": settings,
                })))
            }

            Command::SessionList { project, .. } => {
                let path = panic::lock(&self.rt.db)
                    .jsonl
                    .root()
                    .join("run")
                    .join("SessionLifecycle.lock");
                let _lifecycle = crate::lock::exclusive(&path, crate::lock::Wait::Yes)?;
                let db = panic::lock(&self.rt.db);
                let active_ids: Vec<i64> = self.sessions.keys().copied().collect();
                db.prune_empty_sessions_except(&active_ids)?;
                let list = db.list_sessions(project.as_deref(), i64::MAX)?;
                let (drafts, list): (Vec<_>, Vec<_>) = list
                    .into_iter()
                    .partition(|row| !db.has_any_message(row.id));
                Ok(Some(json!({ "sessions": list, "drafts": drafts })))
            }

            Command::SessionCompact { session, .. } => {
                let s = self.resume(session)?;
                // Busy: the work in progress runs it at its next request or
                // before it lets go. Free: the session is taken for it now.
                let Some(guard) = s.state.compact_or_begin() else {
                    return Ok(Some(json!({ "status": "queued", "session": session })));
                };
                let rt = self.rt.clone();
                let state = s.state.clone();
                self.catalogues.push(std::thread::spawn(move || {
                    let mut guard = Some(guard);
                    let result = panic::catch("manual compaction", || {
                        let settings = state.settings();
                        crate::session::compact::compact_then_release(&rt, &state, &settings, || {
                            drop(guard.take())
                        })
                    })
                    .and_then(|r| r);
                    drop(guard);
                    use crate::session::log;
                    match result {
                        // The compaction itself reports what it did (`compacted`).
                        Ok(true) => {}
                        Ok(false) => log::info(
                            &rt,
                            session,
                            "compact",
                            "compact_skipped",
                            "Nothing to compact: no older complete part of the history can be summarised safely.",
                        ),
                        Err(e) => log::warn(
                            &rt,
                            session,
                            "compact",
                            "compact_failed",
                            format!("Could not compact history: {e}"),
                        ),
                    }
                }));
                Ok(Some(json!({ "status": "started", "session": session })))
            }

            Command::SessionClients { session, .. } => {
                Ok(Some(json!({ "count": 1, "session": session })))
            }

            // What a frontend coming back means for the session's work is the
            // session host's to decide (`Cli/src/host.rs`); the engine only
            // acknowledges it, so the frontend knows it was heard.
            Command::SessionReturning { .. } => Ok(None),

            Command::SessionResume {
                session,
                ref project,
                ..
            } => {
                let (usage, title, created_at, steps, turns, active_seconds, session_project) = {
                    let db = panic::lock(&self.rt.db);
                    let row = db.get_session(session)?;
                    let usage = db.session_usage(session).ok();
                    let toolcalls = db.count_toolcalls(session).unwrap_or(0);
                    let assistants = db.count_assistant_messages(session).unwrap_or(0);
                    let turns = db.max_turn(session).unwrap_or(0);
                    (
                        usage,
                        row.title,
                        row.created_at,
                        toolcalls + assistants,
                        turns,
                        row.active_seconds,
                        row.project,
                    )
                };

                if let Some(expected) = project {
                    let canon_expected = crate::platform::canonical(std::path::Path::new(expected))
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| expected.clone());
                    let canon_session =
                        crate::platform::canonical(std::path::Path::new(&session_project))
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| session_project.clone());
                    if canon_expected != canon_session {
                        return Err(Error::invalid(format!(
                            "Cannot resume session #{session}: it belongs to project '{session_project}', but current project is '{expected}'. A session can only be resumed within its own project workspace."
                        )));
                    }
                }

                let s = self.resume(session)?;
                s.state.background.resume();
                if self.background_settled {
                    s.state.background.settle_unreported();
                }
                // What ended while no window watched is told now.
                s.state.background.deliver_pending();
                self.announce_session(&s);
                let classifier_status =
                    crate::perm::classifier::session_status(&self.rt, &s.state).ok();
                let file_changes = s.state.files.refresh(&self.rt, &s.state)?;
                let settings =
                    crate::settings::manage::describe(&self.settings, &s.state.settings(), true);
                Ok(Some(json!({
                    "session": s.id(),
                    "version": crate::VERSION,
                    "protocol": crate::proto::PROTOCOL,
                    "mode": s.state.mode().as_str(),
                    "title": title,
                    "created_at": created_at,
                    "steps": steps,
                    "turns": turns,
                    "usage": usage,
                    "active_seconds": active_seconds,
                    "classifier_status": classifier_status,
                    "file_changes": file_changes,
                    "settings": settings,
                })))
            }

            Command::SessionHistory { session, .. } => {
                let db = panic::lock(&self.rt.db);
                Ok(Some(json!({ "messages": db.history(session)? })))
            }

            Command::SessionLogs { session, limit, .. } => {
                let db = panic::lock(&self.rt.db);
                Ok(Some(json!({
                    "entries": db.jsonl.read_logs(session, limit)?
                })))
            }

            Command::SessionContext { session, .. } => {
                let s = self.resume(session)?;
                let (rows, compaction) = {
                    let db = panic::lock(&self.rt.db);
                    (db.history_shared(session)?, db.compaction(session)?)
                };
                let injected: Vec<Value> = rows
                    .iter()
                    .filter(|m| m.role == crate::store::sessions::Role::Context)
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "turn": m.turn,
                            "at": m.created_at,
                            "kind": m.context.as_ref().map(|c| c.kind),
                            "name": m.context.as_ref().and_then(|c| c.name.clone()),
                            "text": m.content,
                        })
                    })
                    .collect();
                let values =
                    crate::session::status::current(&self.rt, &s.state, &s.state.settings());
                Ok(Some(json!({
                    "system": crate::session::prompt::SYSTEM,
                    "tools": s.state.registry.declared().list,
                    "context": injected,
                    "summary": compaction.as_ref().map(|c| c.summary.clone()),
                    "summary_context": compaction.as_ref().map(|c| c.context.clone()),
                    "status": values.iter().map(|(name, value)| json!({"name": name, "value": value})).collect::<Vec<_>>(),
                })))
            }

            Command::SessionTasks { session, .. } => {
                let db = panic::lock(&self.rt.db);
                Ok(Some(json!({ "tasks": db.tasks(session)? })))
            }

            Command::SessionRename { session, title, .. } => {
                let db = panic::lock(&self.rt.db);
                if !db.rename_session(session, &title)? {
                    return Err(Error::NotFound(format!("session {session}")));
                }
                Ok(Some(json!({ "renamed": true })))
            }

            // How long a session has been active is the engine's to count: it is
            // the only one that knows when a turn ran. A window used to keep
            // its own clock from the moment it opened and write it here, and
            // two windows on one session disagreed with each other and with the
            // truth. Still answered, so an older window is not left waiting.
            Command::SessionTouch { .. } => Ok(Some(json!({ "touched": false }))),

            Command::SessionToolcalls { session, limit, .. } => {
                let limit = limit
                    .unwrap_or(self.rt.limits.config.number("storage.session_limit") as usize)
                    .clamp(1, 10000);
                let db = panic::lock(&self.rt.db);
                Ok(Some(json!({
                    "toolcalls": db.toolcalls(session, limit)?,
                    "total": db.count_toolcalls(session)?
                })))
            }

            // One admission for every message, decided under the lock the
            // work in progress lets go by: it begins a turn, or waits for that
            // work. A frontend never chooses by what it believes about the
            // session — a window that had seen the turn's `Done` while the
            // turn had not let go yet was refused "already running a turn",
            // and put the message back into the composer though nothing was
            // wrong with it.
            Command::Prompt {
                session,
                text,
                attach,
                ..
            }
            | Command::PromptSteer {
                session,
                text,
                attach,
                ..
            } => {
                if text.trim().is_empty() && attach.is_empty() {
                    return Err(Error::invalid("prompt text is empty"));
                }
                // Read and identify every attachment **before** the turn starts,
                // so a path that is wrong is one error the user sees at once
                // rather than a turn that begins and then complains.
                let media: Vec<crate::media::Attachment> = attach
                    .iter()
                    .map(|p| crate::media::Attachment::at(std::path::Path::new(p)))
                    .collect::<Result<_>>()?;
                let s = self.session(session)?;
                // Queued for the turn in progress, or the session is taken for
                // this message — decided under the lock the turn lets go by, so
                // a steer can never land after the turn's last look at the queue.
                let admitted = s.state.admit(|| {
                    let committed =
                        turn::persist_prompt(&self.rt, &s.state, &text, &media, true, None)?;
                    Ok(crate::session::Queued {
                        text: text.clone(),
                        media: media.clone(),
                        committed: Some(committed),
                        origin: crate::session::Origin::User,
                    })
                })?;
                let busy = match admitted {
                    crate::session::Admitted::Queued => {
                        let composer = panic::lock(&self.rt.db).jsonl.composer(session)?;
                        return Ok(Some(json!({"composer": composer})));
                    }
                    crate::session::Admitted::Began(busy) => busy,
                };
                let committed =
                    turn::persist_prompt(&self.rt, &s.state, &text, &media, false, None)?;
                self.reap(&[]);
                let rt = self.rt.clone();
                let state = Arc::clone(&s.state);
                let handle = std::thread::spawn(move || {
                    let _busy = busy;
                    let _ = turn::run(&rt, &state, &text, media, Some(committed));
                });
                self.running.insert(session, handle);
                let composer = panic::lock(&self.rt.db).jsonl.composer(session)?;
                Ok(Some(json!({"composer": composer})))
            }

            Command::Cancel { session, call, .. } => {
                let s = self.session(session)?;
                let stopped = match call {
                    None => {
                        s.state.request_cancel();
                        "turn"
                    }
                    Some(id) if s.state.cancel_call(&id) => "call",
                    // The call finished a moment before the command arrived.
                    // Not an error: a frontend cannot be asked to win that race.
                    Some(_) => "nothing",
                };
                Ok(Some(json!({ "stopped": stopped })))
            }

            Command::QuestionsReply {
                session,
                req,
                reply,
                ..
            } => {
                let s = self.session(session)?;
                if s.state.questions.reply(&req, reply) {
                    Ok(None)
                } else {
                    Err(Error::invalid(
                        "Invalid, duplicate, or expired question reply",
                    ))
                }
            }

            Command::PermissionReply { req, decision, .. } => {
                if self.broker.reply(&req, decision) {
                    Ok(None)
                } else {
                    // A late answer to a question nobody is waiting for. Worth
                    // saying, not worth failing anything over.
                    Err(Error::NotFound(format!("permission request {req}")))
                }
            }

            Command::StorageList { .. } => Ok(Some(
                json!({ "folders": crate::store::storage::list(&self.home) }),
            )),

            Command::StoragePrune { folders, .. } => {
                // A session this engine holds open is kept by its lease, the
                // same as one held by any other process.
                let vault = panic::lock(&self.rt.settings).home.clone();
                let pruned = crate::store::storage::prune(&self.rt.db, &vault, &folders)?;
                panic::lock(&self.rt.settings).reload_secrets();
                Ok(Some(serde_json::to_value(pruned)?))
            }

            Command::SessionDelete { session, .. } => {
                let s = self.resume(session)?;
                // Held to the end: nothing may start in a session being
                // deleted, and its background work ends with it.
                let _busy = s
                    .state
                    .begin_guarded()
                    .ok_or_else(|| Error::invalid("cannot delete a session while it is running"))?;
                s.state.files.stop();
                s.state.background.stop_all();
                let project = s.state.project.clone();
                let removed = {
                    let db = panic::lock(&self.rt.db);
                    db.delete_session(session)?
                };
                // The manifests are gone with the row, so anything they held is
                // now unreferenced — unless a sibling session of the same project
                // still names it.
                let swept = s.state.checkpoints.collect(&project)?;
                s.state.remove_scratch()?;
                // Its lease, its host's lock, socket, key and log go with it.
                let root = panic::lock(&self.rt.db).jsonl.root().to_path_buf();
                crate::store::run_files::forget(&root, session);
                self.sessions.remove(&session);
                self.running.remove(&session);
                Ok(Some(json!({
                    "deleted": removed,
                    "blobs_freed": swept.blobs,
                    "bytes_freed": swept.bytes
                })))
            }

            Command::CheckpointPrune { session, keep, .. } => {
                let s = self.session(session)?;
                if s.state.is_busy() {
                    return Err(Error::invalid("cannot prune while a turn is running"));
                }
                // Undo history has no limit of its own (owner, 2026-10-11):
                // only a `keep` the caller names trims it; without one, this
                // frees what no snapshot names any more and nothing else.
                let p = match keep {
                    Some(keep) => s.state.checkpoints.prune(session, &s.state.project, keep)?,
                    None => {
                        let swept = s.state.checkpoints.collect(&s.state.project)?;
                        crate::checkpoint::Pruned {
                            manifests: 0,
                            blobs: swept.blobs,
                            bytes: swept.bytes,
                        }
                    }
                };
                Ok(Some(json!({
                    "manifests_removed": p.manifests,
                    "blobs_freed": p.blobs,
                    "bytes_freed": p.bytes
                })))
            }

            Command::SessionMode { session, mode, .. } => {
                let s = self.session(session)?;
                // Live, not gated on `busy`: `set_mode` rebuilds the sandbox at
                // once, under the same lock a running turn builds it with, and
                // every permission gate asks the sandbox, so the next tool call
                // — and nothing before it — runs under the new mode. The old refusal made the composer's mode switcher lie
                // mid-turn, which is the one moment a user most wants to tighten
                // or open access for what the agent is doing right now.
                let mode = Mode::parse(&mode)
                    .ok_or_else(|| Error::invalid("mode must be ask, auto or full"))?;
                s.state.set_mode(mode)?;
                {
                    let db = panic::lock(&self.rt.db);
                    let mut composer = db.jsonl.composer(session)?.unwrap_or_else(|| {
                        crate::store::composer::Composer {
                            settings: s.state.settings(),
                            ..Default::default()
                        }
                    });
                    composer.mode = mode.as_str().into();
                    composer.revision = composer
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Internal("Composer revision exhausted".into()))?;
                    db.jsonl.write_composer(session, &composer)?;
                    self.emit(Event::Composer {
                        session,
                        request: 0,
                        composer: Box::new(composer),
                    });
                }
                self.announce_session(&s);
                let classifier_status =
                    crate::perm::classifier::session_status(&self.rt, &s.state).ok();
                Ok(Some(
                    json!({ "mode": mode.as_str(), "classifier_status": classifier_status }),
                ))
            }

            Command::PermissionList { session, .. } => {
                let s = self.session(session)?;
                let db = panic::lock(&self.rt.db);
                let grants: Vec<Value> = db
                    .grants(&s.state.project)?
                    .into_iter()
                    .map(|g| json!({ "tool": g.tool, "pattern": g.pattern, "granted_at": g.created_at }))
                    .collect();
                Ok(Some(json!({ "grants": grants })))
            }

            Command::PermissionRevoke {
                session,
                tool,
                pattern,
                ..
            } => {
                let s = self.session(session)?;
                let project = s.state.project.clone();
                let db = panic::lock(&self.rt.db);
                let removed = match (tool, pattern) {
                    (Some(t), Some(p)) => db.revoke(&project, &t, &p)? as usize,
                    (Some(t), None) => db.revoke_tool(&project, &t)?,
                    (None, _) => db.revoke_all(&project)?,
                };
                Ok(Some(json!({ "revoked": removed })))
            }

            Command::ToolInvoke {
                session,
                name,
                args,
                ..
            } => {
                self.rt.limits.config.reload_if_changed()?;
                let s = self.session(session)?;
                let busy = s
                    .state
                    .begin_guarded()
                    .ok_or_else(|| Error::invalid("this session is already running a turn"))?;
                self.reap(&[]);
                let rt = self.rt.clone();
                let state = Arc::clone(&s.state);
                let handle = std::thread::spawn(move || {
                    let _busy = busy;
                    run_tool_directly(&rt, &state, &name, &args);
                });
                self.running.insert(session, handle);
                Ok(None)
            }

            Command::ToolInput { session, data, .. } => {
                let s = self.session(session)?;
                let delivered = s.state.write_input(data.as_bytes())?;
                Ok(Some(json!({ "delivered": delivered })))
            }

            Command::ToolResize {
                session,
                rows,
                cols,
                ..
            } => {
                let s = self.session(session)?;
                let applied = s.state.resize_terminal(rows, cols)?;
                Ok(Some(json!({ "applied": applied })))
            }

            Command::ToolList { .. } => Ok(Some(json!({ "tools": self.rt.registry.listing() }))),

            Command::CheckpointFiles { session, .. } => {
                let s = self.session(session)?;
                let files = s
                    .state
                    .checkpoints
                    .session_changes(session, &s.state.jail())?;
                Ok(Some(json!({ "files": files })))
            }

            Command::CheckpointDiff { session, file, .. } => {
                let s = self.session(session)?;
                let diff = s
                    .state
                    .checkpoints
                    .file_diff(session, &file, &s.state.jail())?;
                Ok(Some(json!({ "file": file, "diff": diff })))
            }

            Command::CheckpointRestore { session, file, .. } => {
                self.put_back(session, &file, false)
            }

            Command::CheckpointRedos { session, .. } => {
                let s = self.session(session)?;
                let files = s.state.checkpoints.redo_changes(session, &s.state.jail())?;
                Ok(Some(json!({ "files": files })))
            }

            Command::CheckpointRedoDiff { session, file, .. } => {
                let s = self.session(session)?;
                let diff = s
                    .state
                    .checkpoints
                    .redo_diff(session, &file, &s.state.jail())?;
                Ok(Some(json!({ "file": file, "diff": diff })))
            }

            Command::CheckpointRedo { session, file, .. } => self.put_back(session, &file, true),

            Command::ProviderList { .. } => {
                panic::lock(&self.settings).reload_providers();
                panic::lock(&self.settings).reload_secrets();
                Ok(Some(crate::settings::manage::list(&self.settings)))
            }

            Command::ProviderDryRun { name, model, .. } => Ok(Some(
                crate::settings::manage::dryrun(&self.settings, &name, model.as_deref())?,
            )),
            Command::ProviderKeys { name, .. } => {
                Ok(Some(crate::settings::manage::keys(&self.settings, &name)?))
            }
            Command::ProviderConfigureModel {
                ref provider,
                ref model,
                ..
            } => {
                let mut out = crate::settings::manage::configure_model(
                    &self.settings,
                    provider,
                    (**model).clone(),
                )?;
                out["registry"] = crate::settings::manage::list(&self.settings);
                // Never hold the registry mutex across describe(): it reads the
                // same mutex. A recursive lock here also blocks Cancel and resume.
                let config = panic::lock(&self.settings).providers.get(provider).cloned();
                // The model as the provider settled it, not as the screen sent it.
                let api = config.as_ref().map_or("", |c| c.api_kind());
                let model = config
                    .as_ref()
                    .and_then(|c| c.model(&model.id))
                    .cloned()
                    .unwrap_or_else(|| (**model).clone());
                for session in self.sessions.values() {
                    let prev = session.state.settings();
                    if prev.provider == *provider && prev.model == model.id {
                        let mut updated = prev.clone();
                        updated.thinking = model.reasoning_enabled.unwrap_or(prev.thinking);
                        if model.thinking_required {
                            updated.thinking = true;
                        }
                        if let Some(ref def) = model.default_effort {
                            updated.effort = Some(def.clone());
                        } else if !model.efforts.is_empty() {
                            if !updated
                                .effort
                                .as_ref()
                                .is_some_and(|e| model.efforts.contains(e))
                            {
                                updated.effort = model.starting_effort(api);
                            }
                        } else if !updated.thinking {
                            updated.effort = None;
                        }
                        if let Some(mo) = model.max_output {
                            updated.max_output = Some(mo);
                        }
                        if let Some(p) = config.as_ref() {
                            updated.normalize_for_model(p);
                        }
                        {
                            let db = panic::lock(&self.rt.db);
                            db.set_model_settings_json(
                                provider,
                                &model.id,
                                &serde_json::to_string(&updated)?,
                            )?;
                            db.set_settings_json(session.id(), &serde_json::to_string(&updated)?)?;
                            session.state.set_settings(updated.clone());
                        }
                        let desc =
                            crate::settings::manage::describe(&self.settings, &updated, false);
                        let _ = self.rt.events.send(crate::proto::Event::Settings {
                            session: session.id(),
                            settings: desc,
                        });
                    }
                }
                Ok(Some(out))
            }

            Command::ProviderModels { .. }
            | Command::ProviderAdd { .. }
            | Command::ProviderTest { .. }
            | Command::ProviderKey { .. }
            | Command::ProviderRemove { .. }
            | Command::ProviderTimeout { .. }
            | Command::ProviderDropKey { .. }
            | Command::CredentialsNew { .. }
            | Command::ProviderDropModel { .. }
            | Command::ProviderDefault { .. }
            | Command::ProviderBaseUrl { .. }
            | Command::ServiceAccount { .. }
            | Command::ServiceSelect { .. }
            | Command::ServiceSave { .. }
            | Command::ServiceTest { .. }
            | Command::ServiceRemove { .. }
            | Command::ServiceOptions { .. } => {
                unreachable!("handled asynchronously")
            }

            Command::SettingsGet { session, .. } => {
                let _ = self.rt.limits.config.reload_if_changed();
                panic::lock(&self.settings).reload_providers();
                panic::lock(&self.settings).reload_secrets();
                let st = self.session(session)?;
                let mut description =
                    crate::settings::manage::describe(&self.settings, &st.state.settings(), false);
                description["classifier_status"] =
                    crate::perm::classifier::session_status(&self.rt, &st.state)?;
                Ok(Some(description))
            }

            Command::SettingsSet {
                session,
                key,
                value,
                ..
            } => {
                let res = self.set_settings(Some(session), None, &key, &value, None)?;
                if let Some(ref val) = res {
                    self.emit(crate::proto::Event::Settings {
                        session,
                        settings: val.clone(),
                    });
                }
                // The classifier's status lives in the `Session` event, which is
                // what a joining window's view reads: tell it again, or a window
                // opened after a reload would show the policy from before it.
                if key.starts_with("classifier.") || key.starts_with("config.classifier.") {
                    if let Ok(open) = self.session(session) {
                        self.announce_session(&open);
                    }
                }
                Ok(res)
            }
            Command::SettingsSelect {
                session,
                provider,
                model,
                custom,
                ..
            } => {
                let res = self.set_settings(
                    Some(session),
                    None,
                    "model",
                    &model,
                    Some((&provider, custom)),
                )?;
                if let Some(ref val) = res {
                    self.emit(crate::proto::Event::Settings {
                        session,
                        settings: val.clone(),
                    });
                }
                Ok(res)
            }

            Command::SessionView { .. } => unreachable!("answered in the stream by `handle`"),

            Command::Shutdown { .. } => unreachable!("handled before dispatch"),
        }
    }

    /// One commit path for configuration fields and the provider/model chooser.
    fn set_settings(
        &mut self,
        session: Option<i64>,
        draft_settings: Option<ModelSettings>,
        key: &str,
        value: &str,
        selection: Option<(&str, bool)>,
    ) -> Result<Option<Value>> {
        let st = session.map(|id| self.session(id)).transpose()?;
        let previous = st
            .as_ref()
            .map(|session| session.state.settings())
            .or(draft_settings)
            .ok_or_else(|| Error::invalid("Settings scope is missing."))?;
        if key == "classifier.reload" {
            let Some(st) = &st else {
                return Err(Error::invalid(
                    "Open a session before reloading classifier policy",
                ));
            };
            let _guard = st.state.begin_guarded().ok_or_else(|| {
                Error::invalid("Stop the active work before reloading classifier policy")
            })?;
            crate::perm::classifier::reload(&self.rt, &st.state)?;
            let mut description =
                crate::settings::manage::describe(&self.settings, &previous, false);
            description["classifier_status"] =
                crate::perm::classifier::session_status(&self.rt, &st.state)?;
            return Ok(Some(description));
        }
        let is_config = key.starts_with("config.")
            || key.starts_with("classifier.")
            || crate::settings::configuration::FIELDS
                .iter()
                .any(|field| field.key == key);
        if is_config {
            let canonical = key.strip_prefix("config.").unwrap_or(key);
            let home = panic::lock(&self.settings).home.clone();
            let parsed = if value.is_empty() {
                None
            } else {
                Some(crate::settings::configuration::Configuration::parse_value(
                    canonical, value,
                )?)
            };
            crate::settings::configuration::Configuration::edit(
                &home,
                Some(canonical),
                parsed,
                false,
                None,
            )?;
            panic::lock(&self.settings).config.reload()?;
            if canonical == "storage.busy_timeout" {
                panic::lock(&self.rt.db)
                    .conn()
                    .busy_timeout(self.rt.limits.config.duration(canonical))?;
            }
            if canonical.starts_with("classifier.") {
                if let Some(st) = &st {
                    crate::perm::classifier::invalidate(&st.state);
                }
            }
            let mut description =
                crate::settings::manage::describe(&self.settings, &previous, false);
            if let Some(st) = &st {
                description["classifier_status"] =
                    crate::perm::classifier::session_status(&self.rt, &st.state)?;
            }
            return Ok(Some(description));
        }
        let mut current = previous.clone();
        if matches!(key, "provider" | "model")
            && (value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control))
        {
            return Err(Error::invalid(
                "Choose a valid provider or model ID (1–256 characters).",
            ));
        }
        let target = selection
            .map(|(name, _)| name.to_string())
            .or_else(|| (key == "provider").then(|| value.trim().to_string()))
            .or_else(|| {
                (key == "model" && selection.is_none() && value.contains(':'))
                    .then(|| value.split_once(':').unwrap().0.trim().to_string())
            });

        if let Some(ref name) = target {
            if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                return Err(Error::invalid("Choose a valid provider name."));
            }
            let config = {
                let s = panic::lock(&self.settings);
                let p = s.providers.get(name).cloned().ok_or_else(|| {
                    Error::invalid(format!("Provider '{name}' is not configured."))
                })?;
                if let Some(n) = p.secret.as_deref().filter(|_| selection.is_some()) {
                    if s.secrets.get(n).is_none() {
                        return Err(Error::invalid(format!(
                            "{}. Add a key in Settings › Connections before selecting a model.",
                            s.secrets.key_state(n).note(n)
                        )));
                    }
                }
                p
            };
            if current.provider != *name {
                current = ModelSettings::for_provider(&config);
                if selection.is_none() && key != "model" {
                    let remembered = session
                        .and_then(|session| self.provider_models.get(&(session, name.clone())))
                        .and_then(|id| config.model(id))
                        .cloned();
                    let info = match remembered {
                        Some(info) => Some(info),
                        None => panic::lock(&self.rt.db)
                            .recent_settings_json(50)?
                            .into_iter()
                            .find_map(|j| {
                                let old = serde_json::from_str::<ModelSettings>(&j).ok()?;
                                (old.provider == *name)
                                    .then(|| config.model(&old.model).cloned())
                                    .flatten()
                            }),
                    };
                    if let Some(info) = info {
                        current.model = info.id;
                    }
                }
                // A model switch below starts its model; a provider switch
                // starts the one it lands on here, by the same reading.
                if key == "provider" {
                    let info = config.model(&current.model).cloned();
                    self.start_on_model(&mut current, Some(&config), info.as_ref());
                }
            }
        }

        let target_model: String = if key == "model" {
            if selection.is_none() && value.contains(':') {
                value.split_once(':').unwrap().1.trim().to_string()
            } else {
                value.trim().to_string()
            }
        } else {
            current.model.clone()
        };

        if key == "model" {
            let (config_opt, info, has_configured_providers) = {
                let s = panic::lock(&self.settings);
                let config_opt = s.providers.get(&current.provider).cloned();
                let info = config_opt
                    .as_ref()
                    .and_then(|c| c.model(&target_model))
                    .cloned();
                let has_configured_providers = !s.providers.list.is_empty();
                (config_opt, info, has_configured_providers)
            };

            if let Some(c) = &config_opt {
                if !c.models.is_empty() && info.is_none() {
                    if selection.is_none() {
                        return Err(Error::invalid(format!(
                            "Model '{target_model}' not found in provider '{}'.",
                            current.provider
                        )));
                    }
                    if selection.is_some_and(|(_, custom)| !custom) {
                        return Err(Error::invalid("Model is no longer in this provider's catalog. Refresh the model list."));
                    }
                }
            } else if has_configured_providers {
                return Err(Error::invalid(format!(
                    "Provider '{}' is not configured.",
                    current.provider
                )));
            }

            current.model = target_model;
            self.start_on_model(&mut current, config_opt.as_ref(), info.as_ref());
            if selection.is_some_and(|(_, custom)| custom) {
                panic::lock(&self.settings)
                    .providers
                    .remember_model(&current.provider, &current.model)?;
            }
        } else if key != "provider" {
            let key = key
                .strip_prefix("sampling.")
                .or_else(|| key.strip_prefix("generation."))
                .unwrap_or(key);
            let (unavailable, info, api) = {
                let s = panic::lock(&self.settings);
                let unavailable = if !value.trim().is_empty() {
                    s.providers
                        .get(&current.provider)
                        .map(|p| current.suppressed(p))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let config = s.providers.get(&current.provider);
                let info = config.and_then(|c| c.model(&current.model)).cloned();
                let api = config.map_or("", |c| c.api_kind()).to_string();
                (unavailable, info, api)
            };
            if unavailable.iter().any(|why| {
                why.permanent
                    && (why.key == key
                        || key == "effort"
                            && value.eq_ignore_ascii_case("off")
                            && why.key == "thinking")
            }) {
                return Err(Error::invalid(
                    "This model does not expose that generation control",
                ));
            }
            current.set(key, value, info.as_ref(), &api)?;
            if !current.model.is_empty() {
                panic::lock(&self.rt.db).set_model_settings_json(
                    &current.provider,
                    &current.model,
                    &serde_json::to_string(&current)?,
                )?;
            }
        }
        if let (Some(session), Some(st)) = (session, st.as_ref()) {
            // Publish durable settings and revisioned composer together before
            // fanout; the next API boundary reads this same authoritative state.
            let db = panic::lock(&self.rt.db);
            db.set_settings_json(session, &serde_json::to_string(&current)?)?;
            let mut composer = db.jsonl.composer(session)?.unwrap_or_default();
            composer.settings = current.clone();
            composer.preference_changed = true;
            composer.revision = composer
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Internal("Composer revision exhausted".into()))?;
            db.jsonl.write_composer(session, &composer)?;
            st.state.set_settings(current.clone());
            self.emit(Event::Composer {
                session,
                request: 0,
                composer: Box::new(composer),
            });
            if !previous.provider.is_empty() && !previous.model.is_empty() {
                self.provider_models
                    .insert((session, previous.provider), previous.model);
            }
            self.provider_models
                .insert((session, current.provider.clone()), current.model.clone());
        }
        let mut out = crate::settings::manage::describe(&self.settings, &current, false);
        if let Some(st) = st {
            if let Ok(cs) = crate::perm::classifier::session_status(&self.rt, &st.state) {
                out["classifier_status"] = cs;
            }
        }
        Ok(Some(out))
    }

    /// Open a project and start a new session in it.
    /// Open a session in `project` — the empty one it already has, or a new one.
    ///
    /// Reusing an empty session is what keeps `/new` from being a counter. It is
    /// safe precisely because there is nothing to reuse: a session with no
    /// messages has no history, no plan and no cost, so handing it back is
    /// indistinguishable from handing back a fresh one — except that it does not
    /// leave a row behind. See [`crate::store::Db::empty_session`].
    pub fn open(&mut self, project: &str) -> Result<Session> {
        self.open_with(project, false)
    }

    /// Open a session, optionally insisting on a new identity for an interactive
    /// window. Never attach an unqualified `rune` launch to somebody else's
    /// empty (possibly streaming) session in the same project.
    fn open_with(&mut self, project: &str, fresh: bool) -> Result<Session> {
        let _ = self.rt.limits.config.reload_if_changed();
        // A key stored by another process since this engine started is only on
        // disk. Re-read it here so `/new` — and the first launch, which opens
        // its session the same way — can actually reach the provider.
        panic::lock(&self.settings).reload_secrets();
        let jail = Jail::new(project)?;
        let canonical = jail.root().to_string_lossy().to_string();
        let path = panic::lock(&self.rt.db)
            .jsonl
            .root()
            .join("run")
            .join("SessionLifecycle.lock");
        let _lifecycle = crate::lock::exclusive(&path, crate::lock::Wait::Yes)?;
        let empty = if fresh {
            None
        } else {
            panic::lock(&self.rt.db).empty_session(&canonical)?
        };
        if let Some(id) = empty {
            let s = self.resume(id)?;
            let current_valid = !s.state.settings().model.is_empty() && {
                let p = panic::lock(&self.settings);
                match p.providers.get(&s.state.settings().provider) {
                    Some(pr) => {
                        pr.models.is_empty() || pr.model(&s.state.settings().model).is_some()
                    }
                    None => p.providers.list.is_empty(),
                }
            };
            if !current_valid {
                let fresh = self.settings_for_fresh(Some(id))?;
                if s.state.settings() != fresh {
                    let _ = panic::lock(&self.rt.db)
                        .set_settings_json(id, &serde_json::to_string(&fresh)?);
                    s.state.set_settings(fresh);
                }
            }
            return Ok(s);
        }
        let row = {
            let db = panic::lock(&self.rt.db);
            let row = db.create_session(&canonical)?;
            db.lease_session(row.id)?;
            row
        };
        let session = self.build(row.id, canonical, jail, Mode::default())?;
        self.sessions.insert(row.id, session.clone());
        Ok(session)
    }

    fn resume(&mut self, id: i64) -> Result<Session> {
        if let Some(s) = self.sessions.get(&id) {
            s.state.files.watch(&self.rt, &s.state);
            return Ok(s.clone());
        }
        let row = {
            let db = panic::lock(&self.rt.db);
            db.lease_session(id)?;
            db.get_session(id)?
        };
        let jail = Jail::new(&row.project)?;
        let mode = Mode::parse(&row.mode).unwrap_or_default();
        let session = self.build(id, row.project, jail, mode)?;
        self.sessions.insert(id, session.clone());
        Ok(session)
    }

    fn build(&self, id: i64, project: String, jail: Jail, mode: Mode) -> Result<Session> {
        // Everything this project keeps lives in its one folder (see
        // `crate::project`): its memory, its undo blobs, its sessions, and the
        // scratch each session works in.
        let folder = crate::project::open(&self.home, std::path::Path::new(&project))?;
        let (session_dir, results) = {
            let db = panic::lock(&self.rt.db);
            (
                db.jsonl.register(id, &project)?,
                db.jsonl.tools_result_dir(id),
            )
        };
        // Each session gets its own scratch directory, granted to the sandbox and
        // exported as TMPDIR. Sharing the machine's /tmp would hand a command
        // everything else the user has left lying there.
        let scratch = folder.join("scratch").join(id.to_string());
        // A new session is an operation boundary: take the *current* configuration
        // snapshot (post any SettingsSet), not a value frozen at engine startup.
        let config_now = self.rt.limits.config.snapshot();
        let checkpoints = Arc::new(Checkpoints::new(
            Arc::clone(&self.rt.db),
            folder.join("objects"),
            &self.rt.limits.config,
        ));

        // Sessions in one project share their notes; unrelated projects never
        // do. The directory has to exist before the jail can canonicalise it, and
        // if it cannot be created the session fails loudly rather than opening
        // with a memory the agent would silently write into nowhere.
        let memory = Memory::new(folder.join("memory"));
        let jail = jail.with_annex(memory.ensure()?);
        let gate = Arc::new(
            Gate::new(
                Arc::clone(&self.rt.db),
                project.clone(),
                Arc::clone(&self.broker),
            )
            .with_timeout(self.rt.limits.config.duration("permissions.timeout")),
        );

        let reach = standing_reach(&self.rt.db, &project, &self.forbidden);

        let capabilities = crate::capabilities::Capabilities::open(
            &config_now,
            std::path::Path::new(&project),
            id,
            session_dir.join("capabilities.json"),
            self.rt.mcp.clone(),
            Some(self.rt.events.clone()),
        )?;
        let session = Session::new(SessionState::new(NewSession {
            registry: Arc::new(self.rt.registry.fork()),
            capabilities,
            config: self.rt.limits.config.clone(),
            id,
            project,
            jail,
            reach,
            checkpoints,
            gate,
            memory,
            scratch,
            results,
            mode,
            settings: self.settings_for(id)?,
            forbidden: Arc::clone(&self.forbidden),
            db: Arc::clone(&self.rt.db),
        }));
        let notifier = Arc::new(EngineNotifier {
            rt: self.rt.clone(),
            state: Arc::downgrade(&session.state),
        });
        session.state.background.set_notifier(notifier);
        session
            .state
            .background
            .set_journal(Arc::new(BackgroundJournal {
                store: panic::lock(&self.rt.db).jsonl.clone(),
                session: id,
            }));
        session.state.files.watch(&self.rt, &session.state);
        Ok(session)
    }

    /// The model settings this session should open with.
    ///
    /// A session that has chosen keeps its choice, whatever the install-wide
    /// default has become since. A session that has not is given the current
    /// default **and it is written down**, so tomorrow's change of default does
    /// not silently move an old conversation to a different model.
    fn settings_for(&self, id: i64) -> Result<ModelSettings> {
        let stored = {
            let db = panic::lock(&self.rt.db);
            db.settings_json(id)?
        };
        if let Some(json) = stored {
            if let Ok(mut current) = serde_json::from_str::<ModelSettings>(&json) {
                let previous = current.clone();
                {
                    let providers = panic::lock(&self.settings);
                    let valid = if !providers.providers.list.is_empty() {
                        !current.model.is_empty()
                            && match providers.providers.get(&current.provider) {
                                Some(provider) => {
                                    provider.models.is_empty()
                                        || provider.model(&current.model).is_some()
                                }
                                None => false,
                            }
                    } else {
                        !current.model.is_empty()
                    };
                    if !valid {
                        // Per Cipher architecture: if a session's model was deleted or is no longer
                        // selectable, do NOT silently substitute another model. Clear the model so
                        // it renders as "Select Models" and prevents AI calls until chosen.
                        current.model.clear();
                        current.thinking = false;
                        current.effort = None;
                    } else if let Some(provider) = providers.providers.get(&current.provider) {
                        current.normalize_for_model(provider);
                    }
                }
                if current != previous {
                    panic::lock(&self.rt.db)
                        .set_settings_json(id, &serde_json::to_string(&current)?)?;
                }
                return Ok(current);
            }
        }
        self.settings_for_fresh(Some(id))
    }

    /// The settings `current` starts with on its model (`current.model`): the
    /// person's saved choices for that model, else the model's own start
    /// ([`ModelInfo::starts_thinking`]) under the person's global
    /// `[generation]`. One reading for a model switch and a provider switch.
    fn start_on_model(
        &self,
        current: &mut ModelSettings,
        config: Option<&crate::settings::providers::ProviderConfig>,
        info: Option<&crate::settings::providers::ModelInfo>,
    ) {
        let api = config.map_or("", |c| c.api_kind());
        let saved = {
            let db = panic::lock(&self.rt.db);
            db.get_model_settings_json(&current.provider, &current.model)
                .ok()
                .flatten()
                .or_else(|| {
                    (!current.provider.is_empty())
                        .then(|| {
                            db.get_model_settings_json("", &current.model)
                                .ok()
                                .flatten()
                        })
                        .flatten()
                })
                .and_then(|j| serde_json::from_str::<ModelSettings>(&j).ok())
        };
        if let Some(saved) = saved {
            current.temperature = saved.temperature;
            current.top_p = saved.top_p;
            current.max_output = saved.max_output;
            current.frequency_penalty = saved.frequency_penalty;
            current.presence_penalty = saved.presence_penalty;
            current.thinking = saved.thinking;
            current.effort = saved.effort;
            current.stop = saved.stop;
        } else {
            current.temperature = None;
            current.top_p = None;
            current.max_output = None;
            current.frequency_penalty = None;
            current.presence_penalty = None;
            current.thinking = info.is_some_and(|m| m.starts_thinking(api));
            current.effort = info
                .filter(|_| current.thinking)
                .and_then(|m| m.starting_effort(api));
            current.stop = Vec::new();
            let generation = panic::lock(&self.settings).config.get("generation");
            let _ = current.apply_values(&generation, info, api);
        }
        if let Some(provider) = config {
            current.normalize_for_model(provider);
        }
    }

    /// Compute fresh inherited model settings from the most recently used session with a valid model.
    fn settings_for_fresh(&self, id: Option<i64>) -> Result<ModelSettings> {
        let inherited = {
            let recent_jsons = {
                let db = panic::lock(&self.rt.db);
                db.recent_settings_json(50).unwrap_or_default()
            };
            let providers = panic::lock(&self.settings);
            recent_jsons.into_iter().find_map(|j| {
                let s = serde_json::from_str::<ModelSettings>(&j).ok()?;
                if s.model.is_empty() {
                    return None;
                }
                let is_selectable = match providers.providers.get(&s.provider) {
                    Some(p) => p.models.is_empty() || p.model(&s.model).is_some(),
                    None => providers.providers.list.is_empty(),
                };
                if is_selectable {
                    Some(s)
                } else {
                    None
                }
            })
        };

        let mut fresh = if let Some(inherited) = inherited {
            inherited
        } else {
            panic::lock(&self.settings).defaults().unwrap_or_default()
        };

        {
            let held = panic::lock(&self.settings);
            let generation = held.config.get("generation");
            fresh.apply_values(&generation, None, "")?;
            if let Some(provider) = held.providers.get(&fresh.provider) {
                fresh.normalize_for_model(provider);
            }
        }
        if !fresh.provider.is_empty() && !fresh.model.is_empty() {
            if let Ok(Some(saved)) = panic::lock(&self.rt.db)
                .get_model_settings_json(&fresh.provider, &fresh.model)
                .map(|opt| opt.and_then(|j| serde_json::from_str::<ModelSettings>(&j).ok()))
            {
                fresh.temperature = saved.temperature;
                fresh.top_p = saved.top_p;
                fresh.max_output = saved.max_output;
                fresh.frequency_penalty = saved.frequency_penalty;
                fresh.presence_penalty = saved.presence_penalty;
                fresh.thinking = saved.thinking;
                fresh.effort = saved.effort;
                fresh.stop = saved.stop;
            }
        }
        if !fresh.provider.is_empty() {
            if let Some(provider) = panic::lock(&self.settings).providers.get(&fresh.provider) {
                fresh.normalize_for_model(provider);
            }
            if let Some(id) = id {
                let db = panic::lock(&self.rt.db);
                db.set_settings_json(id, &serde_json::to_string(&fresh)?)?;
            }
        }
        Ok(fresh)
    }

    /// Undo `file` (put it back the way it was before the session) or redo
    /// it (put back what its last undo replaced): one piece of work held
    /// through the session's admission, so no turn writes files halfway
    /// through it; a turn of its own, where what it overwrites is captured and
    /// the model is told.
    fn put_back(&self, session: i64, file: &str, redo: bool) -> Result<Option<Value>> {
        let s = self.session(session)?;
        let _busy = s
            .state
            .begin_guarded()
            .ok_or_else(|| Error::invalid("cannot restore while a turn is running"))?;
        let at = panic::lock(&self.rt.db).next_turn(session)?;
        let jail = s.state.jail();
        let checkpoints = &s.state.checkpoints;
        let write_barrier = s.state.files.write();
        let wrote = if redo {
            checkpoints.redo_file(session, file, &jail, &s.state.memory, at)?
        } else {
            checkpoints.restore_file(session, file, &jail, &s.state.memory, at)?
        };
        if wrote {
            // The model read or wrote it since; what it remembers of it is no
            // longer on disk. A restored AGENTS.md or memory index is told
            // here, not a second time as a context update.
            crate::session::context::written(&self.rt, &s.state, file);
            crate::session::context::restored(&self.rt, &s.state, at, file, redo)?;
        }
        drop(write_barrier);
        let file_changes = s.state.files.refresh(&self.rt, &s.state)?;
        Ok(Some(json!({
            "file": file,
            "restored": wrote,
            "file_changes": file_changes,
            "redos": checkpoints.redo_changes(session, &jail)?,
        })))
    }

    pub fn session(&self, id: i64) -> Result<Session> {
        self.sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("open session {id}")))
    }

    /// Called only by a host holding its exclusive socket ownership. Messages
    /// acknowledged before a crash are reconciled into history once; no tools
    /// or inference are replayed by restoring a pending journal.
    pub fn recover_session(&mut self, id: i64) -> Result<()> {
        let state = self.resume(id)?;
        let pending = panic::lock(&self.rt.db)
            .jsonl
            .composer(id)?
            .map(|composer| composer.pending)
            .unwrap_or_default();
        for pending in pending {
            turn::persist_prompt(
                &self.rt,
                &state.state,
                &pending.text,
                &pending.media,
                false,
                Some(pending.id),
            )?;
        }
        Ok(())
    }

    pub fn sandbox_of(&self, id: i64) -> Result<Arc<Sandbox>> {
        Ok(self.session(id)?.state.sandbox())
    }

    /// Post `event` on the engine's own event channel, behind everything the
    /// engine already emitted. A frontend that must say something *after* the
    /// last ack of the commands it handled (a host announcing its upgrade) needs
    /// that order; a side channel could overtake them.
    pub fn notify(&self, event: Event) {
        let _ = self.rt.events.send(event);
    }

    /// No provider/catalogue worker is still running. A host may only hand over
    /// when this holds, or it would join a slow network call after its clients
    /// were already told to reconnect.
    /// Nothing in flight: no turn running in any session, no provider or
    /// catalogue worker outstanding — the moment a live upgrade may take this
    /// engine's place. Background tasks do not count: they are handed over.
    pub fn idle(&mut self) -> bool {
        self.sessions.values().all(|s| !s.state.is_busy())
            && self.running.values().all(|turn| turn.is_finished())
            && self.catalogues_idle()
    }

    /// The sessions this engine holds open, in id order.
    pub fn open_sessions(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.sessions.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    pub fn catalogues_idle(&mut self) -> bool {
        self.catalogues.retain(|handle| !handle.is_finished());
        self.catalogues.is_empty()
    }

    /// Stop every running turn and wait for the threads to unwind.
    pub fn shutdown(&mut self) {
        for s in self.sessions.values() {
            s.state.files.stop();
            s.state.request_cancel();
        }
        self.broker.clear();
        for (_, handle) in self.running.drain() {
            let _ = handle.join();
        }
        for handle in self.catalogues.drain(..) {
            let _ = handle.join();
        }
        for session in self.sessions.values() {
            session.state.capabilities.close(&session.state.registry);
        }
        // The sessions close here: this process lets go of them, and a
        // session nobody else holds takes its scratch with it.
        let ids: Vec<i64> = self.sessions.keys().copied().collect();
        panic::lock(&self.rt.db)
            .session_leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.release_scratch(&ids);
    }

    /// Clear the scratch of every session in `ids` that nobody has open any
    /// more (owner, 2026-10-09: a session's temporary files go when the
    /// session closes). Called where a process lets go of a session — its
    /// engine shutting down, a host releasing one it served — so whichever
    /// process is the last to let go is the one that clears it.
    pub fn release_scratch(&self, ids: &[i64]) {
        for &id in ids {
            let project = panic::lock(&self.rt.db).get_session(id).map(|s| s.project);
            let Ok(project) = project else {
                continue;
            };
            let Some(folder) = crate::project::find(&self.home, std::path::Path::new(&project))
            else {
                continue;
            };
            remove_scratch_if_closed(&self.home, &folder.join("scratch").join(id.to_string()), id);
        }
    }

    /// Let go of every open session's background tasks for the build this
    /// process is about to `exec` into, still running (see
    /// [`crate::session::background`]). All or nothing across the sessions as
    /// well: when one session's cannot be handed over, what was already let go
    /// is taken back, and every task runs on here.
    pub fn release_tasks(&self) -> Result<Vec<crate::session::background::SessionTasks>> {
        let mut handed: Vec<crate::session::background::SessionTasks> = Vec::new();
        for (id, session) in &self.sessions {
            match session.state.background.release() {
                Ok(tasks) if tasks.is_empty() => {}
                Ok(tasks) => handed.push(crate::session::background::SessionTasks {
                    session: *id,
                    tasks,
                }),
                Err(error) => {
                    self.adopt_tasks(handed);
                    return Err(error);
                }
            }
        }
        Ok(handed)
    }

    /// Take over the background tasks the previous build handed on, each in
    /// its session — which must be open by now. What belongs to a session
    /// this engine does not hold is stopped rather than left running unseen.
    /// Returns what could not be taken over, for the log.
    pub fn adopt_tasks(
        &self,
        handed: Vec<crate::session::background::SessionTasks>,
    ) -> Vec<String> {
        let watch = crate::session::background::Watch::of(&self.rt.limits.config);
        let mut errors = Vec::new();
        for crate::session::background::SessionTasks { session, tasks } in handed {
            match self.sessions.get(&session) {
                Some(open) => errors.extend(open.state.background.adopt(tasks, watch.clone())),
                None => {
                    let orphans = std::sync::Arc::new(
                        crate::session::background::BackgroundTaskManager::new(),
                    );
                    orphans.stop_all();
                    for error in orphans.adopt(tasks, watch.clone()) {
                        errors.push(error);
                    }
                    orphans.stop_all();
                    errors.push(format!(
                        "session #{session} was not reopened; its background tasks were stopped"
                    ));
                }
            }
        }
        errors
    }

    /// Called by a session host once it holds its sessions and has taken over
    /// the background tasks the build before it handed on: every session open
    /// now, and every one it opens later, tells the model of each background
    /// task it started and never heard the end of — one that ended while no
    /// window was open, or one lost to a crash or a power cut. Only a host
    /// does this: a window's own engine never runs a session's tasks, and
    /// must not speak for them.
    pub fn settle_background(&mut self) {
        self.background_settled = true;
        for session in self.sessions.values() {
            session.state.background.settle_unreported();
        }
    }

    /// Drop the handles of turns that have already finished, so a long-lived
    /// engine does not accumulate them.
    pub fn reap(&mut self, detached: &[i64]) -> Vec<i64> {
        self.running.retain(|_, handle| !handle.is_finished());
        let mut released = Vec::new();
        for id in detached {
            if self
                .sessions
                .get(id)
                .is_some_and(|session| !session.state.is_busy() && !self.running.contains_key(id))
            {
                if let Some(session) = self.sessions.remove(id) {
                    session.state.files.stop();
                    session.state.background.stop_all();
                    session.state.capabilities.close(&session.state.registry);
                    panic::lock(&self.rt.db)
                        .session_leases
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(id);
                    self.provider_models.retain(|(session, _), _| session != id);
                    released.push(*id);
                }
            }
        }
        released
    }

    /// A line the host could not parse. Reported and then forgotten: one bad
    /// line from a frontend must not take the engine down with it.
    pub fn report_protocol_error(&self, message: &str) {
        self.emit(Event::Error {
            session: None,
            code: "protocol".into(),
            message: message.to_string(),
        });
    }

    /// Tell the frontend everything it needs to render the session's safety
    /// state: which mode, whether tools are confined, and what the kernel is
    /// actually enforcing.
    fn announce_session(&self, s: &Session) {
        let sandbox = s.state.sandbox();
        self.emit(Event::Session {
            session: s.id(),
            project: s.state.project.clone(),
            mode: s.state.mode().as_str().to_string(),
            confined: s.state.mode().confines(),
            sandbox: sandbox.report().clone(),
            classifier: crate::perm::classifier::session_status(&self.rt, &s.state)
                .unwrap_or(Value::Null),
        });
        let db = panic::lock(&self.rt.db);
        if let Ok(tasks) = db.tasks(s.id()) {
            if !tasks.is_empty() {
                self.emit(Event::Tasks {
                    session: s.id(),
                    tasks,
                });
            }
        }
    }

    fn ack(events: &crate::session::live::Bus, id: u64, result: Result<Option<Value>>) {
        let event = match result {
            Ok(data) => Event::Ack {
                id,
                ok: true,
                error: None,
                data,
            },
            Err(e) => Event::Ack {
                id,
                ok: false,
                error: Some(e.to_string()),
                data: None,
            },
        };
        let _ = events.send(event);
    }

    fn emit(&self, event: Event) {
        let _ = self.rt.events.send(event);
    }
}

/// Remove scratch directories nothing has touched for a day. A running session
/// keeps writing into its own, so age is a safe proxy for "abandoned".
/// The project's standing path grants, re-validated.
///
/// Re-validated rather than trusted: a grant is stored as the path the user read
/// on the prompt, and between then and now that path may have become a symlink,
/// been replaced by the home directory, or stopped existing. Anything that no
/// longer resolves is silently left out — a session that opens with one fewer
/// grant asks again, which is the safe end of that mistake.
pub fn standing_reach(
    db: &Arc<Mutex<Db>>,
    project: &str,
    forbidden: &crate::perm::reach::Forbidden,
) -> Vec<PathBuf> {
    use crate::perm::reach;
    let home = crate::platform::home_dir();
    let rows = {
        let guard = panic::lock(db);
        guard.grants(project).unwrap_or_default()
    };
    rows.into_iter()
        .filter(|g| g.tool == reach::GRANT_TOOL)
        .filter_map(|g| {
            reach::resolve(&g.pattern, home.as_deref())
                .ok()
                .map(|named| reach::candidate(named, home.as_deref(), forbidden).anchor)
        })
        .collect()
}

/// What a crash left: the scratch of a session nobody has open, older than
/// `storage.scratch_lifetime`. A session closed in an orderly way cleared its
/// own ([`Engine::release_scratch`]); this is for one whose last process died
/// without doing it.
fn sweep_stale_scratch(home: &std::path::Path, stale_after: std::time::Duration) {
    for folder in crate::project::all(home) {
        let Ok(sessions) = std::fs::read_dir(folder.path.join("scratch")) else {
            continue;
        };
        for entry in sessions.flatten() {
            // A folder a process took out of a session's way and died before
            // deleting: nothing will ever use it.
            if entry.file_name().to_string_lossy().starts_with(LEAVING) {
                remove_leaving(&entry.path());
                continue;
            }
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i64>().ok())
                .filter(|id| *id > 0)
            else {
                continue;
            };
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|age| age > stale_after).unwrap_or(false))
                .unwrap_or(false);
            if stale {
                remove_scratch_if_closed(home, &entry.path(), id);
            }
        }
    }
}

/// Remove `dir` — session `id`'s scratch, exactly that folder and nothing
/// beside it — when nobody has the session open. Whether it was removed.
///
/// "Nobody" is both of the session's locks free: its lease, held by every
/// process with the session open, and its host's ownership lock, which a host
/// keeps across the `exec` of an upgrade — when its lease is let go and its
/// background tasks, which write their temporary files here, run on. Both are
/// held while the folder is taken out of the session's way — renamed, which
/// is instant however much it holds — so a process opening the session
/// meanwhile waits only for that, and its sandbox makes the folder again,
/// empty. The renamed folder is then deleted with no lock held (a crash
/// part way leaves it to the next start's sweep). A folder already gone is
/// nothing to do.
fn remove_scratch_if_closed(home: &std::path::Path, dir: &std::path::Path, id: i64) -> bool {
    let free = |path: PathBuf| crate::lock::exclusive(&path, crate::lock::Wait::No).ok();
    let leaving = {
        let Some(_lease) = free(crate::store::run_files::lease(home, id)) else {
            return false;
        };
        let Some(_host) = free(crate::store::run_files::host_lock(home, id)) else {
            return false;
        };
        let leaving = dir.with_file_name(format!(
            "{LEAVING}{id}-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        // A link in the folder's place is renamed, never followed.
        if std::fs::rename(dir, &leaving).is_err() {
            return false;
        }
        leaving
    };
    remove_leaving(&leaving);
    true
}

/// The prefix of a scratch folder on its way out ([`remove_scratch_if_closed`]).
/// Never a number, so no session's scratch can be named like one.
const LEAVING: &str = ".leaving-";

/// Delete a folder taken out of a session's way. A link is removed as a link.
fn remove_leaving(path: &std::path::Path) {
    let _ = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => Ok(()),
    };
}

/// Run one tool with no model involved — same jail, same gate, same checkpoint.
///
/// This is what makes a frontend able to offer "run this command" as a button
/// without a second execution path growing next to the agent's one.
/// Identify one direct tool call, uniquely, for the audit trail.
///
/// A millisecond timestamp alone is not unique: two `tool.invoke` commands
/// arriving in the same millisecond produced the same id, the second insert hit
/// the primary key on `toolcalls`, and the *whole call* failed with a database
/// error — a frontend's "run this" button losing to nothing but timing. The
/// counter makes it unique in this process and the pid across processes sharing
/// one database; the timestamp is kept because a sortable, readable id is worth
/// the bytes.
fn next_direct_call_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "direct-{}-{}-{n}",
        crate::store::now_ms(),
        std::process::id()
    )
}

fn run_tool_directly(rt: &Runtime, st: &SessionState, name: &str, args: &Value) {
    let call_id = next_direct_call_id();

    let result = panic::catch("tool.invoke", || -> Result<crate::tools::Outcome> {
        // A tool run without a model is still a tool run: it gets its own turn
        // number so the audit trail, and any checkpoint it captures, sit
        // somewhere a `checkpoint.restore` can name.
        let turn = {
            let db = panic::lock(&rt.db);
            db.next_turn(st.id)?
        };
        call::run_direct(rt, st, turn, &call_id, name, args)
    })
    .unwrap_or_else(Err);

    let finished = match result {
        Err(Error::Cancelled) => call::Finished::stopped(),
        other => call::Finished::of(other),
    };

    // Release the session before announcing the result, for the same reason the
    // turn loop does: a frontend acts the moment it sees the result, and being
    // told "already running" by a flag that is about to clear is a race, not a
    // state. (Caught by the escape battery, which fires calls back to back.)
    st.end();

    if let Err(e) = call::report(rt, st, &call_id, &finished) {
        let _ = rt.events.send(Event::Error {
            session: Some(st.id),
            code: e.code().to_string(),
            message: e.to_string(),
        });
    }
}

/// A session's `background.jsonl`, in the store beside its transcript.
///
/// Its own handle on the files, never the database's lock: a record is
/// written when a task ends, and that can be a session's last owner letting
/// go of it (`SessionState` stops its tasks on drop) wherever that happens.
struct BackgroundJournal {
    store: crate::store::jsonl::JsonlStore,
    session: i64,
}

impl crate::session::background::BackgroundJournal for BackgroundJournal {
    fn append(&self, record: &crate::session::background::BackgroundRecord) {
        let _ = self.store.append_background(self.session, record);
    }

    fn read(&self) -> Vec<crate::session::background::BackgroundRecord> {
        self.store.read_background(self.session).unwrap_or_default()
    }
}

struct EngineNotifier {
    rt: Runtime,
    state: std::sync::Weak<SessionState>,
}

impl crate::session::background::SessionWakeNotifier for EngineNotifier {
    fn notify(&self, text: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if state.background.is_stopped() {
            return;
        }
        let admitted = state.admit(|| {
            Ok(crate::session::Queued {
                text: text.clone(),
                media: Vec::new(),
                committed: None,
                origin: crate::session::Origin::Rune,
            })
        });
        match admitted {
            Ok(crate::session::Admitted::Began(busy)) => {
                let rt = self.rt.clone();
                std::thread::spawn(move || {
                    let _busy = busy;
                    let _ = crate::session::turn::run_notification(&rt, &state, &text);
                });
            }
            // The work in progress hands it to the model with its next
            // request; the person sees it now.
            Ok(crate::session::Admitted::Queued) => {
                let _ = self.rt.events.send(Event::Notification {
                    session: state.id,
                    text,
                    at: crate::store::now_ms(),
                });
            }
            Err(_) => {}
        }
    }

    fn withdraw(&self, text: &str) {
        if let Some(state) = self.state.upgrade() {
            state.withdraw_queued(text);
        }
    }

    fn waiting(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let rt = self.rt.clone();
        std::thread::spawn(move || {
            // Whoever holds the session now takes the queue at its own next
            // decision point; only a free session is taken here.
            if let Some(busy) = state.begin_guarded() {
                let _busy = busy;
                let _ = crate::session::turn::run_queued(&rt, &state);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep finds scratch inside each project's folder and reads each
    /// session's lease under the same root the store keeps it: an open
    /// session's scratch survives however old it looks, an abandoned one goes.
    #[test]
    fn the_sweep_spares_an_open_sessions_scratch_and_clears_an_abandoned_one() {
        let home = std::env::temp_dir()
            .join("rune-test")
            .join(format!("scratch-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let project = home.join("work/p");
        std::fs::create_dir_all(&project).unwrap();
        let folder = crate::project::open(&home, &project).unwrap();
        let open = folder.join("scratch/7");
        let abandoned = folder.join("scratch/8");
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for dir in [&open, &abandoned] {
            std::fs::create_dir_all(dir).unwrap();
            crate::platform::fixture::set_modified(dir, hour_ago).unwrap();
        }
        let _lease =
            crate::lock::shared(&home.join("run/session_7.lock"), crate::lock::Wait::Yes).unwrap();
        sweep_stale_scratch(&home, std::time::Duration::from_secs(60));
        assert!(open.is_dir(), "an open session's scratch is in use");
        assert!(
            !abandoned.exists(),
            "an abandoned session's scratch is cleared"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    fn scratch_home(name: &str) -> (PathBuf, PathBuf) {
        let home = std::env::temp_dir()
            .join("rune-test")
            .join(format!("scratch-close-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let project = home.join("work/p");
        std::fs::create_dir_all(&project).unwrap();
        let folder = crate::project::open(&home, &project).unwrap();
        (home, folder.join("scratch"))
    }

    /// Owner 2026-10-09: a closed session's scratch goes — exactly its own
    /// folder: the parent and every other session's stay.
    #[test]
    fn a_closed_sessions_scratch_goes_and_nothing_beside_it() {
        let (home, scratch) = scratch_home("closed");
        std::fs::create_dir_all(scratch.join("7/deep/er")).unwrap();
        std::fs::write(scratch.join("7/deep/er/file"), "x").unwrap();
        std::fs::create_dir_all(scratch.join("8")).unwrap();
        std::fs::write(scratch.join("8/other"), "y").unwrap();
        assert!(remove_scratch_if_closed(&home, &scratch.join("7"), 7));
        assert!(!scratch.join("7").exists());
        assert_eq!(
            std::fs::read_to_string(scratch.join("8/other")).unwrap(),
            "y"
        );
        let left: Vec<_> = std::fs::read_dir(&scratch)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["8"], "nothing left on its way out");
        // Gone already: nothing to do, and nothing else is touched.
        assert!(!remove_scratch_if_closed(&home, &scratch.join("7"), 7));
        assert!(scratch.join("8/other").is_file());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A session still open anywhere keeps its scratch: its lease held by
    /// another process, or its host's lock held — as a host holds it across
    /// the `exec` of an upgrade while its background tasks run on.
    #[test]
    fn a_session_held_anywhere_keeps_its_scratch() {
        let (home, scratch) = scratch_home("held");
        std::fs::create_dir_all(scratch.join("7")).unwrap();
        {
            let _lease = crate::lock::shared(
                &crate::store::run_files::lease(&home, 7),
                crate::lock::Wait::Yes,
            )
            .unwrap();
            assert!(!remove_scratch_if_closed(&home, &scratch.join("7"), 7));
            assert!(scratch.join("7").is_dir());
        }
        {
            let _host = crate::lock::exclusive(
                &crate::store::run_files::host_lock(&home, 7),
                crate::lock::Wait::Yes,
            )
            .unwrap();
            assert!(!remove_scratch_if_closed(&home, &scratch.join("7"), 7));
            assert!(scratch.join("7").is_dir());
        }
        assert!(remove_scratch_if_closed(&home, &scratch.join("7"), 7));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A link in a scratch folder's place is removed as a link: what it points
    /// at is not Rune's to delete.
    #[test]
    fn a_link_in_a_scratch_folders_place_is_never_followed() {
        let (home, scratch) = scratch_home("link");
        let outside = home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("precious"), "keep").unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        crate::platform::fixture::symlink(&outside, scratch.join("7")).unwrap();
        assert!(remove_scratch_if_closed(&home, &scratch.join("7"), 7));
        assert!(std::fs::symlink_metadata(scratch.join("7")).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("precious")).unwrap(),
            "keep"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A folder left on its way out by a process that died is cleared at the
    /// next start, whatever its age.
    #[test]
    fn the_sweep_clears_a_folder_left_on_its_way_out() {
        let (home, scratch) = scratch_home("leaving");
        let leaving = scratch.join(format!("{LEAVING}7-1-2"));
        std::fs::create_dir_all(leaving.join("a")).unwrap();
        std::fs::create_dir_all(scratch.join("8")).unwrap();
        sweep_stale_scratch(&home, std::time::Duration::from_secs(3600));
        assert!(!leaving.exists());
        assert!(scratch.join("8").is_dir(), "a young scratch stays");
        let _ = std::fs::remove_dir_all(&home);
    }
}
