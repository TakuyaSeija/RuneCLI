package main

import (
	"html"
	"io/fs"
	"regexp"
	"strings"
	"testing"

	ui "atoryn.com/studio/web/UI"
)

// The site in ten languages (owner, 2026-11-04): every translation serves the
// same pages as English, says only what the English says, and never makes Rune
// look as if it spoke a language it does not.

func TestEveryLanguageServesEveryPageAndItsOwnNotFound(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, l := range locales {
		for _, p := range pages {
			path := "/Studio" + l.prefix() + "/" + p.Route
			w := get(t, h, "GET", path)
			if w.Code != 200 {
				t.Fatalf("%s: %d", path, w.Code)
			}
			body := w.Body.String()
			if !strings.Contains(body, `<html lang="`+l.Code+`"`) {
				t.Errorf("%s is not marked %s", path, l.Code)
			}
		}
		w := get(t, h, "GET", "/Studio"+l.prefix()+"/no-such-page")
		if w.Code != 404 || !strings.Contains(w.Body.String(), `<html lang="`+l.Code+`"`) {
			t.Errorf("%s: a missing page is not %s's own 404 (%d)", l.Code, l.Code, w.Code)
		}
		if l == english {
			continue
		}
		for from, to := range map[string]string{
			"/Studio/" + l.Code:                   "/Studio/" + l.Code + "/",
			"/Studio/" + l.Code + "/install":      "/Studio/" + l.Code + "/#install",
			"/Studio/" + l.Code + "/security":     "/Studio/" + l.Code + "/docs/security",
			"/Studio/" + l.Code + "/docs/memory/": "/Studio/" + l.Code + "/docs/memory",
		} {
			w := get(t, h, "GET", from)
			if w.Code != 301 || w.Header().Get("Location") != to {
				t.Errorf("%s: %d → %q, want %q", from, w.Code, w.Header().Get("Location"), to)
			}
		}
	}
	// Folder names are exact: another spelling is a missing page, not a language.
	for _, path := range []string{"/Studio/ID/", "/Studio/zh-cn/", "/Studio/xx/docs"} {
		if w := get(t, h, "GET", path); w.Code != 404 {
			t.Errorf("%s: %d", path, w.Code)
		}
	}
}

// The browser tab names the page it shows, so no two pages of one language
// share a title, and the head carries no em dash (owner, 2026-11-04).
func TestEveryTabTitleIsItsOwnAndTheHeadHasNoEmDash(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	head := regexp.MustCompile(`<title>([^<]*)</title>|<meta (?:name="description"|property="og:(?:title|description)") content="([^"]*)"`)
	for _, l := range locales {
		seen := map[string]string{}
		paths := []string{"/Studio" + l.prefix() + "/no-such-page"}
		for _, p := range pages {
			paths = append(paths, "/Studio"+l.prefix()+"/"+p.Route)
		}
		for _, path := range paths {
			body := get(t, h, "GET", path).Body.String()
			title := ""
			for _, m := range head.FindAllStringSubmatch(body, -1) {
				text := html.UnescapeString(m[1] + m[2])
				if strings.Contains(text, "—") {
					t.Errorf("%s: the head carries an em dash: %q", path, text)
				}
				if m[1] != "" {
					title = text
				}
			}
			if title == "" {
				t.Fatalf("%s: no title", path)
			}
			if other, ok := seen[title]; ok {
				t.Errorf("%s and %s share the title %q", other, path, title)
			}
			seen[title] = path
		}
	}
}

func TestLanguageMenuAndAlternatesPointAtTheSamePage(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	item := regexp.MustCompile(`<a class="lang-item" href="([^"]+)" hreflang="([^"]+)" lang="[^"]+"( aria-current="true")? translate="no">`)
	alt := regexp.MustCompile(`<link rel="alternate" hreflang="([^"]+)" href="([^"]+)">`)
	for _, l := range locales {
		for _, route := range []string{"", "docs/memory", "license"} {
			body := get(t, h, "GET", "/Studio"+l.prefix()+"/"+route).Body.String()
			items := item.FindAllStringSubmatch(body, -1)
			alts := alt.FindAllStringSubmatch(body, -1)
			if len(items) != len(locales) || len(alts) != len(locales)+1 {
				t.Fatalf("%s/%s: %d menu items, %d alternates", l.Code, route, len(items), len(alts))
			}
			for i, other := range locales {
				want := "/Studio" + other.prefix() + "/" + route
				if items[i][2] != other.Code || items[i][1] != want || (items[i][3] != "") != (other == l) {
					t.Errorf("%s/%s: menu entry %v, want %s at %s", l.Code, route, items[i][1:], other.Code, want)
				}
				if alts[i][1] != other.Code || alts[i][2] != "http://127.0.0.1:1"+want {
					t.Errorf("%s/%s: alternate %v", l.Code, route, alts[i][1:])
				}
			}
			if !strings.Contains(body, `<span class="lang-current" translate="no">`+l.Name+`</span><span class="lang-code" translate="no" aria-hidden="true">`+l.Short+`</span>`) {
				t.Errorf("%s/%s: the menu does not name its own language", l.Code, route)
			}
		}
		// A 404 has no page of its own elsewhere: the menu leads to each front page.
		body := get(t, h, "GET", "/Studio"+l.prefix()+"/nowhere").Body.String()
		if alt.MatchString(body) {
			t.Errorf("%s: a 404 names alternates", l.Code)
		}
		for _, m := range item.FindAllStringSubmatch(body, -1) {
			if !strings.HasSuffix(m[1], "/") {
				t.Errorf("%s 404: menu entry %s is not a front page", l.Code, m[1])
			}
		}
	}
}

// A translation once arrived with Windows paths whose \b and \r had become
// control characters (es, ja, de, fr Install): no template may carry one.
func TestNoTemplateCarriesAControlCharacter(t *testing.T) {
	bad := regexp.MustCompile("[\x00-\x08\x0b\x0c\x0e-\x1f\x7f\r]")
	fs.WalkDir(ui.Files, "Pages", func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		raw, _ := fs.ReadFile(ui.Files, p)
		if loc := bad.FindIndex(raw); loc != nil {
			t.Errorf("%s: control character %q at byte %d", p, raw[loc[0]], loc[0])
		}
		return nil
	})
}

var (
	idAttr   = regexp.MustCompile(`\sid="([^"]+)"`)
	codeLike = regexp.MustCompile(`(?s)<(code|kbd|pre)[^>]*>(.*?)</(?:code|kbd|pre)>`)
	digits   = regexp.MustCompile(`\d+`)
)

// A translated article says what the English one says: the same anchors, and
// every command, key, value and keystroke the English shows. When the English
// changes (a release rewrites the manual), its translations fail here until
// they follow, so an old fact never stays behind in another language.
func TestTranslatedArticlesKeepTheEnglishFacts(t *testing.T) {
	read := func(p string) string {
		b, err := fs.ReadFile(ui.Files, p)
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	tokens := func(s string) map[string]bool {
		out := map[string]bool{}
		for _, m := range codeLike.FindAllStringSubmatch(s, -1) {
			out[html.UnescapeString(strings.Join(strings.Fields(m[2]), " "))] = true
		}
		return out
	}
	ids := func(s string) string {
		var out []string
		for _, m := range idAttr.FindAllStringSubmatch(s, -1) {
			out = append(out, m[1])
		}
		return strings.Join(out, " ")
	}
	files := []string{"Home.html", "Changelog.html", "License.html", "NotFound.html", "Docs.html"}
	for _, p := range pages {
		if p.Section != "" {
			files = append(files, p.File+".html")
		}
	}
	for _, name := range files {
		en := read("Pages/" + name)
		for _, l := range locales[1:] {
			tr := read(l.pageFile(name))
			if ids(tr) != ids(en) {
				t.Errorf("%s %s: anchors differ from English", l.Code, name)
			}
			theirs := tokens(tr)
			for tok := range tokens(en) {
				if !theirs[tok] {
					t.Errorf("%s %s: lost %q", l.Code, name, tok)
				}
			}
		}
	}
	// Leads carry the English numbers (the 31 provider presets, ...).
	for _, l := range locales[1:] {
		for _, p := range pages {
			if p.Section == "" {
				continue
			}
			want := strings.Join(digits.FindAllString(p.Lead, -1), " ")
			if got := strings.Join(digits.FindAllString(l.text.Docs[p.Route].Lead, -1), " "); got != want {
				t.Errorf("%s %s lead numbers %q, English %q", l.Code, p.Route, got, want)
			}
		}
	}
}

// Rune draws its own screen in English, so the replay does in every language:
// a translation changes only what a screen reader says about it and the
// site's labels that say what is simulated.
func TestTheReplayShowsWhatTheCLIDrawsInEveryLanguage(t *testing.T) {
	aria := regexp.MustCompile(`aria-label="[^"]*"`)
	read := func(p string) string {
		b, err := fs.ReadFile(ui.Files, p)
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	en := aria.ReplaceAllString(read("Pages/TerminalContent.html"), "")
	home := read("Pages/Home.html")
	drawn := []string{"Press Ctrl+↓ Jump to latest", "SESSIONS", "PLAN", "FILES CHANGED", "ask, or / for commands",
		"Allow once", "Always allow", "Refuse", "Context Window", "TokIn", "TokOut", "CacheIn", "Spend",
		"<dt>Title</dt>", "<dt>Started</dt>", "<dt>Activity</dt>", "asks before actions run",
		"works freely inside workspace", "unconfined system access", "steps &middot; 1 turn", "steps · 1 turn"}
	for _, l := range locales[1:] {
		if aria.ReplaceAllString(read(l.pageFile("TerminalContent.html")), "") != en {
			t.Errorf("%s: the recorded transcript is not drawn as the CLI draws it", l.Code)
		}
		tr := read(l.pageFile("Home.html"))
		for _, s := range drawn {
			if strings.Count(tr, s) != strings.Count(home, s) {
				t.Errorf("%s Home: %q drawn %d times, the CLI %d", l.Code, s, strings.Count(tr, s), strings.Count(home, s))
			}
		}
	}
}

// Only the English license binds; a translated page must say so and lead to it.
func TestTranslatedLicenseSaysTheEnglishBinds(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, l := range locales[1:] {
		raw, _ := fs.ReadFile(ui.Files, l.pageFile("License.html"))
		if strings.Contains(string(raw), "word for word") {
			t.Errorf("%s: a translation claims to be LICENSE.txt word for word", l.Code)
		}
		body := get(t, h, "GET", "/Studio/"+l.Code+"/license").Body.String()
		lead := regexp.MustCompile(`(?s)<p class="article-lead">(.*?)</p>`).FindStringSubmatch(body)
		if lead == nil || !strings.Contains(lead[1], `href="/Studio/license" hreflang="en"`) || !strings.Contains(lead[1], "<code>LICENSE.txt</code>") {
			t.Errorf("%s: the license lead does not lead to the binding English text", l.Code)
		}
	}
}

// The reference names the documented build in every language, never a number
// a translation froze.
func TestTranslatedReferenceNamesTheDocumentedVersion(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	old := regexp.MustCompile(`Rune \d+\.\d+\.\d+`)
	for _, l := range locales[1:] {
		body := get(t, h, "GET", "/Studio/"+l.Code+"/docs/reference").Body.String()
		for _, v := range old.FindAllString(body, -1) {
			if v != "Rune "+documentedVersion {
				t.Errorf("%s reference names %s, documented %s", l.Code, v, documentedVersion)
			}
		}
		if !strings.Contains(body, "Rune "+documentedVersion) {
			t.Errorf("%s reference does not name Rune %s", l.Code, documentedVersion)
		}
	}
}

// The line under each install command is in the page's language and still
// names only what the catalog holds.
func TestInstallNoteSpeaksThePageLanguage(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	english := installNote(nil, "linux")
	for _, l := range locales[1:] {
		body := get(t, h, "GET", "/Studio/"+l.Code+"/").Body.String()
		if strings.Contains(body, english) {
			t.Errorf("%s home shows the English install note", l.Code)
		}
		if !strings.Contains(body, html.EscapeString(l.text.InstallNote.None)) {
			t.Errorf("%s home does not show its own install note", l.Code)
		}
	}
	f.publish(t, "0.9.1", fakeRune("0.9.1"))
	h = newTestServer(t, testConfig(f.dir)).routes()
	for _, l := range locales[1:] {
		body := get(t, h, "GET", "/Studio/"+l.Code+"/").Body.String()
		if !strings.Contains(body, "0.9.1") {
			t.Errorf("%s home does not name the published release", l.Code)
		}
	}
}
