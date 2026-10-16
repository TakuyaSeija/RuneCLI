//! Clarification forms occupy the composer, using Rune's existing ink and editor.
use super::{
    ansi::{self, ACCENT, BOLD, FAINT, MUTED, RESET, REVERSE, TEXT, WARN},
    draw::{self, Hit, Sink},
    input::{Click, Key},
    App,
};
use crate::driver::Driver;
use runeengine::{
    tools::questions::{Question, Reply, Selection, UserAnswer, MAX_CUSTOM_CHARS},
    Command,
};
use std::ops::Range;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Title(usize),
    Option(usize),
    CustomText { option: usize, row: usize },
    Previous,
    Next,
    Submit,
}

struct Draft {
    chosen: Vec<bool>,
    text: String,
    cursor: usize,
    editing: super::editing::State,
    /// The first of the draft's own rows currently on screen. Kept between frames
    /// so the field does not jump while the caret moves inside a long answer.
    first_row: usize,
    confirmed: bool,
}

pub struct State {
    pub req: String,
    questions: Vec<Question>,
    drafts: Vec<Draft>,
    current: usize,
    focus: Target,
    scroll: Option<usize>,
    visible_start: usize,
    visible_count: usize,
    /// How many body rows the modal had the last time it was drawn. The
    /// keyboard needs it to know whether the list has anywhere of its own to
    /// scroll, or whether the wheel should move the transcript instead.
    body_len: usize,
    /// The measure the custom answer's own rows were wrapped at on the last
    /// frame — the renderer's number, so ↑/↓ move the caret across the rows
    /// the eye is actually looking at rather than across a second guess at
    /// where those rows break.
    custom_width: usize,
    hits: Vec<Vec<(Range<usize>, Target)>>,
    text_rows: Vec<super::editor::Row>,
    rendered_question: Option<usize>,
    pub sending: Option<u64>,
    pub error: Option<String>,
    pub esc_armed: bool,
    pub esc_armed_at: Option<Instant>,
}

impl State {
    pub fn new(req: String, questions: Vec<Question>) -> Self {
        Self {
            req,
            // One toggle per option plus the write-your-own row, sized to the
            // question it serves: options are not capped, so nothing here may
            // assume a maximum.
            drafts: questions
                .iter()
                .map(|q| Draft {
                    chosen: vec![false; q.options.len() + 1],
                    text: String::new(),
                    cursor: 0,
                    editing: super::editing::State::default(),
                    first_row: 0,
                    confirmed: false,
                })
                .collect(),
            questions,
            current: 0,
            focus: Target::Option(0),
            scroll: None,
            visible_start: 0,
            visible_count: 1,
            body_len: 0,
            custom_width: 0,
            hits: Vec::new(),
            text_rows: Vec::new(),
            rendered_question: None,
            sending: None,
            error: None,
            esc_armed: false,
            esc_armed_at: None,
        }
    }

    /// Whether the modal's own list is taller than the room it was given, as of
    /// the last frame.
    ///
    /// The keyboard needs this: when the list fits, `↑`/`↓` and the wheel are
    /// free to move the transcript behind the modal — which is the whole point
    /// of asking a question about what is on that transcript. When it does not
    /// fit, the list keeps them, because a list that scrolls away from its own
    /// cursor is a list the user has to chase.
    pub fn overflows(&self) -> bool {
        self.visible_count > 0 && self.body_len > self.visible_count
    }

    /// One step of the card's own scroll — the wheel's gesture and the
    /// PageUp/PageDown gesture away from the custom field. The window is
    /// `visible_start`/`visible_count` of the last frame, so a step moves a
    /// whole window at a time; the renderer clamps and follows from there.
    fn scroll_card(&mut self, up: bool) {
        self.scroll = Some(if up {
            self.visible_start.saturating_sub(self.visible_count)
        } else {
            self.visible_start + self.visible_count
        });
    }

    fn answer(&self, index: usize) -> UserAnswer {
        let draft = &self.drafts[index];
        let q = &self.questions[index];
        let custom_idx = q.options.len();
        UserAnswer {
            id: q.id.clone(),
            selected: q
                .options
                .iter()
                .zip(draft.chosen.iter())
                .filter(|(_, on)| **on)
                .map(|(o, _)| o.id.clone())
                .collect(),
            custom: if draft.chosen[custom_idx] {
                draft.text.clone()
            } else {
                String::new()
            },
        }
    }

    fn complete(&self, index: usize) -> bool {
        let draft = &self.drafts[index];
        let custom_idx = self.questions[index].options.len();
        draft.chosen[..custom_idx].iter().any(|b| *b)
            || (draft.chosen.get(custom_idx).copied().unwrap_or(false)
                && !draft.text.trim().is_empty())
    }

    fn choose(&mut self, index: usize) {
        let draft = &mut self.drafts[self.current];
        draft.confirmed = false;
        let next = !draft.chosen[index];
        if self.questions[self.current].selection == Selection::Single {
            draft.chosen.fill(false);
        }
        draft.chosen[index] = next;
        self.error = None;
    }

    fn navigate(&mut self, forward: bool) {
        if (!forward && self.current == 0) || (forward && self.current + 1 == self.questions.len())
        {
            return;
        }
        self.current = if forward {
            (self.current + 1).min(self.questions.len() - 1)
        } else {
            self.current.saturating_sub(1)
        };
        self.focus = Target::Option(0);
        self.scroll = None;
        self.error = None;
    }

    fn activate(&mut self, target: Target) -> Option<Reply> {
        self.activate_at(target, None)
    }

    fn activate_at(&mut self, target: Target, col: Option<u16>) -> Option<Reply> {
        if self.sending.is_some() {
            return None;
        }
        self.scroll = None;
        match target {
            Target::Title(index) => {
                self.current = index;
                self.focus = Target::Title(index);
            }
            Target::Option(index) => {
                self.focus = target;
                self.choose(index);
            }
            Target::CustomText { option, row } => {
                self.focus = Target::Option(option);
                let draft = &mut self.drafts[self.current];
                draft.confirmed = false;
                self.error = None;
                if self.questions[self.current].selection == Selection::Single {
                    draft.chosen.fill(false);
                }
                draft.chosen[option] = true;
                if let Some(col) = col {
                    if let Some(geometry) = self.text_rows.get(row) {
                        let at = geometry.caret_at(col);
                        if draft.text.is_char_boundary(at) {
                            draft.editing.place_in_row(
                                &draft.text,
                                &mut draft.cursor,
                                geometry,
                                col,
                                self.custom_width,
                            );
                        }
                    }
                }
            }
            Target::Previous => {
                if self.current > 0 {
                    self.current -= 1;
                    self.scroll = None;
                    self.error = None;
                    self.focus = if self.current > 0 {
                        Target::Previous
                    } else {
                        Target::Next
                    };
                }
            }
            Target::Next => {
                if self.complete(self.current) {
                    self.drafts[self.current].confirmed = true;
                }
                if self.current + 1 < self.questions.len() {
                    self.current += 1;
                    self.scroll = None;
                    self.error = None;
                    self.focus = if self.current + 1 < self.questions.len() {
                        Target::Next
                    } else {
                        Target::Submit
                    };
                }
            }
            Target::Submit => {
                let total = self.questions.len();
                return Some(Reply::Answered {
                    answers: (0..total).map(|i| self.answer(i)).collect(),
                });
            }
        }
        None
    }

    fn key(&mut self, key: &Key) -> Option<Reply> {
        if self.sending.is_some() {
            return None;
        }
        let custom_idx = self.questions[self.current].options.len();
        if matches!(key, Key::AltLeft | Key::AltRight) {
            self.navigate(*key == Key::AltRight);
            return None;
        }
        if self.focus == Target::Option(custom_idx) {
            if matches!(key, Key::Up | Key::Down) {
                let draft = &mut self.drafts[self.current];
                let up = *key == Key::Up;
                if draft
                    .editing
                    .vertical_step(&draft.text, &mut draft.cursor, self.custom_width, up)
                    .is_some()
                {
                    self.scroll = None;
                    return None;
                }
                let boundary = if up { 0 } else { draft.text.len() };
                if draft.cursor != boundary {
                    draft
                        .editing
                        .place(&draft.text, &mut draft.cursor, boundary);
                    self.scroll = None;
                    return None;
                }
                // Only an unmodified arrow at the buffer boundary leaves the
                // editor. Selection motion and every other edit stay local.
            } else {
                let draft = &mut self.drafts[self.current];
                let before = draft.text.clone();
                let mut options = super::editing::Options::multiline(self.custom_width);
                options.max_chars = MAX_CUSTOM_CHARS;
                if draft
                    .editing
                    .key(&mut draft.text, &mut draft.cursor, key, options)
                {
                    if draft.text != before {
                        draft.confirmed = false;
                        if self.questions[self.current].selection == Selection::Single {
                            draft.chosen.fill(false);
                        }
                        draft.chosen[custom_idx] = !draft.text.trim().is_empty();
                        self.error = None;
                    }
                    self.scroll = None;
                    return None;
                }
            }
        }
        if matches!(key, Key::PageUp | Key::PageDown) {
            self.scroll_card(*key == Key::PageUp);
            return None;
        }
        self.scroll = None;
        match key {
            Key::Tab | Key::ShiftTab => {
                let mut targets = vec![Target::Title(self.current)];
                for i in 0..=custom_idx {
                    targets.push(Target::Option(i));
                }
                if self.current > 0 {
                    targets.push(Target::Previous);
                }
                if self.current + 1 < self.questions.len() {
                    targets.push(Target::Next);
                }
                targets.push(Target::Submit);

                let at = targets.iter().position(|t| *t == self.focus).unwrap_or(0);
                let next = if *key == Key::Tab {
                    (at + 1) % targets.len()
                } else {
                    (at + targets.len() - 1) % targets.len()
                };
                self.focus = targets[next];
            }
            Key::Up => {
                self.focus = match self.focus {
                    Target::Option(0) => Target::Title(self.current),
                    Target::Option(i) => Target::Option(i - 1),
                    Target::Previous => Target::Option(custom_idx),
                    Target::Next => {
                        if self.current > 0 {
                            Target::Previous
                        } else {
                            Target::Option(custom_idx)
                        }
                    }
                    Target::Submit => {
                        if self.current + 1 < self.questions.len() {
                            Target::Next
                        } else if self.current > 0 {
                            Target::Previous
                        } else {
                            Target::Option(custom_idx)
                        }
                    }
                    _ => Target::Title(self.current),
                };
            }
            Key::Down => {
                self.focus = match self.focus {
                    Target::Title(_) => Target::Option(0),
                    Target::Option(i) if i == custom_idx => {
                        if self.current + 1 < self.questions.len() {
                            Target::Next
                        } else {
                            Target::Submit
                        }
                    }
                    Target::Option(i) => Target::Option(i + 1),
                    Target::Previous => {
                        if self.current + 1 < self.questions.len() {
                            Target::Next
                        } else {
                            Target::Submit
                        }
                    }
                    Target::Next => Target::Submit,
                    Target::Submit => Target::Submit,
                    Target::CustomText { .. } => Target::Option(custom_idx),
                };
            }
            Key::Enter => match self.focus {
                Target::Option(i) if i < custom_idx => {
                    self.choose(i);
                }
                Target::Option(i) if i == custom_idx => {
                    if self.drafts[self.current].text.trim().is_empty() {
                        return None;
                    }
                    self.drafts[self.current].confirmed = true;
                    if self.current + 1 < self.questions.len() {
                        self.navigate(true);
                        self.focus = Target::Option(0);
                    } else {
                        self.focus = Target::Submit;
                    }
                }
                target => return self.activate(target),
            },
            Key::Char(' ') if matches!(self.focus, Target::Option(i) if i < custom_idx) => {
                if let Target::Option(i) = self.focus {
                    self.choose(i);
                }
            }
            Key::Left => match self.focus {
                Target::Submit => {
                    if self.current + 1 < self.questions.len() {
                        self.focus = Target::Next;
                    } else if self.current > 0 {
                        self.focus = Target::Previous;
                    }
                }
                Target::Next => {
                    if self.current > 0 {
                        self.focus = Target::Previous;
                    }
                }
                Target::Previous => {}
                _ => {}
            },
            Key::Right => match self.focus {
                Target::Previous => {
                    if self.current + 1 < self.questions.len() {
                        self.focus = Target::Next;
                    } else {
                        self.focus = Target::Submit;
                    }
                }
                Target::Next => {
                    self.focus = Target::Submit;
                }
                Target::Submit => {}
                _ => {}
            },
            _ => {}
        }
        None
    }
}

/// The wheel over the card, routed here by the keyboard only when the pointer
/// is actually on the card's rows — a card that overflows owns the wheel where
/// it stands, and nowhere else. Kept apart from `key` because PageUp/PageDown
/// on the keyboard now belong to the caret while the custom field is focused:
/// the wheel and the key are different gestures about different things, and
/// one must not borrow the other's meaning.
pub fn wheel(app: &mut App, up: bool, multiplier: usize) {
    let Some(state) = app.questions.as_mut() else {
        return;
    };
    let rows = state.visible_count.saturating_mul(multiplier);
    state.scroll = Some(if up {
        state.visible_start.saturating_sub(rows)
    } else {
        state.visible_start.saturating_add(rows)
    });
}

pub fn key(app: &mut App, key: &Key, driver: &mut Driver) {
    if matches!(key, Key::Esc) {
        if let Some(state) = &mut app.questions {
            let now = Instant::now();
            let armed = state.esc_armed
                && state
                    .esc_armed_at
                    .is_some_and(|at| now.duration_since(at) <= Duration::from_secs(3));
            if armed {
                state.esc_armed = false;
                state.esc_armed_at = None;
                let req = state.req.clone();
                let Some(session) = app.session else {
                    return;
                };
                driver.ask(|id| Command::QuestionsReply {
                    id,
                    session,
                    req,
                    reply: Reply::Cancelled,
                });
                driver.ask(|id| Command::Cancel {
                    id,
                    session,
                    call: None,
                });
            } else {
                state.esc_armed = true;
                state.esc_armed_at = Some(now);
                app.note("press esc again to cancel");
            }
        }
        return;
    }

    if matches!(key, Key::Ctrl('c')) {
        if let Some(state) = &mut app.questions {
            let req = state.req.clone();
            let Some(session) = app.session else {
                return;
            };
            driver.ask(|id| Command::QuestionsReply {
                id,
                session,
                req,
                reply: Reply::Cancelled,
            });
            driver.ask(|id| Command::Cancel {
                id,
                session,
                call: None,
            });
        }
        return;
    }

    let was_esc_armed = app.questions.as_ref().map(|q| q.esc_armed).unwrap_or(false);
    if was_esc_armed {
        app.clear_status();
    }
    let Some(state) = &mut app.questions else {
        return;
    };
    state.esc_armed = false;
    state.esc_armed_at = None;
    if let Key::Mouse(mouse) = key {
        match mouse.kind {
            Click::Release => {
                if let Hit::Questions(r) = draw::hit_at(app, mouse.row) {
                    click(app, r, mouse.col, driver);
                }
            }
            Click::WheelUp | Click::WheelDown => {
                wheel(
                    app,
                    mouse.kind == Click::WheelUp,
                    mouse.modifiers.wheel_multiplier(),
                );
            }
            _ => {}
        }
        return;
    }
    let reply = state.key(key);
    if let Some(reply) = reply {
        let state = app.questions.as_mut().unwrap();
        let req = state.req.clone();
        let Some(session) = app.session else {
            return;
        };
        state.sending = Some(driver.ask(|id| Command::QuestionsReply {
            id,
            session,
            req,
            reply,
        }));
    }
}

pub fn click(app: &mut App, hit_row: usize, col: u16, driver: &mut Driver) {
    if app
        .questions
        .as_ref()
        .is_none_or(|s| s.rendered_question != Some(s.current))
    {
        return;
    }
    let target = app
        .questions
        .as_ref()
        .and_then(|s| s.hits.get(hit_row))
        .and_then(|hits| {
            hits.iter()
                .find(|(range, _)| range.contains(&(col as usize)))
        })
        .map(|(_, target)| *target);
    let reply = target.and_then(|target| {
        app.questions
            .as_mut()
            .unwrap()
            .activate_at(target, Some(col))
    });
    if let Some(reply) = reply {
        let state = app.questions.as_mut().unwrap();
        let req = state.req.clone();
        let Some(session) = app.session else {
            return;
        };
        state.sending = Some(driver.ask(|id| Command::QuestionsReply {
            id,
            session,
            req,
            reply,
        }));
    }
}

pub(super) fn select(app: &mut App, from: (usize, u16), to: (usize, u16)) -> bool {
    let Some(state) = app
        .questions
        .as_mut()
        .filter(|s| s.rendered_question == Some(s.current) && s.sending.is_none())
    else {
        return false;
    };
    let point = |(row, col): (usize, u16)| {
        state.hits.get(row)?.iter().find_map(|(range, target)| {
            if !range.contains(&(col as usize)) {
                return None;
            }
            match target {
                Target::CustomText { option, row } => {
                    Some((*option, state.text_rows.get(*row)?.caret_at(col), *row))
                }
                _ => None,
            }
        })
    };
    let Some((option, start, _)) = point(from) else {
        return false;
    };
    let Some((other, _, row)) = point(to) else {
        return false;
    };
    if option != other {
        return false;
    }
    state.focus = Target::Option(option);
    let draft = &mut state.drafts[state.current];
    draft.editing.select_in_row(
        &draft.text,
        &mut draft.cursor,
        start,
        &state.text_rows[row],
        to.1,
        state.custom_width,
    );
    true
}

struct Row {
    text: String,
    controls: Vec<(Range<usize>, Target)>,
    focus: bool,
    cursor: Option<usize>,
}

impl Row {
    fn new(text: String, target: Option<Target>, focus: Target, width: usize) -> Self {
        Self {
            text,
            controls: target.map(|t| vec![(3..width + 3, t)]).unwrap_or_default(),
            focus: target == Some(focus),
            cursor: None,
        }
    }
}

pub fn render(state: &mut State, cols: usize, height: usize) -> (Sink, Option<(usize, usize)>) {
    state.rendered_question = Some(state.current);
    let height = super::viewport::card_rows(height, 6, height);
    let inner = cols.saturating_sub(4).max(1);
    let total_q = state.questions.len();

    // Budget width per tab so all tabs fit within `inner` without any being dropped.
    let per_tab_budget = inner
        .checked_div(total_q)
        .map(|n| n.max(10))
        .unwrap_or(inner);

    let mut tabs_row = Row::new(String::new(), None, state.focus, inner);
    for i in 0..total_q {
        let title = &state.questions[i].title;
        let is_current = i == state.current;
        let is_complete = state.complete(i);
        let is_focused = state.focus == Target::Title(i);

        let title_text = format!("{}. {}", i + 1, title);
        let max_text_w = per_tab_budget.saturating_sub(4).max(4);
        let truncated = ansi::truncate(&title_text, max_text_w);
        let tab_w = ansi::width(&truncated) + 4;

        let start_col = ansi::width(&tabs_row.text) + 3;
        let tab_str = if is_focused {
            format!("{REVERSE}{ACCENT}  {truncated}  {RESET}")
        } else if is_current {
            format!("{ACCENT}{BOLD}[ {truncated} ]{RESET}")
        } else if is_complete {
            format!("{TEXT}  {truncated}  {RESET}")
        } else {
            format!("{MUTED}  {truncated}  {RESET}")
        };

        tabs_row.text.push_str(&tab_str);
        tabs_row
            .controls
            .push((start_col..start_col + tab_w, Target::Title(i)));
        tabs_row.focus |= is_focused;
    }

    let mut body = Vec::new();
    let question = &state.questions[state.current];

    for line in ansi::wrap(&question.question, inner) {
        body.push(Row::new(
            format!("{TEXT}{line}{RESET}"),
            None,
            state.focus,
            inner,
        ));
    }

    let hint_text = if question.selection == Selection::Multiple {
        "Select one or more options. Press Space to toggle."
    } else {
        "Select one option or write your own."
    };
    for line in ansi::wrap(hint_text, inner) {
        body.push(Row::new(
            format!("{FAINT}{line}{RESET}"),
            None,
            state.focus,
            inner,
        ));
    }
    body.push(Row::new(String::new(), None, state.focus, inner));

    let custom_idx = question.options.len();
    for i in 0..=custom_idx {
        let focused = state.focus == Target::Option(i);
        let on = state.drafts[state.current].chosen[i];

        let caret = if focused { "›" } else { " " };
        let caret_color = if focused { ACCENT } else { MUTED };

        let mark = if question.selection == Selection::Multiple {
            if on {
                "[✓]"
            } else {
                "[ ]"
            }
        } else if on {
            "●"
        } else {
            "○"
        };
        let mark_ink = if on { ACCENT } else { MUTED };
        let weight = if focused { BOLD } else { "" };

        if i < custom_idx {
            let num = format!("{}.", i + 1);
            let raw_label = question.options[i].label.trim();
            let (is_rec, clean_label) = if let Some(rest) = raw_label.strip_prefix("(Recommended) ")
            {
                (true, rest.trim())
            } else if let Some(rest) = raw_label.strip_prefix("(Recommended)") {
                (true, rest.trim())
            } else if let Some(rest) = raw_label.strip_suffix(" (Recommended)") {
                (true, rest.trim())
            } else if let Some(rest) = raw_label.strip_suffix("(Recommended)") {
                (true, rest.trim())
            } else {
                (false, raw_label)
            };

            let rec_suffix = if is_rec {
                format!(" {TEXT}{weight}(Recommended)")
            } else {
                String::new()
            };

            let title_line = format!(
                "{caret_color}{caret} {MUTED}{num} {mark_ink}{mark} {TEXT}{weight}{clean_label}{rec_suffix}{RESET}"
            );
            for (line_index, line) in ansi::wrap(&title_line, inner).into_iter().enumerate() {
                let mut row = Row::new(line, Some(Target::Option(i)), state.focus, inner);
                row.focus = focused && line_index == 0;
                body.push(row);
            }

            for line in ansi::wrap(
                &question.options[i].description,
                inner.saturating_sub(6).max(1),
            ) {
                let mut row = Row::new(
                    format!("      {MUTED}{line}{RESET}"),
                    Some(Target::Option(i)),
                    state.focus,
                    inner,
                );
                row.focus = false;
                body.push(row);
            }
            body.push(Row::new(String::new(), None, state.focus, inner));
        } else {
            let num = format!("{}.", custom_idx + 1);
            let label = "Write your own";

            let title_line = format!(
                "{caret_color}{caret} {MUTED}{num} {mark_ink}{mark} {TEXT}{weight}{label}{RESET}"
            );
            for (line_index, line) in ansi::wrap(&title_line, inner).into_iter().enumerate() {
                let mut row = Row::new(line, Some(Target::Option(custom_idx)), state.focus, inner);
                row.focus = focused && line_index == 0;
                body.push(row);
            }

            // A real multi-line field, laid out exactly as the composer lays out
            // one: rows are wrapped at the field's own measure, the caret is
            // always on screen, and the window follows it once the answer is
            // taller than the rows this form is allowed to take. The measure
            // is published for the keyboard — ↑/↓ must walk the rows the eye
            // is looking at, not a second guess at where they break.
            const MAX_CUSTOM_ROWS: usize = 4;
            state.custom_width = inner.saturating_sub(6).max(1);
            let projection = super::editor::Projection::plain(&state.drafts[state.current].text);
            let at = projection.display_at(state.drafts[state.current].cursor);
            let (first, drawn, here, rows) = {
                let draft = &state.drafts[state.current];
                let rows = draw::compose_rows(&projection.text, state.custom_width);
                let here = draft.editing.row_index(
                    &draft.text,
                    draft.cursor,
                    state.custom_width,
                    &rows,
                    at,
                );
                let max_first = rows.len().saturating_sub(MAX_CUSTOM_ROWS);
                let first = draft.first_row.min(max_first);
                let first = if here < first {
                    here
                } else if here >= first + MAX_CUSTOM_ROWS {
                    here + 1 - MAX_CUSTOM_ROWS
                } else {
                    first
                };
                let drawn = rows.len().saturating_sub(first).clamp(1, MAX_CUSTOM_ROWS);
                (first, drawn, here, rows)
            };
            state.drafts[state.current].first_row = first;
            state.text_rows = rows
                .iter()
                .map(|(start, line)| projection.row(*start, line, 9))
                .collect();
            for n in 0..drawn {
                let line = rows.get(first + n).map(|(_, l)| l.as_str()).unwrap_or("");
                let text =
                    if line.is_empty() && n == 0 && state.drafts[state.current].text.is_empty() {
                        format!("{FAINT}Type your answer…{RESET}")
                    } else {
                        format!(
                            "{TEXT}{}{RESET}",
                            state.text_rows[first + n].styled(
                                state.drafts[state.current].editing.selected(
                                    &state.drafts[state.current].text,
                                    state.drafts[state.current].cursor
                                )
                            )
                        )
                    };
                let mut row = Row::new(
                    format!("      {text}"),
                    Some(Target::CustomText {
                        option: custom_idx,
                        row: first + n,
                    }),
                    state.focus,
                    inner,
                );
                if let Some((range, _)) = row.controls.first_mut() {
                    *range = 3..cols;
                }
                row.focus = focused && first + n == here;
                if focused && first + n == here {
                    let (start, line) = &rows[here];
                    let cut = ansi::caret_in_row(line, *start, at);
                    row.cursor = Some(9 + ansi::width(&line[..cut]));
                }
                body.push(row);
            }
        }
    }

    let mut nav = Row::new(String::new(), None, state.focus, inner);
    let labels = if inner < 42 {
        ["< Back", "Next >", "Submit"]
    } else {
        ["< Previous", "Next >", "Submit answers"]
    };

    let can_prev = state.current > 0;
    let can_next = state.current + 1 < state.questions.len();
    let total_q = state.questions.len();
    let answered_q = (0..total_q).filter(|i| state.complete(*i)).count();
    let can_submit = answered_q == total_q;

    let submit_lbl = if can_submit {
        if inner < 42 {
            "Submit".to_string()
        } else {
            "Submit answers".to_string()
        }
    } else if inner < 42 {
        format!("Submit ({answered_q}/{total_q})")
    } else {
        format!("Submit answers ({answered_q}/{total_q})")
    };

    let btn_configs = [
        (Target::Previous, labels[0], can_prev),
        (Target::Next, labels[1], can_next),
        (Target::Submit, submit_lbl.as_str(), can_submit),
    ];

    let mut formatted_buttons = Vec::new();
    for (target, lbl, enabled) in btn_configs {
        let is_focused = state.focus == target;
        let btn_str = if is_focused {
            format!("{REVERSE}{ACCENT}  {lbl}  {RESET}")
        } else if target == Target::Submit || enabled {
            format!("{TEXT}  {lbl}  {RESET}")
        } else {
            format!("{MUTED}  {lbl}  {RESET}")
        };
        let visible_width = ansi::width(lbl) + 4;
        formatted_buttons.push((btn_str, visible_width, target));
    }

    let total_btn_w: usize = formatted_buttons.iter().map(|(_, w, _)| *w).sum();
    let gap = if inner >= total_btn_w + 10 {
        4
    } else if inner >= total_btn_w + 6 {
        3
    } else if inner >= total_btn_w + 4 {
        2
    } else {
        1
    };

    let mut current_col = 3;
    for (idx, (btn_str, btn_w, target)) in formatted_buttons.into_iter().enumerate() {
        if idx > 0 {
            nav.text.push_str(&" ".repeat(gap));
            current_col += gap;
        }
        nav.text.push_str(&btn_str);
        nav.controls
            .push((current_col..current_col + btn_w, target));
        current_col += btn_w;
    }

    let hint_str = if let Some(err) = &state.error {
        format!("{WARN}{err}{RESET}")
    } else if state.sending.is_some() {
        format!("{MUTED}Submitting answers…{RESET}")
    } else if state.focus == Target::Option(custom_idx) {
        format!("{FAINT}↑↓ cursor/edge controls · Alt+←/→ questions · Alt+Enter new line · Tab controls · Esc 2x cancel{RESET}")
    } else if state.focus == Target::Submit {
        if can_submit {
            format!("{TEXT}Enter submit answers · ← buttons · ↑ options · Esc 2x cancel{RESET}")
        } else {
            format!("{WARN}Answer all questions to submit ({answered_q}/{total_q} completed) · Esc 2x cancel{RESET}")
        }
    } else if matches!(state.focus, Target::Previous | Target::Next) {
        format!("{FAINT}←→ buttons · Enter navigate question · ↑ back to options · Esc 2x cancel{RESET}")
    } else if matches!(state.focus, Target::Title(_)) {
        format!("{FAINT}Alt+←/→ questions · Enter / ↓ options · Esc 2x cancel{RESET}")
    } else {
        format!("{FAINT}Alt+←/→ questions · ↑↓ options · Space / Enter toggle · Tab buttons · Esc 2x cancel{RESET}")
    };

    let mut out = Sink::default();
    let mut cursor = None;

    let use_dividers = height >= 16;
    let fixed_lines = if use_dividers {
        7
    } else if height >= 8 {
        5
    } else {
        4
    };

    let budget = height.saturating_sub(fixed_lines).max(1);
    let focus_idx = body
        .iter()
        .rposition(|r| r.cursor.is_some())
        .or_else(|| body.iter().position(|r| r.focus))
        .unwrap_or(0);
    let start = state
        .scroll
        .unwrap_or_else(|| focus_idx.saturating_sub(budget.saturating_sub(2)))
        .min(body.len().saturating_sub(budget));
    state.visible_start = start;
    state.visible_count = budget;
    state.body_len = body.len();
    state.hits.clear();

    let answered_q = (0..total_q).filter(|i| state.complete(*i)).count();
    let label = if total_q > 1 {
        if start > 0 || body.len() > budget {
            format!(
                "Questions · {answered_q}/{total_q} answered · {}–{}/{}",
                start + 1,
                (start + budget).min(body.len()),
                body.len()
            )
        } else {
            format!("Questions · {answered_q}/{total_q} answered")
        }
    } else if start > 0 || body.len() > budget {
        format!(
            "Questions · {}–{}/{}",
            start + 1,
            (start + budget).min(body.len()),
            body.len()
        )
    } else {
        "Questions".into()
    };

    out.push(draw::box_top(
        &ansi::truncate(&label, cols.saturating_sub(6)),
        cols,
    ));

    let hit = state.hits.len();
    state.hits.push(tabs_row.controls);
    out.push_at(
        draw::boxed(&ansi::truncate(&tabs_row.text, inner), inner),
        Hit::Questions(hit),
    );

    if use_dividers {
        out.push(draw::box_divider(cols));
    }

    for row in body.into_iter().skip(start).take(budget) {
        if let Some(col) = row.cursor {
            // The cell just before the right border is the blank caret cell
            // AFTER a full-width answer. Clamping to cols-2 moved it onto the
            // final character (xxxxx| became xxxx|x).
            cursor = Some((out.lines.len(), col.min(cols.saturating_sub(1))));
        }
        let hit = state.hits.len();
        state.hits.push(row.controls);
        out.push_at(
            draw::boxed(&ansi::truncate(&row.text, inner), inner),
            Hit::Questions(hit),
        );
    }

    if use_dividers {
        out.push(draw::box_divider(cols));
    }

    let hit = state.hits.len();
    state.hits.push(nav.controls);
    out.push_at(draw::boxed(&nav.text, inner), Hit::Questions(hit));

    if height >= 8 {
        out.push(draw::boxed(
            &ansi::truncate(&format!(" {hint_str}"), inner),
            inner,
        ));
    }

    out.push(draw::box_bottom(cols));
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runeengine::perm::broker::Response;
    use serde_json::json;

    #[test]
    fn question_panel_owns_input_and_mouse_without_losing_the_composer_draft() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        let mut app = App::for_test();
        let mut driver = super::super::test_driver("question-composer");
        app.input = "Keep this draft".into();
        app.cursor = app.input.len();
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        runeengine::tools::questions::prepare(&mut questions);
        app.on_event(runeengine::Event::QuestionsRequest {
            session: app.session.unwrap(),
            id: "tool1".into(),
            req: "p1".into(),
            action: "Preferences".into(),
            questions,
        });
        let mode = app.mode.clone();
        app.on_key(Key::Tab, &mut driver);
        assert_eq!(app.mode, mode);
        assert_eq!(app.input, "Keep this draft");
        let state = app.questions.as_mut().unwrap();
        let (sink, _) = render(state, 100, 38);
        let (row, col) = sink
            .owner
            .iter()
            .enumerate()
            .find_map(|(row, hit)| {
                if let Hit::Questions(at) = hit {
                    state.hits[*at]
                        .iter()
                        .find(|(_, target)| *target == Target::Option(2))
                        .map(|(range, _)| (row + 1, range.start))
                } else {
                    None
                }
            })
            .unwrap();
        app.hits = sink.owner;
        app.on_key(
            Key::Mouse(super::super::input::Mouse {
                kind: Click::Release,
                row: row as u16,
                col: col as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );
        assert_eq!(
            app.questions.as_ref().unwrap().answer(0).selected,
            vec!["q0o2"]
        );
        app.on_event(runeengine::Event::QuestionsResolved {
            session: app.session.unwrap(),
            req: "wrong".into(),
        });
        assert!(app.questions.is_some());
        app.on_event(runeengine::Event::QuestionsResolved {
            session: app.session.unwrap(),
            req: "p1".into(),
        });
        assert!(app.questions.is_none());
        assert_eq!(app.input, "Keep this draft");
        assert_eq!(app.cursor, app.input.len());
        driver.engine.shutdown();
    }

    #[test]
    fn navigating_and_editing_preserves_answers_and_requires_explicit_submit() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        runeengine::tools::questions::prepare(&mut questions);
        let mut state = State::new("p1".into(), questions);
        assert!(!state.complete(0));
        state.key(&Key::Char(' '));
        state.key(&Key::Down);
        state.key(&Key::Down);
        state.key(&Key::Char(' '));
        state.key(&Key::Down);
        state.key(&Key::Paste("Only on Wi-Fi".into()));
        assert_eq!(state.answer(0).selected, vec!["q0o0", "q0o2"]);
        state.key(&Key::Enter);
        assert_eq!(state.current, 1);
        state.key(&Key::Enter);
        assert!(state.drafts[1].chosen[0], "Enter selects Option(0)");
        state.key(&Key::Enter);
        assert!(!state.drafts[1].chosen[0], "Enter unselects Option(0)");
        state.activate(Target::Option(3));
        state.key(&Key::Paste("NAS 東京".into()));
        state.key(&Key::Left);
        state.key(&Key::Backspace);
        assert_eq!(state.answer(1).custom, "NAS 京");
        assert!(state.answer(1).selected.is_empty());
        state.key(&Key::Tab);
        state.key(&Key::Enter);
        assert_eq!(state.current, 0);
        assert_eq!(state.answer(0).custom, "Only on Wi-Fi");
        state.focus = Target::Option(0);
        state.key(&Key::Up);
        assert_eq!(state.focus, Target::Title(0));
        state.key(&Key::AltRight);
        assert_eq!(state.current, 1);
        assert_eq!(state.focus, Target::Option(0));
        let reply = state.activate(Target::Submit).unwrap();
        assert!(reply.valid(&state.questions));
        assert!(
            state.sending.is_none(),
            "Editing never implicitly sends a reply"
        );
        state.drafts[1].text.clear();
        let partial_reply = state.activate(Target::Submit).unwrap();
        assert!(partial_reply.valid(&state.questions));
    }

    #[test]
    fn canonical_editing_matrix_keeps_custom_answer_focus_and_actual_answer_content() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        for case in super::super::editing::cases() {
            let mut state = State::new(
                "editing".into(),
                serde_json::from_value(form["questions"].clone()).unwrap(),
            );
            state.activate(Target::Option(3));
            state.drafts[0].text = case.text.into();
            state.drafts[0].cursor = case.at;
            for key in case.keys {
                assert!(state.key(&key).is_none(), "{} submitted", case.name);
            }
            assert_eq!(state.drafts[0].text, case.expected, "{}", case.name);
            assert_eq!(state.focus, Target::Option(3));
            assert!(state.sending.is_none());
        }
    }

    #[test]
    fn custom_answer_leaves_only_at_buffer_edges_and_alt_arrows_switch_questions() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        let mut state = State::new(
            "boundaries".into(),
            serde_json::from_value(form["questions"].clone()).unwrap(),
        );
        state.activate(Target::Option(3));
        state.key(&Key::Up);
        assert_eq!(state.focus, Target::Option(2));
        state.key(&Key::Down);
        state.key(&Key::Down);
        assert_eq!(state.focus, Target::Next);
        state.activate(Target::Option(3));
        state.key(&Key::Paste("abcdefghij".into()));
        render(&mut state, 15, 30);
        state.custom_width = 5;
        state.key(&Key::Up);
        assert_eq!(state.focus, Target::Option(3));
        assert_eq!(state.drafts[0].cursor, 5);
        state.key(&Key::Up);
        assert_eq!(state.drafts[0].cursor, 0);
        state.key(&Key::Up);
        assert_eq!(state.focus, Target::Option(2));
        state.key(&Key::Down);
        state.key(&Key::End);
        state.key(&Key::Down);
        assert_eq!(state.focus, Target::Next);
        state.activate(Target::Option(3));
        state.key(&Key::Char('>'));
        assert!(state.drafts[0].text.ends_with('>'));
        state.key(&Key::AltRight);
        assert_eq!(state.current, 1);
        state.key(&Key::AltLeft);
        assert_eq!(state.current, 0);
        assert_eq!(state.drafts[0].text, "abcdefghij>");
        state.key(&Key::Right);
        assert_eq!(
            state.current, 0,
            "plain arrows are not question-switch aliases"
        );
        state.key(&Key::AltLeft);
        assert_eq!(state.current, 0);
    }

    #[test]
    fn navigation_down_and_pagedown_reaches_action_buttons() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        let mut questions: Vec<Question> =
            serde_json::from_value(form["questions"].clone()).unwrap();
        runeengine::tools::questions::prepare(&mut questions);
        let mut state = State::new("p1".into(), questions);
        for _ in 0..4 {
            state.key(&Key::Tab);
        }
        assert_eq!(state.focus, Target::Next);
        state.key(&Key::Down);
        assert_eq!(state.focus, Target::Submit);
        state.key(&Key::Up);
        assert_eq!(state.focus, Target::Next);
    }

    #[test]
    fn forms_fit_short_and_narrow_terminals_with_visible_edit_cursor_and_click_targets() {
        let form: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/Questions.json")).unwrap();
        let mut state = State::new(
            "p1".into(),
            serde_json::from_value(form["questions"].clone()).unwrap(),
        );
        for (cols, height) in [(20, 5), (32, 12), (60, 20), (100, 38)] {
            state.activate(Target::Option(3));
            state.key(&Key::Paste("東京 with a longer custom answer".into()));
            let (sink, cursor) = render(&mut state, cols, height);
            assert!(
                sink.lines.len() <= height,
                "{} > {height}",
                sink.lines.len()
            );
            assert!(
                sink.lines.iter().all(|line| ansi::width(line) <= cols),
                "{cols}: {:?}",
                sink.lines
            );
            let (row, col) = cursor.expect("custom text must have a visible cursor");
            assert!(row < sink.lines.len() && col < cols);
            assert!(state
                .hits
                .iter()
                .flatten()
                .any(|(_, target)| *target == Target::Submit));
            let previous_start = state.visible_start;
            state.key(&Key::Home);
            let _ = render(&mut state, cols, height);
            assert_eq!(
                state.drafts[0].cursor, 0,
                "Home takes the caret to the start without a duplicate page-key binding"
            );
            assert!(
                state.visible_start <= previous_start,
                "the window follows the caret up, never down: {previous_start} -> {}",
                state.visible_start
            );
        }
        state.navigate(false);
        state.focus = Target::Title(0);
        let (sink, _) = render(&mut state, 100, 38);
        let plain = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("Storage Type") && plain.contains("Sync Strategy"));
        assert!(plain.contains("Write your own") && plain.contains("Submit answers"));
        println!("{plain}");
    }

    /// `Alt+Enter` is the composer's line break, and the custom answer is a
    /// field rather than a one-line slot: a person writing a config or a list of
    /// files needs more than one row, and the engine has always accepted
    /// newlines in `custom`. The editor was the only thing refusing them.
    #[test]
    fn alt_enter_breaks_the_line_in_the_custom_answer_and_both_rows_are_shown() {
        let question_json = json!([
            {
                "id": "detail",
                "title": "Details",
                "question": "Describe the change.",
                "type": "single",
                "options": [
                    {"id": "small", "label": "Small", "description": "one line is fine"},
                    {"id": "large", "label": "Large", "description": "needs several lines"}
                ]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut state = State::new("p-multiline".into(), questions);

        state.activate(Target::Option(2));
        state.key(&Key::Paste("first line".into()));
        state.key(&Key::NewLine);
        state.key(&Key::Paste("second line".into()));

        assert_eq!(
            state.drafts[0].text, "first line\nsecond line",
            "Alt+Enter must break the line, not be dropped"
        );

        let (sink, cursor) = render(&mut state, 100, 30);
        let plain: Vec<String> = sink.lines.iter().map(|l| ansi::plain(l)).collect();
        assert!(
            plain.iter().any(|l| l.contains("first line")),
            "the first row vanished:\n{}",
            plain.join("\n")
        );
        assert!(
            plain.iter().any(|l| l.contains("second line")),
            "the second row vanished:\n{}",
            plain.join("\n")
        );
        let (_, col) = cursor.expect("the caret must still be on screen");
        assert_eq!(
            col,
            9 + "second line".len(),
            "the caret belongs at the end of the second row"
        );

        // The engine's own validation has to agree, or the field is asking for
        // something the protocol will refuse.
        assert!(
            Reply::Answered {
                answers: vec![state.answer(0)]
            }
            .valid(&state.questions),
            "a multi-line custom answer must be a valid reply"
        );
    }

    /// A pasted block is a multi-line block, and this field is now somewhere it
    /// can arrive whole rather than being flattened into one very long row.
    #[test]
    fn a_pasted_block_keeps_its_breaks_in_the_custom_answer() {
        let question_json = json!([
            {
                "id": "detail",
                "title": "Details",
                "question": "Describe the change.",
                "type": "single",
                "options": [{"id": "small", "label": "Small", "description": "one line"}]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut state = State::new("p-paste".into(), questions);
        state.activate(Target::Option(1));
        state.key(&Key::Paste("alpha\r\nbeta\ngamma".into()));
        assert_eq!(state.drafts[0].text, "alpha\nbeta\ngamma");
    }

    #[test]
    fn custom_answer_end_caret_is_after_the_last_character_even_at_the_right_margin() {
        for cols in [20usize, 32, 60, 100] {
            let width = cols - 10;
            for text in [
                "x".repeat(width),
                format!("{}{}", "界".repeat(width / 2), "x".repeat(width % 2)),
                "x".repeat(width + 1),
            ] {
                let questions = serde_json::from_value(json!([{
                    "id":"q", "title":"Q", "question":"Answer", "type":"single",
                    "options":[{"id":"o", "label":"One", "description":"One"}]
                }]))
                .unwrap();
                let mut state = State::new("cursor-edge".into(), questions);
                state.activate(Target::Option(1));
                state.key(&Key::Paste(text.clone()));
                let (sink, cursor) = render(&mut state, cols, 32);
                let (row, col) = cursor.expect("custom caret is visible");
                let composed = draw::compose_rows(&text, width);
                let (_, last) = composed.last().unwrap();
                assert_eq!(col, 9 + ansi::width(last), "{cols} cols / {text:?}");
                assert_eq!(
                    ansi::slice(&sink.lines[row], col - 1, col),
                    " ",
                    "caret must be on the blank AFTER the final character"
                );
                assert_eq!(
                    state.drafts[0].text, text,
                    "rendering must not change the answer"
                );
                if ansi::width(last) == width {
                    assert_eq!(col, cols - 1);
                }
            }
        }
    }

    #[test]
    fn recommended_badge_variable_options_and_cursor_offset() {
        let question_json = json!([
            {
                "id": "stack",
                "title": "Stack Choice",
                "question": "Choose your primary framework for the application.",
                "type": "single",
                "options": [
                    {"id": "rust", "label": "(Recommended) Rust Native", "description": "High performance."},
                    {"id": "node", "label": "Node.js Express", "description": "Fast development."}
                ]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut state = State::new("p2".into(), questions);

        // Check rendering of 2-option question
        let (sink, _) = render(&mut state, 100, 30);
        let plain = sink
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("1. ● Rust Native (Recommended)")
                || plain.contains("1. ○ Rust Native (Recommended)")
        );
        assert!(plain.contains("2. ○ Node.js Express"));
        assert!(plain.contains("3. ○ Write your own"));

        // Check cursor column on Write your own (index 2 for 2-option question)
        state.activate(Target::Option(2));
        state.drafts[0].text = "abc".into();
        state.drafts[0].cursor = 3; // cursor at end of "abc"
        let (_, cursor) = render(&mut state, 100, 30);
        let (_, col) = cursor.expect("cursor must be present");
        assert_eq!(col, 9 + 3, "cursor at end of 'abc' must be at column 12");

        state.drafts[0].cursor = 1; // cursor at "a|bc"
        let (_, cursor) = render(&mut state, 100, 30);
        let (_, col) = cursor.expect("cursor must be present");
        assert_eq!(
            col,
            9 + 1,
            "cursor between 'a' and 'bc' must be at column 10"
        );
    }

    #[test]
    fn esc_requires_double_press_within_3s_to_cancel() {
        let question_json = json!([
            {
                "id": "q1",
                "title": "Title One",
                "question": "Choose an option.",
                "type": "single",
                "options": [
                    {"id": "a", "label": "Opt A", "description": "Desc A."},
                    {"id": "b", "label": "Opt B", "description": "Desc B."}
                ]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut app = crate::ui::App::for_test();
        app.questions = Some(State::new("req-test".into(), questions));
        let mut driver = crate::ui::test_driver("esc-test");

        // 1st Esc: arms cancellation, does not cancel
        key(&mut app, &Key::Esc, &mut driver);
        assert!(app.questions.is_some());
        assert!(app.questions.as_ref().unwrap().esc_armed);
        assert_eq!(app.status.as_deref(), Some("press esc again to cancel"));

        // Any non-Esc key disarms cancellation and clears status
        key(&mut app, &Key::Down, &mut driver);
        assert!(!app.questions.as_ref().unwrap().esc_armed);
        assert_eq!(app.status, None);

        // Re-arm Esc
        key(&mut app, &Key::Esc, &mut driver);
        assert!(app.questions.as_ref().unwrap().esc_armed);

        // Simulate expired arming (> 3s ago) -> 1st Esc again
        app.questions.as_mut().unwrap().esc_armed_at =
            Some(Instant::now() - Duration::from_secs(4));
        key(&mut app, &Key::Esc, &mut driver);
        assert!(
            app.questions.is_some(),
            "Expired arming must not cancel on Esc"
        );
        assert!(app.questions.as_ref().unwrap().esc_armed);

        // 2nd Esc within 3s -> Cancels form
        key(&mut app, &Key::Esc, &mut driver);
        assert!(!app.questions.as_ref().unwrap().esc_armed);
    }

    #[test]
    fn answered_tab_has_clean_contrast_and_all_tabs_fit() {
        let question_json = json!([
            {
                "id": "q1",
                "title": "Pilih Tech Stack",
                "question": "Choose stack.",
                "type": "single",
                "options": [
                    {"id": "a", "label": "Next.js", "description": "React"},
                    {"id": "b", "label": "Nuxt 3", "description": "Vue"}
                ]
            },
            {
                "id": "q2",
                "title": "Pilih Fitur",
                "question": "Choose features.",
                "type": "single",
                "options": [
                    {"id": "c", "label": "Auth", "description": "Login"}
                ]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut state = State::new("req-test".into(), questions);
        state.choose(0); // Answer question 0
        state.current = 1; // Switch to question 1

        let (sink, _) = render(&mut state, 120, 30);
        let rendered_tabs = ansi::plain(&sink.lines[1]);

        // Question 1 (index 0) is complete and not current:
        // Must contain "1. Pilih Tech Stack" without bullet or checkmark
        assert!(rendered_tabs.contains("1. Pilih Tech Stack"));
        assert!(!rendered_tabs.contains('●'));
        assert!(!rendered_tabs.contains('✓'));
        // Question 2 (index 1) is current and not complete:
        // Must be in [ 2. Pilih Fitur ] without bullet
        assert!(rendered_tabs.contains("[ 2. Pilih Fitur ]"));

        // When not all questions answered, submit button must still be white text:
        let raw_nav_line = &sink.lines[sink.lines.len() - 3];
        assert!(
            raw_nav_line.contains(&format!("{TEXT}")),
            "Unfocused submit must have white TEXT ink"
        );

        // Complete all questions so submit button activates
        state.choose(0);
        assert!(state.complete(0) && state.complete(1));

        let (sink_all, _) = render(&mut state, 120, 30);
        let nav_line = ansi::plain(&sink_all.lines[sink_all.lines.len() - 3]);
        assert!(nav_line.contains("Submit answers"));

        // Verify 5 questions all fit and Question 5 is NOT dropped:
        let five_q_json = json!([
            {"id": "q1", "title": "Bahasa Pemrograman", "question": "Q1", "type": "single", "options": [{"id": "a", "label": "A", "description": "D"}]},
            {"id": "q2", "title": "Framework & Tools", "question": "Q2", "type": "single", "options": [{"id": "b", "label": "B", "description": "D"}]},
            {"id": "q3", "title": "Gaya Komunikasi", "question": "Q3", "type": "single", "options": [{"id": "c", "label": "C", "description": "D"}]},
            {"id": "q4", "title": "Prioritas Project", "question": "Q4", "type": "single", "options": [{"id": "d", "label": "D", "description": "D"}]},
            {"id": "q5", "title": "Bahasa Respon", "question": "Q5", "type": "single", "options": [{"id": "e", "label": "E", "description": "D"}]}
        ]);
        let mut five_state = State::new(
            "five-test".into(),
            serde_json::from_value(five_q_json).unwrap(),
        );
        five_state.current = 3; // On question 4 (same as user's photo 2)
        let (five_sink, _) = render(&mut five_state, 120, 30);
        let five_tabs = ansi::plain(&five_sink.lines[1]);
        assert!(five_tabs.contains("1."), "Tab 1 must be present");
        assert!(five_tabs.contains("2."), "Tab 2 must be present");
        assert!(five_tabs.contains("3."), "Tab 3 must be present");
        assert!(five_tabs.contains("4."), "Tab 4 must be present");
        assert!(five_tabs.contains("5."), "Question 5 must be present and not dropped due to overflow! Tabs rendered: {five_tabs}");
    }

    #[test]
    fn mouse_drag_selects_text_without_toggling_questions() {
        use crate::ui::input::{Click, Mouse};

        let question_json = json!([
            {
                "id": "q1",
                "title": "Tech Stack",
                "question": "Select your tech stack.",
                "type": "single",
                "options": [
                    {"id": "a", "label": "Option Alpha", "description": "First option."},
                    {"id": "b", "label": "Option Beta", "description": "Second option."}
                ]
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut app = crate::ui::App::for_test();
        app.questions = Some(State::new("req-mouse".into(), questions));
        let mut driver = crate::ui::test_driver("mouse-test");

        let (sink, _) = render(app.questions.as_mut().unwrap(), 100, 30);
        let (opt_row, opt_col) = sink
            .owner
            .iter()
            .enumerate()
            .find_map(|(row, hit)| {
                if let Hit::Questions(at) = hit {
                    app.questions
                        .as_ref()
                        .unwrap()
                        .hits
                        .get(*at)?
                        .iter()
                        .find(|(_, target)| *target == Target::Option(0))
                        .map(|(range, _)| (row + 1, range.start + 2))
                } else {
                    None
                }
            })
            .unwrap();
        app.hits = sink.owner;
        app.selection_rows = vec!["Selectable line in terminal".into(); 30];

        // 1. Mouse drag across row
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Press,
                row: opt_row as u16,
                col: opt_col as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Drag,
                row: opt_row as u16,
                col: (opt_col + 15) as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Release,
                row: opt_row as u16,
                col: (opt_col + 15) as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );

        // Selection occurred: text copied, option NOT chosen
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("copied ")));
        assert!(
            !app.questions.as_ref().unwrap().complete(0),
            "Drag selection must not toggle question options"
        );

        // 2. Click (press + release at same position) activates the option
        app.status = None;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Press,
                row: opt_row as u16,
                col: opt_col as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Release,
                row: opt_row as u16,
                col: opt_col as u16,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut driver,
        );
        assert!(
            app.questions.as_ref().unwrap().complete(0),
            "Click must activate question option"
        );
    }

    #[test]
    fn actual_mouse_uses_drawn_custom_rows_after_reflow_without_toggling_other_answers() {
        use crate::ui::input::{Modifiers, Mouse};
        let mut driver = crate::ui::test_driver("questions-projection-mouse");
        let mut app = App::for_test();
        let questions = serde_json::from_value(json!([{
            "id":"q1", "question":"Write or pick", "type":"multiple",
            "options":[{"label":"Keep this", "description":"Selected too"}]
        }]))
        .unwrap();
        let mut state = State::new("request".into(), questions);
        state.drafts[0].chosen[0] = true;
        state.drafts[0].text =
            "a\té界 e\u{301}👨\u{200d}💻\x1b[31mred\x1b[0m\nsecond line with wrap".into();
        state.drafts[0].cursor = 0;
        state.focus = Target::Option(1);
        app.questions = Some(state);
        for width in [100, 32, 20, 80] {
            crate::ui::draw::compose_for_test(&mut app, 40, width);
            let positions: Vec<_> = app
                .hits
                .iter()
                .enumerate()
                .filter_map(|(screen, hit)| {
                    let Hit::Questions(index) = hit else {
                        return None;
                    };
                    let state = app.questions.as_ref().unwrap();
                    state.hits[*index]
                        .iter()
                        .find_map(|(_, target)| match target {
                            Target::CustomText { row, .. } => Some((screen + 1, *row)),
                            _ => None,
                        })
                })
                .collect();
            assert!(!positions.is_empty());
            for (screen, index) in positions {
                for col in [9, 10, 13, width.saturating_sub(1)] {
                    let expected = app.questions.as_ref().unwrap().text_rows[index].caret_at(col);
                    for kind in [Click::Press, Click::Release] {
                        app.on_key(
                            Key::Mouse(Mouse {
                                kind,
                                row: screen as u16,
                                col,
                                modifiers: Modifiers::NONE,
                            }),
                            &mut driver,
                        );
                    }
                    let state = app.questions.as_ref().unwrap();
                    assert_eq!(
                        state.drafts[0].cursor, expected,
                        "width {width}, row {index}, col {col}"
                    );
                    assert!(state.drafts[0].chosen[0] && state.drafts[0].chosen[1]);
                    assert_eq!(
                        state.drafts[0].text,
                        "a\té界 e\u{301}👨\u{200d}💻\x1b[31mred\x1b[0m\nsecond line with wrap"
                    );
                }
            }
        }
        driver.engine.shutdown();
    }

    #[test]
    fn a_click_from_the_previous_question_frame_is_ignored_until_repaint() {
        let mut app = App::for_test();
        let mut driver = crate::ui::test_driver("questions-stale-click");
        let questions = serde_json::from_value(json!([
            {"id":"q1","question":"First","type":"single","options":[{"label":"A","description":"A"},{"label":"B","description":"B"},{"label":"C","description":"C"}]},
            {"id":"q2","question":"Second","type":"single","options":[{"label":"D","description":"D"}]}
        ])).unwrap();
        let mut state = State::new("req".into(), questions);
        render(&mut state, 80, 30);
        let (hit, col) = state
            .hits
            .iter()
            .enumerate()
            .find_map(|(hit, controls)| {
                controls.iter().find_map(|(range, target)| {
                    (*target == Target::Option(2)).then_some((hit, range.start as u16))
                })
            })
            .unwrap();
        state.current = 1;
        app.questions = Some(state);
        click(&mut app, hit, col, &mut driver);
        assert!(!app.questions.as_ref().unwrap().drafts[1]
            .chosen
            .iter()
            .any(|chosen| *chosen));
        driver.engine.shutdown();
    }

    #[test]
    fn custom_answer_click_moves_cursor_without_unselecting_option() {
        let question_json = json!([
            {
                "id": "q1",
                "question": "Pick or write",
                "options": [
                    {"label": "Choice A", "description": "Desc A"},
                ],
                "type": "single"
            }
        ]);
        let questions: Vec<Question> = serde_json::from_value(question_json).unwrap();
        let mut state = State::new("req-click-cursor".into(), questions);
        state.drafts[0].text = "Hello world".into();
        state.drafts[0].cursor = 0;

        // Render to populate state.hits and state.custom_width
        let _ = render(&mut state, 80, 20);

        // Custom option index is 1 (after Choice A at index 0)
        // Click at col 9 + 5 (pointing to ' ' after 'Hello')
        state.activate_at(Target::CustomText { option: 1, row: 0 }, Some(9 + 5));
        assert_eq!(
            state.drafts[0].cursor, 5,
            "cursor must jump to clicked column index 5"
        );
        assert!(state.drafts[0].chosen[1], "custom option must be selected");

        // Click again at col 9 + 1 (pointing to 'e')
        state.activate_at(Target::CustomText { option: 1, row: 0 }, Some(9 + 1));
        assert_eq!(
            state.drafts[0].cursor, 1,
            "cursor must move to column index 1"
        );
        assert!(
            state.drafts[0].chosen[1],
            "subsequent clicks must keep custom option selected, not toggle off"
        );
    }
}
