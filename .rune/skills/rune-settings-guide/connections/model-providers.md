# Model provider accounts

Use Settings → Connections for endpoint and API-key access. Add connection asks for what the account needs: a preset with an address of its own asks for the API key alone, and Enter connects. A preset whose address depends on your account or region (Alibaba, Tencent TokenHub, Cloudflare, Custom) asks for that address first; Rune never guesses it, and you can change it on the same form. Connection name and Key name are filled in and can be changed before Connect. Nonsecret account choices are saved in rune.toml; API keys remain in the encrypted vault. Models selects a model; Runtime controls its request timeout. Connecting/testing an account makes a real request and may cost money. A successful save/test applies to the account tested, not a guarantee every model works.

## Main account choices
| Property | Purpose and risk |
| --- | --- |
| `name` | Local account name. Keep it distinct so the intended provider is selected. |
| `api` | Provider type/preset. Choose the correct type rather than renaming an account to pretend it is another service. |
| `base_url` | API address. Changing it can send prompts and credentials to another destination; verify ownership before changing. |
| `timeout` | Optional request timeout in whole seconds 5–3600. Edit under Runtime; empty/default removes the override. |
| `models` | Configured models and explicit model settings. Use Models and Alt+C where available. |

`providers.default` is the fallback provider for new sessions. `providers.excluded_models` lists hidden model IDs; use model Delete/re-add rather than changing it casually.

## Advanced TOML-only options
These do not have extra controls in the Connections page. Preserve working preset values unless you have a specific compatibility requirement.

| Property | User-facing meaning |
| --- | --- |
| `dialect` | API compatibility choice: open_ai or anthropic. Must match the endpoint; an incorrect choice can prevent requests working. |
| `thinking` | Provider reasoning-control compatibility setting: none/object/effort. Not the current conversation's Thinking switch. Prefer Generation for that. |
| `auth.header`, `auth.prefix` | Credential presentation required by the provider. Use its preset/documentation; wrong values can cause authentication failures. Never put a real key here. |
| `secret` | Reference to configured credentials, not the actual key. Use credential management rather than inventing references. |
| `headers` | Extra values an endpoint requires. They can contain private information; do not publish them or insert tokens in examples. |
| `max_payload_size` | Maximum total request size Rune may send. Default 30MiB if omitted, although presets can differ; positive and at most 1GiB. Larger values can increase data/memory use and cannot enlarge the provider's true limit. |
| `force_tool_choice` | Advanced compatibility option for providers accepting explicit tool selection. Leave at the preset/default unless solving a documented compatibility issue; not an access permission. |

## Change or reset
For addresses/keys use Connections, follow its validation and wait for success. Use default on an address restores the preset address. Custom endpoint/name setup is also available with `rune -provider add` options; consult `rune -help` for the installed version.

For TOML-only options, change only the relevant property in the intended account, preserve other accounts, then run `rune -config check`. Removing an optional property restores its applicable default; do not assume every nested property supports `-config reset`. A valid file still needs a real request to prove connectivity, and testing requires consent.

Catalog refresh timing is global: `network.catalog_timeout`, `network.catalog_ttl`, `network.catalog_max_pages`. See [network](../runtime/network.md). Model capability/pricing and current Generation choices are separate from account access.

A local Ollama model account and Ollama's web service need not use the same address/key. The correct endpoint depends on the service, not simply its displayed account name. See [web accounts](web-accounts.md) and [API keys](../permissions/credentials.md).

## Individual model presets
- [mimo](presets/mimo.md)
- [openai](presets/openai.md)
- [openrouter](presets/openrouter.md)
- [ollama](presets/ollama.md)
- [anthropic](presets/anthropic.md)
- [google](presets/google.md)
- [xai](presets/xai.md)
- [deepseek](presets/deepseek.md)
- [groq](presets/groq.md)
- [mistral](presets/mistral.md)
- [9router](presets/9router.md)
- [moonshot](presets/moonshot.md)
- [alibaba](presets/alibaba.md)
- [minimax](presets/minimax.md)
- [tencent](presets/tencent.md)
- [zai](presets/zai.md)
- [meta](presets/meta.md)
- [stepfun](presets/stepfun.md)
- [inclusionai](presets/inclusionai.md)
- [nvidia](presets/nvidia.md)
- [inference](presets/inference.md)
- [cloudflare](presets/cloudflare.md)
- [fireworks](presets/fireworks.md)
- [together](presets/together.md)
- [siliconflow](presets/siliconflow.md)
- [deepinfra](presets/deepinfra.md)
- [novita](presets/novita.md)
- [tokenrouter](presets/tokenrouter.md)
- [cerebras](presets/cerebras.md)
- [huggingface](presets/huggingface.md)
- [compatible](presets/compatible.md)
