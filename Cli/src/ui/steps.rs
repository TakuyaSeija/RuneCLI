//! How one tool call is described to a person.
//!
//! # The tool's name is not the step's name
//!
//! The transcript used to print `create_file`, `inspect_file`, `terminal_use` —
//! the identifiers a model calls, shown to a human who never calls them. They
//! are the wrong words twice over: they leak an internal registry into the
//! interface, and they say what was *invoked* rather than what was *done*.
//!
//! Every call already carries a better answer. `action` is the model's own
//! one-line statement of intent, written before the payload and streamed first
//! precisely so a frontend has something true to show early. So a step reads as:
//!
//! ```text
//! Read the permission gate to see how grants are stored
//!   › Read  Engine/src/perm/mod.rs      lines 40–120
//!   › Read  Engine/src/perm/policy.rs
//! ```
//!
//! The heading is the model's sentence. The bullets are what the engine actually
//! touched, in the user's vocabulary, with the tool's name nowhere on screen.
//!
//! # Read vs Reading
//!
//! `Read` means part of a file — and then the range is worth printing, because
//! "which part" is the question a reader has. `Reading` means the whole of it,
//! and a range would be noise. The verb carries the distinction so the line does
//! not have to explain itself.

use serde_json::Value;

use super::blocks::{Tool, ToolState};

/// The readable payload under a write step. It is derived from the same args as
/// the counters, so the preview and the operation cannot describe different
/// content.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Code(String),
    Patch { before: String, after: String },
}

/// One line under a step's heading.
#[derive(Debug, Clone, PartialEq)]
pub struct Bullet {
    /// `Read`, `Write`, `Search`… Padded by the renderer, so it is stored bare.
    pub verb: &'static str,
    /// The thing acted on: a path, a glob, a query.
    pub target: String,
    /// The qualifier that earns its place — a line range, a match count, a diff.
    pub note: String,
    /// A path that can be opened, when the target is one.
    pub path: Option<String>,
    pub body: Option<Body>,
    pub ok: bool,
    pub error: Option<String>,
}

impl Bullet {
    pub fn new(verb: &'static str, target: impl Into<String>) -> Bullet {
        Bullet {
            verb,
            target: target.into(),
            note: String::new(),
            path: None,
            body: None,
            ok: true,
            error: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Bullet {
        self.note = note.into();
        self
    }

    pub fn linked(mut self) -> Bullet {
        self.path = Some(self.target.clone());
        self
    }

    pub fn failed(mut self, err: impl Into<String>) -> Bullet {
        self.ok = false;
        self.error = Some(err.into());
        self
    }
}

/// The heading of a step: the model's own sentence, or a fallback while it is
/// still being written.
///
/// Never the tool's name. A call whose `action` has not streamed yet says what
/// kind of work is starting, because "" reads as a hung program.
pub fn heading(tool: &Tool) -> String {
    let action = tool.action.trim();
    if !action.is_empty()
        && !action.eq_ignore_ascii_case("Running a command")
        && !action.eq_ignore_ascii_case("Running command")
    {
        return action.to_string();
    }
    match tool.name.as_str() {
        "inspect_file" => {
            if let Some(path) = &tool.progress.path {
                if tool.progress.range.is_some() {
                    format!("Read {path}")
                } else {
                    format!("Reading {path}")
                }
            } else {
                "Reading files".to_string()
            }
        }
        "find_files" => "Looking for files".to_string(),
        "search_text" => "Searching the code".to_string(),
        "web_search" => "Searching the web".to_string(),
        "web_fetch" => "Fetching a web page".to_string(),
        "create_file" => {
            if let Some(path) = &tool.progress.path {
                format!("Write {path}")
            } else {
                "Writing files".to_string()
            }
        }
        "patch_file" => {
            if let Some(path) = &tool.progress.path {
                format!("Edit {path}")
            } else {
                "Editing files".to_string()
            }
        }
        "terminal_use" => {
            let cmds = commands(tool);
            if let Some(cmd) = cmds.first() {
                format!("Run $ {cmd}")
            } else {
                "Running a command".to_string()
            }
        }
        "manage_process" | "manage_processes" | "monitoring_bg" => {
            let pid = tool
                .args
                .as_ref()
                .and_then(|a| a.get("process_id").or_else(|| a.get("task_id")))
                .and_then(|v| v.as_str());
            if let Some(id) = pid {
                if tool
                    .args
                    .as_ref()
                    .and_then(|a| a.get("kill"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    format!("Stop process {id}")
                } else {
                    format!("Manage process {id}")
                }
            } else {
                "Managing background process".to_string()
            }
        }
        "terminal_send_input" => match tool
            .args
            .as_ref()
            .and_then(|a| a.get("process_id"))
            .and_then(|v| v.as_str())
        {
            Some(id) => format!("Type into process {id}"),
            None => "Typing into a background process".to_string(),
        },
        "ask_user_questions" => "Asking you".to_string(),
        "track_tasks" => "Planning".to_string(),
        "discover_capabilities" => "Discovering capabilities".to_string(),
        "read_mcp_resource" => "Reading MCP resource".to_string(),
        other if other.starts_with("mcp__") || other.contains("_mcp_") => {
            "Running MCP tool".to_string()
        }
        _ => "Working".to_string(),
    }
}

/// Whether this step's body is a terminal transcript rather than a list.
///
/// `terminal_use` is the one tool whose output *is* the interesting part, so it
/// gets the command and its output verbatim instead of a bullet that summarises
/// them away.
pub fn is_terminal(tool: &Tool) -> bool {
    tool.name == "terminal_use"
}

/// Whether this step is a plan / task list update.
pub fn is_tasks(tool: &Tool) -> bool {
    tool.name == "track_tasks"
}

/// The tasks of a plan step, in order.
pub fn tasks(tool: &Tool) -> Vec<Value> {
    if let Some(tasks) = tool
        .args
        .as_ref()
        .and_then(|a| a.get("tasks"))
        .and_then(|v| v.as_array())
    {
        return tasks.clone();
    }
    if let Ok(val) = serde_json::from_str::<Value>(&tool.raw) {
        if let Some(tasks) = val.get("tasks").and_then(|v| v.as_array()) {
            return tasks.clone();
        }
    }
    Vec::new()
}

/// The commands of a terminal step, in order.
pub fn commands(tool: &Tool) -> Vec<String> {
    if let Some(cmd) = tool
        .args
        .as_ref()
        .and_then(|a| a.get("command"))
        .and_then(|v| v.as_str())
    {
        return vec![cmd.to_string()];
    }
    if let Some(cmds) = tool
        .args
        .as_ref()
        .and_then(|a| a.get("commands"))
        .and_then(|v| v.as_array())
    {
        let list: Vec<String> = cmds
            .iter()
            .filter_map(|c| {
                c.get("command")
                    .and_then(|v| v.as_str())
                    .or_else(|| c.as_str())
                    .map(String::from)
            })
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    if let Ok(val) = serde_json::from_str::<Value>(&tool.raw) {
        if let Some(cmd) = val.get("command").and_then(|v| v.as_str()) {
            return vec![cmd.to_string()];
        }
        if let Some(cmds) = val.get("commands").and_then(|v| v.as_array()) {
            let list: Vec<String> = cmds
                .iter()
                .filter_map(|c| {
                    c.get("command")
                        .and_then(|v| v.as_str())
                        .or_else(|| c.as_str())
                        .map(String::from)
                })
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
    }
    if !tool.scanner.command.is_empty() {
        return vec![tool.scanner.command.clone()];
    }
    if let Some(ask) = &tool.ask {
        if !ask.preview.is_empty() {
            return vec![ask.preview.clone()];
        }
    }
    Vec::new()
}

/// What the first line of a search's or a read's result says about its size —
/// `=== "x" — 84 match(es) in 6 file(s) ===`, `=== a.rs (lines 5-9 of 4907) ===` —
/// in the words a bullet uses. That line is not drawn in the body, so what it
/// says is shown here.
fn payload_counts(tool: &Tool) -> Option<String> {
    if !tool.state.finished() || tool.content.is_empty() {
        return None;
    }
    let body = runeengine::tools::notice::split(&tool.content).rest;
    let header = body.lines().next()?.trim();
    let inner = header.strip_prefix("=== ")?.strip_suffix(" ===")?;
    match tool.name.as_str() {
        "search_text" | "find_files" => {
            let (_, tail) = inner.rsplit_once(" — ")?;
            let tokens: Vec<&str> = tail.split_whitespace().collect();
            let found: usize = tokens.first()?.parse().ok()?;
            let mut text = counted(found, "match", "matches");
            if tokens.get(2) == Some(&"in") {
                if let Some(files) = tokens.get(3).and_then(|n| n.parse::<usize>().ok()) {
                    text.push_str(&format!(" in {}", counted(files, "file", "files")));
                }
            }
            Some(text)
        }
        "inspect_file" => {
            let open = inner.rfind(" (")?;
            let paren = inner[open + 2..].strip_suffix(')')?;
            if let Some(of) = paren.rsplit_once(" of ") {
                let total: usize = of.1.parse().ok()?;
                Some(format!("of {}", counted(total, "line", "lines")))
            } else {
                let lines: usize = paren.strip_suffix(" lines")?.parse().ok()?;
                Some(counted(lines, "line", "lines"))
            }
        }
        _ => None,
    }
}

fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A note, with what the payload's header adds after it.
fn with_counts(note: String, tool: &Tool) -> String {
    match (payload_counts(tool), note.is_empty()) {
        (Some(counts), true) => counts,
        (Some(counts), false) => format!("{note} · {counts}"),
        (None, _) => note,
    }
}

/// What a failed step says in its error box: the notice the model was sent,
/// tags and all, so the box and the model's text are the same words; the tool's
/// summary only when there is no notice.
pub fn failure_text(tool: &Tool) -> String {
    use runeengine::tools::notice::{split, CLOSE, OPEN};
    let parsed = split(&tool.content);
    if !parsed.notices.is_empty() {
        return parsed
            .notices
            .iter()
            .map(|body| format!("{OPEN}{body}{CLOSE}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if !tool.summary.is_empty() {
        tool.summary.clone()
    } else if !tool.output.is_empty() {
        tool.output.clone()
    } else {
        tool.content.clone()
    }
}

/// Why a call failed, in the words the model was told — without Rune's tags:
/// what a person reads when they open a failed write. The tool's summary when
/// there is no notice.
pub fn failure_reason(tool: &Tool) -> String {
    let parsed = runeengine::tools::notice::split(&tool.content);
    if !parsed.notices.is_empty() {
        return parsed.notices.join("\n");
    }
    tool.summary.clone()
}

/// The engine's code for how a call failed (`tool`, `invalid`, `denied`, …):
/// what the call's audit detail records, never read from its wording.
fn failure_code(tool: &Tool) -> Option<&str> {
    tool.detail.as_ref()?.get("code")?.as_str()
}

/// One bullet per item the call names.
pub fn bullets(tool: &Tool) -> Vec<Bullet> {
    let (displayed_added, displayed_removed) = tool.displayed_counts();
    let args = tool.args.as_ref();

    let str_of = |key: &str| {
        args.and_then(|a| a.get(key))
            .and_then(|v| v.as_str())
            .map(String::from)
    };

    match tool.name.as_str() {
        "inspect_file" => {
            let path = str_of("path")
                .or_else(|| tool.progress.path.clone())
                .unwrap_or_default();
            let view_range = args.and_then(range).or(tool.progress.range);
            let (verb, note) = match view_range {
                Some((from, to)) => ("Read", format!("{from} - {to}")),
                None => ("Reading", String::new()),
            };
            let mut bullet = Bullet::new(verb, path).linked();
            bullet.note = with_counts(note, tool);
            if tool.state == ToolState::Failed {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        "find_files" => {
            let pat = str_of("pattern").unwrap_or_default();
            let mut bullet = Bullet::new("Find", pat).with_note(with_counts(String::new(), tool));
            if tool.state == ToolState::Failed {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        "search_text" => {
            let what = str_of("pattern").unwrap_or_default();
            let mut note = String::new();
            if let Some(path) = str_of("path") {
                note = format!("in {path}");
            } else if let Some(glob) = str_of("include") {
                note = format!("in {glob}");
            }
            let mut bullet = Bullet::new("Search", what).with_note(with_counts(note, tool));
            if tool.state == ToolState::Failed {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        "web_search" | "web_fetch" => {
            let (verb, target) = if tool.name == "web_fetch" {
                ("Fetch", str_of("url").unwrap_or_default())
            } else {
                ("Look up", str_of("query").unwrap_or_default())
            };
            let mut bullet = Bullet::new(verb, target);
            if tool.state == ToolState::Failed {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        "create_file" => {
            let path = str_of("path")
                .or_else(|| tool.progress.path.clone())
                .unwrap_or_default();
            let added = if !tool.state.finished() {
                displayed_added
            } else if let Some(a) = str_of("text").map(|t| t.lines().count()) {
                a
            } else {
                tool.progress.added
            };
            // Write measures the body, not a diff, even on overwrite. The
            // engine's removed count belongs to the ledger, never this title.
            // A failed or stopped call cannot claim it wrote its whole body.
            let unsuccessful = matches!(tool.state, ToolState::Failed | ToolState::Stopped);
            let note = if !tool.state.finished() && added == 0 {
                String::new()
            } else {
                format!("+{}", if unsuccessful { 0 } else { added })
            };
            let mut bullet = Bullet::new("Write", path).with_note(note).linked();
            let text = str_of("text").unwrap_or_else(|| tool.progress.new_text.clone());
            if !text.is_empty() && !unsuccessful {
                bullet.body = Some(Body::Code(text));
            }
            if unsuccessful {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        "patch_file" => {
            let path = str_of("path")
                .or_else(|| tool.progress.path.clone())
                .unwrap_or_default();
            let before = str_of("old_str").unwrap_or_else(|| tool.progress.old_text.clone());
            let after = str_of("new_str").unwrap_or_else(|| tool.progress.new_text.clone());
            let (rem, add) = if !tool.state.finished() {
                (displayed_removed, displayed_added)
            } else {
                (
                    before.lines().count(),
                    if after.is_empty() {
                        0
                    } else {
                        after.lines().count()
                    },
                )
            };
            let (rem, add) = tool
                .detail
                .as_ref()
                .map(|detail| {
                    (
                        detail
                            .get("removed")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .unwrap_or(rem),
                        detail
                            .get("added")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .unwrap_or(add),
                    )
                })
                .unwrap_or((rem, add));
            let unchanged = !before.is_empty() && before == after;
            // The patch itself ran and found no single place for `old_str`
            // (the tool's own failure): its counts and its diff are what it
            // tried. Any other failure — arguments refused, the call denied, a
            // file that is not there — tried nothing: −0 +0 and no diff.
            let unsuccessful = matches!(tool.state, ToolState::Failed | ToolState::Stopped);
            let tried = !unsuccessful
                || (tool.state == ToolState::Failed && failure_code(tool) == Some("tool"));
            let note = if !tried {
                "−0 +0".to_string()
            } else if unchanged {
                "unchanged".to_string()
            } else {
                match (rem > 0, add > 0) {
                    (true, true) => format!("−{rem} +{add}"),
                    (true, false) => format!("−{rem}"),
                    (false, true) => format!("−0 +{add}"),
                    (false, false) if tool.state.finished() => "unchanged".to_string(),
                    (false, false) => String::new(),
                }
            };
            let mut bullet = Bullet::new("Edit", path).with_note(note).linked();
            if tried && (!before.is_empty() || !after.is_empty()) && !unchanged {
                bullet.body = Some(Body::Patch { before, after });
            }
            if unsuccessful {
                bullet = bullet.failed(failure_text(tool));
            }
            vec![bullet]
        }
        // `manage_process` gets no bullet row at all.
        //
        // The step's heading is the model's own sentence about what it is doing,
        // so a row saying `Manage` says it again in smaller words; and the task
        // id it would carry is already the subject of the report underneath and
        // the last word of the log path. The row was three repetitions of
        // something the next line said properly.
        "manage_process" | "manage_processes" | "monitoring_bg" => Vec::new(),
        // Its body is the one sentence of what it typed — it neither waits
        // nor reports — and the heading already names the process.
        "terminal_send_input" => Vec::new(),
        "track_tasks" | "ask_user_questions" => Vec::new(),
        "discover_capabilities" => {
            let q = str_of("query").or_else(|| str_of("id")).unwrap_or_default();
            vec![Bullet::new("Discover", q)]
        }
        "read_mcp_resource" => {
            let uri = str_of("uri").unwrap_or_default();
            vec![Bullet::new("Read", uri)]
        }
        // The proxy's own name says nothing; the server and the tool it calls
        // are what the person wants to see.
        "call_mcp_tool" => {
            let server = str_of("server_id").unwrap_or_default();
            let target = str_of("tool").unwrap_or_default();
            vec![Bullet::new("MCP", format!("{server} / {target}"))]
        }
        "terminal_use" => {
            let cmds = commands(tool);
            let target = cmds.first().cloned().unwrap_or_else(|| "command".into());
            vec![Bullet::new("Run", target)]
        }
        _ => {
            if let Some(path) = &tool.progress.path {
                vec![Bullet::new(verb_of(&tool.name, false), path.clone()).linked()]
            } else {
                vec![Bullet::new(verb_of(&tool.name, false), tool.name.clone())]
            }
        }
    }
}

fn range(item: &Value) -> Option<(i64, i64)> {
    let list = item.get("view_range")?.as_array()?;
    Some((list.first()?.as_i64()?, list.get(1)?.as_i64()?))
}

fn verb_of(tool: &str, ranged: bool) -> &'static str {
    match tool {
        "inspect_file" => {
            if ranged {
                "Read"
            } else {
                "Reading"
            }
        }
        "create_file" => "Write",
        "patch_file" => "Edit",
        "find_files" => "Find",
        "search_text" => "Search",
        "web_search" => "Look up",
        "web_fetch" => "Fetch",
        "discover_capabilities" => "Discover",
        "read_mcp_resource" => "Read",
        "manage_process" | "manage_processes" | "monitoring_bg" => "Manage",
        "terminal_send_input" => "Type",
        "terminal_use" => "Run",
        "track_tasks" => "Plan",
        "ask_user_questions" => "Ask",
        other if other.starts_with("mcp__") || other.contains("_mcp_") => "MCP",
        _ => "Do",
    }
}

/// A label for what is happening **right now**, which never goes backwards.
///
/// Distinct from [`heading`], and the difference is worth keeping: a heading
/// names a step in the transcript for as long as it is on screen, while this is
/// the single word under the shimmer that says what the agent is doing this
/// instant. Once it says "Patching", a pause to reason must not put "Thinking"
/// back — the user reads that as the work being undone — so the caller keeps the
/// last one and only this decides whether there is a stronger word.
pub fn label(tool: &str) -> &'static str {
    match tool {
        "create_file" => "Writing",
        "patch_file" => "Patching",
        "terminal_use" => "Running",
        "inspect_file" => "Reading",
        "find_files" | "search_text" => "Searching",
        "web_search" => "Looking up",
        "web_fetch" => "Fetching",
        "discover_capabilities" => "Discovering",
        "read_mcp_resource" => "Reading",
        "manage_process" | "manage_processes" | "monitoring_bg" => "Managing",
        "terminal_send_input" => "Typing",
        "ask_user_questions" => "Asking you",
        "track_tasks" => "Planning",
        other if other.starts_with("mcp__") || other.contains("_mcp_") => "Running MCP",
        _ => "Working",
    }
}

#[cfg(test)]
mod tests {
    use super::super::blocks::ToolState;
    use super::*;
    use serde_json::json;

    fn tool(name: &str, action: &str, args: Value) -> Tool {
        let mut t = Tool::new("c1".into(), name.into(), action.into());
        t.settle(&args);
        t.state = ToolState::Ok;
        t.snap_counters();
        t
    }

    #[test]
    fn the_heading_is_the_models_sentence_and_never_the_tools_name() {
        let t = tool(
            "inspect_file",
            "check how grants are stored",
            json!({ "path": "a.rs" }),
        );
        assert_eq!(heading(&t), "check how grants are stored");
        assert!(!heading(&t).contains("inspect_file"));
    }

    #[test]
    fn a_step_whose_action_has_not_arrived_still_says_something_true() {
        let t = Tool::new("c1".into(), "terminal_use".into(), String::new());
        assert_eq!(heading(&t), "Running a command");
    }

    #[test]
    fn reading_part_of_a_file_names_the_range_and_reading_all_of_it_does_not() {
        let part = tool(
            "inspect_file",
            "a",
            json!({ "path": "src/x.rs", "view_range": [40, 120] }),
        );
        let b = &bullets(&part)[0];
        assert_eq!(b.verb, "Read");
        assert_eq!(b.note, "40 - 120");

        let whole = tool("inspect_file", "a", json!({ "path": "src/x.rs" }));
        let b = &bullets(&whole)[0];
        assert_eq!(b.verb, "Reading");
        assert_eq!(b.note, "", "a range for a whole file is noise");
    }

    #[test]
    fn streaming_inspect_file_with_range_shows_read_and_range() {
        let mut t = Tool::new("c1".into(), "inspect_file".into(), "read part".into());
        t.absorb(r#"{"action": "read part", "path": "src/lib.rs", "view_range": [10, 50]}"#);
        let b = bullets(&t);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].verb, "Read");
        assert_eq!(b[0].target, "src/lib.rs");
        assert_eq!(b[0].note, "10 - 50");
    }

    #[test]
    fn fetch_has_its_own_heading_label_and_url_target() {
        let fetch = tool("web_fetch", "", json!({"url":"https://example.com/docs"}));
        assert_eq!(heading(&fetch), "Fetching a web page");
        assert_eq!(label("web_fetch"), "Fetching");
        assert_eq!(bullets(&fetch)[0].target, "https://example.com/docs");
        let search = tool(
            "web_search",
            "",
            json!({"query":"https://example.com/docs"}),
        );
        assert_eq!(heading(&search), "Searching the web");
        assert_eq!(bullets(&search)[0].verb, "Look up");
        assert_eq!(bullets(&search)[0].target, "https://example.com/docs");
    }

    #[test]
    fn every_tool_has_a_verb_a_person_would_use() {
        let cases: Vec<(&str, Value, &str)> = vec![
            ("find_files", json!({ "pattern": "**/*.rs" }), "Find"),
            ("search_text", json!({ "pattern": "TODO" }), "Search"),
            ("web_search", json!({ "query": "landlock abi" }), "Look up"),
            (
                "web_search",
                json!({ "query": "https://example.com/docs" }),
                "Look up",
            ),
            (
                "web_fetch",
                json!({ "url": "https://example.com/docs" }),
                "Fetch",
            ),
            (
                "create_file",
                json!({ "path": "a.rs", "text": "one\ntwo" }),
                "Write",
            ),
            (
                "patch_file",
                json!({ "path": "a.rs", "old_str": "a", "new_str": "b\nc" }),
                "Edit",
            ),
            ("discover_capabilities", json!({ "query": "*" }), "Discover"),
            (
                "read_mcp_resource",
                json!({ "uri": "rune-test://res" }),
                "Read",
            ),
            (
                "call_mcp_tool",
                json!({ "server_id": "everything", "tool": "add", "arguments": {} }),
                "MCP",
            ),
        ];
        for (name, args, verb) in cases {
            let b = bullets(&tool(name, "act", args));
            assert_eq!(b[0].verb, verb, "{name}");
            assert!(!b[0].verb.contains('_'), "{name} leaked an identifier");
        }
        let mcp = bullets(&tool(
            "call_mcp_tool",
            "act",
            json!({ "server_id": "everything", "tool": "add", "arguments": {} }),
        ));
        assert_eq!(
            mcp[0].target, "everything / add",
            "the server and its tool are shown, not the proxy"
        );
    }

    #[test]
    fn a_tool_produces_its_bullet_with_path_and_range() {
        let t = tool(
            "inspect_file",
            "read file",
            json!({ "path": "b.rs", "view_range": [1, 5] }),
        );
        let b = bullets(&t);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].target, "b.rs");
        assert_eq!(b[0].note, "1 - 5");
    }

    #[test]
    fn counters_on_a_write_agree_with_the_streaming_scanner() {
        let t = tool(
            "create_file",
            "write",
            json!({ "path": "a.rs", "text": "one\ntwo\nthree" }),
        );
        assert_eq!(bullets(&t)[0].note, "+3");
        assert_eq!(
            bullets(&t)[0].body,
            Some(Body::Code("one\ntwo\nthree".into()))
        );
        assert_eq!(
            t.progress.added, 3,
            "the two counts must not disagree on one call"
        );
    }

    #[test]
    fn writes_measure_only_the_body_even_when_metadata_describes_an_overwrite() {
        for text in ["", "one", "one\ntwo\n", "one\n\nlast"] {
            for removed in [0, 1919] {
                let mut t = tool("create_file", "write", json!({"path":"a.rs","text":text}));
                t.detail = Some(
                    json!({"added":text.lines().count(),"removed":removed,"overwritten":removed>0}),
                );
                let live = bullets(&t)[0].note.clone();
                t.detail = None;
                assert_eq!(live, bullets(&t)[0].note, "live/replay: {text:?}");
                assert_eq!(live, format!("+{}", text.lines().count()));
                assert!(!live.contains('−'));
            }
        }
    }

    #[test]
    fn unsuccessful_writes_never_claim_the_whole_body_was_applied() {
        for state in [ToolState::Failed, ToolState::Stopped] {
            for name in ["create_file", "patch_file"] {
                let mut t = tool(
                    name,
                    "write",
                    json!({"path":"a.rs","text":"a\nb","old_str":"a\nb","new_str":"c\nd"}),
                );
                t.state = state;
                t.detail = Some(json!({"status":"cancelled","code":"cancelled"}));
                let b = bullets(&t);
                assert_eq!(
                    b[0].note,
                    if name == "create_file" {
                        "+0"
                    } else {
                        "−0 +0"
                    }
                );
                assert!(!b[0].ok);
                assert!(b[0].body.is_none());
            }
        }
        let mut patch = tool(
            "patch_file",
            "patch",
            json!({"path":"a.rs","old_str":"a\nb","new_str":"c"}),
        );
        patch.state = ToolState::Failed;
        patch.detail = Some(json!({"status":"failed","code":"tool"}));
        assert_eq!(bullets(&patch)[0].note, "−2 +1");
        assert!(matches!(bullets(&patch)[0].body, Some(Body::Patch { .. })));
        patch.detail = Some(json!({"status":"not_executed","code":"invalid"}));
        assert_eq!(bullets(&patch)[0].note, "−0 +0");
        assert!(bullets(&patch)[0].body.is_none());
    }

    #[test]
    fn an_empty_replacement_is_a_deletion_and_counts_no_added_lines() {
        let t = tool(
            "patch_file",
            "delete",
            json!({ "path": "a.rs", "old_str": "x\ny", "new_str": "" }),
        );
        assert_eq!(bullets(&t)[0].note, "−2");
        assert_eq!(
            bullets(&t)[0].body,
            Some(Body::Patch {
                before: "x\ny".into(),
                after: String::new()
            })
        );
    }

    #[test]
    fn a_path_bullet_is_openable_and_a_query_is_not() {
        let file = bullets(&tool("inspect_file", "a", json!({ "path": "a.rs" })));
        assert_eq!(file[0].path.as_deref(), Some("a.rs"));
        let query = bullets(&tool("search_text", "a", json!({ "pattern": "TODO" })));
        assert_eq!(
            query[0].path, None,
            "a search pattern is not a file to open"
        );
    }

    #[test]
    fn a_step_still_streaming_shows_the_path_the_scanner_found() {
        let mut t = Tool::new("c1".into(), "create_file".into(), "write it".into());
        t.absorb(r#"{"action": "write it", "path": "src/main.rs", "text": "one\ntwo"#);
        let b = bullets(&t);
        assert_eq!(b.len(), 1);
        assert_eq!(
            b[0].target, "src/main.rs",
            "the file must appear while it is being written"
        );
        assert_eq!(b[0].note, "", "a burst must not show +0 before lines climb");
        assert_eq!(b[0].body, Some(Body::Code("one\ntwo".into())));
        t.advance_counters(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert_eq!(bullets(&t)[0].note, "+2");
    }

    #[test]
    fn a_streaming_patch_reveals_removed_then_added_lines() {
        let mut t = Tool::new("c1".into(), "patch_file".into(), "edit it".into());
        t.absorb(
            r#"{"action":"edit it","path":"src/main.rs","old_str":"one\ntwo","new_str":"ONE\nTWO\nTHREE"#,
        );
        let initial = bullets(&t);
        assert_eq!(initial[0].note, "");
        assert_eq!(
            initial[0].body,
            Some(Body::Patch {
                before: "one\ntwo".into(),
                after: "ONE\nTWO\nTHREE".into(),
            })
        );
        let start = t.counter.started;
        t.advance_counters(start + std::time::Duration::from_millis(200));
        let (added, removed) = t.displayed_counts();
        assert_eq!(
            added, 0,
            "added counter must not start before removed climbs"
        );
        assert!(removed > 0, "removed counter must climb first");
        t.advance_counters(start + std::time::Duration::from_millis(350));
        let (added, removed) = t.displayed_counts();
        assert!(
            added > 0 && added < 3,
            "added counter did not pass through a visible midpoint"
        );
        assert_eq!(removed, 2, "removed counter must be finished");
        t.advance_counters(start + std::time::Duration::from_secs(2));
        assert_eq!(bullets(&t)[0].note, "−2 +3");
    }

    #[test]
    fn patch_counters_stream_removed_first_then_added_sequentially() {
        let mut t = Tool::new("c1".into(), "patch_file".into(), "edit it".into());
        t.absorb(r#"{"action":"edit","path":"src/main.rs","old_str":"one\ntwo\nthree"#);
        t.advance_counters(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert_eq!(bullets(&t)[0].note, "−3");
        t.absorb(r#"","new_str":"1\n2\n3\n4\n5""#);
        t.advance_counters(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert_eq!(bullets(&t)[0].note, "−3 +5");
    }

    #[test]
    fn a_terminal_step_is_its_transcript_and_carries_its_commands() {
        let t = tool(
            "terminal_use",
            "build it",
            json!({ "command": "cargo build" }),
        );
        assert!(is_terminal(&t));
        assert_eq!(commands(&t), vec!["cargo build"]);
        let two = tool(
            "terminal_use",
            "two",
            json!({ "commands": [{ "command": "a" }, { "command": "b" }] }),
        );
        assert_eq!(commands(&two), vec!["a", "b"]);
        let mut streaming = Tool::new("stream".into(), "terminal_use".into(), "build".into());
        streaming.absorb(r#"{"action":"build","command":"cargo "#);
        assert_eq!(commands(&streaming), vec!["cargo "]);
        streaming.absorb(r#"test\necho done"#);
        assert_eq!(commands(&streaming), vec!["cargo test\necho done"]);
    }

    #[test]
    fn a_label_exists_for_every_tool_and_none_of_them_reads_as_idling() {
        for name in [
            "create_file",
            "patch_file",
            "terminal_use",
            "web_search",
            "unknown_tool",
        ] {
            let l = label(name);
            assert!(!l.is_empty());
            assert_ne!(l, "Thinking", "an action label must never read as idling");
        }
    }

    #[test]
    fn track_tasks_is_recognized_and_extracts_tasks_without_generic_bullets() {
        let t = tool(
            "track_tasks",
            "Planning next steps",
            json!({
                "tasks": [
                    { "text": "Step 1", "status": "done" },
                    { "text": "Step 2", "status": "doing" },
                    { "text": "Step 3", "status": "todo" },
                ]
            }),
        );
        assert!(is_tasks(&t));
        assert!(
            bullets(&t).is_empty(),
            "tasks must not produce generic bullets"
        );
        let list = tasks(&t);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["text"], "Step 1");
        assert_eq!(list[0]["status"], "done");
    }
}
