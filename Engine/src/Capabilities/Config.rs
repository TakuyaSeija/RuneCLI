//! User-owned capability configuration and whole-entry scope resolution.
use crate::checkpoint::objects::hash_of;
use crate::settings::configuration::Configuration;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}
fn default_page_size() -> usize {
    12
}
fn default_catalog_tokens() -> usize {
    2048
}
fn default_max_entries() -> usize {
    2000
}
fn default_max_skill_bytes() -> usize {
    65536
}
fn default_max_package_bytes() -> usize {
    16 * 1024 * 1024
}
fn default_max_message_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_timeout_seconds() -> u64 {
    60
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_true", skip_serializing)]
    pub enabled: bool,
    #[serde(default, skip_serializing)]
    pub portable_user_skills: Option<bool>,
    #[serde(default, skip_serializing)]
    pub external_skills_compatibility: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub product_skill_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, Server>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, Project>,
    #[serde(default = "default_page_size", skip_serializing)]
    pub page_size: usize,
    #[serde(default = "default_catalog_tokens", skip_serializing)]
    pub catalog_tokens: usize,
    #[serde(default = "default_max_entries", skip_serializing)]
    pub max_entries: usize,
    #[serde(default = "default_max_skill_bytes", skip_serializing)]
    pub max_skill_bytes: usize,
    #[serde(default = "default_max_package_bytes", skip_serializing)]
    pub max_package_bytes: usize,
    #[serde(default = "default_max_message_bytes", skip_serializing)]
    pub max_message_bytes: usize,
    #[serde(default = "default_timeout_seconds", skip_serializing)]
    pub timeout_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            portable_user_skills: None,
            external_skills_compatibility: false,
            skill_roots: Vec::new(),
            product_skill_roots: Vec::new(),
            servers: BTreeMap::new(),
            projects: BTreeMap::new(),
            page_size: 12,
            catalog_tokens: 2048,
            max_entries: 2000,
            max_skill_bytes: 65536,
            max_package_bytes: 16 * 1024 * 1024,
            max_message_bytes: 8 * 1024 * 1024,
            timeout_seconds: 60,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Project {
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, Server>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub approved_servers: BTreeMap<String, String>,
    #[serde(default, skip_serializing)]
    pub approved_skills: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_roots: Vec<PathBuf>,
}

impl Project {
    /// Whether this entry says nothing but where the project is: what is left
    /// after its last server is removed or its last disabled skill enabled,
    /// and is not kept (`rune.toml` collected one such stub per project).
    pub fn holds_nothing(&self) -> bool {
        self.servers.is_empty()
            && self.approved_servers.is_empty()
            && self.disabled_skills.is_empty()
            && self.skill_roots.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub enabled: bool,
    pub transport: Transport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub environment: BTreeMap<String, String>,
    pub auth: Auth,
    pub project_write: bool,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub allow_host_sockets: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: Transport::Http,
            command: None,
            args: Vec::new(),
            url: None,
            environment: BTreeMap::new(),
            auth: Auth::None,
            project_write: false,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            allow_host_sockets: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    Http,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    Bearer {
        secret: String,
    },
    Oauth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        allowed_origins: Vec<String>,
    },
}

#[derive(Clone)]
pub struct ResolvedServer {
    pub id: String,
    pub definition: Server,
    pub source: String,
    pub digest: String,
    pub approved: bool,
}

impl Settings {
    pub fn from_config(config: &Configuration) -> Result<Self> {
        let value = config.get("capabilities");
        let settings: Self = serde_json::from_value(if value.is_null() {
            serde_json::json!({})
        } else {
            value.clone()
        })?;
        settings.check()?;
        Ok(settings)
    }

    pub fn check(&self) -> Result<()> {
        for (value, min, max) in [
            (self.page_size, 1, 100),
            (self.catalog_tokens, 128, 32768),
            (self.max_entries, 1, 10000),
            (self.max_skill_bytes, 1024, 1024 * 1024),
            (self.max_package_bytes, 1024, 128 * 1024 * 1024),
            (self.max_message_bytes, 1024, 64 * 1024 * 1024),
            (self.timeout_seconds as usize, 1, 900),
        ] {
            if !(min..=max).contains(&value) {
                return Err(Error::invalid(
                    "Capability setting is outside its supported range",
                ));
            }
        }
        if self.servers.len() > 128 || self.projects.len() > 10000 {
            return Err(Error::invalid("Too many capability configuration entries"));
        }
        check_roots(&self.skill_roots)?;
        check_roots(&self.product_skill_roots)?;
        for (id, server) in &self.servers {
            server.check(id)?;
        }
        for project in self.projects.values() {
            if !project.root.is_absolute() {
                continue;
            }
            check_roots(&project.skill_roots)?;
            if project.servers.len() > 128 || project.disabled_skills.len() > self.max_entries {
                return Err(Error::invalid("Too many project capability entries"));
            }
            for (id, server) in &project.servers {
                server.check(id)?;
            }
            for (id, digest) in &project.approved_servers {
                check_id(id)?;
                check_digest(digest)?;
            }
        }
        Ok(())
    }

    pub fn project(&self, root: &Path) -> Option<&Project> {
        let expected = project_key(root);
        let canon = crate::platform::canonical(root).ok();
        self.projects
            .get(&expected)
            .or_else(|| {
                self.projects.values().find(|p| {
                    p.root == root
                        || canon.as_deref().is_some_and(|c| {
                            crate::platform::canonical(&p.root).ok().as_deref() == Some(c)
                        })
                })
            })
            .filter(|p| {
                p.root == root
                    || canon.as_deref().is_some_and(|c| {
                        crate::platform::canonical(&p.root).ok().as_deref() == Some(c)
                    })
            })
    }

    pub fn resolve_servers(&self, root: &Path) -> Result<Vec<ResolvedServer>> {
        let mut entries: BTreeMap<String, (Server, String)> = self
            .servers
            .iter()
            .map(|(id, s)| (id.clone(), (s.clone(), "user".into())))
            .collect();
        let shared = root.join(".rune/Mcp.toml");
        match if shared.symlink_metadata().is_err() {
            Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )))
        } else {
            super::skills::read_package_file(root, &shared, self.max_message_bytes)
        } {
            Ok(bytes) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Shared {
                    servers: BTreeMap<String, Server>,
                }
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| Error::invalid("Mcp.toml must be UTF-8"))?;
                let shared: Shared = toml_edit::de::from_str(text)
                    .map_err(|_| Error::invalid("Invalid project Mcp.toml"))?;
                if shared.servers.len() > 128 {
                    return Err(Error::invalid("Too many project MCP servers"));
                }
                for (id, s) in shared.servers {
                    s.check(&id)?;
                    entries.insert(id, (s, "project".into()));
                }
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(project) = self.project(root) {
            for (id, s) in &project.servers {
                entries.insert(id.clone(), (s.clone(), "private-project".into()));
            }
        }
        entries
            .into_iter()
            .map(|(id, (definition, source))| {
                let digest = hash_of(serde_json::to_string(&definition)?.as_bytes());
                let approved = source != "project"
                    || self
                        .project(root)
                        .is_some_and(|p| p.approved_servers.get(&id) == Some(&digest));
                Ok(ResolvedServer {
                    id,
                    definition,
                    source,
                    digest,
                    approved,
                })
            })
            .collect()
    }
}

impl Server {
    pub fn check(&self, id: &str) -> Result<()> {
        check_id(id)?;
        if self.args.len() > 128 || self.environment.len() > 64 {
            return Err(Error::invalid("MCP launch configuration is too large"));
        }
        for value in self.args.iter().chain(self.environment.values()) {
            if value.len() > 8192 || value.contains('\0') {
                return Err(Error::invalid("Invalid MCP launch argument"));
            }
        }
        for key in self.environment.keys() {
            if key.is_empty()
                || key.len() > 128
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(Error::invalid("Invalid MCP environment name"));
            }
        }
        check_roots(&self.read_roots)?;
        check_roots(&self.write_roots)?;
        if !self.enabled && self.command.is_none() && self.url.is_none() {
            return Ok(());
        }
        match self.transport {
            Transport::Stdio => {
                if self.command.as_ref().is_none_or(|p| !p.is_absolute()) || self.url.is_some() {
                    return Err(Error::invalid(
                        "Stdio MCP requires an absolute executable and no URL",
                    ));
                }
            }
            Transport::Http => {
                check_endpoint(
                    self.url
                        .as_deref()
                        .ok_or_else(|| Error::invalid("HTTP MCP requires a URL"))?,
                )?;
                if self.command.is_some() || !self.args.is_empty() || !self.environment.is_empty() {
                    return Err(Error::invalid("HTTP MCP does not accept process arguments"));
                }
            }
        }
        match &self.auth {
            Auth::None => {}
            Auth::Bearer { secret } => {
                check_id(secret)?;
                if self.transport != Transport::Http {
                    return Err(Error::invalid("Bearer authentication requires HTTP"));
                }
            }
            Auth::Oauth {
                client_id,
                scopes,
                allowed_origins,
            } => {
                if self.transport != Transport::Http
                    || client_id
                        .as_ref()
                        .is_some_and(|s| s.is_empty() || s.len() > 2048)
                    || scopes.len() > 64
                    || allowed_origins.len() > 16
                {
                    return Err(Error::invalid("Invalid OAuth configuration"));
                }
                for scope in scopes {
                    if scope.is_empty()
                        || scope.len() > 256
                        || scope.chars().any(char::is_whitespace)
                    {
                        return Err(Error::invalid("Invalid OAuth scope"));
                    }
                }
                for origin in allowed_origins {
                    check_endpoint(origin)?;
                }
            }
        }
        Ok(())
    }
}

pub use crate::project::project_key;

pub fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::invalid(
            "Capability ID must contain 1–48 letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}

pub fn check_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    if endpoint.len() > 4096 {
        return Err(Error::invalid("MCP endpoint is too long"));
    }
    let url =
        reqwest::Url::parse(endpoint).map_err(|_| Error::invalid("Invalid MCP endpoint URL"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(Error::invalid("MCP endpoints require HTTPS (HTTP only on loopback), without credentials, query or fragment"));
    }
    Ok(url)
}

fn check_roots(roots: &[PathBuf]) -> Result<()> {
    if roots.len() > 64
        || roots.iter().any(|p| {
            !p.is_absolute()
                || p.as_os_str().len() > 4096
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        })
    {
        return Err(Error::invalid(
            "Capability roots must be bounded absolute paths without parent traversal",
        ));
    }
    Ok(())
}

fn check_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid("Invalid capability approval digest"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the owner sets under `[capabilities]` is what the engine runs on.
    #[test]
    fn the_owners_capability_limits_are_read_as_written() {
        let home = std::env::temp_dir().join(format!("rune-caps-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[capabilities]\ntimeout_seconds = 7\ncatalog_tokens = 4096\nexternal_skills_compatibility = true\n",
        )
        .unwrap();
        let settings = Settings::from_config(&Configuration::load(&home).unwrap()).unwrap();
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(settings.timeout_seconds, 7);
        assert_eq!(settings.catalog_tokens, 4096);
        assert!(settings.external_skills_compatibility);
    }
}
