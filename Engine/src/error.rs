//! One error type for the whole engine.
//!
//! Hand-written rather than derived: `thiserror` would earn its place if we had
//! dozens of variants with rich formatting, and we do not. This is ~60 lines and
//! costs no proc-macro in the build graph.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// Filesystem, process, or pty failure.
    Io(std::io::Error),
    /// SQLite failure.
    Db(String),
    /// Malformed JSON, in either direction.
    Json(String),
    /// A path escaped the workspace jail, or could not be resolved inside it.
    Jail(String),
    /// The permission gate resolved against the call: the user denied it, the
    /// question timed out, or the thinking classifier said no. The gate itself
    /// never refuses on its own.
    Denied(String),
    /// The thinking classifier did not let the call through
    /// ([`crate::perm::classifier`]).
    ///
    /// Its own variant because what a call is told depends on which way it
    /// went — refused by the policy, or never decided — and that used to be
    /// recovered by searching the message for `classifier_`, which any other
    /// error naming a file of that name matched too.
    Classifier(Supervision),
    /// A tool ran and failed. Carried back to the model as a tool result.
    Tool(String),
    /// The frontend sent something the protocol does not allow.
    Protocol(String),
    /// The model provider failed or produced something unusable.
    Provider(String),
    /// The provider was never reached: DNS, TCP, TLS, or a timeout.
    ///
    /// Its own variant because it is the one failure where **nothing was sent,
    /// nothing was billed and nothing was consumed** — the only state in which
    /// repeating a model call is free of consequence.
    Unreachable(String),
    /// The server went silent: nothing arrived for this long, before its
    /// answer started or between two parts of it (`network.stream_idle_timeout`).
    ///
    /// Its own variant because a connection that stays open and says nothing
    /// never fails by itself: without a limit a turn waited on it, spinner
    /// running, until the person pressed Stop. It is repeated like any other
    /// provider failure.
    Stalled(std::time::Duration),
    /// A provider answered, and the answer was a refusal.
    ///
    /// Separate from [`Error::Provider`] because the **reaction** depends on the
    /// status and nothing else can supply it: 429 rests a key and tries the next
    /// one, 400 must be reported untouched, and a string is a terrible place to
    /// keep a number that decides control flow. A server's `Retry-After` is
    /// not carried: Rune's own curve decides every wait (`crate::retry`).
    Rejected { status: u16, message: String },
    /// An API explicitly identifies an output parameter and its numeric ceiling.
    /// Never inferred from prose, a network failure, or an input-context error.
    OutputLimit { limit: u64, message: String },
    /// The model stopped before it finished: its output limit, or an end the
    /// provider gave no reason for. Deterministic for the same request, so it
    /// is reported, never retried; what was written is kept.
    Incomplete(String),
    /// The provider refused the request as longer than the model's context.
    /// Retrying the same request cannot help; compacting the history can.
    ContextOverflow(String),
    /// The turn was cancelled by the user.
    Cancelled,
    /// Session, checkpoint, or file that does not exist.
    NotFound(String),
    /// Argument failed validation.
    Invalid(String),
    /// A bug in the engine: something panicked and was caught. Never the user's
    /// fault, and never a reason to take the session down with it — see
    /// [`crate::panic`].
    Internal(String),
}

impl Error {
    /// Stable machine-readable code. Frontends switch on this, never on the text.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io(_) => "io",
            Error::Db(_) => "db",
            Error::Json(_) => "json",
            Error::Jail(_) => "jail",
            Error::Denied(_) => "denied",
            Error::Classifier(Supervision::Refused(_)) => "classifier_denied",
            Error::Classifier(Supervision::Undecided { code, .. }) => code,
            Error::Tool(_) => "tool",
            Error::Protocol(_) => "protocol",
            Error::Provider(_) => "provider",
            // The same code on purpose: a frontend switching on this cares that
            // the provider failed, not which half of the failure it was.
            Error::Unreachable(_) => "provider",
            Error::Stalled(_) => "provider",
            Error::Rejected { .. } | Error::OutputLimit { .. } => "provider",
            Error::Incomplete(_) => "incomplete",
            Error::ContextOverflow(_) => "context_overflow",
            Error::Cancelled => "cancelled",
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid",
            Error::Internal(_) => "internal",
        }
    }

    pub fn invalid<S: Into<String>>(msg: S) -> Error {
        Error::Invalid(msg.into())
    }

    pub fn tool<S: Into<String>>(msg: S) -> Error {
        Error::Tool(msg.into())
    }
}

impl fmt::Display for Error {
    /// Every user-visible error text in the engine is built here, which is what
    /// makes this the one place worth scrubbing: an API key that leaked into a
    /// provider's rejection, a URL, or an I/O message cannot reach a frontend,
    /// a log, or a pasted bug report without passing this line first.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::net::redact::scrub(&self.raw()))
    }
}

impl Error {
    fn raw(&self) -> String {
        match self {
            Error::Io(e) => e.to_string(),
            Error::Db(m) => format!("database: {m}"),
            Error::Json(m) => format!("json: {m}"),
            Error::Jail(m) => format!("outside workspace: {m}"),
            Error::Denied(m) => format!("denied: {m}"),
            Error::Classifier(s) => format!("denied: {}", s.reason()),
            Error::Tool(m) => m.clone(),
            Error::Protocol(m) => format!("protocol: {m}"),
            Error::Provider(m) => format!("provider: {m}"),
            Error::Unreachable(m) => format!("could not reach the provider: {m}"),
            Error::Stalled(silence) => format!(
                "provider: nothing arrived for {}s; the request was abandoned",
                silence.as_secs().max(1)
            ),
            Error::Rejected { status, message } => format!("provider: HTTP {status}: {message}"),
            Error::OutputLimit { limit, message } => {
                format!("provider: output limit {limit}: {message}")
            }
            Error::Incomplete(m) => format!("the answer was cut off: {m}"),
            Error::ContextOverflow(m) => {
                format!("the request is longer than the model's context: {m}")
            }
            Error::Cancelled => "cancelled".to_string(),
            Error::NotFound(m) => format!("not found: {m}"),
            Error::Invalid(m) => format!("invalid: {m}"),
            Error::Internal(m) => format!("internal error: {m}"),
        }
    }
}

/// How the thinking classifier failed to let a call through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervision {
    /// The policy refused the action, for this reason (the classifier's words).
    Refused(String),
    /// No decision was reached. `code` names why, as the audit row records it:
    /// `classifier_unavailable`, `classifier_invalid_response`,
    /// `classifier_evidence_limit`, `classifier_evidence_unavailable`,
    /// `classifier_policy_changed`, `classifier_policy_invalid`.
    Undecided { code: &'static str, reason: String },
}

impl Supervision {
    pub fn undecided(code: &'static str, reason: impl Into<String>) -> Supervision {
        Supervision::Undecided {
            code,
            reason: reason.into(),
        }
    }

    /// What the call is told, after the sentence its status carries.
    pub fn reason(&self) -> String {
        match self {
            Supervision::Refused(reason) => format!("Rejected by classifier: {reason}"),
            Supervision::Undecided { code, reason } => format!("{code}: {reason}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::Db(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::Json(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
