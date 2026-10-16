# Web service accounts

Use Settings → Connections to save a web vendor's account access. Then use Tools → Web search/Web fetch → Provider to add and choose the service. Saving an account alone does not enable or prove a search/fetch provider.

## Account options
In rune.toml these appear in `services.list`:

| Property | Purpose |
| --- | --- |
| `name` | Local account name, up to 64 lowercase letters/digits/dot/underscore/hyphen. |
| `api` | The web vendor type. Choose one supported by the installed Rune. |
| `base_url` | Optional custom address; otherwise use the vendor default. Required for a self-hosted service with no default address. |
| `fetch_url` | Separate page-reading address only for vendors that support separate addresses. Do not add it to every account. |
| `search`, `fetch` | Enabled use of each service. Prefer Tools to manage these. |
| `routes.search`, `routes.fetch` | Gateway routes saved for each service, maximum 32 unique routes per service. Only applicable to gateways. |
| `zone` | Bright Data SERP zone where required; choose under the web Provider settings. |

Service selection details such as `route` (active gateway route) and `added` (addition order information) are managed through Tools. Do not edit ordering information to guess which service will be chosen.

## Change or reset
Open Connections → account → address/key field, edit and save. Use default restores an available vendor address. Accounts for user-owned servers need the user's real intended server address, not a sample copied from a guide.

Use Tools for enabling, adding/removing routes and selecting a provider; see [provider steps](web-providers.md). A direct vendor does not take a gateway route. Service choices can be removed without erasing account credentials.

For a necessary TOML edit, change only the intended account and validate with `rune -config check`; do not assume every nested property has a generic reset command.

## Effects and risks
The selected service receives the search query or target page address. A changed endpoint can redirect that information and credentials; obtain the user's decision first. Remote keyed services require HTTPS; keyless/self-hosted exceptions do not mean every private web page becomes fetchable.

Keep credentials in Rune's credential interface, not the account's TOML or chat. A configured account is not proof it has credit or that its requests succeed. See [Connections](../menus/connections.md) and [API keys](../permissions/credentials.md).
