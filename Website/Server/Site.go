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
	Route string // below the base path, "" = home
	File  string // UI/Pages/<File>.html
	Title string
	Lead  string // meta description
	tmpl  *template.Template
}

var pages = []*page{
	{Route: "", File: "Home", Title: "RuneAgent — the coding agent that stays on your machine",
		Lead: "RuneAgent by Atoryn Studio is a local-first coding agent for the terminal: your code stays on your machine, every change can be undone, and nothing runs without your say."},
	{Route: "security", File: "Security", Title: "Security — RuneAgent",
		Lead: "How RuneAgent keeps you in control: enforced sandboxing, permission modes, undo for every change and no telemetry."},
	{Route: "install", File: "Install", Title: "Install — RuneAgent",
		Lead: "Install RuneAgent on Linux with one command. Every download is signed and verified before it is installed."},
	{Route: "docs", File: "Docs", Title: "Getting started — RuneAgent",
		Lead: "Start with RuneAgent: connect a model provider, choose how much freedom the agent gets, and undo anything."},
	{Route: "changelog", File: "Changelog", Title: "Changelog — RuneAgent",
		Lead: "Every published release of RuneAgent."},
	{Route: "license", File: "License", Title: "License — RuneAgent",
		Lead: "The license under which RuneAgent is provided."},
}

// site is everything rendered from the embedded UI: parsed once at start.
type site struct {
	cfg       *Config
	build     string // hash of every static file: one value for the whole release
	static    map[string][]byte
	pages     map[string]*page
	notFound  *template.Template
	installer *texttemplate.Template
}

var staticTypes = map[string]string{
	".css":         "text/css; charset=utf-8",
	".js":          "text/javascript; charset=utf-8",
	".woff2":       "font/woff2",
	".webp":        "image/webp",
	".png":         "image/png",
	".txt":         "text/plain; charset=utf-8",
	".webmanifest": "application/manifest+json",
}

func newSite(cfg *Config) (*site, error) {
	s := &site{cfg: cfg, static: map[string][]byte{}, pages: map[string]*page{}}
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
	}
	for _, p := range pages {
		t, err := template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, "Pages/Layout.html", "Pages/"+p.File+".html")
		if err != nil {
			return nil, fmt.Errorf("page %s: %w", p.File, err)
		}
		cp := *p
		cp.tmpl = t
		s.pages[p.Route] = &cp
	}
	if s.notFound, err = template.New("Layout.html").Funcs(funcs).ParseFS(ui.Files, "Pages/Layout.html", "Pages/NotFound.html"); err != nil {
		return nil, err
	}
	s.installer, err = texttemplate.New("install.sh").Funcs(texttemplate.FuncMap{"shq": shellQuote}).
		ParseFS(ui.Files, "Install/install.sh")
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
	if route == "" {
		return s.cfg.BasePath + "/"
	}
	return s.cfg.BasePath + "/" + route
}

// pageData is what every page template sees.
type pageData struct {
	Page        *page
	Env         string
	Indexing    bool
	Canonical   string
	Origin      string
	InstallCmd  string
	Catalog     *Catalog
	Latest      *Release
	SiteVersion string
}

func (s *site) data(p *page, cat *Catalog) pageData {
	return pageData{
		Page: p, Env: s.cfg.Env, Indexing: s.cfg.Indexing,
		Canonical:  s.cfg.Origin + s.url(p.Route),
		Origin:     s.cfg.Origin,
		InstallCmd: "curl -fsSL " + s.cfg.Origin + s.cfg.BasePath + "/install.sh | sh",
		Catalog:    cat, Latest: cat.Latest(), SiteVersion: s.build,
	}
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

// installData is what the installer template sees. Every value is quoted
// for the shell by shq; none comes from a request.
type installData struct {
	Origin, Base, Version, KeyPEM, Fingerprint string
	Ready                                      bool
}

func (s *site) renderInstaller(cat *Catalog) ([]byte, error) {
	d := installData{Origin: s.cfg.Origin, Base: s.cfg.BasePath}
	if r := cat.Latest(); r != nil && len(cat.KeyPEM) > 0 {
		d.Ready, d.Version, d.KeyPEM, d.Fingerprint = true, r.Version, strings.TrimSpace(string(cat.KeyPEM)), cat.KeyFingerprint
	}
	var b bytes.Buffer
	if err := s.installer.Execute(&b, d); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

// shellQuote wraps a value in single quotes for POSIX sh.
func shellQuote(v string) string {
	return "'" + strings.ReplaceAll(v, "'", `'"'"'`) + "'"
}
