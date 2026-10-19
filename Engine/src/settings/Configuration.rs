//! Canonical user configuration, validation and comment-preserving migration.
//! Loading this module never opens a database or contacts a provider.
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
pub const FILE: &str = "rune.toml";
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
#[derive(Clone, Copy)]
pub enum Kind {
    Count(u64, u64),
    Bool,
    Text,
    Duration,
    Size,
    Money,
    Model,
    Generation,
    ToolNames,
    Capabilities,
    /// A service connection's name (`[services]`), or null for the default.
    Connection,
}
pub struct Field {
    pub key: &'static str,
    pub default: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}
use Kind::*;
pub const FIELDS: &[Field] = &[
Field {key:"capabilities",default:"{}",kind:Capabilities,help:"Scoped skills, product workflows, MCP definitions and context budgets. Managed by rune -skills and rune -mcp."},
Field{key:"network.max_stream_size",default:"\"16MiB\"",kind:Size,help:"Maximum accumulated wire bytes in one model response."},
Field { key:"sanitizer.enabled", default:"true", kind:Bool, help:"Extract original user instructions before eligible classifier review by default. Set false to opt out; never runs for human-authorized, excluded, or inactive classifier actions." },
Field { key:"sanitizer.model", default:"null", kind:Model, help:"Omit to inherit the active model. Failure falls back to a distinct active route, then original user sources." },
Field { key:"sanitizer.timeout", default:"\"120s\"", kind:Duration, help:"Maximum time for one instruction extraction request." },
Field { key:"sanitizer.max_output_tokens", default:"4096", kind:Count(256,10000000), help:"Output allowance for sourced instruction extraction, bounded by the model's capability." },
Field{key:"generation",default:"null",kind:Generation,help:"Global generation defaults; keys match session generation settings. Stop is an exact string array."},
Field { key:"checkpoint.max_file_size", default:"\"64MiB\"", kind:Size, help:"Maximum snapshot size; larger writes fail before mutation." },
Field { key:"checkpoint.refresh_interval", default:"\"1s\"", kind:Duration, help:"Content refresh cadence for files already tracked by an open session; Rune writes publish immediately." },
Field { key:"checkpoint.diff.max_cells", default:"1000000", kind:Count(100,10000000), help:"Myers diff work budget per piece; past it the diff is anchored on unique lines." },
Field { key:"checkpoint.diff.context", default:"3", kind:Count(0,100), help:"Context lines in diffs." },
Field { key:"terminal.command_timeout", default:"\"120s\"", kind:Duration, help:"How long Rune waits for a foreground command before moving it to the background; the command continues running." },
Field { key:"terminal.shell", default:"\"auto\"", kind:Text, help:"Shell for commands: auto (bash where this machine has it, else /bin/sh; on Windows PowerShell 7, else Windows PowerShell, else cmd) or an absolute executable. Each shell gets a command the way it reads one: POSIX shells -c, PowerShell an encoded script, cmd /C." },
Field { key:"terminal.term", default:"\"xterm-256color\"", kind:Text, help:"TERM supplied to tool processes." },
Field { key:"terminal.inherit_environment", default:"false", kind:Bool, help:"Inherit all environment variables instead of the development allowlist." },
Field { key:"terminal.flush_interval", default:"\"150ms\"", kind:Duration, help:"Live output flush interval." },
Field { key:"terminal.input_wait_delay", default:"\"2s\"", kind:Duration, help:"How long a command must print nothing before Rune asks the system whether it is blocked waiting for typed input (checked once per quiet spell, on platforms that can tell)." },
Field { key:"terminal.drain_after_exit", default:"\"200ms\"", kind:Duration, help:"PTY output drain after exit." },
Field { key:"terminal.cancel_grace", default:"\"200ms\"", kind:Duration, help:"Termination grace after cancellation." },
Field { key:"terminal.timeout_grace", default:"\"2s\"", kind:Duration, help:"Termination grace after timeout." },
Field { key:"terminal.rows", default:"40", kind:Count(1,1000), help:"Initial headless terminal rows." },
Field { key:"terminal.columns", default:"200", kind:Count(1,1000), help:"Initial headless terminal columns." },
Field { key:"terminal.max_output_chars", default:"30000", kind:Count(100,10000000), help:"Maximum model-visible command output characters before truncation." },
Field { key:"terminal.max_output_size", default:"\"8MiB\"", kind:Size, help:"Maximum output a foreground command may print while it runs (held in memory for the live view and its result); past it the command is stopped." },
Field { key:"terminal.max_background_log_size", default:"\"100MiB\"", kind:Size, help:"Largest log a background task may write to disk (streamed, not held in memory). A task whose log passes it is stopped, so a runaway server cannot fill the disk." },
Field { key:"terminal.truncate_head_bytes", default:"1536", kind:Count(100,1000000), help:"Beginning output bytes preserved when truncated (1.5 KB)." },
Field { key:"terminal.truncate_tail_bytes", default:"1536", kind:Count(100,1000000), help:"Ending output bytes preserved when truncated (1.5 KB)." },
Field { key:"inspect_file.max_file_size", default:"\"16MiB\"", kind:Size, help:"Maximum file bytes read from disk." },
Field { key:"inspect_file.max_output_chars", default:"30000", kind:Count(100,10000000), help:"Maximum model-visible characters for text files." },
Field { key:"find_files.matches", default:"200", kind:Count(1,100000), help:"Maximum matching file paths returned." },
Field { key:"find_files.max_output_chars", default:"20000", kind:Count(100,10000000), help:"Maximum total characters returned by find_files." },
Field { key:"search_text.matches", default:"100", kind:Count(1,100000), help:"Maximum matching lines returned." },
Field { key:"search_text.max_line_chars", default:"2000", kind:Count(20,100000), help:"Maximum characters per matching line before truncation." },
Field { key:"search_text.max_output_chars", default:"30000", kind:Count(100,10000000), help:"Maximum model-visible characters for text search results." },
Field { key:"discover_capabilities.max_output_chars", default:"30000", kind:Count(100,10000000), help:"Maximum model-visible characters for capability discovery results." },
Field { key:"discover_capabilities.page_size", default:"12", kind:Count(1,100), help:"Default capability results returned per page." },
Field { key:"read_mcp_resource.max_output_chars", default:"50000", kind:Count(100,10000000), help:"Maximum model-visible characters for MCP resource content." },
Field { key:"manage_process.max_output_chars", default:"30000", kind:Count(100,10000000), help:"Maximum model-visible payload characters for managing background processes." },
Field { key:"manage_process.truncate_head_bytes", default:"1536", kind:Count(100,1000000), help:"Beginning payload bytes preserved when truncated." },
Field { key:"manage_process.truncate_tail_bytes", default:"1536", kind:Count(100,1000000), help:"Ending payload bytes preserved when truncated." },
Field { key:"tools.max_actions_per_response", default:"20", kind:Count(1,100), help:"Maximum tool calls allowed per response." },
Field { key:"scan.max_entries", default:"200000", kind:Count(1,10000000), help:"Most files one find_files or search_text call goes through, however many of them match; past it the search stops there. A limit on work, apart from the result limits (matches)." },
Field { key:"scan.max_depth", default:"32", kind:Count(1,256), help:"Deepest folder level find_files and search_text descend into; folders below it are not searched." },
Field { key:"scan.regex_steps", default:"200000", kind:Count(100,10000000), help:"Safety fuse for search_text patterns: the most matching steps one line may cost. A pattern that backtracks explosively (such as (a+)+b on a long line) gives up on that line after this many steps, the search goes on, and the model is told which lines it skipped. Ordinary patterns need far fewer." },
Field { key:"classifier.excluded_tools", default:"[\"ask_user_questions\",\"track_tasks\",\"web_search\",\"web_fetch\",\"find_files\",\"inspect_file\",\"read_mcp_resource\",\"discover_capabilities\",\"search_text\"]", kind:ToolNames, help:"Exact tool names excluded from model classifier review only; validation and permissions still apply. Empty array reviews all tools." },
Field { key:"questions.timeout", default:"\"unlimited\"", kind:Duration, help:"Maximum wait for a user question form (e.g. 30m or UNLIMITED for no timeout); expiry never submits answers." },
Field { key:"classifier.context_tools", default:"[\"terminal_use\",\"inspect_file\",\"find_files\",\"search_text\",\"create_file\",\"patch_file\"]", kind:ToolNames, help:"Tool inputs eligible as historical classifier context: the last three matching calls, independent of excluded_tools. Empty array includes none; outputs are never authorization." },
Field { key:"classifier.enabled", default:"true", kind:Bool, help:"Requires CLASSIFIER.md; always off in Ask." },
Field { key:"classifier.model", default:"null", kind:Model, help:"Omit to inherit the active request provider/model." },
Field { key:"classifier.timeout", default:"\"120s\"", kind:Duration, help:"Maximum classifier evaluation timeout." },
Field { key:"classifier.max_output_tokens", default:"1024", kind:Count(16,10000000), help:"Output tokens for a permission decision; a model that must reason gets auxiliary.mandatory_reasoning_output_tokens instead when that is larger." },
Field { key:"classifier.max_evidence_size", default:"\"256KiB\"", kind:Size, help:"Combined text evidence after formatting: user attachments, requests and tool inputs. Proposed arguments stay complete. Image bytes, policy and decision schema are outside this cap; provider payload limits still apply per request." },
Field { key:"classifier.max_policy_size", default:"\"64KiB\"", kind:Size, help:"Maximum bytes per policy file." },
Field { key:"compact.model", default:"null", kind:Model, help:"Omit to inherit the active request provider/model." },
Field { key:"compact.max_output_tokens", default:"32768", kind:Count(256,10000000), help:"Output tokens for a history summary; the summariser is told to fit within it." },
Field { key:"compact.keep_recent_turns", default:"5", kind:Count(1,100), help:"Turns kept verbatim." },
Field { key:"compact.fallback_context_tokens", default:"200000", kind:Count(1000,10000000), help:"Context budget assumed for a model that publishes no context window." },
Field { key:"compact.message_chars", default:"32000", kind:Count(100,1000000), help:"Unicode characters retained per old assistant/tool-result body. Original user instructions are processed whole, in batches when needed; attachments are separate." },
Field { key:"compact.tool_chars", default:"2000", kind:Count(100,1000000), help:"Unicode characters retained per historical tool call arguments, never one limit shared by an entire call array. Identity and target metadata stay visible." },
Field { key:"title.model", default:"null", kind:Model, help:"Omit to inherit the active request provider/model." },
Field { key:"title.max_output_tokens", default:"256", kind:Count(8,10000000), help:"Output tokens for a session title; a model that must reason gets auxiliary.mandatory_reasoning_output_tokens on top." },
Field { key:"title.max_chars", default:"60", kind:Count(1,200), help:"Maximum title characters." },
Field { key:"permissions.timeout", default:"\"10m\"", kind:Duration, help:"Human permission response timeout. Enter UNLIMITED for no timeout." },
Field { key:"auxiliary.mandatory_reasoning_output_tokens", default:"4096", kind:Count(1025,10000000), help:"Auxiliary output allowance when the selected model requires reasoning; includes the answer." },
Field { key:"network.connect_timeout", default:"\"10s\"", kind:Duration, help:"Connection establishment timeout." },
Field { key:"network.stream_idle_timeout", default:"\"5m\"", kind:Duration, help:"Longest a server may send nothing, before its answer starts or between two parts of it; past it the attempt is abandoned and retried. Raise it for a slow local model." },
Field { key:"network.catalog_timeout", default:"\"10s\"", kind:Duration, help:"Catalogue fetch timeout." },
Field { key:"network.catalog_ttl", default:"\"1h\"", kind:Duration, help:"Maximum age of fetched model capabilities and prices." },
Field { key:"network.catalog_max_pages", default:"100", kind:Count(1,1000), help:"Maximum catalogue pages per refresh." },
Field { key:"network.probe_timeout", default:"\"45s\"", kind:Duration, help:"Explicit provider test timeout." },
Field { key:"network.max_event_size", default:"\"8MiB\"", kind:Size, help:"Maximum SSE line/event bytes." },
Field { key:"network.max_response_size", default:"\"8MiB\"", kind:Size, help:"Maximum non-streamed response bytes." },
Field { key:"network.token_saver", default:"\"off\"", kind:Text, help:"Token-saving routers rewrite long tool results into summaries. 'off' (default) sends x-9router-token-saver: off so they keep tool results verbatim; 'on' stays silent and leaves compression to the router." },
Field { key:"retry.max_attempts", default:"5", kind:Count(0,4294967295), help:"Retries after the initial request; zero disables retrying." },
Field { key:"retry.initial_delay", default:"\"3s\"", kind:Duration, help:"Retry step: retry k waits step x k (3s, 6s, 9s, ...)." },
Field { key:"retry.max_delay", default:"\"1m\"", kind:Duration, help:"Longest retry wait; a wait that would pass it starts the series again from the step." },
Field { key:"retry.credit_cooldown", default:"\"15m\"", kind:Duration, help:"Key cooldown after insufficient balance." },
Field { key:"web_search.provider", default:"null", kind:Connection, help:"The search provider chosen in Tools: a connection or connection:route for a gateway. Omit to use the first search switched on. A missing provider makes search unavailable, never another serving." },
Field { key:"web_search.timeout", default:"\"60s\"", kind:Duration, help:"Search request timeout." },
Field { key:"web_search.results", default:"8", kind:Count(1,100), help:"Maximum returned search results." },
Field { key:"web_search.max_output_chars", default:"100000", kind:Count(100,10000000), help:"Maximum model-visible characters for web search results." },
Field { key:"web_fetch.provider", default:"null", kind:Connection, help:"The fetch provider chosen in Tools: a connection or connection:route for a gateway. Omit to use Rune's built-in reader. A missing provider makes fetch unavailable, never another serving." },
Field { key:"web_fetch.timeout", default:"\"60s\"", kind:Duration, help:"Public page request timeout." },
Field { key:"web_fetch.max_output_chars", default:"100000", kind:Count(100,10000000), help:"Maximum model-visible characters for fetched pages." },
Field { key:"attachments.max_text_chars", default:"60000", kind:Count(100,10000000), help:"Maximum inline text characters." },
Field { key:"attachments.max_history_files", default:"10", kind:Count(0,100), help:"Maximum historical attachments." },
Field { key:"memory.recall_size", default:"\"20KiB\"", kind:Size, help:"Memory index recall budget. Past it the tail is cut and the model is told to compact." },
Field { key:"instructions.max_chars", default:"32768", kind:Count(100,1000000), help:"AGENTS.md character budget (32 KiB, matching the Codex standard)." },
Field { key:"tasks.max_chars_per_task", default:"120", kind:Count(1,10000), help:"Maximum characters per task description." },
Field { key:"tasks.max_tasks", default:"20", kind:Count(1,1000), help:"Maximum tasks per session." },
Field { key:"storage.busy_timeout", default:"\"5s\"", kind:Duration, help:"SQLite contention timeout." },
Field { key:"storage.scratch_lifetime", default:"\"24h\"", kind:Duration, help:"Abandoned scratch lifetime." },
Field { key:"storage.session_limit", default:"200", kind:Count(1,10000), help:"Default session result limit." },
Field { key:"usage.warn_spend_usd", default:"0", kind:Money, help:"Spending notice; zero disables it." },
Field { key:"ui.theme", default:"\"amber\"", kind:Text, help:"violet, cyan or amber." },
Field { key:"ui.color", default:"\"auto\"", kind:Text, help:"auto, none, ansi16, ansi256 or truecolor." },
Field { key:"ui.mouse", default:"true", kind:Bool, help:"Mouse reporting; Shift-drag selects terminal text." },
Field { key:"ui.sidebar", default:"true", kind:Bool, help:"Conversation side panel default visibility." },
Field { key:"ui.scroll_lines", default:"2", kind:Count(1,30), help:"Rows one mouse-wheel notch moves the transcript; with Ctrl or Alt held, three times as far." },
Field { key:"ui.scroll_smooth", default:"true", kind:Bool, help:"Glide to where the wheel pointed instead of jumping there." },
Field { key:"ui.frame_interval", default:"\"16ms\"", kind:Duration, help:"UI refresh interval." },
Field { key:"ui.status_lifetime", default:"\"8s\"", kind:Duration, help:"Transient status lifetime." },
Field { key:"ui.quit_lifetime", default:"\"4s\"", kind:Duration, help:"Quit confirmation lifetime." },
Field { key:"ui.stop_lifetime", default:"\"3s\"", kind:Duration, help:"Stop confirmation lifetime." },
Field { key:"ui.escape_timeout", default:"\"50ms\"", kind:Duration, help:"Escape-sequence timeout." },
Field { key:"host.idle_lifetime", default:"\"60s\"", kind:Duration, help:"How long a detached session host waits with no client attached before exiting." },
Field { key:"ui.color_probe_timeout", default:"\"120ms\"", kind:Duration, help:"Background probe timeout." },
Field { key:"ui.split_min_columns", default:"90", kind:Count(20,1000), help:"Minimum columns for sidebar split." },
Field { key:"ui.scroll_margin", default:"3", kind:Count(0,100), help:"Picker scroll margin." },
Field { key:"ui.spinner_frames", default:"6", kind:Count(1,100), help:"UI ticks per spinner frame." },
Field { key:"ui.counter_min_duration", default:"\"120ms\"", kind:Duration, help:"Minimum counter animation duration." },
Field { key:"ui.counter_max_duration", default:"\"1400ms\"", kind:Duration, help:"Maximum counter animation duration." },
];

/// When a changed value takes effect. Shown in Settings and `-config explain` so a
/// user knows whether an edit is live, applies next turn, or needs a restart.
///
/// With the unified shared configuration, *every* field is read fresh through the
/// atomic handle; nothing is a frozen startup snapshot any more. The three classes
/// below describe the consumer's read pattern, not a reload limitation:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    /// The very next read sees the new value — even mid-turn for tools that read
    /// at call time. Safe to flip at any moment.
    Immediate,
    /// An in-flight operation (one model turn, one compaction) finishes on the
    /// snapshot it started with; the new value applies from the next operation.
    OperationBoundary,
    /// Startup cleanup must not delete scratch owned by active sessions.
    RestartOnly,
}

impl Timing {
    pub fn label(self) -> &'static str {
        match self {
            Timing::Immediate => "live — applies on the next read, no restart",
            Timing::OperationBoundary => "applies from the next operation (turn/request); in-flight work finishes on its start snapshot",
            Timing::RestartOnly => "takes effect on restart",
        }
    }
}

/// The timing class of one configuration key. One function owns this mapping so the
/// answer can never drift between the UI, `-config explain`, and the docs.
pub fn timing(key: &str) -> Timing {
    if key == "storage.scratch_lifetime" {
        return Timing::RestartOnly;
    }
    // Read by the transport at every wait, so a request already waiting on a
    // silent server is held to the new limit.
    if key == "network.stream_idle_timeout" {
        return Timing::Immediate;
    }
    // Operation-boundary: read once at the start of one model operation.
    if key.starts_with("compact.")
        || key.starts_with("title.")
        || key.starts_with("classifier.")
        || key.starts_with("sanitizer.")
        || key.starts_with("generation")
        || key.starts_with("network.")
        || key.starts_with("retry.")
        || matches!(
            key,
            "terminal.command_timeout"
                | "permissions.timeout"
                | "questions.timeout"
                | "tools.max_actions_per_response"
                | "auxiliary.mandatory_reasoning_output_tokens"
        )
    {
        return Timing::OperationBoundary;
    }
    // Everything else is read at point of use and changes live.
    Timing::Immediate
}

#[derive(Debug, Clone)]
pub struct Configuration {
    values: Value,
    pub home: PathBuf,
    document_stamp: Stamp,
}
/// The field a key names, when it names one.
fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// Reads of a key the configuration does not have, or of a field as the wrong
/// type. Both are a bug in the code that asked, never in the user's file (the
/// file is validated when it is loaded), so they are loud where they can be
/// caught — a test or a debug build stops on the spot — and, in a release,
/// answered with the safest value and written down once for the session log
/// ([`take_misuse`]) rather than taking the process down: a typo in a key once
/// killed every window on a Ctrl+O.
static MISUSE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn misuse(key: &str, wrong: &str) {
    debug_assert!(false, "configuration key {key:?} {wrong}");
    let line = format!("{key} {wrong}");
    let mut held = MISUSE.lock().unwrap_or_else(|e| e.into_inner());
    if held.len() < 32 && !held.contains(&line) {
        held.push(line);
    }
}

/// What has been misread since the last call, for whoever can write a log line.
pub fn take_misuse() -> Vec<String> {
    std::mem::take(&mut *MISUSE.lock().unwrap_or_else(|e| e.into_inner()))
}

fn number_of(value: &Value, key: &str) -> u64 {
    value.as_u64().unwrap_or_else(|| {
        if (key == "questions.timeout" || key == "permissions.timeout")
            && matches!(
                value.as_str().unwrap_or("").trim().to_lowercase().as_str(),
                "unlimited" | "none" | "0" | "0s" | "0m" | "0h"
            )
        {
            return 0;
        }
        units(
            value.as_str().unwrap_or(""),
            field(key).map(|f| f.kind).unwrap_or(Count(0, 0)),
        )
        .expect("validated configuration unit")
    })
}

impl Default for Configuration {
    fn default() -> Self {
        let mut values = json!({"version":1});
        for field in FIELDS {
            let value: Value = serde_json::from_str(field.default).expect("configuration default");
            if !value.is_null() {
                set_value(&mut values, field.key, value);
            }
        }
        Self {
            values,
            home: PathBuf::new(),
            document_stamp: Vec::new(),
        }
    }
}
impl Configuration {
    pub fn load(home: &Path) -> Result<Self> {
        let stamp = document_stamp(home);
        let mut config = Self::from_values(read_document(home)?)?;
        config.home = home.to_path_buf();
        config.document_stamp = stamp;
        Ok(config)
    }
    pub(crate) fn from_values(raw: Value) -> Result<Self> {
        validate_tree(&raw, "")?;
        let mut config = Self::default();
        for field in FIELDS {
            if let Some(value) = lookup(&raw, field.key) {
                check_field(field, value)?;
                set_value(&mut config.values, field.key, value.clone());
            }
        }
        if let Some(p) = raw.get("providers") {
            config.values["providers"] = p.clone();
        }
        if let Some(s) = raw.get(super::services::SECTION) {
            config.values[super::services::SECTION] = s.clone();
        }
        for (a, b) in [
            ("retry.initial_delay", "retry.max_delay"),
            ("ui.counter_min_duration", "ui.counter_max_duration"),
        ] {
            if config.number(a) > config.number(b) {
                return Err(Error::invalid(format!("{a} cannot exceed {b}")));
            }
        }
        if !["violet", "cyan", "amber"].contains(&config.text("ui.theme")) {
            return Err(Error::invalid("ui.theme must be violet, cyan or amber"));
        }
        if !["auto", "none", "ansi16", "ansi256", "truecolor"].contains(&config.text("ui.color")) {
            return Err(Error::invalid(
                "ui.color must be auto, none, ansi16, ansi256 or truecolor",
            ));
        }
        let shell = config.text("terminal.shell");
        if shell != crate::tools::terminal::AUTO_SHELL && !Path::new(shell).is_absolute() {
            return Err(Error::invalid(
                "terminal.shell must be auto or an absolute path",
            ));
        }
        // Checked, and kept as the owner wrote it. It used to be replaced by
        // the checked settings written back out, which leave out every switch
        // and limit (`enabled`, `timeout_seconds`, the byte budgets…): each of
        // them read as its default, so `enabled = false` turned nothing off.
        crate::capabilities::config::Settings::from_config(&config)?;
        Ok(config)
    }
    pub fn get(&self, key: &str) -> &Value {
        lookup(&self.values, key).unwrap_or(&Value::Null)
    }
    pub fn number(&self, key: &str) -> u64 {
        let value = self.get(key);
        if value.is_null() {
            // Every field has a value in force (the defaults are laid under the
            // file), so a null here is a key that is not a setting at all: the
            // caller's mistake, never a value to guess.
            return match field(key)
                .filter(|f| matches!(f.kind, Count(..) | Size | Duration | Money))
            {
                Some(f) => {
                    serde_json::from_str::<Value>(f.default).map_or(0, |d| number_of(&d, key))
                }
                None => {
                    misuse(key, "is not a numeric configuration key");
                    0
                }
            };
        }
        number_of(value, key)
    }
    pub fn flag(&self, key: &str) -> bool {
        if let Some(value) = self.get(key).as_bool() {
            return value;
        }
        match field(key) {
            Some(f) if matches!(f.kind, Bool) => f.default.parse::<bool>().unwrap_or(false),
            Some(_) => {
                misuse(key, "is not a boolean configuration field");
                false
            }
            None => {
                misuse(key, "is not a configuration key");
                false
            }
        }
    }
    pub fn text(&self, key: &str) -> &str {
        if let Some(value) = self.get(key).as_str() {
            return value;
        }
        match field(key) {
            Some(f) if matches!(f.kind, Text) => f.default.trim_matches('"'),
            Some(_) => {
                misuse(key, "is not a text configuration field");
                ""
            }
            None => {
                misuse(key, "is not a configuration key");
                ""
            }
        }
    }
    /// The service connection a `Connection` field names, or `None` for its
    /// default.
    pub fn connection(&self, key: &str) -> Option<&str> {
        match field(key) {
            Some(f) if matches!(f.kind, Connection) => self.get(key).as_str(),
            Some(_) => {
                misuse(key, "is not a connection configuration field");
                None
            }
            None => {
                misuse(key, "is not a configuration key");
                None
            }
        }
    }
    pub fn duration(&self, key: &str) -> std::time::Duration {
        std::time::Duration::from_millis(self.number(key))
    }
    pub fn values(&self) -> &Value {
        &self.values
    }
    pub fn parse_value(key: &str, text: &str) -> Result<Value> {
        if key.starts_with("capabilities.") {
            let value: Value = toml_edit::de::from_str(&format!("value = {text}"))
                .map_err(|_| Error::invalid("Invalid TOML capability value"))?;
            return Ok(value["value"].clone());
        }
        if let Some(knob) = key.strip_prefix("generation.") {
            let value: Value = toml_edit::de::from_str(&format!("value = {text}"))
                .map_err(|_| Error::invalid("Invalid generation setting syntax or format"))?;
            super::ModelSettings::default().apply_values(
                &json!({knob:value["value"]}),
                None,
                "",
            )?;
            return Ok(value["value"].clone());
        }
        let field = FIELDS
            .iter()
            .find(|f| f.key == key)
            .ok_or_else(|| Error::invalid(format!("unknown configuration key {key}")))?;
        let value = match field.kind {
            Text => Value::String(text.trim_matches('"').into()),
            Connection => {
                let trimmed = text.trim().trim_matches('"');
                if trimmed.is_empty() || trimmed == "null" || trimmed == "default" {
                    Value::Null
                } else {
                    Value::String(trimmed.into())
                }
            }
            Duration => {
                let trimmed = text.trim().trim_matches('"');
                if trimmed == "null" || trimmed.is_empty() {
                    Value::Null
                } else if matches!(key, "questions.timeout" | "permissions.timeout")
                    && matches!(
                        trimmed.to_lowercase().as_str(),
                        "unlimited" | "none" | "0" | "0s" | "0m" | "0h"
                    )
                {
                    Value::String("unlimited".into())
                } else if let Some(ms) = parse_duration_sum(trimmed) {
                    if ms % 1000 == 0 {
                        Value::String(format!("{}s", ms / 1000))
                    } else {
                        Value::String(format!("{ms}ms"))
                    }
                } else {
                    Value::String(trimmed.into())
                }
            }
            Size => {
                let trimmed = text.trim().trim_matches('"');
                if trimmed == "null" || trimmed.is_empty() {
                    Value::Null
                } else if let Some(bytes) = parse_size_sum(trimmed) {
                    if bytes % (1024 * 1024) == 0 && bytes > 0 {
                        Value::String(format!("{}MiB", bytes / (1024 * 1024)))
                    } else if bytes % 1024 == 0 && bytes > 0 {
                        Value::String(format!("{}KiB", bytes / 1024))
                    } else {
                        Value::String(format!("{bytes}B"))
                    }
                } else {
                    Value::String(trimmed.into())
                }
            }
            ToolNames => {
                let trimmed = text.trim();
                if trimmed.is_empty() || trimmed == "[]" {
                    Value::Array(Vec::new())
                } else if trimmed.starts_with('[') {
                    let document: Value = toml_edit::de::from_str(&format!("value = {trimmed}"))
                        .map_err(|_| Error::invalid("Invalid array format for tool names"))?;
                    document["value"].clone()
                } else if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                    return Err(Error::invalid(
                        "Tool names must be an array of registered names",
                    ));
                } else {
                    let names: Vec<Value> = trimmed
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| Value::String(s.to_string()))
                        .collect();
                    Value::Array(names)
                }
            }
            Model => {
                let trimmed = text.trim();
                if trimmed.is_empty()
                    || trimmed == "null"
                    || trimmed == "inherit"
                    || trimmed == "default"
                    || trimmed == "none"
                {
                    Value::Null
                } else if trimmed.starts_with('{') {
                    let document: Value = toml_edit::de::from_str(&format!("value = {trimmed}"))
                        .map_err(|_| Error::invalid("Invalid model configuration object"))?;
                    document["value"].clone()
                } else if let Some((p, m)) =
                    trimmed.split_once(':').or_else(|| trimmed.split_once('/'))
                {
                    if p.trim().is_empty() || m.trim().is_empty() {
                        return Err(Error::invalid(format!(
                            "Model must specify both provider and model ID (got '{trimmed}')"
                        )));
                    }
                    json!({
                        "provider": p.trim(),
                        "id": m.trim(),
                    })
                } else {
                    return Err(Error::invalid(format!(
                        "Model must be in '{{provider}}:{{modelID}}' or '{{provider}}/{{modelID}}' format (got '{trimmed}')"
                    )));
                }
            }
            _ => {
                let trimmed = text.trim().trim_matches('"');
                if trimmed == "null"
                    || trimmed == "auto"
                    || trimmed == "inherit"
                    || trimmed.is_empty()
                {
                    Value::Null
                } else {
                    let document: Value = toml_edit::de::from_str(&format!("value = {text}"))
                        .map_err(|_| Error::invalid("Invalid setting syntax or value format"))?;
                    document["value"].clone()
                }
            }
        };
        check_field(field, &value)?;
        Ok(value)
    }
    pub fn redacted(&self) -> Value {
        let mut value = self.values.clone();
        if let Some(list) = value
            .pointer_mut("/providers/list")
            .and_then(Value::as_array_mut)
        {
            for provider in list {
                if let Some(headers) = provider.get_mut("headers").and_then(Value::as_object_mut) {
                    for v in headers.values_mut() {
                        *v = json!("[redacted]");
                    }
                }
                if let Some(url) = provider.get_mut("base_url") {
                    if let Some(text) = url.as_str() {
                        *url = json!(text.split('?').next().unwrap_or("[redacted]"));
                    }
                }
            }
        }
        if let Some(list) = value
            .pointer_mut("/services/list")
            .and_then(Value::as_array_mut)
        {
            for connection in list {
                for key in ["base_url", "fetch_url"] {
                    if let Some(url) = connection.get_mut(key) {
                        if let Some(text) = url.as_str() {
                            *url = json!(text.split('?').next().unwrap_or("[redacted]"));
                        }
                    }
                }
            }
        }
        if let Some(cap) = value.get_mut("capabilities") {
            let mut servers = Vec::new();
            if let Some(map) = cap.get("servers").and_then(Value::as_object) {
                servers.extend(map.keys().map(|k| vec!["servers".to_string(), k.clone()]));
            }
            if let Some(projects) = cap.get("projects").and_then(Value::as_object) {
                for (key, project) in projects {
                    if let Some(map) = project.get("servers").and_then(Value::as_object) {
                        servers.extend(map.keys().map(|k| {
                            vec!["projects".into(), key.clone(), "servers".into(), k.clone()]
                        }));
                    }
                }
            }
            for path in servers {
                let mut server = &mut *cap;
                for part in path {
                    server = &mut server[part];
                }
                if let Some(env) = server.get_mut("environment").and_then(Value::as_object_mut) {
                    for value in env.values_mut() {
                        *value = json!("[redacted]");
                    }
                }
            }
        }
        value
    }
    pub fn display_toml(&self) -> Result<String> {
        let mut document = toml_edit::ser::to_document(&self.redacted())
            .map_err(|e| Error::invalid(e.to_string()))?;
        layout(&mut document)?;
        Ok(document.to_string())
    }
    pub fn reference() -> String {
        Self::default()
            .display_toml()
            .expect("default configuration renders")
    }
    pub fn explain(&self, key: &str) -> Result<String> {
        if let Some(knob) = key
            .strip_prefix("generation.")
            .and_then(|key| super::KNOBS.iter().find(|k| k.key == key))
        {
            return Ok(format!("{key} = {}\n{}\nApplies as a default to new sessions; existing session choices remain intact.",self.get(key),knob.help));
        }
        let field = FIELDS
            .iter()
            .find(|f| f.key == key)
            .ok_or_else(|| Error::invalid(format!("unknown configuration key {key}")))?;
        Ok(format!(
            "{key} = {}\n{}\n{}",
            self.get(key),
            field.help,
            if self.get(key).is_null() {
                format!(
                    "Inherited when this role is active. Timing: {}.",
                    timing(key).label()
                )
            } else {
                format!("Effective value. Timing: {}.", timing(key).label())
            }
        ))
    }
    pub fn edit(
        home: &Path,
        key: Option<&str>,
        value: Option<Value>,
        preview: bool,
        expected_provider: Option<&Value>,
    ) -> Result<String> {
        let expected_key =
            if key.is_some_and(|k| k == "capabilities" || k.starts_with("capabilities.")) {
                "capabilities"
            } else if key == Some(super::services::SECTION) {
                super::services::SECTION
            } else {
                "providers"
            };
        let expected: Vec<(&str, &Value)> = expected_provider
            .into_iter()
            .map(|v| (expected_key, v))
            .collect();
        let changes: Vec<(&str, Option<Value>)> =
            key.into_iter().map(|k| (k, value.clone())).collect();
        Self::edit_many(home, &changes, preview, &expected)
    }

    /// One validated document replacement for choices that must move together.
    /// Expected values reject a concurrent edit before either choice is written.
    pub(crate) fn edit_many(
        home: &Path,
        changes: &[(&str, Option<Value>)],
        preview: bool,
        expected: &[(&str, &Value)],
    ) -> Result<String> {
        // Preview is strictly read-only, including no lock file or directory creation.
        let lock = if preview {
            None
        } else {
            let file =
                crate::lock::exclusive(&home.join("Configuration.lock"), crate::lock::Wait::Yes)?;
            Some(file)
        };
        let path = home.join(FILE);
        let original = match std::fs::read_to_string(&path) {
            Ok(v) => Some(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let raw = read_document(home)?;
        for (key, expected) in expected {
            let current = if *key == "capabilities" {
                Self::from_values(raw.clone())?.get(key).clone()
            } else {
                key.split('.').fold(&raw, |v, part| &v[part]).clone()
            };
            if &current != *expected {
                return Err(Error::invalid(
                    "configuration section changed externally; reload Rune before saving",
                ));
            }
        }
        Self::from_values(raw.clone())?;
        let mut document = match &original {
            Some(text) => text
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| Error::invalid(e.to_string()))?,
            None => toml_edit::ser::to_document(&raw).map_err(|e| Error::invalid(e.to_string()))?,
        };
        for (key, value) in changes {
            let key = *key;
            if key != "providers"
                && key != super::services::SECTION
                && !key.starts_with("capabilities.")
                && !key
                    .strip_prefix("generation.")
                    .is_some_and(|k| super::KNOBS.iter().any(|knob| knob.key == k))
                && !FIELDS.iter().any(|f| f.key == key)
            {
                return Err(Error::invalid(format!("unknown configuration key {key}")));
            }
            let replacement = value.clone().unwrap_or(Value::Null);
            expand_tables(document.as_item_mut(), "");
            document_value(&mut document, key, replacement)?;
            // The subject of this edit survives its own layout: pruning runs
            // after the write, and a key on the internal-invariant list —
            // `classifier.max_user_messages` and friends — used to be written
            // here and erased by the same call.
            layout_keeping(&mut document, Some(key))?;
        }
        if changes.is_empty() {
            layout_keeping(&mut document, None)?;
        }

        let candidate = document.to_string();
        let parsed: Value =
            toml_edit::de::from_str(&candidate).map_err(|e| Error::invalid(e.to_string()))?;
        Self::from_values(parsed)?;
        if !preview {
            if std::fs::read_to_string(&path).ok() != original {
                return Err(Error::invalid(
                    "configuration changed externally; retry the edit",
                ));
            }
            super::secrets::atomic_write(&path, candidate.as_bytes())?;
        }
        drop(lock);
        Ok(candidate)
    }
}

/// A single, hot-reloadable `Configuration` shared by the whole engine.
///
/// This is the one source of truth. The engine, every session, every tool, the
/// provider transport and the retry policy all hold a clone of this handle, so a
/// write through `SettingsSet` (or `-config set`) becomes visible to every consumer
/// on its next read — no restart for any field that is not genuinely restart-only.
///
/// Reads are lock-free (`arc_swap::ArcSwap::load` returns an `Arc`, never blocks,
/// never deadlocks). Writes are a single atomic swap: readers already in flight keep
/// the old value until they drop it, which is exactly the operation-boundary
/// semantics an in-flight model request needs.
///
/// Every process follows the file by itself: `rune.toml` is shared by every
/// window, host and command of the home, and a value one of them saved held in
/// another only after something there happened to reload — a draft window's
/// Settings never did, nor a host with no window (owner, 2026-10-14). So a read
/// compares the documents' stamp with disk at most every [`FOLLOW_EVERY`]
/// (`stat`, three files) and swaps in what changed; a document that does not
/// load is kept out, and looked at again only once it changes — one that
/// could not be read just now is read again at the next look. What must happen when
/// the configuration changed (providers re-read, capabilities refreshed, a
/// theme repainted) is keyed to [`Shared::generation`], never to which reader
/// noticed first.
#[derive(Clone)]
pub struct Shared(std::sync::Arc<Live>);

struct Live {
    current: arc_swap::ArcSwap<Configuration>,
    /// Bumped by every swap.
    generation: std::sync::atomic::AtomicU64,
    /// When disk was last compared, in milliseconds of [`since_start`].
    checked: std::sync::atomic::AtomicU64,
    /// The stamp of documents that failed to load.
    refused: std::sync::Mutex<Option<Stamp>>,
}

type Stamp = Vec<Option<(std::time::SystemTime, u64, crate::platform::FileIdentity)>>;

/// How stale a read may be after another process saved the configuration.
pub const FOLLOW_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

fn since_start() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("current", &self.0.current.load())
            .field("generation", &self.generation())
            .finish()
    }
}

impl Shared {
    /// Wrap an already-loaded configuration in a shareable handle.
    pub fn new(config: Configuration) -> Shared {
        use std::sync::atomic::AtomicU64;
        Shared(std::sync::Arc::new(Live {
            current: arc_swap::ArcSwap::from_pointee(config),
            generation: AtomicU64::new(0),
            checked: AtomicU64::new(since_start()),
            refused: std::sync::Mutex::new(None),
        }))
    }

    /// Load `rune.toml` from `home` into a fresh shared handle.
    pub fn load(home: &Path) -> Result<Shared> {
        Ok(Shared::new(Configuration::load(home)?))
    }

    /// Lock-free snapshot of the current configuration.
    pub fn snapshot(&self) -> std::sync::Arc<Configuration> {
        self.follow();
        self.0.current.load_full()
    }

    /// Atomically replace the configuration. In-flight readers keep their snapshot.
    pub fn store(&self, config: Configuration) {
        self.0.current.store(std::sync::Arc::new(config));
        self.0
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Which configuration this is: a number every swap raises. A consumer that
    /// acts on a change remembers the last one it acted on.
    pub fn generation(&self) -> u64 {
        self.follow();
        self.0.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Re-read `rune.toml` from disk and swap it in atomically. One reload path.
    pub fn reload(&self) -> Result<()> {
        let home = self.0.current.load_full().home.clone();
        if home.as_os_str().is_empty() {
            return Ok(());
        }
        self.store(Configuration::load(&home)?);
        Ok(())
    }

    /// Re-read the configuration from disk if any document it is built from was
    /// modified since loaded (see [`document_stamp`]). A document that does
    /// not load is this call's error; one that could not be read just now —
    /// another program holding it a moment on Windows (an antivirus, an
    /// indexer, a replace in progress) — keeps what is loaded, and the next
    /// call reads it again: the stamp still differs.
    pub fn reload_if_changed(&self) -> Result<bool> {
        let current = self.0.current.load_full();
        if current.home.as_os_str().is_empty() {
            return Ok(false);
        }
        self.0
            .checked
            .store(since_start(), std::sync::atomic::Ordering::Release);
        if document_stamp(&current.home) == current.document_stamp {
            return Ok(false);
        }
        match Configuration::load(&current.home) {
            Ok(config) => {
                self.store(config);
                Ok(true)
            }
            Err(Error::Io(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Take in what another process saved, at most once per [`FOLLOW_EVERY`]
    /// and by one reader at a time; every other read goes straight on.
    fn follow(&self) {
        use std::sync::atomic::Ordering;
        let now = since_start();
        let last = self.0.checked.load(Ordering::Acquire);
        if now.saturating_sub(last) < FOLLOW_EVERY.as_millis() as u64
            || self
                .0
                .checked
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let current = self.0.current.load_full();
        if current.home.as_os_str().is_empty() {
            return;
        }
        let stamp = document_stamp(&current.home);
        if stamp == current.document_stamp {
            return;
        }
        let mut refused = self.0.refused.lock().unwrap_or_else(|e| e.into_inner());
        if refused.as_ref() == Some(&stamp) {
            return;
        }
        match Configuration::load(&current.home) {
            Ok(config) => {
                *refused = None;
                self.store(config);
            }
            // Not read just now (see `reload_if_changed`): tried again at the
            // next look, never kept out as if it did not load — a window
            // would follow the file no more until it changed again.
            Err(Error::Io(_)) => {}
            Err(_) => *refused = Some(stamp),
        }
    }

    // Pass-through readers. Signatures mirror `Configuration` so the ~50 existing
    // call sites keep working unchanged; only the field type changed, not the calls.
    // `text`/`get` return owned values because a borrow cannot outlive a swap.
    pub fn number(&self, key: &str) -> u64 {
        self.snapshot().number(key)
    }
    pub fn flag(&self, key: &str) -> bool {
        self.snapshot().flag(key)
    }
    pub fn text(&self, key: &str) -> String {
        self.snapshot().text(key).to_string()
    }
    pub fn duration(&self, key: &str) -> std::time::Duration {
        self.snapshot().duration(key)
    }
    pub fn connection(&self, key: &str) -> Option<String> {
        self.snapshot().connection(key).map(String::from)
    }
    pub fn get(&self, key: &str) -> Value {
        self.snapshot().get(key).clone()
    }
    pub fn values(&self) -> Value {
        self.snapshot().values().clone()
    }
    pub fn redacted(&self) -> Value {
        self.snapshot().redacted()
    }
    pub fn home(&self) -> PathBuf {
        self.snapshot().home.clone()
    }
}

impl Default for Shared {
    fn default() -> Shared {
        Shared::new(Configuration::default())
    }
}

impl From<Configuration> for Shared {
    fn from(config: Configuration) -> Shared {
        Shared::new(config)
    }
}

/// One document update path, shared by normal edits and complete configuration layout.
fn document_value(
    document: &mut toml_edit::DocumentMut,
    key: &str,
    replacement: Value,
) -> Result<()> {
    let mut item = document.as_item_mut();
    let parts: Vec<_> = key.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        if item.get(part).is_none() {
            item[part] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        item = &mut item[part];
    }
    let last = parts[parts.len() - 1];
    if replacement.is_null() {
        if let Some(table) = item.as_table_like_mut() {
            table.remove(last);
        }
    } else {
        let mut encoded = toml_edit::ser::to_document(&json!({"value":replacement}))
            .map_err(|e| Error::invalid(e.to_string()))?;
        let mut next = encoded.remove("value").expect("serialized value");
        if let (Some(old), Some(new)) = (
            item.get(last).and_then(|i| i.as_value()),
            next.as_value_mut(),
        ) {
            *new.decor_mut() = old.decor().clone();
        }
        item[last] = next;
    }
    Ok(())
}

fn expand_tables(item: &mut toml_edit::Item, path: &str) {
    // A provider/model pair is deliberately a compact, readable single value.
    if FIELDS
        .iter()
        .any(|f| f.key == path && matches!(f.kind, Model))
    {
        return;
    }
    if path.ends_with(".models") {
        if let Some(array) = item.as_array_mut() {
            if !array.is_empty() {
                array.set_trailing_comma(true);
                array.set_trailing("\n");
                for value in array.iter_mut() {
                    value.decor_mut().set_prefix("\n    ");
                    value.decor_mut().set_suffix("");
                }
            }
        }
        return;
    }
    if let Some(table) = item.as_inline_table() {
        *item = toml_edit::Item::Table(table.clone().into_table());
    } else if let Some(array) = item.as_array() {
        if !array.is_empty() && array.iter().all(toml_edit::Value::is_inline_table) {
            let mut tables = toml_edit::ArrayOfTables::new();
            for value in array.iter() {
                tables.push(value.as_inline_table().unwrap().clone().into_table());
            }
            *item = toml_edit::Item::ArrayOfTables(tables);
        }
    }
    if let Some(table) = item.as_table_mut() {
        table.set_position(None);
        table.set_dotted(false);
        for (key, child) in table.iter_mut() {
            let next = if path.is_empty() {
                key.to_string()
            } else {
                format!("{path}.{key}")
            };
            expand_tables(child, &next);
        }
    } else if let Some(tables) = item.as_array_of_tables_mut() {
        for table in tables.iter_mut() {
            table.set_position(None);
            for (key, child) in table.iter_mut() {
                expand_tables(child, &format!("{path}.{key}"));
            }
        }
    }
}

pub fn is_internal_invariant(key: &str) -> bool {
    matches!(
        key,
        "terminal.rows"
            | "terminal.columns"
            | "terminal.shell"
            | "terminal.term"
            | "terminal.inherit_environment"
            | "terminal.flush_interval"
            | "terminal.drain_after_exit"
            | "terminal.input_wait_delay"
            | "terminal.cancel_grace"
            | "terminal.timeout_grace"
            | "checkpoint.max_file_size"
            | "checkpoint.refresh_interval"
            | "checkpoint.diff.max_cells"
            | "checkpoint.diff.context"
            | "storage.busy_timeout"
            | "storage.scratch_lifetime"
            | "storage.session_limit"
            | "network.max_stream_size"
            | "network.connect_timeout"
            | "network.catalog_timeout"
            | "network.catalog_ttl"
            | "network.catalog_max_pages"
            | "network.probe_timeout"
            | "network.max_event_size"
            | "network.max_response_size"
            | "retry.credit_cooldown"
            | "ui.frame_interval"
            | "ui.status_lifetime"
            | "ui.quit_lifetime"
            | "ui.stop_lifetime"
            | "ui.escape_timeout"
            | "ui.color_probe_timeout"
            | "host.idle_lifetime"
            | "ui.split_min_columns"
            | "ui.scroll_margin"
            | "ui.spinner_frames"
            | "ui.counter_min_duration"
            | "ui.counter_max_duration"
            | "classifier.max_evidence_size"
            | "classifier.max_policy_size"
            | "classifier.max_output_tokens"
            | "compact.fallback_context_tokens"
            | "compact.message_chars"
            | "compact.tool_chars"
            | "compact.max_output_tokens"
            | "title.max_output_tokens"
            | "attachments.max_history_files"
            | "auxiliary.mandatory_reasoning_output_tokens"
            | "terminal.max_output_size"
            | "search_text.max_file_size"
            | "discover_capabilities.page_size"
    )
}

fn layout(document: &mut toml_edit::DocumentMut) -> Result<()> {
    layout_keeping(document, None)
}

/// `rune.toml` text laid out exactly as a save through [`Configuration::edit`]
/// would write it — every value and comment the person wrote kept, in the
/// usual order, and no default written for them. For an upgrade step that
/// rewrites a file nobody is editing.
pub(crate) fn laid_out(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    layout(&mut document)?;
    Ok(document.to_string())
}

/// `keep` names one field that must survive pruning: the subject of the
/// `edit` call that is about to write it. Without the exemption the same call
/// that set `classifier.max_user_messages` erased it again before the file
/// hit the disk — the UI saved, the value silently reverted, and no error
/// said so.
///
/// The pruning rule for the invariant list is value-aware: a key whose value
/// equals the field default is clutter and is removed, but a key whose value
/// *differs* is a choice somebody made — through this very module's edit path
/// on a previous save — and pruning it here would make every later edit,
/// however unrelated, undo it. A save must never be a revert.
fn layout_keeping(document: &mut toml_edit::DocumentMut, keep: Option<&str>) -> Result<()> {
    expand_tables(document.as_item_mut(), "");
    let raw: Value = toml_edit::de::from_str(&document.to_string())
        .map_err(|e| Error::invalid(e.to_string()))?;

    for field in FIELDS {
        if !is_internal_invariant(field.key) || Some(field.key) == keep {
            continue;
        }
        let Some(present) = lookup(&raw, field.key) else {
            continue;
        };
        let Ok(default) = serde_json::from_str::<Value>(field.default) else {
            continue;
        };
        if *present == default {
            if let Some((tbl, sub)) = field.key.split_once('.') {
                if let Some(table) = document.get_mut(tbl).and_then(|i| i.as_table_like_mut()) {
                    table.remove(sub);
                }
            } else {
                document.as_table_mut().remove(field.key);
            }
        }
    }

    // The file holds the person's choices and nothing else (owner,
    // 2026-10-14): a default written into it would read as a choice, keep the
    // value it had when it was written, and hide every later default from the
    // person who never chose. Unwritten, a key is its default in this build —
    // and "Use default" (a key removed) means the newest one.
    expand_tables(document.as_item_mut(), "");
    drop_empty_tables(document);
    if document
        .get("usage")
        .and_then(|u| u.get("warn_spend_usd"))
        .and_then(|v| v.as_integer())
        .is_some_and(|v| v == 0)
    {
        document.as_table_mut().remove("usage");
    }
    let order = [
        "version",
        "classifier",
        "compact",
        "title",
        "tools",
        "terminal",
        "inspect_file",
        "find_files",
        "search_text",
        "manage_process",
        "web_search",
        "web_fetch",
        "discover_capabilities",
        "read_mcp_resource",
        "tasks",
        "questions",
        "permissions",
        "attachments",
        "instructions",
        "memory",
        "retry",
        "ui",
        "providers",
        "services",
    ];
    document.as_table_mut().sort_values_by(|a, _, b, _| {
        order
            .iter()
            .position(|k| *k == a.get())
            .unwrap_or(order.len())
            .cmp(
                &order
                    .iter()
                    .position(|k| *k == b.get())
                    .unwrap_or(order.len()),
            )
    });
    Ok(())
}

/// A section whose last key went (a choice reset to its default) leaves no
/// empty `[header]` behind — unless the person wrote a comment above it, which
/// is theirs and stays, header and all.
fn drop_empty_tables(document: &mut toml_edit::DocumentMut) {
    fn commented(item: &toml_edit::Item) -> bool {
        item.as_table().is_some_and(|table| {
            table
                .decor()
                .prefix()
                .and_then(|prefix| prefix.as_str())
                .is_some_and(|text| text.contains('#'))
        })
    }
    fn prune(table: &mut dyn toml_edit::TableLike) {
        let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            let empty = match table.get_mut(&key) {
                Some(item) => {
                    let keep = commented(item);
                    match item.as_table_like_mut() {
                        Some(inner) => {
                            prune(inner);
                            inner.is_empty() && !keep
                        }
                        None => false,
                    }
                }
                None => false,
            };
            if empty {
                table.remove(&key);
            }
        }
    }
    prune(document.as_table_mut());
}

pub fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = crate::platform::open_read_nonblocking(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::invalid("expected a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(Error::invalid(format!("file exceeds {max} bytes")));
    }
    Ok(bytes)
}
/// The identity of every file the loaded configuration depends on: `rune.toml`
/// itself, the vault its providers' keys come from, and the classifier policy.
///
/// `None` for a file that does not exist — an install never configured. One
/// stamp for all of them, because a caller asking "has anything changed?" is
/// asking about the resulting configuration, not about one filename.
fn document_stamp(home: &Path) -> Stamp {
    [FILE, "secrets.vault", "CLASSIFIER.md"]
        .into_iter()
        .map(|name| {
            let path = home.join(name);
            let m = std::fs::metadata(&path).ok()?;
            Some((
                m.modified().ok()?,
                m.len(),
                crate::platform::FileIdentity::of_path(&path).ok()?,
            ))
        })
        .collect()
}

fn read_document(home: &Path) -> Result<Value> {
    match read_bounded(&home.join(FILE), MAX_CONFIG_BYTES) {
        Ok(bytes) => toml_edit::de::from_str(
            std::str::from_utf8(&bytes).map_err(|_| Error::invalid("rune.toml must be UTF-8"))?,
        )
        .map_err(|e| Error::invalid(format!("{FILE}: {e}"))),
        // Never configured: every field at its default.
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({"version":1})),
        Err(e) => Err(e),
    }
}
fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |v, k| v.get(k))
}
fn set_value(root: &mut Value, key: &str, value: Value) {
    let mut cursor = root;
    let parts: Vec<_> = key.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        if !cursor.get(part).is_some_and(Value::is_object) {
            cursor[part] = json!({});
        }
        cursor = &mut cursor[part];
    }
    if value.is_null() {
        if let Some(map) = cursor.as_object_mut() {
            map.remove(parts[parts.len() - 1]);
        }
    } else {
        cursor[parts[parts.len() - 1]] = value;
    }
}
fn validate_tree(value: &Value, path: &str) -> Result<()> {
    if path == "providers" {
        let providers: super::Providers = serde_json::from_value(value.clone())?;
        let mut names = std::collections::HashSet::new();
        for p in &providers.list {
            p.check()?;
            if !names.insert(&p.name) {
                return Err(Error::invalid("duplicate provider name"));
            }
        }
        if providers
            .default
            .as_ref()
            .is_some_and(|n| !names.contains(n))
        {
            return Err(Error::invalid(
                "providers.default does not name a configured provider",
            ));
        }
        return Ok(());
    }
    if path == super::services::SECTION {
        return super::services::Services::validate(value);
    }
    if path == "version" {
        return if value == 1 {
            Ok(())
        } else {
            Err(Error::invalid("unsupported rune.toml version"))
        };
    }
    if let Some(field) = FIELDS.iter().find(|f| f.key == path) {
        return check_field(field, value);
    }
    let map = value
        .as_object()
        .ok_or_else(|| Error::invalid(format!("{path} must be a table")))?;
    for (key, v) in map {
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if child != "version"
            && child != "providers"
            && child != super::services::SECTION
            && !FIELDS
                .iter()
                .any(|f| f.key == child || f.key.starts_with(&format!("{child}.")))
        {
            return Err(Error::invalid(format!("unknown configuration key {child}")));
        }
        validate_tree(v, &child)?;
    }
    Ok(())
}
fn check_field(field: &Field, value: &Value) -> Result<()> {
    if value.is_null() && field.default == "null" {
        return Ok(());
    }
    if field.key == "checkpoint.refresh_interval" {
        let delay = value.as_str().and_then(|s| units(s, Duration).ok());
        if !delay.is_some_and(|n| (100..=60_000).contains(&n)) {
            return Err(Error::invalid(
                "checkpoint.refresh_interval must be 100ms..=1m",
            ));
        }
    }
    if field.key == "terminal.command_timeout"
        && value
            .as_str()
            .and_then(|s| units(s, Duration).ok())
            .is_some_and(|n| n < 1000)
    {
        return Err(Error::invalid(format!("{} must be at least 1s", field.key)));
    }

    let valid = match field.kind {
        Capabilities => {
            serde_json::from_value::<crate::capabilities::config::Settings>(value.clone())
                .is_ok_and(|settings| settings.check().is_ok())
        }
        // Exact names only: the list is matched exactly when it is applied
        // (`classifier::excluded`), so a synonym or a former name accepted here
        // would be a setting that silently does nothing.
        ToolNames => value.as_array().is_some_and(|names| {
            let registry = crate::tools::Registry::default();
            let known = registry.names();
            let mut seen = std::collections::HashSet::new();
            names.len() <= known.len()
                && names.iter().all(|name| {
                    name.as_str()
                        .is_some_and(|name| seen.insert(name) && known.contains(&name))
                })
        }),
        Generation => super::ModelSettings::default()
            .apply_values(value, None, "")
            .is_ok(),
        Count(lo, hi) => value.is_null() || value.as_u64().is_some_and(|v| (lo..=hi).contains(&v)),
        Bool => value.is_boolean(),
        Text => value.as_str().is_some_and(|s| {
            !s.trim().is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control)
        }),
        Connection => value
            .as_str()
            .is_some_and(|s| super::services::Target::parse(s).is_ok()),
        Money => value.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0),
        Duration | Size => value.as_str().is_some_and(|s| {
            if (field.key == "questions.timeout" || field.key == "permissions.timeout")
                && matches!(
                    s.trim().to_lowercase().as_str(),
                    "unlimited" | "none" | "0" | "0s" | "0m" | "0h"
                )
            {
                true
            } else {
                units(s, field.kind).is_ok()
            }
        }),
        Model => value.as_object().is_some_and(|m| {
            m.len() == 2
                && ["provider", "id"].iter().all(|k| {
                    m.get(*k).and_then(Value::as_str).is_some_and(|s| {
                        !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                    })
                })
        }),
    };
    if valid {
        Ok(())
    } else {
        let expected = match field.kind {
            Count(lo, hi) => format!("be between {lo} and {hi}"),
            Bool => "be true or false".into(),
            Duration => "be a positive duration such as 60s".into(),
            Size => "be a positive byte size such as 8MiB".into(),
            Money => "be a finite nonnegative number".into(),
            Model => "contain both provider and id".into(),
            Generation => "contain valid generation controls".into(),
            ToolNames => "be an array of unique registered tool names".into(),
            Text => "be nonempty text without control characters".into(),
            Capabilities => "be a valid capabilities object".into(),
            Connection => {
                "name a connection or connection:route (a readable route without control characters), or be null"
                    .into()
            }
        };
        Err(Error::invalid(format!(
            "{} must {expected}. {}",
            field.key, field.help
        )))
    }
}
pub fn parse_duration_sum(text: &str) -> Option<u64> {
    let trimmed = text.trim().trim_matches('"');
    if trimmed.is_empty() || trimmed == "null" || trimmed == "inherit" {
        return None;
    }

    if trimmed.contains(':') {
        let parts: Vec<&str> = trimmed.split(':').map(|s| s.trim()).collect();
        if parts.len() == 2 {
            let m: u64 = parts[0].parse().ok()?;
            let s: u64 = parts[1].parse().ok()?;
            let total = m.checked_mul(60_000)?.checked_add(s.checked_mul(1000)?)?;
            return if total <= 86_400_000 {
                Some(total)
            } else {
                None
            };
        } else if parts.len() == 3 {
            let m: u64 = parts[0].parse().ok()?;
            let s: u64 = parts[1].parse().ok()?;
            let ms: u64 = parts[2].parse().ok()?;
            let total = m
                .checked_mul(60_000)?
                .checked_add(s.checked_mul(1000)?)?
                .checked_add(ms)?;
            return if total <= 86_400_000 {
                Some(total)
            } else {
                None
            };
        }
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if !tokens.is_empty()
        && tokens
            .iter()
            .all(|tok| tok.chars().all(|c| c.is_ascii_digit()))
    {
        if tokens.len() == 1 {
            let secs: u64 = tokens[0].parse().ok()?;
            let total = secs.checked_mul(1000)?;
            return if total <= 86_400_000 {
                Some(total)
            } else {
                None
            };
        } else if tokens.len() == 2 {
            let m: u64 = tokens[0].parse().ok()?;
            let s: u64 = tokens[1].parse().ok()?;
            let total = m.checked_mul(60_000)?.checked_add(s.checked_mul(1000)?)?;
            return if total <= 86_400_000 {
                Some(total)
            } else {
                None
            };
        } else if tokens.len() == 3 {
            let m: u64 = tokens[0].parse().ok()?;
            let s: u64 = tokens[1].parse().ok()?;
            let ms: u64 = tokens[2].parse().ok()?;
            let total = m
                .checked_mul(60_000)?
                .checked_add(s.checked_mul(1000)?)?
                .checked_add(ms)?;
            return if total <= 86_400_000 {
                Some(total)
            } else {
                None
            };
        }
    }

    let mut total_ms: u64 = 0;
    let mut chars = trimmed.chars().peekable();
    let mut found_any = false;

    while let Some(&c) = chars.peek() {
        if c.is_ascii_whitespace() || c == ';' || c == '-' {
            chars.next();
            continue;
        }
        if c.is_ascii_digit() {
            let mut num_str = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num_str.push(d);
                    chars.next();
                } else if (d == '.' || d == ',') && !num_str.contains('.') {
                    num_str.push('.');
                    chars.next();
                } else {
                    break;
                }
            }
            if num_str.ends_with('.') {
                num_str.pop();
            }
            if num_str.len() > 10 {
                return None;
            }

            while let Some(&w) = chars.peek() {
                if w.is_ascii_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }

            let mut unit_str = String::new();
            while let Some(&u) = chars.peek() {
                if u.is_alphabetic() {
                    unit_str.push(u.to_ascii_lowercase());
                    chars.next();
                } else {
                    break;
                }
            }

            let factor: u64 = match unit_str.as_str() {
                "" | "s" | "sec" | "secs" | "second" | "seconds" | "detik" => 1_000,
                "m" | "min" | "mins" | "minute" | "minutes" | "menit" => 60_000,
                "h" | "hr" | "hrs" | "hour" | "hours" | "jam" => 3_600_000,
                "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" | "mili" | "milidetik" => {
                    1
                }
                _ => return None,
            };

            let ms = if num_str.contains('.') {
                let f: f64 = num_str.parse().ok()?;
                if !f.is_finite() || !(0.0..=86_400_000.0).contains(&f) {
                    return None;
                }
                (f * factor as f64).round() as u64
            } else {
                let num: u64 = num_str.parse().ok()?;
                num.checked_mul(factor)?
            };

            total_ms = total_ms.checked_add(ms)?;
            if total_ms > 86_400_000 {
                return None;
            }
            found_any = true;
        } else {
            return None;
        }
    }

    if found_any {
        Some(total_ms)
    } else {
        None
    }
}

pub fn parse_size_sum(text: &str) -> Option<u64> {
    let trimmed = text.trim().trim_matches('"');
    if trimmed.is_empty() || trimmed == "null" || trimmed == "inherit" {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        let kib = trimmed.parse::<u64>().ok()?;
        return kib.checked_mul(1024);
    }

    let mut total_bytes: u64 = 0;
    let mut chars = trimmed.chars().peekable();
    let mut found_any = false;

    while let Some(&c) = chars.peek() {
        if c.is_ascii_whitespace() || c == ',' || c == ';' {
            chars.next();
            continue;
        }
        if c.is_ascii_digit() {
            let mut num_str = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num_str.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if num_str.len() > 10 {
                return None;
            }
            let num: u64 = num_str.parse().ok()?;

            while let Some(&w) = chars.peek() {
                if w.is_ascii_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }

            let mut unit_str = String::new();
            while let Some(&u) = chars.peek() {
                if u.is_alphabetic() {
                    unit_str.push(u.to_ascii_lowercase());
                    chars.next();
                } else {
                    break;
                }
            }

            let factor: u64 = match unit_str.as_str() {
                "" | "kib" | "kb" | "k" => 1024,
                "mib" | "mb" | "m" => 1024 * 1024,
                "gib" | "gb" | "g" => 1024 * 1024 * 1024,
                "b" | "byte" | "bytes" => 1,
                _ => return None,
            };

            total_bytes = total_bytes.checked_add(num.checked_mul(factor)?)?;
            if total_bytes > 1_073_741_824 {
                return None;
            }
            found_any = true;
        } else {
            return None;
        }
    }

    if found_any {
        Some(total_bytes)
    } else {
        None
    }
}

fn units(text: &str, kind: Kind) -> Result<u64> {
    match kind {
        Duration => {
            if let Some(v) = parse_duration_sum(text) {
                if v > 0 && v <= 86400000 {
                    return Ok(v);
                }
            }
        }
        Size => {
            if let Some(v) = parse_size_sum(text) {
                if v > 0 && v <= 1073741824 {
                    return Ok(v);
                }
            }
        }
        _ => {}
    }
    Err(Error::invalid(
        "expected a positive duration (500ms, 60s, 2m) or size (8MiB) within its supported range",
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn checkpoint_refresh_interval_is_finite_bounded_and_has_one_fields_default() {
        for value in ["0s", "1ms", "unlimited", "2m"] {
            assert!(
                Configuration::from_values(json!({"checkpoint":{"refresh_interval":value}}))
                    .is_err()
            );
        }
        for value in ["100ms", "1s", "1m"] {
            assert!(
                Configuration::from_values(json!({"checkpoint":{"refresh_interval":value}}))
                    .is_ok()
            );
        }
        assert_eq!(
            Configuration::default().duration("checkpoint.refresh_interval"),
            std::time::Duration::from_secs(1)
        );
    }

    use super::*;

    /// A save another process made is followed even when the first read of
    /// it failed: on Windows a program holding the file a moment (an
    /// antivirus, an indexer, a replace in progress) fails one read, and that
    /// version was kept out as if it did not load — the window stayed on the
    /// old values until the file changed again. Made unreadable here instead,
    /// its stamp untouched; a document that does not load is still kept out.
    #[test]
    fn a_save_read_too_soon_is_still_followed() {
        let home = std::env::temp_dir().join(format!("rune-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join(FILE);
        std::fs::write(&file, "[ui]\nscroll_lines = 3\n").unwrap();
        let shared = Shared::load(&home).unwrap();
        assert_eq!(shared.number("ui.scroll_lines"), 3);
        let next_look = || std::thread::sleep(FOLLOW_EVERY + std::time::Duration::from_millis(50));

        // Another process saves; this one's first read of it fails.
        next_look();
        std::fs::write(&file, "[ui]\nscroll_lines = 5\n").unwrap();
        crate::platform::set_file_mode(&file, 0o000).unwrap();
        next_look();
        let _ = shared.number("ui.scroll_lines");
        crate::platform::set_file_mode(&file, 0o600).unwrap();
        next_look();
        assert_eq!(shared.number("ui.scroll_lines"), 5, "the save was followed");
        assert!(!shared.reload_if_changed().unwrap());

        // A document that does not load stays out until it changes.
        next_look();
        std::fs::write(&file, "[ui\nscroll_lines = 7\n").unwrap();
        next_look();
        assert_eq!(shared.number("ui.scroll_lines"), 5);
        assert!(shared.reload_if_changed().is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Owner, 2026-10-16: the shell is found on the machine unless chosen.
    #[test]
    fn the_shell_is_auto_or_an_absolute_path() {
        let with = |shell: &str| {
            Configuration::from_values(serde_json::json!({"terminal":{"shell":shell}}))
        };
        assert_eq!(with("auto").unwrap().text("terminal.shell"), "auto");
        // An absolute path on every OS (`/usr/bin/zsh` names no drive on Windows).
        let absolute = std::env::temp_dir().join("zsh");
        assert!(with(absolute.to_str().unwrap()).is_ok());
        let error = with("bash").unwrap_err().to_string();
        assert!(error.contains("auto or an absolute path"), "{error}");
        assert_eq!(
            Configuration::from_values(serde_json::json!({}))
                .unwrap()
                .text("terminal.shell"),
            "auto"
        );
    }

    #[test]
    fn related_web_choices_are_one_write_and_stale_expectations_write_nothing() {
        let home = std::env::temp_dir().join(format!(
            "rune-config-web-transaction-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let section = json!({"list":[{"name":"9router","api":"9router",
            "routes":{"search":["ag","exa"]}}]});
        Configuration::edit(&home, Some("services"), Some(section.clone()), false, None).unwrap();
        let absent = Value::Null;
        let mut chosen = section.clone();
        chosen["list"][0]["search"] = json!({"added":1,"route":"ag"});
        Configuration::edit_many(
            &home,
            &[
                ("services", Some(chosen.clone())),
                ("web_search.provider", Some(json!("9router:ag"))),
            ],
            false,
            &[("services", &section), ("web_search.provider", &absent)],
        )
        .unwrap();
        let saved = std::fs::read_to_string(home.join(FILE)).unwrap();
        let config = Configuration::load(&home).unwrap();
        assert_eq!(config.connection("web_search.provider"), Some("9router:ag"));
        assert_eq!(config.get("services")["list"][0]["search"]["route"], "ag");
        assert!(Configuration::edit_many(
            &home,
            &[
                ("services", Some(section.clone())),
                ("web_search.provider", Some(json!("9router:exa")))
            ],
            false,
            &[("services", &section), ("web_search.provider", &absent)]
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(home.join(FILE)).unwrap(), saved);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Persistence is a harness invariant, not a user preference: there is no
    /// switch for it, and the retired one is refused like any unknown key.
    #[test]
    fn persistence_is_not_a_disableable_configuration() {
        for section in ["terminal", "manage_process"] {
            let key = format!("{section}.persisted_output");
            assert!(!FIELDS.iter().any(|field| field.key == key));
            assert!(
                Configuration::from_values(json!({ section: {"persisted_output": false} }))
                    .is_err()
            );
            assert!(Configuration::parse_value(&key, "false").is_err());
        }
    }

    #[test]
    fn unlimited_is_canonical_only_for_human_wait_fields() {
        for key in ["questions.timeout", "permissions.timeout"] {
            for input in ["UNLIMITED", "unlimited", "Unlimited", "0", "0s", "none"] {
                assert_eq!(
                    Configuration::parse_value(key, input).unwrap(),
                    json!("unlimited")
                );
            }
            assert_eq!(
                Configuration::parse_value(key, "1h 30m").unwrap(),
                json!("5400s")
            );
        }
        // A machine's silence is always bounded: a server that never answers
        // must end in a failure the retry policy can act on, not in a wait only
        // a person's Stop could end.
        for key in [
            "terminal.command_timeout",
            "classifier.timeout",
            "web_search.timeout",
            "network.stream_idle_timeout",
        ] {
            assert!(Configuration::parse_value(key, "UNLIMITED").is_err());
            assert!(Configuration::parse_value(key, "0").is_err());
        }
    }

    #[test]
    fn classifier_exclusions_accept_only_unique_registered_names() {
        let config = Configuration::default();
        let expected = [
            "ask_user_questions",
            "track_tasks",
            "web_search",
            "web_fetch",
            "find_files",
            "inspect_file",
            "read_mcp_resource",
            "discover_capabilities",
            "search_text",
        ];
        assert_eq!(config.get("classifier.excluded_tools"), &json!(expected));
        for name in crate::tools::Registry::default().names() {
            assert_eq!(
                crate::perm::classifier::excluded(&config, name),
                expected.contains(&name)
            );
        }
        assert!(Configuration::parse_value("classifier.excluded_tools", "[]").is_ok());
        assert!(Configuration::parse_value(
            "classifier.excluded_tools",
            r#"["ask_user_questions", "terminal_use"]"#
        )
        .is_ok());
        for bad in [
            r#"["missing_tool"]"#,
            r#"["terminal_use","terminal_use"]"#,
            r#""terminal_use""#,
            "[42]",
        ] {
            assert!(
                Configuration::parse_value("classifier.excluded_tools", bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn configuration_preserves_comments_and_rejects_invalid_edits() {
        let home = std::env::temp_dir().join("rune-test").join(format!(
            "toml-config-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::create_dir_all(&home).unwrap();
        // Never configured: every field at its default, and a preview writes
        // nothing.
        let config = Configuration::load(&home).unwrap();
        assert_eq!(
            config.number("checkpoint.max_file_size"),
            Configuration::default().number("checkpoint.max_file_size")
        );
        Configuration::edit(&home, None, None, true, None).unwrap();
        assert!(!home.join(FILE).exists());
        assert!(!home.join("Configuration.lock").exists());
        Configuration::edit(&home, None, None, false, None).unwrap();
        assert!(home.join(FILE).exists());
        // The file holds choices only: the person's own lines, comments and all.
        let text = "[classifier]\n# Keep my explanation.\nenabled = true # Keep inline comment.\n";
        std::fs::write(home.join(FILE), text).unwrap();
        Configuration::edit(
            &home,
            Some("classifier.enabled"),
            Some(json!(false)),
            false,
            None,
        )
        .unwrap();
        let saved = std::fs::read_to_string(home.join(FILE)).unwrap();
        assert!(saved.contains("Keep my explanation"));
        assert!(saved.contains("Keep inline comment"));
        assert!(!Configuration::load(&home)
            .unwrap()
            .flag("classifier.enabled"));
        assert!(Configuration::edit(
            &home,
            Some("classifier.enabled"),
            Some(json!("false")),
            false,
            None
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(home.join(FILE)).unwrap(), saved);
        assert!(
            Configuration::parse_value("classifier.model", "{ provider = \"p\", id = \"m\" }")
                .is_ok()
        );
        assert!(Configuration::parse_value("classifier.model", "\"\"").is_err());
        std::fs::write(home.join(FILE), "[broken").unwrap();
        assert!(
            Configuration::edit(&home, Some("ui.theme"), Some(json!("cyan")), false, None).is_err()
        );
        assert_eq!(std::fs::read_to_string(home.join(FILE)).unwrap(), "[broken");
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn reference_is_valid_and_unknown_fields_are_never_silent() {
        let raw: Value = toml_edit::de::from_str(&Configuration::reference()).unwrap();
        assert!(Configuration::from_values(raw).is_ok());
        assert!(Configuration::from_values(json!({"classifier":{"enable":true}})).is_err());
        assert!(
            Configuration::from_values(json!({"terminal":{"command_timeout":"500ms"}})).is_err()
        );
        assert!(Configuration::from_values(json!({"classifier":{"timeout":"0s"}})).is_err());
        // A retired key is an unknown key: refused, never quietly renamed.
        for retired in [
            json!({"terminal":{"max_command_timeout":"5m"}}),
            json!({"keep_turns": 7}),
            json!({"tools":{"inspect":{"max_chars": 10}}}),
            json!({"inspect_files":{"max_output_chars": 10}}),
            // Retired by the owner, 2026-10-11: one limit for calls (no
            // separate parallel width), every file shown to a search, undo
            // history kept whole, attachments read from their path.
            json!({"tools":{"max_parallel": 4}}),
            json!({"scan":{"respect_ignore": true}}),
            json!({"scan":{"hidden": false}}),
            json!({"checkpoint":{"keep_turns": 50}}),
            json!({"attachments":{"snapshot_max_size": "64MiB"}}),
        ] {
            assert!(
                Configuration::from_values(retired.clone()).is_err(),
                "{retired}"
            );
        }
    }

    /// The retry curve is the person's to set (owner, 2026-10-06): any number
    /// of retries, a step and a cap, each a user-facing field. The file holds
    /// what the person chose and nothing it did not (owner, 2026-10-14), so an
    /// unchosen step or cap follows this build's default. Only what cannot be
    /// a curve is refused.
    #[test]
    fn the_retry_curve_is_user_facing_and_only_an_impossible_one_is_refused() {
        let laid = laid_out("[retry]\nmax_attempts = 5\n").unwrap();
        assert!(!laid.contains("initial_delay"), "{laid}");
        assert!(!laid.contains("max_delay"), "{laid}");
        assert!(laid.contains("max_attempts = 5"), "{laid}");
        for key in [
            "retry.max_attempts",
            "retry.initial_delay",
            "retry.max_delay",
        ] {
            assert!(!is_internal_invariant(key), "{key} is the person's to set");
        }
        assert!(
            Configuration::from_values(json!({"retry":{"max_attempts":1000}})).is_ok(),
            "any number of retries"
        );
        assert!(
            Configuration::from_values(json!({"retry":{"initial_delay":"0s"}})).is_err(),
            "a zero step would retry without a pause"
        );
        assert!(
            Configuration::from_values(json!({"retry":{"initial_delay":"2m","max_delay":"1m"}}))
                .is_err(),
            "a step longer than the cap"
        );
        let short =
            Configuration::from_values(json!({"retry":{"initial_delay":"500ms","max_delay":"2s"}}))
                .unwrap();
        assert_eq!(short.duration("retry.initial_delay").as_millis(), 500);
        assert_eq!(short.duration("retry.max_delay").as_millis(), 2000);
    }

    #[test]
    fn every_field_has_a_timing_class_and_explain_shows_it() {
        // Representative members of each class, so a future field added to the
        // wrong class trips this test.
        assert_eq!(timing("instructions.max_chars"), Timing::Immediate);
        assert_eq!(timing("find_files.matches"), Timing::Immediate);
        assert_eq!(timing("ui.theme"), Timing::Immediate);
        assert_eq!(timing("retry.max_attempts"), Timing::OperationBoundary);
        assert_eq!(
            timing("compact.keep_recent_turns"),
            Timing::OperationBoundary
        );
        assert_eq!(
            timing("tools.max_actions_per_response"),
            Timing::OperationBoundary
        );
        assert_eq!(timing("storage.busy_timeout"), Timing::Immediate);
        assert_eq!(timing("checkpoint.max_file_size"), Timing::Immediate);
        // explain() surfaces the class, not a generic sentence.
        let shown = Configuration::default()
            .explain("instructions.max_chars")
            .unwrap();
        assert!(
            shown.contains("live"),
            "an immediate field must say so: {shown}"
        );
        let shown = Configuration::default()
            .explain("storage.busy_timeout")
            .unwrap();
        assert!(
            shown.contains("live"),
            "a live SQLite setting must say so: {shown}"
        );
    }

    #[test]
    fn the_shared_handle_swaps_atomically_and_reads_stay_lock_free() {
        // The realtime contract at the handle level: a store() is visible to the
        // next read, while a snapshot taken earlier keeps its old value.
        let shared = Shared::default();
        let before = shared.snapshot();
        assert_eq!(before.number("instructions.max_chars"), 32768);

        let mut changed = (*before).clone();
        // Emulate what SettingsSet does after writing rune.toml.
        let raw = serde_json::json!({"instructions": {"max_chars": 500}});
        let mut values = changed.values().clone();
        values["instructions"] = raw["instructions"].clone();
        changed = Configuration::from_values(values).unwrap();
        shared.store(changed);

        // New reads see the new value; the old snapshot is untouched.
        assert_eq!(shared.number("instructions.max_chars"), 500);
        assert_eq!(before.number("instructions.max_chars"), 32768);

        let home = std::env::temp_dir().join(format!("rune-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join(FILE);
        std::fs::write(&path, "[instructions]\nmax_chars = 500\n").unwrap();
        let shared = Shared::load(&home).unwrap();
        assert!(!shared.reload_if_changed().unwrap());
        std::fs::remove_file(&path).unwrap();
        assert!(shared.reload_if_changed().unwrap());
        assert_eq!(shared.number("instructions.max_chars"), 32768);
        std::fs::write(home.join("secrets.vault"), b"stamp-only-fixture").unwrap();
        assert!(shared.reload_if_changed().unwrap());
        assert!(!shared.reload_if_changed().unwrap());
        std::fs::remove_dir_all(home).unwrap();
    }

    /// Every setting the code names by a literal key is a field of the type it
    /// is read as. This is the net under the crash that ended every window on a
    /// Ctrl+O: the window asked for `theme` and `sidebar`, which are not
    /// settings (`ui.theme`, `ui.sidebar` are), and nothing checked the words.
    #[test]
    fn every_literal_key_the_code_reads_is_a_field_of_the_right_kind() {
        fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        fn fits(reader: &str, kind: Kind) -> bool {
            match reader {
                "flag" => matches!(kind, Bool),
                "text" => matches!(kind, Text),
                "number" => matches!(kind, Count(..) | Size | Duration | Money),
                "duration" => matches!(kind, Duration),
                "connection" => matches!(kind, Connection),
                _ => false,
            }
        }
        let engine = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        sources(&engine.join("src"), &mut files);
        sources(&engine.parent().unwrap().join("Cli/src"), &mut files);
        assert!(
            files.len() > 40,
            "the scan found only {} files",
            files.len()
        );
        let mut wrong = Vec::new();
        let mut read = 0;
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            // A test module may name a wrong key on purpose; the code above it may not.
            let code = text.split("\n#[cfg(test)]\nmod ").next().unwrap();
            for reader in ["flag", "text", "number", "duration", "connection"] {
                let needle = format!(".{reader}(\"");
                for (at, _) in code.match_indices(&needle) {
                    let rest = &code[at + needle.len()..];
                    let Some(end) = rest.find('"') else { continue };
                    let key = &rest[..end];
                    if key.is_empty()
                        || !key.chars().all(|c| {
                            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                        })
                    {
                        continue;
                    }
                    read += 1;
                    match field(key) {
                        None => wrong.push(format!(
                            "{}: {reader}({key:?}) is not a configuration key",
                            file.display()
                        )),
                        Some(f) if !fits(reader, f.kind) => wrong.push(format!(
                            "{}: {reader}({key:?}) reads a field of another type",
                            file.display()
                        )),
                        Some(_) => {}
                    }
                }
            }
        }
        assert!(read > 100, "the scan found only {read} reads");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// Keys built at run time cannot be scanned; the families they come from are
    /// listed here, so a section that loses a field is found by a test.
    #[test]
    fn the_key_families_built_at_run_time_exist() {
        for section in [
            "terminal",
            "manage_process",
            "discover_capabilities",
            "read_mcp_resource",
        ] {
            assert!(
                field(&format!("{section}.max_output_chars")).is_some(),
                "{section}"
            );
        }
        for section in ["terminal", "manage_process"] {
            for key in ["truncate_head_bytes", "truncate_tail_bytes"] {
                assert!(
                    field(&format!("{section}.{key}")).is_some(),
                    "{section}.{key}"
                );
            }
        }
        for role in ["classifier", "compact", "title", "sanitizer"] {
            assert!(
                field(&format!("{role}.max_output_tokens")).is_some(),
                "{role}"
            );
        }
    }

    #[test]
    fn classifier_compact_sanitizer_defaults_are_one_explicit_contract() {
        let c = Configuration::default();
        for (key, value) in [
            ("compact.max_output_tokens", 32768),
            ("compact.keep_recent_turns", 5),
            ("compact.fallback_context_tokens", 200000),
            ("compact.message_chars", 32000),
            ("compact.tool_chars", 2000),
            ("sanitizer.max_output_tokens", 4096),
        ] {
            assert_eq!(c.number(key), value, "{key}");
        }
        assert!(c.flag("sanitizer.enabled"));
        assert!(c.get("sanitizer.model").is_null());
        assert_eq!(c.duration("sanitizer.timeout").as_secs(), 120);
        assert!(field("classifier.max_user_messages").is_none());
        assert!(!crate::perm::classifier::excluded(&c, "manage_process"));
    }

    /// A field always answers, even when the value is missing: with the default
    /// written in `FIELDS`, never with a guess.
    #[test]
    fn a_known_key_without_a_value_answers_with_its_field_default() {
        let mut config = Configuration::default();
        set_value(&mut config.values, "ui.sidebar", Value::Null);
        set_value(&mut config.values, "ui.theme", Value::Null);
        set_value(&mut config.values, "terminal.command_timeout", Value::Null);
        assert!(config.flag("ui.sidebar"));
        assert_eq!(config.text("ui.theme"), "amber");
        assert_eq!(config.duration("terminal.command_timeout").as_secs(), 120);
    }

    /// A key that is not a setting is the caller's bug: it stops a test or a
    /// debug build on the spot, and in a release it is answered with the safest
    /// value and written down once instead of ending the process.
    #[test]
    fn a_key_that_is_not_a_setting_is_loud_and_never_fatal_in_a_release() {
        let config = Configuration::default();
        let key = "ui.no_such_setting_for_this_test";
        let outcome = std::panic::catch_unwind(|| config.flag(key));
        if cfg!(debug_assertions) {
            assert!(
                outcome.is_err(),
                "a debug build must stop on a key that is not a setting"
            );
        } else {
            assert!(!outcome.unwrap());
            assert!(take_misuse().iter().any(|line| line.contains(key)));
        }
    }
}
