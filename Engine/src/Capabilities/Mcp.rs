//! MCP connections owned by Rune; protocol semantics are provided by rmcp.
use super::config::{Auth, ResolvedServer, Settings, Transport};
use crate::{panic, Error, Result};
use rmcp::model::ProtocolVersion;
use rmcp::service::{ClientLifecycleMode, RunningService};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use rmcp::{ClientServiceExt, RoleClient};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

pub struct Runtime {
    inner: tokio::runtime::Runtime,
}

impl Runtime {
    pub fn new() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .max_blocking_threads(2)
                .enable_all()
                .thread_name("RuneMcp")
                .build()?,
        }))
    }

    pub fn wait<T>(
        &self,
        seconds: u64,
        cancel: &AtomicBool,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.wait_pausing(seconds, cancel, || false, future)
    }

    /// [`Self::wait`], with a clock that stops while `paused` says so. The time
    /// a person takes to answer a server's question (an MCP elicitation) is
    /// theirs, not the server's: counted against the server's limit, an answer
    /// given after a minute timed the call out and stopped the server.
    pub fn wait_pausing<T>(
        &self,
        seconds: u64,
        cancel: &AtomicBool,
        paused: impl Fn() -> bool,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        self.inner.block_on(async {
            tokio::pin!(future);
            let limit = Duration::from_secs(seconds);
            let mut spent = Duration::ZERO;
            let mut last = tokio::time::Instant::now();
            let mut tick = tokio::time::interval(Duration::from_millis(25));
            loop {
                tokio::select! {
                    biased;
                    _ = tick.tick() => {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        let now = tokio::time::Instant::now();
                        if !paused() {
                            spent += now - last;
                        }
                        last = now;
                        if spent >= limit {
                            return Err(Error::tool("MCP request timed out; a dispatched operation may have completed"));
                        }
                    },
                    result = &mut future => return result,
                }
            }
        })
    }
}

/// What a stdio server wrote to its error stream, kept in its own folder —
/// never told to the model, whose context a server's words have no business
/// in — so a server that would not start can be diagnosed by the person (and
/// by CI). The last [`STDERR_KEEP`] bytes of the run are kept.
pub const STDERR_LOG: &str = "stderr.log";
const STDERR_KEEP: usize = 16 * 1024;

async fn keep_stderr(mut stderr: crate::platform::PipeReader, log: std::path::PathBuf) {
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        kept.extend_from_slice(&buf[..n]);
        if kept.len() > STDERR_KEEP {
            kept.drain(..kept.len() - STDERR_KEEP);
        }
        let _ = std::fs::write(&log, &kept);
    }
}

pub struct Connection {
    pub definition: ResolvedServer,
    pub catalog: Value,
    pub version: String,
    runtime: Arc<Runtime>,
    service: Mutex<Option<RunningService<RoleClient, super::input::Handler>>>,
    child: Mutex<Option<crate::platform::PipedProcess>>,
    timeout: u64,
    max_bytes: usize,
    subscription: Option<tokio::task::JoinHandle<()>>,
}

impl Connection {
    pub fn connect(
        runtime: Arc<Runtime>,
        definition: ResolvedServer,
        settings: &Settings,
        context: &super::Capabilities,
        scratch: &Path,
        cancel: &AtomicBool,
    ) -> Result<Arc<Self>> {
        let home = &context.home;
        let project = &context.project;
        let input = context.input.clone();
        if !definition.approved || !definition.definition.enabled {
            return Err(Error::Denied(
                "MCP server is disabled or needs approval in Rune".into(),
            ));
        }
        let mut child = None;
        let service=runtime.wait(settings.timeout_seconds,cancel,async {
            let info=super::input::Handler{input:input.clone(),server:definition.id.clone()};
            let lifecycle=ClientLifecycleMode::Auto{preferred_versions:vec![ProtocolVersion::V_2026_07_28],legacy_version:Some(ProtocolVersion::V_2025_11_25)};
            let service=match definition.definition.transport {
                Transport::Stdio=>{
                    let def=&definition.definition;
                    let executable=def.command.as_ref().ok_or_else(||Error::invalid("Missing MCP executable"))?;
                    let sandbox=crate::sandbox::Sandbox::for_mcp(project,scratch,home,def)?;
                    let mut launch=crate::platform::Launch::new(executable).args(&def.args).dir(project).clear_env();
                    for(key,value)in crate::sandbox::mcp_environment(scratch) { launch=launch.env(key,value); }
                    for(key,value)in &def.environment {
                        if let Some(label)=value.strip_prefix("@secret:") {
                            super::config::check_id(label)?;
                            let secrets=crate::settings::secrets::Secrets::load(home)?;
                            let name=credential_key(project,&definition,label);let value=secrets.get(&name).ok_or_else(||Error::Denied(format!("MCP process credential: {}; store it with Rune MCP controls",secrets.key_state(&name).note(label))))?;
                            launch=launch.env(key,value);
                        } else { launch=launch.env(key,value); }
                    }
                    // Its own session, held by its confinement — Unix sockets only
                    // where the person allowed them (`allow_host_sockets`, the
                    // confinement's `local_services`): a socket reaches a program
                    // the confinement does not see.
                    child=Some(crate::platform::spawn_piped(&launch,crate::platform::Placement{confinement:sandbox.confinement(),..crate::platform::Placement::default()})?);
                    let process=child.as_mut().expect("spawned child");
                    let stdout=process.take_stdout().ok_or_else(||Error::tool("MCP stdout is unavailable"))?;
                    let stdin=process.take_stdin().ok_or_else(||Error::tool("MCP stdin is unavailable"))?;
                    if let Some(stderr)=process.take_stderr(){tokio::spawn(keep_stderr(stderr,scratch.join(STDERR_LOG)));}
                    info.serve_with_lifecycle((BoundedReader{inner:stdout,seen:0,max:settings.max_message_bytes},stdin),lifecycle).await
                }
                Transport::Http=>{
                    let endpoint=definition.definition.url.as_ref().ok_or_else(||Error::invalid("Missing MCP URL"))?;
                    let client=reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(10)).read_timeout(Duration::from_secs(settings.timeout_seconds)).build().map_err(|_|Error::tool("Could not initialize MCP HTTP client"))?;
                    let client=super::http::Http{client,max:settings.max_message_bytes};
                    let mut options=StreamableHttpClientTransportConfig::with_uri(endpoint.as_str());
                    options.max_sse_event_size=settings.max_message_bytes;options.max_concurrent_requests=2;options.reinit_on_expired_session=false;
                    match &definition.definition.auth {
                        Auth::None=>{},
                        Auth::Bearer{secret}=>{
                            let secrets=crate::settings::secrets::Secrets::load(home)?;
                            let key=credential_key(project,&definition,secret);
                            options.auth_header=Some(secrets.get(&key).ok_or_else(||Error::Denied(format!("MCP authentication: {}; store the credential through Rune CLI",secrets.key_state(&key).note(secret))))?.into());
                        }
                        Auth::Oauth{..}=>{
                            let mut manager=super::auth::manager(home,project,&definition,settings).await?;
                            if !manager.initialize_from_store().await.map_err(|_|Error::Denied("MCP OAuth credentials need reauthorization".into()))?{return Err(Error::Denied("Use rune -mcp login to authorize this server".into()));}
                            let client=rmcp::transport::auth::AuthClient::new(client,manager);
                            return info.serve_with_lifecycle(StreamableHttpClientTransport::with_client(client,options),lifecycle).await.map_err(|_|Error::tool("MCP OAuth connection failed"));
                        },
                    }
                    info.serve_with_lifecycle(StreamableHttpClientTransport::with_client(client,options),lifecycle).await
                }
            }.map_err(|e|Error::tool(format!("MCP connection failed ({e}); check the endpoint, credentials and supported protocol")))?;
            Ok(service)
        });
        let service = match service {
            Ok(s) => s,
            Err(e) => {
                if let Some(mut c) = child {
                    c.stop(100);
                }
                return Err(e);
            }
        };
        let version = service
            .peer_info()
            .map(|p| p.protocol_version.to_string())
            .unwrap_or_default();
        let connection = Arc::new(Self {
            definition,
            catalog: Value::Null,
            version,
            runtime,
            service: Mutex::new(Some(service)),
            child: Mutex::new(child),
            timeout: settings.timeout_seconds,
            max_bytes: settings.max_message_bytes,
            subscription: None,
        });
        let catalog = connection.fetch_catalog(settings.max_entries, cancel)?;
        let mut connection = Arc::try_unwrap(connection)
            .map_err(|_| Error::tool("MCP connection ownership error"))?;
        connection.catalog = catalog;
        if connection.version == "2026-07-28" {
            let service = panic::lock(&connection.service);
            if let Some(service) = service.as_ref() {
                let caps = service
                    .peer_info()
                    .map(|p| serde_json::to_value(&p.capabilities).unwrap_or_default())
                    .unwrap_or_default();
                if ["tools", "resources", "prompts"]
                    .iter()
                    .any(|k| caps[k]["listChanged"] == true)
                {
                    let filter=serde_json::from_value(json!({"toolsListChanged":caps["tools"]["listChanged"]==true,"resourcesListChanged":caps["resources"]["listChanged"]==true,"promptsListChanged":caps["prompts"]["listChanged"]==true})).map_err(Error::from)?;
                    let subscription =
                        connection.runtime.wait(connection.timeout, cancel, async {
                            service
                                .listen_with_capacity(
                                    filter,
                                    std::num::NonZeroUsize::new(16).expect("nonzero channel bound"),
                                )
                                .await
                                .map_err(protocol_error)
                        })?;
                    let guard = connection.runtime.inner.enter();
                    connection.subscription = Some(tokio::spawn(async move {
                        let mut subscription = subscription;
                        while let Ok(Some(_)) = subscription.next().await {
                            input.changed.store(true, Ordering::Relaxed);
                        }
                    }));
                    drop(guard);
                }
            }
        }
        Ok(Arc::new(connection))
    }

    fn fetch_catalog(&self, max: usize, cancel: &AtomicBool) -> Result<Value> {
        let guard = panic::lock(&self.service);
        let service = guard
            .as_ref()
            .ok_or_else(|| Error::tool("MCP server disconnected"))?;
        self.runtime.wait(self.timeout, cancel, async {
            let capabilities = service
                .peer_info()
                .map(|p| p.capabilities.clone())
                .unwrap_or_default();
            let mut result = json!({"tools":[],"resources":[],"templates":[],"prompts":[]});
            let mut catalog_bytes = 0usize;
            let kinds = [
                ("tools", capabilities.tools.is_some()),
                ("resources", capabilities.resources.is_some()),
                ("templates", capabilities.resources.is_some()),
                ("prompts", capabilities.prompts.is_some()),
            ];
            for (kind, enabled) in kinds {
                if !enabled {
                    continue;
                }
                let mut cursor = None;
                let mut seen = BTreeSet::new();
                let mut entries = Vec::new();
                loop {
                    let params = cursor
                        .clone()
                        .map(|c| serde_json::from_value(json!({"cursor":c})))
                        .transpose()?;
                    let page = match kind {
                        "tools" => serde_json::to_value(
                            service.list_tools(params).await.map_err(protocol_error)?,
                        )?,
                        "resources" => serde_json::to_value(
                            service
                                .list_resources(params)
                                .await
                                .map_err(protocol_error)?,
                        )?,
                        "templates" => serde_json::to_value(
                            service
                                .list_resource_templates(params)
                                .await
                                .map_err(protocol_error)?,
                        )?,
                        _ => serde_json::to_value(
                            service.list_prompts(params).await.map_err(protocol_error)?,
                        )?,
                    };
                    catalog_bytes = catalog_bytes.saturating_add(page.to_string().len());
                    if catalog_bytes > self.max_bytes {
                        return Err(Error::tool("MCP catalog exceeds its total byte budget"));
                    }
                    let key = if kind == "templates" {
                        "resourceTemplates"
                    } else {
                        kind
                    };
                    entries.extend(page[key].as_array().cloned().unwrap_or_default());
                    if entries.len() > max || page.to_string().len() > self.max_bytes {
                        return Err(Error::tool("MCP catalog exceeds configured bounds"));
                    }
                    cursor = page["nextCursor"].as_str().map(str::to_owned);
                    let Some(c) = &cursor else {
                        break;
                    };
                    if !seen.insert(c.clone()) || seen.len() > max {
                        return Err(Error::tool("MCP pagination did not terminate"));
                    }
                }
                result[kind] = json!(entries);
            }
            if result.to_string().len() > self.max_bytes {
                return Err(Error::tool("MCP catalog exceeds configured bounds"));
            }
            Ok(result)
        })
    }

    pub fn request(
        &self,
        kind: &str,
        name: &str,
        args: Value,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        if kind == "resource" && (name.len() > 4096 || reqwest::Url::parse(name).is_err()) {
            return Err(Error::invalid("Invalid MCP resource URI"));
        }
        if kind == "prompt" {
            let prompt = self.catalog["prompts"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["name"] == name))
                .ok_or_else(|| Error::NotFound("MCP prompt".into()))?;
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for argument in prompt["arguments"].as_array().into_iter().flatten() {
                let field = argument["name"]
                    .as_str()
                    .ok_or_else(|| Error::invalid("Invalid MCP prompt argument descriptor"))?;
                properties.insert(field.into(), json!({"type":"string","maxLength":8192}));
                if argument["required"] == true {
                    required.push(field);
                }
            }
            crate::tools::validate_value(
                &json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
                &args,
            )?;
        }
        let guard = panic::lock(&self.service);
        let service = guard
            .as_ref()
            .filter(|s| !s.is_closed())
            .ok_or_else(|| Error::tool("MCP server disconnected; reconnect through Rune"))?;
        // `Ok(Err(..))` is the server's own answer that the request failed: the
        // connection is sound and stays. `Err(..)` is a connection that cannot
        // be trusted any more — timed out, cut, cancelled — and is stopped.
        let future = async {
            let result = match kind {
                "tool" => match answered(
                    service
                        .call_tool(serde_json::from_value(
                            json!({"name":name,"arguments":args}),
                        )?)
                        .await,
                )? {
                    Ok(v) => serde_json::to_value(v)?,
                    Err(refused) => return Ok(Err(refused)),
                },
                "resource" => match answered(
                    service
                        .read_resource(rmcp::model::ReadResourceRequestParams::new(name))
                        .await,
                )? {
                    Ok(v) => serde_json::to_value(v)?,
                    Err(refused) => return Ok(Err(refused)),
                },
                "prompt" => match answered(
                    service
                        .get_prompt(serde_json::from_value(
                            json!({"name":name,"arguments":args}),
                        )?)
                        .await,
                )? {
                    Ok(v) => serde_json::to_value(v)?,
                    Err(refused) => return Ok(Err(refused)),
                },
                _ => return Err(Error::invalid("Unknown MCP operation")),
            };
            if result.to_string().len() > self.max_bytes {
                return Err(Error::tool("MCP response exceeds configured bounds"));
            }
            Ok(Ok(result))
        };
        let input = service.service().input.clone();
        let result =
            self.runtime
                .wait_pausing(self.timeout, cancel, || input.broker.asking(), future);
        drop(guard);
        match result {
            Ok(answer) => answer,
            Err(e) => {
                self.stop();
                Err(e)
            }
        }
    }

    pub fn stop(&self) {
        if let Some(task) = &self.subscription {
            task.abort();
        }
        if let Some(service) = panic::lock(&self.service).take() {
            service.service().input.broker.clear();
            service.cancellation_token().cancel();
        }
        if let Some(mut child) = panic::lock(&self.child).take() {
            child.stop(100);
            let _ = self.runtime.wait(2, &AtomicBool::new(false), async {
                child.wait().await.map_err(Error::from)
            });
        }
    }

    pub fn ready(&self) -> bool {
        panic::lock(&self.service)
            .as_ref()
            .is_some_and(|s| !s.is_closed())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn credential_key(project: &Path, server: &ResolvedServer, label: &str) -> String {
    format!(
        "{}{}:{}:{}",
        credential_prefix(project),
        server.id,
        server.digest,
        label
    )
}

/// What every vault entry of `project`'s MCP servers starts with — so the
/// entries can go with the project ([`crate::store::storage::prune`]).
pub fn credential_prefix(project: &Path) -> String {
    format!("mcp:{}:", super::config::project_key(project))
}

fn protocol_error(_: rmcp::service::ServiceError) -> Error {
    Error::tool("MCP operation failed; its remote outcome may be unknown. Check server status before retrying a mutation")
}

/// Longest part of a server's error message a result repeats.
const SERVER_MESSAGE: usize = 500;

/// A request's outcome, split by who ended it. A JSON-RPC error is the server
/// answering — it refused the request, the connection is sound — so it is
/// `Ok(Err(..))` and carries the server's own words (bounded; a notice quotes
/// and redacts them before the model reads them). Anything else leaves the
/// connection in a state nobody knows: `Err`.
fn answered<T>(result: std::result::Result<T, rmcp::service::ServiceError>) -> Result<Result<T>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(rmcp::service::ServiceError::McpError(data)) => Ok(Err(Error::tool(format!(
            "the MCP server answered with error {}: {}",
            data.code.0,
            crate::tools::cap(data.message.to_string(), SERVER_MESSAGE)
        )))),
        Err(other) => Err(protocol_error(other)),
    }
}

struct BoundedReader<R> {
    inner: R,
    seen: usize,
    max: usize,
}
impl<R: AsyncRead + Unpin> AsyncRead for BoundedReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                for b in &buf.filled()[before..] {
                    this.seen = if *b == b'\n' { 0 } else { this.seen + 1 };
                    if this.seen > this.max {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "MCP message too large",
                        )));
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub struct RemoteTool {
    connection: Arc<Connection>,
    name: String,
    original: String,
    description: String,
    schema: Value,
    validator: jsonschema::Validator,
    output: Option<jsonschema::Validator>,
}

impl RemoteTool {
    pub fn new(connection: Arc<Connection>, descriptor: Value) -> Result<Self> {
        let original = descriptor["name"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .ok_or_else(|| Error::invalid("Invalid MCP tool name"))?
            .to_string();
        let schema = descriptor["inputSchema"].clone();
        let validator = jsonschema::draft202012::options()
            .with_pattern_options(
                jsonschema::PatternOptions::fancy_regex().backtrack_limit(100_000),
            )
            .build(&schema)
            .map_err(|_| Error::invalid("Unsupported MCP schema"))?;
        let output = descriptor
            .get("outputSchema")
            .map(|s| {
                jsonschema::draft202012::new(s)
                    .map_err(|_| Error::invalid("Unsupported MCP output schema"))
            })
            .transpose()?;
        let name = internal_name(&connection.definition.id, &original);
        let description = format!(
            "{} / {}: {}",
            connection.definition.id,
            original,
            descriptor["description"].as_str().unwrap_or("MCP tool")
        );
        if description.len() > 16384 {
            return Err(Error::invalid("MCP tool description is too long"));
        }
        Ok(Self {
            connection,
            name,
            original,
            description,
            schema,
            validator,
            output,
        })
    }
}

impl crate::tools::Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn declaration(&self) -> crate::tools::ToolDecl {
        crate::tools::ToolDecl {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }
    fn mutates(&self) -> bool {
        true
    }
    fn is_external(&self) -> bool {
        true
    }
    fn risk(&self, _: &Value) -> crate::perm::denylist::Risk {
        crate::perm::denylist::Risk::AskEvery {
            why: "External MCP operation with server-defined effects".into(),
        }
    }
    fn grant_key(&self, _: &Value) -> String {
        format!("{}:{}", self.name, self.connection.definition.digest)
    }
    fn request_count(&self, _: &Value) -> usize {
        1
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "{} / {}\n{}",
            self.connection.definition.id,
            self.original,
            crate::tools::cap(
                crate::net::redact::scrub(&args.to_string()).into_owned(),
                2000
            )
        )
    }
    fn mcp_identity(&self) -> Option<(String, String)> {
        Some((self.connection.definition.id.clone(), self.original.clone()))
    }
    /// Against the server's own input schema, naming the field that is wrong:
    /// the model reads this schema from a discovery result rather than a
    /// declaration, so a precise correction is what keeps a weaker model on
    /// track.
    fn validate(&self, args: &Value) -> Result<()> {
        self.validator
            .validate(args)
            .map_err(|e| Error::invalid(crate::tools::correction(&e)))
    }
    fn run(
        &self,
        ctx: &mut crate::tools::ToolCtx<'_>,
        args: &Value,
    ) -> Result<crate::tools::Outcome> {
        let connection =
            ctx.capabilities
                .live(&self.connection.definition.id, ctx.registry, ctx.cancel)?;
        if connection.definition.digest != self.connection.definition.digest {
            return Err(Error::Denied("MCP tool revision changed".into()));
        }
        let result = connection.request("tool", &self.original, args.clone(), ctx.cancel);
        let result = result?;
        if let Some(schema) = &self.output {
            if !result["isError"].as_bool().unwrap_or(false) {
                schema
                    .validate(&result["structuredContent"])
                    .map_err(|_| Error::tool("MCP output does not satisfy its declared schema"))?;
            }
        }
        ctx.capabilities.outcome(
            &connection.definition.id,
            result,
            format!("MCP {} completed", self.original),
        )
    }
}

/// The name Rune keeps an MCP tool under: its server's prefix and a digest of
/// the server's own name for it, so two servers' tools of the same name never
/// meet, and the name is the same every time the server connects.
pub fn internal_name(server: &str, tool: &str) -> String {
    format!(
        "{}{}",
        tool_prefix(server),
        &crate::checkpoint::objects::hash_of(tool.as_bytes())[..16]
    )
}

pub fn tool_prefix(id: &str) -> String {
    format!(
        "mcp__{}_{}__",
        &id[..id.len().min(20)],
        &crate::checkpoint::objects::hash_of(id.as_bytes())[..8]
    )
}
