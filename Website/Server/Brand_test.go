package main

import (
	"bytes"
	"image"
	"image/png"
	"io/fs"
	"net/http"
	"regexp"
	"strings"
	"testing"

	ui "atoryn.com/studio/web/UI"
)

// The owner's mark (2026-10-31) replaced the drawn Raidō glyph: the header and
// the footer carry the logo, and the browser tab gets the same mark on its dark
// plate at each size a browser asks for. A page that fell back to a vector
// stand-in or to a single 96 px icon would look wrong in a tab, so the markup
// is held to the exact set.
func TestEveryPageCarriesTheMarkAtEachTabSize(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, route := range []string{"/Studio/", "/Studio/docs", "/Studio/changelog", "/Studio/license", "/Studio/missing"} {
		body := get(t, h, "GET", route).Body.String()
		if n := strings.Count(body, `class="brand-mark"`); n != 2 {
			t.Errorf("%s: %d brand marks, want the header and the footer", route, n)
		}
		if strings.Contains(body, "brand-rune") || strings.Contains(body, `viewBox="0 0 16 24"`) {
			t.Errorf("%s: the retired glyph is still drawn", route)
		}
		for _, size := range []string{"96x96", "32x32", "16x16"} {
			if !strings.Contains(body, `sizes="`+size+`"`) {
				t.Errorf("%s: no icon for %s", route, size)
			}
		}
		if !strings.Contains(body, `rel="apple-touch-icon"`) {
			t.Errorf("%s: no touch icon", route)
		}
	}
}

func TestTheMarksHaveTheShapeTheMarkupPromises(t *testing.T) {
	cases := []struct {
		name       string
		width      int
		height     int
		transluent bool
	}{
		{"Logo.png", 256, 256, true},
		{"Icon.png", 96, 96, true},
		{"Icon-32.png", 32, 32, true},
		{"Icon-16.png", 16, 16, true},
		{"AppleTouchIcon.png", 180, 180, false},
	}
	for _, c := range cases {
		raw, err := fs.ReadFile(ui.Files, "Static/Images/"+c.name)
		if err != nil {
			t.Fatalf("%s: %v", c.name, err)
		}
		img, err := png.Decode(bytes.NewReader(raw))
		if err != nil {
			t.Fatalf("%s is not a PNG: %v", c.name, err)
		}
		b := img.Bounds()
		if b.Dx() != c.width || b.Dy() != c.height {
			t.Errorf("%s is %dx%d, the markup promises %dx%d", c.name, b.Dx(), b.Dy(), c.width, c.height)
		}
		if hasAlpha(img) != c.transluent {
			t.Errorf("%s: alpha %v, want %v (the mark itself is transparent; a tab or touch icon carries its own plate)", c.name, hasAlpha(img), c.transluent)
		}
		if len(raw) > 64<<10 {
			t.Errorf("%s is %d bytes; a brand mark this small should stay under 64 KiB", c.name, len(raw))
		}
	}
	logo, _ := fs.ReadFile(ui.Files, "Static/Images/Logo.png")
	icon, _ := fs.ReadFile(ui.Files, "Static/Images/Icon.png")
	if bytes.Equal(logo, icon) {
		t.Error("the tab icon and the header mark are the same file; the icon needs its dark plate")
	}
}

func hasAlpha(img image.Image) bool {
	b := img.Bounds()
	for y := b.Min.Y; y < b.Max.Y; y++ {
		for x := b.Min.X; x < b.Max.X; x++ {
			if _, _, _, a := img.At(x, y).RGBA(); a < 0xffff {
				return true
			}
		}
	}
	return false
}

// Every icon URL the markup names is served, immutable, as an image.
func TestEveryIconIsServedAsAnImage(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	body := get(t, h, "GET", "/Studio/").Body.String()
	links := regexp.MustCompile(`(?:rel="icon"[^>]*href|rel="apple-touch-icon" href)="(/Studio/static/[^"]+)"`).FindAllStringSubmatch(body, -1)
	if len(links) != 4 {
		t.Fatalf("%d icon links, want three tab sizes and the touch icon", len(links))
	}
	for _, m := range links {
		w := get(t, h, "GET", m[1])
		if w.Code != http.StatusOK || w.Header().Get("Content-Type") != "image/png" || len(w.Body.Bytes()) == 0 {
			t.Errorf("%s: %d %q, want a served PNG", m[1], w.Code, w.Header().Get("Content-Type"))
		}
	}
}
