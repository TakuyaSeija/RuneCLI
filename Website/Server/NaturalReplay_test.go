package main

import (
	"strings"
	"testing"
)

func TestSimulatedPrefixesStayInsideRecordedRequestBoundaries(t *testing.T) {
	rec := recordedFixture(t)
	view, err := layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	previous := 0
	finals := 0
	for _, step := range view.Steps {
		if step.Start < previous || step.StreamEnd < step.Start {
			t.Fatalf("nonmonotonic prefix %+v", step)
		}
		previous = step.Start
		if step.Kind == "tool" {
			if step.StreamEnd > step.RunAt || step.Start+step.Dwell < step.RunAt {
				t.Fatal("execution overlaps simulated arguments")
			}
			if step.Gate && (step.GateAt < step.StreamEnd || step.GateEnd <= step.GateAt || step.GateEnd != step.RunAt) {
				t.Fatal("approval overlaps arguments or execution")
			}
		}
		if step.Kind == "final" {
			finals++
			if step.StreamEnd <= step.Start {
				t.Fatal("final response does not stream")
			}
		}
	}
	if finals != 1 {
		t.Fatalf("final response count %d", finals)
	}
}

func TestReadMetadataCannotMasqueradeAsArgumentText(t *testing.T) {
	image := stepOf(recCall{Name: "inspect_file", Path: "screenshots/home.png", OK: true, Out: []string{"<tool_notice>Attached screenshots/home.png (image/png, 11.0 KB).</tool_notice>"}})
	if image.Note != "" || image.ArgNote != "" || len(image.Rows) != 1 || !strings.Contains(image.Rows[0].Text, "image/png") {
		t.Fatalf("attachment metadata escaped its result: %+v", image)
	}
	read := stepOf(recCall{Name: "inspect_file", Path: "file.js", OK: true, Range: []int{1, 4}, Out: []string{"=== file.js (lines 1-4 of 20) ===", "1|text"}})
	if read.ArgNote != "1 - 4" || read.Note != "lines 1-4 of 20" {
		t.Fatalf("range/result provenance mixed: %+v", read)
	}
}

func TestOnlyRecordedNotificationsAreProjected(t *testing.T) {
	rec := recordedFixture(t)
	view, err := layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	for _, step := range view.Steps {
		if step.Kind == "notification" {
			t.Fatal("fabricated notification in source655")
		}
	}
	// One inside a shown request appears with its text; one inside a skipped
	// span is not moved into the excerpt.
	shown := rec.Turns[76].Calls[0].StartedAt + 1000
	rec.Notifications = []recNotification{{At: shown, Text: "A genuine recorded background task finished."}, {At: rec.Turns[30].At, Text: "Skipped."}}
	view, err = layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	var labels []string
	for _, step := range view.Steps {
		if step.Kind == "notification" {
			labels = append(labels, step.Label)
		}
	}
	if len(labels) != 1 || labels[0] != rec.Notifications[0].Text {
		t.Fatalf("notifications: %q", labels)
	}
}

func TestNaturalReplayMarkupNamesItsSimulationAndOwnsResponseActivity(t *testing.T) {
	f := newDownloads(t)
	h := newTestServer(t, testConfig(f.dir)).routes()
	home := get(t, h, "GET", "/Studio/").Body.String()
	for _, want := range []string{`Streaming and approvals simulated`, `not measured chunk timing`, `data-tool-time`, `data-stream="thought"`, `data-stream="action"`, `data-stream="command"`, `data-kind="prose"><p class="step-title dim"><span class="step-dot" aria-hidden="true">●</span>Response`, `type="module"`} {
		if !strings.Contains(home, want) {
			t.Errorf("missing contract %s", want)
		}
	}
	if strings.Contains(home, "data-playback-state") {
		t.Fatal("secondary playback text retained")
	}
	if strings.Count(home, `data-scene="response"`) != 1 {
		t.Fatal("duplicated final response")
	}
}
