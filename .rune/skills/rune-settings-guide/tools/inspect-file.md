# Inspect files

The inspect_file tool reads one file. Text is shown with line numbers; supported images/documents/media can be supplied to a compatible model. A requested line range is inclusive. The model may not support every file type.

Location: Settings → Tools → INSPECT FILES. Scope: global.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `inspect_file.max_file_size` |16MiB| Largest file to read. Also limits individual files considered by text search. |
| `inspect_file.max_output_chars` |30000| Maximum text returned for a file read, including presentation overhead such as line numbers. |

## Change or reset
Open a row, enter the value and Apply. Use default restores the default. For CLI use `rune -config explain <key>`, `rune -config set <key> <value>` or `rune -config reset <key>` as appropriate. Size values need explicit units such as 16MiB.

## Effects and risks
A file exceeding the size limit is not simply guaranteed a partial read. A text result can be shortened even when the file fits; follow the notice and request a narrower range. A collapsed UI preview is separate from actual returned-output truncation.

Larger limits can expose more private file content to the chosen model and use more context/memory. Use narrow reads rather than increasing limits automatically. Do not read credential material merely to explain a setting, and do not assume a changed file is identical to an earlier attachment.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `inspect_file.max_file_size` | `"16MiB"` | Positive explicit size up to 1GiB |
| `inspect_file.max_output_chars` | `30000` | Whole number 100–10000000 |
