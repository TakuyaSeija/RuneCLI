package main

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"crypto/ecdsa"
	"crypto/elliptic"
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
	"runtime"
	"sort"
	"strings"
	"testing"

	ui "atoryn.com/studio/web/UI"
)

// ── fixtures ────────────────────────────────────────────────────────────────

type releaseFixture struct {
	dir  string
	priv *ecdsa.PrivateKey
}

func newDownloads(t *testing.T) *releaseFixture {
	t.Helper()
	priv, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	der, _ := x509.MarshalPKIXPublicKey(&priv.PublicKey)
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

// sign is what PublishRune.sh's signer writes: ECDSA P-256 over the SHA-256
// of the bytes, DER-encoded.
func (f *releaseFixture) sign(data []byte) []byte {
	digest := sha256.Sum256(data)
	sig, err := ecdsa.SignASN1(rand.Reader, f.priv, digest[:])
	if err != nil {
		panic(err)
	}
	return sig
}

// publish writes a correctly signed release with one Linux archive, as PublishRune.sh does.
func (f *releaseFixture) publish(t *testing.T, version string, tarFiles map[string]string) string {
	t.Helper()
	name := "rune-" + version + "-linux-x86_64-gnu.tar.gz"
	f.publishArchives(t, version, map[string][]byte{name: tarball(t, tarFiles)})
	return name
}

// publishArchives writes a correctly signed release holding exactly these
// archives; each one's system, processor and C library come from its name.
func (f *releaseFixture) publishArchives(t *testing.T, version string, archives map[string][]byte) {
	t.Helper()
	dir := filepath.Join(f.dir, version)
	os.MkdirAll(dir, 0o755)
	var files []Artifact
	var sums strings.Builder
	names := make([]string, 0, len(archives))
	for name := range archives {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		art := archives[name]
		m := artifactRe.FindStringSubmatch(name)
		if m == nil {
			t.Fatalf("not an archive name: %s", name)
		}
		os.WriteFile(filepath.Join(dir, name), art, 0o644)
		sum := sha256.Sum256(art)
		files = append(files, Artifact{Name: name, OS: m[2] + m[5] + m[7], Arch: m[3] + m[6] + m[8], Libc: m[4],
			Size: int64(len(art)), SHA256: hex.EncodeToString(sum[:])})
		fmt.Fprintf(&sums, "%s  %s\n", hex.EncodeToString(sum[:]), name)
	}
	meta, _ := json.Marshal(Release{Version: version, Date: "2026-10-24", Notes: []string{"First release: the artifact own words, which the page never shows."}, Files: files})
	os.WriteFile(filepath.Join(dir, metaFile), meta, 0o644)
	msum := sha256.Sum256(meta)
	fmt.Fprintf(&sums, "%s  %s\n", hex.EncodeToString(msum[:]), metaFile)
	os.WriteFile(filepath.Join(dir, sumsFile), []byte(sums.String()), 0o644)
	os.WriteFile(filepath.Join(dir, sigFile), f.sign([]byte(sums.String())), 0o644)
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
	// Windows refuses to remove a folder a handle still holds: close both
	// before the test's temporary folders are removed (cleanups run last first).
	t.Cleanup(func() { dl.Close(); cat.root.Close() })
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
	t.Cleanup(func() { cs.root.Close() })
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
	cs, err := openCatalog(f.dir)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { cs.root.Close() })
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

func TestInstanceUsesTheAdjacentHealthPort(t *testing.T) {
	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.Env, cfg.Origin = "Prod", "https://atoryn.com"
	cfg.Listen, cfg.Health = []string{"127.0.0.1:3200"}, "127.0.0.1:3201"
	if err := cfg.validate(); err != nil {
		t.Fatalf("the adjacent production pair was refused: %v", err)
	}
}

func TestProdAddsHSTSAndIndexingOnlyWhenAsked(t *testing.T) {
	f := newDownloads(t)
	cfg := testConfig(f.dir)
	cfg.Env, cfg.Origin, cfg.Indexing = "Prod", "https://atoryn.com", true
	cfg.Listen = []string{"127.0.0.1:3200"}
	h := newTestServer(t, cfg).routes()
	w := get(t, h, "GET", "/Studio/")
	if w.Header().Get("Strict-Transport-Security") == "" || w.Header().Get("X-Robots-Tag") != "" ||
		strings.Contains(w.Body.String(), "noindex") || strings.Contains(w.Body.String(), "Development build") {
		t.Fatal("prod headers or body wrong")
	}
	if w := get(t, h, "GET", "/Studio/sitemap.xml"); w.Code != 200 || !strings.Contains(w.Body.String(), "https://atoryn.com/Studio/docs/security") || strings.Contains(w.Body.String(), "<loc>https://atoryn.com/Studio/security</loc>") {
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

// The Windows installer is served the same way, carries the key in the form
// Windows imports (the point of the same published key), stays ASCII so
// Windows PowerShell reads it the same under any code page, and runs nothing
// until its last line.
func TestWindowsInstallerCarriesTheReleaseKey(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	w := get(t, h, "GET", "/Studio/install.ps1")
	body := w.Body.String()
	want := fmt.Sprintf("$KeyX = '%064x'", f.priv.PublicKey.X)
	if w.Code != 200 || !strings.Contains(body, "$Latest = '0.9.0'") || !strings.Contains(body, want) ||
		!strings.Contains(body, fmt.Sprintf("$KeyY = '%064x'", f.priv.PublicKey.Y)) {
		t.Fatalf("install.ps1: %d\n%s", w.Code, body)
	}
	if ct := w.Header().Get("Content-Type"); ct != "text/plain; charset=utf-8" {
		t.Fatalf("content type %q", ct)
	}
	for i, r := range body {
		if r > 127 {
			t.Fatalf("non-ASCII character %q at byte %d", r, i)
		}
	}
	if !strings.HasPrefix(strings.TrimSpace(body[strings.Index(body, "\n& {"):]), "& {") || !strings.HasSuffix(strings.TrimSpace(body), "}") {
		t.Fatal("the script is not one block run at its end")
	}
	if got := powershellQuote(`it's $(x)`); got != `'it''s $(x)'` {
		t.Fatalf("powershellQuote = %s", got)
	}
	// Without a release, both installers say so and carry no key.
	empty := newTestServer(t, testConfig(newDownloads(t).dir)).routes()
	if b := get(t, empty, "GET", "/Studio/install.ps1").Body.String(); !strings.Contains(b, "$KeyX = ''") || !strings.Contains(b, "$Latest = ''") {
		t.Fatalf("unpublished install.ps1:\n%s", b)
	}
}

// The installer finishes the job in the window that ran it, without editing a
// profile: sourcing it (bash and zsh) adds the install folder to that shell's
// PATH so `rune` runs at once, the piped form says so plainly and prints the one
// line instead of claiming to be ready, and a failing run never closes the shell
// that sourced it (dash and ash report no sourcing, so the work runs in a
// subshell and its status is handed back).
func TestInstallerFinishesInTheSameWindow(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("install.sh runs on Linux and macOS")
	}
	bash, err := exec.LookPath("bash")
	if err != nil {
		t.Skip("bash is needed for the sourced form")
	}
	if _, err := exec.LookPath("openssl"); err != nil {
		t.Skip("openssl not available")
	}
	f := newDownloads(t)
	f.publishArchives(t, "0.9.0", map[string][]byte{unixArchive("0.9.0"): tarball(t, fakeRune("0.9.0"))})
	ts, host := installerServer(t, f.dir)
	script := get(t, ts.Config.Handler, "GET", "/Studio/install.sh", func(r *http.Request) { r.Host = host }).Body.String()

	// run puts the served installer beside a driver script in a fresh home and
	// runs the driver with the given shell; nothing here touches the real home.
	run := func(t *testing.T, shell, driver string, extra ...string) (string, string) {
		t.Helper()
		home := t.TempDir()
		dir := filepath.Join(home, "pkg")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		os.WriteFile(filepath.Join(dir, "install.sh"), []byte(script), 0o644)
		os.WriteFile(filepath.Join(dir, "driver.sh"), []byte(driver), 0o644)
		cmd := exec.Command(shell, "driver.sh")
		cmd.Dir = dir
		// A PATH without any writable folder keeps the run deterministic: the
		// installer cannot link into /usr/local/bin here, so the piped form must
		// print its one line instead of finding another rune.
		env := []string{"HOME=" + home, "PATH=/usr/bin:/bin", "RUNE_INSTALL_DIR=" + filepath.Join(home, "bin")}
		if runtime.GOOS == "linux" {
			env = append(env, "RUNE_LIBC=gnu")
		}
		cmd.Env = append(env, extra...)
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("%s driver failed: %v\n%s", shell, err, out)
		}
		return string(out), filepath.Join(home, "bin")
	}

	// Sourced by a script: the script gets the folder, the person's own shell
	// does not, and the installer says exactly that instead of "Ready".
	out, dir := run(t, bash, ". ./install.sh\ncommand -v rune\nrune --version\n")
	if !strings.Contains(out, dir+"/rune") || !strings.Contains(out, "rune 0.9.0") {
		t.Fatalf("a script that sourced the installer did not get rune:\n%s", out)
	}
	if !strings.Contains(out, "for the script that sourced") || strings.Contains(out, "Ready.") {
		t.Fatalf("a script-sourced run claims the person's shell is ready:\n%s", out)
	}

	// An interactive shell that sources it is ready in that same window.
	if _, err := exec.LookPath("script"); err == nil {
		home := t.TempDir()
		bin := filepath.Join(home, "bin")
		pkg := filepath.Join(home, "pkg")
		if err := os.MkdirAll(pkg, 0o755); err != nil {
			t.Fatal(err)
		}
		os.WriteFile(filepath.Join(pkg, "install.sh"), []byte(script), 0o644)
		os.WriteFile(filepath.Join(home, ".bashrc"), nil, 0o644)
		cmd := exec.Command("script", "-qc", "bash -i", "/dev/null")
		cmd.Dir = pkg
		env := []string{"HOME=" + home, "PATH=/usr/bin:/bin", "RUNE_INSTALL_DIR=" + bin, "TERM=dumb"}
		if runtime.GOOS == "linux" {
			env = append(env, "RUNE_LIBC=gnu")
		}
		cmd.Env = env
		cmd.Stdin = strings.NewReader(". ./install.sh\ncommand -v rune\nexit\n")
		pty, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("interactive run failed: %v\n%s", err, pty)
		}
		if !strings.Contains(string(pty), bin+"/rune") || !strings.Contains(string(pty), "Ready.") {
			t.Fatalf("an interactive shell that sourced the installer is not ready:\n%s", pty)
		}
	}

	// Piped: no claim of readiness, the one line instead.
	out, _ = run(t, bash, "sh ./install.sh > piped.txt 2>&1\ncat piped.txt\ncommand -v rune || echo none\n")
	if strings.Contains(out, "Ready.") {
		t.Fatalf("the piped form claims the window is ready:\n%s", out)
	}
	if !strings.Contains(out, `export PATH=`) || !strings.Contains(out, ". <(curl") {
		t.Fatalf("the piped form does not name the one line:\n%s", out)
	}

	// A failure in a sourced run leaves the calling shell alive.
	for _, shell := range []string{"sh", "bash"} {
		out, _ = run(t, shell, "RUNE_VERSION=9.9.9 . ./install.sh\necho shell-alive\n")
		if !strings.Contains(out, "shell-alive") {
			t.Fatalf("%s closed the shell after a failed sourced run:\n%s", shell, out)
		}
	}
}

// crlf rewrites a release's checksum list with CRLF line ends, as a list
// saved on Windows has them, and signs those bytes again.
func (f *releaseFixture) crlf(t *testing.T, version string) {
	t.Helper()
	path := filepath.Join(f.dir, version, sumsFile)
	sums, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	lf := bytes.ReplaceAll(sums, []byte("\r\n"), []byte("\n"))
	crlf := bytes.ReplaceAll(lf, []byte("\n"), []byte("\r\n"))
	os.WriteFile(path, crlf, 0o644)
	os.WriteFile(filepath.Join(f.dir, version, sigFile), f.sign(crlf), 0o644)
}

// installerServer serves the downloads folder through the real routes on a
// real port, as an installer reaches it.
func installerServer(t *testing.T, downloads string) (*httptest.Server, string) {
	t.Helper()
	cfg := testConfig(downloads)
	ts := httptest.NewUnstartedServer(nil)
	ts.Start()
	t.Cleanup(ts.Close)
	u, _ := url.Parse(ts.URL)
	cfg.Origin, cfg.Hosts = ts.URL, []string{u.Host}
	ts.Config.Handler = newTestServer(t, cfg).routes()
	return ts, u.Host
}

// unixArchive is the archive install.sh picks on this machine.
func unixArchive(version string) string {
	if runtime.GOOS == "darwin" {
		return "rune-" + version + "-macos-" + map[string]string{"arm64": "aarch64", "amd64": "x86_64"}[runtime.GOARCH] + ".tar.gz"
	}
	return "rune-" + version + "-linux-x86_64-gnu.tar.gz"
}

// The real installer against a real server, on Linux and on macOS (with
// LibreSSL there): installs a signed release, and installs nothing when the
// archive or the signature was tampered with.
func TestInstallerEndToEnd(t *testing.T) {
	if runtime.GOOS != "linux" && runtime.GOOS != "darwin" || runtime.GOOS == "linux" && runtime.GOARCH != "amd64" {
		t.Skip("install.sh runs on Linux x86_64 and macOS here")
	}
	if _, err := exec.LookPath("openssl"); err != nil {
		t.Skip("openssl not available")
	}
	publish := func(f *releaseFixture, files map[string]string) {
		tar := tarball(t, files)
		f.publishArchives(t, "0.9.0", map[string][]byte{
			"rune-0.9.0-linux-x86_64-gnu.tar.gz": tar, "rune-0.9.0-macos-aarch64.tar.gz": tar, "rune-0.9.0-macos-x86_64.tar.gz": tar,
		})
	}
	f := newDownloads(t)
	publish(f, fakeRune("0.9.0"))
	ts, host := installerServer(t, f.dir)

	run := func() (string, string, error) {
		script := get(t, ts.Config.Handler, "GET", "/Studio/install.sh", func(r *http.Request) { r.Host = host }).Body.String()
		home := t.TempDir()
		dir := filepath.Join(home, "bin")
		cmd := exec.Command("sh")
		cmd.Stdin = strings.NewReader(script)
		cmd.Env = []string{"HOME=" + home, "PATH=" + os.Getenv("PATH"), "RUNE_INSTALL_DIR=" + dir}
		if runtime.GOOS == "linux" {
			cmd.Env = append(cmd.Env, "RUNE_LIBC=gnu")
		}
		out, err := cmd.CombinedOutput()
		return string(out), dir, err
	}

	out, dir, err := run()
	if err != nil {
		t.Fatalf("install failed: %v\n%s", err, out)
	}
	if b, _ := os.ReadFile(filepath.Join(dir, "rune")); !strings.Contains(string(b), "rune 0.9.0") || !strings.Contains(out, "signature verified") {
		t.Fatalf("not installed:\n%s", out)
	}

	// Tampered archive (the server still lists the old hash: cached catalog).
	os.WriteFile(filepath.Join(f.dir, "0.9.0", unixArchive("0.9.0")), tarball(t, fakeRune("6.6.6")), 0o644)
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
	publish(other, fakeRune("0.9.0"))
	out, _, err = run()
	if err == nil || !strings.Contains(out, "SIGNATURE CHECK FAILED") {
		t.Fatalf("foreign signature accepted: %v\n%s", err, out)
	}

	// A checksum list with CRLF line ends still names its archive.
	g := newDownloads(t)
	publish(g, fakeRune("0.9.0"))
	g.crlf(t, "0.9.0")
	ts, host = installerServer(t, g.dir)
	out, dir, err = run()
	if err != nil {
		t.Fatalf("CRLF checksum list refused: %v\n%s", err, out)
	}
	if b, _ := os.ReadFile(filepath.Join(dir, "rune")); !strings.Contains(string(b), "rune 0.9.0") {
		t.Fatalf("not installed from a CRLF list:\n%s", out)
	}
}

// The Windows installer against a real server, in Windows PowerShell 5.1 and
// in PowerShell 7 where present: installs a signed release whose signature
// exercises both DER integer edge cases, and installs nothing when the
// archive or the signature was tampered with.
func TestWindowsInstallerEndToEnd(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("install.ps1 runs on Windows")
	}
	goTool, err := exec.LookPath("go")
	if err != nil {
		t.Skip("go is needed to build a stand-in rune.exe")
	}
	work := t.TempDir()
	os.WriteFile(filepath.Join(work, "go.mod"), []byte("module fakerune\n\ngo 1.24\n"), 0o644)
	os.WriteFile(filepath.Join(work, "main.go"), []byte("package main\n\nimport \"fmt\"\n\nvar version string\n\nfunc main() { fmt.Println(\"rune \" + version) }\n"), 0o644)
	exe := func(version string) []byte {
		out := filepath.Join(work, "rune-"+version+".exe")
		build := exec.Command(goTool, "build", "-ldflags", "-X main.version="+version, "-o", out, ".")
		build.Dir = work
		build.Env = append(os.Environ(), "CGO_ENABLED=0", "GOFLAGS=-mod=mod")
		if b, err := build.CombinedOutput(); err != nil {
			t.Fatalf("build stand-in: %v %s", err, b)
		}
		b, _ := os.ReadFile(out)
		return b
	}
	zipOf := func(program []byte) []byte {
		var b bytes.Buffer
		z := zip.NewWriter(&b)
		for name, body := range map[string][]byte{"rune.exe": program, "LICENSE.txt": []byte("license\n"), "THIRD-PARTY-NOTICES.txt": []byte("notices\n")} {
			w, _ := z.Create(name)
			w.Write(body)
		}
		z.Close()
		return b.Bytes()
	}
	good, bad := zipOf(exe("0.9.0")), zipOf(exe("6.6.6"))
	machine := map[string]string{"amd64": "x86_64", "arm64": "aarch64"}[runtime.GOARCH]
	name := "rune-0.9.0-windows-" + machine + ".zip"

	f := newDownloads(t)
	f.publishArchives(t, "0.9.0", map[string][]byte{"rune-0.9.0-windows-x86_64.zip": good, "rune-0.9.0-windows-aarch64.zip": good})
	ts, _ := installerServer(t, f.dir)

	shells := []string{"powershell"}
	if _, err := exec.LookPath("pwsh"); err == nil {
		shells = append(shells, "pwsh")
	}
	run := func(shell string) (string, string, error) {
		home := t.TempDir()
		dir := filepath.Join(home, "bin")
		cmd := exec.Command(shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command",
			"irm '"+ts.URL+"/Studio/install.ps1' | iex; exit $LASTEXITCODE")
		cmd.Env = append(os.Environ(), "RUNE_INSTALL_DIR="+dir, "LOCALAPPDATA="+filepath.Join(home, "local"))
		out, err := cmd.CombinedOutput()
		return string(out), dir, err
	}
	resign := func(f *releaseFixture, wantDER func([]byte) bool) {
		sums, _ := os.ReadFile(filepath.Join(f.dir, "0.9.0", sumsFile))
		for i := 0; i < 20000; i++ {
			if sig := f.sign(sums); wantDER(sig) {
				os.WriteFile(filepath.Join(f.dir, "0.9.0", sigFile), sig, 0o644)
				return
			}
		}
		t.Fatal("no signature of the wanted shape")
	}
	// r with a leading zero byte (its top bit set), and an integer shorter than 32 bytes.
	shapes := map[string]func([]byte) bool{
		"leading zero": func(der []byte) bool { return der[3] == 33 },
		"short integer": func(der []byte) bool {
			return der[3] < 32 || der[4+int(der[3])+1] < 32
		},
	}
	for _, shell := range shells {
		for shape, want := range shapes {
			resign(f, want)
			out, dir, err := run(shell)
			if err != nil || !strings.Contains(out, "signature verified") {
				t.Fatalf("%s (%s signature): install failed: %v\n%s", shell, shape, err, out)
			}
			got, err := exec.Command(filepath.Join(dir, "rune.exe"), "--version").Output()
			if err != nil || strings.TrimSpace(string(got)) != "rune 0.9.0" {
				t.Fatalf("%s: installed program says %q (%v)\n%s", shell, got, err, out)
			}
		}
	}

	// Tampered archive (the catalog still lists the signed hash).
	os.WriteFile(filepath.Join(f.dir, "0.9.0", name), bad, 0o644)
	out, dir, err := run(shells[0])
	if err == nil || !strings.Contains(out, "CHECKSUM MISMATCH") {
		t.Fatalf("tampered archive installed: %v\n%s", err, out)
	}
	if _, serr := os.Stat(filepath.Join(dir, "rune.exe")); serr == nil {
		t.Fatal("a program was installed after a failed check")
	}
	os.WriteFile(filepath.Join(f.dir, "0.9.0", name), good, 0o644)

	// Re-signed by a different key: the signature check refuses.
	other := newDownloads(t)
	other.dir = f.dir
	other.publishArchives(t, "0.9.0", map[string][]byte{"rune-0.9.0-windows-x86_64.zip": good, "rune-0.9.0-windows-aarch64.zip": good})
	out, _, err = run(shells[0])
	if err == nil || !strings.Contains(out, "SIGNATURE CHECK FAILED") {
		t.Fatalf("foreign signature accepted: %v\n%s", err, out)
	}

	// A checksum list with CRLF line ends, as Windows saves text, still names its archive.
	g := newDownloads(t)
	g.publishArchives(t, "0.9.0", map[string][]byte{"rune-0.9.0-windows-x86_64.zip": good, "rune-0.9.0-windows-aarch64.zip": good})
	g.crlf(t, "0.9.0")
	ts, _ = installerServer(t, g.dir)
	for _, shell := range shells {
		if out, dir, err := run(shell); err != nil || !strings.Contains(out, "signature verified") {
			t.Fatalf("%s: CRLF checksum list refused: %v\n%s", shell, err, out)
		} else if got, err := exec.Command(filepath.Join(dir, "rune.exe"), "--version").Output(); err != nil || strings.TrimSpace(string(got)) != "rune 0.9.0" {
			t.Fatalf("%s: program from a CRLF list says %q (%v)", shell, got, err)
		}
	}
}

// ── leak gate ───────────────────────────────────────────────────────────────

// The site tells people what RuneAgent does for them. It may name the
// mechanisms that make it trustworthy (sandbox, vault cipher, config file),
// but never how it is built: no internal names, paths, settings, storage formats, hosts or ports.
// Also no inline code or style (the CSP forbids it) and no outside resource.
var ordinaryWords = map[string][]*regexp.Regexp{
	"de":    {regexp.MustCompile(`\pL*schema\pL*`), regexp.MustCompile(`systemdienst\pL*`)},
	"es":    {regexp.MustCompile(`\bcargos?\b`)},
	"pt-BR": {regexp.MustCompile(`\bcargos?\b`)},
}

func TestPublicContentRevealsNoInternals(t *testing.T) {
	deny := []string{
		"perm::", "write_guard", "toolctx", "net::", "proto::", "engine/", "cli/", "crate", "cargo", "tokio",
		"apparmor",
		"sqlite", "jsonl", "ledger", "arc-swap", "pbkdf", "argon",
		"rune.db", "master.key", "secrets.vault", "claude.md",
		"upgrade/v", "protocol ", "schema", "/home/", "noire", "deploy/", "ops/",
		"localhost", ":3200", ":3201", ":3202", ":3203", ":3250", ":3251",
		":3301", ":3311", ":3401", ":3501", "3303", "3313", "3403", "3503",
		"nginx", "cloudflared", "systemd", "seadrive", "github", "takuya",
		"innerhtml", "eval(", "document.write", "style=", "onclick", "onload=", "javascript:",
	}
	external := regexp.MustCompile(`(?i)(?:href|src)\s*=\s*"(?:https?:)?//`)
	ipAddress := regexp.MustCompile(`\b\d{1,3}(?:\.\d{1,3}){3}\b`) // no machine address of any network
	inlineScript := regexp.MustCompile(`(?i)<script(?:\s[^>]*)?>\s*[^<\s]`)
	err := fs.WalkDir(ui.Files, ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || !strings.HasSuffix(p, ".html") && !strings.HasSuffix(p, ".css") && !strings.HasSuffix(p, ".js") && !strings.HasSuffix(p, ".sh") && !(strings.HasSuffix(p, ".json") && strings.HasPrefix(p, "Pages/")) {
			return err
		}
		raw, _ := fs.ReadFile(ui.Files, p)
		low := strings.ToLower(string(raw))
		if strings.HasSuffix(p, ".sh") {
			// The installer necessarily names the install folder; nothing else.
			low = strings.ReplaceAll(low, ".local/bin", "")
		}
		// A translation's ordinary words that merely contain a denied term:
		// German "Farbschema" (colour scheme) and "Systemdienst" (system
		// service), Spanish and Portuguese "a cargo de" (in charge of).
		for code, words := range ordinaryWords {
			if strings.HasPrefix(p, "Pages/"+code+"/") {
				for _, w := range words {
					low = w.ReplaceAllString(low, "")
				}
			}
		}
		for _, term := range deny {
			if strings.Contains(low, term) {
				t.Errorf("%s reveals %q", p, term)
			}
		}
		// Public product interfaces are necessary in the manual, not private
		// deployment details and not marketing copy elsewhere on the site.
		// Each translation's manual and its catalogue (metadata.json holds the
		// articles' leads) are the manual too.
		manual := strings.HasPrefix(p, "Pages/Docs/")
		for _, l := range locales[1:] {
			manual = manual || strings.HasPrefix(p, l.pageFile("Docs/")) || p == l.pageFile("metadata.json")
		}
		if !manual {
			for _, term := range []string{".rune", "rune_home", "classifier.md", "agents.md", "9router"} {
				if strings.Contains(low, term) {
					t.Errorf("%s exposes manual-only term %q", p, term)
				}
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

// One job per page: exactly one h1, one main landmark, the install command on
// the install page only, and every page reachable from the header without a
// second copy of the same action. The site is a manual, so structure is a
// contract like any other.
func TestEveryPageHasOneJobAndNoRepeatedAction(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	for _, p := range pages {
		w := get(t, h, "GET", "/Studio/"+p.Route)
		body := w.Body.String()
		if n := strings.Count(body, "<h1"); n != 1 {
			t.Errorf("%s: %d h1 headings", p.Route, n)
		}
		if n := strings.Count(body, `id="main"`); n != 1 {
			t.Errorf("%s: %d main landmarks", p.Route, n)
		}
		if p.Route == "" {
			continue // overview owns the site's single install command
		}
		if n := strings.Count(body, `class="cmd"`); n != 0 {
			t.Errorf("%s: %d install command blocks", p.Route, n)
		}
		if n := strings.Count(body, `data-copy`); n != 0 {
			t.Errorf("%s: %d copy buttons", p.Route, n)
		}
		for _, id := range []string{"install", "docs", "security"} {
			if n := strings.Count(body, `href="/Studio/`+id+`"`); n > 3 {
				t.Errorf("%s: %d links to %s", p.Route, n, id)
			}
		}
	}
	// One install box, one command per system; every other code block may carry its own copy control.
	w := get(t, h, "GET", "/Studio/")
	if body := w.Body.String(); strings.Count(body, `codeblock cmd"`) != 3 || strings.Count(body, `data-copy="install-cmd"`) != 1 || strings.Count(body, "curl -fsSL") != 2 {
		t.Error("overview: install commands or their copy controls missing or repeated")
	}
	for _, route := range []string{"/Studio/install", "/Studio/install/"} {
		redirect := get(t, h, "GET", route)
		if redirect.Code != 301 || redirect.Header().Get("Location") != "/Studio/#install" {
			t.Errorf("install route not merged: %s %d %s", route, redirect.Code, redirect.Header().Get("Location"))
		}
	}
}

// The index on a manual page must point at sections that exist, so a
// renamed section cannot leave a dead link behind.
func TestEveryIndexEntryResolvesToASection(t *testing.T) {
	f := newDownloads(t)
	f.publish(t, "0.9.0", fakeRune("0.9.0"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	link := regexp.MustCompile(`class="index"[\s\S]*?</nav>`)
	entry := regexp.MustCompile(`href="#([^"]+)"`)
	for _, p := range pages {
		body := get(t, h, "GET", "/Studio/"+p.Route).Body.String()
		block := link.FindString(body)
		if block == "" {
			continue
		}
		for _, m := range entry.FindAllStringSubmatch(block, -1) {
			if !strings.Contains(body, `id="`+m[1]+`"`) {
				t.Errorf("%s: index entry #%s has no section", p.Route, m[1])
			}
		}
	}
}

func TestConfigRefusesPublicListeners(t *testing.T) {
	cases := map[string]func(c *Config){
		"all interfaces": func(c *Config) { c.Listen = []string{":3501"} },
		"any address":    func(c *Config) { c.Listen = []string{"0.0.0.0:3501"} },
		"lan in prod": func(c *Config) {
			c.Env, c.Origin, c.Listen = "Prod", "https://atoryn.com", []string{"192.168.18.10:3200"}
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
	// An absolute folder on every system the tests run on (`/tmp` is not one on Windows).
	abs := t.TempDir()
	for name, mutate := range cases {
		c := testConfig(abs)
		mutate(c)
		if err := c.validate(); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
	ok := testConfig(abs)
	ok.Listen = []string{"127.0.0.1:3501", "192.168.18.10:3501"}
	if err := ok.validate(); err != nil {
		t.Errorf("dev on loopback + LAN refused: %v", err)
	}
}
