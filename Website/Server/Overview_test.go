package main

import (
	"strings"
	"testing"
)

func TestOverviewOwnsOneInstallTheSystemChoiceAndTheSessionCanvas(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	home := get(t, h, "GET", "/Studio/").Body.String()
	for _, required := range []string{`class="shell hero-inner" id="install"`, `id="os-linux" checked`, `id="os-macos"`, `id="os-windows"`, `macOS support is in beta`, `Windows support is in beta`, `id="install-cmd-macos"`, `id="install-cmd-windows"`, `PS&gt; `, `View install.ps1`, `id="session"`, `data-terminal-viewport`, `class="session-still"`, `id="installation-details"`, `data-side-heading`, `data-side-plan-heading`, `data-side-files-heading`, `Press Ctrl+↓ Jump to latest`, `the reading pace and the Ask approvals are simulated`, `the original ran in Full mode`, `preserveAspectRatio="none"`} {
		if !strings.Contains(home, required) {
			t.Errorf("missing overview contract %s", required)
		}
	}
	// The hero names what Rune is in words; no painted image stands in for the product.
	for _, removed := range []string{"RuneStone-", "stone-hero", "hero-art", "See a recorded session"} {
		if strings.Contains(home, removed) {
			t.Errorf("retired hero element retained: %s", removed)
		}
	}
	if n := strings.Count(home, "data-chapter "); n != 6 {
		t.Errorf("chapter rail has %d chapters", n)
	}
	for _, removed := range []string{"data-zoom-in", "data-zoom-out", "data-zoom-reset", "zoom-controls", "2.5×", "status-dot"} {
		if strings.Contains(home, removed) {
			t.Errorf("removed control retained: %s", removed)
		}
	}
	if strings.Count(home, "curl -fsSL") != 2 || strings.Count(home, `data-copy="install-cmd-windows"`) != 1 || strings.Count(home, `data-copy="install-cmd"`) != 1 {
		t.Fatal("the install box does not hold exactly one command per system")
	}
	if _, ok := newTestServer(t, testConfig(f.dir)).site.pages["en"]["install"]; ok {
		t.Fatal("install still rendered as separate page")
	}
}
