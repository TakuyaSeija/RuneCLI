//! `/undo`: every file the session changed, each put back on its own.
//!
//! Undo is per file (owner, 2026-10-09). The list is the session's file
//! ledger — the same rows, order and `+N −N` as the side panel's FILES
//! CHANGED, asked of the engine again as the panel opens so it reads as the
//! files stand now — and undoing a row puts that file back the way it was
//! before the session first changed it. Every change made since is lost, by
//! the agent or anyone else, so the panel says so, Alt+S shows exactly what
//! goes, and the act takes a second Enter within [`CONFIRM`]: the first one
//! starts a countdown in the state line, and one after it ran out starts it
//! again rather than restoring.
//!
//! `/redo` is the same panel the other way (owner, 2026-10-09): A is a file
//! as an undo found it, B as the undo left it. Redo lists only files that
//! were undone, shows the file as it is now against A, puts A back whatever
//! became of B, and is used up — the file leaves the list until its next
//! undo. One module for both: the [`Side`] is the only difference.
//!
//! The cursor stays mid-screen while the list scrolls under it and stops
//! when the last row is drawn — the way Settings › Models moves
//! (`panel_view::Document::centre`).

use std::time::{Duration, Instant};

use runeengine::Command;
use serde_json::Value;

use super::ansi::{self, ACCENT, ADDED, FAINT, MUTED, REMOVED, RESET, RULE, TEXT, WARN};
use super::draw::Hit;
use super::input::Key;
use super::panel_view::Document;
use super::panels::{heading, paragraph};
use super::{App, Awaiting, Panel};
use crate::driver::Driver;

/// How long a first Enter waits for the second one that restores.
pub const CONFIRM: Duration = Duration::from_secs(3);

/// Rows PageUp/PageDown move the cursor, as in Settings › Models.
const PAGE: usize = 5;

/// Which way the panel puts files back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// To before the session first changed the file (`/undo`).
    Undo,
    /// To how the file's last undo found it (`/redo`).
    Redo,
}

impl Side {
    /// The side of the panel on screen, if it is one of these.
    pub fn of(app: &App) -> Option<Side> {
        match app.panel {
            Panel::Undo => Some(Side::Undo),
            Panel::Redo => Some(Side::Redo),
            _ => None,
        }
    }

    pub fn panel(self) -> Panel {
        match self {
            Side::Undo => Panel::Undo,
            Side::Redo => Panel::Redo,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Side::Undo => "undo",
            Side::Redo => "redo",
        }
    }

    fn button(self) -> &'static str {
        match self {
            Side::Undo => "[undo]",
            Side::Redo => "[redo]",
        }
    }

    fn warning(self) -> &'static str {
        match self {
            Side::Undo => "Undo puts a file back the way it was before this session first changed it: every change made to it since — by the agent or anyone else — is lost.",
            Side::Redo => "Redo puts a file back the way it was when you undid it: every change made to it since the undo — by the agent or anyone else — is lost.",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Side::Undo => "This session has changed no file yet.",
            Side::Redo => "Nothing to redo: undo a file first.",
        }
    }

    fn unchanged(self) -> &'static str {
        match self {
            Side::Undo => "Nothing to undo: the file is as it was before this session.",
            Side::Redo => "Nothing to redo: the file is as it was when you undid it.",
        }
    }

    /// The rows this side lists: the side panel's FILES CHANGED for undo,
    /// the files with a redo for redo.
    fn files(self, app: &App) -> &[Value] {
        match self {
            Side::Undo => &app.file_changes,
            Side::Redo => &app.redo_files,
        }
    }
}

/// A restore armed by a first Enter: which file, since when, and the whole
/// seconds left the state line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arm {
    pub file: String,
    pub at: Instant,
    pub left: u64,
}

/// A file's diff on screen in place of the list (Alt+S).
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub file: String,
    pub diff: Value,
}

/// Open the panel and ask the engine for the files as they stand now.
pub fn open(app: &mut App, d: &mut Driver, side: Side) {
    let Some(session) = app.session else {
        app.note(format!(
            "An unsent draft has changed no file yet: there is nothing to {}.",
            side.verb()
        ));
        return;
    };
    app.show_panel(side.panel());
    app.panel_row = 0;
    app.undo_diff = None;
    app.undo_armed = None;
    app.undo_refused = None;
    app.panel_view.reset();
    app.request(d, Awaiting::UndoFiles { side }, |id| match side {
        Side::Undo => Command::CheckpointFiles { id, session },
        Side::Redo => Command::CheckpointRedos { id, session },
    });
}

/// The path the cursor is on.
fn selected(app: &App) -> Option<String> {
    let files = Side::of(app)?.files(app);
    let n = files.len();
    (n > 0)
        .then(|| files[app.panel_row.min(n - 1)]["path"].as_str())
        .flatten()
        .map(str::to_string)
}

/// The file an Enter would undo: the one whose diff is open, else the row.
fn target(app: &App) -> Option<String> {
    match &app.undo_diff {
        Some(shown) => Some(shown.file.clone()),
        None => selected(app),
    }
}

pub fn document(app: &App, width: usize) -> Document {
    let side = Side::of(app).unwrap_or(Side::Undo);
    let mut doc = Document::reader();
    doc.centre = true;
    doc.append(
        "title",
        vec![String::new(), heading(&side.verb().to_uppercase(), width)],
    );
    doc.append("hint", paragraph(&hint(app, side), FAINT, 0, width));
    doc.append("warning", paragraph(side.warning(), WARN, 0, width));
    if let Some(why) = &app.undo_refused {
        doc.append("refused", paragraph(why, WARN, 0, width));
    }
    doc.append("gap", vec![String::new()]);
    match &app.undo_diff {
        Some(shown) => diff(&mut doc, shown, side, width),
        None => list(&mut doc, app, side, width),
    }
    doc
}

/// The one line of keys the panel answers to.
fn hint(app: &App, side: Side) -> String {
    let verb = side.verb();
    if app.undo_diff.is_some() {
        format!("↑↓ pgup/pgdn scroll · enter {verb} · alt+s back to the list · esc close")
    } else {
        format!("↑↓ pgup/pgdn choose · enter {verb} · alt+s show diff · esc close")
    }
}

/// When the file was last changed: the time today, the date before.
fn when(at: Option<i64>) -> String {
    let Some(at) = at else {
        return "—".into();
    };
    let day = |ms: i64| super::term::epoch_ymd_hm(ms).get(..10).map(str::to_string);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    if day(at).is_some() && day(at) == day(now) {
        super::term::epoch_hm(at)
    } else {
        super::term::epoch_ymd_hm(at)
    }
}

fn list(doc: &mut Document, app: &App, side: Side, width: usize) {
    let files = side.files(app);
    let button = side.button();
    if files.is_empty() {
        doc.append("empty", paragraph(side.empty(), MUTED, 2, width));
        return;
    }
    let index_w = files.len().to_string().len();
    let times: Vec<String> = files.iter().map(|f| when(f["last_at"].as_i64())).collect();
    let time_w = times.iter().map(|t| ansi::width(t)).max().unwrap_or(5);
    let counts: Vec<String> = files.iter().map(super::draw::file_change_counts).collect();
    let counts_w = super::panels::file_change_counts_w(&counts);
    // `  › 12  17:49  ` before the path; `  +N −N  [undo]` after it.
    let indent = 4 + index_w + 2 + time_w + 2;
    let right = width.saturating_sub(1);
    let tail = 2 + counts_w + 2 + ansi::width(button);
    let path_w = right.saturating_sub(indent + tail).max(8);
    let button_at = indent + path_w + 2 + counts_w + 2;
    let chosen = app.panel_row.min(files.len() - 1);
    let armed = app.undo_armed.as_ref().map(|arm| arm.file.as_str());
    for (i, file) in files.iter().enumerate() {
        let path = file["path"].as_str().unwrap_or("?");
        let here = i == chosen;
        let cursor = if here {
            format!("{ACCENT}›{RESET}")
        } else {
            " ".into()
        };
        let ink = if here { ACCENT } else { TEXT };
        let button_ink = if armed == Some(path) {
            WARN
        } else if here {
            ACCENT
        } else {
            MUTED
        };
        let parts = ansi::wrap(&ansi::plain(path), path_w);
        let mut rows = Vec::with_capacity(parts.len());
        for (k, part) in parts.iter().enumerate() {
            let row = if k == 0 {
                format!(
                    "  {cursor} {MUTED}{:>index_w$}  {}{RESET}  {ink}{}{RESET}  {}  {button_ink}{button}{RESET}",
                    i + 1,
                    ansi::pad(&times[i], time_w),
                    ansi::pad(part, path_w),
                    ansi::pad(&counts[i], counts_w),
                )
            } else {
                format!("{}{ink}{part}{RESET}", " ".repeat(indent))
            };
            rows.push(ansi::truncate(&row, right));
        }
        let span = doc.append_at(format!("file:{path}"), rows, |k| Hit::UndoFile {
            index: i,
            button: if k == 0 {
                (button_at, button_at + ansi::width(button))
            } else {
                (0, 0)
            },
        });
        if here {
            doc.focus = Some(span);
        }
    }
}

fn diff(doc: &mut Document, shown: &Shown, side: Side, width: usize) {
    let hunks = shown.diff["hunks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let (mut added, mut removed) = (0u64, 0u64);
    for line in hunks
        .iter()
        .flat_map(|h| h["lines"].as_array().into_iter().flatten())
    {
        match line["op"].as_str() {
            Some("add") => added += 1,
            Some("del") => removed += 1,
            _ => {}
        }
    }
    let counts = super::draw::file_change_counts(&serde_json::json!({
        "added": added,
        "removed": removed,
    }));
    let title = format!("{ACCENT}{}{RESET}  {counts}", ansi::plain(&shown.file));
    let span = doc.append("diff:path", vec![ansi::truncate(&title, width)]);
    doc.focus = Some(span);
    doc.append("diff:gap", vec![String::new()]);
    match shown.diff["change"].as_str() {
        Some("binary") => {
            doc.append(
                "diff:body",
                paragraph(
                    "A binary file: there is nothing to show line by line.",
                    MUTED,
                    0,
                    width,
                ),
            );
            return;
        }
        Some("unchanged") => {
            doc.append("diff:body", paragraph(side.unchanged(), MUTED, 0, width));
            return;
        }
        _ => {}
    }
    doc.append("diff:body", hunk_rows(hunks, width));
}

/// The hunks GitHub's way: the old line number, the new one, then the line —
/// a removed line has only the old number, an added one only the new — and a
/// rule across between hunks. A long line wraps under its own text.
pub fn hunk_rows(hunks: &[Value], width: usize) -> Vec<String> {
    let last = hunks
        .iter()
        .map(|h| {
            let end = |start: &str, count: &str| {
                h[start].as_u64().unwrap_or(0) + h[count].as_u64().unwrap_or(0)
            };
            end("old_start", "old_count").max(end("new_start", "new_count"))
        })
        .max()
        .unwrap_or(0);
    let w = last.to_string().len().max(3);
    let gutter = 2 * w + 2;
    let text_w = width.saturating_sub(gutter + 4).max(8);
    let mut out = Vec::new();
    for (n, hunk) in hunks.iter().enumerate() {
        if n > 0 {
            out.push(format!(
                "{RULE}{}┼{}{RESET}",
                "─".repeat(gutter),
                "─".repeat(width.saturating_sub(gutter + 1))
            ));
        }
        let mut old = hunk["old_start"].as_u64().unwrap_or(1);
        let mut new = hunk["new_start"].as_u64().unwrap_or(1);
        for line in hunk["lines"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            let blank = " ".repeat(w);
            let (numbers, ink, mark) = match line["op"].as_str() {
                Some("del") => {
                    let numbers = format!("{REMOVED}{old:>w$}{RESET} {blank}");
                    old += 1;
                    (numbers, REMOVED, "−")
                }
                Some("add") => {
                    let numbers = format!("{blank} {ADDED}{new:>w$}{RESET}");
                    new += 1;
                    (numbers, ADDED, "+")
                }
                _ => {
                    let numbers = format!("{MUTED}{old:>w$} {new:>w$}{RESET}");
                    old += 1;
                    new += 1;
                    (numbers, MUTED, " ")
                }
            };
            let text = ansi::tame(line["text"].as_str().unwrap_or(""));
            let parts = ansi::wrap(&text, text_w);
            for (k, part) in parts.iter().enumerate() {
                if k == 0 {
                    out.push(format!(
                        "{numbers} {RULE}│{RESET} {ink}{mark} {part}{RESET}"
                    ));
                } else {
                    out.push(format!(
                        "{} {RULE}│{RESET} {ink}  {part}{RESET}",
                        " ".repeat(gutter - 1)
                    ));
                }
            }
            if parts.is_empty() {
                out.push(format!("{numbers} {RULE}│{RESET} {ink}{mark}{RESET}"));
            }
        }
    }
    out
}

/// The panel's keys. Whether the key was the panel's.
pub fn key(app: &mut App, key: &Key, d: &mut Driver) -> bool {
    if app.undo_diff.is_some() {
        match key {
            Key::Up | Key::Down => app.panel_view.scroll_by(*key == Key::Up, 1),
            Key::PageUp | Key::PageDown => {
                let rows = app.panel_view.page();
                app.panel_view.scroll_by(*key == Key::PageUp, rows);
            }
            Key::Enter => press(app, d),
            Key::Alt('s') => close_diff(app),
            _ => return false,
        }
        return true;
    }
    let rows = Side::of(app).map_or(0, |side| side.files(app).len());
    let at = app.panel_row.min(rows.saturating_sub(1));
    let to = match key {
        Key::Up => at.saturating_sub(1),
        Key::Down => (at + 1).min(rows.saturating_sub(1)),
        Key::PageUp => at.saturating_sub(PAGE),
        Key::PageDown => (at + PAGE).min(rows.saturating_sub(1)),
        Key::Home => 0,
        Key::End => rows.saturating_sub(1),
        Key::Enter => {
            press(app, d);
            return true;
        }
        Key::Alt('s') => {
            show_diff(app, d);
            return true;
        }
        _ => return false,
    };
    choose(app, to);
    true
}

/// Put the cursor on row `to`; a countdown for another file stops.
fn choose(app: &mut App, to: usize) {
    if to != app.panel_row {
        app.panel_row = to;
        app.undo_refused = None;
        if app.undo_armed.as_ref().map(|arm| &arm.file) != selected(app).as_ref() {
            app.undo_armed = None;
        }
    }
    app.panel_view.reveal();
}

/// A click on a row chooses it; on its `[undo]`, it is also an Enter.
pub fn click(app: &mut App, index: usize, button: (usize, usize), col: u16, d: &mut Driver) {
    let Some(side) = Side::of(app) else {
        return;
    };
    if app.undo_diff.is_some() || index >= side.files(app).len() {
        return;
    }
    choose(app, index);
    let col = (col as usize).saturating_sub(1);
    if (button.0..button.1).contains(&col) {
        press(app, d);
    }
}

/// Esc inside the panel: the diff goes back to the list, a countdown stops.
/// Whether it did either — when not, Esc closes the panel.
pub fn esc(app: &mut App) -> bool {
    if app.undo_diff.is_some() {
        close_diff(app);
        return true;
    }
    app.undo_armed.take().is_some()
}

fn close_diff(app: &mut App) {
    app.undo_diff = None;
    app.panel_view.reset();
    app.panel_view.reveal();
}

/// Enter: the first starts the countdown, a second within it restores.
fn press(app: &mut App, d: &mut Driver) {
    let Some(file) = target(app) else {
        return;
    };
    let now = Instant::now();
    app.undo_refused = None;
    match &app.undo_armed {
        Some(arm) if arm.file == file && now.duration_since(arm.at) < CONFIRM => {
            app.undo_armed = None;
            restore(app, d, file);
        }
        _ => {
            app.undo_armed = Some(Arm {
                file,
                at: now,
                left: CONFIRM.as_secs(),
            })
        }
    }
}

fn restore(app: &mut App, d: &mut Driver, file: String) {
    let (Some(session), Some(side)) = (app.session, Side::of(app)) else {
        return;
    };
    app.status = Some(format!("{}ing {file}…", side.verb()));
    let what = Awaiting::Restored {
        file: file.clone(),
        side,
    };
    app.request(d, what, |id| match side {
        Side::Undo => Command::CheckpointRestore { id, session, file },
        Side::Redo => Command::CheckpointRedo { id, session, file },
    });
}

fn show_diff(app: &mut App, d: &mut Driver) {
    let (Some(file), Some(session), Some(side)) = (selected(app), app.session, Side::of(app))
    else {
        return;
    };
    app.awaiting
        .retain(|_, what| !matches!(what, Awaiting::UndoDiff { .. }));
    let what = Awaiting::UndoDiff {
        file: file.clone(),
        side,
    };
    app.request(d, what, |id| match side {
        Side::Undo => Command::CheckpointDiff { id, session, file },
        Side::Redo => Command::CheckpointRedoDiff { id, session, file },
    });
}

/// The countdown in the state line while a restore is armed.
pub fn countdown(app: &App) -> Option<String> {
    let side = Side::of(app)?;
    let arm = app.undo_armed.as_ref()?;
    Some(format!(
        "Press Enter within {}s to {} {}",
        arm.left,
        side.verb(),
        ansi::plain(&arm.file)
    ))
}

/// Run the countdown down. Whether the screen changed.
pub fn tick(app: &mut App, now: Instant) -> bool {
    let open = Side::of(app).is_some();
    let Some(arm) = app.undo_armed.as_mut() else {
        return false;
    };
    let spent = now.duration_since(arm.at);
    if !open || spent >= CONFIRM {
        app.undo_armed = None;
        return true;
    }
    let left = (CONFIRM - spent).as_secs_f64().ceil() as u64;
    if left != arm.left {
        arm.left = left;
        return true;
    }
    false
}

/// `checkpoint.files` or `checkpoint.redos` answered: the rows. Undo's are
/// the side panel's too.
pub fn listed(app: &mut App, side: Side, data: &Value) {
    if let Some(files) = data["files"].as_array() {
        match side {
            Side::Undo => app.file_changes = files.clone(),
            Side::Redo => app.redo_files = files.clone(),
        }
    }
    if Side::of(app) != Some(side) {
        return;
    }
    app.panel_row = app.panel_row.min(side.files(app).len().saturating_sub(1));
    app.panel_view.reveal();
    app.transcript_dirty = true;
}

/// A file's diff answered: shown unless the cursor moved on.
pub fn diffed(app: &mut App, side: Side, file: String, data: Value) {
    if Side::of(app) != Some(side) || selected(app).as_deref() != Some(file.as_str()) {
        return;
    }
    app.undo_diff = Some(Shown {
        file,
        diff: data["diff"].clone(),
    });
    app.panel_view.reset();
    app.transcript_dirty = true;
}

/// `checkpoint.restore` or `checkpoint.redo` answered: the rows now read as
/// the files stand — an undone file stays at `+0 −0`, a redone one leaves the
/// redo list until its next undo.
pub fn restored(app: &mut App, side: Side, file: &str, data: &Value) {
    let wrote = data["restored"] == true;
    app.status = Some(match (side, wrote) {
        (Side::Undo, true) => format!("Undone: {file} is back the way it was before this session"),
        (Side::Undo, false) => {
            format!("Nothing to undo: {file} is already as it was before this session")
        }
        (Side::Redo, true) => format!("Redone: {file} is back the way it was when you undid it"),
        (Side::Redo, false) => {
            format!("Nothing to redo: {file} is already as it was when you undid it")
        }
    });
    if let Some(files) = data["file_changes"].as_array() {
        app.file_changes = files.clone();
    }
    if let Some(files) = data["redos"].as_array() {
        app.redo_files = files.clone();
    }
    if let Some(side) = Side::of(app) {
        app.panel_row = app.panel_row.min(side.files(app).len().saturating_sub(1));
    }
    app.undo_diff = None;
    app.panel_view.reveal();
    app.transcript_dirty = true;
}

/// The engine refused a diff or a restore: said in the panel until the
/// choice changes.
pub fn refused(app: &mut App, error: Option<&str>) {
    app.undo_refused = Some(if app.busy {
        "Not done: the session is working. Enter again once the turn is done.".into()
    } else {
        format!("Not done: {}", error.unwrap_or("the engine refused it"))
    });
    app.panel_view.reveal();
    app.transcript_dirty = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(doc: &Document) -> String {
        doc.sink
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(files: Vec<Value>) -> App {
        let mut app = App::for_test();
        app.panel = Panel::Undo;
        app.file_changes = files;
        app
    }

    fn files() -> Vec<Value> {
        vec![
            json!({"path": "Cli/src/ui/draw.rs", "change": "modified", "added": 47, "removed": 3, "last_at": 1_700_000_000_000i64}),
            json!({"path": "notes.md", "change": "created", "added": 25, "removed": 0, "last_at": null}),
            json!({"path": "back.rs", "change": "unchanged", "added": 0, "removed": 0}),
        ]
    }

    /// The owner's layout: one header, one line of keys, the warning, then
    /// `N  time  path  +N −N  [undo]` per file in first-touch order.
    #[test]
    fn the_list_is_one_row_per_file_with_its_number_time_counts_and_button() {
        let app = app_with(files());
        let doc = document(&app, 100);
        let shown = text(&doc);
        assert_eq!(shown.matches("UNDO").count(), 1, "{shown}");
        assert!(shown.contains("enter undo · alt+s show diff"), "{shown}");
        assert!(
            shown.contains("before this session first changed it"),
            "{shown}"
        );
        assert!(
            !shown.contains("turn"),
            "no turn left in the panel: {shown}"
        );
        let rows: Vec<&str> = shown.lines().filter(|l| l.contains("[undo]")).collect();
        assert_eq!(rows.len(), 3, "{shown}");
        assert!(rows[0].trim_start().starts_with("› 1"), "{}", rows[0]);
        assert!(
            rows[0].contains(&super::super::term::epoch_ymd_hm(1_700_000_000_000)),
            "{}",
            rows[0]
        );
        assert!(rows[0].contains("+47 −3"), "{}", rows[0]);
        assert!(
            rows[1].contains("2  —") && rows[1].contains("+25") && !rows[1].contains('−'),
            "{}",
            rows[1]
        );
        assert!(
            rows[2].contains("+0 −0"),
            "an undone file stays, at zero: {}",
            rows[2]
        );
        let column = |r: &str| ansi::width(&r[..r.find("[undo]").unwrap()]);
        let buttons: Vec<usize> = rows.iter().map(|r| column(r)).collect();
        assert!(
            buttons.windows(2).all(|w| w[0] == w[1]),
            "the buttons line up: {rows:?}"
        );
    }

    #[test]
    fn a_long_path_hangs_under_itself_and_keeps_the_button_column() {
        let mut list = files();
        list[0]["path"] = json!("a/very/long/path/that/does/not/fit/in/the/column/at/all/file.rs");
        let app = app_with(list);
        let shown = text(&document(&app, 60));
        assert!(shown.lines().all(|l| ansi::width(l) <= 60), "{shown}");
        let first = shown.lines().position(|l| l.contains("› 1")).unwrap();
        let next = shown.lines().nth(first + 1).unwrap();
        assert!(
            !next.contains("[undo]") && next.starts_with("   "),
            "{shown}"
        );
    }

    /// Enter once arms; the state line counts 3, 2, 1; Enter inside restores;
    /// Enter after it ran out arms again instead.
    #[test]
    fn a_restore_takes_a_second_enter_within_the_countdown() {
        let mut app = app_with(files());
        app.session = Some(1);
        let mut d = super::super::test_driver("undo-arm");
        key(&mut app, &Key::Enter, &mut d);
        let arm = app.undo_armed.clone().expect("armed");
        assert_eq!(arm.file, "Cli/src/ui/draw.rs");
        assert_eq!(
            countdown(&app).as_deref(),
            Some("Press Enter within 3s to undo Cli/src/ui/draw.rs")
        );
        assert!(tick(&mut app, arm.at + Duration::from_millis(1100)));
        assert_eq!(app.undo_armed.as_ref().unwrap().left, 2);
        assert!(!app
            .awaiting
            .values()
            .any(|w| matches!(w, Awaiting::Restored { .. })));

        key(&mut app, &Key::Enter, &mut d);
        assert!(app.undo_armed.is_none());
        assert!(app
            .awaiting
            .values()
            .any(|w| matches!(w, Awaiting::Restored { file, .. } if file == "Cli/src/ui/draw.rs")));

        // Run out, then Enter: a fresh countdown, no restore.
        app.awaiting.clear();
        key(&mut app, &Key::Enter, &mut d);
        let at = app.undo_armed.as_ref().unwrap().at;
        assert!(tick(&mut app, at + CONFIRM));
        assert!(app.undo_armed.is_none() && countdown(&app).is_none());
        key(&mut app, &Key::Enter, &mut d);
        assert!(app.undo_armed.is_some());
        assert!(
            app.awaiting.is_empty(),
            "an Enter after the countdown restores nothing"
        );
    }

    #[test]
    fn moving_to_another_file_or_esc_stops_the_countdown() {
        let mut app = app_with(files());
        app.session = Some(1);
        let mut d = super::super::test_driver("undo-esc");
        key(&mut app, &Key::Enter, &mut d);
        key(&mut app, &Key::Down, &mut d);
        assert!(app.undo_armed.is_none());
        key(&mut app, &Key::Enter, &mut d);
        assert!(esc(&mut app));
        assert!(app.undo_armed.is_none());
        assert!(!esc(&mut app), "a second Esc closes the panel");
    }

    #[test]
    fn page_keys_move_five_rows_and_stop_at_the_ends() {
        let list: Vec<Value> = (0..12)
            .map(|i| json!({"path": format!("f{i}.rs"), "added": 1, "removed": 0}))
            .collect();
        let mut app = app_with(list);
        let mut d = super::super::test_driver("undo-page");
        key(&mut app, &Key::PageDown, &mut d);
        assert_eq!(app.panel_row, 5);
        key(&mut app, &Key::PageDown, &mut d);
        key(&mut app, &Key::PageDown, &mut d);
        assert_eq!(app.panel_row, 11);
        key(&mut app, &Key::PageUp, &mut d);
        assert_eq!(app.panel_row, 6);
        key(&mut app, &Key::Home, &mut d);
        assert_eq!(app.panel_row, 0);
    }

    /// The owner's example: changes at 40–46, 90–123 and 400–492 show 37…49,
    /// 87…126 and 397…495, the old and new numbers side by side, a rule
    /// between the hunks.
    #[test]
    fn the_diff_numbers_both_sides_with_three_lines_around_each_change() {
        let before: String = (1..=600).map(|n| format!("line {n}\n")).collect();
        let after: String = (1..=600)
            .map(|n| match n {
                40..=46 | 90..=123 | 400..=492 => format!("changed {n}\n"),
                _ => format!("line {n}\n"),
            })
            .collect();
        let hunks = serde_json::to_value(runeengine::checkpoint::diff::hunks(
            &before, &after, 1_000_000, 3,
        ))
        .unwrap();
        let rows = hunk_rows(hunks.as_array().unwrap(), 100);
        let shown: Vec<String> = rows.iter().map(|r| ansi::plain(r)).collect();
        assert!(
            shown[0].starts_with(" 37  37 │   line 37"),
            "{:?}",
            &shown[..3]
        );
        let rules: Vec<usize> = shown
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains('┼'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rules.len(), 2, "three hunks, two rules");
        assert!(
            shown[rules[0] - 1].starts_with(" 49  49 │"),
            "{}",
            shown[rules[0] - 1]
        );
        assert!(
            shown[rules[0] + 1].starts_with(" 87  87 │"),
            "{}",
            shown[rules[0] + 1]
        );
        assert!(
            shown[rules[1] - 1].starts_with("126 126 │"),
            "{}",
            shown[rules[1] - 1]
        );
        let deleted = shown.iter().find(|l| l.contains("− line 40")).unwrap();
        assert!(
            deleted.starts_with(" 40     │"),
            "only the old number: {deleted}"
        );
        let added = shown.iter().find(|l| l.contains("+ changed 40")).unwrap();
        assert!(
            added.starts_with("     40 │"),
            "only the new number: {added}"
        );
        assert!(
            shown.last().unwrap().starts_with("495 495 │"),
            "{}",
            shown.last().unwrap()
        );
    }

    #[test]
    fn alt_s_asks_for_the_chosen_files_diff_and_a_stale_answer_is_dropped() {
        let mut app = app_with(files());
        app.session = Some(1);
        let mut d = super::super::test_driver("undo-diff");
        key(&mut app, &Key::Alt('s'), &mut d);
        assert!(app
            .awaiting
            .values()
            .any(|w| matches!(w, Awaiting::UndoDiff { file, .. } if file == "Cli/src/ui/draw.rs")));
        key(&mut app, &Key::Down, &mut d);
        diffed(
            &mut app,
            Side::Undo,
            "Cli/src/ui/draw.rs".into(),
            json!({"diff": {"change": "modified", "hunks": []}}),
        );
        assert!(app.undo_diff.is_none(), "the cursor moved on");
        key(&mut app, &Key::Up, &mut d);
        diffed(
            &mut app,
            Side::Undo,
            "Cli/src/ui/draw.rs".into(),
            json!({"diff": {"change": "modified", "hunks": []}}),
        );
        assert!(app.undo_diff.is_some());
        assert!(text(&document(&app, 100)).contains("alt+s back to the list"));
        assert!(esc(&mut app));
        assert!(app.undo_diff.is_none());
    }

    #[test]
    fn a_click_on_the_button_is_an_enter_and_on_the_row_a_choice() {
        let mut app = app_with(files());
        app.session = Some(1);
        let mut d = super::super::test_driver("undo-click");
        let doc = document(&app, 100);
        let (row, button) = doc
            .sink
            .owner
            .iter()
            .enumerate()
            .find_map(|(n, hit)| match hit {
                Hit::UndoFile { index: 1, button } => Some((n, *button)),
                _ => None,
            })
            .unwrap();
        let drawn = ansi::plain(&doc.sink.lines[row]);
        assert_eq!(
            ansi::width(&drawn[..drawn.find("[undo]").unwrap()]),
            button.0,
            "the hit names the columns the button is drawn at"
        );
        click(&mut app, 1, button, 2, &mut d);
        assert_eq!(app.panel_row, 1);
        assert!(app.undo_armed.is_none());
        click(&mut app, 1, button, button.0 as u16 + 1, &mut d);
        assert_eq!(app.undo_armed.as_ref().unwrap().file, "notes.md");
    }

    /// A restore the engine refused — the session was working — says so in
    /// the panel, where it stays (the status line is the turn's, and its next
    /// word replaced it within a second), until the choice changes.
    #[test]
    fn a_refused_restore_says_why_in_the_panel_until_the_choice_changes() {
        let mut app = app_with(files());
        app.session = Some(1);
        let mut d = super::super::test_driver("undo-refused");
        key(&mut app, &Key::Enter, &mut d);
        key(&mut app, &Key::Enter, &mut d);
        let id = *app
            .awaiting
            .keys()
            .max()
            .expect("the restore was asked for");
        app.busy = true;
        app.on_ack(
            id,
            false,
            Some("invalid: cannot restore while a turn is running".into()),
            None,
        );
        let shown = text(&document(&app, 120));
        assert!(
            shown.contains("Not done: the session is working"),
            "{shown}"
        );
        app.clear_status();
        assert!(text(&document(&app, 120)).contains("Not done"));
        key(&mut app, &Key::Down, &mut d);
        assert!(!text(&document(&app, 120)).contains("Not done"));
    }

    /// Undone, the row stays — at `+0 −0`, from the engine's own answer — and
    /// the list is what is on screen again.
    #[test]
    fn an_undone_file_stays_listed_at_zero() {
        let mut app = app_with(files());
        let mut changed = files();
        changed[0] = json!({"path": "Cli/src/ui/draw.rs", "change": "unchanged", "added": 0, "removed": 0, "last_at": 1_700_000_000_000i64});
        restored(
            &mut app,
            Side::Undo,
            "Cli/src/ui/draw.rs",
            &json!({"restored": true, "file_changes": changed}),
        );
        assert_eq!(app.panel, Panel::Undo);
        let shown = text(&document(&app, 100));
        let row = shown.lines().find(|l| l.contains("draw.rs")).unwrap();
        assert!(row.contains("+0 −0"), "{row}");
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("back the way it was"));
    }

    /// `/redo` is `/undo` the other way: its own header, warning, button
    /// and countdown verb; nothing listed before an undo.
    #[test]
    fn the_redo_panel_reads_like_undo_the_other_way() {
        let mut app = App::for_test();
        app.panel = Panel::Redo;
        let empty = text(&document(&app, 100));
        assert!(
            empty.contains("REDO") && empty.contains("Nothing to redo: undo a file first"),
            "{empty}"
        );
        app.redo_files = vec![
            json!({"path": "notes.md", "change": "modified", "added": 1, "removed": 2, "last_at": 1_700_000_000_000i64}),
        ];
        let shown = text(&document(&app, 100));
        assert!(shown.contains("enter redo · alt+s show diff"), "{shown}");
        assert!(shown.contains("when you undid it"), "{shown}");
        let row = shown.lines().find(|l| l.contains("notes.md")).unwrap();
        assert!(
            row.contains("+1 −2") && row.contains("[redo]") && !row.contains("[undo]"),
            "{row}"
        );

        app.session = Some(1);
        let mut d = super::super::test_driver("redo-arm");
        key(&mut app, &Key::Enter, &mut d);
        assert_eq!(
            countdown(&app).as_deref(),
            Some("Press Enter within 3s to redo notes.md")
        );
        key(&mut app, &Key::Enter, &mut d);
        assert!(app.awaiting.values().any(
            |w| matches!(w, Awaiting::Restored { file, side: Side::Redo } if file == "notes.md")
        ));
        key(&mut app, &Key::Alt('s'), &mut d);
        assert!(app.awaiting.values().any(
            |w| matches!(w, Awaiting::UndoDiff { file, side: Side::Redo } if file == "notes.md")
        ));
    }

    /// Redone, the file leaves the redo list (used up) and the undo list
    /// takes the engine's fresh rows.
    #[test]
    fn a_redone_file_leaves_the_redo_list() {
        let mut app = App::for_test();
        app.panel = Panel::Redo;
        app.redo_files = vec![json!({"path": "notes.md", "added": 1, "removed": 0})];
        restored(
            &mut app,
            Side::Redo,
            "notes.md",
            &json!({"restored": true, "redos": [], "file_changes": [{"path": "notes.md", "added": 1, "removed": 0}]}),
        );
        assert!(app.redo_files.is_empty());
        assert_eq!(app.file_changes.len(), 1);
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .starts_with("Redone: notes.md"));
        assert!(text(&document(&app, 100)).contains("Nothing to redo"));
    }

    #[test]
    fn opening_redo_asks_for_the_redo_list() {
        let mut app = App::for_test();
        let mut d = super::super::test_driver("redo-open");
        open(&mut app, &mut d, Side::Redo);
        assert_eq!(app.panel, Panel::Redo);
        assert!(app
            .awaiting
            .values()
            .any(|w| matches!(w, Awaiting::UndoFiles { side: Side::Redo })));
        listed(&mut app, Side::Redo, &json!({"files": [{"path": "a.rs"}]}));
        assert_eq!(app.redo_files.len(), 1);
        assert!(
            app.file_changes.is_empty(),
            "the redo rows are not the side panel's"
        );
    }
}
