//! `process_control` — type into a background task, or stop it.
//!
//! Two jobs, and neither of them is reading. A background task's output
//! reaches the model by its own `<system_notification>`, which carries the
//! report (line and output) the moment the task ends or starts waiting
//! (`session::background_report`); a log it wants more of is a file it reads
//! with `inspect_file` at the path that line names. So this tool has no report
//! mode: asking it for a status was a whole API request spent re-reading what
//! Rune had already been given (owner, 2026-11-03).
//!
//! What is left is what a wake cannot do by itself:
//!
//!  * **type** into a task that waits for it — `[Y/n]`, a password, `Press any
//!    key`. Whether Enter follows is `submit`: by default the terminal's own
//!    mode decides (a line read wants Enter; a single-key read does not, and an
//!    Enter left over there would answer the next question).
//!  * **stop** a task and its whole tree (`platform::terminate_tree`), the one
//!    platform-neutral way to end what Rune started — `kill` spelled in a shell
//!    reaches the wrong process on Windows and misses a daemon's double fork.
//!
//! Typed text can be typing a command (the waiting process may be a shell or a
//! REPL), so the call is judged the way a command is: `runs_commands`, its text
//! classified by the deny-list.
//!
//! `process_id` names one task and nothing else (owner, 2026-11-03): a listing
//! was a third way to spend a request, every launch notice and every wake
//! already names the id it belongs to, and `<status>` names the ones still
//! running. A placeholder that is no id is refused without being repeated
//! back, so no message teaches the model a listing exists.

use serde_json::{json, Value};

use super::{
    field_given_str, field_opt_str, field_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl,
};
use crate::error::{Error, Result};
use crate::perm::denylist::{self, Risk};
use crate::session::background::{state_of, BackgroundTaskStatus};

pub struct ProcessControl;

impl Tool for ProcessControl {
    fn name(&self) -> &'static str {
        "process_control"
    }

    fn runs_commands(&self) -> bool {
        true
    }

    fn mutates(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "process_control".into(),
            description: "Type into a background task that waits for input, or stop it with its whole process tree. It never reads output: a task's output arrives with its own <system_notification>, and the log file it names can be read with inspect_file.".into(),
            parameters: flat_schema(
                json!({
                    "process_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "One background task's id (e.g. bg_1a2b), as its launch notice, its notification or <status> names it."
                    },
                    "do": {
                        "type": "string",
                        "enum": ["type", "stop"],
                        "description": "type: send input to a task that waits. stop: end the task and every process it started."
                    },
                    "text": {
                        "type": "string",
                        "description": "The text to type. Empty with submit: enter presses Enter alone. Not used by stop."
                    },
                    "submit": {
                        "type": "string",
                        "enum": ["auto", "enter", "none"],
                        "description": "auto (the default when omitted or empty): Enter follows the text when the terminal reads a line at Enter, and not when it reads each key as pressed; enter: Enter always follows it (a full-screen program that confirms with Enter); none: the text only."
                    }
                }),
                &["process_id", "do"],
            ),
        }
    }

    /// Typed text may reach any program — a shell among them — so it is judged
    /// in every language a shell of this OS reads, the worst verdict winning
    /// (bash alone on Linux and macOS). A stop is Rune's own marked tree and
    /// nothing else, so it carries no command risk of its own.
    fn risk(&self, args: &Value) -> Risk {
        if field_opt_str(args, "do").map(str::trim) != Some("type") {
            return Risk::Normal;
        }
        let text = field_opt_str(args, "text").unwrap_or("");
        let mut worst = denylist::classify(text);
        if crate::platform::OS == crate::platform::Os::Windows {
            for kind in [
                crate::shell::ShellKind::PowerShell,
                crate::shell::ShellKind::Cmd,
            ] {
                let risk = denylist::classify_for(text, kind);
                if risk.rank() > worst.rank() {
                    worst = risk;
                }
            }
        }
        worst
    }

    fn preview(&self, args: &Value) -> String {
        let id = field_opt_str(args, "process_id").unwrap_or("a background process");
        if field_opt_str(args, "do").map(str::trim) == Some("stop") {
            return format!("stop {id}");
        }
        let text = field_opt_str(args, "text").unwrap_or("");
        format!("type {text:?} into {id}")
    }

    /// What is refused before a process is touched: a placeholder for an id,
    /// and a type that would send nothing. Text beside a stop is not refused:
    /// a provider that sends every declared field fills one it does not use
    /// with a blank or an invented value (AGENTS.md), and a stop reads none.
    fn validate(&self, args: &Value) -> Result<()> {
        super::validate_builtin(&self.declaration().parameters, args)?;
        let id = field_opt_str(args, "process_id").unwrap_or("").trim();
        if is_a_placeholder(id) {
            return Err(not_an_id());
        }
        if field_opt_str(args, "do").map(str::trim) == Some("type") {
            let enter = field_given_str(args, "submit") != Some("none");
            if field_given_str(args, "text").is_none() && !enter {
                return Err(nothing_to_type());
            }
        }
        Ok(())
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let task_id = field_str(args, "process_id")?.trim();
        if is_a_placeholder(task_id) {
            return Err(not_an_id());
        }
        let info = match ctx.background.get(task_id) {
            Some(info) => info,
            None => return Err(untracked(ctx, task_id)),
        };
        match field_opt_str(args, "do").map(str::trim) {
            Some("stop") => stop(ctx, task_id),
            // `type` is the schema's only other value; anything else was
            // refused by validation before a process was touched.
            _ => type_into(ctx, args, task_id, &info),
        }
    }
}

/// The spellings a listing used to answer, and a placeholder a provider that
/// fills every field invents: none of them is a task.
fn is_a_placeholder(id: &str) -> bool {
    matches!(id.to_ascii_lowercase().as_str(), "all" | "*" | "-" | "none")
}

/// The refusal of a placeholder. It names what an id looks like and where one
/// is found, and never the placeholder itself: a message that says "`all` is
/// not an id" puts the word back in front of the model. No tag is named in it
/// either: an error reaches the model inside a notice, where a tag is defused.
fn not_an_id() -> Error {
    Error::invalid(
        "process_id takes one background task's id, such as bg_1a2b: its launch notice and its notification name it, and the status lists those still running.",
    )
}

/// Nothing would reach the process: a call that typed nothing must not say it
/// typed. Blank text counts as no text, as an unused field does.
fn nothing_to_type() -> Error {
    Error::invalid(
        "nothing to type: the text is empty or blank and no Enter follows it; pass the text, or `submit: enter` to press Enter alone.",
    )
}

/// An id this session does not track: not found, as any name a tool cannot
/// find is. A log it left is still worth naming: what the process printed is
/// the one thing such an id can be asked for, and the model reads it as a file.
fn untracked(ctx: &ToolCtx<'_>, task_id: &str) -> Error {
    let out_file = ctx
        .results
        .join("terminal_use")
        .join(format!("{task_id}.txt"));
    if crate::session::background::is_task_id(task_id) && out_file.is_file() {
        return Error::NotFound(format!(
            "background task {task_id} is not tracked in this session, so nothing was done; what it printed is at {} — read it with inspect_file.",
            super::notice::quote(&out_file.display().to_string())
        ));
    }
    Error::NotFound(format!(
        "background task {} is not tracked in this session, so nothing was done; a launch notice and a notification name the ids that exist, and the status lists those still running.",
        super::notice::quote(task_id)
    ))
}

fn stop(ctx: &mut ToolCtx<'_>, task_id: &str) -> Result<Outcome> {
    let model = ctx.reader == super::Reader::Model;
    if !ctx.background.stop(task_id, model) {
        // It had ended already — by itself, or before this call took it. The
        // wake that carries its report either went or will; this only says so.
        let info = ctx.background.get(task_id);
        let (state, log) = match &info {
            Some(info) => (
                state_of(info),
                super::notice::quote(&info.out_file.display().to_string()),
            ),
            None => ("not tracked".to_string(), "unknown".to_string()),
        };
        return Ok(Outcome::new(
            format!("{task_id} had already ended"),
            super::notice::inline(&format!(
                "{task_id} had already ended ({state}); its output is at {log} — read it with inspect_file."
            )),
        )
        .with_detail(json!({
            "process_id": task_id,
            "status": "already ended",
        })));
    }
    // How long it ran, from the end the stop just recorded; `unknown` for a
    // record whose start nobody wrote, never hours since 1970.
    let (pid, ran, log) = match ctx.background.get(task_id) {
        Some(info) => {
            let ended = match info.status {
                BackgroundTaskStatus::Killed { finished_at }
                | BackgroundTaskStatus::Completed { finished_at, .. }
                | BackgroundTaskStatus::Lost { finished_at, .. } => finished_at,
                BackgroundTaskStatus::Running => crate::store::now_ms(),
            };
            (
                info.pid.to_string(),
                crate::session::background_report::ran(info.started_at, ended)
                    .unwrap_or_else(|| "an unknown time".to_string()),
                super::notice::quote(&info.out_file.display().to_string()),
            )
        }
        None => (
            "unknown".to_string(),
            "an unknown time".to_string(),
            "unknown".to_string(),
        ),
    };
    Ok(Outcome::new(
        format!("stopped {task_id}"),
        super::notice::inline(&format!(
            "Stopped {task_id} (PID {pid}) after {ran}; its output so far is in {log} — read it with inspect_file."
        )),
    )
    .with_detail(json!({
        "process_id": task_id,
        "pid": pid,
        "status": "Killed",
    })))
}

fn type_into(
    ctx: &mut ToolCtx<'_>,
    args: &Value,
    task_id: &str,
    info: &crate::session::background::BackgroundTaskInfo,
) -> Result<Outcome> {
    if !matches!(info.status, BackgroundTaskStatus::Running) {
        return Err(Error::invalid(format!(
            "background task '{task_id}' is not running ({}), so nothing was typed",
            state_of(info)
        )));
    }
    // Blank text is no text, as an unused field is: a provider that sends every
    // field must not type its filler into a prompt — a password prompt among
    // them. Text that says something is typed exactly as given.
    let text = field_opt_str(args, "text")
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("");
    let enter = match field_given_str(args, "submit") {
        Some("enter") => true,
        Some("none") => false,
        // The terminal's own mode; where it cannot be read, Enter.
        _ => info
            .waiting
            .map(|wait| wait.line)
            .or_else(|| ctx.background.line_mode(task_id))
            .unwrap_or(true),
    };
    if text.is_empty() && !enter {
        // Nothing would reach the process: the terminal reads each key and the
        // text is blank.
        return Err(nothing_to_type());
    }
    let mut bytes = text.as_bytes().to_vec();
    if enter {
        // What the Enter key sends; the terminal turns it into a newline for a
        // program that reads lines.
        bytes.push(b'\r');
    }
    ctx.background.send_input(task_id, &bytes)?;
    let notice = super::notice::typed(
        task_id,
        !text.is_empty(),
        enter,
        crate::platform::INPUT_WAIT_DETECTION,
    );
    Ok(
        Outcome::new(format!("typed into {task_id}"), notice).with_detail(json!({
            "process_id": task_id,
            "enter": enter,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ProcessControl {
        ProcessControl
    }

    #[test]
    fn the_declaration_puts_action_first_and_offers_two_operations() {
        let decl = tool().declaration();
        let properties = decl.parameters["properties"].as_object().unwrap();
        assert_eq!(properties.keys().next().map(String::as_str), Some("action"));
        assert_eq!(
            decl.parameters["properties"]["do"]["enum"],
            json!(["type", "stop"])
        );
        assert_eq!(
            decl.parameters["properties"]["submit"]["enum"],
            json!(["auto", "enter", "none"])
        );
        let required: Vec<&str> = decl.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"process_id") && required.contains(&"do"));
        assert!(!required.contains(&"text") && !required.contains(&"submit"));
    }

    /// A model that sends every field fills an unmeant `submit` with a blank:
    /// that is the default, never a refusal.
    #[test]
    fn a_blank_submit_is_the_default_and_an_unknown_one_is_refused() {
        let t = tool();
        for blank in ["", "  "] {
            let args = json!({"action": "answer", "process_id": "bg_x", "do": "type", "text": "y", "submit": blank});
            assert!(t.validate(&args).is_ok(), "{blank:?}");
        }
        let args = json!({"action": "answer", "process_id": "bg_x", "do": "type", "text": "y", "submit": "twice"});
        assert!(t.validate(&args).is_err());
        let args = json!({"action": "answer", "process_id": "bg_x", "do": "type", "text": "", "submit": "enter"});
        assert!(t.validate(&args).is_ok(), "Enter alone is a real answer");
        for blank in ["", "   ", "\n"] {
            let args = json!({"action": "answer", "process_id": "bg_x", "do": "type", "text": blank, "submit": "none"});
            let said = t.validate(&args).unwrap_err().to_string();
            assert!(said.contains("nothing to type"), "{blank:?}: {said}");
        }
    }

    /// A placeholder is no id, and its refusal never says the placeholder
    /// back (owner, 2026-11-03: `all` is gone, from the messages too): it says
    /// what an id looks like and where one is found.
    #[test]
    fn a_placeholder_id_is_refused_without_being_repeated() {
        let t = tool();
        for id in ["all", "ALL", "*", "-", "none"] {
            let args = json!({"action": "stop it", "process_id": id, "do": "stop"});
            let said = t.validate(&args).unwrap_err().to_string();
            assert!(said.contains("one background task's id"), "{id}: {said}");
            assert!(said.contains("bg_1a2b"), "{id}: {said}");
            assert!(!said.contains("`all`") && !said.contains("all`"), "{said}");
            assert!(!said.contains('<'), "a tag in an error is defused: {said}");
        }
        let description = t.declaration().parameters["properties"]["process_id"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!description.contains("all"), "{description}");
    }

    /// A stop reads no text, so text beside it — the blank or invented value a
    /// provider that sends every field puts there — never refuses the stop.
    #[test]
    fn a_stop_ignores_the_text_it_does_not_use() {
        let t = tool();
        for text in ["", "  ", "y", "n/a"] {
            let args = json!({"action": "stop it", "process_id": "bg_x", "do": "stop", "text": text, "submit": ""});
            t.validate(&args)
                .unwrap_or_else(|e| panic!("{text:?}: {e}"));
        }
        assert_eq!(
            t.preview(&json!({"process_id": "bg_x", "do": "stop", "text": "y"})),
            "stop bg_x"
        );
    }

    /// Typed text can be a command (the process may be a shell), so it is
    /// judged like one; stopping Rune's own task carries no command risk.
    #[test]
    fn typed_text_is_judged_like_a_command_and_a_stop_is_not() {
        let t = tool();
        assert!(t.runs_commands() && t.mutates());
        assert!(matches!(
            t.risk(&json!({"do": "type", "text": "sudo rm -rf /"})),
            Risk::Severe { .. }
        ));
        assert_eq!(t.risk(&json!({"do": "type", "text": "y"})), Risk::Normal);
        assert_eq!(t.risk(&json!({"do": "stop"})), Risk::Normal);
    }
}
