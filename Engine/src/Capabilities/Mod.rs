//! One scoped catalog for Rune tools, portable workflows and MCP capabilities.
#[path = "Auth.rs"]
pub mod auth;
#[path = "Config.rs"]
pub mod config;
#[path = "Http.rs"]
mod http;
#[path = "Input.rs"]
pub mod input;
#[path = "Manage.rs"]
pub mod manage;
#[path = "Mcp.rs"]
pub mod mcp;
pub mod product;
#[path = "Skills.rs"]
pub mod skills;

use crate::session::compact::estimate_tokens;
use crate::settings::configuration::Configuration;
use crate::{panic, Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Inner {
    skills: Vec<skills::Skill>,
    diagnostics: Vec<Value>,
    /// Skills whose `SKILL.md` the model has read whole in this session, by id,
    /// at the package revision it read. Their packages are checked before any
    /// command runs ([`Capabilities::validate_active_packages`]): a script that
    /// changed after its instructions were read does not run on the old
    /// instructions' authority. Kept on disk so a host restart cannot forget it.
    loaded: BTreeMap<String, String>,
}

/// The skill catalogue as the opening context lists it.
pub struct Catalogue {
    /// Every skill on offer.
    pub total: usize,
    /// The ones that fit the catalogue's budget, in discovery order.
    pub entries: Vec<CatalogueEntry>,
}

pub struct CatalogueEntry {
    pub id: String,
    pub description: String,
    /// Its `SKILL.md`.
    pub path: PathBuf,
}

pub struct Capabilities {
    pub input: Arc<input::Input>,
    pub home: PathBuf,
    pub project: PathBuf,
    pub settings: config::Settings,
    state_path: PathBuf,
    inner: Mutex<Inner>,
    runtime: Arc<mcp::Runtime>,
    connections: Mutex<BTreeMap<String, Arc<mcp::Connection>>>,
    logins: Mutex<BTreeMap<String, auth::Login>>,
}

impl Capabilities {
    pub fn open(
        config: &Configuration,
        project: &Path,
        session: i64,
        // Where this session keeps what it has loaded; MCP media sits beside it,
        // in a folder of the same name.
        state_path: PathBuf,
        runtime: Arc<mcp::Runtime>,
        events: Option<crate::session::live::Bus>,
    ) -> Result<Arc<Self>> {
        let settings = config::Settings::from_config(config)?;
        let (skills, diagnostics) = if settings.enabled {
            skills::discover(&settings, &config.home, project)?
        } else {
            (Vec::new(), Vec::new())
        };
        // A state file in an older shape is not this session's to keep: it reads
        // as nothing loaded, which only means the next read records afresh.
        let mut loaded: BTreeMap<String, String> =
            match crate::settings::configuration::read_bounded(
                &state_path,
                settings.max_package_bytes as u64,
            ) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(e) => return Err(e),
            };
        loaded.retain(|id, revision| {
            skills
                .iter()
                .any(|s| s.id == *id && !s.disabled && s.revision == *revision)
        });
        Ok(Arc::new(Self {
            input: input::Input::new(session, events),
            home: config.home.clone(),
            project: project.to_path_buf(),
            settings,
            state_path,
            inner: Mutex::new(Inner {
                skills,
                diagnostics,
                loaded,
            }),
            runtime,
            connections: Mutex::new(BTreeMap::new()),
            logins: Mutex::new(BTreeMap::new()),
        }))
    }

    pub fn inventory(&self) -> Value {
        let inner = panic::lock(&self.inner);
        json!({"skills":inner.skills.iter().map(skills::Skill::entry).collect::<Vec<_>>(),"diagnostics":inner.diagnostics,"active":inner.loaded.keys().collect::<Vec<_>>()})
    }

    pub fn refresh(&self) -> Result<()> {
        let settings = config::Settings::from_config(&Configuration::load(&self.home)?)?;
        let (skills, diagnostics) = if settings.enabled {
            skills::discover(&settings, &self.home, &self.project)?
        } else {
            (Vec::new(), Vec::new())
        };
        let mut inner = panic::lock(&self.inner);
        inner.loaded.retain(|id, revision| {
            skills
                .iter()
                .any(|s| s.id == *id && !s.disabled && s.revision == *revision)
        });
        inner.skills = skills;
        inner.diagnostics = diagnostics;
        Ok(())
    }

    pub fn connect_enabled(
        &self,
        registry: &crate::tools::Registry,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<Value>> {
        let settings = config::Settings::from_config(&Configuration::load(&self.home)?)?;
        let mut errors = Vec::new();
        if !settings.enabled {
            self.close(registry);
            return Ok(errors);
        }
        if let Ok(servers) = settings.resolve_servers(&self.project) {
            for s in servers {
                if !s.approved || !s.definition.enabled {
                    continue;
                }
                if !self
                    .input
                    .changed
                    .load(std::sync::atomic::Ordering::Relaxed)
                    && panic::lock(&self.connections)
                        .get(&s.id)
                        .is_some_and(|c| c.ready() && c.definition.digest == s.digest)
                {
                    continue;
                }
                if let Err(e) = self.connect(&s.id, registry, cancel) {
                    errors.push(json!({"server":s.id,"error":e.to_string()}));
                }
            }
        }
        self.input
            .changed
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(errors)
    }

    pub fn connect(
        &self,
        id: &str,
        registry: &crate::tools::Registry,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        // The gate below read a snapshot taken at session open: enabling
        // capabilities from another process after this session started was
        // refused until restart. The freshly loaded settings are the authority.
        let config = Configuration::load(&self.home)?;
        let settings = config::Settings::from_config(&config)?;
        let definition = settings
            .resolve_servers(&self.project)?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::NotFound("MCP server".into()))?;
        let scratch = self.state_path.with_extension("").join("Mcp").join(id);
        self.disconnect(id, registry);
        let connection = mcp::Connection::connect(
            self.runtime.clone(),
            definition,
            &settings,
            self,
            &scratch,
            cancel,
        )?;
        let mut diagnostics = Vec::new();
        for descriptor in connection.catalog["tools"].as_array().into_iter().flatten() {
            match mcp::RemoteTool::new(connection.clone(), descriptor.clone()) {
                Ok(tool) => registry.register(Arc::new(tool))?,
                Err(_) => diagnostics.push(
                    json!({"tool":descriptor["name"],"error":"Unsupported MCP tool definition"}),
                ),
            }
        }
        let result = json!({"server_id":id,"status":"ready","protocol":connection.version,"catalog":connection.catalog,"diagnostics":diagnostics});
        panic::lock(&self.connections).insert(id.into(), connection);
        Ok(result)
    }

    pub fn login(&self, id: &str, cancel: &std::sync::atomic::AtomicBool) -> Result<Value> {
        let config = Configuration::load(&self.home)?;
        let settings = config::Settings::from_config(&config)?;
        let server = settings
            .resolve_servers(&self.project)?
            .into_iter()
            .find(|s| s.id == id && s.approved && s.definition.enabled)
            .ok_or_else(|| {
                Error::Denied("MCP server must be enabled and approved before login".into())
            })?;
        if !matches!(server.definition.auth, config::Auth::Oauth { .. }) {
            return Err(Error::invalid("Server does not use OAuth"));
        }
        let (login, url) = self.runtime.wait(
            settings.timeout_seconds,
            cancel,
            auth::begin(self.home.clone(), self.project.clone(), server, settings),
        )?;
        panic::lock(&self.logins).insert(id.into(), login);
        Ok(json!({"server_id":id,"status":"auth-pending","authorization_url":url,"expires_in":300}))
    }

    pub fn login_status(&self, id: &str) -> Value {
        panic::lock(&self.logins)
            .get(id)
            .map(|login| panic::lock(&login.status).clone())
            .unwrap_or_else(|| json!({"status":"no-pending-login"}))
    }

    pub fn disconnect(&self, id: &str, registry: &crate::tools::Registry) {
        registry.remove_server(&mcp::tool_prefix(id));
        panic::lock(&self.connections).remove(id);
    }

    pub fn connection(&self, id: &str) -> Result<Arc<mcp::Connection>> {
        let connection = panic::lock(&self.connections)
            .get(id)
            .cloned()
            .ok_or_else(|| Error::tool("MCP server is not connected; use Rune's MCP controls"))?;
        let settings = config::Settings::from_config(&Configuration::load(&self.home)?)?;
        let current = settings
            .resolve_servers(&self.project)?
            .into_iter()
            .find(|s| s.id == id);
        if !settings.enabled
            || current.is_none_or(|s| {
                !s.definition.enabled || !s.approved || s.digest != connection.definition.digest
            })
        {
            return Err(Error::Denied(
                "MCP definition changed or was revoked; reconnect through Rune".into(),
            ));
        }
        Ok(connection)
    }

    /// The connection to `id`, opened again first if it dropped. A connection
    /// is stopped whenever its state became unknown — a timeout, a cut stream,
    /// a cancelled call — and until now stayed down for the rest of the turn:
    /// every later call failed "disconnected" while discovery still listed the
    /// server's tools. The authority checks are [`Self::connection`]'s, before
    /// and after.
    pub fn live(
        &self,
        id: &str,
        registry: &crate::tools::Registry,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Arc<mcp::Connection>> {
        let connection = self.connection(id)?;
        if connection.ready() {
            return Ok(connection);
        }
        drop(connection);
        self.connect(id, registry, cancel)?;
        self.connection(id)
    }

    pub fn server_status(&self) -> Vec<Value> {
        let connections = panic::lock(&self.connections);
        match Configuration::load(&self.home).and_then(|c|config::Settings::from_config(&c)).and_then(|c|c.resolve_servers(&self.project)){
            Ok(servers)=>servers.into_iter().map(|s|json!({"id":s.id,"source":s.source,"status":if !s.definition.enabled{"disabled"}else if !s.approved{"untrusted"}else if connections.get(&s.id).is_some_and(|c|c.ready()){"ready"}else{"disconnected"}})).collect(),
            Err(_)=>vec![json!({"status":"invalid-config"})],
        }
    }

    /// The MCP servers a model can use now — enabled and approved — each with
    /// whether it is connected, as `<status>` tells them: `id (ready)`. Without
    /// it a model never knew whether any server existed, and searched for
    /// tools no server offered (session 550: seven searches for desktop
    /// automation, then a resource read from a server it named `none`).
    pub fn offered_servers(&self) -> Vec<String> {
        let Ok(settings) =
            Configuration::load(&self.home).and_then(|c| config::Settings::from_config(&c))
        else {
            return Vec::new();
        };
        if !settings.enabled {
            return Vec::new();
        }
        let Ok(servers) = settings.resolve_servers(&self.project) else {
            return Vec::new();
        };
        let connections = panic::lock(&self.connections);
        servers
            .into_iter()
            .filter(|s| s.approved && s.definition.enabled)
            .map(|s| {
                let state = if connections.get(&s.id).is_some_and(|c| c.ready()) {
                    "ready"
                } else {
                    "disconnected"
                };
                format!("{} ({state})", s.id)
            })
            .collect()
    }

    pub fn resource_entries(&self) -> Vec<Value> {
        let connections = panic::lock(&self.connections);
        let mut entries = Vec::new();
        for (id, c) in connections.iter() {
            for kind in ["resources", "templates"] {
                for resource in c.catalog[kind].as_array().into_iter().flatten() {
                    let uri = resource["uri"]
                        .as_str()
                        .or(resource["uriTemplate"].as_str())
                        .unwrap_or("");
                    entries.push(json!({"id":format!("resource:{id}:{}",&crate::checkpoint::objects::hash_of(uri.as_bytes())[..16]),"kind":"resource","source":"mcp","scope":"project","server_id":id,"name":resource["name"],"description":resource["description"],"uri":resource["uri"],"uri_template":resource["uriTemplate"],"ready":c.ready()}));
                }
            }
        }
        entries
    }

    pub fn outcome(
        &self,
        server: &str,
        mut result: Value,
        summary: String,
    ) -> Result<crate::tools::Outcome> {
        let mut media = Vec::new();
        for key in ["content", "contents"] {
            if let Some(blocks) = result[key].as_array_mut() {
                for block in blocks {
                    let encoded = block
                        .get("data")
                        .or_else(|| block.get("blob"))
                        .and_then(Value::as_str);
                    let Some(encoded) = encoded else {
                        continue;
                    };
                    if encoded.len() > self.settings.max_message_bytes {
                        return Err(Error::tool("MCP media exceeds its byte budget"));
                    }
                    let bytes = crate::media::decode(encoded)
                        .map_err(|_| Error::tool("MCP returned invalid base64 media"))?;
                    let hash = crate::checkpoint::objects::hash_of(&bytes);
                    let directory = self.state_path.with_extension("").join("Media");
                    std::fs::create_dir_all(&directory)?;
                    let path = directory.join(&hash);
                    if !path.exists() {
                        crate::settings::secrets::atomic_write(&path, &bytes)?;
                    }
                    // The type is read from the bytes, as for every attachment;
                    // the server's `mimeType` is only its claim. Refusing a claim
                    // the bytes contradicted made a sound resource unreadable (a
                    // text blob sent as `application/octet-stream`), and guarded
                    // nothing, since the claim is never what is sent on.
                    let attachment = crate::media::Attachment::at(&path)?;
                    let meta = json!({"type":"attachment","attachment":attachment});
                    *block = meta;
                    media.push(attachment);
                }
            }
        }
        let ok = !result["isError"].as_bool().unwrap_or(false);
        let mut outcome = crate::tools::Outcome::new(summary, serde_json::to_string(&result)?)
            .with_detail(
                json!({"mcp":result,"server_id":server,"media":media,"undo":"not-covered"}),
            );
        outcome.ok = ok;
        outcome.media = media;
        Ok(outcome)
    }

    pub fn skill_entries(&self, exact: bool) -> Vec<Value> {
        panic::lock(&self.inner)
            .skills
            .iter()
            .filter(|s| !s.disabled && (exact || !s.shadowed))
            .map(skills::Skill::entry)
            .collect()
    }

    /// The skills on offer, as many as fit `catalog_tokens`, each costed as the
    /// line the opening context shows for it.
    pub fn catalogue(&self) -> Catalogue {
        let inner = panic::lock(&self.inner);
        let offered: Vec<&skills::Skill> = inner
            .skills
            .iter()
            .filter(|s| !s.disabled && !s.shadowed)
            .collect();
        let mut entries = Vec::new();
        let mut used = 0;
        for skill in &offered {
            let path = skill.root.join("SKILL.md");
            let cost = estimate_tokens(&format!(
                "- {}: {} (read {})",
                skill.id,
                skill.description,
                path.display()
            ));
            if used + cost > self.settings.catalog_tokens {
                break;
            }
            used += cost;
            entries.push(CatalogueEntry {
                id: skill.id.clone(),
                description: skill.description.clone(),
                path,
            });
        }
        Catalogue {
            total: offered.len(),
            entries,
        }
    }

    pub fn package_root(&self, path: &str) -> Option<PathBuf> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return None;
        }
        let media = self.state_path.with_extension("").join("Media");
        if path.starts_with(&media) {
            return Some(media);
        }
        panic::lock(&self.inner)
            .skills
            .iter()
            .filter(|s| !s.disabled)
            .find(|s| path.starts_with(&s.root))
            .map(|s| s.root.clone())
    }

    /// The build's own text for a file of the official help: whatever its
    /// copy on disk holds now, a reader gets the compiled text, and a copy
    /// that differs is replaced for the next one. A path inside the official
    /// package that is no file of it (an editor's swap file) is not read.
    pub(crate) fn official_text(&self, path: &Path) -> Result<Option<&'static str>> {
        let root = {
            let inner = panic::lock(&self.inner);
            match inner
                .skills
                .iter()
                .find(|s| s.id == product::ID && path.starts_with(&s.root))
            {
                Some(skill) => skill.root.clone(),
                None => return Ok(None),
            }
        };
        let name = path
            .strip_prefix(&root)
            .expect("matched package")
            .components()
            .map(|p| p.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let Some(text) = product::document(&name) else {
            return Err(Error::NotFound(format!(
                "{name} is not a file of Rune's official help — its topics are listed in SKILL.md"
            )));
        };
        if std::fs::read(path).ok().as_deref() != Some(text.as_bytes()) {
            let _ = product::ensure(&self.home);
        }
        Ok(Some(text))
    }

    pub fn package_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<_> = panic::lock(&self.inner)
            .skills
            .iter()
            .filter(|s| !s.disabled)
            .map(|s| s.root.clone())
            .collect();
        let media = self.state_path.with_extension("").join("Media");
        if media.is_dir() {
            roots.push(media);
        }
        roots
    }

    pub fn validate_active_packages(&self) -> Result<()> {
        let inner = panic::lock(&self.inner);
        for (id, revision) in &inner.loaded {
            let Some(skill) = inner.skills.iter().find(|s| &s.id == id) else {
                continue;
            };
            if skills::package_revision(&skill.root, &self.settings)? != *revision {
                return Err(Error::Denied(format!(
                    "Skill {id} changed after its SKILL.md was read; read it again before running commands"
                )));
            }
        }
        Ok(())
    }

    pub fn close(&self, registry: &crate::tools::Registry) {
        let ids: Vec<_> = panic::lock(&self.connections).keys().cloned().collect();
        for id in ids {
            self.disconnect(&id, registry);
        }
        panic::lock(&self.logins).clear();
    }

    pub fn remove_session_artifacts(&self, registry: &crate::tools::Registry) -> Result<()> {
        self.close(registry);
        for result in [
            std::fs::remove_file(&self.state_path),
            std::fs::remove_dir_all(self.state_path.with_extension("")),
        ] {
            if let Err(error) = result {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    /// The bytes of `path` when it names a registered skill's `SKILL.md`, read
    /// only if they are still the revision that was registered — the text a
    /// skill instructs with is the text Rune vetted. A whole read is recorded
    /// (see [`Inner::loaded`]). `None` for any other path.
    pub fn read_skill(&self, path: &str, whole: bool) -> Result<Option<Vec<u8>>> {
        let path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.project.join(path)
        };
        let path = crate::jail::normalize(&path);
        let skill = {
            let inner = panic::lock(&self.inner);
            let Some(skill) = inner
                .skills
                .iter()
                .find(|s| s.root.join("SKILL.md") == path)
                .cloned()
            else {
                return Ok(None);
            };
            skill
        };
        let text = skill.instructions(&self.settings)?;
        if whole {
            self.record_loaded(&skill)?;
        }
        Ok(Some(text.into_bytes()))
    }

    /// Note that the model now holds `skill`'s instructions.
    pub fn record_loaded(&self, skill: &skills::Skill) -> Result<()> {
        let mut inner = panic::lock(&self.inner);
        if inner.loaded.get(&skill.id) == Some(&skill.revision) {
            return Ok(());
        }
        inner
            .loaded
            .insert(skill.id.clone(), skill.revision.clone());
        self.save_loaded(&inner.loaded)
    }

    fn save_loaded(&self, loaded: &BTreeMap<String, String>) -> Result<()> {
        if loaded.is_empty() && !self.state_path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::settings::secrets::atomic_write(
            &self.state_path,
            &crate::store::jsonl::pretty(loaded)?,
        )
    }

    pub fn select_skill(&self, id: &str) -> Result<skills::Skill> {
        let inner = panic::lock(&self.inner);
        let exact = inner.skills.iter().find(|s| s.id == id);
        exact
            .or_else(|| {
                inner
                    .skills
                    .iter()
                    .find(|s| s.name == id && !s.shadowed && !s.disabled)
            })
            .cloned()
            .ok_or_else(|| Error::NotFound("skill".into()))
    }

    pub fn deactivate(&self, id: &str) -> Result<()> {
        let mut inner = panic::lock(&self.inner);
        if id == "all" {
            inner.loaded.clear();
        } else {
            inner.loaded.remove(id);
        }
        self.save_loaded(&inner.loaded)
    }
}
