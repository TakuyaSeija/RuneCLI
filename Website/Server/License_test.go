package main

import (
	"html"
	"os"
	"regexp"
	"strings"
	"testing"
)

// The license page says it is the LICENSE.txt shipped in every archive, so it
// must be that text word for word: a dropped or added sentence is a different
// agreement. The shipped file lives with the release tooling on the owner's
// machine, so a checkout without it skips.
func TestLicensePageIsTheShippedLicenseText(t *testing.T) {
	raw, err := os.ReadFile("../../Ops/Release/LICENSE.txt")
	if err != nil {
		t.Skip("shipped LICENSE.txt is not in this checkout")
	}
	normal := func(s string) string { return strings.Join(strings.Fields(html.UnescapeString(s)), " ") }

	var shipped []string
	heading := regexp.MustCompile(`^\d+\. [A-Z -]+$`)
	text := strings.ReplaceAll(string(raw), "-\n", "-") // a word broken across lines is one word
	for _, line := range strings.Split(text, "\n") {
		line = strings.TrimSpace(strings.TrimPrefix(strings.TrimSpace(line), "- "))
		if line == "" || line == "RUNEAGENT LICENSE" || heading.MatchString(line) {
			continue
		}
		shipped = append(shipped, line)
	}
	want := normal(strings.Join(shipped, " "))

	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.Origin = "https://atoryn.com"
	body := get(t, newTestServer(t, cfg).routes(), "GET", "/Studio/license").Body.String()
	start := strings.Index(body, `<div class="prose license-text">`)
	if start < 0 {
		t.Fatal("license text block missing")
	}
	block := body[start:]
	block = block[:strings.Index(block, `<p class="license-aside">`)]
	var page []string
	for _, m := range regexp.MustCompile(`(?s)<(?:p|li)[^>]*>(.*?)</(?:p|li)>`).FindAllStringSubmatch(block, -1) {
		page = append(page, regexp.MustCompile(`<[^>]+>`).ReplaceAllString(m[1], ""))
	}
	// The effective date and the per-release rule sit in the page head.
	head := regexp.MustCompile(`(?s)<p class="meta">(.*?)</p>`).FindStringSubmatch(body)
	if head == nil {
		t.Fatal("effective date missing")
	}
	got := normal(head[1] + " " + strings.Join(page, " "))
	if got != want {
		t.Fatalf("license page differs from LICENSE.txt\n page: %s\n file: %s", got, want)
	}
}
