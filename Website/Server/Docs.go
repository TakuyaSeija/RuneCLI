package main

import (
	"fmt"
	"html"
	"regexp"
	"strings"
)

// Documentation is ordinary server-rendered pages. Navigation, pagination and
// the sitemap share this catalogue; JavaScript never decides what can be read.
const documentedVersion = "0.11.8"

type docGroup struct {
	Title string
	Pages []*page
}

type docHeading struct {
	ID, Title string
}

var docGroups = []docGroup{
	{Title: "Get started", Pages: []*page{
		{Route: "docs", File: "Start", Label: "Introduction", Lead: "What Rune does, what it sends to a model, and how to start your first session."},
		{Route: "docs/install", File: "Install", Label: "Install & update", Lead: "System requirements, what the installers do, PATH, the data folder, updates and removal on Linux, macOS and Windows."},
		{Route: "docs/platforms", File: "Platforms", Label: "Platform support", Lead: "What works the same on Linux, macOS and Windows, and what each system does differently."},
		{Route: "docs/interface", File: "Interface", Label: "The interface", Lead: "Read the conversation, permission cards, plan, file changes and session status."},
		{Route: "docs/sessions", File: "Sessions", Label: "Sessions & context", Lead: "Start, resume and steer work; inspect context and compact older history."},
	}},
	{Title: "Use Rune", Pages: []*page{
		{Route: "docs/commands", File: "Commands", Label: "Slash commands", Lead: "The conversation commands, their arguments, and what each one changes."},
		{Route: "docs/cli", File: "CLI reference", Label: "Command-line reference", Lead: "Launch options and rune subcommands for configuration, accounts, sessions and maintenance."},
		{Route: "docs/keyboard", File: "Keyboard", Label: "Keyboard & mouse", Lead: "Canonical shortcuts for editing, navigation, questions and the conversation."},
		{Route: "docs/shell", File: "Shell", Label: "Manual commands", Lead: "Run a command yourself with a leading ! — what it may do, when it runs and what the AI sees."},
		{Route: "docs/instructions", File: "Instructions", Label: "Instructions & attachments", Lead: "Supply project guidance, attach files and keep durable project memory."},
		{Route: "docs/memory", File: "Memory", Label: "Memory", Lead: "What project memory holds, what actually reaches the model, its one byte budget, and what it may never decide."},
		{Route: "docs/undo", File: "Undo", Label: "Review, undo & redo", Lead: "Review actual file diffs and understand what a session restore can and cannot reverse."},
	}},
	{Title: "Configure", Pages: []*page{
		{Route: "docs/settings", File: "Settings", Label: "Settings overview", Lead: "Find the right Settings section and distinguish global choices from session controls."},
		{Route: "docs/configuration", File: "Configuration", Label: "Configuration file", Lead: "Inspect, change and reset rune.toml values without confusing choices with defaults."},
		{Route: "docs/reference", File: "Reference", Label: "Settings reference", Lead: "Every setting in rune.toml: its factory value, what it does, the values it accepts and when a change takes effect."},
		{Route: "docs/connections", File: "Connections", Label: "Connections", Lead: "Add model and web accounts, manage credentials, and verify the destination before connecting."},
		{Route: "docs/providers", File: "Providers", Label: "Model providers", Lead: "The 31 provider presets, their keys and addresses, local services, and notes on reasoning by provider."},
		{Route: "docs/models", File: "Models", Label: "Models & capabilities", Lead: "Select a provider/model pair and configure known limits, reasoning options and pricing."},
		{Route: "docs/generation", File: "Generation", Label: "Generation & reasoning", Lead: "Control the current model's reasoning, sampling and requested output."},
		{Route: "docs/auxiliary", File: "Auxiliary", Label: "Auxiliary models", Lead: "Configure sanitizer, classifier, compaction and title jobs, including their data and cost implications."},
		{Route: "docs/runtime", File: "Runtime", Label: "Runtime & retries", Lead: "Understand request deadlines, linear retry waits, credit cooldown and detached-session lifetime."},
		{Route: "docs/tools", File: "Tools", Label: "Tools & limits", Lead: "Set returned-output and work limits, and distinguish them from a display fold or permission."},
		{Route: "docs/terminal", File: "Terminal", Label: "Terminal & background work", Lead: "How commands move to background, receive input and report completion or loss."},
		{Route: "docs/web", File: "Web", Label: "Web search & fetch", Lead: "Set up separate search and page-reading providers without silently changing destinations."},
		{Route: "docs/appearance", File: "Appearance", Label: "Appearance", Lead: "Choose a theme, toggle the side panel and understand terminal-dependent rendering."},
	}},
	{Title: "Extend", Pages: []*page{
		{Route: "docs/skills", File: "Skills", Label: "Skills", Lead: "Discover, activate and manage reusable guides without treating them as authorization."},
		{Route: "docs/mcp", File: "MCP", Label: "MCP connections", Lead: "Review external tool servers, scopes and access before connecting them to Rune."},
	}},
	{Title: "Trust & understand", Pages: []*page{
		{Route: "docs/security", File: "Security", Label: "Security", Lead: "The trust boundary, permission modes, platform limitations and the limits of containment."},
		{Route: "docs/classifier", File: "Classifier", Label: "Classifier policy", Lead: "Write and reload CLASSIFIER.md policy and understand when AI review is actually active."},
		{Route: "docs/privacy", File: "Privacy", Label: "Privacy & data", Lead: "What stays locally, what leaves for a provider, and what to review before sharing diagnostics."},
		{Route: "docs/credentials", File: "Credentials", Label: "Credentials", Lead: "Use the credential-entry UI, distinguish missing from unavailable keys, and recover deliberately."},
		{Route: "docs/verification", File: "Verification", Label: "Release verification", Lead: "Verify the release signature and archive checksum, and understand the signing key's trust limits."},
		{Route: "docs/architecture", File: "Architecture", Label: "Architecture", Lead: "A public overview of the frontend, local engine, model providers, tool execution and session data."},
		{Route: "docs/troubleshooting", File: "Troubleshooting", Label: "Troubleshooting", Lead: "Diagnose the actual status before changing timeouts, providers or access."},
	}},
}

func documentationPages() []*page {
	var out []*page
	for _, group := range docGroups {
		for _, p := range group.Pages {
			p.Section = group.Title
			p.Title = p.Label + " | RuneAgent docs"
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
