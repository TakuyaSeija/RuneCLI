# Detached-session idle lifetime

Key: `host.idle_lifetime`. Location: advanced rune.toml option, not a current Runtime row. Scope: global. Factory:60s; positive duration up to 24h.

Purpose: how long a session is retained without an attached client before it is closed. It is not an account logout timer or a time limit for a session still being watched.

When the last window leaves, the active AI work stops. Background commands may continue during the retention period. Reopen in time to resume and see pending notifications; do not assume background servers run indefinitely after all Rune windows close.

## Change or reset
Inspect: `rune -config explain host.idle_lifetime`.
Change, only if requested: `rune -config set host.idle_lifetime 2m`.
Reset: `rune -config reset host.idle_lifetime`.

## Effects and risks
A longer interval gives more time to reopen accidentally closed work, but can retain background resource use longer. A shorter one can end background work sooner. Coordinate with the user before changing it while important background work is running.

Do not delete Rune data or force-close live sessions merely to apply a preference. Updates and currently running work can require time to settle; report the actual status instead of promising a forced restart is necessary.

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `host.idle_lifetime` | `"60s"` | Positive duration up to 24h |
