# Permission modes and wait timeouts

## Choose a mode
In the conversation, Tab cycles modes; `/mode ask`, `/mode auto` or `/mode full` chooses explicitly. These are session choices, not a `permissions.mode` TOML option. Inside a form, Tab is form navigation instead.

| Mode | User-visible behavior and risk |
| --- | --- |
| Ask | Default. Reads in allowed locations generally proceed; edits and commands can ask. Commands require an explicit answer each time. |
| Auto | Ordinary work in the allowed scope can proceed without repeated approval; sensitive, risky or additional access can still ask. Suitable only when the user wants that autonomy. |
| Full | Broad access as the user's OS account, without normal permission questions. High risk; not a troubleshooting shortcut and not a bypass of every other validation/review. |

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
