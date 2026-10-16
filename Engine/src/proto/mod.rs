//! The wire protocol between a frontend (RuneCli, later RuneStudio) and the engine.
//!
//! Two directions, both line-delimited JSON:
//!   frontend → engine : [`Command`], tagged by `"cmd"`
//!   engine   → frontend: [`Event`],   tagged by `"t"`
//!
//! Everything here is **pure data** on purpose. No handles, no closures, no
//! lifetimes. That is what keeps a future `dart:ffi` frontend possible without
//! reopening the agent loop: the same values cross a C ABI just as well as they
//! cross a pipe.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The wire version. Bumped whenever a frontend and an engine of different
/// builds could misread each other — a new variant either side would drop, a
/// changed field. A frontend compares it with the `protocol` its session host
/// reports and hands itself over to the installed build on a mismatch, instead
/// of silently skipping lines it cannot parse.
pub const PROTOCOL: u32 = 15;

// ═══════════════════════════════════════════════════════════════════════════════
// FRONTEND → ENGINE
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    #[serde(rename = "draft.open")]
    DraftOpen {
        id: u64,
        project: String,
        #[serde(default)]
        token: Option<String>,
    },
    #[serde(rename = "draft.check")]
    DraftCheck {
        id: u64,
        draft: Box<crate::store::drafts::Draft>,
    },
    #[serde(rename = "draft.save")]
    DraftSave {
        id: u64,
        draft: Box<crate::store::drafts::Draft>,
        base: u64,
        composer: Box<crate::store::composer::Composer>,
    },
    #[serde(rename = "draft.settings")]
    DraftSettings {
        id: u64,
        draft: Box<crate::store::drafts::Draft>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        value: String,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        custom: bool,
    },
    #[serde(rename = "draft.promote")]
    DraftPromote {
        id: u64,
        draft: Box<crate::store::drafts::Draft>,
    },
    #[serde(rename = "composer.get")]
    ComposerGet { id: u64, session: i64 },
    #[serde(rename = "composer.edit")]
    ComposerEdit {
        id: u64,
        session: i64,
        base: u64,
        changes: Vec<crate::store::composer::Edit>,
    },
    #[serde(rename = "prompt.committed")]
    PromptCommitted {
        id: u64,
        session: i64,
        token: String,
    },
    /// Open (or re-open) a project directory and start a fresh session in it.
    #[serde(rename = "session.open")]
    SessionOpen {
        #[serde(default)]
        id: u64,
        project: String,
        /// Interactive bare launches and `/new` must not attach to a sibling's
        /// empty session. Headless callers retain the empty-session reuse policy.
        #[serde(default)]
        fresh: bool,
    },

    /// List known sessions, newest first.
    #[serde(rename = "session.list")]
    SessionList {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        project: Option<String>,
    },

    /// This frontend is about to `exec` a newer build of itself and connect
    /// again. A session host that loses its last frontend stops that
    /// session's work — nobody is left to collect it — unless the frontend
    /// said this first: then the work runs on while it comes back, for as
    /// long as an idle session is kept (`host.idle_lifetime`).
    #[serde(rename = "session.returning")]
    SessionReturning {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Re-attach to an existing session and replay nothing — history is fetched
    /// separately so a frontend can page it.
    #[serde(rename = "session.resume")]
    SessionResume {
        #[serde(default)]
        id: u64,
        session: i64,
        #[serde(default)]
        project: Option<String>,
    },

    /// Stored messages of a session, oldest first.
    #[serde(rename = "session.history")]
    SessionHistory {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Give a session a new label.
    #[serde(rename = "session.rename")]
    SessionRename {
        #[serde(default)]
        id: u64,
        session: i64,
        title: String,
    },

    /// Update the recorded active duration of a session in seconds.
    #[serde(rename = "session.touch")]
    SessionTouch {
        #[serde(default)]
        id: u64,
        session: i64,
        active_seconds: i64,
    },

    /// Trigger compaction of the session history.
    #[serde(rename = "session.compact")]
    SessionCompact {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Query the number of active clients/connections attached to this session.
    #[serde(rename = "session.clients")]
    SessionClients {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// The session's plan as it stands. For a frontend that connected after the
    /// last `tasks` event.
    #[serde(rename = "session.tasks")]
    SessionTasks {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// The audit trail: every tool this session ran, newest first.
    ///
    /// Separate from `session.history` because they answer different questions.
    /// History is what the *model* was told; this is what the *engine did to the
    /// machine* — including calls made with `tool.invoke`, which never reach the
    /// conversation at all.
    /// The newest `limit` lines of a session's log, oldest first.
    #[serde(rename = "session.logs")]
    SessionLogs {
        #[serde(default)]
        id: u64,
        session: i64,
        limit: usize,
    },

    /// Everything a screen is made of, as the engine holds it at one definite
    /// place in the event stream: the answer is sent *on the event stream*, so
    /// the events before it are already inside the view and the events after it
    /// are not. A frontend that joins a session — or opens it again after its
    /// connection was lost — drops what it received before the answer and
    /// applies what follows. See `session::view`.
    #[serde(rename = "session.view")]
    SessionView {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// What the model holds that the conversation does not show: the system
    /// prompt, the tools declared, the messages Rune wrote into the session
    /// (the opening context, status updates, notices, notifications, skill and
    /// MCP prompts), the summary of a compressed past, and the values in
    /// `<status>` now. Read only; it changes nothing.
    #[serde(rename = "session.context")]
    SessionContext {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    #[serde(rename = "session.toolcalls")]
    SessionToolcalls {
        #[serde(default)]
        id: u64,
        session: i64,
        /// How many to return. Defaults to 200, capped at 1000.
        #[serde(default)]
        limit: Option<usize>,
    },

    /// The user's message. It begins a turn, or — while the session is busy
    /// with other work — waits for that work and is taken at its next decision
    /// point, decided by the engine under one lock (the same admission as
    /// `prompt.steer`): no frontend has to know whether the session is busy.
    #[serde(rename = "prompt")]
    Prompt {
        #[serde(default)]
        id: u64,
        session: i64,
        text: String,
        /// Files the user attached to this question, by path.
        ///
        /// Paths rather than bytes even over the protocol: the frontend and the
        /// engine are on the same machine, and a host that were not could not
        /// send a 4 MB base64 blob down a line-delimited channel anyway.
        #[serde(default)]
        attach: Vec<String>,
    },

    /// The user's message, sent to steer a running turn: taken at its next
    /// decision point, or — when nothing is running — the start of a turn of
    /// its own. The same admission as `prompt`.
    #[serde(rename = "prompt.steer")]
    PromptSteer {
        #[serde(default)]
        id: u64,
        session: i64,
        text: String,
        #[serde(default)]
        attach: Vec<String>,
    },

    /// Cancel the running turn of a session. Safe to send when nothing is running.
    #[serde(rename = "cancel")]
    Cancel {
        #[serde(default)]
        id: u64,
        session: i64,
        /// Stop one tool call rather than the whole turn. The id is the one the
        /// `tool_call` event carried. Omit it to abandon the turn.
        #[serde(default)]
        call: Option<String>,
    },

    /// Every project folder under the engine's home: the path it belongs to,
    /// whether that path is still there, its size and its sessions. Answered
    /// with `{"folders": [StorageFolder]}`.
    #[serde(rename = "storage.list")]
    StorageList {
        #[serde(default)]
        id: u64,
    },

    /// Remove the named project folders whose project path is gone, with every
    /// session, undo history and grant that belonged to them. A folder whose
    /// project is still there, or that a session holds open, is kept and
    /// says why. Answered with [`StoragePruned`].
    #[serde(rename = "storage.prune")]
    StoragePrune {
        #[serde(default)]
        id: u64,
        /// Folder names exactly as `storage.list` gave them.
        folders: Vec<String>,
    },

    /// Delete a session and everything it owns: messages, tool calls, undo
    /// history, and its scratch directory.
    #[serde(rename = "session.delete")]
    SessionDelete {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Change how much the session trusts the agent: `ask`, `auto`, or `full`.
    #[serde(rename = "session.mode")]
    SessionMode {
        #[serde(default)]
        id: u64,
        session: i64,
        mode: String,
    },

    /// Standing grants of this session's project.
    #[serde(rename = "permission.list")]
    PermissionList {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Withdraw a standing grant. Omit `pattern` to withdraw every grant of that
    /// tool; omit `tool` as well to clear the project entirely.
    #[serde(rename = "permission.revoke")]
    PermissionRevoke {
        #[serde(default)]
        id: u64,
        session: i64,
        #[serde(default)]
        tool: Option<String>,
        #[serde(default)]
        pattern: Option<String>,
    },

    #[serde(rename = "questions.reply")]
    QuestionsReply {
        id: u64,
        session: i64,
        req: String,
        reply: crate::tools::questions::Reply,
    },

    /// Answer an outstanding [`Event::PermissionRequest`].
    #[serde(rename = "permission.reply")]
    PermissionReply {
        #[serde(default)]
        id: u64,
        req: String,
        decision: Decision,
    },

    /// Run one tool directly, with no model in the loop. Used by the CLI and by
    /// the test harness; it goes through the identical jail + permission path.
    #[serde(rename = "tool.invoke")]
    ToolInvoke {
        #[serde(default)]
        id: u64,
        session: i64,
        name: String,
        args: Value,
    },

    /// Type into the command running in this session — a password, a `y`, or
    /// `"\u0003"` for Ctrl-C. Without it, anything that prompts just hangs.
    #[serde(rename = "tool.input")]
    ToolInput {
        #[serde(default)]
        id: u64,
        session: i64,
        data: String,
    },

    /// Tool declarations the engine would send to a model.
    /// Tell the engine how big the frontend's terminal is.
    ///
    /// Applies to the next command, and to the one running right now.
    #[serde(rename = "tool.resize")]
    ToolResize {
        #[serde(default)]
        id: u64,
        session: i64,
        rows: u16,
        cols: u16,
    },

    #[serde(rename = "tool.list")]
    ToolList {
        #[serde(default)]
        id: u64,
    },

    #[serde(rename = "capability.manage")]
    CapabilityManage {
        id: u64,
        session: i64,
        request: crate::capabilities::manage::Request,
    },
    #[serde(rename = "mcp.input.reply")]
    McpInputReply {
        id: u64,
        session: i64,
        req: String,
        reply: crate::capabilities::input::Reply,
    },

    /// Every file the session changed, in first-touch order, as it stands
    /// now against how it was before the session first touched it — the undo
    /// list: `{ "files": [FileChange] }`, each with when it was last changed.
    #[serde(rename = "checkpoint.files")]
    CheckpointFiles {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Trim undo history to the newest `keep` turns and free what is left over.
    /// Never automatic: undo history has no limit of its own (owner,
    /// 2026-10-11). Without `keep`, only what no snapshot names is freed.
    #[serde(rename = "checkpoint.prune")]
    CheckpointPrune {
        #[serde(default)]
        id: u64,
        session: i64,
        #[serde(default)]
        keep: Option<usize>,
    },

    /// What `checkpoint.restore` of `file` would undo, hunk by hunk: the
    /// file before the session first touched it against the file as it is
    /// now — every later change of it together.
    #[serde(rename = "checkpoint.diff")]
    CheckpointDiff {
        #[serde(default)]
        id: u64,
        session: i64,
        /// One file of `checkpoint.files`, by the path it is listed under.
        file: String,
    },

    /// Put `file` back the way it was before the session first touched it
    /// (undo is per file). What it holds now is captured first, as for any
    /// write. The model is told which file was put back.
    #[serde(rename = "checkpoint.restore")]
    CheckpointRestore {
        #[serde(default)]
        id: u64,
        session: i64,
        file: String,
    },

    /// Every file of the session there is a redo for — undone, and not put
    /// back since — in first-touch order: `{ "files": [FileChange] }`, each
    /// counting what redoing it would change, `last_at` when it was undone.
    #[serde(rename = "checkpoint.redos")]
    CheckpointRedos {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// What `checkpoint.redo` of `file` would change: the file as it is now
    /// against the file as its last undo found it.
    #[serde(rename = "checkpoint.redo_diff")]
    CheckpointRedoDiff {
        #[serde(default)]
        id: u64,
        session: i64,
        file: String,
    },

    /// Put back what the last undo of `file` overwrote, whatever happened to
    /// the file since, and use the redo up. What the file holds now is
    /// captured first, as for any write. The model is told.
    #[serde(rename = "checkpoint.redo")]
    CheckpointRedo {
        #[serde(default)]
        id: u64,
        session: i64,
        file: String,
    },

    /// Configured providers, the presets Rune knows, and which keys are stored.
    /// Never returns a key itself.
    #[serde(rename = "provider.list")]
    ProviderList {
        #[serde(default)]
        id: u64,
    },

    /// Add a provider — and prove it works before storing it.
    #[serde(rename = "provider.add")]
    ProviderAdd {
        #[serde(default)]
        id: u64,
        name: String,
        /// A known preset (`mimo`, `openai`, `openrouter`, `ollama`). Defaults to
        /// `name`, so `provider.add {name: "mimo"}` does the obvious thing.
        #[serde(default)]
        preset: Option<String>,
        /// For an endpoint Rune has never heard of, or to override a preset.
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        key_name: Option<String>,
        #[serde(default)]
        key: Option<String>,
        /// Model to probe with, when the preset lists none.
        #[serde(default)]
        model: Option<String>,
    },

    #[serde(rename = "provider.remove")]
    ProviderRemove {
        #[serde(default)]
        id: u64,
        name: String,
    },

    /// Store a credential by name. Also how the web-search key is set.
    #[serde(rename = "provider.key")]
    ProviderKey {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        key_name: Option<String>,
        key: String,
    },

    /// Show the request a provider would receive, without sending it.
    #[serde(rename = "provider.dryrun")]
    ProviderDryRun {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        model: Option<String>,
    },

    /// How long this provider's calls may take. `null` restores the default.
    #[serde(rename = "provider.timeout")]
    ProviderTimeout {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        seconds: Option<u64>,
    },

    /// What keys a name holds, masked, and how many are usable right now.
    #[serde(rename = "provider.keys")]
    ProviderKeys {
        #[serde(default)]
        id: u64,
        name: String,
    },

    /// Drop one key from a ring, by the position `provider.keys` reported.
    #[serde(rename = "provider.dropkey")]
    ProviderDropKey {
        #[serde(default)]
        id: u64,
        name: String,
        position: usize,
    },

    /// Set a credential store that cannot be opened aside, with its master
    /// key, into `backups/Credentials-<time>/`, and start a new one. Only ever
    /// sent on the person's explicit, confirmed choice.
    #[serde(rename = "credentials.new")]
    CredentialsNew {
        #[serde(default)]
        id: u64,
    },

    /// Drop one model from a provider's configured/remembered list.
    #[serde(rename = "provider.dropmodel")]
    ProviderDropModel {
        #[serde(default)]
        id: u64,
        provider: String,
        model: String,
    },

    /// Configure model metadata (context window, max output, price, etc.).
    #[serde(rename = "provider.configuremodel")]
    ProviderConfigureModel {
        #[serde(default)]
        id: u64,
        provider: String,
        model: Box<crate::settings::providers::ModelInfo>,
    },

    /// Which provider a new session opens with.
    #[serde(rename = "provider.default")]
    ProviderDefault {
        #[serde(default)]
        id: u64,
        name: String,
    },

    /// One real request, to find out whether a provider actually works.
    #[serde(rename = "provider.test")]
    ProviderTest {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        model: Option<String>,
    },

    /// The models a provider offers, asked of the provider itself.
    #[serde(rename = "provider.models")]
    ProviderModels {
        #[serde(default)]
        id: u64,
        name: String,
    },

    /// Where a model connection is reached. `null` puts back its preset's
    /// address. Proven by one real request before it is saved.
    #[serde(rename = "provider.baseurl")]
    ProviderBaseUrl {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        base_url: Option<String>,
    },

    /// Save account access only; never chooses or proves a tool request.
    #[serde(rename = "service.account")]
    ServiceAccount {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        api: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        fetch_url: Option<String>,
        #[serde(default)]
        key_name: Option<String>,
        #[serde(default)]
        key: Option<String>,
    },

    /// Choose one exact saved provider after a real request, or remove the pin.
    #[serde(rename = "service.select")]
    ServiceSelect {
        #[serde(default)]
        id: u64,
        service: String,
        #[serde(default)]
        target: Option<String>,
    },

    /// A service connection as it should stand: added when new, changed when
    /// not. Each service it switches on or changes is proven by one real
    /// request before anything is saved; switching every service off keeps
    /// the account's access and credentials. Removal is `service.remove`.
    #[serde(rename = "service.save")]
    ServiceSave {
        #[serde(default)]
        id: u64,
        name: String,
        /// The vendor; the name when absent.
        #[serde(default)]
        api: Option<String>,
        /// An address of the person's own; the vendor's when absent.
        #[serde(default)]
        base_url: Option<String>,
        /// Fetch's own address, for a vendor whose fetch has a host of its own.
        #[serde(default)]
        fetch_url: Option<String>,
        #[serde(default)]
        search: Option<ServiceUse>,
        #[serde(default)]
        fetch: Option<ServiceUse>,
        /// A gateway's routes to keep, per service; absent keeps the ones
        /// kept now. A route in use is always kept, and a route not kept
        /// before is proven by one real request before it is.
        #[serde(default)]
        routes: Option<ServiceRoutes>,
        #[serde(default)]
        zone: Option<String>,
        #[serde(default)]
        key_name: Option<String>,
        /// A key to store before the check; the ring as it was comes back if
        /// the check fails.
        #[serde(default)]
        key: Option<String>,
    },

    /// One real request through a saved service connection.
    #[serde(rename = "service.test")]
    ServiceTest {
        #[serde(default)]
        id: u64,
        name: String,
        /// `search` or `fetch`.
        service: String,
    },

    /// Remove a service connection. Its keys stay in the vault.
    #[serde(rename = "service.remove")]
    ServiceRemove {
        #[serde(default)]
        id: u64,
        name: String,
    },

    /// What a vendor offers to choose from before saving: a gateway's routes,
    /// an account's SERP zones — asked with the key about to be saved, or the
    /// stored one.
    #[serde(rename = "service.options")]
    ServiceOptions {
        #[serde(default)]
        id: u64,
        name: String,
        #[serde(default)]
        api: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        key: Option<String>,
    },

    /// This session's model settings, the ranges they accept, and anything the
    /// provider will ignore.
    #[serde(rename = "settings.get")]
    SettingsGet {
        #[serde(default)]
        id: u64,
        session: i64,
    },

    /// Change one setting of one session. `key` is a knob name or `model`.
    #[serde(rename = "settings.set")]
    SettingsSet {
        #[serde(default)]
        id: u64,
        session: i64,
        key: String,
        value: String,
    },

    /// Select a provider/model pair in one validated session update.
    #[serde(rename = "settings.select")]
    SettingsSelect {
        #[serde(default)]
        id: u64,
        session: i64,
        provider: String,
        model: String,
        #[serde(default)]
        custom: bool,
    },

    /// Close every session and stop the host loop.
    #[serde(rename = "shutdown")]
    Shutdown {
        #[serde(default)]
        id: u64,
    },
}

impl Command {
    pub fn request_id(&self) -> u64 {
        match self {
            Command::DraftOpen { id, .. }
            | Command::DraftCheck { id, .. }
            | Command::DraftSave { id, .. }
            | Command::DraftSettings { id, .. }
            | Command::DraftPromote { id, .. }
            | Command::ComposerGet { id, .. }
            | Command::ComposerEdit { id, .. }
            | Command::PromptCommitted { id, .. }
            | Command::SessionOpen { id, .. }
            | Command::SessionList { id, .. }
            | Command::SessionResume { id, .. }
            | Command::SessionReturning { id, .. }
            | Command::SessionHistory { id, .. }
            | Command::SessionLogs { id, .. }
            | Command::SessionContext { id, .. }
            | Command::SessionView { id, .. }
            | Command::SessionToolcalls { id, .. }
            | Command::SessionRename { id, .. }
            | Command::SessionTouch { id, .. }
            | Command::SessionCompact { id, .. }
            | Command::SessionClients { id, .. }
            | Command::SessionTasks { id, .. }
            | Command::Prompt { id, .. }
            | Command::PromptSteer { id, .. }
            | Command::Cancel { id, .. }
            | Command::StorageList { id }
            | Command::StoragePrune { id, .. }
            | Command::SessionDelete { id, .. }
            | Command::SessionMode { id, .. }
            | Command::CheckpointPrune { id, .. }
            | Command::PermissionList { id, .. }
            | Command::PermissionRevoke { id, .. }
            | Command::PermissionReply { id, .. }
            | Command::QuestionsReply { id, .. }
            | Command::ToolInvoke { id, .. }
            | Command::ToolInput { id, .. }
            | Command::ToolResize { id, .. }
            | Command::ToolList { id }
            | Command::CapabilityManage { id, .. }
            | Command::McpInputReply { id, .. }
            | Command::CheckpointFiles { id, .. }
            | Command::CheckpointDiff { id, .. }
            | Command::CheckpointRestore { id, .. }
            | Command::CheckpointRedos { id, .. }
            | Command::CheckpointRedoDiff { id, .. }
            | Command::CheckpointRedo { id, .. }
            | Command::ProviderList { id }
            | Command::ProviderAdd { id, .. }
            | Command::ProviderRemove { id, .. }
            | Command::ProviderKey { id, .. }
            | Command::ProviderTimeout { id, .. }
            | Command::ProviderDryRun { id, .. }
            | Command::ProviderKeys { id, .. }
            | Command::ProviderDropKey { id, .. }
            | Command::CredentialsNew { id }
            | Command::ProviderDropModel { id, .. }
            | Command::ProviderConfigureModel { id, .. }
            | Command::ProviderDefault { id, .. }
            | Command::ProviderTest { id, .. }
            | Command::ProviderModels { id, .. }
            | Command::ProviderBaseUrl { id, .. }
            | Command::ServiceAccount { id, .. }
            | Command::ServiceSelect { id, .. }
            | Command::ServiceSave { id, .. }
            | Command::ServiceTest { id, .. }
            | Command::ServiceRemove { id, .. }
            | Command::ServiceOptions { id, .. }
            | Command::SettingsGet { id, .. }
            | Command::SettingsSet { id, .. }
            | Command::SettingsSelect { id, .. }
            | Command::Shutdown { id } => *id,
        }
    }
}

/// One service switched on for a connection, as `service.save` wants it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServiceUse {
    /// The gateway route in use (9router: a vendor's id such as `exa`, or a
    /// combo's name); none for a vendor that has none.
    #[serde(default)]
    pub route: Option<String>,
}

/// The routes a gateway connection keeps, per service, as `service.save`
/// wants them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServiceRoutes {
    #[serde(default)]
    pub search: Vec<String>,
    #[serde(default)]
    pub fetch: Vec<String>,
}

/// What the user answered to a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow this one call only.
    AllowOnce,
    /// Allow, and remember for this project until revoked.
    AllowAlways,
    /// Refuse this call.
    Deny,
}

// ═══════════════════════════════════════════════════════════════════════════════
// ENGINE → FRONTEND
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Event {
    #[serde(rename = "composer")]
    Composer {
        session: i64,
        request: u64,
        composer: Box<crate::store::composer::Composer>,
    },
    #[serde(rename = "mcp_input_request")]
    McpInputRequest {
        session: i64,
        id: String,
        req: String,
        server_id: String,
        request: Value,
    },
    #[serde(rename = "mcp_input_resolved")]
    McpInputResolved { session: i64, req: String },
    /// First line the engine ever writes. Reports what it can actually enforce —
    /// a frontend must be able to tell the user when the kernel sandbox is off.
    #[serde(rename = "ready")]
    Ready {
        version: String,
        /// [`PROTOCOL`], so a frontend knows which contract it is talking to.
        protocol: u32,
        sandbox: SandboxReport,
    },

    /// `rune -host` is about to become the installed release, in the same
    /// process, on the same stdin and stdout. Sent after the ack of every
    /// command it accepted: a frontend keeps every command it has no ack for
    /// and sends it again once the successor's `ready` arrives. The sessions
    /// it held are open again in the successor, and their background tasks
    /// still running.
    #[serde(rename = "upgrading")]
    Upgrading,

    /// Correlates with `Command::request_id`. `ok=false` carries `error`.
    #[serde(rename = "ack")]
    Ack {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },

    #[serde(rename = "session")]
    Session {
        session: i64,
        project: String,
        /// `ask`, `auto`, or `full`.
        mode: String,
        /// False in full-access mode: tools may leave the workspace.
        confined: bool,
        /// What the kernel is enforcing for this session, right now.
        sandbox: SandboxReport,
        /// Whether the classifier supervises this session: the engine's own
        /// answer (`enabled`, `policy` — this session holds policy rules —, `active`,
        /// `reason`), so no frontend works it out from files or from a reason
        /// text it read when the mode was something else.
        #[serde(default, skip_serializing_if = "Value::is_null")]
        classifier: Value,
    },

    /// Live session settings update broadcast across connected clients.
    #[serde(rename = "settings")]
    Settings { session: i64, settings: Value },

    /// The session's title was set or changed.
    #[serde(rename = "title")]
    Title { session: i64, title: String },

    #[serde(rename = "turn_start")]
    TurnStart {
        session: i64,
        turn: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The turn opens on a notification of Rune's (a background task
        /// ended), not on something the user wrote.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notification: bool,
        /// The files attached to the message that opened the turn, as the
        /// session stored them. Without them a window that learns of a message
        /// from this event alone — every window but the one that sent it —
        /// would show the words and none of the files.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<crate::media::Attachment>,
        /// When the message was stored, Unix milliseconds by the engine's
        /// clock: the start of a run is the same moment in every window.
        #[serde(default)]
        at: i64,
    },

    /// Something Rune told the model behind the conversation that a person
    /// sees too: a status value that changed, a file the session was handed
    /// (`AGENTS.md`, the memory index, the skill list) that changed since, a
    /// note about the memory, files the user put back. Said as its context
    /// row is stored; a window that opens later finds the row in the history
    /// with the same `note`.
    #[serde(rename = "context")]
    Context {
        session: i64,
        turn: i64,
        /// The row's kind as stored: `status`, `context_update`, `memory`,
        /// `restore`.
        kind: String,
        /// The row in a person's words.
        note: String,
        /// When it was stored, Unix milliseconds by the engine's clock.
        #[serde(default)]
        at: i64,
    },

    /// A background task ended while the session was busy. Shown at once;
    /// the model reads it with its next request, when a `turn_start` with
    /// `notification` and the same `text` follows — which draws nothing new.
    #[serde(rename = "notification")]
    Notification {
        session: i64,
        text: String,
        /// When it was raised, Unix milliseconds by the engine's clock.
        #[serde(default)]
        at: i64,
    },

    /// Model reasoning, when the provider exposes it.
    #[serde(rename = "thinking")]
    Thinking { session: i64, delta: String },

    /// Assistant text, streamed.
    #[serde(rename = "text")]
    Text { session: i64, delta: String },

    /// Arguments of a tool call arriving while the model writes them.
    ///
    /// Progress, not truth: the call itself is `tool_call`, which always
    /// follows. A frontend uses this to show a file growing as it is written —
    /// the line counter that moves — and may ignore it entirely without missing
    /// anything. `index` distinguishes two calls being written at once.
    #[serde(rename = "tool_args")]
    ToolArgs {
        session: i64,
        id: String,
        index: u32,
        name: String,
        delta: String,
    },

    /// The model asked for a tool. Emitted before the permission gate runs, so a
    /// frontend can show the intent while the user is deciding.
    #[serde(rename = "tool_call")]
    ToolCall {
        session: i64,
        id: String,
        name: String,
        action: String,
        args: Value,
        /// When the call began, Unix milliseconds by the engine's clock — the
        /// timer of a running step is the same in every window.
        #[serde(default)]
        at: i64,
    },

    /// Live output of a long-running tool (currently only `terminal_use`).
    /// Throttled by time, never by byte count.
    #[serde(rename = "tool_output")]
    ToolOutput {
        session: i64,
        id: String,
        chunk: String,
        /// The PTY geometry this chunk was read at, not the outer window's size.
        rows: u16,
        cols: u16,
    },

    /// How one tool call ended — the same shape whether the model asked for it
    /// or the frontend did.
    ///
    /// `summary` is the one line a list shows. `content` is exactly what the
    /// model was given, which is what makes a transcript renderable: without it
    /// a frontend saw a file being read and never the file. `detail` is the
    /// tool's own structured extra (`{exit_code, commands}`, `{created, failed}`,
    /// the plan) and is absent when a tool has none. `terminal_use` streams its
    /// output as it happens in `tool_output` and repeats it here, because a
    /// frontend that connected late, reloaded, or is rendering history was never
    /// there for the live one.
    #[serde(rename = "tool_result")]
    ToolResult {
        session: i64,
        id: String,
        ok: bool,
        summary: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
        /// When the call ended, Unix milliseconds by the engine's clock.
        #[serde(default)]
        at: i64,
    },

    /// The tool waits for a validated `questions.reply`, cancellation, or timeout.
    #[serde(rename = "questions_request")]
    QuestionsRequest {
        session: i64,
        id: String,
        req: String,
        action: String,
        questions: Vec<crate::tools::questions::Question>,
    },
    #[serde(rename = "questions_resolved")]
    QuestionsResolved { session: i64, req: String },

    /// The engine waits for `permission.reply` with this request ID.
    #[serde(rename = "permission_request")]
    PermissionRequest {
        session: i64,
        req: String,
        tool: String,
        action: String,
        /// Human-readable summary of exactly what will happen if allowed.
        preview: String,
        /// What `allow_always` would persist, or `None` when the answer may
        /// never be remembered — the ask-every-time classes: deny-list severe,
        /// a sensitive reach, or a full-access call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remember_as: Option<String>,
        /// Paths outside the workspace this call would be allowed to reach.
        ///
        /// Separate from `preview` because it is the part a person must actually
        /// read: everything else is about what the tool does, and this is about
        /// where it does it. Empty for the overwhelming majority of calls.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reach: Vec<String>,
        /// Reach paths the machine itself runs on — credentials, startup
        /// files, system prefixes — as `path — reason`. The system never
        /// refuses these on its own; the card carries the reason so the
        /// answer is an informed one, and no answer is remembered.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sensitive: Vec<String>,
        /// This call cannot be represented by a narrow reach grant. Allowing it
        /// turns off the jail and sandbox for this call only.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        full_access: bool,
        /// Why a mode that would not have asked does, in the engine's words —
        /// `Auto` reading a command aloud because no kernel sandbox holds it
        /// here. `None` for every ordinary question.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    #[serde(rename = "permission_resolved")]
    PermissionResolved {
        session: i64,
        req: String,
        decision: Decision,
    },

    /// A pre-write snapshot was taken. `files` is how many paths it captured.
    /// The session's plan, whenever it changes.
    #[serde(rename = "tasks")]
    Tasks {
        session: i64,
        tasks: Vec<crate::store::tasks::Task>,
    },

    #[serde(rename = "checkpoint")]
    Checkpoint {
        session: i64,
        turn: i64,
        files: usize,
    },

    /// How the session's file tallies stand: one row per path the agent has
    /// ever touched, with its session-long `+N −N` measured from before the
    /// first touch. Sent when a round changed the ledger, and its consumer is
    /// the side panel's FILE CHANGE section — see
    /// [`crate::checkpoint::Checkpoints::session_changes`].
    #[serde(rename = "file_changes")]
    FileChanges {
        session: i64,
        changes: Vec<crate::checkpoint::FileChange>,
    },

    #[serde(rename = "usage")]
    Usage {
        session: i64,
        input: u64,
        output: u64,
        cached: u64,
        cost: Option<f64>,
        details: Box<crate::provider::UsageDetails>,
    },
    Completion {
        session: i64,
        completion: crate::provider::Completion,
    },
    AuxiliaryUsage {
        session: i64,
        role: String,
        input: u64,
        output: u64,
        cached: u64,
        cost: Option<f64>,
    },

    /// The provider call was refused or never landed, and is being opened again.
    ///
    /// Sent so a wait is visible. `rotated` says the next attempt uses a
    /// different API key rather than the same one after a pause; `wait_ms` is
    /// zero in that case, because a fresh key has nothing to wait out.
    #[serde(rename = "retrying")]
    Retrying {
        session: i64,
        attempt: u32,
        of: u32,
        wait_ms: u64,
        rotated: bool,
        reason: String,
    },

    /// History was summarised to keep the conversation sendable. Nothing was
    /// deleted — this says what the *model* will see from now on, so a frontend
    /// can show the seam rather than letting a reply appear to forget.
    #[serde(rename = "compacted")]
    Compacted {
        session: i64,
        through_turn: i64,
        kept_turns: usize,
        summary: String,
    },

    #[serde(rename = "done")]
    Done {
        session: i64,
        turn: i64,
        stop: StopReason,
    },

    /// Something failed outside a tool result — protocol, provider, or engine.
    /// Fatal to whatever was running: a frontend ends the turn it shows.
    #[serde(rename = "error")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<i64>,
        code: String,
        message: String,
    },

    /// The provider attempt streaming now failed and is being repeated: drop
    /// the text, thinking and tool arguments shown since the turn's last
    /// committed point. The repeat streams them again from the start.
    #[serde(rename = "stream_reset")]
    StreamReset { session: i64 },

    /// The session host is retiring to activate an installed release. Sent
    /// through the engine's own channel, so it arrives after the ack of every
    /// command the host accepted; any command without an ack by then was never
    /// accepted and is safe to send again to the next host.
    #[serde(rename = "host_upgrading")]
    HostUpgrading { session: i64 },

    /// Something the user may want to know that the turn does not depend on:
    /// the context Rune told the model, a status update, a retry, a cache miss,
    /// a compaction, a title that could not be made, a request's cost. Written
    /// to the session's log (`session.logs`) and shown there, never in the
    /// conversation; a turn goes on exactly as it was.
    #[serde(rename = "log")]
    Log {
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<i64>,
        entry: LogEntry,
    },
}

impl Event {
    /// The session this event belongs to, or `None` for process-wide events.
    ///
    /// One accessor, because a frontend that renders one session's screen — the
    /// interactive CLI above all — receives a live engine's *whole* event stream:
    /// one engine serves many sessions and one channel carries every event of
    /// all of them (`rt.events` is shared, and the session IPC host forwards the
    /// stream to every connected window). Without a single place that names the
    /// owning session, each handler in each frontend would have to guess which
    /// events are session-scoped, and every missed one leaks another session's
    /// state into this screen — the mode, the plan, the file tallies, the
    /// counters, the status line.
    ///
    /// A new variant without `session` fails this match at compile time, so the
    /// answer cannot drift from the enum.
    pub fn session(&self) -> Option<i64> {
        match self {
            Event::Composer { session, .. }
            | Event::McpInputRequest { session, .. }
            | Event::McpInputResolved { session, .. }
            | Event::Session { session, .. }
            | Event::Settings { session, .. }
            | Event::Title { session, .. }
            | Event::TurnStart { session, .. }
            | Event::Context { session, .. }
            | Event::Notification { session, .. }
            | Event::Thinking { session, .. }
            | Event::Text { session, .. }
            | Event::ToolArgs { session, .. }
            | Event::ToolCall { session, .. }
            | Event::ToolOutput { session, .. }
            | Event::ToolResult { session, .. }
            | Event::QuestionsRequest { session, .. }
            | Event::QuestionsResolved { session, .. }
            | Event::PermissionRequest { session, .. }
            | Event::PermissionResolved { session, .. }
            | Event::Tasks { session, .. }
            | Event::Checkpoint { session, .. }
            | Event::FileChanges { session, .. }
            | Event::Usage { session, .. }
            | Event::Completion { session, .. }
            | Event::AuxiliaryUsage { session, .. }
            | Event::Retrying { session, .. }
            | Event::Compacted { session, .. }
            | Event::Done { session, .. }
            | Event::HostUpgrading { session }
            | Event::StreamReset { session } => Some(*session),
            Event::Error { session, .. } | Event::Log { session, .. } => *session,
            Event::Ready { .. } | Event::Upgrading | Event::Ack { .. } => None,
        }
    }
}

/// One project folder, as `storage.list` reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageFolder {
    /// Its name under `projects/` — what `storage.prune` takes.
    pub folder: String,
    /// The project path it belongs to.
    pub project: String,
    /// Whether that path is still there. False only when the filesystem says
    /// it does not exist; a path that cannot be checked counts as there.
    pub present: bool,
    /// Everything the folder holds, in bytes.
    pub bytes: u64,
    /// Sessions filed under it.
    pub sessions: usize,
}

/// What `storage.prune` did with the folders it was given.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoragePruned {
    pub removed: Vec<String>,
    pub kept: Vec<StorageKept>,
    /// What the removed folders held, in bytes.
    pub bytes_freed: u64,
}

/// A folder `storage.prune` left in place, and why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageKept {
    pub folder: String,
    pub reason: String,
}

/// One line of a session's log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    /// When it happened, in Unix milliseconds.
    pub at: i64,
    pub level: LogLevel,
    /// What it is about: `context`, `status`, `memory`, `title`, `compact`,
    /// `retry`, `cache`, `usage`, `classifier`, `mcp`, `config`, `checkpoint`,
    /// `host`.
    pub source: String,
    /// A stable name for the kind of event, for a frontend that filters.
    pub code: String,
    /// One sentence for a person.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<i64>,
    /// The numbers behind the sentence, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a final answer.
    End,
    Refused,
    /// The user cancelled.
    Cancelled,
    /// The turn ended on an error; an `error` event carries the detail.
    Failed,
}

/// What the sandbox layer actually managed to enforce for this process.
/// Reported, never assumed — a silent fallback is the failure mode that gets
/// somebody's `~/.ssh` overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxReport {
    /// `"landlock"` when the kernel is enforcing, `"none"` when it is not.
    pub kind: String,
    /// Landlock ABI level the kernel reported, 0 when unsupported.
    pub abi: i32,
    pub enforced: bool,
    /// Why it is not enforced, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Ways out of the sandbox that exist on this machine and that the sandbox
    /// cannot close. Empty is the normal case. A frontend must show these:
    /// "enforced: true" with a reachable container daemon is a half-truth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escapes: Vec<Escape>,
}

/// A known way out, named so a person can act on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escape {
    /// Stable identifier, e.g. `container_daemon`.
    pub id: String,
    /// What is reachable, and why that is enough to leave the sandbox.
    pub detail: String,
    /// What the user can actually do about it.
    pub fix: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parses_from_wire() {
        let c: Command =
            serde_json::from_str(r#"{"cmd":"prompt","id":3,"session":7,"text":"hi"}"#).unwrap();
        match c {
            Command::Prompt {
                id,
                session,
                text,
                attach,
            } => {
                assert_eq!((id, session, text.as_str()), (3, 7, "hi"));
                assert!(
                    attach.is_empty(),
                    "a prompt with no files must not invent any"
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn missing_id_defaults_to_zero() {
        let c: Command = serde_json::from_str(r#"{"cmd":"session.list"}"#).unwrap();
        assert_eq!(c.request_id(), 0);
    }

    #[test]
    fn whole_catalogue_reasoning_verification_is_not_a_protocol_command() {
        let command = serde_json::from_str::<Command>(
            r#"{"cmd":"provider.verify_all_reasoning","id":3,"session":7,"provider":"local"}"#,
        );
        assert!(
            command.is_err(),
            "only the selected model may be sent to the reasoning verifier"
        );
    }

    #[test]
    fn event_serialises_tagged() {
        let e = Event::Text {
            session: 1,
            delta: "ok".into(),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"text","session":1,"delta":"ok"}"#
        );
    }

    #[test]
    fn ack_omits_empty_fields() {
        let e = Event::Ack {
            id: 9,
            ok: true,
            error: None,
            data: None,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"ack","id":9,"ok":true}"#
        );
    }

    #[test]
    fn session_event_with_omitted_empty_sandbox_fields_round_trips_over_ipc() {
        let event = Event::Session {
            session: 7,
            project: "/tmp/project".into(),
            mode: "ask".into(),
            confined: true,
            sandbox: SandboxReport {
                kind: "none".into(),
                abi: 0,
                enforced: false,
                reason: None,
                escapes: Vec::new(),
            },
            classifier: Value::Null,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("escapes"));
        assert!(!json.contains("reason"));
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.session(), Some(7));
        assert!(matches!(parsed, Event::Session { mode, .. } if mode == "ask"));
    }

    #[test]
    fn composer_with_acknowledged_operations_round_trips_over_ipc() {
        let mut composer = crate::store::composer::Composer {
            text: "hello".into(),
            ..Default::default()
        };
        composer
            .operations
            .insert("947666".into(), 4070194477531144);
        let event = Event::Composer {
            session: 7,
            request: 4070194477531144,
            composer: Box::new(composer),
        };
        let wire = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&wire).unwrap();
        assert!(
            matches!(parsed, Event::Composer { composer, .. } if composer.text == "hello" && composer.operations.len() == 1)
        );
    }

    #[test]
    fn session_open_defaults_to_reuse_but_can_request_new_identity() {
        let old: Command =
            serde_json::from_str(r#"{"cmd":"session.open","id":2,"project":"/tmp"}"#).unwrap();
        assert!(matches!(old, Command::SessionOpen { fresh: false, .. }));
        let fresh: Command =
            serde_json::from_str(r#"{"cmd":"session.open","id":3,"project":"/tmp","fresh":true}"#)
                .unwrap();
        assert!(matches!(fresh, Command::SessionOpen { fresh: true, .. }));
    }

    #[test]
    fn decision_round_trips() {
        let d: Decision = serde_json::from_str(r#""allow_always""#).unwrap();
        assert_eq!(d, Decision::AllowAlways);
    }
}
