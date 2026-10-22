# inclusionai model connection

Location: Settings → Connections → Add connection → MODEL PROVIDERS → inclusionai. Enter the API key and press Enter to connect; enter credentials in Rune, not chat. InclusionAI's Ling models through Ant Group's own Ling API (developer.ant-ling.com), with an Ant Ling API key. The 3.0 and 3.1 models switch thinking on or off; there are no levels. Off is sent only to a model you offered Off for (Alt+C › Reasoning). A Ling server you host yourself is added with the Custom preset instead.

A new model account is checked with a real request before accepted. This can cost money and send test data to the selected destination; get consent before testing. Saving one account does not prove every model is available.

Use Settings → Models to select an exact model, refresh its available list, or edit its capability/pricing with Alt+C. There is no permanent guaranteed model ID/price in this guide. Generation changes this session's behavior; Runtime holds the account's optional request timeout.

Change the address/keys in Connections. Use default on the address restores the preset; do not guess another endpoint after failure. Use Models → New-session fallback to choose a starting provider, not to switch every open session. Removing an account can keep credentials for reuse.

Risks: requests may be billed and prompts/attachments go to the selected account. Larger output/retry limits can add cost. Another address or provider is a user-owned privacy decision. Inspect the actual error before proposing changes.

[Model account options](../model-providers.md) · [API keys](../../permissions/credentials.md)
