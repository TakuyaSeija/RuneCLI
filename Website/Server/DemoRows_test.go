package main

import (
	"strings"
	"testing"
)

func TestReplayNoticesFollowTheSameSideOfResultsAsCLI(t *testing.T) {
	source := []string{"<tool_notice>truncated output</tool_notice>", "=== file.js (lines 1-2 of 2) ===", "     1|first", "     2|second"}
	read := resultRows(source, true, true)
	if len(read) != 3 || read[0].Text != "     1|first" || read[2].Text != "truncated output" || read[2].Class != "tool-notice" {
		t.Fatalf("read order: %+v", read)
	}
	if len(read[0].Tokens) != 3 || read[0].Tokens[0].Class != "faint" || read[0].Tokens[2].Text != "first" {
		t.Fatal("read line numbers lost their own ink")
	}
	process := resultRows([]string{"<tool_notice>", "Process: Completed", "Exit code: 0", "</tool_notice>", "result"}, false, false)
	if len(process) != 3 || process[0].Text != "Process: Completed" || process[2].Text != "result" {
		t.Fatalf("process envelope: %+v", process)
	}
	terminal := resultRows([]string{"hello", "<tool_notice>captured</tool_notice>"}, false, false)
	if terminal[0].Text != "captured" || terminal[1].Text != "hello" {
		t.Fatalf("terminal envelope: %+v", terminal)
	}
}

func TestReplayAllGateSourceUsesOriginalSyntaxTokens(t *testing.T) {
	rec := recordedFixture(t)
	for _, turn := range rec.Turns {
		for _, call := range turn.Calls {
			if call.Name != "create_file" && call.Name != "terminal_use" {
				continue
			}
			step := stepOf(call)
			for _, row := range step.GateLines {
				if row.Text == "" {
					continue
				}
				var plain strings.Builder
				for _, token := range row.Tokens {
					plain.WriteString(token.Text)
				}
				if plain.String() != row.Text {
					t.Fatalf("permission source mismatch: %s", call.Name)
				}
			}
		}
	}
}

func TestReplayNumberedAnswerItemsDoNotMergeIntoOneParagraph(t *testing.T) {
	blocks := markdownish("Two findings:\n\n1. **First** item\n2. `Second` item\n")
	if len(blocks) != 2 || blocks[1].Kind != "list" || len(blocks[1].Items) != 2 {
		t.Fatalf("numbered items lost: %+v", blocks)
	}
}

func TestReplayCodeInsideBoldStaysCode(t *testing.T) {
	got := string(demoInline("**`albumCount` rendered as `undefined`** and <b>"))
	want := "<strong><code>albumCount</code> rendered as <code>undefined</code></strong> and &lt;b&gt;"
	if got != want {
		t.Fatalf("inline markdown: %s", got)
	}
}
