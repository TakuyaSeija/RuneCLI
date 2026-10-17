//! One turn: prompt in, tool calls executed, final answer out.
//!
//! The loop is deliberately boring, and every branch of it is a decision that
//! was made once and written down:
//!
//!  * the model is called, it may ask for tools, the results go back, repeat
//!  * a tool that fails returns its error **to the model**, not to the user —
//!    a wrong path or a failed compile is information the model can act on
//!  * a refusal is exactly the same shape as a failure, so a denied command
//!    cannot look to the model like a command that silently worked
//!  * cancellation is checked between every step, and inside the tools that take
//!    time
//!  * the loop is unbounded — a local agent works until the task is done, and
//!    cancellation is the user's hand on the only brake

use std::sync::atomic::Ordering;

use crate::error::{Error, Result};
use crate::panic;
use crate::proto::{Event, StopReason};
use crate::provider::{CallRef, Chunk, Flow, Message, Request};
use crate::store::sessions::{NewMessage, Role};

use super::call::{self, Finished};
use super::compact;
use super::{Runtime, SessionState};

pub fn persist_prompt(
    rt: &Runtime,
    st: &SessionState,
    text: &str,
    media: &[crate::media::Attachment],
    queued: bool,
    pending_id: Option<i64>,
) -> Result<i64> {
    persist_prompt_as(rt, st, text, media, queued, pending_id, false)
}

#[allow(clippy::too_many_arguments)]
fn persist_prompt_as(
    rt: &Runtime,
    st: &SessionState,
    text: &str,
    media: &[crate::media::Attachment],
    queued: bool,
    pending_id: Option<i64>,
    continuing: bool,
) -> Result<i64> {
    let db = panic::lock(&rt.db);
    let mut composer =
        db.jsonl
            .composer(st.id)?
            .unwrap_or_else(|| crate::store::composer::Composer {
                settings: st.settings(),
                mode: st.mode().as_str().into(),
                ..Default::default()
            });
    let turn = if queued {
        db.next_message_id()?
    } else {
        db.next_turn(st.id)?
    };
    db.set_title_if_unset(st.id, text)?;
    let mut media = media.to_vec();
    seal_media(rt, &mut media);
    let json = (!media.is_empty())
        .then(|| serde_json::to_string(&media))
        .transpose()?;
    if queued {
        composer
            .pending
            .push(crate::store::composer::PendingPrompt {
                id: turn,
                message_id: None,
                text: text.into(),
                media: media.to_vec(),
            });
    } else {
        let message_id = if let Some(id) = pending_id {
            let message_id = composer
                .pending
                .iter()
                .find(|pending| pending.id == id)
                .and_then(|pending| pending.message_id);
            let message_id = match message_id {
                Some(message_id) => message_id,
                None => db.next_message_id()?,
            };
            if let Some(pending) = composer.pending.iter_mut().find(|pending| pending.id == id) {
                pending.message_id = Some(message_id);
            }
            db.jsonl.write_composer(st.id, &composer)?;
            Some(message_id)
        } else {
            None
        };
        let message_id = match message_id {
            Some(id) => id,
            None => db.next_message_id()?,
        };
        let history = db.history_shared(st.id)?;
        let work = if continuing {
            crate::store::sessions::Work {
                source: super::work::source(&history).unwrap_or(message_id),
                finished: false,
            }
        } else {
            super::work::for_user(&history, message_id)
        };
        db.append_message(
            st.id,
            NewMessage {
                work: Some(work),
                id: Some(message_id),
                media: json.as_deref(),
                ..NewMessage::plain(turn, Role::User, text)
            },
        )?;
        // A session starts when its first message is sent, in the same step
        // that stores the message: a window reading the session finds the two
        // together or neither.
        if turn == 1 {
            let now = crate::store::now_ms();
            let _ = db.conn().execute(
                "UPDATE sessions SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now, st.id],
            );
        }
        // Said in the same critical section as the write (the database is held
        // from here to the end of this function): a window that asks for a view
        // finds the message either in neither place or in both — on disk and in
        // the account of the run — never on disk alone, which would draw it twice.
        emit(
            rt,
            Event::TurnStart {
                session: st.id,
                turn,
                text: Some(text.to_string()),
                notification: false,
                media: media.clone(),
                at: crate::store::now_ms(),
            },
        );
        if let Some(id) = pending_id {
            composer.pending.retain(|pending| pending.id != id);
        }
    }
    {
        // A user submit consumes only its matching composer, not another edit
        // accepted after that visible snapshot. Later edits remain unsent.
        let paths: Vec<_> = media.iter().map(|attachment| &attachment.path).collect();
        if composer.text.trim() == text.trim()
            && composer.attachments.iter().collect::<Vec<_>>() == paths
        {
            composer.text.clear();
            composer.attachments.clear();
            composer.cursor = 0;
        }
    }
    composer.revision = composer
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::Internal("Composer revision exhausted".into()))?;
    db.jsonl.write_composer(st.id, &composer)?;
    emit(
        rt,
        Event::Composer {
            session: st.id,
            request: 0,
            composer: Box::new(composer),
        },
    );
    Ok(turn)
}

pub fn run(
    rt: &Runtime,
    st: &SessionState,
    text: &str,
    media: Vec<crate::media::Attachment>,
    committed_turn: Option<i64>,
) -> Result<StopReason> {
    let _ = rt.limits.config.reload_if_changed();
    let turn = match committed_turn {
        Some(turn) => turn,
        None => persist_prompt(rt, st, text, &media, false, None)?,
    };
    start(rt, st, turn, text, false)
}

/// A turn Rune opens itself: a background task ended while nothing was running.
/// The notification is stored as a context row, never as the user's words.
pub fn run_notification(rt: &Runtime, st: &SessionState, text: &str) -> Result<StopReason> {
    let _ = rt.limits.config.reload_if_changed();
    let turn = persist_notification(rt, st, text)?;
    start(rt, st, turn, text, true)
}

/// Store a notification of Rune's as the opening of its own turn.
fn persist_notification(rt: &Runtime, st: &SessionState, text: &str) -> Result<i64> {
    use crate::store::sessions::{ContextKind, ContextMeta};
    let db = panic::lock(&rt.db);
    let turn = db.next_turn(st.id)?;
    db.append_message(
        st.id,
        NewMessage::context(turn, text, ContextMeta::new(ContextKind::Notification)),
    )?;
    // In the critical section of the write, as for a prompt.
    emit(
        rt,
        Event::TurnStart {
            session: st.id,
            turn,
            text: Some(text.to_string()),
            notification: true,
            media: Vec::new(),
            at: crate::store::now_ms(),
        },
    );
    Ok(turn)
}

/// Announce a prompt a window stored before any host ran it: a promoted draft's
/// first message (`Command::PromptCommitted`). The write was the window's own
/// (`Db::promote`), so no `persist_prompt` is here to speak for it, and a turn
/// nobody announces is a turn no window draws. `_db` is the caller's hold on the
/// database across marking the promotion started and this announcement — the
/// same pairing `persist_prompt` keeps between its write and its word.
pub fn announce_committed(
    rt: &Runtime,
    _db: &crate::store::Db,
    st: &SessionState,
    message: &crate::store::sessions::Message,
) {
    emit(
        rt,
        Event::TurnStart {
            session: st.id,
            turn: message.turn,
            text: Some(message.content.clone()),
            notification: false,
            media: message.media.clone(),
            at: crate::store::now_ms(),
        },
    );
}

fn start(
    rt: &Runtime,
    st: &SessionState,
    mut turn: i64,
    text: &str,
    notification: bool,
) -> Result<StopReason> {
    let began = crate::store::now_ms();
    // The turn was announced when its message was stored (`persist_prompt`,
    // `persist_notification`) or marked started (`announce_committed`): the
    // announcement and the write are one step.
    // A setting this build read wrongly (a key that is not one) is a bug of
    // ours, not of the user's file: said once in the session's log.
    for line in crate::settings::configuration::take_misuse() {
        super::log::warn(
            rt,
            st.id,
            "config",
            "config_key",
            format!("Rune read a configuration setting wrongly: {line}."),
        );
    }
    // What the session opens with, read now that its first request is stored.
    // A failure here costs the model its context, not the user their turn.
    if let Err(e) = super::context::open(rt, st, turn) {
        super::log::warn(
            rt,
            st.id,
            "context",
            e.code(),
            format!("Could not write the session's opening context: {e}"),
        );
    }
    if turn == 1 && !notification {
        let shield = crate::store::sessions::title_of(text);
        if !shield.is_empty() {
            emit(
                rt,
                Event::Title {
                    session: st.id,
                    title: shield,
                },
            );
        }
        if rt.provider.name() == "live" {
            super::title::generate_background(rt.clone(), st.id, text.to_string(), st.settings());
        }
    }

    // A panic anywhere in the loop is a bug in this engine, and the frontend
    // still has to be told the turn is over: without this it simply stops
    // hearing events and waits forever for a `done` that unwound past its emit.
    // The loop keeps going while a steer is waiting, and the session is let go
    // only by `settle`, which decides under the same lock a newcomer is queued
    // by: a steer or a wake can no longer land between the last look at the
    // queue and the release, stored with nobody left to answer it.
    let (stop, final_turn) = 'run: loop {
        let (stop, last) = loop {
            // Undo history is never trimmed (owner, 2026-10-11): every turn's
            // snapshots stay for the session's life, and what a session that
            // is deleted held is collected then (`Checkpoints::collect`).
            let (stop, last) = settle_turn(rt, st, turn);
            if !matches!(stop, StopReason::End | StopReason::Refused) {
                break (stop, last);
            }
            match take_steer_turn(rt, st) {
                Ok(Some(next)) => turn = next,
                Ok(None) => break (stop, last),
                Err(e) => {
                    report_turn_error(rt, st, turn, &e);
                    break (StopReason::Failed, turn);
                }
            }
        };
        let answerable = matches!(stop, StopReason::End | StopReason::Refused);
        loop {
            match st.settle(answerable) {
                // Asked for while the turn ran: it runs now, before the session
                // is let go, so a request racing `done` either belongs to this
                // run or starts an idle job.
                super::Settle::Compact => {
                    if let Err(e) = compact::maybe_compact(rt, st, &st.settings(), "", true) {
                        super::log::warn(
                            rt,
                            st.id,
                            "compact",
                            "compact_failed",
                            format!("Could not compact history: {e}"),
                        );
                    }
                }
                // Arrived after the last decision point: this run answers it.
                super::Settle::Continue(queued) => {
                    let queued = speakable(st, queued);
                    if queued.is_empty() {
                        continue;
                    }
                    match persist_all(rt, st, &queued) {
                        Ok(next) => {
                            turn = next;
                            continue 'run;
                        }
                        Err(e) => {
                            report_turn_error(rt, st, turn, &e);
                            // Let go, keeping what is still waiting.
                            if let super::Settle::Released(rest) = st.settle(false) {
                                store_unanswered(rt, st, rest);
                            }
                            break 'run (StopReason::Failed, last);
                        }
                    }
                }
                // Stopped or failed: what was waiting is stored as its own rows
                // so none is lost when the user continues — and history reads
                // back the way it was written.
                super::Settle::Released(queued) => {
                    store_unanswered(rt, st, queued);
                    break 'run (stop, last);
                }
            }
        }
    };

    // How long the session was working is counted here, by the only one that
    // knows — before the end is announced, so whoever reads the session after
    // `done` finds the time already in it.
    {
        let spent = (crate::store::now_ms().saturating_sub(began) + 500) / 1000;
        if let Ok(db) = rt.db.lock() {
            if let Ok(row) = db.get_session(st.id) {
                let _ = db.update_active_seconds(st.id, row.active_seconds + spent);
            }
        }
    }
    emit(
        rt,
        Event::Done {
            session: st.id,
            turn: final_turn,
            stop,
        },
    );
    Ok(stop)
}

/// Drives one turn and settles every way it can end: a panic becomes a `done`
/// with a `Cancelled` stop, an error is reported and stored, and the stop the
/// frontend sees is decided here exactly once.
fn settle_turn(rt: &Runtime, st: &SessionState, turn: i64) -> (StopReason, i64) {
    match panic::catch("the turn", || drive(rt, st, turn)).unwrap_or_else(Err) {
        Ok((stop, t)) => (stop, t),
        Err(Error::Cancelled) => (StopReason::Cancelled, turn),
        Err(e) => {
            report_turn_error(rt, st, turn, &e);
            (StopReason::Failed, turn)
        }
    }
}

fn report_turn_error(rt: &Runtime, st: &SessionState, turn: i64, e: &Error) {
    emit(
        rt,
        Event::Error {
            session: Some(st.id),
            code: e.code().to_string(),
            message: e.to_string(),
        },
    );
    let db = panic::lock(&rt.db);
    let work = db
        .history_shared(st.id)
        .ok()
        .and_then(|history| super::work::source(&history))
        .map(|source| crate::store::sessions::Work {
            source,
            finished: false,
        });
    let _ = db.append_message(
        st.id,
        NewMessage {
            work,
            ..NewMessage::plain(turn, Role::Error, &e.to_string())
        },
    );
}

/// Queued messages, with the completion wakes a tool result has already
/// answered for taken back out of them.
///
/// A wake is withdrawn rather than delivered when the model has been given the
/// whole report by `manage_process`: the queue would otherwise hand the model a
/// second, thinner copy of a fact it is already holding. The strip runs per
/// message, so withdrawing a wake from one never edits another, and a message
/// that was nothing but an answered wake drops out on its own. An empty result
/// means there is nothing left to say.
fn speakable(st: &SessionState, queued: Vec<super::Queued>) -> Vec<super::Queued> {
    queued
        .into_iter()
        .filter_map(|mut queued| {
            if queued.origin == super::Origin::Rune {
                queued.text = st.background.without_answered_wakes(&queued.text);
            }
            (!queued.text.is_empty() || !queued.media.is_empty()).then_some(queued)
        })
        .collect()
}

/// Every message waiting now, speakable ones only.
fn take_speakable_steers(st: &SessionState) -> Vec<super::Queued> {
    speakable(st, st.take_queued())
}

/// Store each message as a turn of its own, in order; the last turn allocated.
fn persist_all(rt: &Runtime, st: &SessionState, queued: &[super::Queued]) -> Result<i64> {
    let mut last = 0;
    for message in queued {
        // `persist_queued` announces the turn as it stores it.
        last = persist_queued(rt, st, message)?;
    }
    Ok(last)
}

/// Messages a stopped or failed run leaves behind, stored without an answer.
/// A row that cannot be written is said in the session's log: the run's `done`
/// must still be announced, or the frontend waits for it forever.
fn store_unanswered(rt: &Runtime, st: &SessionState, queued: Vec<super::Queued>) {
    for message in speakable(st, queued) {
        if let Err(e) = persist_queued(rt, st, &message) {
            super::log::warn(
                rt,
                st.id,
                "turn",
                e.code(),
                format!("Could not store a waiting message: {e}"),
            );
        }
    }
}

/// Answer what was waiting when the work that held the session ended without
/// taking it — a wake that arrived during a direct tool call or a manual
/// compaction. Called with the session held; nothing left to answer is not an
/// error, and the caller's guard lets the session go.
pub fn run_queued(rt: &Runtime, st: &SessionState) -> Result<StopReason> {
    let _ = rt.limits.config.reload_if_changed();
    let queued = take_speakable_steers(st);
    let Some(last) = queued.last() else {
        return Ok(StopReason::End);
    };
    let (text, notification) = (last.text.clone(), last.origin == super::Origin::Rune);
    let turn = persist_all(rt, st, &queued)?;
    start(rt, st, turn, &text, notification)
}

/// Store one queued message under the role its origin calls for, as a turn of
/// its own.
fn persist_queued(rt: &Runtime, st: &SessionState, queued: &super::Queued) -> Result<i64> {
    match queued.origin {
        super::Origin::User => persist_prompt_as(
            rt,
            st,
            &queued.text,
            &queued.media,
            false,
            queued.committed,
            true,
        ),
        super::Origin::Rune => persist_notification(rt, st, &queued.text),
    }
}

/// Persists every queued message as its own turn, each with its own
/// `TurnStart`, and returns the last turn allocated. The single consumer of the
/// queue inside the loop; `None` means the queue was empty and the turn may end.
///
/// One model call still follows, not one per message: they are separate in
/// history, so the next request carries all of them in order and the model
/// answers them together. A steer is a message, not an interruption.
fn take_steer_turn(rt: &Runtime, st: &SessionState) -> Result<Option<i64>> {
    let mut last = None;
    for queued in take_speakable_steers(st) {
        // `persist_queued` announces the turn as it stores it.
        let turn = persist_queued(rt, st, &queued)?;
        last = Some(turn);
    }
    Ok(last)
}

/// Failures of one exact call before its result says to stop repeating it.
const REPEATED_FAILURES: usize = 3;

/// What makes two calls the same call: the tool and its arguments, without the
/// `action` sentence. The sentence is the model's words to the person, not part
/// of the request — a model that reworded it each time sent one request 54
/// times in a turn and was never told it was repeating itself (2026-10-10).
fn same_call(name: &str, args: &serde_json::Value) -> (String, String) {
    let mut args = args.clone();
    if let Some(fields) = args.as_object_mut() {
        fields.remove("action");
    }
    (name.to_string(), args.to_string())
}

/// A failed result, restated as a repeated failure: the sentence that says so
/// goes first, in place of the notice the failure carried, and whatever the
/// tool printed after it stays. What the sentence asks of the model is in the
/// sentence itself.
fn repeated(finished: &Finished, times: usize) -> String {
    use crate::tools::notice;
    let body = notice::split(&finished.content).rest;
    notice::prepend(&[notice::repeated_failure(times, &finished.reason)], body)
}

fn drive(rt: &Runtime, st: &SessionState, mut turn: i64) -> Result<(StopReason, i64)> {
    panic::lock(&rt.settings).reload_providers();
    panic::lock(&rt.settings).reload_secrets();
    st.capabilities.refresh()?;
    st.rebuild_sandbox();
    // The session's classifier policy is the file as its first work found
    // it, whatever the mode (owner, 2026-10-13); later edits wait for a reload.
    crate::perm::classifier::pin(rt, st);
    report_unavailable(
        rt,
        st,
        &st.capabilities
            .connect_enabled(&st.registry, &st.cancel, &|server, roots| {
                super::call::authorize_mcp_roots(rt, st, server, roots)
            })?,
    );
    // The cached prefix: the same bytes for every request of every session.
    // Everything that moves is told through `super::context`.
    let system = super::prompt::SYSTEM.to_string();
    // Validate the initial selection; each subsequent API request takes its own
    // complete session snapshot so switching models never interrupts a stream.
    if rt.provider.name() == "live" && st.settings().model.trim().is_empty() {
        return Err(Error::invalid(
            "No model selected — please select a model in settings (/settings) before sending prompts.",
        ));
    }

    // The provider/model and tool list of the previous request in this turn,
    // for the cache-miss check: only a request that extends that one should hit.
    let mut warm_route: Option<(String, String, usize)> = None;
    // Whether this turn already compacted because a provider said the request
    // was longer than the model's context.
    let mut overflow_compacted = false;
    // How often each exact call (`same_call`) has failed in this turn.
    let mut failures: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();

    loop {
        if st.cancel.load(Ordering::Relaxed) {
            return Ok((StopReason::Cancelled, turn));
        }

        // Another Rune process wrote rune.toml: every knob this turn reads —
        // limits, policy, enabled skills and MCP servers — must honour it from
        // this round on, exactly as a change made in this window does. Gated on
        // the configuration actually changing — by its generation, so a change
        // a Settings read or a background monitor took in first is still acted
        // on here — because a capability refresh walks the skills tree and a
        // turn can run many rounds.
        let _ = rt.limits.config.reload_if_changed();
        let generation = rt.limits.config.generation();
        if st.configuration_seen.swap(generation, Ordering::AcqRel) != generation {
            super::log::info(
                rt,
                st.id,
                "config",
                "reloaded",
                "rune.toml changed; the new settings apply from this request on.",
            );
            crate::perm::classifier::invalidate(st);
            let _ = st.capabilities.refresh();
            report_unavailable(
                rt,
                st,
                &st.capabilities
                    .connect_enabled(&st.registry, &st.cancel, &|server, roots| {
                        super::call::authorize_mcp_roots(rt, st, server, roots)
                    })?,
            );
            panic::lock(&rt.settings).reload_providers();
            panic::lock(&rt.settings).reload_secrets();
        }
        // A path grant persisted by another Rune process — "always allow" in a
        // second window, or `rune -permission revoke`'s counterpart — belongs to
        // every session on this project, not only the one that asked.
        let standing = crate::engine::standing_reach(&rt.db, &st.project);
        st.reseed_reach(&standing);
        st.reseed_services(crate::engine::standing_services(&rt.db, &st.project));
        // After both: a refresh above may have changed the skill roots, and a
        // tool in the last round may have created the MCP media root.
        st.sync_sandbox();

        // Model/parameter switch: if settings were updated in SQLite by another
        // process or concurrent command, sync into the session state so the NEXT
        // API request boundary reflects the new selection immediately mid-turn.
        if let Ok(Some(json)) = panic::lock(&rt.db).settings_json(st.id) {
            if let Ok(db_settings) = serde_json::from_str::<crate::settings::ModelSettings>(&json) {
                if db_settings != st.settings() {
                    st.set_settings(db_settings);
                }
            }
        }

        // Request-boundary switch: keep the in-flight stream untouched, then use
        // the newly selected provider/model AND its generation parameters together.
        // This snapshot also owns the accounting and tool-policy context for the
        // response, even if the user changes the selection again while it streams.
        let settings = st.settings();
        // Before the request is built, not after it fails: a transcript that has
        // outgrown the model's window is rejected outright, and the rejection
        // arrives after the call was already paid for. A summariser that fails
        // is said in the log and the person is asked whether to try again
        // (owner, 2026-10-13): yes retries and the turn goes on; no stops the
        // turn, which would otherwise send a request too long for its model.
        let force = st.take_compaction();
        // Read once per round and shared: compaction decides over these rows
        // and the request is built from them. Dropped with this round, so the
        // next append extends the store's copy in place.
        let mut transcript = panic::lock(&rt.db).history_shared(st.id)?;
        let mut compaction =
            compact::maybe_compact_with(rt, st, &settings, &system, force, &transcript);
        loop {
            match compaction {
                // A compaction is the sanctioned prefix reset: the next request
                // is cold by design.
                Ok(true) => {
                    warm_route = None;
                    break;
                }
                Ok(false) => break,
                Err(Error::Cancelled) => return Ok((StopReason::Cancelled, turn)),
                Err(e) => {
                    super::log::warn(
                        rt,
                        st.id,
                        "compact",
                        e.code(),
                        format!("Could not summarise history: {e}"),
                    );
                    match compact::ask_retry(rt, st, &e) {
                        // Asked for again on purpose: due or not by the
                        // estimate, it was due when it failed.
                        compact::Retry::Again => {
                            compaction = compact::maybe_compact_with(
                                rt,
                                st,
                                &settings,
                                &system,
                                true,
                                &transcript,
                            )
                        }
                        compact::Retry::Stop => return Err(e),
                        compact::Retry::Cancelled => return Ok((StopReason::Cancelled, turn)),
                    }
                }
            }
        }
        if st.cancel.load(Ordering::Relaxed) {
            return Ok((StopReason::Cancelled, turn));
        }

        // What changed since the model was last told — a status value, a memory
        // index this session's own write pushed over budget — is written now,
        // as rows of its own, before the request that carries them.
        let compaction = panic::lock(&rt.db).compaction(st.id)?;
        let updated = {
            let held = compact::apply(
                transcript.iter().collect::<Vec<_>>(),
                compaction.as_ref().map(|c| c.through_id),
            );
            super::context::before_request(rt, st, &settings, compaction.as_ref(), &held, turn)?
        };
        if updated {
            transcript = panic::lock(&rt.db).history_shared(st.id)?;
        }
        let messages = history(rt, st, &settings, compaction.as_ref(), &transcript)?;
        drop(transcript);
        let declared = st.registry.declared();
        // The provider owns tokenization and context admission. Rune has already
        // bounded its own tool/attachment bytes; do not turn character estimates
        // into a false provider-context rejection for text or any binary media.
        let request = Request {
            control: crate::provider::RequestControl {
                cancel: Some(st.cancel.clone()),
                attachment_text_chars: Some(
                    rt.limits.config.number("attachments.max_text_chars") as usize
                ),
                ..Default::default()
            },
            system: system.clone(),
            messages,
            tools: declared.list.clone(),
            settings: settings.clone(),
        };

        // Calibration is for text-only requests; never infer tokens from media
        // bytes or allow an attachment sample to distort later text estimates.
        let text_estimate = {
            compact::estimate_tokens(&request.system)
                + declared.tokens
                + request
                    .messages
                    .iter()
                    .map(|m| {
                        compact::estimate_tokens(&m.content)
                            + compact::estimate_tokens(
                                &serde_json::to_string(&m.tool_calls).unwrap_or_default(),
                            )
                            + m.reasoning.as_deref().map_or(0, compact::estimate_tokens)
                            + m.native(&settings.provider, &settings.model)
                                .map_or(0, |v| compact::estimate_tokens(&v.to_string()))
                    })
                    .sum::<usize>()
        };
        let estimated_in = if request.messages.iter().any(|m| !m.media.is_empty()) {
            0
        } else {
            text_estimate
        };
        let budget = compact::compact_threshold(rt, &settings);
        if compact::corrected(rt, &settings, text_estimate) > budget {
            return Err(Error::invalid(format!(
                "Request exceeds Rune's usable context budget after compaction (estimated {} tokens; budget {budget}). The latest user input, newest complete tool group, or fixed instructions cannot be reduced safely. Use smaller input/file ranges or select a model with a larger configured budget; no agent API request was sent and no tools were replayed.",
                compact::corrected(rt, &settings, text_estimate))));
        }
        if st.cancel.load(Ordering::Relaxed) {
            return Ok((StopReason::Cancelled, turn));
        }
        let mut text = String::new();
        let mut thinking = String::new();
        let mut calls: Vec<(String, String, serde_json::Value)> = Vec::new();
        let mut invalid_calls = std::collections::HashMap::new();
        let mut continuation = None;
        let mut completion = None;
        let mut accounting_error = None;
        let mut restart_request = false;
        let mut checked_at = None;
        let mut logged_attempt: Option<u32> = None;
        let cancel = st.cancel.clone();

        let stream_res = rt.provider.stream(&request, &mut |chunk| {
            if cancel.load(Ordering::Relaxed) && !matches!(&chunk, Chunk::Usage { .. }) {
                return Flow::Stop;
            }
            match chunk {
                Chunk::Text(delta) => {
                    text.push_str(&delta);
                    emit(
                        rt,
                        Event::Text {
                            session: st.id,
                            delta,
                        },
                    );
                }
                Chunk::Thinking(delta) => {
                    thinking.push_str(&delta);
                    emit(
                        rt,
                        Event::Thinking {
                            session: st.id,
                            delta,
                        },
                    );
                }
                Chunk::ToolArgs {
                    id,
                    index,
                    name,
                    delta,
                } => {
                    emit(
                        rt,
                        Event::ToolArgs {
                            session: st.id,
                            id,
                            index,
                            name,
                            delta,
                        },
                    );
                }
                Chunk::ToolCall { id, name, args } => calls.push((id, name, args)),
                Chunk::InvalidToolCall { id, name, reason } => {
                    invalid_calls.insert(id.clone(), reason);
                    calls.push((id, name, serde_json::json!({})));
                    continuation = None;
                }
                Chunk::Continuation(native) => continuation = Some(native),
                Chunk::Finished(end) => {
                    completion = Some(end.clone());
                    emit(
                        rt,
                        Event::Completion {
                            session: st.id,
                            completion: end,
                        },
                    );
                }
                Chunk::Usage {
                    input,
                    output,
                    cached,
                    details,
                } => {
                    let cost = panic::lock(&rt.settings).price_of(
                        &settings.provider,
                        &settings.model,
                        input,
                        output,
                        cached,
                        &details,
                    );
                    if let Err(error) = panic::lock(&rt.db).record_usage_role(
                        st.id,
                        turn,
                        input,
                        output,
                        cached,
                        cost,
                        &settings.provider,
                        &settings.model,
                        "agent",
                        &details,
                        estimated_in as u64,
                    ) {
                        accounting_error = Some(error);
                    }
                    super::log::usage(
                        rt,
                        st.id,
                        "agent",
                        &settings.model,
                        input,
                        output,
                        cached,
                        cost,
                    );
                    let warm = warm_route.as_ref().is_some_and(|(provider, model, tools)| {
                        provider == &settings.provider
                            && model == &settings.model
                            && *tools == request.tools.len()
                    });
                    if let Some(message) = panic::lock(&st.cache_watch).observe(
                        &settings.provider,
                        &settings.model,
                        warm,
                        input,
                        cached,
                    ) {
                        super::log::warn(rt, st.id, "cache", "cache_miss", message);
                    }
                    emit(
                        rt,
                        Event::Usage {
                            session: st.id,
                            input,
                            output,
                            cached,
                            cost,
                            details: Box::new(details),
                        },
                    );
                }
                Chunk::Reset => {
                    text.clear();
                    thinking.clear();
                    calls.clear();
                    invalid_calls.clear();
                    continuation = None;
                    completion = None;
                    emit(rt, Event::StreamReset { session: st.id });
                }
                Chunk::Retrying {
                    attempt,
                    of,
                    wait_ms,
                    rotated,
                    reason,
                } => {
                    // Once per attempt: the countdown repeats this chunk ten
                    // times a second, and the log is not a clock.
                    if logged_attempt != Some(attempt) {
                        logged_attempt = Some(attempt);
                        super::log::warn(
                            rt,
                            st.id,
                            "retry",
                            "retrying",
                            format!(
                                "The request failed ({reason}); attempt {attempt} of {of} in {}s{}.",
                                wait_ms.div_ceil(1000),
                                if rotated { " with the next API key" } else { "" }
                            ),
                        );
                    }
                    // A countdown sends this ten times a second; the selection —
                    // which the user may change while waiting — is read at most
                    // twice a second, not on every tick (audit RT9).
                    let due = checked_at.is_none_or(|at: std::time::Instant| {
                        at.elapsed() >= std::time::Duration::from_millis(500)
                    });
                    let latest = due
                        .then(|| {
                            checked_at = Some(std::time::Instant::now());
                            panic::lock(&rt.db).settings_json(st.id).ok().flatten()
                        })
                        .flatten();
                    if let Some(latest) = latest.and_then(|json| {
                        serde_json::from_str::<crate::settings::ModelSettings>(&json).ok()
                    }) {
                        // The failed attempt's output is void either way, so a
                        // new selection takes over at once.
                        if latest != settings {
                            if !(text.is_empty() && thinking.is_empty() && calls.is_empty()) {
                                text.clear();
                                thinking.clear();
                                calls.clear();
                                invalid_calls.clear();
                                emit(rt, Event::StreamReset { session: st.id });
                            }
                            continuation = None;
                            completion = None;
                            restart_request = true;
                            emit(
                                rt,
                                Event::Retrying {
                                    session: st.id,
                                    attempt,
                                    of,
                                    wait_ms: 0,
                                    rotated: false,
                                    reason: "Request settings changed".into(),
                                },
                            );
                            return Flow::Stop;
                        }
                    }
                    emit(
                        rt,
                        Event::Retrying {
                            session: st.id,
                            attempt,
                            of,
                            wait_ms,
                            rotated,
                            reason,
                        },
                    );
                }
            }
            Flow::Continue
        });

        // Token accounting records what a turn cost; it is not the turn. A database
        // that refused the write has already refused the work, so taking the turn
        // down over it would throw away a response the model had produced and the
        // user had paid for. Reported, and the turn carries on.
        if let Some(error) = accounting_error {
            super::log::warn(
                rt,
                st.id,
                "usage",
                error.code(),
                format!("could not record token usage: {error}"),
            );
        }
        warm_route = Some((
            settings.provider.clone(),
            settings.model.clone(),
            request.tools.len(),
        ));
        if restart_request && !st.cancel.load(Ordering::Relaxed) {
            // Retry has produced no committed response. Preserve the turn/tool
            // results, then rebuild route, generation settings, history and
            // compaction admission together rather than resending the stale body.
            continue;
        }
        match stream_res {
            Ok(()) => {}
            Err(Error::Cancelled) if st.cancel.load(Ordering::Relaxed) => {}
            // Longer than the model's context: compact, once, and send again.
            // A second overflow after compacting is reported as it is.
            Err(Error::ContextOverflow(_)) if !overflow_compacted => {
                overflow_compacted = true;
                st.ask_compaction();
                continue;
            }
            // Cut off at the output limit: the owner's contract keeps what
            // was written — on screen and in the transcript alike — and ends
            // the turn saying why, rather than repeating a request that would
            // stop at the same place.
            Err(e @ Error::Incomplete(_)) => {
                if !text.trim().is_empty() || !thinking.trim().is_empty() {
                    let db = panic::lock(&rt.db);
                    db.append_message(
                        st.id,
                        NewMessage {
                            tool_calls: None,
                            reasoning: (!thinking.trim().is_empty()).then_some(thinking.as_str()),
                            ..NewMessage::plain(turn, Role::Assistant, &text)
                        },
                    )?;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }

        if st.cancel.load(Ordering::Relaxed) {
            if !text.trim().is_empty() || !thinking.trim().is_empty() {
                let db = panic::lock(&rt.db);
                let _ = db.append_message(
                    st.id,
                    NewMessage {
                        tool_calls: None,
                        reasoning: (!thinking.trim().is_empty()).then_some(thinking.as_str()),
                        ..NewMessage::plain(turn, Role::Assistant, &text)
                    },
                );
            }
            return Ok((StopReason::Cancelled, turn));
        }

        let mut seen = std::collections::HashSet::new();
        if calls
            .iter()
            .any(|(id, _, _)| id.is_empty() || !seen.insert(id.clone()))
        {
            return Err(Error::Provider(
                "duplicate or empty tool call ID in response".into(),
            ));
        }
        let mut prior_ids: std::collections::HashSet<String> = request
            .messages
            .iter()
            .flat_map(|m| {
                m.tool_calls
                    .iter()
                    .map(|c| c.id.clone())
                    .chain(m.tool_call_id.clone())
            })
            .collect();
        for (id, _, _) in &mut calls {
            if let Some(unique_id) = unique_id(id, &mut prior_ids) {
                let old_id = std::mem::replace(id, unique_id.clone());
                if let Some(reason) = invalid_calls.remove(&old_id) {
                    invalid_calls.insert(unique_id.clone(), reason);
                }
                remap_continuation(continuation.as_mut(), &old_id, &unique_id);
            }
        }
        let mut calls = plan_calls(rt, calls);
        for call in &mut calls {
            if let Some(reason) = invalid_calls.remove(&call.id) {
                call.refusal = Some(Error::invalid(reason));
            }
            let resolved = st.registry.get(&call.name);
            if let Ok(tool) = &resolved {
                // Before anything reads the arguments: validation below, the
                // history row after it, and the tool itself all have to see the
                // same thing, and the sentence has to exist before any of them.
                crate::tools::ensure_action(&mut call.args, tool.as_ref());
            }
            // Keep the variant, not prose: Invalid/NotFound never mean that
            // the tool ran. The execution path also validates before the gate.
            if call.refusal.is_none() {
                match resolved {
                    Ok(tool) => {
                        if let Err(error) = tool.validate(&call.args) {
                            call.refusal = Some(error);
                        }
                    }
                    Err(error) => call.refusal = Some(error),
                }
            }
        }
        // Stored whenever the model produced *anything*, including a turn that
        // was only tool calls. Text-only storage was the bug: the calls vanished
        // from history, so the next request carried tool results answering
        // nothing — a transcript a real provider rejects.
        if !text.trim().is_empty() || !calls.is_empty() {
            let refs: Vec<CallRef> = calls
                .iter()
                .map(|call| CallRef {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.args.to_string(),
                })
                .collect();
            let native = continuation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let encoded = if refs.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&refs)?)
            };
            let db = panic::lock(&rt.db);
            let history = db.history_shared(st.id)?;
            let work = super::work::source(&history).map(|source| crate::store::sessions::Work {
                source,
                finished: calls.is_empty()
                    && !completion.as_ref().is_some_and(|c| {
                        c.category == crate::provider::FinishCategory::Continuation
                    }),
            });
            db.append_message(
                st.id,
                NewMessage {
                    work,
                    continuation: native.as_deref(),
                    tool_calls: encoded.as_deref(),
                    reasoning: (!thinking.trim().is_empty()).then_some(thinking.as_str()),
                    ..NewMessage::plain(turn, Role::Assistant, &text)
                },
            )?;
        }

        // The SSE request has ended and its assistant message is persisted.
        // Compaction may run NOW, even if returned tools wait for permission;
        // cut_point excludes unresolved tool groups from the summarized range.
        // A failure here is not asked about while this response's calls wait:
        // the compaction is asked for again, so the next request boundary —
        // after the calls — runs it and asks whether to retry if it fails.
        if st.take_compaction() {
            if let Err(e) = compact::maybe_compact(rt, st, &settings, &system, true) {
                super::log::warn(
                    rt,
                    st.id,
                    "compact",
                    "compact_failed",
                    format!("Could not compact history: {e}"),
                );
                if !matches!(e, Error::Cancelled) {
                    st.ask_compaction();
                }
            }
        }
        let descriptors: Vec<_> = calls
            .iter()
            .filter(|c| c.refusal.is_none())
            .map(|c| crate::perm::classifier::Call {
                id: &c.id,
                name: &c.name,
                args: &c.args,
            })
            .collect();
        if !descriptors.is_empty() {
            if let Err(e) = crate::perm::classifier::prepare(rt, st, turn, &settings, &descriptors)
            {
                let config = crate::perm::classifier::configuration(rt);
                for c in &mut calls {
                    if c.refusal.is_none()
                        && matches!(call::asks_person(st, &c.name, &c.args), Ok(false))
                        && !config
                            .as_ref()
                            .is_ok_and(|cfg| crate::perm::classifier::excluded(cfg, &c.name))
                    {
                        c.refusal = Some(match &e {
                            Error::Classifier(supervision) => {
                                Error::Classifier(supervision.clone())
                            }
                            Error::Cancelled => Error::Cancelled,
                            other => Error::Classifier(crate::error::Supervision::undecided(
                                "classifier_unavailable",
                                other.to_string(),
                            )),
                        });
                    }
                }
            }
        }

        if calls.is_empty() {
            if completion
                .as_ref()
                .is_some_and(|c| c.category == crate::provider::FinishCategory::Continuation)
            {
                continue;
            }
            turn = match take_steer_turn(rt, st)? {
                Some(next) => next,
                None => {
                    return Ok((
                        if completion
                            .as_ref()
                            .is_some_and(|c| c.category == crate::provider::FinishCategory::Refusal)
                        {
                            StopReason::Refused
                        } else {
                            StopReason::End
                        },
                        turn,
                    ));
                }
            };
            continue;
        }

        // Consecutive calls that merely read run together (owner, 2026-10-06,
        // the split Claude Code makes); every other call runs on its own, in
        // the model's order. Each result is recorded in that order either way,
        // and a call on its own is recorded before the next one starts — the
        // history reads exactly as if every call had run one by one.
        let mut next = 0;
        while next < calls.len() {
            let end = together(rt, st, &calls, next);
            let batch = &mut calls[next..end];
            next += batch.len();
            let results = match batch {
                [call] => vec![call::run(
                    rt,
                    st,
                    turn,
                    &call.id,
                    &call.name,
                    &call.args,
                    call.refusal.take(),
                )],
                _ => run_together(rt, st, turn, batch),
            };
            for (call, result) in batch.iter().zip(results) {
                let abandoned = matches!(&result, Err(e) if call::turn_was_cancelled(st, e));
                let mut finished = match result {
                    // Stopped, not failed — and whether that was this one call or the
                    // whole turn is decided below, after both have been recorded.
                    Err(Error::Cancelled) => Finished::stopped(),
                    // Everything else, success or failure, reaches the model in the
                    // same shape: a failed tool result it can read and correct.
                    other => Finished::of(other),
                };
                // The same call failing again and again is a loop, not progress:
                // the third time, the result says so (audit H4).
                if !finished.ok {
                    let repeats = failures
                        .entry(same_call(&call.name, &call.args))
                        .or_insert(0usize);
                    *repeats += 1;
                    if *repeats >= REPEATED_FAILURES {
                        finished.content = repeated(&finished, *repeats);
                        let status = crate::tools::notice::status::REPEATED_FAILURE;
                        finished.status = Some(status.to_string());
                        if let Some(detail) = finished
                            .detail
                            .as_mut()
                            .and_then(serde_json::Value::as_object_mut)
                        {
                            detail.insert("status".into(), serde_json::json!(status));
                        }
                    }
                }

                call::report(rt, st, &call.id, &finished)?;

                // The memory index was written: the next request checks it against
                // its budget (`context::before_request`), whichever tool wrote it.
                // A part of the context the model wrote itself is not news to it.
                if let Some(path) = finished
                    .detail
                    .as_ref()
                    .and_then(|detail| detail["path"].as_str())
                    .filter(|_| finished.ok)
                {
                    if crate::memory::is_index(path) {
                        st.memory_written.store(true, Ordering::Release);
                    }
                    super::context::written(rt, st, path);
                }

                {
                    let db = panic::lock(&rt.db);
                    seal_media(rt, &mut finished.media);
                    let media = (!finished.media.is_empty())
                        .then(|| serde_json::to_string(&finished.media))
                        .transpose()?;
                    db.append_message(
                        st.id,
                        NewMessage {
                            media: media.as_deref(),
                            tool_call_id: Some(&call.id),
                            tool_name: Some(&call.name),
                            ..NewMessage::plain(turn, Role::Tool, &finished.content)
                        },
                    )?;
                }

                // New human answers invalidate decisions prepared before the form.
                // The history append above must precede the next classifier request.
                if call.name == "ask_user_questions" && finished.ok {
                    crate::perm::classifier::invalidate(st);
                }

                // The turn was abandoned. It unwinds only here, with the call closed
                // in the audit trail and answered in the history: a tool call left
                // without a result is a transcript no model can be asked to continue
                // from, and it was left exactly that way before.
                if abandoned {
                    crate::perm::classifier::invalidate(st);
                }
            }
        }

        // A steer queued while the tools were running joins the conversation
        // here: it becomes the next user turn, and the next call sees the tool
        // results and the steer together.
        if let Some(next) = take_steer_turn(rt, st)? {
            turn = next;
        }
    }
}

/// An MCP server that could not be reached is the user's to know about; the
/// model learns it from the call that needs the server, which fails saying so.
fn report_unavailable(rt: &Runtime, st: &SessionState, errors: &[serde_json::Value]) {
    for error in errors {
        super::log::warn(
            rt,
            st.id,
            "mcp",
            "mcp_unavailable",
            format!(
                "MCP server {} is unavailable: {}",
                error["server"].as_str().unwrap_or("?"),
                error["error"].as_str().unwrap_or("unknown error")
            ),
        );
    }
}

struct PlannedCall {
    id: String,
    name: String,
    args: serde_json::Value,
    refusal: Option<Error>,
}

/// Where the batch starting at `from` ends: past every consecutive call that
/// may run alongside the others ([`call::concurrent`]), or just past `from`
/// when it may not. Asked as each batch is about to run, so a grant given to
/// an earlier call already counts.
fn together(rt: &Runtime, st: &SessionState, calls: &[PlannedCall], from: usize) -> usize {
    let may = |call: &PlannedCall| {
        call.refusal.is_none() && call::concurrent(rt, st, &call.name, &call.args)
    };
    if !may(&calls[from]) {
        return from + 1;
    }
    calls[from + 1..]
        .iter()
        .position(|call| !may(call))
        .map_or(calls.len(), |offset| from + 1 + offset)
}

/// Run `batch` — calls [`together`] let in — all at the same time, and hand
/// back their results in the batch's order. A panic is one failed call, as it
/// is for a call on its own. There is no width of its own to configure (owner,
/// 2026-10-11): a batch is part of one response, so
/// `tools.max_actions_per_response` already bounds it.
fn run_together(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    batch: &[PlannedCall],
) -> Vec<Result<crate::tools::Outcome>> {
    let width = batch.len();
    let taken = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Result<crate::tools::Outcome>>>> =
        batch.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..width {
            scope.spawn(|| loop {
                let index = taken.fetch_add(1, Ordering::Relaxed);
                let Some(call) = batch.get(index) else {
                    break;
                };
                let result = panic::catch(&call.name, || {
                    call::run_alongside(rt, st, turn, &call.id, &call.name, &call.args)
                })
                .unwrap_or_else(Err);
                *panic::lock(&slots[index]) = Some(result);
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unwrap_or_else(|| Err(Error::Internal("the call never finished".into())))
        })
        .collect()
}

/// Enforce the server's response budget before any call can reach a tool:
/// each tool call consumes 1 operation towards the configured allowance.
/// Rejected descriptors stay in history and receive a failed tool result,
/// because silently dropping one leaves an invalid provider transcript.
fn plan_calls(rt: &Runtime, calls: Vec<(String, String, serde_json::Value)>) -> Vec<PlannedCall> {
    if calls.is_empty() {
        return Vec::new();
    }
    let max = rt.limits.max_tool_requests_per_response();
    let mut accepted = 0usize;

    calls
        .into_iter()
        .map(|(id, name, args)| {
            let requested = accepted.saturating_add(1);
            let refusal = if requested > max {
                Some(Error::invalid(format!(
                    "Not executed. This response requested {requested} tool operations; the Rune configuration allows at most {max}. Ask for the remaining work in the next response."
                )))
            } else {
                accepted = requested;
                None
            };
            PlannedCall { id, name, args, refusal }
        })
        .collect()
}

/// The conversation as the model will see it: the summary and the context a
/// compaction handed back, then every stored row after it in reading order.
///
/// Rebuilt every round, but out of immutable parts: stored rows and the
/// compaction record replay byte for byte, so each request extends the
/// previous one.
fn history(
    rt: &Runtime,
    st: &SessionState,
    settings: &crate::settings::ModelSettings,
    compaction: Option<&crate::store::jsonl::CompactionRecord>,
    transcript: &[crate::store::sessions::Message],
) -> Result<Vec<Message>> {
    let kept = compact::apply(
        transcript.iter().collect(),
        compaction.map(|c| c.through_id),
    );

    let mut out = Vec::new();
    if let Some(compaction) = compaction {
        let mut opening = crate::frame::block(
            "session_summary",
            &[("note", "the earlier conversation, compressed by Rune")],
            &compaction.summary,
        );
        if !compaction.context.is_empty() {
            opening.push_str("\n\n");
            opening.push_str(&compaction.context);
        }
        out.push(Message::user(opening));
    }
    // The one copy a round makes: of the rows that are sent, nothing else.
    let rows = super::context::reading_order(
        kept.into_iter()
            .filter(|m| m.role != Role::Error)
            .cloned()
            .collect(),
    );
    let first_row = out.len();
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    out.extend(rows.into_iter().map(|m| {
        Message {
            role: m.role,
            content: m.content,
            tool_call_id: m.tool_call_id,
            tool_name: m.tool_name,
            // A row written before this column existed simply has no calls; an
            // unreadable one is treated the same way rather than failing the turn,
            // because a corrupt old row must not make a session unusable forever.
            tool_calls: m
                .tool_calls
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default(),
            continuation: m
                .continuation
                .filter(|c| c.provider == settings.provider && c.model == settings.model),
            reasoning: m.reasoning,
            media: m.media,
        }
    }));
    settle_media(rt, st, settings, &mut out, first_row, &ids)?;
    ensure_unique_tool_call_ids(&mut out);
    close_unanswered_tool_calls(&mut out);
    Ok(out)
}

/// Seal every attachment a message is about to store (see `Attachment::seal`):
/// its hash and the text budget, nothing copied.
fn seal_media(rt: &Runtime, media: &mut [crate::media::Attachment]) {
    let text_chars = rt.limits.config.number("attachments.max_text_chars") as usize;
    for attachment in media {
        attachment.seal(text_chars);
    }
}

/// Point every history attachment at what was sent — its file while it still
/// is, a copy once it is not (`Attachment::settle`) — and apply the session's
/// media evictions, recording any new one this request needs to fit the
/// provider's payload budget. Evictions are permanent, so a message's
/// rendering changes at most once (see `media::evictions`).
fn settle_media(
    rt: &Runtime,
    st: &SessionState,
    settings: &crate::settings::ModelSettings,
    out: &mut [Message],
    first_row: usize,
    ids: &[i64],
) -> Result<()> {
    use crate::store::jsonl::MediaEviction;
    let (dir, evicted) = {
        let db = panic::lock(&rt.db);
        (
            db.jsonl.attachments_dir(st.id),
            db.jsonl.read_evictions(st.id)?,
        )
    };
    let evicted: std::collections::HashSet<MediaEviction> = evicted.into_iter().collect();
    let original = |sha: &str| st.checkpoints.original(sha);
    for (k, &message) in ids.iter().enumerate() {
        for (index, attachment) in out[first_row + k].media.iter_mut().enumerate() {
            attachment.settle(&dir, &st.attachment_prints, &original);
            attachment.evicted = evicted.contains(&MediaEviction { message, index });
        }
    }
    let Some(payload) = panic::lock(&rt.settings)
        .providers
        .get(&settings.provider)
        .map(|cfg| cfg.max_payload_bytes())
    else {
        return Ok(());
    };
    let budget = crate::media::provider_payload_budget(&crate::media::MediaLimits {
        max_payload_bytes: payload,
        ..Default::default()
    });
    let current = out
        .iter()
        .rposition(|m| m.role == Role::User || !m.media.is_empty());
    let max_files = rt.limits.config.number("attachments.max_history_files") as usize;
    for (i, index) in crate::media::evictions(out, current, budget, max_files) {
        let Some(&message) = i.checked_sub(first_row).and_then(|k| ids.get(k)) else {
            continue;
        };
        panic::lock(&rt.db)
            .jsonl
            .append_eviction(st.id, &MediaEviction { message, index })?;
        out[i].media[index].evicted = true;
    }
    Ok(())
}

fn emit(rt: &Runtime, event: Event) {
    // A frontend that has gone away is not an error worth failing a turn over.
    let _ = rt.events.send(event);
}

/// Ensure every tool call ID in the conversation history is globally unique.
///
/// Third-party gateways (such as 9Router, Google Antigravity, or other proxies)
/// often generate pseudo-random 6-digit IDs or reuse sequence counters. In long
/// sessions with hundreds of tool calls, collisions occur with high mathematical
/// probability (Birthday Paradox). When multiple turns use identical call IDs,
/// strict providers (like Google Gemini / Antigravity) reject the request with HTTP 400.
///
/// This universal pass runs on `messages` across ALL providers, guaranteeing 0% collision
/// by disambiguating repeated call IDs and updating the corresponding `Role::Tool` messages.
pub fn ensure_unique_tool_call_ids(messages: &mut [Message]) {
    let mut seen_ids = std::collections::HashSet::new();
    let mut active_remaps = std::collections::HashMap::new();

    for msg in messages {
        match msg.role {
            Role::Assistant => {
                active_remaps.clear();
                for call in &mut msg.tool_calls {
                    if let Some(unique_id) = unique_id(&call.id, &mut seen_ids) {
                        let old_id = std::mem::replace(&mut call.id, unique_id.clone());
                        remap_continuation(msg.continuation.as_mut(), &old_id, &unique_id);
                        active_remaps.insert(old_id, unique_id);
                    }
                }
            }
            Role::Tool => {
                if let Some(tool_call_id) = &mut msg.tool_call_id {
                    if let Some(new_id) = active_remaps.get(tool_call_id) {
                        *tool_call_id = new_id.clone();
                    }
                }
            }
            _ => {}
        }
    }
}

/// Claim `id` in `taken`. `None` when it was free; otherwise the first free
/// spelling of it (`id_2`, `id_3`, …), which is then claimed instead. One rule
/// for the newest response and for the whole history alike (audit D2).
fn unique_id(id: &str, taken: &mut std::collections::HashSet<String>) -> Option<String> {
    if taken.insert(id.to_string()) {
        return None;
    }
    let unique = (2..)
        .map(|n| format!("{id}_{n}"))
        .find(|candidate| !taken.contains(candidate))?;
    taken.insert(unique.clone());
    Some(unique)
}

/// Point a provider continuation at a call's new id.
fn remap_continuation(
    continuation: Option<&mut crate::provider::Continuation>,
    old: &str,
    new: &str,
) {
    let Some(tool_ids) = continuation
        .and_then(|c| c.data.get_mut("tool_ids"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for value in tool_ids.values_mut() {
        if value.as_str() == Some(old) {
            *value = serde_json::json!(new);
        }
    }
}

/// Give every tool call a result, so the transcript is one a provider will accept.
///
/// A turn can end with an assistant message that asked for tools and no
/// `Role::Tool` row answering them: the process died mid-call, or a session was
/// resumed from a transcript written before the call was settled. Every
/// OpenAI-compatible provider rejects that shape outright, and because the
/// gateway has already sent `200 OK` and opened the stream by then, the refusal
/// cannot be reported as a status code — it arrives as an error frame inside the
/// SSE body, mid-stream, where the retry gate never sees it. Every later turn
/// replays the same broken history and fails identically, so one unanswered
/// call costs the whole session.
///
/// Closing them here is what makes a damaged session usable again without
/// editing anyone's transcript. The wording is deliberately explicit that no
/// result is being invented: the model is told the call never ran, which is the
/// truth, and is free to ask again. Dropping the `tool_calls` instead would
/// hide a gap in the record and leave the reasoning that produced it unanchored.
fn close_unanswered_tool_calls(messages: &mut Vec<Message>) {
    let mut answered = std::collections::HashSet::new();
    for m in messages.iter() {
        if m.role == Role::Tool {
            if let Some(id) = &m.tool_call_id {
                answered.insert(id.clone());
            }
        }
    }

    // A synthetic result has to land where the provider expects it: directly
    // after the assistant message that asked, and in the order the calls were
    // made. Appending it at the end of the vector would put it ahead of results
    // already recorded for the same turn, and a `tool` message that precedes
    // the `tool_calls` it answers is as invalid as one that is missing.
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut deferred: Vec<Message> = Vec::new();
    for m in std::mem::take(messages) {
        if !deferred.is_empty() && m.role != Role::Tool {
            out.append(&mut deferred);
        }
        let missing: Vec<CallRef> = m
            .tool_calls
            .iter()
            .filter(|c| !answered.contains(&c.id))
            .cloned()
            .collect();
        if missing.is_empty() || m.role != Role::Assistant {
            out.push(m);
            continue;
        }
        // The reply's own text is kept: the model reasoned its way to these
        // calls, and that reasoning is still the truth about what it wanted.
        out.push(m);
        for call in missing {
            deferred.push(Message::tool_result(
                call.id.clone(),
                call.name.clone(),
                format!(
                    "This tool call has no result. Rune ended the turn before the tool \
                     reported one, so its outcome is unknown and nothing above this line \
                     should be read as its output. Call {} again if you still need it.",
                    call.name
                ),
            ));
        }
    }
    out.append(&mut deferred);
    *messages = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_ids_without_collisions_remain_unchanged() {
        let mut msgs = vec![
            Message::user("do something"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "call_123".into(),
                    name: "terminal_use".into(),
                    args: "{}".into(),
                }],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "output 1".into(),
                tool_call_id: Some("call_123".into()),
                tool_name: Some("terminal_use".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message::user("do step 2"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "call_456".into(),
                    name: "inspect_file".into(),
                    args: "{}".into(),
                }],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "output 2".into(),
                tool_call_id: Some("call_456".into()),
                tool_name: Some("inspect_file".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
        ];

        ensure_unique_tool_call_ids(&mut msgs);

        assert_eq!(msgs[1].tool_calls[0].id, "call_123");
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(msgs[4].tool_calls[0].id, "call_456");
        assert_eq!(msgs[5].tool_call_id.as_deref(), Some("call_456"));
    }

    #[test]
    fn tool_call_ids_with_collision_across_turns_are_disambiguated() {
        let cont_turn35 = crate::provider::Continuation {
            provider: "9router".into(),
            model: "ag/gemini-3.8-flash-high".into(),
            data: serde_json::json!({
                "tool_ids": { "0": "call_175868" }
            }),
        };

        let mut msgs = vec![
            Message::user("turn 3 request"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "call_175868".into(),
                    name: "track_tasks".into(),
                    args: "{}".into(),
                }],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "task recorded".into(),
                tool_call_id: Some("call_175868".into()),
                tool_name: Some("track_tasks".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message::user("turn 35 request"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "call_175868".into(),
                    name: "patch_file".into(),
                    args: "{}".into(),
                }],
                continuation: Some(cont_turn35),
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "patched file".into(),
                tool_call_id: Some("call_175868".into()),
                tool_name: Some("patch_file".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
        ];

        ensure_unique_tool_call_ids(&mut msgs);

        // Turn 3 retains original ID
        assert_eq!(msgs[1].tool_calls[0].id, "call_175868");
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_175868"));

        // Turn 35 has disambiguated ID matching between assistant and tool
        assert_eq!(msgs[4].tool_calls[0].id, "call_175868_2");
        assert_eq!(msgs[5].tool_call_id.as_deref(), Some("call_175868_2"));

        // Continuation tool_ids was also updated
        let updated_cont = msgs[4].continuation.as_ref().unwrap();
        assert_eq!(
            updated_cont.data["tool_ids"]["0"],
            serde_json::json!("call_175868_2")
        );
    }

    #[test]
    fn tool_call_ids_triple_collision_and_multiple_calls_per_turn() {
        let mut msgs = vec![
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![
                    CallRef {
                        id: "dup_call".into(),
                        name: "a".into(),
                        args: "{}".into(),
                    },
                    CallRef {
                        id: "other_call".into(),
                        name: "b".into(),
                        args: "{}".into(),
                    },
                ],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "res1".into(),
                tool_call_id: Some("dup_call".into()),
                tool_name: Some("a".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "res2".into(),
                tool_call_id: Some("other_call".into()),
                tool_name: Some("b".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "dup_call".into(),
                    name: "a".into(),
                    args: "{}".into(),
                }],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "res3".into(),
                tool_call_id: Some("dup_call".into()),
                tool_name: Some("a".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: vec![CallRef {
                    id: "dup_call".into(),
                    name: "a".into(),
                    args: "{}".into(),
                }],
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "res4".into(),
                tool_call_id: Some("dup_call".into()),
                tool_name: Some("a".into()),
                tool_calls: Vec::new(),
                continuation: None,
                reasoning: None,
                media: Vec::new(),
            },
        ];

        ensure_unique_tool_call_ids(&mut msgs);

        // Turn 1
        assert_eq!(msgs[0].tool_calls[0].id, "dup_call");
        assert_eq!(msgs[0].tool_calls[1].id, "other_call");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("dup_call"));
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("other_call"));

        // Turn 2
        assert_eq!(msgs[3].tool_calls[0].id, "dup_call_2");
        assert_eq!(msgs[4].tool_call_id.as_deref(), Some("dup_call_2"));

        // Turn 3
        assert_eq!(msgs[5].tool_calls[0].id, "dup_call_3");
        assert_eq!(msgs[6].tool_call_id.as_deref(), Some("dup_call_3"));
    }

    fn assistant_with_call(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![CallRef {
                id: id.into(),
                name: name.into(),
                args: "{}".into(),
            }],
            continuation: None,
            reasoning: None,
            media: Vec::new(),
        }
    }

    /// The shape that killed session 362: an assistant message holding a tool
    /// call, and then a user turn with no `Role::Tool` row in between. Every
    /// provider rejects it, and the rejection lands mid-stream where it cannot
    /// be retried — so the guard has to fire before the request is built.
    #[test]
    fn an_unanswered_tool_call_is_given_a_result_before_the_request_is_built() {
        let mut msgs = vec![
            Message::user("run it"),
            assistant_with_call("ca82dd28", "manage_process"),
            Message::user("Continue."),
        ];

        close_unanswered_tool_calls(&mut msgs);

        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].tool_calls[0].id, "ca82dd28");
        let answer = &msgs[2];
        assert_eq!(answer.role, Role::Tool);
        assert_eq!(answer.tool_call_id.as_deref(), Some("ca82dd28"));
        assert_eq!(answer.tool_name.as_deref(), Some("manage_process"));
        assert!(
            answer.content.contains("no result"),
            "the model must be told the outcome is unknown, not handed a fake one: {}",
            answer.content
        );
    }

    /// A call that was answered is left exactly as it was — the guard repairs
    /// gaps, it does not rewrite history the engine already settled.
    #[test]
    fn an_answered_tool_call_is_left_alone() {
        let mut msgs = vec![
            Message::user("run it"),
            assistant_with_call("call_1", "terminal_use"),
            Message::tool_result("call_1", "terminal_use", "done"),
        ];

        close_unanswered_tool_calls(&mut msgs);

        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].content, "done");
    }

    /// One answered and one not: only the unanswered one is closed, so a
    /// partial turn is repaired without touching the result already recorded.
    #[test]
    fn only_the_unanswered_call_of_several_is_closed() {
        let mut assistant = assistant_with_call("call_1", "terminal_use");
        assistant.tool_calls.push(CallRef {
            id: "call_2".into(),
            name: "manage_process".into(),
            args: "{}".into(),
        });
        let mut msgs = vec![
            Message::user("run both"),
            assistant,
            Message::tool_result("call_1", "terminal_use", "first done"),
        ];

        close_unanswered_tool_calls(&mut msgs);

        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[2].content, "first done");
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_2"));
    }
}
