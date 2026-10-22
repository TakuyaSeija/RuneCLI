# tokenrouter model connection

Location: Settings → Connections → Add connection → MODEL PROVIDERS → tokenrouter. Enter the API key and press Enter to connect; enter credentials in Rune, not chat. TokenRouter (tokenrouter.com), at the address its console publishes. Use a key from your tokenrouter.com account; similarly named services (tokenrouter.io, tokenrouter.me) are different companies with their own addresses.

A new model account is checked with a real request before accepted. This can cost money and send test data to the selected destination; get consent before testing. Saving one account does not prove every model is available.

Use Settings → Models to select an exact model, refresh its available list, or edit its capability/pricing with Alt+C. There is no permanent guaranteed model ID/price in this guide. Generation changes this session's behavior; Runtime holds the account's optional request timeout.

Change the address/keys in Connections. Use default on the address restores the preset; do not guess another endpoint after failure. Use Models → New-session fallback to choose a starting provider, not to switch every open session. Removing an account can keep credentials for reuse.

Risks: requests may be billed and prompts/attachments go to the selected account. Larger output/retry limits can add cost. Another address or provider is a user-owned privacy decision. Inspect the actual error before proposing changes.

[Model account options](../model-providers.md) · [API keys](../../permissions/credentials.md)
