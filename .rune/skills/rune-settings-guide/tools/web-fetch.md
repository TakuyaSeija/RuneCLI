# Read a web page

web_fetch reads a public http(s) page. Private/internal page targets are not supported. Search for pages with [web_search](web-search.md), not this tool. Built-in reading is not a full interactive browser; a script-dependent/login page may not yield the desired content.

Location: Settings → Tools → WEB FETCH. Scope: global.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `web_fetch.provider` |Rune's built-in reader| Explicit external account/account:route, or default. Adding an external reader alone does not choose it. |
| `web_fetch.timeout` |60s| Page request timeout. |
| `web_fetch.max_output_chars` |100000| Returned page-text limit, 100–10000000 characters. |

Change rows and Apply. Provider selection is under Provider; see [steps](../connections/web-providers.md). Use default or `rune -config reset web_fetch.provider` returns to the built-in reader. Inspect other keys with `rune -config explain <key>`; reset with `rune -config reset <key>`.

Risk: a third-party reader receives the page address and may charge for reading. Obtain the user's choice before selecting it. Large pages can consume context; a cap can omit content. A failed explicit provider is not permission to send the page elsewhere. Increasing timeout or choosing an external reader does not grant access to restricted targets.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `web_fetch.provider` | `Inherit / unset` | Available account or account:route; omit for default |
| `web_fetch.timeout` | `"60s"` | Positive duration up to 24h |
| `web_fetch.max_output_chars` | `100000` | Whole number 100–10000000 |
