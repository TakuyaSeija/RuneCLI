//! The OpenAI wire: how a `/chat/completions` endpoint is spoken to.
//!
//! Xiaomi MiMo, OpenAI, OpenRouter, Google, Groq, a self-hosted 9Router and a
//! local Ollama differ in their base URL, their auth header and how reasoning is
//! switched on — all three of which are data in
//! [`crate::settings::providers`], not code. So there is one parser here, not
//! seven, and adding an eighth endpoint of this kind is a preset rather than a
//! file. Anthropic is the exception, and it has [`super::anthropic`].
//!
//! # Streaming, and the one thing it buys
//!
//! Tool arguments arrive in pieces (`tool_calls[].function.arguments`), and this
//! is where a frontend gets to show a file being written **while** it is written.
//! Measured against MiMo before this was designed:
//!
//! ```text
//! top-level string argument : 166 chunks, mean 8 chars   → truly live
//! array-valued argument     :   1 chunk,      1447 chars → arrives whole
//! ```
//!
//! That measurement is why [`crate::tools`] accepts a single item at the top
//! level as well as an array: the shape of the schema decides whether a progress
//! counter can move at all. Nothing here fakes it — a frontend is fed exactly
//! what the wire produced, and if that is one lump, it sees one lump.

use serde_json::{json, Map, Value};

use super::assembly::Assembly;
use super::{Chunk, Message, Request};
use crate::error::{Error, Result};
use crate::media;
use crate::settings::providers::{ModelInfo, ProviderConfig};
use crate::store::sessions::Role;

/// Where a request of this dialect goes.
pub(crate) fn endpoint(cfg: &ProviderConfig) -> String {
    format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'))
}

/// Wire state for one OpenAI-compatible stream.
///
/// Accumulates partial tool calls and preserves the latest usage statistics so
/// that gateways emitting usage on every streamed chunk (or on multiple frames)
/// only produce a single `Chunk::Usage` at stream completion.
#[derive(Default)]
pub(crate) struct State {
    pub calls: Assembly,
    pub usage: Option<Chunk>,
    pub completion: Option<super::Completion>,
    pub continuation: Map<String, Value>,
    pub request_id: Option<String>,
    pub(crate) thought_markup: String,
    pub(crate) inside_thought: bool,
}

impl State {
    pub fn has_usage(&self) -> bool {
        self.usage.is_some()
    }

    pub fn has_pending_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    /// Separate provider-emitted `<thought>` framing from visible answer text.
    /// The delimiter can be split across arbitrary stream chunks, so unmatched
    /// suffix bytes remain buffered until a later chunk resolves them.
    fn absorb_text(&mut self, text: &str) -> Vec<Chunk> {
        const OPEN: &str = "<thought>";
        const CLOSE: &str = "</thought>";
        self.thought_markup.push_str(text);
        let mut out = Vec::new();
        loop {
            let marker = if self.inside_thought { CLOSE } else { OPEN };
            if let Some(at) = self.thought_markup.find(marker) {
                if at > 0 {
                    let value = self.thought_markup[..at].to_string();
                    out.push(if self.inside_thought {
                        Chunk::Thinking(value)
                    } else {
                        Chunk::Text(value)
                    });
                }
                self.thought_markup.drain(..at + marker.len());
                self.inside_thought = !self.inside_thought;
                continue;
            }
            let retain = (1..marker.len())
                .rev()
                .find(|count| self.thought_markup.ends_with(&marker[..*count]))
                .unwrap_or(0);
            let emit = self.thought_markup.len().saturating_sub(retain);
            if emit > 0 {
                let value: String = self.thought_markup.drain(..emit).collect();
                out.push(if self.inside_thought {
                    Chunk::Thinking(value)
                } else {
                    Chunk::Text(value)
                });
            }
            break;
        }
        out
    }

    pub fn finish(mut self) -> Result<Vec<Chunk>> {
        let mut out = self.calls.finish()?;
        if !self.thought_markup.is_empty() {
            let value = std::mem::take(&mut self.thought_markup);
            out.insert(
                0,
                if self.inside_thought {
                    Chunk::Thinking(value)
                } else {
                    Chunk::Text(value)
                },
            );
        }
        if let Some(u) = self.usage.take() {
            out.push(u);
        }
        if let Some(completion) = self.completion {
            out.push(Chunk::Finished(completion));
        }
        Ok(out)
    }
}

/// Only protocol envelopes declare failure. Text in choices/delta/content is
/// model output, even when it quotes [Error] or an entire error JSON object.
pub(crate) fn stream_error(value: &Value) -> Option<String> {
    let typed = ["type", "event"].iter().any(|key| {
        matches!(
            value.get(*key).and_then(Value::as_str),
            Some("error" | "response.failed")
        )
    });
    let error = value
        .get("error")
        .filter(|v| !v.is_null())
        .or_else(|| typed.then(|| value.pointer("/response/error")).flatten());
    if error.is_none() && !typed {
        return None;
    }
    Some(
        error
            .and_then(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| e.as_str())
            })
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or("the provider reported an error mid-stream")
            .to_string(),
    )
}

/// Turn one streamed frame into whatever it means, in order.
///
/// Returning the chunks rather than emitting them is what lets one loop in
/// [`super::live`] serve both dialects: the loop owns cancellation and the sink,
/// this owns the wire.
pub(crate) fn absorb(value: &Value, state: &mut State) -> Result<Vec<Chunk>> {
    // Some gateways report failure inside the stream rather than by status code
    // — a truncated answer with an explanation attached.
    if let Some(id) = value.get("id").and_then(Value::as_str) {
        state.request_id = Some(id.into());
    }
    if let Some(update) = value.get("usage").filter(|v| !v.is_null()) {
        if !update.is_object() {
            return Err(Error::Provider("invalid usage object".into()));
        }
        for path in [
            "/prompt_tokens",
            "/completion_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/prompt_cache_hit_tokens",
            "/cache_write_tokens",
            "/prompt_tokens_details/cache_creation_tokens",
        ] {
            if update
                .pointer(path)
                .is_some_and(|v| !v.is_null() && v.as_u64().is_none_or(|n| n > i64::MAX as u64))
            {
                return Err(Error::Provider("invalid usage token count".into()));
            }
        }
        let mut merged = match &state.usage {
            Some(Chunk::Usage { details, .. }) => details.native.clone(),
            _ => Value::Null,
        };
        super::merge_usage(&mut merged, update);
        state.usage = usage_of(&json!({"id":state.request_id,"usage":merged}));
    }
    if let Some(message) = stream_error(value) {
        return Err(Error::Provider(message));
    }

    if let Some(reason) = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        state.completion = Some(super::Completion::from_native(reason));
    }
    if value.get("choices").is_some_and(|v| !v.is_array()) {
        return Err(Error::Provider("invalid choices in model stream".into()));
    }
    let Some(delta) = value.pointer("/choices/0/delta") else {
        return Ok(Vec::new());
    };
    if !delta.is_object()
        || delta
            .get("content")
            .is_some_and(|v| !v.is_null() && !v.is_string() && !v.is_array())
    {
        return Err(Error::Provider(
            "invalid content delta in model stream".into(),
        ));
    }
    if ["audio", "images"]
        .iter()
        .any(|key| delta.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(Error::Provider(
            "This output mode is not supported by Rune's text and tool adapter".into(),
        ));
    }

    let mut out = Vec::new();
    if let Some(text) = str_field(delta, "content") {
        out.extend(state.absorb_text(text));
    }
    if let Some(parts) = delta.get("content").and_then(Value::as_array) {
        for part in parts {
            let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
            match part.get("type").and_then(Value::as_str) {
                Some("text") if is_thought => {
                    if let Some(text) = str_field(part, "text") {
                        out.push(Chunk::Thinking(text.into()));
                    }
                }
                Some("text") => {
                    if let Some(text) = str_field(part, "text") {
                        out.push(Chunk::Text(text.into()));
                    }
                }
                Some("thinking") => {
                    if let Some(text) =
                        str_field(part, "thinking").or_else(|| str_field(part, "text"))
                    {
                        out.push(Chunk::Thinking(text.into()));
                    }
                    if let Some(parts) = part.get("thinking").and_then(Value::as_array) {
                        for text in parts.iter().filter_map(|p| str_field(p, "text")) {
                            out.push(Chunk::Thinking(text.into()));
                        }
                    }
                }
                Some("refusal") => {
                    if let Some(text) = str_field(part, "refusal") {
                        out.push(Chunk::Text(text.into()));
                    }
                }
                _ => {
                    return Err(Error::Provider(
                        "unsupported typed content in model response".into(),
                    ))
                }
            }
        }
    }
    // One delta carries one thought. OpenRouter and several gateways send the
    // plain `reasoning` / `reasoning_content` string *and* a `reasoning_details`
    // array holding the very same text, so emitting a Thinking chunk from each
    // of the two doubled every reasoning token: twice on screen, twice in the
    // stored assistant message, and twice in every request that follows. The
    // plain field is the streamed one and the one displayed; the details array
    // is still accumulated below, because that is what the next turn replays.
    let thought = str_field(delta, "reasoning_content").or_else(|| str_field(delta, "reasoning"));
    if let Some(thought) = thought {
        out.push(Chunk::Thinking(thought.to_string()));
        let field = if str_field(delta, "reasoning_content").is_some() {
            "reasoning_content"
        } else {
            "reasoning"
        };
        let mut text = state
            .continuation
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        text.push_str(thought);
        state.continuation.insert(field.into(), json!(text));
    }
    if let Some(refusal) = str_field(delta, "refusal") {
        out.push(Chunk::Text(refusal.into()));
        state.completion = Some(super::Completion::from_native("refusal"));
    }
    if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
        let entries = state
            .continuation
            .entry("reasoning_details")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap();
        for detail in details {
            if thought.is_none() {
                if let Some(text) =
                    str_field(detail, "text").or_else(|| str_field(detail, "summary"))
                {
                    out.push(Chunk::Thinking(text.into()));
                }
            }
            let index = detail.get("index").cloned().unwrap_or(json!(0));
            let kind = detail.get("type").cloned().unwrap_or(Value::Null);
            if let Some(old) = entries.iter_mut().find(|e| {
                e.get("index").cloned().unwrap_or(json!(0)) == index && e.get("type") == Some(&kind)
            }) {
                let fields = detail
                    .as_object()
                    .ok_or_else(|| Error::Provider("invalid reasoning detail".into()))?;
                for (key, value) in fields {
                    if ["text", "summary", "data"].contains(&key.as_str()) {
                        if let (Some(previous), Some(fragment)) =
                            (old.get(key).and_then(Value::as_str), value.as_str())
                        {
                            old[key] = json!(format!("{previous}{fragment}"));
                            continue;
                        }
                    }
                    old[key] = value.clone();
                }
            } else {
                entries.push(detail.clone());
            }
        }
    }
    if let Some(list) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for piece in list {
            if !piece.is_object()
                || piece
                    .get("index")
                    .is_some_and(|v| v.as_u64().is_none_or(|n| n > u32::MAX as u64))
            {
                return Err(Error::Provider("invalid streamed tool descriptor".into()));
            }
            if piece.get("function").is_some_and(|f| {
                !f.is_object()
                    || ["name", "arguments"]
                        .iter()
                        .any(|key| f.get(*key).is_some_and(|v| !v.is_string() && !v.is_null()))
            }) {
                return Err(Error::Provider(
                    "invalid streamed tool function fields".into(),
                ));
            }
            if let Some(id) = piece.get("id").and_then(Value::as_str) {
                let index = piece
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string();
                state
                    .continuation
                    .entry("tool_ids")
                    .or_insert_with(|| json!({}))[index] = json!(id);
            }
            if let Some(extra) = piece.get("extra_content") {
                let index = piece
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string();
                state
                    .continuation
                    .entry("tool_extra_content")
                    .or_insert_with(|| json!({}))[index] = extra.clone();
            }
            out.extend(absorb_call(&mut state.calls, piece));
        }
    }
    Ok(out)
}

// ─── Request shaping ──────────────────────────────────────────────────────────

/// The request body, exactly as it would be sent.
///
/// Visible to the crate so `provider dryrun` can show the real thing rather than
/// a second implementation of it that could drift from this one.
pub(crate) fn body(req: &Request, cfg: &ProviderConfig) -> Value {
    let mut body = Map::new();
    body.extend(req.settings.body(cfg));
    body.insert("stream".into(), json!(true));
    // Usage arrives in a final chunk with no choices. MiMo sends it regardless;
    // OpenAI only sends it when asked, and unknown keys are ignored by the rest.
    body.insert("stream_options".into(), json!({ "include_usage": true }));

    let last_user_idx = req
        .messages
        .iter()
        .rposition(|m| m.role == Role::User || !m.media.is_empty());
    let media = cfg.media_limits(req.control.attachment_text_chars);
    let history_allowlist = media::history_allowlist(&req.messages, last_user_idx);

    let mut messages = Vec::new();
    if !req.system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }
    let model = cfg.model(&req.settings.model);
    let mut pending_media = Vec::new();
    for (i, m) in req.messages.iter().enumerate() {
        messages.push(wire_message(
            m,
            model,
            &media,
            last_user_idx == Some(i),
            i,
            &history_allowlist,
            m.native(&req.settings.provider, &req.settings.model),
        ));
        if m.role == Role::Tool && !m.media.is_empty() {
            let mut media_message = m.clone();
            media_message.content = format!(
                "Media returned by tool {} (external content):",
                m.tool_name.as_deref().unwrap_or("MCP")
            );
            pending_media.push(json!({"role":"user","content":user_parts(&media_message,model,&media,last_user_idx==Some(i),i,&history_allowlist)}));
        }
        if req
            .messages
            .get(i + 1)
            .is_none_or(|next| next.role != Role::Tool)
        {
            messages.append(&mut pending_media);
        }
    }
    // Price is accounting data, not a wire capability. Explicit markers are
    // offered by any endpoint whose model catalogue declares cache_control;
    // automatic cache providers need no marker. Never put blocks/markers inside
    // role:tool: gateways wrap that content in tool_result, where descendants
    // cannot carry breakpoints. Only system/user text is a portable carrier.
    if let Some(model) = model {
        if model.supports_parameter("prompt_cache_breakpoint") == Some(true) {
            super::cache::openai(
                &mut messages,
                "prompt_cache_breakpoint",
                json!({"mode":"explicit"}),
            );
        } else if model.supports_parameter("cache_control") == Some(true) {
            super::cache::openai(&mut messages, "cache_control", json!({"type":"ephemeral"}));
        }
    }
    if req.control.role == Some("classifier") {
        for message in messages.iter_mut().filter(|m| m["role"] == "user") {
            if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if let Some(fields) = block.as_object_mut() {
                        fields.remove("cache_control");
                        fields.remove("prompt_cache_breakpoint");
                    }
                }
            }
        }
    }
    if model.is_some_and(|m| m.supports_parameter("prompt_cache_key") == Some(true)) {
        body.insert(
            "prompt_cache_key".into(),
            json!(crate::checkpoint::objects::hash_of(req.system.as_bytes())),
        );
    }
    body.insert("messages".into(), Value::Array(messages));

    if !req.tools.is_empty() && model.is_none_or(|m| m.supports_parameter("tools") != Some(false)) {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(name) = req.control.force_tool.as_deref().filter(|_| {
            cfg.force_tool_choice
                && model.is_none_or(|m| m.supports_parameter("tool_choice") != Some(false))
        }) {
            body.insert(
                "tool_choice".into(),
                json!({"type":"function","function":{"name":name}}),
            );
            if model.is_none_or(|m| m.supports_parameter("parallel_tool_calls") != Some(false)) {
                body.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    Value::Object(body)
}

/// One stored message in the shape the wire expects.
///
/// The assistant branch is the one that matters: an assistant turn that asked
/// for tools must carry those calls, or the `tool` messages that follow have
/// nothing to attach to and the request is rejected outright.
fn wire_message(
    m: &Message,
    model: Option<&ModelInfo>,
    provider: &media::MediaLimits,
    is_current: bool,
    msg_idx: usize,
    history_allowlist: &std::collections::HashSet<(usize, usize)>,
    native: Option<&Value>,
) -> Value {
    match m.role {
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.content,
        }),
        // A user turn with files becomes the OpenAI multimodal array; without
        // them it stays a plain string, because every endpoint takes that and
        // not all of them take an array of one text block.
        Role::User if !m.media.is_empty() => {
            json!({
                "role": "user",
                "content": user_parts(m, model, provider, is_current, msg_idx, history_allowlist)
            })
        }
        // A context row is Rune's, not the user's, but no endpoint has another
        // place for text mid-conversation; it carries no media.
        Role::User | Role::Context => json!({ "role": "user", "content": m.content }),
        Role::Assistant => {
            let mut out = Map::new();
            out.insert("role".into(), json!("assistant"));
            out.insert("content".into(), json!(m.content));
            if let Some(native) = native.and_then(Value::as_object) {
                for key in ["reasoning_content", "reasoning", "reasoning_details"] {
                    if let Some(value) = native.get(key) {
                        out.insert(key.into(), value.clone());
                    }
                }
            }
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> =
                    m.tool_calls
                        .iter()
                        .map(|c| {
                            let mut call = json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.args },
                            });
                            if let Some(native) = native {
                                let index = native
                                    .get("tool_ids")
                                    .and_then(Value::as_object)
                                    .and_then(|ids| {
                                        ids.iter()
                                            .find(|(_, id)| id.as_str() == Some(c.id.as_str()))
                                            .map(|(index, _)| index)
                                    });
                                if let Some(extra) = index.and_then(|index| {
                                    native.get("tool_extra_content").and_then(|m| m.get(index))
                                }) {
                                    call["extra_content"] = extra.clone();
                                }
                            }
                            call
                        })
                        .collect();
                out.insert("tool_calls".into(), Value::Array(calls));
            }
            Value::Object(out)
        }
        Role::Error => Value::Null,
    }
}

// ─── Response assembly ────────────────────────────────────────────────────────

/// Fold one `tool_calls[]` delta into the shared accumulator.
///
/// The whole of the OpenAI-specific knowledge about tool streaming: where the
/// index, the id, the name and the argument text live in a frame. What is done
/// with them is [`Assembly`]'s, and Anthropic's parser calls the same two
/// methods from a completely different frame shape.
fn absorb_call(calls: &mut Assembly, piece: &Value) -> Option<Chunk> {
    let index = piece.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let function = piece.get("function");
    calls.open(
        index,
        str_field(piece, "id"),
        function.and_then(|f| str_field(f, "name")),
    );
    calls.push(
        index,
        function
            .and_then(|f| str_field(f, "arguments"))
            .unwrap_or(""),
    )
}

fn usage_of(value: &Value) -> Option<Chunk> {
    let usage = value.get("usage")?.as_object()?;
    let input = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // Where every OpenAI-compatible endpoint puts the cache hit, MiMo included.
    // Absent is not zero in principle, but it is the only reading available and
    // it errs towards charging full price rather than pretending to save money.
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    if !usage.contains_key("prompt_tokens") && !usage.contains_key("completion_tokens") {
        return None;
    }
    let details = super::UsageDetails {
        measured: true,
        input_known: usage.get("prompt_tokens").is_some_and(Value::is_u64),
        output_known: usage.get("completion_tokens").is_some_and(Value::is_u64),
        // Gateways name the cache write differently: `cache_write_tokens`
        // (OpenRouter), `cache_creation_tokens` (9router, as Claude's
        // `cache_creation_input_tokens` translated) — the same count.
        cache_write: value
            .pointer("/usage/prompt_tokens_details/cache_write_tokens")
            .or_else(|| usage.get("cache_write_tokens"))
            .or_else(|| value.pointer("/usage/prompt_tokens_details/cache_creation_tokens"))
            .and_then(Value::as_u64),
        reasoning: value
            .pointer("/usage/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        provider_cost: usage
            .get("cost")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0),
        request_id: value.get("id").and_then(Value::as_str).map(String::from),
        native: Value::Object(usage.clone()),
        ..Default::default()
    };
    Some(Chunk::Usage {
        input,
        output,
        cached,
        details,
    })
}

/// A string field that is present, a string, and not empty.
///
/// All three matter: these streams are full of explicit `null`s and empty
/// openers, and treating one as content produces a stray event per chunk.
/// One user turn as OpenAI content blocks.
///
/// The chain is the Clode server's, in the same order and for the same reasons
/// (`CipherAIService.buildContents`): tagged attachments first, in upload order,
/// then the user's text. Each dialect only translates the canonical parts.
fn user_parts(
    m: &Message,
    model: Option<&ModelInfo>,
    provider: &media::MediaLimits,
    is_current: bool,
    msg_idx: usize,
    history_allowlist: &std::collections::HashSet<(usize, usize)>,
) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for part in media::parts_for_message(
        &m.media,
        &m.content,
        model,
        crate::settings::providers::Dialect::OpenAi,
        provider,
        is_current,
        msg_idx,
        history_allowlist,
    ) {
        out.push(match part {
            media::Inlined::Text(text) => json!({ "type": "text", "text": text }),
            media::Inlined::Notice(text) => json!({ "type": "text", "text": text }),
            media::Inlined::Bytes {
                modality,
                mime,
                data,
                name,
            } => match modality {
                "image" => json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{data}") }
                }),
                "audio" => json!({
                    "type": "input_audio",
                    "input_audio": { "data": data, "format": audio_format(&mime) }
                }),
                "video" => json!({
                    "type": "video_url",
                    "video_url": { "url": format!("data:{mime};base64,{data}") }
                }),
                // A PDF, which this shape carries as a named file rather than a
                // data URL.
                _ => json!({
                    "type": "file",
                    "file": {
                        "filename": name,
                        "file_data": format!("data:{mime};base64,{data}")
                    }
                }),
            },
        });
    }
    Value::Array(out)
}

/// What `input_audio` calls this container. The block takes a bare format name,
/// not a MIME type, and the two do not match for half of them.
fn audio_format(mime: &str) -> &'static str {
    match mime {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/aac" => "aac",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        _ => "wav",
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::CallRef;
    use super::*;
    use crate::settings::providers::preset;
    use crate::settings::ModelSettings;

    fn request() -> Request {
        let mut cfg = preset("mimo").unwrap().build("sk-x");
        cfg.models.push(ModelInfo::named("mimo-v2.5-pro", true));
        Request {
            control: Default::default(),
            system: "sys".into(),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            settings: ModelSettings::for_provider(&cfg),
        }
    }

    /// A job whose answer is one call names it; the endpoint is told to make
    /// that call where it accepts being told. The agent is never forced.
    #[test]
    fn a_forced_tool_is_the_one_the_request_names() {
        let mut cfg = preset("mimo").unwrap().build("sk-x");
        cfg.force_tool_choice = true;
        let mut req = request();
        req.tools = vec![crate::tools::ToolDecl {
            name: "set_title".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        assert!(
            body(&req, &cfg).get("tool_choice").is_none(),
            "nothing asked"
        );
        req.control.force_tool = Some("set_title".into());
        assert_eq!(
            body(&req, &cfg)["tool_choice"],
            json!({"type":"function","function":{"name":"set_title"}})
        );
        cfg.force_tool_choice = false;
        assert!(
            body(&req, &cfg).get("tool_choice").is_none(),
            "endpoint opts out"
        );
    }

    /// Behind a gateway that caches only at markers, the marked message must
    /// reach the provider in the same shape in the next request, unmarked: the
    /// newest user message and tool result become blocks in every request, not
    /// only in the one that marks them.
    #[test]
    fn a_marked_message_keeps_its_shape_in_the_next_request() {
        let mut cfg = preset("mimo").unwrap().build("sk-x");
        let mut model = ModelInfo::named("mimo-v2.5-pro", true);
        model.supported_parameters.push("cache_control".into());
        cfg.models.push(model);
        let mut req = request();
        req.settings.model = "mimo-v2.5-pro".into();
        let call = CallRef {
            id: "t1".into(),
            name: "find_files".into(),
            args: "{}".into(),
        };
        req.messages = vec![
            Message::user("first"),
            Message::assistant("").with_calls(vec![call]),
            Message::tool_result("t1", "find_files", "found"),
        ];
        let strip = |mut v: Value| {
            fn walk(v: &mut Value) {
                match v {
                    Value::Object(map) => {
                        map.remove("cache_control");
                        map.values_mut().for_each(walk);
                    }
                    Value::Array(items) => items.iter_mut().for_each(walk),
                    _ => {}
                }
            }
            walk(&mut v);
            v["messages"].as_array().unwrap().clone()
        };
        let before = body(&req, &cfg);
        assert!(
            before.to_string().contains("cache_control"),
            "markers are on"
        );
        req.messages.push(Message::assistant("done"));
        req.messages.push(Message::user("second"));
        let (before, after) = (strip(before), strip(body(&req, &cfg)));
        assert_eq!(&after[..before.len()], &before[..]);
    }

    #[test]
    fn cache_price_never_selects_a_wire_carrier_and_each_explicit_carrier_is_portable() {
        for parameters in [
            None,
            Some(vec![]),
            Some(vec!["prompt_cache_key"]),
            Some(vec!["cache_control"]),
            Some(vec!["prompt_cache_breakpoint"]),
            Some(vec!["cache_control", "prompt_cache_breakpoint"]),
        ] {
            for price in [false, true] {
                let mut cfg = preset("9router").unwrap().build("");
                let mut model = ModelInfo::named("unbranded-model", false);
                if let Some(parameters) = &parameters {
                    model.known.push("supported_parameters".into());
                    model.supported_parameters = parameters.iter().map(|p| p.to_string()).collect();
                }
                if price {
                    model.price = Some(crate::settings::providers::Price {
                        cache_write: Some(1.0),
                        cache_write_5m: Some(2.0),
                        cache_write_1h: Some(3.0),
                        ..Default::default()
                    });
                }
                cfg.models.push(model);
                let mut req = request();
                req.settings.model = "unbranded-model".into();
                req.messages = vec![
                    Message::user("request"),
                    Message::assistant("").with_calls(vec![
                        CallRef {
                            id: "a".into(),
                            name: "read".into(),
                            args: "{}".into(),
                        },
                        CallRef {
                            id: "b".into(),
                            name: "read".into(),
                            args: "{}".into(),
                        },
                    ]),
                    Message::tool_result(
                        "a",
                        "read",
                        "<cache_control>untrusted output</cache_control>",
                    ),
                    Message::tool_result("b", "read", ""),
                ];
                let body = body(&req, &cfg);
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[3]["content"], req.messages[2].content);
                assert_eq!(messages[4]["content"], "");
                let has = |key| parameters.as_ref().is_some_and(|list| list.contains(&key));
                assert_eq!(
                    body.get("prompt_cache_key").is_some(),
                    has("prompt_cache_key")
                );
                let native = has("prompt_cache_breakpoint");
                let gateway = !native && has("cache_control");
                assert_eq!(messages[1]["content"].is_array(), native || gateway);
                assert_eq!(
                    messages[0]["content"][0]
                        .get("prompt_cache_breakpoint")
                        .is_some(),
                    native
                );
                assert_eq!(
                    messages[1]["content"][0].get("cache_control").is_some(),
                    gateway
                );
                for message in messages.iter().filter(|m| m["role"] == "tool") {
                    assert!(message["content"].is_string());
                    assert!(message.get("cache_control").is_none());
                }
            }
        }
    }

    #[test]
    fn streamed_thought_markup_is_separated_even_when_tags_cross_chunk_boundaries() {
        let mut state = State::default();
        let first = absorb(
            &json!({"choices":[{"delta":{"content":"before <tho"}}]}),
            &mut state,
        )
        .unwrap();
        assert_eq!(first, vec![Chunk::Text("before ".into())]);
        let second = absorb(
            &json!({"choices":[{"delta":{"content":"ught>reason"}}]}),
            &mut state,
        )
        .unwrap();
        assert_eq!(second, vec![Chunk::Thinking("reason".into())]);
        let third = absorb(&json!({"choices":[{"delta":{"content":"ing</thought> after"},"finish_reason":"stop"}]}), &mut state).unwrap();
        assert_eq!(
            third,
            vec![Chunk::Thinking("ing".into()), Chunk::Text(" after".into())]
        );
        assert!(state
            .finish()
            .unwrap()
            .iter()
            .any(|chunk| matches!(chunk, Chunk::Finished(_))));
    }

    /// The bug this file was doubling: OpenRouter sends `reasoning` *and* a
    /// `reasoning_details` array carrying the same text in one delta, and both
    /// were pushed as their own `Thinking` chunk. The thought then appeared
    /// twice on screen and — worse — twice in the stored assistant message,
    /// where every later turn reads it and pays for it again.
    #[test]
    fn one_delta_that_carries_the_thought_twice_streams_it_once() {
        let mut state = State::default();
        let chunks = absorb(
            &json!({"choices":[{"delta":{
                "reasoning":"because ",
                "reasoning_details":[{"type":"reasoning.text","index":0,"text":"because "}]
            }}]}),
            &mut state,
        )
        .unwrap();
        assert_eq!(
            chunks,
            vec![Chunk::Thinking("because ".into())],
            "one delta is one thought, however many fields carry it"
        );

        absorb(
            &json!({"choices":[{"delta":{
                "reasoning":"reasons",
                "reasoning_details":[{"type":"reasoning.text","index":0,"text":"reasons"}]
            }}]}),
            &mut state,
        )
        .unwrap();

        let wire = state.continuation.clone();
        assert_eq!(wire["reasoning"], json!("because reasons"));
        assert_eq!(
            wire["reasoning_details"][0]["text"],
            json!("because reasons"),
            "the details array is what the next turn replays, so it is still accumulated"
        );
    }

    /// A provider that sends only `reasoning_details` still streams its thoughts.
    #[test]
    fn reasoning_details_alone_still_reach_the_screen() {
        let mut state = State::default();
        let chunks = absorb(
            &json!({"choices":[{"delta":{
                "reasoning_details":[{"type":"reasoning.text","index":0,"text":"only this"}]
            }}]}),
            &mut state,
        )
        .unwrap();
        assert_eq!(chunks, vec![Chunk::Thinking("only this".into())]);
    }

    #[test]
    fn unterminated_thought_markup_flushes_as_thinking_without_leaking_the_tag() {
        let mut state = State::default();
        let chunks = absorb(
            &json!({"choices":[{"delta":{"content":"<thought>private"}}]}),
            &mut state,
        )
        .unwrap();
        assert_eq!(chunks, vec![Chunk::Thinking("private".into())]);
        assert!(!state
            .finish()
            .unwrap()
            .iter()
            .any(|chunk| matches!(chunk, Chunk::Text(text) if text.contains("thought"))));
    }

    #[test]
    fn sparse_usage_and_structured_reasoning_survive_a_tool_round() {
        let mut state = State::default();
        absorb(&json!({"id":"request","usage":{"prompt_tokens":100,"completion_tokens":1,"prompt_cache_hit_tokens":90},"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"why"}],"tool_calls":[{"index":7,"id":"call","extra_content":{"google":{"thought_signature":"signature"}},"function":{"name":"echo","arguments":"{}"}}]}}]}),&mut state).unwrap();
        absorb(&json!({"usage":{"completion_tokens":2},"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":" now"}]},"finish_reason":"tool_calls"}]}),&mut state).unwrap();
        assert!(matches!(
            &state.usage,
            Some(Chunk::Usage {
                input: 100,
                output: 2,
                cached: 90,
                ..
            })
        ));
        let data = Value::Object(state.continuation.clone());
        let calls = state
            .finish()
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                Chunk::ToolCall { id, name, args } => Some(CallRef {
                    id,
                    name,
                    args: args.to_string(),
                }),
                _ => None,
            })
            .collect();
        let mut message = Message::assistant("").with_calls(calls);
        message.continuation = Some(super::super::Continuation {
            provider: "p".into(),
            model: "m".into(),
            data,
        });
        let wire = wire_message(
            &message,
            None,
            &media::MediaLimits::default(),
            false,
            0,
            &Default::default(),
            message.native("p", "m"),
        );
        assert_eq!(wire["reasoning_details"][0]["text"], "why now");
        assert_eq!(
            wire["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "signature"
        );
        let other = wire_message(
            &message,
            None,
            &media::MediaLimits::default(),
            false,
            0,
            &Default::default(),
            message.native("other", "m"),
        );
        assert!(other.get("reasoning_details").is_none());
        assert!(other["tool_calls"][0].get("extra_content").is_none());
    }

    #[test]
    fn the_body_carries_the_model_the_stream_flag_and_the_system_message() {
        let mut cfg = preset("mimo").unwrap().build("sk-x");
        cfg.models.push(ModelInfo::named("mimo-v2.5-pro", true));
        let b = body(&request(), &cfg);
        assert_eq!(b["model"], json!("mimo-v2.5-pro"));
        assert_eq!(b["stream"], json!(true));
        assert_eq!(b["messages"][0]["role"], json!("system"));
        assert_eq!(b["messages"][1]["content"], json!("hi"));
        assert_eq!(b["thinking"], json!({"type": "enabled"}));
    }

    #[test]
    fn an_assistant_turn_that_called_a_tool_replays_with_the_call_attached() {
        // Without this the next request is a `tool` message with nothing to
        // answer, which a real provider rejects outright.
        let m = Message::assistant("").with_calls(vec![CallRef {
            id: "call_1".into(),
            name: "create_file".into(),
            args: r#"{"path":"a.txt"}"#.into(),
        }]);
        let empty = std::collections::HashSet::new();
        let wire = wire_message(
            &m,
            None,
            &media::MediaLimits::default(),
            true,
            0,
            &empty,
            None,
        );
        assert_eq!(wire["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(
            wire["tool_calls"][0]["function"]["name"],
            json!("create_file")
        );
        assert_eq!(
            wire["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"a.txt"}"#)
        );
    }

    #[test]
    fn reasoning_is_not_replayed_in_assistant_wire_payload() {
        let mut m = Message::assistant("done");
        m.reasoning = Some("because".into());
        let empty = std::collections::HashSet::new();
        let wire = wire_message(
            &m,
            None,
            &media::MediaLimits::default(),
            true,
            0,
            &empty,
            None,
        );
        assert!(wire.get("reasoning_content").is_none());
        assert_eq!(wire["content"], json!("done"));
    }

    #[test]
    fn a_tool_result_replays_with_the_id_that_ties_it_to_its_call() {
        let m = Message::tool_result("call_9", "patch_file", "ok");
        let empty = std::collections::HashSet::new();
        let wire = wire_message(
            &m,
            None,
            &media::MediaLimits::default(),
            true,
            0,
            &empty,
            None,
        );
        assert_eq!(wire["role"], json!("tool"));
        assert_eq!(wire["tool_call_id"], json!("call_9"));
    }

    #[test]
    fn arguments_assemble_from_deltas_and_report_progress_on_the_way() {
        let mut a = Assembly::default();
        let first = absorb_call(
            &mut a,
            &json!({"index":0,"id":"c1","function":{"name":"create_file","arguments":""}}),
        );
        assert!(
            first.is_none(),
            "an opener with no argument text is not progress"
        );
        let mid = absorb_call(
            &mut a,
            &json!({"index":0,"function":{"arguments":"{\"path\": "}}),
        );
        match mid {
            Some(Chunk::ToolArgs {
                id,
                name,
                delta,
                index,
            }) => {
                assert_eq!(
                    (id.as_str(), name.as_str(), index),
                    ("c1", "create_file", 0)
                );
                assert_eq!(delta, "{\"path\": ");
            }
            other => panic!("expected progress, got {other:?}"),
        }
        absorb_call(
            &mut a,
            &json!({"index":0,"function":{"arguments":"\"a.txt\"}"}}),
        );
        let done = a.finish().unwrap();
        assert_eq!(done.len(), 1);
        match &done[0] {
            Chunk::ToolCall { id, name, args } => {
                assert_eq!((id.as_str(), name.as_str()), ("c1", "create_file"));
                assert_eq!(args["path"], json!("a.txt"));
            }
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn two_interleaved_calls_stay_separate_and_come_back_in_index_order() {
        let mut a = Assembly::default();
        absorb_call(
            &mut a,
            &json!({"index":1,"id":"b","function":{"name":"two","arguments":"{\"n\":2}"}}),
        );
        absorb_call(
            &mut a,
            &json!({"index":0,"id":"a","function":{"name":"one","arguments":"{\"n\":1}"}}),
        );
        let done = a.finish().unwrap();
        let names: Vec<&str> = done
            .iter()
            .map(|c| match c {
                Chunk::ToolCall { name, .. } => name.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(names, vec!["one", "two"]);
    }

    #[test]
    fn a_call_with_no_arguments_at_all_still_becomes_an_empty_object() {
        let mut a = Assembly::default();
        absorb_call(
            &mut a,
            &json!({"index":0,"id":"c","function":{"name":"track_tasks"}}),
        );
        match &a.finish().unwrap()[0] {
            Chunk::ToolCall { args, .. } => assert_eq!(args, &json!({})),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arguments_that_are_not_json_name_the_tool_and_do_not_echo_the_payload() {
        let mut a = Assembly::default();
        absorb_call(
            &mut a,
            &json!({"index":0,"id":"c","function":{"name":"create_file","arguments":"{oops"}}),
        );
        let chunks = a.finish().unwrap();
        match &chunks[0] {
            Chunk::InvalidToolCall { id, name, reason } => {
                assert_eq!(id, "c");
                assert_eq!(name, "create_file");
                assert!(reason.contains("no operation was executed"));
                assert!(!reason.contains("{oops"));
            }
            other => panic!("expected recoverable invalid call, got {other:?}"),
        }
    }

    /// The cache hit is what a session's cost turns on: MiMo charges a cached
    /// prompt token at 0.8% of a fresh one.
    #[test]
    fn the_cache_hit_is_read_from_where_every_openai_compatible_api_puts_it() {
        let usage = json!({"usage": {
            "prompt_tokens": 1000, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 900}
        }});
        match usage_of(&usage) {
            Some(Chunk::Usage { input, cached, .. }) => assert_eq!((input, cached), (1000, 900)),
            other => panic!("usage not read: {other:?}"),
        }
        // An endpoint that reports no detail is charged in full, not for free.
        match usage_of(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 3}})) {
            Some(Chunk::Usage { cached, .. }) => assert_eq!(cached, 0),
            other => panic!("usage not read: {other:?}"),
        }
    }

    /// 9router's name for the cache write is read as the cache write.
    #[test]
    fn a_cache_write_is_read_under_either_gateway_name() {
        for details in [
            json!({"cached_tokens": 900, "cache_write_tokens": 300}),
            json!({"cached_tokens": 900, "cache_creation_tokens": 300}),
        ] {
            match usage_of(
                &json!({"usage": {"prompt_tokens": 1320, "completion_tokens": 7, "prompt_tokens_details": details}}),
            ) {
                Some(Chunk::Usage {
                    cached, details, ..
                }) => {
                    assert_eq!(cached, 900);
                    assert_eq!(details.cache_write, Some(300));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn usage_is_read_from_the_final_chunk_and_ignored_when_empty() {
        assert!(usage_of(&json!({"usage": null})).is_none());
        assert!(
            usage_of(&json!({"usage": {"prompt_tokens": 0, "completion_tokens": 0}})).is_some()
        );
        match usage_of(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 3}})) {
            Some(Chunk::Usage { input, output, .. }) => assert_eq!((input, output), (10, 3)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn explicit_nulls_and_empty_strings_never_become_events() {
        let delta = json!({"content": null, "reasoning_content": "", "role": "assistant"});
        assert!(str_field(&delta, "content").is_none());
        assert!(str_field(&delta, "reasoning_content").is_none());
    }

    #[test]
    fn intermediate_usage_frames_do_not_emit_per_chunk_and_only_finish_emits_once() {
        let mut state = State::default();
        // Frame 1 with partial text and running usage
        let f1 = json!({
            "choices": [{"delta": {"content": "Hello"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 1}
        });
        let c1 = absorb(&f1, &mut state).unwrap();
        assert_eq!(c1, vec![Chunk::Text("Hello".into())]);

        // Frame 2 with more text and updated usage
        let f2 = json!({
            "choices": [{"delta": {"content": " world"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 2}
        });
        let c2 = absorb(&f2, &mut state).unwrap();
        assert_eq!(c2, vec![Chunk::Text(" world".into())]);

        // Frame 3 final frame with no delta, just final usage
        let f3 = json!({
            "choices": [],
            "usage": {"prompt_tokens": 100, "completion_tokens": 2}
        });
        let c3 = absorb(&f3, &mut state).unwrap();
        assert!(c3.is_empty());

        // Finish emits the single final usage
        let finished = state.finish().unwrap();
        assert_eq!(
            finished,
            vec![Chunk::Usage {
                input: 100,
                output: 2,
                cached: 0,
                details: super::super::UsageDetails {
                    measured: true,
                    input_known: true,
                    output_known: true,
                    native: json!({"prompt_tokens":100,"completion_tokens":2}),
                    ..Default::default()
                }
            }]
        );
    }
    #[test]
    fn mcp_media_follows_all_tool_results_and_uses_the_existing_image_encoder() {
        let root = std::env::temp_dir()
            .join("rune-test")
            .join(format!("openai-mcp-media-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Image.png");
        std::fs::write(&path,crate::media::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5E0AAAAASUVORK5CYII=").unwrap()).unwrap();
        let image = crate::media::Attachment::at(&path).unwrap();
        let mut cfg = preset("mimo").unwrap().build("fixture");
        cfg.models = vec![ModelInfo {
            id: "mimo-v2.5-pro".into(),
            modalities: vec!["image".into()],
            ..Default::default()
        }];
        let mut req = request();
        req.messages = vec![
            Message::user("Read the image"),
            Message::assistant("").with_calls(vec![
                CallRef {
                    id: "a".into(),
                    name: "mcp_image".into(),
                    args: "{}".into(),
                },
                CallRef {
                    id: "b".into(),
                    name: "mcp_text".into(),
                    args: "{}".into(),
                },
            ]),
            Message::tool_result("a", "mcp_image", "Image returned").with_media(vec![image]),
            Message::tool_result("b", "mcp_text", "Text returned"),
        ];
        let wire = body(&req, &cfg);
        let messages = wire["messages"].as_array().unwrap();
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[5]["role"], "user");
        assert!(messages[5]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "image_url"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structural_error_matrix_does_not_classify_model_text() {
        for value in [
            json!({"type":"error","message":"failed"}),
            json!({"event":"error","message":"failed"}),
            json!({"type":"response.failed","response":{"error":{"message":"failed"}}}),
            json!({"error":"failed"}),
            json!({"error":{"message":"failed","type":"server_error"}}),
            json!({"type":"error"}),
        ] {
            assert!(
                matches!(
                    absorb(&value, &mut State::default()),
                    Err(Error::Provider(_))
                ),
                "{value}"
            );
        }
        for text in [
            "[Error] An error occurred while processing your request.",
            "{\"type\":\"error\",\"error\":\"example\"}",
            "error",
            "response.failed",
        ] {
            let value =
                json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}],"error":null});
            assert_eq!(
                absorb(&value, &mut State::default()).unwrap(),
                vec![Chunk::Text(text.into())]
            );
        }
    }

    #[test]
    fn type_or_event_error_frame_fails_stream() {
        let mut state = State::default();
        let err1 = absorb(
            &json!({"type":"error","error":{"message":"overloaded"}}),
            &mut state,
        );
        assert!(matches!(err1, Err(Error::Provider(m)) if m.contains("overloaded")));

        let mut state2 = State::default();
        let err2 = absorb(
            &json!({"type":"error","message":"rate limit exceeded"}),
            &mut state2,
        );
        assert!(matches!(err2, Err(Error::Provider(m)) if m.contains("rate limit exceeded")));

        let mut state3 = State::default();
        let err3 = absorb(
            &json!({"type":"response.failed","error":{"message":"gateway timeout"}}),
            &mut state3,
        );
        assert!(matches!(err3, Err(Error::Provider(m)) if m.contains("gateway timeout")));
    }
}
