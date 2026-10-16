# Tool-call allowance

Key: `tools.max_actions_per_response`. Location: Settings → Tools → EXECUTION & ORCHESTRATION. Scope: global. Factory 20; range 1–100 whole calls.

Purpose: cap the number of tool calls a model can propose in one response. It is not a total task/turn budget, a number of parallel workers, or a blanket approval for those calls. A complete user task can need multiple responses.

Change the row and Apply, or `rune -config set tools.max_actions_per_response <count>` when requested. Use default/`rune -config reset tools.max_actions_per_response` resets. Inspect with explain. The next applicable operation uses the choice.

Risk: small values may slow multi-step work; large values can allow more proposed activity/output in one response. Individual permissions and tool limits still apply. Do not add a retired tools.max_parallel key or assume all calls run together.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `tools.max_actions_per_response` | `20` | Whole number 1–100 |
