//! Turning the state into a screen, and the screen into as few bytes as
//! possible.
//!
//! # Why the frame is built whole and written in part
//!
//! Composing the entire frame every time is what makes the layout simple: no
//! incremental bookkeeping, no stale corners, no "which region do I invalidate"
//! questions. Writing it whole is what makes it flicker. So the frame is built
//! as lines, compared with the last one, and only the rows that changed are
//! sent — a streaming answer touches two or three rows a frame, and the rest of
//! the screen stays untouched at the wire level.
//!
//! Every line is padded to the terminal's width, which is what keeps a shorter
//! line from leaving the tail of the old one behind it.
//!
//! # The shape of a frame
//!
//! ```text
//!   RuneCLI                       ┆ context ▉▉▁▁▁▁  content
//!                           0.1.0 ┆ out · cache · spend
//!   ─────────────────────────────┆
//!   ─── you ─────────────────────┆   a rule, a question, a rule
//!   what the user asked          ┆
//!   ─────────────────────────────┆
//!   the answer, as markdown      ┆
//!   ─────────────────────────────────────────────────  footer
//!   › what is being typed
//!   mimo-v2.5-pro thinking · Ask                 0.1.0
//!   ─────────────────────────────────────────────────
//!   ~/Project/Clode │ asks before anything that runs
//! ```
//!
//! Five of those placements are answers to specific complaints and are worth
//! stating so they are not undone by accident:
//!
//!   * **the separation is a rule across the whole width**, not a colour and not
//!     a filled card. Hue alone disappears under a translucent terminal, and a
//!     filled card reads as a black box sitting on the wallpaper.
//!   * **the model and the mode sit against the composer**, because they are
//!     facts about the question that is being typed, not about the program.
//!   * **the last line answers one question** — is the engine working? — with
//!     the project path when it is not and with [`super::pulse`] when it is.
//!   * **the version is the last thing in the bottom-right corner**, under the
//!     composer. It is a footnote, and it had been standing under the wordmark
//!     where it was the second-largest thing on an empty screen.
//!   * **there is no shortcut row.** A screen that lists its own keys at all
//!     times is a screen with a permanent advertisement on it; `/help` has them.

use std::io::Write;

use super::ansi::{
    self, ACCENT, ADDED, BOLD, FAINT, MUTED, OK, REMOVED, RESET, RULE, TEXT, THINK, USER, WARN,
};
use super::blocks::{Block, Tool, ToolState};
use super::{markdown, panels, pulse, steps, Anchor, App, Panel, Place, PlanAnchor};

/// What [`Screen::paint`] may send now.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Nothing: the terminal is still reading, or having its quiet moment.
    Wait,
    /// A status request alone, answered once the terminal is done drawing.
    Ask,
    /// The frame.
    Frame,
}

#[derive(Default)]
pub struct Screen {
    prev: Vec<String>,
    /// The terminal said it shows a frame between the synchronized-update
    /// marks whole (DEC 2026). Only then may a page move scroll the rows the
    /// side panel shares: elsewhere the terminal can draw the moment between
    /// the scroll and the panel put back, and the panel is seen to jump.
    pub synchronized: bool,
    /// Frames sent with a status request after them whose answer has not come
    /// — when each was sent and how many bytes it was, oldest first: the
    /// terminal is still reading them.
    unanswered: std::collections::VecDeque<(std::time::Instant, usize)>,
    /// A status request went unanswered: this terminal does not answer, and
    /// frames are sent without asking.
    silent: bool,
    /// How long the terminal takes to take in a frame of real size, smoothed;
    /// `None` until one has been answered.
    pace: Option<std::time::Duration>,
    /// When the terminal last said it had read a frame.
    answered: Option<std::time::Instant>,
    /// The last frame went to a terminal that draws on its own clock, and it
    /// has not yet been asked, after its quiet moment, whether it is done
    /// drawing it — a status request it can only read once it is. See
    /// [`Screen::QUIET`].
    drawing: bool,
}

impl Screen {
    /// How long a status answer may take before the terminal is taken not to
    /// give them. Far above any terminal measured (Konsole 54 ms p50 a frame,
    /// GNOME Terminal 17, kitty 6), so only silence trips it.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);
    /// A frame below this size measures the terminal's latency, not its speed
    /// (a spinner's tick against a page of text), so it does not move `pace`.
    const SIZEABLE: usize = 2048;
    /// The longest a page frame may take for an animation to read as motion
    /// (30 frames a second); a terminal slower than that shows a glide as lag.
    const GLIDE_FRAME: std::time::Duration = std::time::Duration::from_millis(33);
    /// Frames that may be on their way to the terminal at once. One left the
    /// terminal idle between a frame and the next — the answer's way back and
    /// the next frame's composing — and a slow terminal drew 11 a second where
    /// it could read 20 (Konsole, the side panel open, owner 2026-10-08). Two
    /// keep it reading, and still bound how far the screen can lag behind the
    /// hand on the wheel to the frame being read.
    const AHEAD: usize = 2;
    /// How long a terminal that draws on its own clock — not between the
    /// synchronized-update marks — is left with nothing new after a frame,
    /// before the next is sent.
    ///
    /// # Why (measured, owner 2026-10-09)
    ///
    /// Konsole 25.04.2 does not answer DEC 2026 and draws on two timers
    /// (`Emulation::bufferedUpdate`): 10 ms after the last data, or 40 ms after
    /// a stream of data began, whichever comes first. A page frame beside the
    /// side panel is ~6 KB, read in two pieces of 4095 bytes. With frames sent
    /// back to back (two on their way, [`Screen::AHEAD`]) the data never paused
    /// for 10 ms, the 40 ms timer drew whatever had been read — and 14 of the 15
    /// torn pictures in a real Konsole under gdb were exactly that: the first
    /// 4095 bytes of a frame drawn, the rest not (the tearing the owner saw on a
    /// fast wheel). A pause longer than the 10 ms timer after every frame lets
    /// the terminal start drawing it whole before the next begins.
    ///
    /// # And then it must be let finish
    ///
    /// The pause alone was measured too (6 runs): the tearing went on, from
    /// another cause. Konsole drew a 169×50 page for 35–80 ms, its event loop
    /// shut meanwhile; the next frame arrived during the drawing, and when the
    /// loop opened it read the first 4095 bytes before its overdue timer drew
    /// again. So after the pause one more status request is sent alone
    /// ([`Screen::drawing`]): a terminal reads it only once it is done drawing,
    /// and the next frame waits for its answer.
    const QUIET: std::time::Duration = std::time::Duration::from_millis(12);

    /// Send a frame, unless the terminal is still reading the frames before
    /// it.
    ///
    /// A terminal reads and draws at its own pace — Konsole at about 180 KB/s,
    /// some forty times slower than kitty — and frames sent faster than that
    /// queue up in front of it: the screen then shows the past, later and
    /// later, after the hand on the wheel has stopped. So each frame ends with
    /// a status request (`ESC [ 5 n`), which a terminal answers only once it
    /// has read everything before it, and no more than [`Screen::AHEAD`] frames
    /// are on their way unanswered ([`Screen::heard`]). What changed meanwhile
    /// is not lost: the frame sent then is composed from the state as it is
    /// then. Returns whether the frame was sent; one that was not is owed.
    pub fn paint(&mut self, app: &mut App, rows: u16, cols: u16, force: bool) -> bool {
        match self.next_at(std::time::Instant::now(), force) {
            Next::Wait => return false,
            Next::Ask => {
                // The terminal has had its quiet moment after the last frame
                // and is drawing it: this request is read once it is done.
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(b"\x1b[5n");
                let _ = stdout.flush();
                return false;
            }
            Next::Frame => {}
        }
        let mut out = self.render(app, rows, cols, force);
        if !self.silent {
            out.push_str("\x1b[5n");
            self.unanswered
                .push_back((std::time::Instant::now(), out.len()));
            self.drawing = !self.synchronized;
        }
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        true
    }

    /// Ask the terminal what it can do; the answer arrives as a key
    /// ([`super::input::Report`]). A terminal that does not know the question
    /// says nothing, which leaves the careful default.
    pub fn ask(&self) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x1b[?2026$p");
        let _ = stdout.flush();
    }

    /// The terminal's answer to a question the painter asked.
    pub fn heard(&mut self, report: super::input::Report) {
        self.heard_at(report, std::time::Instant::now());
    }

    fn heard_at(&mut self, report: super::input::Report, now: std::time::Instant) {
        match report {
            super::input::Report::Ready => {
                if let Some((sent, bytes)) = self.unanswered.pop_front() {
                    if bytes > 0 {
                        self.answered = Some(now);
                    }
                    if bytes >= Self::SIZEABLE {
                        let took = now.saturating_duration_since(sent);
                        // Half the last reading, half the history: a terminal
                        // that slows down (a bigger window, a busy machine) is
                        // followed within a frame or two.
                        self.pace = Some(self.pace.map_or(took, |pace| (pace + took) / 2));
                    }
                }
            }
            super::input::Report::Synchronized(honoured) => self.synchronized = honoured,
        }
    }

    /// Whether a glide will read as motion here: the terminal shows each
    /// frame whole (DEC 2026) and takes a page frame within a glide's frame.
    ///
    /// The answer to a status request comes when the terminal has *read* a
    /// frame, not when it has drawn it, so the pace alone was not enough:
    /// Konsole answered a real session's 7 KB frames quickly, kept the glide,
    /// and drew it as 445 ms of stutter after the hand stopped (with the glide
    /// off: 2–4 ms, and a quarter of the bytes). The terminals that draw every
    /// frame whole are the ones that draw a glide smoothly (kitty); the ones
    /// that do not say so are the ones it stuttered on (Konsole, GNOME).
    pub fn keeps_up(&self) -> bool {
        self.synchronized && self.pace.is_none_or(|pace| pace <= Self::GLIDE_FRAME)
    }

    /// What may go to the terminal now. A request asked for is counted as on
    /// its way from here on.
    fn next_at(&mut self, now: std::time::Instant, force: bool) -> Next {
        if force {
            return Next::Frame;
        }
        if self.behind_at(now) {
            return Next::Wait;
        }
        if self.drawing {
            self.unanswered.push_back((now, 0));
            self.drawing = false;
            return Next::Ask;
        }
        Next::Frame
    }

    /// Whether the next frame must wait. A terminal that shows each frame whole
    /// may have [`Screen::AHEAD`] on their way; one that draws on its own clock
    /// gets one at a time, and [`Screen::QUIET`] with nothing new after it, so
    /// its clock never falls in the middle of a frame. An answer that waited
    /// past [`Screen::PATIENCE`] is given up, and so are all the others: this
    /// terminal does not answer.
    fn behind_at(&mut self, now: std::time::Instant) -> bool {
        match self.unanswered.front() {
            Some((at, _)) if now.saturating_duration_since(*at) >= Self::PATIENCE => {
                self.unanswered.clear();
                self.silent = true;
                false
            }
            _ if self.synchronized => self.unanswered.len() >= Self::AHEAD,
            _ => {
                !self.unanswered.is_empty()
                    || self
                        .answered
                        .is_some_and(|at| now.saturating_duration_since(at) < Self::QUIET)
            }
        }
    }

    /// How long, up to [`Screen::PATIENCE`], to wait for the status answers
    /// still on their way — said before the terminal is handed back, so they
    /// are read here and not typed into the shell as `^[[0n`.
    pub fn awaiting_answer(&self) -> Option<std::time::Duration> {
        self.unanswered
            .back()
            .map(|(at, _)| Self::PATIENCE.saturating_sub(at.elapsed()))
            .filter(|left| !left.is_zero())
    }

    /// The bytes this frame needs, whole. Kept apart from [`Screen::paint`],
    /// which only owns the writing, so the tests can read what would be sent.
    fn render(&mut self, app: &mut App, rows: u16, cols: u16, force: bool) -> String {
        let frame = compose(app, rows, cols);
        // A resync is the one answer to a screen the painter no longer trusts:
        // bytes lost on a slow link, a ghost left by an older build, a terminal
        // that reflowed behind the program's back. `ctrl+l` asks for every row
        // back wholesale.
        //
        // A scroll move MUST NOT clear screen wholesale: scrolling changes
        // content rows, which the differential painter below overwrites in-place.
        // Emitting CLEAR_SCREEN on scroll wipes the terminal to blank and causes
        // severe, continuous flickering.
        //
        // And a frame that merely got TALLER or SHORTER — a composer
        // wrapping onto a second line, a permission card opening, a sidebar
        // gaining a row — must not clear either. That was the second source of
        // the same flicker, and it fired on ordinary typing. A shorter frame
        // leaves rows below it that the frame no longer claims, so those rows
        // alone are erased; a taller one simply draws the rows it gained.
        let force = std::mem::take(&mut app.resync) || force;
        let mut out = String::new();
        // Synchronized update (DEC mode 2026) and hide cursor while painting to
        // prevent hardware cursor jumps and intermediate frame tearing.
        out.push_str("\x1b[?2026h\x1b[?25l");
        if force {
            out.push_str(ansi::CLEAR_SCREEN);
            self.prev = vec![String::new(); frame.len()];
        }
        self.rows(&frame, cols, &mut out);
        if let Some((r, c)) = app.cursor_terminal {
            // Steady bar cursor (\x1b[6 q), moved to the cell, visible (\x1b[?25h)
            out.push_str(&format!("\x1b[6 q\x1b[{r};{c}H\x1b[?25h"));
        } else {
            out.push_str("\x1b[?25l");
        }
        out.push_str(ansi::RESET);
        out.push_str("\x1b[?2026l");
        self.prev = frame;
        // The colour each row says again and again is said once (see `sgr`).
        super::sgr::minimal(&out)
    }
}

impl Screen {
    /// Bring the terminal from what `prev` says it shows to `frame`, appending
    /// the bytes to `out`. A page that moved by a few rows — the wheel's whole
    /// business — is first moved *by the terminal* ([`Shift`]); then the rows
    /// that still differ are written, and the tail a shorter frame abandoned is
    /// erased.
    fn rows(&mut self, frame: &[String], cols: u16, out: &mut String) {
        if let Some(shift) = Shift::between(&self.prev, frame, self.synchronized) {
            shift.apply(&mut self.prev, out);
        }
        let stale_below = self.prev.len().saturating_sub(frame.len());
        for (i, line) in frame.iter().enumerate() {
            let Some(old) = self.prev.get(i).filter(|p| *p != line) else {
                if self.prev.get(i).is_none() {
                    write_row(out, i, line, cols);
                }
                continue;
            };
            match (seam(old), seam(line)) {
                // A row beside the panel is two writes, each made only when
                // its side changed: the page moved under a panel that did not
                // costs the panel's few cells, not the whole row again.
                (Some(was), Some(at)) => {
                    // The page's side is padded to the seam, so both are one
                    // width: only the columns that changed are sent.
                    if old[..was] != line[..at] {
                        write_changed(out, i, &old[..was], &line[..at], None);
                    }
                    if old[was..] != line[at..] {
                        out.push_str(&ansi::at(i as u16 + 1, 1));
                        out.push_str(ansi::RESET);
                        out.push_str(&line[at..]);
                        if ansi::width(line) < cols as usize {
                            out.push_str("\x1b[K");
                        }
                    }
                }
                _ => write_changed(out, i, old, line, Some(cols)),
            }
        }
        // Erase the tail the shorter frame abandoned, row by row and from the
        // bottom up, so a closing permission card does not leave its last line
        // on the screen. `EL` on its own row is enough and touches nothing
        // above it — the whole point of not clearing the screen.
        for i in (frame.len()..frame.len() + stale_below).rev() {
            out.push_str(&ansi::at(i as u16 + 1, 1));
            out.push_str("\x1b[K");
        }
    }
}

/// A row that changed, from the first column that differs: the columns both
/// frames share at its start (and, when the row kept its width, at its end)
/// are the terminal's already. A wheel step rewrote every row whole — the
/// spine, the box borders and the padding to the side panel included — which
/// on a slow terminal is what a frame costs. `cols` is the screen's width for
/// a row that runs to the edge, whose old tail is erased as [`write_row`]
/// erases it; `None` for the page's side of a row beside the panel, padded to
/// the seam.
fn write_changed(out: &mut String, i: usize, old: &str, line: &str, cols: Option<u16>) {
    let (Some(before), Some(after)) = (ansi::cells(old), ansi::cells(line)) else {
        match cols {
            Some(cols) => write_row(out, i, line, cols),
            None => {
                out.push_str(&ansi::at(i as u16 + 1, 1));
                out.push_str(ansi::RESET);
                out.push_str(line);
            }
        }
        return;
    };
    let shared_start = before
        .cells
        .iter()
        .zip(&after.cells)
        .take_while(|(a, b)| a == b)
        .count();
    let width = after.width();
    let same_width = before.width() == width;
    // Equal cells end at equal columns only in rows of one width.
    let shared_end = if same_width {
        before.cells[shared_start..]
            .iter()
            .rev()
            .zip(after.cells[shared_start..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count()
    } else {
        0
    };
    let changed = &after.cells[shared_start..after.cells.len() - shared_end];
    // The tail past the row: erased when the row got shorter, and when what an
    // erase paints with changed — what writing the row whole did.
    let erase =
        cols.is_some_and(|cols| width < cols as usize) && (!same_width || before.end != after.end);
    if !changed.is_empty() {
        let column: usize = after.cells[..shared_start].iter().map(|c| c.width).sum();
        out.push_str(&ansi::at(i as u16 + 1, column as u16 + 1));
        out.push_str(&ansi::render_cells(changed));
    }
    if erase {
        if changed.is_empty() || shared_end > 0 {
            out.push_str(&ansi::at(i as u16 + 1, width as u16 + 1));
        }
        // From a reset: a row written before may have left its colour on.
        out.push_str(ansi::RESET);
        out.push_str(&after.end);
        out.push_str("\x1b[K");
    }
}

/// One whole row, from its first column.
fn write_row(out: &mut String, i: usize, line: &str, cols: u16) {
    out.push_str(&ansi::at(i as u16 + 1, 1));
    out.push_str(ansi::RESET);
    out.push_str(line);
    // The trailing clear is what keeps a shorter line from leaving the tail of
    // the old one behind it. But a line as wide as the terminal leaves the
    // cursor standing **on its own last cell**, and `EL` counts that cell —
    // erasing the very wall or corner the line just drew. Only a line with a
    // tail after it may be followed by the clear.
    if ansi::width(line) < cols as usize {
        out.push_str("\x1b[K");
    }
}

/// Where a row beside the side panel turns into the panel: the byte at which
/// [`beside`]'s column move begins. Nothing else in a frame moves the cursor —
/// [`ansi::tame`] lets only colour and links through — so the first `CSI n G`
/// of a row is that seam, and a row without one is a row of one piece.
fn seam(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(found) = line[from..].find("\x1b[") {
        let start = from + found;
        let digits = bytes[start + 2..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits > 0 && bytes.get(start + 2 + digits) == Some(&b'G') {
            return Some(start);
        }
        from = start + 2;
    }
    None
}

/// The bytes [`Screen::rows`] sends to turn row `was` into row `now`: nothing
/// when they agree, each side of the panel's seam that differs, or the whole
/// row. Kept beside the writer it prices, so a move is weighed by the very
/// writes it would cause.
fn repaint_cost(was: &str, now: &str) -> usize {
    /// Cursor address, reset and erase around a write.
    const ADDRESS: usize = 16;
    if was == now {
        return 0;
    }
    match (seam(was), seam(now)) {
        (Some(a), Some(b)) => {
            let page = if was[..a] != now[..b] { b + ADDRESS } else { 0 };
            let panel = if was[a..] != now[b..] {
                now.len() - b + ADDRESS
            } else {
                0
            };
            page + panel
        }
        _ => now.len() + ADDRESS,
    }
}

/// The part of a row a page move carries: left of the seam beside the panel,
/// the whole row otherwise.
fn page_part(line: &str) -> &str {
    seam(line).map_or(line, |at| &line[..at])
}

/// A band of rows the new frame shows exactly as the old one did, only `by`
/// rows higher (`up`) or lower: the terminal can move it with one scroll command
/// instead of being sent every row of it again.
///
/// Found by comparing what each row shows of the page — left of the seam
/// beside the side panel ([`page_part`]). The panel does not move with the
/// page, so comparing whole rows found no band at all while it was open and
/// every row was sent again on every step of the wheel. The terminal's scroll
/// moves the panel's cells too; the row diff that follows compares against
/// what the terminal now shows (`apply` updates the old frame to that) and puts
/// back only the panel side of the rows that moved.
struct Shift {
    top: usize,
    /// One past the last row of the band.
    bottom: usize,
    by: usize,
    up: bool,
}

impl Shift {
    /// The furthest page move worth looking for; a wheel gesture's share is a
    /// few rows, and beyond this the whole page has changed anyway.
    const MOST: usize = 16;
    /// Fewer rows than this are cheaper to write than to move.
    const LEAST_ROWS: usize = 6;
    /// What a move must save, in bytes, over writing its rows again.
    const LEAST_BYTES: usize = 120;
    /// The margins, the scroll and the margins' release, in bytes.
    const COMMAND: usize = 24;

    /// `whole` says the terminal shows a frame whole (see
    /// [`Screen::synchronized`]); without it a move that would carry the side
    /// panel's cells along is not made.
    fn between(prev: &[String], frame: &[String], whole: bool) -> Option<Shift> {
        let n = prev.len().min(frame.len());
        if n < Self::LEAST_ROWS + 1 {
            return None;
        }
        if !whole
            && frame[..n]
                .iter()
                .chain(&prev[..n])
                .any(|l| seam(l).is_some())
        {
            return None;
        }
        let pages_prev: Vec<&str> = prev[..n].iter().map(|l| page_part(l)).collect();
        let pages: Vec<&str> = frame[..n].iter().map(|l| page_part(l)).collect();
        let mut best: Option<(usize, Shift)> = None;
        for by in 1..=Self::MOST.min(n - 1) {
            for up in [true, false] {
                // Row `i` of the new frame was row `from(i)` of the old one.
                let from = |i: usize| if up { i + by } else { i.wrapping_sub(by) };
                let mut first = None;
                let mut last = 0;
                let mut rows = 0;
                for i in 0..n {
                    let j = from(i);
                    if j < n && pages[i] == pages_prev[j] && pages[i] != pages_prev[i] {
                        first.get_or_insert(i);
                        last = i;
                        rows += 1;
                    }
                }
                let Some(first) = first else { continue };
                if rows < Self::LEAST_ROWS {
                    continue;
                }
                let (top, bottom) = if up {
                    (first, (last + by + 1).min(n))
                } else {
                    (first.saturating_sub(by), last + 1)
                };
                // What the rows of the band cost to put right afterwards, moved
                // and not: the panel beside a moved page is moved as well and
                // has to be put back, so a move pays only when the page it
                // spares outweighs the panel it disturbs.
                let mut saved = 0isize;
                for i in top..bottom {
                    let j = from(i);
                    let moved = if (top..bottom).contains(&j) {
                        prev[j].as_str()
                    } else {
                        ""
                    };
                    saved += repaint_cost(&prev[i], &frame[i]) as isize
                        - repaint_cost(moved, &frame[i]) as isize;
                }
                let saved = saved - Self::COMMAND as isize;
                if saved < Self::LEAST_BYTES as isize {
                    continue;
                }
                if best.as_ref().is_none_or(|(b, _)| saved as usize > *b) {
                    best = Some((
                        saved as usize,
                        Shift {
                            top,
                            bottom,
                            by,
                            up,
                        },
                    ));
                }
            }
        }
        best.map(|(_, shift)| shift)
    }

    /// Emit the scroll and make `prev` say what the terminal now shows: the
    /// band moved, the rows it uncovered blank.
    fn apply(&self, prev: &mut [String], out: &mut String) {
        // DECSTBM homes the cursor and the margins must not outlive the
        // command: every later write addresses the whole screen.
        out.push_str(&format!("\x1b[{};{}r", self.top + 1, self.bottom));
        out.push_str(&format!(
            "\x1b[{}{}",
            self.by,
            if self.up { 'S' } else { 'T' }
        ));
        out.push_str("\x1b[r");
        let band = &mut prev[self.top..self.bottom];
        if self.up {
            band.rotate_left(self.by.min(band.len()));
            let len = band.len();
            for row in &mut band[len.saturating_sub(self.by)..] {
                row.clear();
            }
        } else {
            band.rotate_right(self.by.min(band.len()));
            for row in band.iter_mut().take(self.by) {
                row.clear();
            }
        }
    }
}

/// A row of the split frame: the page's line, padded to its column, then the
/// divider and the panel's line, placed by column. The reset before the seam
/// is what makes the panel side a piece of its own ([`seam`]): drawn alone or
/// after the page's side, it starts from the same plain state, so neither side
/// can lend its colour to the divider.
fn beside(left: &str, left_w: usize, right: &str, right_w: usize) -> String {
    format!(
        "{}{RESET}\x1b[{}G{RULE}│{RESET} {}",
        ansi::pad(left, left_w),
        left_w + 1,
        ansi::truncate(right, right_w),
        RESET = ansi::RESET,
    )
}

fn compose(app: &mut App, rows: u16, cols: u16) -> Vec<String> {
    if let Some(picker) = app.session_picker.as_mut() {
        app.preferences.height = rows as usize;
        let lines = picker.compose(rows, cols);
        app.cursor_terminal = picker.caret;
        app.hits.clear();
        return finish_frame(app, lines);
    }
    let rows = rows.max(8) as usize;
    let cols = cols.max(20) as usize;

    if app.panel == Panel::Settings {
        app.preferences.width = cols;
        app.preferences.height = rows;
        app.questions_card = None;
        app.permission.card = None;
        app.sidebar_cols = None;
        app.sidebar_last_row = 0;
        let (sink, cursor) = super::settings::render_interactive(app, cols, rows);
        app.hits = sink.owner;
        app.cursor_terminal = cursor.map(|(r, c)| (r as u16, c as u16));
        return finish_frame(app, sink.lines);
    }

    if app.searching() {
        let total = super::keys::matches(&app.input).len();
        app.adjust_palette_scroll(total);
    }

    let (
        Sink {
            lines: footer,
            owner: footer_hits,
        },
        footer_cursor,
    ) = footer(app, rows, cols);
    let content_rows = rows.saturating_sub(footer.len()).max(1);
    app.permission.card = if app.pending.is_some() {
        Some(((content_rows + 1) as u16, (rows.saturating_sub(1)) as u16))
    } else {
        app.permission.reset();
        None
    };

    // The ask-user card fills the footer when it is up: published as one-based
    // screen rows for the wheel, which must ask "is the pointer over the
    // card?" against the geometry of the frame the pointer is looking at —
    // the same argument as `sidebar_cols` below.
    app.questions_card = if app.questions.is_some() {
        Some(((rows - footer.len() + 1) as u16, rows as u16))
    } else {
        None
    };

    if let Some((footer_row, cursor_col)) = footer_cursor {
        let term_row = (content_rows + 1 + footer_row) as u16;
        app.cursor_terminal = Some((term_row, cursor_col as u16));
    } else {
        app.cursor_terminal = None;
    }

    let split =
        app.sidebar && cols as u64 >= super::runtime_configuration().number("ui.split_min_columns");
    let (left_w, right_w) = if split {
        let left = (cols * 66 / 100).max(40);
        (left, cols.saturating_sub(left + 2))
    } else {
        (cols, 0)
    };

    // Nothing in the left column may touch the divider: a rule drawn to the
    // full width and the divider next to it read as one broken line.
    let text_w = if split {
        left_w.saturating_sub(1)
    } else {
        left_w
    };

    // The transcript is kept, not rebuilt. A reader who has scrolled up is
    // asking for history, so the whole of it is laid out at once (`whole` —
    // without it the anchor this frame must find can sit in a part of the
    // transcript that has not been laid out yet, and the view jumps to the
    // nearest laid row instead: the stutter scrolling felt like in long
    // sessions). A reader following the tail needs only the newest screenful,
    // and the rest arrives over the frames after. See [`Transcript`].
    let dirty = app.transcript_dirty;
    let whole = app.scroll > 0;
    let mut transcript = app.transcript_cache.take();
    if app.panel == Panel::None {
        transcript = Some(transcript_into(
            transcript.take(),
            app,
            text_w,
            dirty,
            whole,
        ));
        app.transcript_dirty = false;
    }

    let panel_sink = if app.panel != Panel::None {
        Some(panels::document(app, text_w))
    } else {
        None
    };

    let (left_lines, left_owner): (&[String], &[Hit]) = if let Some(p) = &panel_sink {
        (&p.sink.lines, &p.sink.owner)
    } else {
        let s = &transcript.as_ref().unwrap().sink;
        (&s.lines, &s.owner)
    };

    // Show the tail, unless the user has scrolled up. An overlay and the
    // conversation each keep their own place (`PanelView`): the one covered is
    // left exactly where its reader left it.
    let total = left_lines.len();
    let max_top = total.saturating_sub(content_rows);
    let top = if let Some(doc) = &panel_sink {
        app.panel_view.top(app.panel, doc, text_w, content_rows)
    } else {
        app.panel_view.shown = None;
        let tx = transcript.as_ref().unwrap();
        let top = chat_top(app, tx, max_top);
        app.scroll = max_top.saturating_sub(top);
        app.drawn_rows = total;
        app.drawn_viewport = content_rows;
        app.scroll_anchor = (app.scroll > 0)
            .then(|| anchor_at(tx, top, content_rows))
            .flatten();
        top
    };
    let visible: &[String] = if top < left_lines.len() {
        let end = (top + content_rows).min(left_lines.len());
        &left_lines[top..end]
    } else {
        &[]
    };
    // The map is cut with exactly the same knife. A hit map built from an
    // unscrolled layout is a map of a screen nobody is looking at.
    let mut hits: Vec<Hit> = left_owner
        .iter()
        .skip(top)
        .take(content_rows)
        .copied()
        .collect();
    hits.resize(content_rows, Hit::None);

    let mut lines: Vec<String> = Vec::with_capacity(rows);
    let right = if split {
        // The panel gets the content rows minus the heading it draws above the
        // plan, and it is told its height so it can decide what to hide and say
        // so. Measuring it here rather than letting `compose` slice the result
        // is the fix for the panel that cut items without a word: a cut made
        // downstream is a cut the thing being cut cannot know about.
        let panel_rows = content_rows.saturating_sub(1);
        let side = sidebar(app, right_w, panel_rows);
        // Published for the wheel: "is the pointer over the panel?" is a
        // geometry question, and this is the geometry the last frame used.
        app.sidebar_cols = Some((left_w as u16 + 2, cols as u16));
        app.sidebar_last_row = content_rows as u16;
        side
    } else {
        app.sidebar_cols = None;
        Vec::new()
    };

    // Each row's left column as drawn, for what is laid over it afterwards.
    let mut lefts: Vec<String> = Vec::with_capacity(content_rows);
    for i in 0..content_rows {
        // Cut here, once, for everything the content area can hold: transcript,
        // wordmark and panels alike. One funnel is the only way to be sure no
        // line can wrap and push the whole frame down a row.
        //
        // And tamed *before* it is measured. A line that came out of a pty
        // carries the pty's own `\r`, which occupies no column and throws the
        // cursor back to column one — so the divider drawn after it landed in
        // the middle of the text. See [`ansi::tame`].
        //
        // A line containing `\n` or `\r` MUST NOT reach the terminal as a
        // single frame row: an embedded newline causes the terminal's hardware
        // cursor to jump down a row, leaving the current row's right border and
        // split divider stranded on the line below.
        let raw_l = visible.get(i).map(|s| s.as_str()).unwrap_or("");
        let safe_l = if raw_l.contains('\n') || raw_l.contains('\r') {
            raw_l.replace(['\r', '\n'], " ")
        } else {
            raw_l.to_string()
        };
        let l = ansi::truncate(&ansi::tame(&safe_l), text_w);
        lefts.push(l.clone());
        let line = if split {
            let raw_r = right.get(i).map(|s| s.as_str()).unwrap_or("");
            let safe_r = if raw_r.contains('\n') || raw_r.contains('\r') {
                raw_r.replace(['\r', '\n'], " ")
            } else {
                raw_r.to_string()
            };
            beside(&l, left_w, &ansi::tame(&safe_r), right_w)
        } else {
            l
        };
        lines.push(line);
    }
    app.jump_hint = None;
    if let Some((label, first)) = jump_hint(app, text_w) {
        // Laid over the conversation's last row on screen, covering only its
        // own columns and one blank on each side (owner, 2026-10-09): the rest
        // of that row stays readable and clickable, and the side panel beside
        // it keeps its row.
        let at = content_rows - 1;
        let row = ansi::overlay(&lefts[at], first as usize - 1, &label);
        let right_row = right
            .get(at)
            .map(|r| ansi::tame(&r.replace(['\r', '\n'], " ")));
        lines[at] = match &right_row {
            Some(r) if split => beside(&row, left_w, r, right_w),
            _ if split => beside(&row, left_w, "", right_w),
            _ => row,
        };
        let width = ansi::width(JUMP_HINT) as u16;
        let screen_row = at as u16 + 1;
        app.jump_hint = Some((screen_row, first + 1, first + width));
    }
    hits.extend(footer_hits);
    hits.resize(lines.len() + footer.len(), Hit::None);
    // The slash palette floats above the content instead of taking rows from
    // the frame. It used to be stacked into the footer, which shrank the
    // content area — and with it the side panel, whose bottom rows were cut
    // for as long as the palette was open (`/` made the whole screen jump).
    // An overlay takes no rows from anything: what it covers is painted over,
    // and comes back the moment it closes. In a split frame it covers the
    // **left column only** — the panel beside it keeps every row, divider
    // included, rebuilt here from the same `right` rows the frame used.
    if app.searching() {
        let mut palette_sink = Sink::default();
        palette(app, text_w, cols, content_rows, &mut palette_sink);
        let n = palette_sink.lines.len();
        if n > 0 && n <= content_rows {
            let base = content_rows - n;
            for (i, line) in palette_sink.lines.into_iter().enumerate() {
                let at = base + i;
                let covered = if split {
                    let raw_r = right.get(at).map(|s| s.as_str()).unwrap_or("");
                    let safe_r = if raw_r.contains('\n') || raw_r.contains('\r') {
                        raw_r.replace(['\r', '\n'], " ")
                    } else {
                        raw_r.to_string()
                    };
                    beside(
                        &ansi::truncate(&line, text_w),
                        left_w,
                        &ansi::tame(&safe_r),
                        right_w,
                    )
                } else {
                    line
                };
                lines[at] = ansi::truncate(&covered, cols);
                hits[at] = palette_sink.owner[i];
            }
        }
    }
    app.hits = hits;
    lines.extend(footer);
    if app.panel == Panel::None {
        app.transcript_cache = transcript;
    }
    finish_frame(app, lines)
}

/// What the hint over the composer says while the conversation is scrolled
/// away from its newest row (owner's wording).
pub const JUMP_HINT: &str = "Press Ctrl+↓ Jump to latest";

/// The hint as it is laid over the row — its words with one blank on each
/// side — and the zero-based column its words start at, for a left column
/// `text_w` wide; `None` while it has nothing to say or something else owns
/// that part of the screen.
///
/// Shown as soon as the newest row is off screen, not after some distance: a
/// reader one row up has already lost the line being written. Not shown over
/// an overlay (it is not the conversation), under a card or a form being
/// answered, or with the slash palette open — the palette floats over the
/// same rows.
fn jump_hint(app: &App, text_w: usize) -> Option<(String, u16)> {
    let covered = app.panel != Panel::None
        || app.pending.is_some()
        || app.questions.is_some()
        || app.mcp_input.is_some()
        || app.searching();
    if app.scroll == 0 || covered {
        return None;
    }
    let width = ansi::width(JUMP_HINT);
    if width + 2 > text_w {
        return None;
    }
    let first = (text_w - width) / 2;
    let label =
        format!("{RESET} {MUTED}Press {ACCENT}{BOLD}Ctrl+↓{RESET}{MUTED} Jump to latest{RESET} ");
    Some((label, first as u16))
}

/// One real frame, for tests in another UI module that need the layout to have
/// run — the ask-user modal learns its own geometry from a frame it drew, so a
/// test that never draws is testing a screen nobody sees.
#[cfg(test)]
pub(super) fn compose_for_test(app: &mut App, rows: u16, cols: u16) -> Vec<String> {
    compose(app, rows, cols)
}

fn finish_frame(app: &mut App, mut lines: Vec<String>) -> Vec<String> {
    app.selection_rows = lines.iter().map(|line| ansi::plain(line)).collect();
    if let Some(selection) = app.selection.filter(|selection| selection.moved) {
        for (row, line) in lines.iter_mut().enumerate() {
            if let Some((from, to)) = selection.columns(row) {
                *line = ansi::select(line, from, to);
            }
        }
    }
    lines
}

/// The command's private terminal fits the same closed box the live step draws.
/// This is also used before a call opens, so the process never starts at the
/// full outer width while its output is being shown in a much narrower pane.
pub(super) fn terminal_size(app: &App, rows: u16, cols: u16) -> (u16, u16) {
    let cols = usize::from(cols.max(20));
    let split =
        app.sidebar && cols as u64 >= super::runtime_configuration().number("ui.split_min_columns");
    let text_width = if split {
        (cols * 66 / 100).max(40).saturating_sub(1)
    } else {
        cols
    };
    let box_inner = text_width.saturating_sub(12).max(20);
    (
        super::viewport::compact_rows(usize::from((rows / 2).clamp(4, 20))) as u16,
        box_inner as u16,
    )
}

// ─── The transcript ───────────────────────────────────────────────────────────

/// What a row of the screen is, for the pointer.
///
/// # Why the map is built by the renderer
///
/// A click arrives as a row and a column and nothing else, so something has to
/// say what was drawn there. The alternative — working it out again in the click
/// handler from the same state — is a second layout calculation that agrees with
/// the first until the day one of them is edited, and what they would disagree
/// about is which fold the user just opened.
///
/// So every line is pushed together with what it belongs to, the map is carried
/// through the same scroll and the same truncation as the text, and the handler
/// only ever reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hit {
    #[default]
    None,
    /// A transcript block, by its index in `app.blocks`.
    Block(usize),
    Settings(super::settings::Target),
    Log(usize),
    /// One answer of the permission card.
    Answer(usize),
    Questions(usize),
    McpText(usize),
    /// MCP submit text occupies only these 1-based columns.
    McpSubmit {
        first: usize,
        last: usize,
    },
    /// One entry of the command palette.
    Palette(usize),
    /// The composer's chrome, not an editable row.
    Composer,
    /// One row in the composer's renderer-published hit geometry.
    ComposerText(usize),
    /// One file of `/undo`: a click chooses it, and on `button` (the display
    /// columns its `[undo]` is drawn at, 0-based) presses Enter on it.
    UndoFile {
        index: usize,
        button: (usize, usize),
    },
    /// Only the explicit remove button owns a destructive click.
    Attachments {
        start: usize,
        visible: usize,
        width: usize,
    },
}

pub(super) use super::editor::Row as ComposerRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: u16,
    pub col: u16,
}

/// A terminal-cell selection owned by Rune while mouse reporting is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
    pub moved: bool,
}

impl Selection {
    fn ends(self) -> (Point, Point) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn columns(self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ends();
        let row = row as u16 + 1;
        if row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row {
            start.col.saturating_sub(1) as usize
        } else {
            0
        };
        let to = if row == end.row {
            end.col as usize
        } else {
            usize::MAX
        };
        Some((from, to))
    }

    pub fn selected_text(self, rows: &[String]) -> String {
        if !self.moved {
            return String::new();
        }
        let (start, end) = self.ends();
        let mut selected = Vec::new();
        for row in start.row.saturating_sub(1)..end.row {
            let Some(line) = rows.get(row as usize) else {
                continue;
            };
            let Some((from, to)) = self.columns(row as usize) else {
                continue;
            };
            selected.push(ansi::slice(line, from, to).trim_end().to_string());
        }
        selected.join("\n").trim().to_string()
    }
}

/// Lines, and what each one is.
#[derive(Default, Clone)]
pub struct Sink {
    pub lines: Vec<String>,
    /// One entry per line. Kept as a parallel vector rather than a tuple so that
    /// the common case — a line nothing can be done to — stays `sink.push(l)`.
    pub owner: Vec<Hit>,
}

impl Sink {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn push(&mut self, line: String) {
        self.lines.push(line);
        self.owner.push(Hit::None);
    }

    /// A line the pointer can do something with.
    pub fn push_at(&mut self, line: String, hit: Hit) {
        self.lines.push(line);
        self.owner.push(hit);
    }

    pub fn extend(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            self.push(line);
        }
    }

    fn blank(&mut self) {
        // Never two in a row: a transcript full of double spacing reads as
        // something having gone missing between the paragraphs.
        if self.lines.last().is_some_and(|l| l.is_empty()) {
            return;
        }
        self.push(String::new());
    }
}

/// The ` · 14:32` that follows a rule's name.
///
/// # Why the separator is the only themed part of the stamp
///
/// This line is a *label*, and a label is content: it says whose turn it is or
/// what the program is reporting, and content is inked neutrally so that it stays
/// the same colour under every theme. The separator between the label and its
/// time is furniture, and furniture is what carries the theme — it is why the
/// rule can be found at a glance without the words on it being tinted.
fn rule_stamp(time: Option<&str>) -> String {
    time.filter(|t| !t.is_empty())
        .map(|t| format!("{RULE} · {MUTED}{t}{RESET}"))
        .unwrap_or_default()
}

/// The user rule at the head of a question, with an optional timestamp: `╭─ you · 14:32 ────`.
fn rule_user(time: Option<&str>, width: usize) -> String {
    box_top(
        &format!("{TEXT}{BOLD}you{RESET}{}", rule_stamp(time)),
        width,
    )
}

/// The system notification rule at the head of an automated task notification: `╭─ System_Notification · 14:32 ────`.
///
/// The name is `TEXT` and not the accent: a notification is something the program
/// *told* the user, so it is read, not displayed.
fn rule_system_notification(time: Option<&str>, width: usize) -> String {
    box_top(
        &format!("{TEXT}{BOLD}System_Notification{RESET}{}", rule_stamp(time)),
        width,
    )
}

/// Every line of the conversation, drawn once and kept until it changes.
fn draw_group(app: &App, group: &Group, width: usize, cache: &mut StepCache) -> Sink {
    let inner = width.saturating_sub(2).max(10);
    let mut sink = Sink::default();
    match group {
        Group::One(i) => {
            let available = if matches!(
                app.blocks[*i],
                Block::User { .. }
                    | Block::Notice { .. }
                    | Block::Notification { .. }
                    | Block::Divider { .. }
            ) {
                width
            } else {
                inner
            };
            draw_block(&app.blocks[*i], available, &mut sink);
            if matches!(app.blocks[*i], Block::User { .. }) {
                for owner in sink.owner.iter_mut().skip(1) {
                    *owner = Hit::Block(*i);
                }
            }
        }
        Group::Steps { indices } => draw_steps_with(app, indices, inner, &mut sink, Some(cache)),
    }
    sink
}

/// Rendered steps, keyed by block index, with the stamp they were drawn at.
#[derive(Default)]
pub(super) struct StepCache {
    entries: std::collections::HashMap<usize, (u64, Sink)>,
    ambient: Option<(Ambient, Vec<u64>)>,
}

impl StepCache {
    /// Above this many entries the cache starts over: only the steps of a
    /// running turn are ever re-asked for, so a bound costs nothing real.
    const LIMIT: usize = 4096;

    fn prime(&mut self, ambient: &Ambient, stamps: &[u64]) {
        self.ambient = Some((ambient.clone(), stamps.to_vec()));
        if self.entries.len() > Self::LIMIT {
            self.entries.clear();
        }
    }
}

/// How many lines of head the tail-first pass lays out on one frame.
///
/// The first paint of a resumed session shows the newest screenful, and the
/// rest of the conversation arrives a screenful at a time over the frames
/// after it. Bounded so the fill is never the reason a frame is late, and
/// because a reader who scrolls up gets the whole of it at once anyway
/// ([`transcript_into`] takes `whole` for exactly that).
const HEAD_LINES_PER_FRAME: usize = 240;

/// State every group on screen is drawn against, hashed once per frame.
///
/// Split out of the per-group fingerprint because it is the same for all of
/// them, and because two of its members must NOT be in it: `app.frame` moves
/// every tick while a call is running, and `app.blocks.len()` moves with every
/// block. Folding either in here would redraw the whole transcript on every
/// frame of a stream — the exact cost this cache exists to remove. Both are
/// included per group instead, where they are consulted ([`group_stamp`]).
#[derive(Clone)]
struct Ambient {
    verbose: bool,
    busy: bool,
    frame: usize,
    /// The live plan's content, hashed — not cloned. The plan is consulted
    /// only through this fingerprint, so a hash says everything a clone said
    /// without a deep copy of every task on every dirty frame.
    tasks: u64,
    theme: super::theme::ThemeMode,
}

/// One number that changes when the plan's content changes.
///
/// Derived, not stored: the plan lives in `app.tasks` and is written wholesale
/// by `Event::Tasks`, so no separate dirty flag can be trusted to move with it.
fn plan_hash(tasks: &[serde_json::Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tasks.hash(&mut h);
    h.finish()
}

/// One group's lines, and where they sit in the flat transcript.
struct Laid {
    /// Fingerprint of everything the group's drawing was derived from, or
    /// `None` while the slot has never been drawn. A plain `0` sentinel would
    /// be a guess: a real fingerprint can be zero, and a group that happened
    /// to hash to zero would then keep its placeholder's empty lines forever.
    stamp: Option<u64>,
    start: usize,
    len: usize,
}

/// The transcript, laid out and kept between frames.
///
/// # Why the lines are kept per group
///
/// Rendering a block is markdown, wrapping, and ANSI at terminal width — work
/// proportional to the length of that block. Rendering it again because
/// something *near* it changed is what made a long session stutter: a spinner
/// ticking, one step's counters climbing, a fold opening, any of it redrawing
/// every block in the session ten times a second. So each group is drawn under
/// a fingerprint of everything its drawing reads, and only the groups whose
/// fingerprint moved are drawn again.
///
/// # Why the head arrives late
///
/// A resumed session's transcript can be a couple of megabytes of markdown,
/// and rendering all of it before the first frame is a launch that looks like
/// a hang. The newest screenful is the whole of what a first frame has to
/// show, so that is what it draws; the rest is laid out over the frames
/// after, a screenful at a time. The rows a following reader sees are the same
/// either way — the tail of a transcript does not move when rows are added
/// above it — and scrolling up asks for the whole of it at once, so a reader
/// is never shown a truncated history.
#[derive(Default)]
pub struct Transcript {
    /// Column the lines were drawn at. A different width is a different
    /// layout, not a changed one, and nothing in it can be reused.
    width: usize,
    /// Active theme when the transcript was laid out.
    pub theme: super::theme::ThemeMode,
    /// How many blocks the laid groups were built from. A different count
    /// means the fingerprint pass must run even if nothing said it was dirty.
    blocks: usize,
    /// Each block's own fingerprint, and what it was measured over, so a
    /// block that cannot have changed is not re-read.
    ///
    /// This is what keeps a frame's cost proportional to the block that moved
    /// rather than to the session. The measure is deliberately weak — length,
    /// kind, and a few small fields — because a false "unchanged" would freeze
    /// a block on screen, and a weak measure that only skips the *rehash* is
    /// always safe: when the measure differs the block is hashed for real.
    measured: Vec<(BlockMeasure, u64)>,
    /// The wordmark's own fingerprint, and how many rows it occupies at the
    /// head of the sink. Kept apart from the groups because it is not one: it
    /// is redrawn when the model or the mode changes, with no block involved.
    banner: Option<(u64, usize)>,
    /// Groups `first..first + laid.len()` have their lines in `sink`.
    laid: Vec<Laid>,
    /// How many times a group has been drawn from scratch, ever. Work done is
    /// the honest measure of "a frame costs in proportion to what changed",
    /// which a clock on a loaded machine is not.
    drawn: usize,
    /// Lines of single steps inside Steps groups, by block index and stamp.
    /// A running turn is one Steps group; without this every delta redrew
    /// every step of the turn — diffs, markdown, terminal output (audit S3).
    steps: StepCache,
    /// The first laid group. Everything before it is still to be drawn.
    first: usize,
    /// Every laid line, in order — what the rest of the frame reads.
    pub(super) sink: Sink,
    /// Each line with the colour taken out.
    ///
    /// The scroll anchor and the selection both need the text, and both were
    /// calling [`ansi::plain`] over the whole transcript — a `String` per line,
    /// per frame, to compare against one string. Stripping once when the line
    /// is drawn turns that into a borrow and a compare.
    plain: Vec<String>,
}

impl Transcript {
    /// The group a laid row was drawn in, as a [`Place`]. `None` for the
    /// wordmark's rows and anything not laid.
    fn place_of(&self, line: usize) -> Option<Place> {
        let i = self
            .laid
            .partition_point(|laid| laid.start <= line)
            .checked_sub(1)?;
        let laid = &self.laid[i];
        (line < laid.start + laid.len).then_some(Place {
            group: self.first + i,
            row: line - laid.start,
            rows: laid.len,
            width: self.width,
        })
    }

    /// Where a [`Place`] is in this layout: the same row of its group when the
    /// width is the one it was taken at, the same fraction of the group's
    /// height when the group was wrapped again. `None` when the group is not
    /// laid or holds no rows.
    fn line_at(&self, place: Place) -> Option<usize> {
        let laid = self.laid.get(place.group.checked_sub(self.first)?)?;
        if laid.len == 0 {
            return None;
        }
        let row = if place.width == self.width {
            place.row
        } else {
            place.row * laid.len / place.rows.max(1)
        };
        Some(laid.start + row.min(laid.len - 1))
    }

    /// Older history is still waiting to be laid out. While it is, every frame
    /// lays out another screenful of it, so the window asks for frames until it
    /// is done — otherwise the work waits for the reader's first scroll up and
    /// is done then, all at once, as a stall under the wheel.
    pub fn head_pending(&self) -> bool {
        self.first > 0
    }

    fn new(width: usize, theme: super::theme::ThemeMode) -> Transcript {
        Transcript {
            width,
            theme,
            ..Transcript::default()
        }
    }

    /// Make room for exactly the groups there are now, keeping the ones that
    /// did not change.
    ///
    /// A block *inserted* in the middle — a steer landing under a running turn
    /// — shifts the groups after it, and their fingerprints move with their
    /// indices, so those redraw. Blocks *removed* (`/new`, an undo) can leave
    /// fewer groups than are laid, and the caller starts over: the group
    /// boundaries themselves have moved, so no line in it can be trusted.
    fn fit(&mut self, count: usize) {
        if self.first + self.laid.len() > count {
            let width = self.width;
            let theme = self.theme;
            *self = Transcript::new(width, theme);
            return;
        }
        while self.laid.len() < count - self.first {
            self.laid.push(Laid {
                stamp: None,
                start: self.sink.lines.len(),
                len: 0,
            });
        }
    }

    /// Put freshly drawn lines in place of a group's, moving the rest along.
    fn replace(&mut self, at: usize, stamp: u64, fresh: Sink) {
        let (start, len) = (self.laid[at].start, self.laid[at].len);
        let fresh_plain: Vec<String> = fresh.lines.iter().map(|l| ansi::plain(l)).collect();
        let fresh_len = fresh.lines.len();
        let delta = fresh_len as isize - len as isize;
        self.sink.lines.splice(start..start + len, fresh.lines);
        self.sink.owner.splice(start..start + len, fresh.owner);
        self.plain.splice(start..start + len, fresh_plain);
        self.laid[at] = Laid {
            stamp: Some(stamp),
            start,
            len: fresh_len,
        };
        for laid in self.laid.iter_mut().skip(at + 1) {
            laid.start = (laid.start as isize + delta) as usize;
        }
    }

    /// Add multiple groups above every laid group in a single contiguous batch.
    /// `items` must be ordered from oldest to newest.
    fn prepend_batch(&mut self, items: Vec<(u64, Sink)>) {
        if items.is_empty() {
            return;
        }
        let at = self.banner.map(|(_, len)| len).unwrap_or(0);
        let mut total_len = 0;
        let mut all_lines = Vec::new();
        let mut all_owner = Vec::new();
        let mut all_plain = Vec::new();
        let mut all_laid = Vec::with_capacity(items.len());

        let mut current_start = at;
        let count = items.len();
        for (stamp, fresh) in items {
            let len = fresh.lines.len();
            total_len += len;
            let fresh_plain: Vec<String> = fresh.lines.iter().map(|l| ansi::plain(l)).collect();
            all_lines.extend(fresh.lines);
            all_owner.extend(fresh.owner);
            all_plain.extend(fresh_plain);
            all_laid.push(Laid {
                stamp: Some(stamp),
                start: current_start,
                len,
            });
            current_start += len;
        }

        self.sink.lines.splice(at..at, all_lines);
        self.sink.owner.splice(at..at, all_owner);
        self.plain.splice(at..at, all_plain);
        for laid in self.laid.iter_mut() {
            laid.start += total_len;
        }
        self.laid.splice(0..0, all_laid);
        self.first -= count;
    }

    /// The banner's lines, drawn on demand and kept in the sink.
    ///
    /// The wordmark is not a group — no block owns it — so a change in its
    /// height moves every group's start row. That happens when the model or
    /// the mode changes, which is rare enough to pay a restack for.
    fn banner_into(&mut self, app: &App, width: usize) {
        let stamp = banner_stamp(app, width);
        if self.banner.map(|(s, _)| s) == Some(stamp) {
            return;
        }
        let lines = super::banner::render(app, width);
        let len = lines.len();
        let plain: Vec<String> = lines.iter().map(|l| ansi::plain(l)).collect();
        let owner = vec![Hit::None; len];
        let old = self.banner.map(|(_, n)| n).unwrap_or(0);
        self.sink.lines.splice(0..old, lines);
        self.sink.owner.splice(0..old, owner);
        self.plain.splice(0..old, plain);
        self.banner = Some((stamp, len));
        self.restack();
    }

    /// Recompute every group's start row from the lengths above it.
    fn restack(&mut self) {
        let mut at = self.banner.map(|(_, len)| len).unwrap_or(0);
        for laid in self.laid.iter_mut() {
            laid.start = at;
            at += laid.len;
        }
    }
}

/// A cheap reading of a block, used to decide whether it is worth hashing.
///
/// Never used as the fingerprint itself — it is a filter in front of one. A
/// measure that says "same" must guarantee the block is byte-identical; a
/// measure that says "different" costs one real hash and nothing more. The
/// lengths are the reason: streaming appends, a step's counters climb, a
/// result arrives, and none of those keep a block the same length.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
struct BlockMeasure {
    kind: u8,
    text: usize,
    output: (usize, u64),
    content: usize,
    summary: usize,
    raw: usize,
    action: usize,
    args: bool,
    detail: bool,
    ask: bool,
    /// A notice's own flag: a red notice and a plain one with the same words
    /// draw differently, and a filter that could not tell them apart would
    /// leave an error on screen after it was downgraded.
    bad: bool,
    open: bool,
    state: u8,
    shown: (usize, usize),
    elapsed: u64,
}

impl BlockMeasure {
    fn of(block: &Block) -> BlockMeasure {
        match block {
            Block::User {
                text, media, open, ..
            } => BlockMeasure {
                kind: 0,
                text: text.len() + media.len(),
                output: (0, 0),
                content: 0,
                summary: 0,
                raw: 0,
                action: 0,
                args: false,
                detail: false,
                ask: false,
                bad: false,
                open: *open,
                state: 0,
                shown: (0, 0),
                elapsed: 0,
            },
            Block::Text { text } => BlockMeasure {
                kind: 1,
                text: text.len(),
                output: (0, 0),
                content: 0,
                summary: 0,
                raw: 0,
                action: 0,
                args: false,
                detail: false,
                ask: false,
                bad: false,
                open: false,
                state: 0,
                shown: (0, 0),
                elapsed: 0,
            },
            Block::Thinking { text, open } => BlockMeasure {
                kind: 2,
                text: text.len(),
                output: (0, 0),
                content: 0,
                summary: 0,
                raw: 0,
                action: 0,
                args: false,
                detail: false,
                ask: false,
                bad: false,
                open: *open,
                state: 0,
                shown: (0, 0),
                elapsed: 0,
            },
            Block::Notification { text, .. } | Block::Divider { text } => BlockMeasure {
                kind: 4,
                text: text.len(),
                output: (0, 0),
                content: 0,
                summary: 0,
                raw: 0,
                action: 0,
                args: false,
                detail: false,
                ask: false,
                bad: false,
                open: false,
                state: 0,
                shown: (0, 0),
                elapsed: 0,
            },
            Block::Notice { text, bad } => BlockMeasure {
                kind: 3,
                text: text.len(),
                output: (0, 0),
                content: 0,
                summary: 0,
                raw: 0,
                action: 0,
                args: false,
                detail: false,
                ask: false,
                bad: *bad,
                open: false,
                state: 0,
                shown: (0, 0),
                elapsed: 0,
            },
            Block::Tool(tool) => BlockMeasure {
                kind: 4,
                text: 0,
                output: (tool.output.len(), tool.output_revision),
                content: tool.content.len(),
                summary: tool.summary.len(),
                raw: tool.raw.len(),
                action: tool.action.len(),
                args: tool.args.is_some(),
                detail: tool.detail.is_some(),
                ask: tool.ask.is_some(),
                bad: false,
                open: tool.open,
                state: tool.state as u8,
                shown: tool.displayed_counts(),
                elapsed: tool.elapsed().as_secs(),
            },
        }
    }
}

/// A fingerprint of everything one group's drawing reads.
///
/// Complete by construction or the group goes stale on screen, so this covers
/// the blocks themselves ([`Block::stamp`]), the app state the timeline reads,
/// and the two values that would be ruinous to put in [`Ambient`] — the pulse
/// and the block count — *only* where they are actually consulted.
///
/// The pulse is read for one thing: a running step's bullet blinking. A group
/// with no unfinished call draws the same bullet whichever tick it is drawn
/// on, and putting the frame into every group would redraw the whole
/// transcript sixty times a second while a single step ran.
///
/// The block count is read for one thing too: [`draw_work_text`] calls a
/// thought "Thinking" only while it is the newest block, and [`draw_tasks`]
/// falls back to the live plan only for the newest block. Both are therefore
/// true of at most one group — the one holding the last block — and that is
/// the only group whose fingerprint they enter.
fn group_stamp(app: &App, groups: &[Group], at: usize, ambient: &Ambient, stamps: &[u64]) -> u64 {
    let indices: &[usize] = match &groups[at] {
        Group::One(i) => std::slice::from_ref(i),
        Group::Steps { indices } => indices,
    };
    stamp_of(app, indices, ambient, stamps)
}

/// The fingerprint of blocks drawn together: a whole group, or one step.
fn stamp_of(app: &App, indices: &[usize], ambient: &Ambient, stamps: &[u64]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    indices.hash(&mut h);
    ambient.verbose.hash(&mut h);
    ambient.theme.name().hash(&mut h);
    let holds_last = indices.last() == Some(&app.blocks.len().saturating_sub(1));
    holds_last.hash(&mut h);
    if holds_last {
        ambient.tasks.hash(&mut h);
    }
    for i in indices {
        stamps[*i].hash(&mut h);
    }
    let live = indices.iter().any(|i| {
        matches!(&app.blocks[*i], Block::Tool(t) if !t.state.finished() || t.state == ToolState::Asking)
    });
    // `busy` is read by exactly these groups (the newest block's "Thinking",
    // a live tool's spinner and clock). Hashing it into every group made each
    // turn start and end re-lay the entire transcript (audit S1).
    if holds_last || live {
        ambient.busy.hash(&mut h);
    }
    if indices
        .iter()
        .any(|i| matches!(&app.blocks[*i], Block::Tool(t) if !t.state.finished()))
    {
        ambient.frame.hash(&mut h);
    }
    h.finish()
}

/// Every block's fingerprint, reusing the stored one wherever the block cannot
/// have changed.
///
/// `previous` is the last frame's `(measure, stamp)` per block. A block whose
/// cheap reading is identical keeps its stamp without being read; anything
/// else is hashed for real. The saving is the whole session's text on every
/// frame of a stream, and the price is one length-and-flags reading per block.
fn block_stamps(app: &App, previous: &[(BlockMeasure, u64)]) -> Vec<u64> {
    let mut out = Vec::with_capacity(app.blocks.len());
    for (i, block) in app.blocks.iter().enumerate() {
        let measure = BlockMeasure::of(block);
        out.push(match previous.get(i) {
            Some((old, stamp)) if *old == measure => *stamp,
            _ => block.stamp(),
        });
    }
    out
}

/// A fingerprint of everything the wordmark prints beside the logo.
fn banner_stamp(app: &App, width: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut h);
    app.theme.name().hash(&mut h);
    app.model().hash(&mut h);
    app.provider().hash(&mut h);
    app.thinking().hash(&mut h);
    app.effort().hash(&mut h);
    app.settings["thinking_required"].as_bool().hash(&mut h);
    app.mode.hash(&mut h);
    app.project.hash(&mut h);
    h.finish()
}

/// The conversation, laid out and kept, ready to be sliced into a viewport.
///
/// `cache` is the previous frame's, kept so nothing is drawn twice. `dirty` is
/// the caller's "something moved" signal; when it is false the fingerprints
/// are not recomputed at all, so an idle frame costs nothing.
///
/// It is a signal and not a proof, so the block count is checked as well: a
/// block that arrives without setting the flag — a steer landing under a
/// running turn, a test building a transcript by hand — is still a block, and
/// its absence from the screen would be a lie no other check would catch.
/// `whole` asks for the entire transcript at once: a reader who has scrolled
/// up is asking for history, and half a history is not history.
/// The first row of the conversation the next frame shows.
///
/// At the live end it is the tail. After a deliberate move (the anchor
/// dropped) `scroll` is measured from the current bottom — not from the
/// previous frame's geometry: when the first upward wheel makes a partial,
/// tail-only layout lay out the whole history, the old height is hundreds of
/// rows short, and a three-row notch was flung hundreds of rows up.
///
/// Otherwise the reader is held at what they were reading ([`Anchor`]): the
/// group and the row inside it give the place in this layout — the same row
/// when the width held, the same fraction of the group when it wrapped again —
/// and, when the width held, the row's text is looked for near that place, so
/// a row that moved inside its group (a step above it grew) is still found.
/// The search stays local: a global one can match the same text hundreds of
/// screens away and fling the reader there without a gesture. Text is not
/// trusted across a new width — a reflowed paragraph has no row that reads as
/// before, and a border or a blank-ish row that does is a different place.
fn chat_top(app: &App, tx: &Transcript, max_top: usize) -> usize {
    if app.scroll == 0 {
        return max_top;
    }
    let Some(anchor) = app.scroll_anchor.as_ref() else {
        return max_top.saturating_sub(app.scroll);
    };
    // Where the previous frame's top row is now, had nothing above it moved.
    let arithmetic = app
        .drawn_rows
        .saturating_sub(app.drawn_viewport + app.scroll)
        .min(max_top);
    let place = anchor.place.and_then(|p| tx.line_at(p));
    let same_width = anchor.place.is_none_or(|p| p.width == tx.width);
    if !same_width {
        return place
            .map_or(arithmetic, |line| line.saturating_sub(anchor.offset))
            .min(max_top);
    }
    let target = place.unwrap_or(arithmetic + anchor.offset);
    let lines = &tx.sink.lines;
    let radius = 250;
    let from = target.saturating_sub(radius);
    let to = (target + radius + 1).min(lines.len());
    (from..to)
        .filter(|&at| {
            if tx.plain.len() == lines.len() {
                tx.plain[at] == anchor.text
            } else {
                ansi::plain(&lines[at]) == anchor.text
            }
        })
        .min_by_key(|&at| at.abs_diff(target))
        .unwrap_or(target)
        .saturating_sub(anchor.offset)
        .min(max_top)
}

/// The anchor for a screen whose first row is `top`: its first non-blank
/// row, by text and by place.
fn anchor_at(tx: &Transcript, top: usize, content_rows: usize) -> Option<Anchor> {
    let lines = &tx.sink.lines;
    (top..(top + content_rows).min(lines.len())).find_map(|at| {
        let text = if tx.plain.len() == lines.len() {
            tx.plain[at].clone()
        } else {
            ansi::plain(&lines[at])
        };
        (!text.trim().is_empty()).then(|| Anchor {
            text,
            offset: at - top,
            place: tx.place_of(at),
        })
    })
}

pub fn transcript_into(
    cache: Option<Transcript>,
    app: &App,
    width: usize,
    dirty: bool,
    whole: bool,
) -> Transcript {
    let mut cache = cache;
    if cache.as_ref().is_some_and(|tx| {
        !dirty
            && tx.blocks == app.blocks.len()
            && tx.width == width
            && tx.theme == app.theme
            && tx.first == 0
    }) {
        let mut tx = cache.take().unwrap();
        tx.banner_into(app, width);
        return tx;
    }
    let groups = group(&app.blocks);
    let mut tx = match cache {
        Some(tx)
            if tx.width == width
                && tx.theme == app.theme
                && tx.first + tx.laid.len() <= groups.len() =>
        {
            tx
        }
        // Nothing is laid out yet, and `first` starts at the end of the
        // conversation rather than the beginning: the fill below walks
        // backwards from the newest group, so a first paint draws the tail.
        // Starting at zero instead would have `fit` reserve a slot for every
        // group and then draw all of them — the whole session before the first
        // frame, which is the thing being avoided.
        _ => Transcript {
            width,
            theme: app.theme,
            first: groups.len(),
            ..Transcript::new(width, app.theme)
        },
    };
    let dirty = dirty || tx.blocks != app.blocks.len();
    tx.blocks = app.blocks.len();
    tx.fit(groups.len());
    tx.banner_into(app, width);

    // One ambient reading for the whole pass, and one hash of every block.
    // Both are shared by the redraw loop and the head fill below; building
    // them twice would be the same work done twice for no reason.
    let ambient = Ambient {
        verbose: app.verbose,
        busy: app.busy,
        frame: app.frame,
        tasks: plan_hash(&app.tasks),
        theme: app.theme,
    };
    let stamps = block_stamps(app, &tx.measured);
    tx.measured = app
        .blocks
        .iter()
        .map(BlockMeasure::of)
        .zip(stamps.iter().copied())
        .collect();

    tx.steps.prime(&ambient, &stamps);
    if dirty {
        for i in 0..tx.laid.len() {
            let at = tx.first + i;
            let stamp = group_stamp(app, &groups, at, &ambient, &stamps);
            if tx.laid[i].stamp != Some(stamp) {
                let fresh = draw_group(app, &groups[at], width, &mut tx.steps);
                tx.drawn += 1;
                tx.replace(i, stamp, fresh);
            }
        }
    }

    // The head, a screenful at a time — unless the reader scrolled up, in
    // which case all of it, now. Only the newest screenful is ever needed for
    // a first frame, and the rows a following reader sees do not change when
    // rows are added above them.
    if tx.first > 0 {
        let mut budget = if whole {
            usize::MAX
        } else {
            HEAD_LINES_PER_FRAME
        };
        let mut batch = Vec::new();
        while tx.first > 0 && budget > 0 {
            let at = tx.first - 1;
            let stamp = group_stamp(app, &groups, at, &ambient, &stamps);
            let fresh = draw_group(app, &groups[at], width, &mut tx.steps);
            tx.drawn += 1;
            // A group that draws nothing still costs a step, so the budget
            // spends at least a row on it rather than spinning.
            budget = budget.saturating_sub(fresh.lines.len().max(1));
            batch.push((stamp, fresh));
            tx.first -= 1;
        }
        if !batch.is_empty() {
            batch.reverse();
            tx.first += batch.len();
            tx.prepend_batch(batch);
        }
    }

    tx
}

/// What the transcript is made of once consecutive steps have been collected.
enum Group {
    One(usize),
    /// Every activity before the final response. This grouping exists only to
    /// draw one continuous spine; it never hides any item.
    Steps {
        indices: Vec<usize>,
    },
}

/// Collect each turn's activity into one continuous timeline. A text response
/// joins it only when thinking or a tool follows in the same turn. The last
/// response stays outside as the readable final answer.
fn group(blocks: &[Block]) -> Vec<Group> {
    // Where the next `you` rule is, for every position, in one backward pass.
    //
    // The scan this replaces asked "where does this turn end?" by walking
    // forward from each answer, and a session with n answers and no rule
    // between them answered it n²/2 times — every frame, over the whole
    // transcript, to learn something that never changes between two
    // neighbours. That is the per-frame floor under a long session, and it is
    // most of why streaming a turn in a big session still cost milliseconds.
    let mut next_rule = vec![blocks.len(); blocks.len() + 1];
    for i in (0..blocks.len()).rev() {
        next_rule[i] = if matches!(blocks[i], Block::User { .. }) {
            i
        } else {
            next_rule[i + 1]
        };
    }
    let mut out: Vec<Group> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    let flush = |run: &mut Vec<usize>, out: &mut Vec<Group>| {
        if !run.is_empty() {
            out.push(Group::Steps {
                indices: std::mem::take(run),
            });
        }
    };
    for (i, block) in blocks.iter().enumerate() {
        match block {
            Block::Tool(_) | Block::Thinking { .. } => run.push(i),
            Block::Text { .. } => {
                let turn_end = next_rule[i + 1];
                let followed_by_activity = blocks[i + 1..turn_end]
                    .iter()
                    .any(|next| matches!(next, Block::Tool(_) | Block::Thinking { .. }));
                if followed_by_activity {
                    run.push(i);
                } else {
                    flush(&mut run, &mut out);
                    out.push(Group::One(i));
                }
            }
            Block::User { .. }
            | Block::Notice { .. }
            | Block::Notification { .. }
            | Block::Divider { .. } => {
                flush(&mut run, &mut out);
                out.push(Group::One(i));
            }
        }
    }
    flush(&mut run, &mut out);
    out
}

fn draw_block(block: &Block, inner: usize, sink: &mut Sink) {
    match block {
        Block::User {
            text,
            time,
            media,
            open,
        } => {
            sink.blank();
            let text_wrap = inner.saturating_sub(4).max(8);
            if media.is_empty() {
                sink.push(rule_user(time.as_deref(), inner));
            } else {
                let prefix = format!(
                    "{RULE}╭─ {TEXT}{BOLD}you{RESET}{}{RULE} ──{RESET}",
                    rule_stamp(time.as_deref())
                );
                let (attachments, _, _, _) = attachment_boxes(&prefix, media, inner, false);
                if let Some(top_line) = attachments.first() {
                    sink.push(top_line.clone());
                } else {
                    sink.push(rule_user(time.as_deref(), inner));
                }
            }
            let lines = ansi::wrap(text, text_wrap);
            let shown = lines.len().min(super::viewport::preview_rows(*open));
            for line in &lines[..shown] {
                sink.push(boxed(&format!("{USER}{line}{RESET}"), text_wrap));
            }
            let omitted = lines.len() - shown;
            if omitted > 0 {
                sink.push(boxed(
                    &format!("{MUTED}{}{RESET}", super::viewport::cut_lines(omitted)),
                    text_wrap,
                ));
            }
            sink.push(box_bottom(inner));
        }
        Block::Notification { text, time } => {
            sink.blank();
            let text_wrap = inner.saturating_sub(4).max(8);
            sink.push(rule_system_notification(time.as_deref(), inner));
            // The model reads the tag; a person reads the sentence inside it.
            let shown = text
                .trim()
                .trim_start_matches("<system_notification>")
                .trim_end_matches("</system_notification>")
                .trim();
            for line in ansi::wrap(shown, text_wrap) {
                sink.push(boxed(&format!("{TEXT}{line}{RESET}"), text_wrap));
            }
            sink.push(box_bottom(inner));
        }
        Block::Divider { text } => {
            sink.blank();
            let label = format!(" {text} ");
            let side = inner.saturating_sub(ansi::width(&label)) / 2;
            sink.push(format!(
                "{RULE}{}{FAINT}{label}{RULE}{}{RESET}",
                "─".repeat(side),
                "─".repeat(inner.saturating_sub(side + ansi::width(&label)))
            ));
        }
        Block::Text { text, .. } => {
            // No gutter. There was one — a column of `│` down the left of
            // every answer — and the owner's report on seeing it running is the
            // argument against it: **the vertical rule means "work in
            // progress"**. It is the spine of a run of steps, drawn between the
            // things the agent did, and putting the same mark down the side of
            // the finished answer made the answer look like one more step.
            //
            // A mark that means two things means neither. The answer is now
            // separated the way everything else on this screen is: by the rule
            // above it, the space around it, and nothing else.
            //
            // The answer owns the whole transcript column. The old 92-column
            // cap looked like a split panel even when the split was off and,
            // worse, handed tables the same narrow measure as prose.
            for line in markdown::render(text, inner + 2) {
                sink.push(line);
            }
        }
        Block::Notice { text, bad } => {
            sink.blank();
            let label = if *bad { "error" } else { "system" };
            let text_ink = if *bad { REMOVED } else { TEXT };
            let text_wrap = inner.saturating_sub(4).max(8);
            sink.push(box_top(label, inner));
            for raw_line in text.lines() {
                let trimmed = raw_line.trim_end();
                if trimmed.is_empty() {
                    sink.push(boxed("", text_wrap));
                    continue;
                }
                for line in ansi::wrap(trimmed, text_wrap) {
                    sink.push(boxed(&format!("{text_ink}{line}{RESET}"), text_wrap));
                }
            }
            sink.push(box_bottom(inner));
        }
        Block::Thinking { .. } | Block::Tool(_) => {
            unreachable!("activities are drawn on the timeline")
        }
    }
}

/// Which block a click on this transcript line reaches, if any.
///
/// The map is a fact about a frame, so it is read rather than recomputed — see
/// [`Hit`].
pub fn hit_at(app: &App, row: u16) -> Hit {
    app.hits
        .get(row.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(Hit::None)
}

/// A run of activities, drawn as one uninterrupted timeline.
///
/// # Why there is a spine
///
/// The steps used to be a flat list of headings with bullets under them, and at
/// four or five calls it stopped reading as *one* thing the agent did and
/// started reading as debris between two paragraphs. A single vertical rule
/// down the left says what no amount of indentation could: this began here, it
/// ended there, and everything between the two is the same stretch of work.
///
/// A condition bullet:
/// - Blinks between white and dark while actively processing.
/// - Turns solid green (`OK`) once finished successfully.
/// - Turns solid red (`REMOVED`) or muted if failed/stopped (NO cross/check emotes!).
pub fn condition_bullet(processing: bool, ok: bool, frame: usize) -> String {
    if processing {
        // Slow, calm pulse cycle (~600ms bright / ~600ms faint)
        if (frame / 38).is_multiple_of(2) {
            format!("{TEXT}{BOLD}●{RESET}")
        } else {
            format!("{FAINT}●{RESET}")
        }
    } else if ok {
        format!("{OK}{BOLD}●{RESET}")
    } else {
        format!("{REMOVED}{BOLD}●{RESET}")
    }
}

/// A run of steps, drawn as one piece of work rather than a list of calls.
///
/// # Why there is a spine
///
/// The steps used to be a flat list of headings with bullets under them, and at
/// four or five calls it stopped reading as *one* thing the agent did and
/// started reading as debris between two paragraphs. A single vertical rule
/// down the left says what no amount of indentation could: this began here, it
/// ended there, and everything between the two is the same stretch of work.
///
/// The timeline itself never folds. A thought or tool can still expose its own
/// detail without hiding any sibling activity.
#[cfg(test)]
fn draw_steps(app: &App, indices: &[usize], inner: usize, sink: &mut Sink) {
    draw_steps_with(app, indices, inner, sink, None);
}

fn draw_steps_with(
    app: &App,
    indices: &[usize],
    inner: usize,
    sink: &mut Sink,
    mut cache: Option<&mut StepCache>,
) {
    if indices.is_empty() {
        return;
    }
    sink.blank();
    for (idx, i) in indices.iter().copied().enumerate() {
        let is_last = idx + 1 == indices.len();
        let stamp = cache.as_ref().and_then(|cache| {
            let (ambient, stamps) = cache.ambient.as_ref()?;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&(stamp_of(app, &[i], ambient, stamps), is_last), &mut h);
            Some(std::hash::Hasher::finish(&h))
        });
        let cached = cache
            .as_ref()
            .and_then(|cache| cache.entries.get(&i))
            .filter(|(at, _)| Some(*at) == stamp)
            .map(|(_, lines)| lines);
        match cached {
            Some(lines) => {
                sink.lines.extend(lines.lines.iter().cloned());
                sink.owner.extend(lines.owner.iter().cloned());
            }
            None => {
                let mut step = Sink::default();
                draw_one_step(app, i, is_last, inner, &mut step);
                sink.lines.extend(step.lines.iter().cloned());
                sink.owner.extend(step.owner.iter().cloned());
                if let (Some(cache), Some(stamp)) = (cache.as_mut(), stamp) {
                    cache.entries.insert(i, (stamp, step));
                }
            }
        }
        if !is_last {
            sink.push(format!("  {RULE}│{}", ansi::RESET));
        }
    }
    sink.blank();
}

fn draw_one_step(app: &App, i: usize, is_last: bool, inner: usize, sink: &mut Sink) {
    match &app.blocks[i] {
        Block::Tool(tool) => draw_step(app, i, tool, is_last, inner, sink),
        Block::Thinking { text, open } => draw_work_text(
            app,
            i,
            "thought",
            text,
            *open || app.verbose,
            is_last,
            inner,
            sink,
        ),
        Block::Text { text } => {
            draw_work_text(app, i, "response", text, true, is_last, inner, sink)
        }
        _ => {}
    }
}

/// Thinking or an intermediate response as one activity on the same spine as
/// tool calls. Thinking keeps its live word count; an intermediate response is
/// always open and carries a muted neutral bullet instead of a tool condition.
#[allow(clippy::too_many_arguments)]
fn draw_work_text(
    app: &App,
    at: usize,
    label: &str,
    text: &str,
    open: bool,
    is_last: bool,
    inner: usize,
    sink: &mut Sink,
) {
    let is_processing = app.busy && at == app.blocks.len() - 1;
    let is_response = label == "response";
    // Thinking and prose do not succeed or fail. Their marker stays neutral
    // from the first token through the settled Thought; condition colours are
    // reserved for tools whose outcome really has a state.
    let bullet = format!("{MUTED}●{RESET}");
    let words = text.split_whitespace().count();
    let display_label = if !is_response {
        if is_processing {
            "Thinking"
        } else {
            "Thought"
        }
    } else {
        "Response"
    };
    let label_colour = if is_response { MUTED } else { THINK };
    let word_count = if is_response {
        String::new()
    } else {
        format!(" {MUTED}· {words} words{RESET}")
    };

    // Bullet ● sits at column 2, aligning directly with the │ spine
    let owner = if is_response {
        Hit::None
    } else {
        Hit::Block(at)
    };
    sink.push_at(
        format!("  {bullet} {label_colour}{display_label}{RESET}{word_count}"),
        owner,
    );
    if open {
        let clean_text = text.trim_start_matches(['\r', '\n']);
        let rendered = markdown::render(clean_text, inner.saturating_sub(6));
        let lines = rendered.into_iter().skip_while(|l| l.trim().is_empty());
        for line in lines {
            let line_str = if is_last {
                format!("     {line}")
            } else {
                format!("  {RULE}│{}   {line}", RESET)
            };
            sink.push_at(line_str, owner);
        }
    }
}

fn take_cols_front(s: &str, max_w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = ansi::char_width(c);
        if used + w > max_w {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

fn take_cols_back(s: &str, max_w: usize) -> String {
    let mut out = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = ansi::char_width(c);
        if used + w > max_w {
            break;
        }
        out.push(c);
        used += w;
    }
    out.into_iter().rev().collect()
}

/// Truncates a filename to fit within `budget` columns, preserving the file
/// extension and performing middle truncation when possible.
fn truncate_filename(filename: &str, budget: usize) -> String {
    if ansi::width(filename) <= budget {
        return filename.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget <= 4 {
        return ansi::truncate(filename, budget);
    }

    if let Some((base, ext)) = filename.rsplit_once('.') {
        if !ext.is_empty() && ext.len() <= 8 && ext.len() + 3 <= budget {
            let ext_suffix = format!(".{ext}");
            let ext_w = ansi::width(&ext_suffix);
            let base_budget = budget.saturating_sub(ext_w);

            if base_budget >= 5 {
                let avail_chars = base_budget.saturating_sub(1); // 1 for '…'
                let half_front = avail_chars.div_ceil(2);
                let half_back = avail_chars.saturating_sub(half_front);

                let front = take_cols_front(base, half_front);
                let back = take_cols_back(base, half_back);
                return format!("{front}…{back}{ext_suffix}");
            }
        }
    }

    let avail_chars = budget.saturating_sub(1);
    let half_front = avail_chars.div_ceil(2);
    let half_back = avail_chars.saturating_sub(half_front);
    let front = take_cols_front(filename, half_front);
    let back = take_cols_back(filename, half_back);
    format!("{front}…{back}")
}

/// Shortens directory segments to fit within `budget` columns.
/// For example, `docs/Current/Studio/Complete/2026-09` can become `docs/…/2026-09`.
fn truncate_dir(dir: &str, budget: usize, sep: char) -> String {
    if ansi::width(dir) <= budget {
        return dir.to_string();
    }
    if budget < 4 {
        return ansi::truncate(dir, budget);
    }

    let is_absolute = dir.starts_with(sep);
    let parts: Vec<&str> = dir.split(sep).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return if is_absolute {
            format!("{sep}")
        } else {
            dir.to_string()
        };
    }

    let prefix = if is_absolute {
        format!("{sep}")
    } else {
        String::new()
    };
    let prefix_w = ansi::width(&prefix);
    let avail = budget.saturating_sub(prefix_w);

    if parts.len() == 1 {
        let tail = ansi::truncate(parts[0], avail);
        return format!("{prefix}{tail}");
    }

    let sep_str = sep.to_string();
    let first = parts[0];
    let last = parts[parts.len() - 1];

    // Minimal middle form: "{first}/…/{last}"
    let min_middle_w = ansi::width(first) + 1 + 1 + 1 + ansi::width(last);
    if min_middle_w <= avail {
        let mut front_count = 1;
        let mut back_count = 1;

        while front_count + back_count < parts.len() {
            let next_back = parts[parts.len() - 1 - back_count];
            let additional_back_w = 1 + ansi::width(next_back);

            let front_w: usize = parts[..front_count]
                .iter()
                .map(|p| ansi::width(p) + 1)
                .sum();
            let back_w: usize = parts[parts.len() - back_count..]
                .iter()
                .map(|p| ansi::width(p) + 1)
                .sum();
            let current_w = prefix_w + front_w + 1 + back_w;

            if current_w + additional_back_w <= budget {
                back_count += 1;
                continue;
            }

            let next_front = parts[front_count];
            let additional_front_w = 1 + ansi::width(next_front);
            if current_w + additional_front_w <= budget {
                front_count += 1;
                continue;
            }

            break;
        }

        let front_str = parts[..front_count].join(&sep_str);
        let back_str = parts[parts.len() - back_count..].join(&sep_str);
        format!("{prefix}{front_str}{sep}…{sep}{back_str}")
    } else {
        // Doesn't fit first/…/last.
        // We MUST preserve the root directory / folder terdepan (`first`), because it identifies
        // the project module/subsystem (e.g. `Application`, `docs`, `Server`, `UI`).
        let first_with_dots = format!("{first}{sep}…");
        if prefix_w + ansi::width(&first_with_dots) <= budget {
            format!("{prefix}{first_with_dots}")
        } else {
            let head_avail = budget.saturating_sub(prefix_w + 1); // 1 for ellipsis
            if head_avail >= 2 {
                format!("{prefix}{}…", ansi::truncate(first, head_avail))
            } else {
                format!("{prefix}…")
            }
        }
    }
}

/// Truncate a file path intelligently to fit within `max_cols`, adhering to the
/// strict hierarchy:
///   1. Filename is preserved over directory path.
///   2. When space is constrained, directory middle segments are replaced by `…`.
///   3. Both directory and filename retain bounded minimum quotas so neither is
///      starved down to an empty or bare ellipsis (`…`).
///   4. When filename must be truncated, its extension and distinctive prefix/suffix
///      are preserved via middle truncation.
pub fn truncate_path(path: &str, max_cols: usize) -> String {
    let total_w = ansi::width(path);
    if total_w <= max_cols {
        return path.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    if max_cols <= 4 {
        return ansi::truncate(path, max_cols);
    }

    let last_sep_idx = path.rfind(['/', '\\']);
    let (dir_raw, file_raw, sep) = match last_sep_idx {
        Some(0) => ("/", &path[1..], path.chars().next().unwrap_or('/')),
        Some(idx) => {
            let sep_char = path[idx..].chars().next().unwrap_or('/');
            let dir = &path[..idx];
            let file = &path[idx + sep_char.len_utf8()..];
            (dir, file, sep_char)
        }
        None => ("", path, '/'),
    };

    if dir_raw.is_empty() {
        return truncate_filename(file_raw, max_cols);
    }

    let sep_char_buf = sep.to_string();
    let avail = max_cols.saturating_sub(1); // 1 col for separator

    // Directory quota: at least 25% of max_cols clamped between 6 and 16, capped at dir length
    let min_dir = (max_cols * 25 / 100).clamp(6, 16).min(ansi::width(dir_raw));

    let file_w = ansi::width(file_raw);
    let (dir_budget, file_budget) = if file_w + min_dir <= avail {
        // Filename fits fully while giving dir at least min_dir!
        let fb = file_w;
        let db = avail.saturating_sub(fb).min(ansi::width(dir_raw));
        (db, fb)
    } else {
        // Filename is long, allocate bounded dir budget so dir is not reduced to just "…"
        let db = min_dir;
        let fb = avail.saturating_sub(db);
        (db, fb)
    };

    let dir_display = truncate_dir(dir_raw, dir_budget, sep);
    let file_display = truncate_filename(file_raw, file_budget);

    if dir_display.is_empty() {
        file_display
    } else {
        let sep_str = if dir_display.ends_with(sep) {
            ""
        } else {
            &sep_char_buf
        };
        format!("{dir_display}{sep_str}{file_display}")
    }
}

/// Format the trailing annotation beside a tool bullet.
///
/// Edits and writes carry +/- line counters, which are coloured directly:
/// deletions (N-) in bright red and additions (N+) in bright green. All other
/// notes retain the default faint neutral ink.
fn format_bullet_note(verb: &str, note: &str) -> String {
    if note.is_empty() {
        return String::new();
    }
    if verb == "Edit" || verb == "Write" {
        let mut out = String::new();
        for part in note.split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            if part.starts_with('−') || part.starts_with('-') || part == "failed" {
                out.push_str(&format!("{REMOVED}{part}{RESET}"));
            } else if part.starts_with('+') {
                out.push_str(&format!("{ADDED}{part}{RESET}"));
            } else {
                out.push_str(&format!("{FAINT}{part}{RESET}"));
            }
        }
        format!("  {out}")
    } else {
        format!("  {FAINT}{note}{RESET}")
    }
}

/// Formats a bullet row with priority hierarchy:
///   1. Line numbers / note (highest priority: reserved first, immune to truncation)
///   2. Filename (preserved over parent directory path)
///   3. Parent path (truncated with middle '…' segments when tight)
fn format_bullet_row(
    bullet: &steps::Bullet,
    spine: &str,
    is_file_tool: bool,
    max_cols: usize,
) -> String {
    let note = format_bullet_note(bullet.verb, &bullet.note);
    let note_w = ansi::width(&note);
    let verb_part = if is_file_tool || bullet.path.is_some() {
        format!("{}  ", bullet.verb)
    } else {
        format!("{:<8} ", bullet.verb)
    };
    let prefix_w = ansi::width(spine) + ansi::width(&verb_part);
    let avail_for_target = max_cols.saturating_sub(prefix_w + note_w);
    let display_target = if bullet.path.is_some() || is_file_tool {
        truncate_path(&bullet.target, avail_for_target)
    } else {
        ansi::truncate(&bullet.target, avail_for_target)
    };
    let target = match &bullet.path {
        Some(path) => ansi::link(&format!("file://{path}"), &display_target),
        None => display_target,
    };
    format!("{spine}{MUTED}{verb_part}{TEXT}{target}{RESET}{note}")
}

/// One step: the model's sentence, then what the engine actually did.
fn draw_step(app: &App, at: usize, tool: &Tool, is_last: bool, inner: usize, sink: &mut Sink) {
    let is_processing = (!tool.state.finished() || tool.state == ToolState::Asking) && app.busy;
    let ok = matches!(tool.state, ToolState::Ok | ToolState::Backgrounded);
    let bullet = condition_bullet(is_processing, ok, app.frame);
    let open = tool.open || app.verbose;
    let denied = tool
        .detail
        .as_ref()
        .and_then(|detail| detail["status"].as_str())
        == Some("denied");

    let seconds = tool.elapsed().as_secs();
    let timing = if (!tool.state.finished() && app.busy) || seconds >= 1 {
        let formatted = format_duration(seconds);
        format!(" {FAINT}{formatted}{RESET}")
    } else {
        String::new()
    };
    let head = if tool.state == ToolState::Backgrounded {
        format!("{} · continuing in background", steps::heading(tool))
    } else {
        steps::heading(tool)
    };
    let room = inner.saturating_sub(6 + if timing.is_empty() { 0 } else { 4 });
    let mut head_lines = ansi::wrap(&head, room);
    let rest = head_lines.split_off(1.min(head_lines.len()));
    for line in head_lines {
        // Bullet ● sits at column 2, aligning directly with the │ spine
        sink.push_at(
            format!("  {bullet} {TEXT}{line}{RESET}{timing}",),
            Hit::Block(at),
        );
    }
    let spine = spine(is_last);
    for line in rest {
        sink.push_at(format!("{spine}{MUTED}{line}{RESET}"), Hit::Block(at));
    }

    if tool.state == ToolState::Asking {
        return;
    }
    if steps::is_terminal(tool) {
        if denied || (tool.state == ToolState::Failed && steps::commands(tool).is_empty()) {
            draw_error_box(
                &failure_text(tool),
                failure_label(tool),
                open,
                &spine,
                inner,
                sink,
                at,
            );
        } else {
            draw_terminal(tool, open, is_last, inner, sink, at);
        }
    } else if steps::is_tasks(tool) {
        draw_tasks(app, tool, open, is_last, inner, sink, at);
    } else if tool.name == "ask_user_questions" {
        draw_questions_preview(tool, open, is_last, inner, sink, at);
    } else {
        let bullets = steps::bullets(tool);
        let has_body = bullets.iter().any(|b| b.body.is_some());
        let is_file_tool = matches!(
            tool.name.as_str(),
            "inspect_file" | "create_file" | "patch_file"
        );
        let is_write = matches!(tool.name.as_str(), "create_file" | "patch_file");
        for bullet in &bullets {
            let line = format_bullet_row(bullet, &spine, is_file_tool, inner + 2);
            sink.push_at(ansi::truncate(&line, inner + 2), Hit::Block(at));
            // A write that failed says so by its red bullet and its counts — no
            // word "failed", no box, none of Rune's tags (owner, 2026-10-08):
            // opened, why it failed, dim, in the words the model was told; a
            // patch that ran and found no single match still shows what it tried.
            if is_write && !bullet.ok {
                if open {
                    let rows: Vec<String> = steps::failure_reason(tool)
                        .lines()
                        .flat_map(|line| ansi::wrap(line, inner.saturating_sub(6).max(20)))
                        .collect();
                    draw_notice_rows(&rows, &spine, sink, at);
                }
                if let Some(body @ steps::Body::Patch { .. }) = &bullet.body {
                    if tool.state.finished() {
                        draw_file_body(body, &bullet.target, open, is_last, inner, sink, at);
                    }
                }
                continue;
            }
            if let Some(body) = &bullet.body {
                let can_show = match body {
                    steps::Body::Patch { .. } => tool.state.finished(),
                    steps::Body::Code(_) => tool.state.finished() && bullet.ok,
                };
                if can_show {
                    draw_file_body(body, &bullet.target, open, is_last, inner, sink, at);
                } else if let Some(err) = &bullet.error {
                    draw_error_box(err, failure_label(tool), open, &spine, inner, sink, at);
                }
            } else if let Some(err) = &bullet.error {
                draw_error_box(err, failure_label(tool), open, &spine, inner, sink, at);
            }
        }
        if matches!(tool.state, ToolState::Failed | ToolState::Stopped) {
            if bullets.is_empty() || !bullets.iter().any(|b| b.error.is_some()) {
                let err_text = failure_text(tool);
                if !err_text.trim().is_empty() {
                    draw_error_box(
                        &err_text,
                        failure_label(tool),
                        open,
                        &spine,
                        inner,
                        sink,
                        at,
                    );
                }
            }
        } else {
            let content = if tool.content.is_empty() {
                &tool.output
            } else {
                &tool.content
            };
            if !has_body && !content.trim().is_empty() && (tool.state.finished() || open) {
                if tool.name == "discover_capabilities" {
                    draw_capabilities_preview(content, open, &spine, inner, sink, at);
                } else {
                    let mcp_unwrapped = mcp_text_or_raw(content);
                    let parsed = runeengine::tools::notice::split(&mcp_unwrapped);
                    // Where Rune's notices sit: under the results of a search or
                    // a read (a cut is read after what was found), above the
                    // report of a process (the state comes first).
                    let below = matches!(
                        tool.name.as_str(),
                        "search_text" | "find_files" | "inspect_file" | "web_search" | "web_fetch"
                    );
                    let rows = notice_rows(&parsed.notices, inner.saturating_sub(6).max(20));
                    if !below {
                        draw_notice_rows(&rows, &spine, sink, at);
                    }
                    // Below the results, a notice is the preview's last rows —
                    // inside the fold, which is always the step's last row: a
                    // cut search shows its matches, and opened, ends with why
                    // it stopped.
                    let after: &[String] = if below { &rows } else { &[] };
                    let mut body = parsed.rest;
                    // The header line of a search or a read — what was looked for,
                    // how many were found, which lines of how many — is in the
                    // step's bullet; it is not drawn twice.
                    if matches!(
                        tool.name.as_str(),
                        "search_text" | "find_files" | "inspect_file"
                    ) && body.starts_with("=== ")
                    {
                        body = body.split_once('\n').map(|(_, data)| data).unwrap_or("");
                    }
                    if !body.trim().is_empty() || !after.is_empty() {
                        draw_folded_preview(body, after, open, &spine, inner, sink, at);
                    }
                }
            }
        }
    }
}

/// The plan or task list, rendered along the timeline spine with monochrome
/// geometric progression markers (○ -> ◐ -> ●) and zero duplicate content dump.
/// Limited to 8 tasks when closed, with expandable view when open.
fn draw_tasks(
    app: &App,
    tool: &Tool,
    open: bool,
    is_last: bool,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let spine = spine(is_last);
    let mut tasks = steps::tasks(tool);
    if tasks.is_empty() && at == app.blocks.len().saturating_sub(1) {
        tasks = app.tasks.clone();
    }
    let total_tasks = tasks.len();
    let max_tasks = super::viewport::preview_rows(open);
    let (rendered_tasks, cut_count) = if total_tasks > max_tasks {
        (&tasks[..max_tasks], total_tasks - max_tasks)
    } else {
        (&tasks[..], 0)
    };
    for task in rendered_tasks {
        let text = task.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let status = runeengine::tools::field_given_str(task, "status").unwrap_or("todo");
        let (glyph, colour, text_style) = match status {
            "done" => ("●", TEXT, FAINT),
            "doing" => ("◐", TEXT, TEXT),
            _ => ("○", MUTED, MUTED),
        };
        let line = format!("{spine}{colour}{glyph}{RESET} {text_style}{text}{RESET}",);
        sink.push_at(ansi::truncate(&line, inner + 2), Hit::Block(at));
    }
    if cut_count > 0 {
        sink.push_at(
            format!("{spine}{FAINT}… {cut_count} tasks cut. Click to open all{RESET}"),
            Hit::Block(at),
        );
    }
}

/// Interactive questions and answers rendered borderless along the timeline spine.
fn draw_questions_preview(
    tool: &Tool,
    open: bool,
    is_last: bool,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let spine = spine(is_last);

    let mut q_lines: Vec<(String, String)> = Vec::new();
    let parsed_reply: Option<runeengine::tools::questions::Reply> =
        serde_json::from_str(&tool.content)
            .ok()
            .or_else(|| {
                let text = tool.content.as_str();
                let after_notice = runeengine::tools::notice::split(text).rest;
                let json_part = if after_notice.len() < text.len() {
                    after_notice
                } else {
                    text.split("</user_input>\n\n").nth(1).unwrap_or(text)
                };
                serde_json::from_str(json_part.trim()).ok()
            })
            .or_else(|| {
                tool.detail
                    .as_ref()
                    .and_then(|d| serde_json::from_value(d.clone()).ok())
            });

    let dismissed = matches!(
        parsed_reply,
        Some(
            runeengine::tools::questions::Reply::Dismissed { .. }
                | runeengine::tools::questions::Reply::Cancelled
        )
    ) || tool.state == ToolState::Stopped;
    if let Some(
        runeengine::tools::questions::Reply::Answered { answers }
        | runeengine::tools::questions::Reply::Dismissed { answers },
    ) = parsed_reply
    {
        // The form on this call is the model's raw arguments: the model is
        // never allowed to name a question or an option, so there is no id
        // here to match an answer against. The engine named the form when it
        // opened it — deterministically, by position — and the answers speak
        // exactly those names. Naming the form the same way once more is what
        // lets an answer find its question, so the transcript reads the words
        // the user answered instead of the system's ids.
        let named: Vec<runeengine::tools::questions::Question> = tool
            .args
            .as_ref()
            .and_then(|args| {
                serde_json::from_value::<Vec<runeengine::tools::questions::Question>>(
                    args["questions"].clone(),
                )
                .ok()
            })
            .map(|mut questions| {
                runeengine::tools::questions::prepare(&mut questions);
                questions
            })
            .unwrap_or_default();
        for answer in &answers {
            let question = named.iter().find(|q| q.id == answer.id);
            let title = question.map(|q| q.title.as_str()).unwrap_or(&answer.id);
            let q_prompt = question.map(|q| q.question.as_str()).unwrap_or(title);
            let mut labels: Vec<String> = answer
                .selected
                .iter()
                .map(|id| {
                    question
                        .and_then(|q| q.options.iter().find(|o| &o.id == id))
                        .map(|o| o.label.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            if !answer.custom.is_empty() {
                labels.push(answer.custom.clone());
            }
            let ans_str = if labels.is_empty() {
                "— (no answer)".to_string()
            } else {
                labels.join(", ")
            };
            let q_label = if !title.is_empty() && title != q_prompt {
                format!("{title}: {q_prompt}")
            } else {
                q_prompt.to_string()
            };
            q_lines.push((q_label, ans_str));
        }
        if dismissed {
            for question in &named {
                if !answers.iter().any(|answer| answer.id == question.id) {
                    q_lines.push((
                        question.question.clone(),
                        "Stopped by user · no answer submitted".into(),
                    ));
                }
            }
        }
    } else if dismissed {
        if let Some(questions) = tool
            .args
            .as_ref()
            .and_then(|args| args["questions"].as_array())
        {
            for question in questions {
                q_lines.push((
                    question["question"].as_str().unwrap_or("").into(),
                    "Stopped by user · no answer submitted".into(),
                ));
            }
        }
    } else {
        let raw = if tool.content.is_empty() {
            &tool.output
        } else {
            &tool.content
        };
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('{') {
                continue;
            }
            if let Some((q, a)) = trimmed.split_once(':') {
                q_lines.push((q.trim().to_string(), a.trim().to_string()));
            } else {
                q_lines.push((trimmed.to_string(), String::new()));
            }
        }
    }

    if q_lines.is_empty() {
        return;
    }

    let avail_width = inner.saturating_sub(6).max(20);
    let max_rows = super::viewport::preview_rows(open);

    // Every row a question and its answer need, built first and cut afterwards.
    // A wrapped answer that runs past the budget loses *rows* without losing its
    // question, so counting only whole questions is what let a single long
    // answer disappear with nothing said about it.
    let rows_of = |q_text: &str, answer: &str| -> Vec<String> {
        let mut rows = Vec::new();
        for (i, seg) in ansi::wrap(&ansi::tame(q_text), avail_width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let indent = if i == 0 { "  " } else { "    " };
            rows.push(format!("{indent}{TEXT}{seg}{RESET}"));
        }
        if !answer.is_empty() {
            let a_str = format!("→ {answer}");
            for (i, seg) in ansi::wrap(&ansi::tame(&a_str), avail_width.saturating_sub(4))
                .into_iter()
                .enumerate()
            {
                let indent = if i == 0 { "    " } else { "      " };
                rows.push(format!("{indent}{ADDED}{seg}{RESET}"));
            }
        }
        rows
    };

    let mut visual_rows: Vec<String> = Vec::new();
    // One block per question, each opening with the blank line that separates it
    // from the one before. Building the whole list first is what makes the count
    // below trustworthy: "how many rows was this" and "how many rows are on
    // screen" are measured against the same list, so a long answer cannot vanish
    // into a gap between the two.
    let blocks: Vec<Vec<String>> = q_lines
        .iter()
        .map(|(q_text, answer)| {
            let mut rows = vec![String::new()];
            rows.extend(rows_of(q_text, answer));
            rows
        })
        .collect();
    let total_rows: usize = blocks
        .iter()
        .map(|b| b.len())
        .sum::<usize>()
        .saturating_sub(1);
    'questions: for (idx, block) in blocks.iter().enumerate() {
        for (n, row) in block.iter().enumerate() {
            if visual_rows.len() >= max_rows {
                break 'questions;
            }
            // Nothing precedes the first question, so its separator is not drawn.
            if idx == 0 && n == 0 {
                continue;
            }
            visual_rows.push(row.clone());
        }
    }
    let cut_rows = total_rows.saturating_sub(visual_rows.len());

    for line in &visual_rows {
        sink.push_at(format!("{spine}{line}"), Hit::Block(at));
    }
    if cut_rows > 0 {
        // One mark, one sentence: the rows that did not fit. A second line
        // ("1 question cut") restated the same cut in another unit and the
        // two stacked into noise.
        sink.push_at(
            format!(
                "{spine}  {FAINT}{}{RESET}",
                super::viewport::cut_lines(cut_rows)
            ),
            Hit::Block(at),
        );
    }
}

/// The payload of a create or patch call, visible while it streams and after it
/// settles. The engine's bounded diff is reused so this view cannot grow a
/// second comparison algorithm beside undo/checkpoint review.
pub(super) fn file_body_lines<'a>(body: &steps::Body, path: &'a str) -> (&'a str, Vec<String>) {
    match body {
        steps::Body::Code(text) => {
            let label = path
                .rsplit_once('.')
                .map(|(_, extension)| extension)
                .filter(|extension| !extension.is_empty())
                .unwrap_or("code");
            (label, text.lines().map(String::from).collect::<Vec<_>>())
        }
        steps::Body::Patch { before, after } => {
            let mut lines = Vec::new();
            let hunks = runeengine::checkpoint::diff::hunks(
                before,
                after,
                super::runtime_configuration().number("checkpoint.diff.max_cells") as usize,
                super::runtime_configuration().number("checkpoint.diff.context") as usize,
            );
            if hunks.is_empty() {
                lines.push(" (file unchanged)".into());
            }
            for hunk in hunks {
                for line in hunk.lines {
                    let mark = match line.op {
                        runeengine::checkpoint::diff::Op::Same => ' ',
                        runeengine::checkpoint::diff::Op::Del => '-',
                        runeengine::checkpoint::diff::Op::Add => '+',
                    };
                    lines.push(format!("{mark}{}", line.text));
                }
            }
            ("diff", lines)
        }
    }
}

fn draw_file_body(
    body: &steps::Body,
    path: &str,
    open: bool,
    is_last: bool,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let spine = spine(is_last);
    let (label, lines) = file_body_lines(body, path);
    let max_avail = inner.saturating_sub(6);
    let closed_width = max_avail.min(76);
    let box_width = if open {
        let mut max_w = ansi::width(label) + 6;
        for line in &lines {
            max_w = max_w.max(ansi::width(&ansi::tame(line)));
        }
        (max_w + 4).clamp(closed_width, max_avail)
    } else {
        closed_width
    };
    let box_inner = box_width.saturating_sub(4);
    let is_diff = label.eq_ignore_ascii_case("diff") || label.eq_ignore_ascii_case("patch");

    let max_rows = super::viewport::preview_rows(open);
    let mut visual_rows: Vec<(ansi::Ink, String)> = Vec::new();
    for line in &lines {
        let ink = if is_diff {
            match line.as_bytes().first() {
                Some(b'+') => ADDED,
                Some(b'-') => REMOVED,
                _ => MUTED,
            }
        } else {
            TEXT
        };
        let tamed = if is_diff {
            ansi::tame(line)
        } else {
            super::highlight::source_line(label, line)
        };
        if ansi::width(&tamed) > box_inner {
            let segments = ansi::wrap(&tamed, box_inner);
            for seg in segments {
                visual_rows.push((ink, seg));
            }
        } else {
            visual_rows.push((ink, tamed));
        }
    }

    let shown = visual_rows.len().min(max_rows);
    let cut_count = visual_rows.len().saturating_sub(shown);

    sink.push_at(
        format!("{spine}{}", box_top(label, box_width)),
        Hit::Block(at),
    );
    for (ink, text) in &visual_rows[..shown] {
        let line_str = format!("{ink}{text}{RESET}");
        sink.push_at(
            format!("{spine}{}", boxed(&line_str, box_inner)),
            Hit::Block(at),
        );
    }
    if cut_count > 0 {
        let cut_msg = format!("{MUTED}{}{RESET}", super::viewport::cut_lines(cut_count));
        sink.push_at(
            format!("{spine}{}", boxed(&cut_msg, box_inner)),
            Hit::Block(at),
        );
    }
    sink.push_at(format!("{spine}{}", box_bottom(box_width)), Hit::Block(at));
}

/// When a tool step fails or is denied, render the error reason inside a dedicated error box.
/// Content that was not written or patch that failed is omitted; only the error and reason are shown.
fn draw_error_box(
    summary: &str,
    label: &str,
    open: bool,
    spine: &str,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let max_avail = inner.saturating_sub(6);
    let closed_width = max_avail.min(76);
    let raw = summary.trim();
    if raw.is_empty() {
        return;
    }
    let humanized = raw.to_string();

    let box_inner = closed_width.saturating_sub(4);
    let max_rows = super::viewport::preview_rows(open);
    let mut visual_rows: Vec<String> = Vec::new();
    for raw_line in humanized.lines() {
        let tamed = ansi::tame(raw_line);
        visual_rows.extend(ansi::wrap(&tamed, box_inner));
    }

    let box_width = if open {
        let mut max_w = ansi::width(label) + 6;
        for line in &visual_rows {
            max_w = max_w.max(ansi::width(&ansi::tame(line)));
        }
        (max_w + 4).clamp(closed_width, max_avail)
    } else {
        closed_width
    };
    let box_inner = box_width.saturating_sub(4);

    sink.push_at(
        format!("{spine}{}", box_top(label, box_width)),
        Hit::Block(at),
    );

    let shown = visual_rows.len().min(max_rows);
    let cut_count = visual_rows.len().saturating_sub(shown);

    for text in &visual_rows[..shown] {
        let line_str = format!("{REMOVED}{text}{RESET}");
        sink.push_at(
            format!("{spine}{}", boxed(&line_str, box_inner)),
            Hit::Block(at),
        );
    }
    if cut_count > 0 {
        let cut_msg = format!("{MUTED}{}{RESET}", super::viewport::cut_lines(cut_count));
        sink.push_at(
            format!("{spine}{}", boxed(&cut_msg, box_inner)),
            Hit::Block(at),
        );
    }

    sink.push_at(format!("{spine}{}", box_bottom(box_width)), Hit::Block(at));
}

/// Strip duplicated command lines from the beginning of output lines.
/// Handles single-line commands, multi-line commands, with or without leading '$ '.
fn strip_leading_command_lines(lines: &mut Vec<&str>, cmds: &[String]) {
    for cmd in cmds {
        let cmd_lines: Vec<&str> = cmd
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if cmd_lines.is_empty() {
            continue;
        }
        let mut matches = true;
        let mut count = 0;
        for (i, expected) in cmd_lines.iter().enumerate() {
            if i >= lines.len() {
                matches = false;
                break;
            }
            let line_trimmed = lines[i].trim();
            let matches_line = if i == 0 {
                line_trimmed == *expected
                    || line_trimmed == format!("$ {expected}")
                    || line_trimmed.strip_prefix("$ ").map(str::trim) == Some(*expected)
            } else {
                line_trimmed == *expected
                    || line_trimmed.strip_prefix("$ ").map(str::trim) == Some(*expected)
            };
            if !matches_line {
                matches = false;
                break;
            }
            count += 1;
        }
        if matches && count > 0 {
            lines.drain(..count);
            while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.remove(0);
            }
            return;
        }
    }
    // Fallback: if first line starts with "$ " matching any cmd prefix
    if let Some(first) = lines.first() {
        let trimmed = first.trim();
        if cmds.iter().any(|c| {
            let c_trimmed = c.trim();
            trimmed == c_trimmed
                || trimmed == format!("$ {c_trimmed}")
                || trimmed.strip_prefix("$ ").map(str::trim) == Some(c_trimmed)
                || c.lines().next().map(str::trim) == Some(trimmed)
                || trimmed
                    .strip_prefix("$ ")
                    .and_then(|s| {
                        c.lines()
                            .next()
                            .map(str::trim)
                            .map(|first_cmd| s.trim() == first_cmd)
                    })
                    .unwrap_or(false)
        }) {
            lines.remove(0);
            while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.remove(0);
            }
        }
    }
}

/// What a row of raw tool output *is*, as far as colour is concerned.
///
/// Split out because the terminal box and the borderless preview ask the same
/// question of the same bytes and were carrying two copies of the answer.
enum OutputRow {
    /// The truncation envelope a tool printed around its own output.
    Divider,
    /// A machine-inserted aside: a system notice, a cut notice, a tag block.
    Notice,
    /// Anything the command itself printed.
    Text,
}

/// Classify one line of tool output. **Not** the tool's exit state.
///
/// # Why a line's own shape decides its colour
///
/// A call that failed, was denied, or timed out still *printed things*, and
/// those things are the only record of what went wrong. Painting a whole
/// transcript red because the step ended badly throws away the evidence the
/// reader opened the step to find, and it makes a wall of red mean nothing —
/// every failure looks identical whether it was a schema rejection or a
/// compiler's last twenty lines. Failure is therefore said once, by the bullet
/// and by the `error`/`denied` box, and the output itself stays what it always
/// was: text.
fn classify_output_row(line: &str) -> OutputRow {
    const DIVIDERS: [&str; 6] = [
        "<===",
        "========",
        "<preview_head",
        "</preview_head>",
        "<preview_tail",
        "</preview_tail>",
    ];
    // The only envelope Rune writes into a tool's output (see
    // `runeengine::tools::notice`); everything else in it is the tool's own.
    const NOTICES: [&str; 2] = ["<tool_notice>", "</tool_notice>"];
    let t = line.trim();
    if DIVIDERS.iter().any(|d| t.starts_with(d) && t.ends_with(d)) {
        OutputRow::Divider
    } else if NOTICES.iter().any(|n| t.starts_with(n)) {
        OutputRow::Notice
    } else {
        OutputRow::Text
    }
}

/// The prefix one row of raw tool output is opened with: its ink, plus weight
/// for the machine's own envelope.
///
/// # Why there is no accent and no faint in here
///
/// `FAINT` and `ACCENT` are *theme* roles — they are violet on one theme, cyan
/// on the next, amber on the one after — and they used to be the default for
/// the `<system_notice>` block and the truncation envelope a tool prints around
/// its own output. That made the harness's own housekeeping the most colourful
/// thing in a tool step, and it changed colour with the theme while the words it
/// wrapped did not.
///
/// Everything a tool printed is therefore one neutral ink. What is left to
/// separate the envelope from the words is **weight**, which costs no hue: the
/// `<=== … ===>` and `========` rules are bold, and a notice is not. The box
/// around the whole step is still `RULE` — that is Rune's furniture, drawn by
/// Rune, and not a word the tool said.
fn output_prefix(row: OutputRow) -> String {
    match row {
        OutputRow::Divider => format!("{MUTED}{BOLD}"),
        OutputRow::Notice | OutputRow::Text => MUTED.to_string(),
    }
}

/// The left margin of a step's body: the turn's spine running on to the next
/// step, or blank under the last one. One definition for every step kind (D9).
fn spine(is_last: bool) -> String {
    if is_last {
        "     ".to_string()
    } else {
        format!("  {RULE}│{RESET}   ")
    }
}

/// The `Key: value` lines of a report with the colons in one column, long
/// values (a path) broken after a `/` and carried under the value column. A line
/// that is not `Key: value` is kept as it is. Only the drawing is aligned: what
/// the model was sent stays compact.
fn report_rows(body: &str, width: usize) -> Vec<String> {
    let parsed: Vec<(Option<(String, String)>, &str)> = body
        .lines()
        .map(|line| {
            let field = line.split_once(':').and_then(|(key, value)| {
                let key = key.trim();
                (!key.is_empty() && key.split_whitespace().count() <= 2)
                    .then(|| (key.to_string(), value.trim().to_string()))
            });
            (field, line)
        })
        .collect();
    let key_width = parsed
        .iter()
        .filter_map(|(field, _)| field.as_ref().map(|(key, _)| key.chars().count()))
        .max()
        .unwrap_or(0);
    let mut rows = Vec::new();
    for (field, line) in parsed {
        match field {
            Some((key, value)) => {
                let head = format!("{:<w$} ", format!("{key}:"), w = key_width + 1);
                let room = width.saturating_sub(ansi::width(&head)).max(8);
                let mut parts = break_value(&value, room).into_iter();
                rows.push(format!("{head}{}", parts.next().unwrap_or_default()));
                let pad = " ".repeat(ansi::width(&head));
                for part in parts {
                    rows.push(format!("{pad}{part}"));
                }
            }
            None => rows.extend(ansi::wrap(line, width)),
        }
    }
    rows
}

/// `value` in pieces of at most `room` columns, cut after a `/` (a path) or at a
/// space when there is one, and mid-word only when there is nothing better.
fn break_value(value: &str, room: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while ansi::width(rest) > room {
        let mut cut = 0;
        let mut used = 0;
        for (at, c) in rest.char_indices() {
            let w = ansi::width(&c.to_string());
            if used + w > room && cut > 0 {
                break;
            }
            used += w;
            cut = at + c.len_utf8();
        }
        let piece = &rest[..cut];
        let at = piece
            .rfind('/')
            .map(|i| i + 1)
            .or_else(|| piece.rfind(' ').map(|i| i + 1))
            .filter(|&i| i > 0)
            .unwrap_or(cut);
        out.push(rest[..at].trim_end().to_string());
        rest = rest[at..].trim_start();
    }
    out.push(rest.to_string());
    out
}

/// The notices of a result as they were sent, one row each: a sentence is a
/// single `<tool_notice>…</tool_notice>` row; a report is its two tags with the
/// aligned `Key: value` rows between them. Wrapped to `width`.
fn notice_rows(notices: &[&str], width: usize) -> Vec<String> {
    use runeengine::tools::notice::{is_report, CLOSE, OPEN};
    let mut rows = Vec::new();
    for body in notices {
        if is_report(body) {
            rows.push(OPEN.to_string());
            rows.extend(report_rows(body, width));
            rows.push(CLOSE.to_string());
        } else {
            rows.extend(ansi::wrap(&format!("{OPEN}{body}{CLOSE}"), width));
        }
    }
    rows
}

/// Draw notice rows along the step's spine, in the one neutral ink the harness's
/// own words get (weight, never a theme hue — see [`output_prefix`]).
fn draw_notice_rows(rows: &[String], spine: &str, sink: &mut Sink, at: usize) {
    for row in rows {
        sink.push_at(format!("{spine}{MUTED}{row}{RESET}"), Hit::Block(at));
    }
}

fn failure_text(tool: &Tool) -> String {
    steps::failure_text(tool)
}

/// The title of an error box: `denied` when a person or a rule refused the
/// call, `error` for everything else.
fn failure_label(tool: &Tool) -> &'static str {
    let status = tool
        .detail
        .as_ref()
        .and_then(|detail| detail["status"].as_str());
    if status == Some("denied") {
        "denied"
    } else {
        "error"
    }
}

/// A terminal step is its transcript rendered in a clean container box.
/// When closed, it uses a fixed uniform width (76 cols) and truncated preview.
/// When open, it sizes relative to content, renders all output, and wraps long lines.
fn draw_terminal(tool: &Tool, open: bool, is_last: bool, inner: usize, sink: &mut Sink, at: usize) {
    let spine = spine(is_last);
    let cmds = steps::commands(tool);
    let body = if tool.state.finished() && !tool.content.is_empty() {
        &tool.content
    } else if !tool.output.is_empty() {
        &tool.output
    } else if !tool.content.is_empty() {
        &tool.content
    } else {
        &tool.summary
    };

    // The notices at the head of the result are Rune's word about the call —
    // an exit code, how the command was stopped, a cut — and they are drawn as
    // they were sent, tags and all, first in the output part of the box: the
    // reader sees what the model was told, and which words are Rune's and which
    // the command's. Rune writes none for a command that succeeded and printed
    // its result, so there is nothing to hide: the output is what is folded and
    // tidied, never the notices.
    let parsed = runeengine::tools::notice::split(body);
    let body = parsed.rest;

    // A bottom cut marker always describes an unseen SUFFIX. The live parser
    // retains its prefix even after tail scrollback reaches its byte limit.
    let (cleaned, after) = if !open && !tool.state.finished() {
        if let Some(screen) = &tool.terminal {
            let (head, total) = screen.head_lines(super::viewport::MAIN_PREVIEW_ROWS);
            let after = total.saturating_sub(head.len());
            (
                head.into_iter()
                    .map(|line| ansi::tame(&line))
                    .collect::<Vec<_>>(),
                after,
            )
        } else {
            (body.lines().map(ansi::tame).collect(), 0)
        }
    } else {
        (body.lines().map(ansi::tame).collect(), 0)
    };
    let mut output_lines: Vec<&str> = cleaned.iter().map(String::as_str).collect();
    strip_leading_command_lines(&mut output_lines, &cmds);
    while output_lines
        .first()
        .map(|l| l.trim().is_empty())
        .unwrap_or(false)
    {
        output_lines.remove(0);
    }
    while output_lines
        .last()
        .map(|l| l.trim().is_empty())
        .unwrap_or(false)
    {
        output_lines.pop();
    }

    let max_avail = inner.saturating_sub(6);
    let closed_width = max_avail.min(76);
    let notice_lines = notice_rows(&parsed.notices, closed_width.saturating_sub(4));
    let box_width = if open {
        let mut max_content_w = "Bash".len() + 6;
        for cmd in &cmds {
            for line in cmd.lines() {
                max_content_w =
                    max_content_w.max(ansi::width(&ansi::tame(line.trim_end_matches('\r'))) + 3);
            }
        }
        for line in output_lines
            .iter()
            .copied()
            .chain(notice_lines.iter().map(String::as_str))
        {
            max_content_w = max_content_w.max(ansi::width(line));
        }
        (max_content_w + 4).clamp(closed_width, max_avail)
    } else {
        closed_width
    };
    let box_inner = box_width.saturating_sub(4);

    sink.push_at(
        format!("{spine}{}", box_top("Bash", box_width)),
        Hit::Block(at),
    );

    // Open All reads the original commands; a non-open box has one header row.
    if open {
        for cmd in &cmds {
            let cmd_lines = super::highlight::source_rows(
                "bash",
                cmd.trim_end_matches('\r'),
                box_inner.saturating_sub(2),
            );
            if cmd_lines.is_empty() {
                let cmd_str = format!("{ACCENT}${RESET} ");
                sink.push_at(
                    format!("{spine}{}", boxed(&cmd_str, box_inner)),
                    Hit::Block(at),
                );
            } else {
                for (i, line) in cmd_lines.into_iter().enumerate() {
                    let prefix = if i == 0 {
                        format!("{ACCENT}${RESET} ")
                    } else {
                        "  ".to_string()
                    };
                    let cmd_str = format!("{prefix}{TEXT}{line}{RESET}");
                    sink.push_at(
                        format!("{spine}{}", boxed(&cmd_str, box_inner)),
                        Hit::Block(at),
                    );
                }
            }
        }
    } else if !cmds.is_empty() {
        let flattened = cmds
            .iter()
            .map(|cmd| {
                cmd.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" && ");
        let highlighted = super::highlight::source_line("bash", &flattened);
        let truncated = ansi::truncate(&highlighted, box_inner.saturating_sub(2));
        let cmd_str = format!("{ACCENT}${RESET} {TEXT}{truncated}{RESET}");
        sink.push_at(
            format!("{spine}{}", boxed(&cmd_str, box_inner)),
            Hit::Block(at),
        );
    }

    let live = !tool.state.finished();
    let live_rows = if live {
        tool.terminal.as_ref().map_or(0, |screen| {
            screen
                .occupied_rows_peak
                .min(super::viewport::MAIN_PREVIEW_ROWS)
        })
    } else {
        0
    };
    let has_output = !notice_lines.is_empty() || !output_lines.is_empty() || live_rows > 0;
    // No output yet means command only: no divider, no empty output box.
    // Once content arrives, retain only the live rows it actually occupied so
    // a temporary spinner erase does not make the border flicker.
    if !cmds.is_empty() && has_output {
        sink.push_at(format!("{spine}{}", box_divider(box_width)), Hit::Block(at));
    }

    let notices: Vec<String> = notice_lines
        .iter()
        .map(|row| format!("{MUTED}{row}{RESET}"))
        .collect();
    let output: Vec<String> = output_lines
        .iter()
        .flat_map(|line| {
            let prefix = output_prefix(classify_output_row(line));
            if open {
                ansi::wrap(line, box_inner)
                    .into_iter()
                    .map(|row| format!("{prefix}{row}{RESET}"))
                    .collect::<Vec<_>>()
            } else {
                vec![format!(
                    "{prefix}{}{RESET}",
                    ansi::truncate(line, box_inner)
                )]
            }
        })
        .collect();
    if open {
        for row in notices.iter().chain(&output) {
            sink.push_at(format!("{spine}{}", boxed(row, box_inner)), Hit::Block(at));
        }
    } else {
        let (preview, omitted) = super::viewport::terminal_preview(&notices, &output);
        for row in &preview {
            sink.push_at(format!("{spine}{}", boxed(row, box_inner)), Hit::Block(at));
        }
        for _ in preview.len()..live_rows {
            sink.push_at(format!("{spine}{}", boxed("", box_inner)), Hit::Block(at));
        }
        let omitted = omitted.saturating_add(after);
        if omitted > 0 {
            let footer = format!("{FAINT}{}{RESET}", super::viewport::cut_lines(omitted));
            sink.push_at(
                format!("{spine}{}", boxed(&footer, box_inner)),
                Hit::Block(at),
            );
        }
    }
    sink.push_at(format!("{spine}{}", box_bottom(box_width)), Hit::Block(at));
}

// ─── The sidebar ──────────────────────────────────────────────────────────────

/// What a session is costing, in the four numbers that answer it.
///
/// It held the session id, the model, the mode and the sandbox as well. All four
/// are on screen elsewhere or are facts about the engine rather than about this
/// conversation, and a panel that repeats the status bar is a panel nobody
/// reads. What is left is what only this panel can say: how much of the window
/// is gone, how much came back, how much of the prompt was cached, and what it
/// has cost so far.
/// # What it is given, and what it must never exceed
///
/// `rows` is how many of the panel the transcript has left after the footer.
/// The panel is cut here rather than in [`compose`] because the plan now
/// scrolls, and something that scrolls has to know its own size: the same
/// render that is cut is the one that has to decide there is more to scroll to.
///
/// Every row this returns is measured against the panel, so the composition
/// downstream can take the first `rows` of it knowing there is nothing after.
pub fn sidebar(app: &mut App, width: usize, rows: usize) -> Vec<String> {
    let stats = stats(app, width);
    let stats_rows = stats.len();
    let mut out = stats;
    out.extend(plan(app, width, rows.saturating_sub(stats_rows)));
    out
}

/// The numbers a reader glances at: how long, how much window, what it cost.
///
/// Kept whole and kept first. These are fixed-height and the reader is not
/// asking to have them pushed off the screen by a long plan — they are the
/// panel's one glance, and the plan is the thing being read.
fn stats(app: &App, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let row = |out: &mut Vec<String>, key: &str, value: String| {
        out.push(format!(
            "{MUTED}{key:<15}{} {TEXT}{}{}",
            ansi::RESET,
            ansi::truncate(&value, width.saturating_sub(16)),
            ansi::RESET
        ));
    };

    out.push(panels::heading("SESSIONS", width));

    let (title_str, started_str, activity_str) = if app.turns == 0 && app.steps == 0 {
        (
            app.title.as_deref().unwrap_or("-").to_string(),
            "-".to_string(),
            "-".to_string(),
        )
    } else {
        let total_secs = app.current_active_seconds();
        let dur_str = format_duration(total_secs);
        let step_label = if app.steps == 1 { "step" } else { "steps" };
        let turn_label = if app.turns == 1 { "turn" } else { "turns" };
        (
            app.title.as_deref().unwrap_or("New Session").to_string(),
            format_time_hhmm(app.started_at),
            format!(
                "{dur_str} · {} {step_label} · {} {turn_label}",
                app.steps, app.turns
            ),
        )
    };
    row(&mut out, "Title", title_str);
    row(&mut out, "Started", started_str);
    row(&mut out, "Activity", activity_str);

    let used = if app.context_used > 0 {
        app.context_used
    } else {
        app.input_tokens
    };
    let context_str = if app.context > 0 {
        format!("{} / {}", thousands(used), thousands(app.context))
    } else {
        format!("{} / unknown", thousands(used))
    };
    row(&mut out, "Context Window", context_str);

    let tok_in = if app.tok_in > 0 {
        app.tok_in
    } else {
        app.input_tokens
    };
    row(&mut out, "TokIn", thousands(tok_in));

    let tok_out = if app.tok_out > 0 {
        app.tok_out
    } else {
        app.output_tokens
    };
    row(&mut out, "TokOut", thousands(tok_out));

    let cache_in = if app.cache_in > 0 {
        app.cache_in
    } else {
        app.cached_tokens
    };
    row(&mut out, "CacheIn", thousands(cache_in));

    match app.spend() {
        Some(usd) => row(&mut out, "Spend", format!("${usd:.4}")),
        None => row(&mut out, "Spend", "not priced".to_string()),
    }
    out
}

/// The plan, in the column beside the work it describes.
///
/// It was taken out of this panel on the grounds that `ctrl+t` already had it.
/// That was wrong in a way only visible in use: `ctrl+t` *replaces* the
/// transcript, and a plan is the one thing a person wants to read **while**
/// watching the steps go past it.
///
/// # The panel was lying about being complete
///
/// This used to draw every item, wrap each one, and hand the result to
/// `compose` — which keeps only as many rows as the terminal has. A plan longer
/// than the screen was therefore cut, and *nothing on screen said so*: no
/// `+N more`, no `↑`, no marker. A 30-item plan in a 24-row terminal showed nine
/// items and gave the reader no way to know twenty-one existed, and no way to
/// reach them. The unit test that says "the plan must not be cut" called
/// `sidebar()` directly and never went through `compose`, so it asserted that a
/// function it called was correct and proved nothing about the screen.
///
/// The fix is not to make the panel taller. It is to be honest and to let the
/// reader move: the plan **scrolls within the panel**, the heading stays the
/// work count (`PLAN done/total` — a second number under it said the same
/// list twice), and the two ends are marked while there is more to see.
/// An item still wraps to as many rows as its text needs — nothing is clipped
/// *within* an item — because the fault was never that a long item was cut, it
/// was that a long *list* vanished without a word.
///
/// `SESSIONS` above stays pinned and does not scroll: it is a fixed set of
/// numbers, and a reader glancing at the context window is not asking to have
/// it pushed off by a long plan.
///
/// FILE CHANGE lives here too, below the plan, in the **same** scroll window —
/// the owner's decision: one area, one wheel, no per-section size rules. Its
/// heading travels **with its section** instead of being pinned: pinned, it
/// sat glued under the PLAN heading while its own rows ran off the bottom, and
/// the two lists read as one merged blob — the owner's call after seeing that
/// frame. The plan's heading stays pinned, because `done/total` is the number
/// a reader glances for from anywhere. The rows beneath the FILE CHANGE
/// heading are the engine's own tally — one per path the session has ever
/// touched, measured against the file's state before the session first
/// reached it — so the panel repeats no arithmetic a second renderer could get
/// differently.
fn plan(app: &mut App, width: usize, rows: usize) -> Vec<String> {
    let has_plan = !app.tasks.is_empty();
    let has_files = !app.file_changes.is_empty();
    if !has_plan && !has_files {
        app.reset_plan_scroll();
        app.plan_max = 0;
        return Vec::new();
    }
    let total = app.tasks.len();
    let done = app.tasks.iter().filter(|t| t["status"] == "done").count();

    // Every item's rows, kept beside the item each belongs to. The pairing is
    // the whole point of the anchor: it is what lets the next frame find item
    // 20 again after the plan has grown and its heading has changed. FILE
    // CHANGE continues the numbering below the plan: its heading is one item,
    // and its rows — append-only too, first-touch order, one per path, whatever
    // the session does to the file — continue after it, so an anchor number
    // keeps meaning while either section grows.
    let mut body: Vec<(usize, Vec<String>)> = app
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (i, panels::task_lines(t, width)))
        .collect();
    let file_base = total;
    if has_files {
        body.push((
            file_base,
            vec![
                String::new(),
                panels::heading(&file_change_heading(app), width),
            ],
        ));
        for (i, row) in file_change_rows(app, width).into_iter().enumerate() {
            body.push((file_base + 1 + i, vec![row]));
        }
    }
    let body_rows: usize = body.iter().map(|(_, r)| r.len()).sum();

    // Pinned chrome: the plan's heading — with the blank line that frames it —
    // stays where a reader can always find it. The FILE CHANGE heading is not
    // chrome: it scrolls with its own section, so it is counted in the body
    // above rather than reserved here.
    let chrome = 2 * usize::from(has_plan);
    let room = rows.saturating_sub(chrome);
    let max = max_plan_scroll(body_rows, room);
    let mut scrolled = app.plan_scroll.min(max);

    // Where the window is. Three cases, and the reader is in one of them:
    //
    // * **following** — no anchor, and `plan_scroll` is at the far end. The plan
    //   is being carried out, so the newest rows are what matters and the panel
    //   goes there on its own.
    // * **deliberately moved** — no anchor, because a scroll cleared it. The
    //   number in `plan_scroll` is the reader's own and is used exactly.
    // * **moved by the work** — an anchor is set. The plan shifted underneath,
    //   so the stored item is the better answer than the stored row.
    //
    // `plan_scroll` counts rows hidden from the top, so the far end is `max`.
    if let Some(anchor) = app.plan_anchor {
        if let Some(at) = body.iter().position(|(i, _)| *i == anchor.item) {
            let offset: usize = body[..at].iter().map(|(_, r)| r.len()).sum();
            // The anchor names the absolute row the window opened at: every
            // row before the item, **plus** how far into the item the window
            // had got. Subtracting the depth instead placed the window above
            // where the reader was by twice that depth — invisible on the
            // single-row items the old tests anchored (row was always 0), and
            // a constant upward yank on a wrapped task line, which is what
            // made the panel spring back to the top mid-stream no matter how
            // far down the reader wheeled.
            scrolled = offset.saturating_add(anchor.row).min(max);
        }
    } else if app.plan_scroll >= max {
        // At the newest rows, or past them: follow the end as it grows.
        scrolled = max;
    }
    scrolled = scrolled.min(max);

    // The window is sized so the panel fills the rows it was given *exactly*.
    // This is the arithmetic the old panel got wrong: it drew everything and
    // let `compose` keep what fitted, so the tail vanished with nothing to say
    // it had. Here the number of items comes from the space, not from the list.
    //
    // The order is the whole correctness argument. The top marker spends its
    // row *inside* the room, so the body's capacity is the room minus that one
    // row — and "more below" is tested against that capacity, never against a
    // marker-inclusive allowance: a test on the wrong number answers a
    // question nobody asked, and at the far end it once passed while the last
    // row was still cut. Only after the bottom question is answered does the
    // bottom marker charge its own row out of the drawing allowance. A marker
    // that says "more below" when the list has just ended — or when the
    // wheel, already at its limit, cannot reach what the marker names — is
    // the same lie this scroll exists to stop.
    let hide_above = scrolled > 0;
    let body_capacity = if hide_above {
        room.saturating_sub(1)
    } else {
        room
    };
    let hide_below = scrolled + body_capacity < body_rows;
    let budget = if hide_below {
        room.saturating_sub(1)
    } else {
        room
    };
    let budget = budget.max(1);

    // The plan's heading is the work count alone. Which slice is in front of
    // you is the marks' question — `↑/↓ N rows …` name the rows that are
    // hidden — and a second range under `done/total` read the same list twice.
    let plan_rows: usize = body
        .iter()
        .filter(|(item, _)| *item < file_base)
        .map(|(_, lines)| lines.len())
        .sum();
    let files_active = has_plan && has_files && scrolled >= plan_rows;
    let mut out: Vec<String> = Vec::new();
    if has_plan {
        out.push(String::new());
        let heading = if files_active {
            file_change_heading(app)
        } else {
            format!("PLAN {done}/{total}")
        };
        out.push(panels::heading(&heading, width));
    }

    let mut skipped = 0usize;
    let mut shown = 0usize;
    let mut body_shown = 0usize;
    for (item, lines) in &body {
        for (row, line) in lines.iter().enumerate() {
            if shown >= budget {
                break;
            }
            if skipped < scrolled {
                skipped += 1;
                continue;
            }
            if shown == 0 {
                if hide_above {
                    out.push(format!(
                        "{MUTED}↑ {scrolled} rows above · wheel here{}",
                        ansi::RESET
                    ));
                    shown += 1;
                }
                app.plan_anchor = Some(PlanAnchor { item: *item, row });
            }
            if !(files_active && *item == file_base && row == 1) {
                out.push(line.clone());
                shown += 1;
            }
            body_shown += 1;
        }
        if shown >= budget {
            break;
        }
    }
    if hide_below {
        // `skipped` is the index of the first row actually drawn (it counted
        // every row above the window on its way past); `body_shown` counts
        // body rows only, because the top marker spent a row of the budget
        // without being a row of the list. What is below is what is left.
        let left = body_rows.saturating_sub(skipped + body_shown);
        out.push(format!(
            "{MUTED}↓ {left} rows below · wheel here{}",
            ansi::RESET
        ));
    }
    app.plan_scroll = scrolled;
    app.plan_max = max;
    out
}

/// How far the panel's list can be scrolled: everything that will not fit.
///
/// The list is the plan's items and the FILE CHANGE rows below them — one
/// window over both. Never negative, and never more than the list is long.
///
/// The far end always carries the `↑ rows above` marker — `max > 0` implies
/// `scrolled > 0` implies the marker — so its row is charged here rather than
/// discovered at the bottom. Without the charge the limit fell short by the
/// two markers, and a plan's last rows stayed hidden forever behind a
/// `↓ 2 rows below` that no amount of wheeling could reach.
fn max_plan_scroll(body_rows: usize, room: usize) -> usize {
    body_rows.saturating_sub(room.saturating_sub(1))
}

/// The FILE CHANGE rows: one per path the session has ever touched, the path
/// at the left and its session-long counts at the right. The layout is the
/// one row renderer the theme preview shares — see [`panels::file_change_row`].
fn file_change_rows(app: &App, width: usize) -> Vec<String> {
    let counts: Vec<String> = app.file_changes.iter().map(file_change_counts).collect();
    let counts_w = panels::file_change_counts_w(&counts);
    let path_w = panels::file_change_path_w(width, counts_w);
    app.file_changes
        .iter()
        .zip(&counts)
        .map(|(c, counts)| {
            panels::file_change_row(path_w, counts_w, &file_change_path(c, path_w), counts)
        })
        .collect()
}

/// The counts half of one FILE CHANGE row, painted the way the transcript
/// paints its own diff arithmetic — arrived green, left red. Every row counts,
/// whatever happened to it: a file the session put back reads `+0 −0`, a
/// binary reads `+0 −0` (line arithmetic has nothing to say about bytes), and
/// a diff the engine refused carries the write ledger's own tally. No words —
/// the numbers are the whole row.
pub(super) fn file_change_counts(c: &serde_json::Value) -> String {
    let added = c["added"].as_u64().unwrap_or(0);
    let removed = c["removed"].as_u64().unwrap_or(0);
    match (added > 0, removed > 0) {
        (true, true) => format!("{ADDED}+{added}{RESET} {REMOVED}−{removed}{RESET}"),
        (true, false) => format!("{ADDED}+{added}{RESET}"),
        (false, true) => format!("{REMOVED}−{removed}{RESET}"),
        (false, false) => format!("{ADDED}+0{RESET} {REMOVED}−0{RESET}"),
    }
}

/// The path half of one FILE CHANGE row — project-relative as the jail
/// displays it, absolute when the file lives outside the project, truncated
/// the way the transcript truncates a path: the filename survives whole, the
/// middle of the directory chain becomes `…`, and only a filename too long on
/// its own is cut in the middle. A suffix cut (`28-RuneAsk…`) would hide the
/// one part that names the change. A deleted file needs no extra word: its
/// all-minus counts (`−N`) already say everything.
fn file_change_path(c: &serde_json::Value, path_w: usize) -> String {
    let path = c["path"].as_str().unwrap_or("?");
    format!("{TEXT}{}{RESET}", truncate_path(path, path_w))
}

/// The FILES CHANGED heading: the count leads (`14 FILES CHANGED`) in the plain
/// text ink — a number is content, not decoration — then the label takes the
/// heading's accent back, and the session's line totals follow after a muted
/// bullet. The numbers the rows beneath it add up to; a total with nothing to
/// show leaves its half out, the way the rows do.
fn file_change_heading(app: &App) -> String {
    let (added, removed) = app.file_changes.iter().fold((0u64, 0u64), |(a, r), c| {
        (
            a + c["added"].as_u64().unwrap_or(0),
            r + c["removed"].as_u64().unwrap_or(0),
        )
    });
    let mut text = format!(
        "{TEXT}{}{RESET}{ACCENT}{BOLD} FILES CHANGED",
        app.file_changes.len()
    );
    let mut counts = String::new();
    if added > 0 {
        counts.push_str(&format!("{ADDED}+{added}{RESET}"));
    }
    if removed > 0 {
        if !counts.is_empty() {
            counts.push(' ');
        }
        counts.push_str(&format!("{REMOVED}−{removed}{RESET}"));
    }
    if !counts.is_empty() {
        text.push_str(&format!(" {MUTED}•{RESET} {counts}"));
    }
    text
}

/// `131072` → `131,072`. A raw token count is a number nobody reads.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format duration cleanly: `45s`, `6m 22s`, `1h 05m`.
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let mins = secs / 60;
        let rem = secs % 60;
        format!("{mins}m {rem:02}s")
    } else {
        let hrs = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hrs}h {mins:02}m")
    }
}

/// Format timestamp (epoch in milliseconds) to local time `HH:MM`.
pub fn format_time_hhmm(ms: i64) -> String {
    if ms <= 0 {
        return "-".to_string();
    }
    let secs = ms / 1000;
    if let Some(t) = runeengine::platform::local_time(secs) {
        return format!("{:02}:{:02}", t.hour, t.minute);
    }
    let day_secs = (secs % 86400 + 86400) % 86400;
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}

// ─── The footer ───────────────────────────────────────────────────────────────

/// The bottom of the screen: the composer, or the permission question in its
/// place, and under it the one line that says whether anything is happening.
fn footer(app: &mut App, rows: usize, cols: usize) -> (Sink, Option<(usize, usize)>) {
    let (mut out, cursor) = if let Some(form) = &mut app.mcp_input {
        super::mcp_input::render(form, cols, rows.saturating_sub(3))
    } else if let Some(state) = &mut app.questions {
        super::questions::render(state, cols, rows.saturating_sub(3))
    } else {
        match &app.pending {
            // A question about permission outranks every screen, including the one
            // the user opened themselves: nothing else can be done until it is
            // answered, so nothing else is offered.
            Some(ask) => (
                super::permission::render(
                    ask,
                    app.ask_choice,
                    cols,
                    rows,
                    app.blocks
                        .iter()
                        .filter_map(|block| match block {
                            Block::Tool(tool) => Some(tool.as_ref()),
                            _ => None,
                        })
                        .find(|tool| {
                            tool.ask
                                .as_ref()
                                .is_some_and(|pending| pending.req == ask.req)
                        }),
                    &mut app.permission,
                ),
                None,
            ),
            // The settings screen takes the composer with it. A text field under a
            // screen of controls is an invitation to type a question into the one
            // place that is not listening for one — and a value typed there lands
            // in the row it belongs to instead. See [`super::settings`].
            None if app.panel == Panel::Settings => (Sink::default(), None),
            // The footer may never eat the conversation: on a short terminal a
            // growing composer is a screen with nothing left to read.
            None => composer(app, cols, rows.saturating_sub(3)),
        }
    };
    out.push(state_line(app, cols));
    (out, cursor)
}

/// A fresh permission viewport for layout contract tests. Live rendering uses
/// the window's persistent state in `permission`, not a second card renderer.
#[cfg(test)]
fn permission_card(
    ask: &super::blocks::Ask,
    choice: usize,
    cols: usize,
    rows: usize,
    tool: Option<&Tool>,
) -> Sink {
    super::permission::render(
        ask,
        choice,
        cols,
        rows,
        tool,
        &mut super::permission::State::default(),
    )
}

/// The answers, side by side, with the chosen one standing out of the row.
///
/// The selected answer is drawn in reverse video rather than in a brighter
/// colour: through a translucent window a change of hue is the first thing to
/// go, and "which one am I about to press" is not a question anyone should have
/// to squint at.
#[cfg(test)]
fn answers_row(ask: &super::blocks::Ask, choice: usize) -> String {
    super::permission::answers_row(ask, choice)
}

/// The top edge of a box, with its name set into it.
pub(super) fn box_top(label: &str, cols: usize) -> String {
    edge_top(&RULE.to_string(), &TEXT.to_string(), label, cols)
}

/// One row inside a box, padded so the right edge is straight whatever is in it.
pub(super) fn boxed(body: &str, inner: usize) -> String {
    edge_row(&RULE.to_string(), body, inner)
}

/// The box geometry, once, for the live palette and a preview's alike (D7).
///
/// `╭─ ` plus the space after the name plus `╮` is five columns. Counting six is
/// how a labelled edge ends one column short of every other row in its own box.
fn edge_top(rule: &str, text: &str, label: &str, cols: usize) -> String {
    let rest = cols.saturating_sub(ansi::width(label) + 5);
    format!("{rule}╭─ {text}{label}{rule} {}╮{RESET}", "─".repeat(rest))
}

fn edge_row(rule: &str, body: &str, inner: usize) -> String {
    let safe_body = if body.contains('\n') || body.contains('\r') {
        body.replace(['\r', '\n'], " ")
    } else {
        body.to_string()
    };
    format!(
        "{rule}│{RESET} {}{rule} │{RESET}",
        ansi::pad(&safe_body, inner)
    )
}

fn edge_bottom(rule: &str, cols: usize) -> String {
    format!("{rule}╰{}╯{RESET}", "─".repeat(cols.saturating_sub(2)))
}

pub(super) fn box_divider(cols: usize) -> String {
    format!(
        "{RULE}├{}┤{}",
        "─".repeat(cols.saturating_sub(2)),
        ansi::RESET
    )
}

pub(super) fn box_bottom(cols: usize) -> String {
    edge_bottom(&RULE.to_string(), cols)
}

/// The caret. One small mark, in the brightest ink there is.
///
/// It was two rows — `╲` over `╱` — on the theory that a one-column mark against
/// a picture is a speck. Drawn, it was not a bigger caret but a *second thing*:
/// two strokes that read as a glyph of their own and took a row of the composer
/// with them. Small and white beats large and ambiguous.
const CARET: &str = "›";

/// Where the composer is typed, and what it is being typed to.
///
/// # Its shape is the answer to two complaints
///
/// It was a box. A box draws four edges to say "the input is here", and the
/// owner wanted one thing said instead: a rule above, a rule below, edge to
/// edge. And the model and mode line sits **inside** that pair of rules, hard
/// against the left margin, so that a question which wraps stays aligned with
/// itself and the line that is not part of the question is obviously not part of
/// it.
fn composer(app: &mut App, cols: usize, max_rows: usize) -> (Sink, Option<(usize, usize)>) {
    app.composer_hit_rows.clear();
    let inner_w = cols.saturating_sub(4).max(8);
    let text_w = inner_w.saturating_sub(2).max(6);
    // Publish the column the wrap happens at, so `↑`/`↓` can move the caret by
    // one *visible* row rather than by one logical newline. See
    // [`App::composer_width`] and [`keys::cursor_up`].
    app.composer_width = text_w;

    let projection = super::editor::Projection::plain(&app.input);
    let at = projection.display_at(app.cursor);
    let rows = compose_rows(&projection.text, text_w);
    let here = app
        .input_editor
        .row_index(&app.input, app.cursor, text_w, &rows, at);
    // One row at least, and one is what an empty composer gets: the caret is a
    // single mark now, so a second row would be a blank line pretending to be
    // part of the question. More as the text grows, never more than the screen
    // can spare.
    // 6 × 1.75 = 10.5: round up to 11 editable rows. An empty input remains
    // one row; short terminals still retain room for the conversation.
    const COMPOSER_ROWS: usize = 11;
    let window = max_rows.saturating_sub(2).clamp(1, COMPOSER_ROWS);
    let max_first = rows.len().saturating_sub(window);
    let mut first = app.composer_first_row.min(max_first);
    if here < first {
        first = here;
    } else if here >= first.saturating_add(window) {
        first = here.saturating_add(1).saturating_sub(window);
    }
    app.composer_first_row = first;
    let drawn = rows.len().saturating_sub(first).min(window).max(1);

    let mut out = Sink::default();
    let mut cursor_target = None;

    let top_label = if !app.attached.is_empty() && cols < 50 {
        if cols < 28 {
            format!("[{}]", mode_of(&app.mode).0)
        } else {
            format!("[ {} ]", mode_of(&app.mode).0)
        }
    } else {
        format!("[{} ]", addressed_to(app))
    };
    let prefix = if cols < 30 && !app.attached.is_empty() {
        format!("{RULE}╭─{RESET}{top_label}{RULE}─{RESET}")
    } else {
        format!("{RULE}╭─ {RESET}{top_label}{RULE} ──{RESET}")
    };
    if !app.attached.is_empty() {
        let (attachments, visible, start, item_w) =
            attachment_boxes(&prefix, &app.attached, cols, true);
        if let Some(top_line) = attachments.first() {
            out.push_at(
                ansi::truncate(top_line, cols),
                Hit::Attachments {
                    start,
                    visible,
                    width: item_w,
                },
            );
        } else {
            let rest = cols.saturating_sub(ansi::width(&top_label) + 5);
            let top_line = format!(
                "{RULE}╭─ {RESET}{top_label}{RULE} {}╮{}",
                "─".repeat(rest),
                ansi::RESET
            );
            out.push_at(ansi::truncate(&top_line, cols), Hit::Composer);
        }
    } else {
        let rest = cols.saturating_sub(ansi::width(&top_label) + 5);
        let top_line = format!(
            "{RULE}╭─ {RESET}{top_label}{RULE} {}╮{}",
            "─".repeat(rest),
            ansi::RESET
        );
        out.push_at(ansi::truncate(&top_line, cols), Hit::Composer);
    }

    for n in 0..drawn {
        let caret = if n == 0 {
            format!("{ACCENT}{BOLD}{CARET}{}", ansi::RESET)
        } else {
            " ".to_string()
        };
        let Some((start, text)) = rows.get(first + n) else {
            let line = boxed(&format!("{caret} "), inner_w);
            out.push_at(ansi::truncate(&line, cols), Hit::Composer);
            continue;
        };
        if first + n == here {
            let cut = ansi::caret_in_row(text, *start, at);
            let (before, _) = text.split_at(cut);
            cursor_target = Some((out.lines.len(), 5 + ansi::width(before)));
        }
        let geometry = projection.row(*start, text, 5);
        let body = format!(
            "{USER}{}{RESET}",
            geometry.styled(app.input_editor.selected(&app.input, app.cursor))
        );
        let mut line_content = format!("{caret} {body}");
        // Nothing typed yet: say what this is for, once.
        if rows.len() == 1 && text.is_empty() && n == 0 {
            line_content.push_str(&format!("{FAINT}ask, or / for commands{}", ansi::RESET));
        }
        let line = boxed(&line_content, inner_w);
        let hit = app.composer_hit_rows.len();
        app.composer_hit_rows.push(projection.row(*start, text, 5));
        out.push_at(ansi::truncate(&line, cols), Hit::ComposerText(hit));
    }
    out.push(box_bottom(cols));
    (out, cursor_target)
}

/// One horizontal row of fixed-width or fitted attachment boxes, integrated into the top border.
pub(super) const ATTACHMENT_WIDTH: usize = 22;
fn attachment_boxes(
    prefix: &str,
    files: &[runeengine::media::Attachment],
    width: usize,
    draft: bool,
) -> (Vec<String>, usize, usize, usize) {
    let prefix_effective = if prefix.is_empty() {
        format!("{RULE}╭──{RESET}")
    } else {
        prefix.to_string()
    };
    let prefix_w = ansi::width(&prefix_effective);
    let avail = width.saturating_sub(prefix_w + 2);
    if files.is_empty() || avail < 4 {
        return (Vec::new(), 0, 0, 0);
    }

    let (visible, item_w) = if draft {
        let item_w = ATTACHMENT_WIDTH.min(avail);
        let mut vis = files.len().min((avail + 1) / (item_w + 1));
        while vis > 0 {
            let hidden = files.len() - vis;
            let counter_w = if hidden > 0 {
                let label = format!("+{} more", hidden);
                (ansi::width(&label) + 4).min(avail) + 1
            } else {
                0
            };
            if vis * (item_w + 1) - 1 + counter_w <= avail {
                break;
            }
            vis -= 1;
        }
        (vis, item_w)
    } else {
        let mut vis = 0;
        let mut used = 0;
        for file in files {
            let name_w = ansi::width(&ansi::tame(&file.name)).min(20);
            let box_w = name_w + 4;
            let needed = if vis == 0 { box_w } else { used + 1 + box_w };
            if needed <= avail {
                vis += 1;
                used = needed;
            } else {
                break;
            }
        }
        while vis > 0 && vis < files.len() {
            let hidden = files.len() - vis;
            let counter_w = format!("+{} more", hidden).len() + 4;
            let mut total_w = 0;
            for file in files.iter().take(vis) {
                let name_w = ansi::width(&ansi::tame(&file.name)).min(20);
                total_w += name_w + 4;
            }
            total_w += vis + counter_w;
            if total_w <= avail {
                break;
            }
            vis -= 1;
        }
        (vis, 0)
    };

    let mut boxes: Vec<(String, usize)> = Vec::new();
    if draft {
        let item_w = ATTACHMENT_WIDTH.min(avail);
        for file in files.iter().take(visible) {
            let name = ansi::tame(&file.name);
            let avail_content = item_w.saturating_sub(3);
            let max_name = avail_content.saturating_sub(2);
            let name_trunc = if ansi::width(&name) > max_name {
                let mut label = String::new();
                for c in name.chars() {
                    if ansi::width(&label) + ansi::width(&c.to_string())
                        > max_name.saturating_sub(3)
                    {
                        break;
                    }
                    label.push(c);
                }
                format!("{label}...")
            } else {
                name
            };
            let name_w = ansi::width(&name_trunc);
            let spaces = avail_content.saturating_sub(name_w);
            let left_p = spaces.div_ceil(2);
            let right_p = spaces.saturating_sub(left_p);
            let middle = format!(
                "{RULE}[{RESET}{}{TEXT}{name_trunc}{RESET}{}{MUTED}×{RULE}]{RESET}",
                " ".repeat(left_p),
                " ".repeat(right_p)
            );
            boxes.push((middle, item_w));
        }
        if visible < files.len() {
            let label = format!("+{} more", files.len() - visible);
            let badge_w = (ansi::width(&label) + 4).min(avail);
            let badge = format!("{RULE}[ {TEXT}{label}{RESET}{RULE} ]{RESET}");
            boxes.push((badge, badge_w));
        }
    } else {
        for file in files.iter().take(visible) {
            let name = ansi::tame(&file.name);
            let max_name = 20.min(avail.saturating_sub(4));
            let name_trunc = if ansi::width(&name) > max_name {
                let mut label = String::new();
                for c in name.chars() {
                    if ansi::width(&label) + ansi::width(&c.to_string())
                        > max_name.saturating_sub(3)
                    {
                        break;
                    }
                    label.push(c);
                }
                format!("{label}...")
            } else {
                name
            };
            let badge = format!("{RULE}[ {TEXT}{name_trunc}{RESET}{RULE} ]{RESET}");
            let box_w = ansi::width(&name_trunc) + 4;
            boxes.push((badge, box_w));
        }
        if visible < files.len() {
            let label = format!("+{} more", files.len() - visible);
            let badge_w = (ansi::width(&label) + 4).min(avail);
            let badge = format!("{RULE}[ {TEXT}{label}{RESET}{RULE} ]{RESET}");
            boxes.push((badge, badge_w));
        }
    }

    if boxes.is_empty() {
        return (Vec::new(), 0, 0, 0);
    }

    let total_boxes_w: usize =
        boxes.iter().map(|(_, w)| *w).sum::<usize>() + boxes.len().saturating_sub(1);
    let right_dashes = width.saturating_sub(prefix_w + total_boxes_w + 1);

    let parts: Vec<String> = boxes.into_iter().map(|(b, _)| b).collect();
    let row = format!(
        "{prefix_effective}{}{}{RULE}╮{RESET}",
        parts.join(&format!("{RULE}─{RESET}")),
        format_args!("{RULE}{}{RESET}", "─".repeat(right_dashes))
    );
    let row = ansi::truncate(&row, width);

    (vec![row], visible, prefix_w, item_w)
}

/// How many commands the palette shows at once.
pub const PALETTE_ROWS: usize = 4;

/// Computes the first visible row in the palette with a 1-line scroll margin.
/// When navigating downwards, the cursor stops at the second row from bottom (shown - 2)
/// and auto-scrolls. When navigating upwards, the cursor stops at the second row from top
/// (index 1) and auto-scrolls.
pub fn compute_palette_first(
    selected: usize,
    current_scroll: usize,
    shown: usize,
    total: usize,
) -> usize {
    if shown == 0 || total <= shown {
        return 0;
    }
    let max_scroll = total.saturating_sub(shown);
    let m = 1.min(shown.saturating_sub(1) / 2);

    let mut scroll = current_scroll.min(max_scroll);
    // Heading down: if selected reaches or exceeds scroll + shown - 1 - m
    // (for shown = 4, m = 1: scroll + 2, which is 2nd row from bottom)
    if selected >= scroll + shown.saturating_sub(1 + m) {
        let target = (selected + m + 1).saturating_sub(shown);
        scroll = target.min(max_scroll);
    }
    // Heading up: if selected drops below scroll + m
    // (for m = 1: scroll + 1, which is 2nd row from top)
    if selected < scroll + m {
        scroll = selected.saturating_sub(m);
    }
    scroll.min(max_scroll)
}

/// The command list that opens the moment a line starts with `/`.
fn palette(app: &App, _inner_w: usize, cols: usize, max_rows: usize, out: &mut Sink) {
    if max_rows == 0 {
        return;
    }
    let found = super::keys::matches(&app.input);
    if found.is_empty() {
        return;
    }
    let at = app.palette_row.min(found.len() - 1);
    let shown = PALETTE_ROWS.min(max_rows.saturating_sub(1).max(1));
    let first = compute_palette_first(at, app.palette_scroll, shown, found.len());
    let name_col_w = found
        .iter()
        .skip(first)
        .take(shown)
        .map(|(verb, args, _)| {
            if args.is_empty() {
                verb.len() + 1
            } else {
                verb.len() + 2 + args.len()
            }
        })
        .max()
        .unwrap_or(22)
        .clamp(20, cols.saturating_sub(35).max(20));

    for (n, (verb, args, what)) in found.iter().enumerate().skip(first).take(shown) {
        let name = if args.is_empty() {
            format!("/{verb}")
        } else {
            format!("/{verb} {args}")
        };
        let (mark, name_ink, what_ink) = if n == at {
            (
                format!("{ACCENT}{BOLD}{CARET}{RESET}  "),
                format!("{ACCENT}{BOLD}"),
                ACCENT,
            )
        } else {
            ("   ".to_string(), MUTED.to_string(), MUTED)
        };
        let padded_name = ansi::pad(&name, name_col_w);
        let avail_desc = cols.saturating_sub(2 + 3 + name_col_w + 2 + 1);
        let desc = ansi::truncate(what, avail_desc);
        let line = format!("  {mark}{name_ink}{padded_name}{RESET}  {what_ink}{desc}{RESET}");
        out.push_at(ansi::truncate(&line, cols), Hit::Palette(n));
    }
    let left = found.len().saturating_sub(first + shown);
    if max_rows > shown && (left > 0 || first > 0) {
        let line = format!(
            "     {FAINT}{} of {} · ↑↓ to walk{RESET}",
            at + 1,
            found.len(),
        );
        out.push(ansi::truncate(&line, cols));
    }
}

/// Who the question in the composer is going to, how far it may go — and, in the
/// far corner, what version of the program is asking.
fn addressed_to(app: &App) -> String {
    let model = if app.model().is_empty() {
        "Select Models"
    } else {
        app.model()
    };
    let thinking = match super::thinking_label(app.thinking(), app.effort()) {
        Some(lbl) => format!(" {TEXT}{lbl}{}", ansi::RESET),
        None => String::new(),
    };
    let (mode, ink) = mode_of(&app.mode);
    format!(
        " {TEXT}{}{model}{}{thinking} {FAINT}·{} {ink}{}{mode}{}",
        ansi::BOLD,
        ansi::RESET,
        ansi::RESET,
        ansi::BOLD,
        ansi::RESET
    )
}

/// A permission mode as a person reads it, and the ink its risk deserves.
pub(super) fn mode_of(mode: &str) -> (&str, ansi::Ink) {
    match mode {
        "full" => ("Full", REMOVED),
        "auto" => ("Auto", WARN),
        "ask" => ("Ask", OK),
        other => (other, MUTED),
    }
}

/// The last line: where the work is, or what the engine is doing.
fn state_line(app: &App, cols: usize) -> String {
    let (mode_name, mode_ink) = mode_of(&app.mode);
    let mode_desc = match app.mode.as_str() {
        "full" => {
            if app.classifier_active() {
                "unconfined system access supervised by active classifier policy"
            } else if !app.classifier_enabled() {
                "unconfined system access; classifier is disabled by configuration"
            } else if !app.has_classifier_policy() {
                "unconfined system access; classifier is inactive because no policy file was found"
            } else {
                "unconfined system access; classifier is inactive"
            }
        }
        "auto" => {
            if app.classifier_active() {
                "works freely inside workspace supervised by active classifier policy"
            } else if !app.classifier_enabled() {
                "works freely inside workspace; classifier is disabled by configuration"
            } else if !app.has_classifier_policy() {
                "works freely inside workspace; classifier is inactive because no policy file was found"
            } else {
                "works freely inside workspace; classifier is inactive"
            }
        }
        _ => "asks before actions run; classifier is inactive in ask mode",
    };
    let mode_pill = format!("{mode_ink}[{mode_name}]{RESET}");
    let countdown = super::undo::countdown(app);
    let right = match countdown
        .as_ref()
        .or(app.retry_status.as_ref())
        .or(app.status.as_ref())
    {
        Some(text) if countdown.is_some() => format!("{WARN}{text}{RESET}"),
        Some(status) if status.starts_with("Switching mode") => {
            format!("{mode_pill} {MUTED}{mode_desc}{RESET}")
        }
        Some(status) => format!("{TEXT}{status}{}", ansi::RESET),
        None => format!("{mode_pill} {MUTED}{mode_desc}{RESET}"),
    };
    let left = if app.busy {
        let label = if app.label.is_empty() {
            "working"
        } else {
            &app.label
        };
        let seconds = app.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let formatted = format_duration(seconds);
        let spin_char = pulse::spinner(app.frame);
        format!(
            "{ACCENT}{BOLD}{spin_char}{RESET} {TEXT}{label}{RESET} {FAINT}[{formatted}]{RESET} {RULE}│{RESET} {MUTED}{}{RESET}",
            home(&app.project)
        )
    } else {
        format!("{TEXT}{}{}", home(&app.project), ansi::RESET)
    };
    let body = format!(" {left} {RULE}│{} {right}", ansi::RESET);
    // The build this window runs, never its host's (see `App::host_version`).
    let version = format!("{FAINT}{}{}", runeengine::VERSION, ansi::RESET);
    let room = ansi::width(&version) + 1;
    if let Some(w) = cols.checked_sub(room) {
        if w >= 20 {
            let truncated = ansi::truncate(&body, w);
            return format!("{}{version} ", ansi::pad(&truncated, w));
        }
    }
    ansi::truncate(&body, cols)
}

/// `/home/someone/Project/App` → `~/Project/App`. The prefix is the same on
/// every line a person has ever read and carries nothing.
pub(super) fn home(path: &str) -> String {
    match runeengine::platform::home_dir()
        .as_deref()
        .and_then(std::path::Path::to_str)
    {
        Some(home) if path.starts_with(home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Break the composer's text into rows, each with the byte offset it starts at.
///
/// The offsets are the whole point: without them the cursor cannot be placed
/// after wrapping, and a composer whose cursor is in the wrong row is worse than
/// one that does not wrap at all. Breaks on the last space where there is one,
/// mid-word where there is not — a path with no spaces is not a reason to stop
/// showing what is being typed.
pub(super) fn compose_rows(text: &str, width: usize) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut start = 0;
    let mut row = String::new();
    let width = width.max(1);
    let mut used = 0;
    // Source byte of the last space, and its byte offset in the current row.
    let mut last_space: Option<(usize, usize)> = None;

    for (offset, glyph, w) in ansi::editor_glyphs(text) {
        if glyph == "\n" {
            out.push((start, std::mem::take(&mut row)));
            start = offset + 1;
            used = 0;
            last_space = None;
            continue;
        }
        if used + w > width && !row.is_empty() {
            match last_space {
                Some((byte, split)) if split > 0 => {
                    let carry = row[split + 1..].to_string();
                    row.truncate(split);
                    out.push((start, std::mem::take(&mut row)));
                    start = byte + 1;
                    used = ansi::width(&carry);
                    row = carry;
                }
                _ => {
                    out.push((start, std::mem::take(&mut row)));
                    start = offset;
                    used = 0;
                }
            }
            last_space = None;
            // A space near the start can leave an unbreakable carry that still
            // fills this row; split before this glyph, never through it.
            if used + w > width && !row.is_empty() {
                out.push((start, std::mem::take(&mut row)));
                start = offset;
                used = 0;
            }
        }
        if glyph == " " {
            last_space = Some((offset, row.len()));
        }
        row.push_str(glyph);
        used += w;
    }
    out.push((start, row));
    out
}

/// Content preview for tools without a container box (`inspect_file`, `find_files`,
/// `search_text`, `web_search`, `web_fetch`). Caps at 8 visual lines when closed with a cut
/// indicator, or expands to all lines when open, rendered along the timeline spine.
fn draw_borderless_preview(
    content: &str,
    open: bool,
    spine: &str,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    draw_folded_preview(content, &[], open, spine, inner, sink, at);
}

/// [`draw_borderless_preview`] with Rune's notice rows (already laid out,
/// [`notice_rows`]) as the content's last rows: one budget, one fold, and the
/// fold marker after all of it.
fn draw_folded_preview(
    content: &str,
    notices: &[String],
    open: bool,
    spine: &str,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let avail_width = inner.saturating_sub(6).max(20);
    let max_rows = super::viewport::preview_rows(open);
    let mut all_rows: Vec<String> = Vec::new();
    // Count the rows laid out at this width, not a mixture of source lines and
    // wrapped rows. The transcript cache only does this when the result changes.
    for line in content.lines() {
        // Only the harness's own markup is dropped; a content line that
        // mentions a tag — a search hit in Rune's own source — is content (U6).
        if runeengine::tools::notice::is_markup(line) {
            continue;
        }
        let tamed = ansi::tame(line.trim_end_matches('\r'));
        if ansi::width(&tamed) > avail_width {
            let segments = ansi::wrap(&tamed, avail_width);
            all_rows.extend(segments);
        } else {
            all_rows.push(tamed);
        }
    }

    let content_rows = all_rows.len();
    all_rows.extend(notices.iter().cloned());
    let total_rows = all_rows.len();
    let rows_to_show = if !open {
        total_rows.min(max_rows)
    } else {
        total_rows
    };
    let cut_count = total_rows.saturating_sub(rows_to_show);

    for (n, line) in all_rows[..rows_to_show].iter().enumerate() {
        // The notice rows in the one neutral ink of the harness's own words,
        // as `draw_notice_rows` draws them elsewhere.
        let styled = if n < content_rows {
            style_preview_row(line)
        } else {
            format!("{MUTED}{line}{RESET}")
        };
        sink.push_at(format!("{spine}{styled}"), Hit::Block(at));
    }
    if cut_count > 0 {
        sink.push_at(
            format!(
                "{spine}{FAINT}{}{RESET}",
                super::viewport::cut_lines(cut_count)
            ),
            Hit::Block(at),
        );
    }
}

fn style_preview_row(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with("### ") || trimmed.starts_with("- ") {
        format!("{TEXT}{BOLD}{line}{RESET}")
    } else {
        // Same rule as the terminal box: the tool's own words and the harness's
        // housekeeping around them are one neutral ink, and only the envelope's
        // rules are set apart — by weight, which costs no theme hue.
        let row = classify_output_row(line);
        match row {
            OutputRow::Divider | OutputRow::Notice => {
                format!("{}{line}{RESET}", output_prefix(row))
            }
            OutputRow::Text if trimmed.starts_with("=== ") && trimmed.ends_with(" ===") => {
                format!("{MUTED}{line}{RESET}")
            }
            OutputRow::Text => match split_line_number(line) {
                Some((prefix, rest)) => format!("{FAINT}{prefix}{MUTED}|{TEXT}{rest}{RESET}"),
                None if trimmed.contains("match(es)") || trimmed.contains("lines)") => {
                    format!("{MUTED}{line}{RESET}")
                }
                None => format!("{TEXT}{line}{RESET}"),
            },
        }
    }
}

fn split_line_number(s: &str) -> Option<(&str, &str)> {
    let bar_idx = s.find('|')?;
    let (prefix, rest) = s.split_at(bar_idx);
    let trimmed_num = prefix.trim();
    if !trimmed_num.is_empty() && trimmed_num.chars().all(|c| c.is_ascii_digit()) {
        Some((prefix, &rest[1..]))
    } else {
        None
    }
}

fn mcp_text_or_raw(content: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = content.trim();
    if !trimmed.starts_with('{') {
        return std::borrow::Cow::Borrowed(content);
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let mut texts = Vec::new();
        if let Some(arr) = v
            .get("content")
            .or_else(|| v.get("contents"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    texts.push(text);
                }
            }
        }
        if !texts.is_empty() {
            return std::borrow::Cow::Owned(texts.join("\n"));
        }
    }
    std::borrow::Cow::Borrowed(content)
}

/// Formatted preview for `discover_capabilities` search output.
/// Renders capabilities as a structured list with glyphs, names, badges,
/// and concise descriptions along the timeline spine.
fn draw_capabilities_preview(
    content: &str,
    open: bool,
    spine: &str,
    inner: usize,
    sink: &mut Sink,
    at: usize,
) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(content) else {
        draw_borderless_preview(content, open, spine, inner, sink, at);
        return;
    };
    let Some(entries) = val.get("entries").and_then(|v| v.as_array()) else {
        draw_borderless_preview(content, open, spine, inner, sink, at);
        return;
    };
    if entries.is_empty() {
        let line = format!("{spine}{MUTED}No capabilities matched query{RESET}");
        sink.push_at(ansi::truncate(&line, inner + 2), Hit::Block(at));
        return;
    }
    let total = val
        .get("total")
        .and_then(|v| v.as_u64())
        .map(|u| u as usize)
        .unwrap_or(entries.len());
    let max_entries = super::viewport::preview_rows(open);
    let (rendered, cut_count) = if entries.len() > max_entries {
        (&entries[..max_entries], entries.len() - max_entries)
    } else {
        (&entries[..], 0)
    };
    for entry in rendered {
        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("tool");
        let name = entry
            .get("name")
            .or_else(|| entry.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let source = entry.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let desc = entry
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let line_desc = desc.lines().next().unwrap_or("").trim();
        let short_desc = if let Some((first, _)) = line_desc.split_once(". ") {
            first
        } else if let Some(stripped) = line_desc.strip_suffix('.') {
            stripped
        } else {
            line_desc
        };
        let (kind_glyph, kind_colour) = match kind {
            "skill" => ("◆", ACCENT),
            "resource" => ("■", WARN),
            _ => ("•", MUTED),
        };
        let source_badge = match source {
            "rune-builtin" => format!(" {FAINT}[builtin]{RESET}"),
            "mcp" => format!(" {ACCENT}[mcp]{RESET}"),
            "project" => format!(" {OK}[project]{RESET}"),
            // A badge is a marker, not code, and `Role::Code` now means one
            // thing only. "Yours" is the item worth seeing first, so it is the
            // brightest ink rather than another hue to decode.
            "user" => format!(" {TEXT}[user]{RESET}"),
            _ if !source.is_empty() => format!(" {FAINT}[{source}]{RESET}"),
            _ => String::new(),
        };
        let desc_part = if !short_desc.is_empty() {
            format!(" {MUTED}— {short_desc}{RESET}")
        } else {
            String::new()
        };
        let line = format!(
            "{spine}  {kind_colour}{kind_glyph}{RESET} {TEXT}{BOLD}{name}{RESET}{source_badge}{desc_part}",
        );
        sink.push_at(ansi::truncate(&line, inner + 2), Hit::Block(at));
    }
    let remaining = cut_count + total.saturating_sub(entries.len());
    if remaining > 0 {
        sink.push_at(
            format!("{spine}{FAINT}… {remaining} more capabilities. Click to open all{RESET}"),
            Hit::Block(at),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, state: ToolState) -> Tool {
        let mut t = Tool::new("c1".into(), name.into(), "do the thing".into());
        t.state = state;
        t
    }

    /// The palette's own lines, without the composer around them.
    fn palette_lines(app: &App, cols: usize) -> Vec<String> {
        let mut sink = Sink::default();
        let inner_w = cols.saturating_sub(4).max(8);
        palette(app, inner_w, cols, PALETTE_ROWS + 1, &mut sink);
        sink.lines
    }

    fn screen(app: &mut App, rows: u16, cols: u16) -> String {
        compose(app, rows, cols)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn test_app() -> App {
        App::for_test()
    }

    /// Every line of the conversation, drawn through the production path.
    ///
    /// `whole` is set because a test asking for "the transcript" means all of
    /// it, and the tail-first fill is a first-paint optimisation rather than a
    /// different transcript. The rendering itself is not re-implemented here —
    /// a helper that drew its own would pass while the real one was broken.
    fn transcript(app: &mut App, width: usize) -> Sink {
        transcript_into(None, app, width, true, true).sink
    }

    /// Audit U6: a search hit that merely mentions the notice tag is content,
    /// and stays; only the harness's own markup is dropped.
    #[test]
    fn a_content_line_that_mentions_a_notice_tag_is_kept() {
        let content = "<tool_notice>Stopped at match limit of 1.</tool_notice>\nsrc/x.rs:4: let open = \"<tool_notice>\";";
        let mut sink = Sink::default();
        draw_borderless_preview(content, true, "", 100, &mut sink, 0);
        let text = sink.lines.join("\n");
        assert!(ansi::plain(&text).contains("let open"), "{text}");
        assert!(!ansi::plain(&text).contains("Stopped at match limit"));
    }

    /// Audits S1/S3/S4: in a long session, a frame that changes only the
    /// newest step — or flips `busy` at a turn boundary — costs work in
    /// proportion to that step, not to the session.
    #[test]
    fn a_long_session_redraws_in_proportion_to_what_changed() {
        let mut app = test_app();
        for turn in 0..400 {
            app.blocks.push(Block::user_timed(
                format!("prompt {turn}"),
                None::<String>,
                Vec::new(),
            ));
            for _ in 0..4 {
                app.blocks
                    .push(Block::Tool(Box::new(tool("terminal_use", ToolState::Ok))));
            }
            app.blocks.push(Block::Text {
                text: format!("answer {turn} with **markdown** and `code`"),
            });
        }
        let width = 120;
        let mut tx = Some(transcript_into(None, &app, width, true, true));
        let groups = tx.as_ref().unwrap().drawn;
        assert!(groups > 400, "the whole session is laid out once: {groups}");
        for frame in 0..200 {
            if let Some(Block::Text { text }) = app.blocks.last_mut() {
                text.push_str(" more");
            }
            if frame % 20 == 0 {
                app.busy = !app.busy;
            }
            tx = Some(transcript_into(tx.take(), &app, width, true, false));
        }
        // Counted in groups drawn, not in seconds: a few per frame (the newest
        // step, and the ones a `busy` flip touches), never the session again.
        let redrawn = tx.as_ref().unwrap().drawn - groups;
        assert!(
            redrawn <= 200 * 4,
            "200 dirty frames over {groups} groups redrew {redrawn} of them"
        );
    }

    /// Audit S3: a running turn is one Steps group, so each delta used to
    /// redraw every step in it. Steps now come from a per-step cache; whatever
    /// it reuses must be exactly what a cold render would draw.
    #[test]
    fn cached_steps_render_exactly_like_a_cold_render() {
        let mut app = test_app();
        app.blocks.push(Block::user_timed(
            "go".to_string(),
            None::<String>,
            Vec::new(),
        ));
        app.blocks.push(Block::Thinking {
            text: "plan the work".into(),
            open: true,
        });
        app.blocks
            .push(Block::Tool(Box::new(tool("terminal_use", ToolState::Ok))));
        app.blocks.push(Block::Text {
            text: "first words".into(),
        });
        let width = 100;
        let warm = transcript_into(None, &app, width, true, true);
        if let Some(Block::Text { text }) = app.blocks.last_mut() {
            text.push_str(" and more");
        }
        app.busy = true;
        let warm = transcript_into(Some(warm), &app, width, true, true).sink;
        let cold = transcript(&mut app, width);
        assert_eq!(warm.lines, cold.lines);
        assert_eq!(warm.owner, cold.owner);
        assert!(warm
            .lines
            .iter()
            .any(|l| ansi::plain(l).contains("and more")));
    }

    #[test]
    fn every_way_a_block_can_change_moves_its_cheap_reading() {
        // Each case is a pair of measures and stamps that must differ. Building
        // them as pairs rather than as blocks keeps the test about the filter:
        // what matters is that no real mutation slips past it.
        // A running step, rebuilt rather than cloned: `Tool` carries a live
        // counter ramp, so it is deliberately not `Clone`.
        let running = || {
            let mut t = Tool::new("t1".into(), "terminal_use".into(), "run it".into());
            t.state = ToolState::Running;
            t
        };

        let cases: Vec<(&str, Block, Block)> = vec![
            (
                "text grows",
                Block::Text { text: "one".into() },
                Block::Text {
                    text: "one two".into(),
                },
            ),
            (
                "thought streams",
                Block::Thinking {
                    text: "think".into(),
                    open: false,
                },
                Block::Thinking {
                    text: "think harder".into(),
                    open: false,
                },
            ),
            (
                "thought folded",
                Block::Thinking {
                    text: "think".into(),
                    open: false,
                },
                Block::Thinking {
                    text: "think".into(),
                    open: true,
                },
            ),
            (
                "notice turns bad",
                Block::Notice {
                    text: "careful".into(),
                    bad: false,
                },
                Block::Notice {
                    text: "careful".into(),
                    bad: true,
                },
            ),
            ("step output arrives", Block::of(running()), {
                let mut t = running();
                t.output.push_str("line one\n");
                Block::of(t)
            }),
            (
                "step finishes",
                {
                    let mut t = running();
                    t.state = ToolState::Ok;
                    t.snap_counters();
                    Block::of(t)
                },
                {
                    let mut t = running();
                    t.state = ToolState::Failed;
                    t.summary = "it failed".into();
                    Block::of(t)
                },
            ),
            ("step unfolds", Block::of(running()), {
                let mut t = running();
                t.open = true;
                Block::of(t)
            }),
            ("step action settles", Block::of(running()), {
                let mut t = running();
                t.action = "a different intent".into();
                Block::of(t)
            }),
            ("step arguments settle", Block::of(running()), {
                let mut t = running();
                t.args = Some(serde_json::json!({"command": "ls"}));
                Block::of(t)
            }),
            ("step counters retarget", Block::of(running()), {
                let mut t = running();
                t.snap_counters();
                t.absorb(r#"{"command": "ls -la"}"#);
                Block::of(t)
            }),
        ];

        for (what, before, after) in cases {
            assert_ne!(
                BlockMeasure::of(&before),
                BlockMeasure::of(&after),
                "{what}: the cheap reading did not move, so the filter would skip it"
            );
            assert_ne!(
                before.stamp(),
                after.stamp(),
                "{what}: the fingerprint did not move"
            );
        }

        // And the other direction, or the filter buys nothing: an untouched
        // block must measure and hash the same both times.
        let before = Block::of(running());
        let after = Block::of(running());
        assert_eq!(BlockMeasure::of(&before), BlockMeasure::of(&after));
        assert_eq!(before.stamp(), after.stamp());
    }

    /// The screen a person would actually see, as plain text.
    ///
    /// Every test about what is *on screen* has to come through here. Calling
    /// `sidebar()` directly is how a thirty-item plan came to be asserted
    /// "complete" while nine of it were being cut — the function it called was
    /// correct, and the screen was not.
    fn frame_text(app: &mut App, rows: u16, cols: u16) -> String {
        compose(app, rows, cols)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The highest plan item number visible in a frame, so a test can talk about
    /// "where the window is" without hardcoding a count that changes whenever
    /// the panel's height or the items' wrapping does.
    fn last_plan_item(frame: &str) -> usize {
        frame
            .lines()
            .filter_map(|l| l.split("step ").nth(1))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter_map(|n| n.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
    }

    // ─── Sep 29 sweep: scroll, palette, panel, cross-session ────────────────

    /// A reader who scrolled up in a long session must see the *first* items of
    /// the transcript, not the newest rows the tail-first fill happens to have:
    /// `whole` is wired to the scroll, so scrolling up lays everything out and
    /// the anchor this frame needs is actually there. Without it the view
    /// jumped to whatever was laid out — the stutter scrolling felt like.
    #[test]
    fn scrolling_up_reaches_the_head_of_a_long_session_without_jumping() {
        let mut app = test_app();
        app.sidebar = false;
        // Long enough that the tail-first fill cannot finish in one frame
        // (the budget is 240 lines per frame).
        for n in 0..40 {
            app.blocks.push(Block::Text {
                text: format!(
                    "block number {n:02} — one of forty paragraphs that make \
                               this session long enough to need the head fill"
                ),
            });
        }
        let _ = frame_text(&mut app, 24, 100);

        // Wheel up into history until the head is on screen. Each wheel tick
        // repaints; the head is laid out on demand (`whole`), so it must be
        // there when the scroll gets there — however many ticks that takes.
        let mut reached = String::new();
        for _ in 0..40 {
            app.scroll_by(true, 6);
            reached = frame_text(&mut app, 24, 100);
            if reached.contains("block number 00") {
                break;
            }
        }
        assert!(
            reached.contains("block number 00"),
            "the head must be reachable when the reader scrolls up"
        );
        // The window stayed put relative to the content: a second frame with
        // nothing changed must show the same rows, not drift toward the tail.
        let again = frame_text(&mut app, 24, 100);
        assert_eq!(
            ansi::plain(reached.lines().nth(4).unwrap_or_default()),
            ansi::plain(again.lines().nth(4).unwrap_or_default()),
            "an idle frame while scrolled must not move the window"
        );
    }

    #[test]
    fn three_row_wheel_after_partial_first_paint_never_teleports_to_old_history() {
        let mut app = test_app();
        app.sidebar = false;
        for n in 0..420 {
            app.blocks.push(Block::Text {
                text: format!("unique line number {n:03} in a long conversation"),
            });
        }
        let tail = frame_text(&mut app, 24, 100);
        assert!(tail.contains("unique line number 419"));
        assert!(
            app.transcript_cache.as_ref().unwrap().first > 0,
            "the first paint must still contain an unlaid head to exercise this transition"
        );
        app.scroll_by(true, 3);
        let moved = frame_text(&mut app, 24, 100);
        assert!(
            moved.contains("unique line number 41"),
            "a 3-row wheel must remain near the newest line, not jump into history: {moved}"
        );
        assert!(
            !moved.contains("unique line number 000"),
            "a small wheel move teleported hundreds of rows: {moved}"
        );
        let still = frame_text(&mut app, 24, 100);
        assert_eq!(
            moved, still,
            "an idle frame must not yank the viewport to another duplicate anchor"
        );
    }

    #[test]
    fn a_stale_anchor_cannot_match_distant_duplicate_text_and_jump_the_viewport() {
        let mut app = test_app();
        for n in 0..330 {
            app.blocks.push(Block::Text {
                text: if n == 2 {
                    "distant duplicate anchor".into()
                } else {
                    format!("tail row {n:03}")
                },
            });
        }
        let _ = frame_text(&mut app, 24, 100);
        app.scroll_by(true, 5);
        let before = frame_text(&mut app, 24, 100);
        assert!(
            before.contains("tail row 32"),
            "reader is at the tail: {before}"
        );
        // A previously visible line changed text while the turn streamed.
        // Only an identical line in distant history survives. It must not be
        // picked as the reader's new position — use the local arithmetic.
        app.scroll_anchor = Some(Anchor {
            text: "distant duplicate anchor".into(),
            offset: 0,
            place: None,
        });
        let after = frame_text(&mut app, 24, 100);
        assert!(
            after.contains("tail row 32"),
            "stale anchor teleported the reader: {after}"
        );
        assert!(!after.contains("distant duplicate anchor"));
    }

    /// The slash palette floats over the content instead of taking rows from
    /// the frame: with the sidebar full of a long plan, opening `/` must not
    /// cut the panel's bottom rows.
    #[test]
    fn the_slash_palette_never_shrinks_the_side_panel() {
        let mut app = test_app();
        app.sidebar = true;
        app.mouse = true;
        // A plan far taller than the panel: the panel opens at its live end,
        // where the newest rows — and the `rows above` mark — are showing.
        app.tasks = (0..60)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let before = frame_text(&mut app, 30, 120);
        assert!(
            before.contains("rows above"),
            "the plan overflows: {before}"
        );
        let plan_rows_before = before
            .lines()
            .filter(|l| l.contains("│ ○ step ") || l.contains("│ ● step "))
            .count();
        assert!(plan_rows_before > 0, "plan rows are visible: {before}");

        // Open the palette.
        app.input = "/".into();
        app.cursor = 1;
        app.palette_row = 0;
        let open = frame_text(&mut app, 30, 120);
        assert!(
            open.contains("/attach")
                || open.contains("/compact")
                || open.contains("/help")
                || open.contains("/new"),
            "the palette is showing: {open}"
        );
        assert!(
            open.contains("rows above"),
            "the panel must keep its full height while the palette is open"
        );
        // And the number of plan rows visible is unchanged by the palette.
        let plan_rows_open = open
            .lines()
            .filter(|l| l.contains("│ ○ step ") || l.contains("│ ● step "))
            .count();
        assert_eq!(
            plan_rows_before, plan_rows_open,
            "the palette must not take rows from the panel\n--- before ---\n{before}\n--- open ---\n{open}"
        );
    }

    /// A reader parked on a wrapped (multi-row) plan item is not dragged back
    /// up by the anchor. The old recovery arithmetic subtracted the depth it
    /// should have added, which went unnoticed while every test item was one
    /// row — and in the field it was the panel springing to the top mid-stream.
    #[test]
    fn an_anchor_on_a_wrapped_item_holds_its_row() {
        let mut app = test_app();
        app.sidebar = true;
        // 30 items, each wrapping to several rows.
        app.tasks = (0..30)
            .map(|n| {
                json!({ "text": format!("step {n} does a great deal of work whose \
                        description wraps across more than one row of the panel, \
                        which is exactly what a real plan line looks like {n}"),
                        "status": "todo" })
            })
            .collect();
        let _ = frame_text(&mut app, 24, 120);
        let max = app.plan_max;
        assert!(max > 0);

        // Park deep in the list, on an item deep enough to be wrapped.
        app.plan_scroll_by(false, max, max);
        let parked = frame_text(&mut app, 24, 120);
        let anchor = app.plan_anchor.expect("the panel publishes its anchor");
        assert!(
            anchor.row > 0,
            "the live end of a wrapped plan sits inside a multi-row item: {anchor:?}"
        );

        // The plan is rewritten wholesale — what Event::Tasks does on every
        // update — with item 0's status changing (so the frame changes).
        app.tasks[0]["status"] = json!("done");
        let after = frame_text(&mut app, 24, 120);

        // The reader is where they were: the same anchor, and the same top
        // item — not pulled up by twice the row depth, the old failure.
        let held = app.plan_anchor.expect("still anchored");
        assert_eq!(held.item, anchor.item, "the anchor item held");
        assert_eq!(held.row, anchor.row, "the anchor row held");
        assert_eq!(last_plan_item(&parked), last_plan_item(&after));
    }

    /// The wheel over a following (live-end) panel keeps following while the
    /// plan is rewritten underneath — the mid-stream case the owner reported
    /// as "malah auto scroll keatas lagi".
    #[test]
    fn a_following_panel_keeps_following_through_a_wholesale_rewrite() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let _ = frame_text(&mut app, 24, 120);
        let max = app.plan_max;
        assert!(max > 0, "30 items in a 24-row panel overflow here: {max}");

        // Follow the live end...
        app.plan_scroll_by(false, max, max);
        let _ = frame_text(&mut app, 24, 120);
        assert!(
            app.plan_scroll >= max.saturating_sub(1),
            "following the end"
        );

        // ...and rewrite the whole list, as a streaming plan update does.
        app.tasks[5]["status"] = json!("done");
        let _ = frame_text(&mut app, 24, 120);
        assert!(
            app.plan_scroll >= app.plan_max.saturating_sub(1),
            "a reader at the live end stays at the live end: {} vs {}",
            app.plan_scroll,
            app.plan_max
        );
    }

    /// The lowest plan item number visible in a frame — the window's head,
    /// which is what actually moves when the reader scrolls while the tail
    /// stays pinned by capacity.
    fn first_plan_item(frame: &str) -> usize {
        frame
            .lines()
            .filter_map(|l| l.split("step ").nth(1))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter_map(|n| n.parse::<usize>().ok())
            .min()
            .unwrap_or(0)
    }

    #[test]
    fn attachment_cards_survive_palette_and_small_terminals_and_precede_the_message() {
        let mut app = test_app();
        let file = runeengine::media::Attachment {
            name: "A photograph with a long name 日本語.png".into(),
            mime: "image/png".into(),
            path: "/tmp/photo.png".into(),
            size: 2 * 1024 * 1024,
            ..Default::default()
        };
        app.attached = vec![file.clone(); 12];
        app.input = "/".into();
        app.cursor = 1;
        for rows in [8, 12, 24, 40] {
            for cols in [20, 40, 80, 140] {
                let frame = compose(&mut app, rows, cols);
                assert_eq!(frame.len(), rows as usize, "{rows}x{cols}");
                assert!(
                    frame.iter().all(|line| ansi::width(line) <= cols as usize),
                    "{rows}x{cols}"
                );
                assert!(
                    frame.iter().any(|line| ansi::plain(line).contains("more")),
                    "hidden files need a count"
                );
                let (row, col) = app.cursor_terminal.unwrap();
                assert!(row <= rows && col <= cols);
            }
        }
        app.attached.truncate(1);
        app.input = "Question".into();
        let frame = screen(&mut app, 32, 120);
        let input = frame
            .lines()
            .position(|line| line.contains("› Question"))
            .unwrap();
        let chips = frame
            .lines()
            .position(|line| line.contains("A photograph"))
            .unwrap();
        assert!(chips < input);
        assert!(frame.contains("..."));
        app.input = "/".into();
        let frame = screen(&mut app, 32, 120);
        assert!(frame.find("/attach path").unwrap() < frame.find("› /").unwrap());
        app.blocks.push(Block::user_timed(
            "What is in this photo?",
            Some("12:34".into()),
            vec![file.clone(); 8],
        ));
        let rows = transcript(&mut app, 120)
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>();
        let header = rows
            .iter()
            .find(|line| line.contains("you · 12:34"))
            .unwrap();
        assert!(header.starts_with('╭') && header.ends_with('╮'));
        let bottom = rows
            .iter()
            .rev()
            .find(|line| line.starts_with('╰'))
            .unwrap();
        assert_eq!(ansi::width(header), 120);
        assert_eq!(ansi::width(bottom), 120);
        assert!(rows.iter().any(|line| line.contains("more")));
        assert!(!rows.iter().any(|line| line.contains('×')));
        let (boxes, shown, _, _) = attachment_boxes("", &vec![file.clone(); 8], 70, true);
        assert_eq!(shown, 2);
        assert!(ansi::plain(&boxes[0]).contains("+6 more"));
        assert!(boxes.iter().all(|line| ansi::width(line) <= 70));
        let centered = runeengine::media::Attachment {
            name: "photo.png".into(),
            ..file
        };
        let (boxes, _, _, _) = attachment_boxes("", &[centered], 70, true);
        assert!(ansi::plain(&boxes[0]).contains("photo.png"));
        assert!(ansi::plain(&boxes[0]).starts_with("╭─"));
        assert!(ansi::plain(&boxes[0]).ends_with("╮"));
        assert_eq!(ansi::width(&boxes[0]), 70);
        assert!(boxes.iter().all(|line| ansi::width(line) <= 70));
    }

    // ── The frame ────────────────────────────────────────────────────────────

    #[test]
    fn the_frame_is_exactly_as_tall_as_the_terminal_and_never_wider() {
        let mut app = test_app();
        app.blocks.push(Block::user("hello"));
        app.blocks.push(Block::Text {
            text: "# Title\n\nsome **prose** and `code`".into(),
        });
        app.blocks
            .push(Block::of(tool("create_file", ToolState::Ok)));
        for rows in [8u16, 24, 60] {
            for cols in [20u16, 80, 200] {
                let frame = compose(&mut app, rows, cols);
                assert_eq!(frame.len(), rows.max(8) as usize, "{rows}x{cols}");
                for line in &frame {
                    assert!(
                        ansi::width(line) <= cols.max(20) as usize + 2,
                        "a line overflowed at {rows}x{cols}: {}",
                        ansi::width(line)
                    );
                }
            }
        }
    }

    #[test]
    fn streaming_keeps_the_viewport_anchored_after_the_user_scrolls_up() {
        let mut app = test_app();
        for n in 0..30 {
            app.blocks.push(Block::Notice {
                text: format!("history {n}"),
                bad: false,
            });
        }
        let _ = compose(&mut app, 18, 80);
        app.scroll = 6;
        let before = compose(&mut app, 18, 80);
        let top_before = ansi::plain(&before[0]);

        for n in 0..12 {
            app.blocks.push(Block::Notice {
                text: format!("streamed {n}"),
                bad: false,
            });
        }
        let after = compose(&mut app, 18, 80);
        assert_eq!(
            ansi::plain(&after[0]),
            top_before,
            "streaming pulled the reader downward"
        );
        assert!(
            app.scroll > 6,
            "distance from the growing bottom was not compensated"
        );

        app.scroll = 0;
        let followed = compose(&mut app, 18, 80);
        let text = followed
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("streamed 11"),
            "returning to the bottom did not resume follow"
        );
    }

    /// The transcript cache must not merely produce the right lines — it must
    /// not redraw what did not change, because redrawing the whole session
    /// ten times a second while one step runs is the jank itself.
    ///
    /// Counted through a real markdown body, so a cache that "reuses" by
    /// skipping the drawing would be caught: a group is only reused when its
    /// own content is byte-identical.
    #[test]
    fn only_the_group_that_changed_is_drawn_again() {
        let mut app = test_app();
        for n in 0..20 {
            app.blocks.push(Block::Text {
                text: format!("a settled paragraph {n} with **some** markdown in it"),
            });
        }
        let first = transcript_into(None, &app, 100, true, true);
        let first_lines = first.sink.lines.clone();
        // A second pass over an unchanged app must reproduce the same lines and
        // leave every group where it was: `whole` is off here, so nothing is
        // drawn unless a fingerprint moved.
        let again = transcript_into(Some(first), &app, 100, false, false);
        assert_eq!(again.sink.lines, first_lines, "lines drifted");
        assert_eq!(
            again.first, 0,
            "an unchanged transcript re-laid-out its head for nothing"
        );

        // Changing one block must change exactly one group's lines.
        let before: Vec<String> = again.sink.lines.clone();
        app.blocks[10] = Block::Text {
            text: "this paragraph is different now".into(),
        };
        let after = transcript_into(Some(again), &app, 100, true, true);
        let moved = before
            .iter()
            .zip(after.sink.lines.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(moved > 0, "the changed block did not reach the screen");
        assert!(
            moved < before.len() / 2,
            "{moved} of {} lines moved for one changed block — the cache is not caching",
            before.len()
        );
    }

    /// A resumed session is laid out tail-first: the first frame draws the
    /// newest screenful, and the rest of the conversation arrives on the frames
    /// after. A reader following the tail must never see a different screen
    /// because of it, and a reader who scrolls up must get the whole history.
    #[test]
    fn a_long_transcript_is_laid_out_tail_first_and_whole_on_demand() {
        let mut app = test_app();
        for n in 0..400 {
            app.blocks.push(Block::Notice {
                text: format!("history entry number {n}"),
                bad: false,
            });
        }
        // Following the tail: one screenful's worth of head, not the session.
        let partial = transcript_into(None, &app, 100, true, false);
        assert!(
            partial.first > 0,
            "the first paint laid out the entire session before showing anything"
        );
        assert!(
            partial.sink.lines.len() < app.blocks.len() * 4,
            "first paint drew {} lines for {} blocks",
            partial.sink.lines.len(),
            app.blocks.len()
        );
        // What a reader following the tail sees is the same either way.
        let tail: Vec<String> = partial.sink.lines.iter().rev().take(40).cloned().collect();
        let whole = transcript_into(Some(partial), &app, 100, false, true);
        assert_eq!(
            whole.first, 0,
            "asking for history did not lay out all of it"
        );
        let whole_tail: Vec<String> = whole.sink.lines.iter().rev().take(40).cloned().collect();
        assert_eq!(
            whole_tail, tail,
            "the newest screenful moved when the head arrived"
        );
    }

    /// A line as wide as the terminal leaves the cursor standing on its own
    /// last cell, and a trailing `EL` counts that cell — so the clear that was
    /// meant for the tail after the line would erase the wall or corner the
    /// line just drew. Only a line with a tail may be followed by one.
    #[test]
    fn the_trailing_line_clear_never_erases_the_last_column_of_a_full_width_line() {
        let mut app = test_app();
        app.blocks
            .push(Block::user("a question for the rule to wrap"));
        let cols = 80u16;
        let frame = compose(&mut app, 24, cols);
        let painted: Vec<&String> = frame.iter().filter(|l| !l.is_empty()).collect();
        assert!(
            painted.iter().any(|l| ansi::width(l) == cols as usize),
            "the you-rule should be exactly as wide as the terminal"
        );
        let mut screen = Screen::default();
        let out = screen.render(&mut app, 24, cols, true);
        let clears = out.matches("\x1b[K").count();
        let full = painted
            .iter()
            .filter(|l| ansi::width(l) >= cols as usize)
            .count();
        assert_eq!(
            clears,
            painted.len() - full,
            "a full-width line was followed by EL"
        );
    }

    /// A scroll move must not clear screen wholesale (which causes flickering),
    /// but must update the changed content rows. `ctrl+l` forces a wholesale clear and repaint.
    #[test]
    fn a_scroll_change_does_not_clear_screen_wholesale_and_resync_repaints_wholesale() {
        let mut app = test_app();
        for n in 0..40 {
            app.blocks.push(Block::Notice {
                text: format!("history {n}"),
                bad: false,
            });
        }
        let mut screen = Screen::default();
        assert!(screen
            .render(&mut app, 18, 80, true)
            .contains(ansi::CLEAR_SCREEN));
        assert!(
            !screen
                .render(&mut app, 18, 80, false)
                .contains(ansi::CLEAR_SCREEN),
            "an unchanged screen must not be repainted wholesale"
        );
        app.scroll = 5;
        let scroll_out = screen.render(&mut app, 18, 80, false);
        assert!(
            !scroll_out.contains(ansi::CLEAR_SCREEN),
            "a scroll move must never clear screen wholesale to avoid flicker"
        );
        assert!(
            !scroll_out.is_empty(),
            "changed content rows must be repainted on scroll"
        );
        assert!(!screen
            .render(&mut app, 18, 80, false)
            .contains(ansi::CLEAR_SCREEN));
        app.resync = true;
        assert!(
            screen
                .render(&mut app, 18, 80, false)
                .contains(ansi::CLEAR_SCREEN),
            "ctrl+l must repaint every row"
        );
        assert!(
            !app.resync,
            "the resync was not consumed by the paint that honoured it"
        );
    }

    /// A frame that only got taller or shorter must not clear the screen
    /// either. That is the second source of the same flicker, and it fires on
    /// ordinary typing: the composer wrapping onto a second line, a permission
    /// card opening, the sidebar gaining a row. The rows the shorter frame
    /// gave up are erased on their own; the rows above them are not touched.
    #[test]
    fn a_frame_that_changed_height_repaints_instead_of_clearing_wholesale() {
        let mut app = test_app();
        app.blocks.push(Block::user("hello"));
        let mut screen = Screen::default();
        screen.render(&mut app, 18, 80, true);

        // The composer grows: the footer takes a row, the frame does not.
        app.input = "a".repeat(200);
        app.cursor = app.input.len();
        let taller = screen.render(&mut app, 18, 80, false);
        assert!(
            !taller.contains(ansi::CLEAR_SCREEN),
            "a composer that wrapped cleared the screen"
        );

        // And shrinks back.
        app.input.clear();
        app.cursor = 0;
        let shorter = screen.render(&mut app, 18, 80, false);
        assert!(
            !shorter.contains(ansi::CLEAR_SCREEN),
            "a composer that unwrapped cleared the screen"
        );
        assert!(
            !shorter.is_empty(),
            "the rows that changed must still be repainted"
        );
    }

    #[test]
    fn paint_stream_hides_cursor_and_uses_synchronized_output_without_line_clears() {
        let mut app = test_app();
        app.sidebar = true;
        let mut screen = Screen::default();
        let out = screen.render(&mut app, 24, 100, false);
        assert!(out.starts_with("\x1b[?2026h\x1b[?25l"));
        assert!(out.ends_with("\x1b[?2026l"));
        assert!(
            !out.contains("\x1b[2K"),
            "CLEAR_LINE must not be emitted before row overwrite"
        );
    }

    #[test]
    fn multi_line_command_in_terminal_box_does_not_break_borders_or_split_divider() {
        let mut app = test_app();
        app.sidebar = true;
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run test".into());
        let multiline_cmd = "python3 -c '\nimport os, sys\n\nrows, cols = os.get_terminal_size()\nprint(f\"size: {rows}x{cols}\")\n'";
        tool.args = Some(json!({"command": multiline_cmd}));
        tool.output = "$ python3 -c '\nimport os, sys\n\nsize: 200x40\n[exit 0]\n".to_string();
        tool.state = ToolState::Ok;
        app.blocks.push(Block::of(tool));

        let frame = compose(&mut app, 24, 100);
        for (i, row) in frame.iter().enumerate() {
            assert!(
                !row.contains('\n') && !row.contains('\r'),
                "row {} contains newline or carriage return",
                i + 1
            );
        }

        let plain_frame: Vec<String> = frame.iter().map(|l| ansi::plain(l)).collect();
        let top_idx = plain_frame
            .iter()
            .position(|r| r.contains("╭─ Bash"))
            .expect("top border not found");
        let bottom_idx = plain_frame[top_idx..]
            .iter()
            .position(|r| r.contains('╰'))
            .expect("bottom border not found")
            + top_idx;
        for (i, row) in plain_frame[top_idx..=bottom_idx].iter().enumerate() {
            assert!(!row.contains('\n') && !row.contains('\r'));
            // Every row across the content area must have the split divider │
            assert!(row.contains('│'), "row lost split divider: {row}");
            if i > 0 && i < bottom_idx - top_idx {
                if row.contains('├') {
                    // Divider row between input and output
                    assert!(row.contains('┤'), "divider row lost right corner: {row}");
                } else {
                    // Inner box row: must have left wall │, right wall │, and split divider │
                    assert!(
                        row.matches('│').count() >= 3,
                        "inner box row lost border or split divider: {row}"
                    );
                }
            }
        }
    }

    #[test]
    fn boxed_sanitizes_newlines_and_preserves_single_row_invariant() {
        let b = boxed("line1\nline2\r\nline3", 30);
        assert!(!b.contains('\n') && !b.contains('\r'));
        let plain = ansi::plain(&b);
        assert!(plain.starts_with('│') && plain.ends_with('│'));
    }

    /// A terminal in miniature, answering one question: after the bytes Rune
    /// sends, is the screen the frame the renderer meant to draw?
    ///
    /// It models exactly the two behaviours the painter leans on — `EL 0`
    /// erases from the cursor's cell to the margin, and with auto-wrap off the
    /// cursor *sticks* on the last column instead of moving past it (which is
    /// why a full-width line must never be followed by `EL`). Everything else —
    /// colour, links, modes — is consumed and ignored.
    struct Vt {
        cells: Vec<Vec<char>>,
        r: usize,
        c: usize,
        /// The scroll margins (DECSTBM), top row and one past the bottom.
        margins: (usize, usize),
    }

    impl Vt {
        fn new(rows: usize, cols: usize) -> Self {
            Vt {
                cells: vec![vec![' '; cols]; rows],
                r: 0,
                c: 0,
                margins: (0, rows),
            }
        }

        fn feed(&mut self, bytes: &str) {
            let chars: Vec<char> = bytes.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '\x1b' if i + 1 < chars.len() && chars[i + 1] == '[' => {
                        let mut j = i + 2;
                        while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                            j += 1;
                        }
                        let Some(&cmd) = chars.get(j) else { break };
                        self.csi(&chars[i + 2..j].iter().collect::<String>(), cmd);
                        i = j + 1;
                    }
                    // OSC (hyperlinks): swallow up to BEL or ST.
                    '\x1b' if i + 1 < chars.len() && chars[i + 1] == ']' => {
                        let mut j = i + 2;
                        while j < chars.len() {
                            if chars[j] == '\x07' {
                                j += 1;
                                break;
                            }
                            if chars[j] == '\x1b' && chars.get(j + 1) == Some(&'\\') {
                                j += 2;
                                break;
                            }
                            j += 1;
                        }
                        i = j;
                    }
                    '\x1b' => i += 2,
                    '\r' => {
                        self.c = 0;
                        i += 1;
                    }
                    '\n' => {
                        self.r = (self.r + 1).min(self.cells.len() - 1);
                        i += 1;
                    }
                    c if (c as u32) >= 0x20 => {
                        self.put(c);
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
        }

        fn put(&mut self, ch: char) {
            let cols = self.cells[0].len();
            if self.c < cols {
                self.cells[self.r][self.c] = ch;
                // Auto-wrap off: the cursor stays on the last column, so an
                // `EL` from there would eat the glyph just written.
                if self.c + 1 < cols {
                    self.c += 1;
                }
            }
        }

        fn csi(&mut self, params: &str, cmd: char) {
            let nums: Vec<usize> = params
                .split(';')
                .filter_map(|p| p.trim().parse().ok())
                .collect();
            let cols = self.cells[0].len();
            match cmd {
                'H' => {
                    self.r = nums
                        .first()
                        .copied()
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .min(self.cells.len() - 1);
                    self.c = nums
                        .get(1)
                        .copied()
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .min(cols - 1);
                }
                'J' if nums.first() == Some(&2) => {
                    for row in &mut self.cells {
                        row.fill(' ');
                    }
                }
                // Column, on the current row: the seam beside the side panel.
                'G' => self.c = nums.first().copied().unwrap_or(1).max(1).min(cols) - 1,
                'r' => {
                    let rows = self.cells.len();
                    self.margins = match nums.as_slice() {
                        [top, bottom] => (top.saturating_sub(1), (*bottom).min(rows)),
                        _ => (0, rows),
                    };
                    (self.r, self.c) = (0, 0);
                }
                'S' | 'T' => {
                    let (top, bottom) = self.margins;
                    let band = &mut self.cells[top..bottom];
                    let by = nums.first().copied().unwrap_or(1).max(1).min(band.len());
                    if cmd == 'S' {
                        band.rotate_left(by);
                        let len = band.len();
                        band[len - by..].iter_mut().for_each(|row| row.fill(' '));
                    } else {
                        band.rotate_right(by);
                        band[..by].iter_mut().for_each(|row| row.fill(' '));
                    }
                }
                'K' => match nums.first().copied().unwrap_or(0) {
                    2 => self.cells[self.r].fill(' '),
                    0 => {
                        let from = self.c.min(cols - 1);
                        self.cells[self.r][from..].fill(' ');
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        fn row(&self, i: usize) -> String {
            self.cells[i]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        }
    }

    /// Every frame of a streaming session — a terminal box growing line by
    /// line, the sidebar's numbers moving under it, the turn finishing, the
    /// composer filling, the view scrolling and resyncing — must leave the
    /// screen exactly the frame the renderer composed. Any erase that fires
    /// from the wrong column, any row the diff forgot, shows up here as a row
    /// that disagrees with its own frame.
    #[test]
    fn a_streaming_session_leaves_the_screen_identical_to_every_frame_it_draws() {
        let rows = 30u16;
        let cols = 120u16;
        let mut app = test_app();
        app.sidebar = true;
        app.busy = true;

        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run the build".into());
        tool.args = Some(json!({"commands": [{"command": "cargo build --release -p runecli"}]}));
        tool.state = ToolState::Running;
        app.blocks.push(Block::user("build it please"));
        app.blocks.push(Block::of(tool));

        let mut screen = Screen::default();
        let mut vt = Vt::new(rows as usize, cols as usize);
        let mut step = 0;

        let verify = |screen: &Screen, vt: &Vt, step: usize| {
            for (i, meant) in screen.prev.iter().enumerate() {
                assert_eq!(
                    vt.row(i),
                    ansi::plain(meant).trim_end().to_string(),
                    "step {step}: screen row {} is not the frame's row",
                    i + 1
                );
            }
        };

        // The first frame.
        step += 1;
        vt.feed(&screen.render(&mut app, rows, cols, false));
        verify(&screen, &vt, step);

        // The output streams in while the sidebar's numbers move under it.
        for n in 0..36 {
            if let Some(t) = app.blocks.last_mut().and_then(|b| b.tool_mut()) {
                t.output
                    .push_str(&format!("   Compiling runeengine v0.1.0 ({n})\n"));
            }
            app.tok_in += 1_523;
            app.tok_out += 411;
            app.context = 131_072;
            step += 1;
            vt.feed(&screen.render(&mut app, rows, cols, false));
            verify(&screen, &vt, step);
        }

        // The turn settles, the composer fills, the view scrolls and returns.
        if let Some(t) = app.blocks.last_mut().and_then(|b| b.tool_mut()) {
            t.state = ToolState::Ok;
            t.content = "Finished `release` profile [optimized]".into();
        }
        app.busy = false;
        for typed in ["w", "wh", "what", "what next"] {
            app.input = typed.into();
            app.cursor = typed.len();
            step += 1;
            vt.feed(&screen.render(&mut app, rows, cols, false));
            verify(&screen, &vt, step);
        }
        for scroll in [6, 3, 9, 0] {
            app.scroll = scroll;
            step += 1;
            vt.feed(&screen.render(&mut app, rows, cols, false));
            verify(&screen, &vt, step);
        }
        app.resync = true;
        step += 1;
        vt.feed(&screen.render(&mut app, rows, cols, false));
        verify(&screen, &vt, step);
        assert_eq!(step, 46);
    }

    #[test]
    fn a_drag_selects_across_rows_and_the_renderer_marks_it() {
        let selection = Selection {
            anchor: Point { row: 1, col: 3 },
            head: Point { row: 2, col: 4 },
            moved: true,
        };
        let rows = vec!["abcdef".to_string(), "ghijkl".to_string()];
        assert_eq!(selection.selected_text(&rows), "cdef\nghij");

        let mut app = test_app();
        app.selection = Some(selection);
        let frame = compose(&mut app, 18, 80);
        assert!(
            frame[0].contains(ansi::REVERSE),
            "the selected cells were not highlighted"
        );
    }

    #[test]
    fn a_narrow_terminal_does_not_split_however_much_it_is_asked_to() {
        let mut app = test_app();
        app.sidebar = true;
        // Measured at the column the split would fall on, because the composer
        // is a box now and its own frame is made of the same character.
        let narrow = compose(&mut app, 24, 60);
        assert!(
            narrow.iter().all(|l| at_column(l, 40) != Some('│')),
            "60 columns is too narrow for two of them"
        );
        let wide = compose(&mut app, 24, 120);
        assert!(wide
            .iter()
            .any(|l| at_column(l, 120 * 66 / 100) == Some('│')));
    }

    /// The character standing at a given **column** — not at a given index.
    ///
    /// Written out because getting it wrong is exactly the bug under test: the
    /// first version of this check used `chars().position()`, reported the
    /// divider as crooked, and was itself the only crooked thing involved.
    fn at_column(line: &str, column: usize) -> Option<char> {
        let mut used = 0;
        for c in ansi::plain(line).chars() {
            if used == column {
                return Some(c);
            }
            used += ansi::width(&c.to_string());
        }
        None
    }

    /// The emoji width bug bent this line. Measured on the composed frame, not
    /// on the function that was wrong, so the fix is checked where it mattered.
    #[test]
    fn the_split_divider_is_straight_even_through_emoji_and_wide_text() {
        let mut app = test_app();
        app.sidebar = true;
        app.blocks.push(Block::Text {
            text: "- ✅ Membaca file\n- ❌ Tidak bisa\n- ⚡ 日本語のテキスト\n- plain ascii".into(),
        });
        let cols = 120usize;
        let frame = compose(&mut app, 20, cols as u16);
        let divider = (cols * 66 / 100).max(40);

        let content = frame.len() - footer(&mut app, 20, cols).0.len();
        for (n, line) in frame[..content].iter().enumerate() {
            assert_eq!(
                at_column(line, divider),
                Some('│'),
                "row {n} has the divider somewhere else: {:?}",
                ansi::plain(line)
            );
        }
    }

    /// The owner's photograph: `ls` output running clean through the divider,
    /// with `│` scattered at random columns to the right of it. A pty ends every
    /// line with `\r\n`, and a `\r` occupies no column while throwing the cursor
    /// back to column one — so the divider drawn after it landed inside the text.
    #[test]
    fn what_a_command_printed_cannot_punch_through_the_divider() {
        let mut app = test_app();
        app.sidebar = true;
        let mut t = Tool::new("c1".into(), "terminal_use".into(), "list them".into());
        t.settle(&json!({ "command": "ls ~/Downloads/" }));
        // Exactly what comes back through a pty, tab-aligned and CR-terminated.
        t.output = "catur-pro\tsrc_fixed.zip\r\ndemo.docx\tnotes.md\r\n\x1b[2Kbug1.png\r".into();
        t.state = ToolState::Ok;
        app.blocks.push(Block::of(t));

        let cols = 120usize;
        let frame = compose(&mut app, 24, cols as u16);
        let divider = (cols * 66 / 100).max(40);
        let content = frame.len() - footer(&mut app, 24, cols).0.len();
        for (n, line) in frame[..content].iter().enumerate() {
            assert_eq!(
                at_column(line, divider),
                Some('│'),
                "row {n} lost the divider: {:?}",
                ansi::plain(line)
            );
            assert!(
                !ansi::plain(line).contains('\r'),
                "row {n} still carries a carriage return"
            );
        }
    }

    /// The owner wrote a thirty-item plan and then could not see one item of it
    /// without hiding the work it described. Then reported that the side panel
    /// itself **truncated**.
    ///
    /// And the test written for it was green the whole time, because it
    /// called `sidebar()` directly and never went through `compose`. It proved
    /// a function it called was correct; the cut happens one layer up, where
    /// `compose` keeps only as many rows as the terminal has. Thirty items in a
    /// 24-row terminal showed nine and said nothing at all about the other
    /// twenty-one. These tests go through the frame.
    #[test]
    fn a_long_plan_says_what_it_is_hiding_and_can_be_scrolled_to() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| {
                json!({
                    "text": format!("step {n}"),
                    "status": if n < 3 { "done" } else if n == 3 { "doing" } else { "todo" }
                })
            })
            .collect();
        let text = frame_text(&mut app, 24, 120);
        assert!(text.contains("PLAN 3/30"), "{text}");
        // The panel is scrolled to the live end, and it must SAY that it is
        // scrolled. This is the fault being fixed: the old panel cut the list
        // and said nothing, so a reader could not know the other items existed.
        assert!(
            text.contains("rows above"),
            "a plan that does not fit must say what is hidden above: {text}"
        );
        // And the mark may never name rows the wheel cannot reach: at the
        // live end the tail is on screen, so there is no "below" to claim.
        assert!(
            !text.contains("rows below"),
            "the panel is at the far end and must not claim more below: {text}"
        );
        assert!(
            text.contains("step 29"),
            "the far end must bring the last item into view: {text}"
        );
        // The live end is what a plan is normally read at, so that is where it
        // opens: the work in progress, not the first thing written.
        assert!(
            !text.contains("step 0"),
            "the panel opened at the top instead of the live end: {text}"
        );
    }
    /// Every item of a long plan must be reachable, which is the promise the
    /// old panel broke by dropping them.
    #[test]
    fn scrolling_the_panel_reaches_both_ends_of_a_long_plan() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        // At the live end: the last item is on screen and nothing is claimed
        // below it. The old arithmetic charged the two markers against the
        // limit and left the last rows permanently hidden behind a
        // `↓ 2 rows below` no amount of wheeling could reach.
        let at_end = frame_text(&mut app, 24, 120);
        assert!(
            !at_end.contains("rows below"),
            "the far end must not claim rows below: {at_end}"
        );
        let last_shown = last_plan_item(&at_end);
        assert_eq!(
            last_shown, 29,
            "the far end must show the last item: {at_end}"
        );

        // Scrolled all the way up.
        let max = app.plan_max;
        assert!(max > 0, "a 30-item plan in 24 rows must be scrollable");
        app.plan_scroll_by(true, max, max);
        let at_top = frame_text(&mut app, 24, 120);
        assert!(
            at_top.contains("step 0"),
            "the first item is unreachable: {at_top}"
        );
        assert!(
            at_top.contains("rows below"),
            "the panel scrolled up and did not say there is more: {at_top}"
        );
        assert!(
            !at_top.contains("rows above"),
            "the panel is at the top and should not claim to be scrolled above it: {at_top}"
        );

        // One step down from the top shows one row further into the list, so
        // the window's head moves down — the item that left is the proof. The
        // tail is the wrong measure here: the top marker's row appears with
        // the first scroll, the window narrows by that row, and the tail stays
        // pinned while the head advances.
        app.plan_scroll_by(false, 1, max);
        let moved = frame_text(&mut app, 24, 120);
        assert!(
            first_plan_item(&moved) > first_plan_item(&at_top),
            "scrolling down did not move the window down the list"
        );

        // And all the way back down reaches the newest row, which is where a
        // plan is read while it is being carried out.
        app.plan_scroll_by(false, max, max);
        let back = frame_text(&mut app, 24, 120);
        assert_eq!(
            last_plan_item(&back),
            last_shown,
            "scrolling to the end did not return to the live end"
        );
    }

    /// A plan that fits must not pretend to be scrollable, and the numbers must
    /// clamp so a shrinking plan cannot leave the panel showing empty rows.
    #[test]
    fn a_plan_that_fits_is_not_scrollable_and_the_scroll_clamps() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..2)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let text = frame_text(&mut app, 24, 120);
        assert!(text.contains("step 0") && text.contains("step 1"), "{text}");
        assert_eq!(app.plan_max, 0, "a two-item plan has nothing to scroll");
        assert!(
            !text.contains("rows above") && !text.contains("rows below"),
            "a plan that fits must not claim it is hiding something: {text}"
        );
        // A scroll that cannot go anywhere must stay at zero, not go negative
        // and wrap the panel.
        app.plan_scroll_by(true, 5, 0);
        assert_eq!(app.plan_scroll, 0);
    }

    // ─── FILE CHANGE: the third section, in the same scroll window ──────────────

    /// The tally the engine delivered, drawn beside the plan it sits under:
    /// a heading with the session totals, and one row per path with its own
    /// counts on the same line.
    #[test]
    fn the_file_change_section_carries_its_totals_and_one_row_per_path() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = vec![json!({ "text": "step 0", "status": "done" })];
        app.file_changes = vec![
            json!({ "path": "src/store.rs", "change": "modified", "added": 24, "removed": 3 }),
            json!({ "path": "new.txt", "change": "created", "added": 6, "removed": 0 }),
        ];
        let text = frame_text(&mut app, 24, 120);

        assert!(
            text.contains("2 FILES CHANGED • +30 −3"),
            "the heading must carry the session totals: {text}"
        );
        assert!(text.contains("PLAN 1/1"), "the plan stays above it: {text}");
        // Each path and its counts on **one** line, the way an edit note reads.
        let row = text
            .lines()
            .find(|l| l.contains("src/store.rs") && l.contains("+24"))
            .expect("a file-change row must show the path and its counts together");
        assert!(
            row.contains("−3"),
            "both counts belong on the path's line: {row}"
        );
        // A created file shows only what arrived — there was nothing before it.
        let created = text
            .lines()
            .find(|l| l.contains("new.txt"))
            .expect("the created file has a row");
        assert!(created.contains("+6"), "{created}");
        assert!(
            !created.contains('−'),
            "a created file has nothing removed: {created}"
        );
    }

    /// A file the session put back, a binary, and a change too large to count
    /// do not invent numbers: each says what it is.
    #[test]
    fn a_special_file_still_counts_as_numbers_and_never_speaks() {
        let mut app = test_app();
        app.sidebar = true;
        app.file_changes = vec![
            json!({ "path": "gone.txt", "change": "deleted", "added": 0, "removed": 5 }),
            json!({ "path": "back.rs", "change": "unchanged", "added": 0, "removed": 0 }),
            json!({ "path": "blob.dat", "change": "binary", "added": 0, "removed": 0 }),
            json!({ "path": "huge.log", "change": "toolarge", "added": 910, "removed": 0 }),
        ];
        let text = frame_text(&mut app, 24, 120);

        let deleted = text
            .lines()
            .find(|l| l.contains("gone.txt"))
            .expect("the deleted file still has a row");
        // The word "deleted" is gone from the row — its all-minus counts are the
        // whole story: nothing arrived, five lines left.
        assert!(!deleted.contains("deleted"), "{deleted}");
        assert!(
            deleted.contains("−5"),
            "its whole content counts as removed: {deleted}"
        );
        let reverted = text
            .lines()
            .find(|l| l.contains("back.rs"))
            .expect("a restored file keeps its row");
        assert!(
            reverted.contains("+0") && reverted.contains("−0"),
            "a put-back file reads as its own zero arithmetic:\n{reverted}"
        );
        assert!(!reverted.contains("reverted"), "{reverted}");
        let binary = text
            .lines()
            .find(|l| l.contains("blob.dat"))
            .expect("a binary keeps its row");
        assert!(
            binary.contains("+0") && binary.contains("−0"),
            "a binary reads as its zero line arithmetic:\n{binary}"
        );
        assert!(!binary.contains("bin"), "{binary}");
        let toolarge = text
            .lines()
            .find(|l| l.contains("huge.log"))
            .expect("a too-large file keeps its row");
        assert!(
            !toolarge.contains('±') && toolarge.contains("+910"),
            "a refused diff shows the ledger's own tally:\n{toolarge}"
        );
    }

    /// The files section stands alone when the session has no plan: the counts
    /// a turn leaves behind are a fact about the session, not about the plan.
    #[test]
    fn the_file_change_section_stands_alone_without_a_plan() {
        let mut app = test_app();
        app.sidebar = true;
        app.file_changes =
            vec![json!({ "path": "a.txt", "change": "created", "added": 1, "removed": 0 })];
        let text = frame_text(&mut app, 24, 120);
        assert!(
            text.contains("1 FILES CHANGED • +1"),
            "the section exists with no plan above it: {text}"
        );
        assert!(!text.contains("PLAN "), "there is no plan to count: {text}");
    }

    /// A files-only panel whose rows overflow must still take the wheel —
    /// `plan_max` is the list's number now, plan and files together, and a
    /// section that cannot say "rows above" while the wheel scrolls the chat
    /// would be the old lie in a new place.
    #[test]
    fn files_that_overflow_own_the_wheel_and_honest_marks() {
        let mut app = test_app();
        app.sidebar = true;
        app.file_changes = (0..40)
            .map(|n| {
                json!({ "path": format!("f{N:02}.txt", N = n), "change": "created",
                        "added": 1, "removed": 0 })
            })
            .collect();
        let text = frame_text(&mut app, 24, 120);
        assert!(app.plan_max > 0, "forty rows in twenty-four cannot all fit");
        assert!(
            text.contains("rows above"),
            "the live end hides earlier files and must say so: {text}"
        );
        assert!(
            !text.contains("rows below"),
            "the live end shows the last file and must not claim more: {text}"
        );
        assert!(
            text.contains("f39.txt"),
            "the live end is where the newest file is: {text}"
        );

        // And the whole list is reachable — the far end carries the first file.
        let max = app.plan_max;
        app.plan_scroll_by(true, max, max);
        let top = frame_text(&mut app, 24, 120);
        assert!(
            top.contains("f00.txt"),
            "the first file is unreachable: {top}"
        );
        assert!(top.contains("rows below"), "{top}");
    }

    /// One scroll window over both sections: scrolling past the plan reaches
    /// the files below it, and the marks describe the combined list. The
    /// FILE CHANGE heading travels with its section — not pinned under the
    /// plan's, which is what made the two lists read as one merged blob.
    #[test]
    fn file_rows_scroll_with_the_plan_under_one_window() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        app.file_changes = (0..30)
            .map(|n| {
                json!({ "path": format!("f{N:02}.txt", N = n), "change": "created",
                        "added": 1, "removed": 0 })
            })
            .collect();
        let text = frame_text(&mut app, 24, 120);
        assert!(
            !text.contains("PLAN 0/30"),
            "hidden plan must not label file rows: {text}"
        );
        assert!(
            text.contains("FILES CHANGED"),
            "header follows the active file section: {text}"
        );

        // At the seam: the heading sits directly above the rows it names.
        // The first frame follows the live end, so reach the top first.
        let max = app.plan_max;
        app.plan_scroll_by(true, max, max);
        app.plan_scroll_by(false, 30, max);
        let seam = frame_text(&mut app, 24, 120);
        assert!(
            seam.contains("30 FILES CHANGED • +30"),
            "the heading is at its section's top, not glued under the plan's: {seam}"
        );
        assert!(
            seam.lines()
                .rev()
                .position(|l| l.contains("FILES CHANGED"))
                .is_some_and(|below| {
                    seam.lines()
                        .rev()
                        .take(below + 1)
                        .any(|l| l.contains("f00.txt"))
                }),
            "the first file row follows its heading: {seam}"
        );

        // All the way down: the files are the end of the list, and the plan's
        // first items are what the top mark is talking about.
        let max = app.plan_max;
        app.plan_scroll_by(false, max, max);
        let end = frame_text(&mut app, 24, 120);
        assert!(end.contains("f29.txt"), "the files are reachable: {end}");
        assert!(end.contains("rows above"), "{end}");
        assert!(!end.contains("rows below"), "{end}");

        // All the way back up: the plan's first item, with the files the mark
        // below is talking about.
        let max = app.plan_max;
        app.plan_scroll_by(true, max, max);
        let top = frame_text(&mut app, 24, 120);
        assert!(top.contains("step 0"), "{top}");
        assert!(top.contains("rows below"), "{top}");
        assert!(!top.contains("rows above"), "{top}");
    }

    /// The anchor keeps working across the seam: a reader parked on a file row
    /// is not moved by the plan growing above them.
    #[test]
    fn the_anchor_survives_work_across_the_plan_and_files_seam() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        app.file_changes = (0..10)
            .map(|n| {
                json!({ "path": format!("f{N:02}.txt", N = n), "change": "created",
                        "added": 1, "removed": 0 })
            })
            .collect();
        // One frame to publish the geometry, then a deliberate scroll to the
        // live end — where the newest rows of the combined list are.
        let _ = frame_text(&mut app, 24, 120);
        let max = app.plan_max;
        assert!(max > 0, "forty rows in one panel must overflow");
        app.plan_scroll_by(false, max, max);
        let _ = frame_text(&mut app, 24, 120);
        let anchored = app
            .plan_anchor
            .expect("a scrolled panel publishes what it is looking at");
        assert!(
            anchored.item >= 30,
            "the live end of the combined list is a file row: {anchored:?}"
        );

        // The plan grows while the reader is parked there.
        app.tasks
            .push(json!({ "text": "step 30", "status": "todo" }));
        let _ = frame_text(&mut app, 24, 120);
        let after = app.plan_anchor.expect("still anchored");
        assert_eq!(
            anchored.item, after.item,
            "the panel moved the reader from row {} to {}",
            anchored.item, after.item
        );
    }

    /// The reason the anchor is an item number and not a line of text.
    ///
    /// A status flip rewrites the very row the reader was looking at — `○`
    /// becomes `◐` becomes `●` — and moves the count in the heading. A text
    /// anchor loses its line and the panel jumps. The reader must not be
    /// dragged sideways by work happening underneath them.
    #[test]
    fn working_on_the_plan_does_not_move_a_scrolled_reader() {
        let mut app = test_app();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let max = app.plan_max;
        app.plan_scroll_by(true, max / 2, max);
        let before = frame_text(&mut app, 24, 120);
        let item_before = app
            .plan_anchor
            .expect("a scrolled panel publishes what it is looking at");

        // Three items above the reader finish, and three new ones are written.
        // This is what a turn does while somebody is reading the plan.
        for n in 0..3 {
            app.tasks[n]["status"] = json!("done");
        }
        for n in 30..33 {
            app.tasks
                .push(json!({ "text": format!("step {n}"), "status": "todo" }));
        }
        let after = frame_text(&mut app, 24, 120);
        let item_after = app.plan_anchor.expect("still anchored");

        assert_eq!(
            item_before.item, item_after.item,
            "the panel moved the reader from item {} to {}",
            item_before.item, item_after.item
        );
        assert_eq!(item_before.row, item_after.row);
        // The panel must have actually redrawn, or this test proves nothing.
        assert!(before.contains("rows below") || before.contains("rows above"));
        assert_ne!(before, after, "the plan changed but the frame did not");
    }

    /// Scrolling the plan must not disturb the transcript. They are two
    /// documents side by side, and one move of the reader's hand must not lose
    /// their place in the other.
    #[test]
    fn scrolling_the_plan_leaves_the_transcript_where_it_was() {
        let mut app = test_app();
        app.sidebar = true;
        for n in 0..40 {
            app.blocks.push(Block::Text {
                text: format!("answer number {n}"),
            });
        }
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let _ = frame_text(&mut app, 24, 120);
        let chat_before = app.scroll;
        let max = app.plan_max;
        app.plan_scroll_by(true, 4, max);
        let _ = frame_text(&mut app, 24, 120);
        assert_eq!(
            app.scroll, chat_before,
            "the transcript moved because the plan was scrolled"
        );
    }

    /// A long item still wraps to as many rows as its text needs. The fault was
    /// never that a long *item* was cut — it was that a long *list* vanished
    /// without a word — and this rule must survive the rewrite.
    #[test]
    fn a_long_plan_item_wraps_instead_of_being_clipped() {
        let mut app = test_app();
        app.tasks = vec![json!({
            "text": "rewrite the whole parser so that it handles every edge case the tests found",
            "status": "doing"
        })];
        let text = sidebar(&mut app, 32, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        for word in ["rewrite", "parser", "edge", "tests"] {
            assert!(
                text.contains(word),
                "a wrapped item lost the word {word:?}: {text}"
            );
        }
        assert!(
            !text.contains('…'),
            "the item was cut rather than wrapped: {text}"
        );
    }

    // ── The command palette ──────────────────────────────────────────────────

    /// A slash opens the list; the letters after it narrow the list; only two of
    /// it are ever on screen at once.
    #[test]
    fn typing_a_slash_opens_a_command_list_that_answers_to_what_follows_it() {
        let mut app = test_app();
        assert!(
            palette_lines(&app, 80).is_empty(),
            "prose must not open a list"
        );

        app.input = "/".into();
        let all = palette_lines(&app, 80);
        assert!(
            all.len() <= PALETTE_ROWS + 1,
            "more than two rows reached the screen: {all:?}"
        );
        let total = super::super::keys::COMMANDS.len();
        assert!(
            all.iter()
                .any(|l| ansi::plain(l).contains(&format!("of {total}"))),
            "the count must follow the table rather than be written twice: {all:?}"
        );

        // A letter searches, and it searches what a command *does* as well as
        // what it is called: "color" is not the name of any of them.
        app.input = "/color".into();
        let found: String = palette_lines(&app, 80)
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(found.contains("/theme"), "{found}");

        // And the two that used to be reachable only by knowing they existed.
        app.input = "/model".into();
        let found: String = palette_lines(&app, 80)
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(found.contains("/model"), "{found}");

        // Nothing matches: no list, and the line is left alone as prose.
        app.input = "/zzz".into();
        assert!(palette_lines(&app, 80).is_empty());
    }

    #[test]
    fn the_command_list_marks_the_row_the_arrows_are_on() {
        let mut app = test_app();
        app.input = "/s".into();
        let first: String = palette_lines(&app, 80)
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        app.palette_row = 1;
        let second: String = palette_lines(&app, 80)
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert_ne!(first, second, "the cursor does not move");
        assert_eq!(
            first.matches('›').count(),
            1,
            "exactly one row is chosen: {first}"
        );
        assert_eq!(second.matches('›').count(), 1);
    }

    #[test]
    fn palette_auto_scrolls_locking_cursor_at_second_slot() {
        let mut app = test_app();
        app.input = "/".into();
        let count = super::super::keys::matches(&app.input).len();

        // Moving down:
        // Row 0: selection is at the first visible line (index 0).
        app.palette_row = 0;
        app.adjust_palette_scroll(count);
        let lines_0 = palette_lines(&app, 80);
        assert_eq!(lines_0.iter().position(|l| l.contains('›')), Some(0));

        // Row 1: selection moves to slot 1 (index 1).
        app.palette_row = 1;
        app.adjust_palette_scroll(count);
        let lines_1 = palette_lines(&app, 80);
        assert_eq!(lines_1.iter().position(|l| l.contains('›')), Some(1));

        // Row 2: selection reaches slot 2 (2nd from bottom in 4 rows: 0, 1, 2, 3).
        app.palette_row = 2;
        app.adjust_palette_scroll(count);
        let lines_2 = palette_lines(&app, 80);
        assert_eq!(lines_2.iter().position(|l| l.contains('›')), Some(2));
        assert_eq!(lines_1[0], lines_2[0]);

        // Row 3: selection stays at 2nd from bottom (slot 2) and auto-scrolls.
        app.palette_row = 3;
        app.adjust_palette_scroll(count);
        let lines_3 = palette_lines(&app, 80);
        assert_eq!(lines_3.iter().position(|l| l.contains('›')), Some(2));
        assert_ne!(lines_2[0], lines_3[0]);

        // Row 4: selection stays at 2nd from bottom (slot 2) and auto-scrolls again.
        app.palette_row = 4;
        app.adjust_palette_scroll(count);
        let lines_4 = palette_lines(&app, 80);
        assert_eq!(lines_4.iter().position(|l| l.contains('›')), Some(2));
        assert_ne!(lines_3[0], lines_4[0]);

        // Moving up:
        // Move up to Row 3: selection moves to slot 1 (2nd from top: index 1).
        app.palette_row = 3;
        app.adjust_palette_scroll(count);
        let lines_up_3 = palette_lines(&app, 80);
        assert_eq!(lines_up_3.iter().position(|l| l.contains('›')), Some(1));
        assert_eq!(lines_4[0], lines_up_3[0]);

        // Move up to Row 2: selection stays at 2nd from top (slot 1) and auto-scrolls up.
        app.palette_row = 2;
        app.adjust_palette_scroll(count);
        let lines_up_2 = palette_lines(&app, 80);
        assert_eq!(lines_up_2.iter().position(|l| l.contains('›')), Some(1));
        assert_ne!(lines_up_3[0], lines_up_2[0]);

        // Move up to Row 1: selection stays at 2nd from top (slot 1) and auto-scrolls up to top.
        app.palette_row = 1;
        app.adjust_palette_scroll(count);
        let lines_up_1 = palette_lines(&app, 80);
        assert_eq!(lines_up_1.iter().position(|l| l.contains('›')), Some(1));
        assert_eq!(app.palette_scroll, 0);

        // Move up to Row 0: selection reaches row 0.
        app.palette_row = 0;
        app.adjust_palette_scroll(count);
        let lines_up_0 = palette_lines(&app, 80);
        assert_eq!(lines_up_0.iter().position(|l| l.contains('›')), Some(0));
    }

    // ── Turns and answers ────────────────────────────────────────────────────

    /// The separation is a rule from edge to edge — not a colour, which vanishes
    /// under a translucent terminal, and not a filled card, which reads as a
    /// black box pasted over the wallpaper. Both were tried; this is what the
    /// owner asked for after seeing them.
    #[test]
    fn a_question_is_fenced_off_from_its_answer_by_a_rule_across_the_width() {
        let mut app = test_app();
        app.blocks.push(Block::user("what does this do"));
        app.blocks.push(Block::Text {
            text: "it adds two numbers".into(),
        });
        let frame = compose(&mut app, 24, 80);
        let at = |needle: &str| {
            frame
                .iter()
                .position(|l| ansi::plain(l).contains(needle))
                .unwrap()
        };
        let (question, answer) = (at("what does this"), at("it adds two"));

        // Weight, not boldface. `USER` is the strongest ink on the screen, and
        // bold on top of it made the question heavier than the answer — the
        // owner's words were that the input was too thick.
        assert!(
            frame[question].contains(&USER.to_string()),
            "the user's own words carry weight"
        );
        assert!(
            !frame[question].contains(ansi::BOLD),
            "the user's line went bold again"
        );
        assert!(
            ansi::plain(&frame[question - 1]).starts_with('╭'),
            "no rule above the question"
        );
        assert!(
            ansi::plain(&frame[question + 1]).starts_with('╰'),
            "no rule below the question"
        );
        assert!(
            ansi::plain(&frame[question - 1]).contains("you"),
            "the rule names whose turn it is"
        );
        assert!(question < answer);
        // Nothing paints over the terminal's own background any more.
        assert!(
            !frame[question].contains("\x1b[48;"),
            "the filled card came back"
        );
    }

    #[test]
    fn user_rule_renders_timestamp_cleanly_without_overflow() {
        let plain_no_time = ansi::plain(&rule_user(None, 80));
        assert_eq!(ansi::width(&plain_no_time), 80);
        assert!(plain_no_time.starts_with("╭─ you ──"));

        let with_time = ansi::plain(&rule_user(Some("14:32"), 80));
        assert_eq!(ansi::width(&with_time), 80);
        assert!(with_time.starts_with("╭─ you · 14:32 ──"));
    }

    #[test]
    fn user_preview_counts_visual_rows_and_cache_tracks_its_display_only_fold() {
        let mut app = test_app();
        let text = format!("{}\nlast row", "long-user-word ".repeat(90));
        app.blocks.push(Block::user(&text));
        for width in [50, 80, 120] {
            let visual = ansi::wrap(&text, width - 4);
            let sink = transcript(&mut app, width);
            let plain: Vec<_> = sink.lines.iter().map(|line| ansi::plain(line)).collect();
            let top = plain.iter().position(|line| line.contains("you")).unwrap();
            let cut = super::super::viewport::cut_lines(visual.len() - 4);
            assert!(plain[top + 5].contains(&cut), "width {width}: {plain:?}");
            assert!(plain[top + 6].starts_with('╰'));
            assert_eq!(sink.owner[top + 5], Hit::Block(0));
            for (row, line) in visual.iter().take(4).enumerate() {
                assert!(plain[top + row + 1].contains(line));
            }
            if let Block::User { open, .. } = &mut app.blocks[0] {
                *open = true;
            }
            let expanded = transcript(&mut app, width);
            assert!(expanded
                .lines
                .iter()
                .any(|line| ansi::plain(line).contains("last row")));
            assert!(expanded
                .lines
                .iter()
                .all(|line| !ansi::plain(line).contains("lines cut")));
            if let Block::User { open, .. } = &mut app.blocks[0] {
                *open = false;
            }
        }
        assert!(matches!(&app.blocks[0],Block::User{text:stored,..} if stored==&text));
    }

    #[test]
    fn user_turn_renders_as_a_closed_full_width_box() {
        let mut app = test_app();
        app.blocks.push(Block::User {
            text: "testing\na second line".into(),
            time: Some("19:41".into()),
            media: Vec::new(),
            open: false,
        });
        let rows = transcript(&mut app, 120)
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>();
        let top = rows
            .iter()
            .position(|line| line.contains("you · 19:41"))
            .unwrap();
        assert!(rows[top].starts_with('╭') && rows[top].ends_with('╮'));
        assert!(rows[top + 1].starts_with('│') && rows[top + 1].ends_with('│'));
        assert!(rows[top + 2].starts_with('│') && rows[top + 2].ends_with('│'));
        assert!(rows[top + 3].starts_with('╰') && rows[top + 3].ends_with('╯'));
        assert!(rows[top..top + 4]
            .iter()
            .all(|line| ansi::width(line) == 120));
    }

    /// A notification is Rune's, drawn in a closed box under its own header —
    /// the sentence a person reads, without the tag the model reads it by.
    #[test]
    fn system_notification_renders_as_a_closed_box_with_system_header() {
        let mut app = test_app();
        app.blocks.push(Block::Notification {
            text: "<system_notification>Background task bg_12345 finished: Completed. Call manage_process with process_id: bg_12345 for its result.</system_notification>".into(),
            time: Some("15:30".into()),
        });
        let rows = transcript(&mut app, 120)
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>();
        let top = rows
            .iter()
            .position(|line| line.contains("System_Notification · 15:30"))
            .expect("System_Notification header with timestamp must be present");
        assert!(rows[top].starts_with('╭') && rows[top].ends_with('╮'));
        assert!(rows
            .iter()
            .any(|line| line.contains("Background task bg_12345 finished: Completed.")));
        assert!(
            !rows
                .iter()
                .any(|line| line.contains("system_notification>")),
            "the tag is the model's, not the reader's"
        );
        let bottom = rows
            .iter()
            .skip(top)
            .position(|line| line.starts_with('╰') && line.ends_with('╯'))
            .expect("Box must close with bottom border");
        assert!(bottom > 0);
    }

    /// A rule's name and its time are words on the screen, so they are neutral
    /// ink under every theme. The separator between them is the one mark that
    /// carries the theme — it is the thing the eye finds the line by.
    #[test]
    fn a_rules_name_and_time_stay_neutral_and_only_its_bullet_is_themed() {
        for (rule, name) in [
            (rule_user(Some("14:32"), 80), "you"),
            (
                rule_system_notification(Some("14:32"), 80),
                "System_Notification",
            ),
        ] {
            let plain = ansi::plain(&rule);
            assert!(plain.contains(name), "the rule must name {name}: {plain}");
            assert!(
                !rule.contains(&ACCENT.to_string()),
                "{name} followed the theme: {plain}"
            );
            assert!(
                !rule.contains(&FAINT.to_string()),
                "the time beside {name} followed the theme: {plain}"
            );
            let bullet = format!("{RULE} · ");
            let after = rule
                .split(&bullet)
                .nth(1)
                .unwrap_or_else(|| panic!("{name} lost its bullet: {plain}"));
            assert!(
                after.starts_with(&MUTED.to_string()),
                "the time is content, not a bullet: {plain}"
            );
        }
    }

    /// The notification body is the harness's own report, and a report is read —
    /// never displayed in the theme's colours.
    #[test]
    fn a_system_notifications_body_is_neutral_ink() {
        let mut app = test_app();
        app.blocks.push(Block::Notification {
            text: "<system_notification>Background task bg_1 finished: Completed.</system_notification>".into(),
            time: Some("15:30".into()),
        });
        let rows = transcript(&mut app, 120).lines;
        let body = rows
            .iter()
            .find(|line| ansi::plain(line).contains("Completed"))
            .expect("the notification body must be shown");
        assert!(
            !body.contains(&ACCENT.to_string()),
            "the notification body followed the theme: {body:?}"
        );
        assert!(body.contains(&TEXT.to_string()));
    }

    /// A user's message that happens to contain the tag is still the user's:
    /// only the engine decides what a notification is.
    #[test]
    fn a_users_message_naming_the_tag_is_not_a_notification() {
        let mut app = test_app();
        app.blocks.push(Block::User {
            text: "what does <system_notification> mean?".into(),
            time: Some("15:30".into()),
            media: Vec::new(),
            open: false,
        });
        let rows = transcript(&mut app, 120)
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>();
        assert!(!rows.iter().any(|line| line.contains("System_Notification")));
        assert!(rows
            .iter()
            .any(|line| line.contains("what does <system_notification> mean?")));
    }

    /// A failed step still shows everything the tool printed, in the same ink it
    /// would have been in had it succeeded. The red is the bullet's job, and the
    /// error box's.
    #[test]
    fn a_failed_steps_output_is_not_repainted_and_a_real_error_still_is_red() {
        let app = test_app();
        let mut terminal = Tool::new("c1".into(), "terminal_use".into(), "build".into());
        terminal.settle(&json!({ "command": "cargo build" }));
        terminal.state = ToolState::Failed;
        terminal.open = true;
        terminal.output =
            "error[E0308]: mismatched types\n<system_notice>\n  --> src/main.rs:4:5\n</system_notice>"
                .into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &terminal, true, 100, &mut sink);

        let printed: Vec<&String> = sink
            .lines
            .iter()
            .filter(|line| ansi::plain(line).contains("E0308"))
            .collect();
        assert!(
            !printed.is_empty(),
            "the failed output must still be shown: {:?}",
            sink.lines
        );
        for line in printed {
            assert!(
                !line.contains(&REMOVED.to_string()),
                "a failed step repainted its output red: {line:?}"
            );
            assert!(line.contains(&MUTED.to_string()));
        }

        // The harness's own notice is content too, and is no longer the most
        // colourful thing in the box.
        let notice = sink
            .lines
            .iter()
            .find(|line| ansi::plain(line).contains("<system_notice>"))
            .expect("the system notice must be shown");
        assert!(
            !notice.contains(&ACCENT.to_string()) && !notice.contains(&FAINT.to_string()),
            "the system notice followed the theme: {notice:?}"
        );

        // What is genuinely an error stays red.
        let mut err = Sink::default();
        draw_error_box(
            "error[E0308]: mismatched types",
            "error",
            true,
            "     ",
            100,
            &mut err,
            0,
        );
        assert!(
            err.lines.iter().any(|l| l.contains(&REMOVED.to_string())),
            "a real tool error must stay red: {:?}",
            err.lines
        );
    }

    /// One long custom answer used to vanish past the eight-row budget with
    /// nothing said about it, because the count was in questions and the loss was
    /// in rows. Both are now counted, and the loss is always said.
    #[test]
    fn a_long_single_answer_says_how_many_lines_it_hid() {
        let mut tool = Tool::new(
            "c1".into(),
            "ask_user_questions".into(),
            "ask a question".into(),
        );
        let long = (0..40)
            .map(|i| format!("a detail nobody can read past row eight {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        tool.content = serde_json::to_string(&serde_json::json!({
            "status": "answered",
            "answers": [{ "id": "q0", "selected": [], "custom": long }]
        }))
        .unwrap();
        tool.args = Some(serde_json::json!({
            "questions": [{
                "question": "Describe the change in detail.",
                "type": "single",
                "options": [{"label": "One line", "description": "Say it briefly."}]
            }]
        }));
        let mut sink = Sink::default();
        draw_questions_preview(&tool, false, true, 100, &mut sink, 0);
        let plain = sink
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("lines cut"),
            "omitted rows were dropped without saying so:\n{plain}"
        );
        // Open, the same step shows everything and claims nothing is cut.
        let mut open = Sink::default();
        draw_questions_preview(&tool, true, true, 100, &mut open, 0);
        let open_plain = open
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !open_plain.contains("lines cut"),
            "an open step hid nothing and said it did:\n{open_plain}"
        );
    }

    /// An answer with neither an option chosen nor anything typed still has to
    /// leave a row behind — and, having hidden nothing, must claim nothing.
    #[test]
    fn questions_preview_survives_an_answer_with_neither_option_nor_text() {
        let mut tool = Tool::new(
            "c1".into(),
            "ask_user_questions".into(),
            "ask a question".into(),
        );
        tool.content = serde_json::to_string(&serde_json::json!({
            "status": "answered",
            "answers": [{ "id": "q0", "selected": [], "custom": "" }]
        }))
        .unwrap();
        tool.args = Some(serde_json::json!({
            "questions": [{
                "title": "Details",
                "question": "Describe the change in detail.",
                "type": "single",
                "options": [{"label": "One line", "description": "Say it briefly."}]
            }]
        }));
        let mut sink = Sink::default();
        draw_questions_preview(&tool, false, true, 100, &mut sink, 0);
        let plain = sink
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("—"), "an empty answer says so:\n{plain}");
        assert!(
            !plain.contains("cut") && !plain.contains("more lines"),
            "nothing was hidden, so nothing may be claimed:\n{plain}"
        );
    }

    /// A form the user refused is not a form that renders.
    #[test]
    fn questions_preview_draws_nothing_when_there_is_nothing_to_show() {
        let mut tool = Tool::new(
            "c1".into(),
            "ask_user_questions".into(),
            "ask a question".into(),
        );
        tool.content =
            serde_json::to_string(&serde_json::json!({ "status": "cancelled" })).unwrap();
        let mut sink = Sink::default();
        draw_questions_preview(&tool, false, true, 100, &mut sink, 0);
        assert!(
            sink.lines.is_empty(),
            "a cancelled form left {:?} on screen",
            sink.lines
        );
    }

    /// The form's ids belong to the system: the model's arguments carry none,
    /// and the answers speak the names the engine assigned when it opened the
    /// form. Matching answers against those unnamed arguments used to fall
    /// back to printing the ids themselves — "q0 → q0o0" where the user had
    /// answered in words. Naming the form the way the engine did is what
    /// makes the step read as the conversation it was.
    #[test]
    fn an_answered_form_reads_as_words_not_system_ids() {
        let mut tool = Tool::new(
            "c1".into(),
            "ask_user_questions".into(),
            "ask a question".into(),
        );
        tool.args = Some(serde_json::json!({
            "questions": [
                {
                    "title": "Sync Strategy",
                    "question": "Which synchronization features should be enabled?",
                    "type": "multiple",
                    "options": [
                        {"label": "Automatic sync", "description": "Sync whenever files change."},
                        {"label": "Manual sync", "description": "Sync only when requested."}
                    ]
                },
                {
                    "question": "Where should the workspace files be stored?",
                    "type": "single",
                    "options": [
                        {"label": "Local files", "description": "Keep files on this device."},
                        {"label": "Cloud storage", "description": "Keep files in the cloud."}
                    ]
                }
            ]
        }));
        tool.content = serde_json::to_string(&serde_json::json!({
            "status": "answered",
            "answers": [
                { "id": "q0", "selected": ["q0o0", "q0o1"], "custom": "Only on Wi-Fi" },
                { "id": "q1", "selected": ["q1o1"], "custom": "" }
            ]
        }))
        .unwrap();
        let mut sink = Sink::default();
        draw_questions_preview(&tool, true, true, 100, &mut sink, 0);
        let plain = sink
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("Which synchronization features should be enabled?"),
            "the question the user answered must be on screen:\n{plain}"
        );
        assert!(
            plain.contains("Automatic sync") && plain.contains("Manual sync"),
            "the chosen options must read as their labels:\n{plain}"
        );
        assert!(
            plain.contains("Only on Wi-Fi"),
            "a custom answer must appear as typed:\n{plain}"
        );
        assert!(
            plain.contains("Where should the workspace files be stored?"),
            "a question without a model title still reads as itself:\n{plain}"
        );
        assert!(
            plain.contains("Cloud storage"),
            "the chosen single option must read as its label:\n{plain}"
        );
        assert!(
            !plain.contains("q0") && !plain.contains("q1"),
            "a system id reached the screen:\n{plain}"
        );
    }

    #[test]
    fn an_answer_is_rendered_as_markdown() {
        let mut app = test_app();
        app.blocks.push(Block::Text {
            text: "# Heading\n\n- one\n- two\n\n`code`".into(),
        });
        let text = screen(&mut app, 40, 80);
        assert!(text.contains('•'), "a list must render as a list");
        assert!(
            !text.contains("# Heading"),
            "the marker is styling, not content"
        );
        assert!(!text.contains("`code`"), "backticks are styling too");
    }

    // ── Steps ────────────────────────────────────────────────────────────────

    #[test]
    fn a_step_is_headed_by_the_models_sentence_and_never_by_the_tool_name() {
        let mut app = test_app();
        app.busy = true;
        let mut t = Tool::new(
            "c1".into(),
            "inspect_file".into(),
            "check how grants work".into(),
        );
        t.settle(&json!({ "path": "src/perm.rs", "view_range": [10, 40] }));
        t.state = ToolState::Ok;
        app.blocks.push(Block::of(t));

        let text = screen(&mut app, 30, 100);
        assert!(text.contains("check how grants work"));
        assert!(
            !text.contains("inspect_file"),
            "an internal identifier reached the screen"
        );
        assert!(
            text.contains("Read"),
            "the bullet needs a verb a person would use"
        );
        assert!(text.contains("src/perm.rs"));
        assert!(text.contains("10 - 40"));
    }

    #[test]
    fn a_terminal_step_shows_the_command_and_its_output_rather_than_a_bullet() {
        let mut app = test_app();
        app.busy = true;
        let mut t = Tool::new("c1".into(), "terminal_use".into(), "build it".into());
        t.settle(&json!({ "command": "cargo build" }));
        t.output = "Compiling runecli\nFinished in 3s".into();
        t.state = ToolState::Ok;
        app.blocks.push(Block::of(t));

        let text = screen(&mut app, 30, 100);
        assert!(text.contains("$ cargo build"));
        assert!(text.contains("Finished in 3s"));
        assert!(!text.contains("terminal_use"));
    }

    #[test]
    fn a_run_of_steps_shows_all_activities_without_preview_cap() {
        let mut app = test_app();
        app.busy = true;
        for n in 0..7 {
            let mut t = Tool::new(format!("c{n}"), "inspect_file".into(), format!("step {n}"));
            t.state = ToolState::Ok;
            app.blocks.push(Block::of(t));
        }
        let text = screen(&mut app, 40, 100);
        assert!(
            !text.contains("earlier steps"),
            "preview cap should be removed: {text}"
        );
        assert!(text.contains("step 0"), "first step must stay visible");
        assert!(text.contains("step 6"), "newest step must stay visible");
    }

    #[test]
    fn verbose_reveals_each_tools_detail_without_hiding_its_siblings() {
        let mut app = test_app();
        for n in 0..7 {
            let mut t = Tool::new(format!("c{n}"), "inspect_file".into(), format!("step {n}"));
            t.state = ToolState::Ok;
            t.content = format!("detail {n}");
            app.blocks.push(Block::of(t));
        }
        app.verbose = true;
        let text = screen(&mut app, 60, 100);
        assert!(text.contains("step 0") && text.contains("detail 0"));
        assert!(text.contains("step 6") && text.contains("detail 6"));
    }

    #[test]
    fn a_failed_step_remains_visible_in_the_timeline() {
        let mut app = test_app();
        app.busy = true;
        for n in 0..8 {
            let mut t = Tool::new(format!("c{n}"), "inspect_file".into(), format!("step {n}"));
            t.state = if n == 1 {
                ToolState::Failed
            } else {
                ToolState::Ok
            };
            app.blocks.push(Block::of(t));
        }
        let text = screen(&mut app, 60, 100);
        assert!(text.contains("step 1"), "a failure must survive:\n{text}");
    }

    #[test]
    fn a_terminal_refusal_is_rendered_once_not_as_content_and_summary() {
        let app = test_app();
        let mut terminal = Tool::new("c1".into(), "terminal_use".into(), "scan root".into());
        terminal.settle(&json!({ "command": "find / -maxdepth 1" }));
        terminal.state = ToolState::Failed;
        terminal.summary = "denied: permission required".into();
        terminal.content = terminal.summary.clone();
        let mut sink = Sink::default();
        draw_step(&app, 0, &terminal, false, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.matches("denied: permission required").count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn a_non_terminal_refusal_is_rendered_once_not_as_content_and_summary() {
        use runeengine::tools::notice::{self, status};
        let app = test_app();
        let mut tool = Tool::new("c1".into(), "find_files".into(), "find file".into());
        tool.settle(&json!({ "path": "mockup.html" }));
        tool.state = ToolState::Failed;
        tool.summary = "denied: Rejected by classifier: Policy prohibits deletion of files".into();
        // What the model was sent, and the label beside the call.
        tool.content = notice::failure(
            status::DENIED,
            "Rejected by classifier: Policy prohibits deletion of files",
        );
        tool.detail = Some(json!({ "status": "denied" }));
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, false, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("Rejected by classifier").count(), 1, "{text}");
        assert!(
            text.contains("This call was denied") && text.contains("<tool_notice>"),
            "the box says what the model was told, tags included:\n{text}"
        );
        assert!(
            text.contains("╭─ denied"),
            "refusal must be enclosed in a box:\n{text}"
        );
    }

    /// A create that failed wrote nothing: `+0` beside a red bullet, no word
    /// "failed", no box, none of Rune's tags, no code it never wrote; opened,
    /// why — in the words the model was told (owner, 2026-10-08).
    #[test]
    fn a_failed_create_says_so_by_its_bullet_and_count_and_why_only_when_opened() {
        use runeengine::tools::notice::{self, status};
        let app = test_app();
        let mut create = Tool::new("c1".into(), "create_file".into(), "create script".into());
        create.settle(
            &json!({ "path": "delete_mockup.py", "text": "import os\nos.remove('mockup.html')" }),
        );
        create.state = ToolState::Failed;
        create.summary = "denied: classifier_unavailable: provider: the stream ended badly: timeout: receive response".into();
        create.content = notice::failure(
            status::SERVICE_UNAVAILABLE,
            "classifier_unavailable: provider: the stream ended badly: timeout: receive response",
        );
        create.detail =
            Some(json!({ "status": "service_unavailable", "code": "classifier_unavailable" }));
        let draw = |tool: &Tool| {
            let mut sink = Sink::default();
            draw_step(&app, 0, tool, false, 100, &mut sink);
            sink.lines
                .iter()
                .map(|line| ansi::plain(line))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let closed = draw(&create);
        assert!(
            closed.contains("delete_mockup.py") && closed.contains("+0"),
            "{closed}"
        );
        assert!(!closed.contains("failed"), "{closed}");
        assert!(!closed.contains("tool_notice"), "{closed}");
        assert!(!closed.contains("╭─"), "no box: {closed}");
        assert!(!closed.contains("os.remove"), "{closed}");
        assert!(
            !closed.contains("A service this call needs is unavailable"),
            "{closed}"
        );
        create.open = true;
        let opened = draw(&create);
        assert!(
            opened.contains("A service this call needs is unavailable"),
            "opened, it says why: {opened}"
        );
        assert!(
            !opened.contains("tool_notice") && !opened.contains("╭─"),
            "{opened}"
        );
    }

    /// A `manage_process` step used to carry a bullet row saying
    /// `Manage bg_456 …` above a report that named the same task three more
    /// times — in `Process ID :`, in the log path, and in the path's filename.
    /// The word and the id were both repeats of what the report said properly,
    /// so the row is gone and the report is what is left.
    #[test]
    fn a_manage_process_step_is_its_report_and_nothing_repeated() {
        let app = test_app();
        let mut manage = Tool::new(
            "m2".into(),
            "manage_process".into(),
            "read the result".into(),
        );
        manage.settle(&json!({ "process_id": "bg_456" }));
        manage.state = ToolState::Ok;
        manage.open = true;
        manage.summary = "Completed".into();
        manage.content =
            "PID        : 99\nStatus     : Completed\nOutput log : /tmp/bg_456.txt\n\nhi".into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &manage, true, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !text.contains("Manage"),
            "the heading already says what is being done:\n{text}"
        );
        assert!(
            !text.contains("Process ID :"),
            "the report does not open by naming the task it is about:\n{text}"
        );
        assert_eq!(
            text.matches("Completed").count(),
            1,
            "the status is said once, as the report's own field:\n{text}"
        );
        assert!(
            text.contains("Status     : Completed") && text.contains("PID        : 99"),
            "and the report itself is intact:\n{text}"
        );
    }

    /// Owner, 2026-10-11: a `terminal_send_input` step showed a process
    /// report and the log under its notice, the same as `manage_process`. It
    /// only types now: its step is the heading and the one sentence of what
    /// it typed — no report, no output, no fold.
    #[test]
    fn a_typing_step_is_its_one_sentence() {
        let app = test_app();
        let mut typing = Tool::new(
            "t1".into(),
            "terminal_send_input".into(),
            "answer yes".into(),
        );
        typing.settle(&json!({ "process_id": "bg_1", "text": "y" }));
        typing.state = ToolState::Ok;
        typing.summary = "typed into bg_1".into();
        typing.content = runeengine::tools::notice::typed(
            "bg_1",
            true,
            true,
            runeengine::platform::InputWaitDetection::Every,
        );
        let mut sink = Sink::default();
        draw_step(&app, 0, &typing, true, 120, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("answer yes"), "{text}");
        assert!(
            text.contains("Typed the text, then Enter, into bg_1."),
            "{text}"
        );
        for absent in ["Process:", "PID:", "Output", "lines cut", "\"y\""] {
            assert!(!text.contains(absent), "{absent} in:\n{text}");
        }
        assert!(
            text.trim_end().ends_with("again or ends.</tool_notice>"),
            "the sentence is the step's last row, nothing under it:\n{text}"
        );
    }

    /// `manage_process` was the one tool that took its summary as the bullet
    /// note *and* as the error text, so a failed call said the same sentence on
    /// two consecutive lines — once after the pid, once inside the error box.
    #[test]
    fn a_failed_manage_process_says_its_summary_once() {
        let app = test_app();
        let mut manage = Tool::new(
            "m1".into(),
            "manage_process".into(),
            "check the build".into(),
        );
        manage.settle(&json!({ "process_id": "bg_123" }));
        manage.state = ToolState::Failed;
        manage.summary = "background process 'bg_123' not found".into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &manage, false, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.matches("not found").count(),
            1,
            "one failure is one sentence:\n{text}"
        );
        assert!(text.contains("bg_123"), "the pid is the target:\n{text}");
    }

    /// A patch that ran and found no single place for `old_str` shows what it
    /// tried — its counts and its diff, as the owner's design of 2026-09-19 had
    /// it — with the red bullet saying it failed: no word "failed", no tags,
    /// no box. Any other failure tried nothing: −0 +0 and no diff. Opened, the
    /// reason, dim, without tags.
    #[test]
    fn a_failed_patch_shows_what_it_tried_without_failed_tags_or_box() {
        use runeengine::tools::notice::{self, status};
        let app = test_app();
        let mut patch = Tool::new("p1".into(), "patch_file".into(), "edit kopi.html".into());
        patch.settle(&json!({
            "path": "kopi.html",
            "old_str": "55\n56",
            "new_str": "100\n101"
        }));
        patch.state = ToolState::Failed;
        patch.summary = "Exact match not found; file unchanged.".into();
        patch.content = notice::failure(
            status::FAILED,
            "Exact match not found; file unchanged. Re-read the target and verify whitespace before retrying.",
        );
        patch.detail = Some(json!({ "status": "failed", "code": "tool" }));
        let draw = |tool: &Tool| {
            let mut sink = Sink::default();
            draw_step(&app, 0, tool, false, 100, &mut sink);
            sink.lines
                .iter()
                .map(|line| ansi::plain(line))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let closed = draw(&patch);
        assert!(closed.contains("Edit  kopi.html  −2 +2"), "{closed}");
        assert!(closed.contains("╭─ diff"), "what it tried: {closed}");
        assert!(
            closed.contains("-55") || closed.contains("+100"),
            "{closed}"
        );
        assert!(!closed.contains("failed"), "{closed}");
        assert!(!closed.contains("tool_notice"), "{closed}");
        assert!(!closed.contains("╭─ error"), "{closed}");
        assert!(!closed.contains("Exact match not found"), "{closed}");
        patch.open = true;
        let opened = draw(&patch);
        assert!(
            opened.contains("Exact match not found; file unchanged."),
            "{opened}"
        );
        assert!(!opened.contains("tool_notice"), "{opened}");

        // Refused arguments tried nothing.
        patch.open = false;
        patch.content = notice::failure(status::NOT_EXECUTED, "old_str must not be empty");
        patch.detail = Some(json!({ "status": "not_executed", "code": "invalid" }));
        let refused = draw(&patch);
        assert!(refused.contains("Edit  kopi.html  −0 +0"), "{refused}");
        assert!(!refused.contains("╭─"), "no diff, no box: {refused}");
    }

    #[test]
    fn ask_user_questions_renders_borderless_without_box() {
        let app = test_app();
        let mut ask = Tool::new(
            "q1".into(),
            "ask_user_questions".into(),
            "Dua pertanyaan".into(),
        );
        // The model's form carries no ids: the engine names questions and
        // options by position, and the answers speak exactly those names.
        ask.settle(&json!({
            "questions": [
                { "title": "Minuman Favorit", "question": "Apa minuman favorit?", "type": "single", "options": [{"label": "Kopi ☕", "description": "Sang kopi."}] },
                { "title": "Gaya Coding", "question": "Gaya coding apa?", "type": "single", "options": [{"label": "Paralel 🚀", "description": "Sejajar."}] }
            ]
        }));
        ask.state = ToolState::Ok;
        ask.open = true;
        ask.content = serde_json::to_string(&runeengine::tools::questions::Reply::Answered {
            answers: vec![
                runeengine::tools::questions::UserAnswer {
                    id: "q0".into(),
                    selected: vec!["q0o0".into()],
                    custom: String::new(),
                },
                runeengine::tools::questions::UserAnswer {
                    id: "q1".into(),
                    selected: vec!["q1o0".into()],
                    custom: String::new(),
                },
            ],
        })
        .unwrap();
        let mut sink = Sink::default();
        draw_step(&app, 0, &ask, false, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("╭─ questions"),
            "must NOT have a container box:\n{text}"
        );
        assert!(
            !text.contains("╰─"),
            "must NOT have box bottom border:\n{text}"
        );
        assert!(
            text.contains("Minuman Favorit"),
            "must render question title:\n{text}"
        );
        assert!(text.contains("Kopi ☕"), "must render answer:\n{text}");
        assert!(
            text.contains("Gaya Coding"),
            "must render second question:\n{text}"
        );
        assert!(
            text.contains("Paralel 🚀"),
            "must render second answer:\n{text}"
        );
    }

    #[test]
    fn notice_block_renders_inside_system_box_with_clear_separation() {
        let mut app = test_app();
        app.blocks.push(Block::Notice {
            text: "Classifier Security Guard\nStatus: Active".into(),
            bad: false,
        });
        let mut sink = Sink::default();
        draw_block(&app.blocks[0], 80, &mut sink);
        let rendered = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("╭─ system"),
            "notice must be in a system box:\n{rendered}"
        );
        assert!(
            rendered.contains("╰─"),
            "notice box must have a bottom border:\n{rendered}"
        );
        assert!(
            rendered.contains("Classifier Security Guard"),
            "content must be inside the box:\n{rendered}"
        );
    }

    #[test]
    fn palette_command_does_not_truncate_with_dots_on_wide_screen() {
        let mut app = test_app();
        app.input = "/class".into();
        let mut sink = Sink::default();
        palette(&app, 100, 100, 6, &mut sink);
        let rendered = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("/classifier [on|off|model|reload]"),
            "command syntax must not be truncated with ... on wide screen:\n{rendered}"
        );
        assert!(
            !rendered.contains("/classifier [on|off|m..."),
            "command syntax must not have truncation dots:\n{rendered}"
        );
    }

    #[test]
    fn create_and_patch_payloads_are_visible_inside_their_steps() {
        let app = test_app();
        let mut create = Tool::new("c1".into(), "create_file".into(), "create main".into());
        create.settle(
            &json!({ "path": "src/main.rs", "text": "fn main() {\n    println!(\"hi\");\n}" }),
        );
        create.state = ToolState::Ok;
        let mut sink = Sink::default();
        draw_step(&app, 0, &create, false, 100, &mut sink);
        let created = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            created.contains("fn main()"),
            "created content is hidden:\n{created}"
        );
        assert!(
            created.contains("╭─ rs"),
            "the created file lost its readable code frame"
        );

        let mut patch = Tool::new("p1".into(), "patch_file".into(), "change greeting".into());
        patch.settle(&json!({ "path": "src/main.rs", "old_str": "println!(\"hi\");", "new_str": "println!(\"hello\");" }));
        patch.state = ToolState::Ok;
        let mut sink = Sink::default();
        draw_step(&app, 0, &patch, false, 100, &mut sink);
        let patched = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            patched.contains("−1 +1"),
            "patch counts are missing or reversed:\n{patched}"
        );
        assert!(
            patched.contains("-println!(\"hi\");"),
            "removed text is hidden:\n{patched}"
        );
        assert!(
            patched.contains("+println!(\"hello\");"),
            "replacement text is hidden:\n{patched}"
        );
    }

    #[test]
    fn track_tasks_renders_progression_markers_without_duplication_or_chevrons() {
        let app = test_app();
        let mut tasks_tool = Tool::new(
            "t1".into(),
            "track_tasks".into(),
            "Make sample todo list".into(),
        );
        tasks_tool.settle(&json!({
            "tasks": [
                { "text": "Learn Rust", "status": "done" },
                { "text": "Build Node.js API", "status": "doing" },
                { "text": "Deploy server", "status": "todo" },
            ]
        }));
        tasks_tool.state = ToolState::Ok;
        tasks_tool.open = true;
        tasks_tool.content = "● Learn Rust\n◐ Build Node.js API\n○ Deploy server".into();

        let mut sink = Sink::default();
        draw_step(&app, 0, &tasks_tool, false, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("● Learn Rust"), "done task marker: {text}");
        assert!(
            text.contains("◐ Build Node.js API"),
            "doing task marker: {text}"
        );
        assert!(text.contains("○ Deploy server"), "todo task marker: {text}");
        assert!(!text.contains("›"), "no chevron arrow: {text}");
        assert!(!text.contains("Step    "), "no repeated Step verb: {text}");
        assert!(!text.contains("[x]"), "no raw checklist dump: {text}");
        assert!(!text.contains("[>]"), "no raw checklist dump: {text}");
        assert!(!text.contains("[ ]"), "no raw checklist dump: {text}");
    }

    /// A response followed by more activity is part of that activity timeline;
    /// only the last response stands outside it as the answer.
    #[test]
    fn an_intermediate_response_joins_the_same_timeline() {
        let blocks = vec![
            Block::of(tool("inspect_file", ToolState::Ok)),
            Block::Text {
                text: "here is what I found".into(),
            },
            Block::of(tool("patch_file", ToolState::Ok)),
            Block::Text {
                text: "everything is ready".into(),
            },
        ];
        let groups = group(&blocks);
        assert_eq!(groups.len(), 2);
        assert!(matches!(&groups[0], Group::Steps { indices } if indices == &[0, 1, 2]));
        assert!(matches!(&groups[1], Group::One(3)));
    }

    #[test]
    fn the_timeline_never_collapses_and_prose_uses_a_muted_bullet() {
        let mut app = test_app();
        app.blocks.push(Block::user("do it"));
        app.blocks.push(Block::Thinking {
            text: "one two three".into(),
            open: false,
        });
        for n in 0..2 {
            let mut step = tool("inspect_file", ToolState::Ok);
            step.action = format!("step {n}");
            app.blocks.push(Block::of(step));
        }
        app.blocks.push(Block::Text {
            text: "intermediate response".into(),
        });
        let mut last_step = tool("patch_file", ToolState::Ok);
        last_step.action = "last step".into();
        app.blocks.push(Block::of(last_step));
        app.blocks.push(Block::Text {
            text: "the final response".into(),
        });

        let rendered = transcript(&mut app, 120);
        let plain = rendered
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "step 0",
            "step 1",
            "last step",
            "intermediate response",
            "the final response",
        ] {
            assert!(
                plain.contains(expected),
                "timeline hid {expected}:\n{plain}"
            );
        }
        assert!(
            !plain.to_lowercase().contains("worked"),
            "the removed summary returned:\n{plain}"
        );
        assert!(
            plain.contains("Thought · 3 words"),
            "thinking lost its word count:\n{plain}"
        );

        let response_head = rendered
            .lines
            .iter()
            .find(|line| ansi::plain(line).contains("Response"))
            .expect("the response activity needs a heading");
        assert!(
            response_head.starts_with(&format!("  {MUTED}●{RESET}")),
            "response bullet is too bright or state-coloured"
        );
        let thought_head = rendered
            .lines
            .iter()
            .find(|line| ansi::plain(line).contains("Thought"))
            .expect("thinking needs a heading");
        assert!(
            thought_head.starts_with(&format!("  {MUTED}●{RESET}")),
            "Thought changed to a green success bullet"
        );
        assert!(
            !ansi::plain(response_head).contains("words"),
            "response gained a word counter"
        );
        let response_body = rendered
            .lines
            .iter()
            .find(|line| ansi::plain(line).contains("intermediate response"))
            .expect("the intermediate response must stay open");
        assert!(
            ansi::plain(response_body).contains('│'),
            "response is detached from the spine"
        );
        let final_body = rendered
            .lines
            .iter()
            .find(|line| ansi::plain(line).contains("the final response"))
            .expect("the final response must remain readable");
        assert!(
            !ansi::plain(final_body).contains('│'),
            "the final response became an activity"
        );
    }

    #[test]
    fn condition_bullet_blinks_when_processing_and_green_when_ok() {
        let processing_bright = ansi::plain(&condition_bullet(true, true, 0));
        let processing_dim = ansi::plain(&condition_bullet(true, true, 38));
        assert_eq!(processing_bright, "●");
        assert_eq!(processing_dim, "●");

        let ok_bullet = condition_bullet(false, true, 0);
        assert!(ok_bullet.contains('●'));
        assert_eq!(ansi::plain(&ok_bullet), "●");

        let failed_bullet = condition_bullet(false, false, 0);
        assert!(failed_bullet.contains('●'));
        assert_eq!(ansi::plain(&failed_bullet), "●");
        // Strictly no emojis or crosses
        assert!(!failed_bullet.contains('✗'));
        assert!(!failed_bullet.contains('✓'));
    }

    #[test]
    fn terminal_box_renders_with_borders_and_command_prompt() {
        let app = test_app();
        let mut terminal = Tool::new("c1".into(), "terminal_use".into(), "build project".into());
        terminal.settle(&json!({ "command": "cargo build --release" }));
        terminal.output = "Compiling project\nFinished".into();
        terminal.state = ToolState::Ok;

        let mut sink = Sink::default();
        draw_step(&app, 0, &terminal, false, 80, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("╭─ Bash"));
        assert!(text.contains("$ cargo build --release"));
        assert!(text.contains("Compiling project"));
        assert!(text.contains("╰"));
    }

    // ── Waiting ──────────────────────────────────────────────────────────────

    /// The indicator is not in the transcript any more. It is the last line of
    /// the screen, standing where the project path stands when nothing is
    /// happening — one line that answers one question.
    #[test]
    fn the_last_line_is_the_path_when_idle_and_the_track_when_working() {
        let mut app = test_app();
        app.blocks.push(Block::Text {
            text: "partial answer".into(),
        });

        let idle = compose(&mut app, 24, 80);
        let last = ansi::plain(idle.last().unwrap());
        assert!(
            last.contains("project"),
            "the path belongs on the idle line: {last}"
        );
        assert!(
            !last.contains('▰'),
            "nothing may animate while nothing is happening"
        );

        app.busy = true;
        app.label = "Patching".into();
        let frame = compose(&mut app, 24, 80);
        let last = ansi::plain(frame.last().unwrap());
        assert!(
            last.contains("Patching"),
            "the label belongs on the working line: {last}"
        );
        const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        assert!(
            last.chars().any(|c| SPINNER_CHARS.contains(&c)),
            "the spinner says it is still going: {last}"
        );

        let content = frame.len() - footer(&mut app, 24, 80).0.len();
        assert!(
            frame[..content]
                .iter()
                .all(|l| !ansi::plain(l).contains("Patching")),
            "the transcript must not carry the indicator any more"
        );
    }

    #[test]
    fn the_label_stays_put_while_the_track_moves() {
        let mut app = test_app();
        app.busy = true;
        app.label = "Patching".into();
        let first = ansi::plain(&state_line(&app, 120));
        app.frame = 7;
        let later = ansi::plain(&state_line(&app, 120));
        assert!(first.contains("Patching") && later.contains("Patching"));
        assert_ne!(first, later, "the track is the only thing that moves");
    }

    #[test]
    fn live_terminal_spinner_rewrites_one_cell_and_invalidates_same_length_cached_rows() {
        let mut app = test_app();
        app.busy = true;
        let mut tool = Tool::new(
            "spinner".into(),
            "terminal_use".into(),
            "install packages".into(),
        );
        tool.settle(&json!({"command": "npm install"}));
        tool.absorb_output("⠋", 10, 72);
        app.blocks.push(Block::of(tool));
        let first = screen(&mut app, 24, 100);
        assert!(first.contains("⠋"));
        app.session = Some(1);
        app.on_event(runeengine::Event::ToolOutput {
            session: 1,
            id: "spinner".into(),
            chunk: "\x08⠙".into(),
            rows: 10,
            cols: 72,
        });
        let next = screen(&mut app, 24, 100);
        let terminal_rows = next
            .lines()
            .filter(|line| line.contains("│ ⠙") || line.contains("│ ⠋"))
            .collect::<Vec<_>>();
        assert_eq!(terminal_rows.len(), 1, "{next}");
        assert!(terminal_rows[0].contains('⠙'), "{next}");
        assert!(!next.contains("⠋⠙"));
        assert_eq!(next.lines().count(), 24);
    }

    #[test]
    fn terminal_live_box_starts_command_only_then_grows_with_content_without_erase_flicker() {
        let mut tool = Tool::new("t".into(), "terminal_use".into(), "run".into());
        tool.settle(&json!({"command":"echo 1"}));
        for (chunk, expected) in [
            ("", 3),
            ("⠋", 5),
            ("\r\x1b[2K", 5),
            ("⠙", 5),
            ("\r\x1b[2K1\r\n2", 6),
            ("\r\n3", 7),
            ("\r\n4", 8),
            ("\r\n5", 9),
            ("\r\n6", 9),
        ] {
            tool.absorb_output(chunk, 12, 72);
            let mut sink = Sink::default();
            draw_terminal(&tool, false, true, 100, &mut sink, 0);
            assert_eq!(sink.len(), expected, "chunk {chunk:?}: {}", plain_of(&sink));
            assert!(!tool.open);
            assert_eq!(
                sink.lines
                    .iter()
                    .filter(|line| ansi::plain(line).contains('╭'))
                    .count(),
                1
            );
            if expected == 3 {
                assert!(!plain_of(&sink).contains('├'));
            }
            if expected < 9 {
                assert!(!plain_of(&sink).contains("lines cut"));
            }
            if expected == 9 {
                let text = plain_of(&sink);
                assert!(text.contains("│ 1") && text.contains("│ 4"), "{text}");
                assert!(
                    !text.contains("│ 5"),
                    "the unseen suffix must be below, never the head"
                );
                assert!(ansi::plain(&sink.lines[sink.len() - 2]).contains("lines cut"));
            }
        }
    }

    #[test]
    fn terminal_preview_is_identical_after_allow_and_in_full_and_never_grows_for_notices() {
        let mut ask = test_app();
        ask.session = Some(1);
        ask.busy = true;
        let mut full = test_app();
        full.session = Some(1);
        full.busy = true;
        full.mode = "full".into();
        let call = runeengine::Event::ToolCall {
            session: 1,
            id: "same".into(),
            name: "terminal_use".into(),
            action: "run".into(),
            args: json!({"command": "printf data; # very long command".repeat(100)}),
            at: 0,
        };
        ask.on_event(call.clone());
        full.on_event(call);
        ask.on_event(runeengine::Event::PermissionRequest {
            session: 1,
            req: "p".into(),
            tool: "terminal_use".into(),
            action: "run".into(),
            preview: "$ command".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        ask.on_event(runeengine::Event::PermissionResolved {
            session: 1,
            req: "p".into(),
            decision: runeengine::proto::Decision::AllowOnce,
        });
        let preview = |app: &App| {
            let tool = app
                .blocks
                .iter()
                .find_map(|block| match block {
                    Block::Tool(tool) => Some(tool),
                    _ => None,
                })
                .unwrap();
            assert!(!tool.open);
            let mut sink = Sink::default();
            draw_terminal(tool, false, true, 100, &mut sink, 0);
            sink
        };
        assert_eq!(
            preview(&full).len(),
            3,
            "without output the box is command only"
        );
        for chunk in ["one", "\r\x1b[2K", "1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n"] {
            let event = runeengine::Event::ToolOutput {
                session: 1,
                id: "same".into(),
                chunk: chunk.into(),
                rows: 30,
                cols: 200,
            };
            ask.on_event(event.clone());
            full.on_event(event);
            let a = preview(&ask);
            let f = preview(&full);
            assert_eq!(a.lines, f.lines);
            assert!(a.len() <= 9);
            let plain = plain_of(&a);
            if plain.contains("lines cut") {
                assert!(ansi::plain(&a.lines[a.len() - 2]).contains("lines cut"));
            }
            assert!(!plain.contains("rows hidden") && !plain.contains("wheel here"));
        }
        for content in [
            "",
            "done",
            "output\n".repeat(100).as_str(),
            &format!(
                "<tool_notice>{}</tool_notice>\n\noutcome",
                "long notice ".repeat(100)
            ),
        ] {
            let event = runeengine::Event::ToolResult {
                session: 1,
                id: "same".into(),
                ok: true,
                summary: "ok".into(),
                content: content.into(),
                detail: None,
                at: 0,
            };
            ask.on_event(event.clone());
            full.on_event(event);
            assert_eq!(preview(&ask).lines, preview(&full).lines);
            assert!(preview(&full).len() <= 9);
        }
    }

    #[test]
    fn long_command_permissions_do_not_exceed_the_frame_or_hide_answer_hit_targets() {
        for (rows, cols) in [(8, 20), (16, 40), (24, 80), (42, 150)] {
            let mut app = test_app();
            app.pending = Some(super::super::blocks::Ask {
                req: "mega".into(),
                action: "audit".into(),
                preview: format!("$ {}", "printf benchmark && ".repeat(1000)),
                remember_as: Some("shell".into()),
                reach: vec!["/proc/meminfo".into()],
                sensitive: vec!["/ — host filesystem".into()],
                full_access: true,
                reason: Some("This command runs outside confinement.".into()),
            });
            let frame = compose(&mut app, rows, cols);
            assert_eq!(frame.len(), usize::from(rows));
            assert_eq!(app.hits.len(), usize::from(rows));
            let text = frame
                .iter()
                .map(|line| ansi::plain(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Refuse"), "{text}");
            assert!(app.hits.iter().any(|hit| matches!(hit, Hit::Answer(_))));
        }
    }

    // ── The permission card ──────────────────────────────────────────────────

    /// When a mode that would not have asked does, the card says why, in the
    /// engine's words — whole, wrapped, never cut.
    #[test]
    fn a_card_says_why_a_mode_that_would_not_ask_does() {
        let mut app = test_app();
        let reason = "Commands run unconfined here: this kernel has no Landlock support. Auto asks before each one; Full stops asking.";
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "list the files".into(),
            preview: "$ ls".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: Some(reason.into()),
        });
        let frame = compose(&mut app, 24, 60);
        let text = frame
            .iter()
            .map(|l| {
                ansi::plain(l)
                    .trim_matches(|c: char| c == '│' || c == ' ')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join(" ");
        let words: Vec<&str> = reason.split_whitespace().collect();
        let shown: Vec<&str> = text.split_whitespace().collect();
        assert!(
            shown.windows(words.len()).any(|w| w == words.as_slice()),
            "the reason is on the card, whole: {text}"
        );
        assert!(
            !text.contains("Always allow"),
            "nothing to remember is offered"
        );
    }

    #[test]
    fn the_permission_card_takes_the_composers_place_at_the_bottom() {
        let mut app = test_app();
        app.blocks.push(Block::Text {
            text: "some answer".into(),
        });
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "list the downloads folder".into(),
            preview: "run `ls ~/Downloads`".into(),
            remember_as: Some("ls".into()),
            reach: vec!["/home/dev/Downloads".into()],
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let frame = compose(&mut app, 24, 80);
        let text: String = frame
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("run `ls ~/Downloads`"));
        assert!(
            !text.contains("list the downloads folder"),
            "the duplicated model action must stay out of the card"
        );
        assert!(text.contains("reaches outside the workspace"));
        assert!(text.contains("/home/dev/Downloads"));
        // Answers to walk along, not letters to remember.
        assert!(text.contains("Allow once") && text.contains("Refuse"));
        assert!(text.contains("← → choose"));
        assert!(!text.contains("y allow"), "the letter shortcuts came back");
        // …and the composer is gone while it is up.
        assert!(!frame.iter().any(|l| ansi::plain(l).starts_with("› ")));
        // …and it is at the bottom, not in the transcript.
        let card = frame
            .iter()
            .position(|l| ansi::plain(l).contains("permission"))
            .unwrap();
        assert!(
            card > frame.len() / 2,
            "the card must be at the bottom of the screen"
        );
    }

    /// Nothing on this screen may name a tool. The waiting line was the last
    /// place one got through, because it was built from the event rather than
    /// from the step.
    #[test]
    fn no_internal_tool_identifier_reaches_the_screen() {
        let mut app = test_app();
        app.busy = true;
        app.label = "Waiting on you".into();
        let mut t = Tool::new(
            "c1".into(),
            "create_file".into(),
            "write a notes file".into(),
        );
        t.settle(&json!({ "path": "NOTES.md", "text": "one" }));
        t.state = ToolState::Asking;
        app.blocks.push(Block::of(t));
        app.pending = Some(super::super::blocks::Ask {
            req: "r".into(),
            action: "write a notes file".into(),
            preview: "create NOTES.md (1 line)".into(),
            remember_as: Some("create_file".into()),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });

        let text = screen(&mut app, 30, 100);
        // The one exception, and it is deliberate: "Always allow <key>" names
        // what a stored grant would cover, and that key really is the tool.
        let without_grant: String = text
            .lines()
            .filter(|l| !l.contains("Always allow"))
            .collect::<Vec<_>>()
            .join("\n");
        for name in ["create_file", "inspect_file", "terminal_use", "track_tasks"] {
            assert!(
                !without_grant.contains(name),
                "{name} reached the screen:\n{without_grant}"
            );
        }
    }

    /// Audit U4: a large pending write must not make the card taller than the
    /// screen — the frame overflowed and the painter lost track of it.
    #[test]
    fn a_large_pending_write_keeps_the_card_inside_the_screen() {
        let ask = super::super::blocks::Ask {
            req: "r".into(),
            action: "write the file".into(),
            preview: "big.txt".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        };
        let mut write = Tool::new("w".into(), "create_file".into(), "write".into());
        let body: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        write.settle(&json!({ "action": "write", "path": "big.txt", "text": body }));
        let rows = 24;
        let card = permission_card(&ask, 0, 80, rows, Some(&write));
        assert!(card.lines.len() < rows, "card is {} rows", card.lines.len());
        assert!(card
            .lines
            .iter()
            .any(|l| ansi::plain(l).contains("rows below")));
    }

    #[test]
    fn the_card_offers_always_only_when_it_can_be_remembered() {
        let ask = |remember: Option<&str>| super::super::blocks::Ask {
            req: "r".into(),
            action: "run the tests".into(),
            preview: "npm test".into(),
            remember_as: remember.map(String::from),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        };
        let once: String = permission_card(&ask(None), 0, 60, 40, None)
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(
            !once.contains("Always"),
            "an unrememberable grant was offered anyway"
        );
        assert_eq!(ask(None).answers().len(), 2);

        let always: String = permission_card(&ask(Some("npm")), 0, 60, 40, None)
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(always.contains("Always allow npm"));
        assert_eq!(ask(Some("npm")).answers().len(), 3);

        let mut full = ask(None);
        full.full_access = true;
        let full_text: String = permission_card(&full, 0, 80, 40, None)
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect();
        assert!(full_text.contains("Full access once"));
        assert!(full_text.contains("no jail or sandbox"));
        assert!(
            !full_text.contains("Always"),
            "host access became rememberable"
        );
    }

    /// The owner's rule, on the card: a path the machine runs on its own is
    /// never refused by the system — it is asked with the reason attached.
    #[test]
    fn the_card_shows_why_a_sensitive_path_needs_the_owner() {
        let mut ask = super::super::blocks::Ask {
            req: "r".into(),
            action: "inspect the ssh key".into(),
            preview: "read ~/.ssh/id_ed25519".into(),
            remember_as: None,
            reach: vec!["/home/dev/.ssh/id_ed25519".into()],
            sensitive: vec![
                "/home/dev/.ssh/id_ed25519 — /home/dev/.ssh holds credentials or files the \
                 user's machine runs on its own"
                    .into(),
            ],
            full_access: false,
            reason: None,
        };
        let text: String = permission_card(&ask, 0, 90, 40, None)
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(text.contains("never refused"), "{text}");
        assert!(text.contains("holds credentials"), "{text}");
        assert!(
            !text.contains("Always"),
            "a sensitive path must never be offered a remembered grant"
        );
        assert_eq!(ask.answers().len(), 2);

        // And an ordinary reach carries no such section.
        ask.sensitive.clear();
        let plain: String = permission_card(&ask, 0, 90, 40, None)
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect();
        assert!(!plain.contains("never refused"), "{plain}");
    }

    /// The card is the one boxed thing on the screen, and every row of it has to
    /// reach the same right edge or the box is not a box.
    #[test]
    fn the_permission_box_is_closed_on_all_four_sides_at_every_width() {
        let ask = super::super::blocks::Ask {
            req: "r1".into(),
            action: "list what is in the Downloads folder".into(),
            preview: "$ ls -la ~/Downloads".into(),
            remember_as: Some("ls".into()),
            reach: vec!["/home/dev/Downloads".into()],
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        };
        for cols in [40usize, 60, 100] {
            let rows = permission_card(&ask, 0, cols, 40, None);
            for row in &rows.lines {
                assert_eq!(
                    ansi::width(row),
                    cols,
                    "a card row is not {cols} wide: {:?}",
                    ansi::plain(row)
                );
            }
            let first = ansi::plain(&rows.lines[0]);
            let last = ansi::plain(rows.lines.last().unwrap());
            assert!(first.starts_with('╭') && first.ends_with('╮'), "{first:?}");
            assert!(last.starts_with('╰') && last.ends_with('╯'), "{last:?}");
        }
    }

    /// The whole point of the row: one answer is under the cursor, and moving
    /// along it changes which.
    #[test]
    fn one_answer_stands_out_of_the_row_and_the_arrows_move_it() {
        let ask = super::super::blocks::Ask {
            req: "r".into(),
            action: "a".into(),
            preview: "p".into(),
            remember_as: Some("ls".into()),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        };
        let marked = |choice: usize| {
            let row = answers_row(&ask, choice);
            // Whatever sits between the reverse-video code and the reset.
            let start = row
                .find(ansi::REVERSE)
                .expect("nothing is under the cursor");
            ansi::plain(&row[start..]).trim().to_string()
        };
        assert!(marked(0).starts_with("Allow once"));
        assert!(marked(1).starts_with("Always allow ls"));
        assert!(marked(2).starts_with("Refuse"));
        // Out of range cannot leave the row with nothing selected.
        assert!(marked(9).starts_with("Refuse"));
    }

    /// The history above the first screenful is laid out by the frames the
    /// window keeps asking for, not by the reader's first wheel notch: the
    /// ticks alone finish it, and then they stop asking.
    #[test]
    fn idle_ticks_finish_laying_out_the_history() {
        let mut app = test_app();
        for i in 0..800 {
            app.blocks.push(Block::Text {
                text: format!("answer {i} with enough words to wrap around a narrow column now"),
            });
        }
        let mut screen = Screen::default();
        let _ = screen.render(&mut app, 40, 100, true);
        assert!(app.transcript_cache.as_ref().unwrap().head_pending());
        let mut frames = 0;
        while app.transcript_cache.as_ref().unwrap().head_pending() {
            assert!(app.tick(), "an unfinished layout asks for its next frame");
            let _ = screen.render(&mut app, 40, 100, false);
            frames += 1;
            assert!(frames < 2000, "the layout never finished");
        }
        assert!(!app.tick(), "a finished layout is an idle window");
    }

    /// Just enough of a terminal to check what a stream of paint commands
    /// leaves on the screen: cursor addressing, erase to end of line (painted
    /// with the pen's background, as terminals paint it), margins and the two
    /// scroll commands, colour, and characters two columns wide.
    struct MiniTerm {
        rows: Vec<Vec<Slot>>,
        cols: usize,
        at: (usize, usize),
        margins: (usize, usize),
        /// What the next character is written in; `None` after a colour code
        /// the check does not model.
        pen: Option<super::super::sgr::Style>,
        /// The cell written last and its width, which a character of no width
        /// of its own joins (and U+FE0F widens), as [`ansi::width`] counts it.
        last: Option<(usize, usize, usize)>,
        after_zwj: bool,
    }

    /// One cell of the check's terminal: a character and whatever joined it;
    /// the right half of a character two columns wide holds `"\0"`.
    #[derive(Debug, Clone, PartialEq)]
    struct Slot {
        text: String,
        style: Option<super::super::sgr::Style>,
    }

    const RIGHT_HALF: &str = "\0";

    impl MiniTerm {
        fn new(rows: usize, cols: usize) -> Self {
            let blank = Slot {
                text: " ".into(),
                style: Some(Default::default()),
            };
            MiniTerm {
                rows: vec![vec![blank; cols]; rows],
                cols,
                at: (0, 0),
                margins: (0, rows),
                pen: Some(Default::default()),
                last: None,
                after_zwj: false,
            }
        }

        /// What an erase leaves under the pen.
        fn blank(&self) -> Slot {
            Slot {
                text: " ".into(),
                style: self.pen.map(|pen| pen.erasing()),
            }
        }

        /// Writing over either half of a wide character erases the other
        /// half, as a terminal does.
        fn split_wide(&mut self, r: usize, c: usize) {
            let row = &mut self.rows[r];
            if row[c].text == RIGHT_HALF && c > 0 {
                row[c - 1].text = " ".into();
            }
            if row[c].text != RIGHT_HALF
                && row.get(c + 1).is_some_and(|next| next.text == RIGHT_HALF)
            {
                row[c + 1].text = " ".into();
            }
        }

        fn put(&mut self, ch: char) {
            let (r, c) = self.at;
            let joins = std::mem::take(&mut self.after_zwj) || ch == '\u{200d}';
            self.after_zwj = ch == '\u{200d}';
            let w = ansi::width(ch.encode_utf8(&mut [0; 4]));
            if joins || w == 0 || ch == '\u{fe0f}' {
                let Some((lr, lc, lw)) = self.last else {
                    return;
                };
                self.rows[lr][lc].text.push(ch);
                if ch == '\u{fe0f}' && lw == 1 && lr == r && lc + 1 == c && c < self.cols {
                    self.split_wide(r, c);
                    self.rows[r][c] = Slot {
                        text: RIGHT_HALF.into(),
                        style: self.pen,
                    };
                    self.at.1 += 1;
                    self.last = Some((lr, lc, 2));
                }
                return;
            }
            if c + w > self.cols {
                return;
            }
            self.split_wide(r, c);
            if w == 2 {
                self.split_wide(r, c + 1);
            }
            self.rows[r][c] = Slot {
                text: ch.to_string(),
                style: self.pen,
            };
            if w == 2 {
                self.rows[r][c + 1] = Slot {
                    text: RIGHT_HALF.into(),
                    style: self.pen,
                };
            }
            self.at.1 += w;
            self.last = Some((r, c, w));
        }

        fn feed(&mut self, bytes: &str) {
            let chars: Vec<char> = bytes.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] != '\x1b' {
                    self.put(chars[i]);
                    i += 1;
                    continue;
                }
                if chars[i + 1] == ']' {
                    // OSC (a link): to its BEL or ST, nothing to show.
                    i += 2;
                    while chars[i] != '\u{7}' && !(chars[i] == '\x1b' && chars[i + 1] == '\\') {
                        i += 1;
                    }
                    i += if chars[i] == '\u{7}' { 1 } else { 2 };
                    continue;
                }
                // CSI: parameters up to a final byte in 0x40..=0x7e.
                i += 2;
                let start = i;
                while !('\x40'..='\x7e').contains(&chars[i]) {
                    i += 1;
                }
                let params: String = chars[start..i].iter().collect();
                let last = chars[i];
                i += 1;
                let nums: Vec<usize> = params
                    .split(';')
                    .map(|p| p.trim_start_matches('?').parse().unwrap_or(0))
                    .collect();
                match last {
                    'H' => {
                        self.at = (
                            nums[0].max(1) - 1,
                            nums.get(1).copied().unwrap_or(1).max(1) - 1,
                        );
                        self.last = None;
                    }
                    'K' => {
                        let (r, c) = self.at;
                        if c < self.cols {
                            self.split_wide(r, c);
                        }
                        let blank = self.blank();
                        for slot in self.rows[r].iter_mut().skip(c) {
                            *slot = blank.clone();
                        }
                    }
                    'r' => {
                        self.margins = if params.is_empty() {
                            (0, self.rows.len())
                        } else {
                            (nums[0] - 1, nums[1])
                        };
                        self.at = (0, 0);
                    }
                    'S' | 'T' => {
                        let (top, bottom) = self.margins;
                        let by = nums[0].max(1);
                        let blank = self.blank();
                        let band = &mut self.rows[top..bottom];
                        if last == 'S' {
                            band.rotate_left(by);
                            let len = band.len();
                            for row in &mut band[len - by..] {
                                row.fill(blank.clone());
                            }
                        } else {
                            band.rotate_right(by);
                            for row in band.iter_mut().take(by) {
                                row.fill(blank.clone());
                            }
                        }
                    }
                    'G' => {
                        self.at.1 = nums[0].max(1) - 1;
                        self.last = None;
                    }
                    'J' => {
                        let blank = self.blank();
                        self.rows.iter_mut().for_each(|row| row.fill(blank.clone()));
                    }
                    'm' => {
                        self.pen = self
                            .pen
                            .and_then(|pen| super::super::sgr::apply(pen, &params));
                    }
                    // Modes and the cursor's shape: nothing to show.
                    'h' | 'l' | 'q' => {}
                    other => panic!("the painter sent a command the check does not know: {other}"),
                }
            }
        }

        fn text(&self, r: usize) -> String {
            self.rows[r]
                .iter()
                .filter(|slot| slot.text != RIGHT_HALF)
                .map(|slot| slot.text.as_str())
                .collect()
        }

        fn shows(&self, frame: &[String]) -> bool {
            (0..self.rows.len()).all(|r| {
                let want = ansi::plain(frame.get(r).map(String::as_str).unwrap_or(""));
                self.text(r).trim_end() == want.trim_end()
            })
        }

        /// Every cell, colour and erased tail as writing each row of `frame`
        /// whole leaves them on a cleared screen.
        fn shows_exactly(&self, frame: &[String]) -> Result<(), String> {
            let mut whole = MiniTerm::new(self.rows.len(), self.cols);
            for (i, line) in frame.iter().enumerate() {
                let mut out = String::new();
                write_row(&mut out, i, line, self.cols as u16);
                whole.feed(&out);
                whole.feed(ansi::RESET);
            }
            match (0..self.rows.len()).find(|&r| self.rows[r] != whole.rows[r]) {
                None => Ok(()),
                Some(r) => {
                    let column = (0..self.cols)
                        .find(|&c| self.rows[r][c] != whole.rows[r][c])
                        .unwrap_or(0);
                    Err(format!(
                        "row {r} column {column}: {:?} where {:?}\n shown: {:?}\n frame: {:?}",
                        self.rows[r][column],
                        whole.rows[r][column],
                        self.text(r),
                        whole.text(r),
                    ))
                }
            }
        }
    }

    /// A row that changed is sent from its first changed column — and only up
    /// to its last when it kept its width — since the terminal already shows
    /// the rest. Whatever the change (a word, a colour, a wide character for
    /// narrow ones, a shorter or longer row, a row beside the panel or no
    /// longer beside it, a link or an emoji sequence, which are sent whole),
    /// the terminal then shows every cell, colour and erased tail exactly as
    /// writing each row whole did.
    #[test]
    fn a_changed_row_is_sent_from_its_first_changed_column_and_shows_the_same() {
        const STYLES: [&str; 6] = [
            "",
            "\x1b[31m",
            "\x1b[1m",
            "\x1b[44m",
            "\x1b[38;2;10;20;30m",
            "\x1b[2m\x1b[48;5;100m",
        ];
        const GLYPHS: [&str; 11] = [
            "a", "b", "x", " ", " ", "\u{2502}", "\u{2500}", "\u{25cf}", "\u{4e2d}", "\u{6587}",
            "e",
        ];
        // Rare pieces a terminal may count its own way, and a link.
        const ODD: [&str; 2] = [
            "\u{2764}\u{fe0f}",
            "\x1b]8;;https://example.com\x1b\\ln\x1b]8;;\x1b\\",
        ];
        #[derive(Clone)]
        struct Row {
            spans: Vec<(usize, Vec<&'static str>)>,
            open_end: bool,
            panel: Option<String>,
        }
        let mut seed = 11u64;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        let (rows, cols, left_w) = (10usize, 40usize, 24usize);
        let render = |row: &Row| -> String {
            let limit = if row.panel.is_some() { left_w } else { cols };
            let mut text = String::new();
            for (style, glyphs) in &row.spans {
                text.push_str(STYLES[*style]);
                for glyph in glyphs {
                    if ansi::width(&text) + ansi::width(glyph) <= limit {
                        text.push_str(glyph);
                    }
                }
                text.push_str(ansi::RESET);
            }
            if row.open_end {
                text.push_str(STYLES[3]);
            }
            match &row.panel {
                Some(panel) => beside(&text, left_w, panel, cols - left_w - 2),
                None => text,
            }
        };
        let fresh = |next: &mut dyn FnMut(usize) -> usize| -> Row {
            let spans = (0..1 + next(4))
                .map(|_| {
                    let glyphs = (0..next(12))
                        .map(|_| {
                            if next(40) == 0 {
                                ODD[next(ODD.len())]
                            } else {
                                GLYPHS[next(GLYPHS.len())]
                            }
                        })
                        .collect();
                    (next(STYLES.len()), glyphs)
                })
                .collect();
            Row {
                spans,
                open_end: next(6) == 0,
                panel: (next(3) == 0).then(|| format!("panel {}", next(5))),
            }
        };
        let mut doc: Vec<Row> = (0..rows).map(|_| fresh(&mut next)).collect();
        let mut screen = Screen::default();
        let mut term = MiniTerm::new(rows, cols);
        let (mut sent, mut whole) = (0usize, 0usize);
        for step in 0..1500 {
            for row in doc.iter_mut() {
                match next(10) {
                    0..=4 => {}
                    5 | 6 => {
                        // One glyph in one span.
                        let span = next(row.spans.len());
                        let glyphs = &mut row.spans[span].1;
                        if glyphs.is_empty() {
                            glyphs.push(GLYPHS[next(GLYPHS.len())]);
                        } else {
                            let at = next(glyphs.len());
                            glyphs[at] = GLYPHS[next(GLYPHS.len())];
                        }
                    }
                    7 => {
                        let span = next(row.spans.len());
                        row.spans[span].0 = next(STYLES.len());
                    }
                    8 => {
                        let span = next(row.spans.len());
                        let keep = next(row.spans[span].1.len() + 1);
                        row.spans[span].1.truncate(keep);
                        if next(2) == 0 {
                            row.open_end = !row.open_end;
                        }
                    }
                    _ => *row = fresh(&mut next),
                }
            }
            let frame: Vec<String> = doc.iter().map(render).collect();
            let mut out = String::from("\x1b[?2026h\x1b[?25l");
            screen.rows(&frame, cols as u16, &mut out);
            for (i, (old, line)) in screen.prev.iter().zip(&frame).enumerate() {
                if old != line {
                    write_row(&mut String::new(), i, line, cols as u16);
                    whole += line.len() + 8;
                }
            }
            screen.prev = frame.clone();
            out.push_str(ansi::RESET);
            out.push_str("\x1b[?2026l");
            let out = super::super::sgr::minimal(&out);
            sent += out.len();
            term.feed(&out);
            if let Err(why) = term.shows_exactly(&frame) {
                panic!("step {step}: {why}");
            }
        }
        // Rows rewritten from scratch and recoloured are most of this mix, so
        // it saves less than a wheel step does; it must never cost more.
        assert!(
            sent < whole,
            "rows sent from their first change cost {sent} bytes against {whole} sent whole"
        );
    }

    /// One character changed on a row costs a cursor move and that character,
    /// not the row: a wheel step moves text inside borders and padding that
    /// stay where they are.
    #[test]
    fn one_changed_character_costs_its_cell_not_its_row() {
        let border = "\x1b[38;2;90;90;90m\u{2502}\x1b[0m";
        let row = |word: &str| format!("{border} {word}{} {border}", " ".repeat(30));
        let mut screen = Screen {
            prev: vec![row("abc def")],
            ..Screen::default()
        };
        let mut out = String::new();
        screen.rows(&[row("abX def")], 60, &mut out);
        assert_eq!(
            out,
            format!("{}{}X{}", ansi::at(1, 5), ansi::RESET, ansi::RESET)
        );
        // A row with a sequence a terminal may draw one column or two wide is
        // sent whole, from its first column, where the terminal places it.
        screen.prev = vec![format!("a\u{2764}\u{fe0f} {}", row("abc"))];
        let mut out = String::new();
        screen.rows(&[format!("a\u{2764}\u{fe0f} {}", row("abX"))], 60, &mut out);
        assert!(out.starts_with(&ansi::at(1, 1)), "{out:?}");
    }

    /// On a terminal that shows each frame whole, two frames may be on their
    /// way at once, a third waits for the first's answer, and a terminal that
    /// stopped answering is given up once, for all of them.
    #[test]
    fn two_frames_may_be_on_their_way_and_a_third_waits_for_an_answer() {
        use super::super::input::Report;
        use std::time::{Duration, Instant};
        let mut screen = Screen {
            synchronized: true,
            ..Screen::default()
        };
        let now = Instant::now();
        assert!(!screen.behind_at(Instant::now()));
        screen.unanswered.push_back((now, 9_000));
        assert!(
            !screen.behind_at(Instant::now()),
            "one frame on its way: the next is sent"
        );
        screen.unanswered.push_back((now, 9_000));
        assert!(
            screen.behind_at(Instant::now()),
            "two on their way: the third waits"
        );
        screen.heard_at(Report::Ready, now + Duration::from_millis(30));
        assert!(
            !screen.behind_at(Instant::now()),
            "the first is read: the next goes"
        );
        assert!(
            screen.awaiting_answer().is_some(),
            "leaving waits for the last frame's answer too"
        );
        screen.heard_at(Report::Ready, now + Duration::from_millis(60));
        assert!(screen.awaiting_answer().is_none());
        let long_ago = now.checked_sub(Duration::from_secs(2)).unwrap();
        screen.unanswered.push_back((long_ago, 9_000));
        screen.unanswered.push_back((now, 9_000));
        assert!(
            !screen.behind_at(Instant::now()),
            "an answer that never came: frames go unasked"
        );
        assert!(screen.silent && screen.unanswered.is_empty());
    }

    /// Tearing, owner 2026-10-09, measured in a real Konsole 25.04.2 under
    /// gdb: a terminal that draws on its own clock (no DEC 2026) drew the first
    /// 4095 bytes of a 6 KB frame — when frames came back to back, and when the
    /// next frame arrived while it was still drawing the last. There, one frame
    /// is on its way at a time; after its answer the terminal gets `QUIET`
    /// with nothing new; then a status request goes alone, answered only once
    /// the terminal is done drawing; only then the next frame.
    #[test]
    fn a_terminal_that_draws_on_its_own_clock_is_let_draw_each_frame_whole() {
        use super::super::input::Report;
        use std::time::Duration;
        let mut screen = Screen::default();
        let start = std::time::Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert_eq!(
            screen.next_at(at(0), false),
            Next::Frame,
            "nothing before it"
        );
        // As `paint` records a frame sent.
        screen.unanswered.push_back((at(0), 6_000));
        screen.drawing = true;
        assert_eq!(screen.next_at(at(5), false), Next::Wait, "still being read");
        screen.heard_at(Report::Ready, at(20));
        assert_eq!(
            screen.next_at(at(21), false),
            Next::Wait,
            "no quiet moment yet"
        );
        assert_eq!(
            screen.next_at(at(30), false),
            Next::Wait,
            "10 ms is Konsole's own timer"
        );
        assert_eq!(
            screen.next_at(at(32), false),
            Next::Ask,
            "after the quiet moment: is it done drawing?"
        );
        assert_eq!(
            screen.next_at(at(33), false),
            Next::Wait,
            "the question is on its way"
        );
        assert_eq!(screen.next_at(at(90), false), Next::Wait, "still drawing");
        screen.heard_at(Report::Ready, at(95));
        assert_eq!(
            screen.next_at(at(95), false),
            Next::Frame,
            "done drawing: the next frame goes at once, no second quiet moment"
        );
        assert!(
            screen.answered == Some(at(20)),
            "a question's answer is not a frame's"
        );
        assert!(Screen::QUIET > Duration::from_millis(10));
        // A forced frame (a resize) never waits.
        screen.unanswered.push_back((at(96), 6_000));
        assert_eq!(screen.next_at(at(96), true), Next::Frame);
    }

    /// A terminal that shows each frame whole is never asked the extra
    /// question: two frames may be on their way, back to back.
    #[test]
    fn a_terminal_that_shows_frames_whole_is_never_held_back_for_drawing() {
        use super::super::input::Report;
        use std::time::Duration;
        let mut screen = Screen {
            synchronized: true,
            ..Screen::default()
        };
        let start = std::time::Instant::now();
        screen.unanswered.push_back((start, 6_000));
        assert_eq!(screen.next_at(start, false), Next::Frame);
        screen.unanswered.push_back((start, 6_000));
        assert_eq!(screen.next_at(start, false), Next::Wait);
        screen.heard_at(Report::Ready, start + Duration::from_millis(5));
        assert_eq!(
            screen.next_at(start + Duration::from_millis(5), false),
            Next::Frame,
            "no quiet moment, no question"
        );
    }

    fn page(doc: &[String], top: usize, rows: usize, sidebar: bool) -> Vec<String> {
        let mut frame: Vec<String> = (0..rows)
            .map(|r| {
                let text = doc[top + r].clone();
                if sidebar {
                    // Beside the chat: a panel that does not move with it,
                    // joined as the real frame joins it.
                    let item = if r < 8 || r % 7 == 0 {
                        format!("panel line {r}")
                    } else {
                        String::new()
                    };
                    beside(&text, 40, &item, 18)
                } else {
                    text
                }
            })
            .collect();
        frame.push("-".repeat(30));
        frame.push("> prompt".into());
        frame
    }

    /// The terminal ends up showing the frame, whatever the page did between
    /// two frames — a slow wheel, a spun one, a jump, a panel beside the chat —
    /// and a page that only moved is sent far fewer bytes than its rows.
    #[test]
    fn a_page_that_moved_is_scrolled_by_the_terminal_and_still_shows_the_frame() {
        let doc: Vec<String> = (0..400)
            .map(|n| {
                if n % 9 == 0 {
                    String::new()
                } else {
                    format!("line {n}: {}", "words ".repeat(1 + n % 5))
                }
            })
            .collect();
        // Beside the panel the terminal's scroll is used only where the
        // terminal shows a frame whole; where it does not, the panel is never
        // carried along (it was seen to jump in Konsole).
        for (sidebar, whole) in [(false, false), (true, true), (true, false)] {
            let rows = 30;
            let mut screen = Screen {
                synchronized: whole,
                ..Screen::default()
            };
            let mut term = MiniTerm::new(rows + 2, 60);
            let mut top: usize = 200;
            let mut seed = 7u64;
            let mut moved = 0usize;
            let mut shifted_bytes = 0usize;
            let mut shifted_whole = 0usize;
            for step in 0..300 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let by = ((seed >> 33) % 14) as usize;
                top = match (seed >> 20) % 5 {
                    0 => top.saturating_sub(by),
                    1 => (top + by).min(doc.len() - rows),
                    2 => (seed >> 40) as usize % (doc.len() - rows),
                    3 => top, // nothing moved
                    _ => (top + 1).min(doc.len() - rows),
                };
                let frame = page(&doc, top, rows, sidebar);
                let mut out = String::new();
                if step == 0 {
                    out.push_str(ansi::CLEAR_SCREEN);
                    screen.prev = vec![String::new(); frame.len()];
                }
                screen.rows(&frame, 60, &mut out);
                screen.prev = frame.clone();
                term.feed(&out);
                assert!(
                    term.shows(&frame),
                    "step {step} (top {top}, sidebar {sidebar}) differs"
                );
                if out.contains("\x1b[r") {
                    moved += 1;
                    shifted_bytes += out.len();
                    shifted_whole += frame.iter().map(|l| l.len() + 8).sum::<usize>();
                }
            }
            if sidebar && !whole {
                assert_eq!(moved, 0, "the panel was carried by the terminal's scroll");
                continue;
            }
            assert!(
                moved > 30,
                "the painter almost never used the terminal's scroll ({moved})"
            );
            // With the panel open too: the panel standing still beside the
            // page is no reason to send every row again (it once was).
            assert!(
                shifted_bytes * 2 < shifted_whole,
                "sidebar {sidebar}: moving the page cost {shifted_bytes} bytes against {shifted_whole} for every row"
            );
        }
    }

    /// The terminal's pace is read from the answers to frames of real size:
    /// a spinner's tick says how far away the terminal is, not how fast it
    /// reads a page, and a terminal that slows down is followed.
    #[test]
    fn the_terminals_pace_is_read_from_page_frames_and_followed() {
        use super::super::input::Report;
        use std::time::{Duration, Instant};
        let mut screen = Screen::default();
        assert!(
            !screen.keeps_up(),
            "a terminal that never said it draws whole"
        );
        screen.synchronized = true;
        let t = Instant::now();
        let answer = |screen: &mut Screen, bytes: usize, took: u64| {
            screen.unanswered.push_back((t, bytes));
            screen.heard_at(Report::Ready, t + Duration::from_millis(took));
        };
        assert!(screen.keeps_up(), "unknown counts as keeping up");
        answer(&mut screen, 300, 90);
        assert!(screen.keeps_up(), "a small frame does not measure the pace");
        assert!(screen.unanswered.is_empty(), "but it is answered");
        answer(&mut screen, 9_000, 6);
        assert!(screen.keeps_up(), "kitty: 6 ms a page");
        answer(&mut screen, 9_000, 80);
        answer(&mut screen, 9_000, 80);
        assert!(!screen.keeps_up(), "Konsole beside the panel: 80 ms a page");
        answer(&mut screen, 9_000, 10);
        answer(&mut screen, 9_000, 10);
        answer(&mut screen, 9_000, 10);
        assert!(screen.keeps_up(), "and back when it speeds up");
        // An answer nobody asked for (a frame handed over by an older build)
        // changes nothing.
        screen.heard_at(Report::Ready, t + Duration::from_secs(5));
        assert!(screen.keeps_up());
        // Fast to answer is not enough: one that draws mid-frame stutters.
        screen.heard_at(Report::Synchronized(false), t);
        assert!(
            !screen.keeps_up(),
            "Konsole answers fast and still stutters"
        );
    }

    /// A page moved beside the open side panel is moved by the terminal too;
    /// only the panel's side of the moved rows and the rows that came into
    /// view are sent. Measured on a real session before: every row of the
    /// screen on every step of the wheel (7.4 KB a step), because the panel
    /// standing still beside the page kept any band of rows from matching.
    #[test]
    fn a_wheel_step_beside_the_side_panel_sends_a_fraction_of_the_frame() {
        let mut app = test_app();
        app.sidebar = true;
        // A panel with text on most of its rows, as a session with a plan has.
        app.tasks = (0..30)
            .map(|n| json!({ "text": format!("step {n} of the plan"), "status": "todo" }))
            .collect();
        for n in 0..300 {
            app.blocks.push(Block::Text {
                text: format!("answer {n}: {}", "words that fill the line ".repeat(6)),
            });
        }
        let (rows, cols) = (45u16, 170u16);
        for synchronized in [true, false] {
            app.scroll = 0;
            let mut screen = Screen {
                synchronized,
                ..Screen::default()
            };
            let mut term = MiniTerm::new(rows as usize, cols as usize);
            term.feed(&screen.render(&mut app, rows, cols, true));
            assert!(app.sidebar_cols.is_some(), "the panel is open");
            let whole: usize = screen.prev.iter().map(String::len).sum();
            let mut sent = 0;
            for step in 0..40 {
                app.scroll_by(true, 1);
                let out = screen.render(&mut app, rows, cols, false);
                term.feed(&out);
                assert!(
                    term.shows(&screen.prev),
                    "step {step}: the terminal shows the frame"
                );
                if !synchronized {
                    // A terminal that draws mid-frame (Konsole): the panel is
                    // never carried by a scroll, nor written while it stands
                    // still — only the page's side of the rows is sent.
                    assert!(
                        !out.contains("\x1b[r"),
                        "step {step}: the panel was scrolled"
                    );
                    assert!(
                        seam(&out).is_none(),
                        "step {step}: the still panel was written"
                    );
                }
                sent += out.len();
            }
            let step = sent / 40;
            let bound = if synchronized {
                whole / 2
            } else {
                whole * 3 / 4
            };
            assert!(
                step < bound,
                "synchronized {synchronized}: a step cost {step} bytes against {whole} for the whole frame"
            );
        }
    }

    /// The contract between the screen and the model: what Rune said to the
    /// model about a call is on the screen in the same words. The screen may
    /// add its own chrome (timers, titles, counts) and may fold the body of a
    /// long result, but a notice — the one place Rune speaks to the model — is
    /// never shown differently or left out.
    ///
    /// Every notice sentence the engine can write is built here by the engine's
    /// own constructors, so a new sentence that the screen does not carry fails
    /// this test rather than a reader's trust.
    #[test]
    fn what_rune_told_the_model_about_a_call_is_on_the_screen_in_the_same_words() {
        use runeengine::tools::notice::{self, status};
        let log = std::path::Path::new("/tmp/session/tools_result/bg_1.log");
        let report = notice::Notice::default()
            .field("Process", "Completed")
            .field("Exit code", 0)
            .render();
        // (tool, state, arguments, what the model was given)
        let cases: Vec<(&str, ToolState, serde_json::Value, String)> = vec![
            (
                "terminal_use",
                ToolState::Ok,
                json!({"action":"run","command":"true"}),
                notice::exit_code(0),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"false"}),
                notice::prepend(&[notice::exit_code(101)], "error: boom\nsecond line"),
            ),
            (
                "terminal_use",
                ToolState::Ok,
                json!({"action":"run","command":"sleep 99","run_in_background":true}),
                notice::background_started(
                    "bg_1",
                    4242,
                    log,
                    runeengine::platform::InputWaitDetection::Every,
                ),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"sleep 99"}),
                notice::prepend(&[notice::command_timed_out(30, "why")], "partial output"),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"yes"}),
                notice::prepend(&[notice::command_output_limit("1 MiB")], "y\ny"),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"sleep 99"}),
                notice::prepend(&[notice::command_stopped_by_user()], ""),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"rm x"}),
                notice::failure(status::DENIED, "Permission refused by the user"),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"rm x"}),
                notice::failure(status::DENIED, "outside the workspace"),
            ),
            (
                "create_file",
                ToolState::Failed,
                json!({"action":"create","path":"a.txt","content":"x"}),
                notice::failure(status::NOT_EXECUTED, "the file exists"),
            ),
            (
                "web_search",
                ToolState::Failed,
                json!({"action":"search","query":"rust"}),
                notice::failure(status::SERVICE_UNAVAILABLE, "no search key"),
            ),
            (
                "web_fetch",
                ToolState::Failed,
                json!({"action":"fetch","url":"https://127.0.0.1/"}),
                notice::failure(
                    status::NOT_EXECUTED,
                    "Cannot fetch internal/private network URLs.",
                ),
            ),
            (
                "inspect_file",
                ToolState::Failed,
                json!({"action":"read","path":"a.txt"}),
                notice::failure(status::FAILED, "no such file"),
            ),
            (
                "terminal_use",
                ToolState::Failed,
                json!({"action":"run","command":"false"}),
                notice::repeated_failure(3, "exit 1"),
            ),
            (
                "manage_process",
                ToolState::Ok,
                json!({"action":"status","process_id":"bg_1"}),
                format!("{report}\nline one\nline two"),
            ),
        ];
        let norm = |text: &str| -> String {
            let plain = ansi::plain(text);
            let bare: String = plain
                .chars()
                .map(|c| {
                    if "│╭╮╰╯─┌┐└┘├┤●○◐▸▾›:".contains(c) {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            bare.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        for (name, state, args, payload) in cases {
            let mut app = test_app();
            app.blocks.push(Block::user_timed(
                "PROMPT".to_string(),
                None::<String>,
                Vec::new(),
            ));
            let mut t = Tool::new("c1".into(), name.into(), String::new());
            t.absorb(&args.to_string());
            t.settle(&args);
            t.state = state;
            t.ended = Some(std::time::Instant::now());
            t.content = payload.clone();
            t.open = true;
            app.blocks.push(Block::of(t));
            let drawn = norm(
                &transcript_into(None, &app, 110, true, true)
                    .sink
                    .lines
                    .join("\n"),
            );
            for line in payload.lines() {
                let said = norm(line);
                // The envelope is the screen's to draw as its own marking.
                if said.is_empty() || said == "<tool_notice>" || said == "</tool_notice>" {
                    continue;
                }
                let said = said
                    .trim_start_matches("<tool_notice>")
                    .trim_end_matches("</tool_notice>")
                    .trim()
                    .to_string();
                assert!(
                    drawn.contains(&said),
                    "{name}: the model was told `{said}` and the screen does not say it:\n{drawn}"
                );
            }
        }
    }

    #[test]
    fn probe_paint_cost_with_long_transcript() {
        let mut app = test_app();
        for i in 0..1500 {
            app.blocks.push(Block::Text {
                text: format!("line {i} some assistant text that wraps maybe"),
            });
        }
        let mut screen = Screen::default();
        let _ = screen.render(&mut app, 40, 140, true);
        while app
            .transcript_cache
            .as_ref()
            .is_some_and(|cache| cache.first > 0)
        {
            let _ = screen.render(&mut app, 40, 140, false);
        }
        let t = std::time::Instant::now();
        for c in 0..20 {
            app.input.push('a');
            app.cursor = app.input.len();
            let _ = c;
            let _ = screen.render(&mut app, 40, 140, false);
        }
        eprintln!(
            "PAINT per key: {:.3}ms",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );
    }

    #[test]
    fn composer_starts_at_one_row_and_grows_to_eleven_without_eating_short_screens() {
        let mut app = test_app();
        let (empty, _) = composer(&mut app, 120, 30);
        assert_eq!(empty.lines.len(), 3);
        assert_eq!(app.composer_hit_rows.len(), 1);
        app.input = (0..30)
            .map(|n| format!("input row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.len();
        let (sink, cursor) = composer(&mut app, 120, 30);
        assert_eq!(app.composer_hit_rows.len(), 11);
        assert_eq!(sink.lines.len(), 13);
        assert!(cursor.is_some());
        for height in [8, 12, 20, 50] {
            let frame = compose(&mut app, height, 80);
            assert_eq!(frame.len(), height as usize);
            assert!(app.composer_hit_rows.len() <= 11);
            assert!(app.drawn_viewport >= 2, "height {height}");
        }
    }

    #[test]
    fn editor_wrap_and_hit_projection_preserve_glyphs_source_and_visual_columns() {
        let samples = [
            "a界b e\u{301}z",
            "👨\u{200d}💻xx☀\u{fe0f} Q",
            "  a\tb\n\nlast\n",
            "a abcdefghijk",
            "x",
        ];
        for sample in samples {
            let (shown, source) = ansi::editor_text(sample);
            for width in 1..=30 {
                let rows = compose_rows(&shown, width);
                for (start, row) in &rows {
                    assert!(shown.is_char_boundary(*start));
                    assert_eq!(&shown[*start..*start + row.len()], row);
                    // A glyph wider than a one-cell test row is indivisible;
                    // every other row must fit its declared width.
                    assert!(
                        ansi::width(row) <= width || ansi::editor_glyphs(row).count() == 1,
                        "{sample:?} width {width}: {rows:?}"
                    );
                    let geometry = ComposerRow {
                        start: source[*start],
                        text: row.clone(),
                        source: source[*start..=*start + row.len()]
                            .iter()
                            .map(|at| at - source[*start])
                            .collect(),
                        first_col: 5,
                    };
                    for col in 0..=width + 4 {
                        let at = geometry.caret_at((col + 5) as u16);
                        assert!(sample.is_char_boundary(at), "{sample:?} col {col}");
                        assert!(at >= geometry.start && at <= sample.len());
                    }
                }
                for (start, row) in rows {
                    for (offset, _, _) in ansi::editor_glyphs(&row) {
                        let col = ansi::width(&row[..offset]);
                        assert_eq!(
                            ansi::byte_at_column(&row, col),
                            offset,
                            "{sample:?} width {width} row start {start}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn composer_wrapping_keeps_joined_and_variant_glyphs_in_one_visual_row() {
        for glyph in ["👨\u{200d}💻", "☀\u{fe0f}", "e\u{301}"] {
            let width = 8;
            let prefix = "x".repeat(width - ansi::width(glyph));
            let first = format!("{prefix}{glyph}");
            let text = format!("{first}Q");
            let rows = compose_rows(&text, width);
            assert_eq!(
                rows,
                vec![(0, first.clone()), (first.len(), "Q".into())],
                "glyph {glyph:?}"
            );
            for (_, row) in rows {
                assert!(ansi::width(&row) <= width);
            }
        }
    }

    #[test]
    fn composer_cursor_is_hardware_cursor_without_injected_cell_gap() {
        let mut app = test_app();
        // 1. When empty: placeholder is displayed cleanly without dummy characters,
        // and hardware cursor target is placed at column 5 (directly against 'a' of "ask").
        let (empty_sink, empty_target) = composer(&mut app, 80, 3);
        let empty_plain = ansi::plain(&empty_sink.lines.join("\n"));
        assert!(empty_plain.contains("› ask, or / for commands"));
        assert!(
            !empty_plain.contains("› |ask") && !empty_plain.contains("› │ask"),
            "no dummy box-drawing cursor character"
        );
        assert_eq!(empty_target.map(|(_, col)| col), Some(5));

        // 2. Cursor in typed text: characters remain contiguous without dummy cursor splitting them.
        app.input = "abc".into();
        app.cursor = 2; // between 'b' and 'c'
        let (sink, target) = composer(&mut app, 80, 3);
        let plain = ansi::plain(&sink.lines.join("\n"));
        assert!(
            plain.contains("› abc"),
            "text 'abc' is rendered intact without injected cell: {plain}"
        );
        // '│ › ' is 4 cols, 'ab' is 2 cols -> cursor column = 1 + 4 + 2 = 7 (directly between 'b' and 'c')
        assert_eq!(target.map(|(_, col)| col), Some(7));

        // When cursor is at the end
        app.cursor = 3;
        let (_, target_end) = composer(&mut app, 80, 3);
        // '│ › ' is 4 cols, 'abc' is 3 cols -> cursor column = 1 + 4 + 3 = 8 (directly against 'c')
        assert_eq!(target_end.map(|(_, col)| col), Some(8));

        // 3. Word wrap: second line does not start with a leading space
        let rows = compose_rows("abc def", 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "abc");
        assert_eq!(
            rows[1].1, "def",
            "wrapped line must not start with a leading space"
        );
    }

    // ── The sidebar ──────────────────────────────────────────────────────────

    #[test]
    fn the_sidebar_carries_the_window_the_cache_and_the_bill() {
        let mut app = App::for_test_with(json!({
            "settings": { "model": "mimo-v2.5-pro" },
            "context": 1_050_000,
            "price": { "input": 0.435, "cached": 0.0036, "output": 0.87 },
        }));
        app.input_tokens = 525_000;
        app.output_tokens = 1_200;
        app.cached_tokens = 262_500;
        app.cost = 0.1234;
        let text = sidebar(&mut app, 40, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("525,000 / 1,050,000"), "{text}");
        assert!(text.contains("262,500"), "{text}");
        assert!(!text.contains("50%"), "{text}");
        assert!(text.contains("$0.1234"), "{text}");
    }

    /// A model nobody has published a price for must not be billed at zero.
    #[test]
    fn an_unpriced_model_says_so_rather_than_showing_nothing_spent() {
        let mut app = test_app();
        let text = sidebar(&mut app, 40, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("not priced"), "{text}");
        assert!(
            !text.contains("$0"),
            "a zero here is a claim, and the wrong one"
        );
    }

    #[test]
    fn sidebar_rows_fit_within_panel_width() {
        let mut app = App::for_test_with(json!({ "context": 1_000 }));
        app.input_tokens = 500;
        app.title = Some("Rune UI & Architecture".into());
        app.steps = 18;
        app.turns = 3;
        for width in [24usize, 40, 60] {
            for row in sidebar(&mut app, width, 1000) {
                assert!(
                    ansi::width(&row) <= width,
                    "a panel row overflowed at {width}"
                );
            }
        }
    }

    /// The answer carries **no vertical rule at all**. The owner reported it
    /// on a live session: `│` is the spine of a run of steps, so the same mark
    /// down the side of the finished answer made the answer read as one more
    /// step. No box either — that argument was settled earlier — and nothing
    /// filled.
    #[test]
    fn an_answer_carries_no_edge_no_box_and_no_fill() {
        let mut app = App::for_test();
        app.blocks.push(Block::Text {
            text: "a sentence from the model".into(),
        });
        let mut sink = Sink::default();
        draw_block(&app.blocks[0], 60, &mut sink);
        let body = sink.lines.first().expect("the answer must be drawn");
        assert!(
            !body.contains('│'),
            "the spine is back on the answer: {body:?}"
        );
        assert!(
            !body.contains('┌') && !body.contains('┐'),
            "a box came back: {body:?}"
        );
        assert!(
            !body.contains("\x1b[48"),
            "nothing may paint a background: {body:?}"
        );
        assert!(body.contains("a sentence from the model"));
    }

    /// With no split panel, the answer owns the full transcript width.
    #[test]
    fn prose_uses_the_available_width_when_the_split_is_off() {
        let mut app = App::for_test();
        app.blocks.push(Block::Text {
            text: "word ".repeat(120),
        });
        let mut sink = Sink::default();
        draw_block(&app.blocks[0], 300, &mut sink);
        assert!(
            sink.lines.iter().any(|line| ansi::width(line) > 92),
            "the removed measure still cuts the answer: {:?}",
            sink.lines
                .iter()
                .map(|line| ansi::width(line))
                .collect::<Vec<_>>()
        );
        assert!(sink.lines.iter().all(|line| ansi::width(line) <= 302));
    }

    #[test]
    fn duration_is_formatted_cleanly() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(60), "1m 00s");
        assert_eq!(format_duration(382), "6m 22s");
        assert_eq!(format_duration(3665), "1h 01m");
    }

    // ── The footer ───────────────────────────────────────────────────────────

    /// The owner asked for the shortcut row to go. It is the kind of thing that
    /// grows back one key at a time, so it is asserted away.
    #[test]
    fn no_shortcut_row_stands_under_the_composer() {
        let mut app = test_app();
        let quiet = footer(&mut app, 24, 120)
            .0
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        for gone in [
            "ctrl+o",
            "ctrl+g",
            "ctrl+t",
            "ctrl+u",
            "/help",
            "tab",
            "shift+tab",
        ] {
            assert!(
                !quiet.contains(gone),
                "the shortcut row grew {gone} back:\n{quiet}"
            );
        }
        // What the footer *does* say: who the question is going to.
        assert!(
            quiet.contains("Ask"),
            "the mode belongs against the composer"
        );

        // A question still carries its own three keys, because they are the
        // answer to what is on screen rather than a list of everything.
        app.pending = Some(super::super::blocks::Ask {
            req: "r".into(),
            action: "a".into(),
            preview: "p".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let asking = footer(&mut app, 24, 120)
            .0
            .lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(asking.contains("Allow once") && asking.contains("Refuse"));
        assert!(
            asking.contains("← → choose"),
            "the card explains its own two keys"
        );
    }

    /// The version is a footnote, and a footnote goes at the foot of the page.
    /// It used to be the second-largest thing on an empty screen.
    /// The last line of the screen, in its far corner — beside the path and the
    /// mode, where the owner asked for it. It had been sitting on the model
    /// line, which is the loudest line in the footer and the wrong company for
    /// the least important fact on the screen.
    #[test]
    fn the_version_stands_in_the_far_corner_of_the_last_line() {
        let app = test_app();
        let cols = 100;
        let version = runeengine::VERSION;
        let line = state_line(&app, cols);
        let plain = ansi::plain(&line);
        assert!(
            plain.trim_end().ends_with(version),
            "the version is not in the corner: {plain:?}"
        );
        assert!(
            ansi::width(&line) >= cols - 1,
            "it drifted in from the edge"
        );
        // The path and the mode are its company, which is the point of moving it.
        assert!(
            plain.contains('│'),
            "the last line lost the path and the mode"
        );
        // The version appears in the top banner and in the footer corner.
        let whole: String = compose(&mut test_app(), 30, cols as u16)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            whole.matches(version).count(),
            2,
            "the version is in the banner and the footer"
        );
    }

    /// Three names, three risks, three inks — independent of active theme.
    #[test]
    fn each_permission_mode_is_named_and_coloured_for_what_it_costs() {
        for (mode, name) in [("ask", "Ask"), ("auto", "Auto"), ("full", "Full")] {
            let mut app = test_app();
            app.mode = mode.into();
            let line = addressed_to(&app);
            assert!(
                ansi::plain(&line).contains(name),
                "{mode} is not written as {name}"
            );
        }
        // Each one has its own ink, or the colour says nothing at all.
        let inks = [mode_of("ask").1, mode_of("auto").1, mode_of("full").1];
        assert!(
            inks[0] != inks[1] && inks[0] != inks[2] && inks[1] != inks[2],
            "two modes share an ink"
        );
    }

    #[test]
    fn addressed_to_suppresses_duplicate_thinking_on_composer_header() {
        let mut app = test_app();
        app.settings["settings"]["provider"] = serde_json::json!("9router");
        app.settings["settings"]["model"] = serde_json::json!("kr/claude-opus-5-thinking");
        app.settings["settings"]["thinking"] = serde_json::json!(true);
        app.settings["thinking_required"] = serde_json::json!(true);

        let header = ansi::plain(&addressed_to(&app));
        assert!(
            header.contains("kr/claude-opus-5-thinking"),
            "must show model name: {header}"
        );
        assert!(
            !header.contains("kr/claude-opus-5-thinking thinking"),
            "must NOT duplicate thinking: {header}"
        );

        // Model without explicit effort does not show bare thinking word
        app.settings["settings"]["model"] = serde_json::json!("kr/claude-opus-5");
        app.settings["thinking_required"] = serde_json::json!(false);
        let header2 = ansi::plain(&addressed_to(&app));
        assert!(
            !header2.contains("kr/claude-opus-5 thinking"),
            "must NOT show bare thinking word: {header2}"
        );

        // Model with explicit effort level shows effort
        app.settings["settings"]["effort"] = serde_json::json!("high");
        let header3 = ansi::plain(&addressed_to(&app));
        assert!(
            header3.contains("kr/claude-opus-5 high"),
            "must show effort level: {header3}"
        );

        // Model with thinking_required=true -> no "thinking" label
        app.settings["settings"]["model"] = serde_json::json!("ag/gemini-3.8-flash-high");
        app.settings["settings"]["effort"] = serde_json::Value::Null;
        app.settings["thinking_required"] = serde_json::json!(true);
        let header4 = ansi::plain(&addressed_to(&app));
        assert!(
            !header4.contains("thinking"),
            "must NOT show thinking for mandatory model: {header4}"
        );

        // Model with thinking_required=true + effort -> shows effort, not "thinking"
        app.settings["settings"]["effort"] = serde_json::json!("high");
        let header5 = ansi::plain(&addressed_to(&app));
        assert!(header5.contains("high"), "must show effort: {header5}");
        assert!(
            !header5.contains(" thinking"),
            "must NOT show thinking word with effort: {header5}"
        );
    }

    #[test]
    fn classifier_indicator_reflects_mode_and_policy_in_header_and_footer() {
        let mut app = test_app();
        app.mode = "ask".into();
        app.settings["classifier_status"] = serde_json::json!({
            "enabled": true,
            "active": false,
            "reason": "Disabled in Ask mode"
        });
        let header = addressed_to(&app);
        let footer = state_line(&app, 120);
        assert!(ansi::plain(&header).contains("Ask"));
        assert!(!ansi::plain(&header).contains("🛡"));
        assert!(!ansi::plain(&header).contains("off"));
        assert!(ansi::plain(&footer)
            .contains("asks before actions run; classifier is inactive in ask mode"));
        assert!(!ansi::plain(&footer).contains("🛡"));

        // Auto mode with active policy
        app.mode = "auto".into();
        app.settings["classifier_status"] = serde_json::json!({
            "enabled": true,
            "policy": true,
            "active": true,
            "reason": "Active for proposed tool actions"
        });
        let header = addressed_to(&app);
        let footer = state_line(&app, 120);
        assert!(ansi::plain(&header).contains("Auto"));
        assert!(!ansi::plain(&header).contains("🛡"));
        assert!(ansi::plain(&footer)
            .contains("works freely inside workspace supervised by active classifier policy"));
        assert!(!ansi::plain(&footer).contains("🛡"));

        // Full mode with classifier disabled
        app.mode = "full".into();
        app.settings["classifier_status"] = serde_json::json!({
            "enabled": false,
            "active": false,
            "reason": "Disabled by configuration"
        });
        app.settings["configuration"]["classifier.enabled"] = serde_json::json!(false);
        let header = addressed_to(&app);
        let footer = state_line(&app, 120);
        assert!(ansi::plain(&header).contains("Full"));
        assert!(!ansi::plain(&header).contains("🛡"));
        assert!(ansi::plain(&footer)
            .contains("unconfined system access; classifier is disabled by configuration"));
        assert!(!ansi::plain(&footer).contains("🛡"));
    }

    /// The bug a second window made visible: a window that learned of the
    /// switch to Full from the engine kept the reason it was told in Ask mode
    /// ("Disabled in Ask mode"), read it as "there is a policy", and said the
    /// classifier was supervising a session that had no policy file. The footer
    /// follows what the engine says about the classifier, in whatever mode.
    #[test]
    fn the_footer_follows_the_engines_classifier_state_not_a_reason_from_another_mode() {
        let mut app = test_app();
        app.mode = "ask".into();
        app.settings["classifier_status"] = serde_json::json!({
            "enabled": true, "policy": false, "active": false, "reason": "Disabled in Ask mode"
        });
        // Another window switches to Full; the engine says so, with the facts.
        app.on_event(runeengine::Event::Session {
            session: app.session.unwrap(),
            project: "/p".into(),
            mode: "full".into(),
            confined: false,
            sandbox: runeengine::proto::SandboxReport {
                kind: "none".into(),
                abi: 0,
                enforced: false,
                reason: None,
                escapes: Vec::new(),
            },
            classifier: serde_json::json!({
                "enabled": true, "policy": false, "active": false,
                "reason": "Inactive: no CLASSIFIER.md"
            }),
        });
        let footer = ansi::plain(&state_line(&app, 140));
        assert!(
            footer.contains("classifier is inactive because no policy file was found"),
            "{footer}"
        );
        assert!(
            !footer.contains("supervised by active classifier policy"),
            "{footer}"
        );
        // And with a policy, the same window says it is supervised.
        app.on_event(runeengine::Event::Session {
            session: app.session.unwrap(),
            project: "/p".into(),
            mode: "full".into(),
            confined: false,
            sandbox: runeengine::proto::SandboxReport {
                kind: "none".into(),
                abi: 0,
                enforced: false,
                reason: None,
                escapes: Vec::new(),
            },
            classifier: serde_json::json!({
                "enabled": true, "policy": true, "active": true,
                "reason": "Active for proposed tool actions"
            }),
        });
        assert!(
            ansi::plain(&state_line(&app, 140)).contains("supervised by active classifier policy")
        );
    }

    #[test]
    fn questions_state_line_shows_esc_cancellation_info() {
        let mut app = test_app();
        app.mode = "ask".into();
        let question_json = serde_json::json!([
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
        let questions = serde_json::from_value(question_json).unwrap();
        app.questions = Some(crate::ui::questions::State::new("r1".into(), questions));

        // When questions is active and not armed: normal mode displayed
        let footer = state_line(&app, 120);
        assert!(ansi::plain(&footer)
            .contains("asks before actions run; classifier is inactive in ask mode"));
        assert!(ansi::plain(&footer).contains("[Ask]"));

        // When armed: status replaces the entire mode description (no [Ask])
        app.note("press esc again to close");
        let footer_armed = state_line(&app, 120);
        assert!(ansi::plain(&footer_armed).contains("press esc again to close"));
        assert!(!ansi::plain(&footer_armed).contains("[Ask]"));
    }

    /// Owner, 2026-10-10: the version a window shows is the build it runs.
    /// A window still on 0.3.7 showed its host's 0.3.8 and looked updated
    /// when it was not.
    #[test]
    fn the_version_shown_is_the_windows_own_build_never_its_hosts() {
        let mut app = test_app();
        app.host_version = "9.9.9".into();
        let line = ansi::plain(&state_line(&app, 160));
        assert!(line.trim_end().ends_with(runeengine::VERSION), "{line}");
        assert!(!line.contains("9.9.9"), "{line}");
        let banner = ansi::plain(&super::super::banner::render(&app, 120).join("\n"));
        assert!(!banner.contains("9.9.9"), "{banner}");
    }

    #[test]
    fn state_line_focuses_on_status_without_mode_pill_and_reverts_to_mode_when_cleared() {
        let mut app = test_app();
        app.mode = "ask".into();

        // 1. Idle state: mode pill and mode description are displayed
        let idle = state_line(&app, 120);
        let plain_idle = ansi::plain(&idle);
        assert!(plain_idle.contains("[Ask]"));
        assert!(plain_idle.contains("asks before actions run; classifier is inactive in ask mode"));

        // 2. Status message (e.g. file attached, draft, command usage, settings):
        // focus purely on the status message without mixing mode pill [Ask]
        app.note("File attached · /detach <number> removes it");
        let attached = state_line(&app, 120);
        let plain_attached = ansi::plain(&attached);
        assert!(plain_attached.contains("File attached · /detach <number> removes it"));
        assert!(!plain_attached.contains("[Ask]"));

        app.note("Project draft");
        let draft = state_line(&app, 120);
        let plain_draft = ansi::plain(&draft);
        assert!(plain_draft.contains("Project draft"));
        assert!(!plain_draft.contains("[Ask]"));

        app.note("Usage: /logs [all|warn|error] · what happened behind the conversation");
        let usage = state_line(&app, 120);
        let plain_usage = ansi::plain(&usage);
        assert!(plain_usage
            .contains("Usage: /logs [all|warn|error] · what happened behind the conversation"));
        assert!(!plain_usage.contains("[Ask]"));

        // 3. Retry status: pure message, no [Ask]
        app.clear_status();
        app.retry_status = Some("429 Too Many Requests — retrying in 5s (1 of 3)".into());
        let retrying = state_line(&app, 120);
        let plain_retrying = ansi::plain(&retrying);
        assert!(plain_retrying.contains("429 Too Many Requests — retrying in 5s (1 of 3)"));
        assert!(!plain_retrying.contains("[Ask]"));

        // 4. When status is cleared (e.g. after 8s timeout): reverts to mode description
        app.retry_status = None;
        let reverted = state_line(&app, 120);
        let plain_reverted = ansi::plain(&reverted);
        assert!(plain_reverted.contains("[Ask]"));
        assert!(
            plain_reverted.contains("asks before actions run; classifier is inactive in ask mode")
        );
    }

    /// A rule that stops short of the edge, or runs past it, is the first thing
    /// the eye finds — and the rules are the whole separation now.
    #[test]
    fn every_rule_reaches_exactly_the_edge_and_no_row_overflows() {
        let mut app = test_app();
        for cols in [40usize, 60, 80, 120] {
            for row in composer(&mut app, cols, 3).0.lines {
                let width = ansi::width(&row);
                if ansi::plain(&row).starts_with('─') {
                    assert_eq!(
                        width,
                        cols,
                        "a rule fell short at {cols}: {:?}",
                        ansi::plain(&row)
                    );
                }
                assert!(width <= cols, "a composer row overflowed at {cols}");
            }
        }
        let ask = super::super::blocks::Ask {
            req: "r1".into(),
            preview: "$ ls -la ~/Downloads".into(),
            remember_as: Some("ls".into()),
            reach: vec!["/home/dev/Downloads".into()],
            sensitive: Vec::new(),
            action: "list what is in the Downloads folder".into(),
            full_access: false,
            reason: None,
        };
        for row in permission_card(&ask, 0, 100, 40, None).lines {
            assert!(
                ansi::width(&row) <= 100,
                "permission row: {:?}",
                ansi::plain(&row)
            );
        }
    }

    #[test]
    fn format_bullet_note_colours_counters_by_sign() {
        let edit_note = format_bullet_note("Edit", "−158 +141");
        assert!(edit_note.contains(&format!("{REMOVED}−158{RESET}")));
        assert!(edit_note.contains(&format!("{ADDED}+141{RESET}")));

        let write_note = format_bullet_note("Write", "+10");
        assert!(write_note.contains(&format!("{ADDED}+10{RESET}")));

        let read_note = format_bullet_note("Read", "1 - 50");
        assert!(read_note.contains(&format!("{FAINT}1 - 50{RESET}")));
    }

    #[test]
    fn file_preview_keeps_four_visual_rows_and_puts_omission_footer_last() {
        let code = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = steps::Body::Code(code);
        let mut sink_closed = Sink::default();
        draw_file_body(&body, "test.txt", false, false, 80, &mut sink_closed, 0);
        let text_closed = plain_of(&sink_closed);
        assert!(text_closed.contains("line 4"));
        assert!(!text_closed.contains("line 5"));
        assert!(text_closed.contains("… 16 lines cut. Click to open all"));

        let mut sink_open = Sink::default();
        draw_file_body(&body, "test.txt", true, false, 80, &mut sink_open, 0);
        let text_open = plain_of(&sink_open);
        assert!(text_open.contains("line 20"));
        assert!(!text_open.contains("lines cut"));
    }

    #[test]
    fn draw_terminal_limits_commands_and_output_when_closed() {
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run".into());
        let cmds = (1..=6)
            .map(|i| json!({ "command": format!("echo {i}") }))
            .collect::<Vec<_>>();
        tool.settle(&json!({ "commands": cmds }));
        tool.output = (1..=10)
            .map(|i| format!("out {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        tool.state = ToolState::Ok;

        let mut sink_closed = Sink::default();
        draw_terminal(&tool, false, false, 80, &mut sink_closed, 0);
        let text_closed = plain_of(&sink_closed);
        assert!(text_closed.contains("echo 1"));
        assert_eq!(
            sink_closed.len(),
            9,
            "the folded box must not grow for multiple commands"
        );
        // Folded shows the newest lines: progress while running, the outcome
        // once done (audit U8).
        assert!(text_closed.contains("out 1") && text_closed.contains("out 4"));
        assert!(!text_closed.contains("out 5"));
        assert!(ansi::plain(&sink_closed.lines[sink_closed.len() - 2])
            .contains("… 6 lines cut. Click to open all"));

        let mut sink_open = Sink::default();
        draw_terminal(&tool, true, false, 80, &mut sink_open, 0);
        let text_open = plain_of(&sink_open);
        assert!(text_open.contains("echo 6"));
        assert!(!text_open.contains("commands cut"));
        assert!(text_open.contains("out 10"));
        assert!(!text_open.contains("lines cut"));

        // Only the tail is cleaned, yet the count still covers every line.
        tool.output = (1..=1000)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut sink_big = Sink::default();
        draw_terminal(&tool, false, false, 80, &mut sink_big, 0);
        let text_big = sink_big.lines.join("\n");
        assert!(text_big.contains("row 1") && text_big.contains("row 4"));
        assert!(!text_big.contains("row 5") && !text_big.contains("row 1000"));
        assert!(
            text_big.contains("… 996 lines cut. Click to open all"),
            "{text_big}"
        );
    }

    #[test]
    fn draw_file_body_patch_contains_only_content_lines_no_headers() {
        let body = steps::Body::Patch {
            before: "first line\nold line\nlast line".into(),
            after: "first line\nnew line\nlast line".into(),
        };
        let mut sink = Sink::default();
        draw_file_body(&body, "utils.py", true, false, 80, &mut sink, 0);
        let text = sink.lines.join("\n");
        assert!(
            !text.contains("---"),
            "diff must not contain --- file header"
        );
        assert!(
            !text.contains("+++"),
            "diff must not contain +++ file header"
        );
        assert!(!text.contains("@@"), "diff must not contain @@ hunk header");
        assert!(text.contains("-old line"));
        assert!(text.contains("+new line"));
        assert!(text.contains(" first line"));
    }

    #[test]
    fn draw_file_body_soft_wraps_long_lines_and_preserves_ink() {
        let long_line = (1..=100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let body = steps::Body::Patch {
            before: long_line.clone(),
            after: "short".into(),
        };
        let mut sink = Sink::default();
        draw_file_body(&body, "numbers.txt", true, false, 60, &mut sink, 0);
        let text = sink.lines.join("\n");
        assert!(
            text.contains("100"),
            "end of long line must be wrapped and visible"
        );
        assert!(text.contains("-1"), "start of line must be marked");
        assert!(text.contains(&format!("{REMOVED}")));
    }

    #[test]
    fn dismissed_questions_keep_real_answers_and_mark_only_unanswered_questions() {
        let app = test_app();
        let mut tool = Tool::new("q1".into(), "ask_user_questions".into(), "Ask user".into());
        tool.settle(&json!({"questions":[
            {"question":"First question", "type":"single", "options":[{"label":"Actual choice", "description":"choice"}]},
            {"question":"Second question", "type":"single", "options":[{"label":"Other choice", "description":"choice"}]}
        ]}));
        tool.state = ToolState::Ok;
        tool.open = true;
        tool.content = "<tool_notice>\nStatus : Success\n</tool_notice>\n\n{\"status\":\"answered\",\"answers\":[{\"id\":\"q0\",\"selected\":[\"q0o0\"],\"custom\":\"\"},{\"id\":\"q1\",\"selected\":[],\"custom\":\"\"}]}".into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, true, 100, &mut sink);
        let text = sink
            .lines
            .iter()
            .map(|line| ansi::plain(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Actual choice"), "{text}");
        assert!(text.contains("Second question"), "{text}");
        assert_eq!(text.matches("— (no answer)").count(), 1, "{text}");
        assert!(!text.contains("<tool_notice>"), "{text}");
    }

    #[test]
    fn expanded_read_keeps_path_and_range_and_empty_errors_have_no_box() {
        let app = test_app();
        let mut read = Tool::new("r1".into(), "inspect_file".into(), "Read target".into());
        read.settle(&json!({"path":"src/example.rs", "view_range":[3,8]}));
        read.state = ToolState::Ok;
        read.content = "3| actual data".into();
        for open in [false, true] {
            read.open = open;
            let mut sink = Sink::default();
            draw_step(&app, 0, &read, false, 100, &mut sink);
            let text = sink
                .lines
                .iter()
                .map(|line| ansi::plain(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Read  src/example.rs"), "{text}");
            assert!(text.contains("3 - 8"), "{text}");
            assert!(text.contains("actual data"), "{text}");
        }
        let mut sink = Sink::default();
        draw_error_box("", "error", false, "", 100, &mut sink, 0);
        assert!(sink.lines.is_empty());
        draw_file_body(
            &steps::Body::Patch {
                before: "same".into(),
                after: "same".into(),
            },
            "example.rs",
            true,
            false,
            100,
            &mut sink,
            0,
        );
        let text = sink.lines.join("\n");
        assert!(text.contains("diff"));
        assert!(!text.contains("no textual change"));
    }

    #[test]
    fn open_patch_preserves_diff_without_duplicate_confirmation() {
        let app = test_app();
        let mut patch = Tool::new("p1".into(), "patch_file".into(), "change numbers".into());
        patch.settle(&json!({ "path": "numbers.txt", "old_str": "1", "new_str": "80" }));
        patch.state = ToolState::Ok;
        patch.content = "patched numbers.txt (-1 +1)".into();
        patch.open = true;

        let mut sink = Sink::default();
        draw_step(&app, 0, &patch, false, 80, &mut sink);
        let text = sink.lines.join("\n");
        assert!(text.contains("-1"), "expanded patch must show removed text");
        assert!(
            text.contains("+80"),
            "expanded patch must show replacement text"
        );
        assert!(
            !text.contains("patched numbers.txt (-1 +1)"),
            "expanded diff must not repeat the write confirmation"
        );
    }

    #[test]
    fn streaming_patch_cannot_be_opened_to_show_body() {
        let app = test_app();
        let mut patch = Tool::new("p1".into(), "patch_file".into(), "patching file".into());
        patch.state = ToolState::Running;
        patch.open = true;
        patch.progress.path = Some("numbers.txt".into());
        patch.progress.old_text = "old".into();
        patch.progress.new_text = "new".into();

        let mut sink = Sink::default();
        draw_step(&app, 0, &patch, false, 80, &mut sink);
        let text = sink.lines.join("\n");
        assert!(
            !text.contains("╭─ diff"),
            "diff box should not be visible while patch is streaming"
        );
    }

    #[test]
    fn last_step_or_open_step_omits_trailing_spine_line() {
        let mut app = test_app();
        let mut t1 = Tool::new("t1".into(), "patch_file".into(), "step 1".into());
        t1.state = ToolState::Ok;
        t1.open = true;
        app.blocks.push(Block::of(t1));

        let mut sink = Sink::default();
        draw_steps(&app, &[0], 80, &mut sink);
        let non_empty: Vec<&String> = sink.lines.iter().filter(|l| !l.trim().is_empty()).collect();
        let last_line = non_empty.last().map(|s| ansi::plain(s)).unwrap_or_default();
        assert_ne!(
            last_line.trim(),
            "│",
            "last open step should not have a dangling spine line"
        );
    }

    #[test]
    fn last_open_thought_has_no_vertical_spine() {
        let mut app = test_app();
        app.blocks.push(Block::Thinking {
            text: "This is a thought that is open".into(),
            open: true,
        });
        let mut sink = Sink::default();
        draw_steps(&app, &[0], 80, &mut sink);
        let text = sink.lines.join("\n");
        assert!(text.contains("This is a thought that is open"));
        for line in &sink.lines {
            let plain = ansi::plain(line);
            if plain.contains("This is a thought") {
                assert!(
                    !plain.contains('│'),
                    "open thought line should not contain vertical spine │: {plain}"
                );
            }
        }
    }

    #[test]
    fn last_step_terminal_tool_has_no_vertical_spine() {
        let mut app = test_app();
        let mut terminal = Tool::new("c1".into(), "terminal_use".into(), "run command".into());
        terminal.settle(&json!({ "command": "ls ~/Documents" }));
        terminal.output = "file1.txt\nfile2.txt".into();
        terminal.state = ToolState::Ok;
        app.blocks.push(Block::of(terminal));

        let mut sink = Sink::default();
        draw_steps(&app, &[0], 80, &mut sink);
        let text = ansi::plain(&sink.lines.join("\n"));
        assert!(text.contains("╭─ Bash"));
        for line in &sink.lines {
            let plain = ansi::plain(line);
            if plain.contains("run command") {
                continue;
            }
            if plain.trim().is_empty() {
                continue;
            }
            assert!(
                plain.starts_with("     "),
                "last step terminal line should be indented with 5 spaces without spine: {plain}"
            );
            assert_ne!(
                plain.chars().nth(2),
                Some('│'),
                "last step terminal line should not have left spine │: {plain}"
            );
        }
        let last_non_empty = sink
            .lines
            .iter()
            .rfind(|l| !l.trim().is_empty())
            .map(|s| ansi::plain(s))
            .unwrap_or_default();
        assert_ne!(
            last_non_empty.trim(),
            "│",
            "last step should not have a dangling spine line"
        );
    }

    #[test]
    fn non_last_step_terminal_tool_has_vertical_spine() {
        let mut app = test_app();
        let mut terminal = Tool::new("c1".into(), "terminal_use".into(), "run command".into());
        terminal.settle(&json!({ "command": "ls ~/Documents" }));
        terminal.output = "file1.txt\nfile2.txt".into();
        terminal.state = ToolState::Ok;
        app.blocks.push(Block::of(terminal));
        let step2 = Tool::new("c2".into(), "inspect_file".into(), "read file".into());
        app.blocks.push(Block::of(step2));

        let mut sink = Sink::default();
        draw_steps(&app, &[0, 1], 80, &mut sink);
        let has_spine = sink.lines.iter().any(|l| {
            let plain = ansi::plain(l);
            plain.contains("╭─ Bash") && plain.contains('│')
        });
        assert!(
            has_spine,
            "non-last terminal step must retain vertical spine to connect steps"
        );
    }

    #[test]
    fn photo2_scenario_last_step_terminal_has_no_hanging_spine() {
        let mut app = test_app();
        app.blocks.push(Block::user("coba lakukan ls ~/Documents"));
        app.blocks.push(Block::Text {
            text: "Saya akan menjalankan perintah ls ~/Documents untukmu!".into(),
        });
        let mut terminal = Tool::new(
            "c1".into(),
            "terminal_use".into(),
            "List contents of ~/Documents directory".into(),
        );
        terminal.settle(&json!({ "command": "ls ~/Documents" }));
        terminal.output = "1.jpeg   Docs.zip\nDesktop.zip".into();
        terminal.state = ToolState::Ok;
        app.blocks.push(Block::of(terminal));
        app.blocks.push(Block::Text {
            text: "Ini dia isi direktori ~/Documents kamu! 🎉".into(),
        });

        let sink = transcript(&mut app, 100);
        let text = ansi::plain(&sink.lines.join("\n"));
        assert!(text.contains("╭─ Bash"));
        assert!(text.contains("1.jpeg"));
        assert!(text.contains("Ini dia isi direktori ~/Documents kamu!"));

        // Find the line with ╰─ (bottom of Bash box)
        let bottom_box_idx = sink
            .lines
            .iter()
            .rposition(|l| ansi::plain(l).contains('╰'))
            .expect("must contain bottom of box");

        // The line directly after the bottom box must NOT be a dangling spine `│`
        let next_line = &sink.lines[bottom_box_idx + 1];
        let plain_next = ansi::plain(next_line);
        assert_ne!(
            plain_next.trim(),
            "│",
            "line after terminal box should not be a dangling spine: {plain_next}"
        );

        // All box lines must be indented by 5 spaces without left spine │
        for (i, line) in sink.lines.iter().enumerate() {
            let plain = ansi::plain(line);
            if plain.contains("╭─ Bash") || plain.contains("1.jpeg") || (i == bottom_box_idx) {
                assert!(
                    plain.starts_with("     "),
                    "line {i} must be indented with 5 spaces: {plain}"
                );
                assert_ne!(
                    plain.chars().nth(2),
                    Some('│'),
                    "line {i} must not have left spine │: {plain}"
                );
            }
        }
    }

    #[test]
    fn borderless_preview_keeps_four_rows_with_omission_footer_below() {
        let content = (1..=20)
            .map(|i| format!("file_{i}.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut sink_closed = Sink::default();
        draw_borderless_preview(&content, false, "     ", 80, &mut sink_closed, 0);
        let text_closed = sink_closed.lines.join("\n");
        assert!(text_closed.contains("file_4.rs"));
        assert!(!text_closed.contains("file_5.rs"));
        assert!(text_closed.contains("… 16 lines cut. Click to open all"));
        // Confirm no container borders (╭─, │, ╰─)
        assert!(!text_closed.contains('╭'));
        assert!(!text_closed.contains('╰'));

        let mut sink_open = Sink::default();
        draw_borderless_preview(&content, true, "     ", 80, &mut sink_open, 0);
        let text_open = sink_open.lines.join("\n");
        assert!(text_open.contains("file_1.rs"));
        assert!(text_open.contains("file_20.rs"));
        assert!(!text_open.contains("lines cut"));
    }

    #[test]
    fn terminal_box_and_preview_boxes_fit_narrow_terminals_without_truncation() {
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run".into());
        tool.settle(&json!({ "command": "echo hello" }));
        tool.output = "output line 123456891213\n".into();
        tool.state = ToolState::Ok;

        for narrow_inner in [20, 24, 28, 32] {
            let mut sink = Sink::default();
            draw_terminal(&tool, false, true, narrow_inner, &mut sink, 0);
            for line in &sink.lines {
                let plain = ansi::plain(line);
                let w = ansi::width(line);
                assert!(
                    w <= narrow_inner,
                    "line overflowed narrow inner {narrow_inner}: {w} cols: {plain}"
                );
            }
            let bottom = sink.lines.last().expect("bottom line exists");
            let plain_bottom = ansi::plain(bottom);
            assert!(
                plain_bottom.trim().ends_with('╯'),
                "bottom border was truncated on narrow terminal: {plain_bottom}"
            );

            // Open box on narrow terminal also fits
            let mut sink_open = Sink::default();
            draw_terminal(&tool, true, true, narrow_inner, &mut sink_open, 0);
            for line in &sink_open.lines {
                let w = ansi::width(line);
                assert!(
                    w <= narrow_inner,
                    "open line overflowed narrow inner {narrow_inner}: {w} cols: {line}"
                );
            }
        }
    }

    #[test]
    fn open_boxes_never_shrink_below_closed_width_even_with_short_content() {
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run".into());
        tool.settle(&json!({ "command": "ls" }));
        tool.output = "a\n".into();
        tool.state = ToolState::Ok;

        let inner = 100;
        let mut sink_closed = Sink::default();
        draw_terminal(&tool, false, true, inner, &mut sink_closed, 0);
        let closed_top = sink_closed.lines.first().expect("closed top");
        let closed_w = ansi::width(closed_top);

        let mut sink_open = Sink::default();
        draw_terminal(&tool, true, true, inner, &mut sink_open, 0);
        let open_top = sink_open.lines.first().expect("open top");
        let open_w = ansi::width(open_top);

        assert_eq!(closed_w, 76 + 5); // 76 box_width + 5 spine
        assert!(
            open_w >= closed_w,
            "open box must never shrink below closed width: open {open_w} vs closed {closed_w}"
        );
        assert_eq!(
            open_w, closed_w,
            "with short content, open box width must equal closed width"
        );
    }

    #[test]
    fn open_boxes_expand_with_long_content_up_to_the_wall() {
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run".into());
        tool.settle(&json!({ "command": "ps" }));
        let long_line = "x".repeat(120);
        tool.output = format!("{long_line}\n");
        tool.state = ToolState::Ok;

        let inner = 140;
        let mut sink_closed = Sink::default();
        draw_terminal(&tool, false, true, inner, &mut sink_closed, 0);
        let closed_top = sink_closed.lines.first().expect("closed top");
        assert_eq!(ansi::width(closed_top), 76 + 5);

        let mut sink_open = Sink::default();
        draw_terminal(&tool, true, true, inner, &mut sink_open, 0);
        let open_top = sink_open.lines.first().expect("open top");
        // 120 content chars + 4 borders + 5 spine = 129 columns (flexing with content)
        assert_eq!(
            ansi::width(open_top),
            120 + 4 + 5,
            "open box must flex with content width"
        );

        // When content is wider than max_avail (140 - 6 = 134), box clamps at the wall
        let wall_line = "x".repeat(150);
        tool.output = format!("{wall_line}\n");
        let mut sink_wall = Sink::default();
        draw_terminal(&tool, true, true, inner, &mut sink_wall, 0);
        let wall_top = sink_wall.lines.first().expect("wall top");
        let max_avail = inner.saturating_sub(6);
        assert_eq!(
            ansi::width(wall_top),
            max_avail + 5,
            "open box must reach the wall when content exceeds pane width"
        );

        // Verify terminal_size provides wide PTY columns up to the wall, not clamped to 72.
        let mut app = test_app();
        app.sidebar = false;
        let (_, cols_full) = terminal_size(&app, 40, 160);
        assert!(
            cols_full >= 140,
            "PTY without sidebar must use wide pane: {cols_full}"
        );
        app.sidebar = true;
        let (_, cols_split) = terminal_size(&app, 40, 160);
        assert!(
            cols_split >= 90,
            "PTY with sidebar must use split pane width: {cols_split}"
        );
    }

    #[test]
    fn inspect_file_renders_read_path_and_range_with_borderless_preview() {
        let mut app = test_app();
        app.busy = true;
        let mut t = Tool::new("c1".into(), "inspect_file".into(), "read utils".into());
        t.settle(&json!({ "path": "utils.py", "view_range": [1, 50] }));
        t.content = (1..=10)
            .map(|i| format!("     {i}| line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        t.state = ToolState::Ok;
        app.blocks.push(Block::of(t));

        let text = screen(&mut app, 30, 100);
        assert!(text.contains("Read  utils.py  1 - 50"));
        assert!(text.contains("1| line 1"));
        assert!(text.contains("4| line 4"));
        assert!(!text.contains("5| line 5"));
        assert!(text.contains("… 6 lines cut. Click to open all"));
        let trans = transcript(&mut app, 100).lines.join("\n");
        assert!(!trans.contains('╭'));
        assert!(!trans.contains('╰'));
    }

    #[test]
    fn task_preview_keeps_four_items_and_expands_only_when_open() {
        let app = test_app();
        let mut t = Tool::new("t1".into(), "track_tasks".into(), "plan".into());
        let tasks_json: Vec<serde_json::Value> = (1..=12)
            .map(|i| json!({ "text": format!("task {i}"), "status": "todo" }))
            .collect();
        t.settle(&json!({ "tasks": tasks_json }));
        t.state = ToolState::Ok;

        // Closed
        let mut sink_closed = Sink::default();
        draw_tasks(&app, &t, false, false, 80, &mut sink_closed, 0);
        let text_closed = sink_closed.lines.join("\n");
        assert!(text_closed.contains("task 4"));
        assert!(!text_closed.contains("task 5"));
        assert!(text_closed.contains("… 8 tasks cut. Click to open all"));

        // Open
        let mut sink_open = Sink::default();
        draw_tasks(&app, &t, true, false, 80, &mut sink_open, 0);
        let text_open = sink_open.lines.join("\n");
        assert!(text_open.contains("task 1"));
        assert!(text_open.contains("task 12"));
        assert!(!text_open.contains("tasks cut"));
    }

    #[test]
    fn step_spacing_and_tool_bullets_without_chevrons() {
        let mut app = test_app();
        app.busy = true;
        let mut t1 = Tool::new("c1".into(), "inspect_file".into(), "reading".into());
        t1.settle(&json!({ "path": "main.rs" }));
        t1.state = ToolState::Ok;
        t1.open = false;

        let mut t2 = Tool::new("c2".into(), "inspect_file".into(), "reading".into());
        t2.settle(&json!({ "path": "lib.rs" }));
        t2.state = ToolState::Ok;
        t2.open = true;

        app.blocks.push(Block::of(t1));
        app.blocks.push(Block::of(t2));

        let text = screen(&mut app, 30, 100);
        assert!(text.contains("Reading  main.rs"), "{text}");
        assert!(text.contains("Reading  lib.rs"), "{text}");
        assert!(!text.contains("› Reading"), "no chevron arrow: {text}");
        assert!(!text.contains("⌄ Reading"), "no downward arrow: {text}");
    }

    #[test]
    fn sidebar_displays_user_specified_order_and_null_context_when_unconfigured() {
        let mut app = test_app();
        app.settings["price"] = json!({ "input": 1.0 });
        app.context = 0;
        app.context_used = 1250;
        app.tok_in = 1250;
        app.tok_out = 320;
        app.cache_in = 0;
        app.cost = 0.0042;
        app.title = Some("Rune UI & Architecture".into());
        app.steps = 18;
        app.turns = 3;

        let text = sidebar(&mut app, 45, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("SESSIONS"), "{text}");
        assert!(
            text.contains("Title") && text.contains("Rune UI & Architecture"),
            "{text}"
        );
        assert!(text.contains("Started"), "{text}");
        assert!(text.contains("Activity"), "{text}");
        assert!(text.contains("18 steps · 3 turns"), "{text}");
        assert!(
            text.contains("Context Window") && text.contains("1,250 / unknown"),
            "{text}"
        );
        assert!(text.contains("TokIn") && text.contains("1,250"), "{text}");
        assert!(text.contains("TokOut") && text.contains("320"), "{text}");
        assert!(text.contains("CacheIn") && text.contains("0"), "{text}");
        assert!(
            !text.contains("0%"),
            "CacheIn must not display percentage: {text}"
        );
        assert!(text.contains("Spend") && text.contains("$0.0042"), "{text}");
    }

    #[test]
    fn sidebar_displays_dashes_for_unstarted_session() {
        let mut app = test_app();
        let text = sidebar(&mut app, 45, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Title") && text.contains("-"), "{text}");
        assert!(text.contains("Started") && text.contains("-"), "{text}");
        assert!(text.contains("Activity") && text.contains("-"), "{text}");
        assert!(
            !text.contains("steps"),
            "unstarted session should not show steps: {text}"
        );
        assert!(
            !text.contains("turns"),
            "unstarted session should not show turns: {text}"
        );
    }

    #[test]
    fn sidebar_displays_accumulated_activity_seconds() {
        let mut app = test_app();
        app.turns = 1;
        app.steps = 13;
        app.accumulated_seconds = 49;
        let text = sidebar(&mut app, 45, 1000)
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Activity") && text.contains("49s · 13 steps · 1 turn"),
            "{text}"
        );
    }

    #[test]
    fn draw_capabilities_preview_renders_structured_items() {
        let mut sink = Sink::default();
        let content = serde_json::json!({
            "page": 1,
            "pages": 1,
            "entries": [
                {"id": "t1", "name": "find_files", "kind": "tool", "source": "rune-builtin", "description": "Find files by pattern. More info here."},
                {"id": "s1", "name": "backend-dev", "kind": "skill", "source": "project", "description": "Node.js backend guidelines."},
                {"id": "r1", "name": "db_schema", "kind": "resource", "source": "mcp", "description": "Database schema."}
            ],
            "total": 3
        }).to_string();

        draw_capabilities_preview(&content, false, "  │   ", 80, &mut sink, 0);
        let plain_lines: Vec<String> = sink.lines.iter().map(|l| ansi::plain(l)).collect();
        assert_eq!(plain_lines.len(), 3);
        assert!(
            plain_lines[0].contains("find_files [builtin] — Find files by pattern"),
            "{:?}",
            plain_lines[0]
        );
        assert!(
            plain_lines[1].contains("backend-dev [project] — Node.js backend guidelines"),
            "{:?}",
            plain_lines[1]
        );
        assert!(
            plain_lines[2].contains("db_schema [mcp] — Database schema"),
            "{:?}",
            plain_lines[2]
        );
    }

    #[test]
    fn mcp_text_or_raw_unwraps_content_blocks() {
        let raw_mcp = r#"{"content":[{"type":"text","text":"hello world"}]}"#;
        assert_eq!(mcp_text_or_raw(raw_mcp), "hello world");

        let plain = "just plain text";
        assert_eq!(mcp_text_or_raw(plain), "just plain text");
    }

    #[test]
    fn truncate_path_preserves_short_paths() {
        assert_eq!(truncate_path("src/ui/draw.rs", 50), "src/ui/draw.rs");
        assert_eq!(truncate_path("main.rs", 10), "main.rs");
    }

    #[test]
    fn truncate_path_middle_truncates_directory_keeping_root_parent_and_filename() {
        let long_path = "docs/Current/Studio/Complete/2026-09/12-RuneTerminalBoxBorderGlitchAndViewportTruncationFix.md";
        // With 75 columns, filename (54 chars) fits completely, directory is shortened in the middle
        let truncated = truncate_path(long_path, 75);
        assert!(
            truncated.contains("12-RuneTerminalBoxBorderGlitchAndViewportTruncationFix.md"),
            "filename must be preserved: {truncated}"
        );
        assert!(
            truncated.starts_with("docs/"),
            "root dir preserved: {truncated}"
        );
        assert!(
            truncated.contains("/…/"),
            "middle segments replaced with ellipsis: {truncated}"
        );
        assert!(
            truncated.contains("/2026-09/"),
            "immediate parent preserved: {truncated}"
        );
        assert!(
            ansi::width(&truncated) <= 75,
            "width ({}) exceeds 75: {truncated}",
            ansi::width(&truncated)
        );
    }

    #[test]
    fn truncate_path_allocates_bounded_directory_quota_when_filename_is_very_long() {
        let long_path = "docs/Current/Studio/Complete/2026-09/12-RuneTerminalBoxBorderGlitchAndViewportTruncationFix.md";
        // With 45 columns, neither can fit in full. Dir must retain bounded quota (not just "…")
        let truncated = truncate_path(long_path, 45);
        assert!(
            ansi::width(&truncated) <= 45,
            "width ({}) exceeds 45: {truncated}",
            ansi::width(&truncated)
        );
        assert!(
            truncated.starts_with("docs/"),
            "root dir must always be preserved: {truncated}"
        );
        assert!(
            truncated.ends_with(".md"),
            "extension preserved: {truncated}"
        );
    }

    #[test]
    fn truncate_path_never_drops_leading_project_folder_when_tight() {
        let path = "Application/Studio/Cli/src/ui/draw.rs";
        let truncated = truncate_path(path, 25);
        assert!(
            truncated.starts_with("Application/"),
            "leading project directory must be preserved: {truncated}"
        );
        assert!(truncated.ends_with("draw.rs"));
        assert!(ansi::width(&truncated) <= 25);
    }

    #[test]
    fn format_bullet_row_immune_to_line_counter_truncation() {
        let bullet = steps::Bullet::new(
            "Write",
            "docs/Current/Studio/Complete/2026-09/12-RuneTerminalBoxBorderGlitchAndViewportTruncationFix.md",
        )
        .with_note("+180")
        .linked();
        // Terminal width constraint: 80 cols
        let row = format_bullet_row(&bullet, "     ", true, 80);
        let plain = ansi::plain(&row);
        assert!(
            plain.ends_with("+180"),
            "line counter must be fully preserved at end of row: {plain}"
        );
        assert!(
            !plain.contains("+…"),
            "line counter must never be cut off into +…: {plain}"
        );
        assert!(
            ansi::width(&plain) <= 80,
            "row width ({}) exceeds 80: {plain}",
            ansi::width(&plain)
        );
    }

    #[test]
    fn wiki_links_and_multilingual_text_preserve_split_divider() {
        let wiki = "- [भोजपुरी](https://bh.wikipedia.org/wiki/%E0%A4%86%E0%A4%B0%E0%A5%8D%E0%A4%9F%E0%A4%BF%E0%A4%AB%E0%A4%BF%E0%A4%B6%E0%A4%BF%E0%A4%AF%E0%A4%B2_%E0%A4%87%E0%A4%82%E0%A4%9F%E0%A5%87%E0%A4%B2%E0%A4%BF%E0%A4%9C%E0%A5%87%E0%A4%82%E0%A4%B8)\n\
                    - [ပအိုဝ်ႏဘာႏသာႏ](https://blk.wikipedia.org/wiki/%E1%80%89%E1%80%AC%E1%80%8F%E1%80%BA%E1%82%8F%E1%80%90%E1%80%AF%EA%A9%BB)\n\
                    - [বাংলা](https://bn.wikipedia.org/wiki/%E0%A6%95%E0%A7%83%E0%A6%A4%E0%A7%8D%E0%A6%B0%E0%A6%BF%E0%A6%AE_%E0%A6%AC%E0%A7%81%E0%A6%A6%E0%A7%8D%E0%A6%A7%E0%A6%BF%E0%A6%AE%E0%A6%A4%E0%A7%8D%E0%A6%A4%E0%A6%BE)\n\
                    - [བོད་ཡིག](https://bo.wikipedia.org/wiki/%E0%BD%98%E0%BD%B2%E0%BC%8B%E0%BD%96%E0%BD%9F%E0%BD%BC%E0%BD%A6%E0%BC%8B%E0%BD%A2%E0%BD%B2%E0%BD%82%E0%BC%8B%E0%BD%93%E0%BD%B4%E0%BD%A6%E0%BC%8D)\n\
                    - [Brezhoneg](https://br.wikipedia.org/wiki/Naouegezh_artifisiel)\n\
                    - [Bosanski](https://bs.wikipedia.org/wiki/Umjetna_inteligencija)\n\
                    - [Буряад](https://bxr.wikipedia.org/wiki/%D0%A5%D1%8D%D0%BC%D1%8D%D0%BB_%D0%BE%D1%8E%D1%83%D0%BD)\n\
                    - [Català](https://ca.wikipedia.org/wiki/Intel%C2%B7lig%C3%A8ncia_artificial)\n\
                    - [Нохчийн](https://ce.wikipedia.org/wiki/%D0%9A%D1%85%D0%BE%D1%8C%D0%BB%D0%BB%D0%B8%D0%BD%D0%B0_%D0%B8%D0%BD%D1%82%D0%B5%D0%BB%D0%BB%D0%B5%D0%BA%D1%82)\n\
                    - [Cebuano](https://ceb.wikipedia.org/wiki/Intelihensiya_artipisyal)\n\
                    - [کوردی](https://ckb.wikipedia.org/wiki/%DA%98%D8%8C%D8%B1%D8%B1%DB%8C_%D8%AF%D8%B3%D8%AA%DA%A9%D8%B1%D8%AF)\n\
                    - [Qırımtatarca](https://crh.wikipedia.org/wiki/Suniy_zek%C3%A2)\n\
                    - [Čeština](https://cs.wikipedia.org/wiki/Um%C4%9Bl%C3%A1_inteligence)\n\
                    - [Kaszëbsczi](https://csb.wikipedia.org/wiki/Szt%C3%ABczn%C3%B4_inteligencj%C3%B4)";
        let make = |label: &str| -> (String, App) {
            let mut a = test_app();
            a.sidebar = true;
            match label {
                "Text" => a.blocks.push(Block::Text { text: wiki.into() }),
                "ToolBorderless" => {
                    let mut t = Tool::new(
                        "t1".into(),
                        "web_search".into(),
                        "list every language".into(),
                    );
                    t.output = wiki.into();
                    t.state = ToolState::Ok;
                    t.open = true;
                    a.blocks.push(Block::of(t));
                }
                "ToolTerminal" => {
                    let mut t =
                        Tool::new("t2".into(), "terminal_use".into(), "curl the page".into());
                    t.output = wiki.into();
                    t.state = ToolState::Ok;
                    t.open = true;
                    a.blocks.push(Block::of(t));
                }
                "Notice" => a.blocks.push(Block::Notice {
                    text: wiki.into(),
                    bad: false,
                }),
                _ => {}
            }
            (label.to_string(), a)
        };
        for label in ["Text", "ToolBorderless", "ToolTerminal", "Notice"] {
            for cols in [90usize, 120, 140, 180] {
                let (label, mut app) = make(label);
                let frame = compose(&mut app, 30, cols as u16);
                let divider = (cols * 66 / 100).max(40);
                let content = frame.len() - footer(&mut app, 30, cols).0.len();
                let mut bad = Vec::new();
                for (n, line) in frame[..content].iter().enumerate() {
                    if at_column(line, divider) != Some('│') {
                        bad.push(format!("row {n}: {:?}", ansi::plain(line)));
                    }
                }
                assert!(
                    bad.is_empty(),
                    "Divider displaced in [{label} cols={cols}]:\n{}",
                    bad.join("\n")
                );
            }
        }
    }

    #[test]
    fn format_bullet_row_preserves_edit_and_inspect_line_counters() {
        let edit_bullet = steps::Bullet::new("Edit", "Application/Studio/Cli/src/ui/draw.rs")
            .with_note("−45 +120")
            .linked();
        let row_edit = format_bullet_row(&edit_bullet, "  │   ", true, 60);
        let plain_edit = ansi::plain(&row_edit);
        assert!(
            plain_edit.ends_with("−45 +120"),
            "edit line counters must be intact: {plain_edit}"
        );
        assert!(ansi::width(&plain_edit) <= 60);

        let read_bullet = steps::Bullet::new("Read", "Application/Studio/Cli/src/ui/draw.rs")
            .with_note("100 - 250")
            .linked();
        let row_read = format_bullet_row(&read_bullet, "     ", true, 55);
        let plain_read = ansi::plain(&row_read);
        assert!(
            plain_read.ends_with("100 - 250"),
            "read line range must be intact: {plain_read}"
        );
        assert!(ansi::width(&plain_read) <= 55);
    }

    #[test]
    fn strip_leading_command_lines_removes_multiline_and_single_line_prefixes() {
        let cmds = vec!["echo hello \\\n  world".to_string(), "ls -la".to_string()];

        let mut lines = vec!["$ echo hello \\", "world", "real output 1", "real output 2"];
        strip_leading_command_lines(&mut lines, &cmds);
        assert_eq!(lines, vec!["real output 1", "real output 2"]);

        let mut lines_single = vec!["$ ls -la", "total 0", "-rw-r--r-- 1 test test 0"];
        strip_leading_command_lines(&mut lines_single, &cmds);
        assert_eq!(lines_single, vec!["total 0", "-rw-r--r-- 1 test test 0"]);

        let mut lines_clean = vec!["just output", "no command"];
        strip_leading_command_lines(&mut lines_clean, &cmds);
        assert_eq!(lines_clean, vec!["just output", "no command"]);
    }

    fn plain_of(sink: &Sink) -> String {
        ansi::plain(&sink.lines.join("\n"))
    }

    /// A command stopped at the timeout (one that could not be moved to the
    /// background) shows Rune's sentence as it was sent — tags and all, the
    /// first row of the output part — and what it printed; the command once.
    #[test]
    fn a_command_stopped_at_the_timeout_shows_the_notice_as_sent_and_what_it_printed() {
        let mut tool = Tool::new("t1".into(), "terminal_use".into(), "run".into());
        let cmd = "python3 server.py";
        tool.settle(&json!({ "command": cmd }));
        tool.content = runeengine::tools::notice::prepend(
            &[runeengine::tools::notice::command_timed_out(3, "disk full")],
            "Starting server on port 8080",
        );
        tool.state = ToolState::Failed;

        let mut sink = Sink::default();
        draw_terminal(&tool, true, true, 90, &mut sink, 0);
        let output = plain_of(&sink);

        assert_eq!(output.matches("python3 server.py").count(), 1, "{output}");
        assert!(
            output.contains("<tool_notice>The command timed out after 3s"),
            "{output}"
        );
        assert!(output.contains("(disk full).</tool_notice>"), "{output}");
        let notice_at = output.find("<tool_notice>").unwrap();
        let printed_at = output.find("Starting server on port 8080").unwrap();
        assert!(
            notice_at < printed_at,
            "Rune's word comes first, as the model read it:\n{output}"
        );
        assert_eq!(
            output.matches("Starting server on port 8080").count(),
            1,
            "{output}"
        );
    }

    /// What the box draws is what the model was sent: a notice appears exactly
    /// when the payload has one, with its tags, and the payload is never touched.
    #[test]
    fn draw_terminal_draws_what_the_model_was_sent_and_never_alters_it() {
        let mut tool = Tool::new("t2".into(), "terminal_use".into(), "test".into());
        tool.settle(&json!({ "command": "cargo test" }));
        tool.state = ToolState::Ok;
        for (body, notice, printed) in [
            // Succeeded and printed its result: no notice, nothing added.
            ("real output", None, Some("real output")),
            // Failed: the exit code is a notice, and it is drawn.
            (
                "<tool_notice>Exit code 2</tool_notice>\n\nreal output",
                Some("<tool_notice>Exit code 2</tool_notice>"),
                Some("real output"),
            ),
            // Printed nothing: the exit code is all there is.
            (
                "<tool_notice>Exit code 0</tool_notice>",
                Some("<tool_notice>Exit code 0</tool_notice>"),
                None,
            ),
            // Cut: Rune's sentence, then the two ends of the output.
            (
                "<tool_notice>The output is 45000 characters, over the 30000 limit; the middle is left out (1.5 KB shown from the start, 1.5 KB from the end). The full output is saved at /p/log.txt.</tool_notice>\n\n<preview_head size=\"1.5 KB\">\nrunning 100 tests\n</preview_head>\n\n<preview_tail size=\"1.5 KB\">\ntest result: ok\n</preview_tail>",
                Some("The full output is saved at /p/log.txt.</tool_notice>"),
                Some("test result: ok"),
            ),
        ] {
            tool.content = body.into();
            let mut sink = Sink::default();
            draw_terminal(&tool, true, true, 100, &mut sink, 0);
            let rendered = plain_of(&sink);
            match notice {
                Some(text) => assert!(rendered.contains(text), "{body}\n{rendered}"),
                None => assert!(!rendered.contains("tool_notice"), "{body}\n{rendered}"),
            }
            if let Some(text) = printed {
                assert!(rendered.contains(text), "{body}\n{rendered}");
            }
            assert_eq!(tool.content, body, "UI must never mutate the AI payload");
        }
    }

    /// A background launch is the one line the model got — tags included — in
    /// the box under the command, wrapped, not turned into a record.
    #[test]
    fn a_background_launch_is_drawn_as_the_one_line_the_model_was_sent() {
        let mut tool = Tool::new("t3".into(), "terminal_use".into(), "build".into());
        tool.settle(&json!({ "command": "cargo build --release", "run_in_background": true }));
        tool.content = runeengine::tools::notice::background_started(
            "bg_pEdiPAS1",
            1139359,
            std::path::Path::new(
                "/home/noire/.rune/projects/p/sessions/537/tools_result/terminal_use/bg_pEdiPAS1.txt",
            ),
            runeengine::platform::InputWaitDetection::Every,
        );
        tool.state = ToolState::Backgrounded;
        let mut sink = Sink::default();
        draw_terminal(&tool, true, true, 100, &mut sink, 0);
        let rendered = plain_of(&sink);
        // Wrapped inside the box: take the box's text back out and compare.
        let flat: String = rendered
            .lines()
            .filter_map(|l| l.trim().strip_prefix('│').and_then(|l| l.strip_suffix('│')))
            .collect::<Vec<_>>()
            .join(" ");
        let squeezed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squeezed
                .contains("<tool_notice>Running in the background as bg_pEdiPAS1 (PID 1139359);"),
            "{rendered}"
        );
        assert!(
            squeezed.contains("when it ends.</tool_notice>"),
            "{rendered}"
        );
        assert!(!squeezed.contains("Process id"), "{rendered}");
    }

    /// A command found waiting for input moved to the background: its box
    /// shows the sentence the model got and what the command printed before it
    /// moved, in that order — the same box a finished command has. (One that
    /// moved at the timeout carries the sentence alone.)
    #[test]
    fn a_command_moved_to_the_background_shows_the_notice_and_what_it_printed() {
        let mut tool = Tool::new("t4".into(), "terminal_use".into(), "build".into());
        tool.settle(&json!({ "command": "./gradlew assembleDebug" }));
        tool.content = runeengine::tools::notice::prepend(
            &[runeengine::tools::notice::waiting_for_input(
                "bg_x1",
                7,
                std::path::Path::new("/tmp/bg_x1.txt"),
                false,
            )],
            "> Task :App:preBuild",
        );
        tool.state = ToolState::Backgrounded;
        let mut sink = Sink::default();
        draw_terminal(&tool, true, true, 100, &mut sink, 0);
        let rendered = plain_of(&sink);
        let notice_at = rendered
            .find("<tool_notice>Waiting for input in the background as bg_x1")
            .expect(&rendered);
        let printed_at = rendered.find("> Task :App:preBuild").expect(&rendered);
        assert!(notice_at < printed_at, "{rendered}");
        assert_eq!(
            rendered.matches("./gradlew assembleDebug").count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn a_report_has_its_colons_in_one_column_and_a_path_breaks_after_a_slash() {
        let rows = notice_rows(
            &["Process: Completed\nExit code: 0\nPID: 1139359\nOutput log: /home/noire/.rune/projects/-home-noire-Project-Clode/sessions/537/tools_result/terminal_use/bg_pEdiPAS1.txt"],
            60,
        );
        assert_eq!(rows[0], "<tool_notice>");
        assert_eq!(rows[1], "Process:    Completed");
        assert_eq!(rows[2], "Exit code:  0");
        assert_eq!(rows[3], "PID:        1139359");
        assert!(rows[4].starts_with("Output log: /home/noire/"), "{rows:?}");
        assert!(rows.last().unwrap() == "</tool_notice>");
        let path_rows = &rows[4..rows.len() - 1];
        assert!(path_rows.len() >= 2, "a long path wraps: {rows:?}");
        for row in &path_rows[..path_rows.len() - 1] {
            assert!(
                row.ends_with('/'),
                "cut after a slash, not inside a word: {row:?}"
            );
        }
        assert!(
            path_rows.last().unwrap().ends_with("bg_pEdiPAS1.txt"),
            "{rows:?}"
        );
        for row in &rows {
            assert!(ansi::width(row) <= 60, "{row:?}");
        }
        // A sentence is one row, tags and all.
        assert_eq!(
            notice_rows(&["Exit code 101"], 60),
            vec!["<tool_notice>Exit code 101</tool_notice>".to_string()]
        );
    }

    #[test]
    fn additional_information_is_one_report_field_with_aligned_continuations() {
        let body = "Process: Completed\nExit code: 0\nPID: 42\nOutput log: /project/output/background.txt\nAdditional information: The output is 40000 characters, over the 30000 limit; the middle is left out. The full output is saved at /project/output/full.txt.";
        for width in [40, 60, 100] {
            let rows = notice_rows(&[body], width);
            assert_eq!(rows.iter().filter(|row| *row == "<tool_notice>").count(), 1);
            assert_eq!(
                rows.iter().filter(|row| *row == "</tool_notice>").count(),
                1
            );
            assert!(rows.iter().all(|row| ansi::width(row) <= width), "{rows:?}");
            let at = rows
                .iter()
                .position(|row| row.starts_with("Additional information:"))
                .unwrap();
            let column = "Additional information: ".len();
            for row in &rows[at + 1..rows.len() - 1] {
                assert!(row.starts_with(&" ".repeat(column)), "{row:?}");
            }
            let restored = rows[at..rows.len() - 1]
                .iter()
                .map(|row| row[column..].trim())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(restored.contains("over the 30000 limit"), "{restored}");
            assert!(restored.contains("full.txt."), "{restored}");
        }
    }

    /// A process report comes first, aligned and tagged; folded, the head of the
    /// output is shown and the unseen suffix is counted below it.
    #[test]
    fn process_report_precedes_a_head_preview_and_the_cut_suffix_is_below() {
        let app = test_app();
        let mut tool = Tool::new(
            "m1".into(),
            "manage_process".into(),
            "wait for the build".into(),
        );
        tool.state = ToolState::Ok;
        let output: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        tool.content = format!(
            "<tool_notice>\nProcess: Completed\nExit code: 0\nPID: 42\nOutput log: /tmp/bg_1.txt\n</tool_notice>\n\n{output}"
        );
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, true, 100, &mut sink);
        let text = plain_of(&sink);
        assert!(text.contains("Process:    Completed"), "{text}");
        assert!(text.contains("Exit code:  0"), "{text}");
        assert!(
            text.contains("<tool_notice>") && text.contains("</tool_notice>"),
            "{text}"
        );
        assert!(text.contains("line 1") && text.contains("line 4"), "{text}");
        assert!(
            !text.contains("line 5\n") && !text.contains("line 20\n"),
            "{text}"
        );
        assert!(text.contains("… 16 lines cut. Click to open all"), "{text}");
        let notice_at = text.find("Process:").unwrap();
        assert!(notice_at < text.find("line 1").unwrap(), "{text}");
        assert!(text.find("line 4").unwrap() < text.find("lines cut").unwrap());
        assert!(
            ansi::plain(sink.lines.last().unwrap()).contains("… 16 lines cut. Click to open all")
        );
        assert!(!text.contains("rows hidden") && !text.contains("wheel here"));
    }

    #[test]
    fn process_cut_counts_wrapped_suffix_lines_and_keeps_the_head_in_order() {
        let content = (0..12)
            .map(|i| format!("row-{i:02} {}\n", "wrapped text ".repeat(5)))
            .collect::<String>();
        let mut full = Sink::default();
        draw_borderless_preview(&content, true, "", 40, &mut full, 0);
        let mut folded = Sink::default();
        draw_borderless_preview(&content, false, "", 40, &mut folded, 0);
        let hidden = full.len() - super::super::viewport::MAIN_PREVIEW_ROWS;
        assert!(ansi::plain(folded.lines.last().unwrap())
            .contains(&format!("… {hidden} lines cut. Click to open all")));
        let visible: Vec<_> = folded.lines[..folded.len() - 1]
            .iter()
            .map(|line| ansi::plain(line))
            .collect();
        let expected: Vec<_> = full
            .lines
            .iter()
            .take(super::super::viewport::MAIN_PREVIEW_ROWS)
            .map(|line| ansi::plain(line))
            .collect();
        assert_eq!(visible, expected);
        assert!(!plain_of(&full).contains("lines cut"));
        assert!(!plain_of(&folded).contains("rows hidden"));
    }

    /// A search that stopped at its limit says so under its results, in one
    /// tagged line; what its header counted is in the step's bullet, and the
    /// header line itself is not drawn twice.
    #[test]
    fn a_search_says_where_it_stopped_under_its_results_and_its_counts_are_in_the_bullet() {
        let app = test_app();
        let mut tool = Tool::new("s1".into(), "search_text".into(), "look for o".into());
        tool.settle(&json!({ "pattern": "o", "path": "Cli/src/ui" }));
        tool.state = ToolState::Ok;
        tool.content = "<tool_notice>The search stopped at 100 matching lines.</tool_notice>\n=== \"o\" — 84 match(es) in 6 file(s) ===\nCli/src/ui/keys.rs:560:  Key::Ctrl('o') => {\n".into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, true, 100, &mut sink);
        let text = plain_of(&sink);
        assert!(text.contains("84 matches in 6 files"), "{text}");
        assert!(
            !text.contains("match(es)"),
            "the header line is not drawn twice:\n{text}"
        );
        let hit = text.find("Key::Ctrl('o')").unwrap();
        let notice = text
            .find("<tool_notice>The search stopped at 100 matching lines.</tool_notice>")
            .unwrap();
        assert!(hit < notice, "the notice sits under the results:\n{text}");

        let mut read = Tool::new("r1".into(), "inspect_file".into(), "read".into());
        read.settle(&json!({ "path": "Cli/src/ui/keys.rs", "view_range": [545, 580] }));
        read.state = ToolState::Ok;
        read.content = "=== Cli/src/ui/keys.rs (lines 545-580 of 4907) ===\n   545| }\n".into();
        let mut sink = Sink::default();
        draw_step(&app, 0, &read, true, 100, &mut sink);
        let text = plain_of(&sink);
        assert!(text.contains("545 - 580 · of 4907 lines"), "{text}");
    }

    /// Point 4 (owner, 2026-10-09): a search or a read cut at its limit keeps
    /// its notice INSIDE the fold, after the results: folded, the fold marker
    /// is the step's last row and counts the notice among what it hides;
    /// opened, the notice is the last row, under every result.
    #[test]
    fn a_limit_notice_is_inside_the_fold_and_under_every_result() {
        let app = test_app();
        for (name, args, header, notice) in [
            (
                "search_text",
                json!({ "pattern": "o" }),
                "=== \"o\" — 9 match(es) in 1 file(s) ===",
                "<tool_notice>The search stopped at 9 matching lines.</tool_notice>",
            ),
            (
                "find_files",
                json!({ "pattern": "**/*.rs" }),
                "=== \"**/*.rs\" — 9 match(es) ===",
                "<tool_notice>The search stopped at 9 matches.</tool_notice>",
            ),
            (
                "inspect_file",
                json!({ "path": "a.rs", "view_range": [1, 9] }),
                "=== a.rs (lines 1-9 of 90) ===",
                "<tool_notice>Line 9 is cut and 81 later lines are not shown (output limit 10 characters). Continue with view_range [9, 90].</tool_notice>",
            ),
        ] {
            let mut tool = Tool::new("s1".into(), name.into(), "look".into());
            tool.settle(&args);
            tool.state = ToolState::Ok;
            let body: String = (1..=9).map(|n| format!("     {n}| hit {n}\n")).collect();
            tool.content = format!("{notice}\n{header}\n{body}");

            let mut sink = Sink::default();
            draw_step(&app, 0, &tool, true, 160, &mut sink);
            let rows: Vec<String> = sink.lines.iter().map(|l| ansi::plain(l)).collect();
            let last = rows.last().unwrap();
            assert!(
                last.contains("… 6 lines cut. Click to open all"),
                "{name}: the fold is the last row and counts the notice:\n{}",
                rows.join("\n")
            );
            assert!(!rows.iter().any(|r| r.contains("<tool_notice>")), "{name}");

            tool.open = true;
            let mut sink = Sink::default();
            draw_step(&app, 0, &tool, true, 160, &mut sink);
            let rows: Vec<String> = sink.lines.iter().map(|l| ansi::plain(l)).collect();
            let text = rows.join("\n");
            assert!(rows.last().unwrap().contains("</tool_notice>"), "{name}:\n{text}");
            assert!(text.find("hit 9").unwrap() < text.find("<tool_notice>").unwrap());
            assert!(!text.contains("lines cut"), "{name}:\n{text}");
        }
    }

    #[test]
    fn fetched_page_preview_keeps_its_limit_notice_inside_the_fold() {
        let app = test_app();
        let mut tool = Tool::new("fetch-1".into(), "web_fetch".into(), "read page".into());
        tool.settle(&json!({"url":"https://example.com/docs"}));
        tool.state = ToolState::Ok;
        let body: String = (1..=9).map(|n| format!("page line {n}\n")).collect();
        tool.content = format!("<tool_notice>The result is 200 characters; only the first 100 are shown.</tool_notice>\n{body}");
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, true, 160, &mut sink);
        let rows: Vec<_> = sink.lines.iter().map(|l| ansi::plain(l)).collect();
        assert!(rows
            .last()
            .unwrap()
            .contains("lines cut. Click to open all"));
        assert!(!rows.iter().any(|r| r.contains("<tool_notice>")));
        assert!(
            rows.iter()
                .any(|r| r.contains("Fetch") && r.contains("https://example.com/docs")),
            "{}",
            rows.join("\n")
        );
        tool.open = true;
        let mut sink = Sink::default();
        draw_step(&app, 0, &tool, true, 160, &mut sink);
        let text = plain_of(&sink);
        assert!(
            text.find("page line 9").unwrap() < text.find("<tool_notice>").unwrap(),
            "{text}"
        );
        assert!(!text.contains("lines cut"));
    }

    fn notices(app: &mut App, n: usize) {
        for i in 0..n {
            app.blocks.push(Block::Notice {
                text: format!("history {i}"),
                bad: false,
            });
        }
    }

    fn first_with(frame: &[String], needle: &str) -> String {
        frame
            .iter()
            .map(|l| ansi::plain(l))
            .find_map(|l| {
                l.find(needle)
                    .map(|at| l[at..].split('│').next().unwrap_or("").trim().to_string())
            })
            .unwrap_or_default()
    }

    /// 2C, owner 2026-10-09: an overlay used to borrow the conversation's
    /// scroll. Opened while the reader was up in history, it put the chat back
    /// at the live end; scrolled in itself and closed, it left the chat where
    /// the overlay's own numbers pointed — near the very top of the session.
    /// The overlay keeps its own place; the chat is where its reader left it.
    #[test]
    fn an_overlay_never_moves_the_conversation_under_it() {
        for panel in [
            Panel::Help,
            Panel::Logs,
            Panel::Tasks,
            Panel::Capabilities,
            Panel::Context,
            Panel::Undo,
        ] {
            // Up in history: the chat is still there when the overlay closes.
            let mut app = test_app();
            notices(&mut app, 300);
            let _ = compose(&mut app, 24, 100);
            app.scroll_by(true, 120);
            let before = compose(&mut app, 24, 100);
            let reading = first_with(&before, "history");
            app.panel = panel;
            app.transcript_dirty = true;
            let _ = compose(&mut app, 24, 100);
            app.panel_view.scroll_by(true, 6);
            let _ = compose(&mut app, 24, 100);
            app.panel = Panel::None;
            app.transcript_dirty = true;
            let after = compose(&mut app, 24, 100);
            assert_eq!(
                first_with(&after, "history"),
                reading,
                "{panel:?} moved the chat"
            );
            assert!(app.scroll > 0, "{panel:?} sent the reader to the live end");

            // At the live end: scrolling inside the overlay does not fling
            // the chat anywhere.
            let mut app = test_app();
            notices(&mut app, 300);
            let _ = compose(&mut app, 24, 100);
            app.panel = panel;
            app.transcript_dirty = true;
            let _ = compose(&mut app, 24, 100);
            app.panel_view.scroll_by(true, 6);
            let _ = compose(&mut app, 24, 100);
            app.panel = Panel::None;
            app.transcript_dirty = true;
            let after = compose(&mut app, 24, 100);
            assert_eq!(app.scroll, 0, "{panel:?} scrolled the chat");
            assert_eq!(first_with(&after, "history 299"), "history 299");
        }
    }

    /// An overlay opened again starts at its own end, not where another one
    /// was left.
    #[test]
    fn each_overlay_opens_at_its_own_end() {
        let mut app = test_app();
        notices(&mut app, 50);
        app.panel = Panel::Help;
        let _ = compose(&mut app, 24, 100);
        app.panel_view.scroll_by(true, 6);
        let _ = compose(&mut app, 24, 100);
        assert!(app.panel_view.scroll > 0, "help is taller than the screen");
        app.panel = Panel::Logs;
        let _ = compose(&mut app, 24, 100);
        assert_eq!(app.panel_view.scroll, 0);
    }

    /// The owner's rule (2026-10-09): the chevron stays mid-screen while the
    /// list scrolls under it, the list stops once its last row is drawn, and
    /// the chosen row is on screen through split and resize.
    #[test]
    fn undo_navigation_keeps_its_chevron_centred_across_split_and_resize() {
        use crate::ui::input::Key;
        let mut app = test_app();
        let mut d = crate::ui::test_driver("undo-reveal");
        app.panel = Panel::Undo;
        app.file_changes = (1..=60)
            .map(|n| {
                serde_json::json!({
                    "path": format!("file-{n}/Engine/src/very-long-directory/another-directory/x.rs"),
                    "added": n, "removed": 1, "last_at": 1_700_000_000_000i64
                })
            })
            .collect();
        for i in 0..100 {
            let key = if i < 59 { Key::Down } else { Key::Up };
            app.on_key(key, &mut d);
            for (split, width) in [(false, 169), (true, 169), (true, 110), (false, 80)] {
                app.sidebar = split;
                let frame = compose(&mut app, 24, width);
                let file = format!("file-{}/", app.panel_row + 1);
                let row = frame.iter().position(|line| {
                    let line = ansi::plain(line);
                    line.trim_start().starts_with('›') && line.contains(&file)
                });
                let row = row.unwrap_or_else(|| {
                    panic!(
                        "selection is off-screen at {i}, split {split}, width {width}: {frame:?}"
                    )
                });
                let last_drawn = frame
                    .iter()
                    .any(|line| ansi::plain(line).contains("file-60/"));
                if (8..50).contains(&app.panel_row) && !last_drawn {
                    assert!(
                        (7..=14).contains(&row),
                        "the chevron is not mid-screen at {i} (row {row}), width {width}"
                    );
                }
                assert!(frame
                    .iter()
                    .all(|line| !line.contains('\n') && !line.contains('\r')));
            }
        }
    }

    fn paragraphs(app: &mut App, n: usize) {
        for i in 0..n {
            app.blocks.push(Block::Text {
                text: format!(
                    "paragraph {i}: lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                     eiusmod tempor incididunt ut labore et dolore magna aliqua"
                ),
            });
        }
    }

    fn paragraph_number(frame: &[String]) -> usize {
        first_with(frame, "paragraph ")
            .trim_start_matches("paragraph ")
            .split(':')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a paragraph on screen")
    }

    /// 2C, owner 2026-10-09: the side panel opening or the terminal resized
    /// wraps every paragraph again; the old anchor's text was gone and the
    /// fallback kept the row number — paragraph 130 became 65 (a jump up),
    /// 215 became 280. The reader stays on the paragraph they were reading.
    #[test]
    fn a_new_width_keeps_the_reader_on_the_same_paragraph() {
        for (sidebar_before, sidebar_after, cols_before, cols_after) in [
            (true, false, 140u16, 140u16),
            (false, true, 140, 140),
            (false, false, 140, 90),
            (false, false, 90, 140),
        ] {
            let mut app = test_app();
            paragraphs(&mut app, 300);
            app.sidebar = sidebar_before;
            let _ = compose(&mut app, 24, cols_before);
            app.scroll_by(true, 150);
            let before = paragraph_number(&compose(&mut app, 24, cols_before));
            app.sidebar = sidebar_after;
            app.transcript_dirty = true;
            let after = paragraph_number(&compose(&mut app, 24, cols_after));
            assert!(
                before.abs_diff(after) <= 1,
                "{sidebar_before}->{sidebar_after} {cols_before}->{cols_after}: paragraph {before} became {after}"
            );
            // And back again: no drift accumulates over a round trip.
            app.sidebar = sidebar_before;
            app.transcript_dirty = true;
            let back = paragraph_number(&compose(&mut app, 24, cols_before));
            assert!(
                before.abs_diff(back) <= 1,
                "round trip: {before} became {back}"
            );
        }
    }

    /// A group growing above the reader (a long step, a thought) does not move
    /// them: the anchor finds its own group again.
    #[test]
    fn content_growing_above_the_reader_does_not_move_them() {
        let mut app = test_app();
        paragraphs(&mut app, 120);
        let _ = compose(&mut app, 24, 120);
        app.scroll_by(true, 100);
        let before = paragraph_number(&compose(&mut app, 24, 120));
        if let Block::Text { text } = &mut app.blocks[3] {
            text.push_str(&"\n\nmore rows above the reader".repeat(40));
        }
        app.transcript_dirty = true;
        let after = paragraph_number(&compose(&mut app, 24, 120));
        assert_eq!(before, after);
    }

    /// 2A, owner 2026-10-09: scrolled away from the newest row, the chat's
    /// last row is the hint — centred on the chat column with the side panel
    /// open or closed, and nowhere at the live end or over an overlay.
    /// Point 5 (owner, 2026-10-09): the hint covers only the text behind it —
    /// the conversation's row under it still shows on both sides.
    #[test]
    fn the_jump_hint_covers_only_its_own_columns() {
        for sidebar in [false, true] {
            let mut app = test_app();
            app.sidebar = sidebar;
            for i in 0..100 {
                app.blocks.push(Block::Notice {
                    text: format!("{i:03} {}", "abcdefghij".repeat(12)),
                    bad: false,
                });
            }
            // Some rows under it are the blank between two notices; the
            // first one that holds text must show it on both sides.
            let mut seen = false;
            for _ in 0..6 {
                app.scroll_by(true, 1);
                let frame = compose(&mut app, 24, 140);
                let (row, first, last) = app.jump_hint.expect("the hint is up");
                let line = ansi::plain(&frame[row as usize - 1]);
                let cols: Vec<char> = line.chars().collect();
                assert_eq!(cols[first as usize - 2], ' ', "a blank before it: {line}");
                assert_eq!(cols[last as usize], ' ', "a blank after it: {line}");
                let before: String = cols[..first as usize - 2].iter().collect();
                let after: String = cols[last as usize + 1..].iter().take(10).collect();
                if before.contains("abcdefghij") {
                    // Beside the panel the notice wraps before the hint ends;
                    // full width, its row runs on past it.
                    assert!(
                        sidebar || after.chars().any(|c| c.is_ascii_lowercase()),
                        "{sidebar}: right of it: {line}"
                    );
                    seen = true;
                    break;
                }
            }
            assert!(seen, "{sidebar}: the row under the hint never showed");
        }
    }

    #[test]
    fn the_jump_hint_sits_centred_over_the_composer_while_scrolled_up() {
        for sidebar in [false, true] {
            let mut app = test_app();
            app.sidebar = sidebar;
            notices(&mut app, 100);
            let frame = compose(&mut app, 24, 140);
            assert!(!frame.iter().any(|l| ansi::plain(l).contains(JUMP_HINT)));
            assert!(app.jump_hint.is_none());

            app.scroll_by(true, 1);
            let frame = compose(&mut app, 24, 140);
            let (row, first, last) = app.jump_hint.expect("the hint is up one row away");
            let line = ansi::plain(&frame[row as usize - 1]);
            let at = line.find(JUMP_HINT).expect("the hint is on its row");
            let col = ansi::width(&line[..at]);
            assert_eq!(col as u16 + 1, first);
            assert_eq!(last - first + 1, ansi::width(JUMP_HINT) as u16);
            // Centred in the chat column: equal room on both sides, give or
            // take the odd column.
            let chat = if sidebar {
                app.sidebar_cols.unwrap().0 as usize - 3
            } else {
                140
            };
            let right = chat - col - ansi::width(JUMP_HINT);
            assert!(
                col.abs_diff(right) <= 1,
                "{sidebar}: {col} left, {right} right"
            );
            // Only its own columns (and a blank each side) are covered: the
            // row's text beside it stays.
            let left_part: String = line.chars().take(chat).collect();
            assert!(left_part.contains(&format!(" {JUMP_HINT} ")), "{left_part}");
            assert_eq!(
                row as usize, app.drawn_viewport,
                "the chat's last row, just above the composer"
            );

            app.panel = Panel::Help;
            let _ = compose(&mut app, 24, 140);
            assert!(app.jump_hint.is_none(), "not over an overlay");
        }
    }
}
