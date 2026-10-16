# Settings › Runtime

Runtime controls request waiting and retry behavior, not tool permissions.

## Model request timeouts
Open the provider's row to set an optional timeout in whole seconds, 5–3600. Blank/default removes that override. A request-specific timeout can still differ, particularly for auxiliary jobs.

A model request timeout is different from waiting for a foreground terminal command, and different from the time allowed with no streaming data. Choose the setting for the actual symptom instead of raising every timeout.

## Retry & Resilience
Retries, Initial step, Max delay and Credit cooldown are explained in [retry](../runtime/retry.md). They are global. Very large retry counts can cause prolonged waiting and repeated charges without solving an invalid key or unsupported request.

## Network & Stream Timeouts
Stall limit, Connect timeout and Probe timeout are explained in [network](../runtime/network.md). Catalog and advanced response-size settings are available through TOML rather than extra UI rows here.

To change a value, select its row, edit and Apply. Use default removes a custom global choice. Use `rune -config explain <key>` when you need its current value or timing. Work already running may keep its starting value; stream-idle timeout can apply while a stream is waiting.

Terminal command timeout, web tool timeout, question wait and permission wait belong to Tools. Detached-session retention is a separate [idle lifetime](../runtime/host.md) option.
