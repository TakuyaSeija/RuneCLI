# Compaction settings

Location: Settings → Generation → compact rows. Scope: global. Purpose: summarize older conversation history to free space in the model's context. `/compact` requests a summary manually; Rune can also compact when needed.

| Key | Factory default | Use |
| --- | --- | --- |
| `compact.model` |Inherit active model| Model used to summarize history. |
| `compact.max_output_tokens` |32768| Summary output allowance, setting 256–10000000; the selected model's actual limit still applies. |
| `compact.max_batch_size` |128KiB| Advanced TOML/CLI choice: maximum quoted source/summary text bytes per summary request. It is not model tokens or a guarantee that a small model accepts the batch. |
| `compact.keep_recent_turns` |5| Desired number of recent turns kept in full, range 1–100. Available context may require fewer. |
| `compact.fallback_context_tokens` |200000| Reported input-token threshold for automatic compaction when the model's context limit is unknown, range 1000–10000000. Does not enlarge a model's limit or infer tokens from text length. |
| `compact.message_chars` |32000| Historical assistant/tool-result text considered per message, range 100–1000000 characters. |
| `compact.tool_chars` |2000| Historical arguments considered per tool call, range 100–1000000 characters. |

Original user instructions are treated separately from the assistant/tool excerpts above. Nevertheless, summarization can lose historical detail; it is not a perfect copy of every earlier fact. Compaction is different from deleting a session or freeing disk space.

Automatic compaction uses input usage reported by the selected provider, or an actual context rejection. Rune does not convert text, thinking, image size or `base64` into token counts. Without reported usage the next request's token count is unknown; the provider may reject it before Rune can compact. If no safe older group exists, that provider failure is reported without resending the same request. A larger active model can be used when the selected compact provider actually rejects the context. Batch byte limits only divide text: lower them if a small compact model rejects a batch; smaller batches can increase time and cost. Provider payload-byte and output-token limits still apply independently.

## Change or reset
Select a row, enter/choose its value and confirm Apply. Use default restores the default/inherited model. For a global reset use `rune -config reset <key>`. Inspect its current value with `rune -config explain <key>`.

Changes affect the next compaction, not a summary already produced. While a summary is running, the footer shows `Compacting` with an animated spinner and elapsed time, including when `/compact` was requested after an answer ended. A message sent during this work waits in order and is processed when the summary attempt finishes; reopening the session or sending `Continue` is not required. Press Esc twice to stop the current work without leaving Rune. A second window of the same session shows the same compaction status.

If a required compaction fails, Rune can ask whether to retry; declining stops that attempt. A failed manual `/compact` is reported in `/logs`, not as a completed summary. Do not report a failed summary as completed or ignore the retry question.

## Effects and risks
Keeping more history or making a longer summary can use more context, time and money. Lower excerpt/summary limits can omit useful detail. An unsuitable small model can fail to summarize the available history. Selecting another provider can send relevant conversation history there.

Ask before changing providers or reducing preservation settings. To solve a context error, first confirm the correct model context and current choices rather than raising every limit. See [model limits](../models/capabilities.md) and [auxiliary allowance](budgets.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `compact.model` | `Inherit / unset` | Configured {provider,id}, or omit to inherit |
| `compact.max_output_tokens` | `32768` | Whole number 256–10000000 |
| `compact.max_batch_size` | `"128KiB"` | Explicit positive byte size; inspect/set/reset through `rune -config` |
| `compact.keep_recent_turns` | `5` | Whole number 1–100 |
| `compact.fallback_context_tokens` | `200000` | Whole number 1000–10000000 |
| `compact.message_chars` | `32000` | Whole number 100–1000000 |
| `compact.tool_chars` | `2000` | Whole number 100–1000000 |
