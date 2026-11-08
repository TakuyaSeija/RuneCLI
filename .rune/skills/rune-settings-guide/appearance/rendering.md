# Display timing and footer messages

Rune times its own drawing and its own footer: the refresh interval, how long a
spinner takes to change, how long a counter animates, how long the Escape key
window stays open, how long the terminal's background-colour answer may take,
and how long each kind of footer message stays. Those values are the program's
own constants. They are not settings: no Settings row shows them, `rune.toml`
cannot hold them, `rune -config set` refuses them, and this guide does not list
them as choices. Do not offer to change one; if a person asks, say plainly that
it is fixed and describe what they will see.

What a person does control here is in [display, mouse and scrolling](layout.md):
the theme, the side panel, mouse interaction and how the wheel scrolls.

## Footer messages

A temporary footer message stays 8 seconds when nothing else is waiting. If
another message or a retry countdown is waiting, the current one yields after
3 seconds of visible time, and repeated updates keep only the newest waiting
message. Retry timing continues while another message is shown.

Esc and Ctrl+C confirmations take priority over ordinary messages: the covered
message's visible time pauses until the confirmation ends. Press Esc again
within the stop-confirmation window to stop active AI work,
including retry backoff; another key disarms that confirmation. The hint
disappears when that window ends, not after the ordinary message lifetime.
Stopping or finished control results, and leaving the current screen, can clear
obsolete messages rather than queue them.

## Screen behaviour that follows from these constants

The screen refreshes on its own clock, so a slow terminal is given frames it can
finish drawing rather than a queue of half-drawn ones. A terminal that answers
its background-colour probe quickly gets the matching light or dark ink; the
probe is given a short budget and Rune falls back when no answer arrives. On a
narrow terminal the conversation is shown alone, and the side panel returns when
there is room for it.

Risk: none of this is a model-speed setting. Do not describe a display constant
as a preference, and do not change the terminal profile or install terminal
software without permission.
