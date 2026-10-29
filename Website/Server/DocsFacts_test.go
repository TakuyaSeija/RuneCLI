package main

import (
	"encoding/json"
	"fmt"
	"os"
	"regexp"
	"strings"
	"testing"
)

// The manual is checked against the application it documents, not against
// itself: each test below reads a list from the source and fails when the
// manual leaves an item out or states a value the program does not have.

func readSource(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

// "Reference for Rune X" is a claim that the manual was checked against X.
func TestDocumentedVersionIsTheWorkspaceVersion(t *testing.T) {
	cargo := readSource(t, "../../Cargo.toml")
	m := regexp.MustCompile(`(?s)\[workspace\.package\].*?\nversion = "([^"]+)"`).FindStringSubmatch(cargo)
	if m == nil {
		t.Fatal("workspace version not found")
	}
	if m[1] != documentedVersion {
		t.Fatalf("the manual says Rune %s but the workspace is %s: review the release's user-visible changes in the manual, regenerate the settings reference (Tools/build_settings_reference.py) and update documentedVersion", documentedVersion, m[1])
	}
}

// Every setting, with the factory value the application's own table gives it.
func TestSettingsReferenceCoversEveryFieldWithItsDefault(t *testing.T) {
	src := readSource(t, "../../Engine/src/settings/Configuration.rs")
	start := strings.Index(src, "pub const FIELDS")
	end := strings.Index(src[start:], "];")
	page := readSource(t, "../UI/Pages/Docs/Reference.html")
	if !strings.Contains(page, "Generated from Rune "+documentedVersion+" ") {
		t.Fatalf("settings reference was not generated from Rune %s", documentedVersion)
	}
	fields := regexp.MustCompile(`key\s*:\s*"([^"]+)"\s*,\s*default\s*:\s*"((?:\\.|[^"\\])*)"`).FindAllStringSubmatch(src[start:start+end], -1)
	if len(fields) < 100 {
		t.Fatal("field table moved; review the settings reference")
	}
	entry := regexp.MustCompile(`<div id="([^"]+)"><dt><code>[^<]+</code></dt><dd><p>[^<]*</p><p class="ref-facts"><span><b>Default</b> ([^<]*(?:<code>[^<]*</code>)?)</span>`)
	documented := map[string]string{}
	for _, m := range entry.FindAllStringSubmatch(page, -1) {
		documented[m[1]] = m[2]
	}
	for _, f := range fields {
		var value string
		if err := json.Unmarshal([]byte(`"`+f[2]+`"`), &value); err != nil {
			t.Fatal(err)
		}
		got, ok := documented[f[1]]
		if !ok {
			t.Errorf("%s is not in the settings reference", f[1])
			continue
		}
		want := "not set"
		if value != "null" {
			want = "<code>" + strings.ReplaceAll(templateEscape(value), " ", "") + "</code>"
			got = strings.ReplaceAll(got, " ", "")
		}
		if got != want {
			t.Errorf("%s: reference says %s, the application %s", f[1], got, want)
		}
	}
	if len(documented) != len(fields) {
		t.Errorf("reference lists %d settings, the application has %d", len(documented), len(fields))
	}
}

func templateEscape(s string) string {
	return strings.NewReplacer(`&`, "&amp;", `<`, "&lt;", `>`, "&gt;", `"`, "&quot;", `'`, "&#x27;").Replace(s)
}

// Every provider preset the application offers has its row.
func TestProvidersArticleNamesEveryPreset(t *testing.T) {
	src := readSource(t, "../../Engine/src/settings/providers.rs")
	start := strings.Index(src, "pub const PRESETS")
	end := strings.Index(src[start:], "\n];")
	names := regexp.MustCompile(`Preset \{(?:\s*//[^\n]*)*\s*name:\s*"([^"]+)"`).FindAllStringSubmatch(src[start:start+end], -1)
	article := readSource(t, "../UI/Pages/Docs/Providers.html")
	for _, n := range names {
		if !strings.Contains(article, "<tr><td><code>"+n[1]+"</code></td>") {
			t.Errorf("preset %s has no row in Model providers", n[1])
		}
	}
	if rows := strings.Count(article, "<tr><td><code>"); rows != len(names) || len(names) < 30 {
		t.Errorf("Model providers lists %d presets, the application has %d", rows, len(names))
	}
	if !strings.Contains(article, fmt.Sprintf("knows %d providers", len(names))) {
		t.Errorf("the preset count in the text is not %d", len(names))
	}
}

// Every web service the application can call has its row, with what it serves.
func TestWebArticleNamesEveryService(t *testing.T) {
	entries, err := os.ReadDir("../../Engine/src/web")
	if err != nil {
		t.Fatal(err)
	}
	article := readSource(t, "../UI/Pages/Docs/Web.html")
	count := 0
	for _, e := range entries {
		src := readSource(t, "../../Engine/src/web/"+e.Name())
		at := strings.Index(src, "ADAPTER: Adapter = Adapter {")
		if at < 0 {
			continue
		}
		body := src[at : at+strings.Index(src[at:], "\n};")]
		title := regexp.MustCompile(`title:\s*"([^"]+)"`).FindStringSubmatch(body)[1]
		search, fetch := "No", "No"
		if strings.Contains(body, "search: Some(") {
			search = "Yes"
		}
		if strings.Contains(body, "fetch: Some(") {
			fetch = "Yes"
		}
		count++
		if !strings.Contains(article, "<tr><td>"+title+"</td><td>"+search+"</td><td>"+fetch+"</td>") {
			t.Errorf("web service %s (search %s, fetch %s) has no matching row", title, search, fetch)
		}
	}
	if rows := strings.Count(article, "</td><td>Yes</td><td>"); rows != count || count < 15 {
		t.Errorf("Web lists %d services, the application has %d", rows, count)
	}
}

// Every command the binary's own help screen offers is in the command-line reference.
func TestCommandLineReferenceCoversTheHelpScreen(t *testing.T) {
	src := readSource(t, "../../Cli/src/args.rs")
	start := strings.Index(src, "pub const USAGE: &str")
	usage := src[start : start+strings.Index(src[start:], "\";")]
	article := readSource(t, "../UI/Pages/Docs/CLI reference.html")
	seen := 0
	for _, m := range regexp.MustCompile(`(?m)^\s+rune (-[a-z-]+)(?: ([a-z-]+))?`).FindAllStringSubmatch(usage, -1) {
		seen++
		if !strings.Contains(article, m[1]) {
			t.Errorf("rune %s is missing from the command-line reference", m[1])
		}
		if sub := m[2]; sub != "" && !strings.HasPrefix(sub, "-") && !strings.Contains(article, sub) {
			t.Errorf("rune %s %s is missing from the command-line reference", m[1], sub)
		}
	}
	for _, flag := range regexp.MustCompile(`(?m)^\s+(-[a-z-]+|--[a-z-]+)\s`).FindAllStringSubmatch(usage, -1) {
		if !strings.Contains(article, flag[1]) {
			t.Errorf("global flag %s is missing from the command-line reference", flag[1])
		}
	}
	if seen < 25 {
		t.Fatalf("read only %d commands from the help screen", seen)
	}
}
