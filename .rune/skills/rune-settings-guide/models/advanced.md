# Advanced model information

Location: a configured model's entry under providers.list.models in rune.toml; most are informational rather than extra UI rows. Prefer Models → Alt+C for supported edits, and Refresh catalog for published information. Editing a capability does not make an upstream model support it.

| Property | User-facing meaning |
| --- | --- |
| `id` | Exact model identifier. Do not change it to a guessed similar ID. |
| `context`, `input_limit`, `max_output` | Total context, separately published input limit, and output ceiling in tokens. Unknown is not zero. |
| `tools`, `chat` | Whether tool use/chat is supported where known. |
| `modalities` | Accepted inputs: text, image, audio, video, file. An empty/unknown list is not support for everything. |
| `thinking`, `thinking_required` | Reasoning support/requirement. Not a replacement for this session's Thinking control. |
| `reasoning_enabled`, `reasoning_options` | Starting reasoning switch and allowed choices configured for the model. |
| `efforts`, `default_effort`, `thinking_modes` | Supported effort choices, a starting effort where chosen, and available reasoning modes. Do not copy levels from an unrelated provider. |
| `supported_parameters` | Which generation controls the provider reports accepting. |
| `price` | Rate information; see [pricing](pricing.md). |
| `metadata` | Additional vendor-supplied information. There is no universal list of arbitrary keys to add here. |
| `known`, `manual_fields` | Maintained information distinguishing published data and explicit overrides. Not normal tuning controls; let Rune manage them. |
| `fetched_at`, `source_url`, `listed` | Informational freshness/source/listing status. Do not edit these to simulate a successful refresh. |

Change/reset: use the supported model editor for its controls; clear/default only where offered. For a deliberate advanced TOML edit, change only the intended property, use the provider's published requirements, then run `rune -config check`. Removing an optional override returns to available default/published behavior; it does not guarantee every previously fetched value reappears immediately. Nested properties are not all accepted by the generic reset command.

Risk: incorrect capability overrides can cause rejected requests, incomplete answers or unsupported attachments. Informational fields are not access grants. Preserve the user's explicit choices during refresh; obtain consent before real model tests or sending media.
