# Display, mouse and scrolling

Global options. Appearance UI exposes theme and side panel; other options here are TOML-only.

| Key | Factory default | Use |
| --- | --- | --- |
| `ui.theme` |amber| violet/cyan/amber color theme. |
| `ui.color` |auto| Color output: auto, none, ansi16, ansi256, truecolor. Choose what the terminal supports. |
| `ui.mouse` |true| Mouse interaction. With reporting on, Shift-drag can select terminal text. |
| `ui.sidebar` |true| Default side-panel visibility. |
| `ui.scroll_lines` |2| Rows per wheel notch, 1–30. Ctrl/Alt can move three times as far where the terminal passes that combination through. |
| `ui.scroll_smooth` |true| Prefer a glide when supported; some terminals use jumps. |
| `ui.split_min_columns` |90| Minimum terminal columns for a side-by-side panel, 20–1000. |
| `ui.scroll_margin` |3| List/picker cursor margin, 0–100. |

Change the exposed control or use `rune -config set <key> <value>`; reset with `rune -config reset <key>`. Explain shows the current value. Changes normally affect subsequent display use without restarting. Do not confuse terminal profile settings with Rune's.

Risk: unsupported color modes can display poorly; a small split width can crowd content; disabling mouse removes mouse interactions, not keyboard navigation. Terminal-level Ctrl+wheel zoom/Alt+wheel behavior can override Rune. `--no-color`/NO_COLOR disable colors; RUNE_THEME is a terminal-background hint (dark/light/color), not the violet/cyan/amber theme selection.

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
