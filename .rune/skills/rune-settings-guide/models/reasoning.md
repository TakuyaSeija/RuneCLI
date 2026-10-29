# Reasoning and effort

## Two places, two purposes
- Settings → Models → selected model → Alt+C → Reasoning: configure that model's starting behavior and offered choices.
- Settings → Generation → Thinking/Effort: change reasoning for this session. `/thinking` and Alt+T also control the current choice.

A Thinking badge in the model list means reasoning is supported, not that it is currently on. Saved model/session choices can affect the next time you select a model.

## Choices
- Off: request disabled reasoning; it is not a guarantee of zero thinking. On 9router, a route reporting that thinking cannot be disabled receives a Low request instead, and Minimal is not offered in Alt+C or Generation. A route reporting that it can disable thinking still receives a disable request, but the router/provider may change it; some routes only stop between tool calls. Read the explanation beside the control.
- On: let a supported model reason. Some models require reasoning and do not offer Off.
- Effort level: choose from the levels actually offered for this model/provider. Greater effort may improve difficult work, but can increase latency and token cost. It is not a guarantee of a better answer.
- Route default, when offered: leave the amount to the selected route. This is not the same as Off or a particular level, and does not guarantee that reasoning occurs.

9router offers its supported level choices, subject to route restrictions and the user's model configuration. Minimal is excluded when the route reports that thinking cannot be disabled, including when an older saved option still contains it; saved model overrides are not erased. An unavailable saved session level returns to an available configured default or Route default. This restriction does not remove Minimal from other routes or other providers. Do not assume every route treats each level differently. A word such as high in the model's ID is not necessarily the same as Rune's current effort selection. Omitting the thinking request is not a way to enforce Off: the route may apply a positive default effort.

## Change and reset
On Generation, select the reasoning row and choose an available value; Left/Right can save directly. For persistent model options use Alt+C. If available choices omit Off, reasoning is mandatory for that configured model.

Advanced model properties include `reasoning_enabled` (starting switch), `reasoning_options` (allowed choices), `thinking_required` and `default_effort`. Prefer the model editor to changing these by hand. For a current session, clearing an optional effort value, choosing Off and choosing Route default have different meanings; choose the explicit behavior you want.

## Effects and risks
Reasoning can consume output allowance before the final answer. A very small Max output may leave too little room for an answer. Some sampling controls may become unavailable while reasoning is on.

In the conversation, thinking is a `Thinking` / `Thought` line. When the model sends its thinking as text, the line counts that text's words as it arrives and can be opened to read it. Claude Code's models (`cc/…` on 9router) send no thinking text: their line shows the tokens Anthropic reports while the model thinks: Anthropic reports about once a second, and the number on screen climbs to each report one number after another, never past it, as in Claude Code; once the model moves on to its answer or a tool, the number reaches what was reported within a moment and settles on the number Anthropic billed for the whole thinking. Rune never estimates a count; a model that sends neither text nor a count shows no line.

No displayed thinking text does not prove thinking was disabled or broken: some models/routes do not return it, and some tasks need little reasoning. Diagnose the selected model and displayed setting before changing providers or increasing budgets.
