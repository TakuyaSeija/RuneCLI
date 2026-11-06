package main

import (
	"strings"
	"testing"
)

// A missing page carries the site's own 404 — its words, its actions and its
// footer — never the server's default, and the footer stays the last thing on
// the page (owner, 2026-11-03).
func TestMissingPageIsTheSitesOwn(t *testing.T) {
	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.ParentURL = "http://127.0.0.1:9"
	h := newTestServer(t, cfg).routes()

	w := get(t, h, "GET", "/nope")
	if w.Code != 404 {
		t.Fatalf("status %d, want 404", w.Code)
	}
	body := w.Body.String()
	for _, want := range []string{
		"This page does not exist.",
		`<a class="btn" href="/Studio/">RuneAgent home</a>`,
		`<a class="btn" href="/Studio/docs">Read the docs</a>`,
		`<a href="http://127.0.0.1:9">Atoryn</a>`,
		`<footer class="foot">`,
	} {
		if !strings.Contains(body, want) {
			t.Errorf("the 404 page is missing %q", want)
		}
	}
	if strings.Contains(body, "Not found\n") {
		t.Error("the plain fallback was rendered instead of the page")
	}
}

// The footer's "Atoryn" leads to "/" on one origin, and to the address a host
// names where the site runs on its own port. A value that is neither a path nor
// an absolute http(s) address is refused, never published.
func TestParentAddress(t *testing.T) {
	f := newDownloads(t)
	oneOrigin := testConfig(f.dir)
	if oneOrigin.parent() != "/" {
		t.Errorf("default parent = %q, want /", oneOrigin.parent())
	}
	ownPort := testConfig(f.dir)
	ownPort.ParentURL = "http://192.168.18.10:3050"
	if err := ownPort.validate(); err != nil {
		t.Fatalf("a dev address was refused: %v", err)
	}
	if ownPort.parent() != "http://192.168.18.10:3050" {
		t.Errorf("parent = %q", ownPort.parent())
	}
	pathOnly := testConfig(f.dir)
	pathOnly.ParentURL = "/id/"
	if err := pathOnly.validate(); err != nil {
		t.Fatalf("a path was refused: %v", err)
	}
	for _, bad := range []string{"atoryn.com", "ftp://atoryn.com", "https://atoryn.com/?a=1", "javascript:alert(1)"} {
		cfg := testConfig(f.dir)
		cfg.ParentURL = bad
		if err := cfg.validate(); err == nil {
			t.Errorf("parentUrl %q was accepted", bad)
		}
	}
}
