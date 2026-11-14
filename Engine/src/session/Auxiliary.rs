//! Shared configuration and accounting for non-agent model requests.
use super::Runtime;
use crate::{
    error::{Error, Result},
    panic,
    provider::{Chunk, Flow, Message, Request, RequestControl},
    settings::{configuration::Configuration, ModelSettings},
    tools::ToolDecl,
};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;

pub struct Output {
    pub text: String,
    pub calls: Vec<(String, Value)>,
}
pub struct Job<'a> {
    pub session: i64,
    pub turn: i64,
    pub role: &'static str,
    pub active: &'a ModelSettings,
    pub system: String,
    pub input: String,
    pub media: Vec<crate::media::Attachment>,
    pub tools: Vec<ToolDecl>,
    pub cancel: Arc<AtomicBool>,
    /// Run on the active model whatever `{role}.model` names: the compaction's
    /// fallback when the model chosen for it cannot hold what it must read.
    pub inherit: bool,
}

/// Keep route identity, output allowance and metadata capacity together.
/// `input_budget` compares configured route capacities for a provider-rejection
/// fallback; it is never a measurement or local admission test for source text.
pub struct Route {
    pub settings: ModelSettings,
    pub input_budget: usize,
}

pub fn route(
    rt: &Runtime,
    config: &Configuration,
    role: &str,
    active: &ModelSettings,
    inherit: bool,
) -> Result<Route> {
    let mut settings = active.clone();
    let selected = if inherit {
        &Value::Null
    } else {
        config.get(&format!("{role}.model"))
    };
    let held = panic::lock(&rt.settings);
    if !selected.is_null() {
        let provider = selected["provider"]
            .as_str()
            .ok_or_else(|| Error::invalid("auxiliary provider missing"))?;
        let model = selected["id"]
            .as_str()
            .ok_or_else(|| Error::invalid("auxiliary model missing"))?;
        let cfg = held
            .providers
            .get(provider)
            .ok_or_else(|| Error::invalid(format!("{} provider is not configured", role)))?;
        if cfg.model(model).is_none() {
            return Err(Error::invalid(format!("{} model is not configured", role)));
        }
        settings = ModelSettings::for_provider(cfg);
        settings.model = model.into();
    }
    // Role identity inherits; arbitrary agent stop/sampling controls do not.
    settings.stop.clear();
    settings.temperature = None;
    settings.top_p = None;
    settings.frequency_penalty = None;
    settings.presence_penalty = None;
    let model = held
        .providers
        .get(&settings.provider)
        .and_then(|p| p.model(&settings.model));

    let thinking_cfg = config.text(&format!("{role}.thinking"));
    let thinking_choice = thinking_cfg.trim().to_ascii_lowercase();

    let api_kind = held
        .providers
        .get(&settings.provider)
        .map_or("", |p| p.api_kind());

    if let Some(m) = model {
        let lowest_effort = || {
            m.efforts
                .iter()
                .min_by_key(|e| crate::settings::providers::effort_rank(e))
                .cloned()
        };
        let offered_choices = m.reasoning_choices(api_kind);
        let matches_effort = |level: &str| {
            m.efforts.iter().any(|e| e.eq_ignore_ascii_case(level))
                || offered_choices
                    .iter()
                    .any(|e| e != "off" && e != "on" && e.eq_ignore_ascii_case(level))
        };
        if m.thinking_required {
            settings.thinking = true;
            settings.effort = if thinking_choice.is_empty()
                || thinking_choice == "auto"
                || thinking_choice == "null"
                || thinking_choice == "default"
                || thinking_choice == "on"
            {
                if role == "title" {
                    lowest_effort().or_else(|| m.starting_effort(api_kind))
                } else {
                    m.starting_effort(api_kind)
                }
            } else if matches_effort(&thinking_choice) {
                Some(thinking_choice)
            } else if role == "title" {
                lowest_effort().or_else(|| m.starting_effort(api_kind))
            } else {
                m.starting_effort(api_kind)
            };
        } else if m.reasoning_support() == Some(false)
            || thinking_choice.is_empty()
            || matches!(
                thinking_choice.as_str(),
                "off" | "auto" | "null" | "default"
            )
        {
            settings.thinking = false;
            settings.effort = None;
        } else if thinking_choice == "on" || thinking_choice == "true" {
            settings.thinking = true;
            settings.effort = if m.route_default_level(api_kind) {
                None
            } else {
                m.starting_effort(api_kind)
            };
        } else if matches_effort(&thinking_choice) {
            settings.thinking = true;
            settings.effort = Some(thinking_choice);
        } else {
            settings.thinking = true;
            settings.effort = m.starting_effort(api_kind);
        }
    } else {
        match thinking_choice.as_str() {
            "" | "off" | "auto" | "null" | "default" => {
                settings.thinking = false;
                settings.effort = None;
            }
            "on" | "true" => {
                settings.thinking = true;
                settings.effort = None;
            }
            level => {
                settings.thinking = true;
                settings.effort = Some(level.to_string());
            }
        }
    }
    // Every role's budget is a configured value with its shipped default in
    // the configuration schema, never a number here.
    let output = config.number(&format!("{}.max_output_tokens", role));
    let model_max = model.and_then(|m| m.max_output);
    let context_ceiling = model
        .and_then(|m| m.context)
        .filter(|w| *w > 0)
        .filter(|w| output >= *w)
        .map(|w| crate::settings::model::smart_max_output(Some(w), model_max));
    let effective = model_max.map_or(output, |limit| output.min(limit));
    let effective = context_ceiling.map_or(effective, |limit| effective.min(limit));
    settings.max_output = Some(effective);
    let allowance = config.number("auxiliary.mandatory_reasoning_output_tokens");
    // A title's budget is for the title. Models that reason whether asked to or
    // not spent a small budget on thinking and returned no title at all — seen
    // in `token_usage` with a 12-token budget (audit P8) — so the reasoning
    // allowance always comes on top. Only tokens actually written are billed,
    // so a model that does not reason costs exactly what it did before.
    if role == "title" {
        settings.max_output =
            Some(model_max.map_or(output + allowance, |limit| (output + allowance).min(limit)));
    }
    if settings.thinking {
        let wanted = if role == "title" {
            output + allowance
        } else {
            output.max(allowance)
        };
        settings.max_output = Some(model_max.map_or(wanted, |limit| wanted.min(limit)));
    }
    if let Some(ceiling) = context_ceiling {
        settings.max_output = settings.max_output.map(|n| n.min(ceiling));
    }
    let input_budget = super::compact::budget_usable(
        model.and_then(|m| m.context),
        settings.max_output,
        config.number("compact.fallback_context_tokens") as usize,
        model.and_then(|m| m.input_limit),
    );
    Ok(Route {
        settings,
        input_budget,
    })
}

pub fn run(rt: &Runtime, config: &Configuration, job: Job<'_>) -> Result<Output> {
    let settings = route(rt, config, job.role, job.active, job.inherit)?.settings;
    let timeout = match job.role {
        "classifier" => Some(config.duration("classifier.timeout")),
        "sanitizer" => Some(config.duration("sanitizer.timeout")),
        _ => None,
    };
    // A job that offers exactly one tool is asking for that call and nothing
    // else; the endpoint is told so where it can be.
    let force_tool = (job.tools.len() == 1).then(|| job.tools[0].name.clone());
    let request = Request {
        system: job.system,
        messages: vec![Message::user(job.input).with_media(job.media)],
        tools: job.tools,
        settings,
        control: RequestControl {
            cancel: Some(Arc::clone(&job.cancel)),
            timeout,
            role: Some(job.role),
            config_revision: if job.role == "classifier" && !config.home.as_os_str().is_empty() {
                let path = config.home.join(crate::settings::configuration::FILE);
                Some((
                    path.clone(),
                    crate::settings::configuration::read_bounded(&path, 8 * 1024 * 1024)
                        .ok()
                        .map(|bytes| crate::checkpoint::objects::hash_of(&bytes)),
                ))
            } else {
                None
            },
            attachment_text_chars: Some(config.number("attachments.max_text_chars") as usize),
            force_tool,
        },
    };
    let mut output = Output {
        text: String::new(),
        calls: Vec::new(),
    };
    let mut usage = None;
    let mut overflow = false;
    let mut bytes = 0usize;
    let max = config.number("network.max_event_size") as usize;
    let start = Instant::now();
    let result = rt.provider.stream(&request, &mut |chunk| {
        let timed_out = timeout.is_some_and(|t| start.elapsed() >= t);
        if (job.cancel.load(Ordering::Relaxed) || timed_out)
            && !matches!(&chunk, Chunk::Usage { .. })
        {
            return Flow::Stop;
        }
        match chunk {
            Chunk::Text(s) => {
                bytes = bytes.saturating_add(s.len());
                if bytes <= max {
                    output.text.push_str(&s);
                }
            }
            Chunk::ToolCall { name, args, .. } => {
                bytes = bytes.saturating_add(args.to_string().len());
                if bytes <= max && output.calls.len() < 2 {
                    output.calls.push((name, args));
                } else {
                    overflow = true;
                }
            }
            Chunk::Usage {
                input,
                output,
                cached,
                details,
            } => usage = Some((input, output, cached, details)),
            // The attempt that produced what is held failed and the next one
            // starts over (`Chunk::Reset`). Kept, its text would be glued to the
            // retry's, and its calls counted as decisions the model never
            // finished making — the classifier reads these calls too.
            Chunk::Reset => {
                output.text.clear();
                output.calls.clear();
                bytes = 0;
                overflow = false;
            }
            _ => {}
        }
        if bytes > max {
            overflow = true;
        }
        if overflow {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    if let Some((input, output, cached, details)) = usage {
        let cost = panic::lock(&rt.settings).price_of(
            &request.settings.provider,
            &request.settings.model,
            input,
            output,
            cached,
            &details,
        );
        panic::lock(&rt.db).record_usage_role(
            job.session,
            job.turn,
            input,
            output,
            cached,
            cost,
            &request.settings.provider,
            &request.settings.model,
            job.role,
            &details,
        )?;
        let _ = rt.events.send(crate::proto::Event::AuxiliaryUsage {
            session: job.session,
            role: job.role.into(),
            input,
            output,
            cached,
            cost,
        });
        super::spend::publish(rt, job.session);
        super::log::usage(
            rt,
            job.session,
            job.role,
            &request.settings.model,
            input,
            output,
            cached,
            cost,
            details.reasoning,
        );
    }
    if job.cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if timeout.is_some_and(|t| start.elapsed() >= t) {
        return Err(Error::Provider(format!(
            "{}_timeout: request deadline exceeded",
            job.role
        )));
    }
    result?;
    if overflow {
        return Err(Error::Provider(format!(
            "{}_invalid_response: output limit exceeded",
            job.role
        )));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::scripted::Scripted;
    use serde_json::json;
    use std::sync::Mutex;

    /// Audit P8 (the owner's missing titles): a 12-token title budget is the
    /// title's alone — the reasoning allowance comes on top, and a model that
    /// must think does so at its lowest effort.
    #[test]
    fn a_title_keeps_its_budget_for_the_title() {
        let home = std::env::temp_dir().join(format!("rune-aux-title-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("rune.toml"), "[title]\nmax_output_tokens = 12\n").unwrap();
        let config = Configuration::load(&home).unwrap();
        let _ = std::fs::remove_dir_all(&home);
        let allowance = config.number("auxiliary.mandatory_reasoning_output_tokens");
        for (required, expected_effort) in [(false, None), (true, Some("low"))] {
            let mut settings = crate::settings::Settings::empty();
            let mut provider = crate::settings::providers::preset("anthropic")
                .unwrap()
                .build("");
            provider.models.push(crate::settings::providers::ModelInfo {
                id: "fixture".into(),
                thinking: required,
                thinking_required: required,
                efforts: vec!["high".into(), "low".into(), "medium".into()],
                default_effort: Some("high".into()),
                ..Default::default()
            });
            let active = ModelSettings::for_provider(&provider);
            settings.providers.list.push(provider);
            let settings = Arc::new(Mutex::new(settings));
            let db = Arc::new(Mutex::new(crate::store::Db::memory().unwrap()));
            let session = crate::panic::lock(&db).create_session("/f").unwrap().id;
            let scripted = Arc::new(
                Scripted::from_value(
                    &json!({"responses":[{"chunks":[{"text":"A Title"}]}]}),
                    "fixture".into(),
                )
                .unwrap(),
            );
            let limits = Arc::new(crate::limits::Limits::default());
            let runtime = Runtime {
                mcp: crate::capabilities::mcp::Runtime::new().unwrap(),
                db,
                registry: Arc::new(crate::tools::Registry::read_only(
                    limits.clone(),
                    settings.clone(),
                )),
                limits,
                provider: scripted.clone(),
                settings,
                events: crate::session::live::Bus::detached(),
            };
            let mut active = active;
            active.model = "fixture".into();
            run(
                &runtime,
                &config,
                Job {
                    session,
                    turn: 1,
                    role: "title",
                    active: &active,
                    system: "title".into(),
                    input: "request".into(),
                    media: Vec::new(),
                    tools: Vec::new(),
                    cancel: Arc::new(AtomicBool::new(false)),
                    inherit: false,
                },
            )
            .unwrap();
            let sent = scripted.last_sent().unwrap();
            assert!(
                sent.settings.max_output.unwrap() >= 12 + allowance.min(4096),
                "required {required}: {:?}",
                sent.settings.max_output
            );
            assert_eq!(sent.settings.effort.as_deref(), expected_effort);
        }
    }

    #[test]
    fn every_auxiliary_role_preserves_mandatory_reasoning_and_records_its_own_usage() {
        let config = Configuration::default();
        let mut settings = crate::settings::Settings::empty();
        let mut provider = crate::settings::providers::preset("anthropic")
            .unwrap()
            .build("");
        provider.models.push(crate::settings::providers::ModelInfo {
            id: "fixture".into(),
            thinking: true,
            thinking_required: true,
            context: Some(200000),
            max_output: Some(8192),
            ..Default::default()
        });
        let mut active = ModelSettings::for_provider(&provider);
        active.stop = vec!["old stop".into()];
        active.temperature = Some(0.5);
        settings.providers.list.push(provider);
        let settings = Arc::new(Mutex::new(settings));
        let db = Arc::new(Mutex::new(crate::store::Db::memory().unwrap()));
        let session = crate::panic::lock(&db)
            .create_session("/fixture")
            .unwrap()
            .id;
        let scripted=Arc::new(Scripted::from_value(&json!({"responses":(0..3).map(|_|json!({"chunks":[{"text":"OK"},{"usage":{"input":10,"output":2}}]})).collect::<Vec<_>>()}),"fixture".into()).unwrap());
        let limits = Arc::new(crate::limits::Limits::default());
        let registry = Arc::new(crate::tools::Registry::read_only(
            limits.clone(),
            settings.clone(),
        ));
        let events = crate::session::live::Bus::detached();
        let runtime = Runtime {
            mcp: crate::capabilities::mcp::Runtime::new().unwrap(),
            db: db.clone(),
            registry,
            limits,
            provider: scripted.clone(),
            settings,
            events,
        };
        for role in ["title", "compact", "classifier"] {
            run(
                &runtime,
                &config,
                Job {
                    session,
                    turn: 1,
                    role,
                    active: &active,
                    system: "fixture".into(),
                    input: "test".into(),
                    media: Vec::new(),
                    tools: Vec::new(),
                    cancel: Arc::new(AtomicBool::new(false)),
                    inherit: false,
                },
            )
            .unwrap();
            let sent = scripted.last_sent().unwrap();
            assert!(sent.settings.thinking);
            assert!(sent.settings.max_output.unwrap() >= 4096);
            assert!(sent.settings.stop.is_empty());
            assert_eq!(sent.settings.temperature, None);
            assert_eq!(sent.settings.model, "fixture");
        }
        let usage = crate::panic::lock(&db)
            .session_usage(session, &Default::default())
            .unwrap();
        assert_eq!((usage.tok_in, usage.tok_out, usage.used), (30, 6, 0));
    }

    #[test]
    fn auxiliary_thinking_route_follows_config_model_and_custom_models() {
        let mut settings = crate::settings::Settings::empty();
        let mut provider = crate::settings::providers::preset("openrouter")
            .unwrap()
            .build("");
        provider.models.push(crate::settings::providers::ModelInfo {
            id: "opt-model".into(),
            thinking: true,
            reasoning_enabled: Some(true),
            efforts: vec!["low".into(), "medium".into(), "high".into()],
            default_effort: Some("medium".into()),
            ..Default::default()
        });
        let mut active = ModelSettings::for_provider(&provider);
        active.model = "opt-model".into();
        settings.providers.list.push(provider);
        let settings = Arc::new(Mutex::new(settings));
        let db = Arc::new(Mutex::new(crate::store::Db::memory().unwrap()));
        let limits = Arc::new(crate::limits::Limits::default());
        let runtime = Runtime {
            mcp: crate::capabilities::mcp::Runtime::new().unwrap(),
            db,
            registry: Arc::new(crate::tools::Registry::read_only(
                limits.clone(),
                settings.clone(),
            )),
            limits,
            provider: Arc::new(
                Scripted::from_value(&json!({"responses":[]}), "opt-model".into()).unwrap(),
            ),
            settings,
            events: crate::session::live::Bus::detached(),
        };

        let home = std::env::temp_dir().join(format!("rune-aux-think-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[classifier]\nthinking = \"high\"\n[compact]\nthinking = \"off\"\n[sanitizer]\nthinking = \"on\"\n",
        )
        .unwrap();
        let cfg = Configuration::load(&home).unwrap();

        let cls = route(&runtime, &cfg, "classifier", &active, false).unwrap();
        assert!(cls.settings.thinking);
        assert_eq!(cls.settings.effort.as_deref(), Some("high"));

        let cmp = route(&runtime, &cfg, "compact", &active, false).unwrap();
        assert!(!cmp.settings.thinking);
        assert_eq!(cmp.settings.effort, None);

        let san = route(&runtime, &cfg, "sanitizer", &active, false).unwrap();
        assert!(san.settings.thinking);
        assert_eq!(san.settings.effort.as_deref(), Some("medium"));

        // Unset title defaults to off when thinking is optional
        let ttl = route(&runtime, &cfg, "title", &active, false).unwrap();
        assert!(!ttl.settings.thinking);
        assert_eq!(ttl.settings.effort, None);

        // Unlisted custom model (model == None) still respects explicit off / high, and unset -> off
        let mut custom_active = active.clone();
        custom_active.model = "unlisted-custom-id".into();
        let custom_cls = route(&runtime, &cfg, "classifier", &custom_active, false).unwrap();
        assert!(custom_cls.settings.thinking);
        assert_eq!(custom_cls.settings.effort.as_deref(), Some("high"));
        let custom_cmp = route(&runtime, &cfg, "compact", &custom_active, false).unwrap();
        assert!(!custom_cmp.settings.thinking);
        assert_eq!(custom_cmp.settings.effort, None);
        let custom_ttl = route(&runtime, &cfg, "title", &custom_active, false).unwrap();
        assert!(!custom_ttl.settings.thinking);

        let _ = std::fs::remove_dir_all(&home);
    }
}
