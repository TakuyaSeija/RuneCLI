//! The sampling knobs, and the rules that keep them honest.
//!
//! These belong to a **session**, not to the install. Two conversations open at
//! once are usually two different jobs — one exploring, one editing — and a
//! global temperature makes the second one pay for the first. It is also what
//! the ecosystem already settled on elsewhere: a model chosen per conversation,
//! with no stored global default quietly deciding for a new one.
//!
//! # Validation happens here, once
//!
//! A value out of range is refused at the moment it is set, with the range in
//! the message, rather than being sent and rejected by the provider after the
//! request was already billed. Where a provider has a hard rule of its own —
//! MiMo forbids `temperature` and `top_p` while reasoning is on — the rule is
//! applied when the request is built, and [`ModelSettings::suppressed`] names
//! what was dropped so a settings panel can grey it out instead of lying.

use serde::{Deserialize, Serialize};

use super::providers::{Dialect, ModelInfo, ProviderConfig, Thinking};
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    /// Provider name, as listed in `providers.json`.
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// `max_completion_tokens`. Absent means the provider's own ceiling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Reasoning on or off, where the provider exposes the choice.
    pub thinking: bool,
    /// `low` / `medium` / `high`, for providers that take an effort level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

impl Default for ModelSettings {
    fn default() -> ModelSettings {
        ModelSettings {
            provider: String::new(),
            model: String::new(),
            temperature: None,
            top_p: None,
            max_output: None,
            frequency_penalty: None,
            presence_penalty: None,
            // On by default: a coding agent that reasons before it edits is the
            // reason this engine exists, and the CLI shows the reasoning rather
            // than hiding it.
            thinking: true,
            effort: None,
            stop: Vec::new(),
        }
    }
}

/// One editable knob, described well enough for a settings panel to draw it
/// without knowing anything about models.
///
/// The panel is generated from this list rather than hand-written, so a knob
/// added here appears in the UI with its real range and never drifts from it.
pub struct Knob {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: KnobKind,
    /// Why a user would touch it. One line.
    pub help: &'static str,
}

impl Knob {
    /// Published limits override the documented compatibility fallback used by the UI.
    pub fn bounds(&self, model: Option<&ModelInfo>) -> Option<(f64, f64, f64)> {
        let KnobKind::Float { min, max, step } = self.kind else {
            return None;
        };
        let published = model.and_then(|m| {
            m.metadata
                .pointer(&format!("/parameter_limits/{}", self.key))
        });
        let min = published
            .and_then(|v| v.get("minimum"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(min);
        let max = published
            .and_then(|v| v.get("maximum"))
            .and_then(serde_json::Value::as_f64)
            .or_else(|| {
                if self.key == "temperature" {
                    model
                        .and_then(|m| m.metadata.get("maxTemperature"))
                        .and_then(serde_json::Value::as_f64)
                } else {
                    None
                }
            })
            .unwrap_or(max);
        Some((min, max, step))
    }
}

pub enum KnobKind {
    /// Inclusive range, with the step a UI should nudge by.
    Float {
        min: f64,
        max: f64,
        step: f64,
    },
    /// Inclusive range in whole tokens; the ceiling comes from the model.
    Tokens,
    Bool,
    /// One of a fixed set.
    Choice(&'static [&'static str]),
    Text,
}

/// One knob that is not editable right now, and whether that can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppressed {
    pub key: &'static str,
    /// One line, addressed to the user. Where the state can be changed, it says
    /// how.
    pub reason: &'static str,
    /// True when no setting on this screen will ever make it available: the
    /// endpoint simply has no such control.
    pub permanent: bool,
}

pub const KNOBS: &[Knob] = &[
    Knob {
        key: "thinking",
        label: "Thinking",
        kind: KnobKind::Bool,
        help: "Let the model reason before answering. Slower, better at hard edits.",
    },
    Knob {
        key: "effort",
        label: "Effort",
        kind: KnobKind::Choice(&[]),
        help: "How much reasoning, where the provider takes a level instead of a switch.",
    },
    Knob {
        key: "temperature",
        label: "Temperature",
        // No universal bound-discovery endpoint; fallback Chat API range as of 2026-09-14.
        kind: KnobKind::Float {
            min: 0.0,
            max: 2.0,
            step: 0.05,
        },
        help: "Randomness. Lower is more literal; code usually wants low.",
    },
    Knob {
        key: "top_p",
        label: "Top P",
        kind: KnobKind::Float {
            min: 0.01,
            max: 1.0,
            step: 0.01,
        },
        help: "Nucleus sampling. Change this or temperature, rarely both.",
    },
    Knob {
        key: "max_output",
        label: "Max output",
        kind: KnobKind::Tokens,
        help: "Ceiling on one reply, reasoning included. Set it too low on a thinking model and \
               the reply comes back empty, not shortened.",
    },
    Knob {
        key: "frequency_penalty",
        label: "Frequency penalty",
        kind: KnobKind::Float {
            min: -2.0,
            max: 2.0,
            step: 0.1,
        },
        help: "Discourage repeating the same tokens.",
    },
    Knob {
        key: "presence_penalty",
        label: "Presence penalty",
        kind: KnobKind::Float {
            min: -2.0,
            max: 2.0,
            step: 0.1,
        },
        help: "Discourage repeating the same subjects.",
    },
    Knob {
        key: "stop",
        label: "Stop sequences",
        kind: KnobKind::Text,
        help: "Up to four strings that end the reply. Comma-separated.",
    },
];

fn parameter_supported(model: &ModelInfo, key: &str) -> bool {
    let aliases: &[&str] = match key {
        "max_output" => &["max_tokens", "max_completion_tokens"],
        "thinking" => &[
            "thinking",
            "reasoning",
            "reasoning_effort",
            "include_reasoning",
        ],
        "effort" => &["reasoning", "reasoning_effort", "thinking", "effort"],
        "stop" => &["stop", "stop_sequences"],
        _ => &[key],
    };
    aliases
        .iter()
        .any(|k| model.supports_parameter(k) != Some(false))
}

/// Intelligently calculate max output tokens to prevent exhausting context window budget.
///
/// Bounded between 8,192 and 128,000 tokens based on context window proportion (20%),
/// while strictly honoring the provider/model's maximum output capability.
pub fn smart_max_output(context: Option<u64>, provider_max: Option<u64>) -> u64 {
    const MIN_BOUND: u64 = 8_192;
    const MAX_BOUND: u64 = 128_000;
    const CONTEXT_RATIO_PERCENT: u64 = 20;

    let calculated = if let Some(ctx) = context.filter(|&c| c > 0) {
        let proportional = (ctx * CONTEXT_RATIO_PERCENT) / 100;
        proportional.clamp(MIN_BOUND, MAX_BOUND)
    } else {
        MIN_BOUND
    };

    if let Some(limit) = provider_max.filter(|&l| l > 0) {
        if limit < MIN_BOUND {
            // Respect models whose vendor ceiling is explicitly smaller than 8192 (e.g. 2048, 4096)
            limit
        } else {
            calculated.min(limit)
        }
    } else {
        calculated
    }
}

/// Adapters whose chat completion reference names only `max_tokens` for the
/// output ceiling (read 2026-10-22), so the reasoning-model default
/// `max_completion_tokens` is not sent to them.
const MAX_TOKENS_ONLY: &[&str] = &[
    "moonshot",
    "alibaba",
    "zai",
    "stepfun",
    "siliconflow",
    "novita",
    "together",
    "deepinfra",
    "inference",
    "inclusionai",
];

impl ModelSettings {
    /// The settings a new session starts with for `provider`.
    pub fn for_provider(p: &ProviderConfig) -> ModelSettings {
        let m = p.default_model();
        let mut settings = ModelSettings {
            provider: p.name.clone(),
            model: m.map(|m| m.id.clone()).unwrap_or_default(),
            thinking: m.is_some_and(|m| m.starts_thinking(p.api_kind())),
            ..Default::default()
        };
        settings.effort = m
            .filter(|_| settings.thinking)
            .and_then(|m| m.starting_effort(p.api_kind()));
        settings.normalize_for_model(p);
        settings
    }

    /// Apply model eligibility to retained/global preferences at a model boundary.
    pub fn normalize_for_model(&mut self, provider: &ProviderConfig) {
        let Some(model) = provider.model(&self.model) else {
            return;
        };
        if model.reasoning_support() == Some(false) {
            self.thinking = false;
        }
        if model.thinking_required && model.reasoning_support() != Some(false) {
            self.thinking = true;
        }
        if let Some(options) = &model.reasoning_options {
            if model.reasoning_support() == Some(false) || !options.iter().any(|v| v != "off") {
                self.thinking = false;
            } else if !options.iter().any(|v| v == "off") {
                self.thinking = true;
            }
            if self.effort.as_ref().is_some_and(|e| !options.contains(e)) {
                self.effort = options.iter().find(|v| *v != "off" && *v != "on").cloned();
            }
        }
        if let (Some(requested), Some(limit)) = (self.max_output, model.max_output) {
            self.max_output = Some(requested.min(limit));
        }
        if !self.thinking {
            self.effort = None;
        } else {
            // No level on a route that applies its own is "Route default".
            let valid = self.effort.is_none() && model.route_default_level(provider.api_kind())
                || self.effort.as_ref().is_some_and(|e| {
                    model.efforts.contains(e)
                        || (model.efforts.is_empty() && !e.trim().is_empty() && e != "off")
                        || (e.parse::<u64>().ok().filter(|n| *n > 0).is_some()
                            && (model.thinking_modes.iter().any(|m| m == "enabled")
                                || model
                                    .metadata
                                    .pointer("/reasoning/supports_max_tokens")
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(true)))
                });
            if !valid {
                self.effort = model.starting_effort(provider.api_kind());
            }
        }
        for why in self.suppressed(provider) {
            if why.permanent && !["thinking", "effort"].contains(&why.key) {
                let _ = self.clear(why.key);
            }
        }
        for key in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            let value = match key {
                "temperature" => self.temperature,
                "top_p" => self.top_p,
                "frequency_penalty" => self.frequency_penalty,
                _ => self.presence_penalty,
            };
            if let (Some(value), Some((min, max, _))) = (
                value,
                KNOBS
                    .iter()
                    .find(|k| k.key == key)
                    .and_then(|k| k.bounds(Some(model))),
            ) {
                if !value.is_finite() || value < min || value > max {
                    let _ = self.clear(key);
                }
            }
        }
    }

    /// Apply one knob by name, from text — the shape both the CLI flags and the
    /// settings panel produce. One parser, so `-set temperature=0.2` and the
    /// panel's arrow keys can never diverge in what they accept.
    ///
    /// # An empty value unsets
    ///
    /// Every knob that can be absent can be *made* absent again by setting it to
    /// nothing, and this is the only way back: a user who typed a temperature
    /// once had no way to return the decision to the provider, and a stop
    /// sequence added by mistake was permanent for the life of the session. The
    /// two that cannot be unset say so — a model is not optional, and a switch
    /// has no third position.
    ///
    /// `api` is the adapter of the provider `model` belongs to
    /// ([`ProviderConfig::api_kind`]): what a choice means on a model can be
    /// the adapter's to say (the route default is 9router's).
    pub fn set(
        &mut self,
        key: &str,
        value: &str,
        model: Option<&ModelInfo>,
        api: &str,
    ) -> Result<()> {
        let key = key
            .strip_prefix("sampling.")
            .or_else(|| key.strip_prefix("generation."))
            .unwrap_or(key);
        let v = value.trim();
        if v.is_empty() && self.clear(key)? {
            return Ok(());
        }
        if let Some(m) = model {
            if key == "thinking" {
                if m.reasoning_support() == Some(false) {
                    return Err(Error::invalid(format!(
                        "model `{}` does not support reasoning",
                        m.id
                    )));
                }
            } else if key != "model" && !parameter_supported(m, key) {
                return Err(Error::invalid(format!(
                    "parameter `{key}` is not supported by model `{}`",
                    m.id
                )));
            }
        }
        match key {
            "model" => {
                if v.is_empty() {
                    return Err(Error::invalid("model cannot be empty"));
                }
                self.model = v.to_string();
            }
            "thinking" => {
                let b = parse_bool(v)?;
                if let Some(m) = model {
                    if b && m.reasoning_support() == Some(false) {
                        return Err(Error::invalid(format!(
                            "model `{}` does not support reasoning",
                            m.id
                        )));
                    }
                    if !b
                        && (m.thinking_required
                            || m.reasoning_options
                                .as_ref()
                                .is_some_and(|v| !v.iter().any(|e| e == "off")))
                    {
                        return Err(Error::invalid(
                            "thinking is mandatory for this model and cannot be turned off",
                        ));
                    }
                }
                self.thinking = b;
                if !self.thinking {
                    self.effort = None;
                }
            }
            "effort" => {
                if v.eq_ignore_ascii_case(super::providers::ROUTE_DEFAULT) {
                    if !model.is_some_and(|m| m.route_default_level(api)) {
                        return Err(Error::invalid(
                            "this model has no route default; choose a level or off",
                        ));
                    }
                    self.effort = None;
                    self.thinking = true;
                } else if v.is_empty() || v.eq_ignore_ascii_case("off") {
                    if let Some(m) = model {
                        if m.thinking_required
                            || m.reasoning_options
                                .as_ref()
                                .is_some_and(|v| !v.iter().any(|e| e == "off"))
                        {
                            return Err(Error::invalid("effort cannot be turned off: reasoning is mandatory for this model"));
                        }
                    }
                    self.effort = None;
                    self.thinking = false;
                } else {
                    if let Some(m) = model {
                        if m.reasoning_support() == Some(false) {
                            return Err(Error::invalid(format!(
                                "model `{}` does not support reasoning",
                                m.id
                            )));
                        }
                        let budget = v.parse::<u64>().ok().filter(|n| *n > 0).is_some_and(|_| {
                            m.thinking_modes.iter().any(|mode| mode == "enabled")
                                || m.metadata
                                    .pointer("/reasoning/supports_max_tokens")
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(true)
                        });
                        let allowed = m
                            .reasoning_options
                            .clone()
                            .unwrap_or_else(|| m.efforts.clone());
                        if !budget && !allowed.iter().any(|e| e.eq_ignore_ascii_case(v)) {
                            if m.efforts.is_empty() {
                                return Err(Error::invalid(format!(
                                    "model `{}` does not support effort levels",
                                    m.id
                                )));
                            } else {
                                return Err(Error::invalid(format!(
                                    "effort `{v}` is not supported by model `{}`; available: {}",
                                    m.id,
                                    m.efforts.join(", ")
                                )));
                            }
                        }
                    }
                    if v.len() > 32 || v.chars().any(char::is_control) {
                        return Err(Error::invalid("effort must be 1–32 characters"));
                    }
                    self.effort = Some(v.to_string());
                    self.thinking = true;
                }
            }
            "temperature" => self.temperature = Some(parse_range(v, "temperature", model)?),
            "top_p" => self.top_p = Some(parse_range(v, "top_p", model)?),
            "frequency_penalty" => {
                self.frequency_penalty = Some(parse_range(v, "frequency_penalty", model)?)
            }
            "presence_penalty" => {
                self.presence_penalty = Some(parse_range(v, "presence_penalty", model)?)
            }
            "max_output" => {
                let n: u64 = v
                    .parse()
                    .map_err(|_| Error::invalid("max_output must be a whole number"))?;
                let ceiling = model.and_then(|m| m.max_output).unwrap_or(u64::MAX);
                if n == 0 || n > ceiling {
                    return Err(Error::invalid(format!(
                        "max_output must be between 1 and {ceiling}"
                    )));
                }
                self.max_output = Some(n);
            }
            "stop" => {
                let parts: Vec<String> = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.len() > 4 {
                    return Err(Error::invalid("at most four stop sequences"));
                }
                self.stop = parts;
            }
            other => return Err(Error::invalid(format!("unknown setting `{other}`"))),
        }
        Ok(())
    }

    pub fn apply_values(
        &mut self,
        values: &serde_json::Value,
        model: Option<&ModelInfo>,
        api: &str,
    ) -> Result<()> {
        if values.is_null() {
            return Ok(());
        }
        for (key, value) in values
            .as_object()
            .ok_or_else(|| Error::invalid("generation must be a table"))?
        {
            let knob = KNOBS
                .iter()
                .find(|k| k.key == key)
                .ok_or_else(|| Error::invalid(format!("unknown generation setting {key}")))?;
            let valid = match knob.kind {
                KnobKind::Bool => value.is_boolean(),
                KnobKind::Choice(_) => value.is_string(),
                KnobKind::Float { .. } => value.is_number(),
                KnobKind::Tokens => value.is_u64(),
                KnobKind::Text => value.as_array().is_some_and(|a| {
                    a.len() <= 4
                        && a.iter()
                            .all(|v| v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 1024))
                }),
            };
            if !valid {
                return Err(Error::invalid(format!(
                    "generation.{key} has an invalid type or value"
                )));
            }
            if key == "stop" {
                self.stop = value
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect();
            } else {
                self.set(
                    key,
                    &value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string()),
                    model,
                    api,
                )?;
            }
        }
        Ok(())
    }

    /// Hand one knob back to the provider. `Ok(false)` means this knob has no
    /// unset state, and the caller should read the value as it normally would.
    fn clear(&mut self, key: &str) -> Result<bool> {
        let key = key
            .strip_prefix("sampling.")
            .or_else(|| key.strip_prefix("generation."))
            .unwrap_or(key);
        match key {
            "temperature" => self.temperature = None,
            "top_p" => self.top_p = None,
            "max_output" => self.max_output = None,
            "frequency_penalty" => self.frequency_penalty = None,
            "presence_penalty" => self.presence_penalty = None,
            "effort" => self.effort = None,
            "stop" => self.stop.clear(),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Whether a knob can be handed back to the provider at all.
    ///
    /// Read by the settings screen so that the row it will not clear does not
    /// offer to. One list, so the offer and the outcome cannot disagree.
    pub fn clearable(key: &str) -> bool {
        ModelSettings::default().clear(key).unwrap_or(false)
    }

    /// Knobs this provider will ignore or reject for the current state, and why.
    ///
    /// Not cosmetic: MiMo **rejects** a request that carries `temperature` while
    /// reasoning is on, so the request builder drops them — and a panel that
    /// still showed them as active would be showing a value that is not in force.
    ///
    /// The reasoning switch is judged against the **model**, not only the
    /// endpoint. A provider that speaks the `thinking` dialect can still serve a
    /// model that does not reason, and offering the switch there is offering
    /// something that does nothing.
    ///
    /// # Two kinds of unavailable, and why a frontend must be told which
    ///
    /// `permanent` separates "this endpoint has no such control" from "not while
    /// another setting is where it is". They deserve different treatment and the
    /// difference is not guessable from the reason text:
    ///
    ///   * MiMo has **no** effort level. Its API takes `thinking: {type}` and
    ///     nothing else — verified against the published reference. Nothing the
    ///     user does here will ever make that knob work, so a panel should stop
    ///     offering it rather than refuse it once per keypress.
    ///   * MiMo *does* take `temperature`, just not while reasoning is on. That
    ///     one is a door with a key, and the reason says where the key is.
    pub fn suppressed(&self, p: &ProviderConfig) -> Vec<Suppressed> {
        let mut out = Vec::new();
        let m_opt = p.model(&self.model);
        let model_thinks = m_opt
            .and_then(ModelInfo::reasoning_support)
            .unwrap_or(p.thinking != Thinking::None);
        let thinking_req = m_opt.map(|m| m.thinking_required).unwrap_or(false);

        if thinking_req {
            out.push(Suppressed {
                key: "thinking",
                reason: "reasoning is required for this model",
                permanent: false,
            });
        }

        if p.dialect == super::providers::Dialect::Anthropic {
            for key in ["frequency_penalty", "presence_penalty"] {
                out.push(Suppressed {
                    key,
                    reason: "this endpoint has no such parameter",
                    permanent: true,
                });
            }
        }

        if let Some(m) = m_opt {
            if !m.supported_parameters.is_empty()
                || m.known.iter().any(|k| k == "supported_parameters")
            {
                for key in [
                    "temperature",
                    "top_p",
                    "frequency_penalty",
                    "presence_penalty",
                    "stop",
                    "max_output",
                ] {
                    if !parameter_supported(m, key) {
                        out.push(Suppressed {
                            key,
                            reason: "this model does not support this parameter",
                            permanent: true,
                        });
                    }
                }
            }
        }

        if (self.thinking || thinking_req)
            && model_thinks
            && (p.thinking == Thinking::Object
                || p.dialect == super::providers::Dialect::Anthropic
                || m_opt.is_some_and(|m| {
                    m.metadata
                        .pointer("/reasoning/fixes_temperature")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                }))
        {
            for key in ["temperature", "top_p"] {
                if !out.iter().any(|s| s.key == key) {
                    out.push(Suppressed {
                        key,
                        reason: "fixed by the provider while thinking is on — turn Thinking off to set it",
                        permanent: false,
                    });
                }
            }
        }
        if !model_thinks {
            out.push(Suppressed {
                key: "thinking",
                reason: if p.thinking == Thinking::None {
                    "this provider has no reasoning switch"
                } else {
                    "this model does not reason"
                },
                permanent: true,
            });
            out.push(Suppressed {
                key: "effort",
                reason: if p.thinking == Thinking::None {
                    "this provider has no effort level"
                } else {
                    "this model does not reason"
                },
                permanent: true,
            });
        } else {
            let has_effort = match p.thinking {
                Thinking::None | Thinking::Object => false,
                Thinking::Effort => {
                    if let Some(m) = m_opt {
                        if m.reasoning_support() == Some(false) {
                            false
                        } else if !m.efforts.is_empty()
                            || m.reasoning_options
                                .as_ref()
                                .is_some_and(|v| v.iter().any(|e| e != "off" && e != "on"))
                            || m.thinking_modes.iter().any(|mode| mode == "enabled")
                            || m.metadata
                                .pointer("/reasoning/supports_max_tokens")
                                .and_then(serde_json::Value::as_bool)
                                == Some(true)
                        {
                            true
                        } else {
                            if m.known.iter().any(|k| k == "efforts") && m.efforts.is_empty() {
                                false
                            } else {
                                m.thinking || m.reasoning_support().is_none()
                            }
                        }
                    } else {
                        // When m_opt is None (provider added before key / catalogue fetch), Effort provider supports effort!
                        true
                    }
                }
            };
            if !has_effort {
                out.push(Suppressed {
                    key: "effort",
                    reason: "this model has no effort levels",
                    permanent: true,
                });
            }
        }
        out
    }

    pub(crate) fn is_suppressed(&self, p: &ProviderConfig, key: &str) -> bool {
        self.suppressed(p).iter().any(|s| s.key == key)
    }

    /// The provider-specific half of a request body: everything except the
    /// messages and the tools.
    pub fn body(&self, p: &ProviderConfig) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::{json, Value};
        let mut out = serde_json::Map::new();
        out.insert("model".into(), Value::String(self.model.clone()));
        let mut put = |k: &str, v: Option<f64>| {
            if let Some(v) = v {
                if !self.is_suppressed(p, k) {
                    out.insert(k.into(), json!(v));
                }
            }
        };
        put("temperature", self.temperature);
        put("top_p", self.top_p);
        put("frequency_penalty", self.frequency_penalty);
        put("presence_penalty", self.presence_penalty);
        let m = p.model(&self.model);
        let ceiling = m.and_then(|m| m.max_output);
        let context = m.and_then(|m| m.context);
        let auto_smart = smart_max_output(context, ceiling);
        let n = self
            .max_output
            .unwrap_or_else(|| ceiling.map_or(auto_smart, |limit| auto_smart.min(limit)));
        let supports = |key| m.and_then(|m| m.supports_parameter(key));
        let key = if supports("max_completion_tokens") == Some(true) {
            Some("max_completion_tokens")
        } else if supports("max_tokens") == Some(true) {
            Some("max_tokens")
        } else if supports("max_completion_tokens") == Some(false)
            && supports("max_tokens") == Some(false)
        {
            None
        } else if p.dialect == Dialect::OpenAi
            && !MAX_TOKENS_ONLY.contains(&p.api_kind())
            && (m.is_some_and(|m| m.thinking_required || m.thinking)
                || p.thinking != Thinking::None)
        {
            Some("max_completion_tokens")
        } else {
            Some("max_tokens")
        };
        if let Some(key) = key {
            out.insert(key.into(), json!(n));
        }
        if !self.stop.is_empty() && !self.is_suppressed(p, "stop") {
            out.insert("stop".into(), json!(self.stop));
        }
        // Thinking these APIs would otherwise write into the answer as
        // `<think>…</think>` is asked for in its own field (MiniMax OpenAI API
        // and Novita chat completion references, read 2026-10-22).
        match p.api_kind() {
            "minimax" => {
                out.insert("reasoning_split".into(), json!(true));
            }
            "novita" => {
                out.insert("separate_reasoning".into(), json!(true));
            }
            _ => {}
        }
        let model_thinks = m
            .and_then(ModelInfo::reasoning_support)
            .unwrap_or(p.thinking != Thinking::None);
        let required = m.is_some_and(|m| m.thinking_required);
        if !model_thinks {
            return out;
        }
        let enabled = self.thinking || required;
        let effort = self
            .effort
            .as_deref()
            .filter(|e| m.is_none_or(|m| m.efforts.is_empty() || m.efforts.iter().any(|v| v == *e)))
            .or_else(|| {
                m.and_then(|m| {
                    m.default_effort
                        .as_deref()
                        .filter(|e| m.efforts.is_empty() || m.efforts.iter().any(|x| x == *e))
                })
            })
            .or_else(|| {
                m.filter(|m| !m.route_default_level(p.api_kind()))
                    .and_then(|m| m.efforts.first().map(|s| s.as_str()))
            });
        if matches!(p.api_kind(), "alibaba" | "siliconflow" | "novita") {
            // Thinking is one switch, `enable_thinking`, at the top level of
            // the JSON (Model Studio deep-thinking guide, SiliconFlow and
            // Novita chat completion references, read 2026-10-22); none of
            // them takes a level. Off only where the model is known to stop,
            // as on `Effort` (owner, 2026-10-22).
            if enabled {
                out.insert("enable_thinking".into(), json!(true));
            } else if m.is_some_and(ModelInfo::turns_off) {
                out.insert("enable_thinking".into(), json!(false));
            }
            return out;
        }
        if p.api_kind() == "google" {
            // Wire encoding only; accepted levels come from model metadata.
            // Official OpenAI compatibility docs, checked 2026-10-03: omitting
            // effort uses vendor default; "none" disables optional thinking.
            if !enabled && !required {
                out.insert("reasoning_effort".into(), json!("none"));
            } else if enabled {
                if let Some(level) = effort {
                    out.insert("extra_body".into(), json!({"google": {"thinking_config": {"thinking_level": level.to_ascii_uppercase(), "include_thoughts": true}}}));
                }
            }
            return out;
        }
        if p.api_kind() == "9router" {
            // 9router translates thinking for every upstream from one field,
            // `reasoning_effort` (measured 2026-10-14, 9router 0.5.99): a level
            // turns thinking on at that level, `none` off. A `reasoning`
            // object beside it is not sent: one without `summary` makes the
            // router ask Claude routes to redact the thinking text, and its
            // `enabled` flag alone was never read — "on" reached no upstream.
            //
            // No level chosen sends none ("Route default"), on every model
            // the router serves: the route applies its own, and a level filled
            // in for the person turned `…-high` routes down to it (owner,
            // 2026-10-15). Off is `none` everywhere; a route that cannot stop
            // thinking reads it as its least (`thinking_off_asks_least`).
            let level = if !enabled {
                Some("none")
            } else {
                self.effort
                    .as_deref()
                    .filter(|e| m.is_none_or(|m| m.efforts.iter().any(|v| v == *e)))
            };
            if let Some(level) = level {
                out.insert("reasoning_effort".into(), json!(level));
            }
            return out;
        }
        if p.thinking == Thinking::None {
            return out;
        }
        if p.thinking == Thinking::Effort
            && m.is_some_and(|m| {
                m.supports_parameter("thinking") == Some(true)
                    && m.supports_parameter("reasoning") != Some(true)
                    && effort.is_none()
            })
        {
            out.insert(
                "thinking".into(),
                json!({"type": if enabled { "enabled" } else { "disabled" }}),
            );
            return out;
        }
        match p.thinking {
            Thinking::Object => {
                // MiMo sends Off to every model, as before; an adapter added
                // later sends it only where the model is known to stop, as
                // on `Effort` (owner, 2026-10-22): a model that must think
                // refuses `disabled`.
                if enabled || p.api_kind() == "mimo" || m.is_some_and(ModelInfo::turns_off) {
                    out.insert(
                        "thinking".into(),
                        json!({"type": if enabled { "enabled" } else { "disabled" }}),
                    );
                }
                if enabled {
                    if let Some(e) = effort {
                        out.insert("reasoning_effort".into(), json!(e));
                    }
                }
            }
            Thinking::Effort => {
                if p.api_kind() == "openrouter"
                    || m.is_some_and(|m| m.supports_parameter("reasoning") == Some(true))
                {
                    let mut reasoning = json!({"enabled": enabled});
                    let budget = self
                        .effort
                        .as_ref()
                        .and_then(|e| e.parse::<u64>().ok())
                        .filter(|_| {
                            m.is_some_and(|m| {
                                m.metadata
                                    .pointer("/reasoning/supports_max_tokens")
                                    .and_then(Value::as_bool)
                                    == Some(true)
                            })
                        });
                    if enabled {
                        if let Some(budget) = budget {
                            reasoning["max_tokens"] = json!(budget);
                        } else if let Some(e) = effort {
                            reasoning["effort"] = json!(e);
                        }
                    }
                    if m.is_none_or(|m| m.supports_parameter("reasoning") != Some(false)) {
                        out.insert("reasoning".into(), reasoning);
                    }
                } else if enabled {
                    if let Some(e) = effort {
                        out.insert("reasoning_effort".into(), json!(e));
                    }
                } else if m.is_some_and(ModelInfo::turns_off) {
                    out.insert("reasoning_effort".into(), json!("none"));
                }
            }
            Thinking::None => {}
        }
        out
    }
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(Error::invalid("expected on or off")),
    }
}

fn parse_range(v: &str, name: &str, model: Option<&ModelInfo>) -> Result<f64> {
    let (min, max, _) = KNOBS
        .iter()
        .find(|k| k.key == name)
        .and_then(|k| k.bounds(model))
        .expect("numeric metadata");
    let n: f64 = v
        .parse()
        .map_err(|_| Error::invalid(format!("{name} must be a number")))?;
    if !n.is_finite() || n < min || n > max {
        return Err(Error::invalid(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::super::providers::{preset, Price, ROUTER_LEVELS, ROUTE_DEFAULT};
    use super::*;

    fn mimo() -> ProviderConfig {
        let mut p = preset("mimo").unwrap().build("sk-x");
        p.models = vec![
            ModelInfo {
                id: "mimo-v2.5-pro".into(),
                max_output: Some(131_072),
                context: Some(1_050_000),
                thinking: true,
                price: Some(Price {
                    input: 0.435,
                    cached: 0.0036,
                    output: 0.87,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ModelInfo {
                id: "mimo-v2.5".into(),
                max_output: Some(32_768),
                context: Some(262_144),
                thinking: true,
                ..Default::default()
            },
        ];
        p
    }

    /// Each added adapter's thinking reaches its API in the field that API
    /// documents, through the same Thinking kinds the older presets use.
    #[test]
    fn added_adapters_send_the_thinking_and_output_fields_their_apis_document() {
        let body = |api: &str, model: ModelInfo, thinking: bool, effort: Option<&str>| {
            let mut cfg = preset(api).unwrap().build("k");
            if cfg.base_url.is_empty() {
                cfg.base_url = "https://account.example/v1".into();
            }
            let id = model.id.clone();
            cfg.models = vec![model];
            cfg.settle_thinking();
            let mut s = ModelSettings::for_provider(&cfg);
            s.model = id;
            s.thinking = thinking;
            s.effort = effort.map(String::from);
            s.body(&cfg)
        };
        // Off is sent only to a model known to stop (Alt+C › Reasoning offers
        // Off, or its catalogue says so); one that must think refuses it.
        let stops = || ModelInfo {
            reasoning_options: Some(vec!["off".into(), "on".into()]),
            ..ModelInfo::named("m", true)
        };
        // One switch, no level: Model Studio, SiliconFlow, Novita.
        for api in ["alibaba", "siliconflow", "novita"] {
            let unknown = body(api, ModelInfo::named("m", true), false, None);
            assert!(unknown.get("enable_thinking").is_none(), "{api}");
            let off = body(api, stops(), false, None);
            assert_eq!(off["enable_thinking"], false, "{api}");
            let on = body(api, ModelInfo::named("m", true), true, Some("high"));
            assert_eq!(on["enable_thinking"], true, "{api}");
            assert!(on.get("reasoning_effort").is_none(), "{api}");
            assert!(on.contains_key("max_tokens"), "{api}");
            assert!(!on.contains_key("max_completion_tokens"), "{api}");
        }
        // Thinking asked apart from the answer, on every request.
        let novita = body("novita", ModelInfo::named("m", false), false, None);
        assert_eq!(novita["separate_reasoning"], true);
        let minimax = body("minimax", ModelInfo::named("m", false), false, None);
        assert_eq!(minimax["reasoning_split"], true);
        // Z.ai, TokenHub and the Ant Ling API: a thinking object, its level
        // only when chosen.
        for api in ["zai", "tencent", "inclusionai"] {
            let open = body(api, ModelInfo::named("m", true), true, None);
            assert_eq!(
                open["thinking"],
                serde_json::json!({"type":"enabled"}),
                "{api}"
            );
            assert!(open.get("reasoning_effort").is_none(), "{api}");
            let chosen = body(api, ModelInfo::named("m", true), true, Some("low"));
            assert_eq!(chosen["reasoning_effort"], "low", "{api}");
            let unknown = body(api, ModelInfo::named("m", true), false, None);
            assert!(unknown.get("thinking").is_none(), "{api}");
            let off = body(api, stops(), false, None);
            assert_eq!(
                off["thinking"],
                serde_json::json!({"type":"disabled"}),
                "{api}"
            );
        }
        // MiMo, the older Object adapter, keeps sending Off as before.
        let mimo = body("mimo", ModelInfo::named("m", true), false, None);
        assert_eq!(mimo["thinking"], serde_json::json!({"type":"disabled"}));
        // On Effort, an Off the person offered in Alt+C now reaches the API.
        let fireworks = body("fireworks", stops(), false, None);
        assert_eq!(fireworks["reasoning_effort"], "none");
        let fireworks = body("fireworks", ModelInfo::named("m", true), false, None);
        assert!(fireworks.get("reasoning_effort").is_none());
        // A table adapter on Effort: no level chosen sends none.
        let fireworks = body("fireworks", ModelInfo::named("m", true), true, None);
        assert!(fireworks.get("reasoning_effort").is_none());
        assert!(fireworks.contains_key("max_completion_tokens"));
        let fireworks = body(
            "fireworks",
            ModelInfo::named("m", true),
            true,
            Some("xhigh"),
        );
        assert_eq!(fireworks["reasoning_effort"], "xhigh");
        // A catalogue's own levels keep the older rule: its least to start.
        let listed = ModelInfo {
            efforts: vec!["low".into(), "high".into()],
            known: vec!["thinking".into(), "efforts".into()],
            ..ModelInfo::named("m", true)
        };
        let ling = body("inclusionai", ModelInfo::named("m", true), true, None);
        assert!(ling.contains_key("max_tokens"));
        let inference = body("inference", listed, true, None);
        assert_eq!(inference["reasoning_effort"], "low");
        assert!(inference.contains_key("max_tokens"));
    }

    #[test]
    fn a_new_session_gets_the_providers_first_model() {
        let s = ModelSettings::for_provider(&mimo());
        assert_eq!(s.model, "mimo-v2.5-pro");
        assert!(s.thinking);
    }

    #[test]
    fn values_outside_the_range_are_refused_with_the_range_in_the_message() {
        let mut s = ModelSettings::for_provider(&mimo());
        let e = s
            .set("temperature", "2.5", None, "")
            .unwrap_err()
            .to_string();
        assert!(e.contains("between 0 and 2"), "{e}");
        assert!(s.set("temperature", "0.2", None, "").is_ok());
        assert_eq!(s.temperature, Some(0.2));
    }

    #[test]
    fn max_output_is_clamped_by_the_model_not_by_a_guess() {
        let p = mimo();
        let mut s = ModelSettings::for_provider(&p);
        let small = p.model("mimo-v2.5").unwrap();
        assert!(s
            .set("max_output", "100000", Some(small), p.api_kind())
            .is_err());
        assert!(s
            .set(
                "max_output",
                "100000",
                p.model("mimo-v2.5-pro"),
                p.api_kind()
            )
            .is_ok());
    }

    #[test]
    fn thinking_suppresses_temperature_for_mimo_and_the_body_leaves_it_out() {
        let p = mimo();
        let mut s = ModelSettings::for_provider(&p);
        s.set("temperature", "0.3", None, "").unwrap();
        assert!(s.suppressed(&p).iter().any(|x| x.key == "temperature"));
        let body = s.body(&p);
        assert!(
            !body.contains_key("temperature"),
            "MiMo rejects the request if this is sent"
        );
        assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));

        s.thinking = false;
        let body = s.body(&p);
        assert_eq!(body["temperature"], serde_json::json!(0.3));
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
    }

    #[test]
    fn an_effort_provider_gets_effort_and_a_plain_one_gets_neither() {
        let mut openai = preset("openai").unwrap().build("sk-x");
        openai.models.push(ModelInfo {
            id: "o3-mini".into(),
            thinking: true,
            efforts: vec!["medium".into()],
            ..Default::default()
        });
        let mut s = ModelSettings::for_provider(&openai);
        s.model = "o3-mini".into();
        s.thinking = true;
        s.effort = Some("medium".into());
        assert_eq!(
            s.body(&openai)["reasoning_effort"],
            serde_json::json!("medium")
        );
        let ollama = preset("ollama").unwrap().build("");
        let s = ModelSettings::for_provider(&ollama);
        let body = s.body(&ollama);
        assert!(!body.contains_key("reasoning_effort"));
        assert!(!body.contains_key("thinking"));
    }

    #[test]
    fn a_model_that_does_not_reason_has_no_reasoning_switch_to_offer() {
        let mut p = mimo();
        // Same endpoint, same dialect, a model that simply does not reason.
        p.models.push(super::super::providers::ModelInfo {
            id: "mimo-plain".into(),
            max_output: Some(4096),
            context: Some(32768),
            thinking: false,
            known: vec!["thinking".into()],
            ..Default::default()
        });
        let mut s = ModelSettings::for_provider(&p);
        s.model = "mimo-plain".into();
        let why = s.suppressed(&p);
        assert!(why.iter().any(|x| x.key == "thinking"), "{why:?}");
        // …and the sampling knobs come back, because nothing is reasoning.
        assert!(!why.iter().any(|x| x.key == "temperature"), "{why:?}");
        s.set("temperature", "0.3", None, "").unwrap();
        assert_eq!(s.body(&p)["temperature"], serde_json::json!(0.3));
    }

    /// The distinction a settings panel needs in order to stop offering a knob
    /// that can never work, while still offering one that is only closed for now.
    #[test]
    fn a_missing_control_is_permanent_and_a_locked_one_says_how_to_unlock_it() {
        let p = mimo();
        let s = ModelSettings::for_provider(&p);
        let effort = s
            .suppressed(&p)
            .into_iter()
            .find(|x| x.key == "effort")
            .unwrap();
        assert!(
            effort.permanent,
            "MiMo takes `thinking: {{type}}` and nothing else"
        );

        let temp = s
            .suppressed(&p)
            .into_iter()
            .find(|x| x.key == "temperature")
            .unwrap();
        assert!(!temp.permanent);
        assert!(temp.reason.contains("turn Thinking off"), "{}", temp.reason);
    }

    #[test]
    fn stop_sequences_are_capped_at_four() {
        let mut s = ModelSettings::for_provider(&mimo());
        assert!(s.set("stop", "a,b,c,d,e", None, "").is_err());
        s.set("stop", "END, DONE", None, "").unwrap();
        assert_eq!(s.stop, vec!["END", "DONE"]);
    }

    #[test]
    fn an_unknown_setting_is_named_rather_than_ignored() {
        let mut s = ModelSettings::for_provider(&mimo());
        let e = s
            .set("temprature", "0.2", None, "")
            .unwrap_err()
            .to_string();
        assert!(e.contains("temprature"));
    }

    #[test]
    fn knobs_accept_values_only_when_the_model_supports_them() {
        let p = mimo();
        let mut s = ModelSettings::for_provider(&p);
        for knob in KNOBS {
            let sample = match knob.kind {
                KnobKind::Float { min, .. } => format!("{min}"),
                KnobKind::Tokens => "1024".to_string(),
                KnobKind::Bool => "on".to_string(),
                KnobKind::Choice(opts) => opts.first().copied().unwrap_or("high").to_string(),
                KnobKind::Text => "END".to_string(),
            };
            let unsupported = s
                .suppressed(&p)
                .iter()
                .any(|why| why.key == knob.key && why.permanent);
            assert_eq!(
                s.set(knob.key, &sample, p.model("mimo-v2.5-pro"), p.api_kind())
                    .is_ok(),
                !unsupported,
                "{}",
                knob.key
            );
        }
    }

    #[test]
    fn settings_survive_the_database_round_trip() {
        let mut s = ModelSettings::for_provider(&mimo());
        s.set("temperature", "0.4", None, "").unwrap();
        s.set("stop", "X", None, "").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<ModelSettings>(&json).unwrap(), s);
    }

    #[test]
    fn body_emits_one_supported_output_limit() {
        let p = mimo();
        let mut s = ModelSettings::for_provider(&p);
        s.max_output = Some(32_768);
        let b = s.body(&p);
        assert_eq!(b["max_completion_tokens"], serde_json::json!(32_768));
        assert!(!b.contains_key("max_tokens"));
    }

    #[test]
    fn verified_efforts_reconcile_a_legacy_empty_setting_without_overwriting_a_choice() {
        let mut provider = preset("openrouter").unwrap().build("");
        provider.models.push(ModelInfo {
            id: "verified-model".into(),
            thinking: true,
            efforts: vec!["low".into(), "high".into()],
            default_effort: Some("high".into()),
            ..Default::default()
        });
        let mut current = ModelSettings {
            provider: provider.name.clone(),
            model: "verified-model".into(),
            thinking: true,
            effort: None,
            ..Default::default()
        };
        current.normalize_for_model(&provider);
        assert_eq!(
            current.effort.as_deref(),
            Some("high"),
            "a legacy session with no effort must adopt the verified model default"
        );

        current.effort = Some("low".into());
        current.normalize_for_model(&provider);
        assert_eq!(
            current.effort.as_deref(),
            Some("low"),
            "a valid user-selected effort must survive capability reconciliation"
        );

        current.thinking = false;
        current.normalize_for_model(&provider);
        assert!(
            current.effort.is_none(),
            "turning reasoning off must still clear the effort"
        );
    }

    /// A 9router catalogue row as its /v1/models publishes it (0.5.99).
    fn router_row(id: &str, format: Option<&str>, can_disable: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            thinking: true,
            known: vec!["thinking".into(), "efforts".into()],
            metadata: serde_json::json!({"capabilities":{
                "reasoning": true, "thinkingFormat": format,
                "thinkingCanDisable": can_disable, "thinkingEffortSupported": false
            }}),
            ..Default::default()
        }
    }

    fn router_with(models: Vec<ModelInfo>) -> ProviderConfig {
        let mut router = preset("9router").unwrap().build("");
        router.models = models;
        router.settle_thinking();
        router
    }

    /// Owner, 2026-10-15: every 9router model offers the levels the router
    /// reads — not three, not the upstream's own list — with Off always among
    /// them, a thinking format of the row's own or none.
    #[test]
    fn every_9router_model_offers_the_routers_levels_and_off() {
        let router = router_with(vec![
            router_row("ag/gemini-3.8-flash-high", Some("gemini-level"), false),
            router_row("cc/claude-sonnet-5-5", Some("claude-adaptive"), true),
            router_row("cl/inclusionai/ling-3.0-flash", None, true),
            ModelInfo::named("hand-added", true),
        ]);
        let mut offered = vec!["off".to_string()];
        offered.extend(ROUTER_LEVELS.map(String::from));
        for m in &router.models {
            assert_eq!(m.efforts, ROUTER_LEVELS, "{}", m.id);
            assert_eq!(m.reasoning_choices("9router"), offered, "{}", m.id);
            assert!(m.route_default_level("9router"), "{}", m.id);
            assert!(!m.thinking_required, "{}", m.id);
            assert_eq!(m.default_effort, None, "{}", m.id);
        }
        // What Off does is the router's to publish, never guessed.
        let high = router.model("ag/gemini-3.8-flash-high").unwrap();
        assert!(high.thinking_off_asks_least());
        assert!(!router
            .model("cc/claude-sonnet-5-5")
            .unwrap()
            .thinking_off_asks_least());
        let mut partial = router_row("ag/claude", Some("claude-adaptive"), true);
        assert!(!partial.thinking_off_between_tools_only());
        partial.metadata["capabilities"]["thinkingOffType"] = serde_json::json!("between_tools");
        assert!(partial.thinking_off_between_tools_only());

        // A level the router turns into a request its upstream refuses is not
        // offered: `minimal` on a budget-type Claude route is a 512-token
        // budget Anthropic's API refuses (HTTP 400, live 2026-10-15) — unless
        // the route publishes a range the router holds the budget to.
        let budget = router_with(vec![router_row(
            "cc/claude-haiku-4-5",
            Some("claude-budget"),
            true,
        )]);
        let model = &budget.models[0];
        assert!(
            !model.efforts.iter().any(|e| e == "minimal"),
            "{:?}",
            model.efforts
        );
        assert_eq!(model.efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert!(!model
            .reasoning_choices("9router")
            .iter()
            .any(|c| c == "minimal"));
        let mut ranged = router_row("cl/google/gemini-2.5-flash", Some("claude-budget"), true);
        ranged.metadata["capabilities"]["thinkingRange"] =
            serde_json::json!({"min": 1024, "max": 32000});
        let ranged = router_with(vec![ranged]);
        assert_eq!(ranged.models[0].efforts, ROUTER_LEVELS);

        // The person's options are the levels a session offers.
        let mut configured = router_row("route/configured", Some("claude-budget"), true);
        configured.reasoning_options =
            Some(["off", "low", "high", "max"].map(String::from).to_vec());
        let router = router_with(vec![configured]);
        assert_eq!(router.models[0].efforts, ["low", "high", "max"]);

        // Another adapter's model is not the router's: no route default, its
        // own levels.
        let plain = ModelInfo {
            id: "plain".into(),
            thinking: true,
            efforts: vec!["low".into(), "high".into()],
            ..Default::default()
        };
        assert!(!plain.route_default_level("openrouter"));
        assert_eq!(
            plain.reasoning_choices("openrouter"),
            ["off", "low", "high"]
        );
        assert_eq!(plain.starting_effort("openrouter").as_deref(), Some("low"));
        let err = ModelSettings::default()
            .set("effort", "default", Some(&plain), "openrouter")
            .unwrap_err();
        assert!(err.to_string().contains("no route default"), "{err}");
    }

    /// Owner, 2026-10-15: every 9router model starts Off — `none`, no level
    /// beside a name that may carry one — and is turned on in Settings ›
    /// Models › Alt+C › Reasoning or in the session; on with no level asks for
    /// none ("Route default"), on every model the router serves.
    #[test]
    fn every_9router_model_starts_off_and_on_asks_for_no_level_until_one_is_chosen() {
        let router = router_with(vec![
            router_row("ag/gemini-3.8-flash-high", Some("gemini-level"), false),
            router_row("cl/inclusionai/ling-3.0-flash", None, true),
            ModelInfo::named("hand-added", true),
        ]);
        for m in &router.models {
            assert!(!m.starts_thinking("9router"), "{}", m.id);
            assert!(
                m.starts_thinking("openrouter"),
                "elsewhere a thinking model starts on"
            );
        }

        // A new session: Off, and Off is sent as `none`.
        let mut s = ModelSettings::for_provider(&router);
        assert_eq!(s.model, "ag/gemini-3.8-flash-high");
        assert!(!s.thinking);
        assert_eq!(s.effort, None);
        assert_eq!(s.body(&router)["reasoning_effort"], "none");

        for id in [
            "ag/gemini-3.8-flash-high",
            "cl/inclusionai/ling-3.0-flash",
            "hand-added",
        ] {
            let info = router.model(id);
            s.model = id.into();
            // Off is a choice even where the route cannot stop thinking.
            s.set("thinking", "false", info, "9router").unwrap();
            assert_eq!(s.body(&router)["reasoning_effort"], "none", "{id}");
            // On with no level: nothing sent, the route decides.
            s.set("thinking", "true", info, "9router").unwrap();
            s.normalize_for_model(&router);
            assert_eq!(s.effort, None, "{id}");
            assert!(!s.body(&router).contains_key("reasoning_effort"), "{id}");
            s.set("effort", ROUTE_DEFAULT, info, "9router").unwrap();
            assert!(s.thinking);
            assert!(!s.body(&router).contains_key("reasoning_effort"), "{id}");
            // Every level the router reads is sent as chosen.
            for level in ROUTER_LEVELS {
                s.set("effort", level, info, "9router").unwrap();
                s.normalize_for_model(&router);
                assert_eq!(s.body(&router)["reasoning_effort"], level, "{id}");
            }
            assert!(s.set("effort", "ultra", info, "9router").is_err(), "{id}");
            s.set("effort", "off", info, "9router").unwrap();
            assert!(!s.thinking);
        }

        // Regression: a saved `max` on a row an earlier build gave three
        // levels was silently replaced; settled, it holds.
        let mut stale = router_row("cx/gpt-6.1-sol", Some("openai"), true);
        stale.efforts = ["low", "medium", "high"].map(String::from).to_vec();
        let router = router_with(vec![stale]);
        let mut saved = ModelSettings {
            provider: "9router".into(),
            model: "cx/gpt-6.1-sol".into(),
            thinking: true,
            effort: Some("max".into()),
            ..Default::default()
        };
        saved.normalize_for_model(&router);
        assert_eq!(saved.effort.as_deref(), Some("max"));
        assert_eq!(saved.body(&router)["reasoning_effort"], "max");

        // What the person set for the model holds: Reasoning on starts on,
        // their default level is the start, options without Off always think.
        let mut on = router_row("cc/claude-sonnet-5-5", Some("claude-adaptive"), true);
        on.reasoning_enabled = Some(true);
        on.default_effort = Some("high".into());
        on.manual_fields = vec!["default_effort".into()];
        let mut always = router_row("cbai/glm-5.3", Some("zai"), true);
        always.reasoning_options = Some(["low", "high"].map(String::from).to_vec());
        let mut off = router_row("cbai/glm-5.0", Some("zai"), true);
        off.reasoning_enabled = Some(false);
        let router = router_with(vec![on, always, off]);
        let on = router.model("cc/claude-sonnet-5-5").unwrap();
        assert!(on.starts_thinking("9router"));
        assert_eq!(on.starting_effort("9router").as_deref(), Some("high"));
        let s = ModelSettings::for_provider(&router);
        assert!(s.thinking);
        assert_eq!(s.body(&router)["reasoning_effort"], "high");
        assert!(router
            .model("cbai/glm-5.3")
            .unwrap()
            .starts_thinking("9router"));
        assert!(!router
            .model("cbai/glm-5.0")
            .unwrap()
            .starts_thinking("9router"));
        assert_eq!(
            router
                .model("cbai/glm-5.3")
                .unwrap()
                .starting_effort("9router"),
            None,
            "options are not a default level"
        );
    }

    #[test]
    fn reasoning_models_support_thinking_and_effort_across_providers() {
        let mut router = preset("9router").unwrap().build("");
        router.models.push(ModelInfo {
            id: "kr/claude-opus-5-thinking".into(),
            thinking: true,
            known: vec!["efforts".into()],
            ..Default::default()
        });
        router.settle_thinking();
        let mut s = ModelSettings::for_provider(&router);
        s.model = "kr/claude-opus-5-thinking".into();
        s.thinking = true;
        s.effort = None;
        let supp = s.suppressed(&router);
        assert!(
            !supp.iter().any(|x| x.key == "thinking"),
            "thinking must not be suppressed for 9router reasoning models"
        );
        assert!(
            !supp.iter().any(|x| x.key == "effort"),
            "a 9router model has the router's levels, never a bare switch"
        );
        // "On" with no level is the route default: nothing filled in.
        let body = s.body(&router);
        assert!(!body.contains_key("reasoning_effort"), "{body:?}");
        assert!(!body.contains_key("reasoning"), "{body:?}");
        s.thinking = false;
        let body = s.body(&router);
        assert_eq!(body["reasoning_effort"], serde_json::json!("none"));
        assert!(!body.contains_key("reasoning"), "{body:?}");

        let mut openrouter = preset("openrouter").unwrap().build("");
        openrouter.models.push(ModelInfo {
            id: "deepseek/deepseek-v4.1-flash".into(),
            thinking: true,
            efforts: vec!["low".into(), "high".into(), "max".into()],
            ..Default::default()
        });
        let mut s2 = ModelSettings::for_provider(&openrouter);
        s2.model = "deepseek/deepseek-v4.1-flash".into();
        s2.thinking = true;
        s2.effort = Some("high".into());
        let supp2 = s2.suppressed(&openrouter);
        assert!(!supp2.iter().any(|x| x.key == "thinking"));
        assert!(
            !supp2.iter().any(|x| x.key == "effort"),
            "effort must not be suppressed when model specifies effort choices"
        );

        // Binary reasoning model can toggle thinking on and off
        let binary_model = router.model("kr/claude-opus-5-thinking");
        assert!(s
            .set("thinking", "false", binary_model, router.api_kind())
            .is_ok());
        assert!(!s.thinking);
        assert!(s
            .set("thinking", "true", binary_model, router.api_kind())
            .is_ok());
        assert!(s.thinking);

        // Non-thinking model refuses thinking on
        router.models.push(ModelInfo {
            id: "bzl/auto:free".into(),
            thinking: false,
            known: vec!["thinking".into()],
            ..Default::default()
        });
        let non_thinking_model = router.model("bzl/auto:free");
        s.model = "bzl/auto:free".into();
        assert!(s
            .set("thinking", "true", non_thinking_model, router.api_kind())
            .is_err());
        let supp_non = s.suppressed(&router);
        assert!(supp_non
            .iter()
            .any(|x| x.key == "thinking" && x.reason == "this model does not reason"));
        assert!(supp_non
            .iter()
            .any(|x| x.key == "effort" && x.reason == "this model does not reason"));
    }

    #[test]
    fn google_models_support_thinking_and_suppress_penalties() {
        let mut google = preset("google").unwrap().build("AIzaTestKey");
        google.models.push(ModelInfo {
            id: "gemini-3.8-flash".into(),
            thinking: true,
            thinking_required: false,
            efforts: vec![
                "minimal".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
            ],
            default_effort: Some("medium".into()),
            known: vec!["supported_parameters".into()],
            supported_parameters: vec![
                "temperature".into(),
                "top_p".into(),
                "max_tokens".into(),
                "thinking".into(),
            ],
            metadata: serde_json::json!({"reasoning": {"can_disable": true}}),
            ..Default::default()
        });
        google.models.push(ModelInfo {
            id: "gemini-embedding-001".into(),
            thinking: false,
            known: vec!["thinking".into()],
            ..Default::default()
        });

        let mut s = ModelSettings::for_provider(&google);
        s.model = "gemini-3.8-flash".into();
        s.thinking = true;
        let supp = s.suppressed(&google);
        assert!(
            !supp.iter().any(|x| x.key == "thinking"),
            "Google thinking models must not have thinking suppressed"
        );
        assert!(
            !supp.iter().any(|x| x.key == "effort"),
            "Google models with efforts must not have effort suppressed"
        );
        assert!(
            supp.iter().any(|x| x.key == "frequency_penalty"),
            "frequency_penalty must be suppressed for Google"
        );
        assert!(
            supp.iter().any(|x| x.key == "presence_penalty"),
            "presence_penalty must be suppressed for Google"
        );

        // Verify effort setting and serialization
        let gemini_model = google.model("gemini-3.8-flash");
        assert!(s
            .set("effort", "high", gemini_model, google.api_kind())
            .is_ok());
        assert_eq!(s.effort, Some("high".into()));
        let body = s.body(&google);
        assert_eq!(
            body["extra_body"]["google"]["thinking_config"]["thinking_level"].as_str(),
            Some("HIGH")
        );
        assert_eq!(
            body["extra_body"]["google"]["thinking_config"]["include_thoughts"],
            serde_json::json!(true)
        );
        assert!(!body.contains_key("reasoning_effort"));

        // API metadata advertises optional reasoning in this fixture. The
        // compatibility docs define "none" as its wire disable control.
        assert!(s
            .set("thinking", "false", gemini_model, google.api_kind())
            .is_ok());
        assert!(!s.thinking);
        let body_off = s.body(&google);
        assert!(body_off.get("extra_body").is_none());
        assert_eq!(body_off["reasoning_effort"], serde_json::json!("none"));

        // Toggle thinking back on
        assert!(s
            .set("thinking", "true", gemini_model, google.api_kind())
            .is_ok());
        assert!(s.thinking);

        // Non-thinking model has thinking suppressed
        let embed_model = google.model("gemini-embedding-001");
        s.model = "gemini-embedding-001".into();
        assert!(s
            .set("thinking", "true", embed_model, google.api_kind())
            .is_err());
        let supp_embed = s.suppressed(&google);
        assert!(supp_embed
            .iter()
            .any(|x| x.key == "thinking" && x.reason == "this model does not reason"));
    }

    #[test]
    fn effort_provider_without_api_key_or_unpopulated_efforts_allows_setting_effort() {
        let openai = preset("openai").unwrap().build("");
        let mut s = ModelSettings::for_provider(&openai);
        s.model = "o3-mini".into();
        // Even before any API key or catalogue fetch (models empty), effort is supported!
        let supp = s.suppressed(&openai);
        assert!(
            !supp.iter().any(|x| x.key == "effort"),
            "effort must not be suppressed on keyless Effort provider"
        );
        assert!(s.set("effort", "high", None, "").is_ok());
        assert_eq!(s.effort, Some("high".into()));
        assert!(s.thinking);
        let body = s.body(&openai);
        assert_eq!(
            body.get("reasoning_effort")
                .and_then(serde_json::Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn smart_max_output_calculates_bounded_ratio_honoring_provider_capability() {
        // 1. Context 1M, provider supports 500k -> 20% = 200k, clamped to max 128k bound
        assert_eq!(smart_max_output(Some(1_000_000), Some(500_000)), 128_000);

        // 2. Context 1M, provider supports 64k -> clamped to provider's 64k capability
        assert_eq!(smart_max_output(Some(1_000_000), Some(64_000)), 64_000);

        // 3. Context 200k (e.g. Claude), provider supports 64k -> 20% = 40k <= 64k
        assert_eq!(smart_max_output(Some(200_000), Some(64_000)), 40_000);

        // 4. Context 128k, provider supports 128k -> 20% = 25,600 (prevents zero context budget!)
        assert_eq!(smart_max_output(Some(128_000), Some(128_000)), 25_600);

        // 5. Small model supporting only 4,096 -> respects lower vendor limit
        assert_eq!(smart_max_output(Some(32_000), Some(4_096)), 4_096);

        // 6. Unknown context window -> defaults to minimum bound (8,192)
        assert_eq!(smart_max_output(None, Some(32_000)), 8_192);

        // 7. Unknown capability and context -> 8,192
        assert_eq!(smart_max_output(None, None), 8_192);
    }
}
