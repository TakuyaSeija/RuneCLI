# Shared auxiliary allowance

Auxiliary models perform supporting jobs: title generation, classifier review, instruction extraction and history compaction. Each has its own guide and settings; changing one role does not automatically change all roles.

## Choosing auxiliary models
An omitted/reset model inherits the active model; an explicit choice uses an already configured provider/model. Choosing a different provider may send relevant user instructions, policy or history there. Explain the role's data use and ask before changing that destination.

Separate references: [sanitizer](sanitizer.md), [classifier](classifier.md), [compact](compact.md), [title](title.md).
