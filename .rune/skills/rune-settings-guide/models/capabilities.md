# Model context and output limits

Location: Settings → Models → select a model → Alt+C. These are persistent model settings, not reply-length choices for one session.

## Context window limit
Purpose: tell Rune the model's total context capacity in tokens. A positive whole number is required when set. Use a value actually supported by that provider/model; total context is different from maximum output.

Change: open Context window limit, enter the correct published value and Apply. Clear an override only where the editor offers it. Check the resulting displayed value.

Risk: too high can cause requests the provider rejects. Too low can cause earlier summarization or inaccurate fullness estimates. Entering a larger number does not expand the model itself.

TOML property: `context` in the configured model entry. There is no universal factory context value for every model; an unknown value should not be guessed.

## Max output tokens
Purpose: record the model's output capability. TOML property: `max_output` in that model entry. A set value must be positive. Open the row to edit; empty clears it where offered.

This is different from Generation → Max output, which chooses the desired output ceiling for a conversation. Reasoning may consume part of that allowance. An unknown ceiling does not mean infinite output.

Risk: a wrong ceiling can cause rejection or unwanted restrictions. Use provider information, not a similarly named model's limits.

## Modalities
Purpose: declare the input kinds this model accepts (`text`, `image`, `audio`, `video`, `file`). When the provider's catalog publishes input modalities, they fill the list automatically; when a gateway publishes nothing about its models, you can declare the supported inputs here (`text` by default). A declared choice is marked `set by you` (`modalities` in `manual_fields`) so catalog refreshes do not erase it. Declaring an input kind does not make an upstream model support it.

## Other model information
Reasoning options and prices have separate guides: [reasoning](reasoning.md), [pricing](pricing.md). Published tool support, supported controls and accepted media can differ between models/providers. Advanced model overrides should only state capabilities the provider actually supports; selecting them cannot add a missing upstream feature.

Catalog refresh can update published information while explicit user overrides are retained. Deleting a model forgets its custom choices; see [Models menu](../menus/models.md). Do not edit informational/catalog-maintained fields as if they were normal settings. See [advanced model properties](advanced.md) for their user-facing meanings.
