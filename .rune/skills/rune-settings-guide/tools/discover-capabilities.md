# Discover skills and MCP capabilities

discover_capabilities finds available skills, external tools and resources. It is not a list of every ordinary built-in tool. Search with keywords, * for all, or an exact earlier ID; page selects the next results page. Discovery alone does not install/connect/approve anything.

Global limits:
- `discover_capabilities.max_output_chars`: factory 30000, 100–10000000. Settings → Tools → DISCOVER CAPABILITIES.
- `discover_capabilities.page_size`: factory 12, 1–100. Advanced TOML default results per page.

Change the row or `rune -config set <key> <value>` when requested. Reset with Use default/`rune -config reset <key>`; inspect using explain. Later calls use the limits.

Risk: very short results may omit useful schema/details; more results use context. Read the actual skill or external-tool input description before using it. Discovery is not approval to grant more access. See [skills](../capabilities/skills.md), [MCP](../capabilities/mcp.md) and [capability settings](../capabilities/configuration.md) for separate catalog limits.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `discover_capabilities.max_output_chars` | `30000` | Whole number 100–10000000 |
| `discover_capabilities.page_size` | `12` | Whole number 1–100 |
