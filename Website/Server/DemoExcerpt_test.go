package main

import (
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"
)

func excerptFixture(t *testing.T) (recSession, *demoView) {
	t.Helper()
	rec := recordedFixture(t)
	view, err := layout(&rec, "test")
	if err != nil {
		t.Fatal(err)
	}
	return rec, view
}

func TestExcerptShowsOnlyTheChosenRecordsInOrder(t *testing.T) {
	rec, view := excerptFixture(t)
	var labels, prose []string
	previous := 0
	for _, step := range view.Steps {
		if step.Start < previous {
			t.Fatalf("steps out of order at %d", step.Start)
		}
		previous = step.Start
		switch step.Kind {
		case "tool":
			labels = append(labels, step.Label)
		case "prose":
			prose = append(prose, step.Prose[0].Text)
		}
	}
	want := []string{"Create the implementation plan", "Create atomic JSON store", "Rebuild and run end-to-end tests",
		"Expose playback controls as store actions", "Final verification of all tests", "Mark all tasks complete"}
	if strings.Join(labels, "|") != strings.Join(want, "|") || view.ShownSteps != len(want) {
		t.Fatalf("excerpt calls: %q", labels)
	}
	// Every shown request keeps its own recorded text, whole.
	for _, turn := range []int{2, 5, 87, 141} {
		found := false
		for _, text := range prose {
			if strings.HasPrefix(rec.Turns[turn].Text, strings.TrimSpace(text)) {
				found = true
			}
		}
		if !found {
			t.Errorf("turn %d text missing", turn)
		}
	}
	last := view.Steps[len(view.Steps)-1]
	if last.Kind != "final" || len(last.Prose) == 0 {
		t.Fatal("the excerpt must end with the recorded report")
	}
	names := []string{}
	at := 0
	for _, chapter := range view.Chapters {
		if chapter.At <= at || chapter.Recorded == "" {
			t.Fatalf("chapter %s starts at %d", chapter.Name, chapter.At)
		}
		at = chapter.At
		names = append(names, chapter.Name)
	}
	if strings.Join(names, " ") != "Plan Build Test Fix Verify Report" {
		t.Fatalf("chapters: %v", names)
	}
	if view.Recorded != "35m 36s" {
		t.Fatalf("recorded length: %s", view.Recorded)
	}
}

// The owner asked for a replay people can read, not one compressed until
// nobody can (2026-10-28). These bounds are the research the pacing cites.
func TestExcerptPacingIsReadable(t *testing.T) {
	_, view := excerptFixture(t)
	for i, step := range view.Steps {
		next := 0
		if i+1 < len(view.Steps) {
			next = view.Steps[i+1].Start
		}
		switch step.Kind {
		case "prose":
			text := ""
			for _, block := range step.Prose {
				text += block.Text + " "
			}
			// Streams like a model, then holds for most of its reading time.
			words, size := len(strings.Fields(text)), utf8.RuneCountInString(strings.TrimSpace(text))
			if arrive := step.StreamEnd - step.Start; size*1000/max(1, arrive) > streamCPS+5 || arrive < minHold {
				t.Errorf("prose streams too fast: %d characters in %d ms", size, arrive)
			}
			if next > 0 && next-step.Start < words*60000/readWPM*readShare/100 {
				t.Errorf("prose leaves before it can be read: %d words, %d ms", words, next-step.Start)
			}
		case "tool":
			if next > 0 && next-step.Start < captionTime(step.Label) {
				t.Errorf("%q leaves before its title can be read", step.Label)
			}
			if step.Gate && step.GateEnd-step.GateAt < 2000 {
				t.Errorf("%q approval card shown %d ms", step.Label, step.GateEnd-step.GateAt)
			}
			if run := step.Start + step.Dwell - step.RunAt; run < 500 {
				t.Errorf("%q runs for %d ms", step.Label, run)
			}
		case "final":
			text := 0
			for _, block := range step.Prose {
				text += utf8.RuneCountInString(block.Text)
				for _, item := range block.Items {
					text += utf8.RuneCountInString(item)
				}
			}
			if cps := text * 1000 / max(1, step.StreamEnd-step.Start); cps > reportCPS+5 {
				t.Errorf("report streams at %d characters a second", cps)
			}
		}
	}
	end, _ := strconv.Atoi(strings.Split(view.Schedule, "|")[1])
	if end < 50000 || end > 90000 {
		t.Fatalf("excerpt lasts %d ms; readable pacing should land near 70 s", end)
	}
}

func TestExcerptKeepsRecordedFacts(t *testing.T) {
	rec, view := excerptFixture(t)
	durations := map[string]int{}
	for _, turn := range rec.Turns {
		for _, call := range turn.Calls {
			durations[call.Action] = call.Ms
		}
	}
	for _, step := range view.Steps {
		if step.Kind != "tool" {
			continue
		}
		if step.SourceMs != durations[step.Label] || step.Ordinal < 1 || step.Ordinal > 207 {
			t.Fatalf("%q lost its recorded duration or place", step.Label)
		}
		if step.StreamEnd > step.RunAt || step.Start > step.StreamEnd || step.RunAt > step.Start+step.Dwell {
			t.Fatalf("%q: arguments, approval and execution overlap", step.Label)
		}
	}
	keyframes := strings.Split(strings.Split(view.Schedule, "|")[10], ";")
	play, source := -1, -1
	for _, row := range keyframes {
		parts := strings.Split(row, ",")
		p, _ := strconv.Atoi(parts[0])
		s, _ := strconv.Atoi(parts[1])
		if p < play || s < source {
			t.Fatalf("clock runs backwards at %s", row)
		}
		play, source = p, s
	}
	if want := int(rec.Turns[len(rec.Turns)-1].At - rec.StartAt); source != want {
		t.Fatalf("clock ends at %d, recording at %d", source, want)
	}
}

func TestExcerptApprovalsFollowAskMode(t *testing.T) {
	_, view := excerptFixture(t)
	gates := 0
	for _, step := range view.Steps {
		if step.Kind != "tool" {
			continue
		}
		switch step.Call {
		case "terminal_use":
			// A command asks every time and offers nothing to remember.
			if !step.Gate || step.Always != "" {
				t.Errorf("%q: commands ask each time without Always", step.Label)
			}
		case "create_file", "patch_file":
			if !step.Gate || step.Always != step.Call {
				t.Errorf("%q: edits ask with the tool's standing grant on offer", step.Label)
			}
		default:
			if step.Gate {
				t.Errorf("%q asks although it changes nothing", step.Label)
			}
		}
		if step.Gate {
			gates++
		}
	}
	if gates != 4 {
		t.Fatalf("approvals: %d", gates)
	}
}

func TestClockMapPlacesSkippedMomentsAtTheirCut(t *testing.T) {
	m := clockMap{{0, 0}, {1000, 0}, {1000, 5000}, {3000, 7000}, {3000, 20000}, {4000, 21000}}
	for source, want := range map[int]int{0: 0, 2500: 1000, 5000: 1000, 6000: 2000, 7000: 3000, 15000: 3000, 20500: 3500, 30000: 4000} {
		if got := m.at(source); got != want {
			t.Errorf("at(%d)=%d want %d", source, got, want)
		}
	}
}
