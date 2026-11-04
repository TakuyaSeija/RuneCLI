package main

import (
	"bufio"
	"bytes"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"io/fs"
	"log"
	"os"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// A release folder, as PublishRune.sh writes it. It lives outside the deploy
// tree, at ~/.data/Atoryn/Studio/Releases (DEV: ~/.data/Atoryn/StudioDev/
// Releases), so a release survives every deploy; the unit reads it read-only
// at /srv/studio/downloads.
//
//	SigningKey.pem                        the public key (PEM, ECDSA P-256)
//	<X.Y.Z>/release.json                  date, notes, files with platform
//	<X.Y.Z>/rune-<v>-linux-<arch>-<libc>.tar.gz
//	<X.Y.Z>/rune-<v>-macos-<arch>.tar.gz
//	<X.Y.Z>/rune-<v>-windows-<arch>.zip
//	<X.Y.Z>/SHA256SUMS           every file above, release.json too
//	<X.Y.Z>/SHA256SUMS.sig       ECDSA P-256 signature (DER) of SHA256SUMS's SHA-256
//
// Nothing is listed or served unless the signature verifies with the
// published key AND every file has exactly the signed size and hash. A folder
// someone tampered with simply disappears from the site.

var (
	versionRe = regexp.MustCompile(`^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$`)
	// An archive's name says its system and processor, and on Linux its C
	// library; macOS and Windows archives have none (Libc stays empty).
	artifactRe = regexp.MustCompile(`^rune-([0-9.]+)-(?:(linux)-(x86_64|aarch64)-(gnu|musl)\.tar\.gz|(macos)-(x86_64|aarch64)\.tar\.gz|(windows)-(x86_64|aarch64)\.zip)$`)
	sumLineRe  = regexp.MustCompile(`^([0-9a-f]{64})  ([A-Za-z0-9._-]{1,128})$`)
)

const (
	keyFile  = "SigningKey.pem"
	sumsFile = "SHA256SUMS"
	sigFile  = "SHA256SUMS.sig"
	metaFile = "release.json"
	maxNotes = 20
)

type Artifact struct {
	Name   string `json:"name"`
	OS     string `json:"os"`
	Arch   string `json:"arch"`
	Libc   string `json:"libc"`
	Size   int64  `json:"size"`
	SHA256 string `json:"sha256"`
	// Package marks the offline package: the same binary with an installer
	// that needs no network, so a version can be installed later or elsewhere.
	Package bool `json:"package"`
}

type Release struct {
	Version string     `json:"version"`
	Date    string     `json:"date"` // YYYY-MM-DD
	Notes   []string   `json:"notes"`
	Files   []Artifact `json:"files"`
	order   [3]int
}

type Catalog struct {
	Releases       []*Release // newest first
	byVersion      map[string]*Release
	KeyPEM         []byte
	KeyFingerprint string // sha256 of the raw public key, hex in groups
}

func (c *Catalog) Latest() *Release {
	if c == nil || len(c.Releases) == 0 {
		return nil
	}
	return c.Releases[0]
}

func (c *Catalog) Find(version string) *Release {
	if c == nil {
		return nil
	}
	return c.byVersion[version]
}

// Serves reports whether name is a file of this release a request may download.
func (r *Release) Serves(name string) bool {
	if name == sumsFile || name == sigFile {
		return true
	}
	for _, a := range r.Files {
		if a.Name == name {
			return true
		}
	}
	return false
}

// catalogStore reloads the folder when it changes; readers never block.
type catalogStore struct {
	root   *os.Root
	cur    atomic.Pointer[Catalog]
	stamp  string
	hashes sync.Map // "<path>|<size>|<mtime>" → sha256, so a reload never rehashes unchanged files
}

func openCatalog(dir string) (*catalogStore, error) {
	root, err := os.OpenRoot(dir) // every read below stays inside dir, links included
	if err != nil {
		return nil, err
	}
	s := &catalogStore{root: root}
	s.reload()
	return s, nil
}

func (s *catalogStore) get() *Catalog { return s.cur.Load() }

// watch re-reads the catalog every interval when anything in it changed.
func (s *catalogStore) watch(every time.Duration, stop <-chan struct{}) {
	t := time.NewTicker(every)
	defer t.Stop()
	for {
		select {
		case <-stop:
			return
		case <-t.C:
			s.reload()
		}
	}
}

func (s *catalogStore) reload() {
	stamp := s.fingerprint()
	if stamp == s.stamp && s.cur.Load() != nil {
		return
	}
	cat, err := s.build()
	if err != nil {
		log.Printf("catalog: %v (no releases offered)", err)
		cat = &Catalog{byVersion: map[string]*Release{}}
	}
	s.cur.Store(cat)
	s.stamp = stamp
}

// fingerprint summarises names, sizes and times of everything published.
func (s *catalogStore) fingerprint() string {
	var b strings.Builder
	fs.WalkDir(s.root.FS(), ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if fi, err := d.Info(); err == nil {
			fmt.Fprintf(&b, "%s|%d|%d\n", p, fi.Size(), fi.ModTime().UnixNano())
		}
		return nil
	})
	return b.String()
}

func (s *catalogStore) build() (*Catalog, error) {
	keyPEM, err := readSmall(s.root, keyFile, 4096)
	if err != nil {
		return nil, fmt.Errorf("signing key: %w", err)
	}
	pub, der, err := parsePublicKey(keyPEM)
	if err != nil {
		return nil, err
	}
	sum := sha256.Sum256(der)
	cat := &Catalog{byVersion: map[string]*Release{}, KeyPEM: keyPEM, KeyFingerprint: groupHex(hex.EncodeToString(sum[:]))}
	ents, err := fs.ReadDir(s.root.FS(), ".")
	if err != nil {
		return nil, err
	}
	for _, e := range ents {
		if !e.IsDir() || !versionRe.MatchString(e.Name()) {
			continue
		}
		r, err := s.loadRelease(e.Name(), pub)
		if err != nil {
			log.Printf("catalog: release %s refused: %v", e.Name(), err)
			continue
		}
		cat.Releases = append(cat.Releases, r)
		cat.byVersion[r.Version] = r
	}
	sort.Slice(cat.Releases, func(i, j int) bool {
		a, b := cat.Releases[i].order, cat.Releases[j].order
		for k := range a {
			if a[k] != b[k] {
				return a[k] > b[k]
			}
		}
		return false
	})
	return cat, nil
}

// parsePublicKey reads the release key: one PEM public key, ECDSA P-256. Its
// fingerprint is the SHA-256 of the DER form returned beside it.
func parsePublicKey(raw []byte) (*ecdsa.PublicKey, []byte, error) {
	block, rest := pem.Decode(raw)
	if block == nil || block.Type != "PUBLIC KEY" || len(bytes.TrimSpace(rest)) != 0 {
		return nil, nil, fmt.Errorf("signing key is not one PEM public key")
	}
	k, err := x509.ParsePKIXPublicKey(block.Bytes)
	if err != nil {
		return nil, nil, err
	}
	pub, ok := k.(*ecdsa.PublicKey)
	if !ok || pub.Curve != elliptic.P256() {
		return nil, nil, fmt.Errorf("signing key is not ECDSA P-256")
	}
	return pub, block.Bytes, nil
}

func (s *catalogStore) loadRelease(version string, pub *ecdsa.PublicKey) (*Release, error) {
	sums, err := readSmall(s.root, version+"/"+sumsFile, 64<<10)
	if err != nil {
		return nil, err
	}
	sig, err := readSmall(s.root, version+"/"+sigFile, 1024)
	if err != nil {
		return nil, err
	}
	digest := sha256.Sum256(sums)
	if !ecdsa.VerifyASN1(pub, digest[:], sig) {
		return nil, fmt.Errorf("signature does not verify")
	}
	signed := map[string]string{}
	sc := bufio.NewScanner(bytes.NewReader(sums))
	for sc.Scan() {
		m := sumLineRe.FindStringSubmatch(sc.Text())
		if m == nil || signed[m[2]] != "" {
			return nil, fmt.Errorf("malformed %s line %q", sumsFile, sc.Text())
		}
		signed[m[2]] = m[1]
	}
	if signed[metaFile] == "" {
		return nil, fmt.Errorf("%s is not signed", metaFile)
	}
	// Every signed file must be present with exactly the signed bytes.
	sizes := map[string]int64{}
	for name, want := range signed {
		got, size, err := s.hashFile(version + "/" + name)
		if err != nil {
			return nil, err
		}
		if got != want {
			return nil, fmt.Errorf("%s does not match its signed hash", name)
		}
		sizes[name] = size
	}
	meta, err := readSmall(s.root, version+"/"+metaFile, 64<<10)
	if err != nil {
		return nil, err
	}
	dec := json.NewDecoder(bytes.NewReader(meta))
	dec.DisallowUnknownFields()
	var r Release
	if err := dec.Decode(&r); err != nil {
		return nil, fmt.Errorf("%s: %w", metaFile, err)
	}
	if r.Version != version {
		return nil, fmt.Errorf("%s says %q", metaFile, r.Version)
	}
	if _, err := time.Parse("2006-01-02", r.Date); err != nil {
		return nil, fmt.Errorf("%s date: %w", metaFile, err)
	}
	if len(r.Notes) > maxNotes {
		return nil, fmt.Errorf("too many notes")
	}
	for i, n := range r.Notes {
		r.Notes[i] = strings.TrimSpace(n)
		if r.Notes[i] == "" || len(r.Notes[i]) > 300 {
			return nil, fmt.Errorf("note %d empty or too long", i)
		}
	}
	if len(r.Files) == 0 {
		return nil, fmt.Errorf("no files")
	}
	seen := map[string]bool{}
	for i := range r.Files {
		a := &r.Files[i]
		m := artifactRe.FindStringSubmatch(a.Name)
		if m == nil || m[1] != version || seen[a.Name] {
			return nil, fmt.Errorf("file name %q", a.Name)
		}
		seen[a.Name] = true
		system, arch, libc := m[2]+m[5]+m[7], m[3]+m[6]+m[8], m[4]
		if a.OS != system || a.Arch != arch || a.Libc != libc {
			return nil, fmt.Errorf("%s: platform fields disagree with its name", a.Name)
		}
		if signed[a.Name] == "" || a.SHA256 != signed[a.Name] || a.Size != sizes[a.Name] {
			return nil, fmt.Errorf("%s: size/hash not the signed ones", a.Name)
		}
	}
	parts := strings.Split(version, ".")
	for k := range r.order {
		r.order[k], _ = strconv.Atoi(parts[k])
	}
	return &r, nil
}

func (s *catalogStore) hashFile(name string) (string, int64, error) {
	f, err := s.root.Open(name)
	if err != nil {
		return "", 0, err
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil || !fi.Mode().IsRegular() {
		return "", 0, fmt.Errorf("%s is not a regular file", name)
	}
	key := fmt.Sprintf("%s|%d|%d", name, fi.Size(), fi.ModTime().UnixNano())
	if h, ok := s.hashes.Load(key); ok {
		return h.(string), fi.Size(), nil
	}
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", 0, err
	}
	sum := hex.EncodeToString(h.Sum(nil))
	s.hashes.Store(key, sum)
	return sum, fi.Size(), nil
}

func readSmall(root *os.Root, name string, max int64) ([]byte, error) {
	f, err := root.Open(name)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil || !fi.Mode().IsRegular() || fi.Size() > max {
		return nil, fmt.Errorf("%s: not a regular file of at most %d bytes", name, max)
	}
	return io.ReadAll(io.LimitReader(f, max))
}

func groupHex(h string) string {
	var parts []string
	for i := 0; i+8 <= len(h); i += 8 {
		parts = append(parts, h[i:i+8])
	}
	return strings.ToUpper(strings.Join(parts, " "))
}
