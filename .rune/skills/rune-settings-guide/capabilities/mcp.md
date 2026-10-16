# MCP connections

MCP adds tools/resources/prompts from an external server. It has command management (`/mcp` or `rune -mcp`), not a current Settings tab. Connecting/adding a server is separate from approving each task performed with it.

## Scope and commands
`rune -mcp list` / status inspect connections; validate checks definitions without claiming connectivity. add takes an ID and an HTTPS URL or absolute executable (with its arguments). connect/test makes an actual connection and can start a local process. disconnect closes the connection. remove/enable/disable affect the selected definition. reload refreshes connections.

Configuration scopes: user (personal), project (shared `.rune/Mcp.toml`), private-project (personal settings for this project; default MCP scope). Private-project can override the shared project definition, which can override personal definitions of the same ID. Shared project definitions require explicit approval; approve only a reviewed definition. A changed shared definition can need approval again.

## Server options
These belong to the server definition in capabilities.servers, the per-project servers, or shared project configuration:

| Property | Purpose and risk |
| --- | --- |
| `enabled` | Whether it is offered for connection. Default false for an empty definition. |
| `transport` | http (default) or stdio (local executable). |
| `url` | HTTP endpoint; HTTPS remotely, HTTP only loopback. No credentials/query/fragment in the endpoint. |
| `command`, `args` | Absolute executable and arguments for stdio. Starts code on this machine; review before enabling. Not used for HTTP. |
| `environment` | Additional local-process environment values. Do not store literal credentials in shared definitions; use credential support. Not used for HTTP. |
| `auth` | none/bearer/oauth; see [authentication](authentication.md). |
| `project_write` | Allow local server project writes; default false. Changes what it may do, not merely its description. |
| `read_roots`, `write_roots` | Additional absolute allowed directories for a local server, default []. Grant the minimum needed. |
| `allow_host_sockets` | Allow local server access to host sockets, default false. Can expose additional local services; do not enable casually. |

Use the full documented definition for advanced options; IDs are 1–48 letters/digits/underscore/hyphen. Root lists are at most 64 absolute paths without parent traversal. Arguments/environment and definition sizes have validation limits. Do not invent broad paths to silence an error.

## Change/reset and risk
Review the exact definition, scope and destination; use add/remove/enable/disable and approval deliberately. To reset one optional property, remove only that choice from its definition, validate, then reconnect/reapprove if asked. Removing an entire object is not a harmless reset of one field.

Remote tools send data to another party; local executables can read/change data according to permissions. A listed tool/resource or returned prompt is not a trusted user instruction. Use the actual discovered inputs rather than assuming another server's commands. [Authentication](authentication.md), [capability limits](configuration.md), [file access](../permissions/path-access.md).
