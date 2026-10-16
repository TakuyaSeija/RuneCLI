//! The model seam.
//!
//! The canonical request, response and continuation contract shared by live
//! adapters and scripted fixtures. Network transport and vendor-specific data
//! stay below this seam; sessions consume the same validated events.

pub mod anthropic;
pub mod assembly;
pub(crate) mod cache;
pub mod live;
pub mod openai;
pub mod scripted;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::settings::ModelSettings;
use crate::store::sessions::Role;
use crate::tools::ToolDecl;

/// Native continuation state belongs only to the route that issued it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Continuation {
    pub provider: String,
    pub model: String,
    pub data: Value,
}

/// Additional measured billing dimensions. None means the provider did not report it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageDetails {
    pub measured: bool,
    pub input_known: bool,
    pub output_known: bool,
    pub started_at_ms: Option<i64>,
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub reasoning: Option<u64>,
    pub provider_cost: Option<f64>,
    pub request_id: Option<String>,
    pub native: Value,
    pub complete: bool,
}

/// Provider usage frames may update only a subset of cumulative counters.
pub(crate) fn merge_usage(target: &mut Value, update: &Value) {
    if update.is_null() {
        return;
    }
    if let (Some(target), Some(update)) = (target.as_object_mut(), update.as_object()) {
        for (key, value) in update {
            merge_usage(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *target = update.clone();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub native_reason: String,
    pub category: FinishCategory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishCategory {
    End,
    Tools,
    Length,
    Refusal,
    Continuation,
    Unknown,
}

impl Completion {
    pub fn from_native(reason: &str) -> Self {
        let category = match reason {
            "stop" | "end_turn" | "stop_sequence" => FinishCategory::End,
            "tool_calls" | "tool_use" => FinishCategory::Tools,
            "length" | "max_tokens" | "model_context_window_exceeded" => FinishCategory::Length,
            "content_filter" | "refusal" => FinishCategory::Refusal,
            "pause_turn" => FinishCategory::Continuation,
            _ => FinishCategory::Unknown,
        };
        Self {
            native_reason: reason.into(),
            category,
        }
    }
}

/// One tool call an assistant message asked for, as it must be replayed.
///
/// This exists because a transcript is not a list of texts. The wire format
/// requires the assistant turn that *asked* for a tool to carry the call — id,
/// name and arguments — immediately before the result that answers it. Storing
/// only the text meant the second round of every tool-using conversation sent a
/// tool result with nothing to attach it to, which a real provider rejects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRef {
    pub id: String,
    pub name: String,
    /// Arguments exactly as the model wrote them. Kept as text, not re-encoded:
    /// a model that is shown its own call reformatted learns from a fiction.
    pub args: String,
}

/// One entry of conversation history, in the form a provider is given it.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool calls this assistant message made, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<CallRef>,
    /// The model's own reasoning for this message, when it produced any.
    ///
    /// Replayed rather than dropped because MiMo's documentation is explicit
    /// that previous `reasoning_content` should be sent back with the next
    /// request; a model that is shown a decision without the thinking behind it
    /// re-derives it, badly and at full price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Files the user attached to this turn.
    ///
    /// Only ever on a user message, and carried as far as the dialect, which
    /// decides what this endpoint can be shown and what it must be told about
    /// instead — see [`crate::media`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<crate::media::Attachment>,
}

impl Message {
    pub fn native(&self, provider: &str, model: &str) -> Option<&Value> {
        self.continuation
            .as_ref()
            .filter(|c| c.provider == provider && c.model == model)
            .map(|c| &c.data)
    }
    pub fn user(content: impl Into<String>) -> Message {
        Message::bare(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Message {
        Message::bare(Role::Assistant, content)
    }

    pub fn tool_result(
        id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Message {
        Message {
            tool_call_id: Some(id.into()),
            tool_name: Some(name.into()),
            ..Message::bare(Role::Tool, content)
        }
    }

    fn bare(role: Role, content: impl Into<String>) -> Message {
        Message {
            continuation: None,
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            reasoning: None,
            media: Vec::new(),
        }
    }

    /// The same message, carrying the files the user attached to it.
    pub fn with_media(mut self, media: Vec<crate::media::Attachment>) -> Message {
        self.media = media;
        self
    }

    pub fn with_calls(mut self, calls: Vec<CallRef>) -> Message {
        self.tool_calls = calls;
        self
    }
}

/// Per-operation control is not sent to the model.
#[derive(Debug, Clone, Default)]
pub struct RequestControl {
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub timeout: Option<std::time::Duration>,
    pub role: Option<&'static str>,
    pub config_revision: Option<(std::path::PathBuf, Option<String>)>,
    /// The provider-measured factor by which this model's real input exceeds
    /// Rune's estimate, when one has been recorded (see
    /// [`crate::store::Db::estimate_correction`]). Carried on the request so the
    /// fail-closed guards in provider shaping use the **same** corrected number
    /// the turn loop already decided with — an under-counting estimate must not
    /// let a request the provider would reject slip past a guard and be paid for.
    /// `None` leaves every guard on the raw estimate.
    pub estimate_correction: Option<f64>,
    /// The live `attachments.max_text_chars`, set by whoever owns the
    /// configuration; `None` takes the shipped default.
    pub attachment_text_chars: Option<usize>,
    /// The one tool this request's answer must be, where the endpoint lets a
    /// request say so (`tool_choice`). Set by a job whose answer is a single
    /// structured call — the classifier's decisions, a session title — never by
    /// the agent, which chooses its own tools.
    pub force_tool: Option<String>,
}
impl RequestControl {
    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
            || self.config_revision.as_ref().is_some_and(|(path, hash)| {
                match crate::settings::configuration::read_bounded(path, 8 * 1024 * 1024) {
                    Ok(bytes) => Some(crate::checkpoint::objects::hash_of(&bytes)) != *hash,
                    Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                        hash.is_some()
                    }
                    Err(_) => true,
                }
            })
    }
}

/// What the engine sends for one model call.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    #[serde(skip)]
    pub control: RequestControl,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDecl>,
    /// Which model, and how it is to be sampled. Carried per request rather than
    /// held by the provider because it belongs to the **session**: two open
    /// conversations may legitimately want different models from one endpoint.
    pub settings: ModelSettings,
}

/// One piece of a streamed response.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Continuation(Continuation),
    Finished(Completion),
    Text(String),
    Thinking(String),
    /// Arguments of a tool call, arriving in pieces while the model writes them.
    ///
    /// This is what lets a frontend show a file being written as it is written,
    /// rather than a spinner followed by a finished diff. It is emitted **only**
    /// as progress: the authoritative call is [`Chunk::ToolCall`], which follows
    /// once the arguments parse. A frontend may ignore this variant entirely and
    /// still be correct.
    ToolArgs {
        id: String,
        index: u32,
        name: String,
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    /// A completed descriptor whose arguments cannot be executed. The raw
    /// argument string is not echoed into the result or repaired by guessing.
    InvalidToolCall {
        id: String,
        name: String,
        reason: String,
    },
    /// The call failed before a single byte of answer existed, and is being
    /// opened again — after a wait, or with the next key in the ring.
    ///
    /// Emitted so a retry is never a silent hang, which is the failure this
    /// project has already been bitten by once. A frontend that ignores it is
    /// still correct; it just shows nothing while it waits. Answering
    /// [`Flow::Stop`] to it abandons the retry.
    Retrying {
        attempt: u32,
        of: u32,
        wait_ms: u64,
        rotated: bool,
        reason: String,
    },
    /// Everything streamed since the last attempt began is void: that attempt
    /// failed and the next one starts over. A consumer drops the partial text,
    /// thinking and tool arguments it holds, so screen, transcript and model
    /// stay the same story.
    Reset,
    /// `cached` is the part of `input` that was served from the provider's
    /// prompt cache. It is charged at a fraction of a fresh token — often under
    /// one percent — so a session's cost cannot be worked out without it.
    Usage {
        input: u64,
        output: u64,
        cached: u64,
        details: UsageDetails,
    },
}

/// What the consumer wants the provider to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop streaming — the turn was cancelled.
    Stop,
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// Stream one response. The provider calls `sink` for every chunk and stops
    /// early if it ever answers [`Flow::Stop`].
    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Chunk) -> Flow) -> Result<()>;
}
