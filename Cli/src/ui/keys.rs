//! What every key does.
//!
//! # The keymap is shaped by what terminals can actually send
//!
//! Control letters, `Alt`+letter, `Shift-Tab`, arrows and function keys. That is
//! the whole alphabet (see [`super::input`]), so every shortcut here is one of
//! those — and `Ctrl-S` is deliberately unused, because it is XOFF everywhere
//! this program is not running.
//!
//! # Separate stopping from leaving
//!
//! `Esc` twice stops an active turn. `Ctrl-C` twice leaves the CLI, including
//! while a turn is streaming: its first press never cancels the model. The two
//! gestures are separate so a user never stops work by trying to leave.

use std::time::Instant;

use runeengine::Command;

#[cfg(test)]
use super::ansi;
use super::blocks::{Block, ToolState};
use super::input::Key;
use super::{panels, settings, App, Awaiting, Panel};
use crate::driver::Driver;

/// Every slash command: its verb, the argument it takes, and what it does.
///
/// One table, three readers — [`App::slash`] dispatches from it, `/help` lists
/// it, and the palette that opens on `/` searches it. A command that exists in
/// only one of those is either unfindable or a lie, and the way to make that
/// impossible is to have one list.
pub const COMMANDS: [(&str, &str, &str); 25] = [
    (
        "skills",
        "[list|show|enable|disable|reload]",
        "manage project, personal and Rune product skills",
    ),
    ("skill", "<id>", "activate a skill for this conversation"),
    (
        "mcp",
        "[list|add|connect|status]",
        "manage MCP connections in Rune",
    ),
    ("new", "", "start a fresh session in this workspace"),
    ("set", "key=value", "change one setting"),
    ("settings", "", "all workspace preferences (alias /*)"),
    (
        "model",
        "[<provider>:<id>|<id>]",
        "select active model — no argument opens list",
    ),
    (
        "provider",
        "[name]",
        "choose which provider answers — no name opens the list",
    ),
    (
        "thinking",
        "[off|<effort>]",
        "switch reasoning off or set effort level (Alt+T cycles)",
    ),
    (
        "classifier",
        "[on|off|model|reload]",
        "inspect or configure security classifier",
    ),
    ("theme", "[name]", "switch color theme: violet, cyan, amber"),
    ("panel", "", "toggle the session panel"),
    ("attach", "path", "add a file to the next message"),
    ("detach", "number|all", "remove an attachment explicitly"),
    ("verbose", "", "toggle every folded step and thought open"),
    (
        "mode",
        "[ask|auto|full]",
        "cycle or set permission mode: ask, auto, full",
    ),
    (
        "session",
        "",
        "browse and switch sessions in this workspace",
    ),
    ("resume", "<id>", "switch directly to a session by its id"),
    (
        "compact",
        "",
        "summarise older history to free context space",
    ),
    (
        "logs",
        "[all|warn|error]",
        "what happened behind the conversation: context sent, retries, costs, failures",
    ),
    (
        "context",
        "",
        "what the model holds besides the conversation: system prompt, tools, messages Rune wrote",
    ),
    (
        "undo",
        "",
        "put a file back the way it was before this session",
    ),
    ("redo", "", "put back what an undo of a file replaced"),
    ("help", "", "every key and every command"),
    ("quit", "", "leave"),
];

/// The commands that match what has been typed, best first.
///
/// Empty unless the line starts with `/`, so ordinary prose never opens a list.
/// Only the **first word** is searched: `/set temp` is still looking for `set`,
/// and the rest of the line is the argument being written for it.
///
/// Three degrees of match, in this order: the verb starts with what was typed,
/// the verb contains it, the description mentions it. That last one is what makes
/// the list answer a question rather than a spelling — typing `plan` finds
/// `/tasks`, which is not called plan.
pub fn matches(input: &str) -> Vec<&'static (&'static str, &'static str, &'static str)> {
    let needle_str = if let Some(rest) = input.strip_prefix('/') {
        rest
    } else if let Some(rest) = input.strip_prefix('?') {
        if rest.is_empty() {
            return COMMANDS.iter().filter(|c| c.0 == "help").collect();
        }
        rest
    } else {
        return Vec::new();
    };

    let (first_word, has_space) = match needle_str.split_once(char::is_whitespace) {
        Some((word, _)) => (word.to_ascii_lowercase(), true),
        None => (needle_str.trim().to_ascii_lowercase(), false),
    };

    // If a space was typed after the command name (e.g. "/mode "),
    // only match the exact command verb — do not match longer commands like "/model".
    if has_space && !first_word.is_empty() {
        let exact: Vec<_> = COMMANDS.iter().filter(|c| c.0 == first_word).collect();
        if !exact.is_empty() {
            return exact;
        }
    }

    let mut hits: Vec<(u8, &'static (&str, &str, &str))> = COMMANDS
        .iter()
        .filter_map(|c| rank(c, &first_word).map(|r| (r, c)))
        .collect();
    if first_word.is_empty() {
        hits.sort_by_key(|(rank, command)| (*rank, command.0));
    } else {
        hits.sort_by_key(|(rank, command)| (*rank, command.0.len(), command.0));
    }
    hits.into_iter().map(|(_, command)| command).collect()
}

fn rank(command: &(&str, &str, &str), needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(1);
    }
    if command.0 == needle {
        return Some(0); // Exact match ranks first
    }
    if command.0.starts_with(needle) {
        return Some(1); // Prefix match
    }
    if command.0.contains(needle) {
        return Some(2); // Substring in command name
    }
    command.2.to_ascii_lowercase().contains(needle).then_some(3)
}

/// Every line of `text` as a path to a file that exists, or `None`.
///
/// All or nothing on purpose. A paste that is half paths and half prose is
/// prose — guessing which half was meant would put a file in front of the model
/// that nobody chose, and there is no keystroke that takes that back once it has
/// been sent.
///
/// The unwrapping is what a terminal actually delivers on a drop: a `file://`
/// URI on some desktops, single quotes on others, and a backslash before every
/// space on most.
fn paths_in(text: &str) -> Option<Vec<std::path::PathBuf>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let windows = runeengine::platform::OS == runeengine::platform::Os::Windows;
    let mut out = Vec::new();
    for line in lines {
        // A generic terminal can hand us one path per line, a quoted path, or
        // a single line holding many paths. On Unix the Engine's one
        // shell-style lexer reads it; a Windows path is never read as one —
        // its backslashes are separators, not escapes (`windows_words`). No
        // sender/application identity is involved.
        let tokens = if windows {
            windows_words(line)
        } else {
            runeengine::perm::denylist::tokens(line)
        };
        for token in tokens {
            let bare = token
                .strip_prefix("file://")
                .map(|uri| {
                    let path = uri
                        .strip_prefix("localhost/")
                        .map(|path| format!("/{path}"))
                        .unwrap_or_else(|| uri.to_string());
                    // `file:///C:/x` names `C:/x`.
                    match path.as_bytes() {
                        [b'/', drive, b':', ..] if windows && drive.is_ascii_alphabetic() => {
                            path[1..].to_string()
                        }
                        _ => path,
                    }
                })
                .unwrap_or(token)
                .replace("%20", " ");
            let path = std::path::PathBuf::from(shellexpand(&bare));
            if !path.is_file() {
                return None;
            }
            out.push(path);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The words of a line pasted on Windows: split at blanks outside quotes,
/// the quotes dropped — how Explorer and Windows Terminal hand over a
/// dropped path (`"C:\\My Files\\shot.png"`) — and single quotes taken as
/// quotes only where a word begins, so `O'Brien` stays a name. No escape
/// character: a backslash is a path's.
fn windows_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"') => {
                quote = Some('"');
                started = true;
            }
            (None, '\'') if !started => {
                quote = Some('\'');
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words.retain(|word| !word.is_empty());
    words
}

/// `~/x` → `/home/you/x`. Only the leading tilde: everything else a shell would
/// expand is a shell's business, and this is not one.
fn shellexpand(path: &str) -> String {
    let path_owned;
    let path = match path.strip_prefix("~\\") {
        // Windows spells the same `~\x`.
        Some(rest) if runeengine::platform::OS == runeengine::platform::Os::Windows => {
            path_owned = format!("~/{rest}");
            path_owned.as_str()
        }
        _ => path,
    };
    match path.strip_prefix("~/") {
        Some(rest) => match runeengine::platform::home_dir()
            .as_deref()
            .and_then(std::path::Path::to_str)
        {
            Some(home) => format!("{home}/{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Tests and renderer-facing motion use the same projection as the editor.
#[cfg(test)]
pub(super) fn cursor_up(input: &str, cursor: usize, width: usize) -> Option<usize> {
    super::editing::vertical(input, cursor, width, true, None).0
}
#[cfg(test)]
pub(super) fn cursor_down(input: &str, cursor: usize, width: usize) -> Option<usize> {
    super::editing::vertical(input, cursor, width, false, None).0
}

impl App {
    pub fn on_key(&mut self, key: Key, d: &mut Driver) {
        // Another window promoting this draft is rare; asking the registry on
        // every key was a synchronous engine call on the UI thread (audit T4).
        let check_due = self
            .composition
            .generation_checked
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(250));
        if let Some(mut draft) = self
            .composition
            .draft
            .as_ref()
            .filter(|_| !self.composition.promote_pending && check_due)
            .cloned()
        {
            self.composition.generation_checked = Some(std::time::Instant::now());
            if let Ok(generation) = d
                .local(Command::DraftCheck {
                    id: 0,
                    draft: Box::new(draft.clone()),
                })
                .and_then(|data| {
                    data["generation"]
                        .as_u64()
                        .ok_or_else(|| "Draft generation was not returned".into())
                })
            {
                if generation != draft.generation {
                    draft.generation = generation;
                    draft.composer.text.clear();
                    draft.composer.attachments.clear();
                    draft.composer.cursor = 0;
                    draft.composer.revision = 0;
                    draft.history.clear();
                    self.composition.flight = None;
                    self.take_draft(draft, None);
                }
            }
        }
        // Anything but a second Ctrl-C disarms the quit.
        if !matches!(key, Key::Ctrl('c')) {
            self.quit_armed = false;
            self.quit_armed_at = None;
        }
        if key != Key::Esc {
            self.stop_armed = false;
            self.stop_armed_at = None;
        }

        if key == Key::Ctrl('c') {
            if self.busy {
                let now = Instant::now();
                let armed = self.quit_armed
                    && self.quit_armed_at.is_some_and(|at| {
                        now.duration_since(at)
                            < super::runtime_configuration().duration("ui.quit_lifetime")
                    });
                if armed {
                    self.quit = true;
                    self.quit_armed = false;
                    self.quit_armed_at = None;
                } else {
                    self.quit_armed = true;
                    self.quit_armed_at = Some(now);
                    self.note("press ctrl+c again to leave Rune · esc twice to stop AI");
                }
            } else {
                self.stop_or_quit(d);
            }
            return;
        }

        if let Some(picker) = self.session_picker.as_mut() {
            picker.on_key(
                key,
                self.preferences.height.max(8) as u16,
                super::term::cols() as u16,
            );
            if let Some((session, title)) = picker.pending_rename.take() {
                d.ask(|id| Command::SessionRename { id, session, title });
            }
            if picker.done {
                self.pending_session_switch = picker.selected_id;
                self.session_picker = None;
                self.resync = true;
            }
            return;
        }

        // A visible Settings hierarchy owns all keys except global Ctrl-C:
        // close its editor/page or toggle closed before background actions run.
        if self.panel == Panel::Settings && !matches!(key, Key::Ctrl('c')) {
            if key == Key::Esc {
                self.stop_armed = false;
                self.stop_armed_at = None;
            }
            if let Key::Mouse(m) = key {
                self.on_mouse(m, d);
            } else {
                settings::key(self, &key, d);
            }
            return;
        }

        // Global shortcuts are available from any screen, including modals and prompts.
        match key {
            Key::Ctrl('g') => {
                self.toggle(Panel::Settings);
                return;
            }
            Key::Ctrl('t') => {
                self.toggle(Panel::Tasks);
                return;
            }
            Key::Ctrl('o') => {
                self.set_sidebar(!self.sidebar);
                return;
            }
            Key::Ctrl('l') => {
                self.resync = true;
                return;
            }
            Key::Ctrl('r') => {
                self.toggle_all_steps();
                return;
            }
            _ => {}
        }

        if self.searching()
            && (super::editing::action(&key).is_some()
                || matches!(
                    key,
                    Key::Tab
                        | Key::ShiftTab
                        | Key::Enter
                        | Key::Esc
                        | Key::Char(_)
                        | Key::Paste(_)
                        | Key::NewLine
                ))
        {
            if key == Key::Esc {
                self.input.clear();
                self.cursor = 0;
                self.input_editor.reset();
            } else {
                self.compose(key, d);
            }
            return;
        }

        // Any open panel (Logs, Tasks/Plan, Undo, Help) closes on Esc before
        // turn-level cancellation runs, so reading logs or plan never stops the AI.
        if self.panel != Panel::None && key == Key::Esc {
            self.stop_armed = false;
            self.stop_armed_at = None;
            if matches!(self.panel, Panel::Undo | Panel::Redo) && super::undo::esc(self) {
                // The diff went back to the list, or a countdown stopped.
            } else {
                self.show_panel(Panel::None);
            }
            self.transcript_dirty = true;
            return;
        }

        if key == Key::Esc
            && self
                .awaiting
                .values()
                .any(|request| matches!(request, Awaiting::Sessions))
        {
            self.show_panel(Panel::None);
            self.clear_status();
            return;
        }
        if key == Key::Esc && self.busy && self.questions.is_none() && self.mcp_input.is_none() {
            let now = Instant::now();
            let armed = self.stop_armed
                && self.stop_armed_at.is_some_and(|at| {
                    now.duration_since(at)
                        < super::runtime_configuration().duration("ui.stop_lifetime")
                });
            if armed {
                self.stop_armed = false;
                self.stop_armed_at = None;
                self.stop_or_quit(d);
            } else {
                self.stop_armed = true;
                self.stop_armed_at = Some(now);
                self.note("press esc again to stop");
            }
            return;
        }

        // Back to the newest row from anywhere: a form or a card being
        // answered is answered about the conversation, and reading its end is
        // never in their way.
        if key == Key::CtrlDown {
            self.jump_to_latest();
            return;
        }
        if self.panel != Panel::None {
            if let Key::Mouse(mouse) = key {
                self.on_mouse(mouse, d);
                return;
            }
            if panels::key(self, &key, d) {
                return;
            }
        }
        if self.mcp_input.is_some() {
            if let Key::Mouse(m) = key {
                self.on_mouse(m, d);
            } else {
                super::mcp_input::key(self, &key, d);
            }
            return;
        }
        if self.questions.is_some() {
            if let Key::Mouse(m) = key {
                self.on_mouse(m, d);
            } else if matches!(key, Key::PageUp | Key::PageDown)
                && !self.questions.as_ref().is_some_and(|q| q.overflows())
            {
                // The modal fits, so nothing in it wants these keys. The user is
                // being asked a question *about the conversation above*, and the
                // old dispatch gave this screen every key and returned — so the
                // very chat they needed to re-read to answer was unreachable.
                self.scroll_by(key == Key::PageUp, self.drawn_viewport.max(1) / 2 + 1);
            } else {
                super::questions::key(self, &key, d);
            }
            return;
        }

        // Approval stays on Left/Right/Enter. An overflowing permission document
        // owns Up/Down and PageUp/PageDown; a card that fits leaves them to the
        // transcript. The wheel remains row-scoped, so the conversation above
        // the card can always be reread without answering the pending request.
        if self.pending.is_some()
            && matches!(key, Key::PageUp | Key::PageDown | Key::Up | Key::Down)
        {
            let up = matches!(key, Key::PageUp | Key::Up);
            if self.permission.max > 0 {
                let rows = if matches!(key, Key::PageUp | Key::PageDown) {
                    self.permission.visible.max(1)
                } else {
                    1
                };
                self.permission.scroll_by(up, rows);
            } else {
                let rows = if matches!(key, Key::PageUp | Key::PageDown) {
                    self.drawn_viewport.max(1) / 2 + 1
                } else {
                    1
                };
                self.scroll_by(up, rows);
            }
            return;
        }
        if self.pending.is_some() && self.answer_permission(&key, d) {
            return;
        }

        match key {
            Key::Mouse(m) => self.on_mouse(m, d),
            Key::Alt('t') => self.cycle_thinking(d),
            Key::Esc => {
                if self.panel != Panel::None {
                    self.show_panel(Panel::None);
                } else if self.sidebar {
                    // The same door Ctrl+O uses: the choice is written to
                    // `rune.toml`, so every window and the next start agree
                    // with it instead of this window alone hiding the panel.
                    self.set_sidebar(false);
                } else {
                    self.clear_status();
                }
            }
            Key::PageUp => self.history_prev(),
            Key::PageDown => self.history_next(),
            Key::Tab | Key::ShiftTab if self.panel != Panel::None => {}
            key => self.compose(key, d),
        }
    }

    /// Is the pointer over the side panel, and does the panel's list own the
    /// wheel?
    ///
    /// The column test decides the panel; `plan_max` decides the list. **Once
    /// the panel's list has triggered scrolling — a plan or a FILE CHANGE
    /// section longer than the panel, the state whose mark is `N rows above` —
    /// every wheel over the panel belongs to it, spent directions included.**
    /// A wheel the panel cannot answer does nothing: the chat beside it must
    /// not move because the reader's hand was on the panel.
    ///
    /// A panel whose list fits entirely takes nothing: the reader is looking
    /// at the chat beside it, the panel is showing everything it has, and a
    /// wheel that scrolled an empty margin would look like a broken mouse.
    fn pointer_in_sidebar(&self, row: u16, col: u16) -> bool {
        row > 0
            && row <= self.sidebar_last_row
            && self
                .sidebar_cols
                .is_some_and(|(first, last)| col >= first && col <= last)
    }

    fn wheel_over_the_panel(&self, row: u16, col: u16) -> bool {
        self.pointer_in_sidebar(row, col) && self.plan_max_scroll() > 0
    }

    /// How far the panel's list — plan items and FILE CHANGE rows together —
    /// can be scrolled, in rows.
    ///
    /// The renderer owns this number — it is the one that knows how tall the
    /// panel is and how many rows the items wrap to — so it is published there
    /// and read here, rather than computed a second time from a second guess at
    /// the same geometry. See [`super::draw::Hit`] for why a second layout
    /// calculation is a bug waiting for the day one of the two is edited.
    fn plan_max_scroll(&self) -> usize {
        self.plan_max
    }

    /// Is the pointer over the ask-user card's rows, as the last frame placed
    /// them? The range is a renderer's answer, not a second layout — see
    /// [`App::questions_card`] for why that distinction is the whole argument.
    fn wheel_over_the_card(&self, row: u16) -> bool {
        self.questions_card
            .is_some_and(|(first, last)| row >= first && row <= last)
    }

    /// What the pointer did, answered from the map the last frame left behind.
    ///
    /// A press starts a possible selection. Motion turns it into one; release
    /// copies it. With no motion, release remains the ordinary click that opens
    /// a step. This is the guard that lets selection and click targets coexist.
    fn on_mouse(&mut self, m: super::input::Mouse, d: &mut Driver) {
        use super::input::Click;
        match m.kind {
            // The wheel scrolls whatever is under it. In a list that is the
            // cursor, because a list that scrolls away from its own selection is
            // a list that has to be scrolled back.
            Click::WheelUp | Click::WheelDown => {
                self.selection = None;
                let up = m.kind == Click::WheelUp;
                let multiplier = m.modifiers.wheel_multiplier();
                // The modal's list scrolls only if it has somewhere of its own
                // to go, and — row-scoped, the owner's call after the global
                // kind locked the chat for the length of a long form — only
                // when the pointer is actually over the card's rows. A wheel
                // on the transcript above the card is the reader's hand on the
                // transcript, whatever the card wants; a card that fits takes
                // nothing at all. The settings screen keeps its own wheel.
                if self.panel == Panel::Settings {
                    settings::key(self, &Key::Mouse(m), d);
                } else if self.searching()
                    && !self.pointer_in_sidebar(m.row, m.col)
                    && matches!(
                        super::draw::hit_at(self, m.row),
                        super::draw::Hit::Palette(_)
                    )
                {
                    for _ in 0..multiplier {
                        self.compose(if up { Key::Up } else { Key::Down }, d);
                    }
                } else if self.wheel_over_the_panel(m.row, m.col) {
                    // The wheel is over the side panel of a plan that
                    // triggered scrolling, so the plan moves — and when it has
                    // run out in this direction, the gesture is swallowed. It
                    // must not drag the chat: the reader's hand is on the
                    // panel, and a panel that scrolls the transcript from
                    // under itself is a stray hand on the reader's place.
                    //
                    // # The fallthrough that used to live here is gone
                    //
                    // The old rule passed a spent wheel on to the transcript
                    // — "what a reader pressing up at the end of a list
                    // means". The owner overruled it on 27 Sep: once the plan
                    // scrolls, the two panes are two documents, and a wheel
                    // addressed to one must never move the other, ends
                    // included. A plan that fits (no overflow at all) still
                    // leaves the wheel to the transcript below.
                    let max = self.plan_max_scroll();
                    self.plan_scroll_by(up, 3 * multiplier, max);
                } else if self.questions.as_ref().is_some_and(|q| q.overflows())
                    && self.wheel_over_the_card(m.row)
                {
                    super::questions::wheel(self, up, multiplier);
                } else if self.pending.is_some()
                    && self.permission.contains(m.row)
                    && self.permission.max > 0
                {
                    self.permission.scroll_by(up, 3 * multiplier);
                } else if self.panel != Panel::None {
                    panels::wheel(self, up, multiplier, d);
                } else {
                    let fast = m.modifiers.ctrl || m.modifiers.alt;
                    self.wheel_scroll_at(up, fast);
                }
            }
            Click::Press => {
                let point = super::draw::Point {
                    row: m.row,
                    col: m.col,
                };
                self.selection = Some(super::draw::Selection {
                    anchor: point,
                    head: point,
                    moved: false,
                });
            }
            Click::Drag => {
                if let Some(selection) = &mut self.selection {
                    selection.head = super::draw::Point {
                        row: m.row,
                        col: m.col,
                    };
                    selection.moved |= selection.head != selection.anchor;
                }
            }
            Click::Release => {
                let Some(mut selection) = self.selection.take() else {
                    self.press(m.row, m.col, d);
                    return;
                };
                selection.head = super::draw::Point {
                    row: m.row,
                    col: m.col,
                };
                let hit_anchor = super::draw::hit_at(self, selection.anchor.row);

                // Clicking on an attachment chip or interactive button is never a text selection.
                let is_control = matches!(
                    hit_anchor,
                    super::draw::Hit::Attachments { .. } | super::draw::Hit::Answer(_)
                );

                if is_control && selection.anchor.row == m.row {
                    self.selection = None;
                    let attached_before = self.attached.len();
                    self.press(m.row, m.col, d);
                    // If release col didn't trigger detachment (e.g. slight jitter on release),
                    // try anchor col where the mouse press started.
                    if matches!(hit_anchor, super::draw::Hit::Attachments { .. })
                        && self.attached.len() == attached_before
                        && selection.anchor.col != m.col
                    {
                        self.press(selection.anchor.row, selection.anchor.col, d);
                    }
                    return;
                }

                // A one-cell release jitter over navigation is still one
                // click, not a clipboard selection. Real drags keep selecting.
                let navigation = matches!(
                    hit_anchor,
                    super::draw::Hit::Settings(_)
                        | super::draw::Hit::Log(_)
                        | super::draw::Hit::Questions(_)
                        | super::draw::Hit::McpText(_)
                        | super::draw::Hit::Palette(_)
                        | super::draw::Hit::UndoFile { .. }
                        | super::draw::Hit::McpSubmit { .. }
                );
                if navigation
                    && !selection.moved
                    && selection.anchor.row == m.row
                    && selection.anchor.col.abs_diff(m.col) <= 1
                {
                    self.press(selection.anchor.row, selection.anchor.col, d);
                    return;
                }
                selection.moved |= selection.head != selection.anchor;
                self.selection = Some(selection);
                if selection.moved {
                    if self.select_input(selection.anchor, selection.head) {
                        self.selection = None;
                        return;
                    }
                    let selected = selection.selected_text(&self.selection_rows);
                    if !selected.is_empty() {
                        super::term::copy(&selected);
                        self.status = Some(format!(
                            "copied {} character(s) from the selection",
                            selected.chars().count()
                        ));
                    } else {
                        self.press(m.row, m.col, d);
                    }
                } else {
                    self.press(m.row, m.col, d);
                }
            }
        }
    }

    fn select_input(&mut self, first: super::draw::Point, last: super::draw::Point) -> bool {
        use super::draw::Hit;
        let (from, to) = (
            super::draw::hit_at(self, first.row),
            super::draw::hit_at(self, last.row),
        );
        match (from, to) {
            (Hit::ComposerText(a), Hit::ComposerText(b)) => {
                let Some(start) = self.composer_hit_rows.get(a).map(|r| r.caret_at(first.col))
                else {
                    return false;
                };
                let Some(row) = self.composer_hit_rows.get(b) else {
                    return false;
                };
                self.input_editor.select_in_row(
                    &self.input,
                    &mut self.cursor,
                    start,
                    row,
                    last.col,
                    self.composer_width,
                );
                true
            }
            (Hit::Settings(a), Hit::Settings(b)) => {
                settings::select(self, a, first.col, b, last.col)
            }
            (Hit::Questions(a), Hit::Questions(b)) => {
                super::questions::select(self, (a, first.col), (b, last.col))
            }
            (Hit::McpText(a), Hit::McpText(b)) if a == b => {
                super::mcp_input::select(self, a, first.col, last.col)
            }
            _ => false,
        }
    }

    fn press(&mut self, row: u16, col: u16, d: &mut Driver) {
        if self
            .jump_hint
            .is_some_and(|(at, first, last)| row == at && (first..=last).contains(&col))
        {
            self.jump_to_latest();
            return;
        }
        if self.pointer_in_sidebar(row, col) {
            return;
        }
        self.act(super::draw::hit_at(self, row), col, d)
    }

    /// Do what this hit means. Split from [`App::press`] so that the map lookup
    /// and the answer to it can be read — and tested — apart.
    fn act(&mut self, hit: super::draw::Hit, col: u16, d: &mut Driver) {
        match hit {
            super::draw::Hit::None => {}
            super::draw::Hit::Questions(r) => super::questions::click(self, r, col, d),
            super::draw::Hit::McpText(field) => super::mcp_input::click(self, field, col),
            super::draw::Hit::McpSubmit { first, last } => {
                if (first..=last).contains(&(col as usize)) {
                    super::mcp_input::submit(self, d);
                }
            }
            super::draw::Hit::Settings(target) => settings::click(self, target, col, d),
            super::draw::Hit::Log(index) => panels::click_log(self, index),
            // A thought or tool step opens and closes where it was pressed.
            super::draw::Hit::Block(at) => self.open_block(at),
            super::draw::Hit::Answer(row) => {
                if self.pending.is_some() {
                    if let Some(at) = self.permission.answer_at(row, col) {
                        self.ask_choice = at;
                        self.answer_permission(&Key::Enter, d);
                    }
                }
            }
            super::draw::Hit::Palette(n) => {
                self.palette_row = n;
                if let Some(chosen) = self.chosen_command() {
                    let completed = if chosen.ends_with(' ') {
                        chosen
                    } else {
                        format!("{chosen} ")
                    };
                    self.input = completed;
                    self.cursor = self.input.len();
                }
            }
            super::draw::Hit::Attachments {
                start,
                visible,
                width,
            } => {
                let item_w = width.max(4);
                let slot_w = item_w + 1;
                if let Some(box_area_col) = (col as usize).checked_sub(start + 1) {
                    let index = box_area_col / slot_w;
                    let pos = box_area_col % slot_w;
                    // The close target on an attachment chip (` ×]`) spans the space before
                    // the cross, the cross itself, and the closing bracket.
                    if index < visible
                        && pos >= item_w.saturating_sub(3)
                        && pos < item_w
                        && index < self.attached.len()
                    {
                        let file = self.attached.remove(index);
                        self.status = Some(format!("{} is no longer attached", file.name));
                    }
                }
            }
            super::draw::Hit::UndoFile { index, button } => {
                super::undo::click(self, index, button, col, d)
            }
            super::draw::Hit::ComposerText(index) => {
                if let Some(row) = self.composer_hit_rows.get(index) {
                    let at = row.caret_at(col);
                    if self.input.is_char_boundary(at) {
                        self.input_editor.place_in_row(
                            &self.input,
                            &mut self.cursor,
                            row,
                            col,
                            self.composer_width,
                        );
                        self.selection = None;
                    }
                }
            }
            super::draw::Hit::Composer => {}
        }
    }

    /// Open or close one thought or one step.
    fn open_block(&mut self, at: usize) {
        match self.blocks.get_mut(at) {
            Some(Block::User { open, .. } | Block::Thinking { open, .. }) => *open = !*open,
            // Every step toggles, `track_tasks` included.
            //
            // It used to be excluded on purpose — the plan has a home in the
            // side panel, so the step was left folded. But that left the row the
            // transcript *does* draw, `… 4 tasks cut. Click to open all`, saying
            // "click" and answering nothing: a control that names a gesture and
            // then ignores it is worse than no control at all.
            Some(Block::Tool(tool)) if tool.state.finished() || (tool.name != "patch_file") => {
                tool.open = !tool.open;
            }
            _ => {}
        }
        self.transcript_dirty = true;
    }

    fn toggle(&mut self, panel: Panel) {
        let target = if self.panel == panel {
            Panel::None
        } else {
            panel
        };
        self.show_panel(target);
    }

    /// `ctrl+r`: open every folded step and every thought, or close them again.
    ///
    /// Opening is the useful direction — the transcript folds long runs to keep
    /// the newest thing on screen — but an open-everything key with no way back
    /// is a key that changes the screen and makes the user hunt for the undo.
    /// So it is one toggle: if anything is closed it opens everything, and if
    /// everything is already open it folds everything back.
    fn toggle_all_steps(&mut self) {
        let any_closed = self.blocks.iter().any(|block| match block {
            Block::Thinking { open, .. } => !*open,
            Block::Tool(tool) => !tool.open,
            _ => false,
        });
        for block in self.blocks.iter_mut() {
            match block {
                Block::Thinking { open, .. } => *open = any_closed,
                // Only steps that can be opened take part: a step still
                // streaming has nothing folded to reveal, and forcing it open
                // would show a half-written diff.
                Block::Tool(tool) if tool.state.finished() => tool.open = any_closed,
                _ => {}
            }
        }
        self.transcript_dirty = true;
    }

    /// Left and right choose an answer; enter gives it.
    ///
    /// It was three single letters — `y`, `a`, `n` — printed as a row of hints.
    /// Two things were wrong with that. A letter is a *guess* about what is on
    /// offer, so the card had to explain its own keyboard every time; and the
    /// one key that stands under a finger already resting on it, `y`, was the
    /// one that granted access. Moving along a row and pressing enter is slower
    /// by one keystroke and is the same gesture whatever is being asked.
    ///
    /// The card owns approval keys while it is up; Ctrl+C always handles
    /// leaving, and Esc twice cancels the whole turn before the card sees it.
    fn answer_permission(&mut self, key: &Key, d: &mut Driver) -> bool {
        let Some(ask) = self.pending.clone() else {
            return false;
        };
        let answers = ask.answers();
        let decision = match key {
            Key::Left => {
                self.ask_choice = self.ask_choice.saturating_sub(1);
                return true;
            }
            Key::Right => {
                self.ask_choice = (self.ask_choice + 1).min(answers.len() - 1);
                return true;
            }
            Key::Tab => {
                self.ask_choice = (self.ask_choice + 1) % answers.len();
                return true;
            }
            Key::Enter => answers[self.ask_choice.min(answers.len() - 1)].1,
            // A busy Escape is handled above this card: only the second one
            // cancels the turn. Never turn one Esc into an implicit Deny.
            Key::Esc => return false,
            // A pointer event is not an answer. Answering happens on a click
            // that lands on one of the answers — `press` reads the hit map — and
            // the wheel belongs to the reader. Claiming the whole `Key::Mouse`
            // family here is what made the card's own hint ("click an answer")
            // and its wheel both dead.
            Key::Mouse(_) => return false,
            Key::Ctrl(_) => return true,
            _ => return true,
        };
        d.handle(Command::PermissionReply {
            id: 0,
            req: ask.req,
            decision,
        });
        self.pending = None;
        true
    }

    /// `Esc` while busy stops the turn; idle `Ctrl-C` asks before leaving.
    ///
    /// The composer is never touched here. The keymap says *"stop the turn;
    /// twice on an empty line to leave"*, and the question form promises that
    /// ordinary chat input is restored after a cancel — so a key that silently
    /// wiped a half-written sentence was violating both. A non-empty line is
    /// therefore not an empty line: the first press only says what the second
    /// would do, and what has already been typed stays on screen.
    fn stop_or_quit(&mut self, d: &mut Driver) {
        if self.busy {
            let Some(session) = self.session else {
                return;
            };
            self.queued_prompt = None;
            d.handle(Command::Cancel {
                id: 0,
                session,
                call: None,
            });
            self.status = Some("stopping…".into());
            let now = Instant::now();
            for tool in self.blocks.iter_mut().filter_map(Block::tool_mut) {
                if !tool.state.finished() {
                    tool.state = ToolState::Stopped;
                    if tool.ended.is_none() {
                        tool.ended = Some(now);
                    }
                    tool.ask = None;
                    tool.snap_counters();
                }
            }
            self.transcript_dirty = true;
            return;
        }
        // The draft is untouched on the first press. A reader may need to
        // leave from a form or a half-written message; requiring an empty
        // composer made Ctrl+C twice silently do nothing in that case.
        if self.quit_armed {
            self.quit = true;
        } else {
            self.quit_armed = true;
            self.quit_armed_at = Some(Instant::now());
            self.note("press ctrl+c again to leave");
        }
    }

    pub fn set_mode(&mut self, next: &str, d: &mut Driver) {
        if self.composition.draft.is_some() {
            self.mode = next.to_string();
            self.composition.authoritative.preference_changed = true;
            self.composition
                .dirty_since
                .get_or_insert_with(Instant::now);
            self.sync_composer(d, true);
            self.clear_status();
            return;
        }
        let Some(session) = self.session else {
            return;
        };
        let previous = std::mem::replace(&mut self.mode, next.to_string());
        self.note(format!("Switching mode to {next}…"));
        let awaiting = Awaiting::Mode {
            previous,
            next: next.to_string(),
        };
        self.request(d, awaiting, |id| Command::SessionMode {
            id,
            session,
            mode: next.to_string(),
        });
    }

    pub fn cycle_mode(&mut self, d: &mut Driver) {
        let next = match self.mode.as_str() {
            "ask" => "auto",
            "auto" => "full",
            _ => "ask",
        };
        self.set_mode(next, d);
    }

    /// Product navigation stays outside the shared editor. A text field uses
    /// one canonical chord per edit, with no Readline aliases stealing a panel.
    fn compose(&mut self, key: Key, d: &mut Driver) {
        let editing = super::editing::action(&key);
        if editing.is_some_and(|a| {
            !matches!(
                a,
                super::editing::Action::Left
                    | super::editing::Action::Right
                    | super::editing::Action::WordLeft
                    | super::editing::Action::WordRight
                    | super::editing::Action::Up
                    | super::editing::Action::Down
                    | super::editing::Action::LineStart
                    | super::editing::Action::LineEnd
                    | super::editing::Action::Start
                    | super::editing::Action::End
                    | super::editing::Action::SelectLeft
                    | super::editing::Action::SelectRight
                    | super::editing::Action::SelectWordLeft
                    | super::editing::Action::SelectWordRight
                    | super::editing::Action::SelectUp
                    | super::editing::Action::SelectDown
            )
        }) || matches!(key, Key::Char(_) | Key::Paste(_) | Key::NewLine)
        {
            self.history_index = None;
        }
        // Whether this keystroke was moving through the command list. Everything
        // else changes what has been typed, and therefore what the list holds.
        let choosing = matches!(key, Key::Up | Key::Down);
        match key {
            Key::Paste(text) => {
                // A paste that is nothing but paths to real files is a file
                // being handed over, not text being typed. That is what dropping
                // a file on a terminal window *is* — the terminal types its path
                // — and it is what pasting a copied file does in every file
                // manager that puts a path or a `file://` URI on the clipboard.
                match paths_in(&text) {
                    Some(paths) => self.attach(&paths, d),
                    None => {
                        self.input_editor.key(
                            &mut self.input,
                            &mut self.cursor,
                            &Key::Paste(text),
                            super::editing::Options::multiline(self.composer_width),
                        );
                    }
                }
            }
            key if !matches!(key, Key::Up | Key::Down)
                && self.input_editor.key(
                    &mut self.input,
                    &mut self.cursor,
                    &key,
                    super::editing::Options::multiline(self.composer_width),
                ) => {}
            // While a command is being searched for, the arrows belong to the
            // list. There is nothing to scroll to that matters more than the
            // thing being chosen two rows below.
            Key::Up if self.searching() => {
                let count = matches(&self.input).len();
                self.palette_row = self.palette_row.saturating_sub(1);
                self.adjust_palette_scroll(count);
            }
            Key::Down if self.searching() => {
                let count = matches(&self.input).len();
                if count > 0 {
                    self.palette_row = (self.palette_row + 1).min(count - 1);
                    self.adjust_palette_scroll(count);
                }
            }
            Key::Up => {
                if self
                    .input_editor
                    .vertical_step(&self.input, &mut self.cursor, self.composer_width, true)
                    .is_some()
                {
                    // Moved up one row in multiline text, keeping desired column.
                } else if self.cursor > 0 {
                    // On top row: move to beginning of text.
                    self.input_editor.place(&self.input, &mut self.cursor, 0);
                } else if !self.prompt_history.is_empty() || self.composition.draft.is_some() {
                    // Already at start on top row: recall previous history prompt.
                    self.history_prev();
                    self.input_editor.observe(&self.input, self.cursor);
                }
            }
            Key::Down => {
                if let Some(_down) = self.input_editor.vertical_step(
                    &self.input,
                    &mut self.cursor,
                    self.composer_width,
                    false,
                ) {
                    // Moved down one row in multiline text, keeping desired column.
                } else if self.cursor < self.input.len() {
                    // On bottom row: move to end of text.
                    let len = self.input.len();
                    self.input_editor.place(&self.input, &mut self.cursor, len);
                } else if self.history_index.is_some() {
                    // Already at end on bottom row: recall next history prompt.
                    self.history_next();
                    self.input_editor.observe(&self.input, self.cursor);
                }
            }
            Key::Tab => {
                if self.searching() {
                    if let Some(chosen) = self.chosen_command() {
                        if self.input == chosen {
                            let count = matches(&self.input).len();
                            if count > 1 {
                                self.palette_row = (self.palette_row + 1) % count;
                                self.adjust_palette_scroll(count);
                                if let Some(next) = self.chosen_command() {
                                    self.input = next;
                                    self.cursor = self.input.len();
                                }
                            }
                        } else {
                            self.input = chosen;
                            self.cursor = self.input.len();
                        }
                    }
                } else {
                    self.cycle_mode(d);
                }
            }
            Key::Enter => {
                if self.searching() {
                    let hits = matches(&self.input);
                    if let Some((verb, args_hint, _)) =
                        hits.get(self.palette_row.min(hits.len().saturating_sub(1)))
                    {
                        let typed = self
                            .input
                            .strip_prefix('/')
                            .or_else(|| self.input.strip_prefix('?'))
                            .unwrap_or("");
                        let (_, typed_args) = typed
                            .split_once(char::is_whitespace)
                            .map(|(v, r)| (v.trim(), r.trim()))
                            .unwrap_or((typed.trim(), ""));

                        // If the command takes or accepts arguments, and no arguments are typed yet:
                        let opens_without_arguments = matches!(
                            *verb,
                            "logs" | "context" | "skills" | "mcp" | "model" | "provider"
                        );
                        if !args_hint.is_empty()
                            && typed_args.is_empty()
                            && !(opens_without_arguments && typed.trim() == *verb)
                        {
                            let cmd_prefix = format!("/{verb} ");
                            if self.input != cmd_prefix {
                                self.input = cmd_prefix;
                                self.cursor = self.input.len();
                                self.palette_row = 0;
                                self.palette_scroll = 0;
                                let tip = self.usage_tip(verb);
                                self.note(tip);
                                return;
                            } else {
                                // Input is already "/<verb> " and Enter was pressed again without arguments:
                                match *verb {
                                    "model" | "provider" => {
                                        // Open interactive selection screen
                                    }
                                    _ => {
                                        // Retain "/<verb> " and refresh status bar tip
                                        self.palette_row = 0;
                                        self.palette_scroll = 0;
                                        let tip = self.usage_tip(verb);
                                        self.note(tip);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    if let Some(chosen) = self.chosen_command() {
                        self.input = chosen;
                        self.cursor = self.input.len();
                    }
                }
                self.submit(d)
            }
            _ => {}
        }
        // The list is rebuilt on every keystroke, so a row number from the old
        // one points at the wrong command — or at nothing.
        if !choosing {
            self.palette_row = 0;
            self.palette_scroll = 0;
        }
    }

    /// Whether the composer is searching for a command right now.
    pub fn searching(&self) -> bool {
        self.composer_visible() && !matches(&self.input).is_empty()
    }

    /// The command the palette has under its cursor, written out in full.
    fn chosen_command(&self) -> Option<String> {
        let hits = matches(&self.input);
        let (verb, ..) = hits.get(self.palette_row.min(hits.len().saturating_sub(1)))?;
        let typed = self
            .input
            .strip_prefix('/')
            .or_else(|| self.input.strip_prefix('?'))?;
        let args = typed
            .split_once(char::is_whitespace)
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        Some(if args.is_empty() {
            format!("/{verb}")
        } else {
            format!("/{verb} {args}")
        })
    }

    /// Take these files along with the next question.
    ///
    /// The engine decides what each one *is* — from its bytes, never its name —
    /// so a file that cannot be read is refused here, while the user is still
    /// looking at the composer rather than at a turn that has already started.
    pub fn attach(&mut self, paths: &[std::path::PathBuf], d: &mut Driver) {
        let mut added = 0;
        for path in paths {
            match runeengine::media::Attachment::at(path) {
                Ok(file) => {
                    if self.attached.iter().any(|a| a.path == file.path) {
                        continue;
                    }
                    self.attached.push(file);
                    added += 1;
                }
                Err(e) => {
                    self.note(e.to_string());
                    return;
                }
            }
        }
        match (added, self.attached.last()) {
            (0, _) => self.note("already attached"),
            (1, _) => self.note("File attached · /detach <number> removes it"),
            (n, _) => self.note(format!("{n} files attached")),
        };
        self.refresh_attachment_model(d);
    }

    pub fn refresh_attachment_model(&mut self, d: &mut Driver) {
        let provider = self.provider().to_string();
        if !self.attached.iter().any(|a| !a.is_text())
            || provider.is_empty()
            || self
                .lookup_model_info(&provider, self.model())
                .is_some_and(|model| !model.modalities.is_empty())
            || !self.attachment_catalogues.insert(provider.clone())
        {
            return;
        }
        self.status = Some("Checking attachment support…".into());
        self.request(d, Awaiting::Catalogue(provider.clone()), |id| {
            Command::ProviderModels { id, name: provider }
        });
    }

    pub(super) fn submit(&mut self, d: &mut Driver) {
        let text = self.input.trim().to_string();
        if text.is_empty() && self.attached.is_empty() {
            return;
        }
        // Whatever was being read is done being read: the answer is about to
        // arrive underneath it. A command that opens a panel opens it again
        // below, so `/settings` from inside `/help` still lands.
        self.show_panel(Panel::None);
        if text == "?" || text == "help" {
            self.input.clear();
            self.input_editor.reset();
            self.cursor = 0;
            self.show_panel(Panel::Help);
            return;
        }
        if let Some(command) = text.strip_prefix('/') {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                self.note("Type / followed by a command or use ↑↓ to choose");
                self.input = "/".into();
                self.cursor = 1;
                return;
            }
            self.input.clear();
            self.input_editor.reset();
            self.cursor = 0;
            self.slash(command, d);
            return;
        }
        if let Some(command) = text.strip_prefix('?') {
            self.input.clear();
            self.input_editor.reset();
            self.cursor = 0;
            let cmd = if command.is_empty() { "help" } else { command };
            self.slash(cmd, d);
            return;
        }
        // Some terminal/file-manager combinations paste or drop paths as ordinary
        // key events instead of bracketed paste. Re-run the canonical batch parser
        // at send time so these paths cannot leak into a chat message.
        if let Some(paths) = paths_in(&text) {
            self.input.clear();
            self.input_editor.reset();
            self.cursor = 0;
            self.attach(&paths, d);
            return;
        }
        if self.pending.is_some() || self.questions.is_some() {
            self.note("Answer the active dialog before sending a new prompt");
            return;
        }
        if self.model().is_empty() {
            self.status =
                Some("Select Models — please choose a model in settings (/settings)".into());
            return;
        }
        self.refresh_attachment_model(d);
        if self.composition.draft.is_some() && !self.sync_composer(d, true) {
            return;
        }
        if let Some(mut draft) = self.composition.draft.clone() {
            if self.composition.promote_pending {
                if self
                    .composition
                    .promote_started
                    .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(4))
                {
                    self.composition.promote_pending = false;
                    self.composition.promote_started = None;
                } else {
                    return;
                }
            }
            draft.composer = self.composer_snapshot();
            draft.composer.text = text;
            self.composition.submitted = Some(draft.composer.clone());
            self.composition.promote_pending = true;
            self.composition.promote_started = Some(std::time::Instant::now());
            self.request(d, Awaiting::Promoted, |id| Command::DraftPromote {
                id,
                draft: Box::new(draft),
            });
            return;
        }
        if self.session.is_none() {
            return;
        }
        // The message leaves the composer now, whatever the host is still
        // confirming: what is typed next is the next message's. A submit that
        // waited for an edit in flight once kept the composer and sent it
        // whole when the edit landed — the words typed meanwhile glued on.
        let composer = self.composer_snapshot();
        let media = std::mem::take(&mut self.attached);
        if !text.is_empty()
            && !text.starts_with('/')
            && self.prompt_history.last().map(|s| s.as_str()) != Some(&text)
        {
            self.prompt_history.push(text.clone());
        }
        self.history_index = None;
        self.history_draft.clear();
        self.input.clear();
        self.input_editor.reset();
        self.cursor = 0;
        self.scroll = 0;
        self.scroll_anchor = None;
        self.glide = None;
        // A new turn writes a new plan, so the old scroll position is a
        // position in a list that no longer exists. Left alone, the panel would
        // open halfway down a fresh plan, which reads as though the work had
        // already been started.
        self.reset_plan_scroll();
        self.composition.submits.push_back(super::composer::Submit {
            text,
            media,
            composer,
        });
        self.send_submits(d);
    }

    /// Send the oldest message waiting to be submitted, once the host holds
    /// it: an edit still in flight is waited for (the main loop calls again
    /// when it lands), one unanswered for three seconds is given up.
    pub(super) fn send_submits(&mut self, d: &mut Driver) {
        if self.composition.submits.is_empty() {
            return;
        }
        let Some(session) = self.session else {
            return;
        };
        if self.composition.ready {
            self.composition.urgent = true;
            self.sync_composer(d, false);
            self.composition.urgent = false;
            if self.composition.flight.is_some() {
                if self
                    .composition
                    .flight_started
                    .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(3))
                {
                    self.composition.flight = None;
                    self.composition.flight_started = None;
                } else {
                    return;
                }
            }
        }
        let Some(super::composer::Submit {
            text,
            media,
            composer,
        }) = self.composition.submits.pop_front()
        else {
            return;
        };
        self.composition.submitted = Some(composer);
        let attach: Vec<String> = media.iter().map(|a| a.path.clone()).collect();
        let user_idx = self.blocks.len();
        self.blocks.push(Block::user_timed(
            text.clone(),
            Some(super::term::now_hm()),
            media.clone(),
        ));
        self.transcript_dirty = true;
        // Every message goes the one way: the engine admits it — a turn of its
        // own, or a wait for the work in progress — so it is drawn as waiting
        // until the engine says it has taken it, whatever this window believes
        // about the session being busy.
        let steer = super::QueuedSteer {
            text: text.clone(),
            remote: false,
            pending_id: None,
        };
        match &mut self.queued_prompt {
            Some(queued) => queued.steers.push(steer),
            None => {
                self.queued_prompt = Some(super::QueuedPrompt {
                    steers: vec![steer],
                    first_user_block: user_idx,
                })
            }
        }
        self.request(
            d,
            Awaiting::Prompt {
                text: text.clone(),
                media,
            },
            |id| Command::PromptSteer {
                id,
                session,
                text,
                attach,
            },
        );
    }

    /// How a slash command is used — one table for every place that says so
    /// (audit D8: it was written out three times and had already drifted).
    fn usage_tip(&self, verb: &str) -> &'static str {
        match verb {
            "attach" => "Usage: /attach <path> · specify a file path or drop files",
            "detach" if self.attached.is_empty() => "No files currently attached",
            "detach" => "Usage: /detach <number> or /detach all",
            "set" => "Usage: /set key=value (e.g. /set sampling.temperature=0.7)",
            "mode" => "Usage: /mode [ask|auto|full] (e.g. /mode auto)",
            "theme" => "Usage: /theme [violet|cyan|amber] (e.g. /theme cyan)",
            "classifier" => "Usage: /classifier [on|off|model <id|inherit>|reload|status]",
            "session" => "Usage: /session · browse and switch sessions in this workspace",
            "resume" => "Usage: /resume <session_id> (e.g. /resume 426)",
            "compact" => "Usage: /compact · trigger session history compaction",
            "logs" => "Usage: /logs [all|warn|error] · what happened behind the conversation",
            "model" => "Usage: /model [<provider>:<id>|<id>] · select model, or press Enter again to browse list",
            "provider" => "Usage: /provider <name> · specify provider name, or press Enter again to browse list",
            "thinking" => "Usage: /thinking [off|<effort>] · set reasoning effort (or press Alt+T to cycle)",
            _ => "",
        }
    }

    /// Slash commands: the same operations as the shortcuts, for people who
    /// would rather type than remember a keymap.
    fn slash(&mut self, command: &str, d: &mut Driver) {
        let mut words = command.split_whitespace();
        let verb = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();
        match verb {
            "skills" | "skill" | "mcp" => {
                if self.busy {
                    self.note("Wait for the active turn before managing capabilities");
                    return;
                }
                let Some(session) = self.session else {
                    self.note(
                        "Project capabilities are available after the first accepted message.",
                    );
                    return;
                };
                let parsed = runeengine::perm::denylist::tokens(command);
                let rest: Vec<_> = parsed.iter().skip(1).map(String::as_str).collect();
                let request = runeengine::capabilities::manage::Request {
                    group: if verb == "skill" {
                        "skills".into()
                    } else {
                        verb.into()
                    },
                    operation: if verb == "skill" {
                        "activate".into()
                    } else {
                        rest.first().copied().unwrap_or("list").into()
                    },
                    arguments: if verb == "skill" {
                        rest.iter().map(|s| s.to_string()).collect()
                    } else {
                        rest.iter().skip(1).map(|s| s.to_string()).collect()
                    },
                };
                self.show_panel(Panel::Capabilities);
                self.request(d, Awaiting::Capabilities, |id| Command::CapabilityManage {
                    id,
                    session,
                    request,
                });
            }
            "help" | "?" => self.show_panel(Panel::Help),
            "settings" | "*" => settings::enter(self, settings::Page::Overview, d),
            // With an argument these set; without one they show. Naming the
            // thing you want is faster than cycling to it, and the engine is
            // still the authority on whether it exists.
            "model" => match rest.first().copied() {
                None => settings::enter(self, settings::Page::Models, d),
                Some("config" | "configure" | "setup") => {
                    let target = if rest.len() > 1 {
                        rest[1..].join(" ")
                    } else {
                        self.model().to_string()
                    };
                    let (provider, model) = target
                        .split_once(':')
                        .map(|(p, m)| (p.to_string(), m.to_string()))
                        .unwrap_or_else(|| (self.provider().to_string(), target));
                    settings::enter(self, settings::Page::ModelConfig { provider, model }, d);
                }
                Some(_) => settings::apply(self, "model", &rest.join(" "), d),
            },
            "provider" => match rest.is_empty() {
                true => settings::enter(self, settings::Page::Connections, d),
                false => settings::apply(self, "provider", &rest.join(" "), d),
            },
            "thinking" => match rest.first().copied() {
                None => self.cycle_thinking(d),
                Some("off") => {
                    let mandatory = self.settings["thinking_required"].as_bool() == Some(true);
                    if mandatory {
                        self.note("Thinking is mandatory for this model and cannot be turned off");
                    } else if self.is_suppressed("thinking") && self.is_suppressed("effort") {
                        self.note("Reasoning is not supported by this model or provider");
                    } else {
                        settings::apply(self, "thinking", "false", d);
                        self.settings["settings"]["thinking"] = serde_json::json!(false);
                        self.settings["settings"]["effort"] = serde_json::Value::Null;
                    }
                }
                Some("on") => {
                    if self.is_suppressed("thinking") && self.is_suppressed("effort") {
                        self.note("Reasoning is not supported by this model or provider");
                    } else if self.is_suppressed("effort") {
                        settings::apply(self, "thinking", "true", d);
                        self.settings["settings"]["thinking"] = serde_json::json!(true);
                    } else {
                        settings::apply(self, "thinking", "true", d);
                        self.settings["settings"]["thinking"] = serde_json::json!(true);
                        let choices: Vec<String> = self.settings["knobs"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|k| k["key"] == "effort")
                            .and_then(|k| k["choices"].as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        let route_default = choices.iter().any(|v| v == super::ROUTE_DEFAULT);
                        // The levels this model offers, as the engine lists them.
                        let levels = choices
                            .iter()
                            .filter(|v| *v != "off" && *v != super::ROUTE_DEFAULT)
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join("|");
                        let required = self.settings["thinking_required"].as_bool() == Some(true);
                        // The footer cuts a long note: what to type comes first.
                        let note = if route_default && required {
                            format!("Route default — /thinking {levels} · Alt+T cycles")
                        } else if route_default {
                            format!(
                                "Route default — /thinking {levels} · /thinking off · Alt+T cycles"
                            )
                        } else {
                            "Specify an effort level (e.g. /thinking low) or /thinking off; press Alt+T to cycle".to_string()
                        };
                        self.note(note);
                    }
                }
                _ => {
                    if self.is_suppressed("effort") {
                        self.note(
                            "Reasoning effort levels are not supported by this model or provider",
                        );
                    } else {
                        let eff = rest.join(" ");
                        settings::apply(self, "effort", &eff, d);
                        self.settings["settings"]["thinking"] = serde_json::json!(true);
                        self.settings["settings"]["effort"] = serde_json::json!(&eff);
                    }
                }
            },
            "classifier" => match rest.first().copied() {
                None | Some("status") => {
                    self.show_classifier_status();
                }
                Some("on" | "enable") => {
                    settings::apply(self, "classifier.enabled", "true", d);
                }
                Some("off" | "disable") => {
                    settings::apply(self, "classifier.enabled", "false", d);
                }
                Some("reload") => {
                    settings::apply(self, "classifier.reload", "true", d);
                }
                Some("model") => {
                    let arg = rest[1..].join(" ").trim().to_string();
                    if arg.is_empty()
                        || matches!(arg.as_str(), "inherit" | "reset" | "default" | "none")
                    {
                        settings::apply(self, "classifier.model", "", d);
                    } else if arg.starts_with('{') {
                        settings::apply(self, "classifier.model", &arg, d);
                    } else if let Some((p, m)) = arg.split_once('/') {
                        let formatted =
                            format!("{{ provider = \"{}\", id = \"{}\" }}", p.trim(), m.trim());
                        settings::apply(self, "classifier.model", &formatted, d);
                    } else {
                        let provider = self.provider();
                        let provider = if provider.is_empty() {
                            "anthropic"
                        } else {
                            provider
                        };
                        let formatted =
                            format!("{{ provider = \"{}\", id = \"{}\" }}", provider, arg);
                        settings::apply(self, "classifier.model", &formatted, d);
                    }
                }
                _ => {
                    self.note(self.usage_tip("classifier"));
                }
            },
            "panel" | "detail" => {
                self.set_sidebar(!self.sidebar);
                let min_cols =
                    super::runtime_configuration().number("ui.split_min_columns") as usize;
                let cols = super::term::cols();
                if self.sidebar {
                    if cols < min_cols {
                        self.note(format!(
                            "Session panel is on, but hidden (terminal is {cols} cols, needs {min_cols}+)"
                        ));
                    } else {
                        self.note("Session panel shown");
                    }
                } else {
                    self.note("Session panel hidden");
                }
            }
            "attach" => match rest.is_empty() {
                true => {
                    self.note(self.usage_tip("attach"));
                    self.input = "/attach ".into();
                    self.cursor = self.input.len();
                }
                false => {
                    let raw = command
                        .split_once(char::is_whitespace)
                        .map(|(_, p)| p.trim())
                        .unwrap_or("");
                    let paths = paths_in(raw)
                        .unwrap_or_else(|| vec![std::path::PathBuf::from(shellexpand(raw))]);
                    self.attach(&paths, d);
                }
            },
            "detach" => match rest.as_slice() {
                ["all"] => {
                    self.attached.clear();
                    self.note("All attachments removed");
                }
                [number]
                    if number
                        .parse::<usize>()
                        .ok()
                        .is_some_and(|n| n > 0 && n <= self.attached.len()) =>
                {
                    let file = self.attached.remove(number.parse::<usize>().unwrap() - 1);
                    self.note(format!("{} detached", file.name));
                }
                _ => {
                    if self.attached.is_empty() {
                        self.note("No files currently attached");
                    } else {
                        self.note(format!(
                            "Attached: {} file(s) · Use /detach <number> or /detach all",
                            self.attached.len()
                        ));
                    }
                    self.input = "/detach ".into();
                    self.cursor = self.input.len();
                }
            },
            "verbose" => {
                self.verbose = !self.verbose;
                self.transcript_dirty = true;
                let state = if self.verbose { "expanded" } else { "folded" };
                self.note(format!(
                    "Verbose mode {}: tool steps stay {state}",
                    if self.verbose { "on" } else { "off" }
                ));
            }
            "mode" => match rest.first().copied() {
                Some("ask") => self.set_mode("ask", d),
                Some("auto") => self.set_mode("auto", d),
                Some("full") => self.set_mode("full", d),
                Some(other) => {
                    self.note(format!(
                        "Unknown mode '{other}'. Choose: ask, auto, or full"
                    ));
                }
                None => self.cycle_mode(d),
            },
            "session" => {
                let project = self.project.clone();
                self.show_panel(Panel::None);
                self.request(d, Awaiting::Sessions, |id| Command::SessionList {
                    id,
                    project: Some(project),
                });
            }
            "resume" => {
                let target = rest
                    .first()
                    .and_then(|s| s.trim_start_matches('#').parse::<i64>().ok());
                let Some(target_session) = target else {
                    self.note(self.usage_tip("resume"));
                    self.input = "/resume ".into();
                    self.cursor = self.input.len();
                    return;
                };
                if Some(target_session) == self.session {
                    self.note(format!("Already in session #{target_session}"));
                    return;
                }
                if target_session <= 0 || rest.len() != 1 {
                    self.note("Usage: /resume <positive session_id>");
                    return;
                }
                self.pending_session_switch = Some(target_session);
            }
            "logs" => {
                use runeengine::proto::LogLevel;
                let floor = match rest.first().copied() {
                    None | Some("all") => LogLevel::Info,
                    Some("warn" | "warnings") => LogLevel::Warn,
                    Some("error" | "errors") => LogLevel::Error,
                    Some(_) => {
                        self.note(self.usage_tip("logs"));
                        return;
                    }
                };
                // Like ctrl+t: the same command closes what it opened.
                if self.panel == Panel::Logs && rest.is_empty() {
                    self.show_panel(Panel::None);
                    return;
                }
                self.open_logs(d, floor);
            }
            "context" => {
                // Like /logs: the same command closes what it opened.
                if self.panel == Panel::Context {
                    self.show_panel(Panel::None);
                    return;
                }
                self.open_context(d);
            }
            "undo" => {
                if self.panel == Panel::Undo {
                    self.show_panel(Panel::None);
                    return;
                }
                self.open_undo(d);
            }
            "redo" => {
                if self.panel == Panel::Redo {
                    self.show_panel(Panel::None);
                    return;
                }
                self.open_redo(d);
            }
            "compact" => {
                let Some(session) = self.session else {
                    self.note("An unsent draft has no conversation history to compact.");
                    return;
                };
                self.note(if self.busy {
                    "Compaction queued: waiting for current API call to complete…"
                } else {
                    "Compacting session history…"
                });
                self.request(d, Awaiting::Compact, |id| Command::SessionCompact {
                    id,
                    session,
                });
            }
            "new" => {
                if self.session.is_none() {
                    self.note("Already in a project draft");
                    return;
                }
                if !self.sync_composer(d, true) {
                    return;
                }
                self.note("Starting project draft…");
                let project = self.project.clone();
                self.request(d, Awaiting::NewSession, |id| Command::DraftOpen {
                    id,
                    project,
                    token: None,
                });
            }
            "set" => {
                let pair = rest.join(" ");
                match pair.split_once('=') {
                    Some((k, v)) => {
                        let k = k.trim();
                        let v = v.trim();
                        settings::apply(self, k, v, d)
                    }
                    None => {
                        self.note(self.usage_tip("set"));
                        self.input = "/set ".into();
                        self.cursor = self.input.len();
                    }
                }
            }
            "theme" => match rest.first().copied() {
                None => {
                    let next = self.theme.next();
                    self.set_theme(next);
                    self.note(format!(
                        "Theme: {} (options: violet, cyan, amber)",
                        next.label()
                    ));
                }
                Some("prev" | "previous" | "back") => {
                    let prev = self.theme.prev();
                    self.set_theme(prev);
                    self.note(format!(
                        "Theme: {} (options: violet, cyan, amber)",
                        prev.label()
                    ));
                }
                Some(name) => match super::theme::ThemeMode::parse(name) {
                    Some(m) => {
                        self.set_theme(m);
                        self.note(format!("Theme: {}", m.label()));
                    }
                    None => {
                        self.note("unknown theme: choose violet, cyan, or amber");
                    }
                },
            },
            "quit" | "exit" => self.quit = true,
            other => self.note(format!("Unknown command: /{other} — try /help")),
        }
    }

    /// Cycle thinking state across all supported effort levels or toggle if effort not supported.
    pub fn cycle_thinking(&mut self, d: &mut Driver) {
        if self.is_suppressed("thinking") && self.is_suppressed("effort") {
            self.note("Thinking is not supported by the active model or provider.");
            return;
        }
        let effort_suppressed = self.is_suppressed("effort");
        let thinking_required = self.settings["thinking_required"].as_bool() == Some(true);
        let raw_choices: Vec<String> = self.settings["knobs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|k| k["key"].as_str() == Some("effort"))
            .and_then(|k| k["choices"].as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let real_effort_count = raw_choices
            .iter()
            .filter(|s| *s != "off" && *s != "none" && *s != "disabled")
            .count();
        if effort_suppressed || raw_choices.is_empty() || real_effort_count == 0 {
            if self.thinking() {
                if thinking_required {
                    self.note("Thinking is mandatory for this model and cannot be turned off.");
                    return;
                }
                settings::apply(self, "thinking", "false", d);
                self.settings["settings"]["thinking"] = serde_json::json!(false);
                self.settings["settings"]["effort"] = serde_json::Value::Null;
            } else {
                settings::apply(self, "thinking", "true", d);
                self.settings["settings"]["thinking"] = serde_json::json!(true);
            }
            return;
        }

        let choices = raw_choices;

        let curr = self.effort_choice(&choices);

        let at = choices
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&curr))
            .unwrap_or(0);
        let next_idx = (at + 1) % choices.len();
        let next_val = &choices[next_idx];

        if next_val.eq_ignore_ascii_case("off") {
            settings::apply(self, "effort", "off", d);
            self.settings["settings"]["thinking"] = serde_json::json!(false);
            self.settings["settings"]["effort"] = serde_json::Value::Null;
        } else if next_val == super::ROUTE_DEFAULT {
            settings::apply(self, "effort", next_val, d);
            self.settings["settings"]["thinking"] = serde_json::json!(true);
            self.settings["settings"]["effort"] = serde_json::Value::Null;
        } else {
            settings::apply(self, "effort", next_val, d);
            self.settings["settings"]["thinking"] = serde_json::json!(true);
            self.settings["settings"]["effort"] = serde_json::json!(next_val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_driver;
    use std::time::Instant;

    /// The owner reports a 30-50ms per-keystroke delay. This probe types 20
    /// characters through the real engine and asserts the whole batch stays
    /// well under interactive latency: any single keystroke above ~15ms means
    /// something on the path is touching disk or the lock.
    #[test]
    fn typing_twenty_characters_stays_under_interactive_latency() {
        let mut d = test_driver("typing-latency");
        let mut app = App::for_test();
        app.composition.ready = true;
        let started = Instant::now();
        for _ in 0..20 {
            app.on_key(Key::Char('a'), &mut d);
        }
        let per_key = started.elapsed().as_secs_f64() * 1000.0 / 20.0;
        eprintln!("per-keystroke: {per_key:.3}ms");
        assert_eq!(app.input, "a".repeat(20));
        assert!(
            per_key < 15.0,
            "typing took {per_key:.2}ms per key — above interactive latency"
        );
    }

    /// Build the real ask-user modal the way the engine sends it — not a
    /// hand-made fixture, so the test drives the screen the user actually gets.
    fn ask(app: &mut App, questions: serde_json::Value) {
        app.on_event(runeengine::Event::QuestionsRequest {
            session: app.session.unwrap(),
            id: "tool1".into(),
            req: "p1".into(),
            action: "Preferences".into(),
            questions: serde_json::from_value(questions).unwrap(),
        });
    }

    /// While the ask-user modal is up, the conversation behind it must stay
    /// reachable: the question is *about* that conversation.
    ///
    /// The old dispatch handed the modal **every** key and returned, so a
    /// question that needed the chat re-read could not be answered without
    /// cancelling it first.
    #[test]
    fn the_transcript_scrolls_while_the_question_modal_is_up() {
        let mut d = test_driver("questions-scroll");
        let mut app = App::for_test();
        for i in 0..60 {
            app.blocks.push(Block::Text {
                text: format!("line {i} of the conversation"),
            });
        }
        ask(
            &mut app,
            serde_json::json!([{
                "id": "q", "title": "Pick one", "question": "Which one?", "type": "single",
                "options": [
                    { "id": "a", "label": "Alpha", "description": "the first one" },
                    { "id": "b", "label": "Beta", "description": "the second one" },
                ]
            }]),
        );
        // One real frame, so the modal learns its own geometry and the
        // transcript has scrolled rows to move through.
        let _ = crate::ui::draw::compose_for_test(&mut app, 32, 100);
        assert!(
            app.questions.as_ref().is_some_and(|q| !q.overflows()),
            "this fixture must fit, or the test is about the other branch"
        );

        let before = app.scroll;
        app.on_key(Key::PageUp, &mut d);
        assert!(
            app.scroll > before,
            "PageUp did not scroll the transcript: {before} -> {}",
            app.scroll
        );
        let up = app.scroll;

        app.on_key(Key::PageDown, &mut d);
        assert!(
            app.scroll < up,
            "PageDown did not scroll back: {up} -> {}",
            app.scroll
        );

        // Scrolling is not a way out of the question.
        assert!(app.questions.is_some());
        d.engine.shutdown();
    }

    #[test]
    fn ctrl_g_toggles_settings_from_questions_modal_and_restores_it() {
        let mut d = test_driver("questions-ctrl-g");
        let mut app = App::for_test();
        ask(
            &mut app,
            serde_json::json!([{
                "id": "q", "title": "Pick one", "question": "Which one?", "type": "single",
                "options": [
                    { "id": "a", "label": "Alpha", "description": "the first one" },
                    { "id": "b", "label": "Beta", "description": "the second one" },
                ]
            }]),
        );
        assert!(app.questions.is_some());
        assert_eq!(app.panel, Panel::None);

        // Press Ctrl+G while questions modal is up: opens Settings
        app.on_key(Key::Ctrl('g'), &mut d);
        assert_eq!(app.panel, Panel::Settings);
        assert!(app.questions.is_some());

        // Press Esc in Settings: closes Settings, restores view without losing questions modal
        app.on_key(Key::Esc, &mut d);
        assert_eq!(app.panel, Panel::None);
        assert!(app.questions.is_some());

        d.engine.shutdown();
    }

    #[test]
    fn ctrl_g_toggles_settings_from_permission_card_and_restores_it() {
        let mut d = test_driver("permission-ctrl-g");
        let mut app = App::for_test();
        app.on_event(runeengine::Event::PermissionRequest {
            session: app.session.unwrap(),
            tool: "delete_file".into(),
            req: "req-1".into(),
            preview: "delete file".into(),
            remember_as: None,
            reach: vec![],
            sensitive: vec![],
            full_access: false,
            reason: None,
            action: "delete".into(),
        });
        assert!(app.pending.is_some());
        assert_eq!(app.panel, Panel::None);

        // Press Ctrl+G while permission card is up: opens Settings
        app.on_key(Key::Ctrl('g'), &mut d);
        assert_eq!(app.panel, Panel::Settings);
        assert!(app.pending.is_some());

        // Press Esc in Settings: closes Settings, restores view with permission card intact
        app.on_key(Key::Esc, &mut d);
        assert_eq!(app.panel, Panel::None);
        assert!(app.pending.is_some());

        d.engine.shutdown();
    }

    #[test]
    fn new_session_while_busy_cancels_active_turn() {
        let mut d = test_driver("new-cancels-busy");
        let mut app = App::for_test();
        app.busy = true;
        app.input = "/new".into();
        app.on_key(Key::Enter, &mut d);
        assert!(app
            .awaiting
            .values()
            .any(|a| matches!(a, Awaiting::NewSession)));
        d.engine.shutdown();
    }

    /// The wheel behaves the same way: it moves the transcript when the modal
    /// has nothing of its own to scroll.
    #[test]
    fn the_wheel_scrolls_the_transcript_behind_a_short_modal() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("questions-wheel");
        let mut app = App::for_test();
        for i in 0..60 {
            app.blocks.push(Block::Text {
                text: format!("line {i}"),
            });
        }
        ask(
            &mut app,
            serde_json::json!([{
                "id": "q", "title": "Pick one", "question": "Which one?", "type": "single",
                "options": [
                    { "id": "a", "label": "Alpha", "description": "the first one" },
                    { "id": "b", "label": "Beta", "description": "the second one" },
                ]
            }]),
        );
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 100);
        let before = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(
            app.scroll > before,
            "the wheel did not reach the transcript"
        );
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                row: 5,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(
            app.scroll, before,
            "the wheel came back to where it started"
        );
        d.engine.shutdown();
    }

    /// The rule the whole side-panel scroll hangs on: **the wheel acts on
    /// the thing under the pointer; a thing that scrolls owns the whole
    /// gesture, spent ends included.**
    ///
    /// Three cases, and the third is the one that is easy to get wrong.
    #[test]
    fn the_wheel_follows_the_pointer_and_a_spent_plan_swallows_the_wheel() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("wheel-routing");
        let mut app = App::for_test();
        app.sidebar = true;
        app.tasks = (0..30)
            .map(|n| serde_json::json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        // One real frame, so the panel publishes its column and its range.
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 120);
        // The routing is under test, not the length of the transcript: give it
        // a long one to scroll.
        app.drawn_rows = 400;
        let (first, last) = app.sidebar_cols.expect("the split is on at 120 columns");
        let inside = first + 1;
        let outside = first - 1;
        assert!(inside <= last);
        let max = app.plan_max;
        assert!(max > 0, "a 30-item plan in 24 rows must be scrollable");

        // 1. Over the plan, and the plan can go up: the plan moves, the chat does not.
        app.plan_scroll = max;
        let chat = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: inside,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(
            app.plan_scroll < max,
            "the wheel over the plan did not scroll it"
        );
        assert_eq!(app.scroll, chat, "the transcript moved as well");

        // 2. Over the transcript: the chat moves and the plan does not.
        let plan = app.plan_scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: outside,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(
            app.plan_scroll, plan,
            "the wheel over the chat scrolled the plan"
        );
        assert!(app.scroll > chat, "the wheel over the chat did nothing");

        // 3. Over the plan at the very top: the plan cannot move, and the
        // wheel dies there. The chat beside it must not stir — the owner's
        // rule of 27 Sep: once the plan scrolls, a wheel addressed to the
        // panel never reaches the transcript, ends included. A spent wheel is
        // a silent wheel, not a stray hand on the reader's chat.
        app.plan_scroll = 0;
        let chat = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: inside,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.plan_scroll, 0, "the plan scrolled past its own top");
        assert_eq!(
            app.scroll, chat,
            "a spent plan dragged the transcript along"
        );

        // 4. The same rule at the bottom: spent downward, the wheel dies
        // there too. "Mentok" is not a direction the panel can hand off.
        app.plan_scroll = max;
        let chat = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                row: 5,
                col: inside,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.plan_scroll, max, "the plan scrolled past its own end");
        assert_eq!(
            app.scroll, chat,
            "a spent plan dragged the transcript along"
        );
        d.engine.shutdown();
    }

    /// A plan that fits entirely must leave the wheel to the transcript: a panel
    /// with nothing to hide has nothing to do with the gesture.
    #[test]
    fn a_plan_that_fits_never_takes_the_wheel() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("wheel-fits");
        let mut app = App::for_test();
        app.sidebar = true;
        app.tasks = vec![serde_json::json!({ "text": "one", "status": "todo" })];
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 120);
        // The routing is under test, not the length of the transcript: give it
        // a long one to scroll.
        app.drawn_rows = 400;
        let (first, _) = app.sidebar_cols.expect("the split is on at 120 columns");
        assert_eq!(app.plan_max, 0);
        let chat = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: first + 1,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.plan_scroll, 0);
        assert!(
            app.scroll > chat,
            "a plan that fits stole the transcript's wheel"
        );
        // Both directions: the chat the pointer stands over — beside a panel
        // with nothing to hide — scrolls up and down alike.
        let after_up = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                row: 5,
                col: first + 1,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.plan_scroll, 0);
        assert!(
            app.scroll < after_up,
            "a fitting plan blocked the transcript's wheel on the way down"
        );
        d.engine.shutdown();
    }

    /// Esc closes the side panel through the door Ctrl+O uses — the one that
    /// also writes the choice down — not by flipping this window's flag, which
    /// the next change of `rune.toml` would silently undo.
    #[test]
    fn escape_hides_the_side_panel_through_the_shared_door() {
        let mut d = test_driver("esc-sidebar");
        let mut app = App::for_test();
        app.sidebar = true;
        app.resync = false;
        app.on_key(Key::Esc, &mut d);
        assert!(!app.sidebar);
        assert!(app.resync, "set_sidebar repaints the whole screen");
        d.engine.shutdown();
    }

    /// With the split off there is no panel on screen, so the wheel is the
    /// transcript's and a hidden plan is not scrolled.
    #[test]
    fn no_split_means_no_panel_to_scroll() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("wheel-nosplit");
        let mut app = App::for_test();
        app.sidebar = false;
        app.tasks = (0..30)
            .map(|n| serde_json::json!({ "text": format!("step {n}"), "status": "todo" }))
            .collect();
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 120);
        // The routing is under test, not the length of the transcript: give it
        // a long one to scroll.
        app.drawn_rows = 400;
        assert!(app.sidebar_cols.is_none());
        let chat = app.scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(
            app.scroll > chat,
            "with no split the wheel must still scroll the transcript"
        );
        d.engine.shutdown();
    }

    /// A modal whose list is taller than the room it was given keeps the page
    /// keys and the wheel for itself — a list that scrolls away from its own
    /// cursor is a list the user has to chase.
    #[test]
    fn a_modal_that_does_not_fit_keeps_its_own_scrolling() {
        let mut app = App::for_test();
        ask(
            &mut app,
            serde_json::json!([
                { "id": "q0", "title": "Q0", "question": "Which?", "type": "single",
                  "options": (0..4).map(|o| serde_json::json!({ "id": format!("o{o}"), "label": format!("Option {o}"), "description": "a description long enough to take a row" })).collect::<Vec<_>>() },
                { "id": "q1", "title": "Q1", "question": "Which?", "type": "single",
                  "options": (0..4).map(|o| serde_json::json!({ "id": format!("o{o}"), "label": format!("Option {o}"), "description": "a description long enough to take a row" })).collect::<Vec<_>>() },
            ]),
        );
        // A short terminal: the body cannot fit.
        let _ = crate::ui::draw::compose_for_test(&mut app, 12, 100);
        assert!(
            app.questions.as_ref().is_some_and(|q| q.overflows()),
            "the fixture must actually overflow, or it tests the wrong branch"
        );
    }

    /// The composer speaks one newline, and a paste arrives speaking whichever
    /// convention its source used. `\r\n` is one break — not two — and a bare
    /// `\r` is a break rather than noise. Before this the renderer dropped `\r`
    /// with nothing put in its place, which is exactly how a copied multi-line
    /// block collapsed onto one line with its whole structure gone.
    #[test]
    fn a_paste_keeps_its_line_structure_whatever_it_arrived_as() {
        for (name, pasted, expected) in [
            ("unix", "a\nb\nc", "a\nb\nc"),
            ("windows", "a\r\nb\r\nc", "a\nb\nc"),
            ("classic mac", "a\rb\rc", "a\nb\nc"),
            ("trailing break", "a\n\nb\n", "a\n\nb\n"),
            ("no break at all", "one long line", "one long line"),
        ] {
            let mut d = test_driver("paste-lines");
            let mut app = App::for_test();
            app.on_key(Key::Paste(pasted.into()), &mut d);
            assert_eq!(
                app.input, expected,
                "{name}: the paste was normalised wrong"
            );
            assert_eq!(
                app.cursor,
                expected.len(),
                "{name}: the caret lost the text"
            );
            d.engine.shutdown();
        }
    }

    /// A paste of a real multi-line block arrives in the composer as that block,
    /// and the model is sent the same bytes the user saw — not a flattened line.
    #[test]
    fn a_multiline_paste_reaches_the_model_with_its_formatting_intact() {
        let mut d = test_driver("paste-format");
        let mut app = App::for_test();
        app.on_key(
            Key::Paste("## Title\n\n- one\n- two\n\n```rs\nfn main() {}\n```\n".into()),
            &mut d,
        );
        let sent = app.input.clone();
        assert!(
            sent.contains("\n\n- one\n- two\n"),
            "structure was lost: {sent:?}"
        );
        assert!(
            sent.contains("```rs\nfn main() {}\n```"),
            "a fenced block was flattened: {sent:?}"
        );
        assert!(!sent.contains('\r'), "a carriage return reached the prompt");
        d.engine.shutdown();
    }

    /// `↑` walks the composer by **what is drawn**, one wrapped row at a time.
    ///
    /// The old version moved by `\n` alone, so a long line that the composer had
    /// wrapped across several rows was one row to the keyboard: `↑` skipped the
    /// rows the reader was looking at. The rows here are the renderer's own.
    #[test]
    fn up_and_down_walk_the_rows_the_composer_actually_drew() {
        let mut app = App::for_test();
        // A single logical line, wrapped by the composer at ten columns. Only
        // the renderer knows where those breaks fell.
        app.composer_width = 10;
        app.input = "aaaa bbbb cccc dddd".into();
        let rows = super::super::draw::compose_rows(&app.input, app.composer_width);
        assert!(rows.len() > 1, "the fixture must actually wrap: {rows:?}");

        // Start at the end of the last drawn row and walk up one row at a time.
        app.cursor = app.input.len();
        let last = rows.len() - 1;
        app.cursor = cursor_up(&app.input, app.cursor, app.composer_width).expect("a row above");
        let landed = rows
            .iter()
            .rposition(|(start, _)| *start <= app.cursor)
            .expect("the caret is on a row");
        assert_eq!(landed, last - 1, "↑ moved by more than one drawn row");

        // And back down returns to the row it came from.
        app.cursor = cursor_down(&app.input, app.cursor, app.composer_width).expect("a row below");
        let landed = rows
            .iter()
            .rposition(|(start, _)| *start <= app.cursor)
            .expect("the caret is on a row");
        assert_eq!(landed, last, "↓ did not come back to the row it left");
    }

    /// On the top drawn row `↑` reports "no row above", which is the caller's
    /// cue to fall through to prompt history instead — the same contract the
    /// old, newline-only version had for a one-line draft.
    #[test]
    fn a_long_composer_keeps_its_viewport_until_the_caret_crosses_an_edge() {
        let mut app = App::for_test();
        app.input = (0..20)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.len();
        let _ = super::super::draw::compose_for_test(&mut app, 14, 80);
        let bottom = app.composer_first_row;
        app.on_key(Key::Up, &mut test_driver("composer-viewport"));
        let _ = super::super::draw::compose_for_test(&mut app, 14, 80);
        assert_eq!(
            app.composer_first_row, bottom,
            "caret still visible must not scroll composer"
        );
        for _ in 0..8 {
            app.on_key(Key::Up, &mut test_driver("composer-viewport"));
        }
        let _ = super::super::draw::compose_for_test(&mut app, 14, 80);
        assert!(
            app.composer_first_row < bottom,
            "composer must scroll only once caret crosses its top edge"
        );
    }

    #[test]
    fn caret_keyboard_motion_uses_source_offsets_and_the_current_visual_column() {
        let text = "a\tb\n123456";
        assert_eq!(cursor_down(text, 3, 80), Some(text.len()));
        assert_eq!(cursor_up(text, text.len(), 80), Some(3));
        let text = "abcdefghijklmno";
        // The third wrapped row's column 2 is the second row's column 2,
        // not the logical line's column 12 clamped to its end.
        assert_eq!(cursor_up(text, 12, 5), Some(7));
        assert_eq!(cursor_down(text, 7, 5), Some(12));
    }

    #[test]
    fn up_leaves_the_top_row_to_prompt_history() {
        assert_eq!(cursor_up("one line only", 5, 80), None);
        assert_eq!(cursor_up("a\nb", 0, 80), None);
        assert_eq!(cursor_down("a\nb", 3, 80), None);
    }

    /// Type a run of keys into a screen, the way a person would.
    fn press(app: &mut App, d: &mut Driver, keys: &[Key]) {
        for key in keys {
            app.on_key(key.clone(), d);
        }
    }

    fn typed(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
    }

    fn mouse_click(app: &mut App, d: &mut Driver, row: u16, col: u16) {
        use crate::ui::input::{Click, Modifiers, Mouse};
        for kind in [Click::Press, Click::Release] {
            app.on_key(
                Key::Mouse(Mouse {
                    kind,
                    row,
                    col,
                    modifiers: Modifiers::NONE,
                }),
                d,
            );
        }
    }

    #[test]
    fn settings_mouse_routes_clicks_to_cards_tabs_back_and_editor_buttons() {
        use crate::ui::{
            draw::{self, Hit},
            settings::{Page, Target},
        };
        let mut app = App::for_test();
        let mut d = test_driver("settings-click-routing");
        app.panel = Panel::Settings;
        let frame = draw::compose_for_test(&mut app, 34, 120);
        let card = app
            .hits
            .iter()
            .position(|hit| matches!(hit, Hit::Settings(Target::Cards { .. } | Target::Row(_))))
            .unwrap();
        assert!(!ansi::plain(&frame[card]).is_empty());
        mouse_click(&mut app, &mut d, card as u16 + 1, 8);
        assert_ne!(app.preferences.location.page, Page::Overview);
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let back = app
            .hits
            .iter()
            .position(|hit| *hit == Hit::Settings(Target::Back))
            .unwrap();
        mouse_click(&mut app, &mut d, back as u16 + 1, 4);
        assert_eq!(app.preferences.location.page, Page::Overview);
        let frame = draw::compose_for_test(&mut app, 34, 120);
        let tab = app
            .hits
            .iter()
            .position(|hit| matches!(hit, Hit::Settings(Target::Tabs { .. })))
            .unwrap();
        let col = ansi::plain(&frame[tab]).find("Tools").unwrap() as u16 + 1;
        mouse_click(&mut app, &mut d, tab as u16 + 1, col);
        assert_eq!(app.preferences.location.page, Page::Tools);
        let rows = settings::rows(&app);
        let edit = rows
            .iter()
            .position(|row| matches!(row.kind, settings::Kind::Edit(_)))
            .unwrap();
        settings::open(&mut app, &rows[edit], &mut d);
        assert!(app.preferences.editor.is_some());
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let cancel = app
            .hits
            .iter()
            .position(|hit| *hit == Hit::Settings(Target::Cancel))
            .unwrap();
        mouse_click(&mut app, &mut d, cancel as u16 + 1, 8);
        assert!(app.preferences.editor.is_none());
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let back = app
            .hits
            .iter()
            .position(|hit| *hit == Hit::Settings(Target::Back))
            .unwrap();
        mouse_click(&mut app, &mut d, back as u16 + 1, 4);
        assert_eq!(app.preferences.location.page, Page::Overview);
        // A real drag still selects rather than opening a control.
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let card = app
            .hits
            .iter()
            .position(|hit| matches!(hit, Hit::Settings(Target::Cards { .. } | Target::Row(_))))
            .unwrap();
        use crate::ui::input::{Click, Modifiers, Mouse};
        for (kind, col) in [(Click::Press, 8), (Click::Drag, 30), (Click::Release, 30)] {
            app.on_key(
                Key::Mouse(Mouse {
                    kind,
                    row: card as u16 + 1,
                    col,
                    modifiers: Modifiers::NONE,
                }),
                &mut d,
            );
        }
        assert_eq!(app.preferences.location.page, Page::Overview);
        assert!(app
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("copied ")));
        d.engine.shutdown();
    }

    #[test]
    fn composer_mouse_places_caret_between_characters_and_at_clicked_line_end() {
        use crate::ui::draw::{self, Hit};
        let mut app = App::for_test();
        let mut d = test_driver("composer-click-caret");
        for (input, col, expected) in [
            ("1234", 7, 2),
            ("1\n2\n3\n", 60, 1),
            ("a界b", 8, 4),
            ("e\u{301}x", 6, 3),
            ("a\tb", 10, 2),
        ] {
            app.input = input.into();
            app.cursor = input.len();
            let _ = draw::compose_for_test(&mut app, 34, 120);
            let row = app.hits.iter().position(|hit| matches!(hit, Hit::ComposerText(index) if app.composer_hit_rows[*index].start == 0)).unwrap();
            mouse_click(&mut app, &mut d, row as u16 + 1, col);
            assert_eq!(app.cursor, expected, "{input:?}, col {col}");
            assert!(app.input.is_char_boundary(app.cursor));
            assert!(app.selection.is_none());
        }
        app.input = "1234".into();
        app.cursor = 4;
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let row = app
            .hits
            .iter()
            .position(|hit| matches!(hit, Hit::ComposerText(_)))
            .unwrap();
        mouse_click(&mut app, &mut d, row as u16 + 1, 7);
        app.on_key(Key::Char('X'), &mut d);
        assert_eq!(app.input, "12X34");
        // A scrolled and wrapped row uses the published source offset, not its
        // screen row or an independently recomputed width.
        app.input = "z".repeat(600);
        app.cursor = app.input.len();
        let _ = draw::compose_for_test(&mut app, 12, 40);
        let (row, start) = app
            .hits
            .iter()
            .enumerate()
            .find_map(|(row, hit)| match hit {
                Hit::ComposerText(index) => Some((row, app.composer_hit_rows[*index].start)),
                _ => None,
            })
            .unwrap();
        assert!(start > 0);
        mouse_click(&mut app, &mut d, row as u16 + 1, 7);
        assert_eq!(app.cursor, start + 2);
        // A soft wrap can drop one of a tab's displayed spaces. Clicking its
        // continuation must use the full row projection, not re-project a
        // source substring (which would insert four fresh spaces).
        app.input = format!("{}\tb", "a".repeat(12));
        app.cursor = app.input.len();
        let _ = draw::compose_for_test(&mut app, 34, 20);
        let index = app.composer_hit_rows.len() - 1;
        let expected = app.composer_hit_rows[index].caret_at(5);
        let row = app
            .hits
            .iter()
            .position(|hit| *hit == Hit::ComposerText(index))
            .unwrap();
        mouse_click(&mut app, &mut d, row as u16 + 1, 5);
        assert_eq!(app.cursor, expected);
        assert!(app.input.is_char_boundary(app.cursor));
        d.engine.shutdown();
    }

    #[test]
    fn mcp_mouse_submit_uses_the_same_validation_as_enter() {
        use crate::ui::{
            draw::{self, Hit},
            mcp_input::Form,
        };
        let mut app = App::for_test();
        let mut d = test_driver("mcp-click-submit");
        app.mcp_input = Some(Form::new(
            "req".into(),
            "server".into(),
            serde_json::json!({"mode":"form","requestedSchema":{"type":"object","properties":{"name":{"type":"string","minLength":2}},"required":["name"]}}),
        ));
        let _ = draw::compose_for_test(&mut app, 34, 120);
        let row = app
            .hits
            .iter()
            .position(|hit| matches!(hit, Hit::McpSubmit { .. }))
            .unwrap();
        mouse_click(&mut app, &mut d, row as u16 + 1, 40);
        assert!(app.mcp_input.as_ref().unwrap().error.is_none());
        mouse_click(&mut app, &mut d, row as u16 + 1, 5);
        assert!(app.mcp_input.as_ref().unwrap().error.is_some());
        assert!(app.mcp_input.as_ref().unwrap().sending.is_none());
        d.engine.shutdown();
    }

    #[test]
    fn a_sent_user_message_click_opens_and_refolds_without_changing_its_text() {
        use crate::ui::draw;
        let mut app = App::for_test();
        let mut d = test_driver("user-click-fold");
        let text = (1..=12)
            .map(|n| format!("message row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.blocks.push(Block::user(&text));
        let frame = draw::compose_for_test(&mut app, 40, 120);
        let row = frame
            .iter()
            .position(|line| ansi::plain(line).contains("8 lines cut"))
            .unwrap();
        mouse_click(&mut app, &mut d, row as u16 + 1, 20);
        assert!(
            matches!(&app.blocks[0], Block::User { open:true, text:stored,.. } if stored==&text)
        );
        let frame = draw::compose_for_test(&mut app, 40, 120);
        assert!(frame
            .iter()
            .any(|line| ansi::plain(line).contains("message row 12")));
        assert!(frame
            .iter()
            .all(|line| !ansi::plain(line).contains("lines cut")));
        let row = frame
            .iter()
            .position(|line| ansi::plain(line).contains("you"))
            .unwrap();
        mouse_click(&mut app, &mut d, row as u16 + 1, 20);
        assert!(matches!(&app.blocks[0], Block::User { open: false, .. }));
        d.engine.shutdown();
    }

    /// `/help` used to swallow every keystroke, so the composer went dead and
    /// the only way back to it was `esc`.
    #[test]
    fn a_question_can_be_typed_while_the_help_screen_is_still_open() {
        let mut d = test_driver("keys-help");
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/help"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.panel, Panel::Help);
        assert!(app.input.is_empty());

        press(&mut app, &mut d, &typed("what does this do"));
        assert_eq!(
            app.input, "what does this do",
            "the keyboard was still locked"
        );
        assert_eq!(
            app.panel,
            Panel::Help,
            "typing must not close what is being read"
        );

        // …and sending it puts the answer where the reading was.
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.panel, Panel::None);
    }

    /// The list answers to what is typed, and enter takes the row it is on
    /// without losing the arguments already written after it.
    #[test]
    fn the_command_list_completes_what_was_typed_and_keeps_its_arguments() {
        let mut d = test_driver("keys-palette");
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/verb"));
        assert!(app.searching());
        press(&mut app, &mut d, &[Key::Enter]);
        assert!(app.verbose, "/verbose did not run from four letters of it");

        // An argument survives the completion.
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/se thinking=true"));
        assert_eq!(app.chosen_command().as_deref(), Some("/set thinking=true"));

        // A row number from the old list cannot survive a keystroke that
        // rebuilds it — it would point at a different command, or at nothing.
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/s"));
        press(&mut app, &mut d, &[Key::Down]);
        assert_eq!(app.palette_row, 1);
        press(&mut app, &mut d, &typed("e"));
        assert_eq!(
            app.palette_row, 0,
            "the cursor stayed on a row that had moved"
        );

        // Prose is prose: Up at top row moves cursor to 0, never scrolls.
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("hello"));
        assert!(!app.searching());
        assert_eq!(app.cursor, 5);
        press(&mut app, &mut d, &[Key::Up]);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.scroll, 0, "arrows in prose never scroll");
    }

    #[test]
    fn tab_completes_the_command_under_the_cursor() {
        let mut d = test_driver("keys-tab");
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/verb"));
        press(&mut app, &mut d, &[Key::Tab]);
        assert_eq!(app.input, "/verbose");
    }

    #[test]
    fn question_mark_matches_help_and_submits_to_help_panel() {
        let mut d = test_driver("keys-question");
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("?"));
        assert!(
            app.searching(),
            "typing '?' must search and show help in command palette"
        );
        assert_eq!(matches(&app.input)[0].0, "help");

        // Submitting '?' directly opens Panel::Help
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.panel, Panel::Help);
        assert!(app.input.is_empty());
    }

    #[test]
    fn ctrl_c_leaves_instead_of_cancelling_an_active_turn_and_esc_stops_it() {
        let mut d = test_driver("ctrl-c-live-quit");
        let mut app = App::for_test();
        app.busy = true;
        app.panel = Panel::Settings;
        app.queued_prompt = Some(super::super::QueuedPrompt {
            steers: Vec::new(),
            first_user_block: 0,
        });
        app.on_key(Key::Ctrl('c'), &mut d);
        assert!(app.quit_armed && !app.quit);
        assert!(app.busy);
        assert!(!app.stop_armed);
        assert!(app.queued_prompt.is_some());
        assert_eq!(
            app.status.as_deref(),
            Some("press ctrl+c again to leave Rune · esc twice to stop AI")
        );
        app.on_key(Key::Ctrl('c'), &mut d);
        assert!(app.quit);
        assert!(
            app.queued_prompt.is_some(),
            "Ctrl+C must not cancel the turn"
        );

        let mut other = App::for_test();
        other.busy = true;
        other.on_key(Key::Esc, &mut d);
        assert!(other.stop_armed && !other.quit);
        other.on_key(Key::Esc, &mut d);
        assert_eq!(other.status.as_deref(), Some("stopping…"));
        assert!(!other.quit);
        d.engine.shutdown();
    }

    #[test]
    fn esc_closes_settings_without_arming_stop() {
        let mut d = test_driver("keys-esc-settings-busy");
        let mut app = App::for_test();
        app.busy = true;
        app.panel = Panel::Settings;
        app.on_key(Key::Esc, &mut d);
        assert_eq!(app.panel, Panel::None);
        assert!(!app.stop_armed);
        app.on_key(Key::Esc, &mut d);
        assert!(app.stop_armed);
        assert_ne!(app.status.as_deref(), Some("stopping…"));
        app.on_key(Key::Esc, &mut d);
        assert_eq!(app.status.as_deref(), Some("stopping…"));
        assert!(!app.stop_armed);
        d.engine.shutdown();
    }

    /// Esc, during a turn, requires double-press with 3s timeout to stop.
    #[test]
    fn esc_closes_open_panel_without_stopping_a_running_turn() {
        let mut d = test_driver("keys-esc");
        let mut app = App::for_test();
        app.busy = true;
        app.panel = Panel::Logs;
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.panel, Panel::None);
        assert!(!app.stop_armed);
        assert_ne!(app.status.as_deref(), Some("press esc again to stop"));

        // Same for Panel::Tasks (Plan screen)
        app.panel = Panel::Tasks;
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.panel, Panel::None);
        assert!(!app.stop_armed);

        // When in main screen without panel: Esc arms stop as expected
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.status.as_deref(), Some("press esc again to stop"));
        assert!(app.stop_armed);
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.status.as_deref(), Some("stopping…"));
    }

    #[test]
    fn esc_during_in_flight_tool_stops_it_and_ends_timing() {
        let mut d = test_driver("keys-esc-tool");
        let mut app = App::for_test();
        app.busy = true;
        let mut tool =
            super::super::blocks::Tool::new("c1".into(), "create_file".into(), "create it".into());
        tool.state = super::super::blocks::ToolState::Writing;
        app.blocks.push(super::super::blocks::Block::of(tool));

        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.status.as_deref(), Some("press esc again to stop"));
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.status.as_deref(), Some("stopping…"));
        let tool = app.blocks[0].tool_mut().unwrap();
        assert_eq!(tool.state, super::super::blocks::ToolState::Stopped);
        assert!(tool.ended.is_some());
    }

    #[test]
    fn esc_on_permission_card_replies_deny_and_stops_turn() {
        let mut d = test_driver("keys-esc-perm");
        let mut app = App::for_test();
        app.busy = true;
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "write".into(),
            preview: "create_file".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let mut tool =
            super::super::blocks::Tool::new("c1".into(), "create_file".into(), "write".into());
        tool.state = super::super::blocks::ToolState::Asking;
        app.blocks.push(super::super::blocks::Block::of(tool));

        press(&mut app, &mut d, &[Key::Esc]);
        assert!(
            app.pending.is_some(),
            "first Esc does not answer or cancel the question"
        );
        assert_eq!(app.status.as_deref(), Some("press esc again to stop"));
        press(&mut app, &mut d, &[Key::Esc]);
        assert_eq!(app.status.as_deref(), Some("stopping…"));
        let tool = app.blocks[0].tool_mut().unwrap();
        assert_eq!(tool.state, super::super::blocks::ToolState::Stopped);
        assert!(tool.ended.is_some());
    }

    /// The permission card owns the keyboard, answers to the arrows, and lets
    /// `Ctrl` past so the turn can still be stopped outright.
    #[test]
    fn page_keys_scroll_the_transcript_behind_a_permission_card() {
        let mut d = test_driver("permission-scroll");
        let mut app = App::for_test();
        app.busy = true;
        for n in 0..60 {
            app.blocks.push(super::super::blocks::Block::Text {
                text: format!("conversation row {n}"),
            });
        }
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "run".into(),
            preview: "$ cargo test".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let _ = super::super::draw::compose_for_test(&mut app, 24, 100);
        let before = app.scroll;
        app.on_key(Key::PageUp, &mut d);
        assert!(
            app.scroll > before,
            "permission card trapped transcript PageUp"
        );
        let up = app.scroll;
        app.on_key(Key::PageDown, &mut d);
        assert!(
            app.scroll < up,
            "permission card trapped transcript PageDown"
        );
        assert!(
            app.pending.is_some(),
            "scrolling must not answer permission"
        );
        d.engine.shutdown();
    }

    /// The permission card used to claim the whole `Key::Mouse` family, so the
    /// wheel and the click its own hint advertises ("click an answer") were both
    /// dead exactly when a person most needs to re-read what they are allowing.
    #[test]
    fn the_wheel_and_the_arrows_reach_the_transcript_behind_a_permission_card() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("permission-wheel");
        let mut app = App::for_test();
        app.busy = true;
        for n in 0..60 {
            app.blocks.push(super::super::blocks::Block::Text {
                text: format!("conversation row {n}"),
            });
        }
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "run".into(),
            preview: "$ cargo test".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let _ = super::super::draw::compose_for_test(&mut app, 24, 100);

        let before = app.scroll;
        app.on_key(Key::Up, &mut d);
        assert!(
            app.scroll > before,
            "the arrows did not reach the transcript behind a permission card"
        );
        let up = app.scroll;
        app.on_key(Key::Down, &mut d);
        assert!(app.scroll < up, "the arrows did not come back");

        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 5,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(
            app.scroll > up.saturating_sub(1),
            "the wheel did not reach the transcript behind a permission card: {}",
            app.scroll
        );
        assert!(
            app.pending.is_some(),
            "scrolling behind a permission card must not answer it"
        );
        d.engine.shutdown();
    }

    #[test]
    fn an_overflowing_permission_owns_only_the_wheel_over_its_own_rows() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("permission-viewport-wheel");
        let mut app = App::for_test();
        app.busy = true;
        app.sidebar = true;
        for n in 0..60 {
            app.blocks.push(Block::Text {
                text: format!("conversation row {n}"),
            });
        }
        app.pending = Some(super::super::blocks::Ask {
            req: "r".into(),
            action: "run".into(),
            preview: "command row\n".repeat(200),
            remember_as: Some("shell".into()),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        super::super::draw::compose_for_test(&mut app, 24, 150);
        let (first, _) = app.permission.card.unwrap();
        let chat = app.scroll;
        let panel = app.plan_scroll;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                row: first + 1,
                col: 145,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.permission.scroll, 3);
        assert_eq!(app.scroll, chat);
        assert_eq!(app.plan_scroll, panel);
        app.permission.scroll = app.permission.max;
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelDown,
                row: first + 1,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert_eq!(app.scroll, chat, "a spent card wheel leaked to the chat");
        app.on_key(Key::PageUp, &mut d);
        assert!(app.permission.scroll < app.permission.max);
        assert_eq!(app.scroll, chat);
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::WheelUp,
                row: 2,
                col: 5,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(app.scroll > chat, "the transcript's wheel was trapped");
        assert!(app.pending.is_some());
        d.engine.shutdown();
    }

    /// And the click the hint promises actually lands on an answer.
    #[test]
    fn clicking_an_answer_on_the_permission_card_answers_it() {
        let mut d = test_driver("permission-click");
        let mut app = App::for_test();
        app.busy = true;
        app.pending = Some(super::super::blocks::Ask {
            req: "r2".into(),
            action: "run the tests".into(),
            preview: "npm test".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });
        let frame = super::super::draw::compose_for_test(&mut app, 24, 100);
        let row = frame
            .iter()
            .position(|line| ansi::plain(line).contains("Allow"))
            .expect("the permission card must offer an answer");
        let col = ansi::plain(&frame[row]).find("Allow").unwrap() as u16;

        app.on_key(
            Key::Mouse(crate::ui::input::Mouse {
                kind: crate::ui::input::Click::Press,
                row: row as u16 + 1,
                col,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        app.on_key(
            Key::Mouse(crate::ui::input::Mouse {
                kind: crate::ui::input::Click::Release,
                row: row as u16 + 1,
                col,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        assert!(
            app.pending.is_none(),
            "a click on an answer did not answer the permission"
        );
        d.engine.shutdown();
    }

    #[test]
    fn a_permission_question_is_answered_by_walking_the_row() {
        let mut d = test_driver("keys-ask");
        let mut app = App::for_test();
        app.busy = true;
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "run the tests".into(),
            preview: "npm test".into(),
            remember_as: Some("npm".into()),
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });

        // Letters no longer answer it, and nothing they do reaches the composer.
        press(&mut app, &mut d, &typed("yan"));
        assert!(
            app.pending.is_some(),
            "a letter answered a permission question"
        );
        assert!(
            app.input.is_empty(),
            "the card let a keystroke through to the composer"
        );

        app.input = "keep this draft".into();
        app.cursor = 4;
        for (_, key, _) in super::super::editing::BINDINGS {
            app.on_key(key.clone(), &mut d);
            assert_eq!(app.input, "keep this draft", "permission leaked {key:?}");
            assert_eq!(app.cursor, 4, "permission moved hidden caret {key:?}");
            assert!(app.pending.is_some());
        }
        app.ask_choice = 0;
        press(&mut app, &mut d, &[Key::Right, Key::Right, Key::Right]);
        assert_eq!(app.ask_choice, 2, "the cursor ran off the end of the row");
        press(&mut app, &mut d, &[Key::Left]);
        assert_eq!(app.ask_choice, 1);

        press(&mut app, &mut d, &[Key::Enter]);
        assert!(app.pending.is_none(), "enter did not answer it");
    }
    /// Enter sends; every modified Enter breaks the line. Both halves matter:
    /// a composer that sent on `alt+enter` would send half a thought.
    #[test]
    fn alt_enter_breaks_the_line_and_enter_sends_it() {
        let mut d = crate::ui::test_driver("newline");
        // With a model, because a composer with none refuses to send at all and
        // the refusal would be what this test was measuring.
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        for c in "one".chars() {
            app.on_key(Key::Char(c), &mut d);
        }
        app.on_key(Key::NewLine, &mut d);
        for c in "two".chars() {
            app.on_key(Key::Char(c), &mut d);
        }
        assert_eq!(app.input, "one\ntwo");
        app.on_key(Key::Enter, &mut d);
        assert!(app.input.is_empty(), "enter did not send the question");
        d.engine.shutdown();
    }

    /// The readline keys every other prompt in a terminal already answers to.
    #[test]
    fn the_composer_answers_to_the_keys_a_shell_does() {
        let mut d = crate::ui::test_driver("readline");
        let mut app = App::for_test();
        for c in "one two three".chars() {
            app.on_key(Key::Char(c), &mut d);
        }
        app.on_key(Key::Ctrl('w'), &mut d);
        assert_eq!(app.input, "one two ");
        app.on_key(Key::Ctrl('u'), &mut d);
        assert_eq!(app.input, "");
        for c in "keep this".chars() {
            app.on_key(Key::Char(c), &mut d);
        }
        app.on_key(Key::Ctrl('a'), &mut d);
        assert_eq!(app.cursor, 0);
        app.on_key(Key::Ctrl('k'), &mut d);
        assert_eq!(app.input, "", "ctrl+k did not kill the line");

        // Forward kill preserves the prefix and logical newline. A second
        // kill at that newline joins exactly the following line.
        app.input = "line 1\nhello world\nline 3".into();
        app.cursor = 13;
        app.on_key(Key::Ctrl('k'), &mut d);
        assert_eq!(app.input, "line 1\nhello \nline 3");
        assert_eq!(app.cursor, 13);
        app.on_key(Key::Ctrl('k'), &mut d);
        assert_eq!(app.input, "line 1\nhello line 3");
        assert_eq!(app.cursor, 13);
        app.input = "aaa\nbbb".into();
        app.cursor = 5;
        app.on_key(Key::Ctrl('k'), &mut d);
        assert_eq!(app.input, "aaa\nb");
        assert_eq!(app.cursor, 5);
        app.on_key(Key::Ctrl('k'), &mut d);
        assert_eq!(app.input, "aaa\nb");

        d.engine.shutdown();
    }

    /// `Ctrl+C` asks to leave without editing the draft; `Esc` alone stops a turn.
    #[test]
    fn ctrl_c_never_touches_the_composer_and_leaves_with_a_draft() {
        let mut d = crate::ui::test_driver("ctrl-c");
        let mut app = App::for_test();
        app.input = "keep me\nsecond line".into();
        app.cursor = 3;

        // The first press on a line that holds text keeps every byte of it.
        app.on_key(Key::Ctrl('c'), &mut d);
        assert_eq!(app.input, "keep me\nsecond line", "ctrl+c erased the input");
        assert_eq!(app.cursor, 3, "ctrl+c moved the caret");
        assert!(
            app.quit_armed && !app.quit,
            "first press only arms the leave"
        );
        app.on_key(Key::Ctrl('c'), &mut d);
        assert!(app.quit, "the second press leaves even with a draft");
        assert_eq!(app.input, "keep me\nsecond line");
        assert_eq!(app.cursor, 3);

        d.engine.shutdown();
    }

    /// A thought opens where it was pressed, and only that one.
    #[test]
    fn a_thought_opens_and_closes_under_the_pointer() {
        let mut app = App::for_test();
        app.blocks.push(Block::Thinking {
            text: "a b c".into(),
            open: false,
        });
        app.blocks.push(Block::Thinking {
            text: "d e f".into(),
            open: false,
        });
        app.open_block(0);
        assert!(matches!(app.blocks[0], Block::Thinking { open: true, .. }));
        assert!(
            matches!(app.blocks[1], Block::Thinking { open: false, .. }),
            "both opened at once"
        );
        app.open_block(0);
        assert!(matches!(app.blocks[0], Block::Thinking { open: false, .. }));
    }

    /// The plan's own row says `… 4 tasks cut. Click to open all`. It used to say
    /// "click" while `open_block` deliberately skipped `track_tasks`, so the
    /// gesture the screen named did nothing at all.
    ///
    /// This drives the whole path: a real composed frame, the hit map it left
    /// behind, and a click on the row the text is actually on.
    #[test]
    fn the_promised_click_opens_the_plan() {
        use crate::ui::input::{Click, Mouse};
        let mut d = test_driver("plan-open-all");
        let mut app = App::for_test();
        let mut plan =
            crate::ui::blocks::Tool::new("t1".into(), "track_tasks".into(), "plan".into());
        plan.settle(&serde_json::json!({
            "tasks": (0..12).map(|i| serde_json::json!({ "text": format!("task {i}"), "status": "todo" })).collect::<Vec<_>>()
        }));
        plan.state = crate::ui::blocks::ToolState::Ok;
        app.blocks.push(Block::of(plan));
        assert!(!matches!(&app.blocks[0], Block::Tool(t) if t.open));

        // A real frame, so the hit map is the one the user is clicking on.
        let frame = crate::ui::draw::compose_for_test(&mut app, 24, 100);
        let row = frame
            .iter()
            .position(|line| crate::ui::ansi::plain(line).contains("Click to open all"))
            .expect("the cut row must be on screen") as u16
            + 1;

        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Press,
                row,
                col: 10,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );
        app.on_key(
            Key::Mouse(Mouse {
                kind: Click::Release,
                row,
                col: 10,
                modifiers: crate::ui::input::Modifiers::NONE,
            }),
            &mut d,
        );

        assert!(
            matches!(&app.blocks[0], Block::Tool(t) if t.open),
            "the row says 'Click to open all' and clicking it opened nothing"
        );
        d.engine.shutdown();
    }

    /// The plan's own row says `… 4 tasks cut. Click to open all`. It used to say
    /// "click" while `open_block` deliberately skipped `track_tasks`, so the
    /// gesture the screen named did nothing at all.
    #[test]
    fn the_plan_opens_and_closes_where_it_was_clicked() {
        let mut app = App::for_test();
        let mut plan =
            crate::ui::blocks::Tool::new("t1".into(), "track_tasks".into(), "plan".into());
        plan.settle(&serde_json::json!({
            "tasks": (0..12).map(|i| serde_json::json!({ "text": format!("task {i}"), "status": "todo" })).collect::<Vec<_>>()
        }));
        plan.state = crate::ui::blocks::ToolState::Ok;
        assert!(!plan.open, "a plan step starts folded");
        app.blocks.push(Block::of(plan));

        app.open_block(0);
        assert!(
            matches!(&app.blocks[0], Block::Tool(tool) if tool.open),
            "'Click to open all' clicked and nothing opened"
        );
        app.open_block(0);
        assert!(
            matches!(&app.blocks[0], Block::Tool(tool) if !tool.open),
            "a second click must fold it back"
        );
    }

    /// `ctrl+r` was printed in `/help` — "open every folded step and every
    /// thought" — and answered by nothing. A shortcut written in the one place a
    /// person goes to learn the shortcuts, doing nothing.
    #[test]
    fn ctrl_r_opens_every_step_and_thought_and_closes_them_again() {
        let mut d = test_driver("ctrl-r");
        let mut app = App::for_test();
        app.blocks.push(Block::Thinking {
            text: "thinking".into(),
            open: false,
        });
        for (name, state) in [
            ("inspect_file", crate::ui::blocks::ToolState::Ok),
            ("create_file", crate::ui::blocks::ToolState::Ok),
        ] {
            let mut tool = crate::ui::blocks::Tool::new(name.into(), name.into(), "work".into());
            tool.state = state;
            app.blocks.push(Block::of(tool));
        }

        app.on_key(Key::Ctrl('r'), &mut d);
        assert!(
            matches!(&app.blocks[0], Block::Thinking { open: true, .. }),
            "the thought stayed folded"
        );
        assert!(
            app.blocks[1..]
                .iter()
                .all(|b| matches!(b, Block::Tool(t) if t.open)),
            "a step stayed folded"
        );

        app.on_key(Key::Ctrl('r'), &mut d);
        assert!(
            matches!(&app.blocks[0], Block::Thinking { open: false, .. }),
            "the second press did not fold it back"
        );
        assert!(
            app.blocks[1..]
                .iter()
                .all(|b| matches!(b, Block::Tool(t) if !t.open)),
            "a step stayed open on the second press"
        );
        d.engine.shutdown();
    }

    /// A step still streaming has nothing folded to reveal, and forcing it open
    /// would show a half-written diff. `ctrl+r` leaves it alone.
    #[test]
    fn ctrl_r_does_not_force_open_a_step_still_running() {
        let mut d = test_driver("ctrl-r-running");
        let mut app = App::for_test();
        let mut running =
            crate::ui::blocks::Tool::new("c1".into(), "create_file".into(), "writing".into());
        running.state = crate::ui::blocks::ToolState::Running;
        app.blocks.push(Block::of(running));

        app.on_key(Key::Ctrl('r'), &mut d);
        assert!(
            matches!(&app.blocks[0], Block::Tool(t) if !t.open),
            "a running step was forced open"
        );
        d.engine.shutdown();
    }

    #[test]
    fn a_drag_selects_without_firing_the_click_target_under_it() {
        use crate::ui::input::{Click, Mouse};

        let mut d = crate::ui::test_driver("select-click");
        let mut app = App::for_test();
        app.blocks.push(Block::of(crate::ui::blocks::Tool::new(
            "c1".into(),
            "inspect_file".into(),
            "read it".into(),
        )));
        app.hits = vec![crate::ui::draw::Hit::Block(0)];
        app.selection_rows = vec!["select this text".into()];

        app.on_mouse(
            Mouse {
                kind: Click::Press,
                row: 1,
                col: 1,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Release,
                row: 1,
                col: 1,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        assert!(matches!(&app.blocks[0], Block::Tool(tool) if tool.open));

        app.on_mouse(
            Mouse {
                kind: Click::Press,
                row: 1,
                col: 1,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Drag,
                row: 1,
                col: 6,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Release,
                row: 1,
                col: 6,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        assert!(
            matches!(&app.blocks[0], Block::Tool(tool) if tool.open),
            "selection also fired the click handler"
        );
        assert!(app
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("copied ")));
        d.engine.shutdown();
    }

    /// A drop is a paste of a path, so a paste of nothing but real paths is a
    /// file being handed over. A paste with a word in it is a paste.
    #[test]
    fn a_paste_of_real_paths_attaches_and_a_paste_of_prose_does_not() {
        let dir = std::env::temp_dir().join(format!("rune-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let one = dir.join("shot.png");
        std::fs::write(&one, b"\x89PNG\r\n\x1a\n\x00").unwrap();

        assert_eq!(
            paths_in(&one.display().to_string()),
            Some(vec![one.clone()])
        );
        // The three wrappings a terminal actually delivers on a drop.
        assert_eq!(
            paths_in(&format!("'{}'", one.display())),
            Some(vec![one.clone()])
        );
        // A file URI as each OS writes one: `file:///home/x`, `file:///C:/x`.
        let uri = |host: &str, path: &std::path::Path| {
            let text = path.display().to_string().replace(' ', "%20");
            match runeengine::platform::OS {
                runeengine::platform::Os::Windows => {
                    format!("file://{host}/{}", text.replace('\\', "/"))
                }
                _ => format!("file://{host}{text}"),
            }
        };
        assert_eq!(paths_in(&uri("", &one)), Some(vec![one.clone()]));
        let two = dir.join("two shots.png");
        std::fs::write(&two, b"second").unwrap();
        let canonical = |found: Option<Vec<std::path::PathBuf>>| {
            found.map(|paths| {
                paths
                    .iter()
                    .map(|p| runeengine::platform::canonical(p).unwrap())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            canonical(paths_in(&format!(
                "# copied from file manager\n{}\n{}",
                uri("", &one),
                uri("localhost", &two)
            ))),
            canonical(Some(vec![one.clone(), two.clone()])),
            "a URI-list drop must attach every file atomically"
        );
        let three = dir.join("third.png");
        let four = dir.join("fourth.png");
        std::fs::write(&three, b"third").unwrap();
        std::fs::write(&four, b"fourth").unwrap();
        assert_eq!(
            paths_in(&format!(
                "{} {} {}",
                one.display(),
                three.display(),
                four.display()
            )),
            Some(vec![one.clone(), three.clone(), four.clone()]),
            "a one-line terminal path list must attach every path, not become chat text"
        );
        // Half prose is prose: guessing would put a file in front of the model
        // that nobody chose.
        assert_eq!(paths_in(&format!("look at {}", one.display())), None);
        assert_eq!(paths_in("what is wrong with this?"), None);
        assert_eq!(
            paths_in(&dir.display().to_string()),
            None,
            "a directory is not a file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Windows drop is read with its backslashes whole: quoted or not, one
    /// path or several, and a file URI naming a drive.
    #[test]
    fn a_pasted_windows_path_keeps_its_backslashes() {
        assert_eq!(
            windows_words(r#""C:\My Files\shot.png" D:\a\b.png 'E:\x y.txt'"#),
            [r"C:\My Files\shot.png", r"D:\a\b.png", r"E:\x y.txt"]
        );
        assert_eq!(
            windows_words(r"C:\Users\O'Brien\x.png"),
            [r"C:\Users\O'Brien\x.png"],
            "an apostrophe inside a name is no quote"
        );
        assert_eq!(windows_words("  "), Vec::<String>::new());
    }

    #[test]
    fn an_unbracketed_file_manager_path_is_attached_when_sent() {
        let mut d = crate::ui::test_driver("unbracketed-drop");
        let dir = std::env::temp_dir().join(format!("rune-unbracketed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("copied screenshot.png");
        std::fs::write(&file, b"\x89PNG\r\n\x1a\n\0").unwrap();
        let mut app = App::for_test();
        let text = file.display().to_string().replace(' ', "%20");
        app.input = match runeengine::platform::OS {
            runeengine::platform::Os::Windows => {
                format!("file://localhost/{}", text.replace('\\', "/"))
            }
            _ => format!("file://localhost{text}"),
        };
        app.cursor = app.input.len();
        app.submit(&mut d);
        assert!(app.input.is_empty(), "path must not remain as chat text");
        assert_eq!(app.attached.len(), 1);
        assert_eq!(
            app.attached[0].path,
            runeengine::platform::canonical(&file)
                .unwrap()
                .display()
                .to_string()
        );
        d.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The chip can be taken back before it is sent — with the pointer, and with
    /// the key a person already presses to undo the last thing they added.
    #[test]
    fn an_attachment_can_be_taken_back_before_it_is_sent() {
        let mut d = crate::ui::test_driver("attach");
        let dir = std::env::temp_dir().join(format!("rune-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("notes.md");
        std::fs::write(&f, b"# hi\n").unwrap();

        let mut app = App::for_test();
        let image = dir.join("Image.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        app.settings = serde_json::json!({
            "provider": "cached-provider",
            "settings": {"provider": "cached-provider", "model": "cached-model"},
            "models": [{"id": "cached-model", "modalities": ["text", "image"]}]
        });
        app.busy = true;
        app.attach(std::slice::from_ref(&image), &mut d);
        assert!(
            app.awaiting
                .values()
                .all(|a| !matches!(a, Awaiting::Catalogue(_))),
            "known media capabilities must not wait for provider I/O during streaming"
        );
        assert!(
            app.attachment_catalogues.is_empty(),
            "using cached metadata must not consume the unknown-metadata refresh"
        );
        app.attached.clear();
        app.busy = false;
        app.attach(std::slice::from_ref(&f), &mut d);
        assert_eq!(app.attached.len(), 1);
        // The same file twice is one file.
        app.attach(std::slice::from_ref(&f), &mut d);
        assert_eq!(app.attached.len(), 1);

        app.on_key(Key::Backspace, &mut d);
        assert_eq!(app.attached.len(), 1, "backspace must not delete a file");

        app.on_key(Key::Char('/'), &mut d);
        app.on_key(Key::Enter, &mut d);
        assert_eq!(
            app.attached.len(),
            1,
            "the first slash command must preserve attachments"
        );
        app.input = "/settings".into();
        app.cursor = 0;
        app.on_key(Key::Backspace, &mut d);
        assert_eq!(
            app.attached.len(),
            1,
            "editing at the start of a command must not delete files"
        );
        app.input.clear();
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            2,
            &mut d,
        );
        assert_eq!(
            app.attached.len(),
            1,
            "clicking the prefix must not delete a file"
        );
        assert_eq!(
            app.mode, "ask",
            "clicking the prefix must not change permission mode"
        );
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            75,
            &mut d,
        );
        assert_eq!(
            app.attached.len(),
            1,
            "clicking the name must not delete a file"
        );
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            83,
            &mut d,
        );
        assert_eq!(
            app.attached.len(),
            1,
            "clicking the separator must not delete a file"
        );

        // Space before cross
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            80,
            &mut d,
        );
        assert!(
            app.attached.is_empty(),
            "the space before cross must remove the attachment"
        );

        // Cross glyph itself
        app.attach(std::slice::from_ref(&f), &mut d);
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            81,
            &mut d,
        );
        assert!(
            app.attached.is_empty(),
            "clicking the cross directly must remove the attachment"
        );

        // Closing bracket
        app.attach(std::slice::from_ref(&f), &mut d);
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 1,
                width: 22,
            },
            82,
            &mut d,
        );
        assert!(
            app.attached.is_empty(),
            "clicking the closing bracket must remove the attachment"
        );

        // Mouse click with slight drag/jitter on the cross must still remove attachment
        use crate::ui::input::{Click, Mouse};
        app.attach(std::slice::from_ref(&f), &mut d);
        app.hits = vec![crate::ui::draw::Hit::Attachments {
            start: 60,
            visible: 1,
            width: 22,
        }];
        app.selection_rows = vec!["attachment row text".into()];
        app.on_mouse(
            Mouse {
                kind: Click::Press,
                row: 1,
                col: 81,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Drag,
                row: 1,
                col: 82,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Release,
                row: 1,
                col: 82,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        assert!(
            app.attached.is_empty(),
            "mouse jitter on close button must remove the attachment"
        );

        // When releasing on name but anchor was on cross, it still removes the attachment
        app.attach(std::slice::from_ref(&f), &mut d);
        app.on_mouse(
            Mouse {
                kind: Click::Press,
                row: 1,
                col: 81,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Drag,
                row: 1,
                col: 75,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        app.on_mouse(
            Mouse {
                kind: Click::Release,
                row: 1,
                col: 75,
                modifiers: crate::ui::input::Modifiers::NONE,
            },
            &mut d,
        );
        assert!(
            app.attached.is_empty(),
            "mouse jitter drifting off close button must still remove attachment"
        );

        // Multi-attachment targeting: removing the second attachment preserves the first
        let f2 = dir.join("other.txt");
        std::fs::write(&f2, b"second\n").unwrap();
        app.attach(std::slice::from_ref(&f), &mut d);
        app.attach(std::slice::from_ref(&f2), &mut d);
        assert_eq!(app.attached.len(), 2);
        // Second attachment cross is at 60 + 1 + 23 + 20 = 104
        app.act(
            crate::ui::draw::Hit::Attachments {
                start: 60,
                visible: 2,
                width: 22,
            },
            104,
            &mut d,
        );
        assert_eq!(app.attached.len(), 1);
        assert_eq!(
            app.attached[0].name, "notes.md",
            "first attachment must be preserved when second is removed"
        );
        let _ = std::fs::remove_file(&f2);

        app.slash("detach 0", &mut d);
        assert_eq!(app.attached.len(), 1);
        app.slash("detach 1", &mut d);
        assert!(app.attached.is_empty());

        // A stale session or other engine rejection returns the whole draft.
        app.attach(std::slice::from_ref(&f), &mut d);
        app.settings = serde_json::json!({"settings": {"model": "test"}});
        app.input = "Read my attachment".into();
        app.cursor = app.input.len();
        app.on_key(Key::Enter, &mut d);
        assert!(app.attached.is_empty());
        for event in d.events.try_iter() {
            app.on_event(event);
        }
        assert_eq!(
            app.attached.len(),
            1,
            "a rejected prompt must restore its files"
        );
        assert_eq!(app.input, "Read my attachment");
        assert!(!app.blocks.iter().any(|b| matches!(b, Block::User { .. })));
        app.slash("detach all", &mut d);

        // A path that is not a file is refused where the user can still see it.
        app.attach(&[dir.join("nothing-here")], &mut d);
        assert!(app.attached.is_empty());
        assert!(app.status.is_some(), "a bad path said nothing at all");
        let _ = std::fs::remove_dir_all(&dir);
        d.engine.shutdown();
    }

    #[test]
    fn canonical_editing_matrix_passes_through_composer_dispatch_without_product_conflicts() {
        let mut driver = crate::ui::test_driver("composer-editing-matrix");
        for case in super::super::editing::cases() {
            let mut app = App::for_test();
            app.input = case.text.into();
            app.cursor = case.at;
            for key in case.keys {
                app.on_key(key, &mut driver);
            }
            assert_eq!(app.input, case.expected, "{}", case.name);
            assert_eq!(
                app.panel,
                Panel::None,
                "{} opened a product panel",
                case.name
            );
        }
        driver.engine.shutdown();
    }

    #[test]
    fn arrows_and_page_keys_navigate_lines_and_fall_back_to_history_at_ends() {
        let mut d = crate::ui::test_driver("arrows-and-pgup-history");
        let mut app = App::for_test();
        app.prompt_history = vec!["pesan 1".into(), "pesan 2".into()];

        // The user's exact case:
        // das|das
        // fdsfds
        app.input = "dasdas\nfdsfds".into();
        app.cursor = 3; // at das|das
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("dasdas\nfdsfds", 0),
            "Up at top row moves to 0"
        );
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("pesan 2", 0),
            "Up at 0 recalls previous prompt"
        );

        // Now move down to end of pesan 2, then restore multiline draft:
        app.on_key(Key::Down, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("pesan 2", 7),
            "Down moves to end of recalled prompt"
        );
        app.on_key(Key::Down, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("dasdas\nfdsfds", 13),
            "Down again restores draft at end"
        );

        // The user's exact downward case:
        // dasdas
        // fds|fds
        app.cursor = 10; // at fds|fds
        app.on_key(Key::Down, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("dasdas\nfdsfds", 13),
            "Down at bottom row moves to end"
        );
        app.on_key(Key::Down, &mut d);
        // At the end with no newer history, it remains at the end
        assert_eq!((app.input.as_str(), app.cursor), ("dasdas\nfdsfds", 13));

        // When moving between lines in multiline text, vertical line motion works:
        app.cursor = 10; // line 1 col 3
        app.on_key(Key::Up, &mut d);
        assert_eq!(app.cursor, 3, "Up moves to line 0 keeping column 3");
        app.on_key(Key::Down, &mut d);
        assert_eq!(app.cursor, 10, "Down moves to line 1 keeping column 3");

        // Page keys navigate history directly, without repeating cursor-row motion:
        app.input = "draft typing".into();
        app.cursor = 5;
        app.on_key(Key::PageUp, &mut d);
        assert_eq!((app.input.as_str(), app.cursor), ("pesan 2", 0));
        app.on_key(Key::PageUp, &mut d);
        assert_eq!(app.input, "pesan 1");
        app.on_key(Key::PageDown, &mut d);
        assert_eq!((app.input.as_str(), app.cursor), ("pesan 2", 7));
        app.on_key(Key::PageDown, &mut d);
        assert_eq!((app.input.as_str(), app.cursor), ("draft typing", 12));

        // 4 lines navigation:
        app.input = "baris 1\nbaris 2\nbaris 3\nbaris 4".into();
        app.cursor = 31;
        for cursor in [23, 15, 7, 0] {
            app.on_key(Key::Up, &mut d);
            assert_eq!(app.cursor, cursor);
        }
        app.on_key(Key::Up, &mut d);
        assert_eq!(app.input, "pesan 2");

        // When in a recalled prompt, moving right and pressing Up or Down must NOT immediately jump history:
        app.history_index = None;
        app.prompt_history = vec!["prompt 0".into(), "pwqeoiqw\nwiqoeuw\newq".into()];
        app.input = "current".into();
        app.cursor = 0;
        app.on_key(Key::Up, &mut d); // recalls "pwqeoiqw\nwiqoeuw\newq"
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("pwqeoiqw\nwiqoeuw\newq", 0)
        );
        app.on_key(Key::Down, &mut d); // moves to line 1: wiqoeuw
        assert_eq!(app.cursor, 9);
        app.on_key(Key::Right, &mut d); // moves right into text: w|iqoeuw
        assert_eq!(app.cursor, 10);
        // Pressing Up moves up to row 0 col 1, NOT jumping to prompt 0:
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("pwqeoiqw\nwiqoeuw\newq", 1),
            "Up from line 1 moves to line 0 col 1"
        );
        // Pressing Up again moves to 0 (start of text), NOT jumping to prompt 0:
        app.on_key(Key::Up, &mut d);
        assert_eq!(
            (app.input.as_str(), app.cursor),
            ("pwqeoiqw\nwiqoeuw\newq", 0),
            "Up at line 0 moves to 0"
        );
        // Pressing Up again while already at 0 recalls prompt 0:
        app.on_key(Key::Up, &mut d);
        assert_eq!(app.input, "prompt 0", "Up while at 0 recalls prompt 0");

        d.engine.shutdown();
    }

    #[test]
    fn theme_slash_command_cycles_and_sets_theme() {
        use crate::ui::theme::ThemeMode;

        let mut app = App::for_test();
        let mut d = test_driver("theme-slash");
        assert_eq!(app.theme, ThemeMode::Violet);

        // /theme with no arguments cycles to next
        app.slash("theme", &mut d);
        assert_eq!(app.theme, ThemeMode::Cyan);
        assert!(app.transcript_cache.is_none());
        assert!(app.resync);
        assert!(app.transcript_dirty);

        app.slash("theme", &mut d);
        assert_eq!(app.theme, ThemeMode::Amber);

        app.slash("theme", &mut d);
        assert_eq!(app.theme, ThemeMode::Violet);

        // /theme with specific name
        app.slash("theme cyan", &mut d);
        assert_eq!(app.theme, ThemeMode::Cyan);

        app.slash("theme amber", &mut d);
        assert_eq!(app.theme, ThemeMode::Amber);

        app.slash("theme violet", &mut d);
        assert_eq!(app.theme, ThemeMode::Violet);

        // unknown theme
        app.slash("theme neon", &mut d);
        assert_eq!(app.theme, ThemeMode::Violet);
        assert!(app.status.as_ref().unwrap().contains("unknown theme"));

        d.engine.shutdown();
    }

    #[test]
    fn tab_cycles_permission_mode_in_composer_without_leaking_from_menus() {
        let dir = std::env::temp_dir().join(format!("rune-tab-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = crate::ui::test_driver("tab-mode");
        let mut app = App::for_test();
        app.project = dir.display().to_string();
        let session = d.open(&app.project).unwrap();
        app.session = Some(session);
        assert_eq!(app.mode, "ask");

        let pump = |app: &mut App, d: &mut Driver| {
            for event in d.events.try_iter() {
                app.on_event(event);
            }
        };

        // 1. In composer: Tab cycles ask -> auto -> full -> ask
        app.on_key(Key::Tab, &mut d);
        pump(&mut app, &mut d);
        assert_eq!(app.mode, "auto");
        app.on_key(Key::Tab, &mut d);
        pump(&mut app, &mut d);
        assert_eq!(app.mode, "full");
        app.on_key(Key::Tab, &mut d);
        pump(&mut app, &mut d);
        assert_eq!(app.mode, "ask");

        // 2. ShiftTab must NOT cycle mode
        app.on_key(Key::ShiftTab, &mut d);
        pump(&mut app, &mut d);
        assert_eq!(app.mode, "ask", "ShiftTab must not cycle mode");

        // 3. While typing a slash command, Tab autocompletes and must NOT cycle mode
        app.input = "/sett".into();
        app.cursor = 5;
        app.on_key(Key::Tab, &mut d);
        assert_eq!(app.input, "/settings");
        assert_eq!(
            app.mode, "ask",
            "tab completing command must not change permission mode"
        );
        app.input.clear();
        app.cursor = 0;

        // 4. In settings screen (menu), Tab navigates sections and must NOT cycle mode
        app.panel = Panel::Settings;
        app.preferences.tabs = true;
        app.on_key(Key::Tab, &mut d);
        assert!(
            !app.preferences.tabs,
            "tab should switch away from in_sections in settings"
        );
        assert_eq!(
            app.mode, "ask",
            "tab in settings must not leak to permission mode"
        );
        app.on_key(Key::Tab, &mut d);
        assert!(
            app.preferences.tabs,
            "tab should switch back to in_sections in settings"
        );
        assert_eq!(
            app.mode, "ask",
            "tab in settings must not leak to permission mode"
        );

        // 5. In other panels (Tasks / Help / Undo), Tab must NOT cycle mode
        app.panel = Panel::Tasks;
        app.on_key(Key::Tab, &mut d);
        assert_eq!(
            app.mode, "ask",
            "tab in tasks panel must not leak to permission mode"
        );

        app.panel = Panel::None;

        // 6. Clicking composer top border line (Hit::Composer) must NOT cycle mode
        app.hits = vec![crate::ui::draw::Hit::Composer];
        app.press(1, 50, &mut d);
        assert_eq!(
            app.mode, "ask",
            "clicking composer top border line must not change permission mode"
        );

        d.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classifier_slash_command_autocomplete_and_execution() {
        let dir = std::env::temp_dir().join(format!("rune-classifier-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = crate::ui::test_driver("classifier-slash");
        let mut app = App::for_test();
        app.project = dir.display().to_string();
        let session = d.open(&app.project).unwrap();
        app.session = Some(session);

        // 1. Autocomplete /class -> /classifier
        let hits = matches("/class");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "classifier");

        // 2. /classifier (status) sets note in footer status bar and does NOT pollute chat
        let blocks_before = app.blocks.len();
        app.slash("classifier", &mut d);
        assert_eq!(
            app.blocks.len(),
            blocks_before,
            "slash command must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Classifier:")));
        assert!(
            app.status_at.is_some(),
            "note must set status_at timestamp for 8s timeout"
        );

        // 3. /classifier unknown subcommand shows usage
        app.slash("classifier foobar", &mut d);
        assert_eq!(
            app.blocks.len(),
            blocks_before,
            "unknown subcommand must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /classifier")));

        // 4. Clicking a palette item fills the input line and does NOT submit or add blocks
        app.input = "/class".into();
        app.act(crate::ui::draw::Hit::Palette(0), 10, &mut d);
        assert_eq!(
            app.input, "/classifier ",
            "palette click must autocomplete into input field"
        );
        assert_eq!(
            app.blocks.len(),
            blocks_before,
            "palette click must not submit or add blocks to transcript"
        );

        d.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_palette_enter_autocompletes_argument_commands_without_submitting() {
        let mut d = test_driver("palette-enter-arg");
        let mut app = App::for_test();

        // 1. Type /se -> matches /set
        press(&mut app, &mut d, &typed("/se"));
        assert!(app.searching());
        assert_eq!(matches(&app.input)[0].0, "set");

        // Press Enter on /se -> must autocomplete to "/set " and show usage in footer status bar
        let blocks_before = app.blocks.len();
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/set ",
            "Enter on argument-taking command must autocomplete with trailing space"
        );
        assert_eq!(app.cursor, 5);
        assert_eq!(
            app.blocks.len(),
            blocks_before,
            "autocomplete must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /set")));
        assert!(
            app.status_at.is_some(),
            "must set status_at timestamp for 8s timeout"
        );

        // Press Enter AGAIN while still "/set " without args -> must NOT submit, keep "/set ", and refresh note
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/set ",
            "Enter again without args must retain '/set '"
        );
        assert_eq!(
            app.blocks.len(),
            blocks_before,
            "Enter again must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /set")));

        // Now user types argument and presses Enter -> should submit
        press(&mut app, &mut d, &typed("sampling.temperature=0.7"));
        assert_eq!(app.input, "/set sampling.temperature=0.7");
        press(&mut app, &mut d, &[Key::Enter]);
        assert!(
            app.input.is_empty(),
            "command with arguments must submit and clear input"
        );

        // 2. Type /att -> matches /attach
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/att"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/attach ",
            "Enter on /att must autocomplete to '/attach '"
        );
        assert_eq!(app.cursor, 8);
        assert_eq!(app.blocks.len(), 0, "must not add blocks to transcript");
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /attach")));

        // Press Enter AGAIN on "/attach " without path -> must NOT clear and must refresh note
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/attach ",
            "Enter again on '/attach ' must retain input"
        );
        assert_eq!(
            app.blocks.len(),
            0,
            "repeated Enter must never pollute chat transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /attach")));

        // 3. Type /det -> matches /detach
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/det"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/detach ",
            "Enter on /det must autocomplete to '/detach '"
        );
        assert_eq!(app.cursor, 8);
        assert_eq!(app.blocks.len(), 0, "must not add blocks to transcript");
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("No files currently attached")));

        // Press Enter AGAIN on "/detach " without target -> must retain input and refresh note
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/detach ",
            "Enter again on '/detach ' must retain input"
        );
        assert_eq!(
            app.blocks.len(),
            0,
            "repeated Enter must never pollute chat transcript"
        );

        // 4. Type / on row 0 (> /attach) -> press Enter -> autocompletes to "/attach "
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/"));
        assert_eq!(app.palette_row, 0);
        assert_eq!(matches(&app.input)[0].0, "attach");
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/attach ",
            "Enter on row 0 (> /attach) must autocomplete to '/attach '"
        );
        assert_eq!(app.blocks.len(), 0, "must not add blocks to transcript");
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /attach")));

        // Press Enter again on "/attach "
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/attach ",
            "Enter again must keep '/attach ' in input"
        );
        assert_eq!(app.blocks.len(), 0, "repeated Enter must not push to chat");

        // 5. Type /mod -> matches /mode
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/mod"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/mode ",
            "Enter on /mod must autocomplete to '/mode ' without switching mode"
        );
        assert_eq!(app.cursor, 6);
        assert_eq!(
            app.mode, "ask",
            "mode must not switch when autocompleting command"
        );
        // Press Enter again on "/mode "
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/mode ", "Enter again must keep '/mode '");
        assert_eq!(app.mode, "ask", "mode must still not switch on empty Enter");
        // Now type 'auto' and Enter
        press(&mut app, &mut d, &typed("auto"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Switching mode to auto")));

        // 6. Type /the -> matches /theme
        let mut app = App::for_test();
        let theme_before = app.theme;
        press(&mut app, &mut d, &typed("/the"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/theme ",
            "Enter on /the must autocomplete to '/theme ' without switching theme"
        );
        assert_eq!(app.cursor, 7);
        assert_eq!(
            app.theme, theme_before,
            "theme must not switch when autocompleting command"
        );
        // Press Enter again on "/theme "
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/theme ", "Enter again must keep '/theme '");
        assert_eq!(
            app.theme, theme_before,
            "theme must still not switch on empty Enter"
        );
        // Now type 'cyan' and Enter
        press(&mut app, &mut d, &typed("cyan"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.theme, super::super::theme::ThemeMode::Cyan);

        // 7. Type /class -> matches /classifier
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/class"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/classifier ",
            "Enter on /class must autocomplete to '/classifier ' without disappearing"
        );
        assert_eq!(app.cursor, 12);
        // Press Enter again on "/classifier "
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(
            app.input, "/classifier ",
            "Enter again must keep '/classifier ' in input field"
        );

        // 8. /tasks is gone from COMMANDS (ctrl+t opens the plan); /undo is
        // the one door to the undo history (owner, 2026-10-08).
        assert!(
            matches("/tasks").is_empty(),
            "/tasks must be removed from slash commands"
        );
        assert!(
            matches("/plan").is_empty(),
            "/plan must be removed from slash commands"
        );
        assert!(
            matches("/undo").iter().any(|c| c.0 == "undo"),
            "/undo opens the undo history"
        );
    }

    #[test]
    fn exact_match_precedence_over_prefix_and_chevron_stability() {
        let mut d = test_driver("exact-match-chevron");

        // 1. /mode vs /model:
        // When typing "/mod", both match prefix: mode (alphabetically first) at 0, model at 1
        let hits_mod = matches("/mod");
        assert_eq!(hits_mod[0].0, "mode");
        assert_eq!(hits_mod[1].0, "model");

        // When typing "/mode", exact match for "mode" has rank 0, while "model" (description match) has rank 3.
        let hits_mode = matches("/mode");
        assert_eq!(hits_mode[0].0, "mode");

        // When space is typed "/mode ", ONLY "mode" matches — "model" is excluded completely!
        let hits_mode_space = matches("/mode ");
        assert_eq!(hits_mode_space.len(), 1);
        assert_eq!(hits_mode_space[0].0, "mode");

        // 2. /set vs /session vs /settings:
        // When typing "/se", set is at 0 (len 3), session is at 1 (len 7), settings is at 2 (len 8)
        let hits_se = matches("/se");
        assert_eq!(hits_se[0].0, "set");
        assert_eq!(hits_se[1].0, "session");
        assert_eq!(hits_se[2].0, "settings");

        // When typing "/set", exact match "set" is at 0, "settings" is at 1
        let hits_set = matches("/set");
        assert_eq!(hits_set[0].0, "set");
        assert_eq!(hits_set[1].0, "settings");

        // When space is typed "/set ", ONLY "set" matches — "settings" is excluded completely!
        let hits_set_space = matches("/set ");
        assert_eq!(hits_set_space.len(), 1);
        assert_eq!(hits_set_space[0].0, "set");

        // 3. User interaction: type "/mode" and press Enter twice
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/mode"));
        assert_eq!(app.palette_row, 0, "chevron should point at row 0 (mode)");
        assert_eq!(matches(&app.input)[0].0, "mode");

        // First Enter -> autocompletes to "/mode " and resets palette_row to 0
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/mode ");
        assert_eq!(app.palette_row, 0);
        assert_eq!(app.panel, Panel::None, "must NOT open model picker");

        // Second Enter -> retains "/mode " and refreshes tip, palette_row stays 0, must NOT open model picker
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/mode ");
        assert_eq!(app.palette_row, 0);
        assert_eq!(
            app.panel,
            Panel::None,
            "second Enter must NOT trigger /model"
        );

        // 4. User interaction: navigate with Down arrow, then autocomplete /mode
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/"));
        // Move down several rows
        press(&mut app, &mut d, &[Key::Down, Key::Down, Key::Down]);
        assert_eq!(app.palette_row, 3);
        // Type "mode"
        press(&mut app, &mut d, &typed("mode"));
        assert_eq!(
            app.palette_row, 0,
            "palette_row must reset to 0 upon typing"
        );
        assert_eq!(matches(&app.input)[0].0, "mode");
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/mode ");
        assert_eq!(app.palette_row, 0);
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/mode ");
        assert_eq!(app.palette_row, 0);
        assert_eq!(
            app.panel,
            Panel::None,
            "must not open settings/model picker"
        );

        // 5. User interaction: type "/set" and press Enter twice
        let mut app = App::for_test();
        press(&mut app, &mut d, &typed("/set"));
        assert_eq!(app.palette_row, 0, "chevron points at 'set'");
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/set ");
        assert_eq!(app.palette_row, 0);
        press(&mut app, &mut d, &[Key::Enter]);
        assert_eq!(app.input, "/set ");
        assert_eq!(app.palette_row, 0);
        assert_eq!(app.panel, Panel::None, "must NOT open /settings");
    }

    #[test]
    fn slash_commands_feedback_and_zero_chat_pollution() {
        let mut d = test_driver("cmd-feedback");
        let mut app = App::for_test();

        // 1. /verbose toggles and updates status in footer with 8s timeout, ZERO chat blocks
        assert!(!app.verbose);
        app.slash("verbose", &mut d);
        assert!(app.verbose);
        assert_eq!(
            app.blocks.len(),
            0,
            "verbose toggle must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Verbose mode on")));
        assert!(app.status_at.is_some(), "must set status_at for 8s timeout");

        app.slash("verbose", &mut d);
        assert!(!app.verbose);
        assert_eq!(
            app.blocks.len(),
            0,
            "verbose toggle must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Verbose mode off")));

        // 2. /panel toggles and updates status in footer, ZERO chat blocks
        app.sidebar = false;
        assert!(!app.sidebar);
        app.slash("panel", &mut d);
        assert!(app.sidebar);
        assert_eq!(
            app.blocks.len(),
            0,
            "panel toggle must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Session panel")));

        // 3. /set without args keeps input and shows usage in footer, ZERO chat blocks
        app.slash("set", &mut d);
        assert_eq!(app.input, "/set ");
        assert_eq!(
            app.blocks.len(),
            0,
            "set without args must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /set key=value")));

        // 4. /attach without args keeps input and shows usage in footer, ZERO chat blocks
        app.slash("attach", &mut d);
        assert_eq!(app.input, "/attach ");
        assert_eq!(
            app.blocks.len(),
            0,
            "attach without args must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Usage: /attach <path>")));

        // 5. /detach without args keeps input and shows footer note, ZERO chat blocks
        app.slash("detach", &mut d);
        assert_eq!(app.input, "/detach ");
        assert_eq!(
            app.blocks.len(),
            0,
            "detach without args must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("No files currently attached")));

        // 6. /mode with explicit mode name
        app.slash("mode auto", &mut d);
        assert_eq!(
            app.blocks.len(),
            0,
            "mode switch must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Switching mode to auto")));

        app.slash("mode full", &mut d);
        assert_eq!(
            app.blocks.len(),
            0,
            "mode switch must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Switching mode to full")));

        app.slash("mode invalid", &mut d);
        assert_eq!(
            app.blocks.len(),
            0,
            "invalid mode must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Unknown mode 'invalid'")));

        // 7. /theme cycles or sets theme, ZERO chat blocks
        app.slash("theme", &mut d);
        assert_eq!(
            app.blocks.len(),
            0,
            "theme change must not add blocks to transcript"
        );
        assert!(app.status.as_ref().is_some_and(|s| s.contains("Theme:")));

        // 8. Unknown command shows footer note, ZERO chat blocks
        app.slash("invalid_command", &mut d);
        assert_eq!(
            app.blocks.len(),
            0,
            "unknown command must not add blocks to transcript"
        );
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Unknown command: /invalid_command")));
    }

    /// Owner, 2026-10-15: the levels `/thinking on` names are the model's
    /// own, as the engine lists them — never three written into the text.
    #[test]
    fn thinking_on_names_the_levels_the_model_offers() {
        let mut d = test_driver("thinking-on-levels");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "9router", "model": "ag/gemini-3.8-flash-high", "thinking": false },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices":
                    ["off", "default", "minimal", "low", "medium", "high", "xhigh", "max"] }
            ]
        }));
        app.slash("thinking on", &mut d);
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("Route default — /thinking minimal|low|medium|high|xhigh|max"),
            "what to type comes first: {status}"
        );
        assert!(status.contains("/thinking off"), "{status}");
        assert!(app.thinking());
        assert_eq!(app.effort(), None, "on with no level is the route default");
        d.engine.shutdown();
    }

    #[test]
    fn thinking_command_and_alt_t_cycling() {
        let mut d = test_driver("thinking-test");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "openrouter", "model": "test-effort-model", "thinking": false },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices": ["off", "low", "medium", "high"] }
            ]
        }));

        // 1. Initial test state has thinking=false, effort=None
        assert!(!app.thinking());
        assert_eq!(app.effort(), None);

        // 2. Alt+T cycles thinking from off -> low for model with effort choices
        //    No status note — the composer header indicator is the visual feedback
        app.on_key(Key::Alt('t'), &mut d);
        assert!(app.thinking());
        assert_eq!(app.effort(), Some("low"));

        // 3. /thinking on directs user to effort levels / Alt+T
        app.slash("thinking on", &mut d);
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Specify an effort level")));

        // 4. /thinking high applies effort high
        app.slash("thinking high", &mut d);
        assert_eq!(app.effort(), Some("high"));

        // 5. /thinking off applies thinking off
        app.slash("thinking off", &mut d);
        assert!(!app.thinking());

        // 6. Binary reasoning model (e.g. 9router kr/claude-opus-5-thinking) toggles on,
        //    but cannot be turned off because thinking is mandatory for intrinsic models
        let mut binary_app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "9router", "model": "kr/claude-opus-5-thinking", "thinking": false },
            "thinking_required": true,
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" }
            ],
            "suppressed": [
                { "key": "effort", "reason": "this model has no effort levels", "permanent": true }
            ]
        }));
        binary_app.on_key(Key::Alt('t'), &mut d);
        assert!(binary_app.thinking());
        binary_app.on_key(Key::Alt('t'), &mut d);
        // Thinking cannot be turned off for intrinsic thinking models
        assert!(binary_app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("mandatory")));
        assert!(
            binary_app.thinking(),
            "intrinsic thinking model must stay on"
        );

        // 7. /model tips
        let mut empty_app = App::for_test();
        empty_app.slash("model", &mut d);
        assert_eq!(empty_app.panel, Panel::Settings);
    }

    #[test]
    fn rapid_thinking_effort_cycling_does_not_glitch_or_bounce_back_on_stale_ack() {
        let mut d = test_driver("rapid-effort-cycling");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "openrouter", "model": "test-effort-model", "thinking": true, "effort": "low" },
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool" },
                { "key": "effort", "label": "Effort", "kind": "choice", "choices": ["off", "low", "medium", "high"] }
            ]
        }));

        assert_eq!(app.effort(), Some("low"));

        // Alt+T once -> medium
        app.on_key(Key::Alt('t'), &mut d);
        assert_eq!(app.effort(), Some("medium"));
        let first_token = app.preferences.serial;

        // Alt+T immediately again -> high
        app.on_key(Key::Alt('t'), &mut d);
        assert_eq!(app.effort(), Some("high"));
        let second_token = app.preferences.serial;
        assert!(second_token > first_token);

        // Stale Ack from request 1 arrives carrying older "medium" state
        let session = app.session.unwrap();
        let stale_ack_data = serde_json::json!({
            "settings": { "provider": "openrouter", "model": "test-effort-model", "thinking": true, "effort": "medium" }
        });
        crate::ui::settings::ack(
            &mut app,
            crate::ui::settings::Request {
                token: first_token,
                session: Some(session),
                origin: crate::ui::settings::Page::Overview,
                editor: None,
                operation: crate::ui::settings::Operation::Setting(Some("effort".into())),
                before: None,
            },
            true,
            None,
            Some(stale_ack_data),
        );

        // Effort MUST remain "high", never bouncing back to "medium"
        assert_eq!(app.effort(), Some("high"));
    }

    #[test]
    fn submit_while_tool_active_queues_without_cancelling_tool() {
        let mut d = test_driver("submit-tool-active");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        let mut tool =
            super::super::blocks::Tool::new("t1".into(), "inspect_file".into(), "reading".into());
        tool.state = super::super::blocks::ToolState::Running;
        app.blocks.push(super::super::blocks::Block::of(tool));

        press(&mut app, &mut d, &typed("new request during tool"));
        press(&mut app, &mut d, &[Key::Enter]);

        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(
            app.blocks[0],
            super::super::blocks::Block::Tool(_)
        ));
        assert_eq!(
            app.blocks[0].tool().unwrap().state,
            super::super::blocks::ToolState::Running
        );
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::User { .. }
        ));
        assert!(app.queued_prompt.is_some());
        assert_eq!(
            app.queued_prompt.as_ref().unwrap().steers[0].text,
            "new request during tool"
        );

        // Tool completes normally and reports result into app.blocks[0]
        app.on_event(runeengine::proto::Event::ToolResult {
            session: app.session.unwrap(),
            id: "t1".into(),
            ok: true,
            summary: "read 50 lines".into(),
            content: "file content".into(),
            detail: None,
            at: 0,
        });
        assert_eq!(
            app.blocks[0].tool().unwrap().state,
            super::super::blocks::ToolState::Ok
        );
        assert_eq!(app.blocks[0].tool().unwrap().summary, "read 50 lines");

        // The turn ends: nothing is sent again (the engine holds the message,
        // and starts it itself); it waits on screen until the engine says it
        // took it.
        let sent = app.awaiting.len();
        app.busy = false;
        app.on_event(runeengine::proto::Event::TurnStart {
            session: app.session.unwrap(),
            turn: 2,
            text: Some("new request during tool".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert_eq!(app.awaiting.len(), sent, "the message was not sent twice");
        assert!(app.queued_prompt.is_none());
        assert_eq!(
            app.blocks
                .iter()
                .filter(|b| matches!(b, super::super::blocks::Block::User { .. }))
                .count(),
            1,
            "and it is drawn once"
        );
    }

    #[test]
    fn submit_while_text_streaming_queues_and_preserves_order() {
        let mut d = test_driver("submit-text-streaming");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        app.blocks.push(super::super::blocks::Block::Text {
            text: "partial response".into(),
        });

        press(&mut app, &mut d, &typed("steer request"));
        press(&mut app, &mut d, &[Key::Enter]);

        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(
            app.blocks[0],
            super::super::blocks::Block::Text { .. }
        ));
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::User { .. }
        ));
        assert_eq!(
            app.queued_prompt.as_ref().unwrap().steers[0].text,
            "steer request"
        );

        // Further text delta appends to the Block::Text before the user block
        app.append_text(" continued");
        if let super::super::blocks::Block::Text { text } = &app.blocks[0] {
            assert_eq!(text, "partial response continued");
        } else {
            panic!("expected Block::Text at index 0");
        }
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::User { .. }
        ));

        // The engine starts the waiting message itself; the window only
        // recognises it.
        app.busy = false;
        app.on_event(runeengine::proto::Event::TurnStart {
            session: app.session.unwrap(),
            turn: 2,
            text: Some("steer request".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert!(app.queued_prompt.is_none());
    }

    /// A message the engine refuses (an attachment gone, the session closed)
    /// was never sent: it leaves the queue and the screen, and its words come
    /// back to the composer ahead of whatever was typed since — never lost.
    #[test]
    fn a_refused_message_leaves_the_screen_and_its_words_come_back_first() {
        let mut d = test_driver("refused-message");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        press(&mut app, &mut d, &typed("first message"));
        press(&mut app, &mut d, &[Key::Enter]);
        let id = app
            .awaiting
            .iter()
            .find(|(_, what)| matches!(what, crate::ui::Awaiting::Prompt { .. }))
            .map(|(id, _)| *id)
            .expect("the message was sent");
        assert!(app.queued_prompt.is_some());
        press(&mut app, &mut d, &typed("typed since"));
        app.on_event(runeengine::proto::Event::Ack {
            id,
            ok: false,
            error: Some("invalid: an attachment is gone".into()),
            data: None,
        });
        assert!(app.queued_prompt.is_none());
        assert!(!app
            .blocks
            .iter()
            .any(|b| matches!(b, super::super::blocks::Block::User { .. })));
        assert_eq!(app.input, "first message\ntyped since");
    }

    #[test]
    fn submit_blocked_by_active_dialogs() {
        let mut d = test_driver("submit-dialog-blocked");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        app.input = "blocked message".into();
        app.pending = Some(super::super::blocks::Ask {
            req: "r1".into(),
            action: "read".into(),
            preview: "".into(),
            remember_as: None,
            reach: Vec::new(),
            sensitive: Vec::new(),
            full_access: false,
            reason: None,
        });

        app.submit(&mut d);

        assert!(app.queued_prompt.is_none());
        assert_eq!(app.input, "blocked message");
        assert!(app
            .status
            .as_ref()
            .is_some_and(|s| s.contains("Answer the active dialog")));
    }

    #[test]
    fn explicit_stop_clears_queued_prompt() {
        let mut d = test_driver("submit-stop-clears");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        app.blocks.push(super::super::blocks::Block::Text {
            text: "answer".into(),
        });
        press(&mut app, &mut d, &typed("queued message"));
        press(&mut app, &mut d, &[Key::Enter]);
        assert!(app.queued_prompt.is_some());

        // Pressing Esc twice triggers stop_or_quit which clears queued_prompt
        press(&mut app, &mut d, &[Key::Esc]);
        press(&mut app, &mut d, &[Key::Esc]);
        assert!(app.queued_prompt.is_none());
    }

    #[test]
    fn tool_events_after_submit_stay_locked_in_their_original_box() {
        let mut d = test_driver("tool-stay-locked");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        let mut tool = super::super::blocks::Tool::new(
            "t1".into(),
            "search_text".into(),
            "Searching code".into(),
        );
        tool.state = super::super::blocks::ToolState::Running;
        app.blocks.push(super::super::blocks::Block::of(tool));

        press(&mut app, &mut d, &typed("new response"));
        press(&mut app, &mut d, &[Key::Enter]);

        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(
            app.blocks[0],
            super::super::blocks::Block::Tool(_)
        ));
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::User { .. }
        ));

        // Incoming ToolCall for t1 settles args into app.blocks[0], does NOT create duplicate block!
        app.on_event(runeengine::proto::Event::ToolCall {
            session: app.session.unwrap(),
            id: "t1".into(),
            name: "search_text".into(),
            action: "Searching code".into(),
            args: serde_json::json!({"query": "Rune"}),
            at: 0,
        });
        assert_eq!(
            app.blocks.len(),
            2,
            "ToolCall must not create a duplicate block"
        );

        // Incoming ToolResult for t1 with error updates app.blocks[0], does NOT create duplicate block!
        app.on_event(runeengine::proto::Event::ToolResult {
            session: app.session.unwrap(),
            id: "t1".into(),
            ok: false,
            summary: "Not executed. One response may use only one tool kind".into(),
            content: "error".into(),
            detail: None,
            at: 0,
        });
        assert_eq!(
            app.blocks.len(),
            2,
            "ToolResult must not create a duplicate block"
        );
        let tool = app.blocks[0].tool().unwrap();
        assert_eq!(tool.state, super::super::blocks::ToolState::Failed);
        assert_eq!(
            tool.summary,
            "Not executed. One response may use only one tool kind"
        );
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::User { .. }
        ));
    }

    #[test]
    fn many_rapid_steers_stay_separate_and_are_never_drawn_twice() {
        let mut d = test_driver("rapid-steers");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        app.blocks.push(super::super::blocks::Block::Text {
            text: "working...".into(),
        });

        let typed_messages = ["satu", "dua", "tiga", "empat", "lima", "enam"];
        for message in typed_messages {
            press(&mut app, &mut d, &typed(message));
            press(&mut app, &mut d, &[Key::Enter]);
        }

        // One block per Enter, and the queue holds six separate steers — not one
        // blob of six lines.
        assert_eq!(app.blocks.len(), 7);
        let queued = app.queued_prompt.as_ref().unwrap();
        assert_eq!(queued.steers.len(), 6);
        let queued_text: Vec<&str> = queued.steers.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(queued_text, typed_messages);

        // The engine answers with one TurnStart per steer, each carrying its own
        // text and turn. Every echo must land on the block already on screen: the
        // bug this pins is a tenth block holding all six lines joined by \n.
        for (i, message) in typed_messages.iter().enumerate() {
            app.on_event(runeengine::proto::Event::TurnStart {
                session: app.session.unwrap(),
                turn: 100 + i as i64,
                text: Some((*message).into()),
                notification: false,
                media: Vec::new(),
                at: 0,
            });
            assert_eq!(
                app.blocks.len(),
                7,
                "TurnStart for {message:?} drew a duplicate block"
            );
        }
        assert!(app.queued_prompt.is_none());

        let drawn: Vec<&str> = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                super::super::blocks::Block::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(drawn, typed_messages);
    }

    #[test]
    fn post_tool_text_creates_new_block_after_tools_when_queued_prompt_present() {
        let mut d = test_driver("post-tool-text-queued");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.busy = true;
        app.blocks.push(super::super::blocks::Block::Text {
            text: "Starting tools...".into(),
        });
        let mut tool = super::super::blocks::Tool::new(
            "t1".into(),
            "inspect_file".into(),
            "Reading file".into(),
        );
        tool.state = super::super::blocks::ToolState::Ok;
        app.blocks.push(super::super::blocks::Block::of(tool));

        press(&mut app, &mut d, &typed("tambah 2"));
        press(&mut app, &mut d, &[Key::Enter]);

        assert_eq!(app.blocks.len(), 3);
        assert!(matches!(
            app.blocks[0],
            super::super::blocks::Block::Text { .. }
        ));
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::Tool(_)
        ));
        assert!(matches!(
            app.blocks[2],
            super::super::blocks::Block::User { .. }
        ));

        // Assistant final response delta arrives
        app.on_event(runeengine::proto::Event::Text {
            session: app.session.unwrap(),
            delta: "Results: 3 tools done.".into(),
        });

        // Must have created a new Block::Text at index 2 (between Tool and User prompt)
        assert_eq!(app.blocks.len(), 4);
        match &app.blocks[0] {
            super::super::blocks::Block::Text { text } => {
                assert_eq!(
                    text, "Starting tools...",
                    "Pre-tool text must NOT be contaminated"
                );
            }
            _ => panic!("Expected Block::Text at 0"),
        }
        assert!(matches!(
            app.blocks[1],
            super::super::blocks::Block::Tool(_)
        ));
        match &app.blocks[2] {
            super::super::blocks::Block::Text { text } => {
                assert_eq!(text, "Results: 3 tools done.");
            }
            _ => panic!("Expected Block::Text at 2"),
        }
        assert!(matches!(
            app.blocks[3],
            super::super::blocks::Block::User { .. }
        ));

        // Second delta appends to the new Block::Text at index 2
        app.on_event(runeengine::proto::Event::Text {
            session: app.session.unwrap(),
            delta: " All good!".into(),
        });
        assert_eq!(app.blocks.len(), 4);
        match &app.blocks[2] {
            super::super::blocks::Block::Text { text } => {
                assert_eq!(text, "Results: 3 tools done. All good!");
            }
            _ => panic!("Expected Block::Text at 2"),
        }
    }

    #[test]
    fn mid_turn_steer_preserves_turn_started_timer() {
        let mut app = App::for_test();
        assert!(app.turn_started.is_none());

        // First prompt starts Turn 1 from idle
        app.on_event(runeengine::proto::Event::TurnStart {
            session: app.session.unwrap(),
            turn: 1,
            text: Some("Initial task".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert!(app.busy);
        let first_start = app
            .turn_started
            .expect("turn_started should be initialized");

        // Simulate some elapsed time on turn 1
        std::thread::sleep(std::time::Duration::from_millis(50));
        let elapsed_before = first_start.elapsed();

        // While busy, a mid-turn steer turn arrives via TurnStart
        app.on_event(runeengine::proto::Event::TurnStart {
            session: app.session.unwrap(),
            turn: 2,
            text: Some("Steer response".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert!(app.busy);
        let second_start = app
            .turn_started
            .expect("turn_started should still be present");
        assert_eq!(
            first_start, second_start,
            "mid-turn steer must preserve turn_started and NOT reset the timer"
        );
        assert!(second_start.elapsed() >= elapsed_before);

        // Turn ends via Done
        app.on_event(runeengine::proto::Event::Done {
            session: app.session.unwrap(),
            turn: 2,
            stop: runeengine::proto::StopReason::End,
        });
        assert!(!app.busy);
        assert!(
            app.turn_started.is_none(),
            "turn_started should reset to None when turn genuinely finishes"
        );

        // Next prompt starts a fresh timer from idle
        app.on_event(runeengine::proto::Event::TurnStart {
            session: app.session.unwrap(),
            turn: 3,
            text: Some("Next prompt".into()),
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert!(app.busy);
        let third_start = app.turn_started.expect("new prompt gets new turn_started");
        assert_ne!(first_start, third_start);
    }

    #[test]
    fn session_active_seconds_clock_runs_when_resumed_or_new() {
        let mut app = App::for_test();
        app.accumulated_seconds = 10;
        app.run_started_at = runeengine::store::now_ms() - 2500;
        let active = app.current_active_seconds();
        assert!(
            active >= 12,
            "accumulated seconds should include live elapsed seconds: got {active}"
        );
    }

    fn wheel_at(up: bool, row: u16, col: u16, ctrl: bool, alt: bool) -> Key {
        use crate::ui::input::{Click, Modifiers, Mouse};
        Key::Mouse(Mouse {
            kind: if up { Click::WheelUp } else { Click::WheelDown },
            row,
            col,
            modifiers: Modifiers { ctrl, alt },
        })
    }

    /// 2C: with an overlay up, the wheel moves the overlay in its own place;
    /// the conversation under it stays exactly where it was.
    #[test]
    fn the_wheel_over_an_overlay_moves_the_overlay_and_not_the_chat() {
        let mut d = test_driver("wheel-overlay");
        let mut app = App::for_test();
        app.sidebar = false;
        for n in 0..200 {
            app.blocks.push(Block::Notice {
                text: format!("history {n}"),
                bad: false,
            });
        }
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 100);
        app.scroll_by(true, 40);
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 100);
        let chat = app.scroll;
        app.panel = Panel::Help;
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 100);
        app.on_key(wheel_at(true, 5, 5, false, false), &mut d);
        assert!(app.panel_view.scroll > 0, "the overlay did not scroll");
        assert_eq!(app.scroll, chat, "the chat moved under the overlay");
        d.engine.shutdown();
    }

    /// 2B: Ctrl or Alt held on the wheel over the chat goes three times as
    /// far; the same notch without them goes the ordinary distance.
    #[test]
    fn a_ctrl_or_alt_wheel_over_the_chat_goes_further() {
        let mut d = test_driver("wheel-fast");
        let base = crate::ui::runtime_configuration().number("ui.scroll_lines") as usize;
        let notch = |ctrl: bool, alt: bool, d: &mut Driver| {
            let mut app = App::for_test();
            app.sidebar = false;
            app.terminal_keeps_up = false;
            app.drawn_rows = 400;
            app.drawn_viewport = 20;
            app.on_key(wheel_at(true, 5, 5, ctrl, alt), d);
            app.scroll
        };
        assert_eq!(notch(false, false, &mut d), base);
        assert_eq!(notch(true, false, &mut d), base * crate::ui::FAST_WHEEL);
        assert_eq!(notch(false, true, &mut d), base * crate::ui::FAST_WHEEL);
        d.engine.shutdown();
    }

    /// 2A: Ctrl+↓ is back to the newest row from anywhere — the composer
    /// holding text, a question card up — and leaves the composer alone.
    #[test]
    fn ctrl_down_jumps_to_the_latest_row_from_anywhere() {
        let mut d = test_driver("ctrl-down");
        let mut app = App::for_test();
        app.input = "half a question".into();
        app.cursor = 4;
        app.scroll_by(true, 30);
        app.on_key(Key::CtrlDown, &mut d);
        assert_eq!(app.scroll, 0);
        assert!(app.scroll_anchor.is_none() && app.glide.is_none());
        assert_eq!((app.input.as_str(), app.cursor), ("half a question", 4));
        d.engine.shutdown();
    }

    /// 2A: a click on the hint jumps; a click beside it on the same row does
    /// not.
    #[test]
    fn a_click_on_the_hint_jumps_and_one_beside_it_does_not() {
        use crate::ui::input::{Click, Modifiers, Mouse};
        let mut d = test_driver("hint-click");
        let mut app = App::for_test();
        app.sidebar = false;
        for n in 0..100 {
            app.blocks.push(Block::Notice {
                text: format!("history {n}"),
                bad: false,
            });
        }
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 120);
        app.scroll_by(true, 10);
        let _ = crate::ui::draw::compose_for_test(&mut app, 24, 120);
        let (row, first, last) = app.jump_hint.expect("the hint is up");
        let click = |col: u16| {
            [Click::Press, Click::Release].map(|kind| {
                Key::Mouse(Mouse {
                    kind,
                    row,
                    col,
                    modifiers: Modifiers::NONE,
                })
            })
        };
        for key in click(first.saturating_sub(3).max(1)) {
            app.on_key(key, &mut d);
        }
        assert!(app.scroll > 0, "a click beside the hint jumped");
        for key in click((first + last) / 2) {
            app.on_key(key, &mut d);
        }
        assert_eq!(app.scroll, 0, "a click on the hint did not jump");
        d.engine.shutdown();
    }
}
