//! Host-only management shared by the CLI and engine protocol; no model-side grants.
use super::{
    config::{Project, Server, Settings, Transport},
    Capabilities,
};
use crate::settings::configuration::{read_bounded, Configuration};
use crate::{Error, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub group: String,
    pub operation: String,
    #[serde(default)]
    pub arguments: Vec<String>,
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityRequest")
            .field("group", &self.group)
            .field("operation", &self.operation)
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

pub fn execute(
    cap: &Capabilities,
    registry: &crate::tools::Registry,
    request: &Request,
    cancel: &AtomicBool,
) -> Result<Value> {
    if request.arguments.len() > 128
        || request
            .arguments
            .iter()
            .any(|s| s.len() > 65536 || s.contains('\0'))
    {
        return Err(Error::invalid("Capability command is too large"));
    }
    let mut arguments = request.arguments.clone();
    let mut explicit_scope = false;
    let mut scope = if request.group == "skills" {
        "project"
    } else {
        "private-project"
    }
    .to_owned();
    if let Some(i) = arguments
        .iter()
        .take_while(|s| s.as_str() != "--")
        .position(|s| s == "--scope")
    {
        if i + 1 >= arguments.len() {
            return Err(Error::invalid(
                "--scope needs user, project or private-project",
            ));
        }
        explicit_scope = true;
        scope = arguments.remove(i + 1);
        arguments.remove(i);
    }
    if !["user", "project", "private-project"].contains(&scope.as_str()) {
        return Err(Error::invalid("Unknown capability scope"));
    }
    let op = request.operation.as_str();
    let group = request.group.as_str();
    let id = arguments.first().map(String::as_str).unwrap_or("");
    if !["skills", "mcp"].contains(&group) {
        return Err(Error::invalid("Unknown capability group"));
    }
    let (minimum, maximum) = match (group, op) {
        (_, "list" | "status" | "validate" | "reload") => (0, 0),
        ("mcp", "add") => (2, 128),
        ("mcp", "prompt") => (2, 3),
        ("mcp", "token") => (2, 2),
        ("mcp", "secret") => (3, 3),
        _ => (1, 1),
    };
    if !(minimum..=maximum).contains(&arguments.len()) {
        return Err(Error::invalid(
            "Incorrect number of capability command arguments",
        ));
    }
    if explicit_scope
        && !matches!(
            (group, op),
            ("mcp", "add" | "import" | "remove" | "enable" | "disable")
                | ("skills", "install" | "update")
        )
    {
        return Err(Error::invalid(
            "Scope selects installation or server configuration location for this command",
        ));
    }
    if group == "skills" && scope == "private-project" {
        return Err(Error::invalid(
            "Skill packages support user or project scope",
        ));
    }
    if op == "reload" {
        if group == "skills" {
            cap.refresh()?;
            return Ok(cap.inventory());
        }
        cap.input
            .changed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let errors = cap.connect_enabled(registry, cancel)?;
        return Ok(json!({"servers":cap.server_status(),"errors":errors}));
    }

    if group == "skills" {
        match op {
            "list" => return Ok(cap.inventory()),
            "show" => {
                let skill = cap.select_skill(id)?;
                return Ok(
                    json!({"skill":skill.entry(),"content":String::from_utf8(read_bounded(&skill.root.join("SKILL.md"),cap.settings.max_skill_bytes as u64)?).map_err(|_|Error::invalid("Skill must be UTF-8"))?}),
                );
            }
            "activate" => {
                let skill = cap.select_skill(id)?;
                let content = cap
                    .read_skill(&skill.root.join("SKILL.md").to_string_lossy(), true)?
                    .ok_or_else(|| Error::NotFound("skill".into()))?;
                let content = String::from_utf8(content)
                    .map_err(|_| Error::invalid("Skill must be UTF-8"))?;
                return Ok(json!({"skill":skill.id,"content":content}));
            }
            "deactivate" => {
                cap.deactivate(id)?;
                return Ok(json!({"deactivated":id}));
            }
            _ => {}
        }
    } else if group == "mcp" {
        match op {
            "list" | "status" => return Ok(json!({"servers":cap.server_status()})),
            "validate" => {
                let cfg = Configuration::load(&cap.home)?;
                Settings::from_config(&cfg)?.resolve_servers(&cap.project)?;
                return Ok(json!({"valid":true,"connected":false}));
            }
            "connect" | "test" => return cap.connect(id, registry, cancel),
            "login" => return cap.login(id, cancel),
            "login-status" => return Ok(cap.login_status(id)),
            "disconnect" => {
                cap.disconnect(id, registry);
                return Ok(json!({"server_id":id,"status":"disconnected"}));
            }
            "prompts" => {
                let c = cap.connection(id)?;
                return Ok(json!({"server_id":id,"prompts":c.catalog["prompts"]}));
            }
            "prompt" => {
                if arguments.len() < 2 {
                    return Err(Error::invalid("prompt requires server ID and prompt name"));
                }
                let args = arguments
                    .get(2)
                    .map(|s| serde_json::from_str(s))
                    .transpose()?
                    .unwrap_or(json!({}));
                return cap
                    .connection(id)?
                    .request("prompt", &arguments[1], args, cancel);
            }
            "token" | "secret" => {
                let cfg = Settings::from_config(&Configuration::load(&cap.home)?)?;
                let server = cfg
                    .resolve_servers(&cap.project)?
                    .into_iter()
                    .find(|s| s.id == id && s.approved)
                    .ok_or_else(|| Error::Denied("MCP server requires approval first".into()))?;
                let (label, index) = if op == "secret" {
                    let label = arguments.get(1).ok_or_else(|| {
                        Error::invalid("secret requires server ID, label and value")
                    })?;
                    super::config::check_id(label)?;
                    if !server
                        .definition
                        .environment
                        .values()
                        .any(|v| v == &format!("@secret:{label}"))
                    {
                        return Err(Error::invalid(
                            "Secret label is not referenced by this MCP server",
                        ));
                    }
                    (label.as_str(), 2)
                } else {
                    let super::config::Auth::Bearer { secret } = &server.definition.auth else {
                        return Err(Error::invalid(
                            "Server is not configured for bearer authentication",
                        ));
                    };
                    (secret.as_str(), 1)
                };
                let value = arguments
                    .get(index)
                    .filter(|v| {
                        !v.is_empty() && v.len() <= 8192 && !v.chars().any(char::is_control)
                    })
                    .ok_or_else(|| Error::invalid("Missing or invalid MCP credential"))?;
                let mut secrets = crate::settings::secrets::Secrets::load(&cap.home)?;
                secrets.set(
                    &super::mcp::credential_key(&cap.project, &server, label),
                    value,
                )?;
                return Ok(json!({"server_id":id,"credential_stored":true}));
            }
            _ => {}
        }
    } else {
        return Err(Error::invalid("Unknown capability group"));
    }

    let config = Configuration::load(&cap.home)?;
    let mut settings = Settings::from_config(&config)?;
    let key = super::config::project_key(&cap.project);
    if group == "skills" && ["install", "update"].contains(&op) {
        let root = if scope == "user" {
            cap.home.join("skills")
        } else {
            crate::jail::Jail::new(&cap.project)?.resolve(".rune/skills")?
        };
        let source = PathBuf::from(id);
        let result = super::skills::install(&source, &root, &settings, op == "update")?;
        cap.refresh()?;
        return Ok(result);
    }
    if group == "skills" {
        let skill = cap.select_skill(id)?;
        let project = settings.projects.entry(key).or_insert_with(|| Project {
            root: cap.project.clone(),
            ..Project::default()
        });
        match op {
            "disable"=>{if !project.disabled_skills.contains(&skill.id){project.disabled_skills.push(skill.id.clone());}},
            "enable"=>project.disabled_skills.retain(|s|s!=&skill.id),
            _=>return Err(Error::invalid("Skills commands: list, show, enable, disable, install, activate, deactivate, reload")),
        }
    } else {
        if op == "import" {
            let imported = import_servers(id, settings.max_message_bytes)?;
            if scope == "project" {
                return edit_shared(cap, &settings, op, &arguments);
            }
            let servers = if scope == "user" {
                &mut settings.servers
            } else {
                &mut settings
                    .projects
                    .entry(key)
                    .or_insert_with(|| Project {
                        root: cap.project.clone(),
                        ..Project::default()
                    })
                    .servers
            };
            if imported.keys().any(|id| servers.contains_key(id)) {
                return Err(Error::invalid(
                    "Import would overwrite an existing MCP server",
                ));
            }
            servers.extend(imported);
            Configuration::edit(
                &cap.home,
                Some("capabilities"),
                Some(owned_by_management(config.get("capabilities"), &settings)?),
                false,
                Some(config.get("capabilities")),
            )?;
            return Ok(json!({"imported":true,"scope":scope}));
        }
        super::config::check_id(id)?;
        if op == "approve" {
            let resolved = settings
                .resolve_servers(&cap.project)?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| Error::NotFound("MCP server".into()))?;
            settings
                .projects
                .entry(key)
                .or_insert_with(|| Project {
                    root: cap.project.clone(),
                    ..Project::default()
                })
                .approved_servers
                .insert(id.into(), resolved.digest);
        } else {
            if scope == "project" {
                return edit_shared(cap, &settings, op, &arguments);
            }
            let servers = if scope == "user" {
                &mut settings.servers
            } else {
                &mut settings
                    .projects
                    .entry(key)
                    .or_insert_with(|| Project {
                        root: cap.project.clone(),
                        ..Project::default()
                    })
                    .servers
            };
            match op {
                "add"=>{let server=parse_server(&arguments)?;servers.insert(id.into(),server);},
                "remove"=>{servers.remove(id);},
                "enable"|"disable"=>{let server=servers.get_mut(id).ok_or_else(||Error::NotFound("MCP server in selected scope".into()))?;server.enabled=op=="enable";},
                _=>return Err(Error::invalid("MCP commands: list, status, add, approve, remove, enable, disable, validate, connect, disconnect, token, prompts, prompt")),
            }
        }
    }
    settings
        .projects
        .retain(|_, project| !project.holds_nothing());
    Configuration::edit(
        &cap.home,
        Some("capabilities"),
        Some(owned_by_management(config.get("capabilities"), &settings)?),
        false,
        Some(config.get("capabilities")),
    )?;
    if group == "mcp" {
        cap.disconnect(id, registry);
    }
    Ok(json!({"updated":id,"scope":scope,"reload_required":true}))
}

/// The `capabilities` table to write: the owner's, as written, with only what
/// these commands manage — `servers` and `projects` — taken from `settings`.
/// Writing `settings` whole erased every switch and limit the owner had set
/// beside them, since those are not written out.
fn owned_by_management(current: &Value, settings: &Settings) -> Result<Value> {
    let mut table = current.as_object().cloned().unwrap_or_default();
    let written = serde_json::to_value(settings)?;
    for key in ["servers", "projects"] {
        match written.get(key) {
            Some(value) => table.insert(key.into(), value.clone()),
            None => table.remove(key),
        };
    }
    Ok(Value::Object(table))
}

fn parse_server(args: &[String]) -> Result<Server> {
    let target = args
        .get(1)
        .ok_or_else(|| Error::invalid("add requires an ID and HTTPS URL or absolute executable"))?;
    if target.starts_with('{') {
        if args.len() != 2 {
            return Err(Error::invalid(
                "A JSON server definition must be one complete argument",
            ));
        }
        let server: Server = serde_json::from_str(target)?;
        server.check(&args[0])?;
        return Ok(server);
    }
    let mut server = Server {
        enabled: true,
        ..Server::default()
    };
    if target.starts_with("https://") || target.starts_with("http://") {
        server.url = Some(target.clone());
        if args.len() > 2 {
            return Err(Error::invalid("HTTP add does not take process arguments"));
        }
    } else {
        server.transport = Transport::Stdio;
        server.command = Some(PathBuf::from(target));
        server.args = args[if args.get(2).is_some_and(|s| s == "--") {
            3
        } else {
            2
        }..]
            .to_vec();
    }
    server.check(&args[0])?;
    Ok(server)
}

fn edit_shared(
    cap: &Capabilities,
    settings: &Settings,
    op: &str,
    args: &[String],
) -> Result<Value> {
    let path = crate::jail::Jail::new(&cap.project)?.resolve(".rune/Mcp.toml")?;
    let mut data: Value = match read_bounded(&path, settings.max_message_bytes as u64) {
        Ok(bytes) => toml_edit::de::from_str(
            std::str::from_utf8(&bytes).map_err(|_| Error::invalid("Mcp.toml must be UTF-8"))?,
        )
        .map_err(|_| Error::invalid("Invalid Mcp.toml"))?,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => json!({"servers":{}}),
        Err(e) => return Err(e),
    };
    let servers = data
        .get_mut("servers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::invalid("Mcp.toml requires a servers table"))?;
    let id = &args[0];
    match op {
        "import" => {
            let imported = import_servers(&args[0], settings.max_message_bytes)?;
            if imported.keys().any(|id| servers.contains_key(id)) {
                return Err(Error::invalid(
                    "Import would overwrite an existing project MCP server",
                ));
            }
            for (id, server) in imported {
                servers.insert(id, serde_json::to_value(server)?);
            }
        }
        "add" => {
            servers.insert(id.clone(), serde_json::to_value(parse_server(args)?)?);
        }
        "remove" => {
            servers.remove(id);
        }
        "enable" | "disable" => {
            servers
                .get_mut(id)
                .ok_or_else(|| Error::NotFound("project MCP server".into()))?["enabled"] =
                json!(op == "enable");
        }
        _ => return Err(Error::invalid("Unsupported shared MCP operation")),
    }
    std::fs::create_dir_all(path.parent().expect("MCP directory"))?;
    let text = toml_edit::ser::to_string_pretty(&data)
        .map_err(|_| Error::invalid("Cannot encode MCP configuration"))?;
    crate::settings::secrets::atomic_write(&path, text.as_bytes())?;
    Ok(json!({"updated":id,"scope":"project","approval_required":true}))
}

fn import_servers(path: &str, max: usize) -> Result<std::collections::BTreeMap<String, Server>> {
    let data: Value =
        serde_json::from_slice(&read_bounded(std::path::Path::new(path), max as u64)?)?;
    let map = data
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::invalid("MCP import requires an mcpServers object"))?;
    if map.len() > 128 {
        return Err(Error::invalid("Too many imported MCP servers"));
    }
    let mut servers = std::collections::BTreeMap::new();
    for (id, value) in map {
        let entry = value
            .as_object()
            .ok_or_else(|| Error::invalid("Invalid MCP import entry"))?;
        if entry.keys().any(|k| {
            ![
                "type", "url", "command", "args", "env", "disabled", "headers",
            ]
            .contains(&k.as_str())
        }) {
            return Err(Error::invalid("MCP import contains unsupported fields"));
        }
        if entry
            .get("headers")
            .is_some_and(|h| h.as_object().is_none_or(|m| !m.is_empty()))
        {
            return Err(Error::invalid(
                "Configure imported HTTP credentials through Rune's scoped authentication settings",
            ));
        }
        let mut server = Server {
            enabled: !entry
                .get("disabled")
                .map(|v| {
                    v.as_bool()
                        .ok_or_else(|| Error::invalid("disabled must be boolean"))
                })
                .transpose()?
                .unwrap_or(false),
            ..Default::default()
        };
        if let Some(url) = entry.get("url") {
            server.url = Some(
                url.as_str()
                    .ok_or_else(|| Error::invalid("MCP URL must be text"))?
                    .into(),
            );
        }
        if let Some(command) = entry.get("command") {
            server.transport = Transport::Stdio;
            let command = command
                .as_str()
                .ok_or_else(|| Error::invalid("MCP command must be text"))?;
            let direct = PathBuf::from(command);
            server.command = Some(if direct.is_absolute() {
                direct
            } else {
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .map(|p| p.join(command))
                    .find(|p| p.is_file())
                    .ok_or_else(|| {
                        Error::invalid("Imported MCP executable was not found; install it first")
                    })?
            });
        }
        if let Some(kind) = entry.get("type") {
            let kind = kind
                .as_str()
                .ok_or_else(|| Error::invalid("MCP type must be text"))?;
            if !["stdio", "http"].contains(&kind)
                || (kind == "stdio") != (server.transport == Transport::Stdio)
            {
                return Err(Error::invalid("Unsupported or inconsistent MCP transport"));
            }
        }
        if let Some(args) = entry.get("args") {
            server.args = serde_json::from_value(args.clone())?;
        }
        if let Some(env) = entry.get("env") {
            server.environment = serde_json::from_value(env.clone())?;
        }
        server.check(id)?;
        servers.insert(id.clone(), server);
    }
    Ok(servers)
}
