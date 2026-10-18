//! A session: one conversation, bound to one project directory.
//!
//! The folder holds four things and no more — the state a conversation needs
//! ([`SessionState`]), the machinery every session shares ([`Runtime`]), the
//! loop itself ([`turn`]), and the one place a tool call is run ([`call`],
//! shared with `tool.invoke`). The system prompt lives in [`prompt`] because it
//! is the one part of a turn that is prose rather than control flow.
//!
//! Threading model: a prompt runs on its own thread so the host keeps reading
//! stdin while it works. That is what makes `cancel` and `permission.reply`
//! arrive during a turn rather than after it — without it, the permission gate
//! would deadlock against the frontend that is supposed to answer it.

#[path = "Auxiliary.rs"]
pub mod auxiliary;
#[path = "BackgroundTask.rs"]
pub mod background;
pub mod cache_watch;
pub mod call;
pub mod compact;
pub(crate) mod compact_input;
pub mod context;
pub mod file_changes;
pub mod live;
pub mod log;
pub mod prompt;
pub(crate) mod sanitizer;
pub mod status;
pub(crate) mod text_batches;
pub mod title;
pub mod turn;
pub(crate) mod user_instructions;
pub mod view;
pub(crate) mod work;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::checkpoint::Checkpoints;
use crate::jail::Jail;
use crate::memory::Memory;
use crate::panic;
use crate::perm::{Gate, Mode};
use crate::provider::Provider;
use crate::sandbox::{Policy, Sandbox};
use crate::settings::ModelSettings;
use crate::store::Db;
use crate::tools::{InputSlot, Registry};

/// Largest terminal dimension the engine will accept. A `winsize` field is a
/// `u16`, so anything is *expressible*; this is the range that is meaningful.
/// Nonsense here does not crash anything, it just makes a program draw garbage —
/// which is exactly the kind of input that should be refused at the edge.
const MAX_TERM_DIMENSION: u16 = 1000;

/// Shared by every session in one engine.
pub struct Runtime {
    pub mcp: Arc<crate::capabilities::mcp::Runtime>,
    pub db: Arc<Mutex<Db>>,
    pub registry: Arc<Registry>,
    pub limits: Arc<crate::limits::Limits>,
    pub provider: Arc<dyn Provider>,
    pub settings: Arc<Mutex<crate::settings::Settings>>,
    pub events: live::Bus,
}

impl Clone for Runtime {
    fn clone(&self) -> Runtime {
        Runtime {
            mcp: self.mcp.clone(),
            db: Arc::clone(&self.db),
            registry: Arc::clone(&self.registry),
            limits: Arc::clone(&self.limits),
            provider: Arc::clone(&self.provider),
            settings: Arc::clone(&self.settings),
            events: self.events.clone(),
        }
    }
}

/// Everything one conversation owns.
///
/// The permission mode is the axis: it decides whether the jail confines, whether
/// the kernel sandbox is applied, and which prompts the gate skips. Changing it
/// rebuilds both — there is no way to be in full-access mode with a confining
/// jail, or in ask mode without a sandbox, because neither is expressible here.
/// A message waiting for the running turn's next decision point.
pub struct Queued {
    pub text: String,
    pub media: Vec<crate::media::Attachment>,
    /// The composer entry a steer was committed from, when it came from one.
    pub committed: Option<i64>,
    pub origin: Origin,
}

/// Who wrote a queued message — which decides the role it is stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A steer the user typed.
    User,
    /// A notification Rune raised (a background task ended).
    Rune,
}

/// Who may start work on a session, and what is waiting for the work in
/// progress.
///
/// # Why one lock
///
/// A message that arrives while the session is busy is queued for the work in
/// progress, one that arrives while it is free starts work of its own — and
/// that choice must exclude the work in progress deciding it has nothing left
/// to answer and letting go. With the flag and the queue apart, a background
/// task's wake or a typed steer could land after the turn's last look at the
/// queue and before it let go: stored for nobody, the model never answering it
/// (it looked stuck until someone typed again), and the next turn to begin
/// threw the queue away.
#[derive(Default)]
struct Admission {
    /// The busy period now running, or the last one. A [`BusyGuard`] lets go
    /// of its own period only, never of one begun after it was released.
    period: u64,
    /// A compaction asked for while the session was busy: run at the next
    /// request boundary, or before the session is let go.
    compact: bool,
    /// Messages for the work in progress to take at its next decision point —
    /// each kept as it was sent: three steers are three turns of history.
    queue: Vec<Queued>,
}

/// What became of a message offered to a session ([`SessionState::admit`]).
pub enum Admitted {
    /// Work is in progress: it takes the message at its next decision point.
    Queued,
    /// The session was free and is now held for the caller, which runs the
    /// message itself.
    Began(BusyGuard),
}

/// How a run of turns ends ([`SessionState::settle`]).
pub enum Settle {
    /// A compaction was asked for: run it, then settle again.
    Compact,
    /// Messages arrived for the run to answer: it goes on with them, still
    /// holding the session.
    Continue(Vec<Queued>),
    /// The session is free. What was still waiting is handed back to be stored
    /// unanswered — a run that was stopped or failed does not start again on
    /// its own.
    Released(Vec<Queued>),
}

pub struct SessionState {
    pub registry: Arc<Registry>,
    pub capabilities: Arc<crate::capabilities::Capabilities>,
    pub id: i64,
    pub classifier: Mutex<crate::perm::classifier::State>,
    config: crate::settings::configuration::Shared,
    pub questions: crate::perm::broker::Broker<crate::tools::questions::Reply>,
    max_input_bytes: usize,
    pub project: String,
    /// The confining jail. The mode decides whether it is used as-is or relaxed.
    base_jail: Jail,
    pub checkpoints: Arc<Checkpoints>,
    pub files: file_changes::Publisher,
    pub gate: Arc<Gate>,
    /// This project's notes. Told to the model through `context`; written by
    /// the agent through the ordinary file tools and the jail's annex.
    pub memory: Memory,
    pub tasks: Arc<crate::tasks::Tasks>,
    scratch: PathBuf,
    /// Where this session's confinement records what it leaves on disk
    /// (`sandbox::grants_ledger`), in Rune's home.
    grants_ledger: PathBuf,
    /// The question a confined command's connection to a program outside
    /// the workspace puts to the person ([`services_door`]).
    ask_service: crate::sandbox::AskService,
    /// What that question asks through, held here alone: the door reaches it
    /// only while this session lives.
    _door: Arc<Door>,
    /// Where this session keeps what a tool wrote to disk for it — output too
    /// long to show, a background command's log: its folder's `tools_result/`.
    pub results: PathBuf,
    /// Which model this conversation talks to, and how it is sampled. Per
    /// session rather than per install: two open conversations are usually two
    /// different jobs, and one temperature cannot be right for both.
    settings: Mutex<ModelSettings>,
    sandbox: Mutex<Confinement>,
    /// The mode the person chose. Only they change it ([`Self::set_mode`]).
    mode: Mutex<Mode>,
    /// Calls running now that the person allowed with full access
    /// ([`Self::lift`]). Confinement is dropped while any is; the mode is not
    /// touched, so a switch made meanwhile is the one in force afterwards.
    lifted: std::sync::atomic::AtomicUsize,
    db: Arc<Mutex<Db>>,
    /// The terminal currently running in this session, if any.
    pub input: Arc<InputSlot>,
    /// The size the next terminal opens at, and the size a running one is held
    /// at. Rows, columns.
    term: Mutex<(u16, u16)>,
    /// Paths outside the workspace the user has allowed this session to reach.
    ///
    /// Seeded from the project's standing grants when the session opens, and
    /// added to by an answered permission prompt. Held here rather than in the
    /// jail or the sandbox because both are **rebuilt** from it: they are two
    /// views of one decision, and a decision with two homes is a decision that
    /// drifts (see [`crate::perm::reach`]).
    reach: Mutex<Vec<PathBuf>>,
    /// The project's commands may reach local services: the person answered
    /// "always" for it. Seeded when the session opens and read again at every
    /// turn ([`Self::reseed_services`]), so a grant withdrawn elsewhere stops
    /// at the next turn and one given in another window starts there.
    standing_services: AtomicBool,
    /// Calls running now that the person allowed local services for this
    /// once ([`Self::open_services`]).
    services_held: std::sync::atomic::AtomicUsize,
    /// MCP servers whose folders outside the workspace the person refused in
    /// this session, with the folders refused: not put to them again until
    /// those change ([`crate::session::call::authorize_mcp_roots`]).
    pub(crate) refused_servers: Mutex<Vec<(String, Vec<PathBuf>)>>,
    /// Set to stop the running turn. Cleared when the next one starts.
    pub cancel: Arc<AtomicBool>,
    /// The tool call running right now and the flag that stops just that one.
    ///
    /// Separate from `cancel` because "stop this runaway command" and "abandon
    /// this whole turn" are different intentions, and a frontend that could only
    /// express the second had to throw away the work either way.
    ///
    /// More than one only while calls that merely read run together
    /// (`turn.rs`); each keeps its own flag, so a Stop aimed at one stops that
    /// one and a Stop of the turn reaches every one.
    current_calls: Mutex<Vec<(String, Arc<AtomicBool>)>>,
    /// True while a turn (or a direct tool call, a manual compaction, a
    /// capability operation) holds the session — a second prompt is refused
    /// rather than interleaved, because two loops sharing one history produce
    /// nonsense. Written only under [`Self::admission`]; read without it.
    busy: AtomicBool,
    /// Who may start work on this session, and what waits for the work in
    /// progress: one lock for both (see [`Admission`]).
    admission: Mutex<Admission>,
    /// Which models have proven to cache in this session.
    pub cache_watch: Mutex<cache_watch::CacheWatch>,
    /// Set when a tool of this session wrote the memory index; the next request
    /// checks the index against its budget (`context::before_request`).
    pub memory_written: AtomicBool,
    /// Fingerprints of the file-born parts of the context (`AGENTS.md`, the
    /// memory index) as this session's own writes left them, not yet carried
    /// by a context row: a change the model made itself is not news to it
    /// (`context::before_request`).
    pub sources_known: Mutex<std::collections::BTreeMap<String, String>>,
    /// The fingerprints of the files this session's attachments name, as last
    /// seen: a file is hashed again only when it moved
    /// (`media::Fingerprints`).
    pub attachment_prints: crate::media::Fingerprints,
    pub background: Arc<background::BackgroundTaskManager>,
    /// The configuration generation this session's capabilities, providers
    /// and classifier tickets were last brought up to (`turn.rs`): another
    /// reader taking the change in first must not make the session skip it.
    pub configuration_seen: std::sync::atomic::AtomicU64,
}

/// What a session needs to exist. A struct rather than nine arguments.
pub struct NewSession {
    pub registry: Arc<Registry>,
    pub capabilities: Arc<crate::capabilities::Capabilities>,
    /// The live configuration handle — never a copy, so a rule this session
    /// enforces later reads the value in force then.
    pub config: crate::settings::configuration::Shared,
    pub id: i64,
    pub project: String,
    pub jail: Jail,
    pub checkpoints: Arc<Checkpoints>,
    pub gate: Arc<Gate>,
    pub memory: Memory,
    pub scratch: PathBuf,
    /// The session folder's `tools_result/`.
    pub results: PathBuf,
    pub mode: Mode,
    pub settings: ModelSettings,
    /// The project's standing path grants, already validated. Empty for a
    /// project nobody has ever allowed outside.
    pub reach: Vec<PathBuf>,
    /// The project's standing grant of local services
    /// ([`crate::engine::standing_services`]).
    pub local_services: bool,
    pub db: Arc<Mutex<Db>>,
}

impl SessionState {
    pub fn new(n: NewSession) -> SessionState {
        let inputs = SandboxInputs {
            confined: n.mode.confines(),
            reach: n.reach.clone(),
            roots: n.capabilities.package_roots(),
            local_services: n.local_services,
        };
        // Rune's home: the store's root.
        let grants_ledger = crate::sandbox::grants_ledger(panic::lock(&n.db).jsonl.root());
        let cancel = Arc::new(AtomicBool::new(false));
        let (ask_service, door) = services_door(&n, &cancel);
        let sandbox = Confinement {
            sandbox: Arc::new(build_sandbox(
                &n.jail,
                &n.scratch,
                &grants_ledger,
                &ask_service,
                &inputs,
            )),
            inputs,
        };
        SessionState {
            registry: n.registry,
            capabilities: n.capabilities,
            id: n.id,
            classifier: Mutex::new(crate::perm::classifier::State::default()),
            config: n.config.clone(),
            questions: Default::default(),
            max_input_bytes: crate::limits::MAX_TOOL_ARG_BYTES,
            project: n.project,
            base_jail: n.jail,
            checkpoints: n.checkpoints,
            files: file_changes::Publisher::default(),
            gate: n.gate,
            memory: n.memory,
            tasks: Arc::new(crate::tasks::Tasks::new(
                Arc::clone(&n.db),
                n.config.clone(),
            )),
            scratch: n.scratch,
            grants_ledger,
            ask_service,
            _door: door,
            results: n.results,
            settings: Mutex::new(n.settings),
            sandbox: Mutex::new(sandbox),
            mode: Mutex::new(n.mode),
            lifted: std::sync::atomic::AtomicUsize::new(0),
            db: n.db,
            reach: Mutex::new(n.reach),
            standing_services: AtomicBool::new(n.local_services),
            services_held: std::sync::atomic::AtomicUsize::new(0),
            refused_servers: Mutex::new(Vec::new()),
            input: Arc::new(InputSlot::default()),
            term: Mutex::new((
                n.config.number("terminal.rows") as u16,
                n.config.number("terminal.columns") as u16,
            )),
            cancel,
            current_calls: Mutex::new(Vec::new()),
            busy: AtomicBool::new(false),
            admission: Mutex::new(Admission::default()),
            cache_watch: Mutex::default(),
            memory_written: AtomicBool::new(false),
            sources_known: Mutex::new(Default::default()),
            attachment_prints: Default::default(),
            background: Arc::new(background::BackgroundTaskManager::new()),
            configuration_seen: std::sync::atomic::AtomicU64::new(n.config.generation()),
        }
    }

    pub fn mode(&self) -> Mode {
        *panic::lock(&self.mode)
    }

    /// One complete snapshot per API request. A selection change takes effect
    /// on the next request without changing the stream already in flight.
    pub fn settings(&self) -> ModelSettings {
        panic::lock(&self.settings).clone()
    }

    pub fn set_settings(&self, settings: ModelSettings) {
        *panic::lock(&self.settings) = settings;
    }

    /// Switch modes. Persists the choice, rebuilds the sandbox, and takes effect
    /// on the next tool call — never mid-command.
    pub fn set_mode(&self, mode: Mode) -> crate::error::Result<()> {
        {
            let db = panic::lock(&self.db);
            db.set_mode(self.id, mode.as_str())?;
        }
        *panic::lock(&self.mode) = mode;
        self.sync_sandbox();
        Ok(())
    }

    /// Drop confinement for one call the person allowed with full access (a
    /// path such as `/` that no narrow rule can hold), until [`Self::unlift`].
    ///
    /// The mode is left alone on purpose. It used to be swapped to Full for the
    /// call and swapped back after, which put back the mode from *before* the
    /// call: a person who chose Ask while it ran found Auto in force — and
    /// every status and view said Full while it ran. Crate-private: only the
    /// call runner lifts, and only through its guard.
    pub(crate) fn lift(&self) {
        self.lifted.fetch_add(1, Ordering::AcqRel);
        self.sync_sandbox();
    }

    /// End one [`Self::lift`].
    pub(crate) fn unlift(&self) {
        self.lifted.fetch_sub(1, Ordering::AcqRel);
        self.sync_sandbox();
    }

    /// Let this session's commands reach local services while one call the
    /// person allowed them for runs, until [`Self::close_services`].
    /// Crate-private: only the call runner opens, and only through its guard.
    pub(crate) fn open_services(&self) {
        self.services_held.fetch_add(1, Ordering::AcqRel);
        self.sync_sandbox();
    }

    /// End one [`Self::open_services`].
    pub(crate) fn close_services(&self) {
        self.services_held.fetch_sub(1, Ordering::AcqRel);
        self.sync_sandbox();
    }

    /// The project's standing grant of local services as it is on record now:
    /// kept by an "always" answered here or in another window, gone when it
    /// was withdrawn.
    pub fn reseed_services(&self, standing: bool) {
        if self.standing_services.swap(standing, Ordering::AcqRel) != standing {
            self.sync_sandbox();
        }
    }

    /// Whether an MCP server whose profile names `roots` may start without
    /// asking: nothing is asked in Full, and elsewhere every root is inside
    /// the workspace, already reached, or allowed for this project.
    pub fn mcp_roots_held(&self, roots: &[PathBuf]) -> crate::error::Result<()> {
        if self.outside_roots(roots).is_empty() {
            return Ok(());
        }
        Err(crate::error::Error::Denied(
            "this MCP server reaches folders outside the workspace; it is asked about at the next turn"
                .into(),
        ))
    }

    /// The roots of an MCP profile a confining mode has to ask about.
    pub(crate) fn outside_roots(&self, roots: &[PathBuf]) -> Vec<PathBuf> {
        if !self.mode().confines() {
            return Vec::new();
        }
        let jail = self.jail();
        let sandbox = self.sandbox();
        roots
            .iter()
            .filter(|root| !jail.contains(root) && !sandbox.reachable(root))
            .cloned()
            .collect()
    }

    /// Whether this session's commands may reach local services right now.
    fn local_services(&self) -> bool {
        self.standing_services.load(Ordering::Acquire)
            || self.services_held.load(Ordering::Acquire) > 0
    }

    /// Whether what this session runs is confined right now: the person's mode
    /// confines and no call they allowed with full access is running.
    fn confined(&self) -> bool {
        self.mode().confines() && self.lifted.load(Ordering::Acquire) == 0
    }

    /// The paths this session may reach outside the workspace, right now.
    pub fn reach(&self) -> Vec<PathBuf> {
        panic::lock(&self.reach).clone()
    }

    /// Union the project's standing grants from the database into the live reach
    /// set, and rebuild both confinements if anything new arrived.
    ///
    /// Additive only, deliberately. A grant another Rune process persisted must
    /// take effect here without a restart — the user answered "always allow" in
    /// one window and the answer belongs to every window on this project. It
    /// never removes a path: a path currently held by a one-call widening would
    /// be revoked from under the running call, and a revocation is safe to defer
    /// to the next turn while a mid-call withdrawal is not.
    pub fn reseed_reach(&self, standing: &[PathBuf]) {
        if standing.is_empty() {
            return;
        }
        let added = {
            let mut held = panic::lock(&self.reach);
            let before = held.len();
            for path in standing {
                if !held.contains(path) {
                    held.push(path.clone());
                }
            }
            held.len() != before
        };
        if added {
            self.sync_sandbox();
        }
    }

    /// Add allowed paths and rebuild both confinements from the result.
    ///
    /// Returns the paths this call actually added, which is what
    /// [`SessionState::narrow`] must be given back for a one-call grant: a path
    /// that was already permitted must not be revoked by a later `narrow`.
    pub fn widen(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let added = {
            let mut held = panic::lock(&self.reach);
            let mut added = Vec::new();
            for path in paths {
                if !held.contains(path) {
                    held.push(path.clone());
                    added.push(path.clone());
                }
            }
            added
        };
        if !added.is_empty() {
            self.sync_sandbox();
        }
        added
    }

    /// Take back paths a one-call grant added.
    pub fn narrow(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        {
            let mut held = panic::lock(&self.reach);
            held.retain(|p| !paths.contains(p));
        }
        self.sync_sandbox();
    }

    /// Bring the kernel sandbox in line with the mode, the reach set and the
    /// skill roots as they are now, building a new ruleset only when one of them
    /// changed since the last build. Every change to them calls this, and the
    /// turn calls it each round for the one input nothing announces: a media
    /// directory an MCP tool creates mid-turn becomes a root the moment it exists.
    pub(crate) fn sync_sandbox(&self) {
        self.confine(false);
    }

    /// Build the ruleset again even when nothing it is built from changed. Once
    /// per turn: a rule for a path that did not exist at the last build was
    /// skipped (which denies it), and a path created since must not stay denied
    /// for the rest of the session.
    pub(crate) fn rebuild_sandbox(&self) {
        self.confine(true);
    }

    /// The one writer of the sandbox.
    ///
    /// A Landlock ruleset cannot be relaxed once applied, so widening means
    /// building a **new** one. Commands already running keep the ruleset they
    /// were spawned with, which is the honest behaviour: a grant answered now
    /// cannot retroactively cover a command that started before it.
    ///
    /// Read, build and store happen under one lock. Two threads change what the
    /// sandbox is built from — the turn (one-call grants, skill refreshes) and
    /// the command thread (`SessionMode`) — and a build that read the inputs
    /// before the other thread's change but stored after it would put an older,
    /// possibly wider, ruleset back in force.
    fn confine(&self, force: bool) {
        let mut held = panic::lock(&self.sandbox);
        let inputs = SandboxInputs {
            confined: self.confined(),
            reach: self.reach(),
            roots: self.capabilities.package_roots(),
            local_services: self.local_services(),
        };
        if !force && held.inputs == inputs {
            return;
        }
        held.sandbox = Arc::new(build_sandbox(
            &self.base_jail,
            &self.scratch,
            &self.grants_ledger,
            &self.ask_service,
            &inputs,
        ));
        held.inputs = inputs;
    }

    /// The jail in force right now.
    pub fn jail(&self) -> Jail {
        if self.confined() {
            self.base_jail.clone().with_reach(self.reach())
        } else {
            self.base_jail.clone().unconfined()
        }
    }

    pub fn sandbox(&self) -> Arc<Sandbox> {
        Arc::clone(&panic::lock(&self.sandbox).sandbox)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        // The calls in flight watch their own flags, not this one, so stopping
        // the turn has to reach in and set them too.
        for (_, flag) in panic::lock(&self.current_calls).iter() {
            flag.store(true, Ordering::Relaxed);
        }
        // Anyone parked on a permission question wakes up and unwinds.
        self.gate.broker().wake();
        self.questions.wake();
        self.capabilities.input.broker.wake();
    }

    /// Register the tool call about to run, and hand back the flag it watches.
    ///
    /// Seeded from the turn's own flag: a call that starts after the turn was
    /// cancelled is already cancelled, rather than running once more first.
    pub fn begin_call(&self, id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(self.cancel.load(Ordering::Relaxed)));
        panic::lock(&self.current_calls).push((id.to_string(), Arc::clone(&flag)));
        *panic::lock(&self.capabilities.input.operation) = Some((id.to_string(), flag.clone()));
        flag
    }

    pub fn end_call(&self, id: &str) {
        panic::lock(&self.current_calls).retain(|(call, _)| call != id);
        let mut operation = panic::lock(&self.capabilities.input.operation);
        if operation.as_ref().is_some_and(|(call, _)| call == id) {
            *operation = None;
        }
    }

    /// Stop one tool call by id, leaving the turn running. Returns whether that
    /// call was the one in flight — a frontend cancelling a call that has just
    /// finished has not done anything wrong.
    pub fn cancel_call(&self, id: &str) -> bool {
        let guard = panic::lock(&self.current_calls);
        match guard.iter().find(|(current, _)| current == id) {
            Some((_, flag)) => {
                flag.store(true, Ordering::Relaxed);
                // Only one question can be outstanding per session, and only a
                // call running on its own can have asked it.
                self.gate.broker().wake();
                self.capabilities.input.broker.wake();
                true
            }
            None => false,
        }
    }

    /// Open a busy period, under the admission lock. `None` when one is
    /// already open.
    fn open_period(&self, admission: &mut Admission) -> Option<u64> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        admission.period += 1;
        crate::perm::classifier::pin_config(self, &self.config.snapshot());
        self.cancel.store(false, Ordering::Relaxed);
        self.background.resume();
        Some(admission.period)
    }

    /// Let the session go — the work holding it ended. A message that arrived
    /// meanwhile and was never taken is answered now, in work of its own
    /// ([`background::BackgroundTaskManager::waiting`]): a wake that landed
    /// during a direct tool call used to wait, unseen, for the next prompt, and
    /// that prompt's start threw it away.
    pub fn end(&self) {
        let waiting = {
            let admission = panic::lock(&self.admission);
            self.busy.store(false, Ordering::Release);
            !admission.queue.is_empty()
        };
        if waiting {
            self.background.waiting();
        }
    }

    /// [`Self::end`], for the period a guard holds and no other.
    fn end_period(&self, period: u64) {
        let waiting = {
            let admission = panic::lock(&self.admission);
            if admission.period != period || !self.busy.load(Ordering::Acquire) {
                return;
            }
            self.busy.store(false, Ordering::Release);
            !admission.queue.is_empty()
        };
        if waiting {
            self.background.waiting();
        }
    }

    /// Offer a message: queued for the work in progress, or — the session
    /// being free — the session is taken for it and the caller runs it.
    ///
    /// `queued` builds the message only when it is queued (a steer stores its
    /// composer entry then), under the same lock that decides, so the work in
    /// progress cannot let go between the decision and the queueing.
    pub fn admit(
        self: &Arc<Self>,
        queued: impl FnOnce() -> crate::error::Result<Queued>,
    ) -> crate::error::Result<Admitted> {
        let mut admission = panic::lock(&self.admission);
        if self.busy.load(Ordering::Acquire) {
            admission.queue.push(queued()?);
            return Ok(Admitted::Queued);
        }
        let period = self
            .open_period(&mut admission)
            .ok_or_else(|| crate::error::Error::Internal("admission raced".into()))?;
        Ok(Admitted::Began(BusyGuard {
            state: Arc::clone(self),
            period,
        }))
    }

    /// Every queued message, in order — the work in progress taking them at a
    /// decision point.
    pub fn take_queued(&self) -> Vec<Queued> {
        panic::lock(&self.admission).queue.drain(..).collect()
    }

    /// Take a notification of Rune's back out of the queue while it still
    /// waits there unread — the first one whose text is `text` — because
    /// what it said is over (a wait answered or ended) or a result said it
    /// already. Whether it was still there.
    pub fn withdraw_queued(&self, text: &str) -> bool {
        let mut admission = panic::lock(&self.admission);
        let found = admission
            .queue
            .iter()
            .position(|queued| queued.origin == Origin::Rune && queued.text == text);
        found.map(|at| admission.queue.remove(at)).is_some()
    }

    /// How a run of turns ends: a compaction asked for meanwhile first, then
    /// whatever arrived — answered when `answerable` (the run ended on its
    /// own), handed back to be stored when it was stopped or failed — and only
    /// then is the session let go, in the same hold of the lock.
    pub fn settle(&self, answerable: bool) -> Settle {
        let mut admission = panic::lock(&self.admission);
        if std::mem::take(&mut admission.compact) {
            return Settle::Compact;
        }
        let queued: Vec<Queued> = admission.queue.drain(..).collect();
        if answerable && !queued.is_empty() {
            return Settle::Continue(queued);
        }
        self.busy.store(false, Ordering::Release);
        Settle::Released(queued)
    }

    /// Ask for a compaction at the next request boundary of the work in
    /// progress (or before it lets go).
    pub fn ask_compaction(&self) {
        panic::lock(&self.admission).compact = true;
    }

    /// Take a compaction asked for, if there is one.
    pub fn take_compaction(&self) -> bool {
        std::mem::take(&mut panic::lock(&self.admission).compact)
    }

    /// A compaction asked for from outside a turn: noted for the work in
    /// progress when the session is busy (`None`), or the session is taken for
    /// it now.
    pub fn compact_or_begin(self: &Arc<Self>) -> Option<BusyGuard> {
        let mut admission = panic::lock(&self.admission);
        if self.busy.load(Ordering::Acquire) {
            admission.compact = true;
            return None;
        }
        let period = self.open_period(&mut admission)?;
        Some(BusyGuard {
            state: Arc::clone(self),
            period,
        })
    }

    /// Takes the session for one turn, one direct tool call or one operation,
    /// and lets it go however the work ends — return, error, or a panic
    /// unwinding past the line that would have released it.
    ///
    /// Returns `None` when the session is already held. Work may release the
    /// session earlier itself (a turn does, before announcing `done`); the
    /// guard then lets go of nothing — least of all a period begun after it.
    pub fn begin_guarded(self: &Arc<Self>) -> Option<BusyGuard> {
        let mut admission = panic::lock(&self.admission);
        let period = self.open_period(&mut admission)?;
        Some(BusyGuard {
            state: Arc::clone(self),
            period,
        })
    }

    /// Type into the command that is running right now.
    ///
    /// Not an error when nothing is running — a frontend that sends a keystroke
    /// a moment after a command exited has not done anything wrong. It returns
    /// whether the input actually landed.
    pub fn write_input(&self, bytes: &[u8]) -> crate::error::Result<bool> {
        let limit = self.max_input_bytes;
        if bytes.len() > limit {
            return Err(crate::Error::invalid(
                "terminal input exceeds configured limit",
            ));
        }
        let guard = panic::lock(&self.input);
        match guard.as_ref() {
            None => Ok(false),
            Some(pty) => {
                pty.write_all(bytes)?;
                Ok(true)
            }
        }
    }

    pub fn term_size(&self) -> (u16, u16) {
        *panic::lock(&self.term)
    }

    /// Set the terminal size, and push it to the command running right now.
    ///
    /// Two halves on purpose: the stored size is what the *next* command opens
    /// at, and the ioctl is what makes the *current* one redraw. A frontend that
    /// resizes its window mid-build wants both. Returns whether a running
    /// command was resized — not an error when there is none, for the same
    /// reason [`SessionState::write_input`] is not.
    pub fn resize_terminal(&self, rows: u16, cols: u16) -> crate::error::Result<bool> {
        if rows == 0 || cols == 0 || rows > MAX_TERM_DIMENSION || cols > MAX_TERM_DIMENSION {
            return Err(crate::error::Error::invalid(format!(
                "terminal size must be 1..={MAX_TERM_DIMENSION} rows and columns"
            )));
        }
        *panic::lock(&self.term) = (rows, cols);
        let guard = panic::lock(&self.input);
        match guard.as_ref() {
            None => Ok(false),
            Some(pty) => {
                pty.resize(rows, cols)?;
                Ok(true)
            }
        }
    }

    /// Delete this session's scratch directory. Called when the session is
    /// deleted; the sandbox will recreate it if the session is ever rebuilt.
    pub fn remove_scratch(&self) -> crate::Result<()> {
        self.capabilities.remove_session_artifacts(&self.registry)?;
        let _ = std::fs::remove_dir_all(&self.scratch);
        Ok(())
    }
}

impl Drop for SessionState {
    fn drop(&mut self) {
        self.background.stop_all();
    }
}

/// Everything a sandbox is built from. Kept beside the sandbox it produced, so
/// a rebuild that would produce the same ruleset is skipped.
#[derive(PartialEq)]
struct SandboxInputs {
    confined: bool,
    reach: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    local_services: bool,
}

/// The sandbox in force and what it was built from.
struct Confinement {
    sandbox: Arc<Sandbox>,
    inputs: SandboxInputs,
}

fn build_sandbox(
    jail: &Jail,
    scratch: &Path,
    ledger: &Path,
    ask: &crate::sandbox::AskService,
    inputs: &SandboxInputs,
) -> Sandbox {
    let mut policy = Policy::for_project(jail.root(), scratch)
        .with_reach(inputs.reach.clone())
        .with_local_services(inputs.local_services)
        .asking_services(Some(ask.clone()))
        .recorded_in(ledger.to_path_buf());
    policy.reads.extend_from_slice(&inputs.roots);
    if inputs.confined {
        Sandbox::prepare(&policy)
    } else {
        Sandbox::full_access(&policy)
    }
}

/// The question a confined command's connection to a program running outside
/// the workspace — a container engine, the service manager, a key agent —
/// puts to the person, at the moment it connects (owner, 2026-10-17:
/// automatic, nothing to set up). The project's standing grant answers yes
/// without a question; otherwise the permission gate asks as it asks about
/// any reach outside the workspace — "always" files that grant. One question
/// at a time: connections waiting behind it read the grant an answer filed.
///
/// A supervisor answers for its command for as long as the command runs — a
/// background task outlives its window, a process that left its tree may
/// outlive the session — so the door holds the session's parts weakly: once
/// the session is gone nobody is asked and the connection is refused, and a
/// door never keeps the session's event stream open (a host that ends waits
/// for that stream to close).
fn services_door(
    n: &NewSession,
    cancel: &Arc<AtomicBool>,
) -> (crate::sandbox::AskService, Arc<Door>) {
    let door = Arc::new(Door {
        gate: Arc::clone(&n.gate),
        db: Arc::clone(&n.db),
        events: n.capabilities.input.events.clone(),
        cancel: Arc::clone(cancel),
    });
    let held = Arc::downgrade(&door);
    let project = n.project.clone();
    let session = n.id;
    let one_at_a_time = Arc::new(Mutex::new(()));
    let ask = crate::sandbox::AskService(Arc::new(move |call: &crate::platform::ServiceCall| {
        let Some(door) = held.upgrade() else {
            // The session is gone: nobody to ask.
            return false;
        };
        let standing = || {
            panic::lock(&door.db)
                .is_granted(
                    &project,
                    crate::perm::SERVICES_GRANT_TOOL,
                    crate::perm::SERVICES_GRANT_PATTERN,
                )
                .unwrap_or(false)
        };
        if standing() {
            return true;
        }
        let _turn = panic::lock(&one_at_a_time);
        if standing() {
            return true;
        }
        let Some(events) = &door.events else {
            // Nobody to ask.
            return false;
        };
        let served = call
            .server
            .as_deref()
            .map(|name| format!(" ({name})"))
            .unwrap_or_default();
        let action = format!("connect to {}{served}", call.address);
        let ask = crate::perm::Ask {
            session,
            tool: SERVICE_TOOL,
            action: &action,
            preview: format!(
                "A running command wants to talk to {}{served}, a program running outside the workspace with your rights.",
                call.address
            ),
            grant_key: crate::perm::SERVICES_GRANT_PATTERN.into(),
            mutates: true,
            runs: false,
            risk: crate::perm::denylist::Risk::Normal,
            reach: Vec::new(),
            local_services: true,
            unconfined: None,
            lifts: false,
        };
        let mut emit = |event: crate::proto::Event| {
            let _ = events.send(event);
        };
        // A confined command only: the mode confines, so it asks.
        door.gate
            .authorize(Mode::Auto, ask, &mut emit, &door.cancel)
            .is_ok()
    }));
    (ask, door)
}

/// What [`services_door`] asks through.
struct Door {
    gate: Arc<crate::perm::Gate>,
    db: Arc<Mutex<crate::store::Db>>,
    events: Option<crate::session::live::Bus>,
    cancel: Arc<AtomicBool>,
}

/// The name a connection's question goes by ([`services_door`]).
pub const SERVICE_TOOL: &str = "local_service";

/// A handle the engine keeps per open session.
#[derive(Clone)]
pub struct Session {
    pub state: Arc<SessionState>,
}

impl Session {
    pub fn new(state: SessionState) -> Session {
        Session {
            state: Arc::new(state),
        }
    }

    pub fn id(&self) -> i64 {
        self.state.id
    }
}

/// Holds a session's busy period for the life of one turn or one direct tool
/// call.
///
/// The flag used to be cleared by a statement after the work, which an unwind
/// skipped: the session then refused every later prompt with "already running a
/// turn", and `session.resume` hands back the same in-memory session, so only
/// restarting the host recovered it. It lets go of its own period only: a
/// guard dropped after its turn already released the session — and a new turn
/// began — used to clear that new turn's flag, letting a second one start
/// beside it.
pub struct BusyGuard {
    state: Arc<SessionState>,
    period: u64,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.state.end_period(self.period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real engine and one open session over a throwaway home.
    fn open(name: &str) -> (crate::engine::Engine, Arc<SessionState>, PathBuf) {
        let base = std::env::temp_dir().join(format!("rune-sbx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let provider = Arc::new(crate::provider::scripted::Scripted::new(Vec::new()));
        let cfg =
            crate::engine::Config::at(base.join("rune.db"), base.join("data"), provider).unwrap();
        let (mut engine, _events) = crate::engine::Engine::start(cfg).unwrap();
        let state = engine.open(project.to_str().unwrap()).unwrap().state;
        (engine, state, base)
    }

    fn steer(text: &str) -> Queued {
        Queued {
            text: text.into(),
            media: Vec::new(),
            committed: None,
            origin: Origin::User,
        }
    }

    /// Counts the times the session asked for its queue to be answered.
    #[derive(Default)]
    struct Waiting(std::sync::atomic::AtomicUsize);

    impl background::SessionWakeNotifier for Waiting {
        fn notify(&self, _text: String) {}
        fn waiting(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A message offered to a busy session waits for the work in progress, and
    /// that work does not let go while it holds something to answer: the
    /// window in which a steer or a wake was stored for nobody is gone.
    #[test]
    fn a_message_for_a_busy_session_is_answered_before_it_lets_go() {
        let (mut engine, st, base) = open("admission-queue");
        let guard = st.begin_guarded().expect("a free session");
        assert!(matches!(
            st.admit(|| Ok(steer("one more thing"))).unwrap(),
            Admitted::Queued
        ));
        match st.settle(true) {
            Settle::Continue(queued) => assert_eq!(queued[0].text, "one more thing"),
            _ => panic!("the run must go on with what arrived"),
        }
        assert!(
            st.is_busy(),
            "a run with something to answer keeps the session"
        );
        assert!(matches!(st.settle(true), Settle::Released(left) if left.is_empty()));
        assert!(!st.is_busy());
        drop(guard);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// A free session is taken by the message itself; nothing is queued for a
    /// run that does not exist.
    #[test]
    fn a_free_session_is_taken_by_the_message_itself() {
        let (mut engine, st, base) = open("admission-free");
        let Admitted::Began(guard) = st
            .admit(|| panic!("a free session queues nothing"))
            .unwrap()
        else {
            panic!("a free session must be taken for the message");
        };
        assert!(st.is_busy());
        drop(guard);
        assert!(!st.is_busy());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// Stop means stop: a run that was stopped lets go at once and hands back
    /// what was waiting, to be stored unanswered.
    #[test]
    fn a_stopped_run_lets_go_and_hands_back_what_waited() {
        let (mut engine, st, base) = open("admission-stop");
        let _guard = st.begin_guarded().unwrap();
        st.admit(|| Ok(steer("after the stop"))).unwrap();
        match st.settle(false) {
            Settle::Released(left) => assert_eq!(left.len(), 1),
            _ => panic!("a stopped run must let go"),
        }
        assert!(!st.is_busy());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// A turn lets the session go before it says `done`; its guard drops a
    /// moment later. If a new turn began in that moment, the old guard used to
    /// clear the new turn's flag, and a third could start beside it.
    #[test]
    fn an_old_guard_never_frees_a_newer_period() {
        let (mut engine, st, base) = open("admission-period");
        let old = st.begin_guarded().unwrap();
        assert!(matches!(st.settle(true), Settle::Released(_)));
        let newer = st.begin_guarded().expect("the session is free again");
        drop(old);
        assert!(st.is_busy(), "the old guard freed the newer period");
        assert!(st.begin_guarded().is_none());
        drop(newer);
        assert!(!st.is_busy());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// Work that is not a turn (a direct tool call, a manual compaction) never
    /// looks at the queue. When it lets go with a message still waiting, the
    /// message is handed on to be answered — and the next start keeps it
    /// instead of throwing it away.
    #[test]
    fn letting_go_with_a_message_waiting_has_it_answered_and_never_drops_it() {
        let (mut engine, st, base) = open("admission-waiting");
        let waiting = Arc::new(Waiting::default());
        st.background.set_notifier(waiting.clone());
        let guard = st.begin_guarded().unwrap();
        st.admit(|| Ok(steer("arrived during a direct call")))
            .unwrap();
        drop(guard);
        assert_eq!(
            waiting.0.load(Ordering::SeqCst),
            1,
            "nobody was asked to answer it"
        );
        let _next = st.begin_guarded().unwrap();
        assert_eq!(
            st.take_queued().len(),
            1,
            "starting work threw the message away"
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// A compaction asked for while busy runs before the session is let go.
    #[test]
    fn a_compaction_asked_for_while_busy_runs_before_the_session_is_let_go() {
        let (mut engine, st, base) = open("admission-compact");
        let _guard = st.begin_guarded().unwrap();
        assert!(st.compact_or_begin().is_none(), "busy: noted, not begun");
        assert!(matches!(st.settle(true), Settle::Compact));
        assert!(st.is_busy());
        assert!(matches!(st.settle(true), Settle::Released(_)));
        engine.shutdown();
        let _ = std::fs::remove_dir_all(base);
    }

    /// Calls running together each keep their own stop: one Stop aimed at a
    /// call reaches that call only, a Stop of the turn reaches every one, and a
    /// call that ends leaves the others registered.
    #[test]
    fn calls_in_flight_together_each_keep_their_own_stop() {
        let (mut engine, st, base) = open("calls");
        let a = st.begin_call("a");
        let b = st.begin_call("b");
        let c = st.begin_call("c");
        assert!(st.cancel_call("b"));
        assert!(!a.load(Ordering::Relaxed) && b.load(Ordering::Relaxed));
        assert!(!c.load(Ordering::Relaxed));
        st.end_call("b");
        assert!(!st.cancel_call("b"), "an ended call is not in flight");
        st.request_cancel();
        assert!(a.load(Ordering::Relaxed) && c.load(Ordering::Relaxed));
        st.end_call("a");
        st.end_call("c");
        assert!(!st.cancel_call("c"));
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }

    fn full_access(sandbox: &Sandbox) -> bool {
        sandbox
            .report()
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("full-access"))
    }

    /// A round that changed nothing the sandbox is built from builds nothing; a
    /// change to any input builds a new one.
    #[test]
    fn the_sandbox_is_rebuilt_only_when_what_it_is_built_from_changes() {
        let (mut engine, st, base) = open("sync");
        let before = st.sandbox();
        st.sync_sandbox();
        assert!(Arc::ptr_eq(&before, &st.sandbox()), "nothing changed");
        let outside = base.join("granted");
        std::fs::create_dir_all(&outside).unwrap();
        st.widen(std::slice::from_ref(&outside));
        assert!(!Arc::ptr_eq(&before, &st.sandbox()), "a grant rebuilds it");
        let widened = st.sandbox();
        st.rebuild_sandbox();
        assert!(
            !Arc::ptr_eq(&widened, &st.sandbox()),
            "a forced build always builds"
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The race this guards: the turn thread builds from the mode it read while
    /// the command thread switches it. Whatever the interleaving, once both stop
    /// the sandbox in force must match the mode in force.
    #[test]
    fn a_mode_switch_racing_the_turn_never_leaves_an_older_sandbox_in_force() {
        let (mut engine, st, base) = open("race");
        let switcher = {
            let st = Arc::clone(&st);
            std::thread::spawn(move || {
                for i in 0..200 {
                    let mode = if i % 2 == 0 { Mode::Full } else { Mode::Ask };
                    st.set_mode(mode).unwrap();
                }
            })
        };
        for _ in 0..200 {
            st.sync_sandbox();
            st.rebuild_sandbox();
        }
        switcher.join().unwrap();
        assert_eq!(full_access(&st.sandbox()), st.mode() == Mode::Full);
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }
}
