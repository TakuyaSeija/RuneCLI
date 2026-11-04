package main

import (
	"html"
	"net/url"
	"os"
	"regexp"
	"strconv"
	"strings"
	"testing"
)

func TestSecurityLivesInDocumentation(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, route := range []string{"/Studio/security", "/Studio/security/"} {
		w := get(t, h, "GET", route)
		if w.Code != 301 || w.Header().Get("Location") != "/Studio/docs/security" {
			t.Fatalf("%s: %d %s", route, w.Code, w.Header().Get("Location"))
		}
	}
	body := get(t, h, "GET", "/Studio/docs/security").Body.String()
	if !strings.Contains(body, `<h1>Security</h1>`) || strings.Contains(body, `href="/Studio/security"`) {
		t.Fatal("security is not a single documentation article")
	}
}

func TestDocumentationNavigationAndHeadingContract(t *testing.T) {
	f := newDownloads(t)
	st := newTestServer(t, testConfig(f.dir))
	h := st.routes()
	count := 0
	for _, p := range pages {
		if p.Section == "" {
			continue
		}
		count++
		body := get(t, h, "GET", "/Studio/"+p.Route).Body.String()
		if !strings.Contains(body, `href="/Studio/`+p.Route+`" aria-current="page"`) {
			t.Errorf("%s: no active article", p.Route)
		}
		// The article's own chapter starts open; every chapter opens on its own, none closes another.
		if strings.Count(body, `class="docs-group" open`) != 1 || strings.Contains(body, `name="docs-section"`) {
			t.Errorf("%s: the active chapter is not the one open, or chapters still close each other", p.Route)
		}
		if !strings.Contains(body, `Reference for Rune `+documentedVersion) {
			t.Errorf("%s: reference version absent", p.Route)
		}
		if len(st.site.pages["en"][p.Route].Headings) < 2 {
			t.Errorf("%s: missing TOC", p.Route)
		}
	}
	if count < 25 {
		t.Fatalf("only %d articles", count)
	}
	for _, bad := range []string{`<h2>No ID</h2>`, `<h2 id="a">A</h2><h2 id="a">B</h2>`, `<h2 id="a"><b>A</b></h2><h2 id="b">B</h2>`} {
		if _, err := documentationHeadings([]byte(bad)); err == nil {
			t.Errorf("accepted malformed headings: %s", bad)
		}
	}
}

func TestEveryInternalArticleLinkResolves(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.1", fakeRune("0.9.1"))
	st := newTestServer(t, testConfig(f.dir))
	h := st.routes()
	hrefs := regexp.MustCompile(`href="([^"]+)"`)
	for _, p := range pages {
		base, _ := url.Parse("/Studio/" + p.Route)
		body := get(t, h, "GET", base.String()).Body.String()
		for _, m := range hrefs.FindAllStringSubmatch(body, -1) {
			u, err := url.Parse(html.UnescapeString(m[1]))
			if err != nil {
				t.Fatal(err)
			}
			if u.IsAbs() || strings.HasPrefix(u.Path, "/Studio/static/") {
				continue
			}
			u = base.ResolveReference(u)
			if !strings.HasPrefix(u.Path, "/Studio/") {
				continue
			}
			w := get(t, h, "GET", u.Path)
			if w.Code != 200 {
				t.Errorf("%s links to %s: %d", p.Route, u.Path, w.Code)
				continue
			}
			if u.Fragment != "" && !strings.Contains(w.Body.String(), `id="`+u.Fragment+`"`) {
				t.Errorf("%s: missing target %s#%s", p.Route, u.Path, u.Fragment)
			}
		}
	}
}

func TestDocumentedCommandsAndSelectedDefaultsMatchApplication(t *testing.T) {
	read := func(path string) string {
		b, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	keys := read("../../Cli/src/ui/keys.rs")
	start := strings.Index(keys, "pub const COMMANDS:")
	if start < 0 {
		t.Fatal("application command catalogue moved; review documentation coverage")
	}
	end := strings.Index(keys[start:], "];")
	if end < 0 {
		t.Fatal("application command catalogue has no end")
	}
	commands := regexp.MustCompile(`\(\s*"([a-z]+)"`).FindAllStringSubmatch(keys[start:start+end], -1)
	article := read("../UI/Pages/Docs/Commands.html")
	for _, command := range commands {
		if !strings.Contains(article, "/"+command[1]) {
			t.Errorf("slash command /%s is undocumented", command[1])
		}
	}
	if len(commands) < 20 {
		t.Fatal("command source extraction no longer covers the catalogue")
	}
	fields := read("../../Engine/src/settings/Configuration.rs")
	defaults := map[string]string{}
	for _, m := range regexp.MustCompile(`key:"([^"]+)", default:"((?:\\.|[^"\\])*)"`).FindAllStringSubmatch(fields, -1) {
		value, err := strconv.Unquote(`"` + m[2] + `"`)
		if err != nil {
			t.Fatal(err)
		}
		defaults[m[1]] = value
	}
	for _, item := range []struct{ key, value, file, phrase string }{
		{"retry.credit_cooldown", `"15m"`, "Runtime", "15m"},
		{"network.connect_timeout", `"10s"`, "Runtime", "10 seconds"},
		{"inspect_file.max_file_size", `"16MiB"`, "Tools", "16 MiB"},
		{"terminal.max_background_log_size", `"100MiB"`, "Terminal", "100 MiB"},
		{"tasks.max_tasks", "20", "Tools", "20 tasks"},
		{"compact.max_output_tokens", "32768", "Auxiliary", "32,768"},
	} {
		if defaults[item.key] != item.value {
			t.Errorf("%s default changed from %s to %s; review its article", item.key, item.value, defaults[item.key])
		}
		if !strings.Contains(read("../UI/Pages/Docs/"+item.file+".html"), item.phrase) {
			t.Errorf("%s: expected documented default %s", item.file, item.phrase)
		}
	}
}

func TestHonestHomeAndOneInstallerCommand(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	home := get(t, h, "GET", "/Studio/").Body.String()
	for _, claim := range []string{"One prompt. A music", "Search hundreds", "Every change can be undone", "This is a recorded run"} {
		if strings.Contains(home, claim) {
			t.Errorf("unsupported claim retained: %s", claim)
		}
	}
	for _, required := range []string{"Interactive illustration", "approvals are simulated", "data-demo-pause", "data-demo-replay", "configured providers"} {
		if !strings.Contains(home, required) {
			t.Errorf("missing demo/copy contract: %s", required)
		}
	}
	// The install box holds one command per system: install.sh for Linux and macOS, install.ps1 for Windows.
	for _, p := range pages {
		body := get(t, h, "GET", "/Studio/"+p.Route).Body.String()
		curl, windows := 0, 0
		if p.Route == "" {
			curl, windows = 2, 1
		}
		if n := strings.Count(body, "curl -fsSL"); n != curl {
			t.Errorf("%s: %d curl commands, want %d", p.Route, n, curl)
		}
		if n := strings.Count(body, `id="install-cmd-windows"`); n != windows {
			t.Errorf("%s: %d Windows install commands, want %d", p.Route, n, windows)
		}
	}
	install := get(t, h, "GET", "/Studio/").Body.String()
	if !strings.Contains(install, "currently exits without installing anything") {
		t.Fatal("unpublished release implies installation is ready")
	}
}
