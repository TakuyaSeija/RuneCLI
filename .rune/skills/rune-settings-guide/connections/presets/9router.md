# 9router model connection

Location: Settings → Connections → Add connection → MODEL PROVIDERS → 9router. Follow the displayed key requirements; enter credentials in Rune, not chat. A gateway needs its configured upstream account/model. A local default address is not proof a server runs on every user's machine.

## Claude Code's models: cache and thinking counted

One 9router connection, with one key, serves every model. For Claude Code's models (`cc/…`), Rune talks to 9router's Anthropic door (`/v1/messages`) by itself, because 9router's OpenAI-compatible answer leaves out their cache and thinking counts; every other model goes through the usual OpenAI-compatible door, where its counts already arrive. There is nothing to choose or set up for this. Claude Code's models send no thinking text; Rune asks Anthropic to report the thinking's tokens as it runs, as Claude Code does, and shows exactly what it reports, then the billed number ([reasoning](../../models/reasoning.md)). A second 9router connection made by Rune 0.8.2 or 0.8.3 for this is folded into your 9router connection when you upgrade; its key stays in the credential store.

Thinking choices follow one rule across 9router's two doors (Off, the available levels, Route default). For Claude Code's models they are sent in Claude's own controls. When a route reports that thinking cannot be disabled, Off requests Low instead, and Minimal is excluded from Alt+C and Generation. Otherwise Off requests disabled thinking. Off is a request, not a guarantee: the router/provider may change it, and even a route reporting that it can disable thinking may translate it to a positive effort. Low is not claimed to be every route's minimum. Route default sends no thinking choice, letting the route decide; omitting a choice can enable a positive default rather than turn thinking off.

A new model account is checked with a real request before accepted. This can cost money and send test data to the selected destination; get consent before testing. Saving one account does not prove every model is available.

Use Settings → Models to select an exact model, refresh its available list, or edit its capability/pricing with Alt+C. There is no permanent guaranteed model ID/price in this guide. Generation changes this session's behavior; Runtime holds the account's optional request timeout.

Change the address/keys in Connections. Use default on the address restores the preset; do not guess another endpoint after failure. Use Models → New-session fallback to choose a starting provider, not to switch every open session. Removing an account can keep credentials for reuse.

Risks: requests may be billed and prompts/attachments go to the selected account. Larger output/retry limits can add cost. Another address or provider is a user-owned privacy decision. Inspect the actual error before proposing changes.

[Model account options](../model-providers.md) · [API keys](../../permissions/credentials.md)
