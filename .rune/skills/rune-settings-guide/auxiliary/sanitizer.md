# Sanitizer settings

Location: Settings → Generation → sanitizer rows. Scope: global, for every session using these settings.

Purpose: help the classifier identify the user's original instructions and restrictions. It is not a general tool for cleaning files or rewriting the user's messages. Turning it off does not by itself turn classifier review off.

| Key | Factory default | Use |
| --- | --- | --- |
| `sanitizer.enabled` |true| Enable this preparation for applicable classifier reviews. |
| `sanitizer.model` |Inherit active model| Choose the model used for this work. An explicit choice needs an already configured provider/model. |
| `sanitizer.timeout` |120s| Maximum time for an extraction request. |
| `sanitizer.max_output_tokens` |4096| Output allowance; allowed setting 256–10000000, still constrained by the selected model. |

## Change or reset
Open the row, choose/edit its value, then confirm Apply. Use default restores its default/inheritance. To inspect one key, use `rune -config explain sanitizer.enabled` with the desired key. Global CLI reset example: `rune -config reset sanitizer.model`.

New choices apply to subsequent operations, not retroactively to a review already completed. It runs only where classifier review is applicable; enabling it does not mean it is used for every action.

## Effects and risks
An additional model operation can add latency and cost. Selecting another provider can send relevant user instructions there. Short deadlines or inadequate output allowances can prevent useful extraction. If extraction is unavailable, original user instructions can still be used; do not interpret that as the user's restrictions being removed.

Ask before disabling it or choosing another provider. See [classifier](classifier.md) for the review settings and [auxiliary allowance](budgets.md) for model limits.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `sanitizer.enabled` | `true` | true / false |
| `sanitizer.model` | `Inherit / unset` | Configured {provider,id}, or omit to inherit |
| `sanitizer.timeout` | `"120s"` | Positive duration up to 24h |
| `sanitizer.max_output_tokens` | `4096` | Whole number 256–10000000 |
