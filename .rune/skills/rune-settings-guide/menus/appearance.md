# Settings › Appearance

## Color theme
Choose Violet, Cyan or Amber. Left/Right on the Color theme row switches and saves immediately. Enter opens the theme list; Enter on a selected theme applies it. Esc returns without applying an item you only browsed.

TOML key: `ui.theme`. Factory: `"amber"`. Reset with `rune -config reset ui.theme` or the offered default action. Theme changes do not affect model behavior or permissions.

## Conversation side panel
Shows session information beside the conversation when there is enough terminal width. Toggle here, with Ctrl+O, or `/panel`.

TOML key: `ui.sidebar`. Factory: `true`. Hiding it does not delete information or stop cost tracking. A narrow terminal can hide a side-by-side panel even with the preference enabled.

Other color, mouse, scrolling and display-timing choices are advanced TOML settings, not additional controls on this page. Use the specific key's help rather than assuming a UI row exists.

A background image, blur or transparency visible in a screenshot may be configured by the terminal/desktop. Rune Settings does not provide a background-image control.

[Advanced layout/color/mouse/scroll](../appearance/layout.md) · [Timing](../appearance/rendering.md)
