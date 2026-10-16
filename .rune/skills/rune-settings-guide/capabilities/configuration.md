# Capability settings

Location: advanced `[capabilities]` in rune.toml; management is available through skills/MCP commands, not a separate Settings tab. Scope: the chosen settings home, with per-project choices under projects.

| Option below capabilities | Default | Purpose / accepted range |
| --- | --- | --- |
| `enabled` |true| Master switch for skills/MCP availability. Turning off can also hide official help. |
| `portable_user_skills` |Automatic| Whether to use personal .agents/skills. Normally follows use of the default personal Rune home. Set true/false deliberately, or omit for automatic. |
| `external_skills_compatibility` |false| Include the additional .agents/external_skills locations. Review those packages before enabling. |
| `skill_roots` |[]| Additional personal skill folders. |
| `product_skill_roots` |[]| Additional product-workflow folders; not required to obtain Rune's bundled guide. |
| `servers` |{}| Personal MCP definitions; use MCP management. |
| `projects` |{}| Per-project capability choices. Prefer management commands rather than inventing project entries. |
| `page_size` |12| Capability catalog page count, 1–100. Separate from the discovery tool's page_size. |
| `catalog_tokens` |2048| Allowance for listing skill summaries to the AI, 128–32768. Full guides are read when needed. |
| `max_entries` |2000| Catalog/package entry bound, 1–10000. |
| `max_skill_bytes` |65536| SKILL.md size limit, 1024–1048576 bytes. |
| `max_package_bytes` |16777216| Total skill-package allowance, 1024–134217728 bytes. |
| `max_message_bytes` |8388608| Capability message/content allowance, 1024–67108864 bytes. |
| `timeout_seconds` |60| MCP request wait in seconds, 1–900. |

Additional roots must be absolute, bounded paths without parent traversal; no more than 64 per list. Adding a location can expose its skills/instructions to use. It is not a general file-access permission.

Project options: `root` identifies the project; `servers` supplies private-project MCP definitions; `skill_roots` adds project skill locations; `disabled_skills` keeps the disabled list. `approved_servers` is maintained by explicit approval commands; do not manufacture approvals. `approved_skills` is an obsolete recorded property, not an active skill-approval setting to tune.

## Change and reset
Use [skills](skills.md)/[MCP](mcp.md) management for package/server changes and approval. For advanced options edit only the intended TOML key and run `rune -config check`; removing an optional choice returns to its default. Do not reset the whole capabilities object just to reset one option, because that can remove unrelated server/project choices. Reload/reconnect when required; a live request can retain its starting settings.

Risk: larger limits use more context/resources, smaller limits can omit entries or reject packages/content. Additional servers/packages can access data or execute actions according to their configuration. Obtain consent before installing, enabling or widening any access. Built-in help has no need for new credentials or external servers.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `capabilities` | `{}` | Capability options table; preserve unrelated server/project choices |
