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
#[path = "MonitoringBg.rs"]
pub mod monitoring;
pub mod notice;
pub mod patch;
#[path = "ReadMcpResource.rs"]
pub mod resource;
pub mod search;
pub mod send_input;
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
    /// Rune's word about the process, ahead of what the capture has to say:
    /// it goes in the same `Additional information` field, never a second
    /// envelope.
    pub(super) fn inform_first(mut self, sentence: &str) -> ProcessOutput {
        self.information.insert(0, sentence.to_string());
        self
    }

    pub(super) fn into_outcome(mut self, report: Option<notice::Notice>) -> Outcome {
        self.outcome.content = match report {
            Some(mut report) => {
                if !self.information.is_empty() {
                    report = report.field("Additional information", self.information.join(" "));
                }
                if self.outcome.content.is_empty() {
                    report.render()
                } else {
                    format!("{}\n\n{}", report.render(), self.outcome.content)
                }
            }
            None => {
                let notices: Vec<_> = self
                    .information
                    .iter()
                    .map(|text| notice::inline(text))
                    .collect();
                if notices.is_empty() {
                    self.outcome.content
                } else if self.outcome.content.is_empty() {
                    notices.join("\n")
                } else {
                    format!("{}\n\n{}", notices.join("\n"), self.outcome.content)
                }
            }
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
            .into_outcome(None)
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
                    format!(
                        "The response is {total} characters, over the {max} limit; read it with inspect_file.{kept}"
                    ),
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
/// The two mistakes it actually makes here, a dropped `action` and a field it
/// invented, are answered with the replacement call. Everything else keeps the
/// validator's own wording, which is already specific about the field.
pub(crate) fn correction(e: &jsonschema::ValidationError<'_>) -> String {
    use jsonschema::error::ValidationErrorKind as Kind;
    match e.kind() {
        Kind::Required { property } => match property.as_str() {
            Some("action") => {
                format!("`action` is missing, and every call needs it. {ACTION_DESC}")
            }
            Some(name) => format!("`{name}` is missing. Send the call again with `{name}`."),
            None => "A required field is missing. Send the call again with it.".to_string(),
        },
        Kind::AdditionalProperties { unexpected } => format!(
            "unknown field(s) {}. Send only the fields this tool declares.",
            unexpected
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Masked: the reason, never the value — an argument can carry a secret,
        // and this text reaches the audit trail and the screen.
        _ => format!(
            "Argument at `{}` is not valid: {}. Send the call again with that field corrected.",
            e.instance_path(),
            e.masked()
        ),
    }
}

/// The declarations a request carries, and what they cost in the estimate.
pub struct Declared {
    pub list: Vec<ToolDecl>,
    pub tokens: usize,
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
                Arc::new(terminal::TerminalUse(Arc::clone(&limits))),
                Arc::new(monitoring::ManageProcess(Arc::clone(&limits))),
                Arc::new(send_input::TerminalSendInput),
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

    /// The declarations on offer now, with their estimated tokens.
    pub fn declared(&self) -> Arc<Declared> {
        let mut held = crate::panic::lock(&self.declared);
        if let Some(declared) = held.as_ref() {
            return Arc::clone(declared);
        }
        let list = self.declarations();
        let tokens = crate::session::compact::estimate_tokens(
            &serde_json::to_string(&list).unwrap_or_default(),
        );
        let declared = Arc::new(Declared { list, tokens });
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
        "monitoring_bg" | "manage_processes" | "manage_process_list" => "manage_process",
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

/// The one description of `action` that every tool carries.
///
/// `action` is the only thing a human reads before a call runs, so its wording
/// is fixed here rather than chosen per tool. A tool that phrased it its own way
/// asks the model for a different shape of sentence from one call to the next,
/// and a model that has to guess is a model that drops it.
///
/// Short on purpose: this is read by every model on every request, and the two
/// clauses say everything a longer instruction would.
pub const ACTION_DESC: &str = "One short sentence: what you are about to do, and to what.";

/// The `action` property, byte for byte the same on every tool.
///
/// One home, so no tool can drift from another — which is what made the omission
/// uneven before, when some tools explained the field and two did not at all.
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
        for section in ["terminal", "manage_process"] {
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
            "manage_process",
            "manage_process",
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
        for tool in ["terminal_use", "manage_process"] {
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
                (
                    "find_files",
                    json!({"action":"find","pattern":"*.rs","path":blank}),
                ),
                (
                    "search_text",
                    json!({"action":"search","pattern":"hello","path":blank,"include":blank}),
                ),
                (
                    "terminal_use",
                    json!({"action":"run","command":"pwd","path":blank}),
                ),
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
                "manage_process",
                "patch_file",
                "read_mcp_resource",
                "search_text",
                "terminal_send_input",
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

    /// The omission was uneven because the instruction was: two tools explained
    /// `action` and two said nothing at all about it. One field, one wording.
    #[test]
    fn every_tool_describes_action_identically() {
        let registry = Registry::default();
        let expected = action_field();
        assert!(!expected["description"].as_str().unwrap().is_empty());
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
    fn a_dropped_action_is_answered_with_the_call_to_send() {
        let schema = Registry::default()
            .get("create_file")
            .unwrap()
            .declaration()
            .parameters;
        let err = validate_value(&schema, &json!({"path": "a", "text": "b"})).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("action"), "{text}");
        assert!(
            text.contains("what you are about to do"),
            "the remedy has to say what to write, not only that a field is missing: {text}"
        );
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
