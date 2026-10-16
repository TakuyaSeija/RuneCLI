# Manage background processes

manage_process reads a background command's current status/new output or stops it when requested. It returns promptly; it is not a wait-until-finished control. Use the process ID Rune reported, not an unrelated OS PID.

Location: Settings → Tools → MANAGE PROCESS. Scope: global.

| Key | Factory default | Purpose |
| --- | --- | --- |
| `manage_process.max_output_chars` |30000| Maximum output text returned to the AI. |
| `manage_process.truncate_head_bytes` |1536| Beginning retained when output is shortened. |
| `manage_process.truncate_tail_bytes` |1536| Ending retained when output is shortened. |

Change in the row editor and Apply. Use default or `rune -config reset <key>` resets; `rune -config explain <key>` inspects. Limits apply to later reports.

Reports can say Running, Waiting for input, Completed, Failed, Killed or Lost. Lost means Rune cannot confirm a normal final result; do not call it success. A completed process can have an exit code. Read any additional notice about incomplete capture/output.

Risk: stopping a task can interrupt work; ask before stopping a task not already authorized for cancellation. Small output caps omit diagnostics, large caps use more context. Do not repeatedly poll a running job: wait for Rune's notification while doing other work. [Typing input](terminal-send-input.md) is separate from reading output.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `manage_process.max_output_chars` | `30000` | Whole number 100–10000000 |
| `manage_process.truncate_head_bytes` | `1536` | Whole number 100–1000000 |
| `manage_process.truncate_tail_bytes` | `1536` | Whole number 100–1000000 |
