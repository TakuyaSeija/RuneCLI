//! `rune -p "…"` — one prompt, printed as it happens, then out.
//!
//! For scripts, pipes, and the times a person wants one answer rather than a
//! session. It is the same engine and the same protocol as the screen; only the
//! rendering is different, and deliberately plain — no cursor movement, no
//! colour when the output is not a terminal, nothing that would corrupt a pipe.
//!
//! Each segment of answer text is printed once it is the text the session keeps:
//! a pipe cannot take bytes back, so an attempt that is retried, or a response
//! the repetition guard stops, is never printed twice.
//!
//! # Permission in a pipe
//!
//! There is nobody to ask. Rather than hang on a question no one will answer, a
//! permission request is **refused**, and the refusal says which mode would have
//! allowed it. Defaulting to "yes" here would mean a script that runs `rm` the
//! first time a model decides to.

use std::io::Write;

use runeengine::proto::{Decision, StopReason};
use runeengine::{Command, Event};

use crate::driver::Driver;
use crate::ui::{ansi, term};

pub fn run(mut d: Driver, project: String, text: String) -> Result<(), String> {
    let session = d.open(&project)?;
    let colour = term::is_tty();
    let tint = |c: ansi::Ink, s: &str| {
        if colour {
            format!("{c}{s}{}", ansi::RESET)
        } else {
            s.to_string()
        }
    };

    d.engine.handle(Command::Prompt {
        id: 0,
        session,
        text,
        attach: Vec::new(),
    });

    let mut out = std::io::stdout();
    let mut stop = StopReason::End;
    // The answer text of the segment being streamed, held until the segment can
    // be trusted. A pipe cannot take bytes back: an attempt that fails and is
    // repeated, or a model that starts repeating itself and is stopped, would
    // otherwise be printed twice — once as it should not have been, once as it
    // is. `StreamReset` says the attempt's text is void, which only a buffer can
    // honour. Tool lines flush it first, so what is printed keeps its order.
    let mut segment = String::new();
    let flush = |out: &mut std::io::Stdout, segment: &mut String| {
        if !segment.is_empty() {
            let _ = write!(out, "{segment}");
            let _ = out.flush();
            segment.clear();
        }
    };
    while let Ok(event) = d.events.recv() {
        match event {
            Event::Text { delta, .. } => segment.push_str(&delta),
            // Everything streamed by the attempt that just failed, or that the
            // repetition guard stopped, is void.
            Event::StreamReset { .. } => segment.clear(),
            Event::ToolCall { name, action, .. } => {
                flush(&mut out, &mut segment);
                let _ = writeln!(
                    out,
                    "\n{}",
                    tint(ansi::MUTED, &format!("· {name}: {action}"))
                );
            }
            Event::ToolResult { ok, summary, .. } => {
                flush(&mut out, &mut segment);
                let mark = if ok { ansi::OK } else { ansi::REMOVED };
                let _ = writeln!(out, "{}", tint(mark, &format!("  {summary}")));
            }
            Event::McpInputRequest { session, req, .. } => {
                d.engine.handle(Command::McpInputReply {
                    id: 0,
                    session,
                    req,
                    reply: runeengine::capabilities::input::Reply {
                        action: "decline".into(),
                        content: None,
                    },
                });
            }
            Event::QuestionsRequest { session, req, .. } => {
                d.engine.handle(Command::QuestionsReply {
                    id: 0,
                    session,
                    req,
                    reply: runeengine::tools::questions::Reply::Unavailable,
                });
            }
            Event::PermissionRequest { req, preview, .. } => {
                let _ = writeln!(
                    out,
                    "{}",
                    tint(
                        ansi::WARN,
                        &format!(
                            "  refused (nobody to ask in a one-shot run): {preview}\n  \
                             run it in `rune` to answer, or use -read-only to avoid the question"
                        )
                    )
                );
                d.engine.handle(Command::PermissionReply {
                    id: 0,
                    req,
                    decision: Decision::Deny,
                });
            }
            Event::Error { message, .. } => {
                let _ = writeln!(out, "{}", tint(ansi::REMOVED, &format!("error: {message}")));
            }
            Event::Done { stop: reason, .. } => {
                stop = reason;
                break;
            }
            _ => {}
        }
    }
    // Whatever the last segment holds is the answer the session keeps.
    flush(&mut out, &mut segment);

    let _ = writeln!(out);
    d.engine.shutdown();
    match stop {
        StopReason::Failed => Err("the turn ended on an error".into()),
        _ => Ok(()),
    }
}
