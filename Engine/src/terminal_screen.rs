//! Interpret command output in a private terminal, never in Rune's outer one.
//! AVT owns VT parsing and cursor semantics; this adapter drains old grid rows
//! into a bounded text transcript. Window/clipboard/title escapes never leave
//! the parser, and only an explicit frontend resize may change the geometry.
//!
//! # Rows for the screen, lines for the reader
//!
//! The grid is as wide as the box the frontend draws, so a long line is
//! carried on over several rows. [`Screen::lines`] and [`Screen::head_lines`]
//! are those rows — what the live box shows. [`Screen::text`] is the output as
//! the program wrote it: a row the terminal carried on by itself (a soft wrap)
//! is joined to the next, so the model reads `…/Application/Studio/Cli`, never
//! `…/Application/Studi` and `o/Cli` on two lines, and what it reads does not
//! depend on how wide the person's window happens to be.
//!
//! # One terminal for the foreground and the background
//!
//! A background command's log is read through the same grid ([`Screen::drain`]):
//! a row is final once it has scrolled off the top, as in any terminal's
//! scrollback, and the rows still on the grid are the part a program may yet
//! redraw ([`Shown`]). A line-by-line reading that knew only `\r` glued every
//! frame of a status block redrawn by cursor moves (Gradle's) into one line.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use avt::{
    parser::{Function, Parser},
    terminal::{BufferType, Terminal},
};

/// One row drained from the grid.
#[derive(Debug)]
struct Row {
    /// Its text. A soft-wrapped row keeps its trailing spaces: they are the
    /// program's own, in the middle of its line.
    text: String,
    /// The terminal carried this row on to the next one by itself.
    wrapped: bool,
}

impl Row {
    fn of(line: &avt::Line) -> Row {
        let raw = line.text();
        let wrapped = soft_wrapped(line, &raw);
        Row {
            text: if wrapped {
                raw
            } else {
                raw.trim_end_matches(' ').to_string()
            },
            wrapped,
        }
    }

    /// The row as the screen shows it.
    fn shown(&self) -> String {
        self.text.trim_end_matches(' ').to_string()
    }
}

/// Whether the terminal carried this row on to the next one by itself (a soft
/// wrap) rather than the program ending it with a newline.
///
/// avt keeps that fact private (`Line::wrapped`; avt =0.18.0, the latest
/// release, has no accessor); its `Debug` writes a wrapped row with a trailing
/// `⏎`, and that is the one place it can be read. Exact, not guessed from the
/// width — a program's own line exactly as wide as the screen is not a wrap,
/// nor one that itself ends in `⏎`: the row's text is known, so the two
/// renderings it can have are compared whole. Pinned by
/// `soft_wraps_are_read_from_the_grid_not_guessed_from_width`.
fn soft_wrapped(line: &avt::Line, text: &str) -> bool {
    format!("{line:?}") == format!("{:?}", format!("{text}\u{23ce}"))
}

/// A line written to a log is cut into pieces of this size: a program that
/// never ends its line must not grow it in memory without bound.
const LINE_CAP: usize = 64 * 1024;

/// What a grid holds that no log has yet: what a background task shows after
/// its log, and what one build hands the next across an upgrade's `exec` so
/// the screen goes on exactly where it was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Shown {
    /// The start of a line whose first rows have left the grid while the rest
    /// of it is still on it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub carry: String,
    /// The grid's rows, top to bottom, without the blank rows at its foot:
    /// each its text and whether the terminal carried it on to the next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<(String, bool)>,
    /// Where the cursor stands: row, column.
    #[serde(default)]
    pub cursor: (u16, u16),
}

impl Shown {
    /// The rows as the lines the program wrote, soft wraps joined.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut open = self.carry.clone();
        let mut carried = !open.is_empty();
        for (text, wrapped) in &self.rows {
            open.push_str(text);
            carried = *wrapped;
            if !wrapped {
                lines.push(std::mem::take(&mut open).trim_end_matches(' ').to_string());
            }
        }
        if carried && !open.is_empty() {
            lines.push(open.trim_end_matches(' ').to_string());
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// Decode one read of terminal output. A character cut in half by the read
/// boundary waits in `carry` for its other half rather than becoming two
/// replacement characters (audit H2).
pub fn decode_utf8(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    let complete = match std::str::from_utf8(carry) {
        Ok(_) => carry.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => carry.len(),
    };
    let text = String::from_utf8_lossy(&carry[..complete]).into_owned();
    carry.drain(..complete);
    text
}

#[derive(Debug)]
pub struct Screen {
    parser: Parser,
    terminal: Terminal,
    history: VecDeque<Row>,
    history_bytes: usize,
    max_bytes: usize,
    prefix: Vec<String>,
    prefix_limit: usize,
    pub occupied_rows_peak: usize,
    pub trimmed_rows: usize,
    /// The drained rows of a line [`Screen::drain`] has not ended yet.
    carry: String,
}

impl Screen {
    pub fn new(rows: u16, cols: u16, max_bytes: usize) -> Self {
        Self::with_prefix(rows, cols, max_bytes, 0)
    }

    /// Retain the first drained rows separately from bounded tail scrollback.
    /// Only the frontend preview asks for this; the model's text is unchanged.
    pub fn with_prefix(rows: u16, cols: u16, max_bytes: usize, prefix_limit: usize) -> Self {
        let (rows, cols) = geometry(rows, cols);
        Self {
            parser: Parser::new(),
            terminal: Terminal::new((usize::from(cols), usize::from(rows)), Some(0)),
            history: VecDeque::new(),
            history_bytes: 0,
            max_bytes,
            prefix: Vec::new(),
            prefix_limit,
            occupied_rows_peak: 0,
            trimmed_rows: 0,
            carry: String::new(),
        }
    }

    /// A screen for a background log: rows leave it through [`Screen::drain`]
    /// only, so nothing is ever trimmed unread.
    pub fn log(rows: u16, cols: u16) -> Self {
        Self::new(rows, cols, usize::MAX)
    }

    /// A log's screen going on from one handed over as [`Screen::shown`]: each
    /// line written back at the row it stood on (a wrapped line whole, so the
    /// terminal wraps it again), the cursor put back where it was.
    pub fn resume(rows: u16, cols: u16, shown: &Shown) -> Self {
        let mut screen = Self::log(rows, cols);
        screen.carry = shown.carry.clone();
        let mut line: Option<(usize, String)> = None;
        for (at, (text, wrapped)) in shown.rows.iter().enumerate() {
            let (_, open) = line.get_or_insert_with(|| (at, String::new()));
            open.push_str(text);
            if !wrapped {
                if let Some((start, text)) = line.take() {
                    screen.feed(&format!("\x1b[{};1H{text}", start + 1));
                }
            }
        }
        if let Some((start, text)) = line {
            screen.feed(&format!("\x1b[{};1H{text}", start + 1));
        }
        let (row, col) = shown.cursor;
        screen.feed(&format!("\x1b[{};{}H", row + 1, col + 1));
        screen
    }

    /// The lines whose rows have left the grid since the last call, each with
    /// its newline: final, as a terminal's scrollback is. A line whose end is
    /// still on the grid waits for it (up to [`LINE_CAP`]).
    pub fn drain(&mut self) -> String {
        let mut out = String::new();
        while let Some(row) = self.history.pop_front() {
            self.history_bytes = self.history_bytes.saturating_sub(row.text.len() + 1);
            self.carry.push_str(&row.text);
            if !row.wrapped {
                out.push_str(std::mem::take(&mut self.carry).trim_end_matches(' '));
                out.push('\n');
            } else if self.carry.len() >= LINE_CAP {
                out.push_str(&std::mem::take(&mut self.carry));
                out.push('\n');
            }
        }
        out
    }

    /// What the grid holds now and no log has yet.
    pub fn shown(&self) -> Shown {
        let mut rows: Vec<(String, bool)> = self
            .terminal
            .view()
            .map(|line| {
                let row = Row::of(line);
                (row.text, row.wrapped)
            })
            .collect();
        while rows
            .last()
            .is_some_and(|(text, wrapped)| text.is_empty() && !wrapped)
        {
            rows.pop();
        }
        let cursor = self.terminal.cursor();
        Shown {
            carry: self.carry.clone(),
            rows,
            cursor: (cursor.row as u16, cursor.col as u16),
        }
    }

    pub fn size(&self) -> (u16, u16) {
        let (cols, rows) = self.terminal.size();
        (rows as u16, cols as u16)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = geometry(rows, cols);
        if self.size() != (rows, cols) {
            self.terminal.resize(usize::from(cols), usize::from(rows));
            self.collect();
        }
    }

    pub fn feed(&mut self, raw: &str) {
        // Parsing a bounded character at a time also bounds transient scrollback
        // for a giant single chunk. Partial CSI/OSC sequences survive any split.
        for (index, character) in raw.chars().enumerate() {
            if let Some(operation) = self.parser.feed(character) {
                if matches!(operation, Function::Ris)
                    || (matches!(operation, Function::Ed(avt::parser::EdScope::SavedLines))
                        && self.terminal.active_buffer_type() == BufferType::Primary)
                {
                    self.history.clear();
                    self.history_bytes = 0;
                    self.trimmed_rows = 0;
                    self.prefix.clear();
                }
                let prints = matches!(operation, Function::Print(_));
                if !matches!(operation, Function::Xtwinops(_)) {
                    self.terminal.execute(operation);
                    if prints {
                        let rows = if self.terminal.active_buffer_type() == BufferType::Primary {
                            self.history.len().saturating_add(self.trimmed_rows)
                        } else {
                            0
                        };
                        self.occupied_rows_peak = self
                            .occupied_rows_peak
                            .max(rows.saturating_add(self.terminal.cursor().row + 1));
                    }
                }
            }
            if character == '\n' || index % 256 == 255 {
                self.collect();
            }
        }
        self.collect();
    }

    fn collect(&mut self) {
        for line in self.terminal.gc() {
            let row = Row::of(&line);
            if self.prefix.len() < self.prefix_limit {
                self.prefix.push(row.shown());
            }
            self.history_bytes = self.history_bytes.saturating_add(row.text.len() + 1);
            self.history.push_back(row);
        }
        while self.history_bytes > self.max_bytes {
            let Some(row) = self.history.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(row.text.len() + 1);
            self.trimmed_rows = self.trimmed_rows.saturating_add(1);
        }
    }

    /// Every row the output fills now, in order: the drained history (primary
    /// screen only), then the grid. Visited, not collected — this runs for
    /// every chunk a live command prints.
    fn each_row(&self, mut visit: impl FnMut(&Row)) {
        if self.terminal.active_buffer_type() == BufferType::Primary {
            self.history.iter().for_each(&mut visit);
        }
        for line in self.terminal.view() {
            visit(&Row::of(line));
        }
    }

    /// The rows as the screen shows them — the live box's own lines.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        self.each_row(|row| lines.push(row.shown()));
        while lines.last().is_some_and(|line: &String| line.is_empty()) {
            lines.pop();
        }
        lines
    }

    pub fn head_lines(&self, count: usize) -> (Vec<String>, usize) {
        let alternate = self.terminal.active_buffer_type() == BufferType::Alternate;
        let view: Vec<String> = self
            .terminal
            .view()
            .map(|line| line.text().trim_end_matches(' ').to_string())
            .collect();
        let view_rows = view
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |row| row + 1);
        let history_rows = if alternate {
            0
        } else {
            self.history.len() + self.trimmed_rows
        };
        let total = history_rows + view_rows;
        let mut head = Vec::new();
        if !alternate {
            if self.trimmed_rows > 0 {
                head.extend(self.prefix.iter().take(count).cloned());
                // A prefix request never crosses a gap in bounded scrollback.
                if self.prefix.len() >= self.trimmed_rows && head.len() < count {
                    head.extend(
                        self.history
                            .iter()
                            .skip(self.prefix.len() - self.trimmed_rows)
                            .take(count - head.len())
                            .map(Row::shown),
                    );
                }
            } else {
                head.extend(self.history.iter().take(count).map(Row::shown));
            }
        }
        if head.len() < count
            && (alternate || self.trimmed_rows == 0 || self.prefix.len() >= self.trimmed_rows)
        {
            head.extend(view.into_iter().take(view_rows).take(count - head.len()));
        }
        (head, total)
    }

    /// The output as the program wrote it: each soft-wrapped row joined to the
    /// one it was carried on to, trailing blank lines left out. What the model
    /// reads, and what a finished step shows.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut open = String::new();
        let mut carried = false;
        self.each_row(|row| {
            open.push_str(&row.text);
            carried = row.wrapped;
            if !row.wrapped {
                lines.push(std::mem::take(&mut open).trim_end_matches(' ').to_string());
            }
        });
        if carried {
            lines.push(open.trim_end_matches(' ').to_string());
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

// The same upper bound as the engine's tool.resize command. Output cannot
// allocate a screen of its own choosing (CSI 8 / XTWINOPS is ignored above).
fn geometry(rows: u16, cols: u16) -> (u16, u16) {
    (rows.clamp(1, 1000), cols.clamp(1, 1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_backspace_and_carriage_return_spinners_overwrite_the_same_cell() {
        let mut screen = Screen::new(10, 80, 10000);
        for frame in ["⠋", "\x08⠙", "\x08⠹", "\x08⠸"] {
            screen.feed(frame);
        }
        assert_eq!(screen.text(), "⠸");
        screen.feed("\r\x1b[2Kadded 27 packages\r\nfound 0 vulnerabilities\r\n");
        assert_eq!(screen.text(), "added 27 packages\nfound 0 vulnerabilities");
    }

    /// What the model read in session 546: `…/Application/Studi` and `o/Cli`
    /// on two lines, because the row was as wide as the box. The text is the
    /// program's line; the rows stay the screen's.
    #[test]
    fn soft_wraps_are_read_from_the_grid_not_guessed_from_width() {
        let path = "/home/noire/Project/Clode/Application/Studio/Cli";
        let mut screen = Screen::new(10, 20, 10000);
        screen.feed(&format!("   Compiling runecli v0.2.6 ({path})\r\n"));
        assert_eq!(
            screen.text(),
            format!("   Compiling runecli v0.2.6 ({path})"),
            "one line the program wrote is one line the model reads"
        );
        assert!(screen.lines().len() > 1, "the screen still shows rows");
        assert!(screen.lines().iter().all(|row| row.chars().count() <= 20));

        // A line exactly as wide as the screen, ended by the program, is not a
        // wrap: pytest's separators are that wide.
        let mut screen = Screen::new(10, 10, 10000);
        screen.feed("==========\r\nnext\r\n");
        assert_eq!(screen.text(), "==========\nnext");

        // The space a wrap fell on is the program's, in the middle of its line.
        let mut screen = Screen::new(10, 10, 10000);
        screen.feed("abcdefghi jklm\r\n");
        assert_eq!(screen.lines(), vec!["abcdefghi", "jklm"]);
        assert_eq!(screen.text(), "abcdefghi jklm");

        // A line that itself ends in the mark avt writes for a wrap is told
        // apart by counting: its own ⏎ is not a wrap, one more is.
        let mut screen = Screen::new(10, 10, 10000);
        screen.feed("abcdefghi\u{23ce}\r\nnext\r\n");
        assert_eq!(screen.text(), "abcdefghi\u{23ce}\nnext");
        let mut screen = Screen::new(10, 10, 10000);
        screen.feed("abcdefghi\u{23ce}jk\r\n");
        assert_eq!(screen.text(), "abcdefghi\u{23ce}jk");
        let mut screen = Screen::new(10, 10, 10000);
        screen.feed("say \"hi\" \u{23ce}\u{23ce}\r\n");
        assert_eq!(screen.text(), "say \"hi\" \u{23ce}\u{23ce}");
    }

    /// A wrapped line that has scrolled out of the grid into the drained
    /// history is still read as one line.
    #[test]
    fn a_wrapped_line_drained_into_history_is_still_one_line() {
        let mut screen = Screen::new(3, 10, 10000);
        screen.feed("AAAAAAAAAABBBBBBBBBBCC\r\n");
        for i in 0..6 {
            screen.feed(&format!("row-{i}\r\n"));
        }
        let text = screen.text();
        assert_eq!(
            text.lines().next(),
            Some("AAAAAAAAAABBBBBBBBBBCC"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 7, "{text}");
    }

    #[test]
    fn cursor_up_erase_and_absolute_position_rewrite_rows_without_appending() {
        let mut screen = Screen::new(10, 80, 10000);
        screen.feed(
            "download 10%\r\nbuild 10%\r\n\x1b[2A\r\x1b[2Kdownload 100%\r\n\x1b[2Kbuild 100%",
        );
        assert_eq!(screen.text(), "download 100%\nbuild 100%");
        screen.feed("\x1b[1;1H\x1b[2Kdone");
        assert_eq!(screen.text(), "done\nbuild 100%");
    }

    #[test]
    fn every_character_boundary_keeps_escape_state_and_produces_the_same_screen() {
        let raw = "prefix\r\n⠋\x08⠙\x1b[1G\x1b[2K日本 done\r\n\x1b]0;ignored title\x07last";
        let mut expected = Screen::new(10, 80, 10000);
        expected.feed(raw);
        for cut in (0..=raw.len()).filter(|cut| raw.is_char_boundary(*cut)) {
            let mut split = Screen::new(10, 80, 10000);
            split.feed(&raw[..cut]);
            split.feed(&raw[cut..]);
            assert_eq!(split.text(), expected.text(), "split at {cut}");
        }
    }

    #[test]
    fn command_control_sequences_cannot_escape_the_grid_or_resize_it() {
        let mut screen = Screen::new(10, 80, 10000);
        screen.feed("\x1b[8;65535;65535t\x1b]52;c;c2VjcmV0\x07\x1b]2;title\x1b\\ok\x1b[?1000h");
        assert_eq!(screen.size(), (10, 80));
        assert_eq!(screen.text(), "ok");
        assert!(!screen.text().chars().any(char::is_control));
    }

    #[test]
    fn scrollback_is_text_bounded_and_an_alternate_screen_does_not_duplicate_it() {
        let mut screen = Screen::new(4, 30, 1000);
        for i in 0..1000 {
            screen.feed(&format!("row-{i}\r\n"));
        }
        assert!(screen.history_bytes <= 1000);
        assert!(screen.trimmed_rows > 0);
        let primary = screen.text();
        screen.feed("\x1b[?1049h\x1b[Hstatus\r\n\x1b[1A\r\x1b[2Kfinished");
        assert_eq!(screen.text(), "finished");
        screen.feed("\x1b[?1049l");
        assert_eq!(screen.text(), primary);
    }

    #[test]
    fn reset_and_clear_scrollback_remove_drained_history_without_touching_another_screen() {
        let mut screen = Screen::new(3, 40, 10000);
        screen.feed("one\r\ntwo\r\nthree\r\nfour");
        assert!(screen.text().contains("one"));
        screen.feed("\x1b[?1049h\x1b[3Jalternate\x1b[?1049l");
        assert!(screen.text().contains("one"));
        screen.feed("\x1b[3J");
        assert!(!screen.text().contains("one"));
        screen.feed("\x1bcfresh");
        assert_eq!(screen.text(), "fresh");
    }

    #[test]
    fn a_frontend_prefix_stays_first_when_tail_scrollback_is_bounded() {
        let mut screen = Screen::with_prefix(3, 40, 30, 4);
        for i in 1..=100 {
            screen.feed(&format!("line-{i}\r\n"));
        }
        assert!(screen.trimmed_rows > 0);
        let (head, total) = screen.head_lines(4);
        assert_eq!(head, vec!["line-1", "line-2", "line-3", "line-4"]);
        assert_eq!(total, 100);
        let mut replay = Screen::with_prefix(3, 40, 30, 4);
        replay.feed(
            &(1..=100)
                .map(|i| format!("line-{i}\r\n"))
                .collect::<String>(),
        );
        assert_eq!(screen.head_lines(4), replay.head_lines(4));
        assert_eq!(screen.occupied_rows_peak, replay.occupied_rows_peak);
        screen.feed("\x1bcnew");
        assert_eq!(screen.head_lines(4), (vec!["new".into()], 1));
    }

    #[test]
    fn output_occupancy_starts_at_zero_and_grows_only_after_real_text() {
        let mut screen = Screen::with_prefix(10, 80, 1000, 4);
        screen.feed("\x1b[?25l\x1b[2K");
        assert_eq!(screen.occupied_rows_peak, 0);
        screen.feed("one");
        assert_eq!(screen.occupied_rows_peak, 1);
        screen.feed("\r\x1b[2K");
        assert_eq!(
            screen.occupied_rows_peak, 1,
            "empty erase must not recreate/remove output area"
        );
        screen.feed("one\r\ntwo");
        assert_eq!(screen.occupied_rows_peak, 2);
    }

    /// Gradle's rich console: a status block at the foot of the screen, redrawn
    /// in place by cursor moves, with each finished line printed above it.
    fn gradle_status(percent: u32) -> String {
        format!("\x1b[2K<{percent}% EXECUTING>\r\n\x1b[2K> :App:task\r\x1b[1A")
    }

    /// Session 570: a background Gradle log was `<…> 0% INITIALIZING [88ms]<…>
    /// 0% INITIALIZING [188ms]…` on one line, every frame of the status block
    /// glued to the last, because the log knew `\r` and no cursor move.
    #[test]
    fn a_status_block_redrawn_by_cursor_moves_never_reaches_the_log() {
        let mut screen = Screen::log(4, 40);
        for percent in 0..20 {
            screen.feed(&gradle_status(percent));
        }
        assert_eq!(screen.drain(), "", "nothing has left the screen yet");
        assert_eq!(screen.shown().text(), "<19% EXECUTING>\n> :App:task");
        for n in 0..6 {
            screen.feed(&format!("\x1b[2K> Task :t{n}\r\n"));
            screen.feed(&gradle_status(20 + n));
        }
        assert_eq!(
            screen.drain(),
            "> Task :t0\n> Task :t1\n> Task :t2\n> Task :t3\n",
            "only the finished lines, each once, in order"
        );
        assert_eq!(screen.drain(), "", "a drained line is never drained again");
        assert_eq!(
            screen.shown().text(),
            "> Task :t4\n> Task :t5\n<25% EXECUTING>\n> :App:task"
        );
    }

    #[test]
    fn a_log_keeps_the_last_state_of_a_redrawn_line_and_no_escape() {
        let mut screen = Screen::log(2, 40);
        screen.feed("50%\r100%\r\n\x1b[31mred\x1b[0m\x07\0 ok\r\n\x1b]0;title\x07a\r\nb\r\n");
        assert_eq!(screen.drain(), "100%\nred ok\na\n");
        assert_eq!(screen.shown().text(), "b");
    }

    #[test]
    fn a_line_that_never_ends_is_logged_in_pieces() {
        let mut screen = Screen::log(2, 100);
        screen.feed(&"x".repeat(LINE_CAP + 1000));
        let log = screen.drain();
        assert!(!log.is_empty());
        assert!(log.lines().all(|piece| piece.len() <= LINE_CAP + 100));
        assert!(screen.carry.len() < LINE_CAP);
    }

    /// A background task handed across an upgrade's `exec`: the screen the
    /// successor resumes is the one the predecessor had, so what follows lands
    /// where it would have — the same log, the same rows, line for line.
    #[test]
    fn a_resumed_log_screen_goes_on_exactly_where_it_was() {
        let first = format!(
            "> Task :a\r\n{}{}a line too long for the twenty columns\r\n{}",
            gradle_status(1),
            "\x1b[2K",
            gradle_status(2)
        );
        let second = format!(
            "\x1b[2K> Task :b\r\n{}\x1b[2K> Task :c\r\n{}",
            gradle_status(3),
            gradle_status(4)
        );
        let mut whole = Screen::log(4, 20);
        whole.feed(&first);
        whole.feed(&second);
        let whole_log = whole.drain();

        let mut before = Screen::log(4, 20);
        before.feed(&first);
        let mut log = before.drain();
        let handed: Shown =
            serde_json::from_str(&serde_json::to_string(&before.shown()).unwrap()).unwrap();
        let mut after = Screen::resume(4, 20, &handed);
        assert_eq!(after.shown(), before.shown());
        after.feed(&second);
        log.push_str(&after.drain());
        assert_eq!(log, whole_log);
        assert_eq!(after.shown(), whole.shown());
    }

    #[test]
    fn a_character_split_across_reads_arrives_whole() {
        let bytes = "café ☕".as_bytes();
        let mut carry = Vec::new();
        let mut text = String::new();
        for piece in bytes.chunks(1) {
            text.push_str(&decode_utf8(&mut carry, piece));
        }
        assert_eq!(text, "café ☕");
        assert!(carry.is_empty());
    }

    #[test]
    fn explicit_resize_keeps_parser_state_and_cursor_updates_work_afterwards() {
        let mut screen = Screen::new(4, 20, 1000);
        screen.feed("before");
        screen.resize(5, 40);
        screen.feed("\r\x1b[2Kafter");
        assert_eq!(screen.size(), (5, 40));
        assert_eq!(screen.text(), "after");
    }
}
