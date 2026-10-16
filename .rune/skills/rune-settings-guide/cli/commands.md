# User commands

Use `rune -help` and `/help` for the installed version. The commands below do not all appear in Settings. Commands that change data, connect accounts or test a provider require the user's intended action; merely describing them is not authorization.

## Start and inspect
`rune [dir]` opens a workspace; `-p "prompt"` performs one request. `-project <dir>` chooses a workspace; `RUNE_HOME` selects a settings/data home, `-db <path>` a separate store. `-version`, `-tools`, `-settings`, `-help` inspect their subject. `-read-only` restricts mutating tools; `--no-color`/NO_COLOR disables color. `-host` is a frontend integration mode, not the normal interactive UI. `-script <file>` replays a recorded model script; do not use an unreviewed script on live data.

## Configuration
`-config show|check|defaults|explain <key>` inspects configuration. `-config set <key> <value>`, reset <key>, format change it. `[generation]` defaults differ from `-set key=value` or `/set key=value` for model/session preferences. See [scope](../config/overview.md).

## Accounts/models
`-provider list`, keys <name> inspect masked metadata; add, key, dropkey, remove, default, timeout change account choices; `new-store --yes` sets an unreadable credential store aside and starts a new one (every key is added again — see [credentials](../permissions/credentials.md)). test sends a real request; models refreshes the catalog; dryrun previews request content (can contain private data—do not share wholesale). `-model list|set <id>` lists/selects a model. Prefer the masked credential UI rather than actual tokens in command arguments. `/provider`, `/model`, `/thinking`, `/classifier` offer conversation controls.

## Sessions/storage
`-session` opens the picker/lists sessions; `-resume <id>` resumes. `-session rename <id> <title>` renames; delete <id> permanently removes. `/new`, `/session`, `/resume` navigate sessions. `-storage list|prune` inspects/removes eligible missing-project data; review before pruning. On Windows the command sandbox lets Rune's commands into the project and its tool caches by access entries on those folders, added the first time a command runs there; `-sandbox` lists them and `-sandbox reset` removes every one (a Rune window still open adds what its next command needs again). Linux and macOS leave nothing to remove. `/compact` summarizes context, not disk cleanup; `/undo` and `/redo` restore individual files only after review.

## Conversation and capabilities
`/settings` (alias /*), `/theme`, `/panel`, `/verbose`, `/mode`, `/attach`, `/detach`, `/logs`, `/context`, `/help`, `/quit` control their named views/choices. `/skills`, `/skill` and `/mcp` manage external guides/capabilities; read their [guides](../capabilities/skills.md) before installation/access changes. `/context` shows information supplied to the AI and `/logs` diagnostic events; either can contain private work. `-doctor` can perform connectivity diagnostics; ask before running it.

When a command fails, report the actual result. Do not silently recreate accounts, delete storage, install software or switch permission mode to work around it.
