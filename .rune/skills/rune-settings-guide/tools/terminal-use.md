# Terminal settings

Tool name: `terminal_use`.

Location: Settings → Tools → TERMINAL. Scope: global. These settings control command waiting and output; they do not approve a command.

| Key | Factory default | Purpose and consequence |
| --- | --- | --- |
| `terminal.command_timeout` |120s| Foreground wait before a still-running command moves to background. At least 1s. It is not normally a kill deadline. |
| `terminal.max_output_chars` |30000| Maximum command text returned to the AI. Lower values can omit useful output. |
| `terminal.truncate_head_bytes` |1536| Amount preserved from the start when output is shortened. |
| `terminal.truncate_tail_bytes` |1536| Amount preserved from the end when output is shortened. |
| `terminal.max_background_log_size` |100MiB| Per-background-command log limit. A command exceeding it is stopped; it is not merely an endless rotating log. |

## Advanced TOML-only choices
| Key | Factory default | Purpose and risk |
| --- | --- | --- |
| `terminal.max_output_size` |8MiB| Foreground output limit while a command runs; exceeding it stops the command. Separate from text returned to the AI. |
| `terminal.shell` |auto| Shell used for commands. `auto` uses bash when this machine has it, otherwise `/bin/sh`; on Windows PowerShell 7 when it is installed, otherwise Windows PowerShell, otherwise Command Prompt (cmd). Set an absolute path only to use a different installed shell — bash, zsh, sh, PowerShell (`pwsh.exe`, `powershell.exe`) or Command Prompt (`cmd.exe`). Each reads commands in its own language, so another shell changes which commands work. |
| `terminal.term` |xterm-256color| Terminal type reported to command programs. Wrong values can affect their display. |
| `terminal.inherit_environment` |false| Pass all environment variables instead of the normal selected set. Can expose private configuration/credentials to child programs; ask first. |
| `terminal.rows`, `terminal.columns` |40, 200| Initial headless terminal size, each 1–1000. Not a command to resize the user's outer terminal. |
| `terminal.flush_interval` |150ms| How frequently live output is updated. Faster updates can increase overhead. |
| `terminal.input_wait_delay` |2s| Quiet interval before checking for a typed-input wait. Silence alone does not prove input is required. |
| `terminal.drain_after_exit` |200ms| Time allowed to collect final output after a command exits. |
| `terminal.cancel_grace` |200ms| Grace period on command cancellation. |
| `terminal.timeout_grace` |2s| Grace period for applicable termination paths, not ordinary background handoff. |

## Change or reset
Use the available row, edit and Apply. For an advanced key use `rune -config set <key> <value>` only when requested. Reset with `rune -config reset <key>`. Inspect current values/timing using `rune -config explain <key>`. Use explicit duration/size units.

## Background and display
Each command starts with its own working directory/shell; a previous cd is not kept. Long commands can run in background, and work waiting for input can be reported separately. Read the background report when notified rather than repeatedly polling.

Whether a wait for typed input is noticed depends on the system. Linux tells every one. macOS tells only a hidden entry, such as a password prompt. Windows tells none. On macOS and Windows a quiet background command may be waiting for an answer or still working: read its latest output, and answer it when it asks.

A collapsed preview of four visual lines is a display fold. Opening it cannot restore output omitted by a separate text/capture limit; follow the result's saved-output/continuation guidance where provided.

Large limits can increase memory/disk/context use. Small limits can stop important work or hide diagnostics. Closing all Rune windows can eventually end retained background commands; see [idle lifetime](../runtime/host.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `terminal.command_timeout` | `"120s"` | Duration 1s–24h; foreground wait, not kill deadline |
| `terminal.shell` | `"auto"` | `auto`, or the absolute path of an installed shell executable |
| `terminal.term` | `"xterm-256color"` | Documented nonempty text choice |
| `terminal.inherit_environment` | `false` | true / false |
| `terminal.flush_interval` | `"150ms"` | Positive duration up to 24h |
| `terminal.input_wait_delay` | `"2s"` | Positive duration up to 24h |
| `terminal.drain_after_exit` | `"200ms"` | Positive duration up to 24h |
| `terminal.cancel_grace` | `"200ms"` | Positive duration up to 24h |
| `terminal.timeout_grace` | `"2s"` | Positive duration up to 24h |
| `terminal.rows` | `40` | Whole number 1–1000 |
| `terminal.columns` | `200` | Whole number 1–1000 |
| `terminal.max_output_chars` | `30000` | Whole number 100–10000000 |
| `terminal.max_output_size` | `"8MiB"` | Positive explicit size up to 1GiB |
| `terminal.max_background_log_size` | `"100MiB"` | Positive explicit size up to 1GiB |
| `terminal.truncate_head_bytes` | `1536` | Whole number 100–1000000 |
| `terminal.truncate_tail_bytes` | `1536` | Whole number 100–1000000 |
