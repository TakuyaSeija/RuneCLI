//! SDK OAuth with origin-restricted HTTP and Rune's existing private secret store.
use super::config::{Auth, ResolvedServer, Settings};
use crate::{panic, Error, Result};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
    CredentialRefreshGuard, CredentialStore, OAuthHttpClient, OAuthHttpClientFuture,
    OAuthHttpRequest, StoredCredentials,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Store {
    home: PathBuf,
    key: String,
}
#[async_trait::async_trait]
impl CredentialStore for Store {
    async fn acquire_refresh_guard(
        &self,
    ) -> std::result::Result<Option<CredentialRefreshGuard>, AuthError> {
        let path = self.home.join("run").join(format!(
            "McpAuth-{}.lock",
            &crate::checkpoint::objects::hash_of(self.key.as_bytes())[..16]
        ));
        loop {
            match crate::lock::exclusive(&path, crate::lock::Wait::No) {
                Ok(file) => return Ok(Some(CredentialRefreshGuard::new(file))),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(error) => return Err(store_error(error)),
            }
        }
    }
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let secrets = crate::settings::secrets::Secrets::load(&self.home).map_err(store_error)?;
        if let crate::settings::secrets::Store::Unavailable(why) = secrets.store() {
            return Err(AuthError::CredentialStoreError(why.message()));
        }
        secrets
            .get(&self.key)
            .map(|s| {
                serde_json::from_str(s).map_err(|_| {
                    AuthError::CredentialStoreError("Invalid saved MCP credentials".into())
                })
            })
            .transpose()
    }
    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let text = serde_json::to_string(&credentials)
            .map_err(|_| AuthError::CredentialStoreError("Cannot encode MCP credentials".into()))?;
        let mut secrets =
            crate::settings::secrets::Secrets::load(&self.home).map_err(store_error)?;
        secrets.set(&self.key, &text).map_err(store_error)
    }
    async fn clear(&self) -> std::result::Result<(), AuthError> {
        crate::settings::secrets::Secrets::load(&self.home)
            .and_then(|mut s| s.remove(&self.key).map(|_| ()))
            .map_err(store_error)
    }
}
/// Said with its cause: a store that is unavailable says why. Messages from
/// the store name files and causes, never a credential's value.
fn store_error(error: Error) -> AuthError {
    AuthError::CredentialStoreError(format!(
        "Rune could not access its MCP credential store: {error}"
    ))
}

struct Http {
    client: reqwest::Client,
    origins: Vec<String>,
    max: usize,
}
impl OAuthHttpClient for Http {
    fn execute(&self, input: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let url = reqwest::Url::parse(&input.request.uri().to_string())?;
            if !self.origins.contains(&url.origin().ascii_serialization()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "OAuth origin is not approved; add it to this server's allowed_origins",
                )
                .into());
            }
            let mut request = self
                .client
                .request(input.request.method().clone(), url)
                .headers(input.request.headers().clone())
                .body(input.request.body().clone());
            if let Some(timeout) = input.timeout {
                request = request.timeout(timeout);
            }
            let mut response = request.send().await?;
            // No redirect is followed. Discovery can safely retry only an explicitly approved URL.
            let status = response.status();
            let headers = response.headers().clone();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > self.max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "OAuth response exceeds its limit",
                    )
                    .into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let mut output = ureq::http::Response::builder().status(status).body(bytes)?;
            *output.headers_mut() = headers;
            Ok(output)
        })
    }
}

pub async fn manager(
    home: &Path,
    project: &Path,
    server: &ResolvedServer,
    settings: &Settings,
) -> Result<AuthorizationManager> {
    let endpoint = server
        .definition
        .url
        .as_deref()
        .ok_or_else(|| Error::invalid("OAuth requires HTTP"))?;
    let url = super::config::check_endpoint(endpoint)?;
    let mut origins = vec![url.origin().ascii_serialization()];
    if let Auth::Oauth {
        allowed_origins, ..
    } = &server.definition.auth
    {
        for origin in allowed_origins {
            origins.push(
                super::config::check_endpoint(origin)?
                    .origin()
                    .ascii_serialization(),
            );
        }
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(settings.timeout_seconds))
        .build()
        .map_err(|_| Error::tool("Could not create OAuth client"))?;
    let mut manager = AuthorizationManager::new_with_oauth_http_client(
        endpoint,
        Arc::new(Http {
            client,
            origins,
            max: settings.max_message_bytes,
        }),
    )
    .await
    .map_err(auth_error)?;
    manager.set_credential_store(Store {
        home: home.into(),
        key: super::mcp::credential_key(project, server, "oauth"),
    });
    Ok(manager)
}

pub struct Login {
    pub status: Arc<Mutex<Value>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Login {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn begin(
    home: PathBuf,
    project: PathBuf,
    server: ResolvedServer,
    settings: Settings,
) -> Result<(Login, String)> {
    let mut manager = manager(&home, &project, &server, &settings).await?;
    let metadata = manager.resolve_metadata().await.map_err(auth_error)?;
    manager.set_metadata(metadata.metadata);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let redirect = format!("http://{address}/callback");
    let mut request = AuthorizationRequest::new(&redirect)
        .with_client_name("Rune")
        .with_application_type("native");
    if let Auth::Oauth {
        client_id, scopes, ..
    } = &server.definition.auth
    {
        request = request.with_scopes(scopes.clone());
        if let Some(id) = client_id {
            request = request.with_preregistered_client(id);
        }
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, e)| auth_error(e))?;
    let url = session.get_authorization_url().to_owned();
    let status = Arc::new(Mutex::new(
        json!({"status":"auth-pending","server_id":server.id}),
    ));
    let progress = status.clone();
    let task = tokio::spawn(async move {
        let outcome=tokio::time::timeout(Duration::from_secs(300),async {
            loop {
                let (mut socket,peer)=listener.accept().await?;
                if !peer.ip().is_loopback(){continue;}
                let mut bytes=Vec::new();let mut buf=[0u8;1024];
                loop {
                    let n=tokio::time::timeout(Duration::from_secs(5),socket.read(&mut buf)).await.map_err(|_|Error::tool("OAuth callback timed out"))??;
                    if n==0{break;}bytes.extend_from_slice(&buf[..n]);
                    if bytes.len()>8192{return Err(Error::invalid("OAuth callback is too large"));}
                    if bytes.windows(4).any(|b|b==b"\r\n\r\n"){break;}
                }
                let text=std::str::from_utf8(&bytes).map_err(|_|Error::invalid("Invalid OAuth callback"))?;
                let mut first=text.lines().next().unwrap_or("").split_whitespace();
                if first.next()!=Some("GET"){continue;}
                let target=first.next().unwrap_or("");
                if !target.starts_with("/callback?"){let _=socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;continue;}
                let current=Settings::from_config(&crate::settings::configuration::Configuration::load(&home)?)?.resolve_servers(&project)?;
                if !current.iter().any(|s|s.id==server.id&&s.approved&&s.digest==server.digest){return Err(Error::Denied("MCP definition changed during login".into()));}
                let callback=format!("http://{address}{target}");
                let result=session.handle_callback_url(&callback).await;
                let body=if result.is_ok(){"Rune sign-in complete. You can close this page."}else{"Rune sign-in failed. Return to Rune and try again."};
                let reply=format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _=socket.write_all(reply.as_bytes()).await;
                result.map_err(auth_error)?;return Ok::<(),Error>(());
            }
        }).await;
        *panic::lock(&progress) = match outcome {
            Ok(Ok(())) => json!({"status":"authenticated"}),
            Ok(Err(e)) => json!({"status":"auth-failed","error":e.to_string()}),
            Err(_) => json!({"status":"auth-expired"}),
        };
    });
    Ok((Login { status, task }, url))
}

fn auth_error(_: AuthError) -> Error {
    Error::Denied(
        "MCP authentication failed; check the approved OAuth origins and client configuration"
            .into(),
    )
}
