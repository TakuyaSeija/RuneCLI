# Choose a web provider

Location: Settings → Tools → Web search or Web fetch → Provider. Each tool has its own choice. Connections manages the account address and keys, not the request choice.

## Add
1. Add the required account in Connections first.
2. Open the relevant tool's Provider → Add provider and select that connected account.
3. For a gateway, type the exact route in the empty field. Ask the user for the intended route; do not invent or prefill one. A direct vendor does not need a route.
4. Rune makes a real test request before keeping the provider. It can be billable. Adding a provider does not explicitly select/pin it.
5. Return to Provider and press Enter on the intended provider to test and select it.

A failed add/selection should leave the previous choice in place. Do not report it as successful. Known examples or a provider's catalog are not proof a route works for the user's account.

## Default versus explicit selection
- `web_search.provider` omitted/default: first enabled search provider by its addition order. Adding the first one can consequently make it the default in use.
- `web_fetch.provider` omitted/default: Rune's built-in page reader. Adding an external reader does not automatically select it.
- Explicit selection: exact account, or `account:route` for a gateway. If it is missing/unavailable, the tool does not silently use a different provider.

Use default removes an explicit pin. Reset command: `rune -config reset web_search.provider` or `rune -config reset web_fetch.provider`. These are changes, not diagnostic commands.

## Delete
Select another provider/default before deleting the provider currently used. Delete removes it from this tool only; the account and keys remain. A gateway route can be removed independently of its other routes. Maximum 32 saved routes per service.

## Risks and troubleshooting
Tests send real queries/page requests to the account. Choosing an external page reader sends it the target page address. Changing the provider therefore has cost/privacy implications: ask before doing it.

If unavailable, check the selected account, its credentials, intended route and service. Do not silently change the pin or select a different third party. See [web accounts](web-accounts.md) and [Tools](../menus/tools.md).
