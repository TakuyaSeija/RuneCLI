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
	Locale   *locale
	tmpl     *template.Template
}

var pages = append([]*page{
	{Route: "", File: "Home", Title: "RuneAgent | a coding agent for your terminal",
		Lead: "RuneAgent by Atoryn Studio works in your project with your chosen model, visible tool results, configurable permissions and reviewable file changes."},
	{Route: "changelog", File: "Changelog", Title: "Changelog | RuneAgent",
		Lead: "Every published release of RuneAgent."},
	{Route: "license", File: "License", Title: "License | RuneAgent",
		Lead: "The license under which RuneAgent is provided."},
}, documentationPages()...)

// site is everything rendered from the embedded UI: parsed once at start.
type site struct {
	cfg       *Config
	build     string // hash of every static file: one value for the whole release
	static    map[string][]byte
	gzipped   map[string][]byte           // text assets compressed once; fonts and images already are
	pages     map[string]map[string]*page // locale code → route → page
	lists     map[string][]*page          // locale code → pages in catalogue order
	groups    map[string][]docGroup       // locale code → the docs index in its words
	demos     map[string]*demoView        // locale code → the replay, chapter names in its words
	demo      *demoView
	notFound  map[string]*template.Template
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
	s := &site{cfg: cfg, static: map[string][]byte{}, gzipped: map[string][]byte{},
		pages: map[string]map[string]*page{}, lists: map[string][]*page{}, groups: map[string][]docGroup{},
		demos: map[string]*demoView{}, notFound: map[string]*template.Template{}}
	if err := loadLocaleTexts(); err != nil {
		return nil, err
	}
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

	if s.demo, err = demoOf(documentedVersion); err != nil {
		return nil, err
	}
	for _, l := range locales {
		if err := s.parseLocale(l); err != nil {
			return nil, err
		}
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

// parseLocale parses every page of one locale from its own copy of the
// templates; its url and installNote speak that locale.
func (s *site) parseLocale(l *locale) error {
	funcs := template.FuncMap{
		"asset": s.asset,
		"url":   func(route string) string { return s.urlIn(l, route) },
		"year":  func() int { return time.Now().Year() },
		"mb":    func(n int64) float64 { return float64(n) / (1 << 20) },
		"add":   func(a, b int) int { return a + b },
		"meta":  metaText,
		// A closed box shows the command's first row, cut, never a wrapped block.
		"firstLine":  firstLine,
		"demoInline": demoInline,
		// The panel prints numbers the way the binary does: `152,211`, `$0.4996`.
		"num":   func(n int64) string { return thousands(n) },
		"money": func(v float64) string { return fmt.Sprintf("$%.4f", v) },
		// What a release really published for each system, so the page never names a build that does not exist.
		"linuxBuilds": linuxBuilds,
		"installNote": l.installNote,
		"buildLabel":  buildLabel,
		// The one-line command that installs one named version: the site lists
		// every release, and an older one installs by number.
		"installFor": func(version string) string {
			return "curl -fsSL " + s.cfg.Origin + s.cfg.BasePath + "/install.sh | RUNE_VERSION=" + version + " sh"
		},
		"minorOf": minorOf,
		"seriesTitle": func(minor string) string {
			if t, ok := minorSeriesTitles[minor]; ok {
				return t
			}
			return "Series " + minor
		},
		"missingPlatforms": missingPlatforms,
		"prevRelease": func(releases []*releaseView, i int) *releaseView {
			if i+1 < len(releases) {
				return releases[i+1]
			}
			return nil
		},
		"nextRelease": func(releases []*releaseView, i int) *releaseView {
			if i-1 >= 0 {
				return releases[i-1]
			}
			return nil
		},
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
	list, groups := l.localizedPages()
	s.pages[l.Code], s.groups[l.Code] = map[string]*page{}, groups
	for _, p := range list {
		files := []string{l.pageFile("Layout.html")}
		if p.Section != "" {
			files = append(files, l.pageFile("Docs.html"))
		}
		if p.Route == "" {
			files = append(files, l.pageFile("Terminal.html"), l.pageFile("TerminalContent.html"), l.pageFile("Install.html"))
		}
		files = append(files, l.pageFile(p.File+".html"))
		t, err := template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, files...)
		if err != nil {
			return fmt.Errorf("%s page %s: %w", l.Code, p.File, err)
		}
		p.Locale = l
		if p.Section != "" {
			source, err := fs.ReadFile(ui.Files, l.pageFile(p.File+".html"))
			if err != nil {
				return err
			}
			if p.Headings, err = documentationHeadings(source); err != nil {
				return fmt.Errorf("%s page %s: %w", l.Code, p.File, err)
			}
		}
		p.tmpl = t
		s.pages[l.Code][p.Route] = p
		s.lists[l.Code] = append(s.lists[l.Code], p)
	}
	nf, err := template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, l.pageFile("Layout.html"), l.pageFile("NotFound.html"))
	if err != nil {
		return fmt.Errorf("%s not found page: %w", l.Code, err)
	}
	s.notFound[l.Code] = nf
	demo := *s.demo
	if l != english {
		demo.Chapters = append([]demoChapter(nil), s.demo.Chapters...)
		for i := range demo.Chapters {
			demo.Chapters[i].Name = l.text.Chapters[demo.Chapters[i].Name]
		}
	}
	s.demos[l.Code] = &demo
	return nil
}

// asset is the URL of a static file. The build hash in the path makes every
// URL immutable: a new release serves new URLs, never stale bytes.
func (s *site) asset(name string) (string, error) {
	if _, ok := s.static[name]; !ok {
		return "", fmt.Errorf("unknown asset %q", name)
	}
	return s.cfg.BasePath + "/static/" + s.build + "/" + name, nil
}

func (s *site) url(route string) string { return s.urlIn(english, route) }

// urlIn is a route's address in one locale: English at the base path, every
// translation below its own code.
func (s *site) urlIn(l *locale, route string) string {
	base := s.cfg.BasePath + l.prefix()
	if route == "install" {
		return base + "/#install"
	}
	if route == "" {
		return base + "/"
	}
	return base + "/" + route
}

// pageData is what every page template sees.
// releaseView is one published build with the changelog's words for a language:
// the release itself (date, files, version) plus the recorded changes, which the
// site owns so a version already published can still gain its notes.
type releaseView struct {
	*Release
	Notes []string
}

type historyGroupView struct {
	Minor    string
	Releases []*releaseView
}

// notesFor is the page's language's words for a version, or English's: a reader
// never sees a version's changes in a language the site does not hold, because
// the catalogue is checked at startup for exactly the same versions and lines.
func (s *site) notesFor(l *locale, version string) []string {
	if l != nil && l.text != nil {
		if lines, ok := l.text.Notes[version]; ok {
			return lines
		}
	}
	return englishNotes[version]
}

func (s *site) releaseViews(cat *Catalog, l *locale) []*releaseView {
	if cat == nil {
		return nil
	}
	out := make([]*releaseView, 0, len(cat.Releases))
	for _, r := range cat.Releases {
		out = append(out, &releaseView{Release: r, Notes: s.notesFor(l, r.Version)})
	}
	return out
}

func (s *site) historyViews(cat *Catalog, l *locale) []*historyGroupView {
	var out []*historyGroupView
	for _, group := range cat.History() {
		view := &historyGroupView{Minor: group.Minor}
		for _, r := range group.Releases {
			view.Releases = append(view.Releases, &releaseView{Release: r, Notes: s.notesFor(l, r.Version)})
		}
		out = append(out, view)
	}
	return out
}

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
	Parent         string
	Catalog        *Catalog
	Latest         *Release
	// Releases are the published builds with the changelog's words for this
	// page's language, and History is the same list grouped by minor version for
	// the sidebar.
	Releases       []*releaseView
	History        []*historyGroupView
	SiteVersion    string
	DocGroups      []docGroup
	DocVersion     string
	Previous, Next *page
	// Locale is the language of this page; Locales lists every language,
	// each pointing at this same page in it (its front page from a 404).
	Locale  *locale
	Locales []localeLink
	// The recorded CLI session the home page illustrates. Built once at start:
	// it is read-only data, and rendering it per request would only repeat work.
	Demo *demoView
}

func (s *site) data(p *page, cat *Catalog) pageData {
	l := p.Locale
	d := pageData{
		Page: p, Env: s.cfg.Env, Indexing: s.cfg.Indexing,
		Canonical:         s.cfg.Origin + s.urlIn(l, p.Route),
		Origin:            s.cfg.Origin,
		InstallCmd:        "curl -fsSL " + s.cfg.Origin + s.cfg.BasePath + "/install.sh | sh",
		InstallCmdWindows: "irm " + s.cfg.Origin + s.cfg.BasePath + "/install.ps1 | iex",
		Parent:            s.cfg.parent(),
		Catalog:           cat, Latest: cat.Latest(), SiteVersion: s.build,
		Releases: s.releaseViews(cat, l), History: s.historyViews(cat, l),
		DocGroups: s.groups[l.Code], DocVersion: documentedVersion,
		Demo:   s.demos[l.Code],
		Locale: l,
	}
	for _, other := range locales {
		route := p.Route
		if _, ok := s.pages[other.Code][route]; !ok {
			route = "" // a 404 has no page of its own in another language
		}
		d.Locales = append(d.Locales, localeLink{locale: other, Href: s.urlIn(other, route), Current: other == l})
	}
	if p.Section != "" {
		var previous *page
		found := false
		for _, candidate := range s.lists[l.Code] {
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

// metaText is a lead as the head's description tags carry it. The browser tab,
// search results and link previews show no em dash (owner, 2026-11-04); the
// article itself keeps the lead as written.
var metaDashes = strings.NewReplacer(" — ", ": ", "——", "：", "—", ": ")

func metaText(s string) string { return metaDashes.Replace(s) }

func (s *site) renderNotFound(l *locale, cat *Catalog) []byte {
	var b bytes.Buffer
	p := &page{Route: "-", Title: "Not found | RuneAgent", Lead: "This page does not exist.", Locale: l}
	if l != english {
		t := l.text.Pages["notfound"]
		p.Title, p.Lead = t.Title, t.Lead
	}
	if err := s.notFound[l.Code].Execute(&b, s.data(p, cat)); err != nil {
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

type missingPlatform struct {
	Name     string
	Platform string
}

var minorSeriesTitles = map[string]string{
	"0.11": "Series 0.11 · Streaming & Tools",
	"0.10": "Series 0.10 · Supervision & Guards",
	"0.9":  "Series 0.9 · Shell & Terminal",
	"0.8":  "Series 0.8 · Presets & Compaction",
	"0.7":  "Series 0.7 · Distros & Foundation",
}

func minorOf(version string) string {
	parts := strings.SplitN(version, ".", 3)
	if len(parts) >= 2 {
		return parts[0] + "." + parts[1]
	}
	return version
}

func missingPlatforms(r *releaseView) []missingPlatform {
	if r == nil {
		return nil
	}
	var hasLinux, hasMacOS, hasWindows bool
	for _, f := range r.Files {
		switch f.OS {
		case "linux":
			hasLinux = true
		case "macos":
			hasMacOS = true
		case "windows":
			hasWindows = true
		}
	}
	var missing []missingPlatform
	if !hasMacOS {
		missing = append(missing, missingPlatform{
			Name:     "Rune for macOS",
			Platform: "macOS · Apple silicon / Intel",
		})
	}
	if !hasWindows {
		missing = append(missing, missingPlatform{
			Name:     "Rune for Windows",
			Platform: "Windows · x64 / ARM64",
		})
	}
	if !hasLinux {
		missing = append(missing, missingPlatform{
			Name:     "Rune for Linux",
			Platform: "Linux · x86_64 / aarch64",
		})
	}
	return missing
}

// buildLabel says which machine an archive is for, the way the download list
// and the changelog name it.
func buildLabel(a Artifact) string {
	switch a.OS {
	case "macos":
		label := "macOS · " + map[string]string{"aarch64": "Apple silicon", "x86_64": "Intel"}[a.Arch]
		if a.Package {
			return label + " · offline package"
		}
		return label
	case "windows":
		label := "Windows · " + map[string]string{"x86_64": "x64", "aarch64": "ARM64"}[a.Arch]
		if a.Package {
			return label + " · offline package"
		}
		return label
	}
	libc := map[string]string{"gnu": "glibc", "musl": "static musl"}[a.Libc]
	label := "Linux · " + a.Arch + " · " + libc
	if a.Package {
		return label + " · offline package"
	}
	return label
}

// linuxBuilds names a release's Linux archives the way a person reads them:
// "x86_64 (glibc and static musl)", arm64 where one was published.
func linuxBuilds(r *Release) string {
	return strings.Join(linuxParts(r, "and"), " and ")
}

// linuxParts are linuxBuilds' pieces, joined inside by the given word.
func linuxParts(r *Release, and string) []string {
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
		parts = append(parts, name+" ("+strings.Join(libcs, " "+and+" ")+")")
	}
	return parts
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
	parts := buildParts(r, system)
	if len(parts) == 0 {
		return ""
	}
	if system == "windows" {
		return strings.Join(parts, " and ")
	}
	return "on " + strings.Join(parts, " and ")
}

// buildParts names each build one system has in a release, x64 first on
// Windows (the common machine); Linux's parts carry their C libraries.
func buildParts(r *Release, system string) []string {
	if system == "linux" {
		return linuxParts(r, "and")
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
	if system == "windows" {
		for i, j := 0, len(parts)-1; i < j; i, j = i+1, j-1 {
			parts[i], parts[j] = parts[j], parts[i] // x64 first: the common machine
		}
	}
	return parts
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
