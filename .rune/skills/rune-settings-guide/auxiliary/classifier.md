# Classifier settings

Location: Settings → Generation → classifier rows. Scope: global settings; policy reload affects the selected session.

Purpose: review proposed actions against the user's CLASSIFIER.md policy. It needs usable policy and Auto/Full mode; it is inactive in Ask. Enabled alone does not prove review is active: read Classifier status.

| Key | Factory default | Use |
| --- | --- | --- |
| `classifier.enabled` |true| Enable review when applicable. |
| `classifier.model` |Inherit active model| Choose the review model, or inherit. |
| `classifier.timeout` |120s| Maximum time for a review request. |
| `classifier.max_output_tokens` |1024| Output allowance, configurable 16–10000000; actual model limits apply. |
| `classifier.excluded_tools` |Default tool list| Tools excluded from classifier review only. It does not grant them unconditional access. [] requests review of all applicable tools. |
| `classifier.context_tools` |Default context-tool list| Types of recent tool inputs included as context; up to the last three matching calls. [] includes none. This is separate from exclusions. |
| `classifier.max_evidence_size` |256KiB| Amount of text evidence available for review. |
| `classifier.max_policy_size` |64KiB| Maximum size of each policy file. |

Use `rune -config defaults` or the list editor to see the complete default tool lists; do not infer them from their displayed counts. The current UI editor additionally caps evidence at 64MiB and each policy at 16MiB; a larger general size-format allowance is not a promise that this editor accepts it.

## Change and reload
Select a row, edit/choose and confirm Apply. For a tool list, select the intended names then Apply. Use default restores the default/inheritance; [] is not a reset.

CLASSIFIER.md can supply global or project policy. Change policy only according to the user's instructions. An existing session does not automatically adopt edited policy: when idle, choose Reload classifier policy or `/classifier reload`. Failed reload keeps the previous policy. A new session/host uses the current policy.

For current values use `rune -config explain <key>`. Changes apply to subsequent review operations. Do not diagnose absence of a review solely from the enabled flag; mode, policy, exclusions and an explicit human authorization can affect whether review is used.

## Effects and risks
Disabling review, excluding tools or reducing evidence/policy allowance can reduce review coverage. A smaller/slower review model can affect quality, latency and cost. Selecting another provider may send policy and relevant work there. A failed review is not an approval. This feature is not a guarantee every harmful action is prevented.

Explain the consequences and ask before changing safety-related choices. See [permission modes](../permissions/modes.md), [sanitizer](sanitizer.md) and [auxiliary allowance](budgets.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `classifier.excluded_tools` | `["ask_user_questions","track_tasks","web_search","web_fetch","find_files","inspect_file","read_mcp_resource","discover_capabilities","search_text"]` | Unique exact tool names; [] is an explicit empty selection |
| `classifier.context_tools` | `["terminal_use","inspect_file","find_files","search_text","create_file","patch_file"]` | Unique exact tool names; [] is an explicit empty selection |
| `classifier.enabled` | `true` | true / false |
| `classifier.model` | `Inherit / unset` | Configured {provider,id}, or omit to inherit |
| `classifier.timeout` | `"120s"` | Positive duration up to 24h |
| `classifier.max_output_tokens` | `1024` | Whole number 16–10000000 |
| `classifier.max_evidence_size` | `"256KiB"` | Positive explicit size up to 1GiB |
| `classifier.max_policy_size` | `"64KiB"` | Positive explicit size up to 1GiB |
