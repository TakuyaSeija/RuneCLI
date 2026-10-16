# Search text

The search_text tool searches file contents and returns matching lines with their paths/line numbers. Literal mode is the default; | separates alternatives. Regex mode, case sensitivity, filename include pattern, directory/file and a smaller per-call result limit can be chosen where needed.

Location: Settings → Tools → SEARCH TEXT. Scope: global.

| Key | Factory default | Purpose and range |
| --- | --- | --- |
| `search_text.matches` |100| Maximum matching lines returned, 1–100000. Not a count of every occurrence inside each line. |
| `search_text.max_line_chars` |2000| Maximum text shown from each matching line, 20–100000 characters. |
| `search_text.max_output_chars` |30000| Total returned-text limit, 100–10000000 characters. |

Individual-file size uses `inspect_file.max_file_size`; there is no separate current search_text.max_file_size choice. Shared FILE SCAN settings limit depth/work and expensive regex patterns separately.

## Change or reset
Select the row, edit and Apply. Use default restores the default. CLI inspection: `rune -config explain <key>`; for requested changes use `rune -config set <key> <value>` and reset with `rune -config reset <key>`.

## Effects and risks
Long lines, result limits or skipped files can make a search incomplete. Read the notice before saying there are no further matches. Ignore files/hidden names are generally included; .git and .rune are skipped. Search can omit files that exceed read/work limits or are not text.

Increasing limits can use more time and model context. It can also return private content if the chosen scope contains it. Prefer a narrower directory, include pattern or search term instead of raising limits blindly.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `search_text.matches` | `100` | Whole number 1–100000 |
| `search_text.max_line_chars` | `2000` | Whole number 20–100000 |
| `search_text.max_output_chars` | `30000` | Whole number 100–10000000 |
