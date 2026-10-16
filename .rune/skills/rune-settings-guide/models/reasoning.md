# Reasoning and effort

## Two places, two purposes
- Settings → Models → selected model → Alt+C → Reasoning: configure that model's starting behavior and offered choices.
- Settings → Generation → Thinking/Effort: change reasoning for this session. `/thinking` and Alt+T also control the current choice.

A Thinking badge in the model list means reasoning is supported, not that it is currently on. Saved model/session choices can affect the next time you select a model.

## Choices
- Off: request no reasoning where the route can disable it. Some routes still perform least reasoning or only stop reasoning between tool calls; read the explanation beside the control.
- On: let a supported model reason. Some models require reasoning and do not offer Off.
- Effort level: choose from the levels actually offered for this model/provider. Greater effort may improve difficult work, but can increase latency and token cost. It is not a guarantee of a better answer.
- Route default, when offered: leave the amount to the selected route. This is not the same as Off or a particular level, and does not guarantee that reasoning occurs.

9router offers its supported level choices, subject to route restrictions and the user's model configuration. Do not assume every route treats each level differently. A word such as high in the model's ID is not necessarily the same as Rune's current effort selection.

## Change and reset
On Generation, select the reasoning row and choose an available value; Left/Right can save directly. For persistent model options use Alt+C. If available choices omit Off, reasoning is mandatory for that configured model.

Advanced model properties include `reasoning_enabled` (starting switch), `reasoning_options` (allowed choices), `thinking_required` and `default_effort`. Prefer the model editor to changing these by hand. For a current session, clearing an optional effort value, choosing Off and choosing Route default have different meanings; choose the explicit behavior you want.

## Effects and risks
Reasoning can consume output allowance before the final answer. A very small Max output may leave too little room for an answer. Some sampling controls may become unavailable while reasoning is on.

No displayed thinking text does not prove thinking was disabled or broken: some models/routes do not return it, and some tasks need little reasoning. Diagnose the selected model and displayed setting before changing providers or increasing budgets.
