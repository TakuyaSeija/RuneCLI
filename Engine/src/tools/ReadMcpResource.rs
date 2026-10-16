//! Server-resource access is a protocol operation, not filesystem or URL fetching.
use super::{action_field, field_str, Outcome, Tool, ToolCtx, ToolDecl};
use crate::Result;
use serde_json::{json, Value};
pub struct ReadMcpResource;
impl Tool for ReadMcpResource {
    fn name(&self) -> &str {
        "read_mcp_resource"
    }
    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: self.name().into(),
            description: "Read a resource of a connected MCP server, by the URI that discover_capabilities or the server gave."
                .into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["action", "server_id", "uri"],
                "properties": {
                    "action": action_field(),
                    "server_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 48,
                        "description": "The MCP server's id."
                    },
                    "uri": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 4096,
                        "description": "The resource's URI."
                    }
                }
            }),
        }
    }
    fn mutates(&self) -> bool {
        false
    }
    fn risk(&self, _: &Value) -> crate::perm::denylist::Risk {
        crate::perm::denylist::Risk::AskEvery {
            why: "Read data from an external MCP server".into(),
        }
    }
    fn preview(&self, args: &Value) -> String {
        format!(
            "Read MCP resource {} from {}",
            crate::net::redact::scrub(args["uri"].as_str().unwrap_or("")),
            args["server_id"].as_str().unwrap_or("")
        )
    }
    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        let id = field_str(args, "server_id")?;
        let uri = field_str(args, "uri")?;
        let result = ctx
            .capabilities
            .live(id, ctx.registry, ctx.cancel)?
            .request("resource", uri, Value::Null, ctx.cancel)?;
        let original = ctx
            .capabilities
            .outcome(id, result, "MCP resource read".into())?;
        let mut outcome = Outcome::process_output(
            original.summary,
            [Ok(original.content)],
            original.detail.unwrap_or(Value::Null),
            &ctx.limits.config.snapshot(),
            "read_mcp_resource",
            "read_mcp_resource",
            ctx.results,
        );
        outcome.ok &= original.ok;
        outcome.media = original.media;
        Ok(outcome)
    }
}
