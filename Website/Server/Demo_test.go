package main

import (
	"encoding/json"
	"io/fs"
	"strconv"
	"strings"
	"testing"

	ui "atoryn.com/studio/web/UI"
)

func recordedFixture(t *testing.T) recSession {
	t.Helper()
	data, err := fs.ReadFile(ui.Files, "Data/Session655.json")
	if err != nil {
		t.Fatal(err)
	}
	var rec recSession
	if err := json.Unmarshal(data, &rec); err != nil {
		t.Fatal(err)
	}
	return rec
}

func TestReplayKeepsEveryRequestAndItsFinalAccounting(t *testing.T) {
	rec := recordedFixture(t)
	view, err := layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	parts := strings.Split(view.Schedule, "|")
	if len(parts) != 13 {
		t.Fatalf("schedule fields: %d", len(parts))
	}
	// The excerpt shows six requests, but the panel keeps every one of the
	// 145 so its numbers are the recorded session's, never the excerpt's.
	turns := strings.Split(parts[7], ";")
	if len(turns) != 145 || len(turns) != len(rec.Turns) {
		t.Fatalf("lost requests: %d", len(turns))
	}
	final := strings.Split(turns[len(turns)-1], ",")
	for i, want := range map[int]int64{1: 152211, 2: 14939074, 3: 169221, 4: 14235392, 5: 4996, 6: 207, 7: 2136} {
		if final[i] != strconv.FormatInt(want, 10) {
			t.Errorf("final field %d=%s want %d", i, final[i], want)
		}
	}
	if view.Context != 1000000 || view.Final.Turns != 1 || view.Totals.Count != 64 {
		t.Fatalf("metadata drift: %+v", view.Final)
	}
	counts := strings.Split(parts[11], ";")
	if len(counts) != 207 || !strings.HasSuffix(counts[len(counts)-1], ",207") {
		t.Fatalf("recorded step count lost: %d %s", len(counts), counts[len(counts)-1])
	}
	end, _ := strconv.Atoi(parts[1])
	for _, field := range []int{7, 8, 9, 11} {
		for _, row := range strings.Split(parts[field], ";") {
			n := leadingInt(row)
			if n < 0 || n > end {
				t.Fatalf("field %d clock outside replay: %d", field, n)
			}
		}
	}
	if len(strings.Split(parts[9], ";")) != 64 {
		t.Fatal("changed files lost")
	}
}

func TestReplayBodiesAreCompleteAndKeepToolGeometry(t *testing.T) {
	rec := recordedFixture(t)
	var sawRead, sawSearch, sawManage, sawWrite, sawPatch bool
	for _, turn := range rec.Turns {
		for _, call := range turn.Calls {
			s := stepOf(call)
			switch call.Name {
			case "create_file":
				if call.OK {
					sawWrite = true
					if len(s.Rows) != len(call.Body) || len(s.Rows) == 0 {
						t.Fatal("a Write body was shortened")
					}
					if s.Box != extension(call.Path) || s.Command != nil {
						t.Fatal("Write is a language box without command or output divider")
					}
					for i, row := range s.Rows {
						var text strings.Builder
						for _, token := range row.Tokens {
							text.WriteString(token.Text)
						}
						if text.String() != row.Text {
							t.Fatalf("syntax loses source at row %d", i)
						}
					}
				} else if s.Added != 0 || s.Box != "" {
					t.Fatal("failed writes cannot claim a successful body")
				}
			case "patch_file":
				sawPatch = true
				if len(s.Rows) != len(call.Diff) || s.Box != "diff" {
					t.Fatal("patch hunk source lost")
				}
			case "inspect_file":
				sawRead = true
				if len(s.Rows) == 0 || s.Box != "" {
					t.Fatal("Read output must be borderless and visible")
				}
			case "web_search":
				sawSearch = true
				if len(s.Rows) == 0 {
					t.Fatal("search result was discarded")
				}
			case "manage_process":
				sawManage = true
				if s.Verb != "" || len(s.Rows) == 0 {
					t.Fatal("process report is a borderless body without duplicate bullet")
				}
			}
			for _, row := range s.Rows {
				if strings.Contains(row.Text, "<tool_notice>") || strings.Contains(row.Text, "</tool_notice>") {
					t.Fatal("raw notice tags leaked")
				}
			}
		}
	}
	if !sawRead || !sawSearch || !sawManage || !sawWrite || !sawPatch {
		t.Fatal("missing tool coverage")
	}
}

func TestReplayMarkupIsEscapedAndHasNativeDisclosure(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	home := get(t, h, "GET", "/Studio/").Body.String()
	for _, old := range []string{"An illustrative session:", "recorded session, replayed", "/ unknown", "class=\"demo-note\""} {
		if strings.Contains(home, old) {
			t.Errorf("removed copy retained: %s", old)
		}
	}
	for _, required := range []string{"data-fold", "thought-fold", "data-user-text", "data-terminal-viewport", "data-demo-expand", "data-jump", "command-fold", "<fieldset class=\"demo-permission\"", "Simulated approval", "1,000,000", "Saved model settings"} {
		if !strings.Contains(home, required) {
			t.Errorf("missing contract: %s", required)
		}
	}
	if strings.Count(home, `class="effort-label">max</span>`) != 2 {
		t.Fatal("both effort labels must use the neutral Rune text ink")
	}
	if strings.Contains(home, `class="faint">max</span>`) {
		t.Fatal("effort must not follow the theme's faint ink")
	}
	// The excerpt is complete without a script, and small: the whole recording
	// once made this page 2 MB and 100,000 elements (owner, 2026-10-28).
	rec := recordedFixture(t)
	view, err := layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(home, "data-step=") != len(view.Steps) || strings.Count(home, "class=\"content-row") < 100 {
		t.Fatal("page is not a complete, server-rendered excerpt")
	}
	if len(home) > 160<<10 {
		t.Fatalf("home page is %d bytes", len(home))
	}
	unsafe := "**<script>alert(1)</script>** `\" onerror=\"` <img src=x>"
	got := string(demoInline(unsafe))
	if strings.Contains(got, "<script>") || strings.Contains(got, "<img") || !strings.Contains(got, "&lt;script&gt;") {
		t.Fatalf("unsafe inline markup: %s", got)
	}
}

func TestRecordedPaletteIsTheCLIAmberPalette(t *testing.T) {
	data, _ := fs.ReadFile(ui.Files, "Data/Session655.json")
	var rec struct {
		Palette map[string]string `json:"palette"`
	}
	if err := json.Unmarshal(data, &rec); err != nil {
		t.Fatal(err)
	}
	css, _ := fs.ReadFile(ui.Files, "Static/Styles/Terminal.css")
	for _, role := range []string{"Accent", "Text", "Muted", "Rule", "Good", "Bad", "Think", "CodeKeyword", "CodeString", "CodeNumber", "CodeType", "CodeFunc", "CodeComment"} {
		color := rec.Palette[role]
		if color == "" || !strings.Contains(string(css), color) {
			t.Errorf("palette drift: %s %s", role, color)
		}
	}
}
