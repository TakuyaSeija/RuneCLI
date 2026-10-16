//! Which model endpoints exist, and how to talk to each one.
//!
//! `~/.rune/providers.json` — non-secret by construction: base URLs, model ids
//! and dialect flags. The keys live next door in [`super::secrets`] and are
//! joined only at the moment a request is built. That split is what lets a user
//! copy their provider list into a bug report without thinking about it.
//!
//! # Presets exist because "OpenAI-compatible" is not one protocol
//!
//! Every endpoint here speaks `/chat/completions`, and they still disagree about
//! the things that matter: MiMo authenticates with an `api-key` header and turns
//! reasoning on with a `thinking` object, OpenAI wants `Authorization: Bearer`
//! and an effort level, a local Ollama wants no credential at all. A preset is
//! those differences written down once, so adding a provider is answering one
//! question — the key — instead of five.
//!
//! # The trap the MiMo preset exists to close
//!
//! Xiaomi issues two key formats against **two different hosts**: `sk-` for
//! pay-as-you-go on `api.xiaomimimo.com`, `tp-` for a Token Plan subscription on
//! `token-plan-cn.xiaomimimo.com`. A `tp-` key sent to the first host fails with
//! an authentication error that blames the key rather than the address, and a
//! user can lose an evening to it. [`Preset::base_url_for`] reads the prefix and
//! picks the right host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

pub const FILE: &str = "providers.json";

/// How a provider wants the key presented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Auth {
    /// Header name, e.g. `api-key` or `Authorization`.
    pub header: String,
    /// What goes in front of the key, e.g. `Bearer `. Usually empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
}

impl Auth {
    pub fn bearer() -> Auth {
        Auth {
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        }
    }

    pub fn api_key() -> Auth {
        Auth {
            header: "api-key".into(),
            prefix: String::new(),
        }
    }
}

/// Which protocol an endpoint speaks.
///
/// Two, and there is no ambition for a third: `OpenAi` covers every
/// `/chat/completions` endpoint — MiMo, OpenAI, OpenRouter, Google, xAI,
/// DeepSeek, Groq, Mistral, 9Router, Ollama — and `Anthropic` covers
/// `/v1/messages`, which is genuinely a different protocol rather than a dialect
/// of the first. A gateway that normalises Anthropic models into the OpenAI
/// shape is `OpenAi`; only talking to Anthropic itself is `Anthropic`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    #[default]
    OpenAi,
    Anthropic,
}

/// How this endpoint is asked to think out loud.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    /// No reasoning control at all; the model does what it does.
    None,
    /// `"thinking": {"type": "enabled"|"disabled"}` — Xiaomi MiMo.
    Object,
    /// `"reasoning_effort": "low"|"medium"|"high"` — OpenAI's reasoning models.
    Effort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelInfo {
    pub id: String,
    /// Which parsed fields the vendor actually supplied, including explicit false/empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manual_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
    /// Largest `max_completion_tokens` this model accepts. The settings panel
    /// clamps to it rather than letting the user type a number that is rejected
    /// only after a request has been paid for.
    ///
    /// `None` when nobody has published it — most `/models` endpoints return an
    /// id and nothing else. An unknown ceiling clamps nothing, which is right:
    /// the alternative is refusing a number the provider would have accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    /// Total context window, for the usage meter.
    ///
    /// **Not the same number as `max_output`,** which is the mistake this field
    /// was carrying: `mimo-v2.5-pro` was published here with a 131,072 context
    /// because that is its output ceiling. Its real window is 1,050,000, so the
    /// meter read eight times fuller than the truth.
    ///
    /// `None` when it is not known, and then **no meter is drawn at all**. A
    /// guessed window is worse than no window: it is an authoritative-looking
    /// bar measuring nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Whether reasoning can be turned on for this model.
    #[serde(default)]
    pub thinking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<Vec<String>>,
    /// Whether reasoning is mandatory for this model (cannot be turned off).
    #[serde(default)]
    pub thinking_required: bool,
    /// Specific effort choices supported by this model (from provider API metadata).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    /// Request parameters explicitly supported by this model (e.g. from API supported_parameters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_parameters: Vec<String>,
    /// What a million tokens cost. Absent for a provider nobody has priced —
    /// a local Ollama, or one the user added by hand — and the screen then shows
    /// no spend rather than a made-up zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// Published inputs: `text`, `image`, `audio`, `video`, `file`.
    ///
    /// OpenRouter publishes it as `architecture.input_modalities` and is the only
    /// endpoint that does; empty means unknown, while `["text"]` explicitly
    /// means text only. Unknown binary inputs are described: a file sent to a model that
    /// cannot take it is a rejected request, while one described in a line of
    /// text is a task the agent can still do with its own tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modalities: Vec<String>,
}

impl ModelInfo {
    /// Capability and user selection are separate: selecting Off never erases
    /// vendor support. A 9router model offers every level the router reads
    /// ([`ROUTER_LEVELS`]), whatever its upstream's own API names (owner,
    /// 2026-10-15): the router turns each into that upstream's control.
    pub fn reasoning_choices(&self, api: &str) -> Vec<String> {
        if self.reasoning_support() == Some(false) {
            return Vec::new();
        }
        // Support nobody published (an id added by hand) is the person's to
        // settle in Alt+C: offered what the adapter reads (owner, 2026-10-15).
        let mut choices = if api == "9router" {
            self.router_levels()
        } else if !self.efforts.is_empty() {
            self.efforts.clone()
        } else {
            vec!["on".into()]
        };
        if !self.thinking_required {
            choices.insert(0, "off".into());
        }
        choices
    }

    /// Whether this model's thinking is translated by 9router, which passes a
    /// request that asks for no level on untouched, so the route's own level
    /// holds (9router 0.5.99, read 2026-10-15), the one a route's name carries
    /// (`…-high`) included: "Route default". The router does this for every
    /// model it serves — a row with a thinking format of its own and one
    /// without alike — so it is the provider's fact, never the row's.
    pub fn route_default_level(&self, api: &str) -> bool {
        api == "9router" && self.reasoning_support() == Some(true)
    }

    /// Whether a session on this model starts thinking when the person has
    /// chosen nothing for it in a session: what the person set for the model
    /// (Settings › Models › Alt+C › Reasoning) first, then what the catalogue
    /// says. Every 9router model starts Off (owner, 2026-10-15): Off is
    /// `none`, no level shown beside a name that may carry one. One reading
    /// for a new session and a model switch alike.
    pub fn starts_thinking(&self, api: &str) -> bool {
        if self.reasoning_support() == Some(false) {
            return false;
        }
        if self.thinking_required
            || self
                .reasoning_options
                .as_ref()
                .is_some_and(|options| !options.iter().any(|o| o == "off"))
        {
            return true;
        }
        self.reasoning_enabled
            .or_else(|| {
                self.metadata
                    .pointer("/reasoning/default_enabled")
                    .and_then(Value::as_bool)
            })
            .unwrap_or(self.thinking && api != "9router")
    }

    /// The level a session starts with on this model: the default the model
    /// names (the catalogue's, or the person's), none where the route applies
    /// its own, else the least it offers.
    pub fn starting_effort(&self, api: &str) -> Option<String> {
        self.default_effort
            .clone()
            .filter(|level| self.efforts.is_empty() || self.efforts.contains(level))
            .or_else(|| {
                (!self.route_default_level(api))
                    .then(|| self.efforts.first().cloned())
                    .flatten()
            })
    }

    /// Settle a 9router model's thinking to what the router reads
    /// ([`ProviderConfig::settle_thinking`]).
    ///
    /// Its levels are the router's ([`ROUTER_LEVELS`]), or the ones the person
    /// configured for the model (`reasoning_options`): the router publishes no
    /// level list per model, so whatever an earlier build stored here was
    /// derived, never published, and is derived again.
    ///
    /// No level is a default unless the person set one: a level chosen here
    /// was sent to every route, so `ag/gemini-3.8-flash-high` was asked for
    /// `medium` and shown as "…-high medium" (owner, 2026-10-15).
    ///
    /// Thinking is mandatory only where the person made it so: the router
    /// reads `none` for every model, and on one that cannot stop thinking
    /// (`capabilities.thinkingCanDisable: false`) asks for its least instead
    /// ([`Self::thinking_off_asks_least`]), so Off is always a request it
    /// honours (owner, 2026-10-15).
    pub fn settle_router_thinking(&mut self) {
        let manual = |field: &str| self.manual_fields.iter().any(|f| f == field);
        let (efforts, default_effort, required) = (
            manual("efforts"),
            manual("default_effort"),
            manual("thinking_required"),
        );
        if !required {
            self.thinking_required = false;
        }
        if self.reasoning_support() != Some(true) {
            return;
        }
        if !efforts {
            let configured: Vec<String> = self
                .reasoning_options
                .iter()
                .flatten()
                .filter(|level| !matches!(level.as_str(), "off" | "on" | "none" | "auto"))
                .cloned()
                .collect();
            self.efforts = if configured.is_empty() {
                self.router_levels()
            } else {
                configured
            };
            sort_efforts(&mut self.efforts);
        }
        if !default_effort {
            self.default_effort = None;
        }
    }

    /// This model as its catalogue published it, without what the person set
    /// for it: a field the person overrode lost the published value, so it is
    /// unknown until the catalogue is read again.
    pub fn without_persons_choices(&self) -> ModelInfo {
        let mut model = self.clone();
        for field in std::mem::take(&mut model.manual_fields) {
            match field.as_str() {
                "context" => model.context = None,
                "max_output" => model.max_output = None,
                "thinking" => {
                    model.thinking = model
                        .metadata
                        .pointer("/capabilities/reasoning")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                }
                "thinking_required" => model.thinking_required = false,
                "efforts" => model.efforts.clear(),
                "default_effort" => model.default_effort = None,
                "modalities" => model.modalities.clear(),
                "supported_parameters" => model.supported_parameters.clear(),
                price if price == "price" || price.starts_with("price.") => model.price = None,
                _ => {}
            }
        }
        model.reasoning_enabled = None;
        model.reasoning_options = None;
        model
    }

    /// Mark what this model, as the person wrote it, says about its thinking
    /// as theirs (`manual_fields`), so settling keeps it: a mandatory, a level
    /// list, a default level.
    pub fn keep_written_thinking(&mut self) {
        for (field, written) in [
            ("thinking_required", self.thinking_required),
            ("efforts", !self.efforts.is_empty()),
            ("default_effort", self.default_effort.is_some()),
        ] {
            if written && !self.manual_fields.iter().any(|f| f == field) {
                self.manual_fields.push(field.into());
            }
        }
    }

    /// The router's levels this route can be asked for ([`ROUTER_LEVELS`]
    /// without what its format refuses, [`ROUTER_REFUSED`]).
    pub fn router_levels(&self) -> Vec<String> {
        let capabilities = self.metadata.pointer("/capabilities");
        let format = capabilities
            .and_then(|c| c.get("thinkingFormat"))
            .and_then(Value::as_str);
        let ranged = capabilities
            .and_then(|c| c.get("thinkingRange"))
            .is_some_and(|range| !range.is_null());
        ROUTER_LEVELS
            .into_iter()
            .filter(|level| {
                ranged
                    || !ROUTER_REFUSED
                        .iter()
                        .any(|(refusing, refused)| format == Some(*refusing) && level == refused)
            })
            .map(String::from)
            .collect()
    }

    /// Whether Off on this route asks for its least thinking rather than
    /// none, as the router publishes it (`capabilities.thinkingCanDisable:
    /// false`; 9router 0.5.99 turns `none` into `minimal` there, read
    /// 2026-10-15, and `ag/gemini-3.8-flash-low` with Off still thought and
    /// returned its thinking, live the same day). The request is still `none`;
    /// the screen says what it does.
    pub fn thinking_off_asks_least(&self) -> bool {
        self.metadata
            .pointer("/capabilities/thinkingCanDisable")
            .and_then(Value::as_bool)
            == Some(false)
    }

    /// Whether "off" on this route stops thinking only between tool calls, as
    /// the router publishes it (`capabilities.thinkingOffType`): measured
    /// 2026-10-14, Antigravity's Claude Sonnet 5.5 still thinks before it
    /// answers with `reasoning_effort: none`. The request is right; the screen
    /// says what it will do.
    pub fn thinking_off_between_tools_only(&self) -> bool {
        self.metadata
            .pointer("/capabilities/thinkingOffType")
            .and_then(Value::as_str)
            == Some("between_tools")
    }

    pub fn reasoning_support(&self) -> Option<bool> {
        if let Some(value) = self
            .metadata
            .pointer("/capabilities/reasoning")
            .and_then(Value::as_bool)
        {
            return Some(value);
        }
        if self.thinking {
            Some(true)
        } else if self.known.iter().any(|k| k == "thinking") {
            Some(false)
        } else {
            None
        }
    }

    pub fn supports_parameter(&self, key: &str) -> Option<bool> {
        if key == "tools" && self.tools.is_some() {
            return self.tools;
        }
        if self.supported_parameters.is_empty()
            && !self.known.iter().any(|k| k == "supported_parameters")
        {
            return None;
        }
        Some(self.supported_parameters.iter().any(|p| p == key))
    }
    /// Can this model be shown a file of this kind?
    pub fn accepts(&self, modality: &str) -> bool {
        self.modalities.iter().any(|m| m == modality)
    }

    pub fn named(id: &str, thinking: bool) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            thinking,
            known: vec!["thinking".into()],
            ..Default::default()
        }
    }
}

/// Numerical ranking for sorting reasoning effort from lowest to highest intensity.
pub fn effort_rank(s: &str) -> i32 {
    match s.to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => 10,
    }
}

/// The thinking levels 9router reads in `reasoning_effort`, least first: its
/// level table (`Dl` in 9router 0.5.99, read 2026-10-15), which it turns into
/// each upstream's own control for every model it serves. Two levels can
/// reach an upstream as one (DeepSeek knows only high and max), which is the
/// router's to decide. `ultra` is not among them: the router reads it only as
/// a `model(ultra)` suffix and passes it on raw where a level is copied.
pub const ROUTER_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Levels the router turns into a request its upstream refuses, by the
/// router's thinking format, on a route that publishes no `thinkingRange` the
/// router would hold a budget to. `claude-budget`: `minimal` becomes a
/// 512-token budget (`Dl`), under the 1024 Anthropic's API takes — HTTP 400
/// live on `cc/claude-haiku-4-5-20251001`, 2026-10-15 — and the router's own
/// level table for the format leaves it out; hidden until a published range
/// says otherwise (owner, 2026-10-15). A format, never a model.
const ROUTER_REFUSED: &[(&str, &str)] = &[("claude-budget", "minimal")];

/// The effort choice that asks for no level, so the route applies its own
/// ([`ModelInfo::route_default_level`]); a session holds it as thinking on
/// with no effort.
pub const ROUTE_DEFAULT: &str = "default";

/// Sort effort choices in ascending order of intensity (lowest first, highest last).
pub fn sort_efforts(efforts: &mut Vec<String>) {
    efforts.retain(|e| e != "off" && e != "none" && e != "disabled");
    efforts.sort_by_key(|e| effort_rank(e));
    efforts.dedup();
}

/// US dollars per **million** tokens, as the provider publishes them.
///
/// Three numbers rather than two: a cached prompt token is charged at a small
/// fraction of a fresh one — for MiMo, 0.8% of it — and a session that reuses a
/// long prompt is priced completely wrongly without that distinction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Price {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<RateOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_context_threshold: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_start_utc_hour: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_end_utc_hour: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateOverride {
    pub min_prompt_tokens: u64,
    pub inclusive: bool,
    pub rates: BTreeMap<String, f64>,
}

impl Price {
    pub fn cost(
        &self,
        input: u64,
        output: u64,
        cached: u64,
        details: &crate::provider::UsageDetails,
    ) -> Option<f64> {
        let short = details.cache_write_5m.unwrap_or(0);
        let long = details.cache_write_1h.unwrap_or(0);
        let written = details.cache_write.unwrap_or(short.saturating_add(long));
        let other = written.checked_sub(short.checked_add(long)?)?;
        let fresh = input.checked_sub(cached.checked_add(written)?)?;
        let busy = if self.busy_input.is_some() || self.busy_output.is_some() {
            let start = self.busy_start_utc_hour?;
            let end = self.busy_end_utc_hour?;
            let hour = (details.started_at_ms?.div_euclid(3_600_000).rem_euclid(24)) as u8;
            if start < end {
                hour >= start && hour < end
            } else {
                hour >= start || hour < end
            }
        } else {
            false
        };
        let in_rate = match self.tier_context_threshold {
            Some(thresh) if input > thresh => self.tier_input.unwrap_or(self.input),
            _ => self.input,
        };
        let out_rate = match self.tier_context_threshold {
            Some(thresh) if input > thresh => self.tier_output.unwrap_or(self.output),
            _ => self.output,
        };
        let tier = self
            .overrides
            .iter()
            .filter(|t| {
                if t.inclusive {
                    input >= t.min_prompt_tokens
                } else {
                    input > t.min_prompt_tokens
                }
            })
            .max_by_key(|t| t.min_prompt_tokens);
        let rate = |key: &str, fallback: Option<f64>| {
            if busy {
                match key {
                    "input" => return self.busy_input,
                    "output" => return self.busy_output,
                    _ => {}
                }
            }
            if self
                .tier_context_threshold
                .is_some_and(|threshold| input > threshold)
            {
                match key {
                    "input" => return self.tier_input,
                    "output" => return self.tier_output,
                    _ => {}
                }
            }
            tier.and_then(|t| t.rates.get(key).copied()).or(fallback)
        };
        let mut amount = fresh as f64 * rate("input", Some(in_rate))?
            + cached as f64 * rate("cached", Some(self.cached))?
            + output as f64 * rate("output", Some(out_rate))?;
        for (count, rate) in [
            (
                short,
                rate(
                    "cache_write_5m",
                    rate("cache_write", self.cache_write_5m.or(self.cache_write)),
                ),
            ),
            (long, rate("cache_write_1h", self.cache_write_1h)),
            (other, rate("cache_write", self.cache_write)),
        ] {
            if count > 0 {
                amount += count as f64 * rate?;
            }
        }
        let cost = amount / 1_000_000.0;
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }
}

/// The request size a provider is held to when its configuration names none.
pub const DEFAULT_MAX_PAYLOAD_SIZE: &str = "30MiB";

pub fn default_max_payload_size() -> String {
    DEFAULT_MAX_PAYLOAD_SIZE.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    /// Adapter identity survives renaming the provider in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default = "default_max_payload_size")]
    pub max_payload_size: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_tool_choice: bool,
    /// Local name, and by default the key's name in `secrets.json`.
    pub name: String,
    pub base_url: String,
    pub auth: Auth,
    /// Which protocol to speak. Absent in every file written before Anthropic
    /// existed here, and absent means the one everything else uses.
    #[serde(default)]
    pub dialect: Dialect,
    #[serde(default = "no_thinking")]
    pub thinking: Thinking,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelInfo>,
    /// Which entry of `secrets.json` holds this provider's key. Absent means the
    /// provider needs no key at all — a local Ollama or a self-hosted gateway.
    ///
    /// It is also the name of the **ring**: a provider may hold several keys,
    /// and a rate-limited one is rotated to the back automatically. See
    /// [`super::secrets`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// How long one streamed call may take, in seconds.
    ///
    /// Absent means the engine's default, which is generous because a long
    /// reasoning turn legitimately runs for minutes. That generosity is wrong
    /// for an endpoint the user knows to be fast or flaky — a machine on the
    /// desk, a gateway that hangs rather than refusing — and only they know
    /// which theirs is. So no number is guessed here: the default stands until
    /// someone who knows sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Headers this endpoint wants beyond auth and content type.
    ///
    /// OpenRouter is the reason: it attributes traffic by `HTTP-Referer` and
    /// `X-Title`, and a request without them is anonymous rather than rejected —
    /// which is exactly the kind of difference that is invisible until someone
    /// asks why an app never appears in its own rankings. Data, not code, so a
    /// gateway that wants a header nobody has heard of is a file edit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn max_payload_bytes(&self) -> u64 {
        crate::settings::configuration::parse_size_sum(&self.max_payload_size)
            .expect("max_payload_size is checked when the provider is configured")
    }

    /// This endpoint's payload ceiling, with the text limit the request was
    /// given — the user's live `attachments.max_text_chars` (audit D13: the
    /// wire used to hard-code `60_000` and ignore the setting).
    pub fn media_limits(&self, text_chars: Option<usize>) -> crate::media::MediaLimits {
        crate::media::MediaLimits {
            max_payload_bytes: self.max_payload_bytes(),
            text_chars: text_chars
                .unwrap_or_else(|| crate::media::MediaLimits::default().text_chars),
        }
    }
}

fn no_thinking() -> Thinking {
    Thinking::None
}
/// Whether a model is a chat or text generation model based on vendor metadata and capabilities.
pub fn is_chat_model(m: &ModelInfo) -> bool {
    if let Some(chat) = m.chat {
        return chat;
    }
    // If input modalities are explicitly declared by the API and text is absent, it is non-chat.
    if !m.modalities.is_empty() && !m.modalities.iter().any(|modality| modality == "text") {
        return false;
    }
    // Fallback Rule III debt: third-party /models endpoint publishes only ID without modality metadata as of 2026-09
    let id = m.id.to_ascii_lowercase();
    let non_chat = [
        "embed",
        "embedding",
        "tts",
        "whisper",
        "dall-e",
        "moderation",
        "realtime",
        "transcribe",
        "rerank",
        "search",
    ];
    !non_chat.iter().any(|bad| id.contains(bad))
}

impl ProviderConfig {
    /// The adapter this provider speaks through: the preset it was built from,
    /// recorded in `api`, or `compatible` — a plain OpenAI-shaped endpoint —
    /// when it names none. Never guessed from a URL or a name: a provider is
    /// what its configuration says it is.
    pub fn api_kind(&self) -> &str {
        self.api.as_deref().unwrap_or("compatible")
    }

    /// Settle what this provider's adapter decides about its models' thinking
    /// ([`ModelInfo::settle_router_thinking`] for 9router), wherever models
    /// enter it — loaded, fetched or configured — so every reader sees one
    /// answer.
    pub fn settle_thinking(&mut self) {
        if self.api_kind() == "9router" {
            for model in &mut self.models {
                model.settle_router_thinking();
            }
        }
    }
    /// Validate before any probe can transmit credentials, and before saving.
    pub fn check(&self) -> Result<()> {
        if self
            .api
            .as_ref()
            .is_some_and(|api| api != "compatible" && preset(api).is_none())
        {
            return Err(Error::invalid("unknown provider adapter identity"));
        }
        if self.name.trim().is_empty() {
            return Err(Error::invalid("a provider needs a name"));
        }
        if let Some(seconds) = self.timeout {
            if !(5..=3_600).contains(&seconds) {
                return Err(Error::invalid(
                    "a timeout must be between 5 and 3600 seconds",
                ));
            }
        }
        let uri: ureq::http::Uri = self
            .base_url
            .parse()
            .map_err(|_| Error::invalid("base_url must be a valid HTTP endpoint"))?;
        if uri.host().is_none_or(str::is_empty)
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        {
            return Err(Error::invalid(
                "base_url needs a host and cannot contain credentials",
            ));
        }
        let payload_bytes = crate::settings::configuration::parse_size_sum(&self.max_payload_size)
            .ok_or_else(|| {
                Error::invalid("invalid max_payload_size (expected e.g. '30MiB', '100MiB')")
            })?;
        if payload_bytes == 0 || payload_bytes > 1024 * 1024 * 1024 {
            return Err(Error::invalid(
                "provider max_payload_size must be positive and at most 1GiB",
            ));
        }
        match self.base_url.split_once("://") {
            Some(("https", _)) => {}
            Some(("http", _)) if is_loopback(&self.base_url) => {}
            Some(("http", _)) => {
                return Err(Error::invalid(
                    "plain http:// is only allowed for a server on this machine — a key sent \
                     over http can be read in transit; use https://",
                ))
            }
            _ => {
                return Err(Error::invalid(
                    "base_url must start with http:// or https://",
                ))
            }
        }
        for (name, value) in &self.headers {
            if name.parse::<ureq::http::HeaderName>().is_err()
                || value.parse::<ureq::http::HeaderValue>().is_err()
            {
                return Err(Error::invalid(
                    "provider headers contain an invalid name or value",
                ));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for model in &self.models {
            if model.id.trim().is_empty()
                || model.id.len() > 256
                || model.id.chars().any(char::is_control)
                || !ids.insert(&model.id)
            {
                return Err(Error::invalid(
                    "model IDs must be nonempty, unique and at most 256 characters",
                ));
            }
            if model.context == Some(0) || model.max_output == Some(0) {
                return Err(Error::invalid(
                    "published model token limits must be positive",
                ));
            }
            if let Some(p) = &model.price {
                let rates = [
                    Some(p.input),
                    Some(p.output),
                    Some(p.cached),
                    p.cache_write,
                    p.cache_write_5m,
                    p.cache_write_1h,
                    p.tier_input,
                    p.tier_output,
                    p.busy_input,
                    p.busy_output,
                ];
                if rates
                    .into_iter()
                    .flatten()
                    .chain(p.overrides.iter().flat_map(|t| t.rates.values().copied()))
                    .any(|v| !v.is_finite() || v < 0.0)
                {
                    return Err(Error::invalid(
                        "model prices must be finite nonnegative numbers",
                    ));
                }
                let mut conditions = std::collections::HashSet::new();
                if p.overrides.iter().any(|tier| {
                    !conditions.insert((tier.min_prompt_tokens, tier.inclusive))
                        || tier.rates.keys().any(|key| {
                            ![
                                "input",
                                "output",
                                "cached",
                                "cache_write",
                                "cache_write_5m",
                                "cache_write_1h",
                            ]
                            .contains(&key.as_str())
                        })
                }) {
                    return Err(Error::invalid("price overrides contain duplicate conditions or unsupported rate dimensions"));
                }
                if p.tier_context_threshold == Some(0)
                    || (p.tier_context_threshold.is_some()
                        && (p.tier_input.is_none() || p.tier_output.is_none()))
                    || (p.tier_context_threshold.is_none()
                        && (p.tier_input.is_some() || p.tier_output.is_some()))
                {
                    return Err(Error::invalid("tier pricing requires a positive threshold and both input and output rates"));
                }
                if p.busy_input.is_some() != p.busy_output.is_some() {
                    return Err(Error::invalid(
                        "peak pricing requires both input and output rates",
                    ));
                }
                if [p.busy_start_utc_hour, p.busy_end_utc_hour]
                    .into_iter()
                    .flatten()
                    .any(|h| h > 23)
                {
                    return Err(Error::invalid(
                        "peak pricing requires start and end UTC hours from 0 to 23",
                    ));
                }
            }
            if model
                .modalities
                .iter()
                .any(|m| !["text", "image", "audio", "video", "file"].contains(&m.as_str()))
            {
                return Err(Error::invalid("model has an unknown input modality"));
            }
        }
        if self.name.len() > 128 || self.name.chars().any(char::is_control) {
            return Err(Error::invalid(
                "provider name must be 1–128 characters without control characters",
            ));
        }
        Ok(())
    }

    /// Where a request goes, which depends on the protocol and nothing else.
    pub fn endpoint(&self) -> String {
        match self.dialect {
            Dialect::OpenAi => crate::provider::openai::endpoint(self),
            Dialect::Anthropic => crate::provider::anthropic::endpoint(self),
        }
    }
    pub fn model(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The model a new session gets when the user has not chosen: the first one
    /// listed. Ordering in the file is therefore meaningful, which is why the
    /// presets put the strongest model first.
    pub fn default_model(&self) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|m| is_chat_model(m))
            .or_else(|| self.models.first())
    }
}

// ─── Presets ──────────────────────────────────────────────────────────────────

/// A provider Rune already knows how to configure.
pub struct Preset {
    pub name: &'static str,
    /// Shown when asking for the key, so the user knows which one to paste.
    pub key_hint: &'static str,
    /// Every prefix a valid key for this provider may start with. Empty means
    /// no prefix is known, so nothing can be rejected on shape alone.
    pub key_prefixes: &'static [&'static str],
    build: fn(&str) -> ProviderConfig,
}

impl Preset {
    /// The configuration this preset produces for `key`.
    ///
    /// The key is passed in — not stored — because for some providers it decides
    /// the base URL. It is never written into the returned config.
    pub fn build(&self, key: &str) -> ProviderConfig {
        let mut config = (self.build)(key);
        config.api = Some(self.name.into());
        match self.name {
            "google" => {
                config.max_payload_size = "100MiB".into();
            }
            "anthropic" => {
                config.max_payload_size = "32MiB".into();
                config.force_tool_choice = true;
            }
            "openai" => {
                config.max_payload_size = "50MiB".into();
                config.force_tool_choice = true;
            }
            "groq" => config.max_payload_size = "25MiB".into(),
            "deepseek" => config.max_payload_size = "32MiB".into(),
            "xai" => config.max_payload_size = "30MiB".into(),
            "mistral" => config.max_payload_size = "30MiB".into(),
            "openrouter" => config.max_payload_size = "30MiB".into(),
            "9router" => config.max_payload_size = "30MiB".into(),
            "ollama" => config.max_payload_size = "30MiB".into(),
            "mimo" => config.max_payload_size = "30MiB".into(),
            _ => {}
        }
        config
    }

    /// Whether `key` looks like a credential this provider issues.
    ///
    /// A guess, and deliberately a permissive one: it exists to catch the paste
    /// of an obviously wrong key, not to be the authority. The authority is a
    /// real request, which `provider.test` makes before anything is saved.
    pub fn key_looks_right(&self, key: &str) -> bool {
        self.key_prefixes.is_empty() || self.key_prefixes.iter().any(|p| key.starts_with(p))
    }
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "mimo",
        key_hint: "Xiaomi MiMo key — sk-… for pay-as-you-go, tp-… for a Token Plan",
        key_prefixes: &["sk-", "tp-"],
        build: |key| ProviderConfig {
            api: None,
            max_payload_size: "30MiB".into(),
            force_tool_choice: false,
            name: "mimo".into(),
            base_url: mimo_host(key).into(),
            auth: Auth::api_key(),
            dialect: Dialect::OpenAi,
            thinking: Thinking::Object,
            models: Vec::new(),
            secret: Some("mimo".into()),
            timeout: None,
            headers: BTreeMap::new(),
        },
    },
    Preset {
        name: "openai",
        key_hint: "OpenAI key — sk-…",
        key_prefixes: &["sk-"],
        build: |_| ProviderConfig {
            api: None,
            max_payload_size: "50MiB".into(),
            force_tool_choice: false,
            name: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            auth: Auth::bearer(),
            dialect: Dialect::OpenAi,
            thinking: Thinking::Effort,
            models: Vec::new(),
            secret: Some("openai".into()),
            timeout: None,
            headers: BTreeMap::new(),
        },
    },
    Preset {
        name: "openrouter",
        key_hint: "OpenRouter key — sk-or-…",
        key_prefixes: &["sk-or-", "sk-"],
        build: |_| ProviderConfig {
            api: None,
            max_payload_size: "30MiB".into(),
            force_tool_choice: false,
            name: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            auth: Auth::bearer(),
            dialect: Dialect::OpenAi,
            thinking: Thinking::Effort,
            models: Vec::new(),
            secret: Some("openrouter".into()),
            timeout: None,
            headers: attribution(),
        },
    },
    Preset {
        name: "ollama",
        key_hint: "no key needed — Ollama runs on this machine",
        key_prefixes: &[],
        build: |key| ProviderConfig {
            api: None,
            max_payload_size: "30MiB".into(),
            force_tool_choice: false,
            name: "ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            auth: Auth {
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
            },
            dialect: Dialect::OpenAi,
            thinking: Thinking::None,
            models: Vec::new(),
            secret: (!key.is_empty()).then(|| "ollama".into()),
            timeout: None,
            headers: BTreeMap::new(),
        },
    },
    Preset {
        name: "anthropic",
        key_hint: "Anthropic key — sk-ant-…",
        key_prefixes: &["sk-ant-"],
        build: |_| ProviderConfig {
            api: None,
            max_payload_size: "32MiB".into(),
            force_tool_choice: false,
            name: "anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            // Not `Authorization: Bearer`. Getting this wrong is a 401 that
            // reads like a bad key, which is a whole evening.
            auth: Auth {
                header: "x-api-key".into(),
                prefix: String::new(),
            },
            dialect: Dialect::Anthropic,
            thinking: Thinking::Effort,
            models: Vec::new(),
            secret: Some("anthropic".into()),
            // Required on every request, and versioned rather than negotiated:
            // the endpoint refuses a request that does not name a version.
            timeout: None,
            headers: BTreeMap::from([("anthropic-version".to_string(), "2023-06-01".to_string())]),
        },
    },
    Preset {
        name: "google",
        key_hint: "Google AI Studio key — AIza…",
        key_prefixes: &["AIza", "AQ."],
        build: |_| ProviderConfig {
            api: None,
            max_payload_size: "100MiB".into(),
            force_tool_choice: false,
            name: "google".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            auth: Auth::bearer(),
            dialect: Dialect::OpenAi,
            thinking: Thinking::Effort,
            models: Vec::new(),
            secret: Some("google".into()),
            timeout: None,
            headers: BTreeMap::new(),
        },
    },
    Preset {
        name: "xai",
        key_hint: "xAI key — xai-…",
        key_prefixes: &["xai-"],
        build: |_| {
            let mut c = openai_compatible("xai", "https://api.x.ai/v1", Thinking::Effort);
            c.max_payload_size = "30MiB".into();
            c.models = Vec::new();
            c
        },
    },
    Preset {
        name: "deepseek",
        key_hint: "DeepSeek key — sk-…",
        key_prefixes: &["sk-"],
        build: |_| {
            let mut c =
                openai_compatible("deepseek", "https://api.deepseek.com/v1", Thinking::Effort);
            c.max_payload_size = "32MiB".into();
            c.models = Vec::new();
            c
        },
    },
    Preset {
        name: "groq",
        key_hint: "Groq key — gsk_…",
        key_prefixes: &["gsk_"],
        build: |_| {
            let mut c =
                openai_compatible("groq", "https://api.groq.com/openai/v1", Thinking::Effort);
            c.max_payload_size = "25MiB".into();
            c.models = Vec::new();
            c
        },
    },
    Preset {
        name: "mistral",
        key_hint: "Mistral key",
        key_prefixes: &[],
        build: |_| {
            let mut c = openai_compatible("mistral", "https://api.mistral.ai/v1", Thinking::None);
            c.max_payload_size = "30MiB".into();
            c.models = Vec::new();
            c
        },
    },
    Preset {
        // A gateway on this machine, so no TLS. Accepts an optional API key when
        // gateway authentication is configured.
        name: "9router",
        key_hint: "9Router API key — press enter if no key is configured",
        key_prefixes: &[],
        build: |key| ProviderConfig {
            api: None,
            max_payload_size: "30MiB".into(),
            force_tool_choice: false,
            name: "9router".into(),
            base_url: "http://localhost:20128/v1".into(),
            auth: Auth::bearer(),
            dialect: Dialect::OpenAi,
            thinking: Thinking::Effort,
            models: Vec::new(),
            secret: (!key.is_empty()).then(|| "9router".into()),
            timeout: None,
            headers: BTreeMap::new(),
        },
    },
];

/// The shape almost every vendor shares: one host, `Bearer`, a catalogue that is
/// asked for rather than written down.
///
/// A function instead of six near-identical literals — the difference between
/// these providers is three values, and writing them out six times is how one of
/// them quietly ends up with the wrong auth header.
fn openai_compatible(name: &str, base_url: &str, thinking: Thinking) -> ProviderConfig {
    ProviderConfig {
        api: None,
        max_payload_size: "30MiB".into(),
        force_tool_choice: false,
        name: name.into(),
        base_url: base_url.into(),
        auth: Auth::bearer(),
        dialect: Dialect::OpenAi,
        thinking,
        // Left empty on purpose: none of these publish ceilings or prices in a
        // form worth hard-coding, and inventing them is the bug this file
        // already paid for once. `rune -provider models` fills it from the
        // endpoint, keeping whatever it actually says.
        models: Vec::new(),
        secret: Some(name.into()),
        timeout: None,
        headers: BTreeMap::new(),
    }
}

/// What OpenRouter uses to attribute a request to the app that made it.
fn attribution() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "HTTP-Referer".to_string(),
            "https://github.com/rune-cli".to_string(),
        ),
        ("X-Title".to_string(), "Rune".to_string()),
    ])
}

pub fn preset(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

/// Whether a URL points at this machine, and so may be spoken to without TLS.
///
/// A local Ollama has no certificate and needs none: the bytes never leave the
/// loopback interface. Every other host is different in kind — `http://` there
/// puts the user's API key on the wire in the clear, and a provider list is
/// exactly where someone pastes a base URL a stranger gave them. So the scheme
/// is not a preference here, it is a boundary.
pub(crate) fn is_loopback(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Credentials first, then the port. IPv6 is bracketed, so its colons are
    // part of the address and only the brackets say where the host ends.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Xiaomi's two hosts, chosen by which kind of key the user holds.
fn mimo_host(key: &str) -> &'static str {
    if key.starts_with("tp-") {
        "https://token-plan-cn.xiaomimimo.com/v1"
    } else {
        "https://api.xiaomimimo.com/v1"
    }
}

// ─── The file ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Providers {
    #[serde(skip)]
    revision: std::cell::RefCell<Value>,
    #[serde(skip)]
    configured_models: BTreeMap<String, Vec<ModelInfo>>,
    #[serde(default)]
    pub list: Vec<ProviderConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub excluded_models: BTreeMap<String, Vec<String>>,
    /// Name of the provider a new session uses. The first one added, unless the
    /// user says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip)]
    path: PathBuf,
    /// Where the fetched catalogue is kept: the store's (`store::catalogue`).
    #[serde(skip)]
    catalogue: crate::store::catalogue::Catalogue,
}

fn merge_catalogue(configured: &mut Vec<ModelInfo>, fetched: &[ModelInfo]) {
    for existing in configured.iter_mut() {
        existing.listed = Some(false);
    }
    for model in fetched {
        if let Some(existing) = configured.iter_mut().find(|m| m.id == model.id) {
            let old = existing.clone();
            *existing = model.clone();
            let keep = |key: &str| {
                old.manual_fields
                    .iter()
                    .any(|k| k == key || (key == "price" && k.starts_with("price.")))
                    || !(model.known.iter().any(|k| k == key)
                        || match key {
                            "context" => model.context.is_some(),
                            "max_output" => model.max_output.is_some(),
                            "price" => model.price.is_some(),
                            "thinking" => model.thinking,
                            "modalities" => !model.modalities.is_empty(),
                            "efforts" => !model.efforts.is_empty(),
                            "supported_parameters" => !model.supported_parameters.is_empty(),
                            _ => false,
                        })
            };
            if keep("context") {
                existing.context = old.context.or(model.context);
            }
            if keep("max_output") {
                existing.max_output = old.max_output.or(model.max_output);
            }
            if keep("price") {
                existing.price = old.price.clone().or(model.price.clone());
            }
            if existing.price.is_none()
                && old.price.is_some()
                && old
                    .manual_fields
                    .iter()
                    .any(|k| k == "price" || k.starts_with("price."))
            {
                existing.price = old.price.clone();
            }
            if let (Some(previous), Some(current)) = (&old.price, &mut existing.price) {
                let previous = serde_json::to_value(previous).expect("validated price");
                let mut value = serde_json::to_value(&*current).expect("validated price");
                for key in old
                    .manual_fields
                    .iter()
                    .filter_map(|k| k.strip_prefix("price."))
                {
                    if let Some(rate) = previous.get(key) {
                        value[key] = rate.clone();
                    } else {
                        value.as_object_mut().unwrap().remove(key);
                    }
                }
                *current = serde_json::from_value(value).expect("validated price override");
            }
            if keep("modalities") && !old.modalities.is_empty() {
                existing.modalities = old.modalities;
            }
            if keep("thinking") {
                existing.thinking = old.thinking;
                existing.thinking_required = old.thinking_required;
            }
            // Reconcile the reasoning flag with the catalogue's own evidence. A
            // row written by the model wizard before any catalogue fetch carries
            // `thinking=false` even when the endpoint advertises reasoning, which
            // silently disqualified it from effort verification. 9Router owns
            // this fact; adopt it unless the user pinned `thinking` by hand.
            if !old.manual_fields.iter().any(|k| k == "thinking") {
                if let Some(advertised) = model
                    .metadata
                    .pointer("/capabilities/reasoning")
                    .and_then(Value::as_bool)
                {
                    if advertised && !existing.thinking {
                        existing.thinking = true;
                        if !existing.known.iter().any(|k| k == "thinking") {
                            existing.known.push("thinking".into());
                        }
                    }
                }
            }
            // A level list a fetch does not repeat is kept: a refresh must not
            // clear one, whatever the row's `known` set claims. A provider whose
            // catalogue carries an effort list (OpenRouter, Google) arrives with
            // `model.efforts` populated, and the union below keeps both; a
            // 9router model's levels are the router's, settled again after the
            // merge (`ProviderConfig::settle_thinking`).
            if !old.efforts.is_empty() {
                let mut merged = model.efforts.clone();
                merged.extend(old.efforts.iter().cloned());
                merged.dedup();
                sort_efforts(&mut merged);
                existing.efforts = merged;
            }
            if let Some(default) = old.default_effort.clone() {
                if existing.default_effort.is_none() {
                    existing.default_effort = Some(default)
                        .filter(|v| existing.efforts.contains(v))
                        .or_else(|| existing.efforts.first().cloned());
                }
            }
            if keep("supported_parameters") && !old.supported_parameters.is_empty() {
                existing.supported_parameters = old.supported_parameters;
            }
            existing.reasoning_enabled = old.reasoning_enabled;
            existing.reasoning_options = old.reasoning_options;
            existing.manual_fields = old.manual_fields;
        } else {
            configured.push(model.clone());
        }
    }
}

impl Providers {
    /// The providers configured in `home`'s `rune.toml`, with the catalogue
    /// kept in `catalogue` merged in.
    pub fn load(home: &Path, catalogue: crate::store::catalogue::Catalogue) -> Result<Providers> {
        let config = super::configuration::Configuration::load(home)?;
        let mut p: Providers = if config.get("providers").is_null() {
            Providers::default()
        } else {
            serde_json::from_value(config.get("providers").clone())?
        };
        p.path = home.join(super::configuration::FILE);
        p.catalogue = catalogue;
        p.revision.replace(config.get("providers").clone());
        // rune.toml holds the person's choices: what a model written there says
        // about its thinking is one the adapter's settling keeps.
        for config in &mut p.list {
            if config.api_kind() == "9router" {
                for model in &mut config.models {
                    model.keep_written_thinking();
                }
            }
        }
        // Capture the explicit (rune.toml) model lists BEFORE merging the cached
        // catalogue from rune.db, so `configured_models` stays the user's own config —
        // not the user's config plus whatever the last fetch cached. The cache must
        // never be promoted into the configured set (that is the whole point of the
        // provenance test below).
        p.configured_models = p
            .list
            .iter()
            .map(|c| (c.name.clone(), c.models.clone()))
            .collect();

        // The fetched catalogue, read whole from the store. A row that does
        // not read is an error, not a model quietly missing from the list.
        let mut stored = p.catalogue.read()?;
        for config in &mut p.list {
            let Some(rows) = stored.remove(&config.name) else {
                continue;
            };
            let db_models = rows
                .iter()
                .map(|info| {
                    serde_json::from_str::<ModelInfo>(info).map_err(|e| {
                        Error::invalid(format!(
                            "the stored model catalogue of {} is unreadable: {e}",
                            config.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if db_models.is_empty() {
                continue;
            }
            merge_catalogue(&mut config.models, &db_models);
            let configured_entry = p.configured_models.entry(config.name.clone()).or_default();
            for m in &db_models {
                if !m.manual_fields.is_empty() || m.listed == Some(false) {
                    if let Some(existing) = configured_entry.iter_mut().find(|x| x.id == m.id) {
                        *existing = m.clone();
                    } else {
                        configured_entry.push(m.clone());
                    }
                }
            }
        }

        // The catalogue is read from provider_models (above) only. The old Catalog.json
        // plaintext cache is retired — a provider's model list is runtime configuration
        // and lives in rune.db, not in a second, drifting JSON file.
        for config in &mut p.list {
            if config.models.is_empty() {
                if let Some(preset_cfg) = preset(&config.name) {
                    config.models = preset_cfg.build("").models;
                }
            }
            if let Some(hidden) = p.excluded_models.get(&config.name) {
                config.models.retain(|m| !hidden.contains(&m.id));
            }
            for m in &mut config.models {
                sort_efforts(&mut m.efforts);
            }
            config.settle_thinking();
            config.check()?;
        }
        Ok(p)
    }

    pub fn get(&self, name: &str) -> Option<&ProviderConfig> {
        self.list.iter().find(|p| p.name == name)
    }

    /// The provider a session should use when it has not been told one.
    pub fn active(&self) -> Option<&ProviderConfig> {
        match &self.default {
            Some(name) => self.get(name).or_else(|| self.list.first()),
            None => self.list.first(),
        }
    }

    /// Add or replace a provider. The first one added becomes the default —
    /// a single-provider install should never have to answer that question.
    pub fn add(&mut self, config: ProviderConfig) -> Result<()> {
        config.check()?;
        let previous = self.clone();
        if self
            .get(&config.name)
            .is_none_or(|old| old.models != config.models)
        {
            self.configured_models
                .insert(config.name.clone(), config.models.clone());
        }
        match self.list.iter_mut().find(|p| p.name == config.name) {
            Some(existing) => *existing = config,
            None => {
                if self.default.is_none() {
                    self.default = Some(config.name.clone());
                }
                self.list.push(config);
            }
        }
        if let Err(error) = self.save() {
            *self = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Write down a model the user named that this provider does not list.
    ///
    /// Without it a hand-typed model is set and then immediately unreachable:
    /// the settings screen walks the catalogue with `←` `→`, so the next arrow
    /// press moves to a model that *is* listed and the typed one is gone. The
    /// owner's report was exactly that — type a model, arrow away, and it is not
    /// there.
    ///
    /// Nothing is invented around the id: no window, no ceiling, no price. The
    /// user asserting the id is the only evidence there is that it exists — no
    /// endpoint publishes a complete list — and a guessed window would draw a
    /// meter measuring nothing.
    ///
    /// `Ok(false)` means it was already listed and nothing was written.
    pub fn remember_model(&mut self, provider: &str, id: &str) -> Result<bool> {
        let Some(config) = self.get(provider) else {
            return Ok(false);
        };
        if id.is_empty() || config.model(id).is_some() {
            return Ok(false);
        }
        let mut updated = config.clone();
        updated.models = self
            .configured_models
            .get(provider)
            .cloned()
            .unwrap_or_else(|| config.models.clone());
        // A model the catalogue holds — one deleted before, kept as published —
        // comes back as the catalogue's; only an id it never listed is bare.
        let published = self
            .catalogue
            .read()?
            .remove(provider)
            .into_iter()
            .flatten()
            .filter_map(|row| serde_json::from_str::<ModelInfo>(&row).ok())
            .find(|model| model.id == id);
        let mut model = published.unwrap_or_else(|| ModelInfo {
            id: id.to_string(),
            listed: Some(false),
            ..Default::default()
        });
        if config.api_kind() == "9router" {
            model.settle_router_thinking();
        }
        updated.models.push(model.clone());
        let previous = self.clone();
        if let Some(hidden) = self.excluded_models.get_mut(provider) {
            hidden.retain(|m| m != id);
        }
        self.configured_models
            .insert(provider.into(), updated.models.clone());
        if let Some(existing) = self.list.iter_mut().find(|p| p.name == provider) {
            existing.models.push(model);
        }
        if let Err(e) = self.save() {
            *self = previous;
            return Err(e);
        }
        Ok(true)
    }

    /// Update metadata for a model (context window, max output, price, etc.).
    pub fn update_model_metadata(&mut self, provider: &str, mut model: ModelInfo) -> Result<()> {
        if self.get(provider).is_none() {
            return Err(Error::NotFound(format!("provider {provider}")));
        }
        let mut validated = self.get(provider).unwrap().clone();
        if let Some(old) = validated.models.iter_mut().find(|m| m.id == model.id) {
            *old = model.clone();
        } else {
            validated.models.push(model.clone());
        }
        validated.check()?;
        if let Some(old) = self.get(provider).and_then(|p| p.model(&model.id)) {
            let incoming_manual = model.manual_fields.clone();
            model.manual_fields = old.manual_fields.clone();
            for m in incoming_manual {
                if !model.manual_fields.contains(&m) {
                    model.manual_fields.push(m);
                }
            }
            for (key, changed) in [
                (
                    "context",
                    model.context.is_some()
                        && (old.context != model.context
                            || !model.manual_fields.contains(&"context".to_string())),
                ),
                (
                    "max_output",
                    model.max_output.is_some()
                        && (old.max_output != model.max_output
                            || !model.manual_fields.contains(&"max_output".to_string())),
                ),
                (
                    "thinking",
                    old.thinking != model.thinking
                        || model.manual_fields.contains(&"thinking".to_string()),
                ),
                (
                    "thinking_required",
                    old.thinking_required != model.thinking_required
                        || model
                            .manual_fields
                            .contains(&"thinking_required".to_string()),
                ),
                (
                    "efforts",
                    old.efforts != model.efforts
                        || model.manual_fields.contains(&"efforts".to_string()),
                ),
                (
                    "default_effort",
                    old.default_effort != model.default_effort
                        || model.manual_fields.contains(&"default_effort".to_string()),
                ),
            ] {
                if changed && !model.manual_fields.iter().any(|k| k == key) {
                    model.manual_fields.push(key.into());
                }
            }
            if model.price.is_some() && !model.manual_fields.contains(&"price".to_string()) {
                model.manual_fields.push("price".into());
            }
            let old_price = serde_json::to_value(&old.price)?;
            let new_price = serde_json::to_value(&model.price)?;
            if let Some(fields) = new_price.as_object() {
                for (key, value) in fields {
                    let field = format!("price.{key}");
                    if old_price.get(key) != Some(value) && !model.manual_fields.contains(&field) {
                        model.manual_fields.push(field);
                    }
                }
            }
        } else {
            model.manual_fields = ["context", "max_output", "price"]
                .into_iter()
                .map(String::from)
                .collect();
            // A model the person adds with a default level, or with levels of
            // its own, chose them; a 9router model's levels and level are
            // otherwise the router's and the route's.
            if model.default_effort.is_some() {
                model.manual_fields.push("default_effort".into());
            }
            if !model.efforts.is_empty() {
                model.manual_fields.push("efforts".into());
            }
        }
        let previous = self.clone();
        if let Some(hidden) = self.excluded_models.get_mut(provider) {
            hidden.retain(|m| m != &model.id);
        }
        if let Some(existing) = self.list.iter_mut().find(|p| p.name == provider) {
            if let Some(m) = existing.models.iter_mut().find(|m| m.id == model.id) {
                *m = model.clone();
            } else {
                existing.models.push(model.clone());
            }
            existing.settle_thinking();
            if let Some(settled) = existing.model(&model.id) {
                model = settled.clone();
            }
        }
        let configured = self
            .configured_models
            .entry(provider.to_string())
            .or_default();
        if let Some(m) = configured.iter_mut().find(|m| m.id == model.id) {
            *m = model.clone();
        } else {
            configured.push(model.clone());
        }
        if let Err(e) = self.save() {
            *self = previous;
            return Err(e);
        }
        Ok(())
    }

    /// Forget a model previously remembered or configured for `provider`.
    pub fn forget_model(&mut self, provider: &str, id: &str) -> Result<bool> {
        if id.is_empty() {
            return Ok(false);
        }
        let Some(config) = self.get(provider) else {
            return Ok(false);
        };
        if config.model(id).is_none() {
            return Ok(false);
        }
        // What the catalogue published stays, hidden, as a refresh keeps it;
        // what the person set goes. A model only the person ever wrote is
        // gone whole.
        let published = config
            .model(id)
            .filter(|model| model.fetched_at.is_some())
            .map(ModelInfo::without_persons_choices);
        let previous = self.clone();
        let mut changed = false;
        if let Some(models) = self.configured_models.get_mut(provider) {
            let before = models.len();
            models.retain(|m| m.id != id);
            if models.len() != before {
                changed = true;
            }
        }
        if let Some(existing) = self.list.iter_mut().find(|p| p.name == provider) {
            let before = existing.models.len();
            existing.models.retain(|m| m.id != id);
            if existing.models.len() != before {
                changed = true;
            }
        }
        let hidden = self.excluded_models.entry(provider.into()).or_default();
        if !hidden.iter().any(|m| m == id) {
            hidden.push(id.into());
        }
        if changed {
            let stored = match &published {
                Some(model) => serde_json::to_string(model)
                    .map_err(Error::from)
                    .and_then(|row| {
                        self.catalogue
                            .upsert(&[(provider.to_string(), id.to_string(), row)])
                    }),
                None => self.catalogue.forget_model(provider, id),
            };
            if let Err(e) = stored.and_then(|()| self.save()) {
                *self = previous;
                return Err(e);
            }
        }
        Ok(changed)
    }

    /// Persist fetched catalogue data separately from explicit user overrides.
    /// The catalogue lives in `rune.db` (`provider_models`), not a plaintext JSON file:
    /// a provider's model list is runtime configuration, and the DB is its one home.
    pub fn cache_models(&mut self, name: &str, models: Vec<ModelInfo>) -> Result<()> {
        let mut checked = self
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?
            .clone();
        // The stored set is the user's explicit configuration with the fetched
        // catalogue merged over it — never the bare catalogue. A bare replace
        // deleted rows the wizard had written (manual context/output/price and
        // hand-added models), so a configuration that saved fine vanished on
        // the next start whenever a catalogue refresh ran in between.
        // `merge_catalogue` keeps every `manual_fields` override.
        let mut stored = self
            .configured_models
            .get(name)
            .cloned()
            .unwrap_or_default();
        if stored.is_empty() {
            if let Some(cfg) = self.get(name) {
                stored = cfg
                    .models
                    .iter()
                    .filter(|m| !m.manual_fields.is_empty() || m.listed == Some(false))
                    .cloned()
                    .collect();
            }
        }
        merge_catalogue(&mut stored, &models);
        checked.models = stored;
        checked.settle_thinking();
        let stored = checked.models.clone();
        checked.check()?;
        // The merged set replaces this provider's stored rows, so the next
        // load reads it from the same place.
        let rows = stored
            .iter()
            .map(|model| Ok((model.id.clone(), serde_json::to_string(model)?)))
            .collect::<Result<Vec<_>>>()?;
        self.catalogue.replace(name, &rows)?;
        if let Some(config) = self.list.iter_mut().find(|p| p.name == name) {
            config.models = stored.clone();
            if let Some(hidden) = self.excluded_models.get(name) {
                config.models.retain(|m| !hidden.contains(&m.id));
            }
        }
        let configured_subset: Vec<ModelInfo> = stored
            .iter()
            .filter(|m| !m.manual_fields.is_empty() || m.listed == Some(false))
            .cloned()
            .collect();
        self.configured_models
            .insert(name.to_string(), configured_subset);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let previous = self.clone();
        let before = self.list.len();
        self.list.retain(|p| p.name != name);
        if self.default.as_deref() == Some(name) {
            self.default = self.list.first().map(|p| p.name.clone());
        }
        let removed = self.list.len() != before;
        if removed {
            self.configured_models.remove(name);
            if let Err(e) = self
                .catalogue
                .forget_provider(name)
                .and_then(|()| self.save())
            {
                *self = previous;
                return Err(e);
            }
        }
        Ok(removed)
    }

    pub fn set_default(&mut self, name: &str) -> Result<()> {
        if self.get(name).is_none() {
            return Err(Error::NotFound(format!("provider {name}")));
        }
        let previous = self.default.clone();
        self.default = Some(name.to_string());
        if let Err(error) = self.save() {
            self.default = previous;
            return Err(error);
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            // An in-memory configuration, as used by tests. Writing would put a
            // providers.json in whatever directory the process happens to be in.
            return Err(Error::invalid("this configuration has nowhere to be saved"));
        }
        let home = self
            .path
            .parent()
            .ok_or_else(|| Error::invalid("configuration path has no parent"))?;

        // 1. Every model into the store's catalogue, never into rune.toml.
        let mut rows = Vec::new();
        let all = self
            .list
            .iter()
            .map(|config| (&config.name, &config.models))
            .chain(self.configured_models.iter());
        for (provider, models) in all {
            for model in models {
                rows.push((
                    provider.clone(),
                    model.id.clone(),
                    serde_json::to_string(model)?,
                ));
            }
        }
        self.catalogue.upsert(&rows)?;

        // 2. Clear models so that models = [...] is NEVER written into rune.toml!
        let mut explicit = self.clone();
        for config in &mut explicit.list {
            config.models.clear();
        }
        super::configuration::Configuration::edit(
            home,
            Some("providers"),
            Some(serde_json::to_value(&explicit)?),
            false,
            Some(&self.revision.borrow()),
        )?;
        self.revision.replace(serde_json::to_value(explicit)?);
        Ok(())
    }

    /// Where this configuration's catalogue is kept.
    pub fn catalogue(&self) -> &crate::store::catalogue::Catalogue {
        &self.catalogue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The providers of `dir`'s `rune.toml`, with the catalogue in the
    /// database beside it — the layout of an install.
    fn load(dir: &Path) -> Result<Providers> {
        Providers::load(
            dir,
            crate::store::catalogue::Catalogue::at(
                &dir.join("rune.db"),
                super::super::configuration::Shared::load(dir)?,
            ),
        )
    }

    /// A provider is the adapter its configuration names — never one guessed
    /// from its URL or its name. Naming none means a plain OpenAI-shaped
    /// endpoint.
    #[test]
    fn a_providers_adapter_is_what_its_configuration_names() {
        let mut built = preset("openrouter").unwrap().build("");
        assert_eq!(built.api.as_deref(), Some("openrouter"));
        assert_eq!(built.api_kind(), "openrouter");
        built.api = None;
        assert_eq!(built.api_kind(), "compatible", "the URL decides nothing");
        let mut named = preset("9router").unwrap().build("");
        named.api = None;
        assert_eq!(named.api_kind(), "compatible", "neither does the name");
    }

    /// Audit D13: the wire honours the user's attachment text limit; only a
    /// request with none set falls back to the shipped default.
    #[test]
    fn the_text_limit_on_the_wire_is_the_configured_one() {
        let cfg = preset("openai").unwrap().build("k");
        assert_eq!(cfg.media_limits(Some(1234)).text_chars, 1234);
        assert_eq!(
            cfg.media_limits(None).text_chars,
            crate::media::MediaLimits::default().text_chars
        );
    }

    /// This test used to resolve `Db::home_dir()` — the developer's *real*
    /// `~/.rune` — and run `Configuration::edit` against it. Every registered
    /// config key was therefore written into the user's own `rune.toml` on every
    /// `cargo test`, and a running older binary then rejected that file as holding
    /// unknown keys, taking its entire tool layer down with it. It also asserted
    /// nothing and swallowed the error, so it could never fail and nobody noticed
    /// what it was doing. It now runs against a temp home like every other test in
    /// this file, and checks the two things `save()` actually promises.
    #[test]
    fn saving_writes_models_to_the_database_and_never_back_into_rune_toml() {
        let dir = home("sync-models-to-db");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        providers
            .update_model_metadata(
                "openrouter",
                ModelInfo {
                    id: "cbai/glm-5.3".into(),
                    context: Some(1_000_000),
                    ..Default::default()
                },
            )
            .unwrap();

        load(&dir).unwrap().save().unwrap();

        let toml = std::fs::read_to_string(dir.join("rune.toml")).unwrap();
        assert!(
            !toml.contains("glm-5.3"),
            "the model list belongs in rune.db, never in rune.toml: {toml}"
        );
        // …and the save is not destructive: it survives a reload.
        let reloaded = load(&dir).unwrap();
        assert_eq!(
            reloaded
                .get("openrouter")
                .unwrap()
                .model("cbai/glm-5.3")
                .unwrap()
                .context,
            Some(1_000_000),
            "a save must not drop the model it was saving"
        );
    }

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-prov-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The owner's report: a model configured through the wizard saved fine,
    /// then vanished after restarting — but only sometimes. The "sometimes" was
    /// a catalogue refresh: `cache_models` deleted every `provider_models` row
    /// for the provider and wrote back only the bare fetched catalogue, so the
    /// wizard's row (context, prices, `manual_fields`) was destroyed in between
    /// the save and the next start. The stored set must be the user's explicit
    /// configuration with the catalogue merged over it — never the catalogue alone.
    #[test]
    fn a_refresh_between_the_wizard_and_a_restart_keeps_the_wizard_configuration() {
        let dir = home("wizard-survives-refresh");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        let configured = ModelInfo {
            id: "cbai/glm-5.3".into(),
            context: Some(1_000_000),
            price: Some(Price {
                input: 0.20,
                output: 0.80,
                cached: 0.05,
                ..Default::default()
            }),
            manual_fields: vec![
                "context".into(),
                "price.input".into(),
                "price.cached".into(),
                "price.output".into(),
            ],
            ..Default::default()
        };
        providers
            .update_model_metadata("openrouter", configured.clone())
            .unwrap();

        // A catalogue refresh happens in between — exactly the "kadang" path.
        let fetched = ModelInfo {
            id: "cbai/glm-5.3".into(),
            known: vec!["context".into()],
            ..Default::default()
        };
        providers.cache_models("openrouter", vec![fetched]).unwrap();

        // Restart: the provider list is rebuilt from rune.toml + rune.db.
        let reloaded = load(&dir).unwrap();
        let model = reloaded
            .get("openrouter")
            .unwrap()
            .model("cbai/glm-5.3")
            .unwrap();
        assert_eq!(
            model.context,
            Some(1_000_000),
            "wizard context must survive the refresh"
        );
        let price = model
            .price
            .clone()
            .expect("wizard price must survive the refresh");
        assert!((price.input - 0.20).abs() < 1e-9);
        assert!((price.output - 0.80).abs() < 1e-9);
    }

    /// Levels the person gave a model they added are theirs: a refresh — and
    /// the router's own levels — must not replace them.
    #[test]
    fn verified_efforts_survive_a_catalogue_refresh() {
        let dir = home("efforts-survive-refresh");
        let mut providers = load(&dir).unwrap();
        providers.add(preset("9router").unwrap().build("")).unwrap();
        let verified = ModelInfo {
            id: "cbai/glm-5.3".into(),
            thinking: true,
            efforts: vec!["low".into(), "high".into()],
            default_effort: Some("high".into()),
            known: vec!["thinking".into(), "efforts".into()],
            metadata: serde_json::json!({"capabilities":{"reasoning":true,"thinkingFormat":"zai"}}),
            ..Default::default()
        };
        providers
            .update_model_metadata("9router", verified)
            .unwrap();

        // A catalogue refresh arrives with a row that carries an empty effort
        // list but claims `efforts` is known — the exact shape that dropped them.
        let fetched = ModelInfo {
            id: "cbai/glm-5.3".into(),
            thinking: true,
            known: vec!["thinking".into(), "efforts".into()],
            metadata: serde_json::json!({"capabilities":{"reasoning":true,"thinkingFormat":"zai"}}),
            ..Default::default()
        };
        providers.cache_models("9router", vec![fetched]).unwrap();

        let model = providers
            .get("9router")
            .unwrap()
            .model("cbai/glm-5.3")
            .unwrap();
        assert_eq!(
            model.efforts,
            vec!["low".to_string(), "high".to_string()],
            "the person's levels must survive a refresh"
        );
        assert_eq!(model.default_effort.as_deref(), Some("high"));
    }

    /// A model written by the wizard before any catalogue fetch carries
    /// `thinking=false`; the catalogue's own `reasoning:true` must be adopted so
    /// 9Router effort verification is not silently skipped for it.
    #[test]
    fn catalogue_reasoning_reconciles_a_stale_thinking_flag() {
        let dir = home("thinking-reconciled");
        let mut providers = load(&dir).unwrap();
        providers.add(preset("9router").unwrap().build("")).unwrap();
        let wizard_written = ModelInfo {
            id: "cbai/deepseek-v4.1-flash".into(),
            thinking: false,
            context: Some(1_000_000),
            manual_fields: vec!["context".into()],
            ..Default::default()
        };
        providers
            .update_model_metadata("9router", wizard_written)
            .unwrap();

        let fetched = ModelInfo {
            id: "cbai/deepseek-v4.1-flash".into(),
            thinking: true,
            known: vec!["thinking".into()],
            metadata: serde_json::json!({"capabilities":{"reasoning":true,"thinkingFormat":"openai"}}),
            ..Default::default()
        };
        providers.cache_models("9router", vec![fetched]).unwrap();

        let model = providers
            .get("9router")
            .unwrap()
            .model("cbai/deepseek-v4.1-flash")
            .unwrap();
        assert!(
            model.thinking,
            "catalogue reasoning:true must reconcile the stale local flag"
        );
        assert_eq!(
            model.context,
            Some(1_000_000),
            "the wizard's context is still respected"
        );
    }

    /// A hand-named model the endpoint does not list must also survive the
    /// same refresh-and-restart path: the old replace deleted its row outright.
    #[test]
    fn a_model_the_catalogue_does_not_list_survives_the_refresh() {
        let dir = home("custom-model-survives-refresh");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        providers
            .remember_model("openrouter", "my-private-fine-tune")
            .unwrap();
        providers.cache_models("openrouter", Vec::new()).unwrap();

        let reloaded = load(&dir).unwrap();
        assert!(
            reloaded
                .get("openrouter")
                .unwrap()
                .model("my-private-fine-tune")
                .is_some(),
            "a hand-added model must survive a catalogue refresh and a restart"
        );
    }

    #[test]
    fn a_refresh_after_restart_keeps_wizard_configured_model() {
        let dir = home("wizard-survives-restart-then-refresh");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        let configured = ModelInfo {
            id: "cbai/glm-5.3".into(),
            context: Some(1_000_000),
            price: Some(Price {
                input: 0.20,
                output: 0.80,
                cached: 0.05,
                ..Default::default()
            }),
            ..Default::default()
        };
        providers
            .update_model_metadata("openrouter", configured.clone())
            .unwrap();

        // 1. Restart happens FIRST (process exit and relaunch):
        let mut reloaded = load(&dir).unwrap();

        // 2. Catalogue refresh happens AFTER restart:
        let fetched = ModelInfo {
            id: "cbai/glm-5.3".into(),
            known: vec!["context".into()],
            ..Default::default()
        };
        reloaded.cache_models("openrouter", vec![fetched]).unwrap();

        // 3. Wizard configuration must still be intact in memory:
        let model = reloaded
            .get("openrouter")
            .unwrap()
            .model("cbai/glm-5.3")
            .unwrap();
        assert_eq!(
            model.context,
            Some(1_000_000),
            "wizard context must survive refresh after restart"
        );
        let price = model
            .price
            .clone()
            .expect("wizard price must survive refresh after restart");
        assert!((price.input - 0.20).abs() < 1e-9);
        assert!((price.output - 0.80).abs() < 1e-9);

        // 4. Restart again and verify DB persistence:
        let reloaded2 = load(&dir).unwrap();
        let model2 = reloaded2
            .get("openrouter")
            .unwrap()
            .model("cbai/glm-5.3")
            .unwrap();
        assert_eq!(
            model2.context,
            Some(1_000_000),
            "wizard context must persist across second restart"
        );
        let price2 = model2
            .price
            .clone()
            .expect("wizard price must persist across second restart");
        assert!((price2.input - 0.20).abs() < 1e-9);
        assert!((price2.output - 0.80).abs() < 1e-9);
    }

    #[test]
    fn a_custom_model_survives_restart_then_refresh() {
        let dir = home("custom-model-survives-restart-then-refresh");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        providers
            .remember_model("openrouter", "my-private-fine-tune")
            .unwrap();

        // 1. Restart happens FIRST:
        let mut reloaded = load(&dir).unwrap();

        // 2. Catalogue refresh happens AFTER restart:
        let other_model = ModelInfo::named("some-other-model", false);
        reloaded
            .cache_models("openrouter", vec![other_model])
            .unwrap();

        // 3. Custom model must still exist:
        assert!(
            reloaded
                .get("openrouter")
                .unwrap()
                .model("my-private-fine-tune")
                .is_some(),
            "a hand-added model must survive when catalogue refresh happens after restart"
        );

        // 4. Second restart:
        let reloaded2 = load(&dir).unwrap();
        assert!(
            reloaded2
                .get("openrouter")
                .unwrap()
                .model("my-private-fine-tune")
                .is_some(),
            "a hand-added model must persist across second restart"
        );
    }

    /// The window and the output ceiling are two different numbers, and the
    /// meter believed the smaller one for both.
    #[test]
    fn cache_creation_tiers_and_scheduled_prices_use_disjoint_tokens() {
        let mut price = Price {
            input: 1.0,
            cached: 0.1,
            output: 2.0,
            cache_write: Some(1.25),
            ..Default::default()
        };
        let details = crate::provider::UsageDetails {
            cache_write: Some(500),
            ..Default::default()
        };
        assert!((price.cost(1500, 10, 900, &details).unwrap() - 0.000835).abs() < 1e-12);
        price.overrides.push(RateOverride {
            min_prompt_tokens: 1500,
            inclusive: true,
            rates: BTreeMap::from([("cached".into(), 0.2), ("cache_write".into(), 2.5)]),
        });
        assert!((price.cost(1500, 10, 900, &details).unwrap() - 0.00155).abs() < 1e-12);
        price.busy_input = Some(3.0);
        price.busy_output = Some(4.0);
        assert!(price.cost(100, 10, 0, &Default::default()).is_none());
        price.busy_start_utc_hour = Some(22);
        price.busy_end_utc_hour = Some(6);
        for (hour, expected) in [(23, 0.00034), (12, 0.00012)] {
            let details = crate::provider::UsageDetails {
                started_at_ms: Some(hour * 3_600_000),
                ..Default::default()
            };
            assert!((price.cost(100, 10, 0, &details).unwrap() - expected).abs() < 1e-12);
        }
    }

    #[test]
    fn catalogue_refresh_revokes_capabilities_without_promoting_cache_to_config() {
        let dir = home("catalog-provenance");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        let first = ModelInfo {
            id: "fixture".into(),
            thinking: true,
            efforts: vec!["high".into()],
            context: Some(10000),
            known: vec!["thinking".into(), "efforts".into(), "context".into()],
            ..Default::default()
        };
        providers.cache_models("openrouter", vec![first]).unwrap();
        let mut back = load(&dir).unwrap();
        back.remember_model("openrouter", "custom").unwrap();
        let config = super::super::configuration::Configuration::load(&dir).unwrap();
        assert!(config.get("providers")["list"][0]["models"]
            .as_array()
            .is_none_or(|a| a.iter().all(|m| m["id"] != "fixture")));
        let revoked = ModelInfo {
            id: "fixture".into(),
            known: vec!["thinking".into(), "efforts".into(), "context".into()],
            ..Default::default()
        };
        back.cache_models("openrouter", vec![revoked]).unwrap();
        let model = back.get("openrouter").unwrap().model("fixture").unwrap();
        assert!(!model.thinking);
        assert!(model.efforts.is_empty());
        assert_eq!(model.context, None);
        back.cache_models("openrouter", Vec::new()).unwrap();
        assert!(back.get("openrouter").unwrap().model("fixture").is_none());
        assert!(back.get("openrouter").unwrap().model("custom").is_some());
    }

    #[test]
    fn invalid_model_edits_leave_both_stores_unchanged() {
        let dir = home("invalid-price-edit");
        let mut providers = load(&dir).unwrap();
        providers
            .add(preset("openrouter").unwrap().build(""))
            .unwrap();
        providers
            .cache_models("openrouter", vec![ModelInfo::named("fixture", false)])
            .unwrap();
        let config = std::fs::read(dir.join(super::super::configuration::FILE)).unwrap();
        // The catalogue store is provider_models in rune.db now; snapshot it, then prove
        // an invalid edit changes neither the TOML config nor the DB catalogue.
        let db_catalogue = providers.catalogue().read().unwrap();
        let model = ModelInfo {
            id: "fixture".into(),
            price: Some(Price {
                cache_write: Some(f64::NAN),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(providers
            .update_model_metadata("openrouter", model)
            .is_err());
        assert_eq!(
            std::fs::read(dir.join(super::super::configuration::FILE)).unwrap(),
            config
        );
        let after = providers.catalogue().read().unwrap();
        assert_eq!(
            db_catalogue, after,
            "an invalid edit must not touch the DB catalogue either"
        );
    }

    #[test]
    fn the_published_window_is_not_the_output_ceiling() {
        let price = Price {
            input: 0.435,
            cached: 0.0036,
            output: 0.87,
            tier_context_threshold: Some(200_000),
            tier_input: Some(0.87),
            tier_output: Some(1.74),
            ..Default::default()
        };
        assert!(price.cached < price.input && price.input < price.output);
        let normal_cost = price.cost(100_000, 10_000, 0, &Default::default());
        let tier_cost = price.cost(300_000, 10_000, 0, &Default::default());
        assert!(tier_cost > normal_cost);
    }

    /// A catalogue copied into the user's file the day they added a provider
    /// must not outlive the vendor's own numbers.
    #[test]
    fn explicit_model_metadata_survives_preset_changes() {
        let dir = home("stale");
        let mut p = Providers {
            revision: std::cell::RefCell::new(Value::Null),
            configured_models: BTreeMap::new(),
            list: Vec::new(),
            excluded_models: BTreeMap::new(),
            default: None,
            path: dir.join(FILE),
            catalogue: crate::store::catalogue::Catalogue::at(
                &dir.join("rune.db"),
                super::super::configuration::Shared::load(&dir).unwrap(),
            ),
        };
        let mut stale = preset("mimo").unwrap().build("sk-x");
        stale.models.push(ModelInfo {
            id: "mimo-v2.5-pro".into(),
            context: Some(131_072),
            price: None,
            ..Default::default()
        });
        stale.base_url = "https://api.xiaomimimo.com/v1".into();
        p.add(stale).unwrap();

        let back = load(&dir).unwrap();
        let model = back.get("mimo").unwrap().model("mimo-v2.5-pro").unwrap();
        assert_eq!(
            model.context,
            Some(131_072),
            "explicit metadata must remain authoritative"
        );
        assert!(model.price.is_none());
        // What the user chose is still theirs.
        assert_eq!(
            back.get("mimo").unwrap().base_url,
            "https://api.xiaomimimo.com/v1"
        );
        assert_eq!(back.default.as_deref(), Some("mimo"));
    }

    /// The owner's report: type a model id, press enter, then walk the list
    /// with `←` `→` — and the model just typed is not in it. The screen walks
    /// the catalogue, so a model that never joined the catalogue is set and then
    /// stepped straight past.
    #[test]
    fn a_model_typed_by_hand_joins_the_list_and_survives_a_reload() {
        let dir = home("typed");
        let mut p = Providers {
            revision: std::cell::RefCell::new(Value::Null),
            configured_models: BTreeMap::new(),
            list: Vec::new(),
            excluded_models: BTreeMap::new(),
            default: None,
            path: dir.join(FILE),
            catalogue: crate::store::catalogue::Catalogue::at(
                &dir.join("rune.db"),
                super::super::configuration::Shared::load(&dir).unwrap(),
            ),
        };
        p.add(preset("mimo").unwrap().build("sk-x")).unwrap();
        assert!(p.get("mimo").unwrap().model("mimo-v2.5-flash").is_none());

        assert!(p.remember_model("mimo", "mimo-v2.5-flash").unwrap());
        let listed = p
            .get("mimo")
            .unwrap()
            .model("mimo-v2.5-flash")
            .expect("it did not join");
        // Written down as given, with nothing invented around it: a guessed
        // window would draw a meter measuring nothing.
        assert_eq!(
            (listed.context, listed.max_output, listed.price.clone()),
            (None, None, None)
        );

        // Twice is once, and a name nobody has is nobody's business.
        assert!(!p.remember_model("mimo", "mimo-v2.5-flash").unwrap());
        assert!(!p.remember_model("mimo", "").unwrap());
        assert!(!p.remember_model("nobody", "x").unwrap());

        // And it is on disk, so the next session still has it.
        let back = load(&dir).unwrap();
        assert!(
            back.get("mimo").unwrap().model("mimo-v2.5-flash").is_some(),
            "the typed model did not survive a reload"
        );
    }

    #[test]
    fn the_mimo_preset_follows_the_key_to_the_right_host() {
        let p = preset("mimo").unwrap();
        assert!(p
            .build("sk-anything")
            .base_url
            .contains("api.xiaomimimo.com"));
        assert!(p.build("tp-anything").base_url.contains("token-plan-cn"));
    }

    #[test]
    fn mimo_authenticates_with_the_api_key_header_not_bearer() {
        // Getting this wrong is a 401 that reads like a bad key.
        let c = preset("mimo").unwrap().build("sk-x");
        assert_eq!(c.auth.header, "api-key");
        assert!(c.auth.prefix.is_empty());
    }

    #[test]
    fn a_key_of_the_wrong_shape_is_noticed_but_a_keyless_provider_accepts_anything() {
        assert!(!preset("mimo").unwrap().key_looks_right("ghp_github_token"));
        assert!(preset("mimo").unwrap().key_looks_right("tp-abc"));
        assert!(preset("ollama").unwrap().key_looks_right(""));
    }

    #[test]
    fn endpoint_survives_a_base_url_with_or_without_a_slash() {
        let mut c = preset("mimo").unwrap().build("sk-x");
        assert_eq!(
            c.endpoint(),
            "https://api.xiaomimimo.com/v1/chat/completions"
        );
        c.base_url.push('/');
        assert_eq!(
            c.endpoint(),
            "https://api.xiaomimimo.com/v1/chat/completions"
        );
    }

    #[test]
    fn round_trips_through_the_file_and_first_added_becomes_default() {
        let h = home("rt");
        let mut p = load(&h).unwrap();
        p.add(preset("mimo").unwrap().build("sk-x")).unwrap();
        p.add(preset("ollama").unwrap().build("")).unwrap();
        let again = load(&h).unwrap();
        assert_eq!(again.default.as_deref(), Some("mimo"));
        assert_eq!(again.active().unwrap().name, "mimo");
        assert_eq!(again.list.len(), 2);
    }

    #[test]
    fn removing_the_default_promotes_another_rather_than_leaving_a_dangling_name() {
        let h = home("rm");
        let mut p = load(&h).unwrap();
        p.add(preset("mimo").unwrap().build("sk-x")).unwrap();
        p.add(preset("ollama").unwrap().build("")).unwrap();
        p.remove("mimo").unwrap();
        assert_eq!(p.default.as_deref(), Some("ollama"));
        assert!(p.active().is_some());
    }

    #[test]
    fn adding_the_same_name_twice_replaces_rather_than_duplicates() {
        let h = home("dup");
        let mut p = load(&h).unwrap();
        p.add(preset("mimo").unwrap().build("sk-x")).unwrap();
        p.add(preset("mimo").unwrap().build("tp-x")).unwrap();
        assert_eq!(p.list.len(), 1);
        assert!(p.list[0].base_url.contains("token-plan-cn"));
    }

    #[test]
    fn a_base_url_that_is_not_a_url_is_refused_at_the_door() {
        let h = home("bad");
        let mut p = load(&h).unwrap();
        let mut c = preset("mimo").unwrap().build("sk-x");
        c.base_url = "api.xiaomimimo.com".into();
        assert!(p.add(c).is_err());
    }

    /// `http://` is how a local Ollama is reached, and it is also how a key
    /// leaks. The host decides which.
    /// Every preset must be addable. A typo in a base URL or an auth header is
    /// a 401 that reads like a bad key, and the user pays for the confusion.
    #[test]
    fn every_preset_builds_something_the_door_accepts() {
        let h = home("presets");
        let mut p = load(&h).unwrap();
        for preset in PRESETS {
            let config = preset.build("sk-probe-000000000001");
            assert_eq!(config.name, preset.name);
            let local = is_loopback(&config.base_url);
            assert!(
                config.base_url.starts_with("https://") || local,
                "{} would send a key in the clear",
                preset.name
            );
            assert!(
                !local
                    || config.secret.is_none()
                    || config.name == "9router"
                    || config.name == "ollama",
                "{} runs on this machine and must not require a key",
                preset.name
            );
            assert!(
                p.add(config).is_ok(),
                "{} was refused at the door",
                preset.name
            );
        }
        assert_eq!(p.list.len(), PRESETS.len());
    }

    #[test]
    fn ninerouter_accepts_optional_api_key() {
        let p = preset("9router").unwrap();
        assert_eq!(p.build("").secret, None);
        assert_eq!(p.build("my-token").secret, Some("9router".into()));
    }

    #[test]
    fn google_authenticates_with_bearer_authorization_for_openai_endpoint() {
        let c = preset("google").unwrap().build("AIzaTestKey");
        assert_eq!(c.auth.header, "Authorization");
        assert_eq!(c.auth.prefix, "Bearer ");
        assert!(c.models.is_empty());
    }

    #[test]
    fn presets_can_be_looked_up_case_insensitively() {
        assert!(preset("9router").is_some());
        assert!(preset("9Router").is_some());
        assert!(preset("GOOGLE").is_some());
        assert!(preset("MiMo").is_some());
        assert!(preset("OpenAI").is_some());
    }

    /// OpenRouter attributes traffic by these, and a provider's own headers must
    /// never be able to displace the credential.
    #[test]
    fn extra_headers_ride_along_and_cannot_overwrite_auth() {
        let openrouter = preset("openrouter").unwrap().build("sk-or-x");
        assert_eq!(
            openrouter.headers.get("X-Title").map(String::as_str),
            Some("Rune")
        );
        assert!(openrouter.headers.contains_key("HTTP-Referer"));
        assert!(!openrouter.headers.contains_key("Authorization"));
    }

    #[test]
    fn plain_http_is_allowed_to_this_machine_and_nowhere_else() {
        let h = home("scheme");
        let mut p = load(&h).unwrap();
        let mut ok = preset("ollama").unwrap().build("");
        ok.base_url = "http://127.0.0.1:11434/v1".into();
        assert!(p.add(ok).is_ok());

        for elsewhere in [
            "http://cheap-proxy.example/v1",
            "http://127.0.0.1.evil.example/v1",
            "http://user@evil.example/v1",
            "http://[2001:db8::1]/v1",
        ] {
            let mut bad = preset("openai").unwrap().build("sk-x");
            bad.base_url = elsewhere.into();
            let e = p.add(bad).unwrap_err().to_string();
            assert!(
                e.contains("https") || e.contains("cannot contain credentials"),
                "{elsewhere} was accepted over plain http: {e}"
            );
        }
    }

    #[test]
    fn every_shape_of_loopback_is_recognised_and_nothing_else_is() {
        for yes in [
            "http://localhost/v1",
            "http://LocalHost:1/v1",
            "http://[::1]:8080/v1",
            "http://127.5.5.5/v1",
        ] {
            assert!(is_loopback(yes), "{yes}");
        }
        for no in [
            "http://example.com",
            "http://10.0.0.1",
            "http://localhost.evil.com",
            "not-a-url",
        ] {
            assert!(!is_loopback(no), "{no}");
        }
    }

    /// `providers.json` is the file a user copies into a bug report without
    /// thinking about it. Every preset is checked, not just one, and the header
    /// map is checked too — it is the newest place a credential could be put by
    /// someone who did not know why the split exists.
    #[test]
    fn the_file_never_contains_a_key_by_any_route() {
        let h = home("nokey");
        let mut p = load(&h).unwrap();
        const SECRET: &str = "sk-secret-must-not-land-here";
        for preset in PRESETS {
            let config = preset.build(SECRET);
            assert!(
                !config.headers.values().any(|v| v.contains(SECRET)),
                "{} put the key in a header",
                preset.name
            );
            p.add(config).unwrap();
        }
        let text = std::fs::read_to_string(h.join(crate::settings::configuration::FILE)).unwrap();
        assert!(!text.contains(SECRET), "a key reached providers.json");
    }
    #[test]
    fn reasoning_selection_is_separate_from_api_capability_and_effort_list() {
        let mut model = ModelInfo::named("fixture", true);
        model.reasoning_enabled = Some(false);
        assert_eq!(model.reasoning_support(), Some(true));
        assert_eq!(model.reasoning_choices("openrouter"), vec!["off", "on"]);
        model.efforts = vec!["vendor-level".into()];
        assert_eq!(
            model.reasoning_choices("openrouter"),
            vec!["off", "vendor-level"]
        );
        // A 9router model offers every level the router reads, whatever its
        // upstream's own list says (owner, 2026-10-15).
        let mut router = vec!["off".to_string()];
        router.extend(ROUTER_LEVELS.map(String::from));
        assert_eq!(model.reasoning_choices("9router"), router);
        model.thinking_required = true;
        assert_eq!(model.reasoning_choices("anthropic"), vec!["vendor-level"]);
        model.metadata = serde_json::json!({"capabilities":{"reasoning":false}});
        assert!(
            model.reasoning_choices("9router").is_empty(),
            "explicit false wins over stale manual metadata"
        );
    }

    #[test]
    fn reasoning_selected_subset_survives_refresh_without_reintroducing_options() {
        let mut configured = vec![ModelInfo {
            id: "fixture".into(),
            thinking: true,
            efforts: vec!["low".into(), "high".into()],
            reasoning_options: Some(vec!["off".into(), "high".into()]),
            reasoning_enabled: Some(true),
            ..Default::default()
        }];
        let fetched = vec![ModelInfo {
            id: "fixture".into(),
            thinking: true,
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            ..Default::default()
        }];
        merge_catalogue(&mut configured, &fetched);
        assert_eq!(
            configured[0].reasoning_options.as_ref().unwrap(),
            &vec!["off", "high"]
        );
        assert_eq!(configured[0].reasoning_enabled, Some(true));
    }

    /// Owner, 2026-10-15: every 9router model offers the router's own levels,
    /// none is mandatory unless the person made it so, and Off is always a
    /// choice — whichever door the model came in by: a row an earlier build
    /// stored (three levels, `thinkingCanDisable: false` read as mandatory), a
    /// fresh fetch, or Settings › Models › Alt+C.
    #[test]
    fn a_9router_model_is_settled_to_the_routers_levels_at_every_door() {
        let dir = home("router-thinking-doors");
        let mut providers = load(&dir).unwrap();
        providers.add(preset("9router").unwrap().build("")).unwrap();
        let levels = ROUTER_LEVELS.map(String::from).to_vec();
        let row = |id: &str| ModelInfo {
            id: id.into(),
            thinking: true,
            known: vec!["thinking".into(), "efforts".into()],
            metadata: serde_json::json!({"capabilities":{
                "reasoning": true, "thinkingFormat": "gemini-level",
                "thinkingCanDisable": false, "thinkingEffortSupported": false
            }}),
            ..Default::default()
        };

        // An earlier build's row, as it sits in rune.db.
        let mut stale = row("ag/gemini-3.8-flash-high");
        stale.efforts = ["low", "medium", "high"].map(String::from).to_vec();
        stale.thinking_required = true;
        stale.default_effort = Some("medium".into());
        providers
            .catalogue()
            .replace(
                "9router",
                &[(stale.id.clone(), serde_json::to_string(&stale).unwrap())],
            )
            .unwrap();
        let providers = load(&dir).unwrap();
        let loaded = providers.get("9router").unwrap().model(&stale.id).unwrap();
        assert_eq!(loaded.efforts, levels, "the router's levels, not three");
        assert!(
            !loaded.thinking_required,
            "Off is a request the router takes"
        );
        assert_eq!(
            loaded.default_effort, None,
            "no level the person did not set"
        );
        assert!(loaded.thinking_off_asks_least());

        // A fetch: the row carries no levels; the router's are its levels.
        let mut providers = providers;
        providers
            .cache_models("9router", vec![row("ag/gemini-3.8-flash-low")])
            .unwrap();
        let fetched = providers
            .get("9router")
            .unwrap()
            .model("ag/gemini-3.8-flash-low")
            .unwrap()
            .clone();
        assert_eq!(fetched.efforts, levels);
        assert!(!fetched.thinking_required);
        let stored = providers.catalogue().read().unwrap();
        assert!(
            stored["9router"]
                .iter()
                .any(|json| json.contains("ag/gemini-3.8-flash-low") && json.contains("xhigh")),
            "what is stored is what is settled"
        );

        // Alt+C: the person's options are the levels; their mandatory and
        // their default are kept.
        let mut chosen = fetched;
        chosen.reasoning_options = Some(["off", "low", "max"].map(String::from).to_vec());
        chosen.reasoning_enabled = Some(true);
        chosen.default_effort = Some("max".into());
        providers.update_model_metadata("9router", chosen).unwrap();
        let configured = providers
            .get("9router")
            .unwrap()
            .model("ag/gemini-3.8-flash-low")
            .unwrap();
        assert_eq!(configured.efforts, ["low", "max"]);
        assert_eq!(configured.default_effort.as_deref(), Some("max"));
        let mut mandatory = configured.clone();
        mandatory.reasoning_options = None;
        mandatory.thinking_required = true;
        providers
            .update_model_metadata("9router", mandatory)
            .unwrap();
        let reloaded = load(&dir).unwrap();
        let kept = reloaded
            .get("9router")
            .unwrap()
            .model("ag/gemini-3.8-flash-low")
            .unwrap();
        assert!(kept.thinking_required, "a mandatory the person set is kept");
        assert_eq!(kept.efforts, levels);
        assert_eq!(kept.default_effort.as_deref(), Some("max"));

        // What the person wrote in rune.toml for a 9router model is theirs.
        let written = home("router-thinking-written");
        let mut router = preset("9router").unwrap().build("");
        let mut hand = row("hand/written");
        hand.thinking_required = true;
        hand.efforts = vec!["low".into(), "high".into()];
        hand.default_effort = Some("high".into());
        router.models = vec![hand];
        crate::settings::configuration::Configuration::edit(
            &written,
            Some("providers"),
            Some(serde_json::json!({"list": [serde_json::to_value(&router).unwrap()]})),
            false,
            None,
        )
        .unwrap();
        let read = load(&written).unwrap();
        let hand = read.get("9router").unwrap().model("hand/written").unwrap();
        assert!(hand.thinking_required);
        assert_eq!(hand.efforts, ["low", "high"]);
        assert_eq!(hand.default_effort.as_deref(), Some("high"));

        // Another adapter's model is left as its catalogue says.
        let mut other = row("x");
        other.thinking_required = true;
        let mut openrouter = preset("openrouter").unwrap().build("");
        openrouter.models.push(other);
        openrouter.settle_thinking();
        assert!(openrouter.models[0].thinking_required);
        assert!(openrouter.models[0].efforts.is_empty());
    }

    /// Owner, 2026-10-15: a model deleted and added again came back as a bare
    /// id — no capabilities, Alt+C › Reasoning "Unsupported / unknown" and
    /// nothing to configure — until the catalogue was refreshed by hand.
    /// Deleting forgets what the person set; what the catalogue published
    /// stays (a refresh keeps hidden rows too), so adding it again is the
    /// catalogue's model at once.
    #[test]
    fn a_model_deleted_and_added_again_is_the_catalogues_model_at_once() {
        let dir = home("deleted-added-again");
        let mut providers = load(&dir).unwrap();
        providers.add(preset("9router").unwrap().build("")).unwrap();
        let fetched = ModelInfo {
            id: "ag/route".into(),
            thinking: true,
            listed: Some(true),
            fetched_at: Some(1),
            context: Some(1_000_000),
            known: vec!["thinking".into(), "efforts".into(), "context".into()],
            metadata: serde_json::json!({"capabilities":{
                "reasoning": true, "thinkingFormat": "gemini-level", "thinkingCanDisable": false
            }}),
            ..Default::default()
        };
        providers.cache_models("9router", vec![fetched]).unwrap();
        let mut configured = providers
            .get("9router")
            .unwrap()
            .model("ag/route")
            .unwrap()
            .clone();
        configured.context = Some(1234);
        configured.reasoning_options = Some(["off", "high"].map(String::from).to_vec());
        configured.reasoning_enabled = Some(true);
        providers
            .update_model_metadata("9router", configured)
            .unwrap();

        assert!(providers.forget_model("9router", "ag/route").unwrap());
        assert!(providers
            .get("9router")
            .unwrap()
            .model("ag/route")
            .is_none());
        assert!(providers.remember_model("9router", "ag/route").unwrap());

        let mut offered = vec!["off".to_string()];
        offered.extend(ROUTER_LEVELS.map(String::from));
        for providers in [&providers, &load(&dir).unwrap()] {
            let back = providers.get("9router").unwrap().model("ag/route").unwrap();
            assert_eq!(
                back.reasoning_choices("9router"),
                offered,
                "the catalogue's model"
            );
            assert_eq!(back.efforts, ROUTER_LEVELS);
            assert!(back.thinking_off_asks_least());
            assert_eq!(
                back.reasoning_options, None,
                "what the person set is forgotten"
            );
            assert_eq!(back.reasoning_enabled, None);
            assert_ne!(
                back.context,
                Some(1234),
                "the person's context is forgotten"
            );
            assert!(back.manual_fields.is_empty(), "{:?}", back.manual_fields);
        }

        // A model the person added by hand, never in the catalogue, is gone
        // whole when deleted: nothing of it was published.
        assert!(providers.remember_model("9router", "hand/only").unwrap());
        assert!(providers.forget_model("9router", "hand/only").unwrap());
        let stored = providers.catalogue().read().unwrap();
        assert!(!stored["9router"]
            .iter()
            .any(|row| row.contains("hand/only")));
        assert!(stored["9router"].iter().any(|row| row.contains("ag/route")));
    }
}
