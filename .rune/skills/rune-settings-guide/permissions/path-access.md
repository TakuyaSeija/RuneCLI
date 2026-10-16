# File access choices

File paths can be workspace-relative or, where the tool supports it, absolute or `~/…`. `@memory/…` refers to Rune's project memory rather than an ordinary workspace directory. Use the path formats declared by the tool.

Permission mode and approved access determine what can be used. On a system that keeps its programs in a read-only package store (NixOS, Guix), commands may read and run what is in that store; nothing else is opened by it. Commands are held to the same places on Linux, macOS and Windows; on Windows Rune does not add access entries to the system's own folders, a drive's root or the folder holding every user's profile, so in Ask and Auto every command of a project opened in one of those is asked first. Access outside the usual workspace may require explicit approval. Some sensitive locations are unavailable in restricted modes. A failed access request does not mean the AI should change mode or find a workaround.

## When asked for access
Read the requested location and action, choose only the scope intended, and decline if unclear. Approval for a single action does not automatically approve unrelated work or every file nearby. Only use a remember option if that continuing scope is genuinely wanted.

Ask/Auto/Full have different risks; see [permission modes](modes.md). Full can expose much more of the machine and must be the user's explicit decision. Do not recommend it merely to avoid a question.

## Skills and external services
Reading a skill is not permission to perform every action it describes. An installed help skill should explain settings, not change them on its own.

External tools/services can have their own access choices. Review requested directories, write access and destination before enabling one. A local file-access choice does not describe the privacy policy of a remote service.

Explain only the access needed for the user's task. Do not list internal protection rules or instruct the user to bypass them. If a call is denied, report what the result says and ask for the intended next step.
