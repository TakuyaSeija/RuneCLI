package main

import (
	"bytes"
	"compress/gzip"
	"io"
	"net/http"
	"strconv"
	"strings"
	"testing"
)

// The home page was once 2 MB of uncompressed HTML (owner, 2026-10-28): text
// travels gzipped to every client that asks, and unchanged to one that does not.
func TestTextIsServedGzippedOnlyWhenAccepted(t *testing.T) {
	f := newDownloads(t)
	st := newTestServer(t, testConfig(f.dir))
	h := st.routes()
	css, _ := st.site.asset("Styles/Site.css")
	font, _ := st.site.asset("Fonts/Inter.woff2")
	gzipOK := func(r *http.Request) { r.Header.Set("Accept-Encoding", "br, gzip;q=0.8") }
	for _, target := range []string{"/Studio/", "/Studio/docs", "/Studio/install.sh", css} {
		plain := get(t, h, "GET", target)
		packed := get(t, h, "GET", target, gzipOK)
		if plain.Header().Get("Content-Encoding") != "" || plain.Header().Get("Vary") != "Accept-Encoding" {
			t.Fatalf("%s: plain response %v", target, plain.Header())
		}
		if packed.Header().Get("Content-Encoding") != "gzip" || packed.Header().Get("Content-Length") != strconv.Itoa(packed.Body.Len()) {
			t.Fatalf("%s: not gzipped %v", target, packed.Header())
		}
		z, err := gzip.NewReader(bytes.NewReader(packed.Body.Bytes()))
		if err != nil {
			t.Fatal(err)
		}
		body, _ := io.ReadAll(z)
		if !bytes.Equal(body, plain.Body.Bytes()) || packed.Body.Len() >= plain.Body.Len() {
			t.Fatalf("%s: gzip body differs or is not smaller", target)
		}
		head := get(t, h, "HEAD", target, gzipOK)
		if head.Body.Len() != 0 || head.Header().Get("Content-Length") != packed.Header().Get("Content-Length") {
			t.Fatalf("%s: HEAD disagrees with GET", target)
		}
	}
	refused := get(t, h, "GET", "/Studio/", func(r *http.Request) { r.Header.Set("Accept-Encoding", "gzip;q=0, identity") })
	if refused.Header().Get("Content-Encoding") != "" || !strings.Contains(refused.Body.String(), "<!doctype html>") {
		t.Fatal("q=0 must refuse gzip")
	}
	woff := get(t, h, "GET", font, gzipOK)
	if woff.Header().Get("Content-Encoding") != "" || woff.Header().Get("Vary") != "" {
		t.Fatal("fonts are already compressed")
	}
	home := get(t, h, "GET", "/Studio/", gzipOK)
	if home.Body.Len() > 40<<10 {
		t.Fatalf("home travels as %d bytes", home.Body.Len())
	}
}
