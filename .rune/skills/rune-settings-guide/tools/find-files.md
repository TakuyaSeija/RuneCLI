# Find files

The find_files tool finds paths by glob pattern, not file contents. `*.rs` matches at any depth; `src/**/*.rs` searches below src. The per-call limit can reduce the configured maximum, not raise it.

Location: Settings → Tools → FIND FILES. Scope: global.

| Key | Factory default | Purpose and range |
| --- | --- | --- |
| `find_files.matches` |200| Maximum returned paths, 1–100000. |
| `find_files.max_output_chars` |20000| Maximum returned text, 100–10000000 characters. |

## Change or reset
Open a row, edit and Apply. Use default restores its default. Inspect with `rune -config explain <key>`; use `rune -config set <key> <value>` or `rune -config reset <key>` only for an intended global change.

## Effects and risks
File-scan depth/work limits also bound the search independently. Ignore files and hidden names do not generally hide results; .git and .rune are skipped by these searches. An incomplete-search notice means there may be more matches. Do not claim a file cannot exist just because a limited search did not find it.

Larger values can cost more search time/output context; smaller values can miss useful results. Narrow the directory/pattern when possible. To find content rather than names, use [search text](search-text.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `find_files.matches` | `200` | Whole number 1–100000 |
| `find_files.max_output_chars` | `20000` | Whole number 100–10000000 |
