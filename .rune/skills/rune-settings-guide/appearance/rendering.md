# Advanced display timing

Location: rune.toml only. Scope: global. Keep defaults unless diagnosing a specific display/input issue.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `ui.frame_interval` |16ms| Display refresh interval. |
| `ui.status_lifetime` |8s| How long a temporary footer message remains when no other message is waiting. |
| `ui.status_replace_lifetime` |3s| Visible time before a waiting message or retry countdown may take its place; never longer than the message's normal lifetime. |
| `ui.quit_lifetime` |4s| Quit-confirmation interval. |
| `ui.stop_lifetime` |3s| Stop-confirmation interval. |
| `ui.escape_timeout` |50ms| How long to distinguish an Escape key sequence. |
| `ui.color_probe_timeout` |120ms| Wait for the terminal's background-color answer. |
| `ui.spinner_frames` |6| Refresh ticks between spinner changes, 1–100. |
| `ui.counter_min_duration` |120ms| Minimum counter animation time. |
| `ui.counter_max_duration` |1400ms| Maximum counter animation time. |

Use `rune -config explain <key>` first. Change only the intended key with `rune -config set <key> <value>`; reset with `rune -config reset <key>`. Positive durations have the general [value bounds](../config/values.md). A current display/input operation can finish with its current timing.

Temporary footer messages remain for 8 seconds by default when unopposed. If another message or retry countdown is waiting, the current message yields after 3 seconds of visible time; repeated updates keep only the latest waiting message. Retry timing continues while another message is shown. Esc and Ctrl+C confirmations take priority, and the covered message's visible time pauses until the confirmation ends. Press Esc again within `ui.stop_lifetime` (default 3 seconds) to stop active AI work, including retry backoff; another key disarms that confirmation. The hint disappears when the same stop interval ends, not after the ordinary 8-second message lifetime. Stopping/finished control results and leaving the current screen can clear obsolete messages rather than queue them.

Risk: faster refresh/animations can increase resource use; overly short status/confirmation windows are hard to read/use. An unsuitable Escape timeout can affect perceived input behavior, particularly over slower connections. These are display preferences, not model speed settings. Do not change the terminal profile or install terminal software without permission.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `ui.frame_interval` | `"16ms"` | Positive duration up to 24h |
| `ui.status_lifetime` | `"8s"` | Positive duration up to 24h |
| `ui.status_replace_lifetime` | `"3s"` | Positive duration up to 24h |
| `ui.quit_lifetime` | `"4s"` | Positive duration up to 24h |
| `ui.stop_lifetime` | `"3s"` | Positive duration up to 24h |
| `ui.escape_timeout` | `"50ms"` | Positive duration up to 24h |
| `ui.color_probe_timeout` | `"120ms"` | Positive duration up to 24h |
| `ui.spinner_frames` | `6` | Whole number 1–100 |
| `ui.counter_min_duration` | `"120ms"` | Positive duration up to 24h |
| `ui.counter_max_duration` | `"1400ms"` | Positive duration up to 24h |
