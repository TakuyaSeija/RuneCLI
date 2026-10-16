# File changes, undo and redo

`/undo` offers files changed by this session. Preview the diff, then confirm the intended restore; undo returns that file to before this session first changed it, not merely the last edit. Later external changes can be overwritten too. `/redo` can put back what the latest undo replaced; it is not an unlimited independent backup history.

The side panel shows current file-change counts; it is not a sum of every displayed edit title. Alt+S in the undo/redo list opens a diff. Enter starts confirmation and a second Enter within its interval restores. Do not invoke it while other work is running.

Advanced global TOML:
| Key | Factory default | Purpose and range |
| --- | --- | --- |
| `checkpoint.max_file_size` |64MiB| Largest file Rune can protect for eligible changes. An oversized write can fail rather than proceed unprotected. |
| `checkpoint.refresh_interval` |1s| Refresh cadence for already tracked files, 100ms–1m. Faster updates consume more resources. |
| `checkpoint.diff.max_cells` |1000000| Diff-comparison work allowance, 100–10000000. Affects comparison strategy, not permission or a maximum number of file changes. |
| `checkpoint.diff.context` |3| Surrounding context lines in displayed diffs, 0–100. |

Inspect with explain; change a specific key with set only when requested; reset using `rune -config reset <key>`. New uses follow the choice. There are no dedicated current Settings rows for these advanced options.

Risk: restoring overwrites the file's current state. Verify the file/diff and ask before restore/pruning. Increasing size/work limits uses more storage/processing; decreasing them can stop writes or reduce comparison detail. No undo setting makes arbitrary external commands or remote actions fully reversible.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `checkpoint.max_file_size` | `"64MiB"` | Positive explicit size up to 1GiB |
| `checkpoint.refresh_interval` | `"1s"` | Duration 100ms–1m |
| `checkpoint.diff.max_cells` | `1000000` | Whole number 100–10000000 |
| `checkpoint.diff.context` | `3` | Whole number 0–100 |
