package main

import (
	"encoding/json"
	"fmt"
	"net"
	"net/netip"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

// Config is the host-specific part of a deployment. It lives outside the
// repository (Ops/Web/<Env>.json) and is copied into each release.
type Config struct {
	Env       string   `json:"env"`       // "Prod" | "Dev"
	BasePath  string   `json:"basePath"`  // "/Studio"
	Origin    string   `json:"origin"`    // scheme://host the public sees, no trailing slash
	Hosts     []string `json:"hosts"`     // accepted Host header values (host[:port])
	Listen    []string `json:"listen"`    // site listeners
	Health    string   `json:"health"`    // health listener, loopback only
	Downloads string   `json:"downloads"` // published releases, read-only
	Indexing  bool     `json:"indexing"`  // let search engines index the pages
	// ParentURL is where the footer's "Atoryn" leads. Production serves the
	// family from one origin, so it is "/" there; a dev site lives on its own
	// port, where "/" is this site, so dev names the landing's address.
	ParentURL string `json:"parentUrl"`
}

var basePathRe = regexp.MustCompile(`^/[A-Za-z][A-Za-z0-9]{0,31}$`)

func loadConfig(path string) (*Config, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config: %w", err)
	}
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.DisallowUnknownFields()
	var c Config
	if err := dec.Decode(&c); err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}
	return &c, nil
}

func (c *Config) validate() error {
	if c.Env != "Prod" && c.Env != "Dev" {
		return fmt.Errorf("env must be Prod or Dev")
	}
	if !basePathRe.MatchString(c.BasePath) {
		return fmt.Errorf("basePath must look like /Studio")
	}
	u, err := url.Parse(c.Origin)
	if err != nil || u.Host == "" || u.Path != "" || u.RawQuery != "" || u.User != nil || u.Fragment != "" {
		return fmt.Errorf("origin must be scheme://host only")
	}
	if c.Env == "Prod" && u.Scheme != "https" {
		return fmt.Errorf("prod origin must be https")
	}
	if u.Scheme != "https" && u.Scheme != "http" {
		return fmt.Errorf("origin scheme must be http(s)")
	}
	if len(c.Hosts) == 0 {
		return fmt.Errorf("hosts must list the accepted Host values")
	}
	for _, h := range c.Hosts {
		if h == "" || strings.ContainsAny(h, "/ \t@") {
			return fmt.Errorf("bad host %q", h)
		}
	}
	if len(c.Listen) == 0 {
		return fmt.Errorf("listen is empty")
	}
	for _, l := range c.Listen {
		if err := checkListen(l, c.Env == "Dev"); err != nil {
			return err
		}
	}
	if err := checkListen(c.Health, false); err != nil {
		return fmt.Errorf("health: %w", err)
	}
	if !filepath.IsAbs(c.Downloads) {
		return fmt.Errorf("downloads must be absolute")
	}
	if c.Env == "Dev" && c.Indexing {
		return fmt.Errorf("dev is never indexed")
	}
	if c.ParentURL != "" && !strings.HasPrefix(c.ParentURL, "/") {
		p, err := url.Parse(c.ParentURL)
		if err != nil || p.Host == "" || (p.Scheme != "http" && p.Scheme != "https") || p.RawQuery != "" || p.Fragment != "" || p.User != nil {
			return fmt.Errorf("parentUrl must be an absolute http(s) address or a path starting with /")
		}
	}
	return nil
}

// parent is the footer's "Atoryn" destination: the same origin in production,
// the landing's own address where this site is served from its own port.
func (c *Config) parent() string {
	if c.ParentURL == "" {
		return "/"
	}
	return c.ParentURL
}

// parentFor is the footer's "Atoryn" destination for a specific locale.
func (c *Config) parentFor(l *locale) string {
	base := c.parent()
	if l == nil || l.Code == "en" {
		return base
	}
	if strings.HasSuffix(base, "/") {
		return base + l.Code + "/"
	}
	return base + "/" + l.Code + "/"
}

// devHost returns the host/IP of the dev site without port.
func (c *Config) devHost() string {
	if u, err := url.Parse(c.Origin); err == nil && u.Hostname() != "" {
		return u.Hostname()
	}
	if len(c.Hosts) > 0 {
		h := c.Hosts[0]
		if host, _, err := net.SplitHostPort(h); err == nil && host != "" {
			return host
		}
		return h
	}
	return "192.168.18.10"
}

// checkListen accepts loopback only — and, for DEV, a private LAN address
// (RFC 1918), never one routed from the internet. The public reaches PROD
// exclusively through nginx and the tunnel.
func checkListen(addr string, allowLAN bool) error {
	ap, err := netip.ParseAddrPort(addr)
	if err != nil {
		host, _, serr := net.SplitHostPort(addr)
		if serr == nil && host == "" {
			return fmt.Errorf("listen %q binds every interface", addr)
		}
		return fmt.Errorf("listen %q: %v", addr, err)
	}
	ip := ap.Addr().Unmap()
	if ip.IsLoopback() || allowLAN && ip.Is4() && ip.IsPrivate() {
		return nil
	}
	return fmt.Errorf("listen %q is not loopback", addr)
}
