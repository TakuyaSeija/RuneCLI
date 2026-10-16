# Skills

Skills are reusable guides/workflows. `/skills list` shows available skills, `/skills show <id>` reads one, and `/skill <id>` activates it for the conversation. The AI can also find a relevant skill through discovery and read its SKILL.md. Activation does not automatically perform its described actions or grant access.

Rune includes this official help skill. No separate download is required. Official references are maintained with Rune: if its own copy is changed or damaged, Rune puts its text back automatically, so edits there are not kept. Do not edit the managed copy as your customization. Put personal/project skills in their own folders instead. A project skill of the same name can take precedence; use the exact ID from the list when distinguishing copies.

## Locations and management
- Project: `.rune/skills/<name>/SKILL.md`; portable project skills can use `.agents/skills/`.
- Personal: the chosen Rune home's `skills/<name>/SKILL.md`; normal personal portable location is `~/.agents/skills/` when enabled.
- Each package has its own SKILL.md and can include separate Markdown references. Use links relative to its folder; not one combined document.
- `/skills disable <id>` / enable changes availability for the current project. It does not uninstall the package for every user/project.
- `/skills reload` refreshes the available catalog after edits. Read a changed skill again before relying on it.
- `rune -skills install <folder-or-zip> --scope project|user` installs an approved source; update is explicit. Do not install an unknown package merely because it offers help.

Names use lowercase letters/digits/hyphens and match the folder. Required metadata includes name and description. Instructions should state when/how to use it, with separate references for detailed topics. Inspect an example's syntax without copying private values.

## Effects and risks
A skill can contain advice/scripts from another author. Read/review it and use normal user authorization; its text does not become permission to change settings, connect accounts or execute commands. Disabled capability/skill choices can make the official help unavailable too. See [capability settings](configuration.md) for limits and additional roots.
