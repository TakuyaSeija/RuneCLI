# Model provider presets added 2026-10-22

Twenty OpenAI-compatible presets, each built like the older ones: an address, a key, and one `Thinking` kind (`providers.rs` `PRESETS`, `openai_compatible`). No per-model facts are compiled in; a model's capabilities come from its catalogue or from the person (Settings › Models › Alt+C). Vendor pages were read 2026-10-22; nothing was tested against a vendor's API.

## How each part follows the older presets

- **Thinking kind.** `Effort` sends `reasoning_effort` (as openai, google, deepseek, xai, groq, 9router); `Object` sends a `thinking` object and the level beside it (as mimo). Z.ai, Tencent TokenHub and InclusionAI (Ant Ling API) are `Object`; every other added preset is `Effort`.
- **A different wire.** Where an API switches thinking in a field neither kind writes, `ModelSettings::body` has an adapter branch, as it has for `google` and `9router`: `enable_thinking` for `alibaba`, `siliconflow`, `novita`; `reasoning_split: true` for `minimax` and `separate_reasoning: true` for `novita`, so thinking arrives apart from the answer instead of as `<think>` text.
- **Off.** On the added adapters Off is sent only to a model known to stop — its catalogue says `reasoning.can_disable`, or the person offered Off for it in Alt+C › Reasoning (`ModelInfo::turns_off`) — as the `Effort` rule always did for the catalogue (owner, 2026-10-22); a model that must think (glm-5.3, kimi-k3, …) refuses `disabled`/`false`/`none`. The `Effort` rule now also honours the Alt+C offer. MiMo, the older `Object` adapter, still sends Off to every model. Inference.net's `none` in a model's level list is read as `can_disable`.
- **Output field.** The older rule sends `max_completion_tokens` to a reasoning-capable OpenAI-dialect endpoint. Adapters whose reference names only `max_tokens` are listed in `MAX_TOKENS_ONLY` (moonshot, alibaba, zai, stepfun, siliconflow, novita, together, deepinfra, inference, inclusionai); Novita requires it.
- **Levels, first source: the catalogue.** The shared reader (`manage.rs` `catalogue_entry`) also reads `reasoning_efforts` (Inference.net, per model) and `supports_reasoning` (Moonshot). Catalogue levels keep the older rule: the least is where a session starts.
- **Levels, second source: an adapter table.** Where the API documents one level set for all models and its catalogue lists none, `ADAPTER_LEVELS` gives it, as `ROUTER_LEVELS` does for 9router: zai (minimal–max), fireworks (low–max), deepinfra (low–high), meta (minimal–xhigh), stepfun (low–high), tencent (low–high). Settled like the router's table — re-derived each settle, the person's `reasoning_options` or Alt+C levels first — and, like the router's, a table level is never filled in for the person: no chosen level sends none, so the API's default holds (`route_default_level`).
- **Adapters with no level set.** Moonshot, MiniMax, Cerebras, Together, NVIDIA, Hugging Face, Cloudflare, TokenRouter, InclusionAI and Custom publish levels per model only, or not at all: the person adds a model's documented levels with Alt+C, as for openai or google.
- **Catalogue shapes.** Per-adapter rewriting before the shared reader (`vendor_row`), like the xai/mimo/ollama cases: Together `pricing.input/output` and DeepInfra `metadata.pricing` are USD per million tokens, Novita `pricing.*.price_per_m_decimal` likewise; DeepInfra `metadata.tags` and Novita `features` name `reasoning`. Cloudflare lists models at the account's `/ai/models/search?format=openrouter`.
- **Usage.** The shared usage reader also takes flat `cached_tokens` (StepFun reference), beside DeepSeek's `prompt_cache_hit_tokens`.
- **Address.** A preset with one address for everyone carries it, so Add Connection asks for the key alone and Enter connects (`ask_model_field`). An address that depends on the person's account, region or site — Alibaba (region/workspace), Tencent TokenHub (site), Cloudflare (account ID), Custom — is the person's to enter, never guessed (owner, 2026-10-22): its preset has an empty `base_url` and a `url_hint` naming the official addresses, the form asks for it first, and `check` refuses an empty one. Key optional only on Custom, like ollama/9router.

## Sources (read 2026-10-22)

| Adapter | Facts used | Source |
| --- | --- | --- |
| moonshot | address, `max_tokens`, `supports_reasoning` in the model list; K3 levels low/high/max, K2 none | platform.kimi.ai/docs/api/models-overview, /docs/api/list-models |
| alibaba | `enable_thinking` top-level in raw HTTP, no levels, region-bound keys | alibabacloud.com/help/en/model-studio/deep-thinking |
| minimax | `reasoning_split`, `max_completion_tokens`, levels only on M3.1-Flash | platform.minimax.io/docs/api-reference/text-openai-api |
| tencent | TokenHub regional addresses, `thinking` + `reasoning_effort` low/medium/high | tencentcloud.com/document/product/1300/80632 and /80695 |
| zai | `thinking` object, `reasoning_effort` minimal–max (default max), `max_tokens`, tool choice auto | docs.z.ai/api-reference/llm/chat-completion |
| meta | `reasoning_effort` minimal–xhigh (`none` refused), `stop` refused | dev.meta.ai/docs/protocols/chat-completions |
| stepfun | `reasoning_effort` low/medium/high, `max_tokens`, flat `cached_tokens` | platform.stepfun.ai/docs/en/api-reference/chat/chat-completion-create.md |
| inference | `max_tokens`, `reasoning_effort` (none…max, `none` = off), `reasoning_efforts` per model in the list | docs.inference.net/api/api-quickstart, /api/reasoning |
| fireworks | `reasoning_effort` none, low–max | docs.fireworks.ai/api-reference/post-chatcompletions |
| together | `max_tokens`; per-million prices in the list | docs.together.ai/reference/chat-completions-1 |
| siliconflow | `enable_thinking`, `max_tokens` | docs.siliconflow.com/en/api-reference/chat-completions/chat-completions |
| deepinfra | `reasoning_effort` none/low/medium/high, `max_tokens`, list tags and prices | docs.deepinfra.com/chat/reasoning, /chat/overview |
| novita | `enable_thinking`, `separate_reasoning`, `max_tokens` required, list prices/features | novita.ai/docs/api-reference/model-apis-llm-create-chat-completion |
| cloudflare | account address; `models/search?format=openrouter` returns `data` rows whose `id` is the exact model ID the API takes (OpenRouter provider format) | Cloudflare API schema (models search), openrouter.ai/docs/guides/get-started/for-providers |
| inclusionai | Ant Ling API `https://api.ant-ling.com/v1` (Ant Group serves InclusionAI's Ling), `thinking` object on 3.x, `max_tokens`, `prompt_tokens_details.cached_tokens` | developer.ant-ling.com/en/docs/getting-started/quickstart, /en/docs/api-reference/chat/openai |
| tokenrouter | `https://api.tokenrouter.com/v1`, the `server_address` tokenrouter.com's console publishes (its public docs are switched off); a new-api gateway | tokenrouter.com backend `api/status`, read 2026-10-22 |
| cerebras, nvidia, huggingface, compatible | address and key only; no API-wide level set documented | inference-docs.cerebras.ai, docs.api.nvidia.com, huggingface.co/docs/inference-providers |

## Not verified

Live behaviour of every vendor; Cloudflare prices (OpenRouter's `cost_usd` units are not read, so they stay unknown); TokenRouter's parameters beyond its address (no public reference). A third-party guide's `api.ling.inclusionai.com` resolves to a parked-domain address and is never used.
