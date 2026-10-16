//! Adding, testing and inspecting providers — the operations behind
//! `rune -provider …` and the settings panel.
//!
//! These live here rather than in the frontend for the reason every decision in
//! this engine lives here: `RuneCli` and a later GUI must not each grow their
//! own idea of what a valid provider is. A frontend collects a name and a key;
//! everything that follows — which host the key implies, whether it works,
//! which models exist — happens once, in this file.
//!
//! # Nothing is saved until it is proven
//!
//! [`add`] makes a real request before writing anything. A provider that is
//! stored but broken is worse than one that was never added: the failure
//! surfaces later, inside a turn, looking like a bug in the agent.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::providers::{preset, Auth, ModelInfo, Price, ProviderConfig, Thinking, PRESETS};
use super::secrets::KeyState;
use super::{ModelSettings, Settings};
use crate::error::{Error, Result};
use crate::net::Http;
use crate::panic;

/// Short enough that a wrong base URL fails while the user is still looking at
/// the screen, long enough for a cold model to answer.
pub type Shared = Arc<Mutex<Settings>>;

/// The worker's single dispatch path; every success includes the registry as
/// it now stands, so a UI never races a refresh against a pending mutation.
pub fn execute(settings: &Shared, command: crate::Command) -> Result<Value> {
    use crate::Command;
    let mut out = match command {
        Command::ProviderAdd {
            name,
            preset,
            base_url,
            key_name,
            key,
            model,
            ..
        } => add(
            settings,
            &name,
            preset.as_deref(),
            base_url.as_deref(),
            key_name.as_deref(),
            key.as_deref(),
            model.as_deref(),
        )?,
        Command::ProviderModels { name, .. } => models(settings, &name, None)?,
        Command::ProviderTest { name, model, .. } => test(settings, &name, model.as_deref())?,
        Command::ProviderKey {
            name,
            key_name,
            key,
            ..
        } => add_key(settings, &name, key_name.as_deref(), &key)?,
        Command::ProviderRemove { name, .. } => remove(settings, &name)?,
        Command::ProviderTimeout { name, seconds, .. } => set_timeout(settings, &name, seconds)?,
        Command::ProviderDropKey { name, position, .. } => drop_key(settings, &name, position)?,
        Command::CredentialsNew { .. } => new_credential_store(settings)?,
        Command::ProviderDropModel {
            provider, model, ..
        } => drop_model(settings, &provider, &model)?,
        Command::ProviderConfigureModel {
            provider, model, ..
        } => configure_model(settings, &provider, *model)?,
        Command::ProviderDefault { name, .. } => set_default(settings, &name)?,
        Command::ProviderBaseUrl { name, base_url, .. } => {
            set_base_url(settings, &name, base_url.as_deref())?
        }
        Command::ServiceAccount {
            name,
            api,
            base_url,
            fetch_url,
            key_name,
            key,
            ..
        } => super::connections::account(
            settings,
            super::connections::Account {
                name,
                api,
                base_url,
                fetch_url,
                key_name,
                key,
            },
        )?,
        Command::ServiceSelect {
            service, target, ..
        } => super::connections::select(settings, &service, target)?,
        Command::ServiceSave {
            name,
            api,
            base_url,
            fetch_url,
            search,
            fetch,
            routes,
            zone,
            key_name,
            key,
            ..
        } => super::connections::save(
            settings,
            super::connections::Save {
                name,
                api,
                base_url,
                fetch_url,
                search,
                fetch,
                routes,
                zone,
                key_name,
                key,
            },
        )?,
        Command::ServiceTest { name, service, .. } => {
            super::connections::test(settings, &name, &service)?
        }
        Command::ServiceRemove { name, .. } => super::connections::remove(settings, &name)?,
        Command::ServiceOptions {
            name,
            api,
            base_url,
            key,
            ..
        } => super::connections::options(settings, &name, api, base_url, key)?,
        _ => return Err(Error::invalid("Not a provider operation.")),
    };
    out["registry"] = list(settings);
    let s = panic::lock(settings);
    out["web_pins"] = json!({
        "search": s.config.connection(crate::web::pin_key(super::services::Service::Search)),
        "fetch": s.config.connection(crate::web::pin_key(super::services::Service::Fetch)),
    });
    Ok(out)
}

/// Everything a frontend needs to draw the provider screen.
///
/// Keys are reported as present or absent and **never** returned. A frontend
/// that wanted to display one could only leak it.
pub fn list(settings: &Shared) -> Value {
    let s = panic::lock(settings);
    let configured: Vec<Value> = s
        .providers
        .list
        .iter()
        .map(|p| {
            // How big the ring is and how much of it is usable, so a rotation is
            // visible on the screen instead of only through `-provider keys`.
            // Counts only: the values never leave `secrets`.
            let (keys, ready) = p
                .secret
                .as_deref()
                .map(|n| s.secrets.health(n))
                .unwrap_or((0, 0));
            let keys_list = p
                .secret
                .as_deref()
                .map(|n| s.secrets.keys_metadata(n))
                .unwrap_or_default();
            json!({
                "name": p.name,
                "base_url": p.base_url,
                "api": p.api_kind(),
                "thinking": p.thinking,
                "timeout": p.timeout,
                "models": p.models,
                "needs_key": p.secret.is_some(),
                "has_key": p.secret.as_deref().map(|n| s.secrets.get(n).is_some()).unwrap_or(true),
                // What every surface says of the key: present, missing (and
                // why), or unavailable with the store's cause.
                "key_state": p.secret.as_deref().map_or("none", |n| s.secrets.key_state(n).code()),
                "key_note": p.secret.as_deref().map(|n| s.secrets.key_state(n).note(n)),
                "keys": keys,
                "ready": ready,
                "keys_list": keys_list,
                "default": s.providers.default.as_deref() == Some(p.name.as_str()),
            })
        })
        .collect();
    let mut out = json!({
        "credentials": credentials(&s.secrets),
        "providers": configured,
        "presets": PRESETS
            .iter()
            .map(|p| json!({
                "name": p.name,
                "key_hint": p.key_hint,
                "needs_key": !p.key_prefixes.is_empty(),
            }))
            .collect::<Vec<_>>(),
    });
    // The connections that serve the tools are drawn on the same screen: one
    // list of connections, each with the services it serves.
    if let (Some(out), Value::Object(services)) =
        (out.as_object_mut(), super::connections::list(&s))
    {
        out.extend(services);
    }
    out
}

/// The credential store as every surface shows it: `absent`, `ready` or
/// `unavailable` with its cause and the sentence that says what to do.
pub fn credentials(secrets: &super::secrets::Secrets) -> Value {
    use super::secrets::Store;
    match secrets.store() {
        Store::Absent => json!({ "state": "absent" }),
        Store::Ready => json!({ "state": "ready" }),
        Store::Unavailable(why) => json!({
            "state": "unavailable",
            "cause": why.code(),
            "note": why.message(),
        }),
    }
}

/// Set the unreadable credential store aside with its master key and start a
/// new one — the person's explicit choice, never Rune's.
pub fn new_credential_store(settings: &Shared) -> Result<Value> {
    let home = panic::lock(settings).home.clone();
    if home.as_os_str().is_empty() {
        return Err(Error::invalid(
            "this configuration has nowhere to keep credentials",
        ));
    }
    let aside = super::secrets::start_new_store(&home)?;
    panic::lock(settings).reload_secrets();
    Ok(json!({ "set_aside": aside, "credentials": credentials(&panic::lock(settings).secrets) }))
}

/// Add a provider, after proving it works.
///
/// `preset_name` picks a known endpoint; `base_url` overrides or replaces it for
/// anything OpenAI-compatible that Rune has never heard of.
pub fn add(
    settings: &Shared,
    name: &str,
    preset_name: Option<&str>,
    base_url: Option<&str>,
    key_name: Option<&str>,
    key: Option<&str>,
    model: Option<&str>,
) -> Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid("a provider needs a name"));
    }
    let chosen = preset_name.unwrap_or(name);
    let key = key.map(str::trim).unwrap_or("");

    let mut config = match preset(chosen) {
        Some(p) => {
            // Emptiness is checked before shape: "that does not look like a
            // mimo key" is a baffling thing to say about nothing at all.
            if !p.key_prefixes.is_empty() && key.is_empty() {
                return Err(Error::invalid(format!(
                    "`{name}` needs an API key — {}",
                    p.key_hint
                )));
            }
            if !p.key_looks_right(key) {
                return Err(Error::invalid(format!(
                    "that key does not look like a {} key — expected {}",
                    p.name, p.key_hint
                )));
            }
            let mut c = p.build(key);
            c.name = name.to_string();
            c.secret = c.secret.map(|_| name.to_string());
            c
        }
        None => {
            let existing_opt = panic::lock(settings).providers.get(name).cloned();
            if let Some(mut existing) = existing_opt {
                if let Some(url) = base_url {
                    existing.base_url = url.to_string();
                }
                if !key.is_empty() {
                    existing.secret = Some(name.to_string());
                }
                existing
            } else {
                let url = base_url.ok_or_else(|| {
                    Error::invalid(format!(
                        "`{chosen}` is not a known provider — give a base URL for an OpenAI-compatible endpoint"
                    ))
                })?;
                ProviderConfig {
                    api: Some("compatible".into()),
                    max_payload_size: super::providers::default_max_payload_size(),
                    force_tool_choice: false,
                    name: name.to_string(),
                    base_url: url.to_string(),
                    auth: Auth::bearer(),
                    // A base URL given by hand is an OpenAI-compatible endpoint by
                    // definition: the alternative protocol is reached through its
                    // preset, which knows three other things it needs.
                    dialect: Default::default(),
                    timeout: None,
                    thinking: Thinking::Effort,
                    models: Vec::new(),
                    secret: (!key.is_empty()).then(|| name.to_string()),
                    headers: Default::default(),
                }
            }
        }
    };
    if let Some(url) = base_url {
        config.base_url = url.to_string();
    }
    if config.models.is_empty() {
        if let Some(existing) = panic::lock(settings).providers.get(name) {
            if !existing.models.is_empty() {
                config.models = existing.models.clone();
            }
        }
    }
    if config.secret.is_some() && key.is_empty() {
        return Err(Error::invalid(format!("`{name}` needs an API key")));
    }

    config.check()?;

    // The key has to be stored before the probe — the probe reads it the same
    // way a real turn does, which is what makes a pass here mean something.
    //
    // What was there before is kept, because a failed attempt must leave the
    // install exactly as it found it. Re-adding a working provider with a
    // mistyped key would otherwise destroy the key that was working, and the
    // user would discover it on their next turn rather than here.
    let previous: Vec<crate::settings::secrets::SecretItem> = config
        .secret
        .as_ref()
        .map(|name| panic::lock(settings).secrets.raw_items(name))
        .unwrap_or_default();
    if let Some(secret) = config.secret.clone() {
        if let Some(kname) = key_name.filter(|s| !s.trim().is_empty()) {
            panic::lock(settings).secrets.restore(
                &secret,
                vec![crate::settings::secrets::SecretItem::Named {
                    name: kname.to_string(),
                    key: key.to_string(),
                }],
            )?;
        } else {
            panic::lock(settings).secrets.set(&secret, key)?;
        }
    }

    // Live catalogue fetch directly verifies credentials against the vendor without guessing models
    let (fetched_models, catalog_err) = {
        let s = panic::lock(settings);
        let headers = s.headers_for(&config)?;
        let runtime = s.config.clone();
        drop(s);
        match fetch_catalogue(&config, &headers, &runtime, None) {
            Ok((models, _)) => (models, None),
            Err(Error::Rejected {
                status: 404 | 405, ..
            }) => (Vec::new(), None),
            Err(e) => (Vec::new(), Some(e)),
        }
    };

    if let Some(e) = catalog_err {
        if let Some(secret) = &config.secret {
            let _ = panic::lock(settings).secrets.restore(secret, previous);
        }
        return Err(e);
    }

    if !fetched_models.is_empty() {
        config.models = fetched_models;
    }

    if let Some(m) = model {
        if config.model(m).is_none() {
            // A model the user named that the preset does not list: record it,
            // with the honest admission that its ceilings are unknown.
            config.models.insert(
                0,
                ModelInfo {
                    id: m.to_string(),
                    ..Default::default()
                },
            );
        }
    }

    let mut s = panic::lock(settings);
    if let Err(error) = s.providers.add(config.clone()) {
        if let Some(secret) = &config.secret {
            s.secrets.restore(secret, previous)?;
        }
        return Err(error);
    }
    Ok(json!({
        "added": config.name,
        "base_url": config.base_url,
        "model": model,
        "default": s.providers.default.as_deref() == Some(config.name.as_str()),
    }))
}

pub fn remove(settings: &Shared, name: &str) -> Result<Value> {
    let mut s = panic::lock(settings);
    if !s.providers.remove(name)? {
        return Err(Error::NotFound(format!("provider {name}")));
    }
    // The key outlives the provider entry on purpose: removing a provider to
    // re-add it differently is common, and silently destroying the credential
    // would turn a two-second fix into a trip to the dashboard.
    Ok(json!({ "removed": name, "key_kept": true }))
}

/// Store a key by name. Used for model and service connections alike — one
/// door, so there is one place where a credential can be written.
/// Add a key to `name`'s ring, keeping any that are already there.
///
/// Adding rather than replacing, because a second key is the whole point: when
/// the first is rate-limited the engine moves to the next one on its own. To
/// replace a ring outright, drop what is there with [`drop_key`] first.
pub fn add_key(settings: &Shared, name: &str, key_name: Option<&str>, key: &str) -> Result<Value> {
    let mut s = panic::lock(settings);
    s.secrets.append_named(name, key_name, key)?;
    if let Some(p) = s.providers.list.iter_mut().find(|p| p.name == name) {
        if p.secret.is_none() {
            p.secret = Some(name.to_string());
            let updated = p.clone();
            s.providers.add(updated)?;
        }
    }
    Ok(json!({ "stored": name, "keys": s.secrets.ring(name).len() }))
}

/// Where a model connection is reached: `base_url`, or its preset's address
/// when `None`. Proven before it is saved, the way [`add`] proves a new one: a
/// catalogue request with the connection's own key (an endpoint without a
/// catalogue answers 404/405, which still proves address and key).
pub fn set_base_url(settings: &Shared, name: &str, base_url: Option<&str>) -> Result<Value> {
    let (mut config, key) = {
        let s = panic::lock(settings);
        let config = s
            .providers
            .get(name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?;
        // The address is proven with the connection's own key: one that is
        // missing or unreadable is said as such, not sent as nothing.
        let key = match config.secret.as_deref() {
            Some(n) => s.secrets.require(n)?.to_string(),
            None => String::new(),
        };
        (config, key)
    };
    config.base_url = match base_url.map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => preset(config.api_kind())
            .or_else(|| preset(name))
            .map(|p| p.build(&key).base_url)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "{name} has no preset address to go back to — type its base URL"
                ))
            })?,
    };
    config.check()?;
    let (headers, runtime) = {
        let s = panic::lock(settings);
        (s.headers_for(&config)?, s.config.clone())
    };
    match fetch_catalogue(&config, &headers, &runtime, None) {
        Ok(_)
        | Err(Error::Rejected {
            status: 404 | 405, ..
        }) => {}
        Err(e) => return Err(e),
    }
    panic::lock(settings).providers.add(config.clone())?;
    Ok(json!({ "provider": name, "base_url": config.base_url }))
}

/// How long this provider's calls may take, in seconds. `None` clears it.
pub fn set_timeout(settings: &Shared, name: &str, seconds: Option<u64>) -> Result<Value> {
    let mut config = {
        let s = panic::lock(settings);
        s.providers
            .get(name)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?
    };
    config.timeout = seconds;
    // Through `add`, so the range check lives with the field and not with the
    // command that happens to set it.
    panic::lock(settings).providers.add(config)?;
    Ok(json!({ "provider": name, "timeout": seconds }))
}

/// What `name` holds, masked. The values never leave this function.
pub fn keys(settings: &Shared, name: &str) -> Result<Value> {
    let s = panic::lock(settings);
    let state = s.secrets.key_state(name);
    let (total, ready) = s.secrets.health(name);
    if total == 0 {
        return Err(match state {
            KeyState::Unavailable(_) => Error::invalid(state.note(name)),
            _ => Error::NotFound(state.note(name)),
        });
    }
    let listed: Vec<Value> = s
        .secrets
        .keys_metadata(name)
        .into_iter()
        .map(|k| json!({ "position": k.position, "name": k.name, "masked": k.preview }))
        .collect();
    Ok(json!({ "name": name, "keys": listed, "total": total, "ready": ready }))
}

/// Remove one key by the position [`keys`] printed, counting from 1.
pub fn drop_key(settings: &Shared, name: &str, position: usize) -> Result<Value> {
    let mut s = panic::lock(settings);
    if !s.secrets.remove_at(name, position)? {
        return Err(Error::NotFound(format!(
            "{name} has no key at position {position}"
        )));
    }
    Ok(json!({ "name": name, "keys": s.secrets.ring(name).len() }))
}

/// Remove one model from a provider's configured/remembered list.
pub fn drop_model(settings: &Shared, provider: &str, model: &str) -> Result<Value> {
    let mut s = panic::lock(settings);
    let removed = s.providers.forget_model(provider, model)?;
    if !removed {
        return Err(Error::NotFound(format!(
            "Model '{model}' not found in provider '{provider}'"
        )));
    }
    Ok(json!({ "provider": provider, "model": model, "removed": true }))
}

/// Update and persist metadata for a model.
pub fn configure_model(
    settings: &Shared,
    provider: &str,
    mut model: crate::settings::providers::ModelInfo,
) -> Result<Value> {
    let mut s = panic::lock(settings);
    let config = s
        .providers
        .get(provider)
        .ok_or_else(|| Error::NotFound(format!("provider {provider}")))?;
    if let Some(options) = &model.reasoning_options {
        let choices = model.reasoning_choices(config.api_kind());
        if options.is_empty() || options.iter().any(|option| !choices.contains(option)) {
            return Err(Error::invalid(
                "Choose at least one supported reasoning option",
            ));
        }
    }
    // A level chosen for a model whose reasoning nobody published says it
    // thinks: the person settles what the catalogue left unknown.
    if model.reasoning_support().is_none()
        && model
            .reasoning_options
            .as_ref()
            .is_some_and(|options| options.iter().any(|o| o != "off"))
    {
        model.thinking = true;
    }
    if model.thinking_required && model.reasoning_enabled == Some(false) {
        return Err(Error::invalid(
            "Reasoning cannot be disabled for this model",
        ));
    }
    s.providers.update_model_metadata(provider, model.clone())?;
    Ok(json!({ "provider": provider, "configured": model, "ok": true }))
}

/// Enough of a key to recognise it, never enough to use it.
fn mask(key: &str) -> String {
    let count = key.chars().count();
    if count <= 8 {
        return "•".repeat(count);
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(count - 4).collect();
    format!("{head}…{tail}")
}

pub fn set_default(settings: &Shared, name: &str) -> Result<Value> {
    panic::lock(settings).providers.set_default(name)?;
    Ok(json!({ "default": name }))
}

/// Ask a provider what models it offers.
///
/// Falls back to what is configured when the endpoint has no catalogue: an
/// Ollama does, a preset already knows its own, and neither should look broken
/// because the other convention was used.
pub fn models(
    settings: &Shared,
    name: &str,
    control: Option<&crate::provider::RequestControl>,
) -> Result<Value> {
    if control.is_some_and(|c| c.cancelled()) {
        return Err(Error::Cancelled);
    }
    let (config, headers) = {
        let s = panic::lock(settings);
        let c = s
            .providers
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?
            .clone();
        let h = s.headers_for(&c)?;
        (c, h)
    };
    let runtime = panic::lock(settings).config.clone();
    let (fetched, skipped) = match fetch_catalogue(&config, &headers, &runtime, control) {
        Ok(result) => result,
        Err(Error::Rejected {
            status: 404 | 405, ..
        }) if !config.models.is_empty() => {
            return Ok(
                json!({"provider":name,"configured":config.models,"cached":true,"catalogue_status":"endpoint_unavailable_manual_metadata","available":config.models.iter().map(|m|&m.id).collect::<Vec<_>>()}),
            )
        }
        Err(error) => return Err(error),
    };
    {
        let mut current = panic::lock(settings);
        if current.providers.get(name).is_none_or(|p| {
            p.base_url != config.base_url || p.api != config.api || p.dialect != config.dialect
        }) {
            return Err(Error::invalid(
                "Provider configuration changed while fetching models; refresh again",
            ));
        }
        current.providers.cache_models(name, fetched)?;
    }
    let listed = panic::lock(settings)
        .providers
        .get(name)
        .map(|p| p.models.clone())
        .unwrap_or_default();
    Ok(
        json!({"provider":name,"configured":listed,"cached":false,"available":listed.iter().map(|m|&m.id).collect::<Vec<_>>(),"priced":listed.iter().filter(|m|m.price.is_some()).count(),"skipped_rows":skipped}),
    )
}

/// Fetch and validate a complete provider-owned catalogue before replacing the cache.
/// Returns the parsed models and how many raw rows were skipped as unparseable,
/// so a partially-decodable catalogue still populates every model it can.
fn fetch_catalogue(
    config: &ProviderConfig,
    headers: &[(String, String)],
    runtime: &super::configuration::Shared,
    control: Option<&crate::provider::RequestControl>,
) -> Result<(Vec<ModelInfo>, usize)> {
    if control.is_some_and(|c| c.cancelled()) {
        return Err(Error::Cancelled);
    }
    let base = config.base_url.trim_end_matches('/');
    let api = config.api_kind();
    let root = match api {
        "google" => format!("{}/models", base.strip_suffix("/openai").unwrap_or(base)),
        "xai" => format!("{base}/language-models"),
        "ollama" => format!("{}/api/tags", base.strip_suffix("/v1").unwrap_or(base)),
        _ => format!("{base}/models"),
    };
    let http = Http::configured(
        runtime,
        Some(runtime.duration("network.catalog_timeout")),
        control.cloned().unwrap_or_default(),
    );
    let running = if api == "ollama" {
        let response = http.get_json(
            &format!("{}/api/ps", base.strip_suffix("/v1").unwrap_or(base)),
            headers,
        )?;
        if !response.ok() {
            return Err(crate::net::rejected(response.status, &response.text));
        }
        response
            .body
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| Error::Provider("invalid running-model catalogue".into()))?
    } else {
        Vec::new()
    };
    let mut google_headers;
    let catalog_headers = if api == "google" {
        google_headers = headers.to_vec();
        if let Some((_, val)) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        {
            let key = val.strip_prefix("Bearer ").unwrap_or(val);
            google_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
            google_headers.push(("x-goog-api-key".to_string(), key.to_string()));
        }
        &google_headers
    } else {
        headers
    };
    let mut next = Some(root.clone());
    let mut visited = std::collections::HashSet::new();
    let mut models = Vec::new();
    let mut skipped = 0usize;
    let mut bytes = 0u64;
    while let Some(url) = next.take() {
        if control.is_some_and(|c| c.cancelled()) {
            return Err(Error::Cancelled);
        }
        if visited.len() >= runtime.number("network.catalog_max_pages") as usize
            || !visited.insert(url.clone())
        {
            return Err(Error::Provider(
                "model catalogue pagination exceeded its limit or repeated a page".into(),
            ));
        }
        let response = http.get_json(&url, catalog_headers)?;
        if !response.ok() {
            return Err(crate::net::rejected(response.status, &response.text));
        }
        bytes = bytes.saturating_add(response.text.len() as u64);
        if bytes > runtime.number("network.max_response_size") {
            return Err(Error::Provider(
                "model catalogue exceeds configured byte limit".into(),
            ));
        }
        let envelope = &response.body;
        let rows = envelope
            .get("data")
            .or_else(|| envelope.get("models"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Error::Provider("model catalogue has an unsupported response schema".into())
            })?;
        for raw in rows {
            let mut row = raw.clone();
            if row.get("id").is_none() {
                if let Some(name) = row.get("name").and_then(Value::as_str) {
                    row["id"] = json!(name.strip_prefix("models/").unwrap_or(name));
                }
            }
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Provider("model catalogue row is missing an ID".into()))?
                .to_string();
            if api == "ollama" {
                if let Some(context) = running
                    .iter()
                    .find(|m| m["name"].as_str() == Some(&id) || m["model"].as_str() == Some(&id))
                    .and_then(|m| m.get("context_length"))
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0)
                {
                    row["context_length"] = json!(context);
                }
            }
            if let Some(pricing) = row.get("pricing").and_then(Value::as_object) {
                for key in [
                    "prompt",
                    "completion",
                    "input_cache_read",
                    "input_cache_write",
                    "input_cache_write_1h",
                ] {
                    if let Some(v) = pricing.get(key) {
                        let n = v
                            .as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()));
                        if n.is_none_or(|n| !n.is_finite() || !(n * 1_000_000.0).is_finite()) {
                            // A catalogue row with an unrepresentable price is a
                            // bad row, not a bad catalogue. Drop the price so the
                            // row survives unpriced, rather than discarding the
                            // whole provider — one malformed number must never
                            // hide every model behind it.
                            row.as_object_mut().map(|o| o.remove("pricing"));
                            break;
                        }
                    }
                }
            }
            let Some(mut model) = catalogue_entry(&row) else {
                // One unparseable row must not abort the whole refresh. Skip it
                // and count it; the caller reports the count so a shrunken
                // catalogue is visible instead of silent.
                skipped += 1;
                continue;
            };
            model.fetched_at = Some(crate::store::now_ms());
            model.source_url = Some(config.base_url.clone());
            model.listed = Some(true);
            if api == "xai" {
                model.chat = Some(true);
                model.known.push("price".into());
            }
            if api == "mimo" {
                // No model bounds endpoint is available. MiMo chat parameter docs, 2026-09-14.
                model.metadata["parameter_limits"] =
                    json!({"temperature":{"minimum":0.0,"maximum":1.5}});
            }
            // An effort-support boolean without a published range is not an
            // allowed-values list. Preserve provider metadata without inventing
            // levels; the UI can still expose the advertised binary control.
            if model.default_effort.is_none()
                && !model.efforts.is_empty()
                && !model.route_default_level(api)
            {
                model.default_effort = model.efforts.first().cloned();
            }
            models.push(model);
        }
        if envelope.get("has_more").and_then(Value::as_bool) == Some(true) {
            let cursor = envelope
                .get("last_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::Provider("catalogue pagination is missing its cursor".into())
                })?;
            next = Some(format!(
                "{root}?after_id={}",
                crate::net::encode_component(cursor)
            ));
        } else if let Some(token) = envelope
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            next = Some(format!(
                "{root}?pageToken={}",
                crate::net::encode_component(token)
            ));
        } else if let Some(link) = envelope
            .pointer("/links/next")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let origin: ureq::http::Uri = root
                .parse()
                .map_err(|_| Error::invalid("invalid catalogue URL"))?;
            let target = if link.starts_with('/') {
                format!(
                    "{}://{}{}",
                    origin.scheme_str().unwrap_or("https"),
                    origin.authority().unwrap(),
                    link
                )
            } else {
                link.into()
            };
            let target_uri: ureq::http::Uri = target
                .parse()
                .map_err(|_| Error::Provider("invalid catalogue pagination URL".into()))?;
            if target_uri.scheme() != origin.scheme()
                || target_uri.authority() != origin.authority()
            {
                return Err(Error::Provider(
                    "catalogue pagination cannot change origin".into(),
                ));
            }
            next = Some(target);
        }
    }
    let mut checked = config.clone();
    checked.models = models.clone();
    checked.check()?;
    Ok((models, skipped))
}

/// One `/models` row, as much of it as the endpoint actually published.
///
/// Written against OpenRouter, which is the only one of these that publishes
/// enough to bill from: `context_length`, `top_provider.max_completion_tokens`,
/// and `pricing` **per token as strings**. Everything is optional because for
/// OpenAI, Groq and a local gateway all of it is missing, and a missing number
/// must stay missing — that is the difference between "not priced" and "$0.00".
fn catalogue_entry(row: &Value) -> Option<ModelInfo> {
    let id = row.get("id")?.as_str()?.to_string();
    let supported_parameters: Vec<String> = row
        .get("supported_parameters")
        .or_else(|| row.pointer("/capabilities/supported_parameters"))
        .or_else(|| row.pointer("/capabilities/parameters"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let has_reasoning_param = supported_parameters.iter().any(|p| {
        matches!(
            p.as_str(),
            "reasoning" | "reasoning_effort" | "thinking" | "effort" | "include_reasoning"
        )
    });

    let mut efforts: Vec<String> = row
        .pointer("/reasoning/supported_efforts")
        .or_else(|| row.pointer("/capabilities/supported_efforts"))
        .or_else(|| row.pointer("/capabilities/efforts"))
        .or_else(|| row.pointer("/capabilities/thinking_efforts"))
        .or_else(|| row.pointer("/reasoning/efforts"))
        .or_else(|| row.get("supported_efforts"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    super::providers::sort_efforts(&mut efforts);

    if let Some(levels) = row
        .pointer("/capabilities/effort")
        .and_then(Value::as_object)
    {
        efforts.extend(
            levels
                .iter()
                .filter(|(_, v)| v.get("supported").and_then(Value::as_bool) == Some(true))
                .map(|(k, _)| k.clone()),
        );
        super::providers::sort_efforts(&mut efforts);
    }
    let explicit_reasoning = row
        .pointer("/capabilities/reasoning")
        .and_then(Value::as_bool)
        .or_else(|| {
            row.pointer("/capabilities/thinking/supported")
                .and_then(Value::as_bool)
        })
        .or_else(|| row.get("thinking").and_then(Value::as_bool))
        .or_else(|| row.get("reasoning").and_then(Value::as_bool));
    // 9router's `capabilities.thinkingCanDisable: false` is not mandatory
    // thinking: the router reads `none` for such a route as its least level
    // (`ModelInfo::thinking_off_asks_least`), so Off stays a request it takes.
    let thinking_required = explicit_reasoning != Some(false)
        && (row.pointer("/reasoning/mandatory").and_then(Value::as_bool) == Some(true)
            || row
                .pointer("/capabilities/thinking/mandatory")
                .and_then(Value::as_bool)
                == Some(true)
            || row
                .pointer("/capabilities/reasoningMandatory")
                .and_then(Value::as_bool)
                == Some(true)
            || row.get("thinking_required").and_then(Value::as_bool) == Some(true)
            || row.get("reasoning_mandatory").and_then(Value::as_bool) == Some(true));

    let cap_reasoning = row
        .pointer("/capabilities/reasoning")
        .and_then(Value::as_bool)
        == Some(true)
        || row
            .pointer("/capabilities/thinkingEffortSupported")
            .and_then(Value::as_bool)
            == Some(true)
        || row
            .pointer("/capabilities/thinking/supported")
            .and_then(Value::as_bool)
            == Some(true)
        || row
            .pointer("/capabilities/thinkingFormat")
            .is_some_and(|v| !v.is_null())
        || row
            .get("reasoning")
            .is_some_and(|r| !r.is_null() && r != false)
        || row
            .get("thinking")
            .is_some_and(|t| !t.is_null() && t != false)
        || row.pointer("/reasoning/supported_efforts").is_some();

    let thinking = explicit_reasoning.unwrap_or(
        has_reasoning_param || cap_reasoning || !efforts.is_empty() || thinking_required,
    );
    if !thinking {
        efforts.clear();
    }
    let mut known = Vec::new();
    for (field, paths) in [
        (
            "supported_parameters",
            vec![
                "/supported_parameters",
                "/capabilities/supported_parameters",
                "/capabilities/parameters",
            ],
        ),
        (
            "thinking",
            vec![
                "/capabilities/reasoning",
                "/capabilities/thinking",
                "/reasoning",
                "/thinking",
                "/supported_parameters",
            ],
        ),
        (
            "efforts",
            vec![
                "/reasoning/supported_efforts",
                "/capabilities/supported_efforts",
                "/capabilities/effort",
                "/supported_efforts",
                "/capabilities/thinkingEffortSupported",
            ],
        ),
        ("price", vec!["/pricing"]),
        (
            "context",
            vec![
                "/context_length",
                "/context_window",
                "/max_context_length",
                "/max_context_tokens",
                "/capabilities/contextWindow",
            ],
        ),
        (
            "max_output",
            vec![
                "/top_provider/max_completion_tokens",
                "/max_completion_tokens",
                "/max_tokens",
                "/max_output_tokens",
                "/capabilities/maxOutput",
            ],
        ),
        (
            "modalities",
            vec![
                "/architecture/input_modalities",
                "/input_modalities",
                "/capabilities",
            ],
        ),
    ] {
        if paths.iter().any(|p| row.pointer(p).is_some()) {
            known.push(field.into());
        }
    }
    let tools = row
        .pointer("/capabilities/tools")
        .and_then(Value::as_bool)
        .or_else(|| {
            row.pointer("/capabilities/function_calling")
                .and_then(Value::as_bool)
        })
        .or_else(|| {
            row.get("supported_parameters")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|p| p == "tools"))
        });
    let chat = row
        .pointer("/capabilities/completion_chat")
        .and_then(Value::as_bool)
        .or_else(|| {
            row.get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|m| m == "generateContent"))
        })
        .or_else(|| {
            row.pointer("/architecture/output_modalities")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|m| m == "text"))
        });

    let model = ModelInfo {
        known,
        tools,
        chat,
        input_limit: row
            .get("inputTokenLimit")
            .or_else(|| row.get("max_input_tokens"))
            .and_then(Value::as_u64),
        default_effort: row
            .pointer("/reasoning/default_effort")
            .and_then(Value::as_str)
            .map(String::from),
        thinking_modes: row
            .pointer("/capabilities/thinking/types")
            .and_then(Value::as_object)
            .map(|a| {
                a.iter()
                    .filter(|(_, v)| v.get("supported").and_then(Value::as_bool) == Some(true))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default(),
        metadata: row.clone(),
        id,
        max_output: row
            .pointer("/top_provider/max_completion_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| row.get("max_completion_tokens").and_then(|v| v.as_u64()))
            .or_else(|| {
                row.pointer("/capabilities/maxOutput")
                    .and_then(|v| v.as_u64())
            })
            .or_else(|| row.get("max_tokens").and_then(|v| v.as_u64()))
            .or_else(|| row.get("max_output_tokens").and_then(|v| v.as_u64()))
            .or_else(|| row.get("outputTokenLimit").and_then(Value::as_u64)),
        context: row
            .get("context_length")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                row.pointer("/capabilities/contextWindow")
                    .and_then(|v| v.as_u64())
            })
            .or_else(|| row.get("context_window").and_then(|v| v.as_u64()))
            .or_else(|| row.get("max_context_length").and_then(|v| v.as_u64()))
            .or_else(|| row.get("max_context_tokens").and_then(|v| v.as_u64())),
        thinking,
        reasoning_enabled: None,
        reasoning_options: None,
        thinking_required,
        efforts,
        supported_parameters,
        price: per_million(row),
        modalities: row
            .pointer("/architecture/input_modalities")
            .or_else(|| row.get("input_modalities"))
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|m| m.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_else(|| {
                if !row.get("capabilities").is_some_and(Value::is_object) {
                    return Vec::new();
                }
                let mut inputs = vec!["text".into()];
                let caps = &row["capabilities"];
                if caps["image_input"]["supported"].as_bool() == Some(true)
                    || caps["vision"].as_bool() == Some(true)
                {
                    inputs.push("image".into());
                }
                if caps["pdf_input"]["supported"].as_bool() == Some(true)
                    || caps["pdf"].as_bool() == Some(true)
                {
                    inputs.push("file".into());
                }
                if caps["audioInput"].as_bool() == Some(true) {
                    inputs.push("audio".into());
                }
                if caps["videoInput"].as_bool() == Some(true) {
                    inputs.push("video".into());
                }
                inputs
            }),
        ..Default::default()
    };
    Some(model)
}

/// Prices, converted from per-token to per-million.
///
/// All three or nothing: a row with an input rate and no output rate cannot
/// price a turn, and a half-filled `Price` would be quietly wrong on every
/// answer rather than loudly absent on one screen.
fn per_million(row: &Value) -> Option<Price> {
    let rate = |field: &str| -> Option<f64> {
        let raw = row.pointer(&format!("/pricing/{field}"))?;
        let n = match raw {
            Value::String(s) => s.parse::<f64>().ok()?,
            other => other.as_f64()?,
        };
        let converted = n * 1_000_000.0;
        (converted.is_finite() && converted >= 0.0).then_some(converted)
    };
    let mut overrides = Vec::new();
    if let Some(tiers) = row.pointer("/pricing/overrides").and_then(Value::as_array) {
        for tier in tiers {
            let threshold = tier.get("min_prompt_tokens")?.as_u64()?;
            let mut rates = std::collections::BTreeMap::new();
            for (wire, field) in [
                ("prompt", "input"),
                ("completion", "output"),
                ("input_cache_read", "cached"),
                ("input_cache_write", "cache_write"),
                ("input_cache_write_1h", "cache_write_1h"),
            ] {
                if let Some(raw) = tier.get(wire) {
                    let rate = raw
                        .as_f64()
                        .or_else(|| raw.as_str().and_then(|s| s.parse::<f64>().ok()))?
                        * 1_000_000.0;
                    if !rate.is_finite() || rate < 0.0 {
                        return None;
                    }
                    rates.insert(field.into(), rate);
                }
            }
            overrides.push(super::providers::RateOverride {
                min_prompt_tokens: threshold,
                inclusive: true,
                rates,
            });
        }
    }
    if row.get("prompt_text_token_price").is_some() {
        let input = row.get("prompt_text_token_price")?.as_f64()? / 10_000.0;
        let output = row.get("completion_text_token_price")?.as_f64()? / 10_000.0;
        let cached = row
            .get("cached_prompt_text_token_price")
            .and_then(Value::as_f64)
            .map(|n| n / 10_000.0)
            .unwrap_or(input);
        if let Some(threshold) = row
            .get("long_context_threshold")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            let rates = [
                ("prompt_text_token_price_long_context", "input"),
                ("completion_text_token_price_long_context", "output"),
                ("cached_prompt_text_token_price_long_context", "cached"),
            ]
            .into_iter()
            .filter_map(|(wire, field)| {
                row.get(wire)
                    .and_then(Value::as_f64)
                    .filter(|n| *n > 0.0)
                    .map(|n| (field.into(), n / 10_000.0))
            })
            .collect();
            overrides.push(super::providers::RateOverride {
                min_prompt_tokens: threshold,
                inclusive: true,
                rates,
            });
        }
        return Some(Price {
            input,
            output,
            cached,
            overrides,
            ..Default::default()
        });
    }
    let input = rate("prompt")?;
    let output = rate("completion")?;
    Some(Price {
        input,
        cached: rate("input_cache_read").unwrap_or(input),
        output,
        overrides,
        cache_write: rate("input_cache_write"),
        cache_write_1h: rate("input_cache_write_1h"),
        ..Default::default()
    })
}

/// Show the request that *would* be sent, and send nothing.
///
/// This exists because verifying a provider must not cost the user money. A
/// preset is right or wrong about four things — the URL, the auth header, any
/// extra headers, and how thinking is switched on — and all four are visible
/// here, against the same body builder the live path uses, for zero credits.
/// The key is masked: the point is to be pasteable into a bug report.
pub fn dryrun(settings: &Shared, name: &str, model: Option<&str>) -> Result<Value> {
    let (config, headers) = {
        let s = panic::lock(settings);
        let c = s
            .providers
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?
            .clone();
        let h = s.headers_for(&c)?;
        (c, h)
    };
    let chosen = model
        .map(|m| m.to_string())
        .or_else(|| config.default_model().map(|m| m.id.clone()))
        .ok_or_else(|| {
            Error::invalid(format!(
                "no model to show — pass one, or run `rune -provider models {name}` first"
            ))
        })?;

    let mut settings_for = ModelSettings::for_provider(&config);
    let m_opt = config.model(&chosen);
    let api = config.api_kind();
    // What a new session on the model would start with (`starts_thinking`).
    let chosen_thinks = m_opt
        .map(|m| m.starts_thinking(api))
        .unwrap_or(config.thinking != Thinking::None);
    let has_effort = m_opt.is_some_and(|m| !m.efforts.is_empty());
    settings_for.thinking = chosen_thinks && config.thinking != Thinking::None;
    settings_for.effort =
        (settings_for.thinking && has_effort && matches!(config.thinking, Thinking::Effort))
            .then(|| m_opt.and_then(|m| m.starting_effort(api)))
            .flatten();
    settings_for.model = chosen;
    let request = crate::provider::Request {
        control: Default::default(),
        system: "<system prompt>".into(),
        messages: vec![crate::provider::Message::user("<your message>")],
        tools: Vec::new(),
        settings: settings_for,
    };
    let (url, body) = crate::provider::live::shape(&request, &config)?;
    let shown: Vec<Value> = headers
        .iter()
        .map(|(k, v)| {
            // The scheme stays readable and only the credential is hidden:
            // `Bearer sk-…9f2c` is checkable at a glance, `Bear…9f2c` is not.
            let safe = match k.eq_ignore_ascii_case(&config.auth.header) {
                true => format!(
                    "{}{}",
                    config.auth.prefix,
                    mask(v.trim_start_matches(&config.auth.prefix))
                ),
                false => v.clone(),
            };
            json!({ "header": k, "value": safe })
        })
        .collect();
    Ok(json!({
        "provider": name,
        "url": url,
        "headers": shown,
        "body": body,
        // Part of "what would be sent" in every sense that matters: a call that
        // is cut off at twenty seconds behaves differently from one that is not,
        // and the difference is invisible in the body.
        "timeout": config.timeout,
        "sent": false,
    }))
}

/// Verify provider connectivity and credentials.
///
/// When a specific `model` is passed, runs a real minimal completion against that model.
/// When `model` is None, verifies connectivity and credentials live against the endpoint
/// without guessing arbitrary models or charging unnecessary usage.
pub fn test(settings: &Shared, name: &str, model: Option<&str>) -> Result<Value> {
    let config = {
        let s = panic::lock(settings);
        s.providers
            .get(name)
            .ok_or_else(|| Error::NotFound(format!("provider {name}")))?
            .clone()
    };
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        probe(settings, &config, m)?;
        return Ok(
            json!({ "provider": name, "model": m, "ok": true, "coverage": "credential_endpoint_text_only" }),
        );
    }
    let (headers, runtime) = {
        let s = panic::lock(settings);
        (s.headers_for(&config)?, s.config.clone())
    };
    let result = match fetch_catalogue(&config, &headers, &runtime, None) {
        Ok((models, _)) => Ok(models.len()),
        Err(Error::Rejected {
            status: 404 | 405, ..
        }) => {
            // Models catalogue endpoint not implemented on this endpoint/proxy,
            // but credentials and endpoint reachability succeeded without 401/403.
            Ok(0)
        }
        Err(e) => Err(e),
    };
    let count = result?;
    Ok(json!({
        "provider": name,
        "models_count": count,
        "ok": true,
        "coverage": "credential_endpoint_text_only",
    }))
}

/// The one request that decides whether a configuration is real.
///
/// Deliberately tiny and deliberately non-streaming: this is a reachability and
/// credential check, and the cheapest true answer is the best one.
fn probe(settings: &Shared, config: &ProviderConfig, model: &str) -> Result<()> {
    let headers = panic::lock(settings).headers_for(config)?;
    let mut generation = ModelSettings::for_provider(config);
    generation.model = model.into();
    generation.thinking = config.model(model).is_some_and(|m| m.thinking_required);
    generation.effort = None;
    generation.max_output = Some(if generation.thinking { 4096 } else { 128 });
    let req = crate::provider::Request {
        control: Default::default(),
        system: String::new(),
        messages: vec![crate::provider::Message::user(
            "Reply with the single word: ok",
        )],
        tools: Vec::new(),
        settings: generation,
    };
    let (_, mut body) = crate::provider::live::shape(&req, config)?;
    body["stream"] = json!(false);
    body.as_object_mut().unwrap().remove("stream_options");
    let response = {
        let settings = panic::lock(settings).config.clone();
        Http::configured(
            &settings,
            Some(settings.duration("network.probe_timeout")),
            Default::default(),
        )
    }
    .post_json(&config.endpoint(), &headers, &body)?;
    if !response.ok() {
        return Err(crate::net::rejected(response.status, &response.text));
    }
    let values = if response.body.is_null() && response.text.contains("data:") {
        response
            .text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| !data.is_empty() && *data != "[DONE]")
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::Provider("provider probe returned malformed stream data".into()))?
    } else {
        vec![response.body]
    };
    let useful = values.iter().any(|value| {
        value.pointer("/choices/0").is_some()
            || value.pointer("/content/0").is_some()
            || [
                "/choices/0/message/content",
                "/choices/0/delta/content",
                "/content/0/text",
                "/delta/text",
            ]
            .iter()
            .any(|path| {
                value.pointer(path).is_some_and(|v| {
                    v.as_str().is_some_and(|s| !s.trim().is_empty())
                        || v.as_array().is_some_and(|a| {
                            a.iter().any(|p| {
                                p.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| !s.trim().is_empty())
                            })
                        })
                })
            })
    });
    if !useful {
        return Err(Error::Provider(
            "provider answered without usable text in the expected response format".into(),
        ));
    }
    Ok(())
}

/// The settings screen for one session: the values in force, what this provider
/// will ignore, and the ranges every knob accepts.
pub fn describe(settings: &Shared, current: &ModelSettings, catalogue: bool) -> Value {
    let s = panic::lock(settings);
    let config = s.providers.get(&current.provider);
    let model = config.and_then(|c| c.model(&current.model));
    let raw_context = model.and_then(|m| m.context);
    let ceiling = model.and_then(|m| m.max_output);
    let auto_smart = crate::settings::model::smart_max_output(raw_context, ceiling);
    let max_output = Some(current.max_output.unwrap_or(auto_smart));
    let input_limit = model.and_then(|m| m.input_limit);
    let usable_context = raw_context.or(input_limit).map(|w| {
        crate::session::compact::budget_usable(raw_context, max_output, w as usize, input_limit)
            as u64
    });
    json!({
        "settings": current,
        "configuration": s.config.redacted(),
        // Who serves the web tools under this configuration: what a settings
        // screen shows beside `web_*.provider` the moment it changes.
        "web": {
            "search": crate::web::status(&s, super::services::Service::Search),
            "fetch": crate::web::status(&s, super::services::Service::Fetch),
        },
        "provider": current.provider,
        "dialect": config.map(|c| c.dialect).unwrap_or_default(),
        "catalogue_complete": catalogue,
        "models": if catalogue {
            config.map(|c| c.models.clone()).unwrap_or_default()
        } else {
            model.cloned().into_iter().collect::<Vec<_>>()
        },
        "max_output_ceiling": model.and_then(|m| m.max_output),
        "context": raw_context,
        "input_limit":input_limit,
        "context_usable": usable_context,
        "price": model.and_then(|m| m.price.as_ref()),
        "thinking_required": model.map(|m| m.thinking_required || m.reasoning_options.as_ref().is_some_and(|v| !v.iter().any(|s| s == "off"))).unwrap_or(false),
        "suppressed": config
            .map(|c| current.suppressed(c))
            .unwrap_or_default()
            .into_iter()
            .map(|x| json!({ "key": x.key, "reason": x.reason, "permanent": x.permanent }))
            .collect::<Vec<_>>(),
        "knobs": super::KNOBS
            .iter()
            .map(|k| {
                let (kind, min, max, step, choices) = match k.kind {
                    super::KnobKind::Float { .. } => {let (min,max,step)=k.bounds(model).expect("float bounds");("float", Some(min), Some(max), Some(step), None)},
                    super::KnobKind::Tokens => ("tokens", None, None, None, None),
                    super::KnobKind::Bool => ("bool", None, None, None, None),
                    super::KnobKind::Choice(c) => ("choice", None, None, None, Some(c)),
                    super::KnobKind::Text => ("text", None, None, None, None),
                };
                let mut choices_val: Option<Vec<Value>> = choices.map(|c| c.iter().map(|s| json!(*s)).collect());
                if k.key == "thinking" {
                    let thinking_required = model.map(|m| m.thinking_required).unwrap_or(false);
                    let choices = if thinking_required {
                        vec!["on".to_string()]
                    } else {
                        vec!["off".to_string(), "on".to_string()]
                    };
                    choices_val = Some(choices.into_iter().map(Value::from).collect());
                } else if k.key == "effort" {
                    let mut list = Vec::new();
                    let thinking_required = model.map(|m| m.thinking_required || m.reasoning_options.as_ref().is_some_and(|v| !v.iter().any(|e| e == "off"))).unwrap_or(false);
                    if !thinking_required && !config.is_some_and(|p|current.suppressed(p).iter().any(|why|why.key=="thinking" && why.permanent)) {
                        list.push("off".to_string());
                    }
                    if let Some(m) = model {
                        if m.route_default_level(config.map_or("", |c| c.api_kind())) {
                            list.push(super::providers::ROUTE_DEFAULT.to_string());
                        }
                        let mut effs = m.reasoning_options.as_ref().unwrap_or(&m.efforts).clone();
                        super::providers::sort_efforts(&mut effs);
                        list.extend(effs);
                    }
                    choices_val = (list.len() > 1).then(|| list.into_iter().map(Value::from).collect());
                }
                json!({
                    "key": k.key, "label": k.label, "help": k.help,
                    "kind": kind, "min": min, "max": max, "step": step, "choices": choices_val,
                })
            })
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Arc::new(Mutex::new(Settings::empty()))
    }

    /// A configuration with a real directory behind it, so writing a key
    /// actually happens and the rollback path can be reached.
    fn on_disk(tag: &str) -> Shared {
        let dir = std::env::temp_dir().join(format!("rune-manage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Mutex::new(
            Settings::load(&dir, &dir.join("rune.db")).unwrap(),
        ))
    }

    /// Owner, 2026-10-16: what every screen shows of a key is what is on
    /// disk — a key whose file is gone is never shown as there, and a store
    /// that cannot be opened says why, wherever a key is shown.
    #[test]
    fn every_screen_reads_one_key_state_from_the_disk() {
        let s = on_disk("key-states");
        let home = panic::lock(&s).home.clone();
        panic::lock(&s)
            .providers
            .add(preset("mimo").unwrap().build("unused"))
            .unwrap();
        let mimo = |l: &Value| {
            l["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == "mimo")
                .unwrap()
                .clone()
        };
        let l = list(&s);
        assert_eq!(l["credentials"]["state"], "absent");
        assert_eq!(mimo(&l)["key_state"], "missing");
        assert!(mimo(&l)["key_note"]
            .as_str()
            .unwrap()
            .contains("secrets.vault"));

        panic::lock(&s)
            .secrets
            .set("mimo", "sk-present-key-0001")
            .unwrap();
        let l = list(&s);
        assert_eq!(l["credentials"]["state"], "ready");
        assert_eq!(mimo(&l)["key_state"], "present");

        // Someone removes the store file: the next read says the key is gone.
        std::fs::remove_file(home.join(super::super::secrets::VAULT_FILE)).unwrap();
        panic::lock(&s).reload_secrets();
        let l = list(&s);
        assert_eq!(mimo(&l)["key_state"], "missing");
        assert_eq!(mimo(&l)["has_key"], false);
        assert!(keys(&s, "mimo")
            .unwrap_err()
            .to_string()
            .contains("Key missing"));

        // A store whose master key is gone is unavailable, with its cause.
        panic::lock(&s)
            .secrets
            .set("mimo", "sk-present-key-0002")
            .unwrap();
        std::fs::remove_file(home.join("master.key")).unwrap();
        panic::lock(&s).reload_secrets();
        let l = list(&s);
        assert_eq!(l["credentials"]["state"], "unavailable");
        assert_eq!(l["credentials"]["cause"], "key_missing");
        assert_eq!(mimo(&l)["key_state"], "unavailable");
        assert!(mimo(&l)["key_note"]
            .as_str()
            .unwrap()
            .contains("master.key"));
        assert!(add_key(&s, "mimo", None, "sk-refused-key-0003").is_err());
        assert!(
            !home.join("master.key").exists(),
            "no key made beside the store"
        );

        // The person starts a new store: the pair is kept aside, keys missing.
        let out = new_credential_store(&s).unwrap();
        let aside = std::path::PathBuf::from(out["set_aside"].as_str().unwrap());
        assert!(aside.join(super::super::secrets::VAULT_FILE).is_file());
        let l = list(&s);
        assert_eq!(l["credentials"]["state"], "absent");
        assert_eq!(mimo(&l)["key_state"], "missing");
    }

    #[test]
    fn an_account_save_conflict_restores_the_old_key_and_never_holds_the_settings_lock() {
        let s = on_disk("account-conflict");
        let home = panic::lock(&s).home.clone();
        panic::lock(&s)
            .secrets
            .restore(
                "tavily",
                vec![super::super::secrets::SecretItem::Plain(
                    "old-fixture-key".into(),
                )],
            )
            .unwrap();
        // The projection was loaded before this independent process's edit.
        super::super::configuration::Configuration::edit(
            &home,
            Some("services"),
            Some(json!({"list":[{"name":"exa","api":"exa"}]})),
            false,
            None,
        )
        .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let cloned = Arc::clone(&s);
        let worker = std::thread::spawn(move || {
            let result = super::super::connections::account(
                &cloned,
                super::super::connections::Account {
                    name: "tavily".into(),
                    api: None,
                    base_url: None,
                    fetch_url: None,
                    key_name: None,
                    key: Some("new-fixture-key".into()),
                },
            );
            send.send(result.is_err()).unwrap();
        });
        assert!(receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap());
        worker.join().unwrap();
        assert_eq!(
            panic::lock(&s).secrets.get("tavily"),
            Some("old-fixture-key")
        );
        assert!(std::fs::read_to_string(home.join("rune.toml"))
            .unwrap()
            .contains("name = \"exa\""));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn catalogue_facts_override_names_and_keep_native_modes_and_free_write_rates() {
        let model =
            catalogue_entry(&json!({"id":"fixture-thinking","capabilities":{"reasoning":false}}))
                .unwrap();
        assert!(!model.thinking);
        assert!(!model.thinking_required);
        let native=catalogue_entry(&json!({"id":"native","max_context_length":32768,"capabilities":{"thinking":{"supported":true,"types":{"adaptive":{"supported":true},"enabled":{"supported":false}}},"effort":{"low":{"supported":true},"high":{"supported":true}}},"pricing":{"prompt":"0","completion":"0","input_cache_write":"0"}})).unwrap();
        assert_eq!(native.context, Some(32768));
        assert_eq!(native.thinking_modes, vec!["adaptive"]);
        assert_eq!(native.efforts, vec!["low", "high"]);
        assert_eq!(native.price.unwrap().cache_write, Some(0.0));
        assert!(per_million(&json!({"pricing":{"prompt":"NaN","completion":"0"}})).is_none());
        let router = catalogue_entry(&json!({"id":"router-binary","capabilities":{"reasoning":true,"thinkingEffortSupported":false,"thinkingCanDisable":true,"thinkingFormat":"qwen","thinkingRange":null}})).unwrap();
        assert!(router.thinking);
        assert!(!router.thinking_required);
        // `thinkingEffortSupported: false` says the upstream has no effort
        // parameter of its own; the router still turns a level into that
        // upstream's control (qwen: a thinking budget). A row's levels are
        // settled by its provider (`ProviderConfig::settle_thinking`), never
        // invented here, and no level is its default.
        assert!(router.efforts.is_empty());
        assert_eq!(router.default_effort, None);
        assert!(router.route_default_level("9router"));
        // A route that cannot stop thinking is not mandatory: the router
        // reads `none` there as its least level, so Off stays a choice.
        let fixed = catalogue_entry(&json!({"id":"router-fixed","capabilities":{"reasoning":true,"thinkingEffortSupported":false,"thinkingCanDisable":false,"thinkingFormat":"gemini-level","thinkingRange":null}})).unwrap();
        assert!(fixed.thinking && !fixed.thinking_required);
        assert!(fixed.thinking_off_asks_least());
        assert!(
            router.known.iter().any(|field| field == "efforts"),
            "the published empty effort capability must be retained"
        );
    }

    #[test]
    fn the_listing_names_every_preset_and_never_a_key() {
        let s = shared();
        panic::lock(&s)
            .secrets
            .keys_for_test("mimo", "sk-listing-must-hide-me");
        let out = list(&s);
        let text = out.to_string();
        assert!(text.contains("mimo"), "presets are offered");
        assert!(
            !text.contains("sk-listing-must-hide-me"),
            "a key must never be listed"
        );
    }

    #[test]
    fn an_unknown_provider_without_a_base_url_is_refused_before_any_request() {
        let e = add(&shared(), "mystery", None, None, None, Some("sk-x"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("base URL"), "{e}");
    }

    #[test]
    fn a_key_of_the_wrong_shape_is_refused_before_any_request() {
        let e = add(
            &shared(),
            "mimo",
            Some("mimo"),
            None,
            None,
            Some("ghp_wrong"),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("does not look like"), "{e}");
    }

    /// Rolling back to the *active* key would have quietly destroyed the other
    /// three, and the user would not find out until their next rate limit.
    #[test]
    fn a_failed_add_puts_back_the_whole_ring_not_just_the_front_of_it() {
        let s = on_disk("ring-rollback");
        {
            let mut held = panic::lock(&s);
            held.secrets.set("mimo", "sk-good-one-000001").unwrap();
            held.secrets.append("mimo", "sk-good-two-000002").unwrap();
            held.secrets.append("mimo", "sk-good-three-0003").unwrap();
        }
        let mut broken = preset("mimo").unwrap().build("sk-x");
        broken.base_url = "https://127.0.0.1:1/v1".into();
        panic::lock(&s).providers.list.push(broken);

        let failed = add(
            &s,
            "mimo",
            Some("mimo"),
            Some("https://127.0.0.1:1/v1"),
            None,
            Some("sk-bad-key-000002"),
            Some("m"),
        );
        assert!(failed.is_err());
        assert_eq!(
            panic::lock(&s).secrets.ring("mimo"),
            vec![
                "sk-good-one-000001",
                "sk-good-two-000002",
                "sk-good-three-0003"
            ],
        );
    }

    /// The whole catalogue import, against the shape OpenRouter actually
    /// publishes — parsed offline, so this costs nothing and can never be
    /// wrong about a live account's balance.
    #[test]
    fn an_openrouter_row_becomes_a_priced_model_with_per_million_rates() {
        let row = json!({
            "id": "anthropic/claude-sonnet-4.5",
            "context_length": 200_000,
            "top_provider": { "max_completion_tokens": 64_000 },
            "pricing": { "prompt": "0.000003", "completion": "0.000015", "input_cache_read": "0.0000003" },
            "supported_parameters": ["tools", "reasoning"],
            "architecture": { "input_modalities": ["text", "image"] }
        });
        let m = catalogue_entry(&row).unwrap();
        assert_eq!(m.id, "anthropic/claude-sonnet-4.5");
        assert_eq!(m.context, Some(200_000));
        assert_eq!(m.max_output, Some(64_000));
        assert!(m.thinking);
        assert!(m.accepts("image"));
        assert!(!m.accepts("video"));
        for enabled in [true, false] {
            let native = catalogue_entry(&json!({
                "id": "native-model",
                "max_input_tokens": 200_000,
                "max_tokens": 64_000,
                "capabilities": {
                    "image_input": {"supported": enabled},
                    "pdf_input": {"supported": enabled},
                    "thinking": {"supported": true}
                }
            }))
            .unwrap();
            assert_eq!(native.accepts("image"), enabled);
            assert_eq!(native.accepts("file"), enabled);
            assert!(native.accepts("text"));
            assert!(native.thinking);
            assert_eq!(native.context, None);
            assert_eq!(native.input_limit, Some(200_000));
            assert_eq!(native.max_output, Some(64_000));
        }
        let p = m.price.unwrap();
        assert!(
            (p.input - 3.0).abs() < 1e-9,
            "per-token must become per-million: {}",
            p.input
        );
        assert!((p.output - 15.0).abs() < 1e-9);
        assert!((p.cached - 0.3).abs() < 1e-9);
    }

    /// Most endpoints answer with an id and nothing else. Every unknown stays
    /// unknown — this is the test that stops a guessed window coming back.
    #[test]
    fn a_bare_id_becomes_a_model_with_nothing_claimed_about_it() {
        let m = catalogue_entry(&json!({ "id": "gpt-5" })).unwrap();
        assert_eq!(m.context, None);
        assert_eq!(m.max_output, None);
        assert_eq!(m.price, None);
        assert!(!m.thinking);
        assert!(catalogue_entry(&json!({ "object": "model" })).is_none());
    }

    #[test]
    fn a_free_model_retains_known_zero_pricing() {
        let row = json!({ "id": "x/free", "pricing": { "prompt": "0", "completion": "0" } });
        assert_eq!(catalogue_entry(&row).unwrap().price.unwrap().input, 0.0);
    }

    /// A row missing half its rates cannot price a turn, so it prices none.
    #[test]
    fn half_a_price_is_no_price() {
        let row = json!({ "id": "x/partial", "pricing": { "prompt": "0.000001" } });
        assert_eq!(catalogue_entry(&row).unwrap().price, None);
    }

    #[test]
    fn a_provider_timeout_is_range_checked_and_can_be_cleared_again() {
        // Built directly, never through `add`: `add` probes the endpoint for
        // real, and `ollama` resolves to a machine that may well be running.
        // A unit test must not depend on that, and must not call it.
        let s = on_disk("timeout");
        panic::lock(&s)
            .providers
            .add(preset("ollama").unwrap().build(""))
            .unwrap();

        assert!(set_timeout(&s, "ollama", Some(30)).is_ok());
        assert_eq!(
            panic::lock(&s).providers.get("ollama").unwrap().timeout,
            Some(30)
        );

        // The range lives with the field, so every route to it is checked.
        assert!(set_timeout(&s, "ollama", Some(1)).is_err());
        assert!(set_timeout(&s, "ollama", Some(99_999)).is_err());
        assert_eq!(
            panic::lock(&s).providers.get("ollama").unwrap().timeout,
            Some(30),
            "a refused value must not have been written"
        );

        set_timeout(&s, "ollama", None).unwrap();
        assert_eq!(
            panic::lock(&s).providers.get("ollama").unwrap().timeout,
            None
        );
        assert!(set_timeout(&s, "nothing-here", Some(30)).is_err());
    }

    #[test]
    fn a_listed_key_shows_enough_to_recognise_it_and_never_enough_to_use_it() {
        let s = on_disk("masked");
        panic::lock(&s)
            .secrets
            .set("serper", "abcdefgh12345678")
            .unwrap();
        let out = keys(&s, "serper").unwrap();
        let shown = out["keys"][0]["masked"].as_str().unwrap();
        assert_eq!(shown, "abcdefgh••••••••");
        assert!(!serde_json::to_string(&out)
            .unwrap()
            .contains("abcdefgh12345678"));
        assert!(keys(&s, "nothing-here").is_err());
    }

    #[test]
    fn a_preset_that_needs_a_key_refuses_an_empty_one() {
        let e = add(&shared(), "mimo", Some("mimo"), None, None, Some(""), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("needs an API key"), "{e}");
    }

    #[test]
    fn a_failed_add_leaves_the_key_that_was_already_working() {
        // Re-adding a provider with a mistyped key must not cost the user the
        // key they had. The probe cannot reach anything in a test, so the add
        // fails for a real reason and the rollback is what is being checked.
        let s = on_disk("rollback");
        panic::lock(&s)
            .secrets
            .set("mimo", "sk-the-good-key-000001")
            .unwrap();
        let mut broken = preset("mimo").unwrap().build("sk-x");
        broken.base_url = "https://127.0.0.1:1/v1".into();
        panic::lock(&s).providers.list.push(broken);

        let failed = add(
            &s,
            "mimo",
            Some("mimo"),
            Some("https://127.0.0.1:1/v1"),
            None,
            Some("sk-bad-key-000002"),
            Some("m"),
        );
        let message = failed.unwrap_err().to_string();
        assert!(
            message.contains("could not reach"),
            "the add must fail at the probe, not before it — otherwise the rollback is untested: {message}"
        );
        assert_eq!(
            panic::lock(&s).secrets.get("mimo"),
            Some("sk-the-good-key-000001"),
            "the working key must survive a failed attempt"
        );
    }

    #[test]
    fn removing_a_provider_that_was_never_added_says_so() {
        assert!(remove(&shared(), "ghost").is_err());
    }

    /// Owner, 2026-10-15: a model whose reasoning nobody published (an id
    /// added by hand) said "Unsupported / unknown" in Alt+C › Reasoning and
    /// could not be configured at all. Unknown is the person's to settle:
    /// they are offered what the provider's adapter reads, and choosing a
    /// level says the model thinks.
    #[test]
    fn a_model_of_unknown_reasoning_can_be_configured_in_alt_c() {
        let s = on_disk("unknown-reasoning");
        {
            let mut held = panic::lock(&s);
            held.providers
                .add(
                    super::super::providers::preset("9router")
                        .unwrap()
                        .build(""),
                )
                .unwrap();
            held.providers
                .remember_model("9router", "new/model")
                .unwrap();
        }
        let stub = panic::lock(&s)
            .providers
            .get("9router")
            .unwrap()
            .model("new/model")
            .unwrap()
            .clone();
        assert_eq!(stub.reasoning_support(), None, "nobody published it");
        let mut offered = vec!["off".to_string()];
        offered.extend(super::super::providers::ROUTER_LEVELS.map(String::from));
        assert_eq!(stub.reasoning_choices("9router"), offered);
        assert_eq!(stub.reasoning_choices("openrouter"), ["off", "on"]);

        let mut chosen = stub.clone();
        chosen.reasoning_options = Some(["off", "low", "max"].map(String::from).to_vec());
        chosen.reasoning_enabled = Some(true);
        chosen.default_effort = Some("low".into());
        configure_model(&s, "9router", chosen).unwrap();
        let configured = panic::lock(&s)
            .providers
            .get("9router")
            .unwrap()
            .model("new/model")
            .unwrap()
            .clone();
        assert!(configured.thinking, "choosing a level says it thinks");
        assert_eq!(configured.reasoning_support(), Some(true));
        assert_eq!(configured.efforts, ["low", "max"]);
        assert!(configured.starts_thinking("9router"));
        assert_eq!(
            configured.starting_effort("9router").as_deref(),
            Some("low")
        );

        // Off alone says nothing about whether it can think.
        let mut off = stub;
        off.id = "other/model".into();
        panic::lock(&s)
            .providers
            .remember_model("9router", "other/model")
            .unwrap();
        off.reasoning_options = Some(vec!["off".into()]);
        off.reasoning_enabled = Some(false);
        configure_model(&s, "9router", off).unwrap();
        let other = panic::lock(&s)
            .providers
            .get("9router")
            .unwrap()
            .model("other/model")
            .unwrap()
            .clone();
        assert!(!other.thinking);
        assert!(!other.starts_thinking("9router"));
    }

    #[test]
    fn describe_reports_the_ranges_and_what_this_provider_will_ignore() {
        let s = shared();
        let mut config = preset("mimo").unwrap().build("sk-x");
        config.models.push(ModelInfo {
            id: "mimo-v2.5-pro".into(),
            max_output: Some(131_072),
            context: Some(1_050_000),
            thinking: true,
            ..Default::default()
        });
        let current = ModelSettings::for_provider(&config);
        for index in 0..1000 {
            config
                .models
                .push(ModelInfo::named(&format!("fixture-{index}"), false));
        }
        panic::lock(&s).providers.list.push(config);
        let out = describe(&s, &current, true);
        let compact = describe(&s, &current, false);
        assert_eq!(compact["models"].as_array().unwrap().len(), 1);
        assert_eq!(compact["models"][0]["id"], json!(current.model));
        assert_eq!(compact["catalogue_complete"], json!(false));
        assert_eq!(compact["settings"], out["settings"]);
        assert!(
            serde_json::to_vec(&compact).unwrap().len()
                < serde_json::to_vec(&out).unwrap().len() / 2
        );
        assert_eq!(out["max_output_ceiling"], json!(131_072));
        let suppressed = out["suppressed"].as_array().unwrap();
        assert!(suppressed.iter().any(|v| v["key"] == "temperature"));
        let knobs = out["knobs"].as_array().unwrap();
        assert!(knobs
            .iter()
            .any(|k| k["key"] == "top_p" && k["max"] == json!(1.0)));
    }
    #[test]
    fn unsafe_endpoint_is_rejected_before_storing_or_transmitting_a_key() {
        let s = on_disk("unsafe-preflight");
        let error = add(
            &s,
            "mimo",
            Some("mimo"),
            Some("http://example.invalid/v1"),
            None,
            Some("sk-test-only-000001"),
            Some("mimo-v2.5-pro"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("plain http"));
        assert!(panic::lock(&s).secrets.get("mimo").is_none());
        assert!(panic::lock(&s).providers.get("mimo").is_none());
    }

    #[test]
    fn add_provider_verifies_credentials_without_calling_model_generation() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let payload = json!({
                    "data": [
                        { "id": "custom-model-alpha", "context_length": 65536 }
                    ]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let s = on_disk("add-without-probe");
        let result = add(
            &s,
            "custom-ai",
            None,
            Some(&format!("http://127.0.0.1:{port}/v1")),
            Some("Primary Key"),
            Some("sk-test-any-valid-key"),
            Some("custom-model-alpha"),
        );
        assert!(
            result.is_ok(),
            "adding provider must succeed without calling chat completion probe: {result:?}"
        );
        let lock = panic::lock(&s);
        let p = lock
            .providers
            .get("custom-ai")
            .expect("provider was stored");
        assert_eq!(p.name, "custom-ai");
        assert_eq!(
            p.api.as_deref(),
            Some("compatible"),
            "an endpoint added by hand records what it is"
        );
        assert!(p.model("custom-model-alpha").is_some());
        assert_eq!(
            lock.secrets.keys_metadata("custom-ai")[0].name,
            "Primary Key"
        );
    }
}
