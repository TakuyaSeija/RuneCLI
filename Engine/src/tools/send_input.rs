//! `terminal_send_input` — type into a background process that waits for it.
//!
//! A command that asks a question (`[Y/n]`, a password, `Press any key`)
//! goes to the background the moment Rune sees it blocked reading its
//! terminal (`tools/terminal.rs`, `platform::Pty::input_wait`), and the
//! model is told so with its output. This is how it answers — and all it
//! does: the text is typed into that terminal and the call returns at once.
//! It neither waits for the process nor reports it (owner, 2026-10-11: a
//! call that typed, waited and then gave `manage_process`'s report did two
//! tools' work and printed the same report twice). What the process does
//! next is the monitor's to tell: a new wait for input or its end arrives
//! as a `<system_notification>`, and `manage_process` reads its output.
//!
//! Typing into a terminal can be typing a command — the process waiting may
//! be a shell or a REPL — so this tool runs commands as far as the gate is
//! concerned: `Ask` reads every call to a person, and the text is classified
//! the way a command line is.
//!
//! Whether Enter follows the text is `submit`: by default the terminal's own
//! mode decides (a line read wants Enter; a single-key read does not, and an
//! Enter left over there would answer the next question), and the model can
//! say otherwise for a full-screen program that wants Enter in raw mode.

use serde_json::{json, Value};

use super::{
    field_given_str, field_opt_str, field_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl,
};
use crate::error::{Error, Result};
use crate::perm::denylist::{self, Risk};
use crate::session::background::{state_of, BackgroundTaskStatus};

pub struct TerminalSendInput;

impl Tool for TerminalSendInput {
    fn name(&self) -> &'static str {
        "terminal_send_input"
    }

    fn runs_commands(&self) -> bool {
        true
    }

    fn mutates(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "terminal_send_input".into(),
            description: "Type into a background process that is waiting for input (terminal_use, manage_process or a <system_notification> said so). It types and returns at once: it does not wait for the process and does not report its output. When the process waits for input again or ends, a <system_notification> arrives; read its output with manage_process."
                .into(),
            parameters: flat_schema(
                json!({
                    "process_id": {
                        "type": "string",
                        "description": "The process id (e.g. bg_1a2b)."
                    },
                    "text": {
                        "type": "string",
                        "description": "The text to type. Empty with submit: enter presses Enter alone."
                    },
                    "submit": {
                        "type": "string",
                        "enum": ["auto", "enter", "none"],
                        "description": "auto (the default when omitted or empty): Enter follows the text when the terminal reads a line at Enter, and not when it reads each key as pressed; enter: Enter always follows it (a full-screen program that confirms with Enter); none: the text only."
                    }
                }),
                &["process_id", "text"],
            ),
        }
    }

    /// Typed text may reach any program — a shell among them — so it is
    /// judged in every language a shell of this OS reads, the worst verdict
    /// winning (bash alone on Linux and macOS).
    fn risk(&self, args: &Value) -> Risk {
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
        let text = field_opt_str(args, "text").unwrap_or("");
        format!("type {text:?} into {id}")
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let task_id = field_str(args, "process_id")?;
        let text = field_str(args, "text")?;
        let info = ctx.background.get(task_id).ok_or_else(|| {
            Error::invalid(format!(
                "background process '{task_id}' not found. Pass process_id: 'all' to manage_process to list all processes."
            ))
        })?;
        if !matches!(info.status, BackgroundTaskStatus::Running) {
            return Err(Error::invalid(format!(
                "background process '{task_id}' is not running ({}), so nothing was typed",
                state_of(&info)
            )));
        }
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
            // Nothing would reach the process, and a call that typed nothing
            // must not say it typed.
            return Err(Error::invalid(
                "nothing to type: the text is empty and no Enter follows it; pass submit: enter to press Enter alone",
            ));
        }
        let mut bytes = text.as_bytes().to_vec();
        if enter {
            // What the Enter key sends; the terminal turns it into a newline
            // for a program that reads lines.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> TerminalSendInput {
        TerminalSendInput
    }

    #[test]
    fn the_declaration_puts_action_first_and_offers_no_blank_choice() {
        let decl = tool().declaration();
        let properties = decl.parameters["properties"].as_object().unwrap();
        assert_eq!(properties.keys().next().map(String::as_str), Some("action"));
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
        assert!(required.contains(&"process_id") && required.contains(&"text"));
        assert!(!required.contains(&"submit"));
    }

    /// A model that sends every field fills an unmeant `submit` with a blank:
    /// that is the default, never a refusal.
    #[test]
    fn a_blank_submit_is_the_default_and_an_unknown_one_is_refused() {
        let t = tool();
        for blank in ["", "  "] {
            let args =
                json!({"action": "answer", "process_id": "bg_x", "text": "y", "submit": blank});
            assert!(t.validate(&args).is_ok(), "{blank:?}");
        }
        let args =
            json!({"action": "answer", "process_id": "bg_x", "text": "y", "submit": "twice"});
        assert!(t.validate(&args).is_err());
        let args = json!({"action": "answer", "process_id": "bg_x", "text": ""});
        assert!(t.validate(&args).is_ok(), "Enter alone is a real answer");
    }

    /// Typed text can be a command (the process may be a shell), so it is
    /// judged like one.
    #[test]
    fn typed_text_is_judged_like_a_command() {
        let t = tool();
        assert!(t.runs_commands() && t.mutates());
        assert!(matches!(
            t.risk(&json!({"text": "sudo rm -rf /"})),
            Risk::Severe { .. }
        ));
        assert_eq!(t.risk(&json!({"text": "y"})), Risk::Normal);
    }
}
