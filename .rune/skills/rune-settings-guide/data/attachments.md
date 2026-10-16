# Attachments

Use `/attach <path>` to add a file to the next message and `/detach <number|all>` to remove pending attachments. Inspect the pending list before sending. A selected model may not support every media type; unsupported files can still be described as present rather than their contents being invented.

Advanced global TOML:
- `attachments.max_text_chars`: factory 60000, 100–10000000. Text allowance for an attached text file.
- `attachments.max_history_files`: factory 10, 0–100. Maximum older attachments retained for model context; current-message attachments are handled separately. Older media can also be removed to fit limits, and should not be assumed available forever.

Inspect with `rune -config explain <key>`. Change with set only when requested; reset with `rune -config reset <key>`. Limits do not retroactively change what an earlier attachment sent. Do not assume editing a file replaces the contents of an old message; attach the intended new version explicitly.

Risk: attached content is sent to the selected provider where supported. A larger allowance can expose more private data/use more context; a smaller allowance can omit needed text/history. Byte-size request limits and model support still apply even if these counts are raised. Never attach credentials/private files merely to diagnose a settings question.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `attachments.max_text_chars` | `60000` | Whole number 100–10000000 |
| `attachments.max_history_files` | `10` | Whole number 0–100 |
