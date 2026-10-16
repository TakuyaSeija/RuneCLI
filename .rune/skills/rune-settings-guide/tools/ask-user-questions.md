# User questions

ask_user_questions shows decisions/options and allows a custom answer. A form can allow one choice or several. Choose/enter the intended answer and submit; do not assume the recommended option was accepted merely because it was highlighted.

Location: Settings → Tools → USER QUESTIONS. Scope: global.
`questions.timeout`: factory unlimited. A supported duration limits the wait; unlimited means no expiry. Expiry never supplies an answer for the user.

Open Questions wait timeout, enter a duration or unlimited, Apply. Use default/`rune -config reset questions.timeout` resets; explain shows the current choice. Do not treat changing the limit as answering an already-open question.

Risk: a short wait can interrupt unattended work, while unlimited can leave work waiting. A clarification answer is not the same as a command permission card or a settings confirmation. If requirements remain unclear, ask rather than invent a choice.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `questions.timeout` | `"unlimited"` | Positive duration up to 24h, or unlimited |

## Keyboard navigation

Use Up/Down to choose options. In Write your own, they move through the visible answer rows first. At the input start, Up returns to the preceding option; at the end, Down reaches the controls below. Empty custom answers allow both directions immediately. Alt+Left/Right switches questions from options, custom text, or buttons and keeps each answer. Plain Left/Right remains caret/button movement, not a question switch.

Tab/Shift+Tab moves between controls without changing conversation mode. Enter toggles an option or advances a nonempty custom answer; choose Submit answers explicitly to send. Settings and other screens retain their own navigation while the form waits, and returning does not discard the answer.
