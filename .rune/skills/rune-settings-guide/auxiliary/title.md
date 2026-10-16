# Session title settings

Location: Settings → Generation → title rows. Scope: global. Purpose: generate a short title for a conversation.

| Key | Factory default | Use |
| --- | --- | --- |
| `title.model` |Inherit active model| Choose a model for title generation, or inherit. |
| `title.max_output_tokens` |256| Model output allowance, configurable 8–10000000; actual model limits still apply. |
| `title.max_chars` |60| Maximum title length in characters, range 1–200. |

## Change or reset
Select a row, edit/choose the value, then confirm Apply. Use default restores its default/inheritance. CLI inspection: `rune -config explain title.max_chars`; reset a custom model with `rune -config reset title.model`.

Changes affect future title generation, not every existing title. To rename one conversation, use the session rename action instead of changing these global settings.

## Effects and risks
A title model can add cost and latency. Another provider can receive the relevant conversation information for generating a title. Too little output allowance can prevent a useful title; a larger character limit affects presentation, not the model's context capacity.

Choose a different provider only with the user's consent. See [shared auxiliary allowance](budgets.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `title.model` | `Inherit / unset` | Configured {provider,id}, or omit to inherit |
| `title.max_output_tokens` | `256` | Whole number 8–10000000 |
| `title.max_chars` | `60` | Whole number 1–200 |
