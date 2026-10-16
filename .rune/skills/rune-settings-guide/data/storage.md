# Storage and session cleanup

Rune keeps local conversation/project data in the selected Rune home. Changing a workspace is not deleting its stored history. Use supported session/storage commands rather than removing live data files manually.

Advanced global TOML:
| Key | Factory default | Purpose |
| --- | --- | --- |
| `storage.busy_timeout` |5s| How long to wait when local storage is busy. |
| `storage.scratch_lifetime` |24h| Age before abandoned temporary work is eligible for startup cleanup. Not a timer that deletes an actively open task's files. |
| `storage.session_limit` |200| Default maximum session-list results, 1–10000. Does not mean older sessions were deleted. |

Inspect with explain; change only the desired key and reset using `rune -config reset <key>`. Scratch cleanup lifetime takes effect on restart; other consumers follow their documented timing. Positive durations have the general bounds.

`rune -storage list` lists kept project storage. `rune -storage prune` removes eligible data for missing projects, not arbitrary old active projects. Review the list and obtain explicit consent before pruning or deleting a session. Session delete is permanent; compaction is not disk cleanup.

Risk: deleting data can remove conversation/undo information. Do not delete runtime locks or files used by a running Rune, or copy a live store as an assumed consistent backup. A different `-db` store is separate from choosing `RUNE_HOME`; confirm the actual intended home/store for maintenance.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `storage.busy_timeout` | `"5s"` | Positive duration up to 24h |
| `storage.scratch_lifetime` | `"24h"` | Positive duration up to 24h |
| `storage.session_limit` | `200` | Whole number 1–10000 |
