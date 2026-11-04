package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"html/template"
	"io/fs"
	"path"
	"sort"
	"strings"
	texttemplate "text/template"
	"time"

	ui "atoryn.com/studio/web/UI"
)

// page is one route of the site and the template that renders it.
type page struct {
	Route    string // below the base path, "" = home
	File     string // UI/Pages/<File>.html
	Title    string
	Lead     string // meta description
	Label    string // navigation label for documentation
	Section  string
	Headings []docHeading
	tmpl     *template.Template
}

var pages = append([]*page{
	{Route: "", File: "Home", Title: "RuneAgent — a coding agent for your terminal",
		Lead: "RuneAgent by Atoryn Studio works in your project with your chosen model, visible tool results, configurable permissions and reviewable file changes."},
	{Route: "changelog", File: "Changelog", Title: "Changelog — RuneAgent",
		Lead: "Every published release of RuneAgent."},
	{Route: "license", File: "License", Title: "License — RuneAgent",
		Lead: "The license under which RuneAgent is provided."},
}, documentationPages()...)

// site is everything rendered from the embedded UI: parsed once at start.
type site struct {
	cfg       *Config
	build     string // hash of every static file: one value for the whole release
	static    map[string][]byte
	gzipped   map[string][]byte // text assets compressed once; fonts and images already are
	pages     map[string]*page
	demo      *demoView
	notFound  *template.Template
	installer *texttemplate.Template // install.sh: Linux and macOS
	installPS *texttemplate.Template // install.ps1: Windows
}

var staticTypes = map[string]string{
	".css":         "text/css; charset=utf-8",
	".js":          "text/javascript; charset=utf-8",
	".woff2":       "font/woff2",
	".webp":        "image/webp",
	".avif":        "image/avif",
	".png":         "image/png",
	".txt":         "text/plain; charset=utf-8",
	".webmanifest": "application/manifest+json",
}

func newSite(cfg *Config) (*site, error) {
	s := &site{cfg: cfg, static: map[string][]byte{}, gzipped: map[string][]byte{}, pages: map[string]*page{}}
	var names []string
	err := fs.WalkDir(ui.Files, "Static", func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		if _, ok := staticTypes[path.Ext(p)]; !ok {
			return fmt.Errorf("static file of an unserved type: %s", p)
		}
		b, err := fs.ReadFile(ui.Files, p)
		if err != nil {
			return err
		}
		rel := strings.TrimPrefix(p, "Static/")
		s.static[rel] = b
		if strings.HasPrefix(staticTypes[path.Ext(p)], "text/") || path.Ext(p) == ".webmanifest" {
			s.gzipped[rel] = gzipOf(b)
		}
		names = append(names, rel)
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Strings(names)
	h := sha256.New()
	for _, n := range names {
		fmt.Fprintf(h, "%s\x00%d\x00", n, len(s.static[n]))
		h.Write(s.static[n])
	}
	s.build = hex.EncodeToString(h.Sum(nil))[:16]

	funcs := template.FuncMap{
		"asset": s.asset,
		"url":   s.url,
		"year":  func() int { return time.Now().Year() },
		"mb":    func(n int64) float64 { return float64(n) / (1 << 20) },
		"add":   func(a, b int) int { return a + b },
		// A closed box shows the command's first row, cut, never a wrapped block.
		"firstLine":  firstLine,
		"demoInline": demoInline,
		// The panel prints numbers the way the binary does: `152,211`, `$0.4996`.
		"num":   func(n int64) string { return thousands(n) },
		"money": func(v float64) string { return fmt.Sprintf("$%.4f", v) },
		// What a release really published for each system, so the page never names a build that does not exist.
		"linuxBuilds": linuxBuilds,
		"installNote": installNote,
		"buildLabel":  buildLabel,
		"doneCount": func(tasks []recTask) int {
			done := 0
			for _, task := range tasks {
				if task.Status == "done" {
					done++
				}
			}
			return done
		},
	}
	for _, p := range pages {
		files := []string{"Pages/Layout.html"}
		if p.Section != "" {
			files = append(files, "Pages/Docs.html")
		}
		if p.Route == "" {
			files = append(files, "Pages/Terminal.html", "Pages/TerminalContent.html", "Pages/Install.html")
		}
		files = append(files, "Pages/"+p.File+".html")
		t, err := template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, files...)
		if err != nil {
			return nil, fmt.Errorf("page %s: %w", p.File, err)
		}
		cp := *p
		if p.Section != "" {
			source, err := fs.ReadFile(ui.Files, "Pages/"+p.File+".html")
			if err != nil {
				return nil, err
			}
			cp.Headings, err = documentationHeadings(source)
			if err != nil {
				return nil, fmt.Errorf("page %s: %w", p.File, err)
			}
		}
		cp.tmpl = t
		s.pages[p.Route] = &cp
	}
	if s.notFound, err = template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, "Pages/Layout.html", "Pages/NotFound.html"); err != nil {
		return nil, err
	}
	if s.demo, err = demoOf(documentedVersion); err != nil {
		return nil, err
	}
	s.installer, err = texttemplate.New("install.sh").Funcs(texttemplate.FuncMap{"shq": shellQuote}).
		ParseFS(ui.Files, "Install/install.sh")
	if err != nil {
		return nil, err
	}
	s.installPS, err = texttemplate.New("install.ps1").Funcs(texttemplate.FuncMap{"psq": powershellQuote}).
		ParseFS(ui.Files, "Install/install.ps1")
	if err != nil {
		return nil, err
	}
	return s, nil
}

// asset is the URL of a static file. The build hash in the path makes every
// URL immutable: a new release serves new URLs, never stale bytes.
func (s *site) asset(name string) (string, error) {
	if _, ok := s.static[name]; !ok {
		return "", fmt.Errorf("unknown asset %q", name)
	}
	return s.cfg.BasePath + "/static/" + s.build + "/" + name, nil
}

func (s *site) url(route string) string {
	if route == "install" {
		return s.cfg.BasePath + "/#install"
	}
	if route == "" {
		return s.cfg.BasePath + "/"
	}
	return s.cfg.BasePath + "/" + route
}

// pageData is what every page template sees.
type pageData struct {
	Page       *page
	Env        string
	Indexing   bool
	Canonical  string
	Origin     string
	InstallCmd string
	// Windows has no POSIX shell by default: its installer is a PowerShell script.
	InstallCmdWindows string
	// Parent is where the footer's "Atoryn" leads on this host.
	Parent  string
	Catalog *Catalog
	Latest            *Release
	SiteVersion       string
	DocGroups         []docGroup
	DocVersion        string
	Previous, Next    *page
	// The recorded CLI session the home page illustrates. Built once at start:
	// it is read-only data, and rendering it per request would only repeat work.
	Demo *demoView
}

func (s *site) data(p *page, cat *Catalog) pageData {
	d := pageData{
		Page: p, Env: s.cfg.Env, Indexing: s.cfg.Indexing,
		Canonical:         s.cfg.Origin + s.url(p.Route),
		Origin:            s.cfg.Origin,
		InstallCmd:        "curl -fsSL " + s.cfg.Origin + s.cfg.BasePath + "/install.sh | sh",
		InstallCmdWindows: "irm " + s.cfg.Origin + s.cfg.BasePath + "/install.ps1 | iex",
		Parent:            s.cfg.parent(),
		Catalog:           cat, Latest: cat.Latest(), SiteVersion: s.build,
		DocGroups: docGroups, DocVersion: documentedVersion,
		Demo: s.demo,
	}
	if p.Section != "" {
		var previous *page
		found := false
		for _, candidate := range pages {
			if candidate.Section == "" {
				continue
			}
			if found {
				d.Next = candidate
				break
			}
			if candidate.Route == p.Route {
				d.Previous, found = previous, true
			}
			previous = candidate
		}
	}
	return d
}

func (s *site) render(p *page, cat *Catalog) ([]byte, error) {
	var b bytes.Buffer
	if err := p.tmpl.Execute(&b, s.data(p, cat)); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

func (s *site) renderNotFound(cat *Catalog) []byte {
	var b bytes.Buffer
	p := &page{Route: "-", Title: "Not found — RuneAgent", Lead: "This page does not exist."}
	if err := s.notFound.Execute(&b, s.data(p, cat)); err != nil {
		return []byte("Not found\n")
	}
	return b.Bytes()
}

// installData is what the installer templates see. Every value is quoted
// for its shell (shq, psq); none comes from a request. KeyX and KeyY are the
// release key's point in hex, the form Windows' own cryptography imports.
type installData struct {
	Origin, Base, Version, KeyPEM, Fingerprint string
	KeyX, KeyY                                 string
	Ready                                      bool
}

func (s *site) installerData(cat *Catalog) installData {
	d := installData{Origin: s.cfg.Origin, Base: s.cfg.BasePath}
	if r := cat.Latest(); r != nil && len(cat.KeyPEM) > 0 {
		if pub, _, err := parsePublicKey(cat.KeyPEM); err == nil {
			d.Ready, d.Version, d.KeyPEM, d.Fingerprint = true, r.Version, strings.TrimSpace(string(cat.KeyPEM)), cat.KeyFingerprint
			d.KeyX, d.KeyY = fmt.Sprintf("%064x", pub.X), fmt.Sprintf("%064x", pub.Y)
		}
	}
	return d
}

func (s *site) renderInstaller(cat *Catalog) ([]byte, error) {
	var b bytes.Buffer
	if err := s.installer.Execute(&b, s.installerData(cat)); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

func (s *site) renderInstallerPS(cat *Catalog) ([]byte, error) {
	var b bytes.Buffer
	if err := s.installPS.Execute(&b, s.installerData(cat)); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

// buildLabel says which machine an archive is for, the way the download list
// and the changelog name it.
func buildLabel(a Artifact) string {
	switch a.OS {
	case "macos":
		return "macOS · " + map[string]string{"aarch64": "Apple silicon", "x86_64": "Intel"}[a.Arch]
	case "windows":
		return "Windows · " + map[string]string{"x86_64": "x64", "aarch64": "ARM64"}[a.Arch]
	}
	libc := map[string]string{"gnu": "glibc", "musl": "static musl"}[a.Libc]
	return "Linux · " + a.Arch + " · " + libc
}

// linuxBuilds names a release's Linux archives the way a person reads them:
// "x86_64 (glibc and static musl)", arm64 where one was published.
func linuxBuilds(r *Release) string {
	var parts []string
	for _, arch := range []string{"x86_64", "aarch64"} {
		var libcs []string
		for _, libc := range []string{"gnu", "musl"} {
			for _, a := range r.Files {
				if a.OS == "linux" && a.Arch == arch && a.Libc == libc {
					libcs = append(libcs, map[string]string{"gnu": "glibc", "musl": "static musl"}[libc])
					break
				}
			}
		}
		if len(libcs) == 0 {
			continue
		}
		name := map[string]string{"x86_64": "x86_64", "aarch64": "arm64"}[arch]
		parts = append(parts, name+" ("+strings.Join(libcs, " and ")+")")
	}
	return strings.Join(parts, " and ")
}

// systemNames are the public names of the systems a release can carry builds for.
var systemNames = map[string]string{"linux": "Linux", "macos": "macOS", "windows": "Windows"}

// platformBuilds names a release's builds for one system the way a person
// reads them, completing "for <system> …": Linux names processor and C
// library, macOS the kind of Mac, Windows the processor. Empty: none.
func platformBuilds(r *Release, system string) string {
	if system == "linux" {
		return linuxBuilds(r)
	}
	names := map[string]map[string]string{
		"macos":   {"aarch64": "Apple silicon", "x86_64": "Intel"},
		"windows": {"x86_64": "x64", "aarch64": "ARM64"},
	}[system]
	var parts []string
	for _, arch := range []string{"aarch64", "x86_64"} {
		for _, a := range r.Files {
			if a.OS == system && a.Arch == arch {
				parts = append(parts, names[arch])
				break
			}
		}
	}
	if len(parts) == 0 {
		return ""
	}
	if system == "windows" {
		for i, j := 0, len(parts)-1; i < j; i, j = i+1, j-1 {
			parts[i], parts[j] = parts[j], parts[i] // x64 first: the common machine
		}
		return strings.Join(parts, " and ")
	}
	return "on " + strings.Join(parts, " and ")
}

// installNote is the line under a system's install command: which signed
// build that command installs today, or that it installs nothing yet. It
// never names a build the catalog does not hold.
func installNote(r *Release, system string) string {
	name := systemNames[system]
	beta := ""
	if system != "linux" {
		beta = " " + name + " support is in beta."
	}
	if r == nil {
		return "No signed release is published yet. The command currently exits without installing anything." + beta
	}
	builds := platformBuilds(r, system)
	if builds == "" {
		return "Release " + r.Version + " has no signed " + name + " build. The command exits without installing anything." + beta
	}
	return "Release " + r.Version + ", published " + r.Date + ", for " + name + " " + builds + "." + beta
}

// powershellQuote wraps a value in single quotes for PowerShell, where a
// quote inside is written twice and nothing else is special.
func powershellQuote(v string) string {
	return "'" + strings.ReplaceAll(v, "'", "''") + "'"
}

// shellQuote wraps a value in single quotes for POSIX sh.
func shellQuote(v string) string {
	return "'" + strings.ReplaceAll(v, "'", `'"'"'`) + "'"
}
