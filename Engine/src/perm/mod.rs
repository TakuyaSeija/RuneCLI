//! The permission gate: the engine's answer to "may this actually happen?".
//!
//! Three parts, deliberately separate:
//!   * [`denylist`] — reads a command and says how dangerous it is (pure)
//!   * [`policy`]   — turns danger plus context into a verdict (pure)
//!   * [`broker`]   — carries the question to a human and the answer back
//!
//! `Gate` is the only thing tools talk to, and it is the only place that both
//! decides and remembers. **The engine decides, never the model and never a
//! server** — a request is a request, wherever it came from. And what the
//! engine decides, as of the owner's rule of 2026-09-28, is only ever *who
//! answers*: it never says no on its own. Every dangerous or outside-workspace
//! call becomes a question; the one autonomous denier is the thinking
//! classifier, and only where a CLASSIFIER.md policy says so.

pub mod broker;
#[path = "Classifier.rs"]
pub mod classifier;
mod classifier_context;
pub(crate) mod classifier_input;
mod classifier_media;
pub mod denylist;
pub mod policy;
pub mod reach;
pub(crate) mod windows_shells;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::panic;
use crate::proto::{Decision, Event};
use crate::store::Db;
use broker::{Answer, Broker};
use denylist::Risk;
pub use policy::Mode;
use policy::{Situation, Verdict};

/// The `tool` column a standing grant of local services is filed under, and
/// its one pattern: the right is the project's, not a socket's — Landlock on
/// the kernels Rune runs on today cannot tell one socket from another, so a
/// grant narrower than "this project's commands" would be a promise the
/// kernel does not keep. `@`-prefixed like [`reach::GRANT_TOOL`].
pub const SERVICES_GRANT_TOOL: &str = "@local-services";
pub const SERVICES_GRANT_PATTERN: &str = "unix-sockets";

/// One authorisation question.
pub struct Ask<'a> {
    pub session: i64,
    pub tool: &'a str,
    /// The model's own one-line statement of intent.
    pub action: &'a str,
    /// Exactly what will happen, in the user's terms.
    pub preview: String,
    /// What an "allow always" would remember, e.g. `cargo` or `patch_file`.
    pub grant_key: String,
    pub mutates: bool,
    /// The call is a command line rather than a named path — see
    /// [`policy::Situation::runs`].
    pub runs: bool,
    pub risk: Risk,
    /// Paths outside the workspace this call named and does not yet have.
    ///
    /// Pre-filtered by the caller to what is genuinely new, so a non-empty list
    /// always means there is something to decide.
    pub reach: Vec<reach::Candidate>,
    /// The call reaches a service running outside the workspace through a
    /// Unix socket and this session does not hold that right — see
    /// [`policy::Situation::services`]. Pre-filtered by the caller, like
    /// `reach`.
    pub local_services: bool,
    /// Why a command this session runs now would not be confined by the kernel
    /// sandbox — the sandbox's own reason — or `None` when it would be.
    pub unconfined: Option<String>,
}

/// What an authorised call may now do that it could not a moment ago.
///
/// The gate cannot widen anything itself — it holds no session — so it says
/// what it authorised and [`crate::session::call`] applies it. That split is
/// what keeps "the engine decides" true while leaving the state where it lives.
#[derive(Debug, Default, PartialEq)]
pub struct Allowed {
    /// This exact call was answered by the person, not a remembered grant.
    pub person: bool,
    /// Anchors to add to the jail and the kernel ruleset before the call runs.
    pub reach: Vec<PathBuf>,
    /// Whether that widening outlives this one call.
    pub keep: bool,
    /// Drop confinement for this call only. Never persisted.
    pub full_access: bool,
    /// Let the call's commands reach local services; for this call only
    /// unless `keep`.
    pub local_services: bool,
}

pub struct Gate {
    db: Arc<Mutex<Db>>,
    project: String,
    broker: Arc<Broker>,
    timeout: Duration,
}

impl Gate {
    pub fn new(db: Arc<Mutex<Db>>, project: String, broker: Arc<Broker>) -> Gate {
        Gate {
            db,
            project,
            broker,
            timeout: broker::DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Gate {
        self.timeout = timeout;
        self
    }

    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// Decide, ask if needed, and remember if told to.
    ///
    /// `Ok(())` means proceed. Every other path is an error the caller turns into
    /// a failed tool result — a refusal is a normal outcome, not a crash.
    pub fn authorize(
        &self,
        mode: Mode,
        ask: Ask<'_>,
        emit: &mut dyn FnMut(Event),
        cancel: &Arc<AtomicBool>,
    ) -> Result<Allowed> {
        let situation = self.situation(mode, &ask)?;
        match policy::verdict(&situation) {
            Verdict::Allow => Ok(Allowed::default()),
            Verdict::Ask { remember } => {
                // A mode that would not have asked says why it does.
                let reason = policy::unconfined_command(&situation).then(|| {
                    format!(
                        "Commands run unconfined here: {}. Auto asks before each one; Full stops asking.",
                        ask.unconfined.as_deref().unwrap_or("the kernel sandbox is not enforcing")
                    )
                });
                self.ask_user(ask, remember, reason, emit, cancel)
            }
        }
    }

    /// What this gate decides about `ask`, without asking anybody or changing
    /// anything — the one decision [`Gate::authorize`] acts on, so a caller
    /// that only needs to know (whether a call may run alongside others) gets
    /// the same answer the call itself will.
    pub fn verdict(&self, mode: Mode, ask: &Ask<'_>) -> Result<Verdict> {
        Ok(policy::verdict(&self.situation(mode, ask)?))
    }

    /// What the policy is asked about `ask` in `mode`: the call's own facts and
    /// the grants on record.
    fn situation<'a>(&self, mode: Mode, ask: &'a Ask<'_>) -> Result<Situation<'a>> {
        let granted = {
            let db = panic::lock(&self.db);
            db.is_granted(&self.project, ask.tool, &ask.grant_key)?
        };
        Ok(Situation {
            mode,
            mutates: ask.mutates,
            risk: &ask.risk,
            granted,
            reaches: !ask.reach.is_empty(),
            full_access: ask.reach.iter().any(|c| c.full_access),
            services: ask.local_services,
            runs: ask.runs,
            confined: ask.unconfined.is_none(),
        })
    }

    /// Authorise a call that runs alongside others: allowed outright, or not
    /// run at all. Such a call was let in only because the gate allowed it
    /// without a question; if that stopped being true before it ran (a grant
    /// withdrawn, the mode changed), it fails closed rather than putting a
    /// second question on the screen or widening the jail while other calls
    /// are reading through it.
    pub fn authorize_silently(&self, mode: Mode, ask: &Ask<'_>) -> Result<Allowed> {
        match self.verdict(mode, ask)? {
            Verdict::Allow => Ok(Allowed::default()),
            Verdict::Ask { .. } => Err(Error::Denied(
                "Not executed: this call now needs the person's permission, which is asked only of a call that runs on its own. Call it again.".into(),
            )),
        }
    }

    fn ask_user(
        &self,
        ask: Ask<'_>,
        remember: bool,
        reason: Option<String>,
        emit: &mut dyn FnMut(Event),
        cancel: &Arc<AtomicBool>,
    ) -> Result<Allowed> {
        let req = self.broker.open(());
        emit(Event::PermissionRequest {
            session: ask.session,
            req: req.clone(),
            tool: ask.tool.to_string(),
            action: ask.action.to_string(),
            preview: ask.preview.clone(),
            remember_as: remember.then(|| ask.grant_key.clone()),
            reach: ask
                .reach
                .iter()
                .map(|c| c.named.display().to_string())
                .collect(),
            local_services: ask.local_services,
            full_access: ask.reach.iter().any(|c| c.full_access),
            reason,
        });

        let answer = self.broker.wait(&req, cancel, self.timeout);
        let decision = match answer {
            Answer::Decided(d) => d,
            Answer::Cancelled => return Err(Error::Cancelled),
            Answer::TimedOut => {
                emit(Event::PermissionResolved {
                    session: ask.session,
                    req,
                    decision: Decision::Deny,
                });
                return Err(Error::Denied("nobody answered in time".into()));
            }
        };

        emit(Event::PermissionResolved {
            session: ask.session,
            req,
            decision,
        });

        let full_access = ask.reach.iter().any(|c| c.full_access);
        let anchors: Vec<PathBuf> = ask
            .reach
            .iter()
            .filter(|c| !c.full_access)
            .map(|c| c.anchor.clone())
            .collect();
        match decision {
            Decision::Deny => Err(Error::Denied(
                "Permission refused by the user. The operation was not executed.".into(),
            )),
            Decision::AllowOnce => Ok(Allowed {
                person: true,
                reach: anchors,
                keep: false,
                full_access,
                local_services: ask.local_services,
            }),
            Decision::AllowAlways => {
                // A grant is only ever stored when the policy offered one. This
                // is what makes "cannot be always allowed" true rather than
                // merely intended.
                if remember {
                    let db = panic::lock(&self.db);
                    db.grant(&self.project, ask.tool, &ask.grant_key)?;
                    // Stored as the user read it, never as the anchor: the
                    // anchor is a directory that happened to exist today, and
                    // re-validating the named path on the next session is what
                    // keeps a grant from widening on its own when the tree below
                    // it changes.
                    for c in &ask.reach {
                        db.grant(
                            &self.project,
                            reach::GRANT_TOOL,
                            &c.named.display().to_string(),
                        )?;
                    }
                    if ask.local_services {
                        db.grant(&self.project, SERVICES_GRANT_TOOL, SERVICES_GRANT_PATTERN)?;
                    }
                }
                Ok(Allowed {
                    person: true,
                    reach: anchors,
                    keep: remember,
                    // A frontend cannot make host access persistent by sending
                    // AllowAlways directly. It still answers this open request
                    // as a one-call grant rather than turning a valid decision
                    // into a confusing sandbox failure.
                    full_access,
                    local_services: ask.local_services,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn gate() -> (Gate, Arc<Mutex<Db>>) {
        let db = Arc::new(Mutex::new(Db::memory().unwrap()));
        let g = Gate::new(Arc::clone(&db), "/p".into(), Arc::new(Broker::new()))
            .with_timeout(Duration::from_millis(300));
        (g, db)
    }

    fn ask<'a>(tool: &'a str, mutates: bool, risk: Risk) -> Ask<'a> {
        Ask {
            session: 1,
            tool,
            action: "do the thing",
            preview: "preview".into(),
            grant_key: "key".into(),
            mutates,
            runs: false,
            risk,
            reach: Vec::new(),
            local_services: false,
            unconfined: None,
        }
    }

    fn answer_with(g: &Gate, d: Decision) {
        let b = Arc::clone(g.broker());
        std::thread::spawn(move || {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(5));
                // The id of the only open question in these tests.
                for n in 1..20 {
                    if b.reply(&format!("p{n}"), d) {
                        return;
                    }
                }
            }
        });
    }

    /// A call running alongside others is never put to the person: what the
    /// gate allows outright runs, and what would need a question fails closed —
    /// no card, no grant, nothing widened.
    #[test]
    fn a_call_alongside_others_is_allowed_outright_or_not_run() {
        let (g, db) = gate();
        let read = ask("inspect_file", false, Risk::Normal);
        assert_eq!(g.verdict(Mode::Ask, &read).unwrap(), Verdict::Allow);
        assert!(g.authorize_silently(Mode::Ask, &read).is_ok());

        let write = ask("create_file", true, Risk::Normal);
        assert!(matches!(
            g.verdict(Mode::Ask, &write).unwrap(),
            Verdict::Ask { .. }
        ));
        let refused = g.authorize_silently(Mode::Ask, &write).unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
        assert!(
            !db.lock()
                .unwrap()
                .is_granted("/p", "create_file", "key")
                .unwrap(),
            "nothing was granted"
        );
    }

    /// `Auto` puts a command to the person when nothing would confine it,
    /// says why in the sandbox's own words, and offers nothing to remember;
    /// with the sandbox holding, the same command runs unasked.
    #[test]
    fn auto_says_why_it_asks_before_an_unconfined_command() {
        let (g, _db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        let command = |unconfined: Option<&str>| Ask {
            runs: true,
            unconfined: unconfined.map(str::to_string),
            ..ask("terminal_use", true, Risk::Normal)
        };

        let mut events = Vec::new();
        g.authorize(Mode::Auto, command(None), &mut |e| events.push(e), &cancel)
            .unwrap();
        assert!(events.is_empty(), "a confined command runs unasked in Auto");

        answer_with(&g, Decision::AllowOnce);
        let mut events = Vec::new();
        g.authorize(
            Mode::Auto,
            command(Some("this kernel has no Landlock support")),
            &mut |e| events.push(e),
            &cancel,
        )
        .unwrap();
        let (remember_as, reason) = events
            .iter()
            .find_map(|e| match e {
                Event::PermissionRequest {
                    remember_as,
                    reason,
                    ..
                } => Some((remember_as.clone(), reason.clone())),
                _ => None,
            })
            .expect("the command was put to the person");
        assert_eq!(remember_as, None, "nothing about it may be remembered");
        assert_eq!(
            reason.as_deref(),
            Some("Commands run unconfined here: this kernel has no Landlock support. Auto asks before each one; Full stops asking.")
        );
    }

    #[test]
    fn a_read_is_allowed_without_asking() {
        let (g, _db) = gate();
        let mut events = Vec::new();
        let cancel = Arc::new(AtomicBool::new(false));
        g.authorize(
            Mode::Ask,
            ask("inspect_file", false, Risk::Normal),
            &mut |e| events.push(e),
            &cancel,
        )
        .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn allow_always_is_remembered_and_stops_asking() {
        let (g, db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowAlways);
        g.authorize(
            Mode::Ask,
            ask("patch_file", true, Risk::Normal),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
        assert!(db
            .lock()
            .unwrap()
            .is_granted("/p", "patch_file", "key")
            .unwrap());

        // Second call must not need an answer at all.
        let mut events = Vec::new();
        g.authorize(
            Mode::Ask,
            ask("patch_file", true, Risk::Normal),
            &mut |e| events.push(e),
            &cancel,
        )
        .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn allow_once_is_not_remembered() {
        let (g, db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowOnce);
        g.authorize(
            Mode::Ask,
            ask("patch_file", true, Risk::Normal),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
        assert!(!db
            .lock()
            .unwrap()
            .is_granted("/p", "patch_file", "key")
            .unwrap());
    }

    #[test]
    fn dangerous_consent_is_never_stored_even_when_the_user_says_always() {
        let (g, db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowAlways);
        let risk = Risk::AskEvery { why: "sudo".into() };
        g.authorize(
            Mode::Ask,
            ask("terminal_use", true, risk),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
        assert!(!db
            .lock()
            .unwrap()
            .is_granted("/p", "terminal_use", "key")
            .unwrap());
    }

    #[test]
    fn a_severe_call_is_a_question_the_owner_answers_never_a_refusal() {
        let (g, _db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowOnce);
        let mut events = Vec::new();
        let risk = Risk::Severe {
            why: "removes the filesystem root".into(),
        };
        g.authorize(
            Mode::Ask,
            ask("terminal_use", true, risk),
            &mut |e| events.push(e),
            &cancel,
        )
        .unwrap();
        assert!(
            matches!(
                &events[0],
                Event::PermissionRequest {
                    remember_as: None,
                    ..
                }
            ),
            "the catastrophic class is asked, and nothing may be remembered"
        );
    }

    /// A path that used to be "sensitive" is put to the person like any other
    /// outside path, "always" on offer (owner, 2026-10-17: no folder is
    /// special), and a service reached through a Unix socket rides the same
    /// card; "always" files the project's standing grant of local services.
    #[test]
    fn an_outside_path_and_a_local_service_are_asked_and_may_be_kept() {
        let (g, db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowAlways);
        let mut events = Vec::new();
        let mut a = ask("terminal_use", true, Risk::Normal);
        a.runs = true;
        a.local_services = true;
        a.reach = vec![reach::Candidate {
            named: "/home/dev/.ssh/config".into(),
            anchor: "/home/dev/.ssh/config".into(),
            full_access: false,
        }];
        let allowed = g
            .authorize(Mode::Auto, a, &mut |e| events.push(e), &cancel)
            .unwrap();
        match &events[0] {
            Event::PermissionRequest {
                remember_as,
                reach,
                local_services,
                ..
            } => {
                assert_eq!(remember_as.as_deref(), Some("key"), "always is on offer");
                assert_eq!(reach, &vec!["/home/dev/.ssh/config".to_string()]);
                assert!(local_services, "the card says a service is reached");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(allowed.local_services && allowed.keep);
        let db = db.lock().unwrap();
        assert!(db
            .is_granted("/p", SERVICES_GRANT_TOOL, SERVICES_GRANT_PATTERN)
            .unwrap());
        assert!(db
            .is_granted("/p", reach::GRANT_TOOL, "/home/dev/.ssh/config")
            .unwrap());
    }

    #[test]
    fn denial_and_timeout_both_stop_the_call() {
        let (g, _db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::Deny);
        assert!(g
            .authorize(
                Mode::Ask,
                ask("patch_file", true, Risk::Normal),
                &mut |_| {},
                &cancel
            )
            .is_err());

        // No answering thread this time: the wait times out.
        let err = g
            .authorize(
                Mode::Ask,
                ask("patch_file", true, Risk::Normal),
                &mut |_| {},
                &cancel,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Denied(_)));
    }

    #[test]
    fn cancelling_while_waiting_reports_cancellation_not_denial() {
        let (g, _db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            c2.store(true, Ordering::Relaxed);
        });
        let err = g
            .authorize(
                Mode::Ask,
                ask("patch_file", true, Risk::Normal),
                &mut |_| {},
                &cancel,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }

    #[test]
    fn the_question_carries_what_the_frontend_needs_to_render_it() {
        let (g, _db) = gate();
        let cancel = Arc::new(AtomicBool::new(false));
        answer_with(&g, Decision::AllowOnce);
        let mut events = Vec::new();
        g.authorize(
            Mode::Ask,
            ask("patch_file", true, Risk::Normal),
            &mut |e| events.push(e),
            &cancel,
        )
        .unwrap();
        match &events[0] {
            Event::PermissionRequest {
                tool,
                preview,
                remember_as,
                ..
            } => {
                assert_eq!(tool, "patch_file");
                assert_eq!(preview, "preview");
                assert_eq!(remember_as.as_deref(), Some("key"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
