//! Fetching a model-chosen page is separate from sending a search query: by
//! default this tool reads the page itself, needing no credentials, and every
//! connection stays behind the public-address gate, including every redirect
//! hop.
//!
//! When the person chose a fetch connection (`web_fetch.provider`, owner
//! 2026-10-14) the page is read by that service instead — the URL is handed to
//! a third party, which the person allowed by choosing it. The address is held
//! to the same rule first: a page on this machine or a private network is
//! refused before any service is asked for it, so which reader serves never
//! changes what may be read.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use super::{cap, field_bool, field_given_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::net::cookies::Jar;
use crate::net::{Http, Response};
use crate::proto::LogLevel;
use crate::settings::services::Service;
use crate::settings::Settings;
use crate::web::text::decode_entities;
use crate::web::{self, Choice};

/// Redirect hops a web fetch follows, each one checked.
const MAX_REDIRECTS: usize = 5;

pub struct WebFetch {
    limits: Arc<Limits>,
    settings: Arc<Mutex<Settings>>,
}

impl WebFetch {
    pub fn new(limits: Arc<Limits>, settings: Arc<Mutex<Settings>>) -> WebFetch {
        WebFetch { limits, settings }
    }
}

impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn mutates(&self) -> bool {
        false
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "web_fetch".into(),
            description: "Read one public web page as Markdown. Use web_search to search the web."
                .into(),
            parameters: flat_schema(
                json!({
                    "url": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The http:// or https:// page to read. Private and internal addresses are refused."
                    },
                    "cookies": {
                        "type": "boolean",
                        "description": "Keep the cookies the site sets while it redirects, for this call only. Use it when a fetch failed because the page kept redirecting. Default false."
                    }
                }),
                &["url"],
            ),
        }
    }

    fn preview(&self, args: &Value) -> String {
        let url = field_given_str(args, "url").unwrap_or("?");
        if field_bool(args, "cookies", false) {
            format!("fetch {url:?} with cookies")
        } else {
            format!("fetch {url:?}")
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let url = field_given_str(args, "url")
            .ok_or_else(|| Error::invalid("web_fetch needs a non-empty page URL in `url`"))?;
        let choice = {
            let mut settings = crate::panic::lock(&self.settings);
            settings.follow_services();
            web::choose(&settings, Service::Fetch)
        };
        match choice {
            Choice::Builtin => Ok(self.outcome(
                self.fetch(url, field_bool(args, "cookies", false), ctx.cancel)?,
                None,
            )),
            Choice::Ready(route) => {
                let content = self.through(&route, url, ctx.cancel, ctx.log)?;
                Ok(self.outcome(content, Some(&route.name)))
            }
            Choice::Unavailable(why) => {
                (ctx.log)(LogLevel::Warn, "fetch_unavailable", why.clone(), None);
                Err(Error::NotFound(why))
            }
        }
    }
}

impl WebFetch {
    fn outcome(&self, content: String, connection: Option<&str>) -> Outcome {
        let max_chars = self.limits.config.number("web_fetch.max_output_chars") as usize;
        let mut detail = json!({"results": 0, "fetched": 1});
        if let Some(name) = connection {
            detail["connection"] = json!(name);
        }
        Outcome::new("1 page fetched", cap(content, max_chars)).with_detail(detail)
    }

    /// Read `raw_url` through a fetch connection, retried on the owner's curve
    /// with its key ring, every step written to the session's log.
    fn through(
        &self,
        route: &web::Route,
        raw_url: &str,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        log: &super::LogFn<'_>,
    ) -> Result<String> {
        let url = public_page(raw_url)?;
        let timeout = self.limits.config.duration("web_fetch.timeout");
        let http = web::client(&self.settings, timeout, Some(Arc::clone(cancel)));
        let detail = |extra: Value| {
            let mut d = json!({
                "connection": route.name,
                "vendor": route.adapter.id,
                "host": route.host(),
            });
            if let Some(r) = &route.route {
                d["route"] = json!(r);
            }
            if let (Some(d), Some(extra)) = (d.as_object_mut(), extra.as_object()) {
                d.extend(extra.clone());
            }
            Some(d)
        };
        let started = Instant::now();
        let mut told = 0;
        let mut announce = |w: crate::retry::Waiting| {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return crate::retry::Go::Cancel;
            }
            if w.attempt != told {
                told = w.attempt;
                log(
                    LogLevel::Warn,
                    "fetch_retry",
                    format!(
                        "{} · {} · retry {}/{} in {}s{}",
                        route.name,
                        w.reason,
                        w.attempt,
                        w.of,
                        w.wait.as_secs(),
                        if w.rotated { " with the next key" } else { "" }
                    ),
                    detail(json!({ "attempt": w.attempt, "reason": w.reason })),
                );
            }
            crate::retry::Go::On
        };
        let mut attempt = || {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let key = route.secret.as_deref().and_then(|name| {
                crate::panic::lock(&self.settings)
                    .secrets
                    .get(name)
                    .map(String::from)
            });
            web::fetch_once(&http, route, key.as_deref(), &url)
        };
        let result = crate::retry::run(
            &self.settings,
            route.secret.as_deref(),
            &mut announce,
            &mut attempt,
        );
        let ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(page) => log(
                LogLevel::Info,
                "fetch",
                format!(
                    "{} · {} characters · {ms} ms",
                    route.name,
                    page.text.chars().count()
                ),
                detail(json!({ "characters": page.text.chars().count(), "ms": ms })),
            ),
            Err(Error::Cancelled) => {}
            Err(e) => log(
                LogLevel::Error,
                "fetch_failed",
                format!("{} · {e}", route.name),
                detail(json!({ "error": e.code(), "ms": ms })),
            ),
        }
        let page = result?;
        // The service's own reading is used as it is; only a page it handed
        // back as HTML is read here, with the same parser the built-in reader
        // uses, so the model never gets a vendor's markup instead of a page.
        let (title, text) = if looks_like_html(&page.text) {
            let (parsed, markdown) = html_to_markdown(&page.text, Some(&url));
            (
                page.title
                    .or_else(|| (!parsed.is_empty()).then_some(parsed)),
                markdown,
            )
        } else {
            (page.title, page.text)
        };
        Ok(
            match title.filter(|_| !text.trim_start().starts_with('#')) {
                Some(title) => format!("# {title}\n\n{text}"),
                None => text,
            },
        )
    }

    /// Fetch a web page and convert its content to clean Markdown.
    /// Does not require a Serper API key.
    fn fetch(
        &self,
        raw_url: &str,
        cookies: bool,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String> {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let (_, start) = crate::net::validate_public_url(raw_url)?;
        let timeout = self.limits.config.duration("web_fetch.timeout");
        let http = Http::public(
            &self.limits.config,
            Some(timeout),
            crate::provider::RequestControl {
                cancel: Some(std::sync::Arc::clone(cancel)),
                ..Default::default()
            },
        );

        let headers = vec![
            (
                "User-Agent".to_string(),
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36".to_string(),
            ),
            (
                "Accept".to_string(),
                "text/html,application/xhtml+xml,text/plain,application/json;q=0.9,*/*;q=0.8".to_string(),
            ),
            ("Accept-Language".to_string(), "en-US,en;q=0.9".to_string()),
        ];

        let (valid_url, response) = follow(start, cookies, &headers, |url, headers| {
            http.get(url, headers)
        })?;
        if !response.ok() {
            return Err(Error::tool(format!(
                "could not read {} (HTTP {})",
                host_of(&valid_url),
                response.status
            )));
        }

        let content_type = response
            .content_type
            .as_deref()
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        let is_text = content_type.contains("text/")
            || content_type.contains("application/xhtml")
            || content_type.contains("application/json")
            || content_type.contains("application/xml");

        if !is_text {
            return Err(Error::tool(format!(
                "Unsupported content type: {content_type}. Only text/HTML pages can be fetched."
            )));
        }

        let (title, markdown) = if content_type.contains("html")
            || content_type.contains("xhtml")
            || response.text.trim_start().starts_with('<')
        {
            html_to_markdown(&response.text, Some(&valid_url))
        } else if content_type.contains("json") {
            let pretty = serde_json::from_str::<Value>(&response.text)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| response.text.clone()))
                .unwrap_or_else(|_| response.text.clone());
            (
                "JSON Response".to_string(),
                format!("```json\n{pretty}\n```"),
            )
        } else {
            ("Plain Text".to_string(), response.text.clone())
        };

        let title_line = if !title.is_empty() {
            format!("# {title}\n\n")
        } else {
            String::new()
        };

        Ok(format!("{title_line}{markdown}"))
    }
}

/// Request `start` and follow its redirects, one checked hop at a time — a
/// public page that answers "go to http://127.0.0.1/" must not be obeyed —
/// to the answer that is not one, and the address it came from. With
/// `cookies`, what the site sets on the way is sent back along the same chain
/// and dropped with it ([`Jar`]); without, a chain that set any is said to,
/// since that is the one thing a second call could change.
fn follow(
    start: String,
    cookies: bool,
    headers: &[(String, String)],
    mut get: impl FnMut(&str, &[(String, String)]) -> Result<Response>,
) -> Result<(String, Response)> {
    let mut jar = cookies.then(Jar::new);
    let mut offered = false;
    let mut url = start;
    let mut hops = 0;
    loop {
        let mut sent = headers.to_vec();
        if let Some(cookie) = jar.as_ref().and_then(|jar| jar.header(&url)) {
            sent.push(("Cookie".to_string(), cookie));
        }
        let response = get(&url, &sent)?;
        match jar.as_mut() {
            Some(jar) => jar.store(&url, &response.set_cookies),
            None => offered |= !response.set_cookies.is_empty(),
        }
        let Some(next) = response
            .location
            .as_deref()
            .filter(|_| (300..400).contains(&response.status))
        else {
            return Ok((url, response));
        };
        let next = resolve_relative_url(&url, next);
        if hops == MAX_REDIRECTS {
            let host = host_of(&next);
            return Err(Error::tool(if offered {
                format!("the page kept redirecting ({MAX_REDIRECTS} redirects, last to {host}) and set cookies on the way; call web_fetch again with cookies: true")
            } else {
                format!("the page kept redirecting ({MAX_REDIRECTS} redirects, last to {host}); it may need a sign-in in a browser, so read another source")
            }));
        }
        url = crate::net::validate_public_url(&next)?.1;
        hops += 1;
    }
}

/// The host of an address, for a sentence: never the path or query, which
/// may carry a token.
fn host_of(url: &str) -> &str {
    url.split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .unwrap_or(url)
}

/// Whether `text` is an HTML document rather than text taken out of one: it
/// opens as a document does (a doctype, or its `html`, `head` or `body`
/// element). Judged by what the vendor sent, never by what it was asked for or
/// says it sent. Only a whole document counts: Markdown may quote a tag.
fn looks_like_html(text: &str) -> bool {
    let head: String = text
        .trim_start_matches('\u{feff}')
        .trim_start()
        .chars()
        .take(15)
        .collect::<String>()
        .to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || ["<html", "<head", "<body"].iter().any(|tag| {
            head.strip_prefix(tag)
                .is_some_and(|rest| rest.starts_with(['>', ' ', '\t', '\n', '\r']))
        })
}

/// A page another service may be asked to read: an http(s) address whose
/// host — as spelled and as DNS answers it now — is public. What the built-in
/// reader may not reach, no service is sent to.
fn public_page(raw_url: &str) -> Result<String> {
    let (uri, url) = crate::net::validate_public_url(raw_url)?;
    let host = uri
        .host()
        .unwrap_or("")
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("http") {
            80
        } else {
            443
        });
    if host.parse::<std::net::IpAddr>().is_err() {
        use std::net::ToSocketAddrs;
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::invalid(format!("could not resolve {host}: {e}")))?;
        if addresses
            .into_iter()
            .any(|a| crate::net::is_private(a.ip()))
        {
            return Err(Error::invalid(
                "Cannot fetch internal/private network URLs.",
            ));
        }
    }
    Ok(url)
}

// ─── In-Tree HTML to Markdown Engine ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct Element {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

#[derive(Debug, Clone)]
enum Node {
    Element(Element),
    Text(String),
}

/// Convert an HTML document to clean, formatted Markdown.
/// Extracts the title and primary article body, stripping noise elements like scripts/styles/nav/footers/sidebars/interlanguage.
fn html_to_markdown(html: &str, base_url: Option<&str>) -> (String, String) {
    let (title_opt, roots) = parse_html(html);

    // Identify primary content container (article, main, #bodyContent, etc.)
    let content_nodes = find_main_content(&roots).unwrap_or(&roots);

    let mut ctx = RenderContext {
        base_url,
        strip_boilerplate: true,
        list_depth: 0,
        in_pre: false,
    };
    let mut md = clean_markdown(&render_children(content_nodes, &mut ctx));

    // Fallback 1: If main container rendered too sparse, try whole document
    if md.len() < 80 && content_nodes.as_ptr() != roots.as_slice().as_ptr() {
        md = clean_markdown(&render_children(&roots, &mut ctx));
    }
    // Fallback 2: If still too sparse, relax boilerplate stripping
    if md.len() < 80 {
        ctx.strip_boilerplate = false;
        md = clean_markdown(&render_children(&roots, &mut ctx));
    }

    let title = title_opt
        .or_else(|| extract_first_h1(content_nodes))
        .or_else(|| extract_first_h1(&roots))
        .unwrap_or_else(|| {
            base_url
                .and_then(|u| u.split("://").nth(1))
                .and_then(|r| r.split(['/', '?', '#']).next())
                .unwrap_or("Web Page")
                .to_string()
        });

    (title, md)
}

struct RenderContext<'a> {
    base_url: Option<&'a str>,
    strip_boilerplate: bool,
    list_depth: usize,
    in_pre: bool,
}

fn parse_html(html: &str) -> (Option<String>, Vec<Node>) {
    let mut title = None;
    let mut roots: Vec<Node> = Vec::new();
    let mut stack: Vec<Element> = Vec::new();

    let mut i = 0;
    let bytes = html.as_bytes();
    let len = bytes.len();

    while i < len {
        if bytes[i] == b'<' {
            // Check comment
            if i + 3 < len && &bytes[i..i + 4] == b"<!--" {
                if let Some(end) = html[i + 4..].find("-->") {
                    i += 4 + end + 3;
                    continue;
                } else {
                    break;
                }
            }
            // Check doctype / cdata
            if i + 1 < len && bytes[i + 1] == b'!' {
                if let Some(end) = html[i..].find('>') {
                    i += end + 1;
                    continue;
                } else {
                    break;
                }
            }
            // Check closing tag
            if i + 1 < len && bytes[i + 1] == b'/' {
                if let Some(end) = html[i..].find('>') {
                    let tag_name = html[i + 2..i + end].trim().to_ascii_lowercase();
                    let tag_clean = tag_name.split([' ', '\t', '\r', '\n']).next().unwrap_or("");
                    close_tag(&mut stack, &mut roots, tag_clean);
                    i += end + 1;
                    continue;
                } else {
                    break;
                }
            }

            // Opening or self-closing tag
            if let Some(end) = html[i..].find('>') {
                let tag_content = &html[i + 1..i + end];
                let is_self_closing = tag_content.ends_with('/');
                let tag_inner = if is_self_closing {
                    tag_content[..tag_content.len() - 1].trim()
                } else {
                    tag_content.trim()
                };

                let mut parts = tag_inner.splitn(2, |c: char| c.is_whitespace());
                let tag_name = parts.next().unwrap_or("").to_ascii_lowercase();
                let attr_str = parts.next().unwrap_or("");

                i += end + 1;

                if tag_name.is_empty() {
                    continue;
                }

                // Handle <title>
                if tag_name == "title" {
                    if let Some(close_idx) = html[i..].find("</title>") {
                        let t = decode_entities(html[i..i + close_idx].trim());
                        if !t.is_empty() && title.is_none() {
                            title = Some(t);
                        }
                        i += close_idx + 8;
                        continue;
                    }
                }

                // Discard non-content tags completely: script, style, noscript, svg, canvas, template
                if matches!(
                    tag_name.as_str(),
                    "script" | "style" | "noscript" | "svg" | "canvas" | "template"
                ) {
                    let closing = format!("</{tag_name}>");
                    if let Some(close_idx) = html[i..].to_ascii_lowercase().find(&closing) {
                        i += close_idx + closing.len();
                        continue;
                    } else {
                        break;
                    }
                }

                let attrs = parse_attrs(attr_str);
                let element = Element {
                    tag: tag_name.clone(),
                    attrs,
                    children: Vec::new(),
                };

                let is_void = matches!(
                    tag_name.as_str(),
                    "area"
                        | "base"
                        | "br"
                        | "col"
                        | "embed"
                        | "hr"
                        | "img"
                        | "input"
                        | "link"
                        | "meta"
                        | "param"
                        | "source"
                        | "track"
                        | "wbr"
                );

                if is_self_closing || is_void {
                    append_node(&mut stack, &mut roots, Node::Element(element));
                } else {
                    if (tag_name == "p" || tag_name == "li")
                        && stack.last().is_some_and(|e| e.tag == tag_name)
                    {
                        pop_one(&mut stack, &mut roots);
                    }
                    stack.push(element);
                }
                continue;
            } else {
                break;
            }
        }

        // Text content
        let next_tag = html[i..].find('<').unwrap_or(len - i);
        let text_raw = &html[i..i + next_tag];
        i += next_tag;

        if !text_raw.is_empty() {
            let decoded = decode_entities(text_raw);
            append_node(&mut stack, &mut roots, Node::Text(decoded));
        }
    }

    while !stack.is_empty() {
        pop_one(&mut stack, &mut roots);
    }

    (title, roots)
}

fn append_node(stack: &mut [Element], roots: &mut Vec<Node>, node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        roots.push(node);
    }
}

fn pop_one(stack: &mut Vec<Element>, roots: &mut Vec<Node>) {
    if let Some(elem) = stack.pop() {
        append_node(stack, roots, Node::Element(elem));
    }
}

fn close_tag(stack: &mut Vec<Element>, roots: &mut Vec<Node>, tag: &str) {
    if let Some(pos) = stack.iter().rposition(|e| e.tag == tag) {
        while stack.len() > pos {
            pop_one(stack, roots);
        }
    }
}

fn parse_attrs(attr_str: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut chars = attr_str.trim().chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '=' || c == '/' || c == '>' {
                break;
            }
            name.push(chars.next().unwrap());
        }
        if name.is_empty() {
            break;
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'=') {
            chars.next();
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            if let Some(&quote) = chars.peek() {
                if quote == '"' || quote == '\'' {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == quote {
                            break;
                        }
                        value.push(c);
                    }
                } else {
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() || c == '>' {
                            break;
                        }
                        value.push(chars.next().unwrap());
                    }
                }
            }
        }
        attrs.push((name.to_ascii_lowercase(), decode_entities(&value)));
    }
    attrs
}

fn render_children(nodes: &[Node], ctx: &mut RenderContext<'_>) -> String {
    let mut out = String::new();
    for node in nodes {
        render_node(node, &mut out, ctx);
    }
    out
}

fn render_node(node: &Node, out: &mut String, ctx: &mut RenderContext<'_>) {
    match node {
        Node::Text(text) => {
            if ctx.in_pre {
                out.push_str(text);
            } else {
                let mut prev_space = out.ends_with(' ') || out.ends_with('\n');
                for ch in text.chars() {
                    if ch.is_whitespace() {
                        if !prev_space {
                            out.push(' ');
                            prev_space = true;
                        }
                    } else {
                        out.push(ch);
                        prev_space = false;
                    }
                }
            }
        }
        Node::Element(el) => {
            if ctx.strip_boilerplate && is_noise_element(el) {
                return;
            }

            match el.tag.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = el.tag[1..].parse::<usize>().unwrap_or(1).min(6);
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(&format!("{} {}", "#".repeat(level), trimmed));
                        ensure_block_boundary(out);
                    }
                }
                "p" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(trimmed);
                        ensure_block_boundary(out);
                    }
                }
                "br" => out.push('\n'),
                "hr" => {
                    ensure_block_boundary(out);
                    out.push_str("---");
                    ensure_block_boundary(out);
                }
                "strong" | "b" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("**{}**", trimmed));
                    }
                }
                "em" | "i" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("*{}*", trimmed));
                    }
                }
                "s" | "del" | "strike" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        out.push_str(&format!("~~{}~~", trimmed));
                    }
                }
                "pre" => {
                    let prev_in_pre = ctx.in_pre;
                    ctx.in_pre = true;
                    let mut lang = String::new();
                    for child in &el.children {
                        if let Node::Element(c_el) = child {
                            if c_el.tag == "code" {
                                if let Some((_, class)) =
                                    c_el.attrs.iter().find(|(k, _)| k == "class")
                                {
                                    for part in class.split_whitespace() {
                                        if let Some(l) = part
                                            .strip_prefix("language-")
                                            .or_else(|| part.strip_prefix("lang-"))
                                        {
                                            lang = l.to_string();
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let inner = extract_raw_text(&el.children);
                    ctx.in_pre = prev_in_pre;
                    ensure_block_boundary(out);
                    out.push_str(&format!("```{}\n{}\n```", lang, inner.trim_matches('\n')));
                    ensure_block_boundary(out);
                }
                "code" => {
                    if ctx.in_pre {
                        out.push_str(&extract_raw_text(&el.children));
                    } else {
                        let inner = extract_raw_text(&el.children);
                        let trimmed = inner.trim();
                        if !trimmed.is_empty() {
                            out.push_str(&format!("`{}`", trimmed.replace('`', "'")));
                        }
                    }
                }
                "blockquote" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        ensure_block_boundary(out);
                        for line in trimmed.lines() {
                            out.push_str(&format!("> {}\n", line));
                        }
                        ensure_block_boundary(out);
                    }
                }
                "ul" => {
                    let list = render_list(el, false, ctx);
                    if !list.trim().is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(&list);
                        ensure_block_boundary(out);
                    }
                }
                "ol" => {
                    let list = render_list(el, true, ctx);
                    if !list.trim().is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(&list);
                        ensure_block_boundary(out);
                    }
                }
                "a" => {
                    let href = el
                        .attrs
                        .iter()
                        .find(|(k, _)| k == "href")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");

                    // Check if this anchor only wraps an image
                    let non_empty_children: Vec<&Node> = el
                        .children
                        .iter()
                        .filter(|n| match n {
                            Node::Element(_) => true,
                            Node::Text(t) => !t.trim().is_empty(),
                        })
                        .collect();

                    if non_empty_children.len() == 1 {
                        if let Node::Element(c_el) = non_empty_children[0] {
                            if c_el.tag == "img" {
                                render_node(non_empty_children[0], out, ctx);
                                return;
                            }
                        }
                    }

                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();

                    // If it's a citation marker like [47] or [g] or [edit]
                    if href.starts_with("#cite_")
                        || href.starts_with("#fn")
                        || is_citation_text(trimmed)
                    {
                        return;
                    }

                    // If anchor link to on-page section, keep clean text without link syntax
                    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
                        out.push_str(trimmed);
                        return;
                    }

                    // Collapse internal whitespace/newlines so link text is clean
                    let clean_text = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
                    if clean_text.is_empty() {
                        return;
                    }

                    let resolved = resolve_relative_url(ctx.base_url.unwrap_or(""), href);
                    out.push_str(&format!("[{clean_text}]({resolved})"));
                }
                "img" => {
                    let src = el
                        .attrs
                        .iter()
                        .find(|(k, _)| k == "src")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let alt = el
                        .attrs
                        .iter()
                        .find(|(k, _)| k == "alt")
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");

                    let raw_alt = alt.trim();
                    if src.is_empty() {
                        return;
                    }

                    let clean_src = clean_image_url(src);
                    if clean_src.ends_with(".svg")
                        || clean_src.contains("/icon")
                        || clean_src.contains("button")
                        || clean_src.contains("spacer")
                        || clean_src.contains("1x1")
                    {
                        return;
                    }

                    let final_alt = if raw_alt == "!" || raw_alt.is_empty() {
                        derive_alt_from_url(&clean_src)
                    } else {
                        raw_alt.split_whitespace().collect::<Vec<_>>().join(" ")
                    };

                    if !final_alt.is_empty() {
                        let resolved = resolve_relative_url(ctx.base_url.unwrap_or(""), &clean_src);
                        ensure_block_boundary(out);
                        out.push_str(&format!("![{final_alt}]({resolved})"));
                        ensure_block_boundary(out);
                    }
                }
                "table" => {
                    let tbl = render_table(el, ctx);
                    if !tbl.trim().is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(&tbl);
                        ensure_block_boundary(out);
                    }
                }
                "div" | "section" | "article" | "figure" | "figcaption" => {
                    let inner = render_children(&el.children, ctx);
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        ensure_block_boundary(out);
                        out.push_str(trimmed);
                        ensure_block_boundary(out);
                    }
                }
                _ => {
                    let inner = render_children(&el.children, ctx);
                    out.push_str(&inner);
                }
            }
        }
    }
}

fn ensure_block_boundary(out: &mut String) {
    let trimmed = out.trim_end_matches([' ', '\t']);
    if trimmed.is_empty() {
        out.clear();
        return;
    }
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
}

fn clean_image_url(url: &str) -> String {
    if let Some((clean, _)) = url.split_once('?') {
        clean.to_string()
    } else {
        url.to_string()
    }
}

fn derive_alt_from_url(url: &str) -> String {
    let clean = clean_image_url(url);
    let filename = clean.split('/').next_back().unwrap_or("");
    let name_part = filename.split('.').next().unwrap_or("");
    let clean_name = if let Some(idx) = name_part.find("px-") {
        &name_part[idx + 3..]
    } else {
        name_part
    };
    let decoded = clean_name.replace(['_', '-'], " ");
    let trimmed = decoded.trim();
    if trimmed.is_empty()
        || trimmed.len() < 3
        || trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_punctuation())
    {
        String::new()
    } else {
        trimmed.to_string()
    }
}

fn is_citation_text(s: &str) -> bool {
    let t = s.trim_matches(|c: char| c == '[' || c == ']' || c.is_whitespace());
    if t.is_empty() {
        return false;
    }
    t == "edit"
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()))
}

fn is_noise_element(el: &Element) -> bool {
    // 1. Non-content or interactive tags
    if matches!(
        el.tag.as_str(),
        "script"
            | "style"
            | "noscript"
            | "svg"
            | "canvas"
            | "template"
            | "iframe"
            | "button"
            | "form"
            | "input"
            | "select"
            | "textarea"
            | "dialog"
            | "nav"
            | "footer"
            | "aside"
            | "header"
    ) {
        return true;
    }

    // 2. Structural boilerplate ARIA roles
    if el.attrs.iter().any(|(k, v)| {
        k == "role"
            && matches!(
                v.to_ascii_lowercase().as_str(),
                "navigation" | "banner" | "complementary" | "contentinfo" | "search"
            )
    }) {
        return true;
    }

    // 3. Hidden elements
    if el.attrs.iter().any(|(k, v)| {
        (k == "aria-hidden" && v.eq_ignore_ascii_case("true"))
            || k == "hidden"
            || (k == "style" && {
                let s = v.to_ascii_lowercase();
                s.contains("display:none")
                    || s.contains("display: none")
                    || s.contains("visibility:hidden")
                    || s.contains("visibility: hidden")
            })
    }) {
        return true;
    }

    // 4. Class and ID based noise filters
    for (k, v) in &el.attrs {
        if k == "class" || k == "id" {
            let lower = v.to_ascii_lowercase();
            for token in lower.split(|c: char| c.is_whitespace() || c == '-' || c == '_') {
                if matches!(
                    token,
                    "reference"
                        | "editsection"
                        | "navbox"
                        | "catlinks"
                        | "printfooter"
                        | "sitenotice"
                        | "jumplink"
                        | "sidebar"
                        | "widget"
                        | "social"
                        | "share"
                        | "cookie"
                        | "consent"
                        | "banner"
                        | "promo"
                        | "advert"
                        | "ad"
                        | "disqus"
                        | "comment"
                        | "comments"
                        | "authorbio"
                        | "newsletter"
                        | "subscribe"
                ) {
                    return true;
                }
            }
            if lower.contains("interlanguage-link")
                || lower.contains("mw-portlet-lang")
                || lower.contains("mw-editsection")
                || lower.contains("cite_ref")
                || lower.contains("cite_note")
                || lower.contains("p-lang")
                || lower.contains("vector-menu")
            {
                return true;
            }
            if el.tag == "div" && (lower == "toc" || lower.contains("mw-toc")) {
                return true;
            }
        }
    }

    false
}

fn find_main_content(nodes: &[Node]) -> Option<&[Node]> {
    // 1. Wikipedia article body specifically: #bodyContent, #mw-content-text, .mw-parser-output
    if let Some(children) = find_element_recursive(nodes, &|el| {
        el.attrs.iter().any(|(k, v)| {
            (k == "id"
                && (v.eq_ignore_ascii_case("bodycontent")
                    || v.eq_ignore_ascii_case("mw-content-text")))
                || (k == "class"
                    && v.split_whitespace()
                        .any(|c| c.eq_ignore_ascii_case("mw-parser-output")))
        })
    }) {
        if count_text_length(children) > 150 {
            return Some(children);
        }
    }

    // 2. <article> tag
    if let Some(children) = find_element_recursive(nodes, &|el| el.tag == "article") {
        if count_text_length(children) > 150 {
            return Some(children);
        }
    }

    // 3. <main> or [role="main"]
    if let Some(children) = find_element_recursive(nodes, &|el| {
        el.tag == "main"
            || el
                .attrs
                .iter()
                .any(|(k, v)| k == "role" && v.eq_ignore_ascii_case("main"))
    }) {
        if count_text_length(children) > 150 {
            return Some(children);
        }
    }

    // 4. Typical CMS/documentation article containers
    if let Some(children) = find_element_recursive(nodes, &|el| {
        el.attrs.iter().any(|(k, v)| {
            if k == "id" {
                matches!(
                    v.to_ascii_lowercase().as_str(),
                    "main-content" | "content" | "main" | "article" | "post"
                )
            } else if k == "class" {
                v.split_whitespace().any(|c| {
                    matches!(
                        c.to_ascii_lowercase().as_str(),
                        "article-content"
                            | "post-content"
                            | "entry-content"
                            | "markdown-body"
                            | "content-area"
                    )
                })
            } else {
                false
            }
        })
    }) {
        if count_text_length(children) > 150 {
            return Some(children);
        }
    }

    None
}

fn find_element_recursive<'a, F>(nodes: &'a [Node], predicate: &F) -> Option<&'a [Node]>
where
    F: Fn(&Element) -> bool,
{
    for node in nodes {
        if let Node::Element(el) = node {
            if predicate(el) {
                return Some(&el.children);
            }
            if let Some(res) = find_element_recursive(&el.children, predicate) {
                return Some(res);
            }
        }
    }
    None
}

fn count_text_length(nodes: &[Node]) -> usize {
    let mut total = 0;
    for node in nodes {
        match node {
            Node::Text(t) => total += t.trim().len(),
            Node::Element(el) => total += count_text_length(&el.children),
        }
    }
    total
}

fn render_list(el: &Element, is_ordered: bool, ctx: &mut RenderContext<'_>) -> String {
    let mut out = String::new();
    let mut item_idx = 1;
    let indent = "  ".repeat(ctx.list_depth);
    ctx.list_depth += 1;

    for child in &el.children {
        if let Node::Element(c_el) = child {
            if c_el.tag == "li" {
                let bullet = if is_ordered {
                    let b = format!("{}{}. ", indent, item_idx);
                    item_idx += 1;
                    b
                } else {
                    format!("{}- ", indent)
                };
                let content = render_children(&c_el.children, ctx);
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    out.push_str(&format!("{}{}\n", bullet, trimmed));
                }
            }
        }
    }

    ctx.list_depth -= 1;
    out
}

fn render_table(el: &Element, ctx: &mut RenderContext<'_>) -> String {
    let mut rows: Vec<Vec<String>> = Vec::new();
    collect_table_rows(&el.children, &mut rows, ctx);
    if rows.is_empty() {
        return String::new();
    }

    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if max_cols == 0 {
        return String::new();
    }

    let mut out = String::from("\n\n");

    let header = &rows[0];
    out.push('|');
    for col_idx in 0..max_cols {
        let cell = header.get(col_idx).map(|s| s.as_str()).unwrap_or("");
        out.push_str(&format!(
            " {} |",
            cell.replace('|', "\\|").replace('\n', " ")
        ));
    }
    out.push('\n');

    out.push('|');
    for _ in 0..max_cols {
        out.push_str(" --- |");
    }
    out.push('\n');

    for row in rows.iter().skip(1) {
        out.push('|');
        for col_idx in 0..max_cols {
            let cell = row.get(col_idx).map(|s| s.as_str()).unwrap_or("");
            out.push_str(&format!(
                " {} |",
                cell.replace('|', "\\|").replace('\n', " ")
            ));
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

fn collect_table_rows(nodes: &[Node], rows: &mut Vec<Vec<String>>, ctx: &mut RenderContext<'_>) {
    for node in nodes {
        match node {
            Node::Element(el) => {
                if el.tag == "tr" {
                    let mut cells = Vec::new();
                    for child in &el.children {
                        if let Node::Element(c_el) = child {
                            if c_el.tag == "th" || c_el.tag == "td" {
                                let content =
                                    render_children(&c_el.children, ctx).trim().to_string();
                                cells.push(content);
                            }
                        }
                    }
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                } else {
                    collect_table_rows(&el.children, rows, ctx);
                }
            }
            Node::Text(_) => {}
        }
    }
}

fn extract_raw_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element(el) => out.push_str(&extract_raw_text(&el.children)),
        }
    }
    out
}

fn extract_first_h1(nodes: &[Node]) -> Option<String> {
    for node in nodes {
        if let Node::Element(el) = node {
            if el.tag == "h1" {
                let text = extract_raw_text(&el.children).trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
            if let Some(h1) = extract_first_h1(&el.children) {
                return Some(h1);
            }
        }
    }
    None
}

fn resolve_relative_url(base: &str, relative: &str) -> String {
    let rel = relative.trim();
    if rel.starts_with("http://")
        || rel.starts_with("https://")
        || rel.starts_with("mailto:")
        || rel.starts_with('#')
    {
        return rel.to_string();
    }
    if rel.starts_with("//") {
        return format!("https:{rel}");
    }
    let Some((scheme, rest)) = base.split_once("://") else {
        return rel.to_string();
    };
    let authority = rest.split('/').next().unwrap_or("");
    if rel.starts_with('/') {
        return format!("{scheme}://{authority}{rel}");
    }
    let base_path = rest.strip_prefix(authority).unwrap_or("");
    let dir = match base_path.rfind('/') {
        Some(idx) => &base_path[..=idx],
        None => "/",
    };
    format!("{scheme}://{authority}{dir}{rel}")
}

fn clean_markdown(md: &str) -> String {
    let mut out = String::new();
    let mut blank_count = 0;
    for line in md.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                out.push('\n');
            }
        } else {
            blank_count = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fetch_schema_requires_only_a_url_and_cannot_search() {
        let tool = WebFetch::new(
            Arc::new(Limits::default()),
            Arc::new(Mutex::new(Settings::empty())),
        );
        let schema = tool.declaration().parameters;
        assert_eq!(schema["required"], json!(["action", "url"]));
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(
            props.keys().map(String::as_str).collect::<Vec<_>>(),
            ["action", "url", "cookies"]
        );
        assert_eq!(props["cookies"]["type"], "boolean");
        tool.validate(&json!({"action":"read docs", "url":"https://example.com/docs"}))
            .unwrap();
        for cookies in [true, false] {
            tool.validate(
                &json!({"action":"read docs", "url":"https://example.com/docs", "cookies": cookies}),
            )
            .unwrap();
        }
        for args in [
            json!({"action":"read docs", "query":"https://example.com/"}),
            json!({"action":"read docs", "url":""}),
            json!({"action":"read docs", "url":"https://example.com/", "type":"fetch"}),
            json!({"action":"read docs", "url":"https://example.com/", "count":3}),
        ] {
            assert!(tool.validate(&args).is_err(), "{args}");
        }
        assert_eq!(
            tool.preview(&json!({"url":"https://example.com/docs"})),
            "fetch \"https://example.com/docs\""
        );
        assert_eq!(
            tool.preview(&json!({"url":"https://example.com/docs", "cookies": true})),
            "fetch \"https://example.com/docs\" with cookies"
        );
        assert!(!tool.mutates());
        assert!(tool.concurrent());
    }

    /// A site that answers an anonymous reader with a sign-in check, as
    /// ai.google.dev did (2026-10-21): its page redirects through a sign-in
    /// host and a callback that sends the reader back to the page only with
    /// the cookie set at the start — without it, round again.
    fn sign_in_check(
        requests: &mut Vec<(String, Option<String>)>,
    ) -> impl FnMut(&str, &[(String, String)]) -> Result<Response> + '_ {
        move |url, headers| {
            let cookie = headers
                .iter()
                .find(|(k, _)| k == "Cookie")
                .map(|(_, v)| v.clone());
            requests.push((url.to_string(), cookie.clone()));
            let has = |c: &str| {
                cookie
                    .as_deref()
                    .is_some_and(|h| h.split("; ").any(|p| p == c))
            };
            let (status, location, set): (u16, Option<&str>, &[&str]) = match url {
                "https://ai.site.test/page" if has("done=1") => (200, None, &[]),
                "https://ai.site.test/page" => (
                    302,
                    Some("/oauth?return=%2Fpage"),
                    &[
                        "csrf=1; Path=/; Secure; HttpOnly",
                        "session=; Max-Age=0; Path=/",
                    ],
                ),
                "https://ai.site.test/oauth?return=%2Fpage" => {
                    (302, Some("https://accounts.site.test/auth?client=x"), &[])
                }
                "https://accounts.site.test/auth?client=x" => (
                    302,
                    Some("https://ai.site.test/callback?state=s"),
                    &["acct=anon; Domain=site.test; Path=/; Secure"],
                ),
                "https://ai.site.test/callback?state=s" if has("csrf=1") => (
                    302,
                    Some("https://ai.site.test/page"),
                    &["done=1; Path=/; Secure"],
                ),
                "https://ai.site.test/callback?state=s" => {
                    (302, Some("https://ai.site.test/page"), &[])
                }
                other => panic!("unexpected request {other}"),
            };
            Ok(Response {
                status,
                body: Value::Null,
                text: if status == 200 {
                    "<html><body><p>Prices</p></body></html>".into()
                } else {
                    String::new()
                },
                content_type: Some("text/html".into()),
                location: location.map(String::from),
                set_cookies: set.iter().map(|s| s.to_string()).collect(),
            })
        }
    }

    /// Without cookies the sign-in check loops and the failure says the one
    /// thing that would change it; with them the page is read, each cookie
    /// sent back only to the host that set it, and nothing outlives the call.
    #[test]
    fn a_sign_in_redirect_loop_is_read_with_cookies_and_named_without() {
        let mut requests = Vec::new();
        let error = follow(
            "https://ai.site.test/page".into(),
            false,
            &[],
            &mut sign_in_check(&mut requests),
        )
        .err()
        .expect("the loop fails");
        assert!(matches!(error, Error::Tool(_)), "{error:?}");
        let said = error.to_string();
        assert!(
            said.contains("kept redirecting (5 redirects, last to "),
            "{said}"
        );
        assert!(
            said.contains("call web_fetch again with cookies: true"),
            "{said}"
        );
        assert!(
            !said.contains("oauth") && !said.contains("state="),
            "only a host: {said}"
        );
        assert_eq!(requests.len(), MAX_REDIRECTS + 1);
        assert!(requests.iter().all(|(_, cookie)| cookie.is_none()));

        let mut requests = Vec::new();
        let (url, page) = follow(
            "https://ai.site.test/page".into(),
            true,
            &[("Accept".into(), "text/html".into())],
            &mut sign_in_check(&mut requests),
        )
        .unwrap();
        assert_eq!(
            (url.as_str(), page.status),
            ("https://ai.site.test/page", 200)
        );
        assert_eq!(requests.len(), 5, "four redirects, then the page");
        for (url, cookie) in &requests {
            let cookie = cookie.as_deref().unwrap_or("");
            if url.starts_with("https://accounts.site.test/") {
                assert_eq!(cookie, "", "the sign-in host gets nothing the page set");
            } else {
                assert!(!cookie.contains("acct="), "{url}: {cookie}");
            }
        }
        assert_eq!(requests[3].1.as_deref(), Some("csrf=1"));
        assert_eq!(requests[4].1.as_deref(), Some("csrf=1; done=1"));
    }

    /// A chain that loops without setting a cookie gives a different next
    /// step, and so does one still looping with cookies kept; every hop is
    /// held to the public-address rule, mid-chain too.
    #[test]
    fn a_loop_cookies_cannot_change_says_so_and_every_hop_is_checked() {
        let mut bounce = |url: &str, _: &[(String, String)]| {
            Ok(Response {
                status: 301,
                body: Value::Null,
                text: String::new(),
                content_type: None,
                location: Some(if url.ends_with("/a") { "/b" } else { "/a" }.into()),
                set_cookies: Vec::new(),
            })
        };
        for cookies in [false, true] {
            let said = follow("https://loop.test/a".into(), cookies, &[], &mut bounce)
                .err()
                .expect("the loop fails")
                .to_string();
            assert!(said.contains("last to loop.test"), "{said}");
            assert!(
                said.contains("sign-in in a browser, so read another source"),
                "{said}"
            );
            assert!(!said.contains("cookies: true"), "{said}");
        }
        let mut inward = |_: &str, _: &[(String, String)]| {
            Ok(Response {
                status: 302,
                body: Value::Null,
                text: String::new(),
                content_type: None,
                location: Some("http://127.0.0.1/admin".into()),
                set_cookies: vec!["a=1".into()],
            })
        };
        assert!(
            follow("https://public.test/".into(), true, &[], &mut inward)
                .err()
                .expect("the inward hop is refused")
                .to_string()
                .contains("internal/private")
        );
    }

    #[test]
    fn fetched_output_uses_only_its_own_hot_reloaded_limit() {
        let home = std::env::temp_dir().join(format!("rune-fetch-limit-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("rune.toml"), "[web_search]\nmax_output_chars = 2000\ntimeout = \"2s\"\n[web_fetch]\nmax_output_chars = 100\ntimeout = \"7s\"\n").unwrap();
        let config = crate::settings::configuration::Shared::load(&home).unwrap();
        let tool = WebFetch::new(
            Arc::new(Limits::over(config.clone())),
            Arc::new(Mutex::new(Settings::empty())),
        );
        let text = "é".repeat(200);
        let first = tool.outcome(text.clone(), None);
        assert!(first.content.contains("only the first 100 are shown"));
        assert!(first.content.ends_with(&"é".repeat(100)));
        assert_eq!(first.detail, Some(json!({"results":0, "fetched":1})));
        assert_eq!(
            config.duration("web_fetch.timeout"),
            std::time::Duration::from_secs(7)
        );
        std::fs::write(home.join("rune.toml"), "[web_search]\nmax_output_chars = 100\ntimeout = \"1s\"\n[web_fetch]\nmax_output_chars = 1000\ntimeout = \"9s\"\n").unwrap();
        config.reload().unwrap();
        assert_eq!(tool.outcome(text.clone(), None).content, text);
        assert_eq!(
            config.duration("web_fetch.timeout"),
            std::time::Duration::from_secs(9)
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn fetch_refuses_private_urls_and_cancellation_before_any_request() {
        let tool = WebFetch::new(
            Arc::new(Limits::default()),
            Arc::new(Mutex::new(Settings::empty())),
        );
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for url in [
            "http://localhost:1234/",
            "https://127.0.0.1/",
            "http://169.254.169.254/",
            "http://[::1]/",
            "file:///etc/passwd",
        ] {
            assert!(tool.fetch(url, false, &cancel).is_err(), "{url}");
        }
        // A relative redirect is resolved, then held by the same URL gate.
        let redirect = resolve_relative_url("https://example.com/docs", "http://10.0.0.1/private");
        assert!(crate::net::validate_public_url(&redirect).is_err());
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            tool.fetch("https://example.com/", false, &cancel),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn scripts_and_styles_never_return_even_in_the_sparse_page_fallback() {
        let html = r#"<html><head><style>CSS_MARKER { display: none }</style><script>JS_MARKER()</script></head><body><p>Keep this text.</p><script src="/bundle.js">JS_BODY_MARKER()</script><style>CSS_BODY_MARKER { color: red }</style></body></html>"#;
        let (_, markdown) = html_to_markdown(html, Some("https://example.com/"));
        assert!(markdown.contains("Keep this text."), "{markdown}");
        for unwanted in [
            "CSS_MARKER",
            "JS_MARKER",
            "JS_BODY_MARKER",
            "CSS_BODY_MARKER",
            "bundle.js",
        ] {
            assert!(!markdown.contains(unwanted), "{markdown}");
        }
    }

    #[test]
    fn html_to_markdown_converts_rich_html() {
        let html = r#"
        <!DOCTYPE html>
        <html>
        <head>
            <title>Test Page Title</title>
            <style>body { color: red; }</style>
            <script>alert("ignore me");</script>
        </head>
        <body>
            <nav><a href="/">Home</a><a href="/docs">Docs</a></nav>
            <h1>Main Heading</h1>
            <p>This is a paragraph with <strong>bold text</strong>, <em>italic</em>, a <a href="/guide.html">link</a>, and <a href="subpage.html">subpage</a>.</p>
            <ul>
                <li>First item</li>
                <li>Second item with <code>inline code</code></li>
            </ul>
            <pre><code class="language-rust">fn main() {
    println!("Hello, world!");
}</code></pre>
            <table>
                <thead>
                    <tr><th>Command</th><th>Description</th></tr>
                </thead>
                <tbody>
                    <tr><td>cargo build</td><td>Build the project</td></tr>
                </tbody>
            </table>
            <blockquote>
                <p>Important quote here</p>
            </blockquote>
            <footer>Copyright 2026</footer>
        </body>
        </html>
        "#;

        let (title, md) = html_to_markdown(html, Some("https://example.com/docs/"));
        assert_eq!(title, "Test Page Title");
        assert!(md.contains("# Main Heading"), "{md}");
        assert!(md.contains("**bold text**"), "{md}");
        assert!(md.contains("*italic*"), "{md}");
        assert!(
            md.contains("[link](https://example.com/guide.html)"),
            "{md}"
        );
        assert!(
            md.contains("[subpage](https://example.com/docs/subpage.html)"),
            "{md}"
        );
        assert!(md.contains("- First item"), "{md}");
        assert!(md.contains("`inline code`"), "{md}");
        assert!(md.contains("```rust"), "{md}");
        assert!(md.contains("println!(\"Hello, world!\");"), "{md}");
        assert!(md.contains("| Command | Description |"), "{md}");
        assert!(md.contains("| cargo build | Build the project |"), "{md}");
        assert!(md.contains("> Important quote here"), "{md}");
        // Scripts, styles, and footer navigation should be stripped
        assert!(!md.contains("alert("), "{md}");
        assert!(!md.contains("color: red"), "{md}");
        assert!(!md.contains("Copyright 2026"), "{md}");
    }

    #[test]
    fn html_to_markdown_decodes_entities() {
        let html = "<p>&quot;Hello &amp; World&quot; &mdash; 5 &gt; 3 &lt; 4 &#39;test&#39;</p>";
        let (_, md) = html_to_markdown(html, None);
        assert!(md.contains("\"Hello & World\" — 5 > 3 < 4 'test'"), "{md}");
    }

    #[test]
    fn html_to_markdown_wikipedia_article_cleaning() {
        let wiki_html = r##"
        <!DOCTYPE html>
        <html>
        <head><title>Artificial intelligence - Wikipedia</title></head>
        <body class="skin-vector">
            <div id="mw-navigation">
                <div id="mw-head"><a href="/wiki/Main_Page">Main Page</a></div>
                <div id="mw-panel">
                    <div id="p-lang" class="vector-menu">
                        <ul>
                            <li><a href="https://sk.wikipedia.org/wiki/Umel%C3%A1_inteligencia">Slovenčina</a></li>
                            <li><a href="https://sl.wikipedia.org/wiki/Umetna_inteligenca">Slovenščina</a></li>
                            <li><a href="https://ta.wikipedia.org/wiki/%E0%AE%9A">தமிழ் </a></li>
                        </ul>
                    </div>
                </div>
            </div>
            <div id="content" class="mw-body" role="main">
                <h1 id="firstHeading">Artificial intelligence</h1>
                <div id="bodyContent">
                    <div id="mw-content-text">
                        <div class="mw-parser-output">
                            <p><b>Artificial intelligence</b> (<b>AI</b>) is the capability of computational systems to perform tasks.<sup class="reference" id="cite_ref-1"><a href="#cite_note-1">[1]</a></sup></p>
                            <h2>Machine learning<span class="mw-editsection"><a href="/w/index.php?action=edit">[edit]</a></span></h2>
                            <div class="thumb tright">
                                <div class="thumbinner">
                                    <a href="/wiki/File:Supervised_and_unsupervised_learning.png" class="image">
                                        <img alt="!" src="//thumb.wikimedia.org/wikipedia/commons/thumb/4/4d/500px-Supervised_and_unsupervised_learning.png?utm_source=wiki" />
                                    </a>
                                    <div class="thumbcaption">In supervised learning, the training data is labelled.<sup class="reference"><a href="#cite_note-2">[g]</a></sup></div>
                                </div>
                            </div>
                            <p>There are several kinds of machine learning:<sup class="reference"><a href="#cite_note-3">[47]</a></sup></p>
                            <ul>
                                <li>Unsupervised learning analyses a stream of data.<sup class="reference"><a href="#cite_note-4">[48]</a></sup></li>
                            </ul>
                            <table class="navbox"><tr><td>Branches of AI</td></tr></table>
                            <div id="catlinks" class="catlinks"><a href="/wiki/Category:AI">Category: AI</a></div>
                        </div>
                    </div>
                </div>
            </div>
            <div id="footer">Wikipedia footer copyright</div>
        </body>
        </html>
        "##;

        let (title, md) = html_to_markdown(
            wiki_html,
            Some("https://en.wikipedia.org/wiki/Artificial_intelligence"),
        );
        assert_eq!(title, "Artificial intelligence - Wikipedia");

        // Main content is extracted cleanly
        assert!(
            md.contains(
                "**Artificial intelligence** (**AI**) is the capability of computational systems"
            ),
            "{md}"
        );
        assert!(md.contains("## Machine learning"), "{md}");
        assert!(
            md.contains("- Unsupervised learning analyses a stream of data."),
            "{md}"
        );
        assert!(
            md.contains("In supervised learning, the training data is labelled."),
            "{md}"
        );

        // Interlanguage list is completely stripped!
        assert!(
            !md.contains("Slovenčina"),
            "Must not contain interlanguage sidebar: {md}"
        );
        assert!(
            !md.contains("Slovenščina"),
            "Must not contain interlanguage sidebar: {md}"
        );
        assert!(
            !md.contains("தமிழ்"),
            "Must not contain interlanguage sidebar: {md}"
        );

        // Citations, edit links, navboxes, category links, and footer are stripped!
        assert!(!md.contains("[1]"), "Must not contain citation [1]: {md}");
        assert!(!md.contains("[47]"), "Must not contain citation [47]: {md}");
        assert!(!md.contains("[48]"), "Must not contain citation [48]: {md}");
        assert!(!md.contains("[g]"), "Must not contain citation [g]: {md}");
        assert!(
            !md.contains("[edit]"),
            "Must not contain edit section link: {md}"
        );
        assert!(
            !md.contains("Branches of AI"),
            "Must not contain navbox: {md}"
        );
        assert!(
            !md.contains("Category: AI"),
            "Must not contain catlinks: {md}"
        );
        assert!(
            !md.contains("Wikipedia footer"),
            "Must not contain footer: {md}"
        );

        // Image does not have ugly [![!](...)] or glued text )]In
        assert!(!md.contains("[![!]"), "Must not contain nested [![!]: {md}");
        assert!(
            !md.contains(")]In"),
            "Must not glue image link to caption: {md}"
        );
    }

    #[test]
    fn a_vendor_page_is_html_only_when_it_opens_as_a_document() {
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>x</body></html>"
        ));
        assert!(looks_like_html("\u{feff}  \n<html lang=\"en\">"));
        assert!(looks_like_html("<body>\n<p>x</p>"));
        assert!(!looks_like_html("<header>Not a document</header>"));
        assert!(!looks_like_html("# Title\n\nSome <b>markdown</b> text"));
        assert!(!looks_like_html(
            "<a href=\"x\">a link opening Markdown</a>"
        ));
        assert!(!looks_like_html("Plain text about <html> tags"));
        assert!(!looks_like_html(""));
    }
}
