# Usage and spend warning

The side panel can show context/output/cache usage and spending where known. Unknown context or prices are not zero. A local estimate is not a provider's final bill; see [model prices](../models/pricing.md).

Key: `usage.warn_spend_usd`. Location: advanced rune.toml option. Scope: global. Factory 0 (warning disabled); accepted finite nonnegative USD amount.

Purpose: request a spending notice at the configured amount. It is **not a spending cap or guarantee that requests stop**. Do not depend on it to protect an account's balance.

Inspect with `rune -config explain usage.warn_spend_usd`. Set the intended amount only with consent; reset using `rune -config reset usage.warn_spend_usd`. Existing measured usage is not erased by changing the warning.

Risk: incomplete prices/usage can make costs unknown or estimates differ. More retries, larger output and auxiliary/connection tests can all add cost. Use provider-side limits where a hard budget is needed, and do not configure those external controls without the user's decision.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `usage.warn_spend_usd` | `0` | Finite nonnegative USD amount |
