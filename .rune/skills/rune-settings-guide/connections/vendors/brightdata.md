# Bright Data SERP web account

Vendor choice: `brightdata`. Configure access in Settings → Connections → Add connection → Search & Fetch. Key requirement: required. Enter any actual key in the credential field, never chat/TOML examples.

Rune offers search only for this vendor. Search result ceiling here: 10; Rune's web_search.results and the actual available results can be lower. Do not assume every feature on the vendor's website is supported in Rune.

Also requires the intended SERP zone, selected under Tools → Web search → Provider.

After saving the account, go to Tools → the supported web tool → Provider → Add provider. A real test must succeed; then select the intended provider explicitly. Tests and later requests can be billable and send queries/page addresses to that service. Ask before testing or selecting.

To change access, edit this account in Connections; Use default restores an available vendor address. To change/reset use, choose another provider or Default in Tools. Removing a tool provider does not erase account credentials. A failed explicit provider must not silently send data to another vendor.

[Web provider steps](../web-providers.md) · [Account options](../web-accounts.md) · [Vendor index](../web-vendors.md)
