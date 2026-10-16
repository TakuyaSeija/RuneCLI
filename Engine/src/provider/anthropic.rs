//! Anthropic's `/v1/messages` — the one endpoint here that is not OpenAI-shaped.
//!
//! # Why this file exists at all
//!
//! Anthropic publishes an OpenAI-compatibility layer, and using it would have
//! saved this file. It would also have bought a translation of a translation:
//! the compatibility layer is not where thinking, tool streaming and cache
//! accounting are expressed most faithfully, and those three are exactly what
//! this engine needs from a model. So the native protocol is spoken natively.
//!
//! # What is genuinely different, and what only looks different
//!
//! Only the *shape* is different, and it is different in four specific ways:
//!
//! 1. **The system prompt is a top-level field**, not a message with a role.
//! 2. **`max_tokens` is required.** There is no "let the model decide", so a
//!    number is always sent — the model's own ceiling when it is published,
//!    and a modest default when it is not.
//! 3. **Content is blocks, not strings.** A turn that used tools is a list of
//!    `text` and `tool_use` blocks, and a tool result is a `tool_result` block
//!    inside a *user* message.
//! 4. **The stream is typed events**, `content_block_delta` and friends, where
//!    the block's index says which of several things a fragment belongs to.
//!
//! Everything above the wire — the retry rule, key rotation, cancellation,
//! delivering assembled calls, the accumulator itself — is shared with
//! [`super::openai`] through [`super::live`] and [`super::assembly`]. This file
//! is a translator and nothing else.
//!
//! Signed native thinking blocks are persisted and replayed unchanged only to
//! their original provider/model. Unsupported sampling parameters are omitted.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use super::assembly::Assembly;
use super::{Chunk, Message, Request};
use crate::error::{Error, Result};
use crate::media;
use crate::settings::providers::{ModelInfo, ProviderConfig};
use crate::store::sessions::Role;

/// The floor Anthropic requires of a thinking budget.
pub(crate) const MIN_THINKING_BUDGET: u64 = 1_024;

pub(crate) fn endpoint(cfg: &ProviderConfig) -> String {
    format!("{}/messages", cfg.base_url.trim_end_matches('/'))
}

// ─── Request shaping ──────────────────────────────────────────────────────────

pub(crate) fn body(req: &Request, cfg: &ProviderConfig) -> Value {
    let s = &req.settings;
    let mut body = Map::new();
    body.insert("model".into(), json!(s.model));
    body.insert("stream".into(), json!(true));
    body.insert("system".into(), json!(req.system));

    let model = cfg.model(&s.model);
    let ceiling = model.and_then(|m| m.max_output);
    let context = model.and_then(|m| m.context);
    let auto_smart = crate::settings::model::smart_max_output(context, ceiling);
    let max_tokens = s
        .max_output
        .unwrap_or_else(|| ceiling.map_or(auto_smart, |limit| auto_smart.min(limit)));
    body.insert("max_tokens".into(), json!(max_tokens));
    let thinking = (s.thinking || model.is_some_and(|m| m.thinking_required))
        && model.is_none_or(|m| m.thinking);
    if !s.is_suppressed(cfg, "temperature") {
        if let Some(t) = s.temperature {
            body.insert("temperature".into(), json!(t));
        }
    }
    if !s.is_suppressed(cfg, "top_p") && s.temperature.is_none() {
        if let Some(p) = s.top_p {
            body.insert("top_p".into(), json!(p));
        }
    }
    if !s.stop.is_empty() && !s.is_suppressed(cfg, "stop") {
        body.insert("stop_sequences".into(), json!(s.stop));
    }
    if thinking {
        if model.is_some_and(|m| m.thinking_modes.iter().any(|mode| mode == "adaptive")) {
            body.insert("thinking".into(), json!({"type":"adaptive"}));
            if let Some(effort) = s
                .effort
                .as_ref()
                .filter(|e| model.is_some_and(|m| m.efforts.contains(e)))
            {
                body.insert("output_config".into(), json!({"effort":effort}));
            }
        } else {
            // No budget-bound discovery API is available; documented manual-thinking
            // minimum as of 2026-09-14. The checked shaping boundary rejects smaller outputs.
            let raw = s
                .effort
                .as_ref()
                .and_then(|e| e.parse::<u64>().ok())
                .unwrap_or(max_tokens / 2)
                .max(MIN_THINKING_BUDGET);
            let budget = raw.min(max_tokens.saturating_sub(1));
            body.insert(
                "thinking".into(),
                json!({"type":"enabled","budget_tokens":budget}),
            );
        }
    }
    let media = cfg.media_limits(req.control.attachment_text_chars);
    let mut messages = conversation(
        &req.messages,
        cfg.model(&req.settings.model),
        Some(&media),
        Some((&req.settings.provider, &req.settings.model)),
    );
    // The Messages API caches only at markers. A catalogue that lists its
    // parameters and leaves `cache_control` out opts this model out.
    if model.is_none_or(|m| m.supports_parameter("cache_control") != Some(false)) {
        if !req.system.is_empty() {
            body.insert("system".into(), super::cache::system(&req.system));
        }
        if req.control.role != Some("classifier") {
            if let Some(last) = messages.last_mut() {
                super::cache::mark_last(last);
            }
        }
    }
    body.insert("messages".into(), Value::Array(messages));

    if !req.tools.is_empty() && model.is_none_or(|m| m.supports_parameter("tools") != Some(false)) {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(name) = req.control.force_tool.as_deref().filter(|_| {
            cfg.force_tool_choice
                && !thinking
                && model.is_none_or(|m| m.supports_parameter("tool_choice") != Some(false))
        }) {
            body.insert("tool_choice".into(), json!({"type":"tool","name":name}));
        }
    }
    Value::Object(body)
}

/// Add user-side blocks: to the user message already last, or as a new one.
///
/// Always blocks, never a string. A user message is marked for the cache in the
/// request where it is newest and must keep the same shape in every request
/// after; and when a second user row follows the first (a turn that failed
/// before the model answered), it extends the message by a block instead of
/// rewriting text the provider already cached.
fn push_user(out: &mut Vec<Value>, blocks: Vec<Value>) {
    if let Some(list) = out
        .last_mut()
        .filter(|last| last["role"] == json!("user"))
        .and_then(|last| last["content"].as_array_mut())
    {
        list.extend(blocks);
        return;
    }
    out.push(json!({ "role": "user", "content": blocks }));
}

/// The transcript, in blocks, with consecutive tool results folded together.
///
/// The folding is not cosmetic. Anthropic requires the roles to alternate, and a
/// turn in which the model called three tools produces three stored `Tool`
/// messages in a row. Sent as three user messages the request is rejected; sent
/// as one user message with three `tool_result` blocks it is exactly what the
/// protocol describes.
fn conversation(
    messages: &[Message],
    model: Option<&ModelInfo>,
    provider: Option<&media::MediaLimits>,
    route: Option<(&str, &str)>,
) -> Vec<Value> {
    let fallback = media::MediaLimits::default();
    let prov = provider.unwrap_or(&fallback);
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.role == Role::User || !m.media.is_empty());
    let history_allowlist = media::history_allowlist(messages, last_user_idx);

    let mut out: Vec<Value> = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        match m.role {
            Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": if m.media.is_empty(){json!(m.content)}else{json!(user_blocks(m,model,prov,last_user_idx==Some(i),i,&history_allowlist))},
                });
                push_user(&mut out, vec![block]);
            }
            Role::User if !m.media.is_empty() => {
                let is_current = last_user_idx == Some(i);
                push_user(
                    &mut out,
                    user_blocks(m, model, prov, is_current, i, &history_allowlist),
                );
            }
            // A context row is Rune's, not the user's, but the user's turn is the
            // only place for text mid-conversation; it joins that turn as a
            // block of its own.
            Role::User | Role::Context => {
                push_user(&mut out, vec![json!({ "type": "text", "text": m.content })]);
            }
            Role::Assistant => {
                if let Some(blocks) = route
                    .and_then(|(p, id)| m.native(p, id))
                    .and_then(|n| n.get("content"))
                    .and_then(Value::as_array)
                {
                    if !blocks.is_empty() {
                        out.push(json!({"role":"assistant","content":blocks}));
                        continue;
                    }
                }
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.trim().is_empty() {
                    blocks.push(json!({ "type": "text", "text": m.content }));
                }
                for c in &m.tool_calls {
                    // Arguments were stored as the model wrote them; the wire
                    // wants a value. Text that will not parse is dropped rather
                    // than sent as a string, which the endpoint rejects.
                    let Ok(input) = serde_json::from_str::<Value>(&c.args) else {
                        continue;
                    };
                    blocks.push(
                        json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": input }),
                    );
                }
                if blocks.is_empty() {
                    continue;
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            Role::Error => {}
        }
    }
    out
}

/// One user turn as Anthropic content blocks.
///
/// The same decision as every other dialect makes — [`media::decide`] — rendered
/// into this protocol's shapes: an image is a `source` of base64, a PDF is a
/// `document`, and everything else is text, whether that text is the file's own
/// contents or the line explaining why it could not be sent.
fn user_blocks(
    m: &Message,
    model: Option<&ModelInfo>,
    provider: &media::MediaLimits,
    is_current: bool,
    msg_idx: usize,
    history_allowlist: &std::collections::HashSet<(usize, usize)>,
) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    for part in media::parts_for_message(
        &m.media,
        &m.content,
        model,
        crate::settings::providers::Dialect::Anthropic,
        provider,
        is_current,
        msg_idx,
        history_allowlist,
    ) {
        blocks.push(match part {
            media::Inlined::Text(text) | media::Inlined::Notice(text) => {
                json!({ "type": "text", "text": text })
            }
            media::Inlined::Bytes {
                modality,
                mime,
                data,
                ..
            } => match modality {
                "image" => json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": mime, "data": data }
                }),
                // Eligibility has already restricted this dialect to images
                // and PDFs, in the same place the composer reads its status.
                _ => json!({
                    "type": "document",
                    "source": { "type": "base64", "media_type": mime, "data": data }
                }),
            },
        });
    }
    blocks
}

// ─── Response assembly ────────────────────────────────────────────────────────

/// What a content block at a given index turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text,
    Thinking,
    ToolUse,
}

/// What must be remembered between frames of one Anthropic stream.
///
/// The block kinds are the whole reason this is not stateless: a
/// `content_block_delta` says which index it belongs to and not what that index
/// is, so the meaning of a fragment was decided one event earlier.
#[derive(Default)]
pub struct State {
    pub(crate) calls: Assembly,
    kinds: BTreeMap<u32, Block>,
    pub(crate) usage: Option<Chunk>,
    pub(crate) completion: Option<super::Completion>,
    pub(crate) blocks: BTreeMap<u32, Value>,
    open_blocks: std::collections::BTreeSet<u32>,
    partial_inputs: BTreeMap<u32, String>,
    details: super::UsageDetails,
}

impl State {
    fn update_usage(&mut self, update: &Value) -> Result<()> {
        if !update.is_object() {
            return Err(Error::Provider("invalid Anthropic usage object".into()));
        }
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            if update
                .get(key)
                .is_some_and(|v| !v.is_null() && v.as_u64().is_none_or(|n| n > i64::MAX as u64))
            {
                return Err(Error::Provider("invalid Anthropic usage count".into()));
            }
        }
        super::merge_usage(&mut self.details.native, update);
        self.details.measured = true;
        let usage = &self.details.native;
        self.details.input_known = usage.get("input_tokens").is_some_and(Value::is_u64);
        self.details.output_known = usage.get("output_tokens").is_some_and(Value::is_u64);
        self.details.cache_write_5m = usage
            .pointer("/cache_creation/ephemeral_5m_input_tokens")
            .and_then(Value::as_u64);
        self.details.cache_write_1h = usage
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(Value::as_u64);
        self.details.cache_write = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64);
        let written = self.details.cache_write.unwrap_or(
            self.details
                .cache_write_5m
                .unwrap_or(0)
                .saturating_add(self.details.cache_write_1h.unwrap_or(0)),
        );
        let cached = number(Some(usage), "cache_read_input_tokens");
        let input = number(Some(usage), "input_tokens")
            .saturating_add(cached)
            .saturating_add(written);
        self.usage = Some(Chunk::Usage {
            input,
            output: number(Some(usage), "output_tokens"),
            cached,
            details: self.details.clone(),
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<Chunk>> {
        if !self.open_blocks.is_empty() {
            return Err(Error::Provider(
                "Anthropic stream ended with unfinished content blocks".into(),
            ));
        }
        let mut out = self.calls.finish()?;
        if let Some(usage) = self.usage.take() {
            out.push(usage);
        }
        if let Some(completion) = self.completion.take() {
            out.push(Chunk::Finished(completion));
        }
        Ok(out)
    }
}

pub(crate) fn absorb(value: &Value, state: &mut State) -> Result<Vec<Chunk>> {
    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let mut out = Vec::new();
    if value
        .get("index")
        .is_some_and(|v| v.as_u64().is_none_or(|n| n > u32::MAX as u64))
    {
        return Err(Error::Provider("invalid content block index".into()));
    }

    match kind {
        "error" => {
            let message = value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("the provider reported an error mid-stream");
            return Err(Error::Provider(message.to_string()));
        }
        "message_start" => {
            state.details.request_id = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .map(String::from);
            if let Some(usage) = value.pointer("/message/usage") {
                state.update_usage(usage)?;
            }
        }
        "content_block_start" => {
            let block = value.get("content_block");
            if block.is_none_or(|b| !b.is_object())
                || !state.open_blocks.insert(index)
                || state.blocks.contains_key(&index)
            {
                return Err(Error::Provider(
                    "invalid or repeated content block start".into(),
                ));
            }
            if let Some(block) = block {
                state.blocks.insert(index, block.clone());
            }
            let block_type = block.and_then(|b| b.get("type")).and_then(|v| v.as_str());
            match block_type {
                Some("tool_use") => {
                    state.kinds.insert(index, Block::ToolUse);
                    state.calls.open(
                        index,
                        block.and_then(|b| b.get("id")).and_then(|v| v.as_str()),
                        block.and_then(|b| b.get("name")).and_then(|v| v.as_str()),
                    );
                    if let Some(input) = block
                        .and_then(|b| b.get("input"))
                        .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
                    {
                        out.extend(state.calls.push(index, &input.to_string()));
                    }
                }
                Some("thinking") | Some("redacted_thinking") => {
                    state.kinds.insert(index, Block::Thinking);
                }
                _ => {
                    state.kinds.insert(index, Block::Text);
                }
            }
            if let Some(t) = text(block, "text") {
                out.push(Chunk::Text(t));
            }
            if let Some(t) = text(block, "thinking") {
                out.push(Chunk::Thinking(t));
            }
        }
        "content_block_delta" => {
            let delta = value.get("delta");
            if delta.is_none_or(|v| !v.is_object()) {
                return Err(Error::Provider("invalid content block delta".into()));
            }
            // The delta's own type is the authority when it has one; the block
            // kind recorded at `content_block_start` is the fallback for a
            // fragment that does not repeat it.
            let delta_type = delta.and_then(|d| d.get("type")).and_then(|v| v.as_str());
            if !state.open_blocks.contains(&index) {
                return Err(Error::Provider(
                    "content delta refers to an unopened or closed block".into(),
                ));
            }
            if let Some(delta) = delta.and_then(Value::as_object) {
                for key in ["text", "thinking", "signature"] {
                    if let Some(fragment) = delta.get(key).and_then(Value::as_str) {
                        let block = state.blocks.get_mut(&index).unwrap();
                        let previous = block.get(key).and_then(Value::as_str).unwrap_or_default();
                        block[key] = json!(format!("{previous}{fragment}"));
                    }
                }
                if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                    state
                        .partial_inputs
                        .entry(index)
                        .or_default()
                        .push_str(fragment);
                }
            }
            let block = match delta_type {
                Some("text_delta") => Block::Text,
                Some("thinking_delta") => Block::Thinking,
                Some("input_json_delta") => Block::ToolUse,
                _ => *state.kinds.get(&index).unwrap_or(&Block::Text),
            };
            match block {
                Block::Text => {
                    if let Some(t) = text(delta, "text") {
                        out.push(Chunk::Text(t));
                    }
                }
                Block::Thinking => {
                    if let Some(t) = text(delta, "thinking") {
                        out.push(Chunk::Thinking(t));
                    }
                }
                Block::ToolUse => {
                    let piece = text(delta, "partial_json").unwrap_or_default();
                    out.extend(state.calls.push(index, &piece));
                }
            }
        }
        "content_block_stop" => {
            if !state.open_blocks.remove(&index) {
                return Err(Error::Provider(
                    "unknown or repeated content block stop".into(),
                ));
            }
            if let Some(input) = state.partial_inputs.remove(&index) {
                state
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| Error::Provider("unknown completed content block".into()))?
                    ["input"] = serde_json::from_str(&input).unwrap_or_else(|_| json!({}));
            }
        }
        "message_delta" => {
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.completion = Some(super::Completion::from_native(reason));
            }
            if let Some(usage) = value.get("usage") {
                state.update_usage(usage)?;
            }
        }
        _ => {}
    }
    Ok(out)
}

fn number(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|u| u.get(key))
        .and_then(|n| n.as_u64())
        .unwrap_or(0)
}

fn text(v: Option<&Value>, key: &str) -> Option<String> {
    let s = v?.get(key)?.as_str()?;
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::CallRef;
    use super::*;
    use crate::settings::providers::preset;
    use crate::settings::ModelSettings;
    use crate::tools::ToolDecl;

    fn config() -> ProviderConfig {
        preset("anthropic").unwrap().build("sk-ant-x")
    }

    fn request(messages: Vec<Message>) -> Request {
        let cfg = config();
        let mut settings = ModelSettings::for_provider(&cfg);
        settings.model = "claude-opus-5".into();
        Request {
            control: Default::default(),
            system: "sys".into(),
            messages,
            tools: Vec::new(),
            settings,
        }
    }

    /// A job whose answer is one call names it; the endpoint is told to make
    /// that call where it accepts being told. The agent is never forced.
    #[test]
    fn a_forced_tool_is_the_one_the_request_names() {
        let mut cfg = config();
        cfg.force_tool_choice = true;
        let mut req = request(vec![Message::user("name it")]);
        req.settings.thinking = false;
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
            json!({"type":"tool","name":"set_title"})
        );
    }

    /// Audit C5: without markers the Messages API caches nothing at all.
    #[test]
    fn the_system_and_the_newest_message_carry_cache_breakpoints() {
        let b = body(
            &request(vec![
                Message::user("first"),
                Message::assistant("a"),
                Message::user("second"),
            ]),
            &config(),
        );
        assert_eq!(b["system"][0]["text"], json!("sys"));
        assert_eq!(b["system"][0]["cache_control"]["type"], json!("ephemeral"));
        let messages = b["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(
            last["content"].as_array().unwrap().last().unwrap()["cache_control"]["type"],
            json!("ephemeral")
        );
        assert!(
            !messages[0].to_string().contains("cache_control"),
            "older messages are found through the prefix, not marked"
        );
    }

    #[test]
    fn zero_output_usage_retains_cache_creation_and_read_totals() {
        let mut state = State::default();
        absorb(&json!({"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":900,"cache_creation_input_tokens":500,"cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":300}}}}),&mut state).unwrap();
        absorb(
            &json!({"type":"message_delta","usage":{"output_tokens":0}}),
            &mut state,
        )
        .unwrap();
        let chunks = state.finish().unwrap();
        assert!(chunks.iter().any(|c|matches!(c,Chunk::Usage{input:1500,output:0,cached:900,details} if details.cache_write_5m==Some(200) && details.cache_write_1h==Some(300) && details.output_known)));
    }

    #[test]
    fn signed_thinking_survives_storage_and_only_replays_to_its_own_route() {
        let blocks = json!([{"type":"thinking","thinking":"reason","signature":"opaque-signature"},{"type":"tool_use","id":"c","name":"echo","input":{"text":"abc"}}]);
        let native = crate::provider::Continuation {
            provider: "anthropic".into(),
            model: "fixture".into(),
            data: json!({"content":blocks}),
        };
        let db = crate::store::Db::memory().unwrap();
        let session = db.create_session("/fixture").unwrap();
        let encoded = serde_json::to_string(&native).unwrap();
        db.append_message(
            session.id,
            crate::store::sessions::NewMessage {
                continuation: Some(&encoded),
                ..crate::store::sessions::NewMessage::plain(1, Role::Assistant, "")
            },
        )
        .unwrap();
        let mut message = Message::assistant("");
        message.continuation = db.history(session.id).unwrap()[0].continuation.clone();
        let same = conversation(
            &[message.clone()],
            None,
            None,
            Some(("anthropic", "fixture")),
        );
        assert_eq!(same[0]["content"], blocks);
        assert!(conversation(&[message], None, None, Some(("other", "fixture"))).is_empty());
    }

    #[test]
    fn the_endpoint_is_messages_and_the_key_rides_in_its_own_header() {
        let c = config();
        assert_eq!(c.endpoint(), "https://api.anthropic.com/v1/messages");
        assert_eq!(c.auth.header, "x-api-key");
        assert!(c.auth.prefix.is_empty(), "there is no Bearer here");
        assert_eq!(
            c.headers.get("anthropic-version").map(String::as_str),
            Some("2023-06-01")
        );
    }

    /// The system prompt is a field, and `max_tokens` is not optional — a
    /// request without it is refused outright.
    #[test]
    fn the_system_prompt_is_a_field_and_max_tokens_is_always_present() {
        let b = body(&request(vec![Message::user("hi")]), &config());
        assert_eq!(b["system"][0]["text"], json!("sys"));
        assert_eq!(b["stream"], json!(true));
        assert!(b["max_tokens"].as_u64().unwrap() > 0);
        assert_eq!(b["messages"][0]["role"], json!("user"));
        assert_eq!(b["messages"][0]["content"][0]["text"], json!("hi"));
        // The preset ships no catalogue, so nothing has claimed this model can
        // think and the block is absent rather than guessed at.
        assert!(b.get("thinking").is_none());
    }

    #[test]
    fn a_thinking_budget_always_leaves_room_for_the_answer() {
        let mut req = request(vec![Message::user("hi")]);
        req.settings.thinking = true;
        req.settings.max_output = Some(2_000);
        let b = body(&req, &config());
        let budget = b["thinking"]["budget_tokens"].as_u64().unwrap();
        assert_eq!(b["thinking"]["type"], json!("enabled"));
        assert!(budget >= MIN_THINKING_BUDGET);
        assert!(
            budget < b["max_tokens"].as_u64().unwrap(),
            "a budget equal to the ceiling is refused"
        );
    }

    #[test]
    fn tools_are_declared_with_an_input_schema_not_a_function_wrapper() {
        let mut req = request(vec![Message::user("hi")]);
        req.tools = vec![ToolDecl {
            name: "create_file".into(),
            description: "writes".into(),
            parameters: json!({ "type": "object" }),
        }];
        let b = body(&req, &config());
        assert_eq!(b["tools"][0]["name"], json!("create_file"));
        assert_eq!(b["tools"][0]["input_schema"], json!({ "type": "object" }));
        assert!(b["tools"][0].get("function").is_none());
    }

    /// Three tools called in one turn produce three stored `Tool` messages.
    /// Sent as three user messages the request is rejected for non-alternating
    /// roles; this is the fold that makes it legal.
    #[test]
    fn consecutive_tool_results_become_one_user_message_of_blocks() {
        let messages = vec![
            Message::user("do it"),
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
            Message::tool_result("a", "read", "first"),
            Message::tool_result("b", "read", "second"),
        ];
        let wire = conversation(&messages, None, None, None);
        assert_eq!(
            wire.len(),
            3,
            "user, assistant, and ONE user carrying both results: {wire:?}"
        );
        assert_eq!(wire[1]["role"], json!("assistant"));
        assert_eq!(wire[1]["content"][0]["type"], json!("tool_use"));
        assert_eq!(wire[1]["content"][0]["input"], json!({}));
        assert_eq!(wire[2]["role"], json!("user"));
        assert_eq!(wire[2]["content"][0]["tool_use_id"], json!("a"));
        assert_eq!(wire[2]["content"][1]["tool_use_id"], json!("b"));
    }

    /// A whole stream, in the frames the endpoint really sends. Parsed offline,
    /// so this costs nothing and cannot be wrong about a live balance.
    #[test]
    fn a_full_stream_yields_thinking_then_text_then_a_finished_tool_call() {
        let frames = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":100}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"here"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu_1","name":"create_file"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":":\"a.txt\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","usage":{"output_tokens":42}}),
        ];
        let mut state = State::default();
        let mut chunks = Vec::new();
        for f in &frames {
            chunks.extend(absorb(f, &mut state).unwrap());
        }
        chunks.extend(state.finish().unwrap());

        assert!(chunks.contains(&Chunk::Thinking("hm".into())));
        assert!(chunks.contains(&Chunk::Text("here".into())));
        // Usage is emitted once, with the input side remembered from the start
        // of the message and the output side from its end.
        assert_eq!(
            chunks
                .iter()
                .filter(|c| matches!(c, Chunk::Usage { .. }))
                .count(),
            1,
            "a turn must be billed exactly once"
        );
        assert!(chunks.iter().any(|c| matches!(
            c,
            Chunk::Usage {
                input: 220,
                output: 42,
                cached: 100,
                ..
            }
        )));
        // Arguments streamed in two pieces, so the counter could move twice.
        assert_eq!(
            chunks
                .iter()
                .filter(|c| matches!(c, Chunk::ToolArgs { .. }))
                .count(),
            2
        );
        let call = chunks
            .iter()
            .find_map(|c| match c {
                Chunk::ToolCall { id, name, args } => Some((id, name, args)),
                _ => None,
            })
            .expect("the assembled call must be delivered");
        assert_eq!(call.0, "tu_1");
        assert_eq!(call.1, "create_file");
        assert_eq!(call.2, &json!({ "path": "a.txt" }));
    }

    #[test]
    fn an_error_frame_stops_the_stream_rather_than_being_ignored() {
        let mut state = State::default();
        let frame = json!({"type":"error","error":{"message":"overloaded"}});
        let e = absorb(&frame, &mut state).unwrap_err().to_string();
        assert!(e.contains("overloaded"), "{e}");
    }

    /// A fragment that does not repeat its own type must still land in the right
    /// place, which is what the recorded block kind is for.
    #[test]
    fn a_delta_without_a_type_falls_back_to_the_block_it_belongs_to() {
        let mut state = State::default();
        absorb(
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
            &mut state,
        )
        .unwrap();
        let out = absorb(
            &json!({"type":"content_block_delta","index":0,"delta":{"thinking":"quiet"}}),
            &mut state,
        )
        .unwrap();
        assert_eq!(out, vec![Chunk::Thinking("quiet".into())]);
    }
    #[test]
    fn mcp_image_is_a_typed_block_inside_the_anthropic_tool_result() {
        let root = std::env::temp_dir()
            .join("rune-test")
            .join(format!("anthropic-mcp-media-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Image.png");
        std::fs::write(&path,crate::media::decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5E0AAAAASUVORK5CYII=").unwrap()).unwrap();
        let mut model = ModelInfo::named("fixture", false);
        model.modalities = vec!["image".into()];
        let messages = vec![Message::tool_result("image", "mcp_image", "External image")
            .with_media(vec![crate::media::Attachment::at(&path).unwrap()])];
        let wire = conversation(&messages, Some(&model), None, None);
        assert!(wire[0]["content"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "image"));
        let _ = std::fs::remove_dir_all(root);
    }
}
