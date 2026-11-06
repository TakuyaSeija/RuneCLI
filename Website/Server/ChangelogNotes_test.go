package main

import (
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
	if want := englishNotes["0.11.7"][0]; !strings.Contains(english, want) {
		t.Fatalf("the changelog does not show 0.11.7's recorded change")
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
		if want := l.text.Notes["0.11.7"][0]; !strings.Contains(body, want) {
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