# Permission modes and wait timeouts

## Choose a mode
In the conversation, Tab cycles modes; `/mode ask`, `/mode auto` or `/mode full` chooses explicitly. These are session choices, not a `permissions.mode` TOML option. Inside a form, Tab is form navigation instead.

| Mode | User-visible behavior and risk |
| --- | --- |
| Ask | Default. Reads inside the workspace proceed; every edit asks (Always can be offered), every command asks each time, and reading anything outside the workspace asks. |
| Auto | Reading and writing inside the workspace proceed without questions. Anything outside it asks: a file or folder elsewhere, or a program already running on the computer (a container engine, the service manager, a terminal multiplexer, a key agent). Risky commands still ask each time. Suitable only when the user wants that autonomy. |
| Full | No restriction inside or outside the workspace, as the user's OS account, without permission questions. High risk; not a troubleshooting shortcut and not a bypass of every other validation/review. |

The modes mean the same on Linux, macOS and Windows. No folder of the home is treated specially: a folder such as `~/.ssh` is asked about like any other location outside the workspace.

On Windows, Rune confines commands using a dedicated local user account (`rune-sbx-…`, set up once with Administrator confirmation and removed by `rune -sandbox reset`), or falls back to a restricted token of your user account when Administrator confirmation is declined:
- A command held to its mode can still read what Windows lets every user read (folders at a drive's root, other drives, ProgramData) and create files in ProgramData (`rune -sandbox` names this). Your profile folder stays out of its reach.
- With the sandbox account active, Git Bash (`MSYS2`/Cygwin) and Windows's own HTTPS (`curl.exe`, `Invoke-WebRequest`) work inside a command held to its mode. When falling back to a restricted token without the sandbox account, `MSYS2`/Cygwin programs ask before each command and run that call with full access once allowed, and Windows's own HTTPS fails (git's HTTPS still works).

An approval for one action is not a global mode change. A remember option, when offered, applies to its stated scope; it is not approval for every later command. Deny, cancel or expiry does not execute the action. A clarification question or Settings confirmation is not a general execution approval.

Do not claim an AI policy review makes Full harmless. Explain the difference and let the user choose; never switch to Full merely because a call failed.

## Permission wait
`permissions.timeout`: Settings → Tools → PERMISSIONS. Factory 10m. How long an unanswered permission card waits. `"unlimited"` means no expiry, not automatic approval.

## Question wait
`questions.timeout`: Settings → Tools → USER QUESTIONS. Factory unlimited. How long a question form waits; expiry does not submit an answer.

Change either by opening its row, entering a supported duration/unlimited and Apply. Use default or `rune -config reset <key>` restores the default. Inspect with `rune -config explain <key>`. A new choice is not a retroactive answer to an existing card.

Longer waits preserve time to respond but can leave work waiting; shorter waits can interrupt unattended work. `-read-only` is a separate launch choice restricting mutating tools, not a new permission mode or a guarantee that no data can be read/sent.

See [file access](path-access.md) and [classifier](../auxiliary/classifier.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `permissions.timeout` | `"10m"` | Positive duration up to 24h, or unlimited |
