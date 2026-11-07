# Retry & Resilience

Location: Settings → Runtime. Scope: global. Retries can help temporary failures, but cannot repair a wrong key, invalid endpoint or unsupported model setting.

| Key / row | Factory default | Meaning |
| --- | --- | --- |
| `retry.max_attempts` / Attempts |5| Total attempts one call may make, across every key in its ring, range 0–4294967295. Zero disables retrying. |
| `retry.attempts_per_key` / Attempts per key |10| How many attempts one key gets before the next key in its ring is used, range 1–1000000. |
| `retry.initial_delay` / Initial step |3s| Base step for the wait between two attempts on the same key. |
| `retry.max_delay` / Max delay |1m| When the next wait would exceed this, the sequence restarts from the base step. |

## Two budgets, and what a key is worth

One key is tried `retry.attempts_per_key` times, and each failure waits on the curve below. When that key's allowance is spent it hands over to the next key in the ring **without a wait**, because a fresh key has nothing to wait out. Once every key has spent its allowance the ring starts over, and the whole call stops when `retry.max_attempts` is gone. With a single key there is no ring: that key is simply tried until the global budget is spent.

Every failure spends one attempt of the key in use — a refused key, a rate limit, a server error, a dropped stream alike. Rune keeps no memory of a key between calls: there is no resting or dead state, so a key that failed in the last request is tried first again in the next one. The only thing that changes is the order inside the ring, which moves a spent key to the back and is written down for the next start.

The wait increases linearly: step 3s gives 3, 6, 9, ... seconds. With a 10s max delay it becomes 3, 6, 9, 3, ... . This is not exponential backoff.

## Change or reset
Select the row, enter the value and Apply. Use default restores the factory default. CLI examples for an intended change: `rune -config set retry.attempts_per_key 10`; reset with `rune -config reset retry.attempts_per_key`.

Inspect a particular key with `rune -config explain <key>`. New choices apply to subsequent request operations; do not assume they rewrite a retry operation already underway.

## Effects and risks
Very large counts or waits can make failures take a long time to finish, and repeated requests may cost money. Reducing retries can make transient problems fail sooner. More retries are not automatically more reliable, and not every kind of failure is retried.

Check the failure shown in `/logs` before changing retry settings. The status line names the key in use and both counts, for example `Key A · attempt 3 of 10 · 21/100000`, and a key change is written to the transcript as well. A slow but active model may need a different setting from a completely silent stream; see [network](network.md). Ask before changing an account, testing keys or increasing spending.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `retry.max_attempts` | `5` | Whole number 0–4294967295 |
| `retry.attempts_per_key` | `10` | Whole number 1–1000000 |
| `retry.initial_delay` | `"3s"` | Positive duration up to 24h |
| `retry.max_delay` | `"1m"` | Positive duration up to 24h |
