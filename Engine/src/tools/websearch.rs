//! A search query goes to the connection that serves search ([`crate::web`]):
//! the one `web_search.provider` names, else the first the person added.
//! Reading a particular page belongs to `web_fetch`, so a URL in a query
//! stays a query, never an implicit request to another host.
//!
//! The declaration names no vendor and no limit — it is the first bytes of
//! every request, and adding a connection must not void a session's cache.
//! Who serves, or why nobody can, reaches the model through its status
//! (`session/status.rs`); a call made while nobody can is not executed, with
//! the same sentence. What happened on the way — which connection answered,
//! each retry, a refusal — goes to the session's log, where the person looks
//! when "active" does not mean "working".

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use super::{cap, field_given_str, field_opt_u64, flat_schema, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::proto::LogLevel;
use crate::settings::services::Service;
use crate::settings::Settings;
use crate::web::{self, Choice};

pub struct WebSearch {
    settings: Arc<Mutex<Settings>>,
    limits: Arc<Limits>,
}

impl WebSearch {
    pub fn new(settings: Arc<Mutex<Settings>>, limits: Arc<Limits>) -> WebSearch {
        WebSearch { settings, limits }
    }

    /// Who serves this search, from what disk holds now.
    fn route(&self) -> std::result::Result<web::Route, String> {
        let mut settings = crate::panic::lock(&self.settings);
        settings.follow_services();
        match web::choose(&settings, Service::Search) {
            Choice::Ready(route) => Ok(route),
            Choice::Unavailable(why) => Err(why),
            Choice::Builtin => Err("search has no built-in service".into()),
        }
    }
}

/// The query, required and never a page to fetch.
fn request(args: &Value) -> Result<&str> {
    field_given_str(args, "query")
        .ok_or_else(|| Error::invalid("web_search needs a non-empty search query in `query`"))
}

impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    /// Queries leave this machine, so the preview and audit trail keep what
    /// was searched, even though nothing local changes.
    fn mutates(&self) -> bool {
        false
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "web_search".into(),
            description: "Search the web. Returns links and snippets; use web_fetch to read a particular page as Markdown.".into(),
            parameters: flat_schema(
                json!({
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "description": "What to search for. A URL here is searched, not fetched."
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum results. Default and ceiling: web_search results in <status>."
                    }
                }),
                &["query"],
            ),
        }
    }

    fn preview(&self, args: &Value) -> String {
        let query = field_given_str(args, "query").unwrap_or("?");
        format!("search the web for {query:?}")
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let query = request(args)?;
        let log = ctx.log;
        let route = match self.route() {
            Ok(route) => route,
            Err(why) => {
                log(LogLevel::Warn, "search_unavailable", why.clone(), None);
                return Err(Error::NotFound(why));
            }
        };

        let config = &self.limits.config;
        let max_results = config.number("web_search.results");
        let requested = field_opt_u64(args, "count");
        let wanted = requested.map_or(max_results, |c| c.min(max_results));
        let count = wanted.min(route.adapter.max_results);
        let mut report = String::new();
        if requested.is_some_and(|c| c > count) || count < wanted {
            report.push_str(&super::notice::inline(&format!(
                "The result count was held to {count}."
            )));
            report.push('\n');
        }
        if count < wanted {
            log(
                LogLevel::Info,
                "search_count",
                format!(
                    "{} returns at most {count} results a search; {wanted} were asked for",
                    route.name
                ),
                None,
            );
        }

        let found = search(
            &self.settings,
            config,
            &route,
            query,
            count,
            ctx.cancel,
            log,
        )?;
        let lines = web::render(&found);
        if found.hits.is_empty() && found.answer.is_none() {
            report.push_str("no results\n");
        }
        for line in &lines {
            report.push_str(line);
            report.push('\n');
        }
        let max_chars = config.number("web_search.max_output_chars") as usize;
        Ok(Outcome::new(
            format!("{} result(s)", found.hits.len()),
            cap(report, max_chars),
        )
        .with_detail(json!({
            "results": found.hits.len(),
            "fetched": 0,
            "connection": route.name,
        })))
    }
}

/// One search through `route`, retried on the owner's curve with the
/// connection's key ring, every step written to the session's log.
fn search(
    settings: &Arc<Mutex<Settings>>,
    config: &crate::settings::configuration::Shared,
    route: &web::Route,
    query: &str,
    count: u64,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    log: &super::LogFn<'_>,
) -> Result<web::Found> {
    let timeout = config.duration("web_search.timeout");
    let http = web::client(settings, timeout, Some(Arc::clone(cancel)));
    let started = Instant::now();
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
    let mut told = 0;
    let mut announce = |w: crate::retry::Waiting| {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return crate::retry::Go::Cancel;
        }
        if w.attempt != told {
            told = w.attempt;
            log(
                LogLevel::Warn,
                "search_retry",
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
            crate::panic::lock(settings)
                .secrets
                .get(name)
                .map(String::from)
        });
        web::search_once(
            &http,
            route,
            key.as_deref(),
            query,
            count,
            Instant::now() + timeout,
            cancel,
        )
    };
    let result = crate::retry::run(
        settings,
        route.secret.as_deref(),
        &mut announce,
        &mut attempt,
    );
    let ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(found) => log(
            LogLevel::Info,
            "search",
            format!("{} · {} result(s) · {ms} ms", route.name, found.hits.len()),
            detail(json!({ "results": found.hits.len(), "ms": ms })),
        ),
        Err(Error::Cancelled) => {}
        Err(e) => log(
            LogLevel::Error,
            "search_failed",
            format!("{} · {e}", route.name),
            detail(json!({ "error": e.code(), "ms": ms })),
        ),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> WebSearch {
        WebSearch::new(
            Arc::new(Mutex::new(Settings::empty())),
            Arc::new(Limits::default()),
        )
    }

    #[test]
    fn a_url_stays_a_search_query_and_a_query_is_required() {
        let args = json!({"action": "search", "query": "https://example.com/docs"});
        tool().validate(&args).unwrap();
        assert_eq!(request(&args).unwrap(), "https://example.com/docs");
        assert_eq!(
            tool().preview(&args),
            "search the web for \"https://example.com/docs\""
        );
        for args in [
            json!({}),
            json!({"query":""}),
            json!({"query":"  "}),
            json!({"url":"https://example.com/"}),
        ] {
            assert!(request(&args).is_err(), "{args}");
        }
    }

    /// Owner, 2026-10-14: image search is gone and the schema is a query and
    /// a count — no mode, no vendor, no locale.
    #[test]
    fn the_declaration_is_a_query_and_a_count() {
        let tool = tool();
        let schema = tool.declaration().parameters;
        assert_eq!(schema["required"], json!(["action", "query"]));
        let mut props: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        props.sort();
        assert_eq!(props, ["action", "count", "query"]);
        for args in [
            json!({"action":"search", "url":"https://example.com/"}),
            json!({"action":"search", "query":"rust", "type":"images"}),
            json!({"action":"search", "query":""}),
        ] {
            assert!(tool.validate(&args).is_err(), "{args}");
        }
        tool.validate(&json!({"action":"search", "query":"rust", "count":3}))
            .unwrap();
    }

    #[test]
    fn with_no_connection_the_reason_says_where_to_add_one() {
        let why = tool().route().unwrap_err();
        assert!(why.contains("Settings › Connections"), "{why}");
    }
}
