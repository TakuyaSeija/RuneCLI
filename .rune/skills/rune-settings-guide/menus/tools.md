# Settings › Tools

Tools contains global limits for individual tools and for user questions/permission waits. Scroll to see all groups. Select a value, edit, Apply; Use default returns to its default where offered.

| Group | Purpose |
| --- | --- |
| TERMINAL | Foreground wait, returned output and background log limits. [Guide](../tools/terminal-use.md) |
| INSPECT FILES | Largest file to read and text to return. [Guide](../tools/inspect-file.md) |
| FIND FILES | Maximum returned paths and output length. [Guide](../tools/find-files.md) |
| SEARCH TEXT | Matching lines, line length and total output. [Guide](../tools/search-text.md) |
| FILE SCAN | How far file searches go and limits on expensive patterns.  [Guide](../tools/file-scan.md) |
| MANAGE PROCESS | Amount of background process output returned.  [Guide](../tools/manage-process.md) |
| WEB SEARCH / WEB FETCH | Separate providers, timeouts and output limits. Provider subpages own Add/Delete/Select. [Provider guide](../connections/web-providers.md) |
| DISCOVER CAPABILITIES | Length of skill/MCP discovery results.  [Guide](../tools/discover-capabilities.md) |
| READ MCP RESOURCE | Length of an MCP resource result.  [Guide](../tools/read-mcp-resource.md) |
| TRACK TASKS | Number of plan tasks and task-description length.  [Guide](../tools/track-tasks.md) |
| EXECUTION & ORCHESTRATION | Maximum tool calls in one model response.  [Guide](../tools/orchestration.md) |
| USER QUESTIONS | How long a question form can wait.  [Guide](../tools/ask-user-questions.md) |
| PERMISSIONS | How long a permission request can wait. [Modes](../permissions/modes.md) |
| PROJECT INSTRUCTIONS | Amount of AGENTS.md included as instructions.  [Guide](../data/instructions.md) |

Not every tool has a dedicated settings group. Do not invent a numeric control for a tool merely because the tool exists.

## Effects and risks
A returned-output limit affects how much the AI can read, not only the folded preview you see. Lower values can omit useful evidence. Higher values can increase context use and cost. File-scan work limits and returned-result limits are separate; increasing one does not automatically increase the other.

Tool limits do not grant file/network access. Permission mode and approved access are separate choices. Do not change those just to obtain more output.

Other tools: [create_file](../tools/create-file.md), [patch_file](../tools/patch-file.md), [terminal_send_input](../tools/terminal-send-input.md), [call_mcp_tool](../tools/call-mcp-tool.md). Web tools have separate [search](../tools/web-search.md) and [fetch](../tools/web-fetch.md) guides.
