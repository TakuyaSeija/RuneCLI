# File scan limits

Location: Settings → Tools → FILE SCAN. Scope: global, shared by find_files and search_text.

| Key | Factory default | Purpose and range |
| --- | --- | --- |
| `scan.max_entries` |200000| Maximum entries searched per call, 1–10000000. Separate from returned matches. |
| `scan.max_depth` |32| Maximum folder depth, 1–256. Deeper folders are not searched. |
| `scan.regex_steps` |200000| Work allowance for a search pattern on one line, 100–10000000. An excessively expensive pattern may skip that line. |

Change: select a row, edit, Apply. Use default or `rune -config reset <key>` restores its default. Use `rune -config explain <key>` to inspect the current choice. New searches use the new limits.

Risk: lower limits can leave searches incomplete; higher limits can increase time/resource use. A work-limit notice means there may be more results. Prefer a narrower path/pattern before increasing limits. Hidden/ignored names are generally included, except .git/.rune; there is no current hidden/ignore toggle.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `scan.max_entries` | `200000` | Whole number 1–10000000 |
| `scan.max_depth` | `32` | Whole number 1–256 |
| `scan.regex_steps` | `200000` | Whole number 100–10000000 |
