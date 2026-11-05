package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io/fs"
	"sort"
	"strings"

	ui "atoryn.com/studio/web/UI"
)

// The site is read in ten languages (owner, 2026-11-04). English is the source:
// its routes, files and catalogue live in Docs.go and Site.go. A translation
// owns only words — its templates under UI/Pages/<code>/ and the strings in
// that folder's metadata.json — so it can never add, drop or rename a page.
// Rune itself speaks English: what the CLI draws in the replay stays English
// in every language, because the illustration shows only what the CLI draws.
type locale struct {
	Code  string // html lang, hreflang and the path segment; "en" has none
	Name  string // the language's own name, as the menu lists it
	Short string // what a phone shows in place of the name
	text  *localeText
}

var locales = []*locale{
	{Code: "en", Name: "English", Short: "EN"},
	{Code: "id", Name: "Bahasa Indonesia", Short: "ID"},
	{Code: "zh-CN", Name: "简体中文", Short: "ZH"},
	{Code: "es", Name: "Español", Short: "ES"},
	{Code: "ja", Name: "日本語", Short: "JA"},
	{Code: "ko", Name: "한국어", Short: "KO"},
	{Code: "pt-BR", Name: "Português (Brasil)", Short: "PT"},
	{Code: "de", Name: "Deutsch", Short: "DE"},
	{Code: "fr", Name: "Français", Short: "FR"},
	{Code: "ar", Name: "العربية", Short: "AR"},
}

var english = locales[0]

// prefix is the locale's path segment below the base path: "" or "/id".
func (l *locale) prefix() string {
	if l == english {
		return ""
	}
	return "/" + l.Code
}

// pageFile is where the locale's copy of a template lives inside ui.Files.
func (l *locale) pageFile(name string) string {
	if l == english {
		return "Pages/" + name
	}
	return "Pages/" + l.Code + "/" + name
}

type titled struct {
	Title string `json:"title"`
	Lead  string `json:"lead"`
}

type labelled struct {
	Label string `json:"label"`
	Lead  string `json:"lead"`
}

// installText is the line under each install command, in the locale's words.
// {version}, {date}, {system} and {builds} are filled from the catalog, so a
// translation can never name a build that was not published.
type installText struct {
	None    string `json:"none"`
	Missing string `json:"missing"`
	Release string `json:"release"`
	Beta    string `json:"beta"`
	And     string `json:"and"`
}

type localeText struct {
	Pages       map[string]titled   `json:"pages"`
	// Notes is the changelog's own words, one entry per version and one line per
	// recorded change. English owns the keys; a translation holds exactly the
	// same versions with the same number of lines (owner, 2026-11-05).
	Notes       map[string][]string `json:"notes"`
	Groups      map[string]string   `json:"groups"`
	DocsTitle   string              `json:"docs_title"`
	Docs        map[string]labelled `json:"docs"`
	Chapters    map[string]string   `json:"chapters"`
	InstallNote installText         `json:"install_note"`
}

// localeTextPages are the non-documentation pages a translation titles.
var localeTextPages = []string{"", "changelog", "license", "notfound"}

// englishNotes is the changelog's source of words: version -> recorded changes.
var englishNotes = map[string][]string{}

func loadChangelogNotes() error {
	raw, err := fs.ReadFile(ui.Files, "Pages/ChangelogNotes.json")
	if err != nil {
		return fmt.Errorf("changelog notes: %w", err)
	}
	if err := json.Unmarshal(raw, &englishNotes); err != nil {
		return fmt.Errorf("changelog notes: %w", err)
	}
	for version, lines := range englishNotes {
		if len(lines) == 0 {
			return fmt.Errorf("changelog notes: %s has no lines", version)
		}
		for i, line := range lines {
			if strings.TrimSpace(line) == "" || len(line) > 300 {
				return fmt.Errorf("changelog notes: %s line %d is empty or too long", version, i)
			}
		}
	}
	return nil
}

func loadLocaleTexts() error {
	if err := loadChangelogNotes(); err != nil {
		return err
	}
	for _, l := range locales[1:] {
		raw, err := fs.ReadFile(ui.Files, l.pageFile("metadata.json"))
		if err != nil {
			return fmt.Errorf("locale %s: %w", l.Code, err)
		}
		dec := json.NewDecoder(bytes.NewReader(raw))
		dec.DisallowUnknownFields()
		var t localeText
		if err := dec.Decode(&t); err != nil {
			return fmt.Errorf("locale %s metadata.json: %w", l.Code, err)
		}
		if err := t.check(); err != nil {
			return fmt.Errorf("locale %s metadata.json: %w", l.Code, err)
		}
		l.text = &t
	}
	return nil
}

// check holds a translation to the English catalogue: the same keys, every
// one of them said, the placeholders the catalog fills present.
func (t *localeText) check() error {
	var want []string
	for _, g := range docGroups {
		want = append(want, g.Title)
	}
	if err := sameKeys("groups", keysOf(t.Groups), want); err != nil {
		return err
	}
	want = want[:0]
	for _, p := range pages {
		if p.Section != "" {
			want = append(want, p.Route)
		}
	}
	if err := sameKeys("docs", keysOf(t.Docs), want); err != nil {
		return err
	}
	if err := sameKeys("pages", keysOf(t.Pages), localeTextPages); err != nil {
		return err
	}
	want = want[:0]
	for _, c := range sessionExcerpt {
		want = append(want, c.Name)
	}
	if err := sameKeys("chapters", keysOf(t.Chapters), want); err != nil {
		return err
	}
	if err := sameKeys("notes", keysOf(t.Notes), keysOf(englishNotes)); err != nil {
		return err
	}
	for version, lines := range t.Notes {
		if len(lines) != len(englishNotes[version]) {
			return fmt.Errorf("note %s: %d lines, English has %d", version, len(lines), len(englishNotes[version]))
		}
		for i, line := range lines {
			if strings.TrimSpace(line) == "" || len(line) > 300 {
				return fmt.Errorf("note %s line %d is empty or too long", version, i)
			}
		}
	}
	for k, v := range t.Groups {
		if strings.TrimSpace(v) == "" {
			return fmt.Errorf("group %q has no words", k)
		}
	}
	for k, v := range t.Chapters {
		if strings.TrimSpace(v) == "" {
			return fmt.Errorf("chapter %q has no words", k)
		}
	}
	for k, v := range t.Pages {
		if strings.TrimSpace(v.Title) == "" || strings.TrimSpace(v.Lead) == "" {
			return fmt.Errorf("page %q needs a title and a lead", k)
		}
	}
	for k, v := range t.Docs {
		if strings.TrimSpace(v.Label) == "" || strings.TrimSpace(v.Lead) == "" {
			return fmt.Errorf("article %q needs a label and a lead", k)
		}
	}
	if !strings.Contains(t.DocsTitle, "{label}") {
		return fmt.Errorf("docs_title must hold {label}")
	}
	n := t.InstallNote
	for name, s := range map[string]struct {
		text  string
		holes []string
	}{
		"none":    {n.None, nil},
		"missing": {n.Missing, []string{"{version}", "{system}"}},
		"release": {n.Release, []string{"{version}", "{date}", "{system}", "{builds}"}},
		"beta":    {n.Beta, []string{"{system}"}},
		"and":     {n.And, nil},
	} {
		if strings.TrimSpace(s.text) == "" {
			return fmt.Errorf("install_note.%s has no words", name)
		}
		for _, h := range s.holes {
			if !strings.Contains(s.text, h) {
				return fmt.Errorf("install_note.%s must hold %s", name, h)
			}
		}
	}
	return nil
}

func keysOf[V any](m map[string]V) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}

func sameKeys(what string, got, want []string) error {
	g, w := append([]string(nil), got...), append([]string(nil), want...)
	sort.Strings(g)
	sort.Strings(w)
	if strings.Join(g, "\x00") != strings.Join(w, "\x00") {
		return fmt.Errorf("%s: has %q, the English catalogue has %q", what, g, w)
	}
	return nil
}

// localizedPages is the page catalogue in one locale's words: the same
// routes, files and order as English, the locale's titles, labels and leads.
// Each site gets its own copies, English included: parsing fills them in.
func (l *locale) localizedPages() ([]*page, []docGroup) {
	byRoute := map[string]*page{}
	var out []*page
	for _, p := range pages {
		cp := *p
		if l == english {
			// the catalogue's own words
		} else if p.Section != "" {
			d := l.text.Docs[p.Route]
			cp.Label, cp.Lead, cp.Section = d.Label, d.Lead, l.text.Groups[p.Section]
			cp.Title = strings.ReplaceAll(l.text.DocsTitle, "{label}", d.Label)
		} else {
			t := l.text.Pages[p.Route]
			cp.Title, cp.Lead = t.Title, t.Lead
		}
		byRoute[p.Route] = &cp
		out = append(out, &cp)
	}
	groups := make([]docGroup, len(docGroups))
	for i, g := range docGroups {
		groups[i].Title = g.Title
		if l != english {
			groups[i].Title = l.text.Groups[g.Title]
		}
		for _, p := range g.Pages {
			groups[i].Pages = append(groups[i].Pages, byRoute[p.Route])
		}
	}
	return out, groups
}

// installNote is the locale's line under one system's install command.
func (l *locale) installNote(r *Release, system string) string {
	if l == english {
		return installNote(r, system)
	}
	n := l.text.InstallNote
	name := systemNames[system]
	beta := ""
	if system != "linux" {
		beta = " " + strings.ReplaceAll(n.Beta, "{system}", name)
	}
	if r == nil {
		return n.None + beta
	}
	parts := buildParts(r, system)
	if len(parts) == 0 {
		return strings.NewReplacer("{version}", r.Version, "{system}", name).Replace(n.Missing) + beta
	}
	builds := strings.Join(parts, " "+n.And+" ")
	return strings.NewReplacer("{version}", r.Version, "{date}", r.Date, "{system}", name, "{builds}", builds).Replace(n.Release) + beta
}

// localeLink is one entry of the language menu and of the page's alternates.
type localeLink struct {
	*locale
	Href    string
	Current bool
}
