//! The tools the agent can actually run, and the contract every one of them
//! satisfies.
//!
//! One file per tool. `mod.rs` holds only what they share: the trait, the
//! registry, the execution context, and the argument helpers.
//!
//! # Schema shape — and the measurement that shaped it
//!
//! Every declaration is atomic: `action` followed by the operation's fields.
//! Multiple operations are separate calls; composite tasks/questions retain
//! their domain arrays. There is no legacy single/batch dual-door execution.
//!
//! The array alone was the original shape, copied from the rest of this
//! ecosystem (`Server/tools/*.js`). It was measured against a real provider
//! before this was changed:
//!
//! ```text
//! top-level string argument : 166 chunks, mean 8 chars  → streams as it is written
//! array-valued argument     :   1 chunk,      1447 chars → arrives whole, at the end
//! ```
//!
//! A gateway serialises a structured value and sends it in one piece. So with
//! only the array form, a frontend can show *nothing* about a file until the
//! whole file has arrived — no growing line count, no diff taking shape, and no
//! honest way to fake one. Atomic top-level fields let a write stream
//! character by character without a second execution shape.
//!
//! `action` stays first: it is the one field a model emits before the
//! payload, so a frontend has something true to show while the rest streams.
//!
//! # What a tool may assume
//!
//! * every path it is given has already passed the jail
//! * the permission gate has already said yes
//! * for write tools, the checkpoint for this turn already holds the file's
//!   previous content — `write_guard` is what guarantees it

#[path = "CallMcpTool.rs"]
pub mod call_mcp;
pub mod create;
#[path = "DiscoverCapabilities.rs"]
pub mod discovery;
pub mod find;
pub mod inspect;
pub mod notice;
pub mod patch;
pub mod process_control;
#[path = "ReadMcpResource.rs"]
pub mod resource;
pub mod search;
pub mod terminal;
pub mod track;
pub mod web_fetch;
pub mod websearch;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use crate::checkpoint::Checkpoints;
use crate::error::{Error, Result};
use crate::jail::Jail;
use crate::limits::Limits;
use crate::perm::denylist::Risk;
use crate::proto::Event;
use crate::sandbox::Sandbox;
use crate::settings::Settings;
use crate::tasks::Tasks;

/// A tool's declaration, in the shape a provider expects.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// What a tool produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub media: Vec<crate::media::Attachment>,
    pub ok: bool,
    /// One line for the frontend and the log.
    pub summary: String,
    /// The text handed back to the model.
    pub content: String,
    /// Structured extras for a frontend that wants to render more than text.
    pub detail: Option<Value>,
}

/// The output and Rune's notes are separate until the caller chooses its
/// envelope. In particular, a process report must not discover notes by parsing
/// a command's output: a command can print the same tags.
pub(super) struct ProcessOutput {
    outcome: Outcome,
    information: Vec<String>,
}

impl ProcessOutput {
    /// The result as the model reads it: Rune's notices, one line each, then
    /// the tool's own output.
    ///
    /// The `Key: value` envelope this used to write belongs to the background
    /// report, and the report is gone (owner, 2026-11-03): a task's state and
    /// its output arrive as its own `<system_notification>`, so no tool writes
    /// a field block any more.
    pub(super) fn into_outcome(mut self) -> Outcome {
        let notices: Vec<_> = self
            .information
            .iter()
            .map(|text| notice::inline(text))
            .collect();
        self.outcome.content = if notices.is_empty() {
            self.outcome.content
        } else if self.outcome.content.is_empty() {
            notices.join("\n")
        } else {
            format!("{}\n\n{}", notices.join("\n"), self.outcome.content)
        };
        self.outcome
    }
}

impl Outcome {
    pub fn new(summary: impl Into<String>, content: impl Into<String>) -> Outcome {
        Outcome {
            media: Vec::new(),
            ok: true,
            summary: summary.into(),
            content: content.into(),
            detail: None,
        }
    }

    /// Render process payloads through one envelope. Metadata never enters the
    /// raw file or head/tail excerpts; callers use a single configuration snapshot.
    /// Output too long to show is kept whole under `results` (the session's
    /// `tools_result/`, [`ToolCtx::results`]), in a folder named for the tool.
    pub fn process_output(
        summary: String,
        payload: impl IntoIterator<Item = crate::error::Result<String>>,
        metadata: Value,
        config: &crate::settings::configuration::Configuration,
        section: &str,
        tool: &str,
        results: &std::path::Path,
    ) -> Outcome {
        Self::capture_process_output(summary, payload, metadata, config, section, tool, results)
            .into_outcome()
    }

    pub(super) fn capture_process_output(
        summary: String,
        payload: impl IntoIterator<Item = crate::error::Result<String>>,
        metadata: Value,
        config: &crate::settings::configuration::Configuration,
        section: &str,
        tool: &str,
        results: &std::path::Path,
    ) -> ProcessOutput {
        let max = config.number(&format!("{section}.max_output_chars")) as usize;
        use std::io::Write;
        let structured = matches!(tool, "discover_capabilities" | "read_mcp_resource");
        let head_limit = if structured {
            0
        } else {
            config.number(&format!("{section}.truncate_head_bytes")) as usize
        };
        let tail_limit = if structured {
            0
        } else {
            config.number(&format!("{section}.truncate_tail_bytes")) as usize
        };
        let mut total = 0usize;
        let mut full = String::new();
        let mut head = String::new();
        let mut head_chars = 0;
        let mut head_full = false;
        let mut tail = std::collections::VecDeque::new();
        let mut tail_bytes = 0;
        let mut saved: Option<(std::path::PathBuf, std::fs::File)> = None;
        let mut notices = Vec::new();
        let mut complete = true;
        for chunk in payload {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    notices.push(format!(
                        "The output could not be read completely ({}).",
                        notice::reason(&error.to_string())
                    ));
                    complete = false;
                    break;
                }
            };
            let count = chunk.chars().count();
            if total <= max && total.saturating_add(count) > max {
                let result = (|| -> crate::error::Result<_> {
                    let dir = results.join(tool);
                    std::fs::create_dir_all(&dir)?;
                    let path = dir.join(format!("{}.txt", unique_persist_id(&dir, ".txt")));
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                    file.write_all(full.as_bytes())?;
                    Ok((path, file))
                })();
                match result {
                    Ok(file) => saved = Some(file),
                    Err(error) => {
                        complete = false;
                        notices.push(format!(
                            "The output could not be saved ({}).",
                            notice::reason(&error.to_string())
                        ));
                    }
                }
                full.clear();
            }
            total = total.saturating_add(count);
            if total <= max {
                full.push_str(&chunk);
            }
            if let Some((_, file)) = &mut saved {
                if let Err(error) = file.write_all(chunk.as_bytes()) {
                    complete = false;
                    notices.push(format!(
                        "The full output could not be saved ({}).",
                        notice::reason(&error.to_string())
                    ));
                    saved = None;
                }
            }
            for c in chunk.chars() {
                if !head_full {
                    if head_chars < max / 2 && head.len() + c.len_utf8() <= head_limit {
                        head.push(c);
                        head_chars += 1;
                    } else {
                        head_full = true;
                    }
                }
                tail.push_back(c);
                tail_bytes += c.len_utf8();
                while tail.len() > max - max / 2 || tail_bytes > tail_limit {
                    if let Some(c) = tail.pop_front() {
                        tail_bytes -= c.len_utf8();
                    }
                }
            }
        }
        let saved_path = if let Some((path, mut file)) = saved {
            match file.flush() {
                Ok(()) => Some(path.display().to_string()),
                Err(error) => {
                    complete = false;
                    notices.push(format!(
                        "The full output could not be saved ({}).",
                        notice::reason(&error.to_string())
                    ));
                    None
                }
            }
        } else {
            None
        };
        let content = if total > max {
            let tail: String = tail.into_iter().collect();
            let head_size = crate::media::human(head.len() as u64);
            let tail_size = crate::media::human(tail.len() as u64);
            // One sentence about the cut, and where the whole of it is kept.
            let kept = match (saved_path, complete) {
                (Some(p), true) => format!(" The full output is saved at {}.", notice::quote(&p)),
                (Some(p), false) => {
                    format!(
                        " A partial copy of the output is saved at {}.",
                        notice::quote(&p)
                    )
                }
                (None, _) => String::new(),
            };
            if structured {
                let kept = kept.replace(
                    "The full output is saved at",
                    "The full response is saved at",
                );
                notices.insert(
                    0,
                    format!("The response is {total} characters, over the {max} limit.{kept}"),
                );
            } else {
                notices.insert(
                    0,
                    format!(
                        "The output is {total} characters, over the {max} limit; the middle is left out ({head_size} shown from the start, {tail_size} from the end).{kept}"
                    ),
                );
            }
            if structured {
                String::new()
            } else {
                format!("<preview_head size=\"{head_size}\">\n{head}\n</preview_head>\n\n<preview_tail size=\"{tail_size}\">\n{tail}\n</preview_tail>")
            }
        } else {
            full
        };
        let mut outcome = Outcome::new(summary, content).with_detail(metadata);
        outcome.ok = complete;
        ProcessOutput {
            outcome,
            information: notices,
        }
    }

    pub fn with_detail(mut self, detail: Value) -> Outcome {
        self.detail = Some(detail);
        self
    }
}

/// Where a running terminal parks its master side so that input can reach it.
///
/// Empty unless a command is running. One per session: only one terminal runs at
/// a time, because only one turn runs at a time.
pub type InputSlot = std::sync::Mutex<Option<Arc<crate::platform::Pty>>>;

/// Who reads a call's result — and so whom the call tells anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    /// The model: a call of its own turn, whose result joins its history.
    Model,
    /// Only the person: a call a frontend ran directly (`tool.invoke`). Its
    /// result enters no history the model reads, so it tells the model
    /// nothing — what it found of a background task (its end, its wait) is
    /// still the model's to hear, by a wake.
    Person,
}

/// Everything a tool is allowed to reach.
pub struct ToolCtx<'a> {
    /// The terminal call's engine-pinned executable, stable through permission waits.
    pub shell: Option<&'a std::path::Path>,
    pub capabilities: &'a crate::capabilities::Capabilities,
    pub registry: &'a Registry,
    pub questions: &'a crate::perm::broker::Broker<questions::Reply>,
    pub limits: &'a Limits,
    pub session: i64,
    pub turn: i64,
    pub call_id: &'a str,
    pub jail: &'a Jail,
    /// The project's memory: the one place several sessions write the same
    /// files, so a change there is held against theirs (see [`ToolCtx::hold`]).
    pub memory: &'a crate::memory::Memory,
    pub checkpoints: &'a Checkpoints,
    pub sandbox: &'a Sandbox,
    pub cancel: &'a Arc<AtomicBool>,
    pub input: &'a InputSlot,
    /// The session's plan. A narrow handle rather than the database itself —
    /// a tool has no business reaching any other table.
    pub tasks: &'a Tasks,
    /// Where this session keeps what a tool writes to disk for it: its
    /// folder's `tools_result/`, never a path of the tool's own making.
    pub results: &'a std::path::Path,
    /// Rows and columns a terminal opens at. Owned by the session, because the
    /// frontend that knows the size is the one that sends `tool.resize`.
    pub term_size: (u16, u16),
    pub background: &'a Arc<crate::session::background::BackgroundTaskManager>,
    /// Who reads this call's result.
    pub reader: Reader,
    pub emit: &'a mut dyn FnMut(Event),
    /// The session's log (`/logs`): what the person may want to know about a
    /// call and the model does not need — which service answered, a retry, a
    /// refusal. Level, code, message, detail; the tool's name is the source.
    pub log: &'a LogFn<'a>,
}

/// Where a tool writes to the session's log: level, code, message, detail
/// (the tool's name is the source).
pub type LogFn<'a> = dyn Fn(crate::proto::LogLevel, &str, String, Option<Value>) + 'a;

impl ToolCtx<'_> {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Stop before touching `path` unless its previous state is safely captured.
    ///
    /// Every write in the engine goes through here. That is the whole of "write
    /// tools stay disabled until the checkpoint engine exists": there is no path
    /// to a write that does not pass this line.
    pub fn write_guard(&self, path: &std::path::Path) -> Result<()> {
        self.checkpoints
            .capture(self.session, self.turn, path, self.jail)
            .map(|_| ())
    }

    /// Hold `path` for one change, before it is read for that change; write
    /// it through the returned [`crate::memory::Hold`] after
    /// [`Self::write_guard`]. A memory file is held against every other
    /// session of the project until the hold is dropped.
    pub fn hold(&self, path: &std::path::Path) -> Result<crate::memory::Hold> {
        self.memory.hold(path)
    }

    /// Record a write tool's delta additions and deletions in the session ledger.
    pub fn record_write(
        &self,
        path: &std::path::Path,
        delta_add: u64,
        delta_rem: u64,
    ) -> Result<()> {
        self.checkpoints.record_change(
            self.session,
            self.turn,
            path,
            self.jail,
            delta_add,
            delta_rem,
        )
    }
}

#[path = "AskUserQuestions.rs"]
pub mod questions;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn is_external(&self) -> bool {
        false
    }
    fn declaration(&self) -> ToolDecl;

    /// Does this tool change anything? Read-only tools never prompt.
    fn mutates(&self) -> bool;

    /// May a call of this tool run at the same time as others of one response?
    ///
    /// Only a tool that reads and nothing else says yes: it changes nothing,
    /// runs no command, asks the person nothing of its own and is not another
    /// program's (the owner's rule, 2026-10-06, the same split Claude Code
    /// makes). Whether a given call does run alongside others is still the
    /// turn's decision ([`crate::session::call::concurrent`]): the gate must
    /// allow it without a question first.
    fn concurrent(&self) -> bool {
        false
    }

    /// Does this tool run a command line?
    ///
    /// Separate from [`Tool::mutates`] because it answers a different question:
    /// not *will something change* but *can anyone tell in advance what will*.
    /// Only `terminal_use` says yes, and `Ask` mode reads every one of them to a
    /// person before it runs — see [`crate::perm::policy::Situation::runs`].
    fn runs_commands(&self) -> bool {
        false
    }

    /// How dangerous this specific call is. Only `terminal` has an interesting
    /// answer; the rest are ordinary and let the gate decide.
    fn risk(&self, _args: &Value) -> Risk {
        Risk::Normal
    }

    /// What an "allow always" in this project would cover.
    fn grant_key(&self, _args: &Value) -> String {
        self.name().to_string()
    }

    /// Paths outside the workspace this call names, spelt as the model wrote
    /// them (`~/Downloads`, `/etc/hosts`).
    ///
    /// Returned raw rather than resolved: resolving needs a home directory and a
    /// filesystem, and a tool has neither. [`crate::perm::reach`] turns them into
    /// candidates the user can be asked about, and only an answered prompt ever
    /// widens anything.
    ///
    /// Default empty, which is the honest answer for a tool that takes no path.
    fn reaches(&self, _args: &Value) -> Vec<String> {
        Vec::new()
    }

    /// This call talks to a program already running on this machine through
    /// a Unix socket — read from what it names, as [`Tool::reaches`] is: a
    /// command whose program is such a client, or a `unix:` address. A socket
    /// file among [`Tool::reaches`] counts too; the call runner checks that on
    /// disk. Default no.
    fn local_services(&self, _args: &Value) -> bool {
        false
    }

    /// Why the program this call's commands start cannot run under the
    /// kernel confinement on this machine (`platform::unconfinable`) — the
    /// gate then asks before it, as where no sandbox holds, and an allowed
    /// call runs with confinement lifted for it alone. Default `None`.
    fn unconfinable(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Plain-language statement of what is about to happen, for the prompt.
    fn preview(&self, args: &Value) -> String;

    /// Each tool call represents 1 operation towards the response budget.
    fn request_count(&self, _args: &Value) -> usize {
        1
    }

    /// The server and the server's own name of an MCP tool; `None` for every
    /// tool of Rune's own.
    fn mcp_identity(&self) -> Option<(String, String)> {
        None
    }

    /// Enforce the tool's JSON schema authority.
    fn validate(&self, args: &Value) -> Result<()> {
        let schema = self.declaration().parameters;
        if self.is_external() {
            validate_value(&schema, args)
        } else {
            validate_builtin(&schema, args)
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome>;
}

/// Built-ins reserve blank optional text for "not given". Keep those defaults
/// out of provider enums: Gemini rejects an empty enum member. Validate a
/// canonical copy so required text, unknown fields and wrong types still fail;
/// the gate, audit and tool retain the arguments that were actually sent.
/// External schemas use validate_value directly and are never relaxed.
pub(crate) fn validate_builtin(schema: &Value, args: &Value) -> Result<()> {
    fn defaults(schema: &Value, value: &mut Value) {
        if let Some(map) = value.as_object_mut() {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                let required = schema.get("required").and_then(Value::as_array);
                for (key, property) in properties {
                    let optional = !required.is_some_and(|list| list.iter().any(|v| v == key));
                    if optional && property["type"] == "string" {
                        if let Some(text) = map.get(key).and_then(Value::as_str) {
                            if text.trim().is_empty() {
                                map.remove(key);
                                continue;
                            }
                            if property.get("enum").is_some() {
                                map.insert(key.clone(), json!(text.trim()));
                            }
                        }
                    }
                    if let Some(value) = map.get_mut(key) {
                        defaults(property, value);
                    }
                }
            }
        } else if let Some(list) = value.as_array_mut() {
            if let Some(items) = schema.get("items") {
                for value in list {
                    defaults(items, value);
                }
            }
        }
    }
    let original = validate_value(schema, args);
    if original.is_ok() {
        return original;
    }
    let mut canonical = args.clone();
    defaults(schema, &mut canonical);
    if canonical == *args {
        original
    } else {
        validate_value(schema, &canonical)
    }
}

/// Validate actual values rather than trusting the model's schema adherence.
///
/// Compiled validators are kept by the schema's content: compiling one was
/// most of the cost of a call's validation, paid twice per call (audit H6). A
/// schema that changes — a limit the user tuned — is simply a new key.
pub(crate) fn validate_value(schema: &Value, value: &Value) -> Result<()> {
    static COMPILED: Mutex<Option<std::collections::HashMap<String, Arc<jsonschema::Validator>>>> =
        Mutex::new(None);
    /// More distinct schemas than any session meets; past it the cache starts over.
    const LIMIT: usize = 512;
    let key = crate::checkpoint::objects::hash_of(schema.to_string().as_bytes());
    let cached = crate::panic::lock(&COMPILED)
        .as_ref()
        .and_then(|map| map.get(&key).cloned());
    let validator = match cached {
        Some(validator) => validator,
        None => {
            let built = Arc::new(
                jsonschema::draft202012::options()
                    .with_pattern_options(
                        jsonschema::PatternOptions::fancy_regex().backtrack_limit(100_000),
                    )
                    .build(schema)
                    .map_err(|_| Error::invalid("Unsupported or invalid tool schema"))?,
            );
            let mut held = crate::panic::lock(&COMPILED);
            let map = held.get_or_insert_with(Default::default);
            if map.len() >= LIMIT {
                map.clear();
            }
            map.insert(key, Arc::clone(&built));
            built
        }
    };
    validator
        .validate(value)
        .map_err(|e| Error::invalid(correction(&e)))
}

/// The call to send, not the rule it broke.
///
/// A tool result is a message to the model, and a rejection that only names the
/// violated rule leaves it nothing to act on — so it sends the same call again.
/// Name the violated field without repeating the agent's call-writing rules.
/// Recovery belongs to the system prompt; the validator supplies only the
/// specific failure, with values masked so secrets cannot enter its error.
pub(crate) fn correction(e: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind as Kind;
    match e.kind() {
        Kind::Required { property } => match property.as_str() {
            Some(name) => format!("Required field `{name}` is missing."),
            None => "A required field is missing.".to_string(),
        },
        Kind::AdditionalProperties { unexpected } => format!(
            "unknown field(s) {}.",
            unexpected
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Masked: the reason, never the value — an argument can carry a secret,
        // and this text reaches the audit trail and the screen.
        _ => format!(
            "Argument at `{}` is not valid: {}.",
            e.instance_path(),
            e.masked()
        ),
    }
}

/// Reuse the byte-static declarations without assigning them invented tokens.
pub struct Declared {
    pub list: Vec<ToolDecl>,
}

/// Every tool the engine knows, in the order they are offered to a model.
///
/// What a model is offered is the built-in tools, and nothing else, for the
/// whole life of the process: an MCP server's tools are called through
/// `call_mcp_tool`, so connecting, disconnecting or discovering one never
/// changes the declarations that open every request.
pub struct Registry {
    /// The tools of connected MCP servers, by their internal name. Reached only
    /// through [`Registry::target`].
    external: Mutex<std::collections::BTreeMap<String, Arc<dyn Tool>>>,
    /// [`Declared`], built on first use. Nothing moves a declaration: none
    /// quotes a configured value (see [`Registry::new`]), and the set of tools
    /// is fixed when the registry is built.
    declared: Mutex<Option<Arc<Declared>>>,
    read_only: bool,
    limits: Arc<Limits>,
    tools: Vec<Arc<dyn Tool>>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new(
            Arc::new(Limits::default()),
            Arc::new(Mutex::new(Settings::empty())),
        )
    }
}

impl Registry {
    /// The limits reach the tools that enforce them, read live at each call. No
    /// declaration quotes a configured value: declarations are the first bytes
    /// of every request, so one that moved with `rune.toml` would void every
    /// session's prompt cache. A tool that holds a call to a limit names it in
    /// its result instead.
    pub fn new(limits: Arc<Limits>, settings: Arc<Mutex<Settings>>) -> Registry {
        Registry {
            external: Mutex::new(Default::default()),
            declared: Mutex::new(None),
            read_only: false,
            limits: Arc::clone(&limits),
            tools: vec![
                Arc::new(discovery::DiscoverCapabilities),
                Arc::new(call_mcp::CallMcpTool),
                Arc::new(resource::ReadMcpResource),
                Arc::new(inspect::InspectFile),
                Arc::new(find::FindFiles(Arc::clone(&limits))),
                Arc::new(search::SearchText(Arc::clone(&limits))),
                Arc::new(websearch::WebSearch::new(
                    Arc::clone(&settings),
                    Arc::clone(&limits),
                )),
                Arc::new(web_fetch::WebFetch::new(
                    Arc::clone(&limits),
                    Arc::clone(&settings),
                )),
                Arc::new(terminal::TerminalUse::new(Arc::clone(&limits))),
                Arc::new(process_control::ProcessControl),
                Arc::new(track::TrackTasks),
                Arc::new(questions::AskUserQuestions),
                Arc::new(create::CreateFile(Arc::clone(&limits))),
                Arc::new(patch::PatchFile(Arc::clone(&limits))),
            ],
        }
    }

    /// Only the tools that cannot change anything. This is what a read-only
    /// session offers a model.
    pub fn read_only(limits: Arc<Limits>, settings: Arc<Mutex<Settings>>) -> Registry {
        let mut registry = Self::new(limits, settings);
        registry.read_only = true;
        registry.tools.retain(|tool| !tool.mutates());
        registry
    }

    pub fn fork(&self) -> Registry {
        Registry {
            tools: self.tools.clone(),
            limits: Arc::clone(&self.limits),
            read_only: self.read_only,
            external: Mutex::new(Default::default()),
            declared: Mutex::new(None),
        }
    }

    pub fn limits(&self) -> &Arc<Limits> {
        &self.limits
    }

    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>> {
        self.resolve(name).ok_or_else(|| {
            Error::NotFound(format!(
                "tool `{name}`. The callable tools are: {}",
                self.known().join(", ")
            ))
        })
    }

    /// Every name a model can call.
    fn known(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// A built-in tool named exactly.
    fn exact(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// The tool a call runs as, under the name it runs as, with the arguments it
    /// gets. A `call_mcp_tool` call runs as the MCP tool it names, with the
    /// arguments it carries; every other call runs as itself. The one place the
    /// proxy is resolved, so the permission gate, the classifier and the tool
    /// all see the same call.
    pub fn target(&self, name: &str, args: &Value) -> Result<(Arc<dyn Tool>, String, Value)> {
        let tool = self.get(name)?;
        if tool.name() != call_mcp::NAME {
            return Ok((tool, name.to_string(), args.clone()));
        }
        let (internal, arguments) = call_mcp::target(args)?;
        let remote = crate::panic::lock(&self.external)
            .get(&internal)
            .cloned()
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "MCP tool `{}` on server `{}` is not connected. Find connected MCP tools with discover_capabilities.",
                    args["tool"].as_str().unwrap_or_default(),
                    args["server_id"].as_str().unwrap_or_default()
                ))
            })?;
        Ok((remote, internal, arguments))
    }

    /// Three ways to mean one tool, tried in order: the name itself, a plain
    /// English synonym, then the same name typed a little differently.
    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.exact(name) {
            return Some(tool);
        }
        if let Some(tool) =
            synonym(&name.to_ascii_lowercase()).and_then(|canonical| self.exact(canonical))
        {
            return Some(tool);
        }
        let key = folded(name);
        self.known()
            .into_iter()
            .find(|candidate| folded(candidate) == key)
            .and_then(|candidate| self.exact(&candidate))
    }

    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        if self.tools.iter().any(|t| t.name() == tool.name()) {
            return Err(Error::invalid("External tool collides with a Rune tool"));
        }
        if !self.read_only {
            let mut entries = crate::panic::lock(&self.external);
            if entries.contains_key(tool.name()) {
                return Err(Error::invalid("Duplicate external tool identity"));
            }
            entries.insert(tool.name().into(), tool);
        }
        Ok(())
    }

    pub fn remove_server(&self, prefix: &str) {
        crate::panic::lock(&self.external).retain(|n, _| !n.starts_with(prefix));
    }

    /// The declarations on offer now, shared unchanged across requests.
    pub fn declared(&self) -> Arc<Declared> {
        let mut held = crate::panic::lock(&self.declared);
        if let Some(declared) = held.as_ref() {
            return Arc::clone(declared);
        }
        let list = self.declarations();
        let declared = Arc::new(Declared { list });
        *held = Some(Arc::clone(&declared));
        declared
    }

    pub fn declarations(&self) -> Vec<ToolDecl> {
        self.tools.iter().map(|t| t.declaration()).collect()
    }

    /// Every tool, for discovery and the frontend's catalogue. An MCP tool is
    /// listed by the identity `call_mcp_tool` takes — its server and its own
    /// name — with the input schema its arguments must match.
    pub fn listing(&self) -> Vec<Value> {
        let mut entries: Vec<_> = self
            .tools
            .iter()
            .map(|t| {
                let decl = t.declaration();
                json!({"id":decl.name,"name":decl.name,"kind":"tool","source":"rune-builtin","scope":"product","description":decl.description,"parameters":decl.parameters,"mutates":t.mutates()})
            })
            .collect();
        let external = crate::panic::lock(&self.external);
        entries.extend(external.values().filter_map(|t| {
            let (server, tool) = t.mcp_identity()?;
            let decl = t.declaration();
            Some(json!({"id":format!("{server}/{tool}"),"name":tool,"kind":"tool","source":"mcp","scope":"project","server_id":server,"tool":tool,"description":decl.description,"input_schema":decl.parameters,"mutates":t.mutates()}))
        }));
        entries
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }
}

// ─── Argument helpers ─────────────────────────────────────────────────────────

/// The outside paths named by `fields` in a tool call's arguments.
pub fn outside_paths(args: &Value, fields: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for field in fields {
        let Some(value) = field_opt_str(args, field) else {
            continue;
        };
        if let Some(path) = crate::perm::reach::in_argument(value) {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Where a path pattern's search runs — `find_files`' pattern, `search_text`'s
/// include: the folder the pattern names before its first wildcard, reached
/// through the jail like any path a tool is given, and the glob matched below
/// it. A pattern with no `/` matches a name at any depth of the workspace; one
/// with a `/`, relative or absolute, is matched as written.
///
/// One root, the workspace, unless the pattern names another (owner,
/// 2026-11-03: the tools lost their `path`, and a folder is part of the
/// pattern). The walk starts in the named folder instead of the workspace
/// root: what it finds is what a walk of the whole workspace filtered by the
/// pattern finds, but the entry budget is spent there, not in every other
/// folder first, and a folder no walk enters on its own (`.git`, `.rune`) is
/// searched when the pattern names it.
pub struct Scope {
    /// Where the walk starts.
    pub dir: std::path::PathBuf,
    /// Matched against each path below `dir`; `None` matches every file.
    pub glob: Option<crate::scan::glob::GlobSet>,
    /// The pattern is one file's own path and that file is there: the search
    /// is that file alone, not a walk of its folder.
    pub file: Option<std::path::PathBuf>,
    /// One or more literal files found directly without walking.
    pub files: Vec<std::path::PathBuf>,
}

impl Scope {
    /// Every file of the workspace.
    pub fn workspace(jail: &crate::jail::Jail) -> Scope {
        Scope {
            dir: jail.root().to_path_buf(),
            glob: None,
            file: None,
            files: Vec::new(),
        }
    }

    pub fn of(jail: &crate::jail::Jail, pattern: &str) -> Result<Scope> {
        use crate::scan::glob::{common_walk_root, expand_braces, is_literal, GlobSet};
        let pattern = spelt_for_walk(pattern.trim(), crate::platform::OS);
        let branches = expand_braces(&pattern);

        // If every branch is a literal file path (no wildcards whatsoever),
        // check if they exist directly on disk through the jail.
        if !branches.is_empty() && branches.iter().all(|b| is_literal(b)) {
            let mut resolved_files = Vec::new();
            for b in &branches {
                if let Ok(path) = jail.resolve(b) {
                    if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
                        resolved_files.push(path);
                    }
                }
            }
            if !resolved_files.is_empty() {
                let primary = resolved_files.first().cloned();
                return Ok(Scope {
                    dir: jail.root().to_path_buf(),
                    glob: None,
                    file: primary,
                    files: resolved_files,
                });
            }
        }

        let (root, rel_patterns) = common_walk_root(&branches);

        // A drive alone is that drive's root, never the folder it is in now.
        let drive;
        let root = if root.len() == 2 && root.ends_with(':') {
            drive = format!("{root}/");
            drive.as_str()
        } else {
            root.as_str()
        };

        if root.is_empty() {
            let globs: Vec<String> = rel_patterns
                .into_iter()
                .map(|p| {
                    if p.contains('/') || p.starts_with('/') {
                        p
                    } else {
                        format!("**/{p}")
                    }
                })
                .collect();
            return Ok(Scope {
                dir: jail.root().to_path_buf(),
                glob: Some(GlobSet::new(globs)),
                file: None,
                files: Vec::new(),
            });
        }

        let dir = jail.resolve(root)?;
        Ok(Scope {
            dir,
            glob: Some(GlobSet::new(rel_patterns)),
            file: None,
            files: Vec::new(),
        })
    }
}

/// A path pattern spelt the way the walk names paths, with `/`. Windows
/// separates folders with `\` as well, and so does a model that copied a path
/// from it (`D:\work\src\main.rs`): there a `\` is a separator, never a glob
/// escape — a metacharacter is matched as a class (`[*]`) instead. CI run
/// 41771425869: such an include found nothing on Windows.
fn spelt_for_walk(pattern: &str, os: crate::platform::Os) -> String {
    if os == crate::platform::Os::Windows {
        pattern.replace('\\', "/")
    } else {
        pattern.to_string()
    }
}

/// What a search that stopped at the person's scan bounds says: which bound,
/// that some files were not looked at, and how to look at them — never a
/// result that only seems complete (`scan.max_entries`, `scan.max_depth`).
pub fn walk_stopped(
    config: &crate::settings::configuration::Shared,
    stats: &crate::scan::walk::Stats,
    narrow: &str,
) -> Option<String> {
    let entries = || {
        format!(
            "scan.max_entries ({} files)",
            config.number("scan.max_entries")
        )
    };
    let depth = || {
        format!(
            "scan.max_depth ({} folders deep)",
            config.number("scan.max_depth")
        )
    };
    let bound = match (stats.too_many, stats.too_deep) {
        (true, true) => format!("{} and {}", entries(), depth()),
        (true, false) => entries(),
        (false, true) => depth(),
        (false, false) => return None,
    };
    Some(notice::inline(&format!(
        "The search stopped at {bound} before every file was looked at; {narrow}"
    )))
}

pub fn action_of(args: &Value) -> &str {
    args.get("action").and_then(|v| v.as_str()).unwrap_or("")
}

/// The one field name models reach for instead of the one Rune uses.
///
/// `action` is what the user and the permission classifier read before a call
/// runs, so it stays `action` — the field is the architecture. But models
/// trained on the other widespread convention spell that same sentence
/// `description`, and being told only that "a field is missing" does not tell
/// them which one: 15 of 209 calls in one long session were rejected for this
/// and nothing else, every one of them otherwise a perfect call.
///
/// So the spelling is accepted rather than the field renamed. One branch, and the
/// whole class of failures goes away.
pub fn adopt_action(args: &mut Value) {
    let Some(map) = args.as_object_mut() else {
        return;
    };
    if !map.contains_key("action") {
        let spoken = match map.get("description") {
            Some(Value::String(sentence)) if !sentence.trim().is_empty() => Some(sentence.clone()),
            _ => None,
        };
        if let Some(sentence) = spoken {
            map.insert("action".into(), Value::String(sentence));
            map.remove("description");
        }
    }
    // A sentence that trails off into markup (`… <command>ls</command>`) is cut
    // back to the sentence. Nothing inside the markup is ever lifted into an
    // argument: the stored call would then teach the model that the malformed
    // shape works, and a command could run from prose no schema checked
    // (audit H1). A missing argument is refused with a correction instead.
    if let Some(action_val) = map.get("action").cloned() {
        if let Some(action_str) = action_val.as_str() {
            let mut cleaned = action_str.to_string();
            for tag in &["description", "command", "action", "path"] {
                let open_tag = format!("<{tag}>");
                let close_tag = format!("</{tag}>");
                if let Some(pos) = cleaned.find(&close_tag) {
                    cleaned = cleaned[..pos].trim_end().to_string();
                }
                if let Some(pos) = cleaned.find(&open_tag) {
                    cleaned = cleaned[..pos].trim_end().to_string();
                }
            }
            if !cleaned.trim().is_empty() && cleaned != action_str {
                map.insert("action".into(), Value::String(cleaned));
            }
        }
    }
}

/// Guarantee the sentence the user and the classifier read before a call runs.
///
/// The schema asks the model for `action`, forbids an empty one, and the two
/// above fold in the spelling it reaches for instead. This is the floor under
/// all of that: a call that still arrives without a usable sentence **runs
/// anyway, and still shows one**. Rejecting it would be the opposite of what the
/// field is for — a blank card tells the user nothing, and a refused call tells
/// them less, while the work itself was perfectly well specified.
///
/// The fallback is the tool's own first sentence, which is true by construction:
/// it is what the tool does, said by the tool.
pub fn ensure_action(args: &mut Value, tool: &dyn Tool) {
    adopt_action(args);
    if let Some(map) = args.as_object_mut() {
        map.remove("description");
        if let Some(Value::String(said)) = map.get_mut("action") {
            let cleaned: String = said
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(ACTION_MAX)
                .collect();
            *said = cleaned.trim().to_string();
        }
    }
    if args
        .get("action")
        .and_then(|v| v.as_str())
        .is_some_and(|said| !said.trim().is_empty())
    {
        return;
    }
    if let Some(map) = args.as_object_mut() {
        map.insert(
            "action".into(),
            Value::String(first_sentence(&tool.declaration().description)),
        );
    }
}

/// How long the one sentence a call carries may be.
///
/// `action` is a label the person and the classifier read before anything runs,
/// not payload. A paragraph there is a mistake, but refusing the call costs a
/// whole round trip for a field no tool reads — so the sentence is cut, never
/// the call. The history row keeps exactly what the model sent minus the tail.
pub const ACTION_MAX: usize = 200;

/// Fold the spelling a model reaches for into the argument this build declares.
///
/// Measured (audit 2026-11-01): a call written by another agent's habits arrives
/// with `content` beside `text`, `pending` beside `todo`, or a question whose
/// label sits under `label`. Every entry is the same argument under another
/// name — never a different capability — folded before validation, the history
/// row and the tool all read the arguments, so the call runs as if it had been
/// written the declared way and nothing is ever lifted out of prose. A name
/// that is nobody's synonym is still refused: a guess between two meanings is
/// worse than an error.
pub fn canonical_args(tool: &dyn Tool, args: &mut Value) {
    let Some(map) = args.as_object_mut() else {
        return;
    };
    match tool.name() {
        "create_file" => fold_field(map, "content", "text"),
        "track_tasks" => {
            if let Some(tasks) = map.get_mut("tasks").and_then(Value::as_array_mut) {
                for task in tasks {
                    let Some(item) = task.as_object_mut() else {
                        continue;
                    };
                    let folded = match item.get("status").and_then(Value::as_str) {
                        Some("pending") => Some("todo"),
                        Some("in_progress") => Some("doing"),
                        Some("completed") => Some("done"),
                        _ => None,
                    };
                    if let Some(status) = folded {
                        item.insert("status".into(), json!(status));
                    }
                }
            }
        }
        "ask_user_questions" => {
            if map.get("questions").is_some_and(Value::is_object) {
                if let Some(obj) = map.remove("questions") {
                    map.insert("questions".into(), Value::Array(vec![obj]));
                }
            }
            if !map.contains_key("questions")
                && (map.contains_key("question")
                    || map.contains_key("prompt")
                    || map.contains_key("text"))
            {
                let mut single = serde_json::Map::new();
                for key in [
                    "title", "label", "header", "question", "prompt", "text", "type", "options",
                    "choices",
                ] {
                    if let Some(val) = map.remove(key) {
                        single.insert(key.to_string(), val);
                    }
                }
                map.insert(
                    "questions".into(),
                    Value::Array(vec![Value::Object(single)]),
                );
            }
            if let Some(list) = map.get_mut("questions").and_then(Value::as_array_mut) {
                for question in list {
                    let Some(item) = question.as_object_mut() else {
                        continue;
                    };
                    fold_field(item, "label", "title");
                    fold_field(item, "header", "title");
                    fold_field(item, "prompt", "question");
                    fold_field(item, "text", "question");
                    if !item.contains_key("options") {
                        if let Some(choices) = item.remove("choices") {
                            item.insert("options".into(), choices);
                        }
                    }
                    let folded_type = match item.get("type").and_then(Value::as_str).map(str::trim)
                    {
                        None | Some("") => Some("single"),
                        Some("single_choice" | "radio" | "one") => Some("single"),
                        Some("multi" | "multi_select" | "multiselect" | "checkbox" | "many") => {
                            Some("multiple")
                        }
                        _ => None,
                    };
                    if let Some(kind) = folded_type {
                        item.insert("type".into(), json!(kind));
                    }
                    if let Some(opts) = item.get_mut("options").and_then(Value::as_array_mut) {
                        for opt in opts {
                            if let Some(s) = opt
                                .as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                            {
                                *opt = json!({ "label": s.clone(), "description": s });
                            } else if let Some(opt_map) = opt.as_object_mut() {
                                fold_field(opt_map, "title", "label");
                                fold_field(opt_map, "name", "label");
                                fold_field(opt_map, "value", "label");
                                let has_desc = opt_map
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .is_some_and(|d| !d.trim().is_empty());
                                if !has_desc {
                                    if let Some(lbl) = opt_map
                                        .get("label")
                                        .and_then(Value::as_str)
                                        .map(str::trim)
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                    {
                                        opt_map.insert("description".into(), json!(lbl));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(Value::String(said)) = map.get_mut("action") {
        if said.chars().count() > ACTION_MAX {
            *said = said.chars().take(ACTION_MAX).collect();
        }
    }
}

/// The same field under the name another agent's tool gives it.
///
/// Adopted only where the declared field is absent or blank; a call that carries
/// both (`content` empty beside a real `text`) keeps the declared one and drops
/// the stray, so the strict schema does not refuse a call whose meaning is
/// already clear.
fn fold_field(item: &mut serde_json::Map<String, Value>, from: &str, to: &str) {
    let Some(value) = item.get(from).cloned() else {
        return;
    };
    let declared = item
        .get(to)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if declared {
        item.remove(from);
        return;
    }
    if value.as_str().is_some_and(|text| text.trim().is_empty()) {
        return;
    }
    item.insert(to.to_string(), value);
    item.remove(from);
}

/// The tool's own opening sentence — what it does, in the words it declares.
fn first_sentence(description: &str) -> String {
    let trimmed = description.trim();
    let end = trimmed.find(". ").map(|at| at + 1).unwrap_or(trimmed.len());
    let sentence = trimmed[..end].trim();
    if sentence.is_empty() {
        return trimmed.to_string();
    }
    head_bytes(sentence, 120).to_string()
}

/// The name a model reaches for instead of the real one.
///
/// Every entry is a plain synonym of a Rune tool, never a redirect to a
/// different capability. Explicit and short on purpose: a similarity score
/// matches `write_files` to `patch_file` — nearer by edit distance than to
/// `create_file` — and taking that would be a silently wrong tool. A name that
/// is nobody's synonym stays unknown, and is told what the real ones are.
fn synonym(name: &str) -> Option<&'static str> {
    Some(match name {
        // The plural names are these tools' own former names.
        "inspect_files" | "read_file" | "read_files" | "read_image" => "inspect_file",
        "create_files" | "write_file" | "write_files" => "create_file",
        "patch_files" | "edit_file" | "edit_files" | "modify_file" => "patch_file",
        // `manage_process` and `terminal_send_input` were this tool's two
        // former names until 2026-11-03; a transcript, a stored row or a model
        // that learned them still resolves to the one tool that replaced them.
        "monitoring_bg"
        | "manage_processes"
        | "manage_process_list"
        | "manage_process"
        | "terminal_send_input"
        | "send_terminal_input"
        | "send_input"
        | "kill_process"
        | "stop_process" => "process_control",
        "terminal" | "run_command" | "run_terminal_cmd" | "bash" | "shell" => "terminal_use",
        "grep" | "search" | "search_file" => "search_text",
        "glob" | "find" | "find_file" => "find_files",
        "track_task" | "update_plan" | "todo" => "track_tasks",
        "ask_user_question" | "ask_questions" | "ask_user" => "ask_user_questions",
        _ => return None,
    })
}

/// A name reduced to what it is: case and separators dropped, nothing else.
///
/// Only spelling drift gets here. English plural endings stay in [`synonym`]
/// rather than being stripped by rule — `files` is `file` but `processes` is
/// `process`, and a rule that guesses between them quietly resolves a name to the
/// wrong tool, which is worse than not resolving it at all.
fn folded(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub fn field_str<'a>(item: &'a Value, key: &str) -> Result<&'a str> {
    item.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::invalid(format!("`{key}` is required and must be a string")))
}

pub fn field_opt_str<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key).and_then(|v| v.as_str())
}

/// An optional text argument that says something, trimmed. A model that sends
/// every declared field fills the ones it does not mean with `""` or a blank
/// (every `cx/*` model, measured 2026-10-10), so absent, empty and blank are the
/// same answer: not given.
pub fn field_given_str<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    field_opt_str(item, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn field_opt_u64(item: &Value, key: &str) -> Option<u64> {
    item.get(key).and_then(|v| v.as_u64())
}

pub fn field_bool(item: &Value, key: &str, default: bool) -> bool {
    item.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

/// Short on purpose: this is read by every model on every request, and the two
/// clauses say everything a longer instruction would.
pub const ACTION_DESC: &str = "FILL THIS FIRST before any other field: one short present-tense sentence stating what you are about to do and to what.";

/// Shared structure, not fifteen copies of the agent's writing instruction.
/// The system prompt owns how `action` is written; every schema still requires
/// it first and validates the same nonempty bounded string.
pub fn action_field() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 200,
        "description": ACTION_DESC
    })
}

/// Standard atomic schema scaffold: `action` first, followed by the tool's properties.
pub fn flat_schema(properties: Value, required: &[&str]) -> Value {
    let mut props = serde_json::Map::new();
    props.insert("action".into(), action_field());
    if let Some(fields) = properties.as_object() {
        for (name, spec) in fields {
            props.insert(name.clone(), spec.clone());
        }
    }
    let mut req = vec![json!("action")];
    for r in required {
        if *r != "action" {
            req.push(json!(*r));
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "additionalProperties": false,
        "required": req,
    })
}

/// `1 line`, `2 lines`. Small, and the alternative is "1 lines" in the one
/// sentence a user reads before allowing a change to their disk.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        return format!("{n} {word}");
    }
    // English, minimally: the only irregular ending this program produces is the
    // sibilant one ("match" → "matches"). Anything more is a library nobody
    // needs for four nouns.
    let suffix = if word.ends_with(['s', 'x', 'h']) {
        "es"
    } else {
        "s"
    };
    format!("{n} {word}{suffix}")
}

/// Take up to `max_bytes` from the start of a string at a valid UTF-8 boundary.
pub fn head_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Pick a unique 8-char filename id matching BE Server/Utils/PersistedOutput.js.
/// Uses 6 random bytes -> base64url sliced to 8 chars.
pub fn unique_persist_id(dir: &std::path::Path, ext: &str) -> String {
    use base64::Engine;
    use std::io::Read;
    for _ in 0..5 {
        let mut bytes = [0u8; 6];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut bytes).is_ok() {
                let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
                let candidate = if id.len() > 8 { &id[..8] } else { &id };
                let filename = format!("{candidate}{ext}");
                if !dir.join(&filename).exists() {
                    return candidate.to_string();
                }
            }
        }
    }
    let ts = crate::store::now_ms();
    let pid = std::process::id();
    let mut bytes = [0u8; 6];
    bytes[0..4].copy_from_slice(&((ts as u32) ^ pid).to_le_bytes());
    bytes[4..6].copy_from_slice(&((ts >> 32) as u16).to_le_bytes());
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    if id.len() > 8 {
        id[..8].to_string()
    } else {
        id
    }
}

/// Cut oversized content at a character boundary and say so.
pub fn cap(text: String, max: usize) -> String {
    let total_chars = text.chars().count();
    if total_chars <= max {
        return text;
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{}\n\n{kept}",
        notice::inline(&format!(
            "The result is {total_chars} characters; only the first {max} are shown."
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_envelope_is_shared_bounded_and_keeps_metadata_out_of_payload() {
        let mut values = crate::settings::configuration::Configuration::default()
            .values()
            .clone();
        for section in ["terminal", "process_control"] {
            values[section]["max_output_chars"] = json!(100);
            values[section]["truncate_head_bytes"] = json!(100);
            values[section]["truncate_tail_bytes"] = json!(100);
        }
        let mut config =
            crate::settings::configuration::Configuration::from_values(values).unwrap();
        config.home = std::env::temp_dir().join(format!("rune-envelope-{}", std::process::id()));
        let raw = format!("{}{}", "é".repeat(100), "z".repeat(150));
        let metadata = json!({"exit_code":7,"status":"Completed"});
        let first = Outcome::process_output(
            "done".into(),
            [Ok(raw.clone())],
            metadata.clone(),
            &config,
            "terminal",
            "terminal_use",
            &config.home.join("sessions/0/tools_result"),
        );
        let second = Outcome::process_output(
            "done".into(),
            raw.chars().map(|c| Ok(c.to_string())),
            metadata,
            &config,
            "process_control",
            "process_control",
            &config.home.join("sessions/0/tools_result"),
        );
        // The saved copy's path differs per tool; everything else must not.
        let normalized = |content: &str| match content.split_once(" The full output is saved at ") {
            Some((before, after)) => format!(
                "{before}{}",
                after
                    .split_once(".</tool_notice>")
                    .map_or("", |(_, rest)| rest)
            ),
            None => content.to_string(),
        };
        assert_eq!(
            normalized(&first.content),
            normalized(&second.content),
            "chunk boundaries must not alter output"
        );
        for tool in ["terminal_use", "process_control"] {
            let dir = config.home.join("sessions/0/tools_result").join(tool);
            let files: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|file| file.unwrap().path())
                .collect();
            assert!(!files.is_empty());
            for file in files {
                assert_eq!(std::fs::read_to_string(file).unwrap(), raw);
            }
        }
        assert_eq!(first.content.matches("<tool_notice>").count(), 1);
        assert_eq!(first.content.matches("</tool_notice>").count(), 1);
        assert!(
            first.content.find("</tool_notice>").unwrap()
                < first.content.find("<preview_head").unwrap()
        );
        assert!(!first.content.contains("<tool_metadata>"));
        assert!(!first.content.contains("exit_code"));
        assert!(first.content.contains(&"é".repeat(50)));
        assert!(first.content.contains(&"z".repeat(50)));
        assert!(first.content.contains("250 characters, over the 100 limit"));
        let intact = Outcome::process_output(
            "done".into(),
            [Ok("a\n\n b  \n".into())],
            json!({}),
            &config,
            "terminal",
            "terminal_use",
            &config.home.join("sessions/0/tools_result"),
        );
        assert!(intact.content.ends_with("a\n\n b  \n"));
        assert!(!intact.content.contains("preview_head"));
        std::fs::remove_dir_all(&config.home).unwrap();
    }

    #[test]
    fn failed_persistence_never_claims_complete_saved_output() {
        let mut values = crate::settings::configuration::Configuration::default()
            .values()
            .clone();
        values["terminal"]["max_output_chars"] = json!(100);
        let mut config =
            crate::settings::configuration::Configuration::from_values(values).unwrap();
        let path =
            std::env::temp_dir().join(format!("rune-envelope-blocked-{}", std::process::id()));
        std::fs::write(&path, "not a directory").unwrap();
        config.home = path.clone();
        let result = Outcome::process_output(
            "done".into(),
            [Ok("x".repeat(300))],
            json!({}),
            &config,
            "terminal",
            "terminal_use",
            &config.home.join("sessions/0/tools_result"),
        );
        assert!(!result.ok);
        assert!(result.content.contains("The output could not be saved"));
        assert!(
            !result.content.contains("The full output is saved at"),
            "a failed save is never claimed whole"
        );
        assert!(result.content.contains("preview_head") && result.content.contains("preview_tail"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn structured_responses_persist_complete_json_without_preview() {
        let mut values = crate::settings::configuration::Configuration::default()
            .values()
            .clone();
        for section in ["discover_capabilities", "read_mcp_resource"] {
            values[section]["max_output_chars"] = json!(100);
        }
        let mut config =
            crate::settings::configuration::Configuration::from_values(values).unwrap();
        config.home =
            std::env::temp_dir().join(format!("rune-structured-output-{}", std::process::id()));
        let raw = json!({"entries": ["é".repeat(300)], "next_cursor": "next-page"}).to_string();
        for tool in ["discover_capabilities", "read_mcp_resource"] {
            let outcome = Outcome::process_output(
                "read".into(),
                [Ok(raw.clone())],
                json!({}),
                &config,
                tool,
                tool,
                &config.home.join("sessions/0/tools_result"),
            );
            assert!(outcome.ok);
            assert!(outcome.content.starts_with("<tool_notice>"));
            assert!(!outcome.content.contains("preview_head"));
            assert!(!outcome.content.contains("preview_tail"));
            assert!(!outcome.content.contains("next-page"));
            let dir = config.home.join("sessions/0/tools_result").join(tool);
            let file = std::fs::read_dir(dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let saved = std::fs::read_to_string(file).unwrap();
            assert_eq!(saved, raw);
            assert!(serde_json::from_str::<Value>(&saved).is_ok());
        }
        std::fs::remove_dir_all(config.home).unwrap();
    }

    #[test]
    fn every_builtin_schema_has_provider_safe_nonempty_enums() {
        fn walk(value: &Value, path: &str) {
            match value {
                Value::Object(map) => {
                    if let Some(list) = map.get("enum").and_then(Value::as_array) {
                        assert!(!list.is_empty(), "{path}: empty enum");
                        assert!(
                            list.iter()
                                .all(|v| !v.as_str().is_some_and(|s| s.trim().is_empty())),
                            "{path}: blank enum member"
                        );
                    }
                    for (key, value) in map {
                        walk(value, &format!("{path}/{key}"));
                    }
                }
                Value::Array(list) => {
                    for (index, value) in list.iter().enumerate() {
                        walk(value, &format!("{path}/{index}"));
                    }
                }
                _ => {}
            }
        }
        for declaration in Registry::default().declarations() {
            jsonschema::draft202012::options()
                .build(&declaration.parameters)
                .unwrap();
            walk(&declaration.parameters, &declaration.name);
        }
    }

    #[test]
    fn optional_builtin_text_defaults_are_validated_recursively_without_relaxing_external_schemas()
    {
        let registry = Registry::default();
        for blank in ["", "  ", "\t\n"] {
            for (name, mut args) in [
                ("find_files", json!({"action":"find","pattern":"*.rs"})),
                (
                    "search_text",
                    json!({"action":"search","pattern":"hello","include":blank}),
                ),
                ("terminal_use", json!({"action":"run","command":"pwd"})),
                (
                    "track_tasks",
                    json!({"action":"plan","tasks":[{"text":"first","status":"doing"},{"text":"later","status":blank}]}),
                ),
                (
                    "ask_user_questions",
                    json!({"action":"ask","questions":[{"title":blank,"question":"Which option?","type":"single","options":[{"label":"A","description":"Choose A"}]}]}),
                ),
            ] {
                registry.get(name).unwrap().validate(&args).unwrap();
                args["unknown"] = json!(blank);
                assert!(
                    registry.get(name).unwrap().validate(&args).is_err(),
                    "unknown field: {name}"
                );
            }
        }
        for (name, args) in [
            (
                "web_search",
                json!({"action":"search","query":"rust","type":"fetch"}),
            ),
            (
                "web_search",
                json!({"action":"search","query":"rust","type":null}),
            ),
            (
                "track_tasks",
                json!({"action":"plan","tasks":[{"text":"first","status":"invalid"}]}),
            ),
            (
                "ask_user_questions",
                json!({"action":"ask","questions":[{"question":"Which?","type":" ","options":[{"label":"A","description":"A"}]}]}),
            ),
        ] {
            assert!(
                registry.get(name).unwrap().validate(&args).is_err(),
                "{name}: {args}"
            );
        }
        let schema = json!({"type":"object","properties":{"type":{"type":"string","enum":["web","images"]}},"additionalProperties":false});
        assert!(
            validate_value(&schema, &json!({"type":""})).is_err(),
            "external schemas are exact"
        );
        assert!(validate_builtin(&schema, &json!({"type":" images "})).is_ok());
    }

    #[test]
    fn registry_holds_every_tool_once() {
        let r = Registry::default();
        let mut names = r.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "ask_user_questions",
                "call_mcp_tool",
                "create_file",
                "discover_capabilities",
                "find_files",
                "inspect_file",
                "patch_file",
                "process_control",
                "read_mcp_resource",
                "search_text",
                "terminal_use",
                "track_tasks",
                "web_fetch",
                "web_search"
            ]
        );
        let mut dedup = names.clone();
        dedup.dedup();
        assert_eq!(dedup.len(), names.len());
    }

    #[test]
    fn read_only_registry_contains_nothing_that_mutates() {
        let r = Registry::read_only(
            Arc::new(Limits::default()),
            Arc::new(Mutex::new(Settings::empty())),
        );
        for name in r.names() {
            assert!(!r.get(name).unwrap().mutates(), "{name} mutates");
        }
    }

    /// Only what reads and nothing else may run alongside other calls
    /// (owner, 2026-10-06): exactly these read-only built-ins, none of which
    /// changes anything, runs a command or belongs to another program.
    #[test]
    fn only_tools_that_merely_read_run_alongside_others() {
        let r = Registry::new(
            Arc::new(Limits::default()),
            Arc::new(Mutex::new(Settings::empty())),
        );
        let mut concurrent: Vec<&str> = r
            .names()
            .into_iter()
            .filter(|name| r.get(name).unwrap().concurrent())
            .collect();
        concurrent.sort_unstable();
        assert_eq!(
            concurrent,
            [
                "find_files",
                "inspect_file",
                "search_text",
                "web_fetch",
                "web_search"
            ]
        );
        for name in concurrent {
            let tool = r.get(name).unwrap();
            assert!(
                !tool.mutates() && !tool.runs_commands() && !tool.is_external(),
                "{name}"
            );
        }
    }

    struct Remote(&'static str);

    impl Tool for Remote {
        fn name(&self) -> &str {
            self.0
        }
        fn is_external(&self) -> bool {
            true
        }
        fn declaration(&self) -> ToolDecl {
            ToolDecl {
                name: self.0.into(),
                description: "remote".into(),
                parameters: json!({"type":"object"}),
            }
        }
        fn mutates(&self) -> bool {
            false
        }
        fn preview(&self, _args: &Value) -> String {
            String::new()
        }
        fn run(&self, _ctx: &mut ToolCtx<'_>, _args: &Value) -> Result<Outcome> {
            unreachable!("never run")
        }
    }

    /// The declarations never move: an MCP server's tools connecting and
    /// dropping changes what `call_mcp_tool` can reach, never what a request
    /// declares.
    #[test]
    fn the_declared_tools_never_follow_what_mcp_offers() {
        let registry = Registry::default();
        let built_in = registry.declared();
        assert!(Arc::ptr_eq(&built_in, &registry.declared()), "built once");

        registry
            .register(Arc::new(Remote("mcp__srv__echo")))
            .unwrap();
        assert!(
            Arc::ptr_eq(&built_in, &registry.declared()),
            "nothing moved"
        );
        assert!(!registry
            .declared()
            .list
            .iter()
            .any(|d| d.name.starts_with("mcp__")));
        registry.remove_server("mcp__srv__");
        assert!(
            Arc::ptr_eq(&built_in, &registry.declared()),
            "nothing moved"
        );
    }

    /// A `call_mcp_tool` call runs as the MCP tool it names; any other call runs
    /// as itself; a name no server offers is not found, and says how to look.
    #[test]
    fn a_proxied_call_resolves_to_the_tool_it_names() {
        let registry = Registry::default();
        let internal = crate::capabilities::mcp::internal_name("srv", "echo");
        let leaked: &'static str = Box::leak(internal.clone().into_boxed_str());
        registry.register(Arc::new(Remote(leaked))).unwrap();

        let (tool, name, args) = registry
            .target(
                "call_mcp_tool",
                &json!({"action":"echo","server_id":"srv","tool":"echo","arguments":{"text":"hi"}}),
            )
            .unwrap();
        assert_eq!(tool.name(), internal);
        assert_eq!(name, internal);
        assert_eq!(args, json!({"text":"hi"}));

        let (tool, name, _) = registry
            .target("find_files", &json!({"action":"x","pattern":"*"}))
            .unwrap();
        assert_eq!((tool.name(), name.as_str()), ("find_files", "find_files"));

        let missing = registry
            .target(
                "call_mcp_tool",
                &json!({"action":"x","server_id":"srv","tool":"absent","arguments":{}}),
            )
            .err()
            .unwrap()
            .to_string();
        assert!(missing.contains("discover_capabilities"), "{missing}");
        assert!(
            registry.get(&internal).is_err(),
            "an MCP tool is not callable by name"
        );
    }

    /// The cache contract for tool declarations: changing every configured limit
    /// a tool enforces leaves the declarations byte for byte as they were.
    #[test]
    fn declarations_do_not_move_when_the_configuration_does() {
        let home = std::env::temp_dir().join(format!("rune-decl-static-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = crate::settings::configuration::Shared::load(&home).unwrap();
        let registry = Registry::new(
            Arc::new(Limits::over(config.clone())),
            Arc::new(Mutex::new(Settings::empty())),
        );
        let before = serde_json::to_string(&registry.declarations()).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[find_files]\nmatches = 7\n[search_text]\nmatches = 9\n[web_search]\nresults = 3\n\
             [inspect_file]\nmax_file_size = \"1KiB\"\n[tasks]\nmax_tasks = 4\nmax_chars_per_task = 30\n\
             [terminal]\ncommand_timeout = \"5s\"\n[web_fetch]\ntimeout = \"7s\"\nmax_output_chars = 1234\n",
        )
        .unwrap();
        config.reload().unwrap();
        assert_eq!(
            config.number("find_files.matches"),
            7,
            "the edit took effect"
        );
        let after = serde_json::to_string(&registry.declarations()).unwrap();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn ordinary_tool_schemas_are_clean_and_flat() {
        for decl in Registry::default().declarations() {
            let props = decl
                .parameters
                .get("properties")
                .unwrap()
                .as_object()
                .unwrap();
            assert!(props.contains_key("action"), "{} has no action", decl.name);
            let required = decl.parameters.get("required").unwrap().as_array().unwrap();
            assert!(
                required.contains(&json!("action")),
                "{} must require action",
                decl.name
            );
            assert_eq!(
                decl.parameters.get("additionalProperties"),
                Some(&json!(false)),
                "{} must disallow additional properties",
                decl.name
            );
            // Non-composite tools have no artificial batch array envelopes
            for (key, spec) in props {
                if [
                    "tasks",
                    "questions",
                    "view_range",
                    "options",
                    "selected",
                    "answers",
                    "allowed_domains",
                    "blocked_domains",
                ]
                .contains(&key.as_str())
                {
                    continue;
                }
                assert_ne!(
                    spec.get("type").and_then(Value::as_str),
                    Some("array"),
                    "{} should not have batch array {key}",
                    decl.name
                );
            }
        }
    }

    #[test]
    fn the_listing_says_which_tools_can_change_the_machine() {
        let listing = Registry::default().listing();
        let mutates = |name: &str| {
            listing
                .iter()
                .find(|t| t["name"] == name)
                .and_then(|t| t["mutates"].as_bool())
                .unwrap()
        };
        assert!(mutates("create_file"));
        assert!(mutates("patch_file"));
        assert!(
            mutates("terminal_use"),
            "a shell is the most changing tool there is"
        );
        assert!(!mutates("inspect_file"));
        assert!(!mutates("web_search"));
        assert!(!mutates("web_fetch"));
    }

    #[test]
    fn unknown_tool_is_not_found() {
        assert!(Registry::default().get("rm_rf").is_err());
    }

    #[test]
    fn counts_read_as_english() {
        assert_eq!(plural(1, "line"), "1 line");
        assert_eq!(plural(0, "file"), "0 files");
        assert_eq!(plural(2, "file"), "2 files");
        assert_eq!(plural(3, "match"), "3 matches");
    }

    #[test]
    fn cap_marks_what_it_cut() {
        let big = "x".repeat(1_010);
        let out = cap(big, 1_000);
        assert!(out.contains("<tool_notice>") && out.contains("only the first 1000 are shown"));
        let small = "short".to_string();
        assert_eq!(cap(small.clone(), 1_000), small);
    }

    /// Every tool keeps the same required field while the writing rule is
    /// stated once in the agent prompt, not repeated in each schema.
    #[test]
    fn every_tool_validates_action_identically_without_repeating_its_instruction() {
        let registry = Registry::default();
        let expected = action_field();
        assert_eq!(expected["description"], ACTION_DESC);
        for decl in registry.declarations() {
            assert_eq!(
                decl.parameters["properties"]["action"], expected,
                "{} describes action differently",
                decl.name
            );
        }
    }

    /// `action` is the only thing the UI and the classifier read before a call
    /// runs, so an empty one tells them nothing. The schema has to say so.
    #[test]
    fn action_cannot_be_empty_on_any_tool() {
        for decl in Registry::default().declarations() {
            let field = &decl.parameters["properties"]["action"];
            assert_eq!(
                field["minLength"],
                json!(1),
                "{} allows a blank action",
                decl.name
            );
        }
        let tool = Registry::default().get("create_file").unwrap();
        assert!(tool
            .validate(&json!({"action": "", "path": "a", "text": "b"}))
            .is_err());
    }

    /// The spellings another agent's tools use, measured in the owner's sessions
    /// (audit 2026-11-01): each one is the same argument under another name, so
    /// the call runs instead of costing a round trip. Nothing else is forgiven —
    /// a name that is nobody's synonym is still refused.
    #[test]
    fn the_spelling_another_agent_uses_is_folded_before_validation() {
        let registry = Registry::default();
        let create = registry.get("create_file").unwrap();
        let mut args = json!({"action": "write it", "path": "README.md", "content": "hello"});
        canonical_args(create.as_ref(), &mut args);
        assert_eq!(args["text"], json!("hello"));
        assert!(args.get("content").is_none(), "{args}");
        create.validate(&args).unwrap();
        // The measured shape: an empty `content` beside a real `text`.
        let mut both = json!({"action":"write it","path":"a","content":"","text":"real"});
        canonical_args(create.as_ref(), &mut both);
        assert_eq!(both["text"], json!("real"));
        assert!(both.get("content").is_none(), "{both}");
        // A blank alias never becomes the body.
        let mut blank = json!({"action":"write it","path":"a","content":"  "});
        canonical_args(create.as_ref(), &mut blank);
        assert!(blank.get("text").is_none(), "{blank}");
        // Still a refusal, and still the schema's own words.
        let mut invented = json!({"action":"write it","path":"a","body":"hello"});
        canonical_args(create.as_ref(), &mut invented);
        assert!(create.validate(&invented).is_err(), "{invented}");

        let track = registry.get("track_tasks").unwrap();
        let mut plan = json!({"action":"plan the work","tasks":[
            {"text":"one","status":"in_progress"},
            {"text":"two","status":"pending"},
            {"text":"three","status":"completed"},
            {"text":"four"}]});
        canonical_args(track.as_ref(), &mut plan);
        assert_eq!(plan["tasks"][0]["status"], json!("doing"));
        assert_eq!(plan["tasks"][1]["status"], json!("todo"));
        assert_eq!(plan["tasks"][2]["status"], json!("done"));
        assert!(plan["tasks"][3].get("status").is_none(), "{plan}");
        track.validate(&plan).unwrap();

        let ask = registry.get("ask_user_questions").unwrap();
        let mut form = json!({"action":"ask the owner","questions":[
            {"question":"Which layout do you prefer for the new page?","label":"Layout","options":[
                {"label":"A","description":"the dense one"},
                {"label":"B","description":"the open one"}]},
            {"question":"Which font?","type":"multiple","title":"Font","options":[
                {"label":"A","description":"serif"}]}]});
        canonical_args(ask.as_ref(), &mut form);
        assert_eq!(form["questions"][0]["title"], json!("Layout"));
        assert_eq!(form["questions"][0]["type"], json!("single"));
        assert_eq!(form["questions"][1]["title"], json!("Font"));
        assert_eq!(form["questions"][1]["type"], json!("multiple"));
        ask.validate(&form).unwrap();
        // A question with neither label nor title passes: `title` is a label the
        // engine fills from the question itself, not a field to insist on.
        let mut untitled = json!({"action":"ask","questions":[
            {"question":"Which layout do you prefer for the page?","options":[
                {"label":"A","description":"one"}]}]});
        canonical_args(ask.as_ref(), &mut untitled);
        ask.validate(&untitled).unwrap();
        assert!(
            untitled["questions"][0].get("title").is_none(),
            "{untitled}"
        );
        // Flat top-level question with string-array options and missing action:
        let mut flat = json!({
            "question": "Which database engine should we use?",
            "type": "single_choice",
            "options": ["PostgreSQL (Recommended)", "SQLite"]
        });
        canonical_args(ask.as_ref(), &mut flat);
        ensure_action(&mut flat, ask.as_ref());
        ask.validate(&flat).unwrap();
        assert_eq!(flat["questions"][0]["type"], json!("single"));
        assert_eq!(
            flat["questions"][0]["options"][0]["label"],
            json!("PostgreSQL (Recommended)")
        );
        assert_eq!(
            flat["questions"][0]["options"][0]["description"],
            json!("PostgreSQL (Recommended)")
        );

        // Single-object questions wrapper with multi_select, choices alias, and option missing description:
        let mut single_obj = json!({
            "description": "x".repeat(250),
            "questions": {
                "prompt": "Which targets should we build?",
                "type": "multi_select",
                "choices": [
                    {"name": "Linux x64"},
                    {"title": "macOS ARM64", "description": ""}
                ]
            }
        });
        canonical_args(ask.as_ref(), &mut single_obj);
        ensure_action(&mut single_obj, ask.as_ref());
        ask.validate(&single_obj).unwrap();
        assert_eq!(
            single_obj["action"].as_str().unwrap().chars().count(),
            ACTION_MAX
        );
        assert_eq!(single_obj["questions"][0]["type"], json!("multiple"));
        assert_eq!(
            single_obj["questions"][0]["options"][0]["label"],
            json!("Linux x64")
        );
        assert_eq!(
            single_obj["questions"][0]["options"][1]["description"],
            json!("macOS ARM64")
        );
    }

    /// The sentence a card shows is a label, not payload: a paragraph is cut to
    /// the declared length instead of costing a whole round trip.
    #[test]
    fn a_sentence_too_long_to_read_is_cut_not_refused() {
        let registry = Registry::default();
        let tool = registry.get("inspect_file").unwrap();
        let mut args = json!({"action": "x".repeat(400), "path": "a"});
        assert!(
            tool.validate(&args).is_err(),
            "the schema still asks for less"
        );
        canonical_args(tool.as_ref(), &mut args);
        assert_eq!(
            args["action"].as_str().unwrap().chars().count(),
            ACTION_MAX,
            "cut, not refused"
        );
        tool.validate(&args).unwrap();
    }

    #[test]
    fn a_name_the_model_reached_for_still_finds_the_tool() {
        let registry = Registry::default();
        for (said, meant) in [
            ("write_files", "create_file"),
            ("write_file", "create_file"),
            ("read_files", "inspect_file"),
            ("edit_file", "patch_file"),
            ("bash", "terminal_use"),
        ] {
            assert_eq!(
                registry.get(said).unwrap().name(),
                meant,
                "{said} did not reach {meant}"
            );
        }
    }

    /// The two tools `process_control` replaced (owner, 2026-11-03) are still
    /// names: an older session's transcript, a stored row and a model that
    /// learned them all reach the one tool that does their work now — and no
    /// registry holds them under their old names any more.
    #[test]
    fn every_former_name_of_process_control_reaches_it() {
        let registry = Registry::default();
        for said in [
            "manage_process",
            "manage_processes",
            "manage_process_list",
            "monitoring_bg",
            "terminal_send_input",
            "send_terminal_input",
            "send_input",
            "kill_process",
            "stop_process",
            "Manage_Process",
            "TERMINAL_SEND_INPUT",
        ] {
            assert_eq!(
                registry.get(said).unwrap().name(),
                "process_control",
                "{said} did not reach process_control"
            );
        }
        for gone in ["manage_process", "terminal_send_input"] {
            assert!(
                !registry.names().contains(&gone),
                "{gone} is a tool of its own again"
            );
        }
        assert!(registry.names().contains(&"process_control"));
    }

    fn workspace(tag: &str) -> (std::path::PathBuf, crate::jail::Jail) {
        let root = std::env::temp_dir().join(format!("rune-scope-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::create_dir_all(root.join(".rune/skills/one")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/ui/draw.rs"), "// draw\n").unwrap();
        std::fs::write(root.join(".rune/skills/one/SKILL.md"), "# one\n").unwrap();
        let root = crate::platform::canonical(&root).unwrap();
        let jail = crate::jail::Jail::new(&root).unwrap();
        (root, jail)
    }

    /// A pattern's walk starts in the folders it names, a pattern that is one
    /// file's path — relative or absolute — is that file, and a name with no
    /// folder matches at any depth from the workspace root.
    #[test]
    fn a_pattern_is_searched_where_it_names() {
        let (root, jail) = workspace("names");
        let scope = Scope::of(&jail, "src/**/*.rs").unwrap();
        assert_eq!(scope.dir, root.join("src"));
        assert!(scope.glob.as_ref().unwrap().matches("ui/draw.rs"));
        assert!(scope.file.is_none());

        for one in [
            "src/main.rs".to_string(),
            root.join("src/main.rs").display().to_string(),
            // Every separator the platform's own (`\` on Windows).
            root.join("src").join("main.rs").display().to_string(),
        ] {
            let scope = Scope::of(&jail, &one).unwrap();
            assert_eq!(
                scope.file.as_deref(),
                Some(root.join("src/main.rs").as_path()),
                "{one}"
            );
        }

        let bare = Scope::of(&jail, "*.rs").unwrap();
        assert_eq!(bare.dir, root);
        assert!(bare.glob.as_ref().unwrap().matches("src/ui/draw.rs"));

        // Named, a folder no walk enters on its own is searched.
        let skills = Scope::of(&jail, ".rune/skills/**/SKILL.md").unwrap();
        assert_eq!(skills.dir, root.join(".rune/skills"));
        assert!(skills.glob.as_ref().unwrap().matches("one/SKILL.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Windows writes a path with `\`: it is a separator there, so a path
    /// copied from it searches the folder and file it names; elsewhere `\`
    /// stays the glob's escape. A drive alone is its root.
    #[test]
    fn a_windows_path_is_spelt_with_slashes_for_the_walk() {
        use crate::platform::Os;
        assert_eq!(
            spelt_for_walk(r"D:\work\src\main.rs", Os::Windows),
            "D:/work/src/main.rs"
        );
        assert_eq!(spelt_for_walk(r"src\*.rs", Os::Windows), "src/*.rs");
        assert_eq!(spelt_for_walk(r"a\*b", Os::Linux), r"a\*b");
        assert_eq!(spelt_for_walk(r"a\*b", Os::MacOs), r"a\*b");
        let (root, rest) = crate::scan::glob::split_walk_root("C:/*.rs");
        assert_eq!(
            (root, rest),
            ("C:", "*.rs"),
            "made the drive's root by Scope"
        );
    }

    /// A link is never followed by a walk, so a pattern naming one is not taken
    /// as a file to search: what it points at may lie outside the workspace.
    #[test]
    fn a_pattern_naming_a_link_is_not_its_target() {
        // A link needs a right Windows may not grant a test.
        if crate::platform::OS == crate::platform::Os::Windows {
            return;
        }
        let (root, jail) = workspace("link");
        crate::platform::fixture::symlink("/etc/hostname", root.join("src/host")).unwrap();
        let scope = Scope::of(&jail, "src/host").unwrap();
        assert!(scope.file.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_handles_complex_brace_expansions_from_session_714() {
        let (root, jail) = workspace("brace_scope");

        // 1. Pattern 11: docs/Current/{Desktop,Mobile,Backend}/**/*{Deploy,Port,Build,Release}*.md
        let p11 = "Cloud/SeaDrive/docs/Current/{Desktop,Mobile,Backend}/**/*{Deploy,Port,Build,Release}*.md";
        let scope = Scope::of(&jail, p11).unwrap();
        assert_eq!(scope.dir, root.join("Cloud/SeaDrive/docs/Current"));
        let glob = scope.glob.as_ref().unwrap();
        assert!(glob.matches("Desktop/Deploy.md"));
        assert!(glob.matches("Desktop/Sub/DeployInstructions.md"));
        assert!(glob.matches("Mobile/PortConfig.md"));
        assert!(glob.matches("Backend/BuildGuide.md"));
        assert!(glob.matches("Backend/ReleaseNotes.md"));
        assert!(!glob.matches("Desktop/OtherFile.txt"));
        assert!(!glob.matches("Other/Deploy.md"));

        // 2. Pattern 12: heterogeneous paths
        let p12 = "Cloud/SeaDrive/{DeploySeaDrive.sh,scripts/*.sh,Application/Desktop/**/*.sh,Application/Mobile/**/*.sh}";
        let scope = Scope::of(&jail, p12).unwrap();
        assert_eq!(scope.dir, root.join("Cloud/SeaDrive"));
        let glob = scope.glob.as_ref().unwrap();
        assert!(glob.matches("DeploySeaDrive.sh"));
        assert!(glob.matches("scripts/start.sh"));
        assert!(!glob.matches("scripts/sub/start.sh")); // single star inside scripts/*.sh
        assert!(glob.matches("Application/Desktop/build.sh"));
        assert!(glob.matches("Application/Desktop/nested/deep/run.sh"));
        assert!(glob.matches("Application/Mobile/ci.sh"));
        assert!(!glob.matches("random.sh"));

        // 3. Multi-literal files direct lookup (no walk)
        std::fs::write(root.join("WebsiteDev.sh"), "#!/bin/sh").unwrap();
        std::fs::create_dir_all(root.join("Studio")).unwrap();
        std::fs::write(root.join("Studio/WebsiteDev.sh"), "#!/bin/sh").unwrap();
        let p7 = "{WebsiteDev.sh,Studio/WebsiteDev.sh,Cloud/SeaDrive/WebsiteDev.sh}";
        let scope = Scope::of(&jail, p7).unwrap();
        assert_eq!(scope.files.len(), 2);
        assert!(scope.files.contains(&root.join("WebsiteDev.sh")));
        assert!(scope.files.contains(&root.join("Studio/WebsiteDev.sh")));
        assert!(!scope
            .files
            .contains(&root.join("Cloud/SeaDrive/WebsiteDev.sh")));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A walk cut at a bound says which bound and how to go on; one that ran
    /// whole says nothing.
    #[test]
    fn a_walk_cut_at_its_bounds_says_so() {
        let config = crate::settings::configuration::Shared::new(
            crate::settings::configuration::Configuration::default(),
        );
        let stats = |too_many, too_deep| crate::scan::walk::Stats {
            truncated: too_many || too_deep,
            too_many,
            too_deep,
            ..Default::default()
        };
        assert_eq!(walk_stopped(&config, &stats(false, false), "x."), None);
        let many = walk_stopped(&config, &stats(true, false), "name a folder.").unwrap();
        assert_eq!(
            many,
            "<tool_notice>The search stopped at scan.max_entries (200000 files) before every file was looked at; name a folder.</tool_notice>"
        );
        let deep = walk_stopped(&config, &stats(false, true), "go.").unwrap();
        assert!(deep.contains("scan.max_depth (32 folders deep)"), "{deep}");
        let both = walk_stopped(&config, &stats(true, true), "go.").unwrap();
        assert!(both.contains("200000 files) and scan.max_depth"), "{both}");
    }

    /// Separators and case are spelling and resolve on their own. A plural or a
    /// different word does not — that is a named variant, and a guess between
    /// them would pick the wrong tool silently.
    #[test]
    fn spelling_drift_resolves_but_a_different_word_does_not() {
        let registry = Registry::default();
        assert_eq!(registry.get("Create_File").unwrap().name(), "create_file");
        assert_eq!(registry.get("createfile").unwrap().name(), "create_file");
        // A former name, in any case, is still a name the model learned.
        assert_eq!(registry.get("Create_Files").unwrap().name(), "create_file");
        assert_eq!(registry.get("patch_files").unwrap().name(), "patch_file");
        assert_eq!(
            registry.get("INSPECT_FILES").unwrap().name(),
            "inspect_file"
        );
        assert_eq!(registry.get("INSPECT_FILE").unwrap().name(), "inspect_file");
        assert_eq!(
            registry.get("ask_user_question").unwrap().name(),
            "ask_user_questions"
        );
        // Every letter plausible and nearer to patch_file than create_file by
        // edit distance, which is exactly why the variants are a table.
        assert_eq!(registry.get("Write_Files").unwrap().name(), "create_file");
        assert!(registry.get("mangle_files").is_err());
        assert!(registry.get("delete_repository").is_err());
    }

    /// A bare "not found" leaves the model to guess a second time, usually the
    /// same wrong guess. It is told the real names instead.
    #[test]
    fn an_unknown_name_names_the_tools_that_do_exist() {
        let registry = Registry::default();
        let Err(e) = registry.get("write_filez") else {
            panic!("an unknown name must not resolve");
        };
        let text = e.to_string();
        assert!(text.contains("write_filez"), "{text}");
        for name in registry.names() {
            assert!(text.contains(name), "{name} missing from: {text}");
        }
    }

    #[test]
    fn a_dropped_action_names_the_missing_field_without_repeating_agent_policy() {
        let schema = Registry::default()
            .get("create_file")
            .unwrap()
            .declaration()
            .parameters;
        let err = validate_value(&schema, &json!({"path": "a", "text": "b"})).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("action"), "{text}");
        assert!(
            text.contains("Required field `action` is missing"),
            "{text}"
        );
        assert!(!text.contains("what you are about to do"), "{text}");
        assert!(!text.contains("Send the call again"), "{text}");
    }

    #[test]
    fn an_invented_field_is_named_back() {
        let schema = Registry::default()
            .get("create_file")
            .unwrap()
            .declaration()
            .parameters;
        let err = validate_value(
            &schema,
            &json!({"action": "write it", "path": "a", "text": "b", "content": "c"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("content"), "{err}");
    }

    /// 15 of 209 calls in one session were rejected for spelling this field the
    /// other way, and every one of them was a call that should have worked.
    /// Audit H1: markup inside the sentence is never lifted into an argument —
    /// a command must arrive as the command, where the schema checks it.
    #[test]
    fn markup_in_the_action_is_trimmed_never_promoted_to_an_argument() {
        let mut args = json!({ "action": "list the files <command>rm -rf /</command>" });
        adopt_action(&mut args);
        assert_eq!(args["action"], json!("list the files"));
        assert!(args.get("command").is_none());
    }

    #[test]
    fn a_description_instead_of_an_action_is_adopted() {
        let tool = Registry::default().get("terminal_use").unwrap();
        let mut args = json!({"description": "read the captured log", "command": "tail -20 log"});
        adopt_action(&mut args);
        assert_eq!(args["action"], "read the captured log");
        assert!(
            args.get("description").is_none(),
            "left behind, it would be rejected as an undeclared field"
        );
        assert!(tool.validate(&args).is_ok(), "{:?}", tool.validate(&args));
    }

    #[test]
    fn an_explicit_action_always_wins_and_a_blank_description_never_invents_one() {
        let mut both = json!({"action": "do the real thing", "description": "noise"});
        adopt_action(&mut both);
        assert_eq!(both["action"], "do the real thing");
        assert_eq!(
            both["description"], "noise",
            "nothing to fold in, so nothing changes"
        );

        let mut blank = json!({"description": "   ", "command": "ls"});
        adopt_action(&mut blank);
        assert!(
            blank.get("action").is_none(),
            "a blank sentence is not a sentence, and the UI is owed better than nothing"
        );
        assert!(Registry::default()
            .get("terminal_use")
            .unwrap()
            .validate(&blank)
            .is_err());
    }

    /// A call with no usable sentence must still run and still show one. Rejecting
    /// it is the opposite of what the field is for.
    #[test]
    fn a_call_with_nothing_to_say_still_gets_a_true_sentence_and_still_runs() {
        let tool = Registry::default().get("create_file").unwrap();
        for blank in [
            json!({"path": "a", "text": "b"}),
            json!({"action": "", "path": "a", "text": "b"}),
        ] {
            let mut args = blank;
            ensure_action(&mut args, tool.as_ref());
            let said = args["action"].as_str().unwrap_or_default();
            assert!(
                !said.trim().is_empty(),
                "the UI would get a blank card: {args}"
            );
            assert!(
                said.contains("Create"),
                "the fallback must be what the tool does, not a placeholder: {said}"
            );
            assert!(
                tool.validate(&args).is_ok(),
                "and the call must still run: {args}"
            );
        }
    }

    /// The model's own sentence is the point; the floor never overwrites it.
    #[test]
    fn a_sentence_the_model_wrote_is_never_replaced() {
        let tool = Registry::default().get("create_file").unwrap();
        let mut args = json!({"action": "add the greeting function", "path": "a", "text": "b"});
        ensure_action(&mut args, tool.as_ref());
        assert_eq!(args["action"], "add the greeting function");
    }
}
