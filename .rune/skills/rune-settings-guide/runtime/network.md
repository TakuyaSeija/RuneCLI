# Network waiting and size limits

Scope: global. Settings → Runtime shows Stall limit, Connect timeout and Probe timeout. The other options below are advanced TOML settings, not additional visible rows on that page.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `network.stream_idle_timeout` |5m| How long a response can send no data before the attempt is abandoned/retried. Not a limit on a continuously active request's total length. |
| `network.connect_timeout` |10s| Time allowed to establish a connection. |
| `network.probe_timeout` |45s| Time allowed for an explicit provider test. |
| `network.catalog_timeout` |10s| Time allowed for fetching model-list information. |
| `network.catalog_ttl` |1h| How old model information/prices may be before refresh is due. Not a prompt-cache duration. |
| `network.catalog_max_pages` |100| Maximum catalog pages per refresh, range 1–1000. |
| `network.max_stream_size` |16MiB| Total size allowed for a model's streaming response. Not tokens. |
| `network.max_event_size` |8MiB| Maximum size of one part of a streaming response. |
| `network.max_response_size` |8MiB| Maximum size of a non-streaming response. |
| `network.token_saver` |off| Request that supported routers retain complete tool results. On leaves compression to the router; it does not guarantee compression occurs. |

## Change or reset
Use the available Runtime row, or `rune -config set <key> <value>` for a global option. `rune -config reset <key>` restores its default. Inspect one key with `rune -config explain <key>`. Use [value formats](../config/values.md) for durations/sizes.

Stream idle timeout can affect a request already waiting. Other network choices generally affect subsequent requests. The model account's optional request timeout is separate from all of these.

## Effects and risks
Short waits can interrupt slow working services; long waits can prolong genuine failures. Larger response limits can consume more memory/context. More catalog pages/refreshes can mean more network activity.

Token-saving compression can remove details from what the AI receives. Do not enable it just to reduce cost without explaining that tradeoff. Increasing a limit cannot make an unsupported endpoint/model valid or grant access to a restricted page.

See [Runtime](../menus/runtime.md), [retry](retry.md) and [model accounts](../connections/model-providers.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `network.max_stream_size` | `"16MiB"` | Positive explicit size up to 1GiB |
| `network.connect_timeout` | `"10s"` | Positive duration up to 24h |
| `network.stream_idle_timeout` | `"5m"` | Positive duration up to 24h |
| `network.catalog_timeout` | `"10s"` | Positive duration up to 24h |
| `network.catalog_ttl` | `"1h"` | Positive duration up to 24h |
| `network.catalog_max_pages` | `100` | Whole number 1–1000 |
| `network.probe_timeout` | `"45s"` | Positive duration up to 24h |
| `network.max_event_size` | `"8MiB"` | Positive explicit size up to 1GiB |
| `network.max_response_size` | `"8MiB"` | Positive explicit size up to 1GiB |
| `network.token_saver` | `"off"` | off / on |
