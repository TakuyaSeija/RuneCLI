# Shared auxiliary allowance

Auxiliary models perform supporting jobs: title generation, classifier review, instruction extraction and history compaction. Each has its own guide and settings; changing one role does not automatically change all roles.

## auxiliary.mandatory_reasoning_output_tokens
Location: advanced rune.toml option; no separate current Settings UI row. Scope: global. Factory:4096. Accepted setting:1025–10000000 whole tokens.

Purpose: provide output allowance when a supporting model requires reasoning. The amount needed for the actual result and the model's own output limit still matter. It is not an amount universally added to every request.

Inspect with `rune -config explain auxiliary.mandatory_reasoning_output_tokens`. If the user wants a change, use `rune -config set auxiliary.mandatory_reasoning_output_tokens <count>`. Reset with `rune -config reset auxiliary.mandatory_reasoning_output_tokens`. The next applicable operation uses the new choice.

Effects and risks: insufficient allowance can leave too little room for a usable result; a larger allowance can permit additional time and cost. Raising it cannot expand the selected model's real capacity. Do not treat byte, character and token limits as interchangeable.

## Choosing auxiliary models
An omitted/reset model inherits the active model; an explicit choice uses an already configured provider/model. Choosing a different provider may send relevant user instructions, policy or history there. Explain the role's data use and ask before changing that destination.

Separate references: [sanitizer](sanitizer.md), [classifier](classifier.md), [compact](compact.md), [title](title.md).

## Default and input reference

These are factory values, not a claim about the current user's saved choices. Inspect with explain; use reset to remove a global override. See [value formats](../config/values.md).

| Global key | Factory value | Accepted value / note |
| --- | --- | --- |
| `auxiliary.mandatory_reasoning_output_tokens` | `4096` | Whole number 1025–10000000 |
