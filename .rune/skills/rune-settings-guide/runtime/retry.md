# Retry & Resilience

Location: Settings → Runtime. Scope: global. Retries can help temporary failures, but cannot repair a wrong key, invalid endpoint or unsupported model setting.

| Key / row | Factory default | Meaning |
| --- | --- | --- |
| `retry.max_attempts` / Retries |5| Retries after the initial attempt, range 0–4294967295. Zero disables retries. Five can mean up to six attempts in total. |
| `retry.initial_delay` / Initial step |3s| Base step for the retry wait. |
| `retry.max_delay` / Max delay |1m| When the next wait would exceed this, the sequence restarts from the base step. |
| `retry.credit_cooldown` / Credit cooldown |15m| How long an out-of-credit key rests before it can be used again. Does not replenish credit. |

The wait increases linearly: step 3s gives 3, 6, 9, … seconds. With a 10s max delay it becomes 3, 6, 9, 3, … . This is not exponential backoff.

## Change or reset
Select the row, enter the value and Apply. Use default restores the factory default. CLI examples for an intended change: `rune -config set retry.max_attempts 5`; reset with `rune -config reset retry.max_attempts`.

Inspect a particular key with `rune -config explain <key>`. New choices apply to subsequent request operations; do not assume they rewrite a retry operation already underway.

## Effects and risks
Very large counts or waits can make failures take a long time to finish, and repeated requests may cost money. Reducing retries can make transient problems fail sooner. More retries are not automatically more reliable, and not every kind of failure is retried.

Check the failure shown in `/logs` before changing retry settings. A slow but active model may need a different setting from a completely silent stream; see [network](network.md). Ask before changing an account, testing keys or increasing spending.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `retry.max_attempts` | `5` | Whole number 0–4294967295 |
| `retry.initial_delay` | `"3s"` | Positive duration up to 24h |
| `retry.max_delay` | `"1m"` | Positive duration up to 24h |
| `retry.credit_cooldown` | `"15m"` | Positive duration up to 24h |
