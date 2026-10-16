# Settings › Generation

The main controls affect the current session/model. Auxiliary settings lower on the page are global and affect every session using this settings home.

| Section | What it controls |
| --- | --- |
| REASONING | Thinking or Effort, depending on the selected model's supported choices. |
| SAMPLING | Temperature, Top P, Frequency penalty and Presence penalty. |
| OUTPUT | Max output and Stop sequences. |
| AUXILIARY MODELS | Models and limits used for sanitizer, classifier, compaction and session titles. |

[Sampling](../generation/sampling.md) and [reasoning](../models/reasoning.md) explain accepted values and effects. Unsupported controls may not appear. A control temporarily unavailable with reasoning on is not necessarily unsupported forever.

## Auxiliary settings
Read the relevant topic before changing a global auxiliary value:
- [Sanitizer](../auxiliary/sanitizer.md): extracts user instructions for classifier review.
- [Classifier](../auxiliary/classifier.md): reviews proposed actions against the user's policy.
- [Compact](../auxiliary/compact.md): summarizes older conversation history.
- [Title](../auxiliary/title.md): generates session titles.
- [Shared auxiliary setting](../auxiliary/budgets.md): reasoning/output allowance.

Scroll to reach all rows. Select a row, edit or choose a value, then confirm Apply. `Use default` returns to the documented default/inheritance. Tool lists can select multiple names; [] has a meaning explained in the relevant guide.

Classifier status explains whether review is active. `Reload classifier policy` adopts current policy for an idle session; merely changing the file does not reload an existing session.

Changing auxiliary models can change cost and which provider receives relevant data. Disabling review or reducing its limits can affect safety/coverage. Explain these consequences and obtain the user's decision first.
