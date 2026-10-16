# API keys and account credentials

Location: Settings → Connections → account. Use Key name to distinguish credentials such as Personal or Work; enter the API key in Rune's credential field, not in chat.

## Add or remove
Add API key adds a named key to the account's available keys. It does not mean every existing key is replaced. Check the displayed name/masked preview before removing one; confirm only the intended removal.

Removing a connection/provider can keep its credentials available for reuse. Do not assume account removal securely erases every credential. Use the supported credential-management actions for the installed version if removal is the goal.

## Availability and testing
A key count or Configured status means credentials are present, not that a request succeeded or credit is available. Resting can mean a key is temporarily not ready. Tests are real requests and can cost money; ask before running them.

Multiple keys can be used as alternatives when a key is unavailable. Credit cooldown controls how long an out-of-credit key rests; it does not buy more credit. See [retry](../runtime/retry.md).

## When a key is missing or unavailable
Settings › Connections, `rune -provider list` and `rune -provider keys <name>` always show what Rune's data folder holds now:
- **Key missing**: the credential store has no key for that account, or the store itself is not in Rune's data folder. Add the key again.
- **Unavailable**: the store is there but cannot be opened. The message names the cause — its master key is missing or damaged, the store and key came from different machines or backups, the store is cut off, or it cannot be read.

Rune still starts; only what needs the key says why it cannot run. Rune never changes, replaces or deletes a store it cannot open, and never makes a new master key beside it, so the keys can still come back. The store and its master key belong together: back them up together, and restore both from the same backup.

If they cannot be restored, Settings › Connections › Start a new credential store (or `rune -provider new-store --yes`) sets the old pair aside in the backups folder and starts with no keys; every key must then be added again. Ask for the user's decision first.

## Privacy and risks
Never paste real keys into Markdown examples, source control, a shared configuration dump or a chat response. Do not read/export Rune's private credential data to answer a settings question. Masked account metadata is usually sufficient.

Command arguments can appear in shell history/process information. Prefer the user's credential-entry UI rather than constructing a command containing a real key. A setting that references a credential is not the credential value itself.

Changing an endpoint can send credentials and prompts to a different destination. Verify the intended destination and obtain the user's decision. A successful authentication test is not proof every operation or model is authorized.
