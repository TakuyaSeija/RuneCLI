# Web search

web_search searches a query and returns links/snippets. A URL entered as a query is searched, not fetched; page reading is [web_fetch](web-fetch.md). Available results can also be limited by the selected vendor.

Location: Settings → Tools → WEB SEARCH. Scope: global.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `web_search.provider` |First enabled search| Explicit account/account:route, or default. See [selection steps](../connections/web-providers.md). |
| `web_search.timeout` |60s| Search request timeout. |
| `web_search.results` |8| Maximum returned results, 1–100. A call can request fewer. |
| `web_search.max_output_chars` |100000| Returned text limit, 100–10000000 characters. |

Change rows and Apply; Use default or `rune -config reset <key>` resets. Inspect with `rune -config explain <key>`. Provider changes should use the Provider subpage to add/test/select the intended account. A missing explicit choice is unavailable, not automatic substitution.

Risk: the query is sent to the selected service, possibly billed. More results/text can increase context/cost, and snippets are not proof a page is current/accurate. Short timeout/output can leave incomplete evidence. Ask before sending private queries or switching providers. [Web vendors](../connections/web-vendors.md) documents supported service uses.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `web_search.provider` | `Inherit / unset` | Available account or account:route; omit for default |
| `web_search.timeout` | `"60s"` | Positive duration up to 24h |
| `web_search.results` | `8` | Whole number 1–100 |
| `web_search.max_output_chars` | `100000` | Whole number 100–10000000 |
