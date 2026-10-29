package main

// The recording is public, redacted source, not a collection of preview images.
// Layout follows the CLI's cell geometry. Folding never discards source rows;
// the browser can reopen a file, thought, result or sent prompt without a call.

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"path"
	"strconv"
	"strings"

	ui "atoryn.com/studio/web/UI"
)

type recSession struct {
	Title         string            `json:"title"`
	Started       string            `json:"started"`
	StartAt       int64             `json:"start_at"`
	Active        int               `json:"active"`
	Model         string            `json:"model"`
	Effort        string            `json:"effort"`
	Mode          string            `json:"mode"`
	RecordedMode  string            `json:"recorded_mode"`
	Context       int64             `json:"context"`
	ContextSource string            `json:"context_source"`
	Project       string            `json:"project"`
	Prompt        string            `json:"prompt"`
	Response      string            `json:"response"`
	Tasks         []recTask         `json:"tasks"`
	Files         []recFile         `json:"files"`
	Turns         []recTurn         `json:"turns"`
	Final         recFinal          `json:"final"`
	Notifications []recNotification `json:"notifications"`
}
type recNotification struct {
	At   int64  `json:"at"`
	Text string `json:"text"`
}
type recTask struct {
	Text   string `json:"t"`
	Status string `json:"s"`
}
type recFile struct {
	Path    string `json:"p"`
	Added   int    `json:"a"`
	Removed int    `json:"r"`
	Change  string `json:"c"`
	At      int64  `json:"at"`
}
type recTurn struct {
	At          int64     `json:"at"`
	AccountedAt int64     `json:"accounted_at"`
	Text        string    `json:"text"`
	Reasoning   string    `json:"reasoning"`
	Words       int       `json:"words"`
	Ctx         int64     `json:"ctx"`
	CumIn       int64     `json:"cum_in"`
	CumOut      int64     `json:"cum_out"`
	CumCache    int64     `json:"cum_cache"`
	CumCost     float64   `json:"cum_cost"`
	Steps       int       `json:"steps"`
	Calls       []recCall `json:"calls"`
}
type recCall struct {
	Name          string        `json:"n"`
	Action        string        `json:"a"`
	OK            bool          `json:"ok"`
	Path          string        `json:"p"`
	Cmd           string        `json:"cmd"`
	Query         string        `json:"q"`
	Task          string        `json:"task"`
	Range         []int         `json:"rng"`
	Body          []string      `json:"body"`
	Syntax        [][]demoToken `json:"syntax"`
	CommandTokens []demoToken   `json:"command_tokens"`
	Total         int           `json:"total"`
	Rem           int           `json:"rem"`
	Add           int           `json:"add"`
	Diff          [][]string    `json:"diff"`
	Tasks         []recTask     `json:"tasks"`
	Out           []string      `json:"out"`
	Ms            int           `json:"ms"`
	StartedAt     int64         `json:"started_at"`
	EndedAt       int64         `json:"ended_at"`
}
type recFinal struct {
	Ctx      int64   `json:"ctx"`
	In       int64   `json:"in"`
	Out      int64   `json:"out"`
	Cache    int64   `json:"cache"`
	Cost     float64 `json:"cost"`
	Steps    int     `json:"steps"`
	Turns    int     `json:"turns"`
	Requests int     `json:"requests"`
	Files    int     `json:"files"`
}
type demoStep struct {
	Kind      string
	Call      string
	Label     string
	Words     int
	Thought   []demoBlock
	Prose     []demoBlock
	Verb      string
	Target    string
	Note      string // Result-only metadata; unavailable until execution settles.
	ArgNote   string // A range explicitly supplied by the model, never result metadata.
	Added     int
	Removed   int
	Failed    bool
	Timing    string
	Box       string
	Cmd       string
	Command   []demoToken
	Rows      []demoRow
	Tasks     []recTask
	Gate      bool
	GateTitle string
	GateLines []demoRow
	Start     int // Simulated first argument/text arrival within the recorded request.
	Dwell     int
	StreamEnd int
	RunAt     int // Recorded execution start, after any simulated approval.
	SourceMs  int // Recorded execution duration; never includes argument generation.
	GateAt    int
	GateEnd   int
	Always    string // The remember key an approval card offers; commands offer none.
	Ordinal   int    // The call's position among every recorded call of the session.
}
type demoToken struct {
	Class string
	Text  string
}
type demoRow struct {
	Op     string
	Class  string
	Text   string
	Tokens []demoToken
}
type demoView struct {
	Title         string
	Started       string
	Model         string
	Effort        string
	Mode          string
	Project       string
	Version       string
	Context       int64
	ContextSource string
	Prompt        string
	Steps         []demoStep
	Tasks         []recTask
	Files         []recFile
	Totals        fileTotals
	Active        string
	Final         recFinal
	Schedule      string
	Chapters      []demoChapter
	ShownSteps    int       // Tool steps the excerpt shows.
	Recorded      string    // The recorded session's length, as the CLI prints it.
	Sample        *demoStep // The first shown command: the Ask card the page explains.
	Patch         *demoStep // The first shown edit: the diff the change panel shows.
}
type fileTotals struct {
	Count   int
	Added   int
	Removed int
}
type demoBlock struct {
	Kind  string
	Text  string
	Items []string
}

func demoOf(version string) (*demoView, error) {
	raw, err := fs.ReadFile(ui.Files, path.Join("Data", "Session655.json"))
	if err != nil {
		return nil, err
	}
	var rec recSession
	if err := json.Unmarshal(raw, &rec); err != nil {
		return nil, fmt.Errorf("recorded session: %w", err)
	}
	return layout(&rec, version)
}

func stepOf(c recCall) demoStep {
	s := demoStep{Kind: "tool", Call: c.Name, Label: c.Action, Failed: !c.OK}
	if c.Ms >= 1000 {
		s.Timing = duration(c.Ms / 1000)
	}
	if s.Label == "" {
		s.Label = fallbackLabel(c)
	}
	output := resultRows(c.Out, false, false)
	switch c.Name {
	case "terminal_use":
		s.Box, s.Cmd = "Bash", c.Cmd
		s.Command = c.CommandTokens
		s.Rows = output
		s.GateTitle = "Bash"
		s.GateLines = []demoRow{{Text: "$ " + c.Cmd, Tokens: append([]demoToken{{Text: "$ "}}, c.CommandTokens...)}}
	case "create_file":
		s.Verb, s.Target = "Write", c.Path
		s.Added = c.Total
		s.Box = extension(c.Path)
		if c.OK {
			for i, line := range c.Body {
				var tokens []demoToken
				if i < len(c.Syntax) {
					tokens = c.Syntax[i]
				}
				s.Rows = append(s.Rows, demoRow{Text: line, Tokens: tokens})
			}
		} else {
			s.Added = 0
			s.Box = ""
			s.Rows = output
		}
		s.GateTitle = "Create " + c.Path + " +" + strconv.Itoa(c.Total)
		for i, line := range c.Body {
			var tokens []demoToken
			if i < len(c.Syntax) {
				tokens = c.Syntax[i]
			}
			s.GateLines = append(s.GateLines, demoRow{Text: line, Tokens: tokens})
		}
	case "patch_file":
		s.Verb, s.Target, s.Box = "Edit", c.Path, "diff"
		s.Added, s.Removed = c.Add, c.Rem
		for _, pair := range c.Diff {
			if len(pair) == 2 {
				s.Rows = append(s.Rows, demoRow{Op: pair[0], Text: pair[1]})
			}
		}
		s.GateTitle = "Patch " + c.Path + " −" + strconv.Itoa(c.Rem) + " +" + strconv.Itoa(c.Add)
		s.GateLines = s.Rows
	case "inspect_file":
		s.Verb, s.Target = "Read", c.Path
		if len(c.Range) == 2 {
			s.ArgNote = strconv.Itoa(c.Range[0]) + " - " + strconv.Itoa(c.Range[1])
		}
		s.Note = readNote(c)
		s.Rows = resultRows(c.Out, true, true)
	case "search_text", "find_files":
		s.Verb, s.Target = "Search", c.Query
		if c.Name == "find_files" {
			s.Verb = "Find"
		}
		for _, line := range c.Out {
			if strings.HasPrefix(line, "=== ") {
				if _, tail, ok := strings.Cut(line, " — "); ok {
					s.Note = strings.TrimSuffix(tail, " ===")
				}
				break
			}
		}
		s.Rows = resultRows(c.Out, true, true)
	case "web_search", "web_fetch":
		s.Verb, s.Target = "Look up", c.Query
		if c.Name == "web_fetch" {
			s.Verb = "Fetch"
		}
		s.Rows = resultRows(c.Out, true, false)
	case "track_tasks":
		s.Tasks = c.Tasks
	default:
		s.Rows = output
	}
	return s
}

func resultRows(source []string, below, stripHeader bool) []demoRow {
	var notices, body []demoRow
	inNotice := false
	for _, raw := range source {
		line := raw
		if strings.HasPrefix(strings.TrimSpace(line), "<tool_notice>") {
			inNotice = true
			line = strings.Replace(line, "<tool_notice>", "", 1)
		}
		closing := strings.Contains(line, "</tool_notice>")
		line = strings.ReplaceAll(line, "</tool_notice>", "")
		if line != "" || (!inNotice && len(body) > 0) {
			row := demoRow{Text: line}
			if inNotice {
				row.Class = "tool-notice"
				notices = append(notices, row)
			} else if !stripHeader || !strings.HasPrefix(line, "=== ") {
				body = append(body, styleResultRow(row))
			}
		}
		if closing {
			inNotice = false
		}
	}
	if below {
		return append(body, notices...)
	}
	return append(notices, body...)
}
func styleResultRow(row demoRow) demoRow {
	if bar := strings.IndexByte(row.Text, '|'); bar > 0 {
		prefix := strings.TrimSpace(row.Text[:bar])
		numeric := prefix != ""
		for _, c := range prefix {
			if c < '0' || c > '9' {
				numeric = false
				break
			}
		}
		if numeric {
			row.Tokens = []demoToken{{Class: "faint", Text: row.Text[:bar]}, {Class: "dim", Text: "|"}, {Class: "tfg", Text: row.Text[bar+1:]}}
		}
	}
	return row
}
func readNote(c recCall) string {
	for _, line := range c.Out {
		if !strings.HasPrefix(line, "=== ") || !strings.HasSuffix(line, " ===") {
			continue
		}
		if at := strings.LastIndex(line, " ("); at > 0 {
			return strings.TrimSuffix(strings.TrimSuffix(line[at+2:], " ==="), ")")
		}
	}
	// A binary attachment is the original tool notice in the result body, not
	// an argument-range badge and never unparsed markup in a title.
	return ""
}
func fallbackLabel(c recCall) string {
	if c.Cmd != "" {
		return "Run " + firstLine(c.Cmd)
	}
	if c.Path != "" {
		return "Read " + c.Path
	}
	return "Working"
}
func btoi(v bool) int {
	if v {
		return 1
	}
	return 0
}
func joinInts(values ...int) string {
	out := make([]string, len(values))
	for i, v := range values {
		out[i] = strconv.Itoa(v)
	}
	return strings.Join(out, ",")
}
func firstLine(s string) string { line, _, _ := strings.Cut(s, "\n"); return strings.TrimSpace(line) }
func displayModel(s string) string {
	if _, rest, ok := strings.Cut(s, "/"); ok {
		return rest
	}
	return s
}
func extension(s string) string {
	if dot := strings.LastIndex(s, "."); dot >= 0 {
		return s[dot+1:]
	}
	return "code"
}
func duration(s int) string {
	if s < 60 {
		return strconv.Itoa(s) + "s"
	}
	if s < 3600 {
		return fmt.Sprintf("%dm %02ds", s/60, s%60)
	}
	return fmt.Sprintf("%dh %02dm", s/3600, (s%3600)/60)
}

// A small escaped markdown projection; code remains text, never executable HTML.
func markdownish(text string) []demoBlock {
	var blocks []demoBlock
	var list, para, code []string
	inCode := false
	flushPara := func() {
		if len(para) > 0 {
			blocks = append(blocks, demoBlock{Kind: "p", Text: strings.Join(para, " ")})
			para = nil
		}
	}
	flushList := func() {
		if len(list) > 0 {
			blocks = append(blocks, demoBlock{Kind: "list", Items: list})
			list = nil
		}
	}
	for _, raw := range strings.Split(text, "\n") {
		trimmed := strings.TrimSpace(strings.TrimRight(raw, "\r"))
		if strings.HasPrefix(trimmed, "```") {
			flushPara()
			flushList()
			if inCode {
				blocks = append(blocks, demoBlock{Kind: "code", Text: strings.Join(code, "\n")})
				code = nil
			}
			inCode = !inCode
			continue
		}
		if inCode {
			code = append(code, raw)
			continue
		}
		switch {
		case trimmed == "":
			flushPara()
			flushList()
		case strings.HasPrefix(trimmed, "#"):
			flushPara()
			flushList()
			blocks = append(blocks, demoBlock{Kind: "h", Text: strings.TrimSpace(strings.TrimLeft(trimmed, "#"))})
		case strings.HasPrefix(trimmed, "- "):
			flushPara()
			list = append(list, trimmed[2:])
		case orderedItem(trimmed) != "":
			flushPara()
			list = append(list, orderedItem(trimmed))
		default:
			flushList()
			para = append(para, trimmed)
		}
	}
	flushPara()
	flushList()
	if len(code) > 0 {
		blocks = append(blocks, demoBlock{Kind: "code", Text: strings.Join(code, "\n")})
	}
	return blocks
}
func orderedItem(line string) string {
	at := 0
	for at < len(line) && line[at] >= '0' && line[at] <= '9' {
		at++
	}
	if at > 0 && at+2 <= len(line) && line[at:at+2] == ". " {
		return line[at+2:]
	}
	return ""
}

func thousands(n int64) string {
	digits := strconv.FormatInt(n, 10)
	sign := ""
	if strings.HasPrefix(digits, "-") {
		sign = "-"
		digits = digits[1:]
	}
	var out strings.Builder
	out.WriteString(sign)
	for i, d := range digits {
		if i > 0 && (len(digits)-i)%3 == 0 {
			out.WriteByte(',')
		}
		out.WriteRune(d)
	}
	return out.String()
}
