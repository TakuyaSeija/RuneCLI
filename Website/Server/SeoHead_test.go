package main

import (
	"regexp"
	"strings"
	"testing"
)

// What a search engine reads first (owner, 2026-11-10): a title it can show as
// written, a sitemap that pairs the ten languages of every page, and a footer
// that never becomes the snippet.

func prodHandler(t *testing.T) *server {
	t.Helper()
	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.Env, cfg.Origin, cfg.Indexing = "Prod", "https://atoryn.com", true
	cfg.Listen = []string{"127.0.0.1:3200"}
	return newTestServer(t, cfg)
}

func TestSitemapPairsEveryLanguageOfEveryPage(t *testing.T) {
	h := prodHandler(t).routes()
	body := get(t, h, "GET", "/Studio/sitemap.xml").Body.String()
	if !strings.Contains(body, `xmlns:xhtml="http://www.w3.org/1999/xhtml"`) {
		t.Fatal("the sitemap does not declare the xhtml namespace")
	}
	urls := regexp.MustCompile(`(?s)<url>.*?</url>`).FindAllString(body, -1)
	if want := len(locales) * len(pages); len(urls) != want {
		t.Fatalf("%d url entries, want %d (every language of every page)", len(urls), want)
	}
	link := regexp.MustCompile(`<xhtml:link rel="alternate" hreflang="([^"]+)" href="(https://[^"]+)"/>`)
	for _, u := range urls {
		got := link.FindAllStringSubmatch(u, -1)
		if len(got) != len(locales)+1 {
			t.Fatalf("an entry lists %d alternates, want %d: %s", len(got), len(locales)+1, u)
		}
		loc := regexp.MustCompile(`<loc>([^<]+)</loc>`).FindStringSubmatch(u)[1]
		self := false
		for _, a := range got {
			if a[2] == loc {
				self = true
			}
		}
		if !self {
			t.Errorf("%s does not list itself among its alternates", loc)
		}
	}
}

func TestSitemapDatesOnlyWhatAReleaseChanges(t *testing.T) {
	h := prodHandler(t).routes()
	body := get(t, h, "GET", "/Studio/sitemap.xml").Body.String()
	// No release is published in the fixture, so no page may carry a date:
	// a guessed lastmod is worse than none.
	if strings.Contains(body, "<lastmod>") {
		t.Errorf("a lastmod was written with no release to take it from")
	}
}

func TestFooterIsKeptOutOfSnippets(t *testing.T) {
	// The footer is right as the page shows it (owner, 2026-11-10): the brand
	// and the year stay. What must not happen is that a search snippet quotes
	// that copyright line, so the row carries data-nosnippet.
	h := prodHandler(t).routes()
	for _, l := range locales {
		body := get(t, h, "GET", "/Studio"+l.prefix()+"/").Body.String()
		if !strings.Contains(body, `<div class="foot-row" data-nosnippet>`) {
			t.Errorf("%s: the footer is not marked data-nosnippet", l.Code)
		}
		if !regexp.MustCompile(`<span class="foot-note">&copy; \d{4} Atoryn</span>`).MatchString(body) {
			t.Errorf("%s: the footer note is not the year-stamped copyright", l.Code)
		}
	}
}
