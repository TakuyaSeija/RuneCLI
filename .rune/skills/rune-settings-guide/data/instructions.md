# Project instructions

AGENTS.md at the workspace root supplies project guidance to Rune. It is not an API-key file, a permission grant or the classifier policy. Keep user-facing project rules separate from sensitive credentials and implementation notes you do not intend the model to receive.

Key: `instructions.max_chars`. Location: Settings → Tools → PROJECT INSTRUCTIONS. Scope: global. Factory 32768, range 100–1000000 characters.

Purpose: limit how much project instruction text is included automatically. When instructions exceed it, Rune can report truncation; the AI must not assume the rest does not exist.

Change the row and Apply; Use default/`rune -config reset instructions.max_chars` resets. Explain inspects. Current conversation context is not rewritten as if earlier instructions had never existed; use the indicated update/re-read when instructions change.

Risk: too small a budget can omit relevant rules; too large uses more context and may expose text meant to stay private. Edit AGENTS.md only when the user asks for that project change. Never use it to claim authorization to bypass permission requests. CLASSIFIER.md is a different [policy setting](../auxiliary/classifier.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `instructions.max_chars` | `32768` | Whole number 100–1000000 |
