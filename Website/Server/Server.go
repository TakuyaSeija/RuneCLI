package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"path"
	"strconv"
	"strings"
	"sync"
	"time"
)

type server struct {
	cfg     *Config
	site    *site
	catalog *catalogStore
	dl      *os.Root

	mu       sync.Mutex
	cacheFor *Catalog // the catalog the cached renders were made from
	cache    map[string][]byte
}

func newServer(cfg *Config, st *site, cat *catalogStore, dl *os.Root) *server {
	return &server{cfg: cfg, site: st, catalog: cat, dl: dl}
}

// rendered returns a page/installer render, rebuilt only when the catalog
// changed: a request never runs a template on its own.
func (s *server) rendered(key string, build func(*Catalog) ([]byte, error)) ([]byte, error) {
	cat := s.catalog.get()
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.cacheFor != cat {
		s.cacheFor, s.cache = cat, map[string][]byte{}
	}
	if b, ok := s.cache[key]; ok {
		return b, nil
	}
	b, err := build(cat)
	if err == nil {
		s.cache[key] = b
	}
	return b, err
}

func (s *server) routes() http.Handler {
	mux := http.NewServeMux()
	base := s.cfg.BasePath
	mux.HandleFunc("GET "+base+"/{$}", s.page(""))
	for _, p := range pages {
		if p.Route == "" {
			continue
		}
		mux.HandleFunc("GET "+base+"/"+p.Route, s.page(p.Route))
		mux.HandleFunc("GET "+base+"/"+p.Route+"/{$}", redirectTo(base+"/"+p.Route))
	}
	mux.HandleFunc("GET "+base, redirectTo(base+"/"))
	mux.HandleFunc("GET "+base+"/install.sh", s.install)
	mux.HandleFunc("GET "+base+"/releases.json", s.releases)
	mux.HandleFunc("GET "+base+"/signing-key.pem", s.signingKey)
	mux.HandleFunc("GET "+base+"/download/{version}/{file}", s.download)
	mux.HandleFunc("GET "+base+"/static/{build}/{file...}", s.static)
	mux.HandleFunc("GET "+base+"/sitemap.xml", s.sitemap)
	mux.HandleFunc("/", s.notFound)
	return s.guard(mux)
}

func redirectTo(target string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, target, http.StatusMovedPermanently)
	}
}

func (s *server) page(route string) http.HandlerFunc {
	p := s.site.pages[route]
	return func(w http.ResponseWriter, r *http.Request) {
		b, err := s.rendered("page:"+route, func(c *Catalog) ([]byte, error) { return s.site.render(p, c) })
		if err != nil {
			log.Printf("render %s: %v", p.File, err)
			http.Error(w, "Internal error", http.StatusInternalServerError)
			return
		}
		writeBody(w, r, "text/html; charset=utf-8", "no-cache", b)
	}
}

func (s *server) notFound(w http.ResponseWriter, r *http.Request) {
	b, _ := s.rendered("notfound", func(c *Catalog) ([]byte, error) { return s.site.renderNotFound(c), nil })
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-cache")
	w.WriteHeader(http.StatusNotFound)
	if r.Method != http.MethodHead {
		w.Write(b)
	}
}

func (s *server) install(w http.ResponseWriter, r *http.Request) {
	b, err := s.rendered("install", s.site.renderInstaller)
	if err != nil {
		log.Printf("render installer: %v", err)
		http.Error(w, "Internal error", http.StatusInternalServerError)
		return
	}
	writeBody(w, r, "text/plain; charset=utf-8", "no-cache", b)
}

type releaseJSON struct {
	Latest      string     `json:"latest,omitempty"`
	Fingerprint string     `json:"signingKeyFingerprint,omitempty"`
	Releases    []*Release `json:"releases"`
}

func (s *server) releases(w http.ResponseWriter, r *http.Request) {
	b, _ := s.rendered("releases", func(c *Catalog) ([]byte, error) {
		out := releaseJSON{Fingerprint: c.KeyFingerprint, Releases: c.Releases}
		if out.Releases == nil {
			out.Releases = []*Release{}
		}
		if l := c.Latest(); l != nil {
			out.Latest = l.Version
		}
		return json.MarshalIndent(out, "", "  ")
	})
	writeBody(w, r, "application/json", "no-cache", b)
}

func (s *server) signingKey(w http.ResponseWriter, r *http.Request) {
	cat := s.catalog.get()
	if len(cat.KeyPEM) == 0 {
		s.notFound(w, r)
		return
	}
	writeBody(w, r, "text/plain; charset=utf-8", "no-cache", cat.KeyPEM)
}

func (s *server) sitemap(w http.ResponseWriter, r *http.Request) {
	if !s.cfg.Indexing {
		s.notFound(w, r)
		return
	}
	var b bytes.Buffer
	b.WriteString(`<?xml version="1.0" encoding="UTF-8"?>` + "\n" + `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">` + "\n")
	for _, p := range pages {
		fmt.Fprintf(&b, "  <url><loc>%s%s</loc></url>\n", s.cfg.Origin, s.site.url(p.Route))
	}
	b.WriteString("</urlset>\n")
	writeBody(w, r, "application/xml; charset=utf-8", "no-cache", b.Bytes())
}

// download serves only files the verified catalog lists. The names never
// reach the filesystem unless they matched; os.Root keeps even that inside
// the downloads folder.
func (s *server) download(w http.ResponseWriter, r *http.Request) {
	version, name := r.PathValue("version"), r.PathValue("file")
	rel := s.catalog.get().Find(version)
	if rel == nil || !rel.Serves(name) {
		s.notFound(w, r)
		return
	}
	f, err := s.dl.Open(version + "/" + name)
	if err != nil {
		s.notFound(w, r)
		return
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil || !fi.Mode().IsRegular() {
		s.notFound(w, r)
		return
	}
	ctype := "application/octet-stream"
	switch {
	case strings.HasSuffix(name, ".tar.gz"):
		ctype = "application/gzip"
	case name == sumsFile:
		ctype = "text/plain; charset=utf-8"
	}
	h := w.Header()
	h.Set("Content-Type", ctype)
	h.Set("Content-Disposition", `attachment; filename="`+name+`"`)
	h.Set("Cache-Control", "public, max-age=31536000, immutable")
	http.ServeContent(w, r, "", fi.ModTime(), f)
}

func (s *server) static(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("file")
	b, ok := s.site.static[name]
	if r.PathValue("build") != s.site.build || !ok {
		s.notFound(w, r)
		return
	}
	writeBody(w, r, staticTypes[path.Ext(name)], "public, max-age=31536000, immutable", b)
}

func writeBody(w http.ResponseWriter, r *http.Request, ctype, cache string, b []byte) {
	h := w.Header()
	h.Set("Content-Type", ctype)
	h.Set("Cache-Control", cache)
	h.Set("Content-Length", strconv.Itoa(len(b)))
	w.WriteHeader(http.StatusOK)
	if r.Method != http.MethodHead {
		w.Write(b)
	}
}

// guard runs before any route: recovery, the access log, the security
// headers, then the request rules. Whatever a handler forgets, this holds.
func (s *server) guard(next http.Handler) http.Handler {
	csp := "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; " +
		"connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; " +
		"require-trusted-types-for 'script'; trusted-types 'none'"
	if s.cfg.Env == "Prod" {
		csp += "; upgrade-insecure-requests"
	}
	hosts := map[string]bool{}
	for _, h := range s.cfg.Hosts {
		hosts[strings.ToLower(h)] = true
	}
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		defer func() {
			if v := recover(); v != nil {
				log.Printf("panic serving %s: %v", r.URL.EscapedPath(), v)
				if !rec.wrote {
					http.Error(rec, "Internal error", http.StatusInternalServerError)
				}
			}
			p := r.URL.EscapedPath()
			if len(p) > 200 {
				p = p[:200] + "…"
			}
			log.Printf("%s %s %d %d %s", r.Method, p, rec.status, rec.bytes, time.Since(start).Round(time.Millisecond))
		}()

		h := rec.Header()
		h.Set("Content-Security-Policy", csp)
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("X-Frame-Options", "DENY")
		h.Set("Referrer-Policy", "no-referrer")
		h.Set("Cross-Origin-Opener-Policy", "same-origin")
		h.Set("Cross-Origin-Resource-Policy", "same-origin")
		h.Set("Cross-Origin-Embedder-Policy", "require-corp")
		h.Set("X-Permitted-Cross-Domain-Policies", "none")
		h.Set("Permissions-Policy", "accelerometer=(), browsing-topics=(), camera=(), display-capture=(), geolocation=(), "+
			"gyroscope=(), hid=(), magnetometer=(), microphone=(), payment=(), serial=(), usb=()")
		if s.cfg.Env == "Prod" {
			h.Set("Strict-Transport-Security", "max-age=31536000")
		}
		if !s.cfg.Indexing {
			h.Set("X-Robots-Tag", "noindex, nofollow")
		}

		switch {
		case r.Method != http.MethodGet && r.Method != http.MethodHead:
			h.Set("Allow", "GET, HEAD")
			http.Error(rec, "Method not allowed", http.StatusMethodNotAllowed)
		case r.ContentLength > 0 || len(r.TransferEncoding) > 0:
			http.Error(rec, "Bad request", http.StatusBadRequest)
		case !hosts[strings.ToLower(r.Host)]:
			http.Error(rec, "Misdirected request", http.StatusMisdirectedRequest)
		default:
			next.ServeHTTP(rec, r)
		}
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
	bytes  int64
	wrote  bool
}

func (r *statusRecorder) WriteHeader(code int) {
	if !r.wrote {
		r.status, r.wrote = code, true
	}
	r.ResponseWriter.WriteHeader(code)
}

func (r *statusRecorder) Write(b []byte) (int, error) {
	r.wrote = true
	n, err := r.ResponseWriter.Write(b)
	r.bytes += int64(n)
	return n, err
}
