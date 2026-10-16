# Settings location and scope

## Where to configure Rune
Global settings are in `$RUNE_HOME/rune.toml`, normally `~/.rune/rune.toml`. They apply to Rune using that home. A project's own rune.toml is not automatically a Rune settings file. `rune -project <dir>` selects a workspace, not a different settings home.

Settings UI is opened with Ctrl+G or `/settings`. Not every advanced TOML option has a UI row. The relevant guide says which method to use.

## Global versus session settings
- Tool limits, retry/network settings and auxiliary-model choices are global.
- Models selection and the main Generation controls apply to the current session. Saved preferences can be recalled when returning to a model.
- `[generation]` in rune.toml supplies starting defaults; editing it does not overwrite every existing session's choices.
- New-session fallback selects a starting provider for new sessions, not the provider for every open conversation.

Do not confuse a model's capability ceiling with the output length requested for one conversation.

## Inspect current and default values
| Command | Use |
| --- | --- |
| `rune -config` | Show where settings are located. |
| `rune -config explain <key>` | Explain one global setting's current value and when a change applies. |
| `rune -config defaults` | Read factory defaults. These may differ from saved choices. |
| `rune -config check` | Validate configuration; does not prove a provider connection works. |
| `rune -config show` | View effective configuration. It may contain private account details; do not share it wholesale. |
| `rune -settings` | Inspect model settings for the selected workspace. |

## Change or reset
Use the setting's UI editor, or `rune -config set <key> <value>` for a global choice. `rune -config reset <key>` removes that choice and restores its current default/inheritance. UI `Use default` has the same purpose for global values. `rune -config format` changes formatting; it is not required for inspection.

TOML has no native null value. Remove an inherited/unset key or use reset instead of writing `null` into TOML. Unknown/retired keys are errors; do not invent aliases.

## When changes apply
Settings normally follow changes without relaunching Rune. Some affect the next use immediately; others apply to the next request or operation while work already running keeps its starting settings. Abandoned-scratch cleanup uses its lifetime choice on restart. Use `explain` for the specific setting rather than assuming all timing is the same.

Classifier policy has a separate Reload action; merely editing the policy file does not replace the policy being used by an existing session. See [classifier](../auxiliary/classifier.md).

Before changing a setting, explain its scope and consequence. Keep the user's unrelated choices intact, and ask before connection tests, additional spending or broader access.

TOML top-level `version = 1` describes the configuration format, not the Rune application version. Leave it at the supported value; do not change it to perform an upgrade.
