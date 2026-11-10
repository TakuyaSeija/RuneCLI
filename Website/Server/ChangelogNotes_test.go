package main

import (
	"html"
	"strings"
	"testing"
)

// The changelog speaks the page's language, takes its words from the site (not
// from the published artifact), groups the history by minor version, and says
// honestly when a version's change was never recorded (owner, 2026-11-05).
// Because the words are the site's, a version published long ago can still gain
// its notes — a published version is never replaced.
func TestChangelogSpeaksThePageLanguageAndAdmitsAMissingRecord(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.11.7", fakeRune("0.11.7")) // the site has words for this one
	f.publish(t, "0.8.8", fakeRune("0.8.8"))   // nothing was recorded for this one
	h := newTestServer(t, testConfig(f.dir)).routes()

	english := get(t, h, "GET", "/Studio/changelog").Body.String()
	note := englishNotes["0.11.7"]
	if !strings.Contains(english, note.Title) {
		t.Fatalf("the changelog does not show 0.11.7's title")
	}
	for _, para := range note.Paragraphs {
		if !strings.Contains(english, para) {
			t.Fatalf("the changelog does not show 0.11.7's paragraph")
		}
	}
	if len(note.Points) == 0 || !strings.Contains(english, note.Points[0].Text) {
		t.Fatalf("the changelog does not show 0.11.7's detail lines")
	}
	if !strings.Contains(english, "was not recorded") {
		t.Fatalf("a version whose change was never recorded must say so")
	}
	if strings.Contains(english, "First release.") {
		t.Fatalf("the artifact's own notes must not decide what the page says")
	}
	if strings.Count(english, "history-group") < 2 {
		t.Fatalf("the release history is not grouped by minor version")
	}
	if !strings.Contains(english, "0.11") || !strings.Contains(english, "0.8") {
		t.Fatalf("the sidebar does not name the minor versions it holds")
	}

	for _, l := range locales[1:] {
		body := get(t, h, "GET", "/Studio/"+l.Code+"/changelog").Body.String()
		want := l.text.Notes["0.11.7"]
		// A page escapes its text, so a title with an apostrophe arrives as &#39;.
		if !strings.Contains(body, html.EscapeString(want.Title)) || !strings.Contains(body, html.EscapeString(want.Paragraphs[0])) {
			t.Errorf("%s shows the English words for 0.11.7", l.Code)
		}
		if strings.Contains(body, "was not recorded") {
			t.Errorf("%s kept the English line for a version with no recorded change", l.Code)
		}
	}
	if !strings.Contains(get(t, h, "GET", "/Studio/id/changelog").Body.String(), "tidak tercatat") {
		t.Errorf("Indonesian changelog does not admit a missing record in Indonesian")
	}
}

// A version's entry is layered: a title, the paragraph that says what changed
// and why, then short detail lines (owner, 2026-11-06). A bare list of
// sentences was rejected; this holds every published version to the shape.
func TestEveryChangelogEntryIsLayered(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.11.9", fakeRune("0.11.9"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	if len(englishNotes) == 0 {
		t.Fatal("no changelog notes were loaded")
	}
	for version, n := range englishNotes {
		if strings.TrimSpace(n.Title) == "" {
			t.Errorf("%s has no title", version)
		}
		if len(n.Paragraphs) == 0 {
			t.Errorf("%s has no paragraph saying what changed and why", version)
		}
		for i, p := range n.Paragraphs {
			if strings.TrimSpace(p) == "" {
				t.Errorf("%s paragraph %d is empty", version, i)
			}
		}
	}
	// The page must render every layer, in the reader's own language: a
	// title, the paragraph, and each recorded point as its own anchored
	// heading with its text beneath it (owner, 2026-11-09).
	for _, code := range []string{"", "id", "fr"} {
		path := "/Studio/changelog"
		if code != "" {
			path = "/Studio/" + code + "/changelog"
		}
		body := get(t, h, "GET", path).Body.String()
		for _, want := range []string{"release-note-title", "release-note-body", "release-points", "note-point", "release-point-body"} {
			if !strings.Contains(body, want) {
				t.Errorf("%s changelog does not render %s", path, want)
			}
		}
		n := englishNotes["0.11.9"]
		for _, p := range n.Points {
			if !strings.Contains(body, `data-anchor="pt-`+p.ID+`"`) {
				t.Errorf("%s changelog does not anchor point %q", path, p.ID)
			}
		}
	}
}

// The newest release reads chapter by chapter (owner, 2026-11-09): its note is
// divided into titled sections, the page's contents list every one of them, the
// sidebar names each version with a few words about its fix, and the record
// carries neither testing claims, website fixes, nor install folders that were
// never where the binary went.
func TestChangelogReadsInSectionsWithAnHonestSidebar(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.12.0", fakeRune("0.12.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	body := get(t, h, "GET", "/Studio/changelog").Body.String()

	note := englishNotes["0.12.0"]
	if len(note.Sections) < 3 {
		t.Fatalf("0.12.0's note holds %d sections", len(note.Sections))
	}
	for _, s := range note.Sections {
		// The page escapes its text, so a title with "&" arrives as "&amp;".
		if !strings.Contains(body, `data-anchor="sec-`+s.ID+`"`) || !strings.Contains(body, html.EscapeString(s.Title)) {
			t.Errorf("section %q is not rendered as a chapter", s.ID)
		}
	}
	if !strings.Contains(body, `id="sec-model-providers"`) {
		t.Errorf("the contents do not reach the rendered chapters")
	}
	// On this page lists the points directly, without chapter or documentation titles (owner, 2026-11-09).
	for _, s := range note.Sections {
		for _, p := range s.Points {
			if !strings.Contains(body, `class="toc-point"`) || !strings.Contains(body, `href="#pt-`+p.ID+`"`) {
				t.Errorf("the contents do not list point %q", p.ID)
			}
		}
	}
	if !strings.Contains(body, `class="history-sum"`) || !strings.Contains(body, html.EscapeString(note.Summary)) {
		t.Errorf("the sidebar does not name the version's fix in a few words")
	}
	for _, banned := range []string{"/usr/local/bin", "runners", "Responsive website"} {
		if strings.Contains(body, banned) {
			t.Errorf("the changelog still carries %q", banned)
		}
	}
}

// Each published version has its own distinct title and snippet when requested
// directly or via query, so search engines and tools index the version's own
// facts rather than a generic or lumped changelog label (owner, 2026-11-10).
func TestChangelogServesVersionSpecificTitleAndSnippet(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.12.0", fakeRune("0.12.0"))
	f.publish(t, "0.12.1", fakeRune("0.12.1"))
	h := newTestServer(t, testConfig(f.dir)).routes()

	v0120 := get(t, h, "GET", "/Studio/changelog/0.12.0").Body.String()
	note0120 := englishNotes["0.12.0"]
	if !strings.Contains(v0120, "<title>RuneAgent 0.12.0: "+html.EscapeString(note0120.Title)+"</title>") {
		t.Errorf("0.12.0 title mismatch")
	}
	if !strings.Contains(v0120, html.EscapeString(note0120.Paragraphs[0])) {
		t.Errorf("0.12.0 description does not carry its own paragraph")
	}

	v0121 := get(t, h, "GET", "/Studio/changelog/0.12.1").Body.String()
	note0121 := englishNotes["0.12.1"]
	if !strings.Contains(v0121, "<title>RuneAgent 0.12.1: "+html.EscapeString(note0121.Title)+"</title>") {
		t.Errorf("0.12.1 title mismatch")
	}
	if !strings.Contains(v0121, html.EscapeString(note0121.Paragraphs[0])) {
		t.Errorf("0.12.1 description does not carry its own paragraph")
	}

	// Default /Studio/changelog defaults to latest (0.12.1)
	latest := get(t, h, "GET", "/Studio/changelog").Body.String()
	if !strings.Contains(latest, "<title>RuneAgent 0.12.1: "+html.EscapeString(note0121.Title)+"</title>") {
		t.Errorf("default changelog does not title the latest version")
	}

	// Query parameter ?v=0.12.0
	query := get(t, h, "GET", "/Studio/changelog?v=0.12.0").Body.String()
	if !strings.Contains(query, "<title>RuneAgent 0.12.0: "+html.EscapeString(note0120.Title)+"</title>") {
		t.Errorf("query ?v=0.12.0 did not select 0.12.0")
	}
}
