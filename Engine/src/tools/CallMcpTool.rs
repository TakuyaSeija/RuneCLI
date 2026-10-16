//! `call_mcp_tool` — call a tool an MCP server offers.
//!
//! # Why one tool stands for all of them
//!
//! The tool declarations are the first bytes of every request; a provider caches
//! by exact prefix. Declaring each MCP tool as the model found it changed that
//! prefix mid-session — every discovery and every server that connected or
//! dropped billed the whole conversation again — and a server with fifty tools
//! charged all fifty schemas to every request. So the model is offered this one
//! tool, whose declaration never moves: `discover_capabilities` shows an MCP
//! tool's input schema, and the model calls it here by server and name.
//!
//! # Nothing runs as this tool
//!
//! [`crate::tools::Registry::target`] resolves a call to the MCP tool it names
//! before anything else looks at it, so the permission gate, the classifier and
//! the validation all see the real tool and its real arguments. The arguments
//! are checked against the server's own input schema, and a mismatch names the
//! field (see [`crate::tools::correction`]).

use serde_json::{json, Value};

use super::{action_field, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};

/// The tool's name, the one thing the registry matches it by.
pub const NAME: &str = "call_mcp_tool";

pub struct CallMcpTool;

impl Tool for CallMcpTool {
    fn name(&self) -> &str {
        NAME
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: NAME.into(),
            description: "Call a tool that a connected MCP server offers. Find the tool and its input schema with discover_capabilities first.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["action", "server_id", "tool", "arguments"],
                "properties": {
                    "action": action_field(),
                    "server_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 48,
                        "description": "The MCP server's id, as discover_capabilities lists it."
                    },
                    "tool": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "The tool's name on that server, as discover_capabilities lists it."
                    },
                    "arguments": {
                        "type": "object",
                        "description": "The tool's own arguments: only the fields its input schema (shown by discover_capabilities) declares, nothing of your own such as action or reason."
                    }
                }
            }),
        }
    }

    /// An MCP tool's effects are the server's to define, so the proxy counts as
    /// one that can change things: a read-only session does not offer it.
    fn mutates(&self) -> bool {
        true
    }

    fn preview(&self, args: &Value) -> String {
        format!(
            "{} / {}",
            args["server_id"].as_str().unwrap_or("?"),
            args["tool"].as_str().unwrap_or("?")
        )
    }

    fn run(&self, _ctx: &mut ToolCtx<'_>, _args: &Value) -> Result<Outcome> {
        Err(Error::Internal(
            "call_mcp_tool runs as the MCP tool it names; it was not resolved".into(),
        ))
    }
}

/// The internal name of the MCP tool a call names, and the arguments it gets.
pub fn target(args: &Value) -> Result<(String, Value)> {
    let server = args["server_id"]
        .as_str()
        .ok_or_else(|| Error::invalid("`server_id` is missing"))?;
    let tool = args["tool"]
        .as_str()
        .ok_or_else(|| Error::invalid("`tool` is missing"))?;
    let arguments = args
        .get("arguments")
        .filter(|a| a.is_object())
        .cloned()
        .ok_or_else(|| Error::invalid("`arguments` must be an object"))?;
    Ok((
        crate::capabilities::mcp::internal_name(server, tool),
        arguments,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_names_its_server_tool_and_arguments() {
        let (internal, arguments) = target(&json!({
            "action": "open an issue",
            "server_id": "github",
            "tool": "create_issue",
            "arguments": {"title": "x"}
        }))
        .unwrap();
        assert_eq!(
            internal,
            crate::capabilities::mcp::internal_name("github", "create_issue")
        );
        assert_eq!(arguments, json!({"title": "x"}));
    }

    #[test]
    fn arguments_must_be_an_object() {
        assert!(target(&json!({"server_id": "s", "tool": "t", "arguments": "x"})).is_err());
        assert!(target(&json!({"server_id": "s", "tool": "t"})).is_err());
    }

    /// The declaration is the cached prefix: it quotes no server, no tool and no
    /// limit, so nothing a session does can move it.
    #[test]
    fn the_declaration_names_nothing_that_can_change() {
        let decl = CallMcpTool.declaration().parameters.to_string();
        assert!(!decl.contains("mcp__"));
        let keys: Vec<_> = CallMcpTool.declaration().parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys[0], "action", "action is always the first field");
    }
}
