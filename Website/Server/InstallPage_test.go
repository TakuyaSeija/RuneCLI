package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// The optional export is a test-only browser fixture, never a production route.
func TestExportPublishedBrowserFixture(t *testing.T) {
	dir := os.Getenv("WEBSITE_EXPORT_FIXTURE")
	if dir == "" {
		t.Skip("browser fixture export not requested")
	}
	f := newDownloads(t)
	f.publish(t, "0.9.1", fakeRune("0.9.1"))
	cfg := testConfig(f.dir)
	cfg.Origin = "http://127.0.0.1:18766"
	st := newTestServer(t, cfg)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	for route, file := range map[string]string{"": "home.html", "changelog": "changelog.html", "docs/verification": "verification.html"} {
		body, err := st.site.render(st.site.pages["en"][route], st.catalog.get())
		if err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, file), body, 0o600); err != nil {
			t.Fatal(err)
		}
	}
}

func TestPublishedInstallationShowsOnlyVerifiedArtifacts(t *testing.T) {
	f := newDownloads(t)
	name := f.publish(t, "0.9.1", fakeRune("0.9.1"))
	h := newTestServer(t, testConfig(f.dir)).routes()
	body := get(t, h, "GET", "/Studio/").Body.String()
	for _, required := range []string{"Release 0.9.1", name, `class="download-list"`, `class="hash"`, "/Studio/download/0.9.1/SHA256SUMS.sig", "/Studio/signing-key.pem"} {
		if !strings.Contains(body, required) {
			t.Errorf("published install lacks %q", required)
		}
	}
	if strings.Contains(body, "currently exits without installing anything") || strings.Contains(body, "No public builds yet") {
		t.Fatal("published catalog still appears unavailable")
	}
	if strings.Count(body, "curl -fsSL") != 2 || strings.Count(body, `data-copy="install-cmd"`) != 1 || strings.Count(body, `data-copy="install-cmd-windows"`) != 1 {
		t.Fatal("published install does not hold one command per system")
	}
	// A release without macOS and Windows builds says so under those commands, and never names one.
	for _, want := range []string{"Release 0.9.1 has no signed macOS build.", "Release 0.9.1 has no signed Windows build."} {
		if !strings.Contains(body, want) {
			t.Errorf("missing honest note %q", want)
		}
	}
	verification := get(t, h, "GET", "/Studio/docs/verification").Body.String()
	if !strings.Contains(verification, "Published release-key fingerprint") || !strings.Contains(verification, "independently trusted channel") {
		t.Fatal("key publication omits trust qualification")
	}
}

// The page names only the Linux builds a release really published.
func TestLinuxBuildsNameOnlyPublishedArchives(t *testing.T) {
	for _, c := range []struct {
		files []Artifact
		want  string
	}{
		{[]Artifact{{OS: "linux", Arch: "x86_64", Libc: "gnu"}}, "x86_64 (glibc)"},
		{[]Artifact{{OS: "linux", Arch: "x86_64", Libc: "musl"}, {OS: "linux", Arch: "x86_64", Libc: "gnu"}}, "x86_64 (glibc and static musl)"},
		{[]Artifact{{OS: "linux", Arch: "aarch64", Libc: "gnu"}, {OS: "linux", Arch: "x86_64", Libc: "gnu"}}, "x86_64 (glibc) and arm64 (glibc)"},
	} {
		if got := linuxBuilds(&Release{Files: c.files}); got != c.want {
			t.Errorf("linuxBuilds = %q, want %q", got, c.want)
		}
	}
	f := newDownloads(t)
	f.publish(t, "0.9.1", fakeRune("0.9.1"))
	home := get(t, newTestServer(t, testConfig(f.dir)).routes(), "GET", "/Studio/").Body.String()
	if !strings.Contains(home, "for Linux x86_64 (glibc).") || strings.Contains(home, "arm64 (") {
		t.Fatal("home does not name exactly the published builds")
	}
}

// Each system's note names exactly the builds a release published for it,
// and the download list labels every archive by the machine it is for.
func TestEverySystemNamesOnlyItsPublishedBuilds(t *testing.T) {
	f := newDownloads(t)
	tar := tarball(t, fakeRune("0.9.2"))
	f.publishArchives(t, "0.9.2", map[string][]byte{
		"rune-0.9.2-linux-x86_64-gnu.tar.gz": tar,
		"rune-0.9.2-macos-aarch64.tar.gz":    tar,
		"rune-0.9.2-macos-x86_64.tar.gz":     tar,
		"rune-0.9.2-windows-x86_64.zip":      tar,
	})
	home := get(t, newTestServer(t, testConfig(f.dir)).routes(), "GET", "/Studio/").Body.String()
	for _, want := range []string{
		"for Linux x86_64 (glibc).",
		"for macOS on Apple silicon and Intel. macOS support is in beta.",
		"for Windows x64. Windows support is in beta.",
		"macOS · Apple silicon", "macOS · Intel", "Windows · x64", "Linux · x86_64 · glibc",
	} {
		if !strings.Contains(home, want) {
			t.Errorf("home lacks %q", want)
		}
	}
	if strings.Contains(home, "ARM64") {
		t.Error("home names a Windows ARM64 build that was not published")
	}
	if got := installNote(nil, "windows"); got != "No signed release is published yet. The command currently exits without installing anything. Windows support is in beta." {
		t.Errorf("unpublished note = %q", got)
	}
}
