//! Running one tool call — the only place it happens.
//!
//! A call chosen by the model (`turn.rs`) and a call chosen by the frontend
//! (`tool.invoke`) differ in who wrote the arguments and in nothing else: same
//! jail, same permission gate, same audit row, same checkpoint, same event.
//! There used to be two copies of this sequence, and they had already drifted —
//! the direct path reported its result in a shape the turn path never used, it
//! never announced the checkpoint a write had just created, and `cancel` with a
//! call id could not touch it. One home is what stops that happening again.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

use super::{Runtime, SessionState};
use crate::error::{Error, Result, Supervision};
use crate::panic;
use crate::perm::{reach, Allowed, Ask};
use crate::proto::Event;
use crate::tools::{Outcome, ToolCtx};

/// A finished call, in the parts everyone downstream needs: the frontend's
/// event, the audit row, and the message the model is given.
///
/// One conversion, so a failure cannot mean different things in different
/// places. `content` is what the model reads; for a call that did not run or did
/// not finish that is one sentence in a `<tool_notice>`, a different one for
/// every kind, because each asks something different of the model. `status` is
/// the kind itself, as a label for a frontend (the colour of a bullet): it goes
/// to the audit row and the event, and is never part of what the model reads.
pub struct Finished {
    pub media: Vec<crate::media::Attachment>,
    pub ok: bool,
    pub summary: String,
    pub content: String,
    pub detail: Option<Value>,
    pub status: Option<String>,
    /// Why it failed, in a few words, for the sentence that says the same call
    /// failed again.
    pub reason: String,
}

impl Finished {
    pub fn of(result: Result<Outcome>) -> Finished {
        use crate::tools::notice;
        match result {
            Ok(o) => Finished {
                status: o
                    .detail
                    .as_ref()
                    .and_then(|d| d["status"].as_str())
                    .map(str::to_string),
                reason: o.summary.clone(),
                media: o.media,
                ok: o.ok,
                summary: o.summary,
                content: o.content,
                detail: o.detail,
            },
            Err(e) => {
                let text = e.to_string();
                // The reason without the `denied: ` the error's text leads with.
                let clean_reason = match &e {
                    Error::Denied(reason) => crate::net::redact::scrub(reason).into_owned(),
                    Error::Classifier(supervision) => {
                        crate::net::redact::scrub(&supervision.reason()).into_owned()
                    }
                    _ => text.clone(),
                };
                let clean_reason = clean_reason.as_str();
                let status = match &e {
                    Error::Classifier(Supervision::Refused(_)) | Error::Denied(_) => {
                        notice::status::DENIED
                    }
                    Error::Classifier(Supervision::Undecided { .. }) => {
                        notice::status::SERVICE_UNAVAILABLE
                    }
                    Error::Cancelled => notice::status::CANCELLED,
                    Error::Invalid(_) | Error::NotFound(_) | Error::Jail(_) => {
                        notice::status::NOT_EXECUTED
                    }
                    Error::Provider(_)
                    | Error::Unreachable(_)
                    | Error::Stalled(_)
                    | Error::Rejected { .. }
                    | Error::OutputLimit { .. } => notice::status::SERVICE_UNAVAILABLE,
                    _ => notice::status::FAILED,
                };
                Finished {
                    media: Vec::new(),
                    ok: false,
                    summary: text.clone(),
                    content: notice::failure(status, clean_reason),
                    reason: notice::reason(clean_reason),
                    status: Some(status.to_string()),
                    detail: Some(serde_json::json!({"status": status, "code": e.code()})),
                }
            }
        }
    }

    /// A call the user stopped. Distinct from a failure on purpose: "you stopped
    /// this" and "this went wrong" are different things to tell a model, and the
    /// audit trail is read by someone asking which of the two happened.
    pub fn stopped() -> Finished {
        use crate::tools::notice;
        let text = "stopped by the user".to_string();
        Finished {
            media: Vec::new(),
            ok: false,
            summary: text.clone(),
            content: notice::failure(notice::status::CANCELLED, ""),
            reason: text,
            status: Some(notice::status::CANCELLED.to_string()),
            detail: Some(serde_json::json!({"status":"cancelled","code":"cancelled"})),
        }
    }
}

/// Announce, authorise, and run one tool call.
///
/// Emits `tool_call` before asking for permission — a frontend must be able to
/// show what it is being asked about — and `checkpoint` afterwards if the call
/// captured anything to undo.
pub fn run(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    id: &str,
    name: &str,
    args: &Value,
    response_refusal: Option<Error>,
) -> Result<Outcome> {
    run_as(
        rt,
        st,
        turn,
        id,
        name,
        args,
        response_refusal,
        Company::Alone,
        crate::tools::Reader::Model,
    )
}

/// [`run`], for a call a frontend makes directly (`tool.invoke`): the same
/// sequence and the same gate, but its result is the person's alone — it
/// joins no history the model reads, so it tells the model nothing.
pub fn run_direct(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    id: &str,
    name: &str,
    args: &Value,
) -> Result<Outcome> {
    run_as(
        rt,
        st,
        turn,
        id,
        name,
        args,
        None,
        Company::Alone,
        crate::tools::Reader::Person,
    )
}

/// [`run`], for a call running at the same time as others of its response —
/// one [`concurrent`] let in. The same sequence, except that the gate is never
/// allowed to ask: see [`crate::perm::Gate::authorize_silently`].
pub fn run_alongside(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    id: &str,
    name: &str,
    args: &Value,
) -> Result<Outcome> {
    run_as(
        rt,
        st,
        turn,
        id,
        name,
        args,
        None,
        Company::Alongside,
        crate::tools::Reader::Model,
    )
}

/// Whether a call runs on its own or alongside other calls of its response.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Company {
    Alone,
    Alongside,
}

/// Human authorization replaces auxiliary policy review for this exact call.
/// The same resolved target and Gate facts are read again when it executes;
/// a UI card or model-authored claim never supplies this routing decision.
pub(crate) fn asks_person(st: &SessionState, name: &str, args: &Value) -> Result<bool> {
    let called = st.registry.get(name)?;
    called.validate(args)?;
    let (tool, target, target_args) = st.registry.target(name, args)?;
    tool.validate(&target_args)?;
    let ask = question(
        st,
        &tool,
        &target,
        crate::tools::action_of(args),
        &target_args,
    )?;
    Ok(matches!(
        st.gate.verdict(st.mode(), &ask)?,
        crate::perm::policy::Verdict::Ask { .. }
    ))
}

/// Whether this call may run at the same time as others of its response
/// (owner, 2026-10-06): a tool that only reads ([`crate::tools::Tool::concurrent`]),
/// called as itself rather than through the MCP proxy, not judged by the
/// classifier, and allowed by the gate without a question. Decided by the
/// same functions its run goes through, without changing anything; the run
/// still passes the gate itself. Any doubt — an error on the way — runs it on
/// its own, where it fails or asks exactly as it always did.
pub fn concurrent(rt: &Runtime, st: &SessionState, name: &str, args: &Value) -> bool {
    let Ok(tool) = st.registry.get(name) else {
        return false;
    };
    if !tool.concurrent() || tool.validate(args).is_err() {
        return false;
    }
    if !matches!(crate::perm::classifier::judges(rt, st, name), Ok(false)) {
        return false;
    }
    let action = crate::tools::action_of(args);
    let Ok(ask) = question(st, &tool, name, action, args) else {
        return false;
    };
    matches!(
        st.gate.verdict(st.mode(), &ask),
        Ok(crate::perm::policy::Verdict::Allow)
    )
}

#[allow(clippy::too_many_arguments)]
fn run_as(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    id: &str,
    name: &str,
    args: &Value,
    response_refusal: Option<Error>,
    company: Company,
    reader: crate::tools::Reader,
) -> Result<Outcome> {
    let called = st.registry.get(name);
    let action = crate::tools::action_of(args).to_string();

    // Registered *before* the call is announced. A frontend cancels the moment
    // it sees `tool_call`, and a call that is not registered yet cannot be
    // found by id — the cancel would be answered "nothing" and the command
    // would run to completion. The same race, one event earlier, as the `done`
    // that used to be emitted before the session was released.
    let running = Running::new(st, id);
    let cancel = running.flag();

    emit(
        rt,
        Event::ToolCall {
            session: st.id,
            id: id.to_string(),
            name: name.to_string(),
            action: action.clone(),
            args: args.clone(),
            at: crate::store::now_ms(),
        },
    );

    {
        let db = panic::lock(&rt.db);
        let audit = if called
            .as_ref()
            .is_ok_and(|tool| tool.is_external() || tool.name() == crate::tools::call_mcp::NAME)
        {
            serde_json::json!({"external":true,"argument_fields":args.as_object().map(|m|m.keys().collect::<Vec<_>>())})
        } else {
            args.clone()
        };
        db.begin_toolcall(
            id,
            st.id,
            turn,
            name,
            &crate::net::redact::scrub(&audit.to_string()),
        )?;
    }

    if let Some(error) = response_refusal {
        // A preflight result is not allowed to pre-empt a now-human route.
        if !matches!(&error, Error::Classifier(_)) || !asks_person(st, name, args)? {
            return Err(error);
        }
    }
    let called = called?;
    if args.to_string().len() > crate::limits::MAX_TOOL_ARG_BYTES {
        return Err(Error::invalid("tool arguments exceed configured limit"));
    }
    called.validate(args)?;
    // The tool this call runs as — the MCP tool a `call_mcp_tool` call names,
    // with the arguments it carries — is what the gate, the classifier and the
    // tool itself see from here on. The frontend was told the call as made.
    let (tool, target, target_args) = st.registry.target(name, args)?;
    if target != name {
        tool.validate(&target_args)?;
    }
    // The classifier knows the call as the model made it (its name is what a
    // user excludes, its arguments are what a decision was taken on).
    let made = (name, args);
    let (name, args) = (target.as_str(), &target_args);

    if cancel.load(std::sync::atomic::Ordering::Relaxed)
        || st.cancel.load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err(Error::Cancelled);
    }
    if asks_person(st, made.0, made.1)? {
        crate::perm::classifier::forget(st, id);
    }
    execute(
        rt, st, turn, id, &tool, name, args, made, &cancel, company, reader,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute(
    rt: &Runtime,
    st: &SessionState,
    turn: i64,
    id: &str,
    tool: &Arc<dyn crate::tools::Tool>,
    name: &str,
    args: &Value,
    made: (&str, &Value),
    cancel: &Arc<AtomicBool>,
    company: Company,
    reader: crate::tools::Reader,
) -> Result<Outcome> {
    if company == Company::Alongside && crate::perm::classifier::judges(rt, st, made.0)? {
        return Err(Error::Denied(
            "this parallel call now requires policy review; submit it again on its own".into(),
        ));
    }
    let action = crate::tools::action_of(args);
    let tx = rt.events.clone();
    let mut emit_fn = move |e: Event| {
        let _ = tx.send(e);
    };

    if tool.runs_commands() {
        st.capabilities.validate_active_packages()?;
    }
    let ask = question(st, tool, name, action, args)?;
    let mut allowed = match company {
        Company::Alone => st.gate.authorize(st.mode(), ask, &mut emit_fn, cancel)?,
        Company::Alongside => st.gate.authorize_silently(st.mode(), &ask)?,
    };

    // A mode/grant may change while routing. No actual person answer means
    // this call must still be reviewed now; a real answer replaces review.
    if !allowed.person && company == Company::Alone {
        let reviewed = crate::perm::classifier::take(rt, st, turn, id, made.0, made.1);
        if reviewed.is_err() && asks_person(st, made.0, made.1)? {
            let ask = question(st, tool, name, action, args)?;
            allowed = st.gate.authorize(st.mode(), ask, &mut emit_fn, cancel)?;
            if !allowed.person {
                return Err(Error::Denied(
                    "authorization changed during failed review; submit the call again".into(),
                ));
            }
        } else if let Some(ticket) = reviewed? {
            if let Some(refused) = ticket.decision {
                if asks_person(st, made.0, made.1)? {
                    let ask = question(st, tool, name, action, args)?;
                    allowed = st.gate.authorize(st.mode(), ask, &mut emit_fn, cancel)?;
                    if !allowed.person {
                        return Err(Error::Classifier(refused));
                    }
                } else {
                    return Err(Error::Classifier(refused));
                }
            }
        }
        if !allowed.person {
            let ask = question(st, tool, name, action, args)?;
            if matches!(
                st.gate.verdict(st.mode(), &ask)?,
                crate::perm::policy::Verdict::Ask { .. }
            ) {
                allowed = st.gate.authorize(st.mode(), ask, &mut emit_fn, cancel)?;
                if !allowed.person {
                    return Err(Error::Denied(
                        "authorization changed while the action was reviewed; submit it again"
                            .into(),
                    ));
                }
            }
        }
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed)
        || st.cancel.load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err(Error::Cancelled);
    }
    // Applied before the jail and the sandbox are read below, and taken back by
    // the guard when the answer covered this call only.
    let _widened = Widened::apply(st, allowed);

    let before = checkpoint_size(rt, st, turn);

    let jail = st.jail();
    let sandbox = st.sandbox();
    let log = |level, code: &str, message: String, detail| {
        super::log::record(rt, st.id, level, name, code, message, detail)
    };
    let mut ctx = ToolCtx {
        capabilities: &st.capabilities,
        registry: &st.registry,
        questions: &st.questions,
        limits: &rt.limits,
        session: st.id,
        turn,
        call_id: id,
        jail: &jail,
        memory: &st.memory,
        checkpoints: &st.checkpoints,
        sandbox: &sandbox,
        cancel,
        input: &st.input,
        tasks: &st.tasks,
        results: &st.results,
        term_size: st.term_size(),
        background: &st.background,
        reader,
        emit: &mut emit_fn,
        log: &log,
    };

    let writes =
        (tool.mutates() && !tool.runs_commands() && !tool.is_external()).then(|| st.files.write());
    // A tool is ordinary code and can have an ordinary bug. Catching it here
    // costs this one call; letting it unwind costs the session (see `panic`).
    let outcome = panic::catch(name, || tool.run(&mut ctx, args)).unwrap_or_else(Err);
    let changed = tool.mutates() || tool.runs_commands();
    drop(writes);
    if changed {
        st.files.settled(rt, st);
    }
    // Deliberately after a failure too: a tool that wrote one file and then
    // failed on the second still left something to undo, and the frontend has
    // to be told the undo exists.
    let after = checkpoint_size(rt, st, turn);
    if after > before {
        emit(
            rt,
            Event::Checkpoint {
                session: st.id,
                turn,
                files: after,
            },
        );
    }

    outcome
}

/// What the gate is asked about this call: its risk, and what it names outside
/// the workspace that nothing grants yet.
fn question<'a>(
    st: &SessionState,
    tool: &Arc<dyn crate::tools::Tool>,
    name: &'a str,
    action: &'a str,
    args: &Value,
) -> Result<Ask<'a>> {
    let risk = tool.risk(args);
    let named = tool.reaches(args);
    let local_services = tool.runs_commands()
        && st.mode().confines()
        && !st.sandbox().local_services()
        && (tool.local_services(args) || names_a_socket(&named));
    let reaches = named
        .into_iter()
        .filter(|p| {
            (tool.mutates() && !tool.runs_commands()) || st.capabilities.package_root(p).is_none()
        })
        .collect();
    let (wanted, blocked) = outside_paths(st, reaches, tool.runs_commands());

    // What is left in `blocked` is not a policy verdict — it is a token that
    // could not be resolved to a path at all (a `~` on a process with no
    // HOME), and that goes back to the model as an ordinary tool error. No
    // path is refused for where it is: the owner's rule, 2026-09-28, is that
    // the system never refuses on its own. Only for an otherwise ordinary
    // call, though: anything the deny-list has an opinion on goes to the gate
    // first and keeps the gate's question.
    if matches!(risk, crate::perm::denylist::Risk::Normal) {
        if let Some(why) = blocked.into_iter().next() {
            return Err(why);
        }
    }
    Ok(Ask {
        session: st.id,
        tool: name,
        action,
        preview: tool.preview(args),
        grant_key: tool.grant_key(args),
        mutates: tool.mutates(),
        runs: tool.runs_commands(),
        risk,
        reach: wanted,
        local_services,
        unconfined: {
            let sandbox = st.sandbox();
            (!sandbox.enforced()).then(|| {
                sandbox
                    .report()
                    .reason
                    .clone()
                    .unwrap_or_else(|| "the kernel sandbox is not enforcing".into())
            })
        },
    })
}

/// Close the audit row and tell the frontend.
///
/// Every call reaches this, including one that was stopped: a row opened by
/// [`run`] and never closed is exactly the hole that made the audit trail
/// unreadable at the point a frontend's "run this" button sat. Even an unknown
/// tool is announced and opened before lookup fails, so its name, turn, status
/// and code survive replay. An audit write failure is returned, never hidden.
///
/// The event is emitted whatever the audit write did: a frontend that never
/// hears how a call ended is stuck, and returning the error still lets the
/// caller stop over a database that has begun failing.
pub fn report(rt: &Runtime, st: &SessionState, id: &str, f: &Finished) -> Result<()> {
    let audit = {
        let db = panic::lock(&rt.db);
        db.end_toolcall(
            st.id,
            id,
            f.ok,
            f.status.as_deref(),
            f.detail.as_ref().and_then(|d| d["code"].as_str()),
            &f.summary,
        )
    };
    emit(
        rt,
        Event::ToolResult {
            session: st.id,
            id: id.to_string(),
            ok: f.ok,
            summary: f.summary.clone(),
            content: f.content.clone(),
            detail: f.detail.clone(),
            at: crate::store::now_ms(),
        },
    );
    audit
}

/// Turn the paths a call named into the ones the user has to decide about.
///
/// Two filters, and both matter. A path already inside the jail is not leaving
/// the workspace. A path the sandbox already grants — `/usr/bin/python3`, every
/// system library a build opens — is not a decision either, and prompting for it
/// is how a person is trained to press `y` without reading.
///
/// Returns what the user can be asked about, and separately the tokens that
/// could not be resolved to a path at all — an input error, never a policy
/// verdict. A path the machine runs on its own is *not* here as a rejection:
/// the owner's rule, 2026-09-28, is that the system never refuses on its own,
/// so it rides in `wanted` flagged and the wizard decides.
///
/// What already reaches a path depends on who opens it: a command is held by
/// the kernel sandbox, so what the sandbox grants needs no question; a file
/// tool is held by the jail alone, so a path outside it is asked about even
/// where every command could open it (a system file), and even where no
/// sandbox holds anything — refusing it unasked broke the owner's rule that
/// the system never refuses on its own (CI, 2026-10-17).
fn outside_paths(
    st: &SessionState,
    tokens: Vec<String>,
    commands: bool,
) -> (Vec<reach::Candidate>, Vec<Error>) {
    // Full-access mode confines nothing, so there is nothing to widen and no
    // token that could be refused for naming somewhere unreachable.
    if tokens.is_empty() || !st.mode().confines() {
        return (Vec::new(), Vec::new());
    }
    let jail = st.jail();
    let sandbox = st.sandbox();
    let home = crate::platform::home_dir();

    let mut wanted: Vec<reach::Candidate> = Vec::new();
    let mut blocked: Vec<Error> = Vec::new();
    for token in tokens {
        // Resolved, then tested for reachability, then judged — in that order.
        // Judging first would refuse `/usr/bin/python3` for sitting in a
        // protected prefix, when every command on the machine already reads it.
        let named = match reach::resolve(&token, home.as_deref()) {
            Ok(named) => named,
            Err(why) => {
                blocked.push(why);
                continue;
            }
        };
        if jail.contains(&named) || (commands && sandbox.reachable(&named)) {
            continue;
        }
        // Never an error: where a rule for it can be anchored, or a one-call
        // full access when none can.
        let candidate = reach::candidate(named, home.as_deref());
        if !wanted.contains(&candidate) {
            wanted.push(candidate);
        }
    }
    (wanted, blocked)
}

/// Whether a local MCP server may start with the folders its profile names
/// (owner, 2026-10-17): those outside the workspace are put to the person
/// as any path there is — in Ask and Auto a card naming them, "always" on
/// offer and filed as this project's path grants; nothing asked in Full. Its
/// confinement stays the profile the person wrote, whatever the mode.
pub(crate) fn authorize_mcp_roots(
    rt: &Runtime,
    st: &SessionState,
    server: &str,
    roots: &[PathBuf],
) -> Result<()> {
    let outside = st.outside_roots(roots);
    if outside.is_empty() {
        return Ok(());
    }
    let refused = Error::Denied(format!(
        "the person did not allow MCP server {server} its folders outside the workspace"
    ));
    let key = (server.to_string(), outside.clone());
    if panic::lock(&st.refused_servers).contains(&key) {
        return Err(refused);
    }
    let home = crate::platform::home_dir();
    let mut wanted: Vec<reach::Candidate> = Vec::new();
    for root in outside {
        let candidate = reach::candidate(root, home.as_deref());
        if !wanted.contains(&candidate) {
            wanted.push(candidate);
        }
    }
    let action = format!("start the MCP server {server}");
    let preview = format!(
        "MCP server {server} reads or writes:\n{}",
        wanted
            .iter()
            .map(|c| c.named.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let ask = Ask {
        session: st.id,
        tool: "mcp_server",
        action: &action,
        preview,
        grant_key: server.to_string(),
        mutates: true,
        runs: false,
        risk: crate::perm::denylist::Risk::Normal,
        reach: wanted,
        local_services: false,
        unconfined: None,
    };
    let mut emit_fn = |event: Event| emit(rt, event);
    match st.gate.authorize(st.mode(), ask, &mut emit_fn, &st.cancel) {
        Ok(_) => Ok(()),
        // Refused (or unanswered): asked once a session for these folders.
        Err(Error::Denied(_)) => {
            panic::lock(&st.refused_servers).push(key);
            Err(refused)
        }
        Err(other) => Err(other),
    }
}

/// Whether a path the call names is a Unix socket on disk — a door to a
/// program running outside the workspace, wherever the socket file is.
fn names_a_socket(tokens: &[String]) -> bool {
    let home = crate::platform::home_dir();
    tokens.iter().any(|token| {
        reach::resolve(token, home.as_deref()).is_ok_and(|path| {
            crate::platform::canonical(&path).is_ok_and(|real| crate::platform::is_socket(&real))
        })
    })
}

/// Holds a widening for the life of one call, and takes back exactly what it
/// added when the answer was "this once".
///
/// A guard rather than two statements because the call between them can unwind:
/// a panicking tool would otherwise leave the session permanently able to reach
/// a directory the user allowed for one command.
struct Widened<'a> {
    st: &'a SessionState,
    added: Vec<PathBuf>,
    /// This call was allowed with full access: confinement is lifted for it
    /// alone, and the person's mode is never touched ([`SessionState::lift`]).
    lifted: bool,
    /// This call was allowed local services for this once
    /// ([`SessionState::open_services`]).
    services: bool,
}

impl<'a> Widened<'a> {
    fn apply(st: &'a SessionState, allowed: Allowed) -> Widened<'a> {
        let mut widened = Widened {
            st,
            added: Vec::new(),
            lifted: false,
            services: false,
        };
        if allowed.full_access {
            st.lift();
            widened.lifted = true;
            return widened;
        }
        if allowed.local_services {
            if allowed.keep {
                // Filed by the gate; held from now on, as the next turn's
                // reading of the record would.
                st.reseed_services(true);
            } else {
                st.open_services();
                widened.services = true;
            }
        }
        if !allowed.reach.is_empty() {
            let added = st.widen(&allowed.reach);
            // A grant that outlives the call is not this guard's business.
            if !allowed.keep {
                widened.added = added;
            }
        }
        widened
    }
}

impl Drop for Widened<'_> {
    fn drop(&mut self) {
        if self.lifted {
            self.st.unlift();
        }
        if self.services {
            self.st.close_services();
        }
        self.st.narrow(&self.added);
    }
}

/// Registers the call as the one in flight, and deregisters it however the call
/// ends — including an unwind, which is why this is a guard and not two lines.
struct Running<'a> {
    st: &'a SessionState,
    id: String,
    flag: Arc<AtomicBool>,
}

impl<'a> Running<'a> {
    fn new(st: &'a SessionState, id: &str) -> Running<'a> {
        Running {
            flag: st.begin_call(id),
            id: id.to_string(),
            st,
        }
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.st.end_call(&self.id);
    }
}

/// How many files this turn's checkpoint holds. Used to notice that a write tool
/// captured something, so the frontend can offer an undo the moment one exists.
fn checkpoint_size(rt: &Runtime, st: &SessionState, turn: i64) -> usize {
    let db = panic::lock(&rt.db);
    db.checkpoint(st.id, turn)
        .ok()
        .flatten()
        .map(|c| c.files.len())
        .unwrap_or(0)
}

fn emit(rt: &Runtime, event: Event) {
    // A frontend that has gone away is not an error worth failing a turn over.
    let _ = rt.events.send(event);
}

/// The turn loop's reading of a stopped call: only a cancellation aimed at the
/// whole turn ends it. One call stopped on its own is reported and the loop
/// carries on, which is the entire point of being able to kill one runaway
/// command without losing the conversation.
pub fn turn_was_cancelled(st: &SessionState, e: &Error) -> bool {
    matches!(e, Error::Cancelled) && st.cancel.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// What a failure is told and recorded as follows from what failed, not
    /// from words in its message: an error about a file named `classifier_…`
    /// is not the classifier's, and the classifier's two ways of failing keep
    /// their own status and their own code.
    #[test]
    fn a_failure_is_classified_by_what_failed_not_by_its_wording() {
        let missing = Finished::of(Err(Error::NotFound(
            "classifier_notes.txt — check the path or search with find_files".into(),
        )));
        assert_eq!(missing.status.as_deref(), Some("not_executed"));
        assert_eq!(missing.detail.unwrap()["code"], "not_found");

        let refused = Finished::of(Err(Error::Classifier(Supervision::Refused(
            "writes outside the policy".into(),
        ))));
        assert_eq!(refused.status.as_deref(), Some("denied"));
        assert_eq!(refused.detail.unwrap()["code"], "classifier_denied");
        assert_eq!(
            refused.content,
            "<tool_notice>This call was denied (Rejected by classifier: writes outside the policy). Do not send the same call again; choose another approach or ask the user.</tool_notice>"
        );

        let undecided = Finished::of(Err(Error::Classifier(Supervision::undecided(
            "classifier_evidence_limit",
            "combined actions exceed the configured evidence budget",
        ))));
        assert_eq!(undecided.status.as_deref(), Some("service_unavailable"));
        assert_eq!(
            undecided.detail.unwrap()["code"],
            "classifier_evidence_limit"
        );
        assert!(undecided.content.contains(
            "(classifier_evidence_limit: combined actions exceed the configured evidence budget)"
        ));
        assert_eq!(
            undecided.summary,
            "denied: classifier_evidence_limit: combined actions exceed the configured evidence budget"
        );
    }

    #[test]
    fn decisions_and_interruptions_use_tool_notice_envelope() {
        for (error, status) in [
            (
                Error::Denied("Permission refused by the user".into()),
                "denied",
            ),
            (Error::Cancelled, "cancelled"),
            (Error::invalid("invalid input"), "not_executed"),
            (
                Error::NotFound("foo.rs — check the path or search with find_files".into()),
                "not_executed",
            ),
            (
                Error::Provider("service failed".into()),
                "service_unavailable",
            ),
            (Error::tool("execution failed"), "failed"),
        ] {
            let result = Finished::of(Err(error));
            assert!(!result.ok);
            assert!(result.content.starts_with("<tool_notice>"));
            assert!(result.content.ends_with("</tool_notice>"));
            assert_eq!(result.detail.unwrap()["status"], status);
        }
    }
}
