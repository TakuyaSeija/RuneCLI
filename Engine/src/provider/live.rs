//! The live provider: one engine, two wires.
//!
//! Everything that is true of *every* model call lives here — where the key
//! comes from, when a failed call may be opened again, how cancellation is
//! honoured between events, and when assembled tool calls are delivered. What
//! differs between vendors is the shape of the JSON, and that lives in
//! [`super::openai`] and [`super::anthropic`].
//!
//! Splitting it this way is not tidiness. A second copy of this loop would be a
//! second place for the retry rule to be got wrong, a second place to forget
//! that a stream which ends without `finish_reason` still has calls in it, and a
//! second place for cancellation to be checked one event too late.
//!
//! # Rules
//!
//! * **Never retry a call that produced anything.** A model call that
//!   half-succeeded may already have run a tool; charging for it twice and
//!   running it twice are both worse than an honest failure. Only *opening* the
//!   call is retried, and [`crate::retry`] explains why that boundary is exact.
//! * **Commit before delivering calls.** Transport EOF and usage never prove
//!   completion. Partial output is billable, but cannot authorize tools.
//! * **Cancellation is checked between every event**, so stopping a runaway
//!   answer does not wait for the model to finish it.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{anthropic, openai, Chunk, Flow, Provider, Request};
use crate::error::{Error, Result};
use crate::net::Http;
use crate::panic;
use crate::settings::providers::{Dialect, ProviderConfig};
use crate::settings::Settings;

pub struct Live {
    settings: Arc<Mutex<Settings>>,
    media_catalogues: Mutex<std::collections::HashSet<String>>,
}

/// One request, fully resolved: where it goes, how it authenticates, what it
/// carries.
struct Prepared {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

/// The half of the conversation that depends on which vendor is answering.
enum Wire {
    OpenAi(openai::State),
    Anthropic(anthropic::State),
}

impl Wire {
    fn new(dialect: Dialect, config: &crate::settings::configuration::Configuration) -> Wire {
        match dialect {
            Dialect::OpenAi => Wire::OpenAi(openai::State {
                calls: super::assembly::Assembly::bounded(config),
                ..Default::default()
            }),
            Dialect::Anthropic => {
                let mut state = anthropic::State::default();
                state.calls = super::assembly::Assembly::bounded(config);
                Wire::Anthropic(state)
            }
        }
    }

    fn absorb(&mut self, value: &Value) -> Result<Vec<Chunk>> {
        match self {
            Wire::OpenAi(state) => openai::absorb(value, state),
            Wire::Anthropic(state) => anthropic::absorb(value, state),
        }
    }

    fn finish(self) -> Result<Vec<Chunk>> {
        match self {
            Wire::OpenAi(state) => state.finish(),
            Wire::Anthropic(state) => state.finish(),
        }
    }

    fn has_usage(&self) -> bool {
        match self {
            Wire::OpenAi(state) => state.has_usage(),
            Wire::Anthropic(state) => state.usage.is_some(),
        }
    }

    fn has_pending_calls(&self) -> bool {
        match self {
            Wire::OpenAi(state) => state.has_pending_calls(),
            Wire::Anthropic(state) => !state.calls.is_empty(),
        }
    }
}

/// Endpoint and body for a config, by dialect.
///
/// The **only** place the two wires are chosen between, which is why it is
/// visible to the crate: `provider dryrun` shows what would be sent, and a
/// second dialect switch inside it would be a second thing to keep in step —
/// the first version had exactly that bug, and printed an OpenAI body under an
/// Anthropic URL.
pub(crate) fn shape(req: &Request, cfg: &ProviderConfig) -> Result<(String, Value)> {
    let (url, mut body) = match cfg.dialect {
        Dialect::OpenAi => (openai::endpoint(cfg), openai::body(req, cfg)),
        Dialect::Anthropic => (anthropic::endpoint(cfg), anthropic::body(req, cfg)),
    };
    let model = cfg.model(&req.settings.model);
    for knob in crate::settings::KNOBS {
        if let Some(value) = body.get(knob.key).and_then(Value::as_f64) {
            if let Some((min, max, _)) = knob.bounds(model) {
                if !value.is_finite() || value < min || value > max {
                    return Err(Error::invalid(format!(
                        "{} is outside the model's supported range",
                        knob.key
                    )));
                }
            }
        }
    }
    // Context and input-token limits are owned by the selected provider. This
    // adapter must not reject or reshape a request from character counts, base64
    // lengths, nominal media values, or a locally learned ratio. Where a provider
    // exposes an authoritative count endpoint it belongs before this boundary;
    // otherwise the request is sent and the provider's response/usage is truth.
    let _ = model;
    if cfg.dialect == Dialect::Anthropic
        && body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled")
    {
        let max = body["max_tokens"].as_u64().unwrap_or(0);
        let budget = body["thinking"]["budget_tokens"]
            .as_u64()
            .unwrap_or(0)
            .min(max.saturating_sub(1));
        if budget < anthropic::MIN_THINKING_BUDGET {
            return Err(Error::invalid(
                "The output budget is too small for this model's required thinking budget",
            ));
        }
        body["thinking"]["budget_tokens"] = serde_json::json!(budget);
    }
    let bytes = serde_json::to_vec(&body)?.len() as u64;
    let limit = cfg.max_payload_bytes();
    if bytes > limit {
        return Err(Error::invalid(format!(
            "request payload is {bytes} bytes, exceeding this provider's {limit}-byte max_payload_size; no request was sent"
        )));
    }
    Ok((url, body))
}

impl Live {
    pub fn new(settings: Arc<Mutex<Settings>>) -> Live {
        Live {
            settings,
            media_catalogues: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Bring the provider's catalogue up to date when it is due — its time to
    /// live passed, the base URL moved, or a media request finds the model's
    /// modalities unknown. Once per request, before any attempt, and never the
    /// request's failure: a stale catalogue is still a catalogue (audit RT3).
    fn refresh_catalogue(&self, req: &Request) {
        let due = {
            let s = panic::lock(&self.settings);
            !s.home.as_os_str().is_empty()
                && s.providers.get(&req.settings.provider).is_some_and(|p| {
                    p.models.iter().any(|m| {
                        m.fetched_at.is_some()
                            && m.source_url.as_deref() != Some(p.base_url.as_str())
                    }) || p
                        .models
                        .iter()
                        .filter_map(|m| m.fetched_at)
                        .min()
                        // Models nobody fetched were configured by hand: there
                        // is no catalogue of theirs to refresh.
                        .is_some_and(|at| {
                            crate::store::now_ms().saturating_sub(at) as u64
                                >= s.config.duration("network.catalog_ttl").as_millis() as u64
                        })
                })
        };
        if due {
            let _ = crate::settings::manage::models(
                &self.settings,
                &req.settings.provider,
                Some(&req.control),
            );
        }
        if req
            .messages
            .iter()
            .any(|m| m.media.iter().any(|a| !a.is_text()))
        {
            let missing = {
                let settings = panic::lock(&self.settings);
                settings
                    .providers
                    .get(&req.settings.provider)
                    .and_then(|p| p.model(&req.settings.model))
                    .is_none_or(|m| m.modalities.is_empty())
            };
            if missing && panic::lock(&self.media_catalogues).insert(req.settings.provider.clone())
            {
                // Refresh facts, not a model response. A failed catalogue leaves
                // the explicit unsupported/unknown notice in place.
                let _ = crate::settings::manage::models(
                    &self.settings,
                    &req.settings.provider,
                    Some(&req.control),
                );
            }
        }
    }

    /// Endpoint, headers and body for one request — everything that depends on
    /// user configuration, resolved in one place while the lock is held once.
    fn prepare(&self, req: &Request) -> Result<Prepared> {
        let settings = panic::lock(&self.settings);
        let name = &req.settings.provider;
        let cfg = settings
            .providers
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("provider `{name}` is not configured")))?;
        if req.settings.model.trim().is_empty() {
            return Err(Error::invalid(format!(
                "no model chosen for `{name}` — set one with `rune -model set <id>`"
            )));
        }
        if let Some(model) = cfg.model(&req.settings.model) {
            if model.chat == Some(false) {
                return Err(Error::invalid(
                    "This model does not support Rune's chat endpoint",
                ));
            }
            if !req.tools.is_empty() && model.supports_parameter("tools") == Some(false) {
                return Err(Error::invalid(
                    "This model does not support tools required by this operation",
                ));
            }
        }
        let headers = settings.headers_for(cfg)?;
        let cfg = cfg.clone();
        drop(settings);
        // Media loading and serialization can be expensive. The request owns its
        // provider snapshot; neither operation may block configuration commits.
        let mut resolved = req.clone();
        if req.control.role.is_some() {
            if let Some(ceiling) = cfg.model(&req.settings.model).and_then(|m| m.max_output) {
                if resolved.settings.max_output.is_some_and(|n| n > ceiling) {
                    resolved.settings.max_output = Some(ceiling);
                    if req.control.role == Some("compact") {
                        resolved.system = crate::session::compact::system_for(ceiling);
                    }
                }
            }
        }
        let (url, body) = shape(&resolved, &cfg)?;
        Ok(Prepared { url, headers, body })
    }
}

impl Live {
    /// One whole call: open it, read its stream to the end, and hand every
    /// chunk to `sink`. Any failure is the attempt's; whether it is repeated is
    /// decided by [`crate::retry::run`], the one loop around it.
    fn attempt(
        &self,
        req: &Request,
        dialect: Dialect,
        started: std::time::Instant,
        sink: &mut dyn FnMut(Chunk) -> Flow,
    ) -> Result<()> {
        let config = panic::lock(&self.settings).config.clone();
        let timeout = req.control.timeout;
        let started_at_ms = crate::store::now_ms();
        let remaining = match timeout {
            Some(t) => Some(
                t.checked_sub(started.elapsed())
                    .ok_or_else(|| Error::Provider("request deadline exceeded".into()))?,
            ),
            None => None,
        };
        // Prepared per attempt: that is what makes the next attempt use the key
        // a rotation just moved to the front.
        let call = self.prepare(req)?;
        let http = Http::configured(&config, timeout, req.control.clone());
        let mut sse = http.post_sse(&call.url, &call.headers, &call.body, remaining)?;
        let config_now = config.snapshot();
        let mut wire = Wire::new(dialect, &config_now);

        let mut completed = false;
        let mut meaningful = false;
        let mut received_bytes = 0u64;
        let markerless_commit = matches!(
            panic::lock(&self.settings)
                .providers
                .get(&req.settings.provider)
                .map(|p| p.api_kind()),
            Some("google" | "9router")
        );
        let result = (|| -> Result<()> {
            loop {
                if req.control.cancelled() {
                    return Err(Error::Cancelled);
                }
                if let Some(t) = timeout {
                    if started.elapsed() >= t {
                        return Err(Error::Provider("request deadline exceeded".into()));
                    }
                }
                let event = sse.next_event().map_err(|e| {
                    if req.control.cancelled() {
                        Error::Cancelled
                    } else if let Some(silence) = crate::net::stalled(&e) {
                        Error::Stalled(silence)
                    } else {
                        Error::Provider(format!("the stream ended badly: {e}"))
                    }
                })?;
                let Some(data) = event else { break };
                received_bytes = received_bytes.saturating_add(data.len() as u64);
                if received_bytes > config.number("network.max_stream_size") {
                    return Err(Error::Provider(
                        "model response exceeds configured byte limit".into(),
                    ));
                }
                if matches!(sse.event_name(), "error" | "response.failed") {
                    let reason = serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|value| super::openai::stream_error(&value))
                        .unwrap_or_else(|| {
                            "the provider reported an error event mid-stream".into()
                        });
                    return Err(Error::Provider(reason));
                }
                if data.trim() == "[DONE]" {
                    if dialect == Dialect::OpenAi {
                        completed = true;
                    }
                    break;
                }
                let value: Value = serde_json::from_str(&data)
                    .map_err(|_| Error::Provider("malformed JSON data in model stream".into()))?;
                if !value.is_object() {
                    return Err(Error::Provider(
                        "model stream data must be an object".into(),
                    ));
                }
                if (dialect == Dialect::OpenAi
                    && value
                        .pointer("/choices/0/finish_reason")
                        .is_some_and(|v| v.is_string()))
                    || (dialect == Dialect::Anthropic && value["type"] == "message_stop")
                {
                    completed = true;
                }
                for chunk in wire.absorb(&value)? {
                    meaningful |= matches!(&chunk, Chunk::Text(s) | Chunk::Thinking(s) if !s.trim().is_empty());
                    if sink(chunk) == Flow::Stop {
                        return Err(Error::Cancelled);
                    }
                }
            }
            Ok(())
        })();
        // Capture this before the one-time accounting move below.
        let observed_usage = wire.has_usage();
        // Measured usage survives cancellation, malformed arguments and transport failure.
        // It is emitted exactly once and never acts as proof of completion.
        let usage = match &mut wire {
            Wire::OpenAi(state) => state.usage.take(),
            Wire::Anthropic(state) => state.usage.take(),
        };
        if let Some(Chunk::Usage {
            input,
            output,
            cached,
            mut details,
        }) = usage
        {
            details.started_at_ms = Some(started_at_ms);
            details.complete = completed && result.is_ok();
            let _ = sink(Chunk::Usage {
                input,
                output,
                cached,
                details,
            });
        } else {
            let request_id = match &wire {
                Wire::OpenAi(state) => state.request_id.clone(),
                Wire::Anthropic(_) => None,
            };
            let _ = sink(Chunk::Usage {
                input: 0,
                output: 0,
                cached: 0,
                details: super::UsageDetails {
                    started_at_ms: Some(started_at_ms),
                    request_id,
                    ..Default::default()
                },
            });
        }
        result?;
        // Google compatibility and 9Router are observed to close some committed
        // text/thought streams without forwarding a finish marker. This narrow
        // fallback never authorizes a pending tool descriptor: such a stream is
        // still indistinguishable from truncation and remains an error.
        if !completed
            && markerless_commit
            && meaningful
            && observed_usage
            && !wire.has_pending_calls()
        {
            completed = true;
        }
        if !completed {
            return Err(Error::Provider(
                "stream closed prematurely before completion".into(),
            ));
        }
        let (completion, mut native) = match &wire {
            Wire::OpenAi(state) => (
                state.completion.clone(),
                Value::Object(state.continuation.clone()),
            ),
            Wire::Anthropic(state) => (
                state.completion.clone(),
                serde_json::json!({"content":state.blocks.values().collect::<Vec<_>>()}),
            ),
        };
        if let Some(end) = &completion {
            native["completion"] = serde_json::to_value(end)?;
        }
        let chunks = wire.finish()?;
        let has_calls = chunks
            .iter()
            .any(|c| matches!(c, Chunk::ToolCall { .. } | Chunk::InvalidToolCall { .. }));
        if let Some(ref end) = completion {
            if matches!(
                end.category,
                super::FinishCategory::Length | super::FinishCategory::Unknown
            ) {
                let _ = sink(Chunk::Finished(end.clone()));
                if req.control.role == Some("title") && meaningful {
                    // Title generation: text was returned, length cutoff is acceptable.
                } else {
                    return Err(Error::Incomplete(format!(
                        "the model stopped before finishing ({})",
                        end.native_reason
                    )));
                }
            }
            if has_calls && matches!(end.category, super::FinishCategory::Refusal) {
                return Err(Error::Provider(
                    "refused response contained uncommitted tool calls".into(),
                ));
            }
        }
        if !meaningful
            && !has_calls
            && !completion
                .as_ref()
                .is_some_and(|c| c.category == super::FinishCategory::Refusal)
        {
            return Err(Error::Provider("model returned an empty response".into()));
        }
        if !chunks
            .iter()
            .any(|chunk| matches!(chunk, Chunk::InvalidToolCall { .. }))
            && native.as_object().is_some_and(|o| !o.is_empty())
            && sink(Chunk::Continuation(super::Continuation {
                provider: req.settings.provider.clone(),
                model: req.settings.model.clone(),
                data: native,
            })) == Flow::Stop
        {
            return Err(Error::Cancelled);
        }
        for chunk in chunks {
            if req.control.cancelled() || sink(chunk) == Flow::Stop {
                return Err(Error::Cancelled);
            }
        }
        Ok(())
    }
}

impl Provider for Live {
    fn name(&self) -> &str {
        "live"
    }

    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Chunk) -> Flow) -> Result<()> {
        // Provider facts are refreshed once per request, before any attempt; a
        // failed refresh is not the request's failure (audit RT3).
        self.refresh_catalogue(req);
        let (secret, dialect) = {
            let settings = panic::lock(&self.settings);
            match settings.providers.get(&req.settings.provider) {
                Some(c) => (c.secret.clone(), c.dialect),
                None => (None, Dialect::OpenAi),
            }
        };
        let timeout = req.control.timeout;
        let started = std::time::Instant::now();
        let sink = std::cell::RefCell::new(sink);
        // Whether the attempt running now has shown the frontend anything, so
        // the next one can tell it to drop that first.
        let delivered = std::cell::Cell::new(false);
        let mut announce = |w: crate::retry::Waiting| {
            if req.control.cancelled() {
                return crate::retry::Go::Cancel;
            }
            if timeout.is_some_and(|t| started.elapsed() >= t) {
                return crate::retry::Go::GiveUp;
            }
            match (sink.borrow_mut())(Chunk::Retrying {
                attempt: w.attempt,
                of: w.of,
                wait_ms: w.wait.as_millis() as u64,
                rotated: w.rotated,
                reason: w.reason,
            }) {
                Flow::Continue => crate::retry::Go::On,
                Flow::Stop => crate::retry::Go::Cancel,
            }
        };
        let adjusted = std::cell::RefCell::new(req.clone());
        let mut attempt = || -> Result<()> {
            if req.control.cancelled() {
                return Err(Error::Cancelled);
            }
            if delivered.replace(false) && (sink.borrow_mut())(Chunk::Reset) == Flow::Stop {
                return Err(Error::Cancelled);
            }
            let mut forward = |chunk: Chunk| {
                if matches!(
                    chunk,
                    Chunk::Text(_)
                        | Chunk::Thinking(_)
                        | Chunk::ToolArgs { .. }
                        | Chunk::ToolCall { .. }
                        | Chunk::InvalidToolCall { .. }
                        | Chunk::Continuation(_)
                        | Chunk::Finished(_)
                ) {
                    delivered.set(true);
                }
                (sink.borrow_mut())(chunk)
            };
            let current = adjusted.borrow().clone();
            let result = self.attempt(&current, dialect, started, &mut forward);
            if let Err(Error::OutputLimit { limit, .. }) = &result {
                if req.control.role == Some("compact")
                    && current.settings.max_output.is_some_and(|n| *limit < n)
                {
                    let mut next = adjusted.borrow_mut();
                    next.settings.max_output = Some(*limit);
                    next.system = crate::session::compact::system_for(*limit);
                }
            }
            result
        };
        let result = crate::retry::run(
            &self.settings,
            secret.as_deref(),
            &mut announce,
            &mut attempt,
        );
        // Out of attempts: the last one's partial output is void too, so the
        // screen drops it like the transcript does (audit U7). A cut-off answer
        // and a cancelled one are kept by the turn, and stay.
        if result
            .as_ref()
            .is_err_and(|e| !matches!(e, Error::Incomplete(_) | Error::Cancelled))
            && delivered.get()
        {
            let _ = (sink.borrow_mut())(Chunk::Reset);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Attachment, MAX_INLINE_BYTES};
    use crate::provider::Message;
    use crate::settings::{
        providers::{preset, ModelInfo},
        ModelSettings,
    };
    use std::time::Duration;

    #[test]
    fn structured_output_limit_adjusts_compact_on_the_existing_retry_path() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for n in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut data = Vec::new();
                let mut byte = [0u8; 1];
                while !data.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    data.push(byte[0]);
                }
                let headers = String::from_utf8_lossy(&data);
                let len: usize = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; len];
                socket.read_exact(&mut body).unwrap();
                bodies.push(serde_json::from_slice::<Value>(&body).unwrap());
                let (status, content_type, answer) = if n == 0 {
                    ("400 Bad Request","application/json",r#"{"error":{"param":"max_tokens","max_value":512,"message":"output ceiling"}}"#.to_string())
                } else {
                    ("200 OK","text/event-stream","data: {\"choices\":[{\"delta\":{\"content\":\"summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string())
                };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",answer.len()).unwrap();
            }
            bodies
        });
        let home =
            std::env::temp_dir().join(format!("compact-output-limit-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut cfg = preset("compatible")
            .unwrap_or_else(|| preset("openai").unwrap())
            .build("");
        cfg.name = "fixture".into();
        cfg.secret = None;
        cfg.base_url = format!("http://{address}/v1");
        cfg.models = vec![ModelInfo::named("judge", false)];
        let mut settings = Settings::empty();
        settings.providers.list = vec![cfg.clone()];
        settings.config.store(crate::settings::configuration::Configuration::from_values(serde_json::json!({"retry":{"max_attempts":1,"initial_delay":"1ms","max_delay":"1ms"},"network":{"catalog_ttl":"1h"}})).unwrap());
        let live = Live::new(Arc::new(Mutex::new(settings)));
        let mut knobs = ModelSettings::for_provider(&cfg);
        knobs.model = "judge".into();
        knobs.max_output = Some(32768);
        let request = Request {
            system: crate::session::compact::system_for(32768),
            messages: vec![Message::user("old session")],
            tools: Vec::new(),
            settings: knobs,
            control: crate::provider::RequestControl {
                role: Some("compact"),
                ..Default::default()
            },
        };
        live.stream(&request, &mut |_| Flow::Continue).unwrap();
        let bodies = server.join().unwrap();
        assert_eq!(
            bodies[0]
                .get("max_tokens")
                .or_else(|| bodies[0].get("max_completion_tokens"))
                .unwrap(),
            32768
        );
        assert_eq!(
            bodies[1]
                .get("max_tokens")
                .or_else(|| bodies[1].get("max_completion_tokens"))
                .unwrap(),
            512
        );
        assert!(bodies[1]["messages"][0]["content"]
            .to_string()
            .contains("never more than 256"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn payload_cap_counts_the_whole_body_per_call_not_across_roles() {
        let mut cfg = preset("openai").unwrap().build("");
        cfg.max_payload_size = "2KiB".into();
        let mut req = Request {
            control: Default::default(),
            system: "policy".into(),
            messages: vec![Message::user("x".repeat(900))],
            tools: Vec::new(),
            settings: ModelSettings::for_provider(&cfg),
        };
        req.settings.model = "test".into();
        for role in [None, Some("classifier"), Some("compact")] {
            req.control.role = role;
            assert!(shape(&req, &cfg).is_ok(), "each independent request fits");
        }
        req.system = "policy".repeat(400);
        let error = shape(&req, &cfg).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "{error:?}");
        assert!(error.to_string().contains("request payload"));
    }

    #[test]
    fn classifier_cache_marks_the_stable_policy_not_the_changing_suffix() {
        for dialect in [Dialect::OpenAi, Dialect::Anthropic] {
            let mut cfg = preset(if dialect == Dialect::Anthropic {
                "anthropic"
            } else {
                "openai"
            })
            .unwrap()
            .build("");
            let mut model = ModelInfo::named("judge", false);
            model.supported_parameters = vec!["tools".into(), "cache_control".into()];
            cfg.models = vec![model];
            let mut req = Request {
                control: super::super::RequestControl {
                    role: Some("classifier"),
                    ..Default::default()
                },
                system: "stable policy".into(),
                messages: vec![Message::user("Action first")],
                tools: Vec::new(),
                settings: ModelSettings::for_provider(&cfg),
            };
            req.settings.model = "judge".into();
            let (_, first) = shape(&req, &cfg).unwrap();
            req.messages[0].content = "Action second".into();
            let (_, second) = shape(&req, &cfg).unwrap();
            if dialect == Dialect::Anthropic {
                assert_eq!(first["system"], second["system"]);
            } else {
                assert_eq!(first["messages"][0], second["messages"][0]);
            }
            let suffix = serde_json::to_string(&second["messages"]).unwrap();
            if dialect == Dialect::Anthropic {
                assert!(!suffix.contains("cache_control"));
            } else {
                assert!(!serde_json::to_string(&second["messages"][1])
                    .unwrap()
                    .contains("cache_control"));
            }
        }
    }

    /// Opt-in integration runner. Its plan and credentials live only in an isolated test home.
    #[test]
    #[ignore]
    fn configured_provider_conformance() {
        use serde_json::json;
        let root = std::path::PathBuf::from(
            std::env::var_os("RUNE_PROVIDER_TEST_HOME")
                .expect("explicit isolated test home required"),
        );
        assert!(
            root.starts_with(std::env::temp_dir()),
            "live verification must use an isolated temporary home"
        );
        let shared = Arc::new(Mutex::new(
            Settings::load(&root, &root.join("rune.db")).unwrap(),
        ));
        let names = panic::lock(&shared)
            .providers
            .list
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        let mut catalog = Vec::new();
        for name in names {
            let result = crate::settings::manage::models(&shared, &name, None);
            catalog.push(json!({"provider":name,"ok":result.is_ok(),"error":result.err().map(|e|e.to_string())}));
        }
        let mut shapes = Vec::new();
        for cfg in &panic::lock(&shared).providers.list {
            for model in &cfg.models {
                let mut settings = ModelSettings::for_provider(cfg);
                settings.model = model.id.clone();
                settings.max_output = Some(128);
                settings.thinking = model.thinking;
                let request = Request {
                    control: Default::default(),
                    system: "Compatibility fixture".into(),
                    messages: vec![Message::user("Reply OK")],
                    tools: vec![crate::tools::ToolDecl {
                        name: "audit_echo".into(),
                        description: "Return a test string; no external effects".into(),
                        parameters: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
                    }],
                    settings,
                };
                let result = shape(&request, cfg);
                shapes.push(match result {Ok((_,body))=>json!({"provider":cfg.name,"model":model.id,"chat":model.chat,"tools":model.tools,"request":body}),Err(e)=>json!({"provider":cfg.name,"model":model.id,"error":e.to_string()})});
            }
        }
        std::fs::write(
            root.join("Shapes.json"),
            serde_json::to_vec_pretty(&json!({"catalogues":catalog,"models":shapes})).unwrap(),
        )
        .unwrap();
        let plan: Vec<Value> =
            serde_json::from_slice(&std::fs::read(root.join("LivePlan.json")).unwrap()).unwrap();
        let live = Live::new(shared.clone());
        let mut results = Vec::new();
        for case in plan {
            let name = case["provider"].as_str().unwrap();
            let id = case["model"].as_str().unwrap();
            let cfg = panic::lock(&shared).providers.get(name).unwrap().clone();
            let mut settings = ModelSettings::for_provider(&cfg);
            settings.model = id.into();
            settings.max_output = Some(case["max_output"].as_u64().unwrap_or(2048));
            settings.thinking = case["thinking"].as_bool().unwrap_or(false);
            settings.effort = None;
            let mut request=Request{control:super::super::RequestControl{timeout:Some(Duration::from_secs(45)),..Default::default()},system:"API interoperability test. Follow the test instruction.".into(),messages:vec![Message::user("Call audit_echo exactly once with text abc. After its result, reply OK without another tool call.")],tools:vec![crate::tools::ToolDecl{name:"audit_echo".into(),description:"Return a synthetic test value. No files or system access.".into(),parameters:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false})}],settings};
            for round in 1..=2 {
                let mut text = String::new();
                let mut thinking = String::new();
                let mut calls = Vec::new();
                let mut native = None;
                let mut usage = Vec::new();
                let mut retry = None;
                let result=live.stream(&request,&mut|chunk|{match chunk{
                    Chunk::Text(s)=>text.push_str(&s),Chunk::Thinking(s)=>thinking.push_str(&s),
                    Chunk::Continuation(c)=>native=Some(c),Chunk::ToolCall{id,name,args}=>calls.push(super::super::CallRef{id,name,args:args.to_string()}),
                    Chunk::Usage{input,output,cached,details}=>usage.push(json!({"input":input,"output":output,"cached":cached,"details":details})),
                    Chunk::Retrying{reason,..}=>{retry=Some(reason);return Flow::Stop;},_=>{}
                }Flow::Continue});
                let passed = result.is_ok();
                let report = json!({"provider":name,"model":id,"thinking_requested":request.settings.thinking,"round":round,"ok":passed,"error":result.err().map(|e|e.to_string()),"retry":retry,"text_chars":text.len(),"thinking_chars":thinking.len(),"tools":calls.len(),"usage":usage,"continuation":native.is_some()});
                println!("{}", report);
                results.push(report);
                std::fs::write(
                    root.join("LiveResults.json"),
                    serde_json::to_vec_pretty(&results).unwrap(),
                )
                .unwrap();
                if !passed || calls.is_empty() {
                    break;
                }
                let mut message = Message::assistant(text).with_calls(calls.clone());
                message.reasoning = Some(thinking);
                message.continuation = native;
                request.messages.push(message);
                for call in calls {
                    request
                        .messages
                        .push(Message::tool_result(call.id, call.name, "abc"));
                }
            }
        }
    }

    /// Audit RT1/RT2: an error frame injected into the stream is retried for
    /// every model — the price of a model never decided whether it is repeated.
    #[test]
    fn an_injected_stream_error_is_retried_whatever_the_model_costs() {
        use std::io::{Read, Write};
        for free in [true, false] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = [0; 4096];
                let _ = stream.read(&mut bytes).unwrap();
                let payload =
                    "data: {\"error\":{\"message\":\"Provider returned an empty response\"}}\n\n";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                while stream.read(&mut bytes).is_ok_and(|read| read > 0) {}
            });
            let mut settings = Settings::empty();
            let mut cfg = preset("ollama").unwrap().build("");
            cfg.base_url = format!("http://{address}");
            let mut model = ModelInfo::named("fixture", false);
            model.price = Some(crate::settings::providers::Price {
                input: if free { 0.0 } else { 1.0 },
                ..Default::default()
            });
            cfg.models = vec![model];
            let mut model_settings = ModelSettings::for_provider(&cfg);
            model_settings.model = "fixture".into();
            settings.providers.list.push(cfg);
            let live = Live::new(Arc::new(Mutex::new(settings)));
            let request = Request {
                control: Default::default(),
                system: "fixture".into(),
                messages: vec![Message::user("test")],
                tools: Vec::new(),
                settings: model_settings,
            };
            let mut retries = 0;
            let result = live.stream(&request, &mut |chunk| {
                if matches!(chunk, Chunk::Retrying { .. }) {
                    retries += 1;
                    return Flow::Stop;
                }
                Flow::Continue
            });
            worker.join().unwrap();
            assert!(result.is_err());
            assert_eq!(retries, 1, "free: {free}");
        }
    }

    #[test]
    fn structural_sse_errors_retry_but_a_models_error_text_does_not() {
        use std::io::{Read, Write};
        for (index, payload) in [
            "event: error\ndata: {\"message\":\"failed\"}\n\n",
            "data: {\"type\":\"error\",\"message\":\"failed\"}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"[Error] An error occurred\"},\"finish_reason\":\"stop\"}]}\n\n",
        ].into_iter().enumerate() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut bytes = [0u8; 65536];
                let _ = stream.read(&mut bytes).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                while stream.read(&mut bytes).is_ok_and(|read| read > 0) {}
            });
            let mut settings = Settings::empty();
            let mut cfg = preset("ollama").unwrap().build("");
            cfg.base_url = format!("http://{address}");
            cfg.models = vec![ModelInfo::named("fixture", false)];
            let mut model_settings = ModelSettings::for_provider(&cfg);
            model_settings.model = "fixture".into();
            settings.providers.list.push(cfg);
            let live = Live::new(Arc::new(Mutex::new(settings)));
            let request = Request { control: Default::default(), system:"fixture".into(), messages:vec![Message::user("test")], tools:Vec::new(), settings:model_settings };
            let mut retries = 0;
            let mut text = String::new();
            let result = live.stream(&request, &mut |chunk| {
                match chunk {
                    Chunk::Retrying { .. } => { retries += 1; return Flow::Stop; }
                    Chunk::Text(part) => text.push_str(&part),
                    _ => {}
                }
                Flow::Continue
            });
            worker.join().unwrap();
            if index < 3 { assert_eq!(retries, 1); assert!(result.is_err()); }
            else { assert_eq!(retries, 0); assert!(result.is_ok()); assert_eq!(text, "[Error] An error occurred"); }
        }
    }

    #[test]
    fn free_model_empty_stream_retries_then_delivers_a_single_answer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            for payload in [
                "data: {\"error\":{\"message\":\"Provider returned an empty response\"}}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\n",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut request = Vec::new();
                let mut bytes = [0u8; 4096];
                loop {
                    let read = stream.read(&mut bytes).unwrap();
                    assert!(read > 0, "request closed before body arrived");
                    request.extend_from_slice(&bytes[..read]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers.lines().find_map(|line| {
                            line.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        }).unwrap_or(0);
                        if request.len() >= end + 4 + length { break; }
                    }
                    assert!(request.len() < 65536);
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
            }
        });
        let mut settings = Settings::empty();
        let mut cfg = preset("ollama").unwrap().build("");
        cfg.base_url = format!("http://{address}");
        let mut model = ModelInfo::named("fixture", false);
        model.price = Some(crate::settings::providers::Price::default());
        cfg.models = vec![model];
        let mut model_settings = ModelSettings::for_provider(&cfg);
        model_settings.model = "fixture".into();
        settings.providers.list.push(cfg);
        let live = Live::new(Arc::new(Mutex::new(settings)));
        let request = Request {
            control: Default::default(),
            system: "fixture".into(),
            messages: vec![Message::user("test")],
            tools: Vec::new(),
            settings: model_settings,
        };
        let mut answer = String::new();
        let mut attempts = std::collections::BTreeSet::new();
        let result = live.stream(&request, &mut |chunk| {
            match chunk {
                Chunk::Text(text) => answer.push_str(&text),
                Chunk::Reset => answer.clear(),
                // A countdown repeats the same attempt number every tick.
                Chunk::Retrying { attempt, .. } => {
                    attempts.insert(attempt);
                }
                _ => {}
            }
            Flow::Continue
        });
        worker.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(answer, "OK");
        assert_eq!(attempts.len(), 1);
    }

    /// Audit U7: when the attempts run out, the last one's partial answer is
    /// dropped from the screen too — nothing of it reaches the transcript.
    #[test]
    fn a_final_failure_resets_what_it_streamed() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n";
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0u8; 65536];
            let _ = stream.read(&mut bytes).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            while stream.read(&mut bytes).is_ok_and(|read| read > 0) {}
        });
        let home = std::env::temp_dir().join(format!("rune-final-reset-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("rune.toml"), "[retry]\nmax_attempts = 0\n").unwrap();
        let settings = Settings::empty();
        settings
            .config
            .store(crate::settings::configuration::Configuration::load(&home).unwrap());
        let mut settings = settings;
        let mut cfg = preset("ollama").unwrap().build("");
        cfg.base_url = format!("http://{address}");
        cfg.models = vec![ModelInfo::named("fixture", false)];
        let mut model_settings = ModelSettings::for_provider(&cfg);
        model_settings.model = "fixture".into();
        settings.providers.list.push(cfg);
        let live = Live::new(Arc::new(Mutex::new(settings)));
        let request = Request {
            control: Default::default(),
            system: "fixture".into(),
            messages: vec![Message::user("test")],
            tools: Vec::new(),
            settings: model_settings,
        };
        let mut seen = Vec::new();
        let result = live.stream(&request, &mut |chunk| {
            match chunk {
                Chunk::Text(text) => seen.push(text),
                Chunk::Reset => seen.push("<reset>".into()),
                _ => {}
            }
            Flow::Continue
        });
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(&home);
        assert!(result.is_err());
        assert_eq!(seen, vec!["half", "<reset>"]);
    }

    /// Audit RT5 and the owner's choice: a stream that dies after part of an
    /// answer streamed is repeated, and the frontend is told to drop the
    /// partial first — the answer it ends up with is the repeat's alone.
    #[test]
    fn a_stream_that_dies_mid_answer_is_reset_and_repeated() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            for payload in [
                "data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"whole answer\"},\"finish_reason\":\"stop\"}]}\n\n",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut bytes = [0u8; 65536];
                let _ = stream.read(&mut bytes).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                while stream.read(&mut bytes).is_ok_and(|read| read > 0) {}
            }
        });
        let mut settings = Settings::empty();
        let mut cfg = preset("ollama").unwrap().build("");
        cfg.base_url = format!("http://{address}");
        let mut model = ModelInfo::named("fixture", false);
        model.price = Some(crate::settings::providers::Price {
            input: 3.0,
            ..Default::default()
        });
        cfg.models = vec![model];
        let mut model_settings = ModelSettings::for_provider(&cfg);
        model_settings.model = "fixture".into();
        settings.providers.list.push(cfg);
        let live = Live::new(Arc::new(Mutex::new(settings)));
        let request = Request {
            control: Default::default(),
            system: "fixture".into(),
            messages: vec![Message::user("test")],
            tools: Vec::new(),
            settings: model_settings,
        };
        let mut seen = Vec::new();
        let result = live.stream(&request, &mut |chunk| {
            match chunk {
                Chunk::Text(text) => seen.push(text),
                Chunk::Reset => seen.push("<reset>".into()),
                _ => {}
            }
            Flow::Continue
        });
        worker.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(seen, vec!["half an ans", "<reset>", "whole answer"]);
    }

    #[test]
    fn stream_commit_matrix_never_executes_partial_calls_and_retains_usage() {
        use std::io::{Read, Write};
        let cases=[
            ("data: {\"usage\":null,\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",false,0,1),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]}}]}\n\n",false,0,1),
            ("data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}\n\n",true,0,1),
            ("data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0}}\n\ndata: [DONE]\n\n",false,0,1),
            ("data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":0}}\n\ndata: broken\n\n",false,0,1),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"length\"}]}\n\n",false,0,1),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",true,1,1),
        ];
        for (payload, success, call_count, usage_count) in cases {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut bytes = [0u8; 4096];
                loop {
                    let read = stream.read(&mut bytes).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..read]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    assert!(request.len() < 65536);
                }
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",payload.len(),payload).unwrap();
            });
            let mut settings = Settings::empty();
            let mut cfg = preset("ollama").unwrap().build("");
            cfg.base_url = format!("http://{address}");
            cfg.models = vec![ModelInfo::named("fixture", false)];
            let req = Request {
                control: Default::default(),
                system: "fixture".into(),
                messages: vec![Message::user("test")],
                tools: Vec::new(),
                settings: ModelSettings::for_provider(&cfg),
            };
            settings.providers.list.push(cfg);
            let live = Live::new(Arc::new(Mutex::new(settings)));
            let mut chunks = Vec::new();
            // One attempt is what this matrix is about; repeating a failed one
            // is the retry tests' subject, so the first announcement stops it.
            let result = live.stream(&req, &mut |chunk| {
                if matches!(chunk, Chunk::Retrying { .. }) {
                    return Flow::Stop;
                }
                chunks.push(chunk);
                Flow::Continue
            });
            worker.join().unwrap();
            assert_eq!(result.is_ok(), success, "{payload}: {result:?}");
            assert_eq!(
                chunks
                    .iter()
                    .filter(|c| matches!(c, Chunk::ToolCall { .. }))
                    .count(),
                call_count
            );
            assert_eq!(
                chunks
                    .iter()
                    .filter(|c| matches!(c, Chunk::Usage { .. }))
                    .count(),
                usage_count,
                "{payload}: {result:?}, chunks={chunks:?}"
            );
        }
    }

    #[test]
    fn markerless_google_text_with_usage_commits_but_markerless_tool_fragment_does_not() {
        use std::io::{Read, Write};
        for (payload, success) in [
            ("data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}\n\n", true),
            ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"echo\",\"arguments\":\"{}\"}}]}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}\n\n", false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut bytes = [0; 4096];
                while let Ok(read) = stream.read(&mut bytes) {
                    if read == 0 { break; }
                    request.extend_from_slice(&bytes[..read]);
                    if request.windows(4).any(|v| v == b"\r\n\r\n") { break; }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                while stream.read(&mut bytes).is_ok_and(|read| read > 0) {}
            });
            let mut settings = Settings::empty();
            let mut cfg = preset("google").unwrap().build("AIza-test");
            cfg.base_url = format!("http://{address}");
            cfg.api = Some("google".into());
            cfg.secret = None;
            cfg.models = vec![ModelInfo::named("fixture", true)];
            let request = Request { control: Default::default(), system: "fixture".into(), messages: vec![Message::user("test")], tools: Vec::new(), settings: ModelSettings::for_provider(&cfg) };
            settings.providers.list.push(cfg);
            let live = Live::new(Arc::new(Mutex::new(settings)));
            let result = live.stream(&request, &mut |_| Flow::Continue);
            worker.join().unwrap();
            assert_eq!(result.is_ok(), success, "{result:?}");
        }
    }

    #[test]
    fn every_preset_keeps_two_round_tool_media_prefixes_and_legal_cache_carriers() {
        fn strip(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove("cache_control");
                    map.remove("prompt_cache_breakpoint");
                    map.values_mut().for_each(strip);
                }
                Value::Array(list) => list.iter_mut().for_each(strip),
                _ => {}
            }
        }
        fn legal(value: &Value, inside_result: bool) {
            match value {
                Value::Object(map) => {
                    assert!(
                        !inside_result
                            || (!map.contains_key("cache_control")
                                && !map.contains_key("prompt_cache_breakpoint")),
                        "nested cache marker: {value}"
                    );
                    let result = value["type"] == "tool_result";
                    for (key, value) in map {
                        legal(value, inside_result || (result && key == "content"));
                    }
                }
                Value::Array(list) => {
                    for value in list {
                        legal(value, inside_result);
                    }
                }
                _ => {}
            }
        }
        let dir = std::env::temp_dir().join(format!("cache-media-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n\0synthetic").unwrap();
        let mut file = Attachment::at(&path).unwrap();
        file.seal(1024);
        let tools = crate::tools::Registry::default().declarations();
        for preset in crate::settings::providers::PRESETS {
            for dialect in [Dialect::OpenAi, Dialect::Anthropic] {
                for parameters in [
                    None,
                    Some(vec![]),
                    Some(vec!["cache_control"]),
                    Some(vec!["prompt_cache_breakpoint", "prompt_cache_key"]),
                ] {
                    let mut cfg = preset.build("");
                    cfg.dialect = dialect;
                    let mut model = ModelInfo::named("arbitrary/model", false);
                    model.tools = Some(true);
                    model.modalities = vec!["text".into(), "image".into()];
                    model.price = Some(crate::settings::providers::Price {
                        cache_write: Some(1.0),
                        ..Default::default()
                    });
                    if let Some(parameters) = parameters {
                        model.known.push("supported_parameters".into());
                        model.supported_parameters =
                            parameters.into_iter().map(String::from).collect();
                    }
                    cfg.models = vec![model];
                    let mut req = Request {
                        control: Default::default(),
                        system: "static".into(),
                        settings: ModelSettings::for_provider(&cfg),
                        tools: tools.clone(),
                        messages: vec![
                            Message::user("first"),
                            Message::assistant("").with_calls(vec![
                                super::super::CallRef {
                                    id: "a".into(),
                                    name: "inspect_file".into(),
                                    args: r#"{"action":"read","path":"image.png"}"#.into(),
                                },
                                super::super::CallRef {
                                    id: "b".into(),
                                    name: "find_files".into(),
                                    args: r#"{"action":"find","pattern":"*.rs"}"#.into(),
                                },
                            ]),
                            Message::tool_result("a", "inspect_file", "external image")
                                .with_media(vec![file.clone()]),
                            Message::tool_result("b", "find_files", "found"),
                        ],
                    };
                    let (_, mut before) = shape(&req, &cfg).unwrap();
                    assert_eq!(before["tools"].as_array().unwrap().len(), tools.len());
                    legal(&before, false);
                    if dialect == Dialect::OpenAi {
                        for message in before["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|m| m["role"] == "tool")
                        {
                            assert!(message["content"].is_string());
                        }
                    }
                    req.messages.push(Message::assistant("done"));
                    req.messages.push(Message::user("next"));
                    let (_, mut after) = shape(&req, &cfg).unwrap();
                    legal(&after, false);
                    strip(&mut before);
                    strip(&mut after);
                    assert_eq!(
                        before["tools"], after["tools"],
                        "{}: {dialect:?}",
                        preset.name
                    );
                    let old = before["messages"].as_array().unwrap();
                    let new = after["messages"].as_array().unwrap();
                    assert_eq!(&new[..old.len()], old, "{}: {dialect:?}", preset.name);
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn media_payloads_follow_capabilities_for_every_dialect_and_model_id() {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("media-wire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.png");
        let bytes = b"\x89PNG\r\n\x1a\n\0image fixture";
        std::fs::write(&path, bytes).unwrap();
        let file = Attachment::at(&path).unwrap();
        for dialect in [Dialect::OpenAi, Dialect::Anthropic] {
            for id in ["custom-vision", "vendor/model", "local-model:latest"] {
                for modalities in [vec!["text", "image"], vec!["text"], vec![]] {
                    let mut cfg = preset("openrouter").unwrap().build("");
                    cfg.dialect = dialect;
                    let mut model = ModelInfo::named(id, false);
                    model.modalities = modalities.iter().map(|m| (*m).into()).collect();
                    cfg.models = vec![model];
                    let request = Request {
                        control: Default::default(),
                        system: "Identify the attached image".into(),
                        messages: vec![
                            Message::user("What is this?").with_media(vec![file.clone()])
                        ],
                        tools: Vec::new(),
                        settings: ModelSettings::for_provider(&cfg),
                    };
                    let (_, body) = shape(&request, &cfg).unwrap();
                    let content = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|m| m["role"] == "user")
                        .unwrap()["content"]
                        .as_array()
                        .unwrap();
                    if modalities.contains(&"image") {
                        assert!(content[0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("<file_attachment path="));
                        assert!(content[0]["text"].as_str().unwrap().contains("photo.png"));
                        assert!(content[2]["text"]
                            .as_str()
                            .unwrap()
                            .contains("</file_attachment>"));
                        assert_eq!(content.last().unwrap()["text"], "What is this?");
                        let part = &content[1];
                        match dialect {
                            Dialect::OpenAi => assert_eq!(
                                part["image_url"]["url"],
                                format!("data:image/png;base64,{}", crate::media::base64(bytes))
                            ),
                            Dialect::Anthropic => {
                                assert_eq!(part["source"]["data"], crate::media::base64(bytes))
                            }
                        }
                    } else {
                        assert_eq!(content[0]["type"], "text");
                        assert!(content[0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("status=\"unsupported_by_model\""));
                        assert!(content[0]["text"].as_str().unwrap().contains("photo.png"));
                        assert!(!content[0].to_string().contains("base64"));
                        assert_eq!(content[1]["text"], "What is this?");
                    }
                }
            }
        }
        let mut model = ModelInfo::named("multimodal", false);
        model.modalities = vec!["image".into(), "video".into()];
        let video = Attachment {
            mime: "video/mp4".into(),
            ..file.clone()
        };
        assert!(video
            .inline_reason(Some(&model), Dialect::OpenAi, MAX_INLINE_BYTES)
            .is_none());
        assert!(video
            .inline_reason(Some(&model), Dialect::Anthropic, MAX_INLINE_BYTES)
            .is_some());
        let large = Attachment {
            size: MAX_INLINE_BYTES + 1,
            ..file.clone()
        };
        assert!(large
            .inline_reason(Some(&model), Dialect::OpenAi, MAX_INLINE_BYTES)
            .is_some());
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            crate::media::decide(&file, Some(&model), Dialect::OpenAi, MAX_INLINE_BYTES),
            crate::media::Inlined::Notice(_)
        ));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn shape_with_image_attachment_does_not_exceed_context_window_via_base64() {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("media-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_shot.png");
        // 120 KB PNG file -> ~160 KB base64. Rune must not turn those bytes into
        // a provider-token value at all; the provider is the accounting authority.
        let bytes = vec![0x89; 120_000];
        std::fs::write(&path, &bytes).unwrap();
        let file = Attachment::at(&path).unwrap();

        let mut cfg = preset("openrouter").unwrap().build("");
        cfg.dialect = Dialect::OpenAi;
        let mut model = ModelInfo::named("vision-model", false);
        model.modalities = vec!["image".into()];
        // A published context window must not cause Rune to reject this request
        // from its own text/base64 estimate. Provider admission and usage decide.
        model.context = Some(20_000);
        model.max_output = Some(4_000);
        cfg.models = vec![model];

        let request = Request {
            control: Default::default(),
            system: "You are a vision assistant.".into(),
            messages: vec![Message::user("What is this screenshot?").with_media(vec![file])],
            tools: Vec::new(),
            settings: ModelSettings::for_provider(&cfg),
        };

        let result = shape(&request, &cfg);
        assert!(
            result.is_ok(),
            "shape must succeed without blowing context window on base64, got {:?}",
            result.err()
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
    #[test]
    fn reasoning_selection_reaches_all_adapter_payloads_without_invented_vendor_efforts() {
        for api in ["9router", "openrouter", "mimo", "anthropic", "google"] {
            let mut cfg = preset(api).unwrap().build("");
            let model = ModelInfo {
                id: "fixture".into(),
                thinking: true,
                efforts: vec!["high".into()],
                reasoning_options: Some(vec!["off".into(), "high".into()]),
                thinking_modes: if api == "anthropic" {
                    vec!["adaptive".into()]
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            cfg.models = vec![model];
            let mut settings = ModelSettings::for_provider(&cfg);
            settings.model = "fixture".into();
            settings.thinking = true;
            settings.effort = Some("high".into());
            settings.normalize_for_model(&cfg);
            assert_eq!(
                settings.effort.as_deref(),
                Some("high"),
                "{api} must retain selected effort"
            );
            let req = Request {
                control: Default::default(),
                system: String::new(),
                messages: vec![Message::user("fixture")],
                tools: Vec::new(),
                settings,
            };
            let (_, body) = shape(&req, &cfg).unwrap();
            let value = match api {
                "anthropic" => &body["output_config"]["effort"],
                "google" => &body["extra_body"]["google"]["thinking_config"]["thinking_level"],
                "openrouter" => &body["reasoning"]["effort"],
                _ => &body["reasoning_effort"],
            };
            assert_eq!(
                value.as_str().unwrap().to_ascii_lowercase(),
                "high",
                "{api}: {body}"
            );
            if api == "9router" || api == "google" {
                let mut req = req;
                req.settings.thinking = false;
                req.settings.effort = None;
                let (_, body) = shape(&req, &cfg).unwrap();
                if api == "9router" {
                    assert!(body.get("reasoning").is_none(), "{body}");
                }
                assert_eq!(body["reasoning_effort"], "none");
            }
        }
    }
}
