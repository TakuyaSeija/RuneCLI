# Display, mouse and scrolling

Global options. Appearance UI exposes theme and side panel; other options here are TOML-only.

| Key | Factory default | Use |
| --- | --- | --- |
| `ui.theme` |amber| violet/cyan/amber color theme. |
| `ui.color` |auto| Color output: auto, none, ansi16, ansi256, truecolor. Choose what the terminal supports. |
| `ui.mouse` |true| Click controls or place the text cursor; drag text to copy on release. The drag highlight disappears after copying. |
| `ui.sidebar` |true| Default side-panel visibility. |
| `ui.scroll_lines` |2| Rows per wheel notch, 1–30. Ctrl/Alt can move three times as far where the terminal passes that combination through. |
| `ui.scroll_smooth` |true| Prefer a glide when supported; some terminals use jumps. |
| `ui.split_min_columns` |90| Minimum terminal columns for a side-by-side panel, 20–1000. |
| `ui.scroll_margin` |3| List/picker cursor margin, 0–100. |

Change the exposed control or use `rune -config set <key> <value>`; reset with `rune -config reset <key>`. Explain shows the current value. Changes normally affect subsequent display use without restarting. Do not confuse terminal profile settings with Rune's.

Risk: unsupported color modes can display poorly; a small split width can crowd content; disabling mouse removes mouse interactions, not keyboard navigation. Terminal-level Ctrl+wheel zoom/Alt+wheel behavior can override Rune. `--no-color`/NO_COLOR disable colors; RUNE_THEME is a terminal-background hint (dark/light/color), not the violet/cyan/amber theme selection.

## Copying text with the mouse

With `ui.mouse = true`, hold the left mouse button and drag over text, then release to copy. The highlight is temporary and disappears on release. Copy stays in the pane where the drag began: chat and tool contents do not include the adjacent sidebar or Rune's box borders. Tables copy their cells separated by tabs. Literal border characters in the text itself are kept.

Ordinary text fields use the same drag-to-copy behavior, including chat input, Models search, Settings values, session search/rename, Questions custom answers and MCP input. A drag does not move the text cursor or replace your draft; an ordinary click still places the cursor. Use Shift+arrows or Ctrl+Shift+Left/Right when you want to select input for replacement or deletion instead. Masked secret fields are not copied automatically.

The terminal must allow clipboard writes for copy to reach your clipboard; SSH or multiplexer configuration can restrict them. Shift-drag, where supported by your terminal, bypasses Rune and uses the terminal's own selection instead: it can include UI borders and follows the terminal's highlight behavior. With `ui.mouse = false`, selection belongs to the terminal too. Copying places the chosen text in your system clipboard; do not copy sensitive material unintentionally. A resize or a change to selected content cancels an unfinished drag; start the selection again.

## Response text and references

Assistant response prose, including intermediate responses, uses bright neutral ink on dark backgrounds and dark neutral ink on light ones. This does not brighten tool output, thinking, the sidebar or other UI text. A wrapped tool action keeps the same text color on every line.

With mouse interaction enabled, click a labelled reference to review its complete URL/path in **REFERENCE**. For an HTTP(S) URL, choose **Open in browser** or press Enter to hand that reviewed address to your system browser. Choose **Copy complete destination** or press C to copy it. Esc closes the panel. Opening a URL can contact that website in your browser; do not open a destination you do not trust.

File links and other unsupported schemes show their complete destination for copying only. Rune does not run the file, open it through an OS file association or add a file viewer. Dragging across a link copies selected text instead of opening it. Where the terminal provides native hyperlinks, its modifier-click behavior can still differ; Rune's reference panel does not require a terminal-native link click.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `ui.theme` | `"amber"` | violet / cyan / amber |
| `ui.color` | `"auto"` | auto / none / ansi16 / ansi256 / truecolor |
| `ui.mouse` | `true` | true / false |
| `ui.sidebar` | `true` | true / false |
| `ui.scroll_lines` | `2` | Whole number 1–30 |
| `ui.scroll_smooth` | `true` | true / false |
| `ui.split_min_columns` | `90` | Whole number 20–1000 |
| `ui.scroll_margin` | `3` | Whole number 0–100 |
