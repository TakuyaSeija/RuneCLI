//! One bounded document for every permission request. Only its body scrolls;
//! the answers stay pinned, and input uses the geometry of the frame it saw.

use std::ops::Range;

use super::{
    ansi::{self, ACCENT, BOLD, FAINT, MUTED, REMOVED, RESET, REVERSE, TEXT},
    blocks::{Ask, Tool},
    draw::{self, Hit, Sink},
    steps,
};

#[derive(Default)]
pub(super) struct State {
    req: String,
    pub scroll: usize,
    pub max: usize,
    pub visible: usize,
    pub card: Option<(u16, u16)>,
    answers: Vec<Vec<(Range<usize>, usize)>>,
    document: Option<Document>,
}

struct Document {
    width: usize,
    tool: Option<String>,
    title: String,
    body: Vec<String>,
}

impl State {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn scroll_by(&mut self, up: bool, rows: usize) {
        self.scroll = if up {
            self.scroll.saturating_sub(rows)
        } else {
            self.scroll.saturating_add(rows).min(self.max)
        };
    }

    pub fn contains(&self, row: u16) -> bool {
        self.card
            .is_some_and(|(first, last)| row >= first && row <= last)
    }

    pub fn answer_at(&self, row: usize, col: u16) -> Option<usize> {
        let column = usize::from(col).checked_sub(3)?;
        self.answers
            .get(row)?
            .iter()
            .find_map(|(range, answer)| range.contains(&column).then_some(*answer))
    }
}

fn wrapped(out: &mut Vec<String>, text: &str, ink: ansi::Ink, width: usize) {
    out.extend(
        ansi::wrap(&ansi::tame(text), width)
            .into_iter()
            .map(|line| format!("{ink}{line}{RESET}")),
    );
}

fn document(ask: &Ask, tool: Option<&Tool>, width: usize) -> (String, Vec<String>) {
    let mut body = Vec::new();
    if let Some(tool) = tool.filter(|tool| steps::is_terminal(tool)) {
        let warning = if !ask.sensitive.is_empty() {
            Some(if ask.full_access {
                "Sensitive · review the submitted command."
            } else {
                "Sensitive access — review the submitted command."
            })
        } else if !ask.reach.is_empty() {
            Some(if ask.full_access {
                "Outside workspace · review the submitted command."
            } else {
                "Outside workspace — review the submitted command."
            })
        } else if ask.full_access || ask.reason.is_some() {
            Some("Unconfined — review the submitted command.")
        } else {
            None
        };
        let title = warning
            .map(|warning| format!("Bash {MUTED}• {REMOVED}{warning}{RESET}"))
            .unwrap_or_else(|| "Bash".into());
        let commands = steps::commands(tool);
        for command in commands {
            let rows =
                super::highlight::source_rows("bash", &command, width.saturating_sub(2).max(1));
            for (index, row) in rows.into_iter().enumerate() {
                let prefix = if index == 0 {
                    format!("{ACCENT}${RESET} ")
                } else {
                    "  ".into()
                };
                body.push(format!("{prefix}{row}"));
            }
        }
        return (title, body);
    }
    // Scope is part of the request, not decoration: put the engine's warnings
    // before a command that might otherwise hide them for many screenfuls.
    if let Some(reason) = &ask.reason {
        wrapped(&mut body, reason, REMOVED, width);
    }
    if !ask.reach.is_empty() {
        wrapped(&mut body, "reaches outside the workspace", REMOVED, width);
        for path in &ask.reach {
            wrapped(&mut body, path, TEXT, width);
        }
    }
    if !ask.sensitive.is_empty() {
        wrapped(
            &mut body,
            "never refused — yours to open, never remembered",
            REMOVED,
            width,
        );
        for why in &ask.sensitive {
            wrapped(&mut body, why, TEXT, width);
        }
    }
    if ask.full_access {
        wrapped(
            &mut body,
            "this call runs on the host with no jail or sandbox",
            REMOVED,
            width,
        );
    }

    if let Some(tool) = tool.filter(|t| matches!(t.name.as_str(), "create_file" | "patch_file")) {
        let path = tool.progress.path.as_deref().unwrap_or("?");
        let path = ansi::plain(&ansi::tame(path)).replace('\n', " ");
        let title = if tool.name == "patch_file" {
            format!(
                "Patch {path} {REMOVED}-{}{RESET} {}+{}{RESET}",
                tool.progress.removed,
                super::ansi::ADDED,
                tool.progress.added
            )
        } else {
            format!(
                "Create {path} {}+{}{RESET}",
                super::ansi::ADDED,
                tool.progress.added
            )
        };
        // The edge has only one row. A path that cannot fit there remains
        // readable in full in the scrollable document, never just an ellipsis.
        if ansi::width(&ansi::tame(&title)) > width.saturating_sub(1) {
            wrapped(&mut body, &title, TEXT, width);
        }
        for bullet in steps::bullets(tool) {
            if let Some(content) = bullet.body {
                let (language, lines) = draw::file_body_lines(&content, &bullet.target);
                for line in lines {
                    if matches!(content, steps::Body::Code(_)) {
                        body.extend(super::highlight::source_rows(language, &line, width));
                    } else {
                        let ink = match line.as_bytes().first() {
                            Some(b'+') => super::ansi::ADDED,
                            Some(b'-') => REMOVED,
                            _ => MUTED,
                        };
                        wrapped(&mut body, &line, ink, width);
                    }
                }
            }
        }
        return (title, body);
    }

    let requested = if ask.preview.trim().is_empty() {
        &ask.action
    } else {
        &ask.preview
    };
    body.extend(
        ansi::wrap(&ansi::tame(requested), width)
            .into_iter()
            .map(|line| format!("{TEXT}{BOLD}{line}{RESET}")),
    );
    ("permission".into(), body)
}

fn answer_button(label: &str, chosen: bool, refusal: bool) -> String {
    if chosen {
        let ink = if refusal { REMOVED } else { ACCENT };
        format!("{REVERSE}{ink} {label} {RESET}")
    } else {
        format!("{MUTED} {label} {RESET}")
    }
}

/// Buttons wrap as units, not a clipped sentence: even a narrow card offers a
/// visible refusal. The ranges stored beside them are the only click layout.
fn answer_rows(ask: &Ask, choice: usize, width: usize, state: &mut State) -> Vec<String> {
    let answers = ask.answers();
    let mut rows = vec![String::new()];
    state.answers = vec![Vec::new()];
    let mut column = 0;
    for (index, (label, _)) in answers.iter().enumerate() {
        let label = ansi::plain(&ansi::tame(label)).replace('\n', " ");
        let label = ansi::truncate(&label, width.saturating_sub(2));
        let button_width = ansi::width(&label) + 2;
        if column > 0 && column + 2 + button_width > width {
            rows.push(String::new());
            state.answers.push(Vec::new());
            column = 0;
        }
        if column > 0 {
            rows.last_mut().unwrap().push_str("  ");
            column += 2;
        }
        rows.last_mut().unwrap().push_str(&answer_button(
            &label,
            index == choice.min(answers.len() - 1),
            index + 1 == answers.len(),
        ));
        state
            .answers
            .last_mut()
            .unwrap()
            .push((column..column + button_width, index));
        column += button_width;
    }
    rows
}

#[cfg(test)]
pub(super) fn answers_row(ask: &Ask, choice: usize) -> String {
    answer_rows(ask, choice, usize::MAX, &mut State::default()).join("\n")
}

pub(super) fn render(
    ask: &Ask,
    choice: usize,
    cols: usize,
    rows: usize,
    tool: Option<&Tool>,
    state: &mut State,
) -> Sink {
    if state.req != ask.req {
        state.reset();
        state.req.clone_from(&ask.req);
    }
    let inner = cols.saturating_sub(4).max(1);
    let tool_id = tool.map(|tool| tool.id.as_str());
    if state
        .document
        .as_ref()
        .is_none_or(|document| document.width != inner || document.tool.as_deref() != tool_id)
    {
        let (title, body) = document(ask, tool, inner);
        state.document = Some(Document {
            width: inner,
            tool: tool_id.map(String::from),
            title,
            body,
        });
    }
    let answers = answer_rows(ask, choice, inner, state);
    let document = state.document.as_ref().unwrap();
    let title = &document.title;
    let body = &document.body;
    // The cap includes ALL chrome and body rows, not just a file preview.
    // Leave a transcript row and the status line even in a very short window.
    let compact = rows <= 10;
    let chrome = 2 + answers.len() + 2 * usize::from(!compact);
    let cap = super::viewport::card_rows(
        (rows / 2).max(chrome + 3),
        chrome + 3,
        rows.saturating_sub(2),
    );
    let room = cap.saturating_sub(chrome).max(1);
    let total = body.len();
    state.max = if total <= room {
        0
    } else {
        total.saturating_sub(room.saturating_sub(1).max(1))
    };
    state.scroll = state.scroll.min(state.max);
    let above = state.scroll;
    let mut out = Sink::default();
    let title_room = cols.saturating_sub(5);
    let title = ansi::tame(title).replace('\n', " ");
    let title = if room == 1 && total > room {
        let marker = format!(" ↑{above} ↓{}", total.saturating_sub(above + 1));
        format!(
            "{}{}",
            ansi::truncate(&title, title_room.saturating_sub(ansi::width(&marker))),
            marker
        )
    } else {
        ansi::truncate(&title, title_room)
    };
    out.push(draw::box_top(&title, cols));

    let (take, below) = if room >= 3 {
        let capacity = room - usize::from(above > 0);
        let below = above + capacity < total;
        let take = capacity - usize::from(below);
        (take.min(total.saturating_sub(above)), below)
    } else {
        (
            room.saturating_sub(usize::from(total > room && room > 1))
                .max(1),
            false,
        )
    };
    state.visible = take;
    let left = total.saturating_sub(above + take);
    if room < 3 && total > room && room > 1 {
        out.push(draw::boxed(
            &format!("{MUTED}↑ {above} · ↓ {left} rows · wheel here{RESET}"),
            inner,
        ));
    } else if above > 0 && room >= 3 {
        out.push(draw::boxed(
            &format!("{MUTED}↑ {above} rows above · wheel here{RESET}"),
            inner,
        ));
    }
    for line in body.iter().skip(above).take(take) {
        out.push(draw::boxed(line, inner));
    }
    if below {
        out.push(draw::boxed(
            &format!("{MUTED}↓ {left} rows below · wheel here{RESET}"),
            inner,
        ));
    }
    if !compact {
        out.push(draw::box_divider(cols));
    }
    for (row, line) in answers.into_iter().enumerate() {
        out.push_at(draw::boxed(&line, inner), Hit::Answer(row));
    }
    if !compact {
        out.push(draw::boxed(
            &format!("{FAINT}click an answer · ← → choose · enter answer · esc refuse{RESET}"),
            inner,
        ));
    }
    out.push(draw::box_bottom(cols));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ask() -> Ask {
        Ask {
            req: "r".into(),
            action: "ask".into(),
            preview: "request".into(),
            remember_as: Some("tool".into()),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        }
    }

    fn text(sink: &Sink) -> String {
        sink.lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_permission_component_shares_one_bounded_scrollable_document() {
        for component in ["preview", "action", "reason", "reach", "sensitive"] {
            let mut ask = ask();
            let long = (0..200)
                .map(|i| format!("{component} row {i}\n"))
                .collect::<String>();
            match component {
                "preview" => ask.preview = long,
                "action" => {
                    ask.preview.clear();
                    ask.action = long;
                }
                "reason" => ask.reason = Some(long),
                "reach" => ask.reach = vec![long],
                _ => ask.sensitive = vec![long],
            }
            for rows in [8, 10, 16, 24, 40, 80] {
                for cols in [20, 40, 80, 150] {
                    let mut state = State::default();
                    let card = render(&ask, 0, cols, rows, None, &mut state);
                    assert!(
                        card.len() <= rows - 2,
                        "{component} {rows}x{cols}: {}",
                        card.len()
                    );
                    let first = text(&card);
                    assert!(
                        first.contains("Allow once") && first.contains("Refuse"),
                        "{first}"
                    );
                    assert!(!first.contains("lines cut") && !first.contains("more lines"));
                    assert!(state.max > 0);
                    for line in &card.lines {
                        assert_eq!(ansi::width(line), cols);
                    }
                    state.scroll_by(false, usize::MAX);
                    let end = render(&ask, 0, cols, rows, None, &mut state);
                    assert!(text(&end).contains("Refuse"));
                    assert!(!text(&end).contains("rows below"));
                }
            }
        }
    }

    #[test]
    fn create_and_patch_use_their_own_title_without_nested_code_or_diff_boxes() {
        for (name, title, args) in [
            (
                "create_file",
                "Create demo.js +120",
                json!({"path":"demo.js", "text": "line\n".repeat(120)}),
            ),
            (
                "patch_file",
                "Patch demo.js -120 +120",
                json!({"path":"demo.js", "old_str": "old\n".repeat(120), "new_str": "new\n".repeat(120)}),
            ),
        ] {
            let mut tool = Tool::new("t".into(), name.into(), "write".into());
            tool.settle(&args);
            let mut state = State::default();
            let card = render(&ask(), 0, 80, 24, Some(&tool), &mut state);
            let first = text(&card);
            assert!(first.contains(title), "{first}");
            assert_eq!(first.matches('╭').count(), 1, "{first}");
            assert!(
                !first.contains("permission") && !first.contains("diff") && !first.contains("(120")
            );
            assert!(first.contains("rows below"));
            state.scroll = state.max;
            let end = text(&render(&ask(), 0, 80, 24, Some(&tool), &mut state));
            assert!(
                end.contains("rows above") && !end.contains("rows below"),
                "{end}"
            );
        }
    }

    #[test]
    fn file_counts_are_coloured_and_create_source_is_highlighted_but_patch_is_only_a_diff() {
        let mut create = Tool::new("c".into(), "create_file".into(), "create".into());
        create.settle(&json!({"path":"demo.js", "text":"const value = 'hello';\n"}));
        let card = render(&ask(), 0, 100, 40, Some(&create), &mut State::default());
        assert!(card.lines[0].contains(&format!("{}+1", super::super::ansi::ADDED)));
        assert!(card
            .lines
            .iter()
            .any(|line| line.contains(&super::super::ansi::CODE_KEYWORD.to_string())));
        let mut patch = Tool::new("p".into(), "patch_file".into(), "patch".into());
        patch.settle(
            &json!({"path":"demo.js", "old_str":"const old = 1;", "new_str":"const new = 2;"}),
        );
        let card = render(&ask(), 0, 100, 40, Some(&patch), &mut State::default());
        assert!(card.lines[0].contains(&format!("{REMOVED}-1")));
        assert!(card.lines[0].contains(&format!("{}+1", super::super::ansi::ADDED)));
        assert!(!card
            .lines
            .iter()
            .any(|line| line.contains(&super::super::ansi::CODE_KEYWORD.to_string())));
    }

    #[test]
    fn bash_permission_has_command_only_and_one_concise_warning_when_reach_is_real() {
        let mut tool = Tool::new("t".into(), "terminal_use".into(), "run".into());
        tool.settle(&json!({"command":"if true; then echo 'hello'; fi"}));
        let card = render(&ask(), 0, 100, 40, Some(&tool), &mut State::default());
        assert!(ansi::plain(&card.lines[0]).contains("Bash"));
        assert!(!text(&card).contains("outside workspace"));
        assert!(card
            .lines
            .iter()
            .any(|line| line.contains(&super::super::ansi::CODE_KEYWORD.to_string())));
        let mut outside = ask();
        outside.reach = vec!["/tmp/report".into()];
        let card = render(&outside, 0, 100, 40, Some(&tool), &mut State::default());
        assert!(ansi::plain(&card.lines[0])
            .contains("Bash • Outside workspace — review the submitted command."));
        assert!(!text(&card).contains("reaches outside") && !text(&card).contains("/tmp/report"));
        assert!(text(&card).contains("Allow once") && text(&card).contains("Refuse"));
    }

    #[test]
    fn all_rows_remain_reachable_and_a_new_request_resets_the_viewport() {
        let mut ask = ask();
        ask.preview = (0..70).map(|i| format!("unique-{i:03}\n")).collect();
        let mut state = State::default();
        let mut seen = String::new();
        loop {
            seen.push_str(&text(&render(&ask, 0, 80, 24, None, &mut state)));
            if state.scroll == state.max {
                break;
            }
            state.scroll_by(false, 1);
        }
        for i in 0..70 {
            assert!(seen.contains(&format!("unique-{i:03}")));
        }
        ask.req = "next".into();
        render(&ask, 0, 80, 24, None, &mut state);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn untrusted_paths_and_grant_keys_cannot_break_box_titles_or_answer_rows() {
        let mut ask = ask();
        ask.remember_as = Some("shell\n\x1b[2Jinjected".into());
        let mut tool = Tool::new("t".into(), "create_file".into(), "write".into());
        tool.settle(&json!({"path":"demo\n\x1b[8;999;999t.js", "text":"hello"}));
        let card = render(&ask, 0, 60, 24, Some(&tool), &mut State::default());
        for row in &card.lines {
            assert!(!row.contains('\n') && !row.contains('\r'), "{row:?}");
            assert!(!row.contains("\x1b[2J") && !row.contains("\x1b[8;"));
            assert_eq!(ansi::width(row), 60);
        }
        assert!(text(&card).contains("Refuse"));
    }

    #[test]
    fn resizing_a_scrolled_card_rewraps_its_document_and_retains_visible_answers() {
        let mut ask = ask();
        ask.preview = "long command argument ".repeat(100);
        let mut state = State::default();
        render(&ask, 0, 120, 40, None, &mut state);
        state.scroll = state.max;
        for (cols, rows) in [(40, 16), (20, 8), (100, 24)] {
            let card = render(&ask, 0, cols, rows, None, &mut state);
            assert!(card.len() <= rows - 2);
            assert_eq!(state.document.as_ref().unwrap().width, cols - 4);
            assert!(state.scroll <= state.max);
            assert!(text(&card).contains("Refuse"));
            for line in &card.lines {
                assert_eq!(ansi::width(line), cols);
            }
        }
    }

    #[test]
    fn wrapped_answer_buttons_use_the_same_ranges_for_drawing_and_clicks() {
        let mut state = State::default();
        let card = render(&ask(), 2, 24, 16, None, &mut state);
        let plain = text(&card);
        assert!(plain.contains("Refuse"));
        for (row, ranges) in state.answers.iter().enumerate() {
            for (range, answer) in ranges {
                assert_eq!(state.answer_at(row, range.start as u16 + 3), Some(*answer));
                assert_eq!(state.answer_at(row, range.end as u16 + 2), Some(*answer));
            }
        }
        assert_eq!(state.answers.len(), 3);
    }
}
