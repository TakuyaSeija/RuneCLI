# Sampling and output

Location: Settings → Generation. These controls affect the current session/model. Global `[generation]` values in rune.toml provide starting defaults; they do not replace every current session's choices.

| Key / row | Purpose and values |
| --- | --- |
| `thinking` / Thinking | Enable reasoning where supported. See [reasoning](../models/reasoning.md). |
| `effort` / Effort | Choose a supported reasoning level; choices depend on the model/provider. |
| `temperature` / Temperature | Randomness. Usual range 0–2; a model may have different published bounds. Lower values tend to be more literal, not guaranteed identical. |
| `top_p` / Top P | Nucleus sampling, usual range.01–1. Usually adjust this or temperature rather than both. |
| `frequency_penalty` / Frequency penalty | Discourage repeated tokens, usual range-2–2. Negative values encourage repetition. |
| `presence_penalty` / Presence penalty | Discourage repeated subjects, usual range-2–2. |
| `max_output` / Max output | Positive whole-token reply ceiling, no greater than a known model limit. Reasoning can use part of it. |
| `stop` / Stop sequences | Up to four strings that end generation. |

## Change or reset
Select a row, enter/choose a valid value, then Apply. Optional values can be cleared to return control to the provider/default. The model and Thinking switch cannot be left empty. Unsupported controls may not appear; some are unavailable only while another setting is active.

Session example: `/set temperature=0.2`. `/set temperature=` clears that optional choice. Use the current conversation's UI when you intend to change that conversation, rather than confusing it with global defaults.

Global example: `rune -config set generation.temperature 0.2`; reset with `rune -config reset generation.temperature`. Only run these when a global default is intended.

In TOML, stop is an exact array such as `stop = ["END,EXACT", "STOP"]`. The session text editor takes comma-separated entries instead. Empty clears the list. Global entries must be nonempty and at most 1024 bytes each.

## Effects and risks
Higher randomness can reduce consistency. Excessive penalties can make answers unnatural. Stop strings can end an answer unexpectedly. Too small an output allowance can leave no final answer after reasoning; too large an allowance can permit more spending. An omitted limit is not a promise of unlimited output.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `generation` | `Inherit / unset` | Generation table, or omit for starting defaults |
