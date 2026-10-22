# alibaba model connection

Location: Settings → Connections → Add connection → MODEL PROVIDERS → alibaba. Enter your address first, then the API key, and press Enter to connect; enter credentials in Rune, not chat. This provider has no single address: enter your own API base URL. Use the compatible-mode address of your Model Studio workspace and region, with a key from the same region. Thinking is switched on or off; there are no levels. Off is sent only to a model you offered Off for (Alt+C › Reasoning) or whose model list says it can stop; otherwise the model keeps its own default. Never offer Off for a model that always thinks (for example `glm-5.3` or `kimi-k2.7-code`): it refuses it. GLM models on Model Studio return tool calls only with an option Rune does not send, so use them through Z.ai instead.

A new model account is checked with a real request before accepted. This can cost money and send test data to the selected destination; get consent before testing. Saving one account does not prove every model is available.

Use Settings → Models to select an exact model, refresh its available list, or edit its capability/pricing with Alt+C. There is no permanent guaranteed model ID/price in this guide. Generation changes this session's behavior; Runtime holds the account's optional request timeout.

Change the address/keys in Connections. Use default on the address restores the preset; do not guess another endpoint after failure. Use Models → New-session fallback to choose a starting provider, not to switch every open session. Removing an account can keep credentials for reuse.

Risks: requests may be billed and prompts/attachments go to the selected account. Larger output/retry limits can add cost. Another address or provider is a user-owned privacy decision. Inspect the actual error before proposing changes.

[Model account options](../model-providers.md) · [API keys](../../permissions/credentials.md)
