# MCP authentication

Authentication belongs to the chosen MCP server definition, not the model provider's API-key field. Do not reuse credentials for a different destination without the user's decision.

## Choices
- `auth.type = "none"`: no configured authentication (default). This is not proof the server accepts anonymous use.
- bearer: `auth.secret` names the credential reference for an HTTP server; it is not the token itself. Supply the token with the supported MCP credential command, not shared TOML/chat.
- oauth: optional `auth.client_id`, `auth.scopes` and `auth.allowed_origins`. These identify the intended client, requested access and permitted authentication origins. Scopes must match the service; adding an origin can allow credentials to be sent there. Ask before expanding either.

Bearer/OAuth are for HTTP, not stdio credential fields. A local server can use supported environment credential references with `rune -mcp secret`. Reference names are not real secret values.

## User steps
Inspect the server identity/address and scope before authenticating. Use `rune -mcp login <server-id>` and `rune -mcp login-status <server-id>` for supported OAuth login, then complete the actual presented consent flow. Use `rune -mcp token <server-id>` or `rune -mcp secret <server-id> <reference>` for the configured references. Where the CLI supports omitted token/secret values through stdin, do not expose the value in command arguments. Never ask a user to paste the actual token into the AI chat.

To change/reset authentication, adjust only this server's definition and intended credential through supported management, validate, then reconnect/reapprove as needed. Removing a reference does not necessarily erase a stored credential or revoke authorization at the remote service. If revocation is required, use that service's account controls too.

Risk: OAuth consent and tokens can provide broad external access. An authenticated connection is not permission for all future tool actions. Refuse to guess missing credentials and report a failed login honestly. See [MCP](mcp.md) and [API-key safety](../permissions/credentials.md).
