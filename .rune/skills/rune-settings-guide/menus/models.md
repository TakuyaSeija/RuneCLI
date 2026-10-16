# Settings › Models

## Select a model
Choose a configured provider on the left, or All providers. Search filters model IDs. Browsing a provider does not change the active conversation. Select a result and press Enter to use that provider/model for this session.

Ctrl+F returns to search. Arrows move between panes/results; Home/End and PageUp/PageDown navigate the focused list. The active model is shown separately. A Thinking badge describes reasoning support, not whether this session currently has reasoning on.

## Other actions
| Action | Use and consequence |
| --- | --- |
| Alt+C on a model | Open persistent model configuration: context/output capability, reasoning options and pricing. |
| T on a result | Send a real model test. It can cost money; ask before testing. |
| Enter model ID… | Add/select an exact custom model ID. This alone does not prove it works. Unknown limits stay unknown. |
| Refresh catalog | Update available models and published information. Does not switch the active session. Makes a network request. |
| New-session fallback | Set this provider as the starting fallback for new sessions. Existing choices stay unchanged. |
| Delete on a result | Hide/remove the model from Rune's list and forget its custom model settings. Does not delete an upstream model. Ask before removing. |

Re-adding a removed model uses available catalog information again; do not promise its previous custom choices will return.

## Model configuration
[Capabilities](../models/capabilities.md), [Reasoning](../models/reasoning.md) and [Pricing](../models/pricing.md) explain separate settings. A model's maximum output capability is not the same as Generation's desired reply length. Changing a displayed capability cannot expand what the provider really accepts.

To change a current conversation's creativity or reasoning effort, use [Generation](generation.md). To add an account/key, use [Connections](connections.md).
