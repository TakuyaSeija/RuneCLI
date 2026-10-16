# Project memory

Rune can keep durable project notes with an index, MEMORY.md. The index helps the AI find relevant notes; it is not every note's complete contents. @memory/ paths are used through file tools, not an ordinary workspace folder to publish.

Key: `memory.recall_size`. Location: advanced rune.toml option. Scope: global. Factory 20KiB. Purpose: budget for the recalled index. Past the budget its tail can be omitted and the AI is told to shorten/organize it.

Inspect: `rune -config explain memory.recall_size`. Change with set only when intended; reset using `rune -config reset memory.recall_size`. The next applicable context use follows the value; changing it does not delete the notes.

Risk: a small index budget can omit useful references; a larger one consumes model context. Store only durable useful facts, not keys/private credentials or every temporary output. A memory note is not permission to perform an action unrelated to the user's request.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `memory.recall_size` | `"20KiB"` | Positive explicit size up to 1GiB |
