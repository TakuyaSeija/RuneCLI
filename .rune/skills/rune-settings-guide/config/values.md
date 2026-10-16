# Value types and reset

Use the type and range of the particular setting. A valid value for one option is not necessarily valid for another.

| Type | How to enter it | Important distinction |
| --- | --- | --- |
| Boolean | TOML `true` or `false` | Do not quote TOML booleans. |
| Count/tokens | Whole integer in the allowed range | Zero is valid only where allowed. Tokens are not characters or bytes. |
| Duration | Quoted TOML string such as `"500ms"`, `"60s"`, `"2m"` | Prefer explicit units. General positive durations are at most 24h; some options have narrower ranges. |
| Size | Quoted TOML string such as `"128KiB"` or `"16MiB"` | Use explicit units: a unitless size means KiB, not bytes. These units use powers of 1024. |
| Money | Nonnegative USD number | Zero price means free; an unknown price is different. |
| Text | Quoted TOML string | Use the documented choices; arbitrary text may not be meaningful. |
| Auxiliary model | `{ provider = "my-account", id = "exact-model-id" }` | The provider must already be configured. Omit/reset to inherit. |
| Tool list | `["inspect_file", "terminal_use"]` | Exact unique names. Empty [] is a deliberate selection, not a reset. |
| Web provider | `"account"` or `"account:route"` | Add/test/select the provider in Tools. Do not guess gateway routes. |

## Default, inherit and unlimited
For global settings, `Use default` or `rune -config reset <key>` is the reliable way to reset. TOML has no native null. Do not assume blank, auto, inherit or none is accepted for every field.

Only question and permission wait settings support unlimited: `questions.timeout` and `permissions.timeout`. Unlimited means no expiry, not automatic approval or submission. It does not mean an unlimited terminal, model or web request.

For optional session Generation controls, blank returns control to the provider/default. Model selection and the Thinking switch cannot simply be left empty. Explicit reasoning Off, a cleared effort level and 9router Route default are different choices; see [reasoning](../models/reasoning.md).

## Safe examples
These show syntax, not changes to apply automatically:
```toml
[terminal]
command_timeout = "2m"

[generation]
temperature = 0.2
stop = ["END,EXACT", "STOP"]
```
`[generation]` is a global starting default, not a change to every current session. Its stop array preserves exact entries, including commas. The session text editor instead takes comma-separated entries.

Shell example for a global value: `rune -config set terminal.command_timeout 2m`.
Reset: `rune -config reset terminal.command_timeout`.
These operate on the selected settings home; only run them when the user wants the change.

Character, byte and token limits measure different things. A visible four-line preview is only a display fold; it is not the same as a tool output limit. A larger limit can increase model context use and cost.
