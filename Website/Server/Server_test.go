package main

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	ui "atoryn.com/studio/web/UI"
)

// ── fixtures ────────────────────────────────────────────────────────────────

type releaseFixture struct {
	dir  string
	priv ed25519.PrivateKey
}

func newDownloads(t *testing.T) *releaseFixture {
	t.Helper()
	pub, priv, _ := ed25519.GenerateKey(rand.Reader)
	der, _ := x509.MarshalPKIXPublicKey(pub)
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, keyFile), pem.EncodeToMemory(&pem.Block{Type: "PUBLIC KEY", Bytes: der}), 0o644)
	return &releaseFixture{dir: dir, priv: priv}
}

func tarball(t *testing.T, files map[string]string) []byte {
	t.Helper()
	var b bytes.Buffer
	zw := gzip.NewWriter(&b)
	tw := tar.NewWriter(zw)
	for name, body := range files {
		tw.WriteHeader(&tar.Header{Name: name, Mode: 0o755, Size: int64(len(body)), Typeflag: tar.TypeReg})
		tw.Write([]byte(body))
	}
	tw.Close()
	zw.Close()
	return b.Bytes()
}

// publish writes a correctly signed release, as PublishRune.sh does.
func (f *releaseFixture) publish(t *testing.T, version string, tarFiles map[string]string) string {
	t.Helper()
	dir := filepath.Join(f.dir, version)
	os.MkdirAll(dir, 0o755)
	name := "rune-" + version + "-linux-x86_64-gnu.tar.gz"
	art := tarball(t, tarFiles)
	os.WriteFile(filepath.Join(dir, name), art, 0o644)
	sum := sha256.Sum256(art)
	meta, _ := json.Marshal(Release{Version: version, Date: "2026-10-24", Notes: []string{"First release."},
		Files: []Artifact{{Name: name, OS: "linux", Arch: "x86_64", Libc: "gnu", Size: int64(len(art)), SHA256: hex.EncodeToString(sum[:])}}})
	os.WriteFile(filepath.Join(dir, metaFile), meta, 0o644)
	msum := sha256.Sum256(meta)
	sums := fmt.Sprintf("%s  %s\n%s  %s\n", hex.EncodeToString(sum[:]), name, hex.EncodeToString(msum[:]), metaFile)
	os.WriteFile(filepath.Join(dir, sumsFile), []byte(sums), 0o644)
	os.WriteFile(filepath.Join(dir, sigFile), ed25519.Sign(f.priv, []byte(sums)), 0o644)
	return name
}

func fakeRune(version string) map[string]string {
	return map[string]string{
		"rune":                    "#!/bin/sh\necho \"rune " + version + "\"\n",
		"LICENSE.txt":             "license\n",
		"THIRD-PARTY-NOTICES.txt": "notices\n",
	}
}

func testConfig(downloads string) *Config {
	return &Config{Env: "Dev", BasePath: "/Studio", Origin: "http://127.0.0.1:1", Hosts: []string{"example.test"},
		Listen: []string{"127.0.0.1:3501"}, Health: "127.0.0.1:3503", Downloads: downloads}
}

func newTestServer(t *testing.T, cfg *Config) *server {
	t.Helper()
	if err := cfg.validate(); err != nil {
		t.Fatal(err)
	}
	st, err := newSite(cfg)
	if err != nil {
		t.Fatal(err)
	}
	dl, err := os.OpenRoot(cfg.Downloads)
	if err != nil {
		t.Fatal(err)
	}
	cat, err := openCatalog(cfg.Downloads)
	if err != nil {
		t.Fatal(err)
	}
	return newServer(cfg, st, cat, dl)
}

func get(t *testing.T, h http.Handler, method, target string, mutate ...func(*http.Request)) *httptest.ResponseRecorder {
	t.Helper()
	r := httptest.NewRequest(method, "http://example.test"+target, nil)
	for _, m := range mutate {
		m(r)
	}
	w := httptest.NewRecorder()
	h.ServeHTTP(w, r)
	return w
}

// ── catalog ─────────────────────────────────────────────────────────────────

func TestCatalogListsOnlyCorrectlySignedReleases(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	f.publish(t, "0.10.0", fakeRune("0.10.0"))

	tampered := f.publish(t, "0.9.1", fakeRune("0.9.1"))
	os.WriteFile(filepath.Join(f.dir, "0.9.1", tampered), []byte("evil"), 0o644)

	f.publish(t, "0.9.2", fakeRune("0.9.2"))
	os.WriteFile(filepath.Join(f.dir, "0.9.2", sigFile), bytes.Repeat([]byte{1}, 64), 0o644)

	f.publish(t, "0.9.3", fakeRune("0.9.3"))
	meta, _ := os.ReadFile(filepath.Join(f.dir, "0.9.3", metaFile))
	os.WriteFile(filepath.Join(f.dir, "0.9.3", metaFile), bytes.Replace(meta, []byte("First"), []byte("Fake!"), 1), 0o644)

	os.MkdirAll(filepath.Join(f.dir, "../escape"), 0o755)
	os.Symlink("../escape", filepath.Join(f.dir, "0.9.4"))

	cs, err := openCatalog(f.dir)
	if err != nil {
		t.Fatal(err)
	}
	c := cs.get()
	var got []string
	for _, r := range c.Releases {
		got = append(got, r.Version)
	}
	if strings.Join(got, ",") != "0.10.0,0.9.0" {
		t.Fatalf("listed %v, want only the two intact releases, newest (by number) first", got)
	}
	if c.KeyFingerprint == "" {
		t.Fatal("no fingerprint")
	}
}

func TestCatalogWithoutKeyOffersNothing(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	os.Remove(filepath.Join(f.dir, keyFile))
	cs, _ := openCatalog(f.dir)
	if n := len(cs.get().Releases); n != 0 {
		t.Fatalf("%d releases offered without a key", n)
	}
}

// ── HTTP surface ────────────────────────────────────────────────────────────

func TestEveryPageRendersWithTheSecurityHeaders(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, p := range pages {
		w := get(t, h, "GET", "/Studio/"+p.Route)
		if w.Code != 200 {
			t.Fatalf("%s: %d", p.Route, w.Code)
		}
		hd := w.Header()
		for _, k := range []string{"Content-Security-Policy", "X-Content-Type-Options", "X-Frame-Options", "Referrer-Policy",
			"Cross-Origin-Opener-Policy", "Cross-Origin-Resource-Policy", "Permissions-Policy", "X-Robots-Tag"} {
			if hd.Get(k) == "" {
				t.Errorf("%s: missing %s", p.Route, k)
			}
		}
		if !strings.Contains(hd.Get("Content-Security-Policy"), "default-src 'none'") ||
			strings.Contains(hd.Get("Content-Security-Policy"), "unsafe") {
			t.Errorf("weak CSP: %s", hd.Get("Content-Security-Policy"))
		}
		body := w.Body.String()
		if !strings.Contains(body, "<title>") || !strings.Contains(body, "noindex") {
			t.Errorf("%s: dev page lacks title or noindex", p.Route)
		}
		// Every referenced asset resolves.
		for _, m := range regexp.MustCompile(`(?:href|src)="(/Studio/static/[^"]+)"`).FindAllStringSubmatch(body, -1) {
			if a := get(t, h, "GET", m[1]); a.Code != 200 || !strings.Contains(a.Header().Get("Cache-Control"), "immutable") {
				t.Errorf("%s: asset %s → %d", p.Route, m[1], a.Code)
			}
		}
	}
}

func TestProdAddsHSTSAndIndexingOnlyWhenAsked(t *testing.T) {
	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.Env, cfg.Origin, cfg.Indexing = "Prod", "https://atoryn.com", true
	cfg.Listen = []string{"127.0.0.1:3401"}
	h := newTestServer(t, cfg).routes()
	w := get(t, h, "GET", "/Studio/")
	if w.Header().Get("Strict-Transport-Security") == "" || w.Header().Get("X-Robots-Tag") != "" ||
		strings.Contains(w.Body.String(), "noindex") || strings.Contains(w.Body.String(), "Development build") {
		t.Fatal("prod headers or body wrong")
	}
	if w := get(t, h, "GET", "/Studio/sitemap.xml"); w.Code != 200 || !strings.Contains(w.Body.String(), "https://atoryn.com/Studio/security") {
		t.Fatalf("sitemap: %d", w.Code)
	}
}

func TestRequestRulesHold(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, m := range []string{"POST", "PUT", "DELETE", "OPTIONS", "TRACE", "PATCH"} {
		if w := get(t, h, m, "/Studio/"); w.Code != 405 || w.Header().Get("Allow") != "GET, HEAD" {
			t.Errorf("%s → %d", m, w.Code)
		}
	}
	if w := get(t, h, "GET", "/Studio/", func(r *http.Request) { r.Host = "evil.test" }); w.Code != http.StatusMisdirectedRequest {
		t.Errorf("foreign host → %d", w.Code)
	}
	if w := get(t, h, "GET", "/Studio/", func(r *http.Request) { r.ContentLength = 5; r.Body = io.NopCloser(strings.NewReader("hello")) }); w.Code != 400 {
		t.Errorf("GET with body → %d", w.Code)
	}
	if w := get(t, h, "HEAD", "/Studio/"); w.Code != 200 || w.Body.Len() != 0 {
		t.Errorf("HEAD → %d with %d bytes", w.Code, w.Body.Len())
	}
	if w := get(t, h, "GET", "/Studio"); w.Code != 301 || w.Header().Get("Location") != "/Studio/" {
		t.Errorf("/Studio → %d %s", w.Code, w.Header().Get("Location"))
	}
	if w := get(t, h, "GET", "/Studio/install/"); w.Code != 301 {
		t.Errorf("trailing slash → %d", w.Code)
	}
	if w := get(t, h, "GET", "/Studio/nothing"); w.Code != 404 || !strings.Contains(w.Body.String(), "Nothing") {
		t.Errorf("unknown page → %d", w.Code)
	}
	if w := get(t, h, "GET", "/"); w.Code != 404 {
		t.Errorf("outside the base path → %d", w.Code)
	}
}

// No request can read a file the catalog does not list, whatever the encoding.
func TestNothingOutsideTheCatalogIsServed(t *testing.T) {
	f := newDownloads(t)
	name := f.publish(t, "0.9.0", fakeRune("0.9.0"))
	os.WriteFile(filepath.Join(f.dir, "secret.txt"), []byte("SECRET"), 0o644)
	os.WriteFile(filepath.Join(f.dir, "0.9.0", "extra.txt"), []byte("SECRET"), 0o644)
	srv := newTestServer(t, testConfig(f.dir))
	h := srv.routes()

	if w := get(t, h, "GET", "/Studio/download/0.9.0/"+name); w.Code != 200 || !strings.Contains(w.Header().Get("Content-Disposition"), "attachment") {
		t.Fatalf("artifact → %d", w.Code)
	}
	if w := get(t, h, "GET", "/Studio/download/0.9.0/"+name, func(r *http.Request) { r.Header.Set("Range", "bytes=0-9") }); w.Code != 206 {
		t.Errorf("range → %d", w.Code)
	}
	for _, p := range []string{
		"/Studio/download/0.9.0/extra.txt",
		"/Studio/download/0.9.0/release.json",
		"/Studio/download/0.9.0/..%2fsecret.txt",
		"/Studio/download/0.9.0/%2e%2e%2fsecret.txt",
		"/Studio/download/..%2f/secret.txt",
		"/Studio/download/0.9.0/../secret.txt",
		"/Studio/download/0.9.9/" + name,
		"/Studio/static/" + srv.site.build + "/../../go.mod",
		"/Studio/static/" + srv.site.build + "/..%2f..%2fgo.mod",
		"/Studio/static/0000000000000000/Styles/Site.css",
		"/Studio/static/" + srv.site.build + "/Styles/Missing.css",
	} {
		w := get(t, h, "GET", p)
		if strings.Contains(w.Body.String(), "SECRET") || strings.Contains(w.Body.String(), "module ") {
			t.Fatalf("%s leaked a file", p)
		}
		if w.Code == 200 {
			t.Errorf("%s → 200", p)
		}
	}
}

func TestStaticRequiresTheCurrentBuild(t *testing.T) {
	f := newDownloads(t)
	srv := newTestServer(t, testConfig(f.dir))
	h := srv.routes()
	if w := get(t, h, "GET", "/Studio/static/"+srv.site.build+"/Styles/Site.css"); w.Code != 200 || !strings.HasPrefix(w.Header().Get("Content-Type"), "text/css") {
		t.Fatalf("css → %d %s", w.Code, w.Header().Get("Content-Type"))
	}
}

// ── installer ───────────────────────────────────────────────────────────────

func TestInstallerIsValidShellAndQuotesItsValues(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	w := get(t, h, "GET", "/Studio/install.sh")
	body := w.Body.String()
	if w.Code != 200 || !strings.Contains(body, "LATEST='0.9.0'") || !strings.Contains(body, "BEGIN PUBLIC KEY") {
		t.Fatalf("installer: %d\n%s", w.Code, body)
	}
	if !strings.HasSuffix(strings.TrimSpace(body), `main "$@"`) {
		t.Fatal("main is not the last line")
	}
	cmd := exec.Command("sh", "-n")
	cmd.Stdin = strings.NewReader(body)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("sh -n: %v %s", err, out)
	}
	if got := shellQuote(`a'b$(x)`); got != `'a'"'"'b$(x)'` {
		t.Fatalf("shellQuote = %s", got)
	}
}

// The real installer against a real server: installs a signed release, and
// installs nothing when the archive or the signature was tampered with.
func TestInstallerEndToEnd(t *testing.T) {
	if _, err := exec.LookPath("openssl"); err != nil {
		t.Skip("openssl not available")
	}
	f := newDownloads(t)
	name := f.publish(t, "0.9.0", fakeRune("0.9.0"))
	cfg := testConfig(f.dir)
	ts := httptest.NewUnstartedServer(nil)
	ts.Start()
	defer ts.Close()
	u, _ := url.Parse(ts.URL)
	cfg.Origin, cfg.Hosts = ts.URL, []string{u.Host}
	srv := newTestServer(t, cfg)
	ts.Config.Handler = srv.routes()

	run := func() (string, string, error) {
		script := get(t, ts.Config.Handler, "GET", "/Studio/install.sh", func(r *http.Request) { r.Host = u.Host }).Body.String()
		home := t.TempDir()
		dir := filepath.Join(home, "bin")
		cmd := exec.Command("sh")
		cmd.Stdin = strings.NewReader(script)
		cmd.Env = []string{"HOME=" + home, "PATH=" + os.Getenv("PATH"), "RUNE_INSTALL_DIR=" + dir, "RUNE_LIBC=gnu"}
		out, err := cmd.CombinedOutput()
		return string(out), dir, err
	}

	out, dir, err := run()
	if err != nil {
		t.Fatalf("install failed: %v\n%s", err, out)
	}
	if b, _ := os.ReadFile(filepath.Join(dir, "rune")); !strings.Contains(string(b), "rune 0.9.0") {
		t.Fatalf("not installed:\n%s", out)
	}

	// Tampered archive (the server still lists the old hash: cached catalog).
	os.WriteFile(filepath.Join(f.dir, "0.9.0", name), tarball(t, fakeRune("6.6.6")), 0o644)
	out, dir, err = run()
	if err == nil || !strings.Contains(out, "CHECKSUM MISMATCH") {
		t.Fatalf("tampered archive installed: %v\n%s", err, out)
	}
	if _, serr := os.Stat(filepath.Join(dir, "rune")); serr == nil {
		t.Fatal("a binary was installed after a failed check")
	}

	// Re-signed by a different key: the signature check refuses.
	other := newDownloads(t)
	other.dir = f.dir
	other.publish(t, "0.9.0", fakeRune("0.9.0"))
	out, _, err = run()
	if err == nil || !strings.Contains(out, "SIGNATURE CHECK FAILED") {
		t.Fatalf("foreign signature accepted: %v\n%s", err, out)
	}
}

// ── leak gate ───────────────────────────────────────────────────────────────

// The site must tell people what RuneAgent does for them, never how it is
// built: no internal names, paths, settings, storage formats, hosts or ports.
// Also no inline code or style (the CSP forbids it) and no outside resource.
func TestPublicContentRevealsNoInternals(t *testing.T) {
	deny := []string{
		"perm::", "write_guard", "toolctx", "net::", "proto::", "engine/", "cli/", "crate", "cargo", "tokio",
		"landlock", "seccomp", "seatbelt", "job object", "sandbox-exec", "apparmor",
		"sqlite", "jsonl", "ledger", "arc-swap", "aes-256", "aes256", "gcm", "pbkdf", "argon",
		".rune", "rune.toml", "rune.db", "master.key", "secrets.vault", "classifier.md", "agents.md", "claude.md",
		"rune_home", "upgrade/v", "protocol ", "schema", "/home/", "noire", "deploy/", "project/", "ops/",
		"localhost", ":3401", ":3501", "3403", "3503",
		"nginx", "cloudflared", "systemd", "seadrive", "9router", "github", "takuya",
		"innerhtml", "eval(", "document.write", "style=", "onclick", "onload=", "javascript:",
	}
	external := regexp.MustCompile(`(?i)(?:href|src)\s*=\s*"(?:https?:)?//`)
	ipAddress := regexp.MustCompile(`\b\d{1,3}(?:\.\d{1,3}){3}\b`) // no machine address of any network
	inlineScript := regexp.MustCompile(`(?i)<script(?:\s[^>]*)?>\s*[^<\s]`)
	err := fs.WalkDir(ui.Files, ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || !strings.HasSuffix(p, ".html") && !strings.HasSuffix(p, ".css") && !strings.HasSuffix(p, ".js") && !strings.HasSuffix(p, ".sh") {
			return err
		}
		raw, _ := fs.ReadFile(ui.Files, p)
		low := strings.ToLower(string(raw))
		if strings.HasSuffix(p, ".sh") {
			// The installer necessarily names the install folder; nothing else.
			low = strings.ReplaceAll(low, ".local/bin", "")
		}
		for _, term := range deny {
			if strings.Contains(low, term) {
				t.Errorf("%s reveals %q", p, term)
			}
		}
		if ip := ipAddress.FindString(string(raw)); ip != "" {
			t.Errorf("%s reveals an address: %s", p, ip)
		}
		if external.MatchString(string(raw)) {
			t.Errorf("%s loads an outside resource", p)
		}
		if strings.HasSuffix(p, ".html") && inlineScript.MatchString(string(raw)) {
			t.Errorf("%s has an inline script", p)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestConfigRefusesPublicListeners(t *testing.T) {
	cases := map[string]func(c *Config){
		"all interfaces": func(c *Config) { c.Listen = []string{":3501"} },
		"any address":    func(c *Config) { c.Listen = []string{"0.0.0.0:3501"} },
		"lan in prod": func(c *Config) {
			c.Env, c.Origin, c.Listen = "Prod", "https://atoryn.com", []string{"192.168.18.10:3401"}
		},
		"health on lan":         func(c *Config) { c.Health = "192.168.18.10:3503" },
		"shared address in dev": func(c *Config) { c.Listen = []string{"100.64.0.1:3501"} },
		"public address in dev": func(c *Config) { c.Listen = []string{"8.8.8.8:3501"} },
		"http prod":             func(c *Config) { c.Env, c.Origin = "Prod", "http://atoryn.com" },
		"origin with path":      func(c *Config) { c.Origin = "https://atoryn.com/x" },
		"dev indexed":           func(c *Config) { c.Indexing = true },
		"bad base path":         func(c *Config) { c.BasePath = "/Studio/../x" },
		"relative downloads":    func(c *Config) { c.Downloads = "Downloads" },
		"host with credentials": func(c *Config) { c.Hosts = []string{"a@b"} },
	}
	for name, mutate := range cases {
		c := testConfig("/tmp")
		mutate(c)
		if err := c.validate(); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
	ok := testConfig("/tmp")
	ok.Listen = []string{"127.0.0.1:3501", "192.168.18.10:3501"}
	if err := ok.validate(); err != nil {
		t.Errorf("dev on loopback + LAN refused: %v", err)
	}
}
