package main

import (
	"fmt"
	"html"
	"regexp"
	"strings"
)

// Documentation is ordinary server-rendered pages. Navigation, pagination and
// the sitemap share this catalogue; JavaScript never decides what can be read.
const documentedVersion = "0.12.3"

type docGroup struct {
	Title string
	Pages []*page
}

type docHeading struct {
	ID, Title string
}

var docGroups = []docGroup{
	{Title: "Get started", Pages: []*page{
		{Route: "docs", File: "Start", Label: "Introduction", Title: "Introduction to RuneAgent: AI Coding in the Terminal", Lead: "How RuneAgent plans, inspects files, and executes terminal commands with your connected model. Understand prompt flow, execution boundaries, and sessions."},
		{Route: "docs/install", File: "Install", Label: "Install & update", Title: "Installing and Updating RuneAgent on Linux, macOS and Windows", Lead: "Platform requirements, single-command shell and PowerShell installers, PATH configuration, data directory isolation, verified upgrades, and uninstall steps."},
		{Route: "docs/platforms", File: "Platforms", Label: "Platform support", Title: "Platform Support: Linux, macOS and Windows in RuneAgent", Lead: "Cross-platform runtime parity across Linux glibc and musl, macOS Apple Silicon and Intel, and Windows: shell differences, PTY, and native execution."},
		{Route: "docs/interface", File: "Interface", Label: "The interface", Title: "RuneAgent Terminal Interface: Transcript, Plan and Status", Lead: "Navigate the unified transcript, visual folds, command preview boxes, side panel accounting, permission cards, user question forms, and status bar indicators."},
		{Route: "docs/sessions", File: "Sessions", Label: "Sessions & context", Title: "Sessions and Context Management in RuneAgent", Lead: "Start, resume, and switch project sessions; inspect live token usage; configure automatic context compaction; and preserve turn history in local storage."},
	}},
	{Title: "Use Rune", Pages: []*page{
		{Route: "docs/commands", File: "Commands", Label: "Slash commands", Title: "Slash Commands Reference for RuneAgent", Lead: "Interactive conversation commands: /mode for permission switching, /undo for file restoration, /mcp for tool discovery, and /panel for sidebar visibility."},
		{Route: "docs/cli", File: "CLI reference", Label: "Command-line reference", Title: "Rune CLI Reference: Command-Line Flags and Options", Lead: "Command-line flags and subcommands for headless prompts (-p), settings check (-config), credential management (-secrets), MCP configuration, and storage pruning."},
		{Route: "docs/keyboard", File: "Keyboard", Label: "Keyboard & mouse", Title: "Keyboard and Mouse Shortcuts in RuneAgent", Lead: "Terminal keybindings for navigation, editing, Ctrl+O panel toggling, Ctrl+T plan access, double Esc cancellation, and cross-terminal mouse scroll behavior."},
		{Route: "docs/shell", File: "Shell", Label: "Manual commands", Title: "Manual Commands: Running Shell Scripts in RuneAgent", Lead: "Execute manual shell scripts directly with leading ! prefix: one-time full sandbox access, interactive confirmation preview, and environment variable scoping."},
		{Route: "docs/instructions", File: "Instructions", Label: "Instructions & attachments", Title: "Project Guidance, Attachments and Memory in RuneAgent", Lead: "Supply project rules via AGENTS.md, attach workspace files and images, defuse untrusted prompt injections, and maintain persistent project memory notes."},
		{Route: "docs/memory", File: "Memory", Label: "Memory", Title: "Project Memory Architecture and Persistent Storage in RuneAgent", Lead: "Structured project experience in MEMORY.md: byte budget constraints, immutable history indexing, and guidelines for recording durable architectural decisions."},
		{Route: "docs/undo", File: "Undo", Label: "Review, undo & redo", Title: "Reviewing Diffs, Undo and Redo in RuneAgent", Lead: "Inspect exact unified file diffs, track per-session mutations, restore modified files to pre-session state with /undo, and understand rollback limitations."},
	}},
	{Title: "Configure", Pages: []*page{
		{Route: "docs/settings", File: "Settings", Label: "Settings overview", Title: "RuneAgent Settings: Global Preferences and Session Controls", Lead: "Navigate the settings sections: distinguish global configuration in rune.toml from per-session model overrides, and configure defaults without guessing."},
		{Route: "docs/configuration", File: "Configuration", Label: "Configuration file", Title: "Configuring rune.toml: Settings and Defaults in RuneAgent", Lead: "Manage configuration in rune.toml: understand factory defaults, hot-reloading architecture, explicit key reset rules, and environment variable precedence."},
		{Route: "docs/reference", File: "Reference", Label: "Settings reference", Title: "RuneAgent Settings Reference: Complete Options in rune.toml", Lead: "Exhaustive reference for every rune.toml setting: factory values, valid types, hot-reload behavior, and execution impact across runtime, tools, and UI."},
		{Route: "docs/connections", File: "Connections", Label: "Connections", Title: "Model Connections and API Credentials in RuneAgent", Lead: "Add and manage model provider accounts, enter API credentials into encrypted storage, verify live endpoint reachability, and inspect network endpoints."},
		{Route: "docs/providers", File: "Providers", Label: "Model providers", Title: "Model Providers: 31 Supported Presets in RuneAgent", Lead: "Explore 31 preconfigured provider presets including Anthropic, OpenAI, DeepSeek, Ollama, and Z.ai: endpoint URLs, thinking controls, and credential scopes."},
		{Route: "docs/models", File: "Models", Label: "Models & capabilities", Title: "Configuring Models, Reasoning and Limits in RuneAgent", Lead: "Select active model pairs, configure reasoning effort levels, adjust context ceilings, and specify custom pricing rules for accurate spend tracking."},
		{Route: "docs/generation", File: "Generation", Label: "Generation & reasoning", Title: "Controlling Model Reasoning, Sampling and Output in RuneAgent", Lead: "Fine-tune reasoning effort, sampling temperature, max output token limits, and thinking token visibility across OpenAI, Anthropic, and gateway providers."},
		{Route: "docs/auxiliary", File: "Auxiliary", Label: "Auxiliary models", Title: "Auxiliary Models for Compaction, Classifier and Titles", Lead: "Configure dedicated lightweight models for background transcript compaction, safety policy evaluation via classifier, and automatic session titling."},
		{Route: "docs/runtime", File: "Runtime", Label: "Runtime & retries", Title: "Runtime Deadlines, Retries and Session Lifetime in RuneAgent", Lead: "Configure HTTP request timeouts, linear retry curves with rate limit cooldown, background process idle lifetime, and graceful disconnect recovery."},
		{Route: "docs/tools", File: "Tools", Label: "Tools & limits", Title: "Tool Execution Limits and Output Budgets in RuneAgent", Lead: "Configure stdout and stderr capture ceilings, character truncation thresholds, max tool actions per turn, and per-tool permission timeout rules."},
		{Route: "docs/terminal", File: "Terminal", Label: "Terminal & background work", Title: "Terminal Execution and Background Tasks in RuneAgent", Lead: "Run shell commands via terminal_use, manage background processes with process_control, track exit codes, and receive self-contained completion alerts."},
		{Route: "docs/web", File: "Web", Label: "Web search & fetch", Title: "Web Search and Fetch Configuration in RuneAgent", Lead: "Configure separate web search and page fetch providers, manage gateway routing pins, set output character limits, and inspect retrieved HTML markdown."},
		{Route: "docs/appearance", File: "Appearance", Label: "Appearance", Title: "Themes, Layout and Terminal Rendering in RuneAgent", Lead: "Select obsidian, amber, and neutral color themes, configure synchronized frame pacing, customize scroll sensitivity, and adjust responsive layout modes."},
	}},
	{Title: "Extend", Pages: []*page{
		{Route: "docs/skills", File: "Skills", Label: "Skills", Title: "Discovering, Activating and Managing Skills in RuneAgent", Lead: "Install, inspect, and execute reusable skill guides under .rune/skills/: discover capabilities via tools, manage skill scopes, and enforce permissions."},
		{Route: "docs/mcp", File: "MCP", Label: "MCP connections", Title: "Model Context Protocol (MCP): Connecting Tool Servers to RuneAgent", Lead: "Connect external MCP servers over stdio or SSE: discover declared tool interfaces, manage server authentication, and enforce runtime permission gates."},
	}},
	{Title: "Trust & understand", Pages: []*page{
		{Route: "docs/security", File: "Security", Label: "Security", Title: "Security Architecture, Sandboxing and Trust Boundary in RuneAgent", Lead: "Core security guarantees: Landlock kernel isolation, strict workspace containment, unalterable tool execution gates, and secret token redaction in logs."},
		{Route: "docs/classifier", File: "Classifier", Label: "Classifier policy", Title: "Classifier Policy: Writing CLASSIFIER.md in RuneAgent", Lead: "Define automated security boundaries using CLASSIFIER.md: enforce rule evaluation before command execution and inspect policy verification receipts."},
		{Route: "docs/privacy", File: "Privacy", Label: "Privacy & data", Title: "Privacy and Data Retention: What Leaves Your Machine in RuneAgent", Lead: "Local-first privacy design: verify what stays strictly on your computer, what reaches third-party API providers, and how telemetry is completely excluded."},
		{Route: "docs/credentials", File: "Credentials", Label: "Credentials", Title: "Managing API Keys and Encrypted Credentials in RuneAgent", Lead: "Secure local secret storage: primary encryption key, masked terminal entry fields, automated key validation, and safe credential recovery procedures."},
		{Route: "docs/verification", File: "Verification", Label: "Release verification", Title: "Verifying Release Signatures and Checksums in RuneAgent", Lead: "Verify cryptographic integrity: check ECDSA release signatures, validate SHA archive checksums, and audit official Atoryn signing keys."},
		{Route: "docs/architecture", File: "Architecture", Label: "Architecture", Title: "RuneAgent Architecture: Local Engine, Jail and Provider Layer", Lead: "Technical architecture of the Rust engine: single-thread UI loop, async tool execution jail, multi-provider dialect translation, and local registries."},
		{Route: "docs/troubleshooting", File: "Troubleshooting", Label: "Troubleshooting", Title: "Troubleshooting Connections, Models and Permissions in RuneAgent", Lead: "Diagnose network timeouts, authentication rejections, terminal rendering issues, permission denials, and broken background tasks using /logs inspection."},
	}},
}

func documentationPages() []*page {
	var out []*page
	for _, group := range docGroups {
		for _, p := range group.Pages {
			p.Section = group.Title
			if p.Title == "" {
				p.Title = p.Label + ": RuneAgent Documentation"
			}
			p.File = "Docs/" + p.File
			out = append(out, p)
		}
	}
	return out
}

// Article headings have an intentionally small authoring contract: literal,
// plain-text h2s with stable IDs. Fail at startup rather than ship a dead TOC.
var docH2 = regexp.MustCompile(`<h2 id="([a-z][a-z0-9-]*)">([^<]+)</h2>`)

func documentationHeadings(source []byte) ([]docHeading, error) {
	text := string(source)
	matches := docH2.FindAllStringSubmatch(text, -1)
	if len(matches) < 2 || len(matches) != strings.Count(text, "<h2") {
		return nil, fmt.Errorf("article needs at least two plain-text h2 headings with stable IDs")
	}
	seen := map[string]bool{}
	var out []docHeading
	for _, m := range matches {
		if seen[m[1]] {
			return nil, fmt.Errorf("duplicate heading ID %q", m[1])
		}
		seen[m[1]] = true
		out = append(out, docHeading{ID: m[1], Title: html.UnescapeString(m[2])})
	}
	return out, nil
}
