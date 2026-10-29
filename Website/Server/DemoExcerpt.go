package main

// The home page shows an excerpt of recorded session 655, not the whole
// fourteen-minute replay: six chapters of original records, paced for reading.
// What a step shows is the recorded source. Only the speed at which text
// arrives, the pauses that let a person read it and the Ask-mode approval
// cards are presentation, and the page labels all three.

import (
	"fmt"
	"math"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"
)

// Reading research behind the pacing (owner, 2026-10-28: readable first,
// never sped up until nobody can follow it):
//   - Adults read English non-fiction silently at 238 words a minute on
//     average, most between 175 and 300 (Brysbaert 2019, 190 studies). That
//     is a pause to read, not a typing speed: text typed at reading pace
//     looked choppy and slow (owner, 2026-10-28), so prose streams whole words
//     like a model's output and the next step waits for most of the reading.
//   - Adult subtitles are capped at 20 characters a second, and one stays at
//     least five sixths of a second (Netflix timed-text guides). Titles,
//     commands and approval cards are held at least that long.
//   - Moving content that runs longer than five seconds needs a way to pause
//     it (WCAG 2.2.2): Pause, Replay and the chapter rail stay.
//
// What stays on screen keeps being read while the next step begins, so a step
// holds for two thirds of its reading time; the rest overlaps.
const (
	readWPM     = 238  // Average silent reading speed, words a minute.
	captionCPS  = 20   // Subtitle ceiling, characters a second.
	minHold     = 900  // One caption event, rounded up from 5/6 s.
	streamCPS   = 90   // Prose streams like a model's output, whole words at a time.
	readShare   = 65   // Percent of the reading time a step holds before the next.
	argumentCPS = 240  // Paths, commands and code arrive in provider chunks.
	reportCPS   = 200  // A final report streams like a fast model's output.
	typeStart   = 100  // The composer starts to fill.
	typeEnd     = 1200 // The prompt is sent.
	promptHold  = 1200 // The sent prompt is read before the first step.
	thinkCount  = 1200 // A folded thought counts its words.
	settle      = 400  // The eye returns before the next step starts.
	previewRows = 4    // A closed preview shows four rows, like the CLI.
	loopHold    = 7000 // Completion pause before the replay loops (owner, 2026-10-26).
)

// An excerpt part is one recorded request: its thought and text always show,
// with the listed calls. Parts follow the recording's order.
type excerptPart struct {
	Turn  int
	Calls []int
}

type excerptChapter struct {
	Name  string
	Parts []excerptPart
}

// Plan, build, a real failure, its fix, the final verification and the report:
// the arc of the session in its own records.
var sessionExcerpt = []excerptChapter{
	{"Plan", []excerptPart{{Turn: 2, Calls: []int{0}}}},
	{"Build", []excerptPart{{Turn: 5, Calls: []int{1}}}},
	{"Test", []excerptPart{{Turn: 76, Calls: []int{0}}}},
	{"Fix", []excerptPart{{Turn: 87, Calls: []int{0}}}},
	{"Verify", []excerptPart{{Turn: 141, Calls: []int{0}}}},
	{"Report", []excerptPart{{Turn: 142, Calls: []int{0}}, {Turn: 144}}},
}

type demoChapter struct {
	Name     string
	At       int    // Playback millisecond the chapter starts at.
	Recorded string // Where the chapter begins in the recorded session.
}

func wordCount(s string) int { return len(strings.Fields(s)) }

// Prose is read by words at the average reading speed.
func proseTime(text string) int { return max(minHold, wordCount(text)*60000/readWPM) }

// Titles, commands and cards are held like subtitles.
func captionTime(text string) int {
	return max(minHold, utf8.RuneCountInString(text)*1000/captionCPS)
}

// A recorded run time, from milliseconds to seconds, keeps its order without
// keeping its length: a 36 s test run plays for about two seconds.
func runTime(ms int) int {
	return min(2100, max(500, 450+int(330*math.Log2(1+float64(ms)/1000))))
}

// The reading pause after a step depends on how many preview rows it shows.
func glanceTime(rows int) int { return 800 + 300*min(rows, previewRows) }

func argumentTime(c recCall) int {
	size := utf8.RuneCountInString(c.Path + c.Cmd + c.Query)
	for _, line := range c.Body {
		size += utf8.RuneCountInString(line) + 1
	}
	for _, pair := range c.Diff {
		if len(pair) == 2 {
			size += utf8.RuneCountInString(pair[1]) + 1
		}
	}
	for _, task := range c.Tasks {
		size += utf8.RuneCountInString(task.Text) + 1
	}
	return min(1400, max(400, size*1000/argumentCPS))
}

// Ask mode is simulated over the original Full run, the way the engine would
// ask: every command each time with nothing to remember, every edit with the
// tool's standing grant on offer, reads and plans never.
func approval(c recCall) (gate bool, always string) {
	switch c.Name {
	case "terminal_use":
		return true, ""
	case "create_file", "patch_file":
		return true, c.Name
	}
	return false, ""
}

// keyframe pairs a playback moment with the recorded moment it shows, in
// milliseconds since the session started. Two keyframes at one playback
// moment are a cut: the recording skips ahead there.
type keyframe struct{ play, source int }

type clockMap []keyframe

// at is where a recorded moment appears in the excerpt: inside a shown span it
// is interpolated; inside a skipped span it appears at the cut that skips it.
func (m clockMap) at(source int) int {
	i := sort.Search(len(m), func(i int) bool { return m[i].source >= source })
	if i == len(m) {
		return m[len(m)-1].play
	}
	if i == 0 {
		return m[0].play
	}
	a, b := m[i-1], m[i]
	if b.play == a.play || b.source == a.source {
		return b.play
	}
	return a.play + (source-a.source)*(b.play-a.play)/(b.source-a.source)
}

func layout(rec *recSession, version string) (*demoView, error) {
	return excerptLayout(rec, version, sessionExcerpt)
}

func excerptLayout(rec *recSession, version string, chapters []excerptChapter) (*demoView, error) {
	if len(rec.Turns) == 0 || rec.Mode == "" || rec.Context <= 0 || rec.StartAt <= 0 || len(chapters) == 0 {
		return nil, fmt.Errorf("incomplete recording metadata")
	}
	v := &demoView{Title: rec.Title, Started: rec.Started, Model: displayModel(rec.Model), Effort: rec.Effort,
		Mode: strings.ToUpper(rec.Mode[:1]) + rec.Mode[1:], Project: rec.Project, Version: version,
		Context: rec.Context, ContextSource: rec.ContextSource, Prompt: rec.Prompt,
		Tasks: rec.Tasks, Files: rec.Files, Active: duration(rec.Active), Final: rec.Final}
	since := func(at int64) int { return int(max(0, at-rec.StartAt)) }
	// Every call's place in the whole session, and where each request began.
	ordinal := map[[2]int]int{}
	requestStart := make([]int, len(rec.Turns))
	count, cursor := 0, 0
	for ti, turn := range rec.Turns {
		requestStart[ti] = cursor
		cursor = max(cursor, since(turn.At))
		for ci, call := range turn.Calls {
			if call.StartedAt <= 0 || call.EndedAt < call.StartedAt {
				return nil, fmt.Errorf("call missing recorded timing")
			}
			count++
			ordinal[[2]int{ti, ci}] = count
			cursor = max(cursor, since(call.EndedAt))
		}
	}

	var clock clockMap
	mark := func(play, source int) {
		if n := len(clock); n > 0 {
			play, source = max(play, clock[n-1].play), max(source, clock[n-1].source)
		}
		clock = append(clock, keyframe{play, source})
	}
	mark(0, 0)
	t := typeEnd + promptHold
	lastTurn := -1
	type shownCall struct {
		call recCall
		step int
	}
	for _, chapter := range chapters {
		if len(chapter.Parts) == 0 {
			return nil, fmt.Errorf("empty chapter %s", chapter.Name)
		}
		for pi, part := range chapter.Parts {
			if part.Turn <= lastTurn || part.Turn >= len(rec.Turns) {
				return nil, fmt.Errorf("excerpt turn %d out of order", part.Turn)
			}
			lastTurn = part.Turn
			turn := rec.Turns[part.Turn]
			if pi == 0 {
				v.Chapters = append(v.Chapters, demoChapter{Name: chapter.Name, At: t, Recorded: clockText(requestStart[part.Turn])})
			}
			// The recorded clock holds while the previous step is read, then
			// cuts to this request; it never runs through a skipped span.
			mark(t, clock[len(clock)-1].source)
			mark(t, requestStart[part.Turn])
			// The request: its thought, its text and the arguments of every
			// shown call arrive before any of them runs, as in the CLI.
			if turn.Words > 0 {
				v.Steps = append(v.Steps, demoStep{Kind: "think", Words: turn.Words, Thought: markdownish(turn.Reasoning), Start: t, StreamEnd: t + thinkCount, Dwell: thinkCount})
				t += thinkCount + settle
			}
			if turn.Text != "" {
				if part.Turn == len(rec.Turns)-1 {
					arrive := max(minHold, utf8.RuneCountInString(turn.Text)*1000/reportCPS)
					v.Steps = append(v.Steps, demoStep{Kind: "final", Prose: markdownish(turn.Text), Start: t, StreamEnd: t + arrive, Dwell: arrive})
					t += arrive
				} else {
					arrive := max(minHold, utf8.RuneCountInString(turn.Text)*1000/streamCPS)
					v.Steps = append(v.Steps, demoStep{Kind: "prose", Prose: markdownish(turn.Text), Start: t, StreamEnd: t + arrive, Dwell: arrive})
					t += max(arrive+settle, proseTime(turn.Text)*readShare/100)
				}
			}
			var calls []shownCall
			for _, ci := range part.Calls {
				if ci < 0 || ci >= len(turn.Calls) {
					return nil, fmt.Errorf("excerpt call %d.%d out of range", part.Turn, ci)
				}
				call := turn.Calls[ci]
				s := stepOf(call)
				s.Ordinal = ordinal[[2]int{part.Turn, ci}]
				s.SourceMs = call.Ms
				s.Start, s.StreamEnd = t, t+argumentTime(call)
				t = s.StreamEnd
				calls = append(calls, shownCall{call, len(v.Steps)})
				v.Steps = append(v.Steps, s)
			}
			mark(t, since(turn.At))
			for _, shown := range calls {
				s := &v.Steps[shown.step]
				s.Gate, s.Always = approval(shown.call)
				if s.Gate {
					card := s.GateTitle
					if len(s.GateLines) > 0 {
						card += " " + s.GateLines[0].Text
					}
					s.GateAt = max(t, s.StreamEnd)
					s.GateEnd = s.GateAt + min(3600, max(2000, captionTime(card)+700))
					mark(s.GateAt, since(shown.call.StartedAt))
					t = s.GateEnd
				}
				s.RunAt = max(t, s.StreamEnd)
				mark(s.RunAt, since(shown.call.StartedAt))
				end := s.RunAt + runTime(shown.call.Ms)
				mark(end, since(shown.call.EndedAt))
				s.Dwell = end - s.Start
				rows := len(s.Rows) + len(s.Tasks)
				t = max(end+glanceTime(rows), s.Start+captionTime(s.Label)+settle)
				v.ShownSteps++
			}
		}
	}
	end := t
	if n := len(v.Steps); n == 0 || v.Steps[n-1].Kind != "final" {
		return nil, fmt.Errorf("the excerpt must end with the recorded report")
	}
	for _, notification := range rec.Notifications {
		// Only a notice inside a shown span appears; one in a skipped span is
		// neither invented elsewhere nor moved.
		at := since(notification.At)
		play := clock.at(at)
		shown := false
		for i := 1; i < len(clock); i++ {
			if clock[i].play > clock[i-1].play && clock[i-1].source <= at && at <= clock[i].source {
				shown = true
				break
			}
		}
		if shown {
			v.Steps = append(v.Steps, demoStep{Kind: "notification", Label: notification.Text, Start: play, StreamEnd: play})
		}
	}
	sort.SliceStable(v.Steps, func(i, j int) bool { return v.Steps[i].Start < v.Steps[j].Start })
	for i := range v.Steps {
		switch {
		case v.Steps[i].Call == "terminal_use" && v.Sample == nil:
			v.Sample = &v.Steps[i]
		case v.Steps[i].Call == "patch_file" && v.Patch == nil:
			v.Patch = &v.Steps[i]
		}
	}

	// The panel keeps the recorded state of the whole session: what happened
	// in a skipped span is there at the cut that skips it.
	var steps, turns, plans, files, counts, keyframes, starts []string
	for _, s := range v.Steps {
		steps = append(steps, joinInts(s.Start, s.Dwell, s.Words, btoi(s.Gate), s.GateEnd, s.StreamEnd, s.RunAt, s.SourceMs, s.GateAt, s.Ordinal))
	}
	last := since(rec.Turns[len(rec.Turns)-1].At)
	for ti, turn := range rec.Turns {
		active := int(int64(rec.Active) * int64(since(turn.At)) / int64(max(1, last)))
		turns = append(turns, joinInts(min(end, clock.at(since(turn.At))), int(turn.Ctx), int(turn.CumIn), int(turn.CumOut), int(turn.CumCache), int(turn.CumCost*10000+.5), turn.Steps, active))
		for ci, call := range turn.Calls {
			at := clock.at(since(call.StartedAt))
			for _, s := range v.Steps {
				if s.Ordinal == ordinal[[2]int{ti, ci}] {
					at = s.Start
				}
			}
			counts = append(counts, joinInts(min(end, at), ordinal[[2]int{ti, ci}]))
			if len(call.Tasks) > 0 {
				done := 0
				for _, task := range call.Tasks {
					if task.Status == "done" {
						done++
					}
				}
				plans = append(plans, joinInts(min(end, clock.at(since(call.EndedAt))), done, len(call.Tasks)))
			}
		}
	}
	sort.SliceStable(counts, func(i, j int) bool { return leadingInt(counts[i]) < leadingInt(counts[j]) })
	for _, file := range rec.Files {
		files = append(files, strconv.Itoa(min(end, clock.at(since(file.At)))))
		v.Totals.Count++
		v.Totals.Added += file.Added
		v.Totals.Removed += file.Removed
	}
	for _, k := range clock {
		keyframes = append(keyframes, joinInts(k.play, k.source))
	}
	keyframes = append(keyframes, joinInts(end, clock[len(clock)-1].source))
	for _, c := range v.Chapters {
		starts = append(starts, strconv.Itoa(c.At))
	}
	v.Recorded = duration(last / 1000)
	v.Schedule = strings.Join([]string{strconv.Itoa(end), strconv.Itoa(end), strconv.Itoa(typeStart), strconv.Itoa(typeEnd),
		strconv.Itoa(rec.Active), strconv.Itoa(loopHold), strings.Join(steps, ";"), strings.Join(turns, ";"),
		strings.Join(plans, ";"), strings.Join(files, ";"), strings.Join(keyframes, ";"), strings.Join(counts, ";"),
		strings.Join(starts, ";")}, "|")
	return v, nil
}

func leadingInt(row string) int {
	head, _, _ := strings.Cut(row, ",")
	n, _ := strconv.Atoi(head)
	return n
}

// clockText prints a recorded moment the way a timeline reads: 3:19, 23:22.
func clockText(ms int) string {
	s := ms / 1000
	if s >= 3600 {
		return fmt.Sprintf("%d:%02d:%02d", s/3600, s%3600/60, s%60)
	}
	return fmt.Sprintf("%d:%02d", s/60, s%60)
}
