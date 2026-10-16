# Compaction settings

Location: Settings → Generation → compact rows. Scope: global. Purpose: summarize older conversation history to free space in the model's context. `/compact` requests a summary manually; Rune can also compact when needed.

| Key | Factory default | Use |
| --- | --- | --- |
| `compact.model` |Inherit active model| Model used to summarize history. |
| `compact.max_output_tokens` |32768| Summary output allowance, setting 256–10000000; the selected model's actual limit still applies. |
| `compact.keep_recent_turns` |5| Desired number of recent turns kept in full, range 1–100. Available context may require fewer. |
| `compact.fallback_context_tokens` |200000| Assumed context budget when the model's context limit is unknown, range 1000–10000000. Does not enlarge a known model's limit. |
| `compact.message_chars` |32000| Historical assistant/tool-result text considered per message, range 100–1000000 characters. |
| `compact.tool_chars` |2000| Historical arguments considered per tool call, range 100–1000000 characters. |

Original user instructions are treated separately from the assistant/tool excerpts above. Nevertheless, summarization can lose historical detail; it is not a perfect copy of every earlier fact. Compaction is different from deleting a session or freeing disk space.

## Change or reset
Select a row, enter/choose its value and confirm Apply. Use default restores the default/inherited model. For a global reset use `rune -config reset <key>`. Inspect its current value with `rune -config explain <key>`.

Changes affect the next compaction, not a summary already produced. If a required compaction fails, Rune can ask whether to retry; declining stops that attempt. Do not report a failed summary as completed or ignore the retry question.

## Effects and risks
Keeping more history or making a longer summary can use more context, time and money. Lower excerpt/summary limits can omit useful detail. An unsuitable small model can fail to summarize the available history. Selecting another provider can send relevant conversation history there.

Ask before changing providers or reducing preservation settings. To solve a context error, first confirm the correct model context and current choices rather than raising every limit. See [model limits](../models/capabilities.md) and [auxiliary allowance](budgets.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `compact.model` | `Inherit / unset` | Configured {provider,id}, or omit to inherit |
| `compact.max_output_tokens` | `32768` | Whole number 256–10000000 |
| `compact.keep_recent_turns` | `5` | Whole number 1–100 |
| `compact.fallback_context_tokens` | `200000` | Whole number 1000–10000000 |
| `compact.message_chars` | `32000` | Whole number 100–1000000 |
| `compact.tool_chars` | `2000` | Whole number 100–1000000 |
