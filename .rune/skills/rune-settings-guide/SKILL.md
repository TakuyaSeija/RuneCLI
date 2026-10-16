---
name: rune-settings-guide
description: "Help users understand and configure Rune: Settings menus, rune.toml options, models, connections, tool limits and permission choices. Explain what a setting does, how to change or reset it, and its effects and risks."
metadata:
  publisher: Rune
---
# Rune user settings guide

Use this skill when a user asks how to use or configure Rune. Answer in the user's language; keep menu labels, commands and setting names unchanged.

## How to help
1. Identify the user's goal and read the relevant topic below. Read only the references needed for the question.
2. Distinguish factory defaults, saved choices and current-session values. Screenshots show a particular user's choices, not universal defaults. When necessary, check `rune -version` and `rune -config explain <key>`; use `rune -config defaults` for factory values.
3. Explain: **purpose → location → accepted values → change/reset steps → scope and when it applies → effects and risks**. State when an option is TOML-only; never invent a UI control.
4. A request for an explanation is not a request to change settings, run a paid connection test, send data to another service or grant broader access. Ask for the user's decision before doing those things.
5. Do not request API keys in chat or include private configuration in examples. Use placeholders and Rune's credential-entry interface. Inspect only the information needed; a complete configuration can contain private account details.
6. This is user help, not implementation documentation. Do not include source paths, internal component designs, storage/protocol mechanisms, or audit notes in answers or new guide files. Explain configurable advanced options only in terms of their use and consequences.
7. If the installed version differs from this guide, report that difference and use that version's help. Do not guess whether a model, route or external service is supported.

References are relative to this skill's folder. Find that folder from the available skill's path rather than assuming a developer-specific path.

## Configuration basics
- [Settings location and scope](config/overview.md)
- [Value types, units and reset](config/values.md)

## Settings menus
- [Overview and navigation](menus/overview.md)
- [Models](menus/models.md): [capabilities](models/capabilities.md), [reasoning](models/reasoning.md), [pricing](models/pricing.md)
- [Generation](menus/generation.md): [sampling](generation/sampling.md), [sanitizer](auxiliary/sanitizer.md), [classifier](auxiliary/classifier.md), [compact](auxiliary/compact.md), [title](auxiliary/title.md), [shared auxiliary settings](auxiliary/budgets.md)
- [Connections](menus/connections.md): [model providers](connections/model-providers.md), [web accounts](connections/web-accounts.md), [web provider selection](connections/web-providers.md), [API keys](permissions/credentials.md)
- [Runtime](menus/runtime.md): [retry](runtime/retry.md), [network](runtime/network.md), [session idle lifetime](runtime/host.md)
- [Tools](menus/tools.md): [terminal](tools/terminal-use.md), [inspect files](tools/inspect-file.md), [find files](tools/find-files.md), [search text](tools/search-text.md)
- [Appearance](menus/appearance.md)
- [Permission modes](permissions/modes.md) and [file access](permissions/path-access.md)

## Other user help
- [Skills](capabilities/skills.md), [capability limits](capabilities/configuration.md), [MCP servers](capabilities/mcp.md), [MCP authentication](capabilities/authentication.md)
- [Attachments](data/attachments.md), [project instructions](data/instructions.md), [memory](data/memory.md), [storage](data/storage.md), [undo/redo](data/checkpoints.md), [spend notice](data/usage.md)
- [Advanced display choices](appearance/layout.md), [display timing](appearance/rendering.md)
- [CLI commands](cli/commands.md), [shortcuts](cli/shortcuts.md), [troubleshooting](troubleshooting.md)
- [Web vendors](connections/web-vendors.md); individual model providers are linked from [model accounts](connections/model-providers.md)

Use the menu/topic relevant to the question; do not load every guide to answer one setting.
