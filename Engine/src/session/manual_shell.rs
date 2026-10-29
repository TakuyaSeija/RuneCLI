//! Manual shell authorization is a frontend operation, never syntax interpreted
//! in model prose, tool arguments, attachments or recovered history.
use super::{call, Runtime, SessionState};
use crate::{
    error::{Error, Result},
    panic,
    store::sessions::{ContextKind, ContextMeta, NewMessage},
};
use serde_json::json;

/// Only the first byte opts in. Never trim before checking the marker.
pub fn command(text: &str) -> Option<&str> {
    text.strip_prefix('!')
        .map(|text| text.strip_prefix(' ').unwrap_or(text))
}

pub fn validate(text: &str) -> Result<&str> {
    let command = command(text).ok_or_else(|| {
        Error::invalid("A manual shell submission must start with ! in the first position.")
    })?;
    if command.trim().is_empty()
        || command.contains('\0')
        || command.len() > crate::limits::MAX_TOOL_ARG_BYTES.saturating_sub(1024)
    {
        return Err(Error::invalid("Enter a nonempty shell command after !, within the input limit and without NUL characters."));
    }
    Ok(command)
}

/// Admission is already held. The command is stored before execution and never
/// replayed from that row; its output is Rune context, not user authorization.
pub(super) fn execute(rt: &Runtime, st: &SessionState, turn: i64, text: &str) -> Result<()> {
    let command = validate(text)?;
    let id = format!("manual-{}-{turn}", st.id);
    let args = json!({"action":"Running your manual shell command", "command":command});
    let outcome = panic::catch("manual shell", || {
        call::run_manual_shell(rt, st, turn, &id, &args)
    })
    .unwrap_or_else(Err);
    let mut finished = call::Finished::of(outcome);
    finished.detail.get_or_insert_with(|| json!({}))["manual_shell"] = json!(true);
    let shell = finished
        .detail
        .as_ref()
        .and_then(|detail| detail.get("shell"))
        .cloned();
    let recorded = json!({"id":id,"name":"terminal_use","args":args,"shell":shell,"ok":finished.ok,"summary":finished.summary,"detail":finished.detail,"content":finished.content});
    let body = format!(
        "The person's one-call manual execution attempt has settled.\ncommand: {}\nresult: {}",
        serde_json::to_string(command)?,
        serde_json::to_string(&finished.content)?
    );
    let mut meta = ContextMeta::named(ContextKind::ManualShell, id.clone());
    meta.note = Some(recorded.to_string());
    let stored = panic::lock(&rt.db).append_message(
        st.id,
        NewMessage::context(turn, &crate::frame::block("manual_shell", &[], &body), meta),
    );
    let reported = call::report(rt, st, &id, &finished);
    stored?;
    reported
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_first_position_selects_a_manual_script_and_all_later_bytes_survive() {
        for text in [
            "hello ! echo x",
            "hello\n! echo x",
            " ! echo x",
            "\n! echo x",
            "！ echo x",
            "`! echo x`",
        ] {
            assert!(command(text).is_none(), "{text:?}");
        }
        assert_eq!(
            validate("! echo one\n! echo two").unwrap(),
            "echo one\n! echo two"
        );
        assert_eq!(validate("!printf 'x'  \n").unwrap(), "printf 'x'  \n");
        for text in ["!", "! \n", "! echo\0x"] {
            assert!(validate(text).is_err());
        }
    }
}
