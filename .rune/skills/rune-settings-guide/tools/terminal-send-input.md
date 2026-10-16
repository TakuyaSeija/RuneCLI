# Send input to a background command

terminal_send_input types into a reported background process. It does not wait for completion or return the next output. Use [manage_process](manage-process.md) to read the question/output when notified.

Inputs: the reported process_id, text, and optional submit choice. `auto` uses the program's input mode where known; `enter` appends Enter; `none` types only the text. Empty text with enter presses Enter alone. Do not guess a password or answer an important prompt without the user's decision.

There is no dedicated numeric Settings row for this tool. Terminal wait/output settings and [permission choices](../permissions/modes.md) are separate. Changing a timeout does not submit an answer.

Risk: typed text can be a command or confirmation, not just harmless chat. Inspect what the process actually asks and type only the authorized response. Hidden-entry input must remain private; do not repeat it in explanations/log examples. After typing, await the next notification rather than polling or sending a second answer blindly.
