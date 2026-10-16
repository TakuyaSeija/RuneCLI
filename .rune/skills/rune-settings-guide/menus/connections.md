# Settings › Connections

Use Connections to manage accounts, endpoint addresses and API keys. It does not choose the model or web provider used for a request.

The list combines model and web accounts. Keys shows a count. State describes credential availability; Configured, Keyless or Resting does not by itself mean a live request succeeded. Serves lists enabled uses such as Models, Search and Fetch.

## Add or edit an account
- Add connection → model provider: follow Key name/API key setup. Rune tests a new model account before accepting it.
- Add connection → Search & Fetch vendor: enter the required address/key, then Save account. This stores access; add and test its actual providers under Tools.
- Open an existing account to edit its Base URL and manage named keys. Some vendors show separate web/fetch addresses.
- `Use default` on an endpoint restores the provider/vendor address, not an address guessed by the AI.
- Remove a key only after confirming the intended key. Removing a connection may keep its keys available for reuse; do not assume it is credential erasure.

Model endpoint changes are tested before being accepted. A web account can be saved without enabling search/fetch; saving it alone is not proof a provider works. Tests make real requests and can cost money.

## Where request choices belong
- Model selection, catalog refresh and new-session fallback: [Models](models.md).
- Model request timeout: [Runtime](runtime.md).
- Web provider add/delete/select and gateway route: [Tools](tools.md).

There is no separate Quick Actions or Configuration section under Connections. Advanced account options are explained as TOML options in [model providers](../connections/model-providers.md) and [web accounts](../connections/web-accounts.md), not invented UI controls.

Changing an endpoint or provider can change where prompts, queries or pages are sent. Ask before changing it. Enter credentials in Rune's credential field, not in chat; see [API keys](../permissions/credentials.md).
