//! Search the same catalog used by Rune's host controls; reveal metadata only.
//!
//! # Why one required `query` and a numbered `page`
//!
//! Some providers decode a tool call strictly: every declared property is
//! always sent, optional or not (measured 2026-10-10 on every `cx/*` GPT model:
//! an "optional" `id` arrived as `","`, `" "` or an id the model made up, and a
//! `cursor` as `"page_cursor_17"`). A schema whose optional fields exclude each
//! other — `query` or `id`, never both — could therefore never be called by
//! those models, and with it no MCP tool either, since this is where their
//! schemas are read. So every field here means something whatever value it is
//! given: `query` is the one way to ask, an exact id is just a query that names
//! one entry, and `page` is a number whose natural value, 1, is the default.
//!
//! # What is not listed
//!
//! Rune's own tools: their declarations are already in every request. Listing
//! them again invited a model trained on deferred tools to "load" a tool it
//! could already call (session 407 spent a call "loading" `manage_process`).
use super::{action_field, field_str, Outcome, Tool, ToolCtx, ToolDecl};
use crate::Result;
use serde_json::{json, Value};

pub struct DiscoverCapabilities;
impl Tool for DiscoverCapabilities {
    fn name(&self) -> &str {
        "discover_capabilities"
    }
    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: self.name().into(),
            description: "Find what is not already listed: skills beyond <available_skills>, the tools of connected MCP servers (each with the input schema call_mcp_tool needs), and MCP resources.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["action", "kind", "query"],
                "properties": {
                    "action": action_field(),
                    "kind": {
                        "type": "string",
                        "enum": ["all", "skill", "tool", "resource"],
                        "description": "What to look for."
                    },
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 512,
                        "description": "Keywords to search for, * for everything, or the exact id of one entry as an earlier result listed it."
                    },
                    "page": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Which page of the results to read: 1 for the first, the default."
                    }
                }
            }),
        }
    }
    fn mutates(&self) -> bool {
        false
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "Discover capabilities: {}",
            args["query"].as_str().unwrap_or("")
        )
    }
    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        let _ = ctx.capabilities.refresh();
        let kind = field_str(args, "kind")?;
        let query = field_str(args, "query")?.trim();
        let page = args["page"].as_u64().unwrap_or(1).max(1) as usize;
        let mut entries: Vec<Value> = ctx
            .registry
            .listing()
            .into_iter()
            .filter(|e| e["source"] != "rune-builtin")
            .collect();
        // A shadowed skill is found only by its exact id, so it is kept here
        // and dropped below unless the query names it.
        entries.extend(ctx.capabilities.skill_entries(true));
        entries.extend(ctx.capabilities.resource_entries());
        entries.retain(|e| kind == "all" || e["kind"] == kind);
        let exact: Vec<Value> = entries
            .iter()
            .filter(|e| e["id"] == query || e["name"] == query)
            .cloned()
            .collect();
        let ranked: Vec<Value> = if !exact.is_empty() {
            exact
        } else {
            entries.retain(|e| e["shadowed"] != true);
            ranked(entries, query)
        };
        let total = ranked.len();
        let page_size = ctx.limits.config.number("discover_capabilities.page_size") as usize;
        let pages = total.div_ceil(page_size).max(1);
        let selected: Vec<Value> = ranked
            .into_iter()
            .skip((page - 1).saturating_mul(page_size))
            .take(page_size)
            .collect();
        let shown = selected.len();
        let result = json!({
            "entries": selected,
            "total": total,
            "page": page,
            "pages": pages,
            "servers": ctx.capabilities.server_status(),
        });
        let content = serde_json::to_string(&result)?;
        Ok(Outcome::process_output(
            format!("Found {shown} capabilities"),
            [Ok(content)],
            result,
            &ctx.limits.config.snapshot(),
            "discover_capabilities",
            "discover_capabilities",
            ctx.results,
        ))
    }
}

/// `entries` that match `query`, best first: by how many of its words an entry
/// holds, then by id. `*` matches everything.
fn ranked(entries: Vec<Value>, query: &str) -> Vec<Value> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut scored: Vec<(usize, Value)> = entries
        .into_iter()
        .filter_map(|e| {
            if query == "*" {
                return Some((0, e));
            }
            // Every text a model may know an entry by: a resource is often
            // asked for by the URI it saw elsewhere.
            let text = [
                "name",
                "description",
                "source",
                "id",
                "server_id",
                "uri",
                "uri_template",
            ]
            .iter()
            .filter_map(|key| e[*key].as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
            let score = words.iter().filter(|w| text.contains(w.as_str())).count();
            (score > 0).then_some((score, e))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1["id"].as_str().cmp(&b.1["id"].as_str()))
    });
    scored.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model that sends every declared field must still be able to call the
    /// tool: no two fields exclude each other, and every optional field has a
    /// value that means "the default".
    #[test]
    fn every_field_may_always_be_sent() {
        let decl = DiscoverCapabilities.declaration().parameters;
        let required: Vec<_> = decl["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["action", "kind", "query"]);
        let keys: Vec<_> = decl["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["action", "kind", "query", "page"]);
        assert_eq!(decl["properties"]["page"]["minimum"], 1);
        assert!(crate::tools::validate_value(
            &decl,
            &json!({"action":"Find MCP tools","kind":"all","query":"*","page":1})
        )
        .is_ok());
    }

    #[test]
    fn a_resource_is_found_by_its_uri_and_words_rank_matches() {
        let entries = vec![
            json!({"id":"resource:s:1","kind":"resource","name":"Resource 2","uri":"test://static/resource/2"}),
            json!({"id":"resource:s:2","kind":"resource","name":"Resource 3","uri":"test://static/resource/3"}),
            json!({"id":"s/echo","kind":"tool","name":"echo","description":"Echo a static resource"}),
        ];
        let found = ranked(entries.clone(), "resource/2");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "resource:s:1");
        let found = ranked(entries.clone(), "static echo");
        assert_eq!(found[0]["id"], "s/echo", "two words beat one");
        assert_eq!(ranked(entries, "*").len(), 3);
    }
}
