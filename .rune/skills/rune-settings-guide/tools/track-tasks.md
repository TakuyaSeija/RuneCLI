# Plan tasks

track_tasks keeps the current plan. Each update supplies the complete list; an empty list clears it. Tasks are todo/doing/done, with one active task. A plan item is a progress description, not permission to execute an operation.

Location: Settings → Tools → TRACK TASKS. Scope: global.
- `tasks.max_tasks`: factory 20, 1–1000 items.
- `tasks.max_chars_per_task`: factory 120, 1–10000 characters for each description.

Change a row and Apply. Use default/`rune -config reset <key>` resets; explain inspects. Later updates must fit the limits. Too-small limits can prevent useful plans, while large plans use more space/context. Prefer concise task descriptions rather than arbitrarily raising caps. Ctrl+T opens the plan view.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `tasks.max_chars_per_task` | `120` | Whole number 1–10000 |
| `tasks.max_tasks` | `20` | Whole number 1–1000 |
