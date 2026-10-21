//! The engine's only door to the network.
//!
//! Everything that leaves this machine leaves through here: the model provider
//! and the web tools' services ([`crate::web`]), and nothing else. Keeping it to one file is what
//! makes "what does Rune talk to?" a question with a readable answer — which
//! matters more, not less, now that the source is public.
//!
//! # What is borrowed and what is not
//!
//! `ureq` + `rustls` carry TLS and HTTP/1.1. That is a deliberate exception to
//! this workspace's dependency policy, written down where the exception is made
//! rather than in a commit message: TLS is the one part of this program where
//! being clever is a vulnerability. Everything above the socket — the SSE
//! framing ([`sse`]), the partial-JSON scanning, the provider protocol — is
//! still ours, because those are parsing problems and parsing problems are
//! testable.
//!
//! # Rules this module keeps
//!
//! * **No retries here.** This module never repeats anything on its own; a
//!   retried tool call can run a command twice and a retried model call is
//!   billed twice. What it does instead is describe the failure precisely enough
//!   — [`Error::Unreachable`] versus [`Error::Rejected`] with its status, and a
//!   silent server as [`Error::Stalled`] — for [`crate::retry`] to decide one
//!   layer up, where it is known whether anything has been consumed yet.
//! * **No secret ever reaches an error string.** Keys are registered with
//!   [`redact`] the moment they are loaded, and error text is scrubbed at the
//!   one funnel every message passes through.
//! * **A non-2xx is data, not an exception.** The body of a rejection says what
//!   is actually wrong ("model not found", "insufficient balance") and the user
//!   needs to read it, so it is parsed rather than thrown away.

pub mod cookies;
#[path = "Interrupt.rs"]
mod interrupt;
pub mod redact;
pub mod sse;

use std::io::Read;
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};

pub use sse::Sse;

/// Encode one path or query component without allowing separators to change its meaning.
pub fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A reader that owns its response body. Boxed because the streaming and
/// non-streaming paths hand back different concrete types and the caller has no
/// reason to care which.
pub type BodyReader = Box<dyn Read + Send>;

pub struct Http {
    agent: ureq::Agent,
    max_response: u64,
    max_event: usize,
}

/// A completed non-streaming call.
pub struct Response {
    pub status: u16,
    /// Parsed body, or `Value::Null` when the server sent something that was not
    /// JSON — an HTML error page from a proxy, most often.
    pub body: Value,
    /// The raw text, kept for the error message when `body` could not be parsed.
    pub text: String,
    /// The Content-Type header reported by the server, if present.
    pub content_type: Option<String>,
    /// Where a redirect points. Only an [`Http::public`] client sees one: it
    /// follows nothing on its own, so every hop can be checked first.
    pub location: Option<String>,
    /// Every `Set-Cookie` header, as sent. Read only by a fetch that asked to
    /// keep cookies ([`cookies::Jar`]); no client here keeps any on its own.
    pub set_cookies: Vec<String>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl Http {
    /// `timeout` bounds the whole call. For a streaming model call it must be
    /// generous — a long answer legitimately takes minutes — which is why
    /// cancellation is checked between chunks rather than left to the clock.
    pub fn new(timeout: Duration) -> Http {
        Self::configured(
            &crate::settings::configuration::Shared::default(),
            Some(timeout),
            Default::default(),
        )
    }
    pub fn configured(
        settings: &crate::settings::configuration::Shared,
        timeout: Option<Duration>,
        control: crate::provider::RequestControl,
    ) -> Http {
        Http::with_reach(settings, timeout, control, false)
    }

    /// A client for addresses the *model* chose (web fetches): it connects
    /// only to public addresses — checked on what DNS answered, not on how the
    /// URL was spelled — and follows no redirect on its own, so the caller can
    /// check every hop (audit H5).
    pub fn public(
        settings: &crate::settings::configuration::Shared,
        timeout: Option<Duration>,
        control: crate::provider::RequestControl,
    ) -> Http {
        Http::with_reach(settings, timeout, control, true)
    }

    fn with_reach(
        settings: &crate::settings::configuration::Shared,
        timeout: Option<Duration>,
        control: crate::provider::RequestControl,
        public_only: bool,
    ) -> Http {
        use ureq::unversioned::transport::{
            ConnectProxyConnector, Connector, RustlsConnector, TcpConnector,
        };
        let config = ureq::Agent::config_builder()
            .timeout_global(timeout)
            .timeout_connect(Some(if let Some(t) = timeout {
                settings.duration("network.connect_timeout").min(t)
            } else {
                settings.duration("network.connect_timeout")
            }))
            .timeout_recv_response(timeout)
            .timeout_recv_body(None)
            .timeout_send_body(timeout)
            .http_status_as_error(false)
            .max_redirects(if public_only { 0 } else { 10 })
            .user_agent(concat!("rune/", env!("CARGO_PKG_VERSION")))
            .build();
        let connector = ConnectProxyConnector::default()
            .chain(TcpConnector::default())
            .chain(interrupt::Interrupt {
                control,
                config: settings.clone(),
            })
            .chain(RustlsConnector::default());
        Http {
            agent: ureq::Agent::with_parts(
                config,
                connector,
                PublicResolver {
                    public_only,
                    inner: ureq::unversioned::resolver::DefaultResolver::default(),
                },
            ),
            max_response: settings.number("network.max_response_size"),
            max_event: settings.number("network.max_event_size") as usize,
        }
    }

    pub fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> Result<Response> {
        let resp = self
            .build(url, headers)
            .send_json(body)
            .map_err(transport)?;
        self.decode_response(resp)
    }

    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<Response> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.call().map_err(transport)?;
        self.decode_response(resp)
    }

    pub fn get_json(&self, url: &str, headers: &[(String, String)]) -> Result<Response> {
        self.get(url, headers)
    }

    fn decode_response(&self, mut resp: ureq::http::Response<ureq::Body>) -> Result<Response> {
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let set_cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        let text = resp
            .body_mut()
            .with_config()
            .limit(self.max_response)
            .read_to_string()
            .map_err(transport)?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Response {
            status,
            body,
            text,
            content_type,
            location,
            set_cookies,
        })
    }

    /// Open a streaming call. Fails before returning a reader when the server
    /// rejected the request, so a caller never has to distinguish "no events" from
    /// "never accepted".
    /// `timeout` overrides the agent's own for this one call. That override is
    /// what lets a local Ollama fail in seconds while a long reasoning turn
    /// against a hosted model is still allowed its minutes — one number cannot
    /// be right for both, and the provider is who knows which it is.
    pub fn post_sse(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<Sse<BodyReader>> {
        let mut request = self
            .build(url, headers)
            .header("Accept", "text/event-stream");
        if let Some(t) = timeout {
            request = request.config().timeout_global(Some(t)).build();
        }
        let mut resp = request.send_json(body).map_err(transport)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp
                .body_mut()
                .with_config()
                .limit(self.max_response)
                .read_to_string()
                .unwrap_or_default();
            return Err(rejected(status, &text));
        }
        Ok(
            Sse::new(Box::new(resp.into_body().into_reader()) as BodyReader)
                .with_limit(self.max_event),
        )
    }

    fn build(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let mut req = self.agent.post(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req
    }
}

/// Turn a rejection body into the clearest sentence available.
///
/// Providers bury the useful line at different depths — `error.message`,
/// `message`, `error` as a bare string — so all three are tried before falling
/// back to the raw text. The status is always kept: "401" and "429" mean
/// different things to do next, and only the number says which.
pub fn rejected(status: u16, text: &str) -> Error {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let message = extract_rejection_message(&parsed, text);
    if matches!(status, 400 | 422) {
        let error = parsed.get("error").unwrap_or(&parsed);
        let parameter = error["param"]
            .as_str()
            .or_else(|| error["parameter"].as_str());
        let limit = error["max_value"]
            .as_u64()
            .or_else(|| error["maximum"].as_u64());
        if parameter.is_some_and(|p| {
            ["max_tokens", "max_completion_tokens", "max_output_tokens"].contains(&p)
        }) {
            if let Some(limit) = limit.filter(|v| *v > 0) {
                return Error::OutputLimit { limit, message };
            }
        }
    }
    if context_overflow(status, &message) {
        return Error::ContextOverflow(message);
    }

    let hint = match status {
        401 | 403 => " — check the API key for this provider",
        402 => " — the account is out of credit",
        404 => " — check the model name and the base URL",
        429 => " — rate limited; wait and try again",
        500..=599 => " — the provider is having trouble; this is not your request",
        _ => "",
    };
    Error::Rejected {
        status,
        message: format!("{message}{hint}"),
    }
}

/// Whether a rejection says the request is longer than the model's context.
///
/// No provider exposes this as a field or a code of its own — every one says
/// it in a sentence — so the sentence is read. No third-party API available;
/// phrasings observed as of 2026-10. A miss costs nothing worse than before:
/// the rejection is reported as it always was.
fn context_overflow(status: u16, message: &str) -> bool {
    if status == 413 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "exceeds the context",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// The clearest sentence a refusal's body holds — the one reading
/// [`rejected`] uses, for a caller that names its own next step.
pub fn rejection_message(text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    extract_rejection_message(&parsed, text)
}

fn extract_rejection_message(parsed: &Value, text: &str) -> String {
    // 1. Direct pointer / property lookups in JSON object
    if let Some(msg) = parsed
        .pointer("/error/message")
        .or_else(|| parsed.pointer("/0/error/message"))
        .or_else(|| parsed.pointer("/errors/0/message"))
        .or_else(|| parsed.pointer("/0/message"))
        .or_else(|| parsed.pointer("/error/details/0/message"))
        .or_else(|| parsed.pointer("/error/details/0/description"))
        .or_else(|| parsed.get("message"))
        .or_else(|| parsed.get("detail"))
        .or_else(|| parsed.get("error"))
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("message").and_then(|m| m.as_str()).map(String::from),
            _ => None,
        })
    {
        let trimmed = msg.trim();
        if !trimmed.is_empty() {
            return trimmed.chars().take(1000).collect();
        }
    }

    // 2. If it's a JSON array, inspect the first element for error / message
    if let Some(first) = parsed.as_array().and_then(|a| a.first()) {
        if let Some(msg) = first
            .pointer("/error/message")
            .or_else(|| first.get("message"))
            .or_else(|| first.get("error"))
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("message").and_then(|m| m.as_str()).map(String::from),
                _ => None,
            })
        {
            let trimmed = msg.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(1000).collect();
            }
        }
    }

    let raw = text.trim();
    if raw.is_empty() {
        return "no response body".into();
    }

    // 3. If text is HTML, extract title or body text rather than raw tags like <html>
    if raw.starts_with('<') {
        if let Some(start) = raw.find("<title>") {
            if let Some(end) = raw[start..].find("</title>") {
                let title = raw[start + 7..start + end].trim();
                if !title.is_empty() {
                    return title.chars().take(300).collect();
                }
            }
        }
    }

    // 4. If text is multi-line formatted JSON or raw text, do NOT just take line 1 if line 1 is `[{` or `{`
    let clean_lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && *line != "{"
                && *line != "["
                && *line != "[{"
                && *line != "}"
                && *line != "]"
        })
        .collect();

    if !clean_lines.is_empty() {
        let joined = clean_lines.join(" ");
        return joined.chars().take(500).collect();
    }

    raw.chars().take(300).collect()
}

/// Everything below HTTP: DNS, TCP, TLS, timeout.
///
/// Kept separate from [`rejected`] because the two need different reactions —
/// one is "your network or their outage", the other is "your request".
fn transport(e: ureq::Error) -> Error {
    if let ureq::Error::Io(io) = &e {
        if let Some(silence) = stalled(io) {
            return Error::Stalled(silence);
        }
    }
    Error::Unreachable(e.to_string())
}

/// How long the server had been silent, when `e` is the stall limit giving up
/// on it (`network.stream_idle_timeout`) rather than any other I/O failure —
/// the same reading for a request that never got an answer and for a stream
/// that went quiet half-way, whose reader hands the transport's error back as
/// it was.
pub fn stalled(e: &std::io::Error) -> Option<Duration> {
    e.get_ref()?
        .downcast_ref::<interrupt::Stalled>()
        .map(|stalled| stalled.0)
}

/// Whether an address belongs to this machine or a private network — the
/// places a fetch the model asked for must never reach.
pub fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0 // "this network" 0.0.0.0/8
                || (o[0] == 100 && (o[1] & 0xC0) == 64) // CGNAT 100.64.0.0/10
                || o[0] >= 240 // reserved 240.0.0.0/4
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(std::net::IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// Resolves names, and for a public-only client refuses any name that
/// answers with a private address (DNS rebinding, `localtest.me`, decimal or
/// octal spellings of 127.0.0.1 — all end as an address, and the address is
/// what is checked).
#[derive(Debug)]
struct PublicResolver {
    public_only: bool,
    inner: ureq::unversioned::resolver::DefaultResolver,
}

impl ureq::unversioned::resolver::Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        if self.public_only && addrs.iter().any(|addr| is_private(addr.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the address is internal or private; web fetches reach only public hosts",
            )));
        }
        Ok(addrs)
    }
}

/// Validate an external URL for web fetching. Blocks internal, private, and loopback addresses (SSRF prevention).
pub fn validate_public_url(raw_url: &str) -> Result<(ureq::http::Uri, String)> {
    let mut url_str = raw_url.trim().to_string();
    if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
        url_str = format!("https://{url_str}");
    }
    let uri: ureq::http::Uri = url_str
        .parse()
        .map_err(|_| Error::invalid("invalid URL format"))?;

    let scheme = uri.scheme_str().unwrap_or("");
    if scheme != "http" && scheme != "https" {
        return Err(Error::invalid("Only HTTP and HTTPS URLs are allowed."));
    }

    let Some(host) = uri.host() else {
        return Err(Error::invalid("URL must contain a host."));
    };

    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(Error::invalid("URL cannot contain credentials."));
    }

    let host_lower = host.to_ascii_lowercase();

    if host_lower == "localhost"
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
        || host_lower.ends_with(".localhost")
    {
        return Err(Error::invalid(
            "Cannot fetch internal/private network URLs.",
        ));
    }

    // An address literal is checked here; a name is checked on what it
    // resolves to, by `PublicResolver`. `Uri::host` keeps an IPv6 literal's
    // brackets, which once let `[::1]` through unparsed.
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    {
        if is_private(ip) {
            return Err(Error::invalid(
                "Cannot fetch internal/private network URLs.",
            ));
        }
    }

    Ok((uri, url_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_structured_output_parameter_ceiling_is_adaptive() {
        let body =
            r#"{"error":{"param":"max_tokens","max_value":8192,"message":"unsupported output"}}"#;
        assert!(matches!(
            rejected(400, body),
            Error::OutputLimit { limit: 8192, .. }
        ));
        for (status, text) in [
            (500, body),
            (
                400,
                r#"{"error":{"message":"max_tokens must be below 8192"}}"#,
            ),
            (
                400,
                r#"{"error":{"param":"input_tokens","max_value":8192}}"#,
            ),
            (400, r#"{"error":{"param":"max_tokens","max_value":0}}"#),
        ] {
            assert!(!matches!(rejected(status, text), Error::OutputLimit { .. }));
        }
    }

    #[test]
    fn digs_the_message_out_of_every_common_error_shape() {
        let openai = rejected(
            401,
            r#"{"error":{"message":"Invalid API key","type":"auth"}}"#,
        );
        assert!(openai.to_string().contains("Invalid API key"));
        let flat = rejected(400, r#"{"message":"bad model"}"#);
        assert!(flat.to_string().contains("bad model"));
        let bare = rejected(400, r#"{"error":"nope"}"#);
        assert!(bare.to_string().contains("nope"));
        let google_array = rejected(
            429,
            r#"[
  {
    "error": {
      "code": 429,
      "message": "Quota exceeded for quota metric 'Generate Content API requests'...",
      "status": "RESOURCE_EXHAUSTED"
    }
  }
]"#,
        );
        assert!(google_array.to_string().contains("Quota exceeded"));
        assert!(!google_array.to_string().contains("HTTP 429: [{ —"));
        let cloudflare = rejected(503, r#"{"errors":[{"message":"Origin unreachable"}]}"#);
        assert!(cloudflare.to_string().contains("Origin unreachable"));
        let detail = rejected(404, r#"{"detail":"Endpoint not found"}"#);
        assert!(detail.to_string().contains("Endpoint not found"));
    }

    #[test]
    fn survives_a_body_that_is_not_json_at_all() {
        let e = rejected(502, "<html><body>Bad Gateway</body></html>");
        assert!(e.to_string().contains("502"));
        assert!(e.to_string().contains("Bad Gateway"));
    }

    #[test]
    fn says_what_to_do_about_the_common_statuses() {
        assert!(rejected(401, "{}").to_string().contains("API key"));
        assert!(rejected(404, "{}").to_string().contains("model name"));
        assert!(rejected(429, "{}").to_string().contains("rate limited"));
        assert!(rejected(503, "{}").to_string().contains("not your request"));
    }

    #[test]
    fn keeps_the_status_code_even_with_an_empty_body() {
        assert!(rejected(418, "").to_string().contains("418"));
    }

    #[test]
    fn an_error_carrying_a_registered_key_is_scrubbed_when_displayed() {
        redact::register("sk-net-test-key-0123456789");
        let e = rejected(
            401,
            r#"{"error":{"message":"bad key sk-net-test-key-0123456789"}}"#,
        );
        assert!(!e.to_string().contains("sk-net-test-key-0123456789"));
    }

    #[test]
    fn validate_public_url_blocks_internal_and_private_hosts() {
        assert!(validate_public_url("http://localhost:8080").is_err());
        assert!(validate_public_url("https://127.0.0.1/api").is_err());
        assert!(validate_public_url("http://192.168.1.1").is_err());
        assert!(validate_public_url("https://10.0.0.1").is_err());
        assert!(validate_public_url("http://172.16.0.1").is_err());
        assert!(validate_public_url("http://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_public_url("http://service.local").is_err());
        assert!(validate_public_url("http://user:pass@example.com").is_err());
        assert!(validate_public_url("https://example.com/docs").is_ok());
        assert!(validate_public_url("example.com/docs").is_ok());
    }

    /// Audit H5: the spellings that used to slip past the literal check.
    #[test]
    fn bracketed_and_mapped_addresses_are_private_too() {
        assert!(validate_public_url("http://[::1]:8080/").is_err());
        assert!(validate_public_url("http://[::ffff:127.0.0.1]/").is_err());
        assert!(validate_public_url("http://[fd00::1]/").is_err());
        assert!(is_private("::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_private("224.0.0.1".parse().unwrap()));
        assert!(!is_private("1.1.1.1".parse().unwrap()));
    }

    /// Audit H5: what is checked is the address a name resolved to — so a
    /// name (or a decimal spelling) that lands on this machine is refused at
    /// connect time, whatever the URL looked like.
    #[test]
    fn a_public_client_refuses_a_name_that_resolves_to_this_machine() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let http = Http::public(
            &crate::settings::configuration::Shared::default(),
            Some(Duration::from_secs(5)),
            Default::default(),
        );
        // The decimal spelling of 127.0.0.1 only where this system's resolver
        // reads it (Windows's does not: there it names nothing at all).
        use std::net::ToSocketAddrs;
        let decimal = ("2130706433", port).to_socket_addrs().is_ok();
        for url in [
            Some(format!("http://localhost:{port}/")),
            decimal.then(|| format!("http://2130706433:{port}/")),
        ]
        .into_iter()
        .flatten()
        {
            let Err(error) = http.get(&url, &[]) else {
                panic!("{url} was fetched");
            };
            assert!(error.to_string().contains("private"), "{url}: {error}");
        }
    }
}
