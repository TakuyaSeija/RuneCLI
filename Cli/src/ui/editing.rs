//! One keymap and one edit transaction serve every text field. Private editing
//! state never enters a composer snapshot, another field, or a provider request.
use super::{
    ansi,
    editor::Projection,
    input::{Direction, Key},
};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    LineStart,
    LineEnd,
    Start,
    End,
    SelectLeft,
    SelectRight,
    SelectWordLeft,
    SelectWordRight,
    SelectUp,
    SelectDown,
    Backspace,
    Delete,
    KillStart,
    KillEnd,
    KillWordLeft,
    KillWordRight,
    Yank,
    YankPrevious,
    Undo,
    Redo,
    Transpose,
    TransposeWords,
    Upper,
    Lower,
    Capitalize,
}

/// Each semantic editing action has exactly one chord. Terminal encodings of
/// the same physical key are decoded before this table, never extra aliases.
pub const BINDINGS: &[(&str, Key, Action)] = &[
    ("←", Key::Left, Action::Left),
    ("→", Key::Right, Action::Right),
    (
        "Ctrl+←",
        Key::Motion {
            direction: Direction::Left,
            ctrl: true,
            shift: false,
        },
        Action::WordLeft,
    ),
    (
        "Ctrl+→",
        Key::Motion {
            direction: Direction::Right,
            ctrl: true,
            shift: false,
        },
        Action::WordRight,
    ),
    ("↑", Key::Up, Action::Up),
    ("↓", Key::Down, Action::Down),
    ("Ctrl+A", Key::Ctrl('a'), Action::LineStart),
    ("Ctrl+E", Key::Ctrl('e'), Action::LineEnd),
    ("Home", Key::Home, Action::Start),
    ("End", Key::End, Action::End),
    (
        "Shift+←",
        Key::Motion {
            direction: Direction::Left,
            ctrl: false,
            shift: true,
        },
        Action::SelectLeft,
    ),
    (
        "Shift+→",
        Key::Motion {
            direction: Direction::Right,
            ctrl: false,
            shift: true,
        },
        Action::SelectRight,
    ),
    (
        "Ctrl+Shift+←",
        Key::Motion {
            direction: Direction::Left,
            ctrl: true,
            shift: true,
        },
        Action::SelectWordLeft,
    ),
    (
        "Ctrl+Shift+→",
        Key::Motion {
            direction: Direction::Right,
            ctrl: true,
            shift: true,
        },
        Action::SelectWordRight,
    ),
    (
        "Shift+↑",
        Key::Motion {
            direction: Direction::Up,
            ctrl: false,
            shift: true,
        },
        Action::SelectUp,
    ),
    (
        "Shift+↓",
        Key::Motion {
            direction: Direction::Down,
            ctrl: false,
            shift: true,
        },
        Action::SelectDown,
    ),
    ("Backspace", Key::Backspace, Action::Backspace),
    ("Delete", Key::Delete, Action::Delete),
    ("Ctrl+U", Key::Ctrl('u'), Action::KillStart),
    ("Ctrl+K", Key::Ctrl('k'), Action::KillEnd),
    ("Ctrl+W", Key::Ctrl('w'), Action::KillWordLeft),
    ("Alt+D", Key::Alt('d'), Action::KillWordRight),
    ("Ctrl+Y", Key::Ctrl('y'), Action::Yank),
    ("Alt+Y", Key::Alt('y'), Action::YankPrevious),
    ("Ctrl+_", Key::Ctrl('_'), Action::Undo),
    ("Alt+R", Key::Alt('r'), Action::Redo),
    ("Alt+X", Key::Alt('x'), Action::Transpose),
    ("Alt+W", Key::Alt('w'), Action::TransposeWords),
    ("Alt+U", Key::Alt('u'), Action::Upper),
    ("Alt+L", Key::Alt('l'), Action::Lower),
    ("Alt+V", Key::Alt('v'), Action::Capitalize),
];

impl Action {
    pub fn description(self) -> &'static str {
        match self {
            Self::Left => "move one visible character left",
            Self::Right => "move one visible character right",
            Self::WordLeft => "move to the previous word",
            Self::WordRight => "move to the next word end",
            Self::Up => "move one visible text row up",
            Self::Down => "move one visible text row down",
            Self::LineStart => "go to the current logical line start",
            Self::LineEnd => "go to the current logical line end",
            Self::Start => "go to the whole input start",
            Self::End => "go to the whole input end",
            Self::SelectLeft => "select one visible character left",
            Self::SelectRight => "select one visible character right",
            Self::SelectWordLeft => "select to the previous word",
            Self::SelectWordRight => "select to the next word end",
            Self::SelectUp => "select one visible text row up",
            Self::SelectDown => "select one visible text row down",
            Self::Backspace => "delete left character or selection",
            Self::Delete => "delete right character or selection",
            Self::KillStart => "kill to logical line start",
            Self::KillEnd => "kill forward to line end; at newline, join",
            Self::KillWordLeft => "kill the word left of the caret",
            Self::KillWordRight => "kill the word right of the caret",
            Self::Yank => "insert the most recent killed text",
            Self::YankPrevious => "cycle the immediately preceding yank",
            Self::Undo => "undo a text edit (not file restore)",
            Self::Redo => "redo an undone text edit",
            Self::Transpose => "exchange adjacent visible characters",
            Self::TransposeWords => "exchange adjacent words",
            Self::Upper => "uppercase next word or selection",
            Self::Lower => "lowercase next word or selection",
            Self::Capitalize => "capitalize next word or selection",
        }
    }
}

pub fn action(key: &Key) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|(_, chord, _)| chord == key)
        .map(|(_, _, action)| *action)
}

#[derive(Clone, Copy)]
pub struct Options {
    pub single_line: bool,
    pub max_chars: usize,
    pub max_bytes: usize,
    pub width: usize,
}
impl Options {
    pub fn multiline(width: usize) -> Self {
        Self {
            single_line: false,
            max_chars: usize::MAX,
            max_bytes: usize::MAX,
            width: width.max(1),
        }
    }
    pub fn single(max_chars: usize, max_bytes: usize) -> Self {
        Self {
            single_line: true,
            max_chars,
            max_bytes,
            width: usize::MAX,
        }
    }
}

#[derive(Clone)]
struct Change {
    at: usize,
    before: String,
    after: String,
    cursor_before: usize,
    cursor_after: usize,
    anchor_before: Option<usize>,
    anchor_after: Option<usize>,
}
impl Change {
    fn bytes(&self) -> usize {
        self.before.len().saturating_add(self.after.len())
    }
}

// These are transient memory ceilings, not defaults for a user setting. No
// operation can keep an unbounded second history of a large pasted document.
const HISTORY_BYTES: usize = 1024 * 1024;
const HISTORY_ENTRIES: usize = 256;
const RING_BYTES: usize = 128 * 1024;
const RING_ENTRIES: usize = 16;

#[derive(Clone, Default)]
pub struct State {
    known: Option<String>,
    last_cursor: Option<usize>,
    anchor: Option<usize>,
    desired: Option<usize>,
    /// A soft-wrap boundary belongs to two rows. Keep the upstream side only
    /// at the width that established it; source text and protocol stay unchanged.
    upstream: Option<(usize, usize)>,
    undo: Vec<Change>,
    redo: Vec<Change>,
    ring: Vec<String>,
    killed: Option<bool>,
    yank: Option<(Range<usize>, usize)>,
}

impl State {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A host/other window/history replacement is not a local edit. Replaying
    /// an old undo across it would replace somebody else's words.
    pub fn observe(&mut self, text: &str, cursor: usize) {
        if self.known.as_deref() != Some(text) {
            self.reset();
            self.known = Some(text.to_owned());
        } else if self.last_cursor != Some(cursor) {
            self.anchor = None;
            self.desired = None;
            self.upstream = None;
            self.killed = None;
            self.yank = None;
        }
        self.last_cursor = Some(cursor);
    }

    pub fn selected(&self, text: &str, cursor: usize) -> Option<Range<usize>> {
        if self.known.as_deref() != Some(text) || self.last_cursor != Some(cursor) {
            return None;
        }
        self.anchor
            .filter(|&a| a != cursor)
            .map(|a| a.min(cursor)..a.max(cursor))
    }

    pub fn place(&mut self, text: &str, cursor: &mut usize, at: usize) {
        self.observe(text, *cursor);
        *cursor = boundary(text, at);
        self.anchor = None;
        self.desired = None;
        self.upstream = None;
        self.yank = None;
        self.killed = None;
        self.last_cursor = Some(*cursor);
    }

    pub(super) fn place_in_row(
        &mut self,
        text: &str,
        cursor: &mut usize,
        row: &super::editor::Row,
        column: u16,
        width: usize,
    ) {
        self.place(text, cursor, row.caret_at(column));
        if *cursor > row.start && *cursor == row.end() {
            self.upstream = Some((*cursor, width));
        }
    }

    pub fn row_index(
        &self,
        text: &str,
        cursor: usize,
        width: usize,
        rows: &[(usize, String)],
        display_at: usize,
    ) -> usize {
        let upstream = self.known.as_deref() == Some(text)
            && self.last_cursor == Some(cursor)
            && self.upstream == Some((cursor, width));
        row_index(rows, display_at, upstream)
    }

    pub fn select(&mut self, text: &str, cursor: &mut usize, start: usize, end: usize) {
        self.observe(text, *cursor);
        self.anchor = Some(boundary(text, start));
        *cursor = boundary(text, end);
        self.last_cursor = Some(*cursor);
        self.desired = None;
        self.upstream = None;
        self.yank = None;
        self.killed = None;
    }

    pub(super) fn select_in_row(
        &mut self,
        text: &str,
        cursor: &mut usize,
        start: usize,
        row: &super::editor::Row,
        column: u16,
        width: usize,
    ) {
        self.select(text, cursor, start, row.caret_at(column));
        if *cursor > row.start && *cursor == row.end() {
            self.upstream = Some((*cursor, width));
        }
    }

    pub fn vertical_step(
        &mut self,
        text: &str,
        cursor: &mut usize,
        width: usize,
        up: bool,
    ) -> Option<usize> {
        self.observe(text, *cursor);
        let step = vertical_at(
            text,
            *cursor,
            width,
            up,
            self.desired,
            self.upstream == Some((*cursor, width)),
        );
        self.desired = Some(step.column);
        self.yank = None;
        self.killed = None;
        if let Some(pos) = step.position {
            *cursor = boundary_after(text, pos);
            self.upstream = step.upstream.then_some((*cursor, width));
            self.anchor = None;
            self.last_cursor = Some(*cursor);
            Some(*cursor)
        } else {
            None
        }
    }

    pub fn key(
        &mut self,
        text: &mut String,
        cursor: &mut usize,
        key: &Key,
        options: Options,
    ) -> bool {
        let act = action(key);
        let insert = match key {
            Key::Char(c) if !c.is_control() => Some(c.to_string()),
            Key::NewLine if !options.single_line => Some("\n".into()),
            Key::Paste(value) => {
                let value = normalize_newlines(value);
                if options.single_line && value.chars().any(char::is_control) {
                    return true;
                }
                Some(value)
            }
            _ => None,
        };
        if act.is_none() && insert.is_none() {
            return false;
        }
        *cursor = boundary(text, *cursor);
        self.observe(text, *cursor);
        if matches!(act, Some(Action::Undo | Action::Redo)) {
            self.restore(text, cursor, act == Some(Action::Redo), options);
            return true;
        }
        let before_cursor = *cursor;
        let before_anchor = self.anchor;
        let selected = self.selected(text, *cursor);
        let mut next = text.clone();
        let mut at = *cursor;
        let mut anchor = self.anchor;
        let mut kill = None;
        let mut yank = None;
        let mut desired = None;
        let mut upstream = None;
        if let Some(value) = insert {
            let range = selected.unwrap_or(at..at);
            at = range.start + value.len();
            next.replace_range(range, &value);
            anchor = None;
        } else if let Some(act) = act {
            use Action::*;
            let selecting = matches!(
                act,
                SelectLeft | SelectRight | SelectWordLeft | SelectWordRight | SelectUp | SelectDown
            );
            let motion = match act {
                Left | SelectLeft => Some(
                    selected
                        .as_ref()
                        .filter(|_| !selecting)
                        .map_or_else(|| previous(text, at), |r| r.start),
                ),
                Right | SelectRight => Some(
                    selected
                        .as_ref()
                        .filter(|_| !selecting)
                        .map_or_else(|| following(text, at), |r| r.end),
                ),
                WordLeft | SelectWordLeft => Some(word_left(text, at)),
                WordRight | SelectWordRight => Some(word_right(text, at)),
                LineStart => Some(line_start(text, at)),
                LineEnd => Some(line_end(text, at)),
                Start => Some(0),
                End => Some(text.len()),
                Up | Down | SelectUp | SelectDown if !options.single_line => {
                    let step = vertical_at(
                        text,
                        at,
                        options.width,
                        matches!(act, Up | SelectUp),
                        self.desired,
                        self.upstream == Some((at, options.width)),
                    );
                    desired = Some(step.column);
                    let position = step.position.unwrap_or(at);
                    upstream = if step.position.is_some() {
                        step.upstream.then_some((position, options.width))
                    } else {
                        self.upstream
                    };
                    Some(position)
                }
                Up | Down | SelectUp | SelectDown => Some(at),
                _ => None,
            };
            if let Some(position) = motion {
                if selecting {
                    anchor.get_or_insert(at);
                } else {
                    anchor = None;
                }
                at = position;
            } else {
                match act {
                    Backspace | Delete | KillStart | KillEnd | KillWordLeft | KillWordRight => {
                        let range = selected.unwrap_or_else(|| match act {
                            Backspace => previous(text, at)..at,
                            Delete => at..following(text, at),
                            KillStart => line_start(text, at)..at,
                            KillEnd => {
                                let end = line_end(text, at);
                                at..if end == at && end < text.len() {
                                    end + 1
                                } else {
                                    end
                                }
                            }
                            KillWordLeft => word_left(text, at)..at,
                            _ => at..word_right(text, at),
                        });
                        if matches!(act, KillStart | KillEnd | KillWordLeft | KillWordRight)
                            && !range.is_empty()
                        {
                            kill = Some((
                                text[range.clone()].to_owned(),
                                matches!(act, KillStart | KillWordLeft),
                            ));
                        }
                        at = range.start;
                        next.replace_range(range, "");
                        anchor = None;
                    }
                    Yank | YankPrevious => {
                        let target = if act == YankPrevious {
                            self.yank
                                .clone()
                                .map(|(r, i)| (r, (i + 1) % self.ring.len().max(1)))
                        } else {
                            Some((selected.unwrap_or(at..at), 0))
                        };
                        if let Some((range, index)) = target {
                            if let Some(value) = self.ring.get(index) {
                                at = range.start + value.len();
                                yank = Some((range.start..at, index));
                                next.replace_range(range, value);
                                anchor = None;
                            }
                        }
                    }
                    Transpose => {
                        if let Some((range, value)) = transpose(text, at) {
                            at = range.start + value.len();
                            next.replace_range(range, &value);
                            anchor = None;
                        }
                    }
                    TransposeWords => {
                        if let Some((range, value)) = transpose_words(text, at) {
                            at = range.start + value.len();
                            next.replace_range(range, &value);
                            anchor = None;
                        }
                    }
                    Upper | Lower | Capitalize => {
                        let range = selected.or_else(|| word_at(text, at));
                        if let Some(range) = range {
                            let value = match act {
                                Upper => text[range.clone()].to_uppercase(),
                                Lower => text[range.clone()].to_lowercase(),
                                _ => capitalize(&text[range.clone()]),
                            };
                            at = range.start + value.len();
                            next.replace_range(range, &value);
                            anchor = None;
                        }
                    }
                    _ => {}
                }
            }
        }
        // All surfaces, including paste/yank/case-expansion/undo, obey their
        // own bounds as one transaction. A rejected edit changes no history.
        if next.len() > options.max_bytes || next.chars().count() > options.max_chars {
            return true;
        }
        if next != *text {
            let change = change(text, &next, before_cursor, at, before_anchor, anchor);
            self.redo.clear();
            if change.bytes() <= HISTORY_BYTES {
                self.undo.push(change);
                trim_history(&mut self.undo);
            } else {
                self.undo.clear();
            }
            *text = next;
        }
        if let Some((value, backward)) = kill {
            self.keep_kill(value, backward);
        } else {
            self.killed = None;
        }
        self.yank = yank;
        self.anchor = anchor;
        self.desired = desired;
        self.upstream = upstream;
        *cursor = boundary_after(text, at);
        self.known = Some(text.clone());
        self.last_cursor = Some(*cursor);
        true
    }

    fn keep_kill(&mut self, value: String, backward: bool) {
        if value.len() > RING_BYTES {
            self.killed = None;
            return;
        }
        if self.killed == Some(backward)
            && self
                .ring
                .first()
                .is_some_and(|s| s.len() + value.len() <= RING_BYTES)
        {
            if backward {
                self.ring[0].insert_str(0, &value);
            } else {
                self.ring[0].push_str(&value);
            }
        } else {
            self.ring.insert(0, value);
        }
        while self.ring.len() > RING_ENTRIES
            || self.ring.iter().map(String::len).sum::<usize>() > RING_BYTES
        {
            self.ring.pop();
        }
        self.killed = Some(backward);
    }

    fn restore(&mut self, text: &mut String, cursor: &mut usize, redo: bool, options: Options) {
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(change) = stack.pop() else {
            return;
        };
        let (old, new, at, anchor) = if redo {
            (
                &change.before,
                &change.after,
                change.cursor_after,
                change.anchor_after,
            )
        } else {
            (
                &change.after,
                &change.before,
                change.cursor_before,
                change.anchor_before,
            )
        };
        let end = change.at + old.len();
        if text.get(change.at..end) != Some(old.as_str()) {
            self.reset();
            return;
        }
        let bytes = text.len() - old.len() + new.len();
        let chars = text.chars().count() - old.chars().count() + new.chars().count();
        if bytes > options.max_bytes
            || chars > options.max_chars
            || options.single_line && new.chars().any(char::is_control)
        {
            if redo {
                self.redo.push(change);
            } else {
                self.undo.push(change);
            }
            return;
        }
        text.replace_range(change.at..end, new);
        *cursor = boundary_after(text, at);
        self.anchor = anchor;
        let destination = if redo { &mut self.undo } else { &mut self.redo };
        destination.push(change);
        trim_history(destination);
        self.desired = None;
        self.upstream = None;
        self.yank = None;
        self.killed = None;
        self.known = Some(text.clone());
        self.last_cursor = Some(*cursor);
    }
}

fn trim_history(history: &mut Vec<Change>) {
    let mut bytes = history.iter().map(Change::bytes).sum::<usize>();
    while history.len() > HISTORY_ENTRIES || bytes > HISTORY_BYTES {
        bytes -= history.remove(0).bytes();
    }
}

fn change(
    before: &str,
    after: &str,
    cb: usize,
    ca: usize,
    ab: Option<usize>,
    aa: Option<usize>,
) -> Change {
    let mut prefix = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(prefix) || !after.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = before[prefix..]
        .bytes()
        .rev()
        .zip(after[prefix..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(before.len() - suffix)
        || !after.is_char_boundary(after.len() - suffix)
    {
        suffix -= 1;
    }
    Change {
        at: prefix,
        before: before[prefix..before.len() - suffix].into(),
        after: after[prefix..after.len() - suffix].into(),
        cursor_before: cb,
        cursor_after: ca,
        anchor_before: ab,
        anchor_after: aa,
    }
}

pub fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn boundaries(text: &str) -> Vec<usize> {
    let projection = Projection::plain(text);
    let mut stops = vec![0];
    for (start, glyph, _) in ansi::editor_glyphs(&projection.text) {
        stops.push(projection.source_at(start));
        stops.push(projection.source_at(start + glyph.len()));
    }
    stops.push(text.len());
    stops.dedup();
    stops
}
pub fn boundary(text: &str, at: usize) -> usize {
    boundaries(text)
        .into_iter()
        .take_while(|&start| start <= at)
        .last()
        .unwrap_or(0)
}
fn boundary_after(text: &str, at: usize) -> usize {
    boundaries(text)
        .into_iter()
        .find(|&start| start >= at)
        .unwrap_or(text.len())
}
fn previous(text: &str, at: usize) -> usize {
    boundaries(text)
        .into_iter()
        .take_while(|&start| start < at)
        .last()
        .unwrap_or(0)
}
fn following(text: &str, at: usize) -> usize {
    boundaries(text)
        .into_iter()
        .find(|&start| start > at)
        .unwrap_or(text.len())
}
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}
fn word_left(text: &str, at: usize) -> usize {
    let head = text[..at].trim_end();
    head.rfind(char::is_whitespace).map_or(0, |space| {
        space + head[space..].chars().next().unwrap().len_utf8()
    })
}
fn word_right(text: &str, at: usize) -> usize {
    let mut end = at;
    let mut begun = false;
    for (offset, c) in text[at..].char_indices() {
        let space = c.is_whitespace();
        if begun && space {
            break;
        }
        begun |= !space;
        end = at + offset + c.len_utf8();
    }
    end
}
fn word_at(text: &str, at: usize) -> Option<Range<usize>> {
    let start = text[at..]
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(offset, _)| at + offset)?;
    let end = text[start..]
        .char_indices()
        .take_while(|(_, c)| {
            c.is_alphanumeric()
                || *c == '_'
                || !c.is_control() && !c.is_whitespace() && ansi::char_width(*c) == 0
        })
        .map(|(offset, c)| start + offset + c.len_utf8())
        .last()?;
    Some(start..end)
}
fn capitalize(value: &str) -> String {
    let mut first = true;
    let mut out = String::new();
    for c in value.chars() {
        if first && c.is_alphanumeric() {
            out.extend(c.to_uppercase());
            first = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}
fn transpose(text: &str, at: usize) -> Option<(Range<usize>, String)> {
    let end = if at == text.len() || text[at..].starts_with('\n') {
        at
    } else {
        following(text, at)
    };
    let middle = previous(text, end);
    let start = previous(text, middle);
    if middle == start || text[start..end].contains('\n') {
        return None;
    }
    Some((
        start..end,
        format!("{}{}", &text[middle..end], &text[start..middle]),
    ))
}
fn transpose_words(text: &str, at: usize) -> Option<(Range<usize>, String)> {
    let mut words = Vec::new();
    let mut from = 0;
    while let Some(range) = word_at(text, from) {
        from = range.end;
        words.push(range);
    }
    let right = words
        .iter()
        .position(|r| r.end >= at)
        .unwrap_or_else(|| words.len().saturating_sub(1));
    let right = right.max(1);
    let left = right - 1;
    let (a, b) = (words.get(left)?, words.get(right)?);
    if text[a.end..b.start].contains('\n') {
        return None;
    }
    Some((
        a.start..b.end,
        format!(
            "{}{}{}",
            &text[b.clone()],
            &text[a.end..b.start],
            &text[a.clone()]
        ),
    ))
}

/// Rows and columns come from the same projection and glyph measure as paint.
#[cfg(test)]
pub fn vertical(
    text: &str,
    cursor: usize,
    width: usize,
    up: bool,
    desired: Option<usize>,
) -> (Option<usize>, usize) {
    let step = vertical_at(text, cursor, width, up, desired, false);
    (step.position, step.column)
}

fn row_index(rows: &[(usize, String)], at: usize, upstream: bool) -> usize {
    let here = rows
        .iter()
        .rposition(|(start, _)| *start <= at)
        .unwrap_or(0);
    if upstream && here > 0 && rows[here].0 == at {
        let (start, previous) = &rows[here - 1];
        if *start + previous.len() == at {
            return here - 1;
        }
    }
    here
}

struct Vertical {
    position: Option<usize>,
    column: usize,
    upstream: bool,
}

fn vertical_at(
    text: &str,
    cursor: usize,
    width: usize,
    up: bool,
    desired: Option<usize>,
    upstream: bool,
) -> Vertical {
    let projection = Projection::plain(text);
    let at = projection.display_at(cursor);
    let rows = super::draw::compose_rows(&projection.text, width.max(1));
    let here = row_index(&rows, at, upstream);
    let (start, row) = &rows[here];
    let cut = ansi::caret_in_row(row, *start, at);
    let column = desired.unwrap_or_else(|| ansi::width(&row[..cut]));
    let next = if up {
        here.checked_sub(1)
    } else {
        Some(here + 1)
    };
    let target = next.and_then(|n| rows.get(n).map(|row| (n, row)));
    let (position, upstream) = target.map_or((None, false), |(n, (start, row))| {
        let display = start + ansi::byte_at_column(row, column);
        let upstream = rows.get(n + 1).is_some_and(|(next, _)| *next == display);
        (Some(projection.source_at(display)), upstream)
    });
    Vertical {
        position,
        column,
        upstream,
    }
}

#[cfg(test)]
pub(super) struct Case {
    pub name: &'static str,
    pub text: &'static str,
    pub at: usize,
    pub keys: Vec<Key>,
    pub expected: &'static str,
}

#[cfg(test)]
pub(super) fn cases() -> Vec<Case> {
    let motion = |direction, ctrl, shift| Key::Motion {
        direction,
        ctrl,
        shift,
    };
    vec![
        Case {
            name: "kill-end",
            text: "abcTAIL",
            at: 3,
            keys: vec![Key::Ctrl('k')],
            expected: "abc",
        },
        Case {
            name: "end-noop",
            text: "abc",
            at: 3,
            keys: vec![Key::Ctrl('k')],
            expected: "abc",
        },
        Case {
            name: "kill-start",
            text: "abcTAIL",
            at: 3,
            keys: vec![Key::Ctrl('u')],
            expected: "TAIL",
        },
        Case {
            name: "word-left",
            text: "one two",
            at: 7,
            keys: vec![motion(Direction::Left, true, false), Key::Char('X')],
            expected: "one Xtwo",
        },
        Case {
            name: "word-right",
            text: "one two",
            at: 0,
            keys: vec![motion(Direction::Right, true, false), Key::Char('X')],
            expected: "oneX two",
        },
        Case {
            name: "word-kill-left",
            text: "one two",
            at: 7,
            keys: vec![Key::Ctrl('w')],
            expected: "one ",
        },
        Case {
            name: "word-kill-right",
            text: "one two",
            at: 3,
            keys: vec![Key::Alt('d')],
            expected: "one",
        },
        Case {
            name: "kill-yank",
            text: "one two",
            at: 7,
            keys: vec![Key::Ctrl('w'), Key::Ctrl('y')],
            expected: "one two",
        },
        Case {
            name: "undo",
            text: "one two",
            at: 7,
            keys: vec![Key::Ctrl('w'), Key::Ctrl('_')],
            expected: "one two",
        },
        Case {
            name: "redo",
            text: "one two",
            at: 7,
            keys: vec![Key::Ctrl('w'), Key::Ctrl('_'), Key::Alt('r')],
            expected: "one ",
        },
        Case {
            name: "ring-previous",
            text: "one two",
            at: 7,
            keys: vec![
                Key::Ctrl('w'),
                Key::Left,
                Key::End,
                Key::Ctrl('u'),
                Key::Ctrl('y'),
                Key::Alt('y'),
            ],
            expected: "two",
        },
        Case {
            name: "select-left-replace",
            text: "abcd",
            at: 3,
            keys: vec![motion(Direction::Left, false, true), Key::Char('X')],
            expected: "abXd",
        },
        Case {
            name: "select-right-delete",
            text: "abcd",
            at: 1,
            keys: vec![motion(Direction::Right, false, true), Key::Delete],
            expected: "acd",
        },
        Case {
            name: "select-word-left",
            text: "one two",
            at: 7,
            keys: vec![motion(Direction::Left, true, true), Key::Paste("X".into())],
            expected: "one X",
        },
        Case {
            name: "select-word-right",
            text: "one two",
            at: 0,
            keys: vec![motion(Direction::Right, true, true), Key::Backspace],
            expected: " two",
        },
        Case {
            name: "transpose-glyph",
            text: "abcd",
            at: 2,
            keys: vec![Key::Alt('x')],
            expected: "acbd",
        },
        Case {
            name: "transpose-word",
            text: "one two",
            at: 7,
            keys: vec![Key::Alt('w')],
            expected: "two one",
        },
        Case {
            name: "uppercase",
            text: "one two",
            at: 4,
            keys: vec![Key::Alt('u')],
            expected: "one TWO",
        },
        Case {
            name: "lowercase",
            text: "ONE two",
            at: 0,
            keys: vec![Key::Alt('l')],
            expected: "one two",
        },
        Case {
            name: "capitalize",
            text: "hELLo two",
            at: 0,
            keys: vec![Key::Alt('v')],
            expected: "Hello two",
        },
        Case {
            name: "line-start",
            text: "abc",
            at: 3,
            keys: vec![Key::Ctrl('a'), Key::Char('X')],
            expected: "Xabc",
        },
        Case {
            name: "line-end",
            text: "abc",
            at: 0,
            keys: vec![Key::Ctrl('e'), Key::Char('X')],
            expected: "abcX",
        },
        Case {
            name: "buffer-start",
            text: "abc",
            at: 3,
            keys: vec![Key::Home, Key::Char('X')],
            expected: "Xabc",
        },
        Case {
            name: "buffer-end",
            text: "abc",
            at: 0,
            keys: vec![Key::End, Key::Char('X')],
            expected: "abcX",
        },
        Case {
            name: "paste-preserves-spaces",
            text: "abc",
            at: 1,
            keys: vec![Key::Paste(" X ".into())],
            expected: "a X bc",
        },
        Case {
            name: "glyph-backspace",
            text: "e\u{301}x",
            at: 3,
            keys: vec![Key::Backspace],
            expected: "x",
        },
        Case {
            name: "glyph-delete",
            text: "👩\u{200d}💻x",
            at: 0,
            keys: vec![Key::Delete],
            expected: "x",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual_row(state: &State, text: &str, cursor: usize, width: usize) -> usize {
        let projection = Projection::plain(text);
        let rows = super::super::draw::compose_rows(&projection.text, width);
        state.row_index(text, cursor, width, &rows, projection.display_at(cursor))
    }

    #[test]
    fn full_wrapped_rows_keep_the_side_of_the_boundary_and_never_stick_or_skip() {
        for width in [1, 2, 5, 70, 74] {
            for glyph in ["x", "界", "e\u{301}"] {
                let cells = ansi::width(glyph);
                let width = width.max(cells);
                let line = glyph.repeat(width / cells);
                let width = ansi::width(&line);
                let mut text = line.repeat(2);
                let mut at = text.len();
                let mut state = State::default();
                assert!(state.vertical_step(&text, &mut at, width, true).is_some());
                assert_eq!(at, line.len());
                assert_eq!(visual_row(&state, &text, at, width), 0);
                assert!(state.vertical_step(&text, &mut at, width, true).is_none());
                assert!(state.vertical_step(&text, &mut at, width, false).is_some());
                assert_eq!(at, text.len());
                state.key(
                    &mut text,
                    &mut at,
                    &Key::Motion {
                        direction: Direction::Up,
                        ctrl: false,
                        shift: true,
                    },
                    Options::multiline(width),
                );
                assert_eq!(state.selected(&text, at), Some(line.len()..text.len()));
                assert_eq!(visual_row(&state, &text, at, width), 0);
                assert_eq!(
                    visual_row(&state, &text, at, width * 2),
                    0,
                    "old width affinity is not reused"
                );
            }
            let mut text = format!("{}\n{}", "h".repeat(width), "t".repeat(width * 2));
            let mut at = width;
            let mut state = State::default();
            state.vertical_step(&text, &mut at, width, false);
            assert_eq!(
                visual_row(&state, &text, at, width),
                1,
                "Down must not skip a wrapped row"
            );
            state.key(
                &mut text,
                &mut at,
                &Key::Char('X'),
                Options::multiline(width),
            );
            assert!(text.contains('X'));
        }
    }

    #[test]
    fn clicking_the_end_of_a_wrapped_row_retains_that_row() {
        let text = "abcdefghij";
        let projection = Projection::plain(text);
        let row = projection.row(0, "abcde", 5);
        let mut state = State::default();
        let mut at = 0;
        state.place_in_row(text, &mut at, &row, 100, 5);
        assert_eq!(at, 5);
        assert_eq!(visual_row(&state, text, at, 5), 0);
        state.vertical_step(text, &mut at, 5, false);
        assert_eq!(at, 10);
        state.select_in_row(text, &mut at, 0, &row, 100, 5);
        assert_eq!(state.selected(text, at), Some(0..5));
        assert_eq!(visual_row(&state, text, at, 5), 0);
    }

    fn run(text: &str, at: usize, keys: &[Key]) -> (String, usize, State) {
        let mut text = text.to_owned();
        let mut at = at;
        let mut state = State::default();
        for key in keys {
            assert!(
                state.key(&mut text, &mut at, key, Options::multiline(80)),
                "{key:?}"
            );
        }
        (text, at, state)
    }
    fn chord(action: Action) -> Key {
        BINDINGS
            .iter()
            .find(|(_, _, a)| *a == action)
            .unwrap()
            .1
            .clone()
    }

    #[test]
    fn deterministic_edit_sequences_keep_every_boundary_valid_and_each_change_reversible() {
        let mut seed = 0x61d3_f00d_u64;
        for width in [1, 3, 12, 80] {
            let mut text = "a\té界e\u{301}👩🏽\u{200d}💻🇮🇩\nsecond line".to_owned();
            let mut cursor = text.len();
            let mut state = State::default();
            let mut options = Options::multiline(width);
            options.max_bytes = 4096;
            options.max_chars = 1024;
            for step in 0..800 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let key = match seed % 7 {
                    0 => Key::Paste(
                        ["\r\ntext", "é", "e\u{301}", "👩\u{200d}💻", "🇮🇩", "  "]
                            [(seed as usize >> 16) % 6]
                            .into(),
                    ),
                    1 => Key::Char(['a', 'ß', '界', '\u{301}'][(seed as usize >> 12) % 4]),
                    _ => BINDINGS[(seed as usize >> 24) % BINDINGS.len()].1.clone(),
                };
                let before = text.clone();
                let before_cursor = cursor;
                state.key(&mut text, &mut cursor, &key, options);
                assert!(text.len() <= 4096 && text.chars().count() <= 1024);
                assert!(text.is_char_boundary(cursor), "{step} {key:?}");
                assert!(boundaries(&text).contains(&cursor), "{step} {key:?}");
                if let Some(selected) = state.selected(&text, cursor) {
                    assert!(boundaries(&text).contains(&selected.start));
                    assert!(boundaries(&text).contains(&selected.end));
                }
                if before != text && !matches!(key, Key::Ctrl('_') | Key::Alt('r')) {
                    let after = text.clone();
                    let after_cursor = cursor;
                    let mut clone = state.clone();
                    clone.key(&mut text, &mut cursor, &Key::Ctrl('_'), options);
                    assert_eq!(text, before, "undo {step} {key:?}");
                    assert_eq!(cursor, boundary_after(&before, before_cursor));
                    clone.key(&mut text, &mut cursor, &Key::Alt('r'), options);
                    assert_eq!(
                        (text.as_str(), cursor),
                        (after.as_str(), after_cursor),
                        "redo {step} {key:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn undo_and_ring_memory_limits_are_enforced_and_a_shrunk_field_refuses_restore() {
        let mut text = String::new();
        let mut at = 0;
        let mut state = State::default();
        let opts = Options::multiline(80);
        for _ in 0..600 {
            state.key(&mut text, &mut at, &Key::Char('a'), opts);
        }
        assert_eq!(state.undo.len(), HISTORY_ENTRIES);
        state.key(&mut text, &mut at, &Key::Ctrl('u'), opts);
        assert_eq!(text, "");
        state.key(&mut text, &mut at, &Key::Ctrl('_'), Options::single(5, 5));
        assert_eq!(text, "");
        state.key(&mut text, &mut at, &Key::Ctrl('_'), opts);
        assert_eq!(text.len(), 600);
        for n in 0..30 {
            state.observe("", 0);
            text = String::new();
            at = 0;
            state.key(&mut text, &mut at, &Key::Paste(format!("{n}")), opts);
            state.key(&mut text, &mut at, &Key::Ctrl('u'), opts);
        }
        assert!(state.ring.len() <= RING_ENTRIES);
        assert!(state.ring.iter().map(String::len).sum::<usize>() <= RING_BYTES);
    }

    #[test]
    fn canonical_chords_and_actions_are_both_unique_without_readline_aliases() {
        for (i, (_, key, act)) in BINDINGS.iter().enumerate() {
            assert!(
                !BINDINGS[..i].iter().any(|(_, k, a)| k == key || a == act),
                "{key:?} {act:?}"
            );
        }
        for key in [
            Key::Alt('b'),
            Key::Alt('f'),
            Key::Alt('\u{8}'),
            Key::Ctrl('b'),
            Key::Ctrl('f'),
            Key::Ctrl('d'),
        ] {
            assert_eq!(action(&key), None);
        }
    }

    #[test]
    fn every_canonical_key_has_user_help_and_no_application_shortcut_collision() {
        let help = include_str!("../../../.rune/skills/rune-settings-guide/cli/shortcuts.md");
        for (label, key, _) in BINDINGS {
            assert!(
                help.contains(&format!("| {label} |")),
                "missing user help: {label}"
            );
            assert!(
                !matches!(
                    key,
                    Key::Ctrl('g' | 't' | 'o' | 'l' | 'r' | 'c') | Key::Alt('t') | Key::CtrlDown
                ),
                "application collision {label}"
            );
        }
        assert!(help.contains("Ctrl+K never erases the prefix"));
        assert!(help.contains("recalls prompt history"));
    }

    #[test]
    fn ctrl_k_only_kills_forward_and_at_newline_joins_exactly_one_line() {
        for (before, at, expected) in [
            ("abcTAIL", 3, "abc"),
            ("abc", 3, "abc"),
            ("", 0, ""),
            ("first\nabcTAIL\nlast", 9, "first\nabc\nlast"),
            ("abc\nlast", 3, "abclast"),
            ("\nlast", 0, "last"),
            ("a\n\nb", 2, "a\nb"),
        ] {
            let (text, cursor, _) = run(before, at, &[Key::Ctrl('k')]);
            assert_eq!(text, expected, "{before:?} at {at}");
            assert_eq!(cursor, at);
        }
    }

    #[test]
    fn every_canonical_nonselect_action_has_its_own_expected_transaction() {
        use Action::*;
        for (act, text, at, expected, cursor) in [
            (Left, "ab", 2, "ab", 1),
            (Right, "ab", 0, "ab", 1),
            (WordLeft, "one two", 7, "one two", 4),
            (WordRight, "one two", 0, "one two", 3),
            (LineStart, "a\nbc", 4, "a\nbc", 2),
            (LineEnd, "a\nbc\nx", 2, "a\nbc\nx", 4),
            (Start, "a\nbc", 4, "a\nbc", 0),
            (End, "a\nbc", 0, "a\nbc", 4),
            (Backspace, "abc", 2, "ac", 1),
            (Delete, "abc", 1, "ac", 1),
            (KillStart, "a\nbcTAIL", 4, "a\nTAIL", 2),
            (KillEnd, "abcTAIL", 3, "abc", 3),
            (KillWordLeft, "one two ", 8, "one ", 4),
            (KillWordRight, "one two", 3, "one", 3),
            (Transpose, "abcd", 2, "acbd", 3),
            (Transpose, "abcd", 4, "abdc", 4),
            (TransposeWords, "one two three", 7, "two one three", 7),
            (Upper, "one straße", 4, "one STRASSE", 11),
            (Lower, "ABC", 0, "abc", 3),
            (Capitalize, "hELLo", 0, "Hello", 5),
        ] {
            let (got, pos, _) = run(text, at, &[chord(act)]);
            assert_eq!((got.as_str(), pos), (expected, cursor), "{act:?}");
        }
    }

    #[test]
    fn all_selection_motions_replace_only_selected_source_and_undo_restores_it() {
        use Action::*;
        for (act, at, end) in [
            (SelectLeft, 3, 2),
            (SelectRight, 1, 2),
            (SelectWordLeft, 7, 4),
            (SelectWordRight, 0, 3),
        ] {
            let mut text = "one two".to_owned();
            let before = text.clone();
            let mut cursor = at;
            let mut state = State::default();
            state.key(&mut text, &mut cursor, &chord(act), Options::multiline(80));
            assert_eq!(
                state.selected(&text, cursor),
                Some(at.min(end)..at.max(end))
            );
            state.key(
                &mut text,
                &mut cursor,
                &Key::Char('X'),
                Options::multiline(80),
            );
            let mut expected = before.clone();
            expected.replace_range(at.min(end)..at.max(end), "X");
            assert_eq!(text, expected);
            state.key(
                &mut text,
                &mut cursor,
                &Key::Ctrl('_'),
                Options::multiline(80),
            );
            assert_eq!(text, before);
            state.key(
                &mut text,
                &mut cursor,
                &Key::Alt('r'),
                Options::multiline(80),
            );
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn vertical_motion_keeps_desired_column_across_short_lines_and_selects_rows() {
        let mut text = "abcd\nx\nabcd".to_owned();
        let mut at = 3;
        let mut state = State::default();
        let options = Options::multiline(80);
        for key in [Key::Down, Key::Down] {
            state.key(&mut text, &mut at, &key, options);
        }
        assert_eq!(at, 10);
        state.key(&mut text, &mut at, &Key::Up, options);
        assert_eq!(at, 6);
        state.key(&mut text, &mut at, &Key::Up, options);
        assert_eq!(at, 3);
        for act in [Action::SelectDown, Action::SelectUp] {
            state.key(&mut text, &mut at, &chord(act), options);
        }
        assert_eq!(at, 3);
        assert_eq!(state.selected(&text, at), None);
        state.key(&mut text, &mut at, &Key::Up, options);
        assert_eq!(at, 3, "top edge holds, no history");
    }

    #[test]
    fn kills_yank_and_ring_cycle_are_separate_undoable_bounded_actions() {
        let mut text = "one two".to_owned();
        let mut at = 7;
        let mut state = State::default();
        let opts = Options::multiline(80);
        for key in [Key::Ctrl('w'), Key::Ctrl('y')] {
            state.key(&mut text, &mut at, &key, opts);
        }
        assert_eq!(text, "one two");
        state.key(&mut text, &mut at, &Key::Ctrl('u'), opts);
        assert_eq!(text, "");
        state.key(&mut text, &mut at, &Key::Ctrl('y'), opts);
        assert_eq!(text, "one two");
        state.key(&mut text, &mut at, &Key::Alt('y'), opts);
        assert_eq!(text, "two");
        state.key(&mut text, &mut at, &Key::Ctrl('_'), opts);
        assert_eq!(text, "one two");
        state.key(&mut text, &mut at, &Key::Alt('r'), opts);
        assert_eq!(text, "two");
        state.key(&mut text, &mut at, &Key::Char('!'), opts);
        state.key(&mut text, &mut at, &Key::Alt('y'), opts);
        assert_eq!(text, "two!");
        let (text, _, _) = run(
            "one two",
            7,
            &[Key::Ctrl('w'), Key::Ctrl('w'), Key::Ctrl('y')],
        );
        assert_eq!(text, "one two");
    }

    #[test]
    fn an_ack_preserves_undo_but_other_text_or_another_field_never_inherits_it() {
        let mut text = String::new();
        let mut at = 0;
        let mut state = State::default();
        let opts = Options::multiline(80);
        state.key(&mut text, &mut at, &Key::Paste("secret-local".into()), opts);
        state.observe(&text, at);
        state.key(&mut text, &mut at, &Key::Ctrl('_'), opts);
        assert_eq!(text, "");
        state.key(&mut text, &mut at, &Key::Alt('r'), opts);
        assert_eq!(text, "secret-local");
        text = "other window".into();
        at = text.len();
        state.key(&mut text, &mut at, &Key::Ctrl('_'), opts);
        assert_eq!(text, "other window");
        let mut other = State::default();
        other.key(&mut text, &mut at, &Key::Ctrl('y'), opts);
        assert_eq!(text, "other window");
        state.reset();
        state.key(&mut text, &mut at, &Key::Ctrl('y'), opts);
        assert_eq!(text, "other window");
    }

    #[test]
    fn glyph_moves_and_deletes_do_not_split_accents_zwj_or_variation_sequences() {
        for glyph in [
            "é",
            "e\u{301}",
            "界",
            "👩\u{200d}💻",
            "❤️",
            "👍🏽",
            "🇮🇩",
            "1️⃣",
        ] {
            let input = format!("{glyph}X");
            let end = glyph.len();
            let (text, at, _) = run(&input, end, &[Key::Backspace]);
            assert_eq!((text.as_str(), at), ("X", 0));
            let (text, at, _) = run(&input, 0, &[Key::Delete]);
            assert_eq!((text.as_str(), at), ("X", 0));
            assert_eq!(run(&input, end, &[Key::Left]).1, 0);
            assert_eq!(run(&input, 0, &[Key::Right]).1, end);
        }
    }

    #[test]
    fn word_motion_treats_tabs_newlines_and_unicode_spaces_as_boundaries() {
        for text in ["one\ttwo", "one\ntwo", "one\u{2003}two"] {
            let (_, at, _) = run(text, 0, &[chord(Action::WordRight)]);
            assert_eq!(at, 3, "{text:?}");
            let (_, at, _) = run(text, text.len(), &[chord(Action::WordLeft)]);
            assert_eq!(at, text.len() - 3, "{text:?}");
            let (got, _, _) = run(text, 0, &[Key::Alt('d')]);
            assert_eq!(got, &text[3..]);
        }
        let (_, at, _) = run("e\u{301}\tword", 0, &[chord(Action::WordRight)]);
        assert_eq!(at, 3);
        assert_eq!(run("one\ttwo", 7, &[Key::Alt('w')]).0, "two\tone");
        assert_eq!(run("one\ttwo", 0, &[Key::Alt('u')]).0, "ONE\ttwo");
    }

    #[test]
    fn paste_limits_case_expansion_and_single_line_are_atomic_without_trimming() {
        let mut text = String::new();
        let mut at = 0;
        let mut state = State::default();
        let opts = Options::single(5, 10);
        state.key(&mut text, &mut at, &Key::Paste(" ab ".into()), opts);
        assert_eq!(text, " ab ");
        state.key(&mut text, &mut at, &Key::Paste("\r\nX".into()), opts);
        assert_eq!(text, " ab ");
        state.key(&mut text, &mut at, &Key::Paste("XX".into()), opts);
        assert_eq!(text, " ab ");
        state.key(&mut text, &mut at, &Key::Ctrl('_'), opts);
        assert_eq!(text, "");
        let (text, _, _) = run("", 0, &[Key::Paste("a\r\nb\rc".into())]);
        assert_eq!(text, "a\nb\nc");
        let mut text = "ßß".into();
        let mut at = 0;
        let mut state = State::default();
        state.key(&mut text, &mut at, &Key::Alt('u'), Options::single(3, 10));
        assert_eq!(text, "ßß");
    }
}
