# Model prices

Location: Settings → Models → selected model → Alt+C. Prices are USD per million tokens. They help estimate spending; they do not change what the provider charges.

| Row / model price property | Meaning |
| --- | --- |
| Input price / `input` | Price of fresh input tokens. |
| Output price / `output` | Price of generated output tokens. |
| Cached read price / `cached` | Price of input read from cache. |
| Cache write price / `cache_write` | Price of cache creation. |
| 5-minute / 1-hour cache write price / `cache_write_5m`, `cache_write_1h` | Separate rates where the provider bills these durations differently. These do not select a cache duration. |
| Context tiered pricing / `tier_context_threshold`, `tier_input`, `tier_output` | Both input/output rates used when the input token count is above the positive threshold. |
| Peak hours pricing / `busy_input`, `busy_output` | Both rates used during the configured peak period. |
| Peak start/end hour / `busy_start_utc_hour`, `busy_end_utc_hour` | UTC whole hours 0–23. Start is included, end excluded; an overnight interval is possible. Equal hours cover all day. |

## Change or reset
Open the appropriate row, enter a finite nonnegative rate, then Apply. Zero means known-free; unknown/not applicable is different. Use a clear/default action only where offered; do not enter 0 to mean unknown.

Context tiered pricing opens a subpage: supply the positive threshold and both rates together. Clearing the threshold disables that manual tier. Peak pricing also has its own subpage; use both rates and the intended UTC hours. Read-only provider tiers may appear without editable controls.

Advanced `overrides` can describe additional published tiers: `min_prompt_tokens` is the token threshold, `inclusive` says whether equality qualifies, and `rates` contains the applicable prices. Prefer published prices or supported editors; do not invent tier rules.

## Effects and risks
Wrong rates or hours make estimates misleading. Rates should match the exact provider/model, and can change over time. An input price is not a model's context limit. A cached rate does not enable caching or guarantee a cache hit.

Unknown usage or prices may prevent a reliable estimate. A provider-reported charge can differ from a local estimate. Use the provider's billing as the authority; this is not a spending cap. Confirm any pricing change rather than silently replacing the user's choices during refresh.
