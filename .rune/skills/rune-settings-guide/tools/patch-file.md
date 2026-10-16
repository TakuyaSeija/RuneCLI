# Patch a file

patch_file replaces one exact piece of text in a file. The old text must occur **exactly once**, including whitespace/newlines. Empty replacement deletes that passage. A missing/ambiguous match fails rather than silently choosing one.

There is no dedicated numeric Settings row. Shared [checkpoint settings](../data/checkpoints.md) and [permission/access choices](../permissions/modes.md) apply. For an entire new file use [create_file](create-file.md), remembering it overwrites existing files.

Inspect current content first; use a unique, narrow old passage. If a match fails, read again instead of guessing or replacing the whole file. For rune.toml prefer supported setting editors/commands when available, preserving unrelated choices.

Risk: an exact replacement can still be the wrong semantic change. Explain/authorize the intended edit and verify the result. Undo is not a substitute for checking the file.
