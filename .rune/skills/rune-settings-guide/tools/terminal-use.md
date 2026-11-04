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

## Manual commands with `!`

In the interactive composer, put `!` in the very first position, followed by the command, for example `! echo test`. The composer shows **Shell · full access once** before Enter. This is a command you choose to run yourself, not a prompt asking the AI to interpret it.

**Risk:** Enter authorizes that whole script once with the access of your OS account, outside Rune's workspace jail and command sandbox, without classifier/sanitizer review or another tool-permission question. It does not grant Administrator/root rights, change the session mode, or approve later AI tools. Review the entire script before Enter, especially pasted commands. Outputs and commands become AI context and may contain private information.

Only a leading `!` selects manual execution. `hello ! echo test`, `hello` followed by a new line and `! echo test`, or whitespace before `!` remain ordinary prompts. In shell mode every later line belongs to the same script: `! echo one` followed by `echo two` runs both. A later `!` is shell syntax, not another Rune prefix. A paste never submits itself; Enter runs the complete current script. Empty `!` is not executed. Remove the leading marker to return to ordinary input.

A manual command sent while a turn is active waits for that turn to finish and runs in submission order. Live output is visible. When the command finishes, or the same process moves to background at the configured wait/input wait, its observed input/result are added to context and the AI responds; that response uses the selected model and can incur its normal cost. Do not re-run a command just to read its result. A reconnect resends the same submission without executing it again; after a crash Rune does not automatically replay a possibly executed command.

A terminal refusal can explain this manual option, but it never submits `!` for you or automatically retries the refused action. Other tools do not gain this manual bypass.

While arguments stream, the terminal box uses the engine's configured shell for the preview. Execution confirms the shell actually used, such as Bash, zsh, PowerShell or Cmd, not a fixed platform label. Bash/zsh use `$`, PowerShell uses `PS>`, and Cmd uses `>`. Historical calls that recorded no shell show Terminal rather than guessing. Cmd does not imply Administrator, and macOS's interactive login shell is not necessarily the shell Rune was configured to use.

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
Each command starts with its own working directory/shell; a previous cd is not kept. When the next step needs a command's result, leaving `run_in_background` unset or false returns that result in the same call if it finishes within the foreground wait. If work can proceed without it, `run_in_background: true` returns immediately and keeps the command in background. Choose from the task's dependencies and expected lifetime, not merely whether a command is called a build or test. Unknown duration alone does not require background: a still-running foreground command moves automatically at `terminal.command_timeout` without being restarted.

An input wait detected by the system can move a foreground command earlier; the current detection capability is shown to the AI with the command wait and shell. Silence alone is not proof of a wait. Read the background report when notified rather than repeatedly polling, and do not start the same command again to obtain its result. Once a task is tracked in background, prefer independent work or end the turn so its notification can resume the work. A new shell `for`/`while` loop that only checks the log/PID, or sleeps until completion, does not speed up the original task: the waiting command can itself move to background and add unnecessary work and model cost. A purposeful progress/diagnostic check is still useful when it answers the user's question; ordinary loops that perform actual task work are not waiting loops. A background launch that cannot create its output log does not start; if a foreground command cannot be moved at its wait limit, it is stopped and the reason is reported.

Whether a wait for typed input is noticed depends on the system. Linux tells every one. macOS tells only a hidden entry, such as a password prompt. Windows tells none. On macOS and Windows a quiet background command may be waiting for an answer or still working: read its latest output, and answer it when it asks.

A collapsed command preview fills one visual row, even while arguments stream or the command runs. Each original newline appears as `↵`, including blank lines; `…` appears only when a suffix cannot fit the available width. These are display marks, not shell separators or changes to the executed command. The script keeps its actual newlines, blank lines and indentation. Rune's drag-copy restores original newlines rather than copying the added `↵` or `…` marks; the terminal's own selection copies the visible display. Click to open all restores the complete script in its original multiline layout. Command output has its own four-row head preview, with the total omitted command and output lines counted at the bottom; command lines already represented by the compact preview are not counted as hidden. Opening restores every display-folded row, but cannot restore output omitted by a separate text/capture limit; follow the result's saved-output/continuation guidance where provided.

An input-wait handoff shows the output printed so far. Everything after it arrives with the task's notification: Rune's line (task, how it ended, its PID, how long it ran, the log's path) and the output the AI has not been given yet, so nothing has to be asked for.

The saved log includes the current screen while the process is running or waiting, including a question without a final newline. It is not necessary to wait for the command to finish or for rows to scroll off. Before a detected input wait is announced, the saved output is synchronized to disk; a restarted Rune can read that file without the old conversation or screen. This is not a guarantee against disk failure, an incomplete write at a crash, capture failure or the configured limits.

The log is plain terminal text after interpreting cursor moves and escape sequences, not raw escape bytes. Completed scrollback stays, while the live screen at the end of the file is updated in place. Progress redraws and an answer echoed onto the question's own line replace that line rather than appending duplicates. The notification uses this same saved text; it does not append a second copy from an in-memory screen. Do not run a command again merely to recover its output.

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
