//! Full-screen preferences: hierarchical navigation, one editor, acknowledged writes.
//! Page ancestry is explicit. Esc cancels an editor, then returns to its parent;
//! it never shortcuts a provider detail page directly to Overview.

use super::ansi::{self, ACCENT, BOLD, MUTED, REMOVED, RESET, RULE, TEXT};
use super::draw::{thousands, Hit, Sink};
use super::input::Key;
use super::{panels, theme, App, Awaiting, Panel};
use crate::driver::Driver;
use runeengine::settings::services::Service;
use runeengine::Command;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Instant;

#[path = "settings_web.rs"]
mod web_ui;
use web_ui::{
    access_rows as service_access_rows, add_rows as web_add_rows,
    provider_rows as web_provider_rows,
};

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Page {
    #[default]
    Overview,
    Models,
    Generation,
    Connections,
    Provider(String),
    AddProvider,
    Setup(String),
    Appearance,
    Runtime,
    Themes,
    Tools,
    WebProviders {
        service: Service,
    },
    AddWebProvider {
        service: Service,
    },
    ModelConfig {
        provider: String,
        model: String,
    },
    ModelTierPricing {
        provider: String,
        model: String,
    },
    ModelBusyPricing {
        provider: String,
        model: String,
    },
}

pub const SECTIONS: &[(&str, Page)] = &[
    ("Overview", Page::Overview),
    ("Models", Page::Models),
    ("Generation", Page::Generation),
    ("Connections", Page::Connections),
    ("Runtime", Page::Runtime),
    ("Tools", Page::Tools),
    ("Appearance", Page::Appearance),
];

impl Page {
    fn parent(&self) -> Option<Page> {
        match self {
            Self::Overview => None,
            Self::Provider(_) | Self::AddProvider => Some(Self::Connections),
            Self::Setup(_) => Some(Self::AddProvider),
            Self::Themes => Some(Self::Appearance),
            Self::Tools | Self::Runtime | Self::Appearance => Some(Self::Overview),
            Self::ModelConfig { .. } => Some(Self::Models),
            Self::ModelTierPricing { provider, model } => Some(Self::ModelConfig {
                provider: provider.clone(),
                model: model.clone(),
            }),
            Self::ModelBusyPricing { provider, model } => Some(Self::ModelConfig {
                provider: provider.clone(),
                model: model.clone(),
            }),
            Self::WebProviders { .. } => Some(Self::Tools),
            Self::AddWebProvider { service } => Some(Self::WebProviders { service: *service }),
            _ => Some(Self::Overview),
        }
    }

    fn title(&self) -> &str {
        match self {
            Self::Overview => "Overview",
            Self::Models => "Models",
            Self::Generation => "Generation",
            Self::Connections => "Connections",
            Self::Runtime => "Runtime",
            Self::Provider(name) | Self::Setup(name) => name,
            Self::AddProvider => "Add connection",
            Self::Appearance => "Appearance",
            Self::Themes => "Color theme",
            Self::Tools => "Tools",
            Self::WebProviders { service } => service_title(*service),
            Self::AddWebProvider { .. } => "Add provider",
            Self::ModelConfig { model, .. } => model.as_str(),
            Self::ModelTierPricing { .. } => "Context tiered pricing",
            Self::ModelBusyPricing { .. } => "Peak hours pricing",
        }
    }

    fn tab(&self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Models
            | Self::ModelConfig { .. }
            | Self::ModelTierPricing { .. }
            | Self::ModelBusyPricing { .. } => 1,
            Self::Generation => 2,
            Self::Connections | Self::Provider(_) | Self::AddProvider | Self::Setup(_) => 3,
            Self::Runtime => 4,
            Self::Tools | Self::WebProviders { .. } | Self::AddWebProvider { .. } => 5,
            Self::Appearance | Self::Themes => 6,
        }
    }
}

#[derive(Default, Clone)]
pub struct Location {
    pub page: Page,
    pub row: usize,
    pub query: String,
    pub cursor: usize,
    pub(super) editing: super::editing::State,
    pub provider: String,
    pub focus: ModelFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelFocus {
    Providers,
    #[default]
    Search,
    List,
}

#[derive(Default)]
pub struct State {
    /// What a vendor offered when last asked (`service.options`): routes, zones.
    offered: Value,
    pub location: Location,
    parents: Vec<Location>,
    pub tabs: bool,
    pub editor: Option<Editor>,
    pub width: usize,
    pub height: usize,
    text_rows: Vec<super::editor::Row>,
    notice: Option<Notice>,
    health: HashMap<String, Notice>,
    pub(super) pending: Option<u64>,
    pub(crate) serial: u64,
    setup: Option<ProviderDraft>,
}

impl State {
    pub(super) fn expire_notice(&mut self, now: Instant) -> bool {
        if self.notice.as_ref().is_some_and(|n| {
            n.tone != Tone::Busy
                && now.saturating_duration_since(n.created)
                    >= super::runtime_configuration().duration("ui.status_lifetime")
        }) {
            self.notice = None;
            return true;
        }
        false
    }

    /// Keep the shared ancestry, including the cursor/query in every ancestor.
    pub fn navigate(&mut self, page: Page) {
        if self.editor.is_some() {
            return;
        }
        let mut path = vec![page.clone()];
        while let Some(parent) = path.last().and_then(Page::parent) {
            path.push(parent);
        }
        path.reverse();
        let mut old = std::mem::take(&mut self.parents);
        old.push(std::mem::take(&mut self.location));
        let common = path
            .iter()
            .zip(&old)
            .take_while(|(page, loc)| **page == loc.page)
            .count();
        let mut next: Vec<Location> = old.into_iter().take(common).collect();
        next.extend(path.into_iter().skip(common).map(|page| Location {
            page,
            ..Location::default()
        }));
        self.location = next.pop().unwrap_or_default();
        self.parents = next;
        self.tabs = false;
        if !matches!(page, Page::Setup(_)) {
            self.setup = None;
        }
    }

    /// False only at the root: the caller then closes Settings.
    pub fn back(&mut self) -> bool {
        if self.editor.take().is_some() {
            return true;
        }
        if let Some(parent) = self.parents.pop() {
            self.location = parent;
            self.tabs = false;
            self.setup = None;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Operation {
    Setting(Option<String>),
    Registry(String),
    Catalogue(String),
    Probe(String),
    Added(String),
    /// What a vendor offers was asked; the editor for `then` opens with it.
    Options(Field),
    /// A service connection changed; the message says how.
    Service(String),
    /// A service connection set up from Settings was checked and saved.
    Connected(String),
}

#[derive(Clone, PartialEq)]
pub struct Request {
    pub(crate) token: u64,
    pub(crate) session: Option<i64>,
    pub(crate) origin: Page,
    pub(crate) editor: Option<u64>,
    pub(crate) operation: Operation,
    /// The session settings as they were before an optimistic edit, put back
    /// if the engine refuses it — the screen must never show a knob the engine
    /// does not hold.
    pub(crate) before: Option<Value>,
}

#[derive(Clone, Debug)]
struct Notice {
    page: Page,
    text: String,
    tone: Tone,
    created: Instant,
    page_offset: usize,
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tone {
    Good,
    Error,
    Busy,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Field {
    Knob(String),
    Global(String),
    Model,
    Key(String),
    KeyValue {
        name: String,
        key_name: String,
    },
    Timeout(String),
    SetupKeyName,
    SetupKey,
    RemoveKey(String, usize),
    /// Set the unreadable credential store aside and start a new one.
    NewCredentialStore,
    ModelContext {
        provider: String,
        model: String,
    },
    ModelMaxOutput {
        provider: String,
        model: String,
    },
    ModelInputPrice {
        provider: String,
        model: String,
    },
    ModelOutputPrice {
        provider: String,
        model: String,
    },
    ModelCachedPrice {
        provider: String,
        model: String,
    },
    ModelExtraRate {
        provider: String,
        model: String,
        key: &'static str,
    },
    ModelTierThreshold {
        provider: String,
        model: String,
    },
    ModelTierInput {
        provider: String,
        model: String,
    },
    ModelTierOutput {
        provider: String,
        model: String,
    },
    ModelBusyInput {
        provider: String,
        model: String,
    },
    ModelBusyOutput {
        provider: String,
        model: String,
    },
    ModelReasoning {
        provider: String,
        model: String,
    },
    /// Where a model connection is reached.
    BaseUrl(String),
    /// Where a service connection is reached for one service (search's
    /// address is its base URL; fetch's, for a vendor that splits them).
    ServiceUrl(String, Service),
    /// A gateway route to keep, typed by hand: what a gateway does not list
    /// may still serve (9router 0.5.99 left routes that work out of its list).
    ServiceRouteText(String, Service),
    /// Bright Data's SERP zone, picked from the account's zones.
    ServiceZone(String),
    /// Remove a service connection (its keys stay).
    RemoveService(String),
    /// A service setup: the address of the person's own server.
    SetupUrl,
}

impl Field {
    /// A field that only asks to confirm something that cannot be taken back
    /// from the screen; it opens on the answer that keeps things as they are.
    fn confirms(&self) -> bool {
        matches!(
            self,
            Field::RemoveKey(..) | Field::RemoveService(_) | Field::NewCredentialStore
        )
    }

    pub fn is_aux(&self) -> bool {
        match self {
            Field::Global(key) => ["classifier.", "compact.", "title.", "sanitizer."]
                .iter()
                .any(|p| key.starts_with(p)),
            _ => false,
        }
    }
}

/// The editor's choice list is a focus target, not an Apply button.
/// `button` is only 0..3 when the reader explicitly tabs to an action.
const CHOICES_FOCUS: usize = usize::MAX;

pub struct Editor {
    id: u64,
    field: Field,
    title: String,
    help: String,
    pub text: String,
    pub cursor: usize,
    editing: super::editing::State,
    choices: Vec<String>,
    clearable: bool,
    button: usize,
    pub confirm_apply_at: Option<std::time::Instant>,
}

impl Editor {
    fn secret(&self) -> bool {
        matches!(self.field, Field::KeyValue { .. } | Field::SetupKey)
    }
    fn confirmation(&self) -> bool {
        self.field.confirms()
    }
    pub fn is_aux(&self) -> bool {
        self.field.is_aux()
    }
    pub fn is_multi_choice(&self) -> bool {
        match &self.field {
            Field::Global(k) => k.ends_with(".excluded_tools") || k.ends_with(".context_tools"),
            Field::ModelReasoning { .. } => self.choices.iter().any(|c| c != "off" && c != "on"),
            _ => false,
        }
    }
    pub fn is_duration(&self) -> bool {
        match &self.field {
            Field::Timeout(_) => true,
            Field::Global(k) => runeengine::settings::configuration::FIELDS.iter().any(|f| {
                f.key == k && matches!(f.kind, runeengine::settings::configuration::Kind::Duration)
            }),
            _ => false,
        }
    }
    pub fn is_size(&self) -> bool {
        match &self.field {
            Field::Global(k) => runeengine::settings::configuration::FIELDS.iter().any(|f| {
                f.key == k && matches!(f.kind, runeengine::settings::configuration::Kind::Size)
            }),
            _ => false,
        }
    }
    pub fn allows_char(&self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        match &self.field {
            Field::Global(key) => {
                if let Some(field_def) = runeengine::settings::configuration::FIELDS
                    .iter()
                    .find(|f| f.key == key)
                {
                    match field_def.kind {
                        runeengine::settings::configuration::Kind::Count(..) => c.is_ascii_digit(),
                        runeengine::settings::configuration::Kind::Duration => {
                            c.is_ascii_digit()
                                || matches!(
                                    c,
                                    ' ' | 's'
                                        | 'S'
                                        | 'm'
                                        | 'M'
                                        | 'h'
                                        | 'H'
                                        | 'd'
                                        | 'D'
                                        | 'u'
                                        | 'U'
                                        | 'n'
                                        | 'N'
                                        | 'l'
                                        | 'L'
                                        | 'i'
                                        | 'I'
                                        | 't'
                                        | 'T'
                                        | 'e'
                                        | 'E'
                                        | 'a'
                                        | 'A'
                                        | 'o'
                                        | 'O'
                                        | 'r'
                                        | 'R'
                                        | 'k'
                                        | 'K'
                                )
                        }
                        runeengine::settings::configuration::Kind::Size => {
                            c.is_ascii_digit()
                                || matches!(
                                    c,
                                    ' ' | 'b'
                                        | 'B'
                                        | 'k'
                                        | 'K'
                                        | 'm'
                                        | 'M'
                                        | 'g'
                                        | 'G'
                                        | 'i'
                                        | 'I'
                                        | 'a'
                                        | 'A'
                                        | 'u'
                                        | 'U'
                                        | 't'
                                        | 'T'
                                        | 'o'
                                        | 'O'
                                        | 'n'
                                        | 'N'
                                        | 'l'
                                        | 'L'
                                )
                        }
                        runeengine::settings::configuration::Kind::Money => {
                            c.is_ascii_digit() || matches!(c, '.' | '$')
                        }
                        runeengine::settings::configuration::Kind::Bool => false,
                        _ => true,
                    }
                } else if key.ends_with("_chars")
                    || key.ends_with("_limit")
                    || key.ends_with("_turns")
                    || key.ends_with("_messages")
                {
                    c.is_ascii_digit()
                } else {
                    true
                }
            }
            Field::Timeout(_) => {
                c.is_ascii_digit()
                    || matches!(
                        c,
                        ' ' | 's'
                            | 'S'
                            | 'm'
                            | 'M'
                            | 'h'
                            | 'H'
                            | 'd'
                            | 'D'
                            | 'u'
                            | 'U'
                            | 'n'
                            | 'N'
                            | 'l'
                            | 'L'
                            | 'i'
                            | 'I'
                            | 't'
                            | 'T'
                            | 'e'
                            | 'E'
                            | 'a'
                            | 'A'
                            | 'o'
                            | 'O'
                            | 'r'
                            | 'R'
                    )
            }
            Field::Knob(key) => {
                if key == "temperature" || key == "top_p" {
                    c.is_ascii_digit() || c == '.'
                } else if key == "max_output" {
                    c.is_ascii_digit() || matches!(c, 'a' | 'A' | 'u' | 'U' | 't' | 'T' | 'o' | 'O')
                } else {
                    true
                }
            }
            Field::ModelContext { .. }
            | Field::ModelMaxOutput { .. }
            | Field::ModelTierThreshold { .. } => c.is_ascii_digit(),
            Field::ModelInputPrice { .. }
            | Field::ModelOutputPrice { .. }
            | Field::ModelCachedPrice { .. }
            | Field::ModelTierInput { .. }
            | Field::ModelTierOutput { .. }
            | Field::ModelBusyInput { .. }
            | Field::ModelBusyOutput { .. }
            | Field::ModelExtraRate { .. } => c.is_ascii_digit() || c == '.',
            _ => true,
        }
    }
    pub fn is_confirming_apply(&self) -> bool {
        if let Some(t) = self.confirm_apply_at {
            t.elapsed() < std::time::Duration::from_secs(3)
        } else {
            false
        }
    }
    pub fn remaining_confirm_secs(&self) -> u64 {
        if let Some(t) = self.confirm_apply_at {
            let elapsed = t.elapsed();
            if elapsed < std::time::Duration::from_secs(3) {
                3_u64.saturating_sub(elapsed.as_secs())
            } else {
                0
            }
        } else {
            0
        }
    }
}

fn aux_model_spec(app: &App, role: &str) -> (String, String, Option<u64>, Option<u64>) {
    let key = format!("{role}.model");
    let val = key
        .split('.')
        .fold(&app.settings["configuration"], |v, k| &v[k]);
    let (p_name, m_id) = if val.is_null() {
        (
            app.provider().to_string(),
            app.settings["settings"]["model"]
                .as_str()
                .unwrap_or_else(|| app.model())
                .to_string(),
        )
    } else if val.is_object() {
        let p = val
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or(app.provider())
            .to_string();
        let m = val
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| app.model())
            .to_string();
        (p, m)
    } else if let Some(s) = val.as_str() {
        let s = s.trim();
        if s.is_empty() || s == "null" || s == "inherit" || s == "default" {
            (
                app.provider().to_string(),
                app.settings["settings"]["model"]
                    .as_str()
                    .unwrap_or_else(|| app.model())
                    .to_string(),
            )
        } else if let Some((p, m)) = s.split_once(':').or_else(|| s.split_once('/')) {
            (p.trim().to_string(), m.trim().to_string())
        } else {
            (app.provider().to_string(), s.to_string())
        }
    } else {
        (
            app.provider().to_string(),
            app.settings["settings"]["model"]
                .as_str()
                .unwrap_or_else(|| app.model())
                .to_string(),
        )
    };

    let providers = provider_list(app);
    let catalog = if p_name == app.provider() {
        app.settings["models"].as_array().or_else(|| {
            providers
                .iter()
                .find(|p| p["name"].as_str() == Some(&p_name))
                .and_then(|p| p["models"].as_array())
        })
    } else {
        providers
            .iter()
            .find(|p| p["name"].as_str() == Some(&p_name))
            .and_then(|p| p["models"].as_array())
    };

    let (max_output, context) = catalog
        .into_iter()
        .flatten()
        .find(|m| m["id"].as_str() == Some(&m_id))
        .map(|m| (m["max_output"].as_u64(), m["context"].as_u64()))
        .unwrap_or((None, None));

    (p_name, m_id, max_output, context)
}

impl Editor {
    pub fn max_len(&self, app: &App) -> usize {
        match &self.field {
            Field::Global(k) => {
                if k.ends_with(".max_output_tokens") {
                    let role = k.split('.').next().unwrap_or("");
                    let (_, _, max_out, _) = aux_model_spec(app, role);
                    if let Some(c) = max_out {
                        (c.to_string().len() + 1).max(5)
                    } else {
                        7
                    }
                } else if k == "compact.fallback_context_tokens" {
                    let (_, _, _, ctx) = aux_model_spec(app, "compact");
                    if let Some(w) = ctx {
                        (w.to_string().len() + 1).max(5)
                    } else {
                        8
                    }
                } else if let Some(f) = runeengine::settings::configuration::FIELDS
                    .iter()
                    .find(|f| f.key == k)
                {
                    match f.kind {
                        runeengine::settings::configuration::Kind::Count(_, hi) => {
                            hi.to_string().len() + 1
                        }
                        runeengine::settings::configuration::Kind::Duration => 24,
                        runeengine::settings::configuration::Kind::Size => 16,
                        runeengine::settings::configuration::Kind::Model => 64,
                        runeengine::settings::configuration::Kind::Bool => 5,
                        _ => 128,
                    }
                } else {
                    128
                }
            }
            Field::Knob(k) if k == "max_output" => {
                let ceiling = app.settings["models"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|m| m["id"].as_str() == Some(app.model()))
                            .and_then(|m| m["max_output"].as_u64())
                    })
                    .or_else(|| app.settings["max_output_ceiling"].as_u64());
                if let Some(c) = ceiling {
                    (c.to_string().len() + 1).max(5)
                } else {
                    7
                }
            }
            Field::Key(_) => 64,
            Field::KeyValue { .. } => 512,
            Field::Timeout(_) => 24,
            Field::Model => 64,
            Field::BaseUrl(_) | Field::ServiceUrl(..) | Field::SetupUrl => 512,
            Field::ServiceRouteText(..) => 256,
            _ => 128,
        }
    }
}

struct ProviderDraft {
    preset: String,
    name: String,
    url: String,
    key_name: String,
    key: String,
    model: String,
    /// A search/fetch connection rather than a model one.
    service: bool,
}

impl ProviderDraft {
    fn model(name: &str) -> ProviderDraft {
        ProviderDraft {
            preset: name.into(),
            name: name.into(),
            url: String::new(),
            key_name: "Primary".to_string(),
            key: String::new(),
            model: String::new(),
            service: false,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Kind {
    Fact,
    Navigate(Page),
    Model {
        provider: String,
        id: String,
    },
    Edit(Field),
    Refresh(String),
    Default(String),
    AddPreset(String),
    /// Start a search/fetch connection with this vendor.
    AddService(String),
    /// A route a connection keeps for one service: the one in use, or one to
    /// switch to (Delete takes it off the list).
    RouteUse(String, Service, String),
    WebSelect(Service, Option<String>),
    /// Keep another route for one service: what the gateway lists, or typed.
    RouteAdd(String, Service),
    /// Switch on a service that takes no route (after its check).
    ServiceOn(String, Service),
    /// A service setup: save it (after its check).
    Connect,
    Sidebar,
    ReloadClassifier,
    Theme(theme::ThemeMode),
}

pub struct Row {
    pub kind: Kind,
    pub label: String,
    pub value: String,
    pub note: String,
    pub help: String,
    pub background: ansi::Rgb,
    pub color_depth: ansi::Depth,
}

impl Row {
    fn new(kind: Kind, label: &str, value: String) -> Self {
        Self {
            background: theme::palette().bg,
            color_depth: ansi::depth(),
            kind,
            label: label.into(),
            value,
            note: String::new(),
            help: String::new(),
        }
    }
    fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }
    fn help(mut self, help: &str) -> Self {
        self.help = help.into();
        self
    }
    pub fn pickable(&self) -> bool {
        self.kind != Kind::Fact
    }
}

pub fn rows(app: &App) -> Vec<Row> {
    match &app.preferences.location.page {
        Page::Overview => vec![
            Row::new(
                Kind::Navigate(Page::Models),
                "01 / MODELS",
                "Manage active models and catalog parameters.".into(),
            )
            .note("Configure context windows, token limits, and pricing tiers.")
            .help("Browse models →"),
            Row::new(
                Kind::Navigate(Page::Generation),
                "02 / GENERATION",
                "Control model reasoning and sampling behavior.".into(),
            )
            .note("Tune reasoning effort, temperature, penalties, and ceilings.")
            .help("Tune responses →"),
            Row::new(
                Kind::Navigate(Page::Connections),
                "03 / CONNECTIONS",
                "Manage external providers and service endpoints.".into(),
            )
            .note("Manage account credentials and endpoint addresses; requests are configured elsewhere.")
            .help("Manage connections →"),
            Row::new(
                Kind::Navigate(Page::Runtime),
                "04 / RUNTIME",
                "Execution harness, retry policies, and network timeouts.".into(),
            )
            .note("Tune request retries, backoff curve, stream stall limit, and cooldowns.")
            .help("Configure runtime →"),
            Row::new(
                Kind::Navigate(Page::Tools),
                "05 / TOOLS",
                "Tool execution policies, timeouts, and sandboxing.".into(),
            )
            .note("Control command deadlines, background tasks, and permissions.")
            .help("Customize tools →"),
            Row::new(
                Kind::Navigate(Page::Appearance),
                "06 / APPEARANCE",
                "Personalize visual aesthetics and interface layout.".into(),
            )
            .note("Customize color themes, workspace display, and side panel.")
            .help("Customize appearance →"),
        ],
        Page::Models => model_rows(app),
        Page::ModelConfig { provider, model } => model_config_rows(app, provider, model),
        Page::ModelTierPricing { provider, model } => model_tier_rows(app, provider, model),
        Page::ModelBusyPricing { provider, model } => model_busy_rows(app, provider, model),
        Page::Generation => sampling_rows(app),
        Page::Connections | Page::Provider(_) => provider_rows(app),
        Page::WebProviders { service } => web_provider_rows(app, *service),
        Page::AddWebProvider { service } => web_add_rows(app, *service),
        Page::Runtime => runtime_rows(app),
        Page::Appearance | Page::Themes => screen_rows(app),
        Page::Tools => tools_rows(app),
        Page::AddProvider => {
            let mut out = vec![Row::new(Kind::Fact, "MODEL PROVIDERS", String::new())];
            out.extend(
                app.providers["presets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| {
                        let name = p["name"].as_str()?;
                        (!provider_list(app)
                            .into_iter()
                            .any(|p| p["name"].as_str() == Some(name)))
                        .then(|| {
                            Row::new(
                                Kind::AddPreset(name.into()),
                                name,
                                if p["needs_key"].as_bool().unwrap_or(true) {
                                    "API key required"
                                } else {
                                    "Key optional"
                                }
                                .into(),
                            )
                            .help(p["key_hint"].as_str().unwrap_or("Configure this endpoint."))
                        })
                    }),
            );
            out.push(Row::new(Kind::Fact, "SEARCH & FETCH", String::new()));
            for a in app.providers["adapters"].as_array().into_iter().flatten() {
                let Some(id) = a["id"].as_str() else { continue };
                if service_of(app, id).is_some() {
                    continue;
                }
                let mut serves = Vec::new();
                if a["search"].as_bool().unwrap_or(false) {
                    serves.push("Search");
                }
                if a["fetch"].as_bool().unwrap_or(false) {
                    serves.push("Fetch");
                }
                let key = format!(
                    "{} — key from {}.",
                    a["key_hint"].as_str().unwrap_or("API key"),
                    a["site"].as_str().unwrap_or("the vendor")
                );
                // A vendor already connected for models gains its services
                // on that connection's page: one connection, one key ring.
                // Its row reads like every other, so the columns stay one.
                let connected = provider_list(app)
                    .into_iter()
                    .any(|p| p["name"].as_str() == Some(id));
                let (value, help) = if connected {
                    (
                        "Already connected",
                        format!(
                            "{id} is already a connection: its endpoint and keys are managed here; Tools chooses providers \
                             with the same key ring. {key}"
                        ),
                    )
                } else {
                    (
                        match a["key"].as_str() {
                            Some("none") => "Your own server",
                            Some("optional") => "Key optional",
                            _ => "API key required",
                        },
                        key,
                    )
                };
                out.push(
                    Row::new(Kind::AddService(id.into()), id, value.into())
                        .note(format!(
                            "{} · {}",
                            a["title"].as_str().unwrap_or(id),
                            serves.join(" · ")
                        ))
                        .help(&help),
                );
            }
            out
        }
        Page::Setup(name) => {
            let Some(d) = &app.preferences.setup else {
                return Vec::new();
            };
            if d.service {
                return service_setup_rows(app, d);
            }
            let hint = runeengine::settings::providers::preset(name)
                .map(|p| p.key_hint)
                .unwrap_or("API key for this provider");
            vec![
                Row::new(
                    Kind::Edit(Field::SetupKeyName),
                    "Key name",
                    if d.key_name.is_empty() {
                        "[ Primary ↵ ]".into()
                    } else {
                        format!("[ {} ↵ ]", d.key_name)
                    },
                )
                .help("Label to distinguish this key in the provider's key ring (e.g. Primary, Work)."),
                Row::new(
                    Kind::Edit(Field::SetupKey),
                    "API key",
                    if d.key.is_empty() {
                        "[ Set API key ↵ ]".into()
                    } else {
                        "••••••••  [ edit ↵ ]".into()
                    },
                )
                .help(hint),
            ]
        }
    }
}

fn provider_list(app: &App) -> Vec<&Value> {
    let mut list: Vec<&Value> = app.providers["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    list.sort_by_key(|p| p["name"].as_str().unwrap_or("").to_lowercase());
    list
}

/// The adapter `provider` speaks through, as the engine's registry names it
/// (`api`, always published): what a reasoning choice means is the adapter's.
fn provider_api<'a>(app: &'a App, provider: &str) -> &'a str {
    provider_list(app)
        .into_iter()
        .find(|p| p["name"].as_str() == Some(provider))
        .and_then(|p| p["api"].as_str())
        .unwrap_or("compatible")
}

fn model_provider(app: &App) -> &str {
    let chosen = &app.preferences.location.provider;
    if chosen == "all" {
        return "all";
    }
    let list = provider_list(app);
    list.iter()
        .find(|p| p["name"].as_str() == Some(chosen.as_str()))
        .or_else(|| {
            list.iter()
                .find(|p| p["name"].as_str() == Some(app.provider()))
        })
        .or_else(|| list.first())
        .and_then(|p| p["name"].as_str())
        .unwrap_or("")
}

fn model_config_rows(app: &App, provider: &str, model: &str) -> Vec<Row> {
    let info = app.lookup_model_info(provider, model);
    let ctx_val = info
        .as_ref()
        .and_then(|m| m.context)
        .map(|n| format!("{} tokens", thousands(n)))
        .unwrap_or_else(|| "Not set".into());
    let max_val = info
        .as_ref()
        .and_then(|m| m.max_output)
        .map(|n| format!("{} tokens", thousands(n)))
        .unwrap_or_else(|| "Not set (or unlimited)".into());

    let (in_val, out_val, cached_val, tier_summary, busy_summary) =
        if let Some(p) = info.as_ref().and_then(|m| m.price.as_ref()) {
            let in_str = format!("${:.4} / 1M", p.input);
            let out_str = format!("${:.4} / 1M", p.output);
            let cached_str = format!("${:.4} / 1M", p.cached);
            let tier_str = if let Some(th) = p.tier_context_threshold {
                format!(
                    ">{} tok (${:.2}/${:.2})",
                    thousands(th),
                    p.tier_input.unwrap_or(p.input),
                    p.tier_output.unwrap_or(p.output)
                )
            } else if !p.overrides.is_empty() {
                format!("{} provider tiers", p.overrides.len())
            } else {
                "Not configured".into()
            };
            let busy_str = if let Some(bi) = p.busy_input {
                format!("${:.2} / ${:.2}", bi, p.busy_output.unwrap_or(p.output))
            } else {
                "Not configured".into()
            };
            (in_str, out_str, cached_str, tier_str, busy_str)
        } else {
            (
                "Not set".into(),
                "Not set".into(),
                "Not set".into(),
                "Not configured".into(),
                "Not configured".into(),
            )
        };

    let mut rows=vec![
        Row::new(
            Kind::Edit(Field::ModelContext {
                provider: provider.into(),
                model: model.into(),
            }),
            "Context window limit",
            ctx_val,
        )
        .help("Total tokens the model accepts (e.g. 1000000 for 1M, 200000 for 200k, 128000 for 128k)"),
        Row::new(
            Kind::Edit(Field::ModelMaxOutput {
                provider: provider.into(),
                model: model.into(),
            }),
            "Max output tokens",
            max_val,
        )
        .help("Maximum completion tokens generated in a single turn (leave empty to clear)"),
        Row::new(
            Kind::Edit(Field::ModelInputPrice {
                provider: provider.into(),
                model: model.into(),
            }),
            "Input price ($/M tokens)",
            in_val,
        )
        .help("Price per million input tokens in USD (e.g. 0.15, 3.00, 0 for free)"),
        Row::new(
            Kind::Edit(Field::ModelOutputPrice {
                provider: provider.into(),
                model: model.into(),
            }),
            "Output price ($/M tokens)",
            out_val,
        )
        .help("Price per million output tokens in USD (e.g. 0.60, 15.00, 0 for free)"),
        Row::new(
            Kind::Edit(Field::ModelCachedPrice {
                provider: provider.into(),
                model: model.into(),
            }),
            "Cached read price ($/M tokens)",
            cached_val,
        )
        .help("Price per million cached tokens (optional, defaults to input price)"),
        Row::new(
            Kind::Navigate(Page::ModelTierPricing {
                provider: provider.into(),
                model: model.into(),
            }),
            "Context tiered pricing",
            format!("{tier_summary} →"),
        )
        .help("Press Enter to open tiered pricing screen for large context windows"),
        Row::new(
            Kind::Navigate(Page::ModelBusyPricing {
                provider: provider.into(),
                model: model.into(),
            }),
            "Peak hours pricing",
            format!("{busy_summary} →"),
        )
        .help("Press Enter to open peak/busy hours pricing screen"),
        Row::new(
            Kind::Edit(Field::ModelReasoning {
                provider: provider.into(),
                model: model.into(),
            }),
            "Reasoning",
            info.as_ref().map(|m| {
                let api = provider_api(app, provider);
                let choices = m.reasoning_choices(api);
                if choices.is_empty() { "Unsupported / unknown".into() }
                else if m.thinking_required && m.efforts.is_empty() && api != "9router" { "On (Mandatory)".into() }
                else {
                    let options = m.reasoning_options.as_ref().unwrap_or(&choices);
                    format!("{} · {}", if m.reasoning_enabled.unwrap_or(false) { "On" } else { "Off" }, options.join(", "))
                }
            }).unwrap_or_else(|| "Unsupported / unknown".into()),
        )
        .help("Configure reasoning support and effort levels for this model"),
    ];
    for (key, label) in [
        ("cache_write", "Cache write price"),
        ("cache_write_5m", "5-minute cache write price"),
        ("cache_write_1h", "1-hour cache write price"),
        ("busy_start_utc_hour", "Peak start hour (UTC)"),
        ("busy_end_utc_hour", "Peak end hour (UTC)"),
    ] {
        let value = info
            .as_ref()
            .and_then(|m| m.price.as_ref())
            .and_then(|p| serde_json::to_value(p).ok())
            .and_then(|v| v.get(key).and_then(Value::as_f64))
            .map(|n| {
                if key.ends_with("utc_hour") {
                    format!("{n:02.0}:00 UTC")
                } else {
                    format!("${n:.4} / 1M")
                }
            })
            .unwrap_or_else(|| "Unknown / not applicable".into());
        rows.insert(5,Row::new(Kind::Edit(Field::ModelExtraRate{provider:provider.into(),model:model.into(),key}),label,value).help(if key.ends_with("utc_hour"){ "UTC hour from 0 to 23; start is inclusive and end is exclusive. Equal hours cover all day." }else{"Zero is free; empty is unknown. Rates are per million cache-creation tokens."}));
    }
    rows
}

fn model_tier_rows(app: &App, provider: &str, model: &str) -> Vec<Row> {
    let info = app.lookup_model_info(provider, model);
    let (th_val, in_val, out_val) = if let Some(p) = info.as_ref().and_then(|m| m.price.as_ref()) {
        (
            p.tier_context_threshold
                .map(|n| format!("{} tokens", thousands(n)))
                .unwrap_or_else(|| "Not set".into()),
            p.tier_input
                .map(|n| format!("${:.4} / 1M", n))
                .unwrap_or_else(|| "Not set".into()),
            p.tier_output
                .map(|n| format!("${:.4} / 1M", n))
                .unwrap_or_else(|| "Not set".into()),
        )
    } else {
        ("Not set".into(), "Not set".into(), "Not set".into())
    };

    let mut rows = vec![
        Row::new(
            Kind::Edit(Field::ModelTierThreshold {
                provider: provider.into(),
                model: model.into(),
            }),
            "Context tier threshold",
            th_val,
        )
        .help("Token count above which tiered pricing activates (e.g. 200000, empty to disable)"),
        Row::new(
            Kind::Edit(Field::ModelTierInput {
                provider: provider.into(),
                model: model.into(),
            }),
            "Tier input price ($/M tokens)",
            in_val,
        )
        .help("Price per million input tokens when prompt length exceeds threshold"),
        Row::new(
            Kind::Edit(Field::ModelTierOutput {
                provider: provider.into(),
                model: model.into(),
            }),
            "Tier output price ($/M tokens)",
            out_val,
        )
        .help("Price per million output tokens when prompt length exceeds threshold"),
    ];
    if let Some(price) = info.as_ref().and_then(|m| m.price.as_ref()) {
        for tier in &price.overrides {
            rows.push(Row::new(
                Kind::Fact,
                &format!(
                    "Provider tier {} {} tokens",
                    if tier.inclusive { "≥" } else { ">" },
                    thousands(tier.min_prompt_tokens)
                ),
                "Fetched from provider".into(),
            ));
            for (key, rate) in &tier.rates {
                let label = match key.as_str() {
                    "input" => "Input",
                    "output" => "Output",
                    "cached" => "Cache read",
                    "cache_write" => "Cache write",
                    "cache_write_5m" => "5-minute cache write",
                    _ => "1-hour cache write",
                };
                rows.push(Row::new(Kind::Fact, label, format!("${rate:.4} / 1M")));
            }
        }
    }
    rows
}

fn model_busy_rows(app: &App, provider: &str, model: &str) -> Vec<Row> {
    let info = app.lookup_model_info(provider, model);
    let (in_val, out_val) = if let Some(p) = info.as_ref().and_then(|m| m.price.as_ref()) {
        (
            p.busy_input
                .map(|n| format!("${:.4} / 1M", n))
                .unwrap_or_else(|| "Not set".into()),
            p.busy_output
                .map(|n| format!("${:.4} / 1M", n))
                .unwrap_or_else(|| "Not set".into()),
        )
    } else {
        ("Not set".into(), "Not set".into())
    };

    vec![
        Row::new(
            Kind::Edit(Field::ModelBusyInput {
                provider: provider.into(),
                model: model.into(),
            }),
            "Peak hours input price ($/M tokens)",
            in_val,
        )
        .help("Price per million input tokens during busy hours (leave empty to disable)"),
        Row::new(
            Kind::Edit(Field::ModelBusyOutput {
                provider: provider.into(),
                model: model.into(),
            }),
            "Peak hours output price ($/M tokens)",
            out_val,
        )
        .help("Price per million output tokens during busy hours"),
    ]
}

fn model_rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    let query = app.preferences.location.query.trim().to_lowercase();
    let name = model_provider(app);
    let providers = provider_list(app);
    if name == "all" {
        for p in &providers {
            let p_name = p["name"].as_str().unwrap_or("");
            let catalog = if p_name == app.provider() {
                let active_models = app.settings["models"].as_array();
                let provider_models = p["models"].as_array();
                match (active_models, provider_models) {
                    (Some(a), Some(p)) if a.len() >= p.len() => Some(a),
                    (_, Some(p)) => Some(p),
                    (Some(a), None) => Some(a),
                    (None, None) => None,
                }
            } else {
                p["models"].as_array()
            };
            let mut models: Vec<&Value> = catalog
                .into_iter()
                .flatten()
                .filter(|m| {
                    m["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                })
                .collect();
            models.sort_by_key(|m| m["id"].as_str().unwrap_or("").to_lowercase());
            if !models.is_empty() {
                out.push(Row::new(Kind::Fact, p_name, String::new()));
                for m in models {
                    let Some(id) = m["id"].as_str() else { continue };
                    let active = p_name == app.provider() && id == app.model();
                    let mut meta = Vec::new();
                    if active {
                        meta.push("Active in this session".into());
                    }
                    if let Some(n) = m["context"].as_u64() {
                        meta.push(format!("{} context", thousands(n)));
                    }
                    if m["thinking"].as_bool().unwrap_or(false) {
                        meta.push("Thinking".into());
                    }
                    if meta.is_empty() {
                        meta.push("Context not published".into());
                    }
                    out.push(
                        Row::new(
                            Kind::Model {
                                provider: p_name.into(),
                                id: id.into(),
                            },
                            id,
                            meta.join(" · "),
                        )
                        .help(if p_name == app.provider() {
                            "Enter applies the selected model to this session."
                        } else {
                            "Enter selects this provider and model together, using this provider's generation defaults."
                        }),
                    );
                }
            }
        }
        if out.is_empty() {
            out.push(Row::new(
                Kind::Fact,
                "No models found",
                "Try another search or check configured providers.".into(),
            ));
        }
        return out;
    }
    if let Some(p) = providers
        .into_iter()
        .find(|p| p["name"].as_str() == Some(name))
    {
        let catalog = if name == app.provider() {
            let active_models = app.settings["models"].as_array();
            let provider_models = p["models"].as_array();
            match (active_models, provider_models) {
                (Some(a), Some(p)) if a.len() >= p.len() => Some(a),
                (_, Some(p)) => Some(p),
                (Some(a), None) => Some(a),
                (None, None) => None,
            }
        } else {
            p["models"].as_array()
        };
        let mut models: Vec<&Value> = catalog
            .into_iter()
            .flatten()
            .filter(|m| {
                m["id"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        models.sort_by_key(|m| m["id"].as_str().unwrap_or("").to_lowercase());
        for m in models {
            let Some(id) = m["id"].as_str() else { continue };
            let active = name == app.provider() && id == app.model();
            let mut meta = Vec::new();
            if active {
                meta.push("Active in this session".into());
            }
            if let Some(n) = m["context"].as_u64() {
                meta.push(format!("{} context", thousands(n)));
            }
            if m["thinking"].as_bool().unwrap_or(false) {
                meta.push("Thinking".into());
            }
            if meta.is_empty() {
                meta.push("Context not published".into());
            }
            out.push(Row::new(Kind::Model{provider:name.into(),id:id.into()},id,meta.join(" · ")).help(
                if name==app.provider(){"Enter applies the selected model to this session."}else{"Enter selects this provider and model together, using this provider's generation defaults."}));
        }
        if out.is_empty() {
            out.push(Row::new(
                Kind::Fact,
                "No models found",
                "Try another search or refresh this provider's catalog.".into(),
            ));
        }
        out.push(
            Row::new(
                Kind::Edit(Field::Model),
                "Enter model ID…",
                format!("via {name}"),
            )
            .help("Enter an exact custom model ID for this provider."),
        );
        out.push(
            Row::new(
                Kind::Default(name.into()),
                "New-session fallback",
                if p["default"].as_bool().unwrap_or(false) {
                    "Active fallback default"
                } else {
                    "Set as fallback ↵"
                }
                .into(),
            )
            .help(
                "The initial provider for new sessions; existing session choices stay unchanged.",
            ),
        );
        out.push(
            Row::new(
                Kind::Refresh(name.into()),
                "Refresh catalog",
                "Update available models".into(),
            )
            .help("Fetch the model list without changing the active session."),
        );
    } else {
        out.push(Row::new(
            Kind::Navigate(Page::Connections),
            "Set up a provider",
            "Open Connections →".into(),
        ));
    }
    out
}

fn knob_choices(app: &App, key: &str) -> Option<Vec<String>> {
    let k = app.settings["knobs"]
        .as_array()?
        .iter()
        .find(|k| k["key"].as_str() == Some(key))?;
    let thinking_required = app.settings["thinking_required"].as_bool() == Some(true);
    if key == "thinking" || k["kind"] == "bool" {
        if key == "thinking" && thinking_required {
            return Some(vec!["on".into()]);
        }
        return Some(vec!["off".into(), "on".into()]);
    }
    if key == "effort" {
        let mut list = Vec::new();
        if let Some(arr) = k["choices"].as_array() {
            for v in arr {
                if let Some(s) = v.as_str() {
                    if s != "off" && s != "none" && s != "disabled" && !list.iter().any(|x| x == s)
                    {
                        list.push(s.to_string());
                    }
                }
            }
        }
        let route_default = list.iter().any(|s| s == super::ROUTE_DEFAULT);
        list.retain(|s| s != super::ROUTE_DEFAULT);
        runeengine::settings::providers::sort_efforts(&mut list);
        if list.is_empty() {
            return None;
        }
        // Off, the route's own level, then the levels from least to most.
        if route_default {
            list.insert(0, super::ROUTE_DEFAULT.to_string());
        }
        if !thinking_required {
            list.insert(0, "off".to_string());
        }
        return Some(list);
    }
    let list: Vec<String> = k["choices"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

fn cycle_knob(app: &mut App, key: &str, delta: isize, d: &mut Driver) -> bool {
    let Some(choices) = knob_choices(app, key) else {
        return false;
    };
    if choices.is_empty() {
        return false;
    }
    let curr = if key == "effort" {
        app.effort_choice(&choices)
    } else if key == "thinking" {
        if app.thinking() {
            "on".to_string()
        } else {
            "off".to_string()
        }
    } else {
        panels::value_of(&app.settings["settings"], key)
    };
    let at = choices
        .iter()
        .position(|c| c.eq_ignore_ascii_case(&curr))
        .unwrap_or(0);
    let next = if delta > 0 {
        (at + 1).min(choices.len() - 1)
    } else if delta < 0 {
        at.saturating_sub(1)
    } else {
        at
    };
    if next != at {
        let new_val = &choices[next];
        apply(app, key, new_val, d);
    }
    true
}

fn sampling_rows(app: &App) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let order = [
        "thinking",
        "effort",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "max_output",
        "stop",
    ];
    let mut knobs: Vec<&Value> = app.settings["knobs"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    knobs.sort_by_key(|k| {
        order
            .iter()
            .position(|n| Some(*n) == k["key"].as_str())
            .unwrap_or(order.len())
    });

    let effort_suppressed = app.settings["suppressed"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["key"].as_str() == Some("effort") && s["permanent"].as_bool() == Some(true));
    let thinking_suppressed = app.settings["suppressed"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["key"].as_str() == Some("thinking") && s["permanent"].as_bool() == Some(true));
    let effort_choices_count = app.settings["knobs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|k| k["key"].as_str() == Some("effort"))
        .and_then(|k| k["choices"].as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| *s != "off" && *s != "none" && *s != "disabled")
                .count()
        })
        .unwrap_or(0);

    let has_effort_levels = !effort_suppressed && effort_choices_count > 0;

    for knob in knobs {
        let Some(key) = knob["key"].as_str() else {
            continue;
        };

        if app.settings["suppressed"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["key"].as_str() == Some(key) && s["permanent"].as_bool() == Some(true))
        {
            continue;
        }
        // Deduplicate reasoning settings:
        // 1. If provider/model supports effort levels, omit redundant "thinking" row.
        // 2. If provider/model only supports binary thinking (or effort suppressed / no effort choices), omit "effort" row and show "thinking".
        // 3. If model does not reason at all, omit "effort" and let "thinking" show Unavailable.
        if key == "thinking" && has_effort_levels {
            continue;
        }
        if key == "effort" && (!has_effort_levels || (effort_suppressed && thinking_suppressed)) {
            continue;
        }

        if let Some(group) = match key {
            "thinking" | "effort" => Some("REASONING"),
            "temperature" => Some("SAMPLING"),
            "max_output" => Some("OUTPUT"),
            _ => None,
        } {
            if group != "REASONING" || !out.iter().any(|r| r.label == "REASONING") {
                out.push(Row::new(Kind::Fact, group, String::new()));
            }
        }
        let suppression = app.settings["suppressed"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["key"].as_str() == Some(key) && s["permanent"].as_bool() == Some(true));
        let label = knob["label"].as_str().unwrap_or(key);
        out.push(match suppression {
            Some(s) => Row::new(Kind::Fact, label, "Unavailable".into()).note(
                s["reason"]
                    .as_str()
                    .unwrap_or("Not supported by this endpoint."),
            ),
            None => {
                let choices = knob_choices(app, key);
                let raw_val = if key == "effort" {
                    super::effort_label(&app.effort_choice(choices.as_deref().unwrap_or(&[])))
                        .to_string()
                } else if key == "thinking" {
                    if app.thinking() {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }
                } else {
                    panels::value_of(&app.settings["settings"], key)
                };
                let disp_val = if choices.is_some() {
                    format!("‹ {} ›", raw_val)
                } else {
                    raw_val
                };
                // What Off does on this route, as the router publishes it.
                let off_reads = matches!(key, "thinking" | "effort")
                    .then(|| app.lookup_model_info(app.provider(), app.model()))
                    .flatten();
                let off_partial = off_reads
                    .as_ref()
                    .is_some_and(|m| m.thinking_off_between_tools_only());
                let off_least = !off_partial
                    && off_reads
                        .as_ref()
                        .is_some_and(|m| m.thinking_off_asks_least());
                let mut help = if off_partial {
                    format!(
                        "{} On this route Off stops thinking between tool calls only; the model still thinks before it answers.",
                        knob["help"].as_str().unwrap_or("")
                    )
                } else if off_least {
                    format!(
                        "{} This route cannot stop thinking: Off asks it for its least thinking.",
                        knob["help"].as_str().unwrap_or("")
                    )
                } else {
                    knob["help"].as_str().unwrap_or("").to_string()
                };
                if key == "effort"
                    && choices
                        .as_ref()
                        .is_some_and(|c| c.iter().any(|x| x == super::ROUTE_DEFAULT))
                {
                    help.push_str(
                        " Route default asks for no level: the route decides whether and how much to think (a level in the model's name included; some routes do not think unless asked).",
                    );
                }
                let mut row =
                    Row::new(Kind::Edit(Field::Knob(key.into())), label, disp_val).help(&help);
                if off_partial {
                    row = row.note("←→ switches and saves · Off: between tools only");
                } else if off_least {
                    row = row.note("←→ switches and saves · Off: least thinking");
                } else if choices.is_some() {
                    row = row.note("←→ switches and saves");
                }
                row
            }
        });
    }
    out.push(Row::new(
        Kind::Fact,
        "AUXILIARY MODELS — SENSITIVE AREA",
        "Every session (rune.toml) · Injudicious changes impact performance, safety, and memory"
            .into(),
    ));
    out.push(Row::new(
        Kind::Fact,
        "Classifier status",
        app.settings["classifier_status"]["reason"]
            .as_str()
            .unwrap_or("Requires CLASSIFIER.md and Auto/Full mode")
            .into(),
    ));
    for field in runeengine::settings::configuration::FIELDS
        .iter()
        .filter(|f| {
            ["classifier.", "compact.", "title.", "sanitizer."]
                .iter()
                .any(|prefix| f.key.starts_with(prefix))
        })
    {
        let value = field
            .key
            .split('.')
            .fold(&app.settings["configuration"], |v, k| &v[k]);
        let role = field.key.split('.').next().unwrap_or("");
        let shown = if value.is_null()
            || value.as_str() == Some("auto")
            || value.as_str() == Some("null")
        {
            if field.key.ends_with(".model") {
                format!(
                    "Inherited · {} / {}",
                    app.provider(),
                    app.settings["settings"]["model"]
                        .as_str()
                        .unwrap_or("selected model")
                )
            } else if field.key.ends_with(".timeout") {
                field.default.trim_matches('"').to_string()
            } else if field.key.ends_with(".max_output_tokens") {
                let (_, m_id, max_out, _) = aux_model_spec(app, role);
                let role_def = serde_json::from_str::<Value>(field.default)
                    .expect("FIELDS default is valid JSON")
                    .as_u64()
                    .expect("output token field has an integer default");
                let effective = max_out.map(|m| role_def.min(m)).unwrap_or(role_def);
                match max_out {
                    Some(m) => format!("Auto · {effective} tokens ({m_id} max: {m})"),
                    None => format!("Auto · {effective} tokens ({m_id})"),
                }
            } else if field.key == "compact.fallback_context_tokens" {
                let (_, m_id, _, ctx) = aux_model_spec(app, "compact");
                match ctx {
                    Some(w) => format!("Auto · {} tokens ({m_id})", thousands(w)),
                    None => format!(
                        "Auto · {} tokens (Default · {m_id})",
                        thousands(
                            serde_json::from_str::<u64>(field.default)
                                .expect("FIELDS count default")
                        )
                    ),
                }
            } else {
                "Auto".to_string()
            }
        } else if field.key.ends_with(".excluded_tools") || field.key.ends_with(".context_tools") {
            if let Some(arr) = value.as_array() {
                if arr.is_empty() {
                    if field.key.ends_with(".context_tools") {
                        "None (no historical tools)"
                    } else {
                        "None (reviews all tools)"
                    }
                    .to_string()
                } else {
                    format!(
                        "{} tools {}",
                        arr.iter().filter(|v| v.is_string()).count(),
                        if field.key.ends_with(".context_tools") {
                            "included in context"
                        } else {
                            "excluded"
                        }
                    )
                }
            } else {
                value.to_string()
            }
        } else if field.key.ends_with(".model") && value.is_object() {
            if let (Some(p), Some(id)) = (
                value.get("provider").and_then(Value::as_str),
                value.get("id").and_then(Value::as_str),
            ) {
                format!("{p}:{id}")
            } else {
                value.to_string()
            }
        } else if let Some(s) = value.as_str() {
            s.into()
        } else {
            value.to_string()
        };
        out.push(
            Row::new(
                Kind::Edit(Field::Global(field.key.into())),
                field.key,
                shown,
            )
            .help(field.help),
        );
    }
    out.push(
        Row::new(
            Kind::ReloadClassifier,
            "Reload classifier policy",
            "[ Reload policy ↵ ]".into(),
        )
        .help("Available when no turn is running. Missing rules never activate the classifier."),
    );
    out
}

fn provider_rows(app: &App) -> Vec<Row> {
    if let Page::Provider(name) = &app.preferences.location.page {
        let Some(p) = provider_list(app)
            .into_iter()
            .find(|p| p["name"].as_str() == Some(name))
        else {
            if service_of(app, name).is_some() {
                return service_page(app, name);
            }
            return vec![Row::new(
                Kind::Fact,
                "Provider unavailable",
                "Return to Connections and refresh.".into(),
            )];
        };
        let is_active = name == app.provider();
        let is_default = p["default"].as_bool().unwrap_or(false);
        let endpoint_url = p["base_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                runeengine::settings::providers::preset(name).map(|pr| pr.build("").base_url)
            })
            .unwrap_or_else(|| "Standard API".to_string());
        let models_cnt = p["models"].as_array().map(|a| a.len()).unwrap_or(0);
        let status_desc = match (is_active, is_default) {
            (true, true) => "Active session · Default fallback",
            (true, false) => "Active session",
            (false, true) => "Configured · Default fallback",
            (false, false) => "Configured on this device",
        };
        let mut out = vec![
            Row::new(Kind::Fact, "ENDPOINT", String::new()),
            Row::new(
                Kind::Edit(Field::BaseUrl(name.clone())),
                "Base URL",
                endpoint_url,
            )
            .help("Where this connection is reached. Saved only after a request there succeeds; Use default puts back the preset's address."),
            Row::new(Kind::Fact, "Status", format!("{status_desc} · {models_cnt} models")),
            credential_row(app, p),
            Row::new(Kind::Edit(Field::Key(name.clone())), "Add API key", "Add key ↵".into())
                .help("Adds a named key to this provider's ring without replacing existing keys."),
        ];
        if let Some(keys) = p["keys_list"].as_array() {
            for k in keys {
                let pos = k["position"].as_u64().unwrap_or(1) as usize;
                let k_name = k["name"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("Key {pos}"));
                let preview = k["preview"].as_str().unwrap_or("••••••••");
                out.push(
                    Row::new(
                        Kind::Edit(Field::RemoveKey(name.clone(), pos)),
                        &k_name,
                        format!("{preview}   remove ↵"),
                    )
                    .help("Removal requires confirmation. Esc keeps this key."),
                );
            }
        } else {
            for n in 1..=p["keys"].as_u64().unwrap_or(0) {
                out.push(
                    Row::new(
                        Kind::Edit(Field::RemoveKey(name.clone(), n as usize)),
                        &format!("Key {n}"),
                        "••••••••   remove ↵".into(),
                    )
                    .help("Removal requires confirmation. Esc keeps this key."),
                );
            }
        }
        out.extend(service_access_rows(app, name, true));
        return out;
    }
    let names = connection_names(app);
    let mut out = Vec::new();
    if let Some(note) = credentials_unavailable(app) {
        out.push(Row::new(
            Kind::Fact,
            "CREDENTIALS",
            format!("Unavailable · {}", store_trouble(app)),
        ));
        out.push(
            Row::new(
                Kind::Edit(Field::NewCredentialStore),
                "Start a new credential store",
                "set the old one aside ↵".into(),
            )
            .help(note),
        );
    }
    out.push(Row::new(
        Kind::Fact,
        "CONNECTIONS",
        format!("{} configured", names.len()),
    ));
    out.push(
        Row::new(
            Kind::Fact,
            "Provider",
            format!("{:^4}    {:^16}", "Keys", "State"),
        )
        .note("Serves"),
    );
    for name in &names {
        let model = provider_list(app)
            .into_iter()
            .find(|p| p["name"].as_str() == Some(name));
        let service = service_of(app, name);
        let (keys, ready, keyless) = match (model, service) {
            (Some(p), _) => (
                p["keys"].as_u64().unwrap_or(0),
                p["ready"].as_u64().unwrap_or(0),
                !p["needs_key"].as_bool().unwrap_or(false),
            ),
            (None, Some(s)) => (
                s["keys"].as_u64().unwrap_or(0),
                s["ready"].as_u64().unwrap_or(0),
                adapter_of(app, s["api"].as_str().unwrap_or(""))
                    .is_some_and(|a| a["key"] != "required"),
            ),
            (None, None) => (0, 0, false),
        };
        let held = model
            .map(|p| &p["key_state"])
            .or(service.map(|s| &s["key_state"]))
            .and_then(Value::as_str);
        let state = match (keys, ready) {
            _ if held == Some("unavailable") => "Unavailable",
            _ if held == Some("missing") => "Key missing",
            (0, _) if keyless => "Keyless",
            (0, _) => "Not configured",
            (_, 0) => "Resting",
            _ => "Configured",
        };
        // Credentials being available is not a successful API health check.
        let badge = format!("{:^4}    {:^16}", keys, state);
        let mut serves = Vec::new();
        if let Some(p) = model {
            serves.push(if name == app.provider() {
                "Models (active)"
            } else {
                "Models"
            });
            if p["default"].as_bool().unwrap_or(false) {
                serves.push("Fallback");
            }
        }
        if let Some(s) = service {
            if !s["search"].is_null() {
                serves.push("Search");
            }
            if !s["fetch"].is_null() {
                serves.push("Fetch");
            }
        }
        out.push(
            Row::new(Kind::Navigate(Page::Provider(name.clone())), name, badge)
                .note(serves.join(" · "))
                .help("Manage account credentials and endpoint addresses. Tool providers are managed in Tools."),
        );
    }
    out.push(
        Row::new(
            Kind::Navigate(Page::AddProvider),
            "Add connection",
            format!(
                "{}    {ACCENT}{:^16}{RESET}",
                " ".repeat(4),
                "+ new connection"
            ),
        )
        .note("Models · Search · Fetch")
        .help("Add a model provider, or a service that searches the web or reads pages for the tools."),
    );
    out
}

/// Every connection, model and service alike, once each, by name.
fn connection_names(app: &App) -> Vec<String> {
    let mut names: Vec<String> = provider_list(app)
        .into_iter()
        .chain(service_list(app))
        .filter_map(|c| c["name"].as_str().map(String::from))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    names
}

fn service_list(app: &App) -> Vec<&Value> {
    app.providers["services"]
        .as_array()
        .into_iter()
        .flatten()
        .collect()
}

fn service_of<'a>(app: &'a App, name: &str) -> Option<&'a Value> {
    app.providers["services"]
        .as_array()?
        .iter()
        .find(|s| s["name"].as_str() == Some(name))
}

fn adapter_of<'a>(app: &'a App, id: &str) -> Option<&'a Value> {
    app.providers["adapters"]
        .as_array()?
        .iter()
        .find(|a| a["id"].as_str() == Some(id))
}

fn service_name(service: Service) -> &'static str {
    match service {
        Service::Search => "search",
        Service::Fetch => "fetch",
    }
}

fn service_title(service: Service) -> &'static str {
    match service {
        Service::Search => "Web search",
        Service::Fetch => "Web fetch",
    }
}

/// The connection serving `service` now, as the engine names it — from the
/// settings description, re-read after every settings change, else from the
/// connection list, re-read after every connection change.
fn serving(app: &App, service: Service) -> String {
    app.settings["web"][service_name(service)]
        .as_str()
        .or_else(|| app.providers["web"][service_name(service)].as_str())
        .unwrap_or("")
        .to_string()
}

/// Take `route` off the routes `name` keeps for `svc`. The route in use stays
/// until another is used or the service is switched off — a service is
/// never left on with no route.
fn forget_route(app: &mut App, d: &mut Driver, name: &str, svc: Service, route: &str) {
    let mut state = ServiceState::of(app, name);
    if state.uses(svc).as_ref().and_then(|u| u.route.as_deref()) == Some(route) {
        *state.uses(svc) = None;
    }
    state.kept(svc).retain(|r| r != route);
    state.send(
        app,
        d,
        &format!(
            "{route} removed from {name}'s {} routes.",
            service_name(svc)
        ),
        "Saving…",
    );
}

/// The routes `name` keeps for `svc`, in the order they were added.
fn kept_routes(app: &App, name: &str, svc: Service) -> Vec<String> {
    service_of(app, name)
        .map(|s| &s["routes"][service_name(svc)])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(String::from))
        .collect()
}

/// The page of a connection that serves only the tools.
fn service_page(app: &App, name: &str) -> Vec<Row> {
    let Some(s) = service_of(app, name) else {
        return Vec::new();
    };
    let api = s["api"].as_str().unwrap_or(name);
    let adapter = adapter_of(app, api);
    let title = adapter.and_then(|a| a["title"].as_str()).unwrap_or(api);
    let split = adapter.is_some_and(|a| a["split"].as_bool().unwrap_or(false));
    let mut out = vec![
        Row::new(Kind::Fact, "ENDPOINT", String::new()),
        Row::new(Kind::Fact, "Service", title.to_string()),
        Row::new(
            Kind::Edit(Field::ServiceUrl(name.into(), Service::Search)),
            if split { "Search URL" } else { "Base URL" },
            s["base_url"].as_str().unwrap_or("").to_string(),
        )
        .help("Where this account is reached. Provider requests are added and checked in Tools; Use default restores the vendor address."),
    ];
    if split {
        out.push(
            Row::new(
                Kind::Edit(Field::ServiceUrl(name.into(), Service::Fetch)),
                "Fetch URL",
                s["fetch_url"].as_str().unwrap_or("").to_string(),
            )
            .help("Where page reading is reached; this vendor serves it on a host of its own."),
        );
    }

    let needs_key = adapter.is_some_and(|a| a["key"] != "none");
    if needs_key {
        out.push(credential_row(app, s));
        out.push(
            Row::new(
                Kind::Edit(Field::Key(name.into())),
                "Add API key",
                "Add key ↵".into(),
            )
            .help("Adds a named key to this connection's ring; a refused key rotates to the next."),
        );
        for k in s["keys_list"].as_array().into_iter().flatten() {
            let pos = k["position"].as_u64().unwrap_or(1) as usize;
            let k_name = k["name"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("Key {pos}"));
            let preview = k["preview"].as_str().unwrap_or("••••••••");
            out.push(
                Row::new(
                    Kind::Edit(Field::RemoveKey(name.into(), pos)),
                    &k_name,
                    format!("{preview}   remove ↵"),
                )
                .help("Removal requires confirmation. Esc keeps this key."),
            );
        }
    }
    out.push(Row::new(Kind::Fact, "REMOVE", String::new()));
    out.push(
        Row::new(
            Kind::Edit(Field::RemoveService(name.into())),
            "Remove connection",
            "Remove ↵".into(),
        )
        .help("Removes this connection; its keys stay in the vault. Esc keeps it."),
    );
    out
}

/// The rows of a search/fetch connection being set up.
fn service_setup_rows(app: &App, d: &ProviderDraft) -> Vec<Row> {
    let adapter = adapter_of(app, &d.preset);
    let key = adapter
        .and_then(|a| a["key"].as_str())
        .unwrap_or("required");
    let own_server = adapter.is_some_and(|a| a["base_url"].is_null());
    let mut out = vec![Row::new(
        Kind::Edit(Field::SetupUrl),
        "Base URL",
        if !d.url.is_empty() {
            format!("[ {} ↵ ]", d.url)
        } else if own_server {
            "[ Set your server's address ↵ ]".into()
        } else {
            format!(
                "{} (default)",
                adapter.and_then(|a| a["base_url"].as_str()).unwrap_or("")
            )
        },
    )
    .help(if own_server {
        "The address of your own server, e.g. http://localhost:8888."
    } else {
        "Leave as is for the vendor's address; set one only for a proxy of your own."
    })];
    let vault_keys = adapter.and_then(|a| a["keys"].as_u64()).unwrap_or(0);
    if key != "none" {
        out.push(
            Row::new(
                Kind::Edit(Field::SetupKeyName),
                "Key name",
                format!(
                    "[ {} ↵ ]",
                    if d.key_name.is_empty() {
                        "Primary"
                    } else {
                        &d.key_name
                    }
                ),
            )
            .help("Label to distinguish this key in the connection's key ring."),
        );
        out.push(
            Row::new(
                Kind::Edit(Field::SetupKey),
                "API key",
                if !d.key.is_empty() {
                    "••••••••  [ edit ↵ ]".into()
                } else if vault_keys > 0 {
                    format!(
                        "[ {vault_keys} key{} in the vault ↵ ]",
                        if vault_keys == 1 { "" } else { "s" }
                    )
                } else if key == "optional" {
                    "[ Optional ↵ ]".into()
                } else {
                    "[ Set API key ↵ ]".into()
                },
            )
            .help(&if vault_keys > 0 {
                format!(
                    "The vault still holds the key{} {} had: it is used unless you give a new one, which replaces {}.",
                    if vault_keys == 1 { "" } else { "s" },
                    d.name,
                    if vault_keys == 1 { "it" } else { "them" }
                )
            } else {
                adapter
                    .and_then(|a| a["key_hint"].as_str())
                    .unwrap_or("API key for this service")
                    .to_string()
            }),
        );
    }
    out.push(
        Row::new(Kind::Connect, "Save account", "Save ↵".into()).help(
            "Stores endpoint and credentials only. Add and prove search/fetch providers in Tools.",
        ),
    );
    out
}

/// A connection's services as the screen holds them, to change one thing and
/// send the whole wanted state (`service.save`).
struct ServiceState {
    name: String,
    api: String,
    base_url: Option<String>,
    fetch_url: Option<String>,
    search: Option<runeengine::proto::ServiceUse>,
    fetch: Option<runeengine::proto::ServiceUse>,
    routes: runeengine::proto::ServiceRoutes,
    zone: Option<String>,
}

impl ServiceState {
    fn of(app: &App, name: &str) -> ServiceState {
        let s = service_of(app, name);
        let text = |key: &str| {
            s.and_then(|s| s[key].as_str())
                .filter(|t| !t.is_empty())
                .map(String::from)
        };
        let used = |svc: Service| {
            s.map(|s| &s[service_name(svc)])
                .filter(|u| !u.is_null())
                .map(|u| runeengine::proto::ServiceUse {
                    route: u["route"].as_str().map(String::from),
                })
        };
        ServiceState {
            name: name.to_string(),
            api: text("api")
                .or_else(|| web_ui::account_api(app, name).map(String::from))
                .unwrap_or_else(|| name.to_string()),
            base_url: text("base_url_set"),
            fetch_url: text("fetch_url_set"),
            search: used(Service::Search),
            fetch: used(Service::Fetch),
            routes: runeengine::proto::ServiceRoutes {
                search: kept_routes(app, name, Service::Search),
                fetch: kept_routes(app, name, Service::Fetch),
            },
            zone: text("zone"),
        }
    }

    fn uses(&mut self, service: Service) -> &mut Option<runeengine::proto::ServiceUse> {
        match service {
            Service::Search => &mut self.search,
            Service::Fetch => &mut self.fetch,
        }
    }

    fn kept(&mut self, service: Service) -> &mut Vec<String> {
        match service {
            Service::Search => &mut self.routes.search,
            Service::Fetch => &mut self.routes.fetch,
        }
    }

    fn send(self, app: &mut App, d: &mut Driver, done: &str, busy: &str) {
        send(app, d, Operation::Service(done.into()), busy, |id| {
            Command::ServiceSave {
                id,
                name: self.name,
                api: Some(self.api),
                base_url: self.base_url,
                fetch_url: self.fetch_url,
                search: self.search,
                fetch: self.fetch,
                routes: Some(self.routes),
                zone: self.zone,
                key_name: None,
                key: None,
            }
        });
    }
}

/// Ask a vendor what it offers (`service.options`), then open `then` with it.
fn ask_options(
    app: &mut App,
    d: &mut Driver,
    name: &str,
    api: &str,
    base_url: Option<String>,
    key: Option<String>,
    then: Field,
) {
    let (name, api) = (name.to_string(), api.to_string());
    send(
        app,
        d,
        Operation::Options(then),
        "Asking what it offers…",
        |id| Command::ServiceOptions {
            id,
            name,
            api: Some(api),
            base_url,
            key,
        },
    );
}

/// Open the editor for `field`, from its row when the page shows one.
fn edit_now(app: &mut App, field: Field, label: &str, help: &str) {
    let row = rows(app)
        .into_iter()
        .find(|r| r.kind == Kind::Edit(field.clone()))
        .unwrap_or_else(|| Row::new(Kind::Edit(field.clone()), label, String::new()).help(help));
    open_editor(app, field, &row);
}

/// Take the service setup one step further: ask for what is still missing,
/// or check and save it.
fn connect_service(app: &mut App, d: &mut Driver) {
    let Some(draft) = app.preferences.setup.as_ref() else {
        return;
    };
    let adapter = adapter_of(app, &draft.preset)
        .cloned()
        .unwrap_or(Value::Null);
    let key = adapter["key"].as_str().unwrap_or("required").to_string();
    let title = adapter["title"]
        .as_str()
        .unwrap_or(&draft.preset)
        .to_string();
    let (name, preset) = (draft.name.clone(), draft.preset.clone());
    let url = (!draft.url.trim().is_empty()).then(|| draft.url.trim().to_string());
    let secret = (!draft.key.trim().is_empty()).then(|| draft.key.trim().to_string());
    if adapter["base_url"].is_null() && url.is_none() {
        edit_now(
            app,
            Field::SetupUrl,
            "Base URL",
            "The address of your own server.",
        );
        return;
    }
    let vault_keys = adapter["keys"].as_u64().unwrap_or(0);
    if key == "required" && secret.is_none() && vault_keys == 0 {
        edit_now(app, Field::SetupKey, "API key", "");
        return;
    }
    let key_name = (!draft.key_name.trim().is_empty()).then(|| draft.key_name.trim().to_string());
    send(
        app,
        d,
        Operation::Connected(name.clone()),
        &format!("Saving {title} account…"),
        |id| Command::ServiceAccount {
            id,
            name,
            api: Some(preset),
            base_url: url,
            fetch_url: None,
            key_name,
            key: secret,
        },
    );
}

/// What a route/zone editor offers: what the vendor listed, and for routes a
/// way to type one it did not.
fn offered(app: &App, field: &Field) -> Vec<String> {
    if !matches!(field, Field::ServiceZone(_)) {
        return Vec::new();
    }
    app.preferences.offered["zones"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|z| z.as_str().map(String::from))
        .collect()
}

fn runtime_rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();
    out.push(Row::new(
        Kind::Fact,
        "MODEL REQUEST TIMEOUTS",
        String::new(),
    ));
    for provider in provider_list(app) {
        if let Some(name) = provider["name"].as_str() {
            out.push(
                Row::new(
                    Kind::Edit(Field::Timeout(name.into())),
                    name,
                    provider["timeout"]
                        .as_u64()
                        .map(|n| format!("{n}s"))
                        .unwrap_or_else(|| "Engine default".into()),
                )
                .help(
                    "Request timeout for this model connection. Empty restores the engine default.",
                ),
            );
        }
    }
    out.push(Row::new(Kind::Fact, "RETRY & RESILIENCE", String::new()));
    for (key, label, note) in [
        (
            "retry.max_attempts",
            "Retries",
            "Attempts after a failed request",
        ),
        (
            "retry.initial_delay",
            "Initial step",
            "Retry k waits step × k",
        ),
        (
            "retry.max_delay",
            "Max delay",
            "Past it the series restarts",
        ),
        (
            "retry.credit_cooldown",
            "Credit cooldown",
            "Key cooldown after insufficient balance",
        ),
    ] {
        if let Some(row) = tool_config_row(app, key, label, note) {
            out.push(row);
        }
    }
    out.push(Row::new(
        Kind::Fact,
        "NETWORK & STREAM TIMEOUTS",
        String::new(),
    ));
    for (key, label, note) in [
        (
            "network.stream_idle_timeout",
            "Stall limit",
            "No data this long → retry",
        ),
        (
            "network.connect_timeout",
            "Connect timeout",
            "Connection establishment timeout",
        ),
        (
            "network.probe_timeout",
            "Probe timeout",
            "Explicit provider test timeout",
        ),
    ] {
        if let Some(row) = tool_config_row(app, key, label, note) {
            out.push(row);
        }
    }
    out
}

fn screen_rows(app: &App) -> Vec<Row> {
    if app.preferences.location.page == Page::Themes {
        return [
            theme::ThemeMode::Violet,
            theme::ThemeMode::Cyan,
            theme::ThemeMode::Amber,
        ]
        .into_iter()
        .map(|t| {
            Row::new(
                Kind::Theme(t),
                t.label(),
                if app.theme == t {
                    "Active"
                } else {
                    "Enter to apply"
                }
                .into(),
            )
            .help("Saved on this device. Arrows browse; Enter applies the selected theme.")
        })
        .collect();
    }
    vec![
        Row::new(
            Kind::Navigate(Page::Themes),
            "Color theme",
            format!("‹ {} ›", app.theme.label()),
        )
        .note("←→ switches and saves")
        .help("Left/Right changes theme immediately. Enter opens the theme list."),
        Row::new(
            Kind::Sidebar,
            "Conversation side panel",
            on_off(app.sidebar),
        )
        .note("Saved workspace setting")
        .help(
            "Show context, output, cache and spend beside the conversation. Saved across sessions.",
        ),
    ]
}

fn tool_config_row(app: &App, key: &str, label: &str, note: &str) -> Option<Row> {
    let field = runeengine::settings::configuration::FIELDS
        .iter()
        .find(|f| f.key == key)?;
    let value = field
        .key
        .split('.')
        .fold(&app.settings["configuration"], |v, k| &v[k]);
    let shown =
        if value.is_null() || value.as_str() == Some("auto") || value.as_str() == Some("null") {
            field.default.trim_matches('"').to_string()
        } else if let Some(b) = value.as_bool() {
            on_off(b)
        } else if let Some(s) = value.as_str() {
            if s.eq_ignore_ascii_case("unlimited") {
                "UNLIMITED".into()
            } else {
                s.to_string()
            }
        } else {
            value.to_string()
        };
    let shown = if shown.eq_ignore_ascii_case("unlimited") {
        "UNLIMITED".into()
    } else {
        shown
    };
    Some(
        Row::new(Kind::Edit(Field::Global(field.key.into())), label, shown)
            .note(note)
            .help(field.help),
    )
}

/// The choice that leaves a web tool unpinned, in the words of what then
/// serves it.
fn pin_default(service: Service) -> &'static str {
    match service {
        Service::Search => "Default — the first switched on",
        Service::Fetch => "Default — Rune's built-in reader",
    }
}

/// Who serves a web tool: the connection pinned in `web_*.provider`, or the
/// default — the first search connection added, the built-in page reader.
fn web_provider_row(app: &App, service: Service) -> Row {
    let key = match service {
        Service::Search => "web_search.provider",
        Service::Fetch => "web_fetch.provider",
    };
    let pinned = key
        .split('.')
        .fold(&app.settings["configuration"], |v, k| &v[k])
        .as_str()
        .map(String::from);
    let now = serving(app, service);
    let unavailable = now.starts_with("unavailable");
    let shown = match (pinned, service) {
        (Some(name), _) if unavailable => format!("{name} (unavailable)"),
        (Some(name), _) => name,
        (None, Service::Search) if unavailable => "First added · none yet".into(),
        (None, Service::Search) => format!("First added · {now}"),
        (None, Service::Fetch) => "Built-in reader".into(),
    };
    let help = runeengine::settings::configuration::FIELDS
        .iter()
        .find(|f| f.key == key)
        .map(|f| f.help)
        .unwrap_or("");
    // No note: a connection's name does not fit the narrow value column the
    // numeric rows beside it use, and it is the whole point of the row.
    Row::new(
        Kind::Navigate(Page::WebProviders { service }),
        "Provider",
        shown,
    )
    .help(help)
}

fn tools_rows(app: &App) -> Vec<Row> {
    let mut out = Vec::new();

    // Group 1: Terminal Use
    out.push(Row::new(
        Kind::Fact,
        "TERMINAL (terminal_use)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "terminal.command_timeout",
        "Command timeout",
        "Foreground wait → background",
    ) {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "terminal.max_output_chars",
        "Max output characters",
        "Model output ceiling",
    ) {
        out.push(r);
    }

    for (key, label) in [
        ("terminal.truncate_head_bytes", "Truncate head bytes"),
        ("terminal.truncate_tail_bytes", "Truncate tail bytes"),
    ] {
        if let Some(r) = tool_config_row(app, key, label, "Oversized output preview") {
            out.push(r);
        }
    }
    if let Some(r) = tool_config_row(
        app,
        "terminal.max_background_log_size",
        "Background log size",
        "Disk limit per background task",
    ) {
        out.push(r);
    }

    // Group 2: Inspect Files
    out.push(Row::new(
        Kind::Fact,
        "INSPECT FILES (inspect_file)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "inspect_file.max_file_size",
        "Max file size",
        "Disk read limit",
    ) {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "inspect_file.max_output_chars",
        "Max output characters",
        "Text content ceiling",
    ) {
        out.push(r);
    }

    // Group 3: Find Files
    out.push(Row::new(
        Kind::Fact,
        "FIND FILES (find_files)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(app, "find_files.matches", "Matches", "Search match ceiling") {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "find_files.max_output_chars",
        "Max output characters",
        "Output text budget",
    ) {
        out.push(r);
    }

    // Group 4: Search Text
    out.push(Row::new(
        Kind::Fact,
        "SEARCH TEXT (search_text)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "search_text.matches",
        "Matches",
        "Search match ceiling",
    ) {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "search_text.max_line_chars",
        "Max line characters",
        "Long line truncation",
    ) {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "search_text.max_output_chars",
        "Max output characters",
        "Search text output budget",
    ) {
        out.push(r);
    }

    // The walk both searches share: how far it goes, and the fuse on a
    // pattern that costs more than a line is worth.
    out.push(Row::new(
        Kind::Fact,
        "FILE SCAN (find_files, search_text)",
        String::new(),
    ));
    for (key, label, note) in [
        (
            "scan.max_entries",
            "Files per search",
            "Work limit per search",
        ),
        ("scan.max_depth", "Folder depth", "Deepest folder searched"),
        (
            "scan.regex_steps",
            "Pattern step fuse",
            "Regex steps per line",
        ),
    ] {
        if let Some(r) = tool_config_row(app, key, label, note) {
            out.push(r);
        }
    }

    // Group 5: Manage Process
    out.push(Row::new(
        Kind::Fact,
        "MANAGE PROCESS (manage_process)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "manage_process.max_output_chars",
        "Max output characters",
        "Process output budget",
    ) {
        out.push(r);
    }

    for (key, label, note) in [
        (
            "manage_process.truncate_head_bytes",
            "Truncate head bytes",
            "Oversized output preview",
        ),
        (
            "manage_process.truncate_tail_bytes",
            "Truncate tail bytes",
            "Oversized output preview",
        ),
    ] {
        if let Some(r) = tool_config_row(app, key, label, note) {
            out.push(r);
        }
    }

    // Group 6: Web Search
    out.push(Row::new(
        Kind::Fact,
        "WEB SEARCH (web_search)",
        String::new(),
    ));
    out.push(web_provider_row(app, Service::Search));
    if let Some(r) = tool_config_row(
        app,
        "web_search.max_output_chars",
        "Max output characters",
        "Search output budget",
    ) {
        out.push(r);
    }
    if let Some(r) = tool_config_row(app, "web_search.timeout", "Search timeout", "API request") {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "web_search.results",
        "Search results",
        "Returned links ceiling",
    ) {
        out.push(r);
    }

    // Web Fetch owns its page limits, separately from search results.
    out.push(Row::new(Kind::Fact, "WEB FETCH (web_fetch)", String::new()));
    out.push(web_provider_row(app, Service::Fetch));
    for (key, label, note) in [
        (
            "web_fetch.max_output_chars",
            "Max output characters",
            "Page content ceiling",
        ),
        ("web_fetch.timeout", "Fetch timeout", "Public page request"),
    ] {
        if let Some(r) = tool_config_row(app, key, label, note) {
            out.push(r);
        }
    }

    // Group 7: Discover Capabilities
    out.push(Row::new(
        Kind::Fact,
        "DISCOVER CAPABILITIES (discover_capabilities)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "discover_capabilities.max_output_chars",
        "Max output characters",
        "Discovery output budget",
    ) {
        out.push(r);
    }

    // Group 8: Read MCP Resource
    out.push(Row::new(
        Kind::Fact,
        "READ MCP RESOURCE (read_mcp_resource)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "read_mcp_resource.max_output_chars",
        "Max output characters",
        "Resource output budget",
    ) {
        out.push(r);
    }

    // Group 9: Track Tasks
    out.push(Row::new(
        Kind::Fact,
        "TRACK TASKS (track_tasks)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(app, "tasks.max_tasks", "Max tasks", "Tasks ceiling") {
        out.push(r);
    }
    if let Some(r) = tool_config_row(
        app,
        "tasks.max_chars_per_task",
        "Max chars per task",
        "Task description limit",
    ) {
        out.push(r);
    }

    // Group 10: Execution & Orchestration
    out.push(Row::new(
        Kind::Fact,
        "EXECUTION & ORCHESTRATION",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "tools.max_actions_per_response",
        "Max actions per turn",
        "Calls & batch items",
    ) {
        out.push(r);
    }

    // Group 11: User Questions
    out.push(Row::new(
        Kind::Fact,
        "USER QUESTIONS (ask_user_questions)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "questions.timeout",
        "Questions wait timeout",
        "UNLIMITED = no timeout",
    ) {
        out.push(r);
    }

    // Group 12: Permissions
    out.push(Row::new(Kind::Fact, "PERMISSIONS", String::new()));
    if let Some(r) = tool_config_row(
        app,
        "permissions.timeout",
        "Permission wait timeout",
        "UNLIMITED = no timeout",
    ) {
        out.push(r);
    }

    // Group 13: Project Instructions
    out.push(Row::new(
        Kind::Fact,
        "PROJECT INSTRUCTIONS (AGENTS.md)",
        String::new(),
    ));
    if let Some(r) = tool_config_row(
        app,
        "instructions.max_chars",
        "Instruction budget",
        "Character cap per turn",
    ) {
        out.push(r);
    }

    out
}

fn on_off(value: bool) -> String {
    if value { "On" } else { "Off" }.into()
}
/// The credential store's trouble, when it has one — what every key screen
/// says, and why no key can be added or removed now (owner, 2026-10-16: what
/// is shown is what is on disk).
fn credentials_unavailable(app: &App) -> Option<&str> {
    (app.providers["credentials"]["state"] == "unavailable").then(|| {
        app.providers["credentials"]["note"]
            .as_str()
            .unwrap_or("Rune's credential store cannot be opened")
    })
}

/// A few words for why the store cannot be opened, for a value column.
fn store_trouble(app: &App) -> &'static str {
    match app.providers["credentials"]["cause"].as_str() {
        Some("key_missing") => "master.key missing",
        Some("key_damaged") => "master.key damaged",
        Some("mismatch") => "store does not match master.key",
        Some("truncated") => "store cut off",
        Some("content") => "store does not read",
        _ => "store cannot be read",
    }
}

/// The API CREDENTIALS row of a connection: its ring when the key is there,
/// and plainly missing or unavailable — with why — when it is not.
fn credential_row(app: &App, entry: &Value) -> Row {
    let value = match entry["key_state"].as_str() {
        Some("missing") if app.providers["credentials"]["state"] == "absent" => {
            "Key missing · secrets.vault not found".to_string()
        }
        Some("missing") => "Key missing · not in the credential store".to_string(),
        Some("unavailable") => format!("Unavailable · {}", store_trouble(app)),
        _ => ring(
            entry["keys"].as_u64().unwrap_or(0),
            entry["ready"].as_u64().unwrap_or(0),
        ),
    };
    let row = Row::new(Kind::Fact, "API CREDENTIALS", value);
    match entry["key_note"].as_str() {
        Some(note) if entry["key_state"] != "present" => row.help(note),
        _ => row,
    }
}

fn ring(keys: u64, ready: u64) -> String {
    match (keys, keys.saturating_sub(ready)) {
        (0, _) => "No key configured".into(),
        (n, 0) => format!(
            "{n} key{} configured · not checked",
            if n == 1 { "" } else { "s" }
        ),
        (n, resting) => format!("{n} keys · {resting} resting"),
    }
}

// Targets are generated with the frame, then clipped with the same viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Provider(usize),
    ModelLine {
        row: Option<usize>,
        provider: Option<usize>,
        split: usize,
    },
    Tabs {
        first: usize,
        last: usize,
    },
    Row(usize),
    Cards {
        left: usize,
        right: Option<usize>,
        split: usize,
    },
    Search,
    Text {
        row: usize,
        editor: Option<u64>,
    },
    Back,
    Apply,
    Cancel,
    Reset,
    Choice(usize),
}

fn tab_label(index: usize) -> String {
    format!(" {} ", SECTIONS[index].0)
}

/// Plain external text cannot inject terminal controls into trusted ANSI markup.
fn safe(text: &str) -> String {
    ansi::plain(text)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

fn field_guidance(app: &App, field: &Field) -> Option<String> {
    match field {
        Field::Global(key) => {
            let field_kind = runeengine::settings::configuration::FIELDS
                .iter()
                .find(|f| f.key == key)
                .map(|f| f.kind);

            if let Some(kind) = field_kind {
                match kind {
                    runeengine::settings::configuration::Kind::Bool => {
                        Some("Space / ← → to toggle · Enter to apply.".into())
                    }
                    runeengine::settings::configuration::Kind::Duration => {
                        Some("Units: m, s, ms (or menit, detik) · Supports mixed e.g. '3m 24s 99ms' · Empty or 'auto' inherits default.".into())
                    }
                    runeengine::settings::configuration::Kind::Size => {
                        Some("Units: KiB, MiB, B · Supports mixed e.g. '1MiB 512KiB' · Plain number defaults to KiB.".into())
                    }
                    runeengine::settings::configuration::Kind::Count(min, max) => {
                        Some(format!("Range: {min} – {max} (raw integer count)."))
                    }
                    _ => {
                        if key.ends_with(".model") {
                            Some("Format: {provider}:{modelID} (e.g. mimo:mimo-v2.5-pro) · Empty or 'auto' inherits active model.".into())
                        } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                            Some(if key.ends_with(".context_tools") { "Press Space to toggle tools · Checked tools may supply the last three historical inputs, independently of review exclusions." } else { "Press Space to toggle tools · Checked [✓] tools are excluded from model classifier review." }.into())
                        } else {
                            None
                        }
                    }
                }
            } else if key.ends_with(".model") {
                Some("Format: {provider}:{modelID} (e.g. mimo:mimo-v2.5-pro) · Empty or 'auto' inherits active model.".into())
            } else if key.ends_with(".timeout") {
                Some("Units: m, s, ms (or menit, detik) · Supports mixed e.g. '3m 24s 99ms' · Empty or 'auto' inherits default.".into())
            } else if key.ends_with("_size") {
                Some("Units: KiB, MiB, B · Supports mixed e.g. '1MiB 512KiB' · Plain number defaults to KiB.".into())
            } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                Some(if key.ends_with(".context_tools") { "Press Space to toggle tools · Checked tools may supply the last three historical inputs, independently of review exclusions." } else { "Press Space to toggle tools · Checked [✓] tools are excluded from model classifier review." }.into())
            } else if key.ends_with("_chars") {
                Some("Character count limit.".into())
            } else {
                None
            }
        }
        Field::Knob(key) if key == "max_output" => {
            let ceiling = app.settings["models"]
                .as_array()
                .and_then(|arr| arr.iter().find(|m| m["id"].as_str() == Some(app.model())).and_then(|m| m["max_output"].as_u64()))
                .or_else(|| app.settings["max_output_ceiling"].as_u64());
            if let Some(c) = ceiling {
                Some(format!("Active model: {} · API max output: {c} tokens · Enter 'auto' or 1–{c}.", app.model()))
            } else {
                Some(format!("Active model: {} · Enter token limit or 'auto' for provider default.", app.model()))
            }
        }
        Field::Key(_) => Some("Give this key a label to distinguish its quota, project, or role (e.g. Work, Testing, Personal).".into()),
        Field::KeyValue { .. } => Some("The raw API secret key is stored encrypted in ~/.rune/secrets.vault (mode 0600); the master key lives in ~/.rune/master.key.".into()),
        Field::Timeout(_) => Some("Timeout in seconds (e.g. 30, 60, 120) or mixed (e.g. 3m 20s) · 'auto' inherits default.".into()),
        Field::BaseUrl(_) | Field::ServiceUrl(..) | Field::SetupUrl => Some("https:// for any host; plain http:// only for this machine (or a keyless server of your own). Saved only after a request there succeeds.".into()),
        Field::ServiceRouteText(..) => Some("Type the provider route. The field starts empty; a real request must succeed before it is kept.".into()),
        _ => None,
    }
}

fn field_placeholder(app: &App, field: &Field) -> Option<String> {
    match field {
        Field::Global(key) => {
            if key.ends_with(".model") {
                Some("e.g. mimo:mimo-v2.5-pro or 'auto'".into())
            } else if key.ends_with(".timeout") || key.ends_with("_timeout") {
                Some("e.g. 3m 24s 99ms, 180s, or 'auto'".into())
            } else if key.ends_with("_size") {
                Some("e.g. 128KiB, 1MiB (digits default to KiB)".into())
            } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                Some("Space to toggle items".into())
            } else if key == "tools.max_actions_per_response" {
                Some("e.g. 8 (1 – 100 actions)".into())
            } else if key.ends_with(".max_output_tokens") {
                let role = key.split('.').next().unwrap_or("");
                let (_, _, max_out, _) = aux_model_spec(app, role);
                if let Some(c) = max_out {
                    Some(format!("'auto' or 1–{c} tokens"))
                } else {
                    Some("'auto' or tokens".into())
                }
            } else if key == "compact.keep_recent_turns" {
                Some("e.g. 4 turns".into())
            } else if key == "compact.fallback_context_tokens" {
                let (_, _, _, ctx) = aux_model_spec(app, "compact");
                if let Some(w) = ctx {
                    Some(format!("'auto' ({} tokens) or manual", thousands(w)))
                } else {
                    Some("'auto' or tokens".into())
                }
            } else if key.ends_with("_chars") {
                Some("e.g. 60, 4000 characters".into())
            } else {
                None
            }
        }
        Field::Knob(key) if key == "max_output" => Some("auto (provider default) or tokens".into()),
        Field::Key(_) => Some("e.g. Work Account, Testing Key, Personal".into()),
        Field::KeyValue { .. } => Some("e.g. sk-..., AIzaSy..., or provider key string".into()),
        Field::BaseUrl(_) | Field::ServiceUrl(..) => Some("empty: the default address".into()),
        Field::SetupUrl => Some("e.g. http://localhost:8888".into()),
        Field::ServiceRouteText(..) => None,
        _ => None,
    }
}

fn format_duration_summary(total_ms: u64) -> (String, String) {
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let hours = total_mins / 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if secs > 0 {
        parts.push(format!("{secs}s"));
    }
    if ms > 0 {
        parts.push(format!("{ms}ms"));
    }
    if parts.is_empty() {
        parts.push("0s".to_string());
    }
    let main_str = parts.join(" ");

    let sec_str = if ms > 0 && total_secs > 0 {
        let frac = format!("{ms:03}").trim_end_matches('0').to_string();
        format!("{total_secs}.{frac}s")
    } else if total_secs > 0 {
        format!("{total_secs}s")
    } else {
        format!("{ms}ms")
    };

    (main_str, sec_str)
}

fn duration_breakdown_line(text: &str, is_inherited: bool, default_val: Option<&str>) -> String {
    let trimmed = text.trim();
    let total_line = if trimmed.eq_ignore_ascii_case("unlimited") {
        format!("{TEXT}{BOLD}UNLIMITED{RESET} {MUTED}(No timeout){RESET}")
    } else if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("auto")
        || trimmed.eq_ignore_ascii_case("inherit")
    {
        if is_inherited {
            format!("{MUTED}Inherited · Provider request timeout{RESET}")
        } else if default_val.is_some_and(|value| value.eq_ignore_ascii_case("unlimited")) {
            format!("{TEXT}{BOLD}UNLIMITED{RESET} {MUTED}(Default · No timeout){RESET}")
        } else if let Some(def) =
            default_val.and_then(runeengine::settings::configuration::parse_duration_sum)
        {
            let (main_str, sec_str) = format_duration_summary(def);
            format!("{TEXT}{BOLD}{main_str}{RESET} {MUTED}(Default · {sec_str}){RESET}")
        } else {
            format!("{MUTED}0s{RESET}")
        }
    } else if let Some(total_ms) = runeengine::settings::configuration::parse_duration_sum(trimmed)
    {
        let (main_str, sec_str) = format_duration_summary(total_ms);
        format!("{TEXT}{BOLD}{main_str}{RESET} {MUTED}({sec_str}){RESET}")
    } else {
        format!("{MUTED}Waiting for valid duration (e.g. 3m 24s 99ms, 180s){RESET}")
    };

    format!("  {ACCENT}{BOLD}Segmented breakdown:{RESET} {total_line}")
}

fn format_size_summary(total_bytes: u64) -> (String, String) {
    let b = total_bytes % 1024;
    let total_kib = total_bytes / 1024;
    let kib = total_kib % 1024;
    let total_mib = total_kib / 1024;
    let mib = total_mib % 1024;
    let gib = total_mib / 1024;

    let mut parts = Vec::new();
    if gib > 0 {
        parts.push(format!("{gib}GiB"));
    }
    if mib > 0 {
        parts.push(format!("{mib}MiB"));
    }
    if kib > 0 {
        parts.push(format!("{kib}KiB"));
    }
    if b > 0 {
        parts.push(format!("{b}B"));
    }
    if parts.is_empty() {
        parts.push("0B".to_string());
    }
    let main_str = parts.join(" ");
    let bytes_str = format!("{total_bytes} bytes");
    (main_str, bytes_str)
}

fn size_breakdown_line(text: &str, default_val: Option<&str>, max_bytes: Option<u64>) -> String {
    let trimmed = text.trim();
    let total_line = if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("auto")
        || trimmed.eq_ignore_ascii_case("inherit")
    {
        if let Some(def) = default_val.and_then(runeengine::settings::configuration::parse_size_sum)
        {
            let (main_str, bytes_str) = format_size_summary(def);
            format!("{TEXT}{BOLD}{main_str}{RESET} {MUTED}(Default · {bytes_str}){RESET}")
        } else {
            format!("{MUTED}0B{RESET}")
        }
    } else if let Some(total_bytes) = runeengine::settings::configuration::parse_size_sum(trimmed) {
        if let Some(max) = max_bytes {
            if total_bytes > max {
                let max_mib = max / (1024 * 1024);
                format!("{REMOVED}{BOLD}Exceeds maximum size limit of {max_mib}MiB{RESET}")
            } else {
                let (main_str, bytes_str) = format_size_summary(total_bytes);
                format!("{TEXT}{BOLD}{main_str}{RESET} {MUTED}({bytes_str}){RESET}")
            }
        } else {
            let (main_str, bytes_str) = format_size_summary(total_bytes);
            format!("{TEXT}{BOLD}{main_str}{RESET} {MUTED}({bytes_str}){RESET}")
        }
    } else {
        format!("{MUTED}Waiting for valid size (e.g. 128KiB, 16MiB){RESET}")
    };

    format!("  {ACCENT}{BOLD}Segmented breakdown:{RESET} {total_line}")
}

fn toggle_tool(text: &mut String, tool: &str) {
    let mut current: Vec<String> = text
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(pos) = current.iter().position(|s| s == tool) {
        current.remove(pos);
    } else {
        current.push(tool.to_string());
    }
    *text = current.join(", ");
}

/// Responsive geometry is shared by the renderer and keyboard navigation.
fn overview_columns(width: usize, height: usize) -> usize {
    if width >= 72 && height >= 32 {
        2
    } else {
        1
    }
}

/// A single boxed text field for both search and editing. Cursor coordinates
/// refer to the returned lines; masking is done before measuring or slicing.
fn input_box(
    label: &str,
    text: &str,
    cursor: usize,
    secret: bool,
    selected: Option<std::ops::Range<usize>>,
    placeholder: &str,
    width: usize,
) -> (Vec<String>, (usize, usize), super::editor::Row) {
    let projection = if secret {
        super::editor::Projection::masked(text)
    } else {
        super::editor::Projection::single_line(text)
    };
    let available = width.saturating_sub(7).max(1);
    // Two columns outside the box and four inside it: text begins at column 7.
    let (geometry, col) = projection.window(cursor, available, 7);
    let value = if text.is_empty() {
        format!("{MUTED}{}{RESET}", ansi::truncate(placeholder, available))
    } else {
        format!("{TEXT}{}{RESET}", geometry.styled(selected))
    };
    let line = format!("{ACCENT}› {value}{RESET}");
    (
        vec![
            super::draw::box_top(&ansi::truncate(label, width.saturating_sub(6)), width),
            super::draw::boxed(&line, width.saturating_sub(4)),
            super::draw::box_bottom(width),
        ],
        (1, col - 3),
        geometry,
    )
}

/// The screen owns its viewport. Conversation scroll is never used by Settings.
pub fn render(app: &App, width: usize, height: usize) -> (Sink, Option<(usize, usize)>) {
    let (sink, cursor, _) = layout(app, width, height);
    (sink, cursor)
}

pub fn render_interactive(
    app: &mut App,
    width: usize,
    height: usize,
) -> (Sink, Option<(usize, usize)>) {
    let (sink, cursor, geometry) = layout(app, width, height);
    app.preferences.text_rows = geometry;
    (sink, cursor)
}

fn layout(
    app: &App,
    width: usize,
    height: usize,
) -> (Sink, Option<(usize, usize)>, Vec<super::editor::Row>) {
    let mut text_rows = Vec::new();
    let width = width.max(20);
    let height = height.max(8);
    let inner = width.saturating_sub(4);
    let state = &app.preferences;
    let page = &state.location.page;
    let mut head = Sink::default();
    let mut cursor = None;
    if width >= 48 && ((page == &Page::Overview && height >= 32) || height >= 40) {
        for (i, pixels) in super::banner::logo(app.theme).into_iter().enumerate() {
            let text = match i {
                2 => format!("{ACCENT}{BOLD}Rune CLI{RESET}  {MUTED}/ SETTINGS{RESET}"),
                3 => format!("{TEXT}Workspace preferences{RESET}"),
                4 => format!("{MUTED}Your session. Your connections. Your terminal.{RESET}"),
                _ => String::new(),
            };
            head.push(format!("  {pixels}    {text}"));
        }
    } else {
        head.push(format!(
            "  {ACCENT}{BOLD}Rune CLI{RESET} {MUTED}/ SETTINGS{RESET}"
        ));
    }
    if width < 54 || height < 16 {
        let at = page.tab();
        head.push_at(
            format!("  {ACCENT}{} {MUTED}· Tab{RESET}", SECTIONS[at].0),
            Hit::Settings(Target::Tabs {
                first: at,
                last: at + 1,
            }),
        );
    } else {
        let mut nav = String::from("  ");
        let mut first = 0;
        for (i, _) in SECTIONS.iter().enumerate() {
            let label = tab_label(i);
            if ansi::width(&nav) + ansi::width(&label) + 1 > width && i > first {
                head.push_at(nav, Hit::Settings(Target::Tabs { first, last: i }));
                nav = "  ".into();
                first = i;
            }
            let ink = if page.tab() == i { ACCENT } else { MUTED };
            let weight = if page.tab() == i { BOLD } else { "" };
            nav.push_str(&format!("{ink}{weight}{label}{RESET} "));
        }
        head.push_at(
            nav,
            Hit::Settings(Target::Tabs {
                first,
                last: SECTIONS.len(),
            }),
        );
    }
    head.push(format!("  {RULE}{}{RESET}", "─".repeat(inner)));
    let mut crumbs: Vec<String> = state.parents.iter().map(|l| safe(l.page.title())).collect();
    crumbs.push(safe(page.title()));
    if let Some(e) = &state.editor {
        crumbs.push(e.title.clone());
    }
    if height >= 16 {
        head.push(format!("  {MUTED}{}{RESET}", crumbs.join(" / ")));
    }
    if state.editor.is_none() {
        match page {
            Page::Models => {
                if height >= 20 {
                    head.push(format!(
                        "  {MUTED}Active session  ·  {ACCENT}{}{RESET}  {MUTED}/{RESET}  {TEXT}{BOLD}{}{RESET}",
                        safe(app.provider()),
                        safe(app.model())
                    ));
                }
                let search_target = if model_provider(app) == "all" {
                    "all models".to_string()
                } else {
                    safe(model_provider(app))
                };
                let (lines, (r, c), geometry) = input_box(
                    &format!("Search {search_target}"),
                    &state.location.query,
                    state.location.cursor,
                    false,
                    state.location.editing.selected(&state.location.query, state.location.cursor),
                    "Type to filter models…",
                    inner,
                );
                if state.location.focus == ModelFocus::Search && !state.tabs {
                    cursor = Some((head.lines.len() + r + 1, c + 3));
                }
                if height >= 16 {
                    let row = text_rows.len();
                    text_rows.push(geometry);
                    for (index, l) in lines.into_iter().enumerate() {
                        let target = if index == r {
                            Target::Text { row, editor: None }
                        } else {
                            Target::Search
                        };
                        head.push_at(format!("  {l}"), Hit::Settings(target));
                    }
                } else {
                    let projection = super::editor::Projection::plain(&state.location.query);
                    let (geometry, col) =
                        projection.window(state.location.cursor, width.saturating_sub(11), 11);
                    if state.location.focus == ModelFocus::Search && !state.tabs {
                        cursor = Some((head.lines.len() + 1, col));
                    }
                    let row = text_rows.len();
                    head.push_at(
                        format!("  {TEXT}Search: {}{RESET}", geometry.styled(state.location.editing.selected(&state.location.query, state.location.cursor))),
                        Hit::Settings(Target::Text { row, editor: None }),
                    );
                    text_rows.push(geometry);
                }
            }
            // The knobs are this session's; the AUXILIARY rows below them are
            // `rune.toml`'s and hold for every session — the header says which.
            Page::Generation if height >= 20 => head.push(format!(
                "  {MUTED}Knobs: this session  ·  {TEXT}{}{RESET}{MUTED}  ·  Auxiliary rows: every session{RESET}",
                safe(app.model())
            )),
            _ => {}
        }
    }
    let rs = rows(app);
    let selected = state.location.row.min(rs.len().saturating_sub(1));
    let mut foot = Sink::default();
    foot.push(format!("  {RULE}{}{RESET}", "─".repeat(inner)));
    let active_notice = state.notice.as_ref().filter(|n| {
        n.page == *page
            && (n.tone == Tone::Busy
                || n.created.elapsed()
                    < super::runtime_configuration().duration("ui.status_lifetime"))
    });
    if let Some(n) = active_notice {
        let ink = match n.tone {
            Tone::Good => TEXT,
            Tone::Error => REMOVED,
            Tone::Busy => ACCENT,
        };
        let message = match n.tone {
            Tone::Error => format!("Error: {}", safe(&n.text)),
            Tone::Busy => format!("{} {}", super::pulse::spinner(app.frame), safe(&n.text)),
            Tone::Good => safe(&n.text),
        };
        let wrapped = ansi::wrap(&message, inner);
        let limit = if height >= 26 { 2 } else { 1 };
        let pages = wrapped.len().div_ceil(limit).max(1);
        let start = (n.page_offset / limit % pages) * limit;
        for (i, line) in wrapped.iter().skip(start).take(limit).enumerate() {
            let line = if i + 1 == limit && pages > 1 {
                let hint = format!(" … Alt+N {}/{}", start / limit + 1, pages);
                format!(
                    "{}{hint}",
                    ansi::truncate(line, inner.saturating_sub(ansi::width(&hint)))
                )
            } else {
                line.clone()
            };
            foot.push(format!("  {ink}{line}{RESET}"));
        }
    } else if state.pending.is_some() {
        foot.push(format!(
            "  {ACCENT}Working… You can return while this finishes.{RESET}"
        ));
    }
    let hint_text = if width < 54 {
        if state.editor.is_some() {
            "↵ apply · Esc back".to_string()
        } else {
            "↑↓ · ↵ · Tab · Esc".to_string()
        }
    } else {
        hint(app)
    };

    let parent = if state.editor.is_some() {
        page.title().to_string()
    } else {
        page.parent()
            .map(|p| p.title().to_string())
            .unwrap_or("conversation".into())
    };
    let esc_text = format!("{ACCENT}Esc{RESET} {MUTED}← {}{RESET}", safe(&parent));

    let help_raw = if active_notice.is_none()
        && state.pending.is_none()
        && state.editor.is_none()
        && page != &Page::Overview
    {
        rs.get(selected)
            .filter(|r| !state.tabs && !r.help.is_empty())
            .map(|r| r.help.as_str())
            .unwrap_or("")
    } else {
        ""
    };

    let status_rows = if height >= 26 { 2 } else { 1 };
    if active_notice.is_none() && state.pending.is_none() {
        let wrapped = ansi::wrap(help_raw, inner);
        for (i, line) in wrapped.iter().take(status_rows).enumerate() {
            let more = i + 1 == status_rows && wrapped.len() > status_rows;
            let line = if more {
                format!("{} …", ansi::truncate(line, inner.saturating_sub(2)))
            } else {
                line.clone()
            };
            foot.push(format!("  {MUTED}{line}{RESET}"));
        }
    }
    // Fixed status reservation: notices never move the config body's bottom.
    while foot.lines.len() < status_rows + 1 {
        foot.push(String::new());
    }
    foot.push(format!("  {MUTED}{hint_text}{RESET}"));
    if height >= 18 {
        foot.push_at(format!("  {esc_text}"), Hit::Settings(Target::Back));
    }
    let capacity = height
        .saturating_sub(head.lines.len() + foot.lines.len())
        .max(1);
    let mut body = Sink::default();
    let mut body_cursor = None;
    let mut selection_line = 0;
    if let Some(e) = &state.editor {
        if e.is_aux() {
            body.push(format!(
                "  {REMOVED}{BOLD}SENSITIVE PARAMETER{RESET}  {REMOVED}— Requires double confirmation to apply.{RESET}"
            ));
            if capacity >= 11 {
                body.push(format!(
                    "  {REMOVED}Incorrect values may degrade performance, disrupt context budgeting, or weaken policy enforcement.{RESET}"
                ));
            }
            body.push(String::new());
        }
        if capacity >= 9 {
            for line in ansi::wrap(&e.help, inner).into_iter().take(2) {
                body.push(format!("  {MUTED}{line}{RESET}"));
            }
            if let Some(guide) = field_guidance(app, &e.field) {
                body.push(format!("  {ACCENT}{guide}{RESET}"));
            }
            body.push(String::new());
        }
        if !e.confirmation() {
            if !e.choices.is_empty() {
                body.push(format!("  {TEXT}{BOLD}{}{RESET}", safe(&e.title)));
                body.push(String::new());
                for (ci, choice) in e.choices.iter().enumerate() {
                    let is_checked = if e.is_multi_choice() {
                        e.text.split(',').any(|s| s.trim() == choice)
                    } else {
                        choice == &e.text || (e.text.is_empty() && ci == 0)
                    };
                    let check = if is_checked {
                        format!("{ACCENT}[✓]{RESET}")
                    } else {
                        format!("{MUTED}[ ]{RESET}")
                    };
                    let label = match choice.as_str() {
                        "true" => "true  (Enabled)",
                        "false" => "false (Disabled)",
                        "on" => "on   (Enabled)",
                        "off" => "off  (Disabled)",
                        other => other,
                    };
                    let is_focused_choice = e.button == CHOICES_FOCUS && e.cursor == ci;
                    let (cursor_mark, text_style) = if is_focused_choice {
                        (format!("{ACCENT}❯{RESET} "), format!("{TEXT}{BOLD}"))
                    } else if is_checked {
                        (
                            "  ".to_string(),
                            format!("{TEXT}{}", if e.is_multi_choice() { "" } else { BOLD }),
                        )
                    } else {
                        ("  ".to_string(), format!("{MUTED}"))
                    };
                    body.push_at(
                        format!("  {cursor_mark}{check} {text_style}{label}{RESET}"),
                        Hit::Settings(Target::Choice(ci)),
                    );
                }
                body.push(String::new());
            } else {
                let placeholder_dyn = field_placeholder(app, &e.field);
                let placeholder = placeholder_dyn.as_deref().unwrap_or(if e.clearable {
                    "Provider default"
                } else {
                    "Enter a value…"
                });
                let (lines, (r, c), geometry) = input_box(
                    &safe(&e.title),
                    &e.text,
                    e.cursor,
                    e.secret(),
                    e.editing.selected(&e.text, e.cursor),
                    placeholder,
                    inner,
                );
                let target = Target::Text {
                    row: text_rows.len(),
                    editor: Some(e.id),
                };
                text_rows.push(geometry);
                if capacity >= 6 {
                    body_cursor = Some((body.lines.len() + r, c + 2));
                    for (index, l) in lines.into_iter().enumerate() {
                        body.push_at(
                            format!("  {l}"),
                            if index == r {
                                Hit::Settings(target)
                            } else {
                                Hit::None
                            },
                        );
                    }
                } else {
                    body_cursor = Some((body.lines.len(), c + 2));
                    body.push_at(format!("  {}", lines[1]), Hit::Settings(target));
                }
                if e.is_duration() {
                    let is_inherited = match &e.field {
                        Field::Timeout(_) => true,
                        Field::Global(key) => runeengine::settings::configuration::FIELDS
                            .iter()
                            .find(|f| f.key == key)
                            .map(|f| f.default.trim_matches('"') == "null")
                            .unwrap_or(false),
                        _ => false,
                    };
                    let default_val = match &e.field {
                        Field::Global(key) => runeengine::settings::configuration::FIELDS
                            .iter()
                            .find(|f| f.key == key)
                            .map(|f| f.default.trim_matches('"'))
                            .filter(|d| *d != "null"),
                        _ => None,
                    };
                    body.push(duration_breakdown_line(&e.text, is_inherited, default_val));
                } else if e.is_size() {
                    let default_val = match &e.field {
                        Field::Global(key) => runeengine::settings::configuration::FIELDS
                            .iter()
                            .find(|f| f.key == key)
                            .map(|f| f.default.trim_matches('"'))
                            .filter(|d| *d != "null"),
                        _ => None,
                    };
                    let max_bytes = match &e.field {
                        Field::Global(k) if k == "classifier.max_evidence_size" => {
                            Some(64 * 1024 * 1024)
                        }
                        Field::Global(k) if k == "classifier.max_policy_size" => {
                            Some(16 * 1024 * 1024)
                        }
                        _ => None,
                    };
                    body.push(size_breakdown_line(&e.text, default_val, max_bytes));
                }
            }
        } else {
            body.push(format!("  {TEXT}{BOLD}{}{RESET}", safe(&e.title)));
        }
        let mut labels = if e.confirmation() {
            let (yes, no) = match &e.field {
                Field::RemoveService(_) => ("Remove connection", "Keep it"),
                _ => ("Remove key", "Keep key"),
            };
            vec![
                (yes.to_string(), Target::Apply),
                (no.to_string(), Target::Cancel),
            ]
        } else {
            let apply_label = if e.is_aux() && e.is_confirming_apply() {
                format!(
                    "Apply [Press again to confirm ({}s)]",
                    e.remaining_confirm_secs()
                )
            } else {
                "Apply".to_string()
            };
            vec![
                (apply_label, Target::Apply),
                ("Cancel".to_string(), Target::Cancel),
            ]
        };
        if e.clearable {
            let default_label = if e.is_aux() && e.is_confirming_apply() && e.button == 2 {
                format!(
                    "Use default [Press again to confirm ({}s)]",
                    e.remaining_confirm_secs()
                )
            } else {
                "Use default".to_string()
            };
            labels.push((default_label, Target::Reset));
        }
        for (i, (label, target)) in labels.into_iter().enumerate() {
            let active = e.button == i;
            let ink = if active && e.is_aux() && e.is_confirming_apply() && (i == 0 || i == 2) {
                REMOVED
            } else if active {
                TEXT
            } else {
                MUTED
            };
            body.push_at(
                format!(
                    "  {ACCENT}{} {ink}{}{label}{RESET}",
                    if active { "❯" } else { " " },
                    if active { BOLD } else { "" }
                ),
                Hit::Settings(target),
            );
        }
    } else if page == &Page::Overview && overview_columns(width, height) == 2 && capacity >= 15 {
        for pair in (0..rs.len()).step_by(2) {
            let card_w = inner.saturating_sub(3) / 2;
            let left_card = card(&rs[pair], pair == selected && !state.tabs, card_w);
            let right_card = if pair + 1 < rs.len() {
                Some(card(
                    &rs[pair + 1],
                    pair + 1 == selected && !state.tabs,
                    card_w,
                ))
            } else {
                None
            };
            if let Some(right) = right_card {
                for (left, r) in left_card.iter().zip(&right) {
                    body.push_at(
                        format!("  {left}   {r}"),
                        Hit::Settings(Target::Cards {
                            left: pair,
                            right: Some(pair + 1),
                            split: 2 + card_w + 3,
                        }),
                    );
                }
            } else {
                for left in &left_card {
                    body.push_at(
                        format!("  {left}"),
                        Hit::Settings(Target::Cards {
                            left: pair,
                            right: None,
                            split: 2 + card_w + 3,
                        }),
                    );
                }
            }
            if pair + 2 < rs.len() {
                body.push(String::new());
            }
        }
    } else if page == &Page::Models {
        body = model_body(app, &rs, width, capacity);
    } else {
        for (i, row) in rs.iter().enumerate() {
            if i == selected {
                selection_line = body.lines.len();
            }
            let hit = if row.pickable() {
                Hit::Settings(Target::Row(i))
            } else {
                Hit::None
            };
            for line in draw_row(row, i == selected && !state.tabs, inner, None) {
                body.push_at(format!("  {line}"), hit);
            }
        }
        if rs.is_empty() {
            body.push(format!(
                "  {MUTED}Nothing configured here yet. Open Connections to get started.{RESET}"
            ));
        }
    }
    let max_top = body.lines.len().saturating_sub(capacity);
    let top = if let Some(editor) = &state.editor {
        let target = if editor.button == CHOICES_FOCUS {
            Target::Choice(editor.cursor)
        } else {
            match editor.button {
                1 => Target::Cancel,
                2 => Target::Reset,
                _ => Target::Apply,
            }
        };
        body.owner
            .iter()
            .position(|hit| *hit == Hit::Settings(target))
            .unwrap_or(0)
            .saturating_sub(capacity.saturating_sub(1))
            .min(max_top)
    } else if page == &Page::Models {
        0
    } else {
        selection_line.saturating_sub(capacity / 2).min(max_top)
    };
    if top + capacity < body.lines.len() || top > 0 {
        if let Some(rule) = foot
            .lines
            .iter_mut()
            .find(|l| ansi::plain(l).trim().chars().all(|c| c == '─'))
        {
            *rule = format!(
                "  {}",
                panels::heading(
                    if top + capacity < body.lines.len() {
                        "↓ More below"
                    } else {
                        "↑ More above"
                    },
                    inner
                )
            );
        }
    }
    if let Some((r, c)) = body_cursor.filter(|(r, _)| *r >= top && *r < top + capacity) {
        cursor = Some((head.lines.len() + r - top + 1, c + 1));
    }
    for (line, hit) in body
        .lines
        .into_iter()
        .zip(body.owner)
        .skip(top)
        .take(capacity)
    {
        head.push_at(line, hit);
    }
    while head.lines.len() + foot.lines.len() < height {
        head.push(String::new());
    }
    for (line, hit) in foot.lines.into_iter().zip(foot.owner) {
        head.push_at(line, hit);
    }
    head.lines.truncate(height);
    head.owner.truncate(height);
    for line in &mut head.lines {
        *line = ansi::truncate(line, width);
    }
    (
        head,
        cursor.filter(|(r, c)| *r <= height && *c <= width),
        text_rows,
    )
}

/// The width of the first column of the Add connection vendor list: its
/// longest value, "Already connected".
const ADD_SERVICE_VALUE: usize = 17;

fn draw_row(row: &Row, selected: bool, width: usize, split_override: Option<usize>) -> Vec<String> {
    let label = safe(&row.label);
    // `value` is our own trusted markup on tabular rows (status dots carry
    // inks); `safe()` here would strip those colours and flatten the badge to
    // plain text. Untrusted text still cannot reach this path raw: labels and
    // notes are sanitised, and value content that comes from data is passed
    // through `safe()` at its call sites.
    let value = if matches!(
        row.kind,
        Kind::Navigate(Page::Provider(_)) | Kind::Navigate(Page::AddProvider)
    ) {
        row.value.clone()
    } else {
        safe(&row.value)
    };
    if !row.pickable()
        && row.note.is_empty()
        && (label == label.to_uppercase() || (row.help.is_empty() && value.is_empty()))
    {
        let mut lines = vec![format!("{ACCENT}{BOLD}{label}{RESET}")];
        if !value.is_empty() {
            lines.push(format!("{MUTED}{value}{RESET}"));
            lines.push(String::new());
        } else if label == label.to_uppercase() {
            lines.push(String::new());
        }
        return lines;
    }
    if matches!(row.kind, Kind::Fact) && label == "Provider" && row.note == "Serves" {
        return vec![
            format!(
                "  {MUTED}{}  {}  Serves{RESET}",
                ansi::pad(&label, 22),
                ansi::pad(&value, 28)
            ),
            String::new(),
        ];
    }
    if !row.pickable() && label != label.to_uppercase() && !value.is_empty() {
        let split_name = 24;
        let name_col = ansi::pad(&ansi::truncate(&label, split_name), split_name);
        let line1 = format!("  {MUTED}{name_col}{RESET}  {TEXT}{value}{RESET}");
        return vec![line1, String::new()];
    }
    let ink = if selected { TEXT } else { MUTED };
    let weight = if selected { BOLD } else { "" };
    let mark = if selected { "❯" } else { " " };
    let is_key_row = matches!(row.kind, Kind::Edit(Field::RemoveKey(..)));
    let is_tabular = (value.starts_with('[') && value.ends_with(']'))
        || matches!(
            row.kind,
            Kind::Navigate(Page::Provider(_)) | Kind::Navigate(Page::AddProvider)
        );

    let mut lines = if is_tabular && width >= 48 {
        let split_name = 22;
        let split_badge = 28;
        let name_col = ansi::pad(&ansi::truncate(&label, split_name), split_name);
        let badge_col = ansi::pad(&ansi::truncate(&value, split_badge), split_badge);
        let arrow = if matches!(row.kind, Kind::Navigate(_))
            && !value.ends_with('→')
            && !value.contains('→')
        {
            "→"
        } else {
            ""
        };
        let note_text = if !row.note.is_empty() {
            format!("{MUTED}{}{RESET}  ", safe(&row.note))
        } else {
            String::new()
        };
        let line1 = format!(
            "{ACCENT}{mark} {ink}{weight}{name_col}{RESET}  {badge_col}{RESET}  {note_text}{MUTED}{arrow}{RESET}"
        );
        vec![line1]
    } else if let Kind::Theme(mode) = row.kind {
        // A theme row is not a settings row: what it has to say is *what the
        // theme looks like*, so it carries two live chips — the accent and the
        // rule — drawn in the theme's own palette, plus an explicit active mark.
        // The chip is the whole point: a list of three names in the same grey
        // tells the reader nothing about which one they are choosing.
        let palette = theme::build_with(row.background, mode);
        let accent = ansi::encode(palette.of(ansi::Role::Accent), row.color_depth, false);
        let rule = ansi::encode(palette.of(ansi::Role::Rule), row.color_depth, false);
        let good = ansi::encode(palette.of(ansi::Role::Good), row.color_depth, false);
        let active = value == "Active";
        let status = if active {
            format!("{good}{BOLD}● active{RESET}")
        } else {
            format!("{MUTED}○ apply{RESET}")
        };
        let chip = format!("{accent}▐▌{rule}▐▌{RESET}");
        let name_w = 20.min(width.saturating_sub(22));
        let name_col = ansi::pad(&ansi::truncate(&label, name_w), name_w);
        vec![format!(
            "{ACCENT}{mark} {chip} {ink}{weight}{name_col}{RESET} {status}{RESET}"
        )]
    } else if is_key_row && width >= 48 {
        let split_name = 24;
        let name_col = ansi::pad(&ansi::truncate(&label, split_name), split_name);
        let line1 = format!("{ACCENT}{mark} {ink}{weight}{name_col}{RESET}  {TEXT}{value}{RESET}");
        vec![line1]
    } else if matches!(
        row.kind,
        Kind::Edit(
            Field::Knob(_)
                | Field::Timeout(_)
                | Field::ModelContext { .. }
                | Field::ModelMaxOutput { .. }
                | Field::ModelInputPrice { .. }
                | Field::ModelOutputPrice { .. }
                | Field::ModelCachedPrice { .. }
                | Field::ModelExtraRate { .. }
                | Field::ModelTierThreshold { .. }
                | Field::ModelTierInput { .. }
                | Field::ModelTierOutput { .. }
                | Field::ModelBusyInput { .. }
                | Field::ModelBusyOutput { .. }
                | Field::ModelReasoning { .. }
        ) | Kind::Sidebar
    ) && width >= 54
    {
        let split = 24;
        vec![format!(
            "{ACCENT}{mark} {ink}{weight}{}{RESET} {ACCENT}[ {TEXT}{value}{ACCENT} ]{RESET}",
            ansi::pad(&ansi::truncate(&label, split), split)
        )]
    } else {
        let detail = if row.note.is_empty() {
            value
        } else if matches!(row.kind, Kind::AddService(_)) {
            // The vendor list is a table: what a vendor needs, then what it
            // is, each in its own column whatever the first one says.
            format!(
                "{}  {}",
                ansi::pad(&value, ADD_SERVICE_VALUE),
                safe(&row.note)
            )
        } else {
            let val_str = if matches!(row.kind, Kind::Edit(Field::Global(_))) {
                ansi::pad(&value, 8)
            } else {
                value
            };
            format!("{val_str} · {}", safe(&row.note))
        };
        let split_name = split_override.unwrap_or_else(|| {
            if matches!(
                row.kind,
                Kind::Model { .. } | Kind::RouteUse(..) | Kind::WebSelect(..)
            ) {
                width.saturating_sub(32).clamp(24, 72)
            } else {
                24
            }
        });
        let name_col = ansi::pad(&ansi::truncate(&label, split_name), split_name);
        if detail.is_empty() {
            vec![format!("{ACCENT}{mark} {ink}{weight}{label}{RESET}")]
        } else {
            vec![format!(
                "{ACCENT}{mark} {ink}{weight}{name_col}{RESET}  {ink}{}{RESET}",
                ansi::truncate(&detail, width.saturating_sub(split_name + 6))
            )]
        }
    };
    lines.push(String::new());
    lines
}

fn model_body(app: &App, rs: &[Row], width: usize, capacity: usize) -> Sink {
    let state = &app.preferences;
    let providers = provider_list(app);
    let name = model_provider(app);
    let at = if name == "all" {
        0
    } else {
        1 + providers
            .iter()
            .position(|p| p["name"].as_str() == Some(name))
            .unwrap_or(0)
    };
    let total_providers = 1 + providers.len();
    let wide = width >= 64;
    let rail = if wide { (width / 4).clamp(18, 28) } else { 0 };
    let right_w = width.saturating_sub(rail + if wide { 7 } else { 4 });
    let ideal_model_w = rs
        .iter()
        .filter(|r| matches!(r.kind, Kind::Model { .. }))
        .map(|r| r.label.chars().count())
        .max()
        .map(|len| (len + 3).clamp(26, right_w.saturating_sub(30).max(26)))
        .unwrap_or(28);
    let selected = state.location.row.min(rs.len().saturating_sub(1));
    let mut right = Sink::default();
    let mut selected_line = 0;
    for (i, row) in rs.iter().enumerate() {
        if i == selected {
            selected_line = right.lines.len();
        }
        for line in draw_row(
            row,
            i == selected && state.location.focus == ModelFocus::List && !state.tabs,
            right_w,
            Some(ideal_model_w),
        ) {
            right.push_at(
                line,
                if row.pickable() {
                    Hit::Settings(Target::Row(i))
                } else {
                    Hit::None
                },
            );
        }
    }
    let count = rs
        .iter()
        .filter(|r| matches!(r.kind, Kind::Model { .. }))
        .count();
    let visible = capacity.saturating_sub(1);
    let top = selected_line
        .saturating_sub(visible / 2)
        .min(right.lines.len().saturating_sub(visible));

    let (header_name, header_count) = if name == "all" {
        let active_p = (top..top + visible)
            .find_map(|line_idx| {
                right.owner.get(line_idx).and_then(|h| match h {
                    Hit::Settings(Target::Row(r)) => match rs.get(*r) {
                        Some(row) => match &row.kind {
                            Kind::Model { provider, .. } => Some(provider.clone()),
                            _ => None,
                        },
                        None => None,
                    },
                    _ => None,
                })
            })
            .or_else(|| match rs.get(selected) {
                Some(row) => match &row.kind {
                    Kind::Model { provider, .. } => Some(provider.clone()),
                    _ => None,
                },
                None => None,
            })
            .unwrap_or_else(|| "all".to_string());

        let p_models = rs
            .iter()
            .filter(|r| match &r.kind {
                Kind::Model { provider, .. } => provider == &active_p,
                _ => false,
            })
            .count();

        if active_p == "all" {
            ("All providers".to_string(), count)
        } else {
            (active_p, p_models)
        }
    } else {
        (safe(name).to_string(), count)
    };

    let mut out = Sink::default();
    if wide {
        out.push(format!(
            "  {MUTED}{}{RULE}│{RESET}  {TEXT}{BOLD}{}{RESET} {MUTED}· {header_count} models{RESET}",
            ansi::pad("PROVIDERS", rail),
            safe(&header_name)
        ));
    } else {
        out.push_at(
            format!(
                "  {ACCENT}{} {TEXT}{BOLD}{}{RESET} {MUTED}· {header_count} models{RESET}",
                if state.location.focus == ModelFocus::Providers {
                    "❯"
                } else {
                    "‹"
                },
                safe(&header_name)
            ),
            Hit::Settings(Target::Provider(at)),
        );
    }
    let rail_top = at.saturating_sub(visible.saturating_sub(1) / 2);
    for line in 0..visible {
        let idx = line + rail_top;
        let p_item = if wide && idx < total_providers {
            if idx == 0 {
                Some((0, "All providers", name == "all"))
            } else {
                let p_idx = idx - 1;
                providers.get(p_idx).map(|p| {
                    let n = p["name"].as_str().unwrap_or("");
                    (idx, n, name == n)
                })
            }
        } else {
            None
        };
        let left = p_item
            .map(|(_, label, here)| {
                format!(
                    "{}{} {}{}{RESET}",
                    if here { ACCENT } else { MUTED },
                    if here && state.location.focus == ModelFocus::Providers {
                        "❯"
                    } else {
                        " "
                    },
                    if here { BOLD } else { "" },
                    safe(label)
                )
            })
            .unwrap_or_default();
        let row = right.owner.get(top + line).and_then(|h| match h {
            Hit::Settings(Target::Row(i)) => Some(*i),
            _ => None,
        });
        let text = right.lines.get(top + line).cloned().unwrap_or_default();
        let hit = Target::ModelLine {
            row,
            provider: p_item.map(|(idx, _, _)| idx),
            split: if wide { rail + 3 } else { 0 },
        };
        out.push_at(
            if wide {
                format!(
                    "  {}{RULE}│{RESET}  {text}",
                    ansi::pad(&ansi::truncate(&left, rail), rail)
                )
            } else {
                format!("  {text}")
            },
            Hit::Settings(hit),
        );
    }
    out
}

fn card(row: &Row, selected: bool, width: usize) -> Vec<String> {
    let label = safe(&row.label);
    let mut out = vec![super::draw::box_top(&label, width)];
    for content in [
        String::new(),
        format!("{TEXT}{}{RESET}", safe(&row.value)),
        format!("{MUTED}{}{RESET}", safe(&row.note)),
        String::new(),
        format!(
            "{ACCENT}{}{}{RESET}",
            if selected { "❯ " } else { "" },
            row.help
        ),
    ] {
        out.push(super::draw::boxed(
            &ansi::truncate(&content, width.saturating_sub(4)),
            width.saturating_sub(4),
        ));
    }
    out.push(super::draw::box_bottom(width));
    let rule = RULE.to_string();
    if !rule.is_empty() {
        let ink = if selected {
            format!("{ACCENT}{BOLD}")
        } else {
            MUTED.to_string()
        };
        for l in &mut out {
            *l = l.replace(&rule, &ink);
        }
    }
    out
}

pub fn hint(app: &App) -> String {
    if let Some(editor) = &app.preferences.editor {
        if app.preferences.pending.is_some() {
            return "Saving… · Esc returns (operation continues)".into();
        }
        if editor.confirmation() {
            "↑↓ choose · Enter confirm · Esc cancel".into()
        } else if editor.is_multi_choice() {
            "Space toggle · Enter toggles · ↑↓ select tool · Tab Apply · Esc cancel".into()
        } else if !editor.choices.is_empty() {
            "Space / ←→ switch option · Enter select · Tab Apply · Esc cancel".into()
        } else {
            "Enter apply · Tab actions · Esc cancel".into()
        }
    } else if app.preferences.tabs {
        "←→ section · ↓ content · Enter open · Esc back".into()
    } else if app.preferences.location.page == Page::Models {
        match app.preferences.location.focus {
            ModelFocus::Providers => "↑↓ provider · → models · Tab sections · Esc back".into(),
            ModelFocus::Search => "←→ cursor · ↑↓ results · Enter apply · Esc back".into(),
            ModelFocus::List => {
                "↑↓ model · ← providers · Enter apply · Alt+C configure · 't' test · Del delete"
                    .into()
            }
        }
    } else if app.preferences.location.page == Page::Appearance
        && rows(app)
            .get(app.preferences.location.row)
            .is_some_and(|r| r.kind == Kind::Navigate(Page::Themes))
    {
        "←→ switch theme · Enter palette · ↑↓ navigate · Esc back".into()
    } else if app.preferences.location.page == Page::Overview {
        "↑↓←→ choose · Enter open · Tab sections · Esc back".into()
    } else if rows(app)
        .get(app.preferences.location.row)
        .is_some_and(|r| matches!(r.kind, Kind::RouteUse(..)))
    {
        "↑↓ navigate · Enter use · Del remove route · Esc back".into()
    } else {
        "↑↓ navigate · Enter open · Tab sections · Esc back".into()
    }
}

pub fn enter(app: &mut App, page: Page, d: &mut Driver) {
    if app.preferences.editor.is_some() {
        return;
    }
    app.show_panel(Panel::Settings);
    app.preferences.navigate(page);
    settle(app);
    app.refresh(d);
}

fn settle(app: &mut App) {
    let rs = rows(app);
    if !rs
        .get(app.preferences.location.row)
        .is_some_and(Row::pickable)
    {
        app.preferences.location.row = rs.iter().position(Row::pickable).unwrap_or(0);
    }
}

fn move_row(app: &mut App, by: i32) {
    let rs = rows(app);
    let at = app.preferences.location.row;
    let next = if by < 0 {
        rs.iter()
            .enumerate()
            .take(at)
            .rev()
            .find(|(_, r)| r.pickable())
            .map(|(i, _)| i)
    } else {
        rs.iter()
            .enumerate()
            .skip(at + 1)
            .find(|(_, r)| r.pickable())
            .map(|(i, _)| i)
    };
    if let Some(next) = next {
        app.preferences.location.row = next;
    }
}

fn browse_provider(app: &mut App, index: usize) {
    let name = if index == 0 {
        Some("all".to_string())
    } else {
        provider_list(app)
            .get(index - 1)
            .and_then(|p| p["name"].as_str())
            .map(String::from)
    };
    if let Some(name) = name {
        if name != model_provider(app) {
            app.preferences.location.provider = name;
            app.preferences.location.row = 0;
            app.preferences.location.query.clear();
            app.preferences.location.cursor = 0;
        }
        app.preferences.location.focus = ModelFocus::Providers;
        settle(app);
    }
}

pub fn key(app: &mut App, key: &Key, d: &mut Driver) -> bool {
    if matches!(key, Key::Alt('n')) {
        if let Some(n) = &mut app.preferences.notice {
            n.page_offset = n
                .page_offset
                .saturating_add(if app.preferences.height >= 26 { 2 } else { 1 });
            n.created = Instant::now();
        }
        return true;
    }
    if let Key::Mouse(mouse) = key {
        if matches!(
            mouse.kind,
            super::input::Click::WheelUp | super::input::Click::WheelDown
        ) && mouse.modifiers.wheel_multiplier() > 1
        {
            let mut plain = *mouse;
            plain.modifiers = super::input::Modifiers::default();
            for _ in 0..mouse.modifiers.wheel_multiplier() {
                self::key(app, &Key::Mouse(plain), d);
            }
            return true;
        }
    }
    let wheel = match key {
        Key::Mouse(m) if m.kind == super::input::Click::WheelUp => Some(Key::Up),
        Key::Mouse(m) if m.kind == super::input::Click::WheelDown => Some(Key::Down),
        _ => None,
    };
    if wheel.is_some() && (app.preferences.editor.is_some() || app.preferences.tabs) {
        return true;
    }
    let keyboard = wheel.is_none();
    let key = wheel.as_ref().unwrap_or(key);
    if matches!(key, Key::Esc | Key::Ctrl('c') | Key::Ctrl('g')) {
        app.preferences.pending = None;
        if !app.preferences.back() {
            app.show_panel(Panel::None);
            if matches!(key, Key::Ctrl('c')) {
                app.quit_armed = true;
                app.quit_armed_at = Some(std::time::Instant::now());
                app.note("press ctrl+c again to leave");
            }
        }
        return true;
    }
    if let Some(ref e) = app.preferences.editor {
        let is_model_cfg = matches!(
            e.field,
            Field::ModelContext { .. }
                | Field::ModelMaxOutput { .. }
                | Field::ModelInputPrice { .. }
                | Field::ModelOutputPrice { .. }
                | Field::ModelCachedPrice { .. }
                | Field::ModelExtraRate { .. }
                | Field::ModelTierThreshold { .. }
                | Field::ModelTierInput { .. }
                | Field::ModelTierOutput { .. }
                | Field::ModelBusyInput { .. }
                | Field::ModelBusyOutput { .. }
                | Field::ModelReasoning { .. }
        );
        if !is_model_cfg && app.preferences.pending.is_some() {
            return true;
        }
        match key {
            Key::Enter => {
                let action = app
                    .preferences
                    .editor
                    .as_ref()
                    .map(|e| e.button)
                    .unwrap_or(0);
                if action == CHOICES_FOCUS {
                    let e = app.preferences.editor.as_mut().unwrap();
                    e.confirm_apply_at = None;
                    if e.is_multi_choice() {
                        if let Some(choice) = e.choices.get(e.cursor).cloned() {
                            toggle_tool(&mut e.text, &choice);
                        }
                        return true;
                    } else if let Some(choice) = e.choices.get(e.cursor).cloned() {
                        e.text = choice;
                    }
                    if check_aux_confirmation(app) {
                        save_editor(app, d);
                    }
                    return true;
                }
                match action {
                    1 => {
                        app.preferences.back();
                    }
                    2 => {
                        if check_aux_confirmation(app) {
                            if let Some(e) = app.preferences.editor.as_mut() {
                                e.text.clear();
                                e.cursor = 0;
                            }
                            save_editor(app, d);
                        }
                    }
                    _ => {
                        if check_aux_confirmation(app) {
                            save_editor(app, d);
                        }
                    }
                }
            }
            Key::Tab | Key::ShiftTab => {
                let e = app.preferences.editor.as_mut().unwrap();
                e.confirm_apply_at = None;
                let count = if e.clearable { 3 } else { 2 };
                let back = matches!(key, Key::ShiftTab);
                e.button = if e.choices.is_empty() {
                    if back {
                        (e.button + count - 1) % count
                    } else {
                        (e.button + 1) % count
                    }
                } else if e.button == CHOICES_FOCUS {
                    if back {
                        count - 1
                    } else {
                        0
                    }
                } else if (back && e.button == 0) || (!back && e.button + 1 == count) {
                    CHOICES_FOCUS
                } else if back {
                    e.button - 1
                } else {
                    e.button + 1
                };
            }
            Key::Down | Key::Up | Key::PageUp | Key::PageDown => {
                let e = app.preferences.editor.as_mut().unwrap();
                e.confirm_apply_at = None;
                let choices = e.choices.len();
                let buttons = if e.clearable { 3 } else { 2 };
                let total = choices + buttons;
                let at = if e.button == CHOICES_FOCUS {
                    e.cursor.min(choices.saturating_sub(1))
                } else {
                    choices + e.button.min(buttons - 1)
                };
                let next = match *key {
                    Key::Down => (at + 1) % total,
                    Key::Up => (at + total - 1) % total,
                    Key::PageUp => 0,
                    Key::PageDown if at < choices.saturating_sub(1) => choices - 1,
                    Key::PageDown if at < choices => choices,
                    Key::PageDown => total - 1,
                    _ => unreachable!(),
                };
                if next < choices {
                    e.button = CHOICES_FOCUS;
                    e.cursor = next;
                    if !e.is_multi_choice() {
                        e.text = e.choices[next].clone();
                    }
                } else {
                    e.button = next - choices;
                }
            }
            Key::Char(' ')
                if app
                    .preferences
                    .editor
                    .as_ref()
                    .is_some_and(|e| e.is_multi_choice()) =>
            {
                let e = app.preferences.editor.as_mut().unwrap();
                e.confirm_apply_at = None;
                if let Some(tool) = e.choices.get(e.cursor).cloned() {
                    toggle_tool(&mut e.text, &tool);
                    e.button = CHOICES_FOCUS;
                }
            }
            // Space, Left, and Right cycle the choice in single-choice lists.
            Key::Left | Key::Right | Key::Char(' ')
                if app
                    .preferences
                    .editor
                    .as_ref()
                    .is_some_and(|e| !e.choices.is_empty() && !e.is_multi_choice()) =>
            {
                let e = app.preferences.editor.as_mut().unwrap();
                e.confirm_apply_at = None;
                let at = e.choices.iter().position(|v| v == &e.text).unwrap_or(0);
                let next = if *key == Key::Left {
                    (at + e.choices.len() - 1) % e.choices.len()
                } else {
                    (at + 1) % e.choices.len()
                };
                e.text = e.choices[next].clone();
                e.cursor = next;
                e.button = CHOICES_FOCUS;
            }
            _ => {
                let max_len = app
                    .preferences
                    .editor
                    .as_ref()
                    .map(|e| e.max_len(app))
                    .unwrap_or(128);
                let e = app.preferences.editor.as_mut().unwrap();
                if !e.confirmation() && e.choices.is_empty() {
                    e.confirm_apply_at = None;
                    let allowed = match key {
                        Key::Char(c) => e.allows_char(*c),
                        _ => true,
                    };
                    if allowed {
                        e.editing.key(
                            &mut e.text,
                            &mut e.cursor,
                            key,
                            super::editing::Options::single(max_len, 16_384),
                        );
                    }
                    e.button = 0;
                }
            }
        }
        return true;
    }
    if app.preferences.tabs {
        match key {
            Key::Left | Key::Right => {
                let at = app.preferences.location.page.tab();
                let n = SECTIONS.len();
                let next = if *key == Key::Right {
                    (at + 1) % n
                } else {
                    (at + n - 1) % n
                };
                app.preferences.navigate(SECTIONS[next].1.clone());
                settle(app);
                app.preferences.tabs = true;
            }
            Key::Tab | Key::ShiftTab | Key::Enter | Key::Down => {
                app.preferences.tabs = false;
                if *key == Key::Down && app.preferences.location.page == Page::Models {
                    app.preferences.location.focus = ModelFocus::Search;
                }
            }
            _ => {}
        }
        return true;
    }
    if keyboard && *key == Key::Up {
        let page = &app.preferences.location.page;
        let top = if *page == Page::Overview {
            app.preferences.location.row
                < overview_columns(app.preferences.width.max(20), app.preferences.height.max(8))
        } else {
            rows(app)
                .iter()
                .position(Row::pickable)
                .is_none_or(|first| app.preferences.location.row <= first)
        };
        if *page != Page::Models && top {
            app.preferences.tabs = true;
            return true;
        }
        if *page == Page::Models {
            match app.preferences.location.focus {
                ModelFocus::Search => {
                    app.preferences.tabs = true;
                    return true;
                }
                ModelFocus::List if top => {
                    app.preferences.location.focus = ModelFocus::Search;
                    return true;
                }
                ModelFocus::Providers if model_provider(app) == "all" => {
                    app.preferences.location.focus = ModelFocus::Search;
                    return true;
                }
                _ => {}
            }
        }
    }
    if app.preferences.location.page == Page::Models {
        let focus = app.preferences.location.focus;
        match key {
            Key::Tab | Key::ShiftTab => app.preferences.tabs = true,
            Key::Left | Key::Right if focus != ModelFocus::Search => {
                app.preferences.location.focus = if *key == Key::Left {
                    ModelFocus::Providers
                } else {
                    ModelFocus::List
                };
            }
            Key::Up | Key::Down if focus == ModelFocus::Providers => {
                let list = provider_list(app);
                let at = if model_provider(app) == "all" {
                    0
                } else {
                    1 + list
                        .iter()
                        .position(|p| p["name"].as_str() == Some(model_provider(app)))
                        .unwrap_or(0)
                };
                let next = if *key == Key::Up {
                    at.saturating_sub(1)
                } else {
                    (at + 1).min(list.len())
                };
                browse_provider(app, next);
            }
            Key::Home | Key::End if focus == ModelFocus::Providers => browse_provider(
                app,
                if *key == Key::Home {
                    0
                } else {
                    provider_list(app).len()
                },
            ),
            Key::Up | Key::Down => {
                if focus == ModelFocus::Search {
                    app.preferences.location.focus = ModelFocus::List;
                } else {
                    move_row(app, if *key == Key::Up { -1 } else { 1 });
                }
            }
            Key::Home | Key::End if focus == ModelFocus::List => {
                let rs = rows(app);
                app.preferences.location.row = if *key == Key::Home {
                    rs.iter().position(Row::pickable)
                } else {
                    rs.iter().rposition(Row::pickable)
                }
                .unwrap_or(0);
            }
            Key::PageUp | Key::PageDown => {
                app.preferences.location.focus = ModelFocus::List;
                for _ in 0..5 {
                    move_row(app, if *key == Key::PageUp { -1 } else { 1 });
                }
            }
            Key::Enter if focus == ModelFocus::Providers => {
                app.preferences.location.focus = ModelFocus::List
            }
            Key::Enter => {
                if let Some(row) = rows(app).get(app.preferences.location.row) {
                    open(app, row, d);
                }
            }
            Key::Ctrl('f') => app.preferences.location.focus = ModelFocus::Search,
            Key::Char('t' | 'T') if focus == ModelFocus::List => {
                let to_test =
                    rows(app)
                        .get(app.preferences.location.row)
                        .and_then(|r| match &r.kind {
                            Kind::Model { provider, id } => Some((provider.clone(), id.clone())),
                            _ => None,
                        });
                if let Some((provider, id)) = to_test {
                    send(
                        app,
                        d,
                        Operation::Probe(provider.clone()),
                        &format!("Testing model '{id}' on {provider}…"),
                        |req_id| Command::ProviderTest {
                            id: req_id,
                            name: provider,
                            model: Some(id),
                        },
                    );
                    return true;
                }
            }
            Key::Alt('c' | 'C') if focus != ModelFocus::Providers => {
                let to_config = rows(app)
                    .get(app.preferences.location.row)
                    .and_then(|r| match &r.kind {
                        Kind::Model { provider, id } => Some((provider.clone(), id.clone())),
                        _ => None,
                    });
                if let Some((provider, id)) = to_config {
                    app.preferences.navigate(Page::ModelConfig {
                        provider,
                        model: id,
                    });
                    settle(app);
                    return true;
                }
            }
            Key::Delete if focus == ModelFocus::List => {
                let to_drop =
                    rows(app)
                        .get(app.preferences.location.row)
                        .and_then(|r| match &r.kind {
                            Kind::Model { provider, id } => Some((provider.clone(), id.clone())),
                            _ => None,
                        });
                if let Some((provider, id)) = to_drop {
                    send(
                        app,
                        d,
                        Operation::Registry(format!("Model '{id}' removed from {provider}.")),
                        "Removing model…",
                        |req_id| Command::ProviderDropModel {
                            id: req_id,
                            provider,
                            model: id,
                        },
                    );
                } else {
                    notice(app, "Select a model row to delete.", Tone::Error);
                }
            }
            _ => {
                let loc = &mut app.preferences.location;
                let before = loc.query.clone();
                if loc.editing.key(
                    &mut loc.query,
                    &mut loc.cursor,
                    key,
                    super::editing::Options::single(16_384, 16_384),
                ) {
                    loc.focus = ModelFocus::Search;
                }
                if loc.query != before {
                    loc.row = 0;
                    settle(app);
                }
            }
        }
        return true;
    }
    match key {
        Key::Left | Key::Right
            if app.preferences.location.page == Page::Appearance
                && rows(app)
                    .get(app.preferences.location.row)
                    .is_some_and(|r| r.kind == Kind::Navigate(Page::Themes)) =>
        {
            let next = if *key == Key::Right {
                app.theme.next()
            } else {
                app.theme.prev()
            };
            open(
                app,
                &Row::new(Kind::Theme(next), next.label(), String::new()),
                d,
            );
        }
        Key::Left | Key::Right
            if rows(app)
                .get(app.preferences.location.row)
                .is_some_and(|r| matches!(r.kind, Kind::Sidebar)) =>
        {
            app.set_sidebar(*key == Key::Right);
        }
        Key::Left | Key::Right
            if rows(app)
                .get(app.preferences.location.row)
                .is_some_and(|r| matches!(r.kind, Kind::Edit(Field::Knob(_)))) =>
        {
            if let Some(row) = rows(app).get(app.preferences.location.row) {
                if let Kind::Edit(Field::Knob(ref k)) = row.kind {
                    let delta = if *key == Key::Right { 1 } else { -1 };
                    cycle_knob(app, k, delta, d);
                }
            }
        }
        Key::Delete
            if rows(app)
                .get(app.preferences.location.row)
                .is_some_and(|r| {
                    matches!(r.kind, Kind::RouteUse(..) | Kind::WebSelect(_, Some(_)))
                }) =>
        {
            match rows(app)
                .get(app.preferences.location.row)
                .map(|r| r.kind.clone())
            {
                Some(Kind::RouteUse(name, svc, route)) => {
                    web_ui::delete(app, d, svc, &format!("{name}:{route}"))
                }
                Some(Kind::WebSelect(svc, Some(target))) => web_ui::delete(app, d, svc, &target),
                _ => {}
            }
        }
        Key::Up | Key::Down | Key::Left | Key::Right
            if app.preferences.location.page == Page::Overview =>
        {
            let columns =
                overview_columns(app.preferences.width.max(20), app.preferences.height.max(8));
            let total = rows(app).len();
            let at = app.preferences.location.row.min(total.saturating_sub(1));
            app.preferences.location.row = match key {
                Key::Up if at >= columns => at - columns,
                Key::Down if at + columns < total => at + columns,
                Key::Left if columns == 2 && !at.is_multiple_of(2) => at - 1,
                Key::Right if columns == 2 && at.is_multiple_of(2) && at + 1 < total => at + 1,
                _ => at,
            };
        }
        Key::Up => move_row(app, -1),
        Key::Down => move_row(app, 1),
        Key::Home => {
            app.preferences.location.row = 0;
            settle(app);
        }
        Key::End => {
            let rs = rows(app);
            app.preferences.location.row = rs.iter().rposition(Row::pickable).unwrap_or(0);
        }
        Key::PageUp | Key::PageDown => {
            for _ in 0..5 {
                move_row(app, if *key == Key::PageUp { -1 } else { 1 });
            }
        }
        Key::Tab | Key::ShiftTab => app.preferences.tabs = true,
        Key::Enter => {
            if let Some(row) = rows(app).get(app.preferences.location.row) {
                open(app, row, d);
            }
        }
        _ => {}
    }
    // No Settings keystroke is allowed to modify the hidden conversation draft.
    true
}

pub(super) fn select(app: &mut App, from: Target, first: u16, to: Target, last: u16) -> bool {
    let (
        Target::Text { row: a, editor: id },
        Target::Text {
            row: b,
            editor: other,
        },
    ) = (from, to)
    else {
        return false;
    };
    if id != other {
        return false;
    }
    let Some(start) = app.preferences.text_rows.get(a).map(|r| r.caret_at(first)) else {
        return false;
    };
    let Some(end) = app.preferences.text_rows.get(b).map(|r| r.caret_at(last)) else {
        return false;
    };
    match (id, app.preferences.editor.as_mut()) {
        (Some(id), Some(e)) if e.id == id => {
            e.editing.select(&e.text, &mut e.cursor, start, end);
            true
        }
        (None, None) => {
            let loc = &mut app.preferences.location;
            loc.editing.select(&loc.query, &mut loc.cursor, start, end);
            true
        }
        _ => false,
    }
}

pub fn click(app: &mut App, target: Target, col: u16, d: &mut Driver) {
    if app.panel != Panel::Settings {
        return;
    }
    if target == Target::Back {
        key(app, &Key::Esc, d);
        return;
    }
    if let Target::Text { row, editor } = target {
        let Some(geometry) = app.preferences.text_rows.get(row) else {
            return;
        };
        let at = geometry.caret_at(col);
        match (editor, app.preferences.editor.as_mut()) {
            (Some(id), Some(e)) if e.id == id && e.text.is_char_boundary(at) => {
                e.editing.place(&e.text, &mut e.cursor, at);
                e.button = 0;
                e.confirm_apply_at = None;
            }
            (None, None)
                if app.preferences.location.page == Page::Models
                    && app.preferences.location.query.is_char_boundary(at) =>
            {
                app.preferences.tabs = false;
                app.preferences.location.focus = ModelFocus::Search;
                let loc = &mut app.preferences.location;
                loc.editing.place(&loc.query, &mut loc.cursor, at);
            }
            _ => {}
        }
        return;
    }
    if app.preferences.editor.is_some() {
        match target {
            Target::Cancel => {
                app.preferences.back();
            }
            Target::Apply => {
                if check_aux_confirmation(app) {
                    save_editor(app, d);
                }
            }
            Target::Reset => {
                if check_aux_confirmation(app) {
                    if let Some(e) = app.preferences.editor.as_mut() {
                        if e.clearable {
                            e.text.clear();
                            e.cursor = 0;
                        }
                    }
                    save_editor(app, d);
                }
            }
            Target::Choice(ci) => {
                if let Some(e) = app.preferences.editor.as_mut() {
                    if let Some(c) = e.choices.get(ci).cloned() {
                        if e.is_multi_choice() {
                            toggle_tool(&mut e.text, &c);
                            e.cursor = ci;
                        } else {
                            e.text = c;
                            e.cursor = ci;
                        }
                        e.button = CHOICES_FOCUS;
                        e.confirm_apply_at = None;
                    }
                }
            }
            Target::Tabs { first, last } => {
                app.preferences.editor = None;
                let mut left = 3;
                for (i, section) in SECTIONS.iter().enumerate().take(last).skip(first) {
                    let len = ansi::width(&tab_label(i));
                    if (col as usize) >= left && (col as usize) < left + len {
                        app.preferences.navigate(section.1.clone());
                        settle(app);
                        break;
                    }
                    left += len + 1;
                }
            }
            _ => {}
        }
        return;
    }
    let row = match target {
        Target::Tabs { first, last } => {
            let mut left = 3;
            for (i, section) in SECTIONS.iter().enumerate().take(last).skip(first) {
                let len = ansi::width(&tab_label(i));
                if (col as usize) >= left && (col as usize) < left + len {
                    app.preferences.navigate(section.1.clone());
                    settle(app);
                    break;
                }
                left += len + 1;
            }
            return;
        }
        Target::Cards { left, right, split } => {
            if col as usize > split {
                right.unwrap_or(left)
            } else {
                left
            }
        }
        Target::Row(n) => n,
        Target::Provider(i) => {
            browse_provider(app, i);
            return;
        }
        Target::ModelLine {
            row,
            provider,
            split,
        } => {
            if (col as usize) <= split {
                if let Some(i) = provider {
                    browse_provider(app, i);
                }
                return;
            }
            let Some(row) = row else { return };
            app.preferences.location.focus = ModelFocus::List;
            row
        }
        Target::Search => {
            app.preferences.tabs = false;
            app.preferences.location.focus = ModelFocus::Search;
            return;
        }
        _ => return,
    };
    app.preferences.tabs = false;
    if let Some(r) = rows(app).get(row) {
        // Opening a page/editor is reversible. A model/theme/toggle requires a
        // second click on the selected row, matching keyboard select + Enter.
        let open_now = matches!(
            r.kind,
            Kind::Navigate(_) | Kind::Edit(_) | Kind::AddPreset(_) | Kind::AddService(_)
        ) || app.preferences.location.row == row;
        app.preferences.location.row = row;
        if open_now {
            open(app, r, d);
        }
    }
}

pub fn open(app: &mut App, row: &Row, d: &mut Driver) {
    match &row.kind {
        Kind::Fact => {}
        Kind::Navigate(page) => {
            let provider = if *page == Page::Models {
                match &app.preferences.location.page {
                    Page::Provider(n) => Some(n.clone()),
                    _ => None,
                }
            } else {
                None
            };
            app.preferences.navigate(page.clone());
            if let Some(name) = provider {
                app.preferences.location.provider = name;
            }
            settle(app);
        }
        Kind::Model { provider, id } => {
            if provider == app.provider() && id == app.model() {
                notice(
                    app,
                    "This model is already active. Press Alt+C to configure capabilities.",
                    Tone::Good,
                );
                return;
            }
            select_model(app, provider.clone(), id.clone(), false, d);
        }
        Kind::Edit(Field::ServiceZone(name)) => {
            // The zones are the account's, asked now — never a list left
            // over from another screen, or none at all.
            let api = ServiceState::of(app, name).api;
            let name = name.clone();
            ask_options(
                app,
                d,
                &name,
                &api,
                None,
                None,
                Field::ServiceZone(name.clone()),
            );
        }
        Kind::Edit(field) => {
            if let Field::Knob(ref k) = field {
                if let Some(choices) = knob_choices(app, k) {
                    if !choices.is_empty() {
                        let curr = if k == "effort" {
                            app.effort_choice(&choices)
                        } else if k == "thinking" {
                            if app.thinking() {
                                "on".to_string()
                            } else {
                                "off".to_string()
                            }
                        } else {
                            panels::value_of(&app.settings["settings"], k)
                        };
                        let at = choices
                            .iter()
                            .position(|c| c.eq_ignore_ascii_case(&curr))
                            .unwrap_or(0);
                        let next = (at + 1) % choices.len();
                        apply(app, k, &choices[next], d);
                        return;
                    }
                }
            }
            open_editor(app, field.clone(), row);
        }
        Kind::Refresh(name) => {
            let name = name.clone();
            send(
                app,
                d,
                Operation::Catalogue(name.clone()),
                "Refreshing model catalog…",
                |id| Command::ProviderModels { id, name },
            );
        }
        Kind::Default(name) => {
            let is_already = provider_list(app)
                .iter()
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| p["default"].as_bool())
                .unwrap_or(false);
            if is_already {
                notice(
                    app,
                    &format!("{name} is already the fallback for new sessions. Select another provider to change."),
                    Tone::Good,
                );
                return;
            }
            let name = name.clone();
            send(
                app,
                d,
                Operation::Registry(format!(
                    "{name} is now the default fallback for new sessions."
                )),
                "Saving fallback…",
                |id| Command::ProviderDefault { id, name },
            );
        }
        Kind::AddPreset(name) => {
            app.preferences.navigate(Page::Setup(name.clone()));
            app.preferences.setup = Some(ProviderDraft::model(name));
            let r_list = rows(app);
            if let Some(r) = r_list
                .iter()
                .find(|r| matches!(&r.kind, Kind::Edit(Field::SetupKeyName)))
            {
                open_editor(app, Field::SetupKeyName, r);
            }
        }
        Kind::AddService(id)
            if provider_list(app)
                .into_iter()
                .any(|p| p["name"].as_str() == Some(id)) =>
        {
            // Already a connection (for models): its page switches the
            // services on, with the key ring it already has.
            app.preferences.navigate(Page::Provider(id.clone()));
            settle(app);
        }
        Kind::AddService(id) => {
            let adapter = adapter_of(app, id).cloned().unwrap_or(Value::Null);
            app.preferences.navigate(Page::Setup(id.clone()));
            app.preferences.setup = Some(ProviderDraft {
                service: true,
                ..ProviderDraft::model(id)
            });
            if adapter["base_url"].is_null() {
                edit_now(
                    app,
                    Field::SetupUrl,
                    "Base URL",
                    "The address of your own server.",
                );
            } else if adapter["key"] != "none" && adapter["keys"].as_u64().unwrap_or(0) == 0 {
                edit_now(app, Field::SetupKeyName, "Key name", "");
            }
        }
        Kind::Connect => connect_service(app, d),
        Kind::RouteUse(name, svc, route) => {
            web_ui::select(app, d, *svc, Some(format!("{name}:{route}")));
        }
        Kind::WebSelect(svc, target) => {
            web_ui::select(app, d, *svc, target.clone());
        }
        Kind::RouteAdd(name, svc) => {
            if kept_routes(app, name, *svc).len() >= runeengine::settings::services::MAX_ROUTES {
                notice(
                    app,
                    &format!(
                        "{name} keeps at most {} routes for {}. Remove one first (Delete on its row).",
                        runeengine::settings::services::MAX_ROUTES,
                        service_name(*svc)
                    ),
                    Tone::Error,
                );
                return;
            }
            app.preferences
                .navigate(Page::WebProviders { service: *svc });
            edit_now(
                app,
                Field::ServiceRouteText(name.clone(), *svc),
                "Route",
                "Type the provider route. Kept only after a real request succeeds.",
            );
        }
        Kind::ServiceOn(name, svc) => {
            let mut state = ServiceState::of(app, name);
            app.preferences
                .navigate(Page::WebProviders { service: *svc });
            if adapter_of(app, &state.api).is_some_and(|a| a["zone"].as_bool().unwrap_or(false))
                && state.zone.is_none()
            {
                let field = Field::ServiceZone(name.clone());
                ask_options(app, d, name, &state.api, None, None, field);
                return;
            }
            *state.uses(*svc) = Some(runeengine::proto::ServiceUse::default());
            let (name, svc) = (name.clone(), *svc);
            state.send(
                app,
                d,
                &format!("{} is on for {name}: its check passed.", service_title(svc)),
                "Checking with a real request…",
            );
        }
        Kind::ReloadClassifier => apply(app, "classifier.reload", "true", d),
        Kind::Sidebar => {
            let next = !app.sidebar;
            app.set_sidebar(next);
            notice(
                app,
                &format!("Conversation side panel {} (saved).", on_off(app.sidebar)),
                Tone::Good,
            );
        }
        Kind::Theme(next) => {
            if app.theme == *next {
                notice(app, "This theme is already active.", Tone::Good);
                return;
            }
            app.set_theme(*next);
            if app.theme == *next {
                notice(
                    app,
                    &format!("{} saved on this device.", next.label()),
                    Tone::Good,
                );
            } else {
                notice(
                    app,
                    "Could not save the theme. Check configuration access and try again.",
                    Tone::Error,
                );
            }
        }
    }
}

fn next_editor_id() -> u64 {
    static NEXT_EDITOR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_EDITOR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn open_editor(app: &mut App, field: Field, row: &Row) {
    // No key can be added or removed while the store cannot be opened: say
    // why before anything is typed (owner, 2026-10-16).
    if matches!(
        field,
        Field::Key(_)
            | Field::KeyValue { .. }
            | Field::RemoveKey(..)
            | Field::SetupKey
            | Field::SetupKeyName
    ) {
        if let Some(note) = credentials_unavailable(app).map(String::from) {
            notice(app, &note, Tone::Error);
            return;
        }
    }
    let is_model_cfg = matches!(
        field,
        Field::ModelContext { .. }
            | Field::ModelMaxOutput { .. }
            | Field::ModelInputPrice { .. }
            | Field::ModelOutputPrice { .. }
            | Field::ModelCachedPrice { .. }
            | Field::ModelExtraRate { .. }
            | Field::ModelTierThreshold { .. }
            | Field::ModelTierInput { .. }
            | Field::ModelTierOutput { .. }
            | Field::ModelBusyInput { .. }
            | Field::ModelBusyOutput { .. }
            | Field::ModelReasoning { .. }
    );
    if !is_model_cfg && app.preferences.pending.is_some() {
        notice(
            app,
            "Wait for the current operation to finish before editing.",
            Tone::Busy,
        );
        return;
    }
    let mut choices = Vec::new();
    let mut clearable = false;
    let text = match &field {
        Field::Global(key) => {
            clearable = true;
            let value = key
                .split('.')
                .fold(&app.settings["configuration"], |v, k| &v[k]);
            let is_bool_field = runeengine::settings::configuration::FIELDS.iter().any(|f| {
                f.key == key && matches!(f.kind, runeengine::settings::configuration::Kind::Bool)
            });
            if value.is_boolean() || is_bool_field {
                choices = vec!["true".into(), "false".into()];
            } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                choices = runeengine::tools::Registry::default()
                    .names()
                    .into_iter()
                    .map(String::from)
                    .collect();
            }
            if value.is_null() {
                if is_bool_field {
                    runeengine::settings::configuration::FIELDS
                        .iter()
                        .find(|f| f.key == key)
                        .map(|f| f.default.trim_matches('"'))
                        .unwrap_or("true")
                        .to_string()
                } else {
                    String::new()
                }
            } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                if let Some(arr) = value.as_array() {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    value.to_string()
                }
            } else if key.ends_with(".model") && value.is_object() {
                if let (Some(p), Some(id)) = (
                    value.get("provider").and_then(Value::as_str),
                    value.get("id").and_then(Value::as_str),
                ) {
                    format!("{p}:{id}")
                } else {
                    format!(
                        "{{ provider = {}, id = {} }}",
                        value["provider"], value["id"]
                    )
                }
            } else if let Some(s) = value.as_str() {
                s.into()
            } else if value.is_object() {
                format!(
                    "{{ provider = {}, id = {} }}",
                    value["provider"], value["id"]
                )
            } else {
                value.to_string()
            }
        }
        Field::Knob(key) => {
            clearable = runeengine::settings::ModelSettings::clearable(key);
            if let Some(k) = app.settings["knobs"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|k| k["key"].as_str() == Some(key))
            {
                choices = if k["kind"] == "bool" {
                    vec!["on".into(), "off".into()]
                } else {
                    k["choices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                };
            }
            if app.settings["settings"][key].is_null() {
                String::new()
            } else {
                panels::value_of(&app.settings["settings"], key)
            }
        }
        Field::Model => String::new(),
        Field::Timeout(name) => {
            clearable = true;
            provider_list(app)
                .into_iter()
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| p["timeout"].as_u64())
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::SetupKeyName => app
            .preferences
            .setup
            .as_ref()
            .map(|p| p.key_name.clone())
            .unwrap_or_else(|| "Primary".into()),
        Field::SetupKey => app
            .preferences
            .setup
            .as_ref()
            .map(|p| p.key.clone())
            .unwrap_or_default(),
        Field::Key(_) | Field::KeyValue { .. } | Field::RemoveKey(..) => String::new(),
        Field::NewCredentialStore => String::new(),
        Field::RemoveService(_) => String::new(),
        Field::BaseUrl(name) => {
            clearable = true;
            provider_list(app)
                .into_iter()
                .find(|p| p["name"].as_str() == Some(name))
                .and_then(|p| p["base_url"].as_str())
                .unwrap_or("")
                .to_string()
        }
        Field::ServiceUrl(name, svc) => {
            clearable = true;
            service_of(app, name)
                .and_then(|s| {
                    s[match svc {
                        Service::Search => "base_url",
                        Service::Fetch => "fetch_url",
                    }]
                    .as_str()
                })
                .unwrap_or("")
                .to_string()
        }
        Field::ServiceZone(name) => {
            choices = offered(app, &field);
            if choices.is_empty() {
                notice(app, "This account lists no SERP zone. Create one in the vendor's dashboard, then try again.", Tone::Error);
                return;
            }
            service_of(app, name)
                .and_then(|s| s["zone"].as_str())
                .unwrap_or("")
                .into()
        }
        Field::ServiceRouteText(..) => String::new(),
        Field::SetupUrl => app
            .preferences
            .setup
            .as_ref()
            .map(|d| d.url.clone())
            .unwrap_or_default(),
        Field::ModelContext { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.context)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelMaxOutput { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.max_output)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelInputPrice { provider, model } => {
            clearable = false;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .map(|p| p.input.to_string())
                .unwrap_or_default()
        }
        Field::ModelOutputPrice { provider, model } => {
            clearable = false;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .map(|p| p.output.to_string())
                .unwrap_or_default()
        }
        Field::ModelExtraRate {
            provider,
            model,
            key,
        } => app
            .lookup_model_info(provider, model)
            .and_then(|m| m.price)
            .and_then(|p| serde_json::to_value(p).ok())
            .and_then(|v| v.get(*key).and_then(Value::as_f64))
            .map(|n| n.to_string())
            .unwrap_or_default(),
        Field::ModelCachedPrice { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .map(|p| p.cached.to_string())
                .unwrap_or_default()
        }
        Field::ModelTierThreshold { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .and_then(|p| p.tier_context_threshold)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelTierInput { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .and_then(|p| p.tier_input)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelTierOutput { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .and_then(|p| p.tier_output)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelBusyInput { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .and_then(|p| p.busy_input)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelBusyOutput { provider, model } => {
            clearable = true;
            let info = app.lookup_model_info(provider, model);
            info.and_then(|m| m.price)
                .and_then(|p| p.busy_output)
                .map(|n| n.to_string())
                .unwrap_or_default()
        }
        Field::ModelReasoning { provider, model } => {
            clearable = false;
            let Some(m) = app.lookup_model_info(provider, model) else {
                notice(app, "Model metadata unavailable.", Tone::Error);
                return;
            };
            choices = m.reasoning_choices(provider_api(app, provider));
            if choices.is_empty() {
                notice(
                    app,
                    "This model does not advertise reasoning controls.",
                    Tone::Good,
                );
                return;
            }
            if choices.iter().any(|c| c != "off" && c != "on") {
                m.reasoning_options
                    .clone()
                    .unwrap_or_else(|| {
                        if m.thinking_required {
                            choices.clone()
                        } else {
                            vec!["off".into()]
                        }
                    })
                    .into_iter()
                    .filter(|c| choices.contains(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else if m.thinking_required || m.reasoning_enabled.unwrap_or(false) {
                "on".into()
            } else {
                "off".into()
            }
        }
    };
    let editor_id = next_editor_id();
    let confirmation = field.confirms();
    let help = match &field {
        Field::Key(_) => {
            "Enter a memorable name for this key (e.g. Personal, Work, Testing) ↵".into()
        }
        Field::KeyValue { .. } => {
            "Enter the API secret key (stored encrypted in ~/.rune/secrets.vault) ↵".into()
        }
        _ => row.help.clone(),
    };
    let is_multi = matches!(&field, Field::Global(k) if (k.ends_with(".excluded_tools") || k.ends_with(".context_tools")))
        || matches!(&field, Field::ModelReasoning { .. })
            && choices.iter().any(|c| c != "off" && c != "on");
    let has_choices = !choices.is_empty();
    let initial_cursor = if has_choices {
        if is_multi {
            choices
                .iter()
                .position(|c| text.split(',').any(|s| s.trim() == c))
                .unwrap_or(0)
        } else {
            choices.iter().position(|c| c == &text).unwrap_or(0)
        }
    } else {
        text.len()
    };
    app.preferences.editor = Some(Editor {
        id: editor_id,
        title: match &field {
            Field::SetupKeyName => format!(
                "Key name / {}",
                app.preferences
                    .setup
                    .as_ref()
                    .map(|p| p.preset.as_str())
                    .unwrap_or("provider")
            ),
            Field::SetupKey => {
                let (preset, key_name) = app
                    .preferences
                    .setup
                    .as_ref()
                    .map(|p| (p.preset.clone(), p.key_name.clone()))
                    .unwrap_or_else(|| ("provider".into(), String::new()));
                if key_name.is_empty() {
                    format!("API key / {preset}")
                } else {
                    format!("API key / {preset} ({key_name})")
                }
            }
            Field::Key(name) => format!("Key name / {name}"),
            Field::KeyValue { name, key_name } => format!("API key / {name} ({key_name})"),
            Field::Timeout(name) => format!("Request timeout / {name}"),
            Field::RemoveKey(name, _) => format!("Remove {} from {name}?", row.label),
            Field::RemoveService(name) => format!("Remove {name}? Its keys stay."),
            Field::NewCredentialStore => {
                "Set the old credential store aside and start a new one? Every key is added again."
                    .into()
            }
            Field::BaseUrl(name) => format!("Base URL / {name}"),
            Field::ServiceUrl(name, Service::Search) => format!("Address / {name}"),
            Field::ServiceUrl(name, Service::Fetch) => format!("Fetch URL / {name}"),
            // Opened only from the service's own page, whose path names the
            // connection already.
            Field::ServiceRouteText(_, svc) => {
                format!("Add a {} route", service_name(*svc))
            }
            Field::ServiceZone(name) => format!("SERP zone / {name}"),
            Field::SetupUrl => format!(
                "Base URL / {}",
                app.preferences
                    .setup
                    .as_ref()
                    .map(|p| p.preset.as_str())
                    .unwrap_or("account")
            ),
            Field::ModelContext { model, .. } => format!("{model} / Context window limit"),
            Field::ModelMaxOutput { model, .. } => format!("{model} / Max output tokens"),
            Field::ModelInputPrice { model, .. } => format!("{model} / Input price"),
            Field::ModelOutputPrice { model, .. } => format!("{model} / Output price"),
            Field::ModelExtraRate { model, key, .. } => format!(
                "{model} / {}",
                match *key {
                    "cache_write" => "Cache write price ($/M tokens)",
                    "cache_write_5m" => "5-minute cache write price ($/M tokens)",
                    "cache_write_1h" => "1-hour cache write price ($/M tokens)",
                    "busy_start_utc_hour" => "Peak start hour (UTC)",
                    _ => "Peak end hour (UTC)",
                }
            ),
            Field::ModelCachedPrice { model, .. } => format!("{model} / Cached read price"),
            Field::ModelTierThreshold { model, .. } => format!("{model} / Context tier threshold"),
            Field::ModelTierInput { model, .. } => format!("{model} / Tier input price"),
            Field::ModelTierOutput { model, .. } => format!("{model} / Tier output price"),
            Field::ModelBusyInput { model, .. } => format!("{model} / Peak hours input price"),
            Field::ModelBusyOutput { model, .. } => format!("{model} / Peak hours output price"),
            Field::ModelReasoning { model, .. } => format!("{model} / Reasoning"),
            _ => row.label.clone(),
        },
        field,
        help,
        cursor: initial_cursor,
        editing: super::editing::State::default(),
        text,
        choices,
        clearable,
        button: if confirmation {
            1
        } else if has_choices {
            CHOICES_FOCUS
        } else {
            0
        },
        confirm_apply_at: None,
    });
    app.preferences.notice = None;
}

fn check_aux_confirmation(app: &mut App) -> bool {
    let Some(e) = app.preferences.editor.as_mut() else {
        return false;
    };
    if !e.is_aux() {
        return true;
    }
    if e.is_confirming_apply() {
        e.confirm_apply_at = None;
        true
    } else {
        e.confirm_apply_at = Some(std::time::Instant::now());
        false
    }
}

fn save_editor(app: &mut App, d: &mut Driver) {
    let Some(e) = &app.preferences.editor else {
        return;
    };
    let field = e.field.clone();
    let text = e.text.trim().to_string();

    let is_model_cfg = matches!(
        field,
        Field::ModelContext { .. }
            | Field::ModelMaxOutput { .. }
            | Field::ModelInputPrice { .. }
            | Field::ModelOutputPrice { .. }
            | Field::ModelCachedPrice { .. }
            | Field::ModelExtraRate { .. }
            | Field::ModelTierThreshold { .. }
            | Field::ModelTierInput { .. }
            | Field::ModelTierOutput { .. }
            | Field::ModelBusyInput { .. }
            | Field::ModelBusyOutput { .. }
            | Field::ModelReasoning { .. }
    );
    if !is_model_cfg && app.preferences.pending.is_some() {
        return;
    }

    if e.is_aux() {
        if let Field::Global(ref key) = field {
            if key.ends_with(".max_output_tokens") {
                let trimmed = text.trim();
                if !trimmed.is_empty()
                    && !trimmed.eq_ignore_ascii_case("null")
                    && !trimmed.eq_ignore_ascii_case("auto")
                    && !trimmed.eq_ignore_ascii_case("inherit")
                {
                    let role = key.split('.').next().unwrap_or("");
                    let (_, m_id, max_out, _) = aux_model_spec(app, role);
                    match trimmed.parse::<u64>() {
                        Ok(v) if v >= 1 => {
                            if let Some(ceiling) = max_out {
                                if v > ceiling {
                                    notice(
                                        app,
                                        &format!("{key} cannot exceed {m_id} max output of {ceiling} tokens (from API catalog)."),
                                        Tone::Error,
                                    );
                                    return;
                                }
                            }
                        }
                        _ => {
                            notice(
                                app,
                                &format!("{key} must be an integer >= 1 or 'auto'."),
                                Tone::Error,
                            );
                            return;
                        }
                    }
                }
            } else if key == "compact.fallback_context_tokens" {
                let trimmed = text.trim();
                if !trimmed.is_empty()
                    && !trimmed.eq_ignore_ascii_case("null")
                    && !trimmed.eq_ignore_ascii_case("auto")
                    && !trimmed.eq_ignore_ascii_case("inherit")
                {
                    let (_, m_id, _, ctx) = aux_model_spec(app, "compact");
                    match trimmed.parse::<u64>() {
                        Ok(v) if v >= 100 => {
                            if let Some(window) = ctx {
                                if v > window {
                                    notice(
                                        app,
                                        &format!("{key} cannot exceed {m_id} context window of {} tokens (from API catalog).", thousands(window)),
                                        Tone::Error,
                                    );
                                    return;
                                }
                            }
                        }
                        _ => {
                            notice(
                                app,
                                &format!("{key} must be an integer >= 100 or 'auto'."),
                                Tone::Error,
                            );
                            return;
                        }
                    }
                }
            } else if let Some(f) = runeengine::settings::configuration::FIELDS
                .iter()
                .find(|f| f.key == key)
            {
                match f.kind {
                    runeengine::settings::configuration::Kind::Count(lo, hi) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.eq_ignore_ascii_case("null")
                            && !trimmed.eq_ignore_ascii_case("auto")
                            && !trimmed.eq_ignore_ascii_case("inherit")
                        {
                            let parsed: std::result::Result<u64, _> = trimmed.parse();
                            match parsed {
                                Ok(v) if v >= lo && v <= hi => {}
                                _ => {
                                    notice(
                                        app,
                                        &format!("{key} must be an integer between {lo} and {hi} or 'auto'."),
                                        Tone::Error,
                                    );
                                    return;
                                }
                            }
                        }
                    }
                    runeengine::settings::configuration::Kind::Duration => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.eq_ignore_ascii_case("null")
                            && !trimmed.eq_ignore_ascii_case("auto")
                            && !trimmed.eq_ignore_ascii_case("inherit")
                        {
                            if let Err(error) =
                                runeengine::settings::configuration::Configuration::parse_value(
                                    key, trimmed,
                                )
                            {
                                notice(app, &error.to_string(), Tone::Error);
                                return;
                            }
                        }
                    }
                    runeengine::settings::configuration::Kind::Size => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.eq_ignore_ascii_case("null")
                            && !trimmed.eq_ignore_ascii_case("auto")
                            && !trimmed.eq_ignore_ascii_case("inherit")
                        {
                            let max = if key == "classifier.max_evidence_size" {
                                64 * 1024 * 1024
                            } else if key == "classifier.max_policy_size" {
                                16 * 1024 * 1024
                            } else {
                                1024 * 1024 * 1024
                            };
                            match runeengine::settings::configuration::parse_size_sum(trimmed) {
                                Some(b) if b > 0 && b <= max => {}
                                Some(_) => {
                                    notice(
                                        app,
                                        &format!("{key} cannot exceed {}MiB.", max / (1024 * 1024)),
                                        Tone::Error,
                                    );
                                    return;
                                }
                                None => {
                                    notice(
                                        app,
                                        &format!(
                                            "{key} must be a valid size (e.g. 128KiB, 16MiB)."
                                        ),
                                        Tone::Error,
                                    );
                                    return;
                                }
                            }
                        }
                    }
                    runeengine::settings::configuration::Kind::Model => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty()
                            && !trimmed.eq_ignore_ascii_case("null")
                            && !trimmed.eq_ignore_ascii_case("inherit")
                            && !trimmed.eq_ignore_ascii_case("default")
                            && !trimmed.eq_ignore_ascii_case("auto")
                            && !trimmed.eq_ignore_ascii_case("none")
                            && !trimmed.starts_with('{')
                        {
                            if let Some((p, m)) =
                                trimmed.split_once(':').or_else(|| trimmed.split_once('/'))
                            {
                                if p.trim().is_empty() || m.trim().is_empty() {
                                    notice(
                                        app,
                                        "Model must specify both provider and model ID (e.g. openai:gpt-4o).",
                                        Tone::Error,
                                    );
                                    return;
                                }
                            } else {
                                notice(
                                    app,
                                    "Model must be in '{provider}:{modelID}' format.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    match field {
        Field::Knob(ref key) if key == "max_output" => {
            let trimmed = text.trim();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("auto")
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("default")
            {
                apply(app, key, "", d);
            } else {
                let ceiling = app.settings["models"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|m| m["id"].as_str() == Some(app.model()))
                            .and_then(|m| m["max_output"].as_u64())
                    })
                    .or_else(|| app.settings["max_output_ceiling"].as_u64());
                match trimmed.parse::<u64>() {
                    Ok(v) if v >= 1 => {
                        if let Some(c) = ceiling {
                            if v > c {
                                notice(
                                    app,
                                    &format!("max_output cannot exceed {} max output of {c} tokens (from API catalog).", app.model()),
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                        apply(app, key, &v.to_string(), d);
                    }
                    _ => {
                        notice(
                            app,
                            "max_output must be an integer >= 1 or 'auto'.",
                            Tone::Error,
                        );
                    }
                }
            }
        }
        Field::Knob(key) => apply(app, &key, &text, d),
        Field::Global(key) => {
            let trimmed = text.trim();
            let field_def = runeengine::settings::configuration::FIELDS
                .iter()
                .find(|f| f.key == key);
            let field_kind = field_def.map(|f| f.kind);

            let val = if trimmed.eq_ignore_ascii_case("auto")
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("inherit")
                || trimmed.is_empty()
            {
                "null".to_string()
            } else if matches!(
                field_kind,
                Some(runeengine::settings::configuration::Kind::Bool)
            ) || trimmed == "true"
                || trimmed == "false"
            {
                if trimmed == "true" || trimmed == "false" {
                    trimmed.to_string()
                } else if trimmed == "on" {
                    "true".to_string()
                } else if trimmed == "off" {
                    "false".to_string()
                } else {
                    notice(app, &format!("{key} must be true or false."), Tone::Error);
                    return;
                }
            } else if key.ends_with(".excluded_tools") || key.ends_with(".context_tools") {
                if trimmed == "[]" {
                    "[]".to_string()
                } else if trimmed.starts_with('[') {
                    trimmed.to_string()
                } else {
                    let items: Vec<String> = trimmed
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').trim_matches('\''))
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("\"{s}\""))
                        .collect();
                    format!("[{}]", items.join(", "))
                }
            } else if matches!(
                field_kind,
                Some(runeengine::settings::configuration::Kind::Duration)
            ) || (field_kind.is_none() && key.ends_with(".timeout"))
            {
                match runeengine::settings::configuration::Configuration::parse_value(&key, trimmed)
                {
                    Ok(value) => value.to_string(),
                    Err(error) => {
                        notice(app, &error.to_string(), Tone::Error);
                        return;
                    }
                }
            } else if let Some(runeengine::settings::configuration::Kind::Count(min, max)) =
                field_kind
            {
                match trimmed.parse::<u64>() {
                    Ok(n) if (min..=max).contains(&n) => n.to_string(),
                    _ => {
                        notice(
                            app,
                            &format!("{key} must be an integer between {min} and {max}."),
                            Tone::Error,
                        );
                        return;
                    }
                }
            } else if matches!(
                field_kind,
                Some(runeengine::settings::configuration::Kind::Size)
            ) || key.ends_with("_size")
            {
                if let Some(bytes) = runeengine::settings::configuration::parse_size_sum(trimmed) {
                    if bytes % (1024 * 1024) == 0 && bytes > 0 {
                        format!("\"{}MiB\"", bytes / (1024 * 1024))
                    } else if bytes % 1024 == 0 && bytes > 0 {
                        format!("\"{}KiB\"", bytes / 1024)
                    } else {
                        format!("\"{}B\"", bytes)
                    }
                } else {
                    "null".to_string()
                }
            } else {
                trimmed.to_string()
            };
            apply(app, &format!("config.{key}"), &val, d)
        }
        Field::Model => {
            let provider = if model_provider(app) == "all" {
                app.provider().to_string()
            } else {
                model_provider(app).to_string()
            };
            select_model(app, provider, text, true, d);
        }
        Field::Key(name) => {
            let key_name = text.trim().to_string();
            if key_name.is_empty() {
                notice(app, "Key name cannot be empty.", Tone::Error);
                return;
            }
            let key_id = next_editor_id();
            app.preferences.editor = Some(Editor {
                id: key_id,
                title: format!("API key / {name} ({key_name})"),
                field: Field::KeyValue { name, key_name },
                help:
                    "Enter the API key secret string (stored encrypted in ~/.rune/secrets.vault) ↵"
                        .into(),
                cursor: 0,
                editing: super::editing::State::default(),
                text: String::new(),
                choices: Vec::new(),
                clearable: false,
                button: 0,
                confirm_apply_at: None,
            });
            app.preferences.notice = None;
        }
        Field::KeyValue { name, key_name } => {
            let key = text.trim().to_string();
            if key.is_empty() {
                notice(app, "API key secret cannot be empty.", Tone::Error);
                return;
            }
            send(
                app,
                d,
                Operation::Registry("API key added. Test the connection to verify it.".into()),
                "Saving API key…",
                |id| Command::ProviderKey {
                    id,
                    name,
                    key_name: Some(key_name),
                    key,
                },
            );
        }
        Field::Timeout(name) => {
            let seconds = if text.is_empty() {
                None
            } else if let Some(ms) = runeengine::settings::configuration::parse_duration_sum(&text)
            {
                let secs = (ms / 1000).max(1);
                if !(5..=3600).contains(&secs) {
                    notice(
                        app,
                        "Request timeout must be from 5 to 3600 seconds.",
                        Tone::Error,
                    );
                    return;
                }
                Some(secs)
            } else {
                notice(
                    app,
                    "Request timeout must be a valid duration (e.g. 30s, 3m).",
                    Tone::Error,
                );
                return;
            };
            send(
                app,
                d,
                Operation::Registry("Request timeout updated.".into()),
                "Saving timeout…",
                |id| Command::ProviderTimeout { id, name, seconds },
            );
        }
        Field::RemoveKey(name, position) => {
            send(
                app,
                d,
                Operation::Registry("API key removed.".into()),
                "Removing API key…",
                |id| Command::ProviderDropKey { id, name, position },
            );
        }
        Field::NewCredentialStore => {
            send(
                app,
                d,
                Operation::Registry(
                    "New credential store started; the old one is kept in the backups folder. Add keys again.".into(),
                ),
                "Setting the old credential store aside…",
                |id| Command::CredentialsNew { id },
            );
        }
        Field::BaseUrl(name) => {
            let base_url = (!text.is_empty()).then(|| text.clone());
            send(
                app,
                d,
                Operation::Service(format!(
                    "{name}: address saved after a request there succeeded."
                )),
                "Checking the new address…",
                |id| Command::ProviderBaseUrl { id, name, base_url },
            );
        }
        Field::ServiceUrl(name, svc) => {
            let mut state = ServiceState::of(app, &name);
            let chosen = (!text.is_empty()).then(|| text.clone());
            match svc {
                Service::Search => state.base_url = chosen,
                Service::Fetch => state.fetch_url = chosen,
            }
            send(
                app,
                d,
                Operation::Service(format!("{name}: account address saved.")),
                "Saving account address…",
                |id| Command::ServiceAccount {
                    id,
                    name,
                    api: Some(state.api),
                    base_url: state.base_url,
                    fetch_url: state.fetch_url,
                    key_name: None,
                    key: None,
                },
            );
        }
        Field::ServiceRouteText(name, svc) => {
            let mut state = ServiceState::of(app, &name);
            // Kept in the spelling the gateway's doors take, as the engine
            // keeps it: `exa/search` copied from its list is `exa`.
            let Some(route) =
                runeengine::web::adapter(&state.api).and_then(|a| a.route(svc, &text))
            else {
                notice(app, "A route cannot be empty.", Tone::Error);
                return;
            };
            if state.kept(svc).contains(&route) {
                notice(
                    app,
                    &format!("{route} is already kept for {}.", service_name(svc)),
                    Tone::Good,
                );
                app.preferences.editor = None;
                return;
            }
            state.kept(svc).push(route.clone());
            state.send(
                app,
                d,
                &format!(
                    "{name}: {route} checked and kept. Choose it in the list to use it for {}.",
                    service_name(svc)
                ),
                &format!("Checking {route} with a real request…"),
            );
        }
        Field::ServiceZone(name) => {
            let mut state = ServiceState::of(app, &name);
            state.zone = Some(text.clone());
            if state.search.is_none() {
                state.search = Some(runeengine::proto::ServiceUse::default());
            }
            state.send(
                app,
                d,
                &format!("{name}: zone {text} saved after its check passed."),
                "Checking the zone with a real search…",
            );
        }
        Field::RemoveService(name) => {
            send(
                app,
                d,
                Operation::Service(format!("{name} removed. Its keys stay in the vault.")),
                "Removing…",
                |id| Command::ServiceRemove { id, name },
            );
        }
        Field::SetupUrl => {
            if let Some(p) = app.preferences.setup.as_mut() {
                p.url = text.clone();
            }
            app.preferences.editor = None;
            let needs_key = app
                .preferences
                .setup
                .as_ref()
                .and_then(|p| adapter_of(app, &p.preset))
                .is_some_and(|a| a["key"] != "none" && a["keys"].as_u64().unwrap_or(0) == 0);
            if needs_key {
                edit_now(app, Field::SetupKeyName, "Key name", "");
            } else {
                connect_service(app, d);
            }
        }
        Field::SetupKeyName => {
            let key_name = if text.trim().is_empty() {
                "Primary".to_string()
            } else {
                text.trim().to_string()
            };
            if let Some(p) = app.preferences.setup.as_mut() {
                p.key_name = key_name;
            }
            app.preferences.editor = None;
            let r_list = rows(app);
            if let Some(r) = r_list
                .iter()
                .find(|r| matches!(&r.kind, Kind::Edit(Field::SetupKey)))
            {
                open_editor(app, Field::SetupKey, r);
            }
        }
        Field::SetupKey => {
            if let Some(p) = app.preferences.setup.as_mut() {
                p.key = text.clone();
            }
            app.preferences.editor = None;
            if let Some(draft) = app.preferences.setup.as_ref().filter(|p| p.service) {
                let adapter = adapter_of(app, &draft.preset);
                let required = adapter.is_none_or(|a| a["key"] == "required")
                    && adapter.and_then(|a| a["keys"].as_u64()).unwrap_or(0) == 0;
                if text.trim().is_empty() && required {
                    notice(app, "API key cannot be empty.", Tone::Error);
                    return;
                }
                connect_service(app, d);
                return;
            }
            let trimmed = text.trim();
            if trimmed.is_empty() {
                notice(app, "API key cannot be empty.", Tone::Error);
                return;
            }
            if let Some(p) = &app.preferences.setup {
                let (name, preset, base_url, key_name, key, model) = (
                    p.name.clone(),
                    Some(p.preset.clone()),
                    (!p.url.is_empty()).then(|| p.url.clone()),
                    (!p.key_name.is_empty()).then(|| p.key_name.clone()),
                    Some(trimmed.to_string()),
                    (!p.model.is_empty()).then(|| p.model.clone()),
                );
                send(
                    app,
                    d,
                    Operation::Added(name.clone()),
                    "Verifying API key with provider…",
                    |id| Command::ProviderAdd {
                        id,
                        name,
                        preset,
                        base_url,
                        key_name,
                        key,
                        model,
                    },
                );
            }
        }
        Field::ModelContext {
            ref provider,
            ref model,
        }
        | Field::ModelMaxOutput {
            ref provider,
            ref model,
        }
        | Field::ModelInputPrice {
            ref provider,
            ref model,
        }
        | Field::ModelOutputPrice {
            ref provider,
            ref model,
        }
        | Field::ModelCachedPrice {
            ref provider,
            ref model,
        }
        | Field::ModelExtraRate {
            ref provider,
            ref model,
            ..
        }
        | Field::ModelTierThreshold {
            ref provider,
            ref model,
        }
        | Field::ModelTierInput {
            ref provider,
            ref model,
        }
        | Field::ModelTierOutput {
            ref provider,
            ref model,
        }
        | Field::ModelBusyInput {
            ref provider,
            ref model,
        }
        | Field::ModelBusyOutput {
            ref provider,
            ref model,
        }
        | Field::ModelReasoning {
            ref provider,
            ref model,
        } => {
            let mut info = app.lookup_model_info(provider, model).unwrap_or_else(|| {
                runeengine::settings::providers::ModelInfo {
                    id: model.clone(),
                    ..Default::default()
                }
            });
            match &field {
                Field::ModelContext { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        info.context = None;
                    } else {
                        match trimmed.replace(['_', ','], "").parse::<u64>() {
                            Ok(n) if n > 0 => info.context = Some(n),
                            _ => {
                                notice(
                                    app,
                                    "Context window limit must be a positive integer or empty.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    }
                }
                Field::ModelMaxOutput { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        info.max_output = None;
                    } else {
                        match trimmed.replace(['_', ','], "").parse::<u64>() {
                            Ok(n) if n > 0 => info.max_output = Some(n),
                            _ => {
                                notice(
                                    app,
                                    "Max output tokens must be a positive integer or empty.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    }
                }
                Field::ModelInputPrice { .. } => {
                    let trimmed = text.trim();
                    match trimmed.parse::<f64>() {
                        Ok(n) if n.is_finite() && n >= 0.0 => {
                            let mut price = info.price.unwrap_or_default();
                            price.input = n;
                            info.price = Some(price);
                        }
                        _ => {
                            notice(
                                app,
                                "Input price must be a non-negative number in USD.",
                                Tone::Error,
                            );
                            return;
                        }
                    }
                }
                Field::ModelOutputPrice { .. } => {
                    let trimmed = text.trim();
                    match trimmed.parse::<f64>() {
                        Ok(n) if n.is_finite() && n >= 0.0 => {
                            let mut price = info.price.unwrap_or_default();
                            price.output = n;
                            info.price = Some(price);
                        }
                        _ => {
                            notice(
                                app,
                                "Output price must be a non-negative number in USD.",
                                Tone::Error,
                            );
                            return;
                        }
                    }
                }
                Field::ModelExtraRate { key, .. } => {
                    let value = if text.trim().is_empty() {
                        Value::Null
                    } else {
                        match text.trim().parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => serde_json::json!(n),
                            _ => {
                                notice(
                                    app,
                                    "Price must be finite and nonnegative, or empty when unknown.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    };
                    let mut encoded =
                        serde_json::to_value(info.price.take().unwrap_or_default()).unwrap();
                    encoded[*key] = if key.ends_with("utc_hour") && !value.is_null() {
                        let hour = value.as_f64().unwrap();
                        if hour.fract() != 0.0 || hour > 23.0 {
                            notice(
                                app,
                                "UTC hour must be a whole number from 0 to 23.",
                                Tone::Error,
                            );
                            return;
                        }
                        serde_json::json!(hour as u8)
                    } else {
                        value
                    };
                    info.price = Some(serde_json::from_value(encoded).unwrap());
                }
                Field::ModelCachedPrice { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.cached = price.input;
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.cached = n;
                                info.price = Some(price);
                            }
                            _ => {
                                notice(
                                    app,
                                    "Cached price must be a non-negative number in USD or empty.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    }
                }
                Field::ModelTierThreshold { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.tier_context_threshold = None;
                            price.tier_input = price.tier_context_threshold.map(|_| price.input);
                            price.tier_output = price.tier_context_threshold.map(|_| price.output);
                        }
                    } else {
                        match trimmed.replace(['_', ','], "").parse::<u64>() {
                            Ok(n) if n > 0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.tier_context_threshold = Some(n);
                                price.tier_input.get_or_insert(price.input);
                                price.tier_output.get_or_insert(price.output);
                                info.price = Some(price);
                            }
                            _ => {
                                notice(
                                    app,
                                    "Tier threshold must be a number of tokens or empty.",
                                    Tone::Error,
                                );
                                return;
                            }
                        }
                    }
                }
                Field::ModelTierInput { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.tier_input = price.tier_context_threshold.map(|_| price.input);
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.tier_input = Some(n);
                                info.price = Some(price);
                            }
                            _ => {
                                notice(app, "Tier input price must be a non-negative number in USD or empty.", Tone::Error);
                                return;
                            }
                        }
                    }
                }
                Field::ModelTierOutput { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.tier_output = price.tier_context_threshold.map(|_| price.output);
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.tier_output = Some(n);
                                info.price = Some(price);
                            }
                            _ => {
                                notice(app, "Tier output price must be a non-negative number in USD or empty.", Tone::Error);
                                return;
                            }
                        }
                    }
                }
                Field::ModelBusyInput { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.busy_input = None;
                            price.busy_output = None;
                            price.busy_input = None;
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.busy_input = Some(n);
                                price.busy_output.get_or_insert(price.output);
                                info.price = Some(price);
                            }
                            _ => {
                                notice(app, "Peak hours input price must be a non-negative number in USD or empty.", Tone::Error);
                                return;
                            }
                        }
                    }
                }
                Field::ModelBusyOutput { .. } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
                        if let Some(price) = info.price.as_mut() {
                            price.busy_output = None;
                            price.busy_input = None;
                        }
                    } else {
                        match trimmed.parse::<f64>() {
                            Ok(n) if n.is_finite() && n >= 0.0 => {
                                let mut price = info.price.unwrap_or_default();
                                price.busy_output = Some(n);
                                price.busy_input.get_or_insert(price.input);
                                info.price = Some(price);
                            }
                            _ => {
                                notice(app, "Peak hours output price must be a non-negative number in USD or empty.", Tone::Error);
                                return;
                            }
                        }
                    }
                }
                Field::ModelReasoning { provider, .. } => {
                    let choices = info.reasoning_choices(provider_api(app, provider));
                    let selected: Vec<String> = text
                        .split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(String::from)
                        .collect();
                    if selected.is_empty() || selected.iter().any(|v| !choices.contains(v)) {
                        notice(
                            app,
                            "Choose at least one advertised reasoning option.",
                            Tone::Error,
                        );
                        return;
                    }
                    // Never overwrite capability, mandatory vendor policy, or
                    // API effort list with the user's current On/Off selection.
                    info.reasoning_enabled = Some(selected.iter().any(|v| v != "off"));
                    info.reasoning_options = Some(selected.clone());
                    info.default_effort =
                        selected.iter().find(|v| *v != "off" && *v != "on").cloned();
                }
                _ => {}
            }
            let p_clone = provider.clone();
            let m_clone = model.clone();
            let mut info_clone = info.clone();
            if info_clone.price.is_some()
                && !info_clone.manual_fields.contains(&"price".to_string())
            {
                info_clone.manual_fields.push("price".into());
            }
            if info_clone.context.is_some()
                && !info_clone.manual_fields.contains(&"context".to_string())
            {
                info_clone.manual_fields.push("context".into());
            }
            if info_clone.max_output.is_some()
                && !info_clone.manual_fields.contains(&"max_output".to_string())
            {
                info_clone.manual_fields.push("max_output".into());
            }
            // Optimistically update local cache and context immediately so the screen updates with zero delay!
            app.update_cached_model(&p_clone, &info_clone);
            app.sync_active_models();
            if app.provider() == p_clone && app.model() == m_clone {
                if let Some(ctx) = info_clone.context {
                    let max_out = info_clone.max_output.unwrap_or(0);
                    let reserve = if max_out > 0 && max_out < ctx {
                        max_out
                    } else {
                        0
                    };
                    app.context = ctx - reserve;
                } else {
                    app.context = 0;
                }
            }
            app.request(
                d,
                super::Awaiting::ModelConfig(p_clone.clone(), m_clone.clone(), None),
                |id| Command::ProviderConfigureModel {
                    id,
                    provider: p_clone,
                    model: Box::new(info_clone),
                },
            );
            app.preferences.editor = None;
            app.status = Some("Model configuration saved.".into());
        }
    }
}

fn notice(app: &mut App, text: &str, tone: Tone) {
    app.preferences.notice = Some(Notice {
        page: app.preferences.location.page.clone(),
        text: text.into(),
        tone,
        created: Instant::now(),
        page_offset: 0,
    });
}

/// Re-read the active provider's model list from the registry into the session's
/// settings snapshot.
///
/// The model picker draws the *active* provider from `settings["models"]` and
/// every other provider from the registry. A model add, delete, edit, or a
/// catalogue refresh mutates the registry, but only a refresh of the active
/// provider used to copy the result back into the snapshot — so adds and deletes
/// on the provider the user is actually working in stayed invisible until the
/// engine re-read its settings on the next launch. One function, called after any
/// registry mutation, so the two sources cannot drift.
fn send(
    app: &mut App,
    d: &mut Driver,
    operation: Operation,
    message: &str,
    make: impl FnOnce(u64) -> Command,
) {
    if !matches!(operation, Operation::Setting(_)) && app.preferences.pending.is_some() {
        notice(app, "Wait for the current operation to finish.", Tone::Busy);
        return;
    }
    app.preferences.serial += 1;
    let token = app.preferences.serial;
    let request = Request {
        token,
        session: app.session,
        origin: app.preferences.location.page.clone(),
        editor: app.preferences.editor.as_ref().map(|e| e.id),
        before: matches!(operation, Operation::Setting(_))
            .then(|| app.settings["settings"].clone()),
        operation,
    };
    app.preferences.pending = Some(token);
    notice(app, message, Tone::Busy);
    app.request(d, Awaiting::Preferences(request), make);
}

pub fn apply(app: &mut App, key: &str, value: &str, d: &mut Driver) {
    if let Some(mut draft) = app.composition.draft.clone() {
        draft.composer = app.composer_snapshot();
        let key = key.to_string();
        let value = value.to_string();
        send(
            app,
            d,
            Operation::Setting(Some(key.clone())),
            "Saving draft settings…",
            |id| Command::DraftSettings {
                id,
                draft: Box::new(draft),
                key: Some(key),
                value,
                provider: None,
                custom: false,
            },
        );
        return;
    }
    let Some(session) = app.session else {
        return;
    };
    let (key_str, value) = (key.to_string(), value.to_string());
    send(
        app,
        d,
        Operation::Setting(Some(key_str.clone())),
        "Saving session settings…",
        |id| Command::SettingsSet {
            id,
            session,
            key: key_str,
            value,
        },
    );
}

pub fn select_model(app: &mut App, provider: String, model: String, custom: bool, d: &mut Driver) {
    if let Some(mut draft) = app.composition.draft.clone() {
        draft.composer = app.composer_snapshot();
        send(
            app,
            d,
            Operation::Setting(Some("model".into())),
            "Applying draft model…",
            |id| Command::DraftSettings {
                id,
                draft: Box::new(draft),
                key: Some("model".into()),
                value: model,
                provider: Some(provider),
                custom,
            },
        );
        return;
    }
    let Some(session) = app.session else {
        return;
    };
    send(
        app,
        d,
        Operation::Setting(Some("model".into())),
        if custom {
            "Applying custom model…"
        } else {
            "Applying provider and model…"
        },
        |id| Command::SettingsSelect {
            id,
            session,
            provider,
            model,
            custom,
        },
    );
}

pub fn ack(app: &mut App, request: Request, ok: bool, error: Option<String>, data: Option<Value>) {
    if request.token < app.preferences.serial {
        return;
    }
    if app.preferences.pending == Some(request.token) {
        app.preferences.pending = None;
    }
    let data = data.unwrap_or(Value::Null);
    if let Some(registry) = data.get("registry") {
        app.providers = registry.clone();
        // Who serves the web tools moved with the connections.
        if !registry["web"].is_null() {
            app.settings["web"] = registry["web"].clone();
        }
    }
    if ok && data["web_pins"].is_object() {
        for svc in [Service::Search, Service::Fetch] {
            let section = match svc {
                Service::Search => "web_search",
                Service::Fetch => "web_fetch",
            };
            if !app.settings["configuration"][section].is_object() {
                app.settings["configuration"][section] = serde_json::json!({});
            }
            app.settings["configuration"][section]["provider"] =
                data["web_pins"][service_name(svc)].clone();
        }
    }
    let text = if ok {
        // Take in what the engine wrote, if it wrote anything: a forced
        // reload counted as a change and made every window read its settings
        // again for nothing.
        let _ = super::runtime_configuration().reload_if_changed();
        super::ansi::invalidate_depth();
        super::theme::reload();
        if request.editor.is_some()
            && app.preferences.editor.as_ref().map(|e| e.id) == request.editor
        {
            app.preferences.editor = None;
        }
        match &request.operation {
            Operation::Setting(key) => {
                if request.session == app.session {
                    app.take_settings(data.clone());
                    if app.composition.draft.is_some() {
                        app.composition.authoritative.preference_changed = true;
                        if let Some(draft) = app.composition.draft.as_mut() {
                            draft.composer.preference_changed = true;
                        }
                        app.composition.dirty_since.get_or_insert_with(Instant::now);
                    }
                }
                if let Some(k) = key.as_deref() {
                    if k == "classifier.reload" {
                        let reason = data["classifier_status"]["reason"]
                            .as_str()
                            .unwrap_or("")
                            .trim();
                        if reason.is_empty() {
                            "Classifier policy reloaded successfully.".to_string()
                        } else {
                            format!("Classifier policy reloaded: {reason}.")
                        }
                    } else if k.ends_with("classifier.enabled") {
                        let on = data["configuration"]["classifier.enabled"]
                            .as_bool()
                            .unwrap_or(true);
                        let reason = data["classifier_status"]["reason"]
                            .as_str()
                            .unwrap_or("")
                            .trim();
                        if on {
                            if reason.is_empty() {
                                "Classifier enabled for autonomous mode.".to_string()
                            } else {
                                format!("Classifier enabled: {reason}.")
                            }
                        } else {
                            if reason.is_empty() {
                                "Classifier disabled by configuration.".to_string()
                            } else {
                                format!("Classifier disabled: {reason}.")
                            }
                        }
                    } else if k.contains("classifier.model") {
                        let cfg = &data["configuration"];
                        let m = cfg
                            .get("classifier")
                            .and_then(|c| c.get("model"))
                            .and_then(Value::as_object)
                            .or_else(|| cfg.get("classifier.model").and_then(Value::as_object));
                        let desc = match m {
                            Some(obj) => format!(
                                "{}/{}",
                                obj.get("provider").and_then(Value::as_str).unwrap_or(""),
                                obj.get("id").and_then(Value::as_str).unwrap_or("")
                            ),
                            None => "inherited from active session".to_string(),
                        };
                        format!("Classifier model set to {desc}.")
                    } else if k.contains("classifier.") {
                        let reason = data["classifier_status"]["reason"]
                            .as_str()
                            .unwrap_or("")
                            .trim();
                        if reason.is_empty() {
                            "Classifier settings saved successfully.".to_string()
                        } else {
                            format!("Classifier settings saved: {reason}.")
                        }
                    } else {
                        format!(
                            "Settings saved. Active: {} via {}.",
                            data["settings"]["model"].as_str().unwrap_or("model"),
                            data["settings"]["provider"].as_str().unwrap_or("provider")
                        )
                    }
                } else {
                    format!(
                        "Settings saved. Active: {} via {}.",
                        data["settings"]["model"].as_str().unwrap_or("model"),
                        data["settings"]["provider"].as_str().unwrap_or("provider")
                    )
                }
            }
            Operation::Registry(message) => {
                if let Some(name) = data["stored"].as_str().or(data["name"].as_str()) {
                    app.preferences.health.remove(name);
                }
                // A model was added, removed, or edited: the registry above already
                // reflects it, so the active provider's snapshot must follow suit
                // now, not on the next launch.
                app.sync_active_models();
                message.clone()
            }
            Operation::Catalogue(name) => {
                // The refresh mutated the registry for `name`; mirror it into the
                // session snapshot when `name` is the active provider, so a model
                // the endpoint just published appears without a restart.
                app.sync_active_models();
                let count = data["configured"].as_array().map(Vec::len).unwrap_or(0);
                let skipped = data["skipped_rows"].as_u64().unwrap_or(0);
                if data["cached"].as_bool().unwrap_or(false) {
                    format!(
                        "{name}: showing {count} configured models; endpoint returned no catalog."
                    )
                } else if skipped > 0 {
                    format!(
                        "{name}: {count} models refreshed ({skipped} unreadable row(s) skipped)."
                    )
                } else {
                    format!("{name}: {count} models refreshed.")
                }
            }
            Operation::Probe(name) => {
                if let Some(m) = data.get("model").and_then(Value::as_str) {
                    format!("{name}: connection check passed for {m}.")
                } else {
                    let count = data
                        .get("models_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    format!("{name}: connection verified successfully ({count} models available).")
                }
            }
            Operation::Added(name) => {
                if app.preferences.location.page == request.origin
                    && app.preferences.editor.is_none()
                {
                    app.preferences.navigate(Page::Provider(name.clone()));
                }
                format!("{name} connected and saved. Setup check passed.")
            }
            Operation::Connected(name) => {
                if app.preferences.location.page == request.origin
                    && app.preferences.editor.is_none()
                {
                    app.preferences.navigate(Page::Provider(name.clone()));
                }
                format!("{name} account saved. Add and check providers in Tools.")
            }
            Operation::Service(message) => {
                if let Some(gone) = data["removed"].as_str() {
                    // Out of every page of the connection that went.
                    while match &app.preferences.location.page {
                        Page::Provider(name) => name == gone,
                        _ => false,
                    } && app.preferences.editor.is_none()
                    {
                        if !app.preferences.back() {
                            break;
                        }
                    }
                }
                message.clone()
            }
            Operation::Options(_) => {
                app.preferences.offered = data.clone();
                "Choose from what it offers.".into()
            }
        }
    } else if matches!(
        request.operation,
        Operation::Options(_) | Operation::Service(_) | Operation::Connected(_)
    ) {
        // The engine's sentence names the next step for this vendor (a
        // SearXNG that serves no JSON, a key over plain http); a generic
        // reading of its status would lose it.
        let raw = error.as_deref().unwrap_or("");
        let message = ["provider: ", "invalid: ", "not found: "]
            .iter()
            .find_map(|p| raw.strip_prefix(p))
            .unwrap_or(raw)
            .trim();
        if message.is_empty() {
            "The request failed. Check the address and the key, then retry.".into()
        } else {
            safe(message)
        }
    } else {
        if let Some(before) = &request.before {
            app.settings["settings"] = before.clone();
        }
        let raw = error.as_deref().unwrap_or("");
        if raw.contains("HTTP 401") {
            "API key rejected (HTTP 401). Update the key in Connections, then retry.".into()
        } else if raw.contains("HTTP 403") {
            "Access denied (HTTP 403). Check this key's permissions and model access.".into()
        } else if raw.contains("HTTP 429") {
            "Provider rate limit reached (HTTP 429). Wait and retry, or add another key.".into()
        } else if raw.starts_with("could not reach") {
            format!("Connection failed: {raw}")
        } else if let Some(message) = raw.strip_prefix("invalid: ") {
            if message.contains("TOML parse error")
                || message.contains("syntax error")
                || message.contains("expected literal string")
                || message.contains("string values must be quoted")
            {
                "Invalid syntax or format. Please check the input value.".into()
            } else {
                safe(message)
            }
        } else if let Some(message) = raw.strip_prefix("provider: ") {
            let msg = message.trim();
            if msg.is_empty() {
                "Provider request failed. Check the endpoint, API key and model ID, then retry."
                    .into()
            } else {
                safe(msg)
            }
        } else if let Some(message) = raw.strip_prefix("provider:") {
            let msg = message.trim();
            if msg.is_empty() {
                "Provider request failed. Check the endpoint, API key and model ID, then retry."
                    .into()
            } else {
                safe(msg)
            }
        } else if !raw.is_empty() {
            safe(raw)
        } else {
            "Operation failed. Check configuration access and try again.".into()
        }
    };
    let page = if ok
        && matches!(
            request.operation,
            Operation::Added(_) | Operation::Connected(_) | Operation::Options(_)
        )
        || (ok && data["removed"].is_string())
    {
        app.preferences.location.page.clone()
    } else {
        request.origin.clone()
    };
    let n = Notice {
        page,
        text: text.clone(),
        tone: if ok { Tone::Good } else { Tone::Error },
        created: Instant::now(),
        page_offset: 0,
    };
    match &request.operation {
        Operation::Probe(name) | Operation::Added(name) | Operation::Connected(name) => {
            app.preferences.health.insert(name.clone(), n.clone());
        }
        Operation::Catalogue(name) if !ok => {
            app.preferences.health.insert(name.clone(), n.clone());
        }
        _ => {}
    }
    if request.session == app.session {
        app.preferences.notice = Some(n);
        let is_reasoning_setting = matches!(&request.operation, Operation::Setting(Some(k)) if k == "thinking" || k == "effort");
        if app.panel == super::Panel::Settings || !is_reasoning_setting {
            app.status = Some(text);
        }
    }
    // What the vendor offered opens as a choice, on the page it was asked from.
    if let (true, Operation::Options(field)) = (ok, &request.operation) {
        if app.preferences.location.page == request.origin {
            let (label, help) = (
                "SERP zone",
                "The account's SERP zones; searches run through the one chosen.",
            );
            edit_now(app, field.clone(), label, help);
        }
    }
    settle(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn product_help_covers_every_settings_page_and_rendered_global_control() {
        use runeengine::capabilities::product::document;
        let matrix: Value = serde_json::from_str(include_str!(
            "../../../Engine/src/Capabilities/product_coverage.json"
        ))
        .unwrap();
        let mut app = app();
        for (name, page) in SECTIONS {
            app.preferences.navigate(page.clone());
            let help_path = matrix["pages"][*name].as_str().unwrap();
            let help = document(help_path).unwrap();
            assert!(help.contains(name), "menu {name}");
            for row in rows(&app) {
                if let Kind::Edit(Field::Global(key)) = row.kind {
                    let path = matrix["fields"][&key]["document"]
                        .as_str()
                        .expect("document each new key");
                    assert!(
                        document(path).unwrap().contains(&key),
                        "undocumented visible key {key}"
                    );
                }
            }
        }
        for name in [
            "ModelConfig",
            "ModelTierPricing",
            "ModelBusyPricing",
            "Provider",
            "AddProvider",
            "Setup",
            "Themes",
            "WebProviders",
            "AddWebProvider",
        ] {
            assert!(
                document(matrix["pages"][name].as_str().unwrap()).is_some(),
                "nested menu {name}"
            );
        }
    }

    /// Owner, 2026-10-16: a key whose file is gone is never shown as there,
    /// and a store that cannot be opened says why on every key screen, offers
    /// a new store, and lets no key be typed meanwhile.
    #[test]
    fn connections_say_a_missing_or_unavailable_key_and_offer_a_new_store() {
        let mut app = app();
        app.providers["providers"] = json!([{
            "name": "mimo", "needs_key": true, "has_key": false, "keys": 0, "ready": 0,
            "models": [], "key_state": "missing",
            "key_note": "Key missing: Rune's credential store (secrets.vault) is not in its home, so `mimo` has no key"
        }]);
        app.providers["credentials"] = json!({ "state": "absent" });
        app.preferences.navigate(Page::Connections);
        let list = rows(&app);
        let mimo = list.iter().find(|r| r.label == "mimo").unwrap();
        assert!(mimo.value.contains("Key missing"), "{}", mimo.value);
        assert!(!list
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::NewCredentialStore)));
        app.preferences.navigate(Page::Provider("mimo".into()));
        let credential = |app: &App| {
            rows(app)
                .into_iter()
                .find(|r| r.label == "API CREDENTIALS")
                .unwrap()
        };
        assert_eq!(
            credential(&app).value,
            "Key missing · secrets.vault not found"
        );
        assert!(credential(&app).help.contains("secrets.vault"));
        app.providers["credentials"]["state"] = json!("ready");
        assert_eq!(
            credential(&app).value,
            "Key missing · not in the credential store"
        );

        let note = "Rune's credential store is there but its master.key is missing — restore master.key from a backup, or start a new credential store";
        app.providers["credentials"] =
            json!({ "state": "unavailable", "cause": "key_missing", "note": note });
        app.providers["providers"][0]["key_state"] = json!("unavailable");
        app.providers["providers"][0]["key_note"] = json!(format!("Key unavailable: {note}"));
        assert_eq!(credential(&app).value, "Unavailable · master.key missing");
        assert!(credential(&app).help.contains("master.key"));
        let add = rows(&app)
            .into_iter()
            .find(|r| r.kind == Kind::Edit(Field::Key("mimo".into())))
            .unwrap();
        open_editor(&mut app, Field::Key("mimo".into()), &add);
        assert!(
            app.preferences.editor.is_none(),
            "no key is typed into a store that cannot take it"
        );
        assert!(app
            .preferences
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("master.key"));

        app.preferences.navigate(Page::Connections);
        let list = rows(&app);
        assert_eq!(list[0].label, "CREDENTIALS");
        assert_eq!(list[0].value, "Unavailable · master.key missing");
        assert_eq!(list[1].kind, Kind::Edit(Field::NewCredentialStore));
        assert_eq!(list[1].help, note);
        assert!(Field::NewCredentialStore.confirms());
        let mimo = list.iter().find(|r| r.label == "mimo").unwrap();
        assert!(mimo.value.contains("Unavailable"), "{}", mimo.value);
    }

    #[test]
    fn sanitizer_has_one_generation_home_and_retired_count_is_absent() {
        let app = app();
        let rows = sampling_rows(&app);
        for key in [
            "sanitizer.enabled",
            "sanitizer.model",
            "sanitizer.timeout",
            "sanitizer.max_output_tokens",
        ] {
            assert_eq!(
                rows.iter()
                    .filter(|r| matches!(&r.kind,Kind::Edit(Field::Global(k)) if k==key))
                    .count(),
                1,
                "{key}"
            );
        }
        assert!(!rows.iter().any(
            |r| matches!(&r.kind,Kind::Edit(Field::Global(k)) if k=="classifier.max_user_messages")
        ));
    }

    #[test]
    fn runtime_offers_the_retry_curve() {
        let mut app = app();
        app.preferences.navigate(Page::Runtime);
        let rows = rows(&app);
        let header = rows
            .iter()
            .position(|row| matches!(row.kind, Kind::Fact) && row.label == "RETRY & RESILIENCE")
            .expect("a RETRY group on the Runtime page");
        let shown: Vec<(String, String)> = rows[header + 1..header + 5]
            .iter()
            .filter_map(|row| match &row.kind {
                Kind::Edit(Field::Global(key)) => Some((key.clone(), row.value.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("retry.max_attempts", "5"),
                ("retry.initial_delay", "3s"),
                ("retry.max_delay", "1m"),
                ("retry.credit_cooldown", "15m"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
    }

    #[test]
    fn web_fetch_has_its_own_tools_group_and_independent_editable_limits() {
        let mut app = app();
        app.preferences.navigate(Page::Tools);
        app.settings["configuration"] = json!({
            "web_search": {"max_output_chars": 1234, "timeout": "11s", "results": 3},
            "web_fetch": {"max_output_chars": 5678, "timeout": "22s"}
        });
        let rows = tools_rows(&app);
        let group = |heading: &str| {
            rows.iter()
                .position(|r| matches!(r.kind, Kind::Fact) && r.label == heading)
                .unwrap()
        };
        let search = group("WEB SEARCH (web_search)");
        let fetch = group("WEB FETCH (web_fetch)");
        let next = group("DISCOVER CAPABILITIES (discover_capabilities)");
        assert!(search < fetch && fetch < next);
        let fields = |start, end| {
            rows[start..end]
                .iter()
                .filter_map(|row| match &row.kind {
                    Kind::Edit(Field::Global(key)) => Some((key.as_str(), row.value.as_str())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            fields(search, fetch),
            [
                ("web_search.max_output_chars", "1234"),
                ("web_search.timeout", "11s"),
                ("web_search.results", "3"),
            ]
        );
        assert_eq!(
            fields(fetch, next),
            [
                ("web_fetch.max_output_chars", "5678"),
                ("web_fetch.timeout", "22s"),
            ]
        );
    }

    fn tavily() -> Value {
        json!({"name":"tavily","api":"tavily","title":"Tavily","base_url":"https://api.tavily.com",
            "fetch_url":null,"base_url_set":null,"fetch_url_set":null,
            "search":{"added":1,"route":null},"fetch":null,"zone":null,
            "keys":1,"ready":1,"keys_list":[{"position":1,"name":"Primary","preview":"tvly…1234"}]})
    }

    /// Owner, 2026-10-14: one list of connections, each once, with what it
    /// serves — a vendor that serves models and search is one row.
    #[test]
    fn connections_are_one_list_and_a_shared_vendor_is_one_row() {
        let mut app = app();
        app.providers["providers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"9router","needs_key":true,"keys":1,"ready":1,"models":[]}));
        let mut router = tavily();
        router["name"] = json!("9router");
        router["api"] = json!("9router");
        router["search"] = json!({"added":2,"route":"exa"});
        router["routes"] = json!({"search":["exa"],"fetch":[]});
        with_services(&mut app, json!([tavily(), router]));
        app.preferences.navigate(Page::Connections);
        let rows = rows(&app);
        assert_eq!(rows[0].label, "CONNECTIONS");
        assert!(!rows
            .iter()
            .any(|r| r.label == "WEB SEARCH" || r.label == "MODEL PROVIDERS"));
        let named: Vec<(&str, &str)> = rows
            .iter()
            .filter(|r| matches!(r.kind, Kind::Navigate(Page::Provider(_))))
            .map(|r| (r.label.as_str(), r.note.as_str()))
            .collect();
        assert_eq!(
            named,
            [
                ("9router", "Models · Search"),
                ("alpha", "Models (active)"),
                ("tavily", "Search"),
                ("zeta", "Models"),
            ]
        );
        assert!(rows.iter().any(|r| r.label == "Add connection"));
    }

    #[test]
    fn a_service_connection_has_only_account_access_and_removal() {
        let mut app = app();
        with_services(&mut app, json!([tavily()]));
        app.preferences.navigate(Page::Provider("tavily".into()));
        let page = rows(&app);
        assert!(page
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::ServiceUrl("tavily".into(), Service::Search))));
        assert!(page
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::RemoveKey("tavily".into(), 1))));
        assert!(page
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::RemoveService("tavily".into()))));
        for label in [
            "SERVICES",
            "Web search",
            "Web fetch",
            "QUICK ACTIONS",
            "CONFIGURATION",
        ] {
            assert!(!page.iter().any(|r| r.label == label), "{label}");
        }
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        assert!(rows(&app).iter().any(|r| r.label == "tavily"));
        app.preferences.navigate(Page::AddWebProvider {
            service: Service::Fetch,
        });
        assert!(rows(&app)
            .iter()
            .any(|r| r.kind == Kind::ServiceOn("tavily".into(), Service::Fetch)));
    }

    /// Label and value of every row of the page shown.
    fn rows_of(app: &App) -> Vec<(String, String)> {
        rows(app).into_iter().map(|r| (r.label, r.value)).collect()
    }

    #[test]
    fn a_model_connection_is_access_only_and_request_controls_have_other_homes() {
        let mut app = app();
        with_router(&mut app);
        app.preferences.navigate(Page::Provider("9router".into()));
        let page = rows(&app);
        assert!(page
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::BaseUrl("9router".into()))));
        for label in [
            "QUICK ACTIONS",
            "CONFIGURATION",
            "SERVICES",
            "Request timeout",
            "New-session fallback",
            "Browse models",
        ] {
            assert!(!page.iter().any(|r| r.label == label), "{label}");
        }
        app.preferences.navigate(Page::Runtime);
        assert!(rows(&app)
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::Timeout("9router".into()))));
        app.preferences.navigate(Page::Models);
        app.preferences.location.provider = "9router".into();
        assert!(rows(&app)
            .iter()
            .any(|r| r.kind == Kind::Default("9router".into())));
    }

    #[test]
    fn add_connection_offers_search_and_fetch_vendors_and_routes_a_known_one_to_its_page() {
        let mut app = app();
        let mut d = super::super::test_driver("add-connection");
        app.providers["presets"] = json!([{"name":"openai","key_hint":"sk-…","needs_key":true}]);
        app.providers["providers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"ollama","needs_key":false,"keys":0,"ready":0,"models":[]}));
        with_services(&mut app, json!([tavily()]));
        app.preferences.navigate(Page::AddProvider);
        let rows = rows(&app);
        let at = |label: &str| rows.iter().position(|r| r.label == label);
        assert!(at("MODEL PROVIDERS").unwrap() < at("SEARCH & FETCH").unwrap());
        assert!(at("tavily").is_none(), "already connected");
        let exa = &rows[at("exa").unwrap()];
        assert_eq!(exa.kind, Kind::AddService("exa".into()));
        assert_eq!(exa.note, "Exa · Search · Fetch");
        assert_eq!(rows[at("searxng").unwrap()].value, "Your own server");
        let ollama = &rows[at("ollama").unwrap()];
        assert_eq!(
            (ollama.kind.clone(), ollama.value.as_str()),
            (Kind::AddService("ollama".into()), "Already connected")
        );
        assert!(at("serpdive").is_none(), "owner, 2026-10-14: left out");

        // Photo 1 (owner, 2026-10-15): every vendor row is one table — the
        // name, what it needs, what it is — whatever the row says.
        let shown = ansi::plain(&render(&app, 140, 60).0.lines.join("\n"));
        let column = |name: &str, text: &str| {
            let line = shown
                .lines()
                .find(|l| {
                    l.trim_start_matches(['❯', ' '])
                        .starts_with(&format!("{name} "))
                })
                .unwrap_or_else(|| panic!("{name}: {shown}"));
            ansi::width(&line[..line.find(text).unwrap_or_else(|| panic!("{text}: {line}"))])
        };
        let note = column("exa", "Exa · Search");
        for (name, what) in [
            ("ollama", "Ollama Web Search"),
            ("searxng", "SearXNG · Search"),
            ("jina", "Jina AI"),
            ("brave", "Brave Search"),
        ] {
            assert_eq!(column(name, what), note, "{name}");
        }
        assert_eq!(
            column("ollama", "Already connected"),
            column("exa", "API key required")
        );

        // One connection, one key ring: a vendor connected for models gains
        // its services on that connection's page.
        open(&mut app, ollama, &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::Provider("ollama".into())
        );
        d.engine.shutdown();
    }

    #[test]
    fn the_provider_rows_say_who_serves_pinned_or_by_default() {
        let mut app = app();
        let value = |app: &App, svc| web_provider_row(app, svc).value;
        assert_eq!(value(&app, Service::Search), "First added · none yet");
        app.settings["web"] = json!({"search":"exa","fetch":"built-in reader"});
        assert_eq!(value(&app, Service::Search), "First added · exa");
        app.settings["configuration"]["web_search"]["provider"] = json!("tavily");
        app.settings["web"]["search"] = json!("unavailable — web_search.provider names tavily");
        assert_eq!(value(&app, Service::Search), "tavily (unavailable)");
        app.settings["configuration"]["web_fetch"]["provider"] = json!("jina");
        app.settings["web"]["fetch"] = json!("jina");
        assert_eq!(value(&app, Service::Fetch), "jina");
    }

    /// The 9router connection of these tests: search through `ag`, `glm` kept
    /// beside it, fetch off with `exa` kept.
    fn router() -> Value {
        json!({"name":"9router","api":"9router","title":"9router",
            "base_url":"http://localhost:20128/v1","fetch_url":null,
            "base_url_set":null,"fetch_url_set":null,
            "search":{"added":2,"route":"ag"},"fetch":null,
            "routes":{"search":["ag","glm"],"fetch":["exa"]},"zone":null,
            "keys":1,"ready":1,"keys_list":[]})
    }

    fn with_router(app: &mut App) {
        app.providers["providers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"9router","needs_key":true,"keys":1,"ready":1,"models":[]}));
        with_services(app, json!([router()]));
        app.settings["web"] = json!({"search":"9router (ag)","fetch":"built-in reader"});
    }

    /// The message a sent request will show once the engine says it worked.
    fn sent(app: &App) -> Vec<String> {
        app.awaiting
            .values()
            .filter_map(|r| match r {
                Awaiting::Preferences(Request {
                    operation: Operation::Service(message),
                    ..
                }) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Owner, 2026-10-15: a route added is kept, to switch back to; the one in
    /// use is marked On in the list itself — no separate status row.
    #[test]
    fn tools_lists_exact_gateway_providers_and_marks_the_selection() {
        let mut app = app();
        with_router(&mut app);
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        assert_eq!(app.preferences.location.page.tab(), 5);
        assert_eq!(app.preferences.location.page.parent(), Some(Page::Tools));
        let page = rows_of(&app);
        assert!(page.contains(&("9router:ag".into(), "In use · default".into())));
        assert!(page.contains(&("9router:glm".into(), "Use ↵ · Del removes".into())));
        assert!(page.iter().any(|(label, _)| label == "Add provider"));
        app.preferences.navigate(Page::WebProviders {
            service: Service::Fetch,
        });
        assert!(rows_of(&app).contains(&("9router:exa".into(), "Use ↵ · Del removes".into())));
        assert!(rows_of(&app).contains(&(pin_default(Service::Fetch).into(), "Chosen".into())));
    }

    #[test]
    fn choosing_a_kept_route_sends_an_exact_checked_selection() {
        let mut app = app();
        let mut d = super::super::test_driver("route-use");
        with_router(&mut app);
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        let row = rows(&app)
            .into_iter()
            .find(|r| r.label == "9router:glm")
            .unwrap();
        open(&mut app, &row, &mut d);
        assert_eq!(sent(&app), ["Web search: 9router:glm chosen."]);
        assert_eq!(
            app.settings["web"]["search"], "9router (ag)",
            "not optimistic before ACK"
        );
        d.engine.shutdown();
    }

    #[test]
    fn delete_takes_a_kept_route_off_the_list_but_never_the_one_in_use() {
        let mut app = app();
        let mut d = super::super::test_driver("route-forget");
        with_router(&mut app);
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        let at = |app: &App, label: &str| rows(app).iter().position(|r| r.label == label).unwrap();
        app.preferences.location.row = at(&app, "9router:ag");
        assert!(hint(&app).contains("Del remove route"));
        app.on_key(Key::Delete, &mut d);
        assert!(sent(&app).is_empty(), "the route in use stays");
        app.preferences.location.row = at(&app, "9router:glm");
        app.on_key(Key::Delete, &mut d);
        assert_eq!(sent(&app), ["glm removed from 9router's search routes."]);
        d.engine.shutdown();
    }

    /// Adding a route offers what the gateway lists that is not kept yet, and
    /// a way to type one it serves unlisted (9router 0.5.99 lists no chat
    /// model's search, `ag` among them).
    #[test]
    fn adding_a_route_in_tools_is_empty_without_catalog_or_examples_and_only_keeps() {
        let mut app = app();
        let mut d = super::super::test_driver("service-route");
        with_router(&mut app);
        app.preferences.offered = json!({"routes":{"search":["exa","glm"]}});
        app.preferences.navigate(Page::AddWebProvider {
            service: Service::Search,
        });
        let row = rows(&app)
            .into_iter()
            .find(|r| r.label == "9router")
            .unwrap();
        open(&mut app, &row, &mut d);
        assert!(
            app.awaiting.is_empty(),
            "opening Add sends no catalog request"
        );
        assert_eq!(
            app.preferences.location.page,
            Page::WebProviders {
                service: Service::Search
            }
        );
        let editor = app.preferences.editor.as_ref().unwrap();
        assert_eq!(editor.title, "Add a search route");
        assert!(editor.text.is_empty() && editor.choices.is_empty());
        let shown = ansi::plain(&render(&app, 120, 40).0.lines.join("\n"));
        assert!(
            !shown.contains("exa") && !shown.contains("glm") && !shown.contains("e.g."),
            "{shown}"
        );
        app.preferences.editor.as_mut().unwrap().text = "exa".into();
        save_editor(&mut app, &mut d);
        assert_eq!(
            sent(&app),
            ["9router: exa checked and kept. Choose it in the list to use it for search."]
        );
        d.engine.shutdown();
    }

    /// A check says which route it went through, passed or failed alike.
    #[test]
    fn a_selection_ack_updates_the_exact_pin_without_reopening_a_superseded_page() {
        let mut app = app();
        with_router(&mut app);
        let page = Page::WebProviders {
            service: Service::Search,
        };
        let session = app.session;
        app.preferences.navigate(Page::Connections);
        ack(
            &mut app,
            Request {
                token: 1,
                session,
                origin: page,
                editor: None,
                operation: Operation::Service("Web search: 9router:ag chosen.".into()),
                before: None,
            },
            true,
            None,
            Some(json!({"web_pins":{"search":"9router:ag","fetch":null}})),
        );
        assert_eq!(
            app.settings["configuration"]["web_search"]["provider"],
            "9router:ag"
        );
        assert_eq!(app.preferences.location.page, Page::Connections);
    }

    /// Switching a service off asks first and opens on keeping it on; the
    /// removal of a connection does the same (it opened on Remove, labelled
    /// "Remove key", until 2026-10-15).
    #[test]
    fn removing_an_account_asks_first_and_opens_on_keeping_it() {
        let mut app = app();
        let mut d = super::super::test_driver("service-off");
        with_services(&mut app, json!([tavily()]));
        app.preferences.navigate(Page::Provider("tavily".into()));
        let remove = rows(&app)
            .into_iter()
            .find(|r| r.label == "Remove connection")
            .unwrap();
        open(&mut app, &remove, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().button, 1);
        let shown = ansi::plain(&render(&app, 120, 40).0.lines.join("\n"));
        assert!(
            shown.contains("Remove connection")
                && shown.contains("Keep it")
                && !shown.contains("Remove key"),
            "{shown}"
        );
        d.engine.shutdown();
    }

    #[test]
    fn removing_the_last_tool_provider_keeps_its_account() {
        let mut app = app();
        let mut d = super::super::test_driver("service-last");
        with_services(&mut app, json!([tavily()]));
        app.settings["web"]["search"] = json!("other");
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        app.preferences.location.row = rows(&app).iter().position(|r| r.label == "tavily").unwrap();
        app.on_key(Key::Delete, &mut d);
        assert_eq!(
            sent(&app),
            ["tavily removed from this tool. Its account stays."]
        );
        assert!(service_of(&app, "tavily").is_some());
        d.engine.shutdown();
    }

    #[test]
    fn a_service_setup_asks_for_what_is_missing_before_it_checks_anything() {
        let mut app = app();
        let mut d = super::super::test_driver("service-setup");
        app.preferences.navigate(Page::AddProvider);
        let exa = rows(&app).into_iter().find(|r| r.label == "exa").unwrap();
        open(&mut app, &exa, &mut d);
        assert_eq!(app.preferences.location.page, Page::Setup("exa".into()));
        assert_eq!(
            app.preferences.editor.as_ref().map(|e| e.field.clone()),
            Some(Field::SetupKeyName),
            "a keyed vendor starts with its key"
        );
        app.preferences.editor = None;
        let labels: Vec<String> = rows(&app).into_iter().map(|r| r.label).collect();
        assert_eq!(labels, ["Base URL", "Key name", "API key", "Save account"]);
        // Connect with no key goes back to the key, sends nothing.
        connect_service(&mut app, &mut d);
        assert_eq!(app.preferences.pending, None);
        assert_eq!(
            app.preferences.editor.as_ref().map(|e| e.field.clone()),
            Some(Field::SetupKey)
        );
        // SearXNG starts with the address of the person's own server.
        app.preferences.editor = None;
        app.preferences.navigate(Page::AddProvider);
        let searxng = rows(&app)
            .into_iter()
            .find(|r| r.label == "searxng")
            .unwrap();
        open(&mut app, &searxng, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().map(|e| e.field.clone()),
            Some(Field::SetupUrl)
        );
        app.preferences.editor = None;
        assert!(
            !rows(&app).iter().any(|r| r.label == "API key"),
            "SearXNG takes no key"
        );
        d.engine.shutdown();
    }

    /// Unpinned is a choice of its own in Tools › Web search/fetch › Provider —
    /// the first connection is never shown chosen while nothing is pinned.
    #[test]
    fn a_web_tools_provider_has_an_explicit_default_and_no_connection_editor() {
        let mut app = app();
        let mut fetching = tavily();
        fetching["fetch"] = json!({"added":3,"route":null});
        with_services(&mut app, json!([fetching]));
        app.preferences.navigate(Page::Tools);
        assert_eq!(
            web_provider_row(&app, Service::Fetch).kind,
            Kind::Navigate(Page::WebProviders {
                service: Service::Fetch
            })
        );
        app.preferences.navigate(Page::WebProviders {
            service: Service::Fetch,
        });
        assert!(rows_of(&app).contains(&(pin_default(Service::Fetch).into(), "Chosen".into())));
        assert!(rows_of(&app).contains(&("tavily".into(), "Use ↵ · Del removes".into())));
        app.settings["configuration"]["web_fetch"]["provider"] = json!("tavily");
        assert!(rows_of(&app).contains(&("tavily".into(), "Chosen".into())));
    }

    /// The SERP zones are the account's, asked when the row opens.
    #[test]
    fn the_serp_zone_is_chosen_from_what_the_account_lists_now() {
        let mut app = app();
        let mut d = super::super::test_driver("service-zone");
        let mut bright = tavily();
        bright["name"] = json!("brightdata");
        bright["api"] = json!("brightdata");
        bright["zone"] = json!("serp_api1");
        with_services(&mut app, json!([bright]));
        app.preferences.navigate(Page::WebProviders {
            service: Service::Search,
        });
        let zone = rows(&app)
            .into_iter()
            .find(|r| r.label == "brightdata SERP zone")
            .unwrap();
        assert_eq!(zone.value, "serp_api1");
        open(&mut app, &zone, &mut d);
        assert!(
            app.preferences.editor.is_none(),
            "nothing offered before the account answers"
        );
        assert!(app.awaiting.values().any(|a| matches!(
            a,
            Awaiting::Preferences(Request {
                operation: Operation::Options(Field::ServiceZone(_)),
                ..
            })
        )));
        d.engine.shutdown();
    }

    /// A connection removed keeps its keys ("Its keys stay in the vault"), so
    /// setting it up again neither demands nor replaces them unless a new one
    /// is given.
    #[test]
    fn setting_up_a_vendor_whose_keys_stayed_in_the_vault_needs_no_key() {
        let mut app = app();
        let mut d = super::super::test_driver("service-setup-kept-key");
        for a in app.providers["adapters"].as_array_mut().unwrap() {
            if a["id"] == "exa" {
                a["keys"] = json!(1);
            }
        }
        app.preferences.navigate(Page::AddProvider);
        let exa = rows(&app).into_iter().find(|r| r.label == "exa").unwrap();
        open(&mut app, &exa, &mut d);
        assert_eq!(app.preferences.location.page, Page::Setup("exa".into()));
        assert!(app.preferences.editor.is_none(), "no key asked for");
        let key = rows(&app)
            .into_iter()
            .find(|r| r.label == "API key")
            .unwrap();
        assert_eq!(key.value, "[ 1 key in the vault ↵ ]");
        assert!(
            key.help.contains("used unless you give a new one"),
            "{}",
            key.help
        );
        connect_service(&mut app, &mut d);
        assert!(
            app.preferences.pending.is_some(),
            "checked and saved with the key the vault holds"
        );
        d.engine.shutdown();
    }

    /// Owner, 2026-10-11: the background log's disk limit sits with the
    /// terminal's, and the walk both searches share has a group of its own —
    /// each row its default and the help that says what it does; the five
    /// retired limits are nowhere.
    #[test]
    fn the_scan_limits_and_the_background_log_are_on_the_tools_page() {
        let mut app = app();
        app.preferences.navigate(Page::Tools);
        let rows = tools_rows(&app);
        let group = |heading: &str| {
            rows.iter()
                .position(|r| matches!(r.kind, Kind::Fact) && r.label == heading)
                .unwrap_or_else(|| panic!("no {heading} group"))
        };
        let row = |key: &str| {
            rows.iter()
                .position(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == key))
                .unwrap_or_else(|| panic!("no {key} row"))
        };
        let terminal = group("TERMINAL (terminal_use)");
        let inspect = group("INSPECT FILES (inspect_file)");
        let log = row("terminal.max_background_log_size");
        assert!(terminal < log && log < inspect);
        assert_eq!(rows[log].value, "100MiB");
        let scan = group("FILE SCAN (find_files, search_text)");
        assert!(group("SEARCH TEXT (search_text)") < scan);
        assert!(scan < group("MANAGE PROCESS (manage_process)"));
        for (key, value) in [
            ("scan.max_entries", "200000"),
            ("scan.max_depth", "32"),
            ("scan.regex_steps", "200000"),
        ] {
            let at = row(key);
            assert!(scan < at, "{key} sits in FILE SCAN");
            assert_eq!(rows[at].value, value, "{key}");
        }
        assert!(rows[row("scan.regex_steps")].help.contains("Safety fuse"));
        for page in [
            Page::Tools,
            Page::Generation,
            Page::Appearance,
            Page::Runtime,
            Page::Connections,
        ] {
            app.preferences.navigate(page.clone());
            for row in super::rows(&app) {
                if let Kind::Edit(Field::Global(key)) = &row.kind {
                    for retired in [
                        "tools.max_parallel",
                        "scan.respect_ignore",
                        "scan.hidden",
                        "checkpoint.keep_turns",
                        "attachments.snapshot_max_size",
                    ] {
                        assert_ne!(key, retired, "{page:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn tools_truncation_and_unlimited_are_visible_and_footer_is_isolated() {
        let mut app = app();
        app.preferences.navigate(Page::Tools);
        let rows = tools_rows(&app);
        for key in [
            "terminal.truncate_head_bytes",
            "terminal.truncate_tail_bytes",
            "manage_process.truncate_head_bytes",
            "manage_process.truncate_tail_bytes",
        ] {
            assert!(rows
                .iter()
                .any(|row| matches!(&row.kind, Kind::Edit(Field::Global(k)) if k == key)));
        }
        assert!(!rows.iter().any(|row| matches!(&row.kind, Kind::Edit(Field::Global(k)) if k.ends_with(".persisted_output"))));
        assert_eq!(rows.iter().find(|row| matches!(&row.kind, Kind::Edit(Field::Global(k)) if k == "questions.timeout")).unwrap().value, "UNLIMITED");
        for width in [40, 80, 160] {
            for height in [18, 30, 50] {
                let before = render(&app, width, height).0;
                notice(&mut app, &"Long error detail ".repeat(30), Tone::Error);
                let after = render(&app, width, height).0;
                let rule = |sink: &Sink| {
                    sink.lines.iter().rposition(|line| {
                        ansi::plain(line).contains("More below") || {
                            let plain = ansi::plain(line);
                            !plain.trim().is_empty() && plain.trim().chars().all(|c| c == '─')
                        }
                    })
                };
                assert_eq!(
                    rule(&before),
                    rule(&after),
                    "footer must not move with notice"
                );
                let error_row = after
                    .lines
                    .iter()
                    .position(|line| ansi::plain(line).contains("Error:"))
                    .unwrap();
                assert!(error_row > rule(&after).unwrap());
                assert!(after.lines.iter().all(|line| ansi::width(line) <= width));
                app.preferences.notice = None;
            }
        }
        notice(&mut app, "Working", Tone::Busy);
        app.preferences.notice.as_mut().unwrap().created -= std::time::Duration::from_secs(60);
        assert!(!app.preferences.expire_notice(Instant::now()));
        notice(&mut app, "Saved", Tone::Good);
        app.preferences.notice.as_mut().unwrap().created -= std::time::Duration::from_secs(60);
        assert!(app.preferences.expire_notice(Instant::now()));
    }

    fn app() -> App {
        let mut app = App::for_test_with(json!({
            "settings":{"provider":"alpha","model":"z-model","thinking":true},
            "models":[{"id":"z-model","thinking":true},{"id":"a-model"}],
            "knobs":[{"key":"thinking","label":"Thinking","kind":"bool","help":"Reason first"},
                {"key":"stop","label":"Stop sequences","kind":"text","help":"Up to four strings"},
                {"key":"temperature","label":"Temperature","kind":"float","help":"Randomness"}],
            "suppressed":[{"key":"temperature","reason":"Turn thinking off first","permanent":false}]
        }));
        app.providers = json!({"providers":[
            {"name":"zeta","needs_key":true,"keys":1,"ready":1,"models":[{"id":"zeta-b"},{"id":"zeta-a"}]},
            {"name":"alpha","needs_key":false,"keys":0,"ready":0,"models":[{"id":"z-model"},{"id":"a-model"}]}
        ]});
        with_services(&mut app, json!([]));
        app.panel = Panel::Settings;
        app
    }

    #[test]
    fn canonical_editing_matrix_covers_settings_secret_and_model_search_without_aliases() {
        let mut driver = super::super::test_driver("settings-editing-matrix");
        for secret in [false, true] {
            for case in super::super::editing::cases() {
                let mut app = app();
                app.preferences.editor = Some(Editor {
                    id: 1,
                    field: if secret {
                        Field::SetupKey
                    } else {
                        Field::Knob("stop".into())
                    },
                    title: "Edit".into(),
                    help: String::new(),
                    text: case.text.into(),
                    cursor: case.at,
                    editing: super::super::editing::State::default(),
                    choices: Vec::new(),
                    clearable: false,
                    button: 0,
                    confirm_apply_at: None,
                });
                for key in case.keys {
                    app.on_key(key, &mut driver);
                }
                let editor = app.preferences.editor.as_ref().unwrap();
                assert_eq!(editor.text, case.expected, "{} secret={secret}", case.name);
                let (sink, _) = render(&app, 100, 40);
                if secret && case.expected.len() > 2 {
                    assert!(
                        !sink.lines.join("\n").contains(case.expected),
                        "secret rendered"
                    );
                }
                app.preferences.back();
                assert!(app.preferences.editor.is_none());
            }
        }
        for case in super::super::editing::cases() {
            let mut app = app();
            app.preferences.navigate(Page::Models);
            app.preferences.tabs = false;
            let loc = &mut app.preferences.location;
            loc.focus = ModelFocus::Search;
            loc.query = case.text.into();
            loc.cursor = case.at;
            for key in case.keys {
                app.on_key(key, &mut driver);
            }
            assert_eq!(
                app.preferences.location.query, case.expected,
                "search {}",
                case.name
            );
        }
        driver.engine.shutdown();
    }

    /// The engine's own vendor catalogue and status, with `services` listed.
    fn with_services(app: &mut App, services: Value) {
        let web = runeengine::settings::connections::list(&runeengine::settings::Settings::empty());
        app.providers["adapters"] = web["adapters"].clone();
        app.providers["web"] = web["web"].clone();
        app.providers["services"] = services;
    }

    /// A route that stops thinking only between tool calls says so where the
    /// person switches it off (measured 2026-10-14: Antigravity's Claude Sonnet
    /// 5.5 still thinks before answering with `reasoning_effort: none`).
    #[test]
    fn an_off_that_only_stops_thinking_between_tools_is_said_where_it_is_chosen() {
        let mut app = app();
        let row_note = |app: &App| {
            sampling_rows(app)
                .into_iter()
                .find(|row| row.label == "Thinking")
                .map(|row| (row.note, row.help))
                .unwrap()
        };
        let (note, help) = row_note(&app);
        assert!(!note.contains("between tools") && !help.contains("between tool"));
        // Where the engine's provider list carries each model's catalogue row.
        app.providers["providers"][1]["models"][0]["metadata"] =
            json!({"capabilities":{"reasoning":true,"thinkingOffType":"between_tools"}});
        let (note, help) = row_note(&app);
        assert!(note.contains("Off: between tools only"), "{note}");
        assert!(help.contains("still thinks before it answers"), "{help}");
    }

    /// Owner, 2026-10-15: Off is a choice on every 9router route, and on one
    /// the router cannot stop thinking (`thinkingCanDisable: false`) the
    /// screen says Off asks for its least, where Off is chosen.
    #[test]
    fn an_off_that_asks_a_route_for_its_least_is_said_where_it_is_chosen() {
        let mut app = app();
        let row_note = |app: &App| {
            sampling_rows(app)
                .into_iter()
                .find(|row| row.label == "Thinking")
                .map(|row| (row.note, row.help))
                .unwrap()
        };
        let (note, help) = row_note(&app);
        assert!(!note.contains("least") && !help.contains("least"));
        app.providers["providers"][1]["models"][0]["metadata"] =
            json!({"capabilities":{"reasoning":true,"thinkingCanDisable":false}});
        let (note, help) = row_note(&app);
        assert!(note.contains("Off: least thinking"), "{note}");
        assert!(help.contains("cannot stop thinking"), "{help}");
        // The more exact note wins where the router publishes both.
        app.providers["providers"][1]["models"][0]["metadata"] = json!({"capabilities":{
            "reasoning":true,"thinkingCanDisable":false,"thinkingOffType":"between_tools"}});
        let (note, _) = row_note(&app);
        assert!(note.contains("between tools only"), "{note}");
    }

    /// A model added to or removed from the registry must show in the picker at
    /// once for the provider the user is working in, which reads the session's
    /// `models` snapshot rather than the registry. Before this, an add or a delete
    /// on the active provider stayed invisible until the next launch.
    #[test]
    fn a_registry_change_reaches_the_active_provider_snapshot_without_a_restart() {
        let mut app = app(); // active provider is "alpha", snapshot has 2 models
        assert_eq!(app.settings["models"].as_array().unwrap().len(), 2);

        // The engine answers a delete with the registry as it now stands.
        app.providers = json!({"providers":[
            {"name":"zeta","needs_key":true,"keys":1,"ready":1,"models":[{"id":"zeta-b"},{"id":"zeta-a"}]},
            {"name":"alpha","needs_key":false,"keys":0,"ready":0,"models":[{"id":"a-model"}]}
        ]});
        app.sync_active_models();
        let ids: Vec<&str> = app.settings["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["a-model"],
            "the deleted model must be gone from the active snapshot at once"
        );

        // And an add reaches it the same way.
        app.providers = json!({"providers":[
            {"name":"zeta","needs_key":true,"keys":1,"ready":1,"models":[{"id":"zeta-b"}]},
            {"name":"alpha","needs_key":false,"keys":0,"ready":0,"models":[{"id":"a-model"},{"id":"new-model"}]}
        ]});
        app.sync_active_models();
        let ids: Vec<&str> = app.settings["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert!(
            ids.contains(&"new-model"),
            "an added model must appear without a restart"
        );
    }

    #[test]
    fn ack_reloads_runtime_configuration_live() {
        let mut app = app();
        let request = Request {
            token: 1234,
            session: app.session,
            origin: Page::Tools,
            editor: None,
            operation: Operation::Setting(Some("config.tools.max_actions_per_response".into())),
            before: None,
        };
        ack(
            &mut app,
            request,
            true,
            None,
            Some(json!({"settings": {"model": "m", "provider": "p"}})),
        );
        assert_eq!(app.preferences.pending, None);
        app.preferences.serial = 2000;
        app.preferences.pending = Some(2000);
        notice(&mut app, "Newest operation", Tone::Busy);
        ack(
            &mut app,
            Request {
                token: 1999,
                session: Some(1),
                origin: Page::Tools,
                editor: None,
                operation: Operation::Setting(Some("permissions.timeout".into())),
                before: None,
            },
            false,
            Some("Old failure".into()),
            None,
        );
        assert_eq!(app.preferences.pending, Some(2000));
        assert_eq!(
            app.preferences.notice.as_ref().unwrap().text,
            "Newest operation"
        );
    }

    #[test]
    fn escape_returns_one_level_and_restores_the_parent_selection() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-back");
        app.preferences.location.row = 2;
        app.preferences.navigate(Page::Connections);
        app.preferences.location.row = 2;
        app.preferences.navigate(Page::Provider("zeta".into()));
        let row = rows(&app)
            .into_iter()
            .find(|r| matches!(r.kind, Kind::Edit(Field::Key(_))))
            .unwrap();
        open(&mut app, &row, &mut d);
        for expected in [
            Page::Provider("zeta".into()),
            Page::Connections,
            Page::Overview,
        ] {
            app.on_key(Key::Esc, &mut d);
            assert_eq!(app.preferences.location.page, expected);
            assert_eq!(app.panel, Panel::Settings);
        }
        assert_eq!(app.preferences.location.row, 2);
        app.on_key(Key::Esc, &mut d);
        assert_eq!(app.panel, Panel::None);
        d.engine.shutdown();
    }

    #[test]
    fn every_nested_route_has_its_actual_parent_even_when_opened_directly() {
        for (page, parent) in [
            (Page::Provider("openai".into()), Page::Connections),
            (Page::Setup("openai".into()), Page::AddProvider),
            (Page::Themes, Page::Appearance),
        ] {
            let mut state = State::default();
            state.navigate(page);
            assert!(state.back());
            assert_eq!(state.location.page, parent);
        }
    }

    #[test]
    fn models_are_sorted_inside_contiguous_provider_groups_even_after_search() {
        let mut app = app();
        app.preferences.navigate(Page::Models);
        let providers = provider_list(&app)
            .into_iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(providers, ["alpha", "zeta"]);
        for (provider, ids) in [
            ("alpha", vec!["a-model", "z-model"]),
            ("zeta", vec!["zeta-a", "zeta-b"]),
        ] {
            app.preferences.location.provider = provider.into();
            let pairs = rows(&app)
                .into_iter()
                .filter_map(|r| match r.kind {
                    Kind::Model { provider, id } => Some((provider, id)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                pairs,
                ids.into_iter()
                    .map(|id| (provider.into(), id.into()))
                    .collect::<Vec<_>>()
            );
        }
        app.preferences.location.query = "zeta-b".into();
        let rs = rows(&app);
        assert_eq!(
            rs.iter()
                .filter(|r| matches!(r.kind, Kind::Model { .. }))
                .count(),
            1
        );
        assert!(rs
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Model { provider, .. } => Some(provider),
                _ => None,
            })
            .all(|p| p == "zeta"));
    }

    #[test]
    fn all_providers_option_groups_models_by_provider_and_updates_header_dynamically() {
        let mut app = app();
        app.preferences.navigate(Page::Models);
        app.preferences.location.provider = "all".into();
        let rs = rows(&app);
        let facts: Vec<&str> = rs
            .iter()
            .filter(|r| matches!(r.kind, Kind::Fact))
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(facts, ["alpha", "zeta"]);

        let models: Vec<(&str, &str)> = rs
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Model { provider, id } => Some((provider.as_str(), id.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            models,
            [
                ("alpha", "a-model"),
                ("alpha", "z-model"),
                ("zeta", "zeta-a"),
                ("zeta", "zeta-b")
            ]
        );

        // Global search filters across all providers
        app.preferences.location.query = "zeta-a".into();
        let rs_search = rows(&app);
        let found: Vec<&str> = rs_search
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Model { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(found, ["zeta-a"]);
        assert!(rs_search.iter().any(|r| r.label == "zeta"));
        assert!(!rs_search.iter().any(|r| r.label == "alpha"));

        // Render check: No underline (\x1b[4m) on tabs, active session sub-title present, no duplicate box
        let (sink, _) = render(&app, 100, 32);
        let full_rendered = sink.lines.join("\n");
        assert!(!full_rendered.contains("\x1b[4m"));
        assert!(full_rendered.contains("Active session"));
        assert!(!full_rendered.contains("ACTIVE MODEL / THIS SESSION"));

        // Section headings (e.g. in Connections) do not have "─" trailing rule
        app.preferences.navigate(Page::Connections);
        let (conn_sink, _) = render(&app, 100, 32);
        for line in &conn_sink.lines {
            if line.contains("CONNECTIONS") {
                assert!(!line.contains('─'));
            }
        }
        let header = conn_sink
            .lines
            .iter()
            .find(|l| l.contains("Provider") && l.contains("Keys"))
            .expect("header row");
        let plain_header = ansi::plain(header);
        assert!(plain_header.contains("Keys"));
        let prov_row = conn_sink
            .lines
            .iter()
            .find(|l| l.contains("alpha"))
            .expect("provider row");
        let plain_row = ansi::plain(prov_row);
        // Keys count is centered under "Keys"
        assert!(plain_row.contains(" 1  ") || plain_row.contains(" 0  "));
    }

    #[test]
    fn print_connections_visual() {
        let mut app = app();
        app.preferences.navigate(Page::Connections);
        let (sink, _) = render(&app, 120, 30);
        println!("\n=== VISUAL CONNECTIONS RENDER ===");
        for line in &sink.lines {
            let plain = ansi::plain(line);
            if plain.contains("Provider")
                || plain.contains("alpha")
                || plain.contains("zeta")
                || plain.contains("Add connection")
            {
                println!("{}", plain);
            }
        }
        println!("=== END VISUAL RENDER ===\n");
    }

    #[test]
    fn settings_never_consumes_or_modifies_the_conversation_draft_or_mode() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-draft");
        app.input = "keep my conversation draft".into();
        app.cursor = 5;
        for key in [
            Key::Char('x'),
            Key::Paste("file.txt".into()),
            Key::Backspace,
            Key::Delete,
            Key::Tab,
            Key::Right,
            Key::Tab,
        ] {
            app.on_key(key, &mut d);
        }
        assert_eq!(app.input, "keep my conversation draft");
        assert_eq!(app.cursor, 5);
        assert_eq!(app.mode, "ask");
        d.engine.shutdown();
    }

    #[test]
    fn all_pages_fit_small_and_large_terminals_with_a_matching_hit_map() {
        let mut app = app();
        with_router(&mut app);
        for page in [
            Page::Overview,
            Page::Models,
            Page::Generation,
            Page::Connections,
            Page::Provider("zeta".into()),
            Page::Provider("tavily".into()),
            Page::AddProvider,
            Page::WebProviders {
                service: Service::Search,
            },
            Page::WebProviders {
                service: Service::Fetch,
            },
            Page::AddWebProvider {
                service: Service::Search,
            },
            Page::AddWebProvider {
                service: Service::Fetch,
            },
            Page::Runtime,
            Page::Appearance,
            Page::Themes,
            Page::Tools,
        ] {
            app.preferences.navigate(page);
            for width in [20, 40, 72, 100, 140] {
                for height in [8, 12, 24, 40] {
                    let (sink, cursor) = render(&app, width, height);
                    assert_eq!(sink.lines.len(), height);
                    assert_eq!(sink.owner.len(), height);
                    assert!(sink.lines.iter().all(|l| ansi::width(l) <= width));
                    assert!(cursor.is_none_or(|(r, c)| r <= height && c <= width));
                }
            }
        }
    }

    #[test]
    fn editor_masks_credentials_and_is_cancelled_before_its_page() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-secret");
        app.preferences.navigate(Page::Provider("zeta".into()));
        let row = rows(&app)
            .into_iter()
            .find(|r| matches!(r.kind, Kind::Edit(Field::Key(_))))
            .unwrap();
        open(&mut app, &row, &mut d);
        // Step 1: Input humanist key name
        app.on_key(Key::Paste("Work Key".into()), &mut d);
        app.on_key(Key::Enter, &mut d);
        // Step 2: Input secret key value
        app.on_key(Key::Paste("sk-test-secret-never-render".into()), &mut d);
        for (width, height) in [(20, 8), (40, 12), (100, 32)] {
            let (sink, cursor) = render(&app, width, height);
            let text = ansi::plain(&sink.lines.join("\n"));
            assert!(!text.contains("sk-test-secret"));
            assert!(text.contains('•'));
            assert!(cursor.is_some());
        }
        app.on_key(Key::Esc, &mut d);
        assert!(app.preferences.editor.is_none());
        assert_eq!(app.preferences.location.page, Page::Provider("zeta".into()));
        app.preferences.navigate(Page::Generation);
        let row = rows(&app)
            .into_iter()
            .find(|r| r.kind == Kind::Edit(Field::Knob("stop".into())))
            .unwrap();
        open(&mut app, &row, &mut d);
        app.on_key(Key::Ctrl('y'), &mut d);
        app.on_key(Key::Ctrl('_'), &mut d);
        assert!(
            app.preferences.editor.as_ref().unwrap().text.is_empty(),
            "secret editor history crossed a field boundary"
        );
        d.engine.shutdown();
    }

    #[test]
    fn failed_save_retains_the_field_and_late_ack_cannot_close_a_new_editor() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-ack");
        app.preferences.navigate(Page::Generation);
        let row = rows(&app)
            .into_iter()
            .find(|r| r.kind == Kind::Edit(Field::Knob("stop".into())))
            .unwrap();
        open(&mut app, &row, &mut d);
        let e = app.preferences.editor.as_mut().unwrap();
        e.text = "a,b,c,d,e".into();
        e.cursor = e.text.len();
        let id = e.id;
        let request = Request {
            token: 100,
            session: app.session,
            origin: Page::Generation,
            editor: Some(id),
            operation: Operation::Setting(None),
            before: None,
        };
        ack(
            &mut app,
            request.clone(),
            false,
            Some("invalid: at most four stop sequences".into()),
            None,
        );
        assert_eq!(app.preferences.editor.as_ref().unwrap().text, "a,b,c,d,e");
        assert_eq!(
            app.preferences.notice.as_ref().unwrap().tone as u8,
            Tone::Error as u8
        );
        app.preferences.back();
        open(&mut app, &row, &mut d);
        let new = app.preferences.editor.as_ref().unwrap().id;
        let data = app.settings.clone();
        ack(&mut app, request, true, None, Some(data));
        assert_eq!(app.preferences.editor.as_ref().unwrap().id, new);
        d.engine.shutdown();
    }

    #[test]
    fn key_removal_defaults_to_keep_and_requires_an_explicit_confirmation() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-remove");
        app.preferences.navigate(Page::Provider("zeta".into()));
        let row = rows(&app)
            .into_iter()
            .find(|r| matches!(r.kind, Kind::Edit(Field::RemoveKey(..))))
            .unwrap();
        open(&mut app, &row, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().button, 1);
        app.on_key(Key::Enter, &mut d);
        assert!(app.preferences.editor.is_none());
        assert!(app.awaiting.is_empty());
        d.engine.shutdown();
    }

    #[test]
    fn errors_are_actionable_without_exposing_provider_response_bodies() {
        let mut app = app();
        let origin = Page::Provider("zeta".into());
        app.preferences.navigate(origin.clone());
        ack(
            &mut app,
            Request {
                token: 1,
                session: Some(1),
                origin,
                editor: None,
                operation: Operation::Probe("zeta".into()),
                before: None,
            },
            false,
            Some("provider: HTTP 401: secret-server-message".into()),
            None,
        );
        let shown = ansi::plain(&render(&app, 100, 32).0.lines.join("\n"));
        assert!(shown.contains("HTTP 401"));
        assert!(shown.contains("Update the key"));
        assert!(!shown.contains("secret-server-message"));
    }
    #[test]
    fn overview_arrows_follow_the_visible_grid_and_do_not_wrap_edges() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-grid");
        app.preferences.width = 100;
        app.preferences.height = 40;
        for (key, row) in [
            (Key::Down, 2),
            (Key::Right, 3),
            (Key::Up, 1),
            (Key::Left, 0),
            (Key::Left, 0),
        ] {
            app.on_key(key, &mut d);
            assert_eq!(app.preferences.location.row, row);
        }
        app.preferences.width = 40;
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.preferences.location.row, 1);
        app.on_key(Key::Right, &mut d);
        assert_eq!(app.preferences.location.row, 1);
        d.engine.shutdown();
    }

    #[test]
    fn model_focus_and_search_cursor_do_not_reset_selection_on_horizontal_keys() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-model-focus");
        app.preferences.navigate(Page::Models);
        app.on_key(Key::Down, &mut d);
        let selected = app.preferences.location.row;
        app.on_key(Key::Right, &mut d);
        assert_eq!(app.preferences.location.row, selected);
        app.on_key(Key::Left, &mut d);
        assert_eq!(app.preferences.location.focus, ModelFocus::Providers);
        app.on_key(Key::Down, &mut d);
        assert_eq!(model_provider(&app), "zeta");
        app.on_key(Key::Right, &mut d);
        app.on_key(Key::Char('z'), &mut d);
        app.on_key(Key::Down, &mut d);
        app.on_key(Key::Down, &mut d);
        let selected = app.preferences.location.row;
        app.on_key(Key::Ctrl('f'), &mut d);
        app.on_key(Key::Left, &mut d);
        assert_eq!(app.preferences.location.row, selected);
        assert_eq!(app.preferences.location.cursor, 0);
        d.engine.shutdown();
    }

    #[test]
    fn selection_uses_chevrons_and_transparent_cards_and_terminal_has_no_mouse_toggle() {
        let mut app = app();
        let rs = rows(&app);
        let line = card(&rs[0], true, 44).join("");
        assert!(!line.contains("\x1b[48;"));
        assert!(!line.contains('▌'));
        assert!(line.contains('❯'));
        app.preferences.navigate(Page::Appearance);
        assert!(rows(&app)
            .iter()
            .all(|r| !r.label.to_lowercase().contains("mouse")));
        let view = ansi::plain(&render(&app, 100, 32).0.lines.join("\n"));
        assert!(!view.contains('✓'));
    }
    #[test]
    fn up_enters_navigation_and_down_returns_without_tab_but_wheel_does_not() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-navigation-boundary");
        app.preferences.width = 100;
        app.preferences.height = 40;
        app.on_key(Key::Up, &mut d);
        assert!(app.preferences.tabs);
        app.on_key(Key::Down, &mut d);
        assert!(!app.preferences.tabs);
        assert_eq!(app.preferences.location.row, 0);
        key(
            &mut app,
            &Key::Mouse(super::super::input::Mouse {
                kind: super::super::input::Click::WheelUp,
                row: 15,
                col: 10,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(!app.preferences.tabs);
        app.preferences.navigate(Page::Generation);
        settle(&mut app);
        let selected = app.preferences.location.row;
        app.on_key(Key::Up, &mut d);
        assert!(app.preferences.tabs);
        app.on_key(Key::Down, &mut d);
        assert!(!app.preferences.tabs);
        assert_eq!(app.preferences.location.row, selected);
        app.preferences.navigate(Page::Models);
        settle(&mut app);
        app.on_key(Key::Up, &mut d);
        assert_eq!(app.preferences.location.focus, ModelFocus::Search);
        app.on_key(Key::Up, &mut d);
        assert!(app.preferences.tabs);
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.preferences.location.focus, ModelFocus::Search);
        assert!(!app.preferences.tabs);
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.preferences.location.focus, ModelFocus::List);
        d.engine.shutdown();
    }

    #[test]
    fn terminal_arrows_switch_theme_in_place_and_provider_identity_stays_visible() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-theme-arrows");
        app.preferences.navigate(Page::Appearance);
        let old = app.theme;
        app.on_key(Key::Right, &mut d);
        assert_eq!(app.theme, old.next());
        assert_eq!(app.preferences.location.page, Page::Appearance);
        app.on_key(Key::Left, &mut d);
        assert_eq!(app.theme, old);
        app.preferences.navigate(Page::Provider("zeta".into()));
        let top_text = ansi::plain(&render(&app, 100, 32).0.lines.join("\n"));
        assert!(top_text.contains("ENDPOINT"));
        assert!(top_text.contains("zeta"));
        app.preferences.location.row = rows(&app).len() - 1;
        let bottom_text = ansi::plain(&render(&app, 100, 32).0.lines.join("\n"));
        assert!(bottom_text.contains("zeta"));
        d.engine.shutdown();
    }

    #[test]
    fn delete_model_in_settings_models_list() {
        let mut app = app();
        let mut d = super::super::test_driver("delete-model-settings");
        app.preferences.navigate(Page::Models);
        app.preferences.location.focus = ModelFocus::List;
        app.preferences.location.provider = "alpha".into();
        settle(&mut app);

        let r = rows(&app);
        assert!(r
            .get(app.preferences.location.row)
            .is_some_and(|row| matches!(row.kind, Kind::Model { .. })));

        app.on_key(Key::Delete, &mut d);
        assert!(app.preferences.pending.is_some());
        assert!(app
            .preferences
            .notice
            .as_ref()
            .is_some_and(|n| n.text.contains("Removing model")));
        d.engine.shutdown();
    }

    #[test]
    fn effort_deduplication_and_arrow_cycling_in_settings() {
        let mut app = app();
        let mut d = super::super::test_driver("test-effort-cycling");
        app.settings = json!({
            "settings": { "provider": "openrouter", "model": "google/gemini-2.5-flash", "thinking": true, "effort": "medium" },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices": ["off", "minimal", "low", "medium", "high", "max"] },
                { "key": "temperature", "label": "Temperature", "kind": "float" }
            ],
            "suppressed": []
        });
        app.preferences.navigate(Page::Generation);
        let s_rows = rows(&app);

        // 1. Thinking row is omitted when effort is available (deduplicated!)
        assert!(!s_rows
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::Knob("thinking".into()))));
        let effort_row_idx = s_rows
            .iter()
            .position(|r| r.kind == Kind::Edit(Field::Knob("effort".into())))
            .unwrap();
        assert_eq!(s_rows[effort_row_idx].value, "‹ medium ›");
        assert_eq!(s_rows[effort_row_idx].note, "←→ switches and saves");

        // 2. Right arrow cycles to next choice ("high") in place without opening editor
        app.preferences.location.row = effort_row_idx;
        app.on_key(Key::Right, &mut d);
        assert!(app.preferences.editor.is_none());
        assert!(!app.awaiting.is_empty());

        // 3. Left arrow cycles back
        app.on_key(Key::Left, &mut d);
        assert!(app.preferences.editor.is_none());

        // 4. When effort is off, value_of returns off
        app.settings["settings"]["thinking"] = json!(false);
        app.settings["settings"]["effort"] = json!("off");
        assert!(!app.thinking());
        assert_eq!(app.effort(), None);

        d.engine.shutdown();
    }

    /// Owner, 2026-10-15: a 9router route whose name carries its level showed
    /// "…-high medium". With no level chosen the row says "route default",
    /// it sits between Off and the levels, and every way of choosing (arrows,
    /// Enter, Alt+T) starts from it and can come back to it.
    #[test]
    fn a_gateway_routes_own_level_is_a_named_choice_between_off_and_the_levels() {
        let mut app = app();
        let mut d = super::super::test_driver("route-default-effort");
        app.settings = json!({
            "settings": { "provider": "9router", "model": "ag/gemini-3.8-flash-high", "thinking": true },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice",
                  "choices": ["off", "default", "low", "medium", "high"] }
            ],
            "suppressed": []
        });
        assert_eq!(
            knob_choices(&app, "effort").unwrap(),
            ["off", "default", "low", "medium", "high"]
        );
        let choices = knob_choices(&app, "effort").unwrap();
        assert_eq!(app.effort_choice(&choices), "default");
        assert_eq!(
            super::super::thinking_label(app.thinking(), app.effort()),
            None
        );

        app.preferences.navigate(Page::Generation);
        let rows = rows(&app);
        let row = rows
            .iter()
            .find(|r| r.kind == Kind::Edit(Field::Knob("effort".into())))
            .unwrap();
        assert_eq!(row.value, "‹ route default ›");
        assert!(
            row.help.contains("Route default asks for no level"),
            "{}",
            row.help
        );

        // Alt+T from the route default goes to the least level; from the most
        // it wraps to Off and then back to the route default, never a level
        // in its place.
        app.cycle_thinking(&mut d);
        assert_eq!(app.effort(), Some("low"));
        app.settings["settings"]["effort"] = json!("high");
        app.cycle_thinking(&mut d);
        assert!(!app.thinking());
        app.cycle_thinking(&mut d);
        assert!(app.thinking());
        assert_eq!(app.effort(), None);
        assert_eq!(app.effort_choice(&choices), "default");

        // A route that must think offers no Off; the route default stays first.
        app.settings["thinking_required"] = json!(true);
        assert_eq!(
            knob_choices(&app, "effort").unwrap(),
            ["default", "low", "medium", "high"]
        );
        d.engine.shutdown();
    }

    #[test]
    fn google_provider_renders_effort_row_when_efforts_available() {
        let mut app = app();
        let mut d = super::super::test_driver("test-google-thinking-ui");
        app.settings = json!({
            "settings": { "provider": "google", "model": "gemini-3.8-flash", "thinking": true, "effort": "medium" },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices": ["off", "minimal", "low", "medium", "high"] },
                { "key": "temperature", "label": "Temperature", "kind": "float" }
            ],
            "suppressed": [
                { "key": "frequency_penalty", "reason": "this endpoint has no such parameter", "permanent": true },
                { "key": "presence_penalty", "reason": "this endpoint has no such parameter", "permanent": true }
            ]
        });
        app.preferences.navigate(Page::Generation);
        let s_rows = rows(&app);

        // Effort row MUST be present and show active choice
        let effort_row = s_rows
            .iter()
            .find(|r| r.kind == Kind::Edit(Field::Knob("effort".into())));
        assert!(
            effort_row.is_some(),
            "Effort row must be displayed for Google thinking model"
        );
        let row = effort_row.unwrap();
        assert_eq!(row.label, "Effort");
        assert_eq!(row.value, "‹ medium ›");
        assert_eq!(row.note, "←→ switches and saves");

        // Redundant boolean thinking row is deduplicated when effort choices exist
        assert!(!s_rows
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::Knob("thinking".into()))));

        d.engine.shutdown();
    }

    #[test]
    fn openai_and_effort_providers_render_effort_row_when_efforts_available() {
        let mut app = app();
        let mut d = super::super::test_driver("test-openai-effort-ui");
        app.settings = json!({
            "settings": { "provider": "openai", "model": "o3-mini", "thinking": true, "effort": "medium" },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices": ["off", "low", "medium", "high"] },
                { "key": "temperature", "label": "Temperature", "kind": "float" }
            ],
            "suppressed": []
        });
        app.preferences.navigate(Page::Generation);
        let s_rows = rows(&app);

        let effort_row = s_rows
            .iter()
            .find(|r| r.kind == Kind::Edit(Field::Knob("effort".into())));
        assert!(
            effort_row.is_some(),
            "Effort row must be displayed for OpenAI thinking model"
        );
        let row = effort_row.unwrap();
        assert_eq!(row.label, "Effort");
        assert_eq!(row.value, "‹ medium ›");
        assert_eq!(row.note, "←→ switches and saves");
        assert!(!s_rows
            .iter()
            .any(|r| r.kind == Kind::Edit(Field::Knob("thinking".into()))));

        d.engine.shutdown();
    }

    #[test]
    fn model_alt_c_opens_optional_model_config_and_tab_click_navigates() {
        let mut app = app();
        let mut d = super::super::test_driver("test-configured-model-screens");
        app.preferences.navigate(Page::Models);
        app.preferences.location.provider = "alpha".into();
        settle(&mut app);

        // c opens the same editor even when the API list has no limits or prices.
        let r = rows(&app);
        let model_idx = r
            .iter()
            .position(|row| matches!(row.kind, Kind::Model { .. }))
            .unwrap();
        let Kind::Model {
            provider: initial_provider,
            id: initial_id,
        } = &r[model_idx].kind
        else {
            unreachable!()
        };
        app.preferences.location.row = model_idx;
        app.preferences.location.focus = ModelFocus::List;
        app.on_key(Key::Alt('c'), &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::ModelConfig {
                provider: initial_provider.clone(),
                model: initial_id.clone()
            }
        );
        assert!(rows(&app).iter().any(|r| r.value == "Not set"));

        click(&mut app, Target::Tabs { first: 0, last: 5 }, 5, &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::Overview,
            "Should switch to Overview"
        );

        // 3. Mark model as configured and verify 'c' navigates to Page::ModelConfig
        app.preferences.navigate(Page::Models);
        app.preferences.location.provider = "alpha".into();
        settle(&mut app);
        let Kind::Model {
            ref provider,
            ref id,
        } = rows(&app)[model_idx].kind.clone()
        else {
            panic!()
        };
        let model_info = runeengine::settings::providers::ModelInfo {
            id: id.clone(),
            context: Some(1_000_000),
            max_output: Some(8192),
            price: Some(runeengine::settings::providers::Price {
                input: 0.15,
                cached: 0.075,
                output: 0.60,
                tier_context_threshold: Some(200_000),
                tier_input: Some(0.30),
                tier_output: Some(1.20),
                busy_input: Some(0.20),
                busy_output: Some(0.80),
                ..Default::default()
            }),
            ..Default::default()
        };
        app.update_cached_model(provider, &model_info);

        // The same c editor is available after the model is configured.
        app.preferences.location.row = model_idx;
        app.preferences.location.focus = ModelFocus::List;
        app.on_key(Key::Alt('c'), &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::ModelConfig {
                provider: provider.clone(),
                model: id.clone()
            }
        );

        // 4. On Page::ModelConfig, verify sub-screens for tiered pricing and busy pricing
        let cfg_rows = rows(&app);
        let tier_idx = cfg_rows
            .iter()
            .position(|r| matches!(r.kind, Kind::Navigate(Page::ModelTierPricing { .. })))
            .unwrap();
        app.preferences.location.row = tier_idx;
        app.on_key(Key::Enter, &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::ModelTierPricing {
                provider: provider.clone(),
                model: id.clone()
            }
        );

        // Esc goes back to Page::ModelConfig
        app.on_key(Key::Esc, &mut d);
        assert_eq!(
            app.preferences.location.page,
            Page::ModelConfig {
                provider: provider.clone(),
                model: id.clone()
            }
        );

        d.engine.shutdown();
    }

    #[test]
    fn saving_model_context_updates_app_context_immediately_and_is_not_blocked_by_pending() {
        let mut app = app();
        let mut d = super::super::test_driver("test-model-context-immediate");
        app.preferences.navigate(Page::ModelConfig {
            provider: "alpha".into(),
            model: "z-model".into(),
        });
        settle(&mut app);
        // Simulate a pending background operation from previous action:
        app.preferences.pending = Some(999);
        assert_eq!(app.context, 0);

        // Open context window editor:
        let r = rows(&app);
        let ctx_idx = r
            .iter()
            .position(|r| matches!(r.kind, Kind::Edit(Field::ModelContext { .. })))
            .unwrap();
        app.preferences.location.row = ctx_idx;
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_some(),
            "ModelConfig editor must open even if pending is some"
        );

        // Type 1,000,000 and press Enter:
        if let Some(e) = app.preferences.editor.as_mut() {
            e.text = "1000000".into();
        }
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_none(),
            "Editor must apply and close"
        );
        assert_eq!(
            app.context, 1_000_000,
            "Context must be immediately updated without delay"
        );
        assert!(
            rows(&app).iter().any(|r| r.value == "1,000,000 tokens"),
            "ModelConfig page must immediately reflect the new value"
        );

        d.engine.shutdown();
    }

    #[test]
    fn model_config_reasoning_toggle_enter_saves_off_immediately() {
        let mut app = app();
        let mut d = super::super::test_driver("model-config-reasoning-toggle");
        app.providers = serde_json::json!({
            "providers": [
                {
                    "name": "openrouter",
                    "models": [
                        {
                            "id": "test-thinking-model",
                            "thinking": true, "reasoning_enabled": true,
                            "context": 128000
                        }
                    ]
                }
            ]
        });
        app.preferences.navigate(Page::ModelConfig {
            provider: "openrouter".into(),
            model: "test-thinking-model".into(),
        });
        settle(&mut app);

        let r = rows(&app);
        let thinking_row_idx = r
            .iter()
            .position(|r| matches!(r.kind, Kind::Edit(Field::ModelReasoning { .. })))
            .expect("Reasoning row");
        assert!(r[thinking_row_idx].value.starts_with("On"));

        // Open editor
        app.preferences.location.row = thinking_row_idx;
        app.on_key(Key::Enter, &mut d);
        assert!(app.preferences.editor.is_some());
        let ed = app.preferences.editor.as_ref().unwrap();
        assert_eq!(ed.choices, vec!["off", "on"]);
        assert_eq!(ed.cursor, 1);

        // Move cursor down to "off"
        app.on_key(Key::Up, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().cursor, 0);
        assert_eq!(app.preferences.editor.as_ref().unwrap().text, "off");

        // Press Enter on "off" -> MUST SAVE AND CLOSE IMMEDIATELY!
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_none(),
            "Editor must immediately save and close on Enter over single-choice option"
        );

        // Verify row now shows Disabled
        let r_after = rows(&app);
        assert!(r_after[thinking_row_idx].value.starts_with("Off"));
        assert!(
            app.lookup_model_info("openrouter", "test-thinking-model")
                .unwrap()
                .thinking,
            "Off must not erase the capability"
        );
        app.preferences.location.row = thinking_row_idx;
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_some(),
            "Off must remain reversible"
        );

        d.engine.shutdown();
    }

    #[test]
    fn model_config_default_effort_enter_saves_effort_immediately() {
        let mut app = app();
        let mut d = super::super::test_driver("model-config-effort-toggle");
        app.providers = serde_json::json!({
            "providers": [
                {
                    "name": "openrouter",
                    "models": [
                        {
                            "id": "test-effort-model",
                            "thinking": true,
                            "context": 128000,
                            "efforts": ["low", "medium", "high"],
                            "default_effort": "low"
                        }
                    ]
                }
            ]
        });
        app.preferences.navigate(Page::ModelConfig {
            provider: "openrouter".into(),
            model: "test-effort-model".into(),
        });
        settle(&mut app);

        let r = rows(&app);
        let effort_row_idx = r
            .iter()
            .position(|r| matches!(r.kind, Kind::Edit(Field::ModelReasoning { .. })))
            .expect("Reasoning row");
        assert!(r[effort_row_idx].value.contains("low"));

        // Open editor
        app.preferences.location.row = effort_row_idx;
        app.on_key(Key::Enter, &mut d);
        assert!(app.preferences.editor.is_some());
        let ed = app.preferences.editor.as_ref().unwrap();
        assert_eq!(ed.choices, vec!["off", "low", "medium", "high"]);
        assert_eq!(ed.cursor, 0);
        assert!(ed.is_multi_choice());

        // Move to "high" (cursor 3)
        app.on_key(Key::Down, &mut d);
        app.on_key(Key::Down, &mut d);
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().cursor, 3);

        app.on_key(Key::Enter, &mut d);
        assert!(app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .text
            .contains("high"));
        assert!(
            app.preferences.editor.is_some(),
            "checklist Enter toggles, not save"
        );
        app.on_key(Key::Tab, &mut d);
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_none(),
            "Editor must immediately save and close on Enter over effort choice"
        );

        // Verify row now shows "high"
        let r_after = rows(&app);
        assert!(r_after[effort_row_idx].value.contains("high"));

        d.engine.shutdown();
    }

    /// Owner, 2026-10-15: a model added by hand (or deleted and added again
    /// before its catalogue came back) said "Unsupported / unknown" in Alt+C ›
    /// Reasoning and opened nothing. Unknown is configurable.
    #[test]
    fn model_config_reasoning_of_a_bare_model_is_configurable() {
        let mut app = app();
        let mut d = super::super::test_driver("model-config-bare-reasoning");
        app.providers = serde_json::json!({
            "providers": [
                { "name": "9router", "api": "9router", "models": [ { "id": "hand/new" } ] }
            ]
        });
        app.preferences.navigate(Page::ModelConfig {
            provider: "9router".into(),
            model: "hand/new".into(),
        });
        settle(&mut app);
        let r = rows(&app);
        let row_idx = r
            .iter()
            .position(|r| matches!(r.kind, Kind::Edit(Field::ModelReasoning { .. })))
            .expect("Reasoning row");
        assert!(
            !r[row_idx].value.contains("Unsupported"),
            "{}",
            r[row_idx].value
        );
        app.preferences.location.row = row_idx;
        app.on_key(Key::Enter, &mut d);
        let ed = app.preferences.editor.as_ref().expect("the editor opens");
        assert!(ed.is_multi_choice());
        assert_eq!(
            ed.choices,
            vec!["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
        d.engine.shutdown();
    }

    #[test]
    fn model_config_9router_reasoning_checklist_multi_choice() {
        let mut app = app();
        let mut d = super::super::test_driver("model-config-9router-checklist");
        app.providers = serde_json::json!({
            "providers": [
                {
                    "name": "9router",
                    "api": "9router",
                    "models": [
                        {
                            "id": "bzl/claude-opus-4.7",
                            "thinking": true,
                            "capabilities": {
                                "reasoning": true,
                                "thinkingFormat": "claude-adaptive"
                            }
                        }
                    ]
                }
            ]
        });
        app.preferences.navigate(Page::ModelConfig {
            provider: "9router".into(),
            model: "bzl/claude-opus-4.7".into(),
        });
        settle(&mut app);

        let r = rows(&app);
        let row_idx = r
            .iter()
            .position(|r| matches!(r.kind, Kind::Edit(Field::ModelReasoning { .. })))
            .expect("Reasoning row for 9router");

        // Open editor
        app.preferences.location.row = row_idx;
        app.on_key(Key::Enter, &mut d);
        assert!(app.preferences.editor.is_some());
        let ed = app.preferences.editor.as_ref().unwrap();
        assert!(ed.is_multi_choice());
        assert_eq!(
            ed.choices,
            vec!["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        );

        d.engine.shutdown();
    }

    #[test]
    fn aux_settings_require_double_confirmation_and_render_sensitive_warnings() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-aux-double-confirm");
        app.preferences.navigate(Page::Generation);
        settle(&mut app);

        // 1. Verify section header has SENSITIVE AREA banner
        let rs = rows(&app);
        let header_row = rs
            .iter()
            .find(|r| r.label.contains("AUXILIARY MODELS"))
            .expect("auxiliary models header row");
        assert!(header_row.label.contains("SENSITIVE AREA"));
        let (sink, _) = render(&app, 100, 40);
        let rendered = sink.lines.join("\n");
        assert!(rendered.contains("AUXILIARY MODELS"));
        assert!(rendered.contains("SENSITIVE AREA"));

        // 2. Find an aux setting row (e.g. compact.message_chars or compact.keep_recent_turns)
        let aux_row = rs
            .iter()
            .find(|r| {
                matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "compact.keep_recent_turns")
            })
            .unwrap();
        open(&mut app, aux_row, &mut d);
        assert!(app.preferences.editor.is_some());
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(e.is_aux());

        // 3. Render editor and check for red sensitive warning
        let (sink, _) = render(&app, 100, 40);
        let editor_render = sink.lines.join("\n");
        assert!(editor_render.contains("SENSITIVE PARAMETER"));

        // 4. First Enter: does NOT save, sets confirm_apply_at and updates button countdown
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_some(),
            "Editor should stay open on first Enter"
        );
        assert!(app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .is_confirming_apply());
        assert!(
            app.preferences.notice.is_none(),
            "No duplicate notice should be displayed"
        );

        // 5. Check button reflects confirmation countdown, while hint remains clean
        let (sink, _) = render(&app, 100, 40);
        let editor_render = sink.lines.join("\n");
        assert!(editor_render.contains("Apply [Press again to confirm ("));
        let current_hint = hint(&app);
        assert!(
            !current_hint.contains("confirm"),
            "Hint should not duplicate the 3s countdown"
        );

        // 6. Simulate timeout expiration (> 3s)
        app.preferences.editor.as_mut().unwrap().confirm_apply_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(4));
        assert!(!app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .is_confirming_apply());

        // Next Enter resets the confirmation window instead of applying
        app.on_key(Key::Enter, &mut d);
        assert!(
            app.preferences.editor.is_some(),
            "Editor should stay open after expired confirmation"
        );
        assert!(
            app.preferences
                .editor
                .as_ref()
                .unwrap()
                .is_confirming_apply(),
            "Timer should reset to active 3s window"
        );

        // 7. Second Enter within 3s: confirmation succeeds, triggers save!
        app.on_key(Key::Enter, &mut d);
        assert!(!app.awaiting.is_empty() || app.preferences.editor.is_none());

        // 8. Test checklist on boolean aux setting (classifier.enabled)
        app.preferences.pending = None;
        app.preferences.editor = None;
        let bool_row = rs
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.enabled"))
            .unwrap();
        open(&mut app, bool_row, &mut d);
        let (sink, _) = render(&app, 100, 40);
        let bool_render = ansi::plain(&sink.lines.join("\n"));
        assert!(bool_render.contains("[✓] true") || bool_render.contains("[✓] false"));
        // The cursor field is the text caret until normalised; aim it at the
        // checked item the way the movement keys do, then Space takes it.
        {
            let e = app.preferences.editor.as_mut().unwrap();
            e.cursor = e.choices.iter().position(|c| c == &e.text).unwrap_or(0);
        }
        let prev_val = app.preferences.editor.as_ref().unwrap().text.clone();
        app.on_key(Key::Char(' '), &mut d);
        assert_ne!(prev_val, app.preferences.editor.as_ref().unwrap().text);

        d.engine.shutdown();
    }

    #[test]
    fn context_tools_has_one_independent_checklist_home_and_six_defaults() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-classifier-context-tools");
        app.settings["configuration"] =
            runeengine::settings::configuration::Configuration::default().redacted();
        app.preferences.navigate(Page::Generation);
        settle(&mut app);
        let rs = rows(&app);
        let context: Vec<_> = rs.iter().filter(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.context_tools")).collect();
        assert_eq!(context.len(), 1);
        assert!(context[0].value.contains("included in context"));
        open(&mut app, context[0], &mut d);
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(e.is_multi_choice());
        assert_eq!(
            e.text.split(',').filter(|v| !v.trim().is_empty()).count(),
            6
        );
        let guidance =
            field_guidance(&app, &Field::Global("classifier.context_tools".into())).unwrap();
        assert!(guidance.contains("historical inputs") && !guidance.contains("excluded from"));
        d.engine.shutdown();
    }

    #[test]
    fn excluded_tools_checklist_and_duration_breakdown() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-excluded-tools-checklist");
        app.preferences.navigate(Page::Generation);
        settle(&mut app);

        // 1. Verify title.timeout and compact.timeout are removed, classifier.timeout remains
        let rs = rows(&app);
        assert!(
            !rs.iter()
                .any(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "title.timeout")),
            "title.timeout must NOT be present (model timeouts removed)"
        );
        assert!(
            !rs.iter()
                .any(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "compact.timeout")),
            "compact.timeout must NOT be present (model timeouts removed)"
        );
        let classifier_timeout_row = rs
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.timeout"))
            .expect("classifier.timeout row");
        assert_eq!(classifier_timeout_row.value, "120s");

        // 2. Open classifier.excluded_tools editor
        let tools_row = rs
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.excluded_tools"))
            .expect("classifier.excluded_tools row");
        open(&mut app, tools_row, &mut d);
        assert!(app.preferences.editor.is_some());
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(
            e.is_multi_choice(),
            "excluded_tools must be a multi_choice editor"
        );
        assert!(e.choices.contains(&"ask_user_questions".to_string()));
        assert!(e.choices.contains(&"web_search".to_string()));
        assert!(e.choices.contains(&"web_fetch".to_string()));

        // 3. Render editor and check for checklist display
        let (sink, _) = render(&app, 100, 40);
        let rendered = ansi::plain(&sink.lines.join("\n"));
        assert!(rendered.contains("ask_user_questions"));
        assert!(rendered.contains("[ ]") || rendered.contains("[✓]"));
        let current_hint = hint(&app);
        assert!(
            current_hint.contains("Space toggle"),
            "Hint must indicate Space to toggle checklist"
        );

        // 4. Toggle tool with Space
        let tool_name = app.preferences.editor.as_ref().unwrap().choices[0].clone();
        app.on_key(Key::Char(' '), &mut d);
        assert!(app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .text
            .contains(&tool_name));

        // 5. Navigate with Down arrow and toggle second tool
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().cursor, 1);
        let second_tool = app.preferences.editor.as_ref().unwrap().choices[1].clone();
        app.on_key(Key::Char(' '), &mut d);
        assert!(app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .text
            .contains(&second_tool));

        // 6. Test duration breakdown line and rollover calculations (no duplicate 3 boxes!)
        let d_line_3m = ansi::plain(&duration_breakdown_line("3m", false, None));
        assert!(
            !d_line_3m.contains("Min") && !d_line_3m.contains("Sec") && !d_line_3m.contains("Ms")
        );
        assert!(d_line_3m.contains("Segmented breakdown: 3m (180s)"));

        // Rollover: 1500ms -> 1s 500ms (1.5s)
        let d_line_1500ms = ansi::plain(&duration_breakdown_line("1500ms", false, None));
        assert!(d_line_1500ms.contains("Segmented breakdown: 1s 500ms (1.5s)"));

        // Rollover: 65400ms -> 1m 5s 400ms (65.4s)
        let d_line_65400ms = ansi::plain(&duration_breakdown_line("65400ms", false, None));
        assert!(d_line_65400ms.contains("Segmented breakdown: 1m 5s 400ms (65.4s)"));

        // Rollover: 90s -> 1m 30s (90s)
        let d_line_90s = ansi::plain(&duration_breakdown_line("90s", false, None));
        assert!(d_line_90s.contains("Segmented breakdown: 1m 30s (90s)"));

        // Empty inherited (like title.timeout): MUST display Inherited
        let d_line_empty_inherited = ansi::plain(&duration_breakdown_line("", true, None));
        assert!(d_line_empty_inherited
            .contains("Segmented breakdown: Inherited · Provider request timeout"));

        // Aux model timeout limit (> 60m rejected)
        let d_line_exceeded = ansi::plain(&duration_breakdown_line("25h", false, None));
        assert!(d_line_exceeded.contains("Waiting for valid duration"));

        // Size breakdown line
        let s_line = ansi::plain(&size_breakdown_line("128KiB", None, Some(64 * 1024 * 1024)));
        assert!(s_line.contains("Segmented breakdown: 128KiB (131072 bytes)"));

        // 7. Verify classifier.timeout UI render displays the single breakdown line (NO 3 boxes)
        app.preferences.editor = None;
        let classifier_timeout_row = rs
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.timeout"))
            .expect("classifier.timeout row");
        open(&mut app, classifier_timeout_row, &mut d);
        assert!(app.preferences.editor.is_some());
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(e.is_duration(), "classifier.timeout must be duration");
        assert_eq!(e.text, "");
        let (classifier_sink, _) = render(&app, 100, 40);
        let classifier_render = ansi::plain(&classifier_sink.lines.join("\n"));
        assert!(
            !classifier_render.contains("╭─ Min")
                && !classifier_render.contains("╭─ Sec")
                && !classifier_render.contains("╭─ MiB")
        );
        assert!(classifier_render.contains("Segmented breakdown: 2m (Default · 120s)"));

        // 8. Verify count fields (e.g. compact.message_chars) NEVER render duration breakdown
        app.preferences.editor = None;
        let count_row = rs
            .iter()
            .find(
                |r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "compact.message_chars"),
            )
            .expect("compact.message_chars row");
        open(&mut app, count_row, &mut d);
        assert!(app.preferences.editor.is_some());
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(!e.is_duration(), "message_chars must NOT be duration");
        assert!(!e.is_size(), "message_chars must NOT be size");
        assert_eq!(
            e.max_len(&app),
            8,
            "compact.message_chars editor must fit its configured count range"
        );
        let (count_sink, _) = render(&app, 100, 40);
        let count_render = ansi::plain(&count_sink.lines.join("\n"));
        assert!(
            !count_render.contains("Segmented breakdown:"),
            "Pure integer count must NEVER show segmented duration breakdown!"
        );

        // 9. Verify dynamic AUX models (max_output_tokens, fallback_context_tokens) resolution
        app.preferences.editor = None;
        let gen_rows = sampling_rows(&app);
        let classifier_tok_row = gen_rows
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.max_output_tokens"))
            .expect("classifier.max_output_tokens row");
        assert!(classifier_tok_row.value.starts_with("Auto · 1024 tokens"));

        let compact_ctx_row = gen_rows
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "compact.fallback_context_tokens"))
            .expect("compact.fallback_context_tokens row");
        assert!(compact_ctx_row.value.starts_with("Auto ·"));

        // 10. Verify Photo 1 input filtering & Photo 2 boolean validation & single timeout architecture
        app.preferences.editor = None;
        let t_rows = tools_rows(&app);
        assert!(
            !t_rows.iter().any(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "terminal.max_command_timeout")),
            "terminal.max_command_timeout must NOT be present in tools rows (single timeout architecture)"
        );

        // Photo 1: Find files max limit (Count) only allows digits
        let find_limit_row = t_rows
            .iter()
            .find(
                |r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "find_files.matches" || k == "find_files.max_matches" || k == "find_files.max_limit"),
            )
            .expect("find_files.matches row");
        open(&mut app, find_limit_row, &mut d);
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(e.allows_char('2'));
        assert!(e.allows_char('0'));
        assert!(
            !e.allows_char('e'),
            "Count field must NOT allow letter 'e' (Photo 1 fix)"
        );
        assert!(
            !e.allows_char('w'),
            "Count field must NOT allow letter 'w' (Photo 1 fix)"
        );

        // Duration field allows digits and duration units
        app.preferences.editor = None;
        let timeout_row = t_rows
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "terminal.command_timeout"))
            .expect("terminal.command_timeout row");
        open(&mut app, timeout_row, &mut d);
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(e.allows_char('5'));
        assert!(e.allows_char('m'));
        assert!(e.allows_char('s'));
        assert!(e.allows_char(' '));
        assert!(
            !e.allows_char('x'),
            "Duration field must not allow arbitrary letters like 'x'"
        );

        // Photo 2: Boolean field guidance and validation
        app.preferences.editor = None;
        let preference = Row::new(
            Kind::Edit(Field::Global("ui.sidebar".into())),
            "Session panel",
            "true".into(),
        );
        open(&mut app, &preference, &mut d);
        let guidance = field_guidance(&app, &Field::Global("ui.sidebar".into())).unwrap();
        assert!(
            guidance.contains("Space"),
            "Boolean field guidance must explain toggle, not duration units"
        );
        assert!(
            !guidance.contains("Units: m, s"),
            "Boolean field must NEVER show duration units guidance (Photo 2 fix)"
        );

        // Apply boolean value "true" cleanly without duration error
        app.preferences.editor.as_mut().unwrap().text = "true".into();
        save_editor(&mut app, &mut d);
        assert!(
            app.preferences
                .notice
                .as_ref()
                .map(|n| n.tone != Tone::Error)
                .unwrap_or(true),
            "Applying boolean true must NOT produce duration error: {:?}",
            app.preferences.notice
        );

        d.engine.shutdown();
    }

    /// Every choice-bearing editor answers the same movement keys — and the
    /// page keys reach the item list instead of falling into the fallback arm
    /// and dying. The old split sent Up/Down to the buttons on a
    /// single-choice editor, so the checklist and the on/off pair disagreed,
    /// and the caret "jumped to the middle" (the button row).
    #[test]
    fn choice_editors_navigate_items_with_arrows_and_page_keys() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-choice-nav");
        app.preferences.navigate(Page::Generation);
        settle(&mut app);

        // A single-choice editor: classifier.enabled is a boolean → [true,
        // false]. Up/Down must move the item cursor, PageUp/PageDown must pin
        // it to the ends, and the buttons must stay where Tab left them.
        let rs = rows(&app);
        let bool_row = rs
            .iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.enabled"))
            .expect("classifier.enabled row");
        open(&mut app, bool_row, &mut d);
        let e = app.preferences.editor.as_ref().unwrap();
        assert!(!e.choices.is_empty(), "a boolean editor lists its choices");
        // The editor's cursor field is a caret into the *text* for input
        // fields and an item index for choice lists; open() seeds it from the
        // caret, so normalise the list cursor to the shown value first.
        {
            let e = app.preferences.editor.as_mut().unwrap();
            e.cursor = e.choices.iter().position(|c| c == &e.text).unwrap_or(0);
        }
        assert_eq!(app.preferences.editor.as_ref().unwrap().cursor, 0);

        app.on_key(Key::Down, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            1,
            "Down moves the item cursor on a single-choice editor"
        );
        app.on_key(Key::PageDown, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            1,
            "PageDown pins to the last item"
        );
        app.on_key(Key::PageUp, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            0,
            "PageUp pins to the first item"
        );
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            0,
            "Wrapping to actions retains the last choice cursor"
        );
        assert_eq!(app.preferences.editor.as_ref().unwrap().button, 2);
        app.on_key(Key::Down, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().button,
            CHOICES_FOCUS
        );
        assert_eq!(app.preferences.editor.as_ref().unwrap().cursor, 0);
        app.preferences.editor = None;

        // The multi-choice checklist keeps its own consistent language, and
        // the page keys now reach it too.
        let rs = rows(&app);
        let tools_row = rs
            .iter()
            .find(|r| {
                matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.excluded_tools")
            })
            .expect("classifier.excluded_tools row");
        open(&mut app, tools_row, &mut d);
        let count = app.preferences.editor.as_ref().unwrap().choices.len();
        assert!(count > 2, "the checklist has real content: {count}");

        app.on_key(Key::PageDown, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            count - 1,
            "PageDown reaches the last tool in the checklist"
        );
        app.on_key(Key::Down, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().button,
            0,
            "Down at the end of the checklist moves to Apply instead of wrapping to top"
        );
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().button,
            CHOICES_FOCUS,
            "Up returns focus to the checklist"
        );
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            count - 1,
            "Up lands on the last tool in the checklist"
        );
        app.on_key(Key::PageUp, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().cursor,
            0,
            "PageUp returns to the first tool"
        );
        d.engine.shutdown();
    }

    #[test]
    fn checklist_enter_toggles_selection_without_starting_apply_confirmation() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-enter-checklist");
        app.preferences.navigate(Page::Generation);
        settle(&mut app);
        let row = rows(&app)
            .into_iter()
            .find(|r| matches!(&r.kind, Kind::Edit(Field::Global(k)) if k == "classifier.excluded_tools"))
            .expect("checklist row");
        open(&mut app, &row, &mut d);
        let editor = app.preferences.editor.as_ref().unwrap();
        assert_eq!(editor.button, CHOICES_FOCUS);
        let first = editor.choices[editor.cursor].clone();
        let initially_checked = editor.text.split(',').any(|s| s.trim() == first);
        app.on_key(Key::Enter, &mut d);
        let editor = app.preferences.editor.as_ref().unwrap();
        assert_eq!(editor.button, CHOICES_FOCUS);
        assert!(editor.confirm_apply_at.is_none());
        assert_eq!(
            editor.text.split(',').any(|s| s.trim() == first),
            !initially_checked,
            "Enter toggles just the selected tool"
        );
        assert!(app.preferences.pending.is_none());
        app.on_key(Key::Enter, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().confirm_apply_at,
            None
        );
        app.on_key(Key::Tab, &mut d);
        assert_eq!(app.preferences.editor.as_ref().unwrap().button, 0);
        app.on_key(Key::Enter, &mut d);
        assert!(app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .is_confirming_apply());
        assert!(app.preferences.pending.is_none());
        app.on_key(Key::ShiftTab, &mut d);
        assert_eq!(
            app.preferences.editor.as_ref().unwrap().button,
            CHOICES_FOCUS
        );
        assert!(!app
            .preferences
            .editor
            .as_ref()
            .unwrap()
            .is_confirming_apply());
        d.engine.shutdown();
    }

    /// PgUp/PgDn on the settings *page* (no editor open) still walk the rows,
    /// so the page keys answer everywhere on the screen.
    #[test]
    fn page_keys_walk_the_settings_page_when_no_editor_is_open() {
        let mut app = app();
        let mut d = super::super::test_driver("preferences-page-walk");
        app.preferences.navigate(Page::Generation);
        settle(&mut app);
        // Park somewhere with rows above, then check the page keys move the
        // selection both ways. `start` is wherever the last pickable row is.
        let rs = rows(&app);
        let last = rs.iter().rposition(Row::pickable).unwrap_or(0);
        app.preferences.location.row = last;
        app.on_key(Key::PageUp, &mut d);
        assert!(
            app.preferences.location.row < last,
            "PageUp moves the selection up"
        );
        let after_up = app.preferences.location.row;
        app.on_key(Key::PageDown, &mut d);
        assert!(
            app.preferences.location.row > after_up,
            "PageDown moves the selection down"
        );
        d.engine.shutdown();
    }
    #[test]
    fn mouse_caret_edits_settings_and_search_from_the_rendered_projection() {
        use super::super::input::{Click, Modifiers, Mouse};
        let mut app = app();
        let mut driver = super::super::test_driver("settings-text-geometry");
        for secret in [false, true] {
            app.preferences.navigate(Page::Tools);
            let row = Row::new(
                Kind::Edit(if secret {
                    Field::SetupKey
                } else {
                    Field::Global("terminal.shell".into())
                }),
                "Input",
                String::new(),
            );
            open_editor(
                &mut app,
                match &row.kind {
                    Kind::Edit(field) => field.clone(),
                    _ => unreachable!(),
                },
                &row,
            );
            let text = "abcé界e\u{301}👨\u{200d}💻abcdefghijklmnopqrstuvwxyz";
            {
                let e = app.preferences.editor.as_mut().unwrap();
                e.text = text.into();
                e.cursor = text.len();
            }
            for (width, height) in [(100, 40), (20, 8), (32, 16)] {
                let frame = super::super::draw::compose_for_test(&mut app, height, width);
                let (screen, geometry) = app
                    .hits
                    .iter()
                    .enumerate()
                    .find_map(|(screen, hit)| match hit {
                        Hit::Settings(Target::Text {
                            row,
                            editor: Some(_),
                        }) => Some((screen, *row)),
                        _ => None,
                    })
                    .expect("visible editor text row");
                if secret {
                    assert!(!ansi::plain(&frame.join("\n")).contains(text));
                }
                let expected = app.preferences.text_rows[geometry].caret_at(9);
                for kind in [Click::Press, Click::Release] {
                    app.on_key(
                        Key::Mouse(Mouse {
                            kind,
                            row: screen as u16 + 1,
                            col: 9,
                            modifiers: Modifiers::NONE,
                        }),
                        &mut driver,
                    );
                }
                let editor = app.preferences.editor.as_ref().unwrap();
                assert_eq!(editor.cursor, expected);
                assert_eq!(editor.text, text);
                assert!(editor.confirm_apply_at.is_none());
            }
            app.preferences.back();
        }
        app.preferences.navigate(Page::Models);
        app.preferences.location.query = "helloé界 world".into();
        app.preferences.location.cursor = 0;
        for height in [8, 16, 40] {
            super::super::draw::compose_for_test(&mut app, height, 60);
            let (screen, index) = app
                .hits
                .iter()
                .enumerate()
                .find_map(|(screen, hit)| match hit {
                    Hit::Settings(Target::Text { row, editor: None }) => Some((screen, *row)),
                    _ => None,
                })
                .unwrap();
            let col = app.preferences.text_rows[index].first_col as u16 + 2;
            let expected = app.preferences.text_rows[index].caret_at(col);
            for kind in [Click::Press, Click::Release] {
                app.on_key(
                    Key::Mouse(Mouse {
                        kind,
                        row: screen as u16 + 1,
                        col,
                        modifiers: Modifiers::NONE,
                    }),
                    &mut driver,
                );
            }
            assert_eq!(app.preferences.location.cursor, expected);
            assert_eq!(app.preferences.location.focus, ModelFocus::Search);
        }
        driver.engine.shutdown();
    }

    #[test]
    fn global_fields_have_one_editable_home_and_tool_timeouts_stay_in_tools() {
        let mut app = app();
        let mut owners: std::collections::BTreeMap<String, Vec<Page>> =
            std::collections::BTreeMap::new();
        for (_, page) in SECTIONS {
            app.preferences.navigate(page.clone());
            for row in rows(&app) {
                if let Kind::Edit(Field::Global(key)) = row.kind {
                    owners.entry(key).or_default().push(page.clone());
                }
            }
        }
        for (key, pages) in &owners {
            assert_eq!(pages.len(), 1, "duplicate {key}: {pages:?}");
        }
        for key in [
            "terminal.command_timeout",
            "permissions.timeout",
            "questions.timeout",
        ] {
            assert_eq!(owners.get(key), Some(&vec![Page::Tools]), "{key}");
        }
        for key in [
            "retry.max_attempts",
            "retry.initial_delay",
            "retry.max_delay",
            "retry.credit_cooldown",
            "network.stream_idle_timeout",
            "network.connect_timeout",
            "network.probe_timeout",
        ] {
            assert_eq!(owners.get(key), Some(&vec![Page::Runtime]), "{key}");
        }
        assert!(SECTIONS.iter().all(|(name, page)| *name == page.title()));
        assert!(SECTIONS
            .iter()
            .enumerate()
            .all(|(index, (_, page))| index == page.tab()));
    }

    #[test]
    fn appearance_page_has_no_preview_and_contains_color_theme_and_sidebar() {
        let mut app = app();
        app.preferences.navigate(Page::Appearance);
        let rs = rows(&app);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].label, "Color theme");
        assert_eq!(rs[1].label, "Conversation side panel");

        let (sink, _) = render(&app, 100, 32);
        let rendered = sink.lines.join("\n");
        assert!(!rendered.contains("Preview Start"));
        assert!(!rendered.contains("Preview End"));
        assert!(!rendered.contains("Add an LRU cache"));
    }

    #[test]
    fn runtime_page_contains_only_resilience_and_network_timeouts() {
        let mut app = app();
        app.preferences.navigate(Page::Runtime);
        let rs = rows(&app);
        assert!(rs.iter().any(|r| r.label == "RETRY & RESILIENCE"));
        assert!(rs.iter().any(|r| r.label == "NETWORK & STREAM TIMEOUTS"));
        assert!(rs.iter().all(|r| r.label != "SYSTEM TIMEOUTS"));

        let keys: Vec<_> = rs
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Edit(Field::Global(k)) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert!(keys.contains(&"retry.max_attempts"));
        assert!(keys.contains(&"retry.initial_delay"));
        assert!(keys.contains(&"retry.max_delay"));
        assert!(keys.contains(&"retry.credit_cooldown"));
        assert!(keys.contains(&"network.stream_idle_timeout"));
        for key in [
            "terminal.command_timeout",
            "permissions.timeout",
            "questions.timeout",
        ] {
            assert!(!keys.contains(&key), "{key} belongs only to Tools");
        }
    }

    #[test]
    fn connections_page_does_not_contain_retry_group() {
        let mut app = app();
        app.preferences.navigate(Page::Connections);
        let rs = rows(&app);
        assert!(rs.iter().all(|r| r.label != "RETRY"));
        assert!(rs.iter().all(|r| match &r.kind {
            Kind::Edit(Field::Global(k)) => !k.starts_with("retry."),
            _ => true,
        }));
    }

    /// A theme row says what it looks like: two chips in the theme's own colours,
    /// and an explicit active marker rather than the same grey for all three.
    #[test]
    fn a_theme_row_carries_its_own_colour_chip() {
        let mut app = app();
        app.preferences.location.page = Page::Themes;
        app.theme = theme::ThemeMode::Cyan;
        let rs = rows(&app);
        let amber = rs
            .iter()
            .find(|r| matches!(r.kind, Kind::Theme(theme::ThemeMode::Amber)))
            .expect("the amber row");
        let rendered = draw_row(amber, false, 100, None).join("\n");
        let accent = ansi::encode(
            theme::build_with(amber.background, theme::ThemeMode::Amber).of(ansi::Role::Accent),
            amber.color_depth,
            false,
        );
        assert!(
            rendered.contains(&accent),
            "the amber row must chip the amber accent: {:?}",
            ansi::plain(&rendered)
        );

        let active = rs
            .iter()
            .find(|r| matches!(r.kind, Kind::Theme(theme::ThemeMode::Cyan)))
            .expect("the active row");
        let rendered = draw_row(active, false, 100, None).join("\n");
        assert!(
            ansi::plain(&rendered).contains("active"),
            "the active theme must be marked: {:?}",
            ansi::plain(&rendered)
        );
    }

    #[test]
    fn model_rows_displays_catalog_when_settings_has_only_active_model() {
        let mut app = app();
        app.providers = serde_json::json!({
            "providers": [
                {
                    "name": "test-prov",
                    "models": [
                        { "id": "model-1", "context": 128000 },
                        { "id": "model-2", "context": 200000 },
                        { "id": "model-3", "context": 64000 }
                    ]
                }
            ]
        });
        app.settings = serde_json::json!({
            "provider": "test-prov",
            "models": [
                { "id": "model-1", "context": 128000 }
            ],
            "settings": {
                "provider": "test-prov",
                "model": "model-1"
            }
        });
        app.preferences.location.provider = "test-prov".into();
        let rows = model_rows(&app);
        let ids: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert!(ids.contains(&"model-1"));
        assert!(ids.contains(&"model-2"));
        assert!(ids.contains(&"model-3"));
    }
}
