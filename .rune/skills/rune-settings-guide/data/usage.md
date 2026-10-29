# Usage and spend warning

The side panel can show context/output/cache usage and spending where known. Token usage comes from the provider's reports, never a text-length, thinking-length or image/`base64` conversion. Context usage is the latest reported input count, not a promise about the next request; changing models can change the capacity shown beside it. Unknown context or prices are not zero. A price calculation from reported usage and configured rates is not necessarily the provider's final bill; see [model prices](../models/pricing.md).

Spend counts every request of the session that has a price. A request whose model had no price when it ran is priced as soon as you set that model's price (Settings → Models → Alt+C), at the price it has then, in every window and session at once; a request that had a price when it ran keeps what it was charged. While some requests still have no price, Spend shows the total of the priced ones with a line below it, `N requests not priced`; with none priced at all it says `not priced`.

Key: `usage.warn_spend_usd`. Location: advanced rune.toml option. Scope: global. Factory 0 (warning disabled); accepted finite nonnegative USD amount.

Purpose: request a spending notice at the configured amount. It is **not a spending cap or guarantee that requests stop**. Do not depend on it to protect an account's balance.

Inspect with `rune -config explain usage.warn_spend_usd`. Set the intended amount only with consent; reset using `rune -config reset usage.warn_spend_usd`. Existing measured usage is not erased by changing the warning.

Risk: incomplete prices/usage can make costs unknown or estimates differ. More retries, larger output and auxiliary/connection tests can all add cost. Use provider-side limits where a hard budget is needed, and do not configure those external controls without the user's decision.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `usage.warn_spend_usd` | `0` | Finite nonnegative USD amount |
