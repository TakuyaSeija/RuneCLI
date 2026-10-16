# Keyboard and mouse shortcuts

## Editing text

These keys work in the conversation composer, Write your own answers, Settings text fields and model search, MCP form fields, and session search/rename. A list, choice, confirmation button, or other non-text control keeps its own navigation.

Each editing action has one canonical shortcut; Rune does not add alternative Readline chords for the same action. This is Rune's keymap, not full Bash/Readline emulation. Some terminals reserve combinations or do not report modifiers; a key that never reaches Rune cannot be handled by Rune.

| Key | Action |
| --- | --- |
| ← | Move left by one visible character |
| → | Move right by one visible character |
| Ctrl+← | Move to the previous whitespace-delimited word |
| Ctrl+→ | Move to the end of the current/next whitespace-delimited word |
| ↑ | Move to the previous visible row in multiline text, keeping the desired column |
| ↓ | Move to the next visible row in multiline text, keeping the desired column |
| Ctrl+A | Move to the beginning of the current logical line |
| Ctrl+E | Move to the end of the current logical line |
| Home | Move to the beginning of the whole input |
| End | Move to the end of the whole input |
| Backspace | Delete the visible character on the left, or the selected text |
| Delete | Delete the visible character on the right, or the selected text |
| Ctrl+U | Kill text from the caret back to the current logical line's beginning |
| Ctrl+K | Kill text from the caret to the current logical line's end; at a newline, remove that newline only; at the end of the whole input, do nothing |
| Ctrl+W | Kill the whitespace-delimited word on the left, including trailing spaces |
| Alt+D | Kill the whitespace-delimited word on the right, including leading spaces |
| Ctrl+Y | Insert the most recently killed text |
| Alt+Y | Replace the immediately preceding yank with the previous kept kill |
| Ctrl+_ | Undo one text edit |
| Alt+R | Redo one undone text edit |
| Shift+← | Extend the selection left by one visible character |
| Shift+→ | Extend the selection right by one visible character |
| Ctrl+Shift+← | Extend the selection to the previous word |
| Ctrl+Shift+→ | Extend the selection to the next word |
| Shift+↑ | Extend the selection to the previous visible row |
| Shift+↓ | Extend the selection to the next visible row |
| Alt+X | Exchange adjacent visible characters; at line end, exchange the last two |
| Alt+W | Exchange adjacent words without exchanging their separator |
| Alt+U | Uppercase the following word, or the selected text |
| Alt+L | Lowercase the following word, or the selected text |
| Alt+V | Capitalize the following word, or the selected text |

A logical line ends at an inserted newline, not at a soft wrap caused by a narrow terminal. Ctrl+K never erases the prefix left of the caret. Typing, pasting, Backspace, or Delete replaces a selection. A mouse drag whose endpoints are in the same text field selects editable text; dragging conversation text still copies it. Secret fields remain masked and do not send their editable selection to the terminal clipboard.

Undo/redo and killed text belong to that input field only. They are temporary, have bounded memory, and are cleared when the field closes, a message is sent, a different session opens, or another window replaces the input. They are not a backup and are not shared with other fields or windows. Very large edits can exceed the temporary undo allowance. A rejected paste or edit that exceeds the field's limit leaves the value unchanged. Single-line fields refuse pasted line breaks rather than hiding them.

Ctrl+B/F/D, Alt+B/F, Alt+Backspace, Ctrl+Delete, and Ctrl+X Ctrl+U are not alternative editing bindings. `/undo` and `/redo` restore files, not typed text. On a terminal using traditional control characters, Ctrl+_ is byte `0x1f`; whether a physical combination produces it depends on that terminal.

## Conversation and application

Ctrl+G opens Settings; Ctrl+O toggles the side panel; Ctrl+T opens the plan; Ctrl+R expands/refolds steps; Ctrl+L repaints; Alt+T changes reasoning; Tab completes a slash command or changes permission mode. These application shortcuts do not change into Readline transpose/history-search keys while a text field is active.

Enter sends the conversation input. Alt+Enter inserts a newline in multiline text. Modified Return encodings can also insert a newline when the terminal reports them. In a form, Enter confirms/advances rather than sending a chat message. Tab/Shift+Tab moves between form controls.

Esc twice stops active AI work; Esc closes a view when idle. Ctrl+C first arms leaving rather than stopping the turn; repeat to leave. The first Ctrl+C does not cancel a running task or erase the draft.

Ctrl+Down jumps to latest chat. Wheel scrolls by ui.scroll_lines; Ctrl/Alt+wheel can go three times as far if the terminal passes it through. In the composer, ↑/↓ move through visible text rows with sticky desired columns, moving to the text start/end when at the top/bottom row, and repeating at the start/end recalls prompt history; PgUp/PgDn recall input history directly without moving through text rows first. History is per session; a fresh session does not import another conversation's prompts.

## Lists and forms

Settings: arrows choose, Enter opens/applies, Esc cancels/goes back. Tab/Shift+Tab moves focus. Models: Ctrl+F focuses search, Alt+C configures a model, T tests it with a real request, and Delete removes the selected model when the list is focused. Read the row/footer before acting.

Session picker: type/paste to search; ↑/↓ choose a session; PgUp/PgDn move through the list; Ctrl+R opens rename. Search and rename use the text-editing keys above. Enter resumes the chosen session or saves a rename; Esc cancels a rename, clears a nonempty search, or leaves an empty picker.

Permission/question forms keep their own choices and explicit confirmation. In Write your own, plain Up/Down first move through visual text rows, then to the input start/end; at the start Up returns to the preceding option, and at the end Down goes to the controls below. Empty answers can leave in either direction immediately. Shift arrows keep selecting text. Tab/Shift+Tab and clicks also move focus; Enter advances a nonempty answer. Alt+Left/Right switches questions from any question control without changing the saved answer. Plain Left/Right edits the answer or moves between buttons; literal < and > remain text, not question shortcuts. Page keys read the question card rather than duplicating Home/End. Scrolling is not selecting an answer. Ctrl/Alt mouse combinations and Shift-drag may be reserved by the terminal itself.

`/help` lists the installed version's commands and canonical editing keys. A folded preview can be opened by clicking it; expansion does not recover text omitted by an output limit.

## Reading a screen while typing

Settings and session search keep their own keys even while a question is waiting. Wheel navigation follows the visible surface under the pointer, not a form behind it. Ctrl+wheel or Alt+wheel applies three ordinary notches to that same surface: Settings and session lists, Logs/undo/redo selections, Context and other readers, the command palette, side panel, and scrollable question/permission documents. At a boundary it stays there; it never continues into a screen behind it. Text editors and controls with no wheel navigation remain unchanged. The terminal may reserve Ctrl/Alt+wheel itself; Rune cannot handle a gesture it never receives. Logs and undo/redo file lists move their selection and viewport together with arrows or the wheel; Context and other plain readers scroll their document. In Logs, Enter opens/refolds details and page keys read long open details.

When a slash-command palette is visible above the composer, Up/Down, Tab and Enter belong to that palette before the reader behind it. Esc dismisses the palette first. Executing another screen command replaces the current screen rather than stacking it; a late reply never brings an older screen back. Esc closes the current screen, while Settings keeps its normal page ancestry and undo/redo can return from a diff to its list.
