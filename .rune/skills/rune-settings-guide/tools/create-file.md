# Create or overwrite a file

create_file writes the supplied complete text to one path. **An existing file is overwritten.** It is not append and not an edit of one matched passage. For a targeted replacement use [patch_file](patch-file.md).

There is no dedicated numeric Settings row. Shared file-change/undo limits are in [checkpoint settings](../data/checkpoints.md); [permission mode/access](../permissions/modes.md) still applies.

Before use, inspect an existing file and confirm replacement is intended. A request to explain configuration does not authorize replacing rune.toml or the user's policies. Keep unrelated user choices intact.

Risk: overwriting can remove content. Confirm completion from the tool result; a failed call is not a saved file. Undo can restore eligible session changes but is not a reason to overwrite carelessly. Never place actual API keys in generated guide/example files.
