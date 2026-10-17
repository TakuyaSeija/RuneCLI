//! The interactive screen.
//!
//! # Shape
//!
//! One thread owns the engine and the screen. Everything that can arrive —
//! keystrokes, engine events, and the animation tick — is funnelled into a
//! single channel, so there is exactly one place where state changes and
//! exactly one place where the screen is drawn. No locks, no shared mutable
//! state between threads, and no possibility of a half-applied frame.
//!
//! ```text
//!   stdin ──► input thread ──┐
//!   engine ─► forwarder  ────┼──► Msg ──► App::handle ──► draw
//!   10 Hz  ─► ticker     ────┘
//! ```
//!
//! # Why a full screen rather than inline
//!
//! The session panel is drawn **beside the transcript**, and a panel cannot be
//! drawn next to scrollback the program no longer owns. So Rune takes the
//! alternate screen, keeps its own scrollback, and hands the terminal back
//! untouched on the way out — printing the tail of the conversation as it goes,
//! so a session still leaves something behind.

pub mod ansi;
pub mod banner;
pub mod blocks;
#[path = "Composer.rs"]
pub mod composer;
pub mod draw;
mod editing;
mod editor;
#[path = "Highlight.rs"]
pub mod highlight;
pub mod input;
mod keys;
pub mod markdown;
mod panel_view;
pub mod panels;
#[cfg(test)]
mod source_style_tests;
use panel_view::PanelView;
mod permission;
pub mod picker;
pub mod pulse;
#[path = "Questions.rs"]
mod questions;
pub mod settings;
mod sgr;
pub mod steps;
pub mod term;
pub mod theme;
mod undo;
mod viewport;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use runeengine::{Command, Event};
use serde_json::Value;

use crate::driver::Driver;
use blocks::{Block, Tool, ToolState};
use input::Key;

/// Counter animation cadence, matching a browser's requestAnimationFrame. The
/// event loop skips paint when no visible value changed, so idle Rune does not
/// compose sixty identical frames per second.
#[cfg(test)]
const TICK: Duration = Duration::from_millis(16);

/// How long the hands rest on the keyboard before a window being typed into
/// hands itself over to a newer build (owner, 2026-10-09).
pub const HANDOVER_IDLE: Duration = Duration::from_secs(3);

/// Transient status lines disappear after 8 seconds so the screen reverts to
/// showing the idle state / mode explanation.
/// A single ctrl+c arm expires after 4 seconds if not confirmed.
pub(crate) enum Msg {
    Key(Key),
    Event(Event),
    Tick,
    RemoteCommand(runeengine::proto::Command),
    SessionDetached(i64),
    HostUpgrade(i64),
    /// The event stream of a session ended without its host saying it was
    /// upgrading: the host went away, or the connection never reached the
    /// host that serves the session now. The stream's number tells the
    /// current one from one this window let go of on purpose.
    HostGone(i64, u64),
    /// The worker found the session's next host.
    Reconnected(
        i64,
        crate::session_ipc::SessionClient,
        std::sync::mpsc::Receiver<Event>,
    ),
    /// The worker could neither reach a host nor start one.
    ReconnectFailed(i64, String),
}

/// Which overlay is open, if any.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Panel {
    None,
    Settings,
    Tasks,
    Undo,
    /// Put back what an undo replaced (`/redo`).
    Redo,
    Help,
    Capabilities,
    /// What happened behind the conversation (`/logs`).
    Logs,
    /// What the model holds that the conversation does not show (`/context`).
    Context,
}

/// How many lines of a session's log the panel holds — the newest ones.
const LOG_LINES: usize = 2000;

/// What an outstanding request was for, so its answer can be put somewhere.
///
/// The screen never blocks on the engine, so every question it asks is a row in
/// this table until the matching ack arrives.
#[derive(Clone, PartialEq)]
pub enum Awaiting {
    /// Settings, in the shape the panel draws, read when the preference
    /// requests numbered up to `serial` had been asked. An answer read before a
    /// later change is older than that change's own answer and never replaces
    /// it (a draft's model snapped back to the one before, owner 2026-10-15).
    Settings {
        serial: u64,
    },
    DraftOpened,
    DraftSaved,
    ComposerEdit,
    Promoted,
    Providers,
    Capabilities,
    Catalogue(String),
    Prompt {
        text: String,
        media: Vec<runeengine::media::Attachment>,
    },
    /// A mode switch, shown before the engine agreed: `previous` is what the
    /// footer goes back to if it refuses, so the screen never claims a mode the
    /// engine does not enforce.
    Mode {
        previous: String,
        next: String,
    },
    /// The resume that re-registers this window with a new host.
    Resumed,
    /// Undoing or redoing one file (`checkpoint.restore` / `checkpoint.redo`).
    Restored {
        file: String,
        side: undo::Side,
    },
    /// The files `/undo` or `/redo` lists, asked for as the panel opens.
    UndoFiles {
        side: undo::Side,
    },
    /// What undoing or redoing one file would change (Alt+S).
    UndoDiff {
        file: String,
        side: undo::Side,
    },
    NewSession,
    Compact,
    Sessions,
    /// The session's log, read from disk when the panel opens.
    Logs,
    /// What the model holds beyond the conversation, asked of the engine when
    /// the panel opens.
    Context,
    /// The session's view, asked for after a reconnect.
    View,
    Preferences(settings::Request),
    ModelConfig(String, String, Option<i64>),
}

#[path = "McpInput.rs"]
mod mcp_input;
mod navigation;
#[cfg(test)]
mod navigation_tests;

/// One message the engine has not yet said it took: sent from this window, or
/// queued from another window of the session. Drawn where it was typed until
/// the engine's `TurnStart` for it, which only recognises it; never sent again
/// from here — the engine holds the queue.
#[derive(Clone, Debug)]
pub struct QueuedSteer {
    pub text: String,
    /// Typed in another window of the session: drawn here so every window shows
    /// the same, never sent from here.
    pub remote: bool,
    /// The composer entry the engine filed this message under, once it has: the
    /// message is one thing, and the engine's word on it must not draw it twice.
    pub pending_id: Option<i64>,
}

/// The messages waiting for the engine, in the order they were typed.
#[derive(Clone, Debug)]
pub struct QueuedPrompt {
    pub steers: Vec<QueuedSteer>,
    pub first_user_block: usize,
}

/// Which plan item the side panel was cut at, and how far into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanAnchor {
    pub item: usize,
    pub row: usize,
}

/// Where the reader is in the conversation, held by what they are reading
/// rather than by a row number.
///
/// # Why the text is not enough
///
/// The first non-blank row on screen, as text, survives rows changing above
/// it while the width holds. It does not survive a new width: the side panel
/// opening, or the terminal resized, wraps every paragraph again, the row's
/// text is gone, and the old fallback kept the *row number* — which after a
/// reflow is a different part of the conversation (the reader at paragraph 130
/// landed on 65 when the side panel opened). [`Place`] names the drawn group
/// the row belongs to and how far into it the row sat, so a new layout finds
/// the same part of the same group.
#[derive(Clone, Debug, PartialEq)]
pub struct Anchor {
    /// The row's text, colour taken out.
    pub text: String,
    /// The row's distance from the top of the screen.
    pub offset: usize,
    /// The group the row was drawn in, when it was in one (the wordmark is
    /// not).
    pub place: Option<Place>,
}

/// A row as a position inside its drawn group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    pub group: usize,
    pub row: usize,
    /// The group's height when the row was taken, so a reflowed group can be
    /// entered at the same fraction of its height.
    pub rows: usize,
    /// The width the group was laid out at.
    pub width: usize,
}

/// How many times further a notch goes with Ctrl or Alt held (owner,
/// 2026-10-09: two rows a notch, six with the modifier).
pub const FAST_WHEEL: usize = 3;

/// What a wheel gesture still owes the transcript, in rows: positive toward
/// the top, negative toward the live end. Paid in shares by
/// [`App::glide_step`].
#[derive(Debug, Clone, Copy)]
pub struct Glide {
    pub owed: isize,
}

pub struct App {
    /// The release the session's host runs, as it reported it — what the
    /// window follows when the host is later (`is_cli_outdated`). Never what
    /// the screen calls this window's version: that is the build this process
    /// runs, `runeengine::VERSION` (owner, 2026-10-10 — a window still on
    /// 0.3.7 showed its host's 0.3.8 and looked updated when it was not).
    pub host_version: String,
    /// Whether this window said a newer build is installed where it cannot
    /// become that build in place (`platform::REPLACES_IN_PLACE`): said once.
    pub told_newer_build: bool,
    pub queued_prompt: Option<QueuedPrompt>,
    pending_model_selection: Option<(Option<i64>, String, String)>,
    pub mcp_input: Option<mcp_input::Form>,
    pub capability_view: Value,
    pub blocks: Vec<Block>,
    pub input: String,
    pub(super) input_editor: editing::State,
    /// Files going with the next question.
    ///
    /// Held here rather than written into the text, because a path in a sentence
    /// is a path the model reads and an attachment is a file the model *sees* —
    /// and only one of those two can be taken back with a keystroke.
    pub attached: Vec<runeengine::media::Attachment>,
    pub attachment_catalogues: std::collections::HashSet<String>,
    pub cursor: usize,
    /// The composer's text column width as of the last frame — the width its
    /// soft wrap happened at.
    ///
    /// `↑` and `↓` have to move the caret by **what the reader sees**, one
    /// wrapped row at a time, and only the renderer knows where the wrap fell.
    /// The composer publishes it here so the keyboard and the eye agree about
    /// what "the line above" means.
    pub composer_width: usize,
    /// First visible wrapped row in a long composer. Kept separately from the
    /// transcript scroll so caret navigation can reveal input without moving chat.
    pub composer_first_row: usize,
    /// Rendered input rows with their exact display-to-source byte map.
    composer_hit_rows: Vec<draw::ComposerRow>,
    /// Sent prompt history for replay via PageUp / PageDown. It belongs to
    /// the session: nothing from the workspace's other sessions is ever
    /// merged in, and `/new` empties it with the rest of the transcript.
    pub prompt_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_draft: String,
    /// Lines scrolled up from the bottom. Zero means following the newest.
    pub scroll: usize,
    /// Geometry of the previous content frame. Together with `scroll`, this
    /// identifies the top row the user is reading so streaming can add lines
    /// beneath it without dragging the viewport down.
    pub drawn_rows: usize,
    pub drawn_viewport: usize,
    /// A non-empty line from the visible viewport and its offset from the top.
    /// This survives activity rows changing above it, where row-count
    /// arithmetic alone would still move the reader.
    pub scroll_anchor: Option<Anchor>,
    /// The overlay's own scroll; see [`PanelView`].
    pub panel_view: PanelView,
    /// The distance a wheel gesture still has to cover; see
    /// [`App::wheel_scroll`].
    pub glide: Option<Glide>,
    /// The drawn conversation, kept between frames so a frame only redraws the
    /// blocks whose own content moved. See [`draw::Transcript`].
    pub transcript_cache: Option<draw::Transcript>,
    /// Marked true whenever blocks, events, verbose mode, or folding change.
    pub transcript_dirty: bool,
    /// The information panel beside the transcript. Session, context, plan —
    /// see [`draw::sidebar`]. It used to hold one step's raw arguments, which
    /// is what the transcript itself now shows properly.
    pub sidebar: bool,
    /// Rows of the panel's list hidden above the scroll window — plan items
    /// first, then the FILE CHANGE rows below them — so the panel starts
    /// partway down the list. Zero means the very first row is at the top.
    ///
    /// Separate from [`App::scroll`] on purpose, and measured from the opposite
    /// end: the transcript counts rows raised from the bottom, because it grows
    /// downwards as a turn streams; the panel's list is read in order, so
    /// counting from the top is what "scrolled past the first screen" means.
    ///
    /// One number for two panes would lose the reader's place in each one on
    /// every move, and a wheel over the panel is not a gesture about the chat
    /// beside it.
    pub plan_scroll: usize,
    /// Where the panel's list was cut, so the next frame can find the same row
    /// again.
    ///
    /// `None` while the panel is following the live end, which is its state
    /// before the reader has touched it and again after they scroll back to the
    /// newest row. A deliberate scroll sets the anchor to `None` — see
    /// [`plan_scroll_by`] — and that is the signal to the renderer that the
    /// number in `plan_scroll` is the reader's own and must not be second
    /// guessed.
    ///
    /// When it *is* `Some`, it is an **item number over the combined list**,
    /// not the text of the row: plan items count `0..n`, the FILE CHANGE
    /// heading is one item at `n`, and its rows continue `n+1..n+1+m` in
    /// first-touch order. A row's text changes under the reader while they are
    /// looking at it — `○` becomes `◐` becomes `●`, and the heading's count
    /// moves with it — so a text anchor loses its own line and the panel
    /// jumps. An item's number never moves: both sections are append-only and
    /// their order is fixed, so "the reader was looking at item 20" stays true
    /// while item 3 finishes and the list grows.
    pub plan_anchor: Option<PlanAnchor>,
    /// The rows of the ask-user card on the last frame, as one-based **screen**
    /// rows, published for the wheel: a card that overflows owns the wheel
    /// only where the card actually is, and the transcript above it stays the
    /// reader's — the row-scoped rule the side panel lives by. `None` whenever
    /// no card is up.
    pub questions_card: Option<(u16, u16)>,
    /// Where the side panel sits on screen, as a one-based column range, or
    /// `None` when the split is off or the terminal is too narrow to have one.
    ///
    /// The wheel reads this to answer "what is under the pointer". Recomputing
    /// the split here instead would be a second layout calculation that agrees
    /// with the renderer's until the day one of the two is edited — see the
    /// note on [`draw::Hit`], which is the same argument for the same reason.
    pub sidebar_cols: Option<(u16, u16)>,
    pub sidebar_last_row: u16,
    /// Where the last frame drew the jump-to-latest hint: its one-based screen
    /// row and first and last column. The renderer's geometry, published for
    /// the click — the same argument as `sidebar_cols`.
    pub jump_hint: Option<(u16, u16, u16)>,
    /// How far the panel's list — plan items and FILE CHANGE rows together —
    /// can be scrolled, in rows, as of the last frame.
    ///
    /// Published by the renderer because the renderer is what knows: the height
    /// of the panel depends on the footer, and the height of the list depends on
    /// how each row wraps. The wheel asks this to decide whether the panel can
    /// move before it takes the gesture, and a list that fits entirely must not
    /// be able to steal it.
    pub plan_max: usize,
    pub verbose: bool,
    pub panel: Panel,
    pub panel_row: usize,
    pub preferences: settings::State,
    /// What each row of the last frame was, so a click can be answered from the
    /// layout that produced it rather than from a second guess at it.
    pub hits: Vec<draw::Hit>,
    /// Terminal screen coordinate (row, col) for the hardware cursor (1-indexed).
    /// When `Some`, the terminal displays a steady bar cursor at that cell.
    /// When `None` (e.g. during permission question or idle), hardware cursor is hidden.
    pub cursor_terminal: Option<(u16, u16)>,
    /// Drag selection and the unstyled rows it selects from. Keeping the rows
    /// from the exact frame prevents a second renderer from guessing what was
    /// under a coordinate.
    pub selection: Option<draw::Selection>,
    pub selection_rows: Vec<String>,
    /// Which command the `/` palette has under its cursor. Reset by every
    /// keystroke that changes what is being searched for.
    pub palette_row: usize,
    /// The top visible item index in the `/` palette, maintaining scroll margin.
    pub palette_scroll: usize,
    /// Which answer the permission card has under its cursor.
    pub ask_choice: usize,
    permission: permission::State,
    pub session: Option<i64>,
    pub composition: composer::State,
    pub project: String,
    pub mode: String,
    pub settings: Value,
    pub providers: Value,
    pub pending_provider_refresh: bool,
    /// The configuration generation this window last followed
    /// ([`App::reload_configuration`]).
    pub config_generation: u64,
    /// `ui.mouse` changed: the terminal is told at the next frame.
    pub mouse_dirty: bool,
    pub busy: bool,
    /// The model's context window, for the meter in the sidebar. Zero until the
    /// settings arrive, and the meter simply does not draw until then rather
    /// than inventing a denominator.
    pub context: u64,
    /// The strongest thing happening right now. Never regresses to "Thinking"
    /// once real work has started — a label that goes backwards reads as the
    /// work being undone.
    pub label: String,
    pub frame: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The part of the newest prompt the provider served from its cache.
    pub cached_tokens: u64,
    /// Context window tokens used in the latest request.
    pub context_used: u64,
    /// Cumulative input tokens consumed in this session.
    pub tok_in: u64,
    /// Cumulative output tokens produced in this session.
    pub tok_out: u64,
    /// Cumulative cache tokens consumed in this session.
    pub cache_in: u64,
    /// What this session has cost so far, in US dollars — accumulated per call.
    pub cost: f64,
    pub unpriced_calls: u64,
    /// Whether the terminal is reporting the pointer to us. Read from
    /// `~/.rune/rune.toml` at start-up and switchable from the settings
    /// screen. When on, Rune owns selection and copies it on release; when off,
    /// the terminal owns selection and alternate scrolling.
    pub mouse: bool,
    /// The active visual color theme.
    pub theme: theme::ThemeMode,
    /// Spend, in dollars, at which to say something once. Zero is off, which is
    /// the default — see `Limits::warn_spend_usd`.
    pub warn_spend: f64,
    /// Whether that has already been said. A guardrail that repeats every turn
    /// is one the reader learns to skip past.
    warned_spend: bool,
    pub turn_started: Option<Instant>,
    pub pending: Option<blocks::Ask>,
    pub questions: Option<questions::State>,
    pub tasks: Vec<Value>,
    /// The session's log: read when the panel opens, extended live.
    pub logs: Vec<runeengine::proto::LogEntry>,
    /// The engine's answer to `session.context`, while `/context` is open.
    pub context_view: Value,
    /// A view of the session has been asked for and has not come: whatever the
    /// stream says until it does is inside it, so none of it is applied.
    pub awaiting_view: bool,
    /// The composer entries (messages waiting for the running turn) that are
    /// already on screen, so none is drawn twice.
    pub shown_pending: std::collections::HashSet<i64>,
    /// This window drew the message it just sent before the engine stored it;
    /// the engine's word on the turn is that block's echo, not a new message.
    /// The last turn the stored conversation drew on this screen. A session's
    /// first message is stored by the window that wrote it before its host
    /// announces it, so a view taken between the two holds it as history and
    /// the announcement that follows is not a message to draw again.
    pub stored_turn: i64,
    /// The terminal draws a glide as motion (`draw::Screen::keeps_up`: it
    /// shows frames whole and takes them fast). Where it does not, a wheel
    /// notch jumps: on Konsole a glide was 445 ms of stutter after the hand
    /// stopped. Until the terminal has answered, as before it was asked.
    pub terminal_keeps_up: bool,
    /// The least severe line the panel shows (`/logs warn`, `/logs error`).
    pub log_floor: runeengine::proto::LogLevel,
    /// The line whose detail is open in the panel.
    pub log_open: Option<usize>,
    pub session_picker: Option<picker::SessionPicker>,
    pub pending_session_switch: Option<i64>,
    /// Notifications shown when they were raised and not yet read by the
    /// model: the `turn_start` that hands each one over draws nothing new.
    pub notified: Vec<String>,
    /// The undo panel's file diff (Alt+S), shown in place of its list. The
    /// list itself is `file_changes`: the side panel's rows. See [`undo`].
    pub undo_diff: Option<undo::Shown>,
    /// The restore a first Enter armed; a second one within
    /// [`undo::CONFIRM`] does it.
    pub undo_armed: Option<undo::Arm>,
    /// Why the engine refused the last diff or restore, said in the undo
    /// panel until the choice changes. The status line is the turn's, and the
    /// turn's next word replaced it within a second.
    pub undo_refused: Option<String>,
    /// The files there is a redo for, as `checkpoint.redos` (or the last
    /// undo or redo) answered: what `/redo` lists.
    pub redo_files: Vec<Value>,
    /// What this session has done to every file it touched — the engine's
    /// session tally, one row per path with its session-long `+N −N`. Delivered
    /// on resume, re-sent by every round that changed it, and refreshed after a
    /// restore. The side panel's FILE CHANGE section renders it.
    pub file_changes: Vec<Value>,
    pub status: Option<String>,
    pub status_at: Option<Instant>,
    /// Backoff owns the footer until the retry announces API calling (wait=0).
    pub retry_status: Option<String>,
    pub sandbox: String,
    pub sandbox_ok: bool,
    pub escapes: Vec<String>,
    pub awaiting: std::collections::HashMap<u64, Awaiting>,
    pub title: Option<String>,
    pub started_at: i64,
    pub run_started_at: i64,
    pub accumulated_seconds: u64,
    pub steps: usize,
    pub turns: usize,
    /// Set after the first ctrl+c; another within the deadline leaves Rune.
    pub quit_armed: bool,
    pub quit_armed_at: Option<Instant>,
    /// Set after the first Esc while AI is running; a second Esc stops it.
    pub stop_armed: bool,
    pub stop_armed_at: Option<Instant>,
    /// Set by `ctrl+l`: the next paint must redraw every row, not only the ones
    /// whose frame text moved. The one way back from a screen the differential
    /// painter can no longer trust — see [`draw::Screen::render`].
    pub resync: bool,
    /// A reconnect finished: try handing this window over to the installed
    /// build at the next chance.
    pub handover_due: bool,
    /// The host speaks another protocol version. Handover then no longer
    /// waits for an empty composer — the input travels with it.
    pub protocol_skew: bool,
    /// Keys read from the terminal and applied here, counted as the reader
    /// counts the ones it sent ([`input::sent`]).
    pub keys_applied: u64,
    /// When the last of them came.
    pub last_key: Option<Instant>,
    /// Handovers in a row that led to this window, and when the last was —
    /// carried into the next one, so a build that keeps handing over to
    /// itself is stopped.
    pub handover_chain: crate::handover::Chain,
    pub quit: bool,
}

pub(crate) fn runtime_configuration() -> &'static runeengine::settings::configuration::Shared {
    static CONFIG: std::sync::OnceLock<runeengine::settings::configuration::Shared> =
        std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        settings_home()
            .and_then(|home| runeengine::settings::configuration::Shared::load(&home))
            .unwrap_or_default()
    })
}

/// The home whose `rune.toml` a window reads. A test reads none and runs on
/// the defaults in `FIELDS`: reading the owner's live file made a test's
/// result depend on how the owner had set Rune up (found 2026-10-09, when the
/// owner's `ui.scroll_lines = 3` failed a test of the default).
fn settings_home() -> runeengine::Result<std::path::PathBuf> {
    #[cfg(test)]
    {
        Err(runeengine::Error::NotFound(
            "a test reads no configuration".into(),
        ))
    }
    #[cfg(not(test))]
    {
        runeengine::store::Db::home_dir()
    }
}

impl App {
    pub fn current_active_seconds(&self) -> u64 {
        if self.run_started_at > 0 {
            let elapsed = (runeengine::store::now_ms() - self.run_started_at).max(0) / 1000;
            self.accumulated_seconds.saturating_add(elapsed as u64)
        } else {
            self.accumulated_seconds
        }
    }

    pub fn set_theme(&mut self, next: theme::ThemeMode) {
        #[cfg(not(test))]
        {
            let saved = runeengine::store::Db::home_dir().and_then(|home| {
                runeengine::settings::configuration::Configuration::edit(
                    &home,
                    Some("ui.theme"),
                    Some(serde_json::Value::String(next.name().to_string())),
                    false,
                    None,
                )
            });
            if saved.is_err() {
                self.status = Some(
                    "Could not save the theme. Check configuration access and try again.".into(),
                );
                return;
            }
        }
        self.theme = next;
        theme::set_theme(next);
        self.transcript_cache = None;
        self.transcript_dirty = true;
        self.resync = true;
        self.status = Some(format!("Theme: {}", next.label()));
    }

    pub fn set_sidebar(&mut self, enabled: bool) {
        #[cfg(not(test))]
        {
            let _ = runeengine::store::Db::home_dir().and_then(|home| {
                runeengine::settings::configuration::Configuration::edit(
                    &home,
                    Some("ui.sidebar"),
                    Some(serde_json::Value::Bool(enabled)),
                    false,
                    None,
                )
            });
        }
        self.sidebar = enabled;
        self.transcript_dirty = true;
        self.resync = true;
    }
}

/// A real engine on a throwaway database, for the tests that need one.
///
/// The keyboard cannot be tested without one: every key that does anything sends
/// a command, and a fake would be testing the fake. One helper, shared, because
/// two ways of starting an engine for a test is two ways for a test to be
/// starting a different engine from the one it means to.
#[cfg(test)]
pub fn test_driver(name: &str) -> Driver {
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    Driver::start(crate::driver::Options {
        db: Some(base.join("rune.db")),
        read_only: false,
        script: None,
        request_next: None,
    })
    .expect("the engine should start")
}

#[cfg(test)]
impl App {
    /// A screen with nothing in it, for the rendering tests. Not a shortcut past
    /// the real constructor: it goes through it.
    pub fn for_test() -> App {
        let mut app = App::new(
            Some(1),
            "/tmp/project".into(),
            &runeengine::proto::SandboxReport {
                kind: "landlock".into(),
                abi: 6,
                enforced: true,
                reason: None,
                escapes: Vec::new(),
            },
        );
        // App::new reads the developer's real ~/.rune for theme/sidebar/mouse/spend,
        // which makes any UI test that asserts an initial value host-dependent (a
        // `sidebar = true` in the owner's rune.toml fails `!app.sidebar`). Pin every
        // environment-read field to the engine default so a test means the same thing
        // on every machine.
        let defaults = runeengine::limits::Limits::default();
        app.theme = theme::ThemeMode::Violet;
        app.sidebar = defaults.sidebar();
        app.mouse = defaults.mouse();
        app.warn_spend = defaults.warn_spend_usd();
        app.composer_width = 80;
        app
    }

    /// The same screen, with a settings payload already in place.
    pub fn for_test_with(settings: Value) -> App {
        let mut app = App::for_test();
        app.take_settings(settings);
        app
    }
}

/// Open the window. `handed` is what a previous window handed over when it
/// activated this build (`crate::handover`); `refused` says why a handover
/// could not be read, when one could not.
pub fn run(
    mut d: Driver,
    project: String,
    resume: Option<i64>,
    handed: Option<crate::handover::Window>,
    refused: Option<String>,
) -> Result<(), String> {
    if !term::is_tty() {
        return Err(
            "rune's screen needs a terminal. Use `rune -p \"…\"` in a pipe, or `rune -host`."
                .into(),
        );
    }

    // One resume answers everything: the session id, the session's mode,
    // title, tallies, usage and its file ledger. It used to be asked for
    // twice — once for the id, once for the payload — and the file ledger
    // means each ask re-reads every changed file and diffs it, so the
    // duplication was the single largest line on the startup clock.
    let (session, resume_res) = match resume {
        Some(id) => {
            #[cfg(not(test))]
            {
                // Back from its own handover, this window's host is most likely
                // between builds too: it is looked for as a reconnecting window
                // looks, patiently, before another is started beside it.
                let found = if handed.is_some() {
                    crate::driver::find_host(id, crate::driver::RESUME_DEADLINE)
                } else {
                    crate::session_ipc::SessionClient::connect(id).ok()
                };
                if let Some((client, rx)) = found {
                    d.client = Some(client);
                    let local = std::mem::replace(&mut d.events, rx);
                    d.local_events = Some(local);
                } else {
                    d.attach_detached(id, &project)?;
                }
            }
            let (id, res) = match d.resume(id, Some(project.clone())) {
                Ok(pair) => pair,
                #[cfg(not(test))]
                Err(e)
                    if d.client.is_some()
                        && !e.starts_with("not found:")
                        && !e.starts_with("invalid:") =>
                {
                    if let Some(old) = d.client.take() {
                        old.disconnect();
                    }
                    // A timeout does not prove the owner is dead. Never unlink
                    // its socket and start a second writer for the same session.
                    return Err(format!(
                        "Session #{id} host did not complete resume: {e}. The existing host and session data were left untouched."
                    ));
                }
                Err(e) => return Err(e),
            };
            (Some(id), Some(res))
        }
        None => {
            let data = d.call(|id| Command::DraftOpen {
                id,
                project: project.clone(),
                token: handed.as_ref().and_then(|w| w.draft.clone()),
            })?;
            (None, Some(data))
        }
    };
    #[cfg(not(test))]
    if let Some(session) = session.filter(|_| d.client.is_none()) {
        d.attach_detached(session, &project)?;
    }
    let report = session
        .and_then(|session| d.engine.sandbox_of(session).ok())
        .map(|s| s.report().clone())
        .unwrap_or_else(|| runeengine::proto::SandboxReport {
            kind: "none".into(),
            abi: 0,
            enforced: false,
            reason: None,
            escapes: Vec::new(),
        });

    let mut app = App::new(session, project, &report);
    if handed.is_some() {
        app.note(format!(
            "Rune CLI updated to v{} (live).",
            runeengine::VERSION
        ));
    }
    if let Some(why) = refused {
        app.note(format!(
            "Rune was updated, but what the previous window handed over could not be read ({why}); it starts fresh."
        ));
    }
    theme::set_theme(app.theme);
    // Through the same door the ack takes, so the first frame is not missing
    // what every later one has: the context meter had no denominator until the
    // user happened to change a setting.
    app.providers = d
        .call(|id| Command::ProviderList { id })
        .unwrap_or(Value::Null);
    if let Some(session) = session {
        app.take_settings(
            d.call(|id| Command::SettingsGet { id, session })
                .unwrap_or(Value::Null),
        );
        if let Ok(saved) = d.call(|id| Command::ComposerGet { id, session }) {
            if let Some(composer) = saved
                .get("composer")
                .and_then(|state| serde_json::from_value(state.clone()).ok())
            {
                app.restore_composer(composer, true);
            }
        }
    } else if let Some(saved) = resume_res.as_ref() {
        let draft =
            serde_json::from_value(saved["draft"].clone()).map_err(|error| error.to_string())?;
        app.take_draft(draft, saved.get("settings").cloned());
    }
    // Over whatever the composer restored: what was on screen a moment ago
    // is what the person was looking at.
    if let Some(window) = handed {
        app.restore_handover(window);
    }
    app.sync_active_models();
    app.mode = resume_res
        .as_ref()
        .and_then(|v| v["mode"].as_str().map(String::from))
        .unwrap_or_else(|| app.mode.clone());
    if let Some(res) = &resume_res {
        if let Some(v) = res.get("version").and_then(|v| v.as_str()) {
            app.host_version = v.to_string();
        }
    }
    // Everything else the screen shows of the session — the state, the
    // conversation, and the turn that may be running right now — is the
    // engine's one answer, taken at one place in the stream.
    if let Some(session) = session {
        match d.view(session) {
            Ok(view) => app.apply_view(&view),
            Err(e) => app.note(format!("Could not read the session: {e}")),
        }
    }
    // Pre-populate prompt history from this session alone: pgup replays what
    // was said *here*, never what the workspace's other sessions said. A new
    // session therefore starts with a composer that has nothing behind it.
    // (`replay` above has already filled this on a resume; the fetch below is
    // the belt for the paths it does not cover.)
    if let Some(session) = session.filter(|_| app.prompt_history.is_empty()) {
        if let Ok(hist) = d.call(|id| Command::SessionHistory { id, session }) {
            if let Some(msgs) = hist["messages"].as_array() {
                for m in msgs {
                    if m["role"].as_str() == Some("user") {
                        let text = m["content"].as_str().unwrap_or("").trim();
                        if !text.is_empty()
                            && !text.starts_with('/')
                            && !app.prompt_history.contains(&text.to_string())
                        {
                            app.prompt_history.push(text.to_string());
                        }
                    }
                }
            }
        }
    }

    term::guard_panics();
    let mut terminal = Some(term::enter_or_report(app.mouse)?);
    let (tx, rx) = channel::<Msg>();
    for ev in std::mem::take(&mut d.pending_events) {
        let _ = tx.send(Msg::Event(ev));
    }
    spawn_sources(&tx, std::mem::replace(&mut d.events, channel().1), session);

    let mut screen = draw::Screen::default();
    let (mut rows, mut cols) = match terminal.as_ref() {
        Some(t) => t.size(),
        None => (24, 80),
    };
    screen.paint(&mut app, rows, cols, true);
    screen.ask();
    // A frame the terminal was not ready for (see `Screen::paint`): sent as soon
    // as it is, composed from the state as it is then.
    let mut owed = false;
    let mut sized_terminal: Option<(i64, u16, u16)> = None;

    while !app.quit {
        let previous_session = app.session;
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        let mut repaint = match msg {
            Msg::Key(Key::Terminal(report)) => {
                screen.heard(report);
                app.terminal_keeps_up = screen.keeps_up();
                owed
            }
            Msg::Key(key) => {
                app.typed();
                app.on_key(key, &mut d);
                true
            }
            Msg::Event(event) => {
                if d.observe(&event) {
                    app.on_event(event);
                }
                true
            }
            Msg::Tick => {
                let changed = app.tick();
                if !app.quit
                    && !app.quit_armed
                    && !app.busy
                    && app.queued_prompt.is_none()
                    && !d.reconnecting
                    && app.frame.is_multiple_of(60)
                    && perform_cli_handover(&mut app, &mut d, &mut terminal)
                {
                    return Ok(());
                }
                changed
            }
            Msg::RemoteCommand(cmd) => {
                d.engine.handle(cmd);
                false
            }
            Msg::SessionDetached(_) => false,
            Msg::HostUpgrade(id) => {
                start_reconnect(&mut app, &mut d, &tx, id);
                true
            }
            Msg::HostGone(id, stream) => {
                if stream == EVENT_STREAM.load(std::sync::atomic::Ordering::Relaxed) && !app.quit {
                    start_reconnect(&mut app, &mut d, &tx, id);
                }
                true
            }
            Msg::Reconnected(id, client, events) => {
                finish_reconnect(&mut app, &mut d, &tx, id, client, events);
                true
            }
            Msg::ReconnectFailed(id, error) => {
                if Some(id) == app.session && d.reconnecting {
                    app.note(format!(
                        "The updated session host could not be reached ({error}); session data was preserved."
                    ));
                    d.reconnecting = false;
                    app.quit = true;
                }
                true
            }
        };
        // Continuous model deltas, remote IPC commands, and fast wheel events
        // can keep `try_recv()` nonempty indefinitely. Draining until empty
        // then postpones paint indefinitely, so some rows lag or vanish for a
        // few frames despite fast rendering. Bound each burst: preserve every
        // event in the queue, give the terminal a frame, then resume reading.
        for _ in 0..16 {
            if app.session != previous_session || app.pending_session_switch.is_some() {
                // `/new` must switch sockets before accepting the next key:
                // forwarding a new-session prompt to the old socket can lose
                // its first streamed deltas before this window reconnects.
                break;
            }
            let Ok(next) = rx.try_recv() else { break };
            match next {
                Msg::Key(Key::Terminal(report)) => {
                    screen.heard(report);
                    app.terminal_keeps_up = screen.keeps_up();
                    repaint |= owed;
                }
                Msg::Key(key) => {
                    app.typed();
                    app.on_key(key, &mut d);
                    repaint = true;
                }
                Msg::Event(event) => {
                    if d.observe(&event) {
                        app.on_event(event);
                    }
                    repaint = true;
                }
                Msg::Tick => repaint |= app.tick(),
                Msg::RemoteCommand(cmd) => {
                    d.engine.handle(cmd);
                }
                Msg::SessionDetached(_) => {}
                Msg::HostUpgrade(id) => {
                    start_reconnect(&mut app, &mut d, &tx, id);
                    repaint = true;
                }
                Msg::HostGone(id, stream) => {
                    if stream == EVENT_STREAM.load(std::sync::atomic::Ordering::Relaxed)
                        && !app.quit
                    {
                        start_reconnect(&mut app, &mut d, &tx, id);
                    }
                    repaint = true;
                }
                Msg::Reconnected(id, client, events) => {
                    finish_reconnect(&mut app, &mut d, &tx, id, client, events);
                    repaint = true;
                }
                Msg::ReconnectFailed(id, error) => {
                    if Some(id) == app.session && d.reconnecting {
                        app.note(format!(
                            "The updated session host could not be reached ({error}); session data was preserved."
                        ));
                        d.reconnecting = false;
                        app.quit = true;
                    }
                    repaint = true;
                }
            }
        }
        if std::mem::take(&mut app.handover_due)
            && perform_cli_handover(&mut app, &mut d, &mut terminal)
        {
            return Ok(());
        }
        let switched = if let Some(target) = app.pending_session_switch.take() {
            app.switch_session(target, &mut d);
            if app.session == Some(target) && Some(target) != previous_session {
                for event in std::mem::take(&mut d.pending_events) {
                    app.on_event(event);
                }
                spawn_event_source(
                    &tx,
                    std::mem::replace(&mut d.events, channel().1),
                    Some(target),
                );
                true
            } else {
                false
            }
        } else {
            false
        };
        if app.session != previous_session && !switched {
            // `/new` changed this window's *session identity*, not merely its
            // content. Switch command routing and the event source together.
            // The old host keeps streaming to other windows still attached to
            // the old session; this window drops its client and reads only the
            // new session's socket from here on. Do not spawn a second keyboard
            // or ticker thread — only the event receiver changes.
            if let Some(next) = app.session {
                let connection = crate::session_ipc::SessionClient::connect(next);
                #[cfg(not(test))]
                let connection = connection.or_else(|_| {
                    d.attach_detached(next, &app.project).map(|_| {
                        (
                            d.client.take().expect("detached host attaches its client"),
                            std::mem::replace(&mut d.events, channel().1),
                        )
                    })
                });
                match connection {
                    Ok((client, events)) => {
                        if let Some(old) = d.client.replace(client) {
                            old.disconnect();
                        }
                        spawn_event_source(&tx, events, Some(next));
                        if let Some(token) = app.composition.promotion.take() {
                            d.handle(Command::PromptCommitted {
                                id: 0,
                                session: next,
                                token,
                            });
                        }
                    }
                    Err(error) => {
                        app.note(format!("Committed session #{next} connection failed: {error}; its first message was preserved."));
                    }
                }
            } else if let Some(old) = d.client.take() {
                old.disconnect();
            }
        }
        if let Some((session, provider, model)) = app.pending_model_selection.take() {
            if session == app.session {
                settings::select_model(&mut app, provider, model, false, &mut d);
            }
        }
        if app.pending_provider_refresh && app.preferences.pending.is_none() {
            app.pending_provider_refresh = false;
            app.refresh(&mut d);
        }
        if terminal.is_some() && std::mem::take(&mut app.mouse_dirty) {
            term::set_mouse(app.mouse);
        }
        app.sync_composer(&mut d, false);
        if !app.composition.submits.is_empty() && app.composition.flight.is_none() {
            app.send_submits(&mut d);
        }
        for event in std::mem::take(&mut d.pending_events) {
            app.on_event(event);
        }
        if let Some(events) = d.local_events.take() {
            spawn_event_source(&tx, events, None);
        }
        let (resized, (r, c)) = match terminal.as_ref() {
            Some(t) => (t.resized(), t.size()),
            None => (false, (rows, cols)),
        };
        if (r, c) != (rows, cols) {
            rows = r;
            cols = c;
        }
        let (tool_rows, tool_cols) = draw::terminal_size(&app, rows, cols);
        let geometry = app.session.map(|session| (session, tool_rows, tool_cols));
        if geometry != sized_terminal {
            if let Some((session, rows, cols)) = geometry {
                d.handle(Command::ToolResize {
                    id: 0,
                    session,
                    rows,
                    cols,
                });
            }
            sized_terminal = geometry;
        }
        if app.quit && !app.sync_composer(&mut d, true) {
            app.quit = false;
            app.quit_armed = false;
            app.quit_armed_at = None;
            app.note("Exit paused: composition is not saved yet. The input was retained; retry after the save completes.");
            repaint = true;
        }
        if repaint || resized || owed {
            owed = !screen.paint(&mut app, rows, cols, resized);
        }
    }

    // The status answers still on their way — one for each frame the terminal
    // has not finished reading — are read here rather than typed into the
    // shell the terminal goes back to.
    while let Some(left) = screen.awaiting_answer() {
        match rx.recv_timeout(left) {
            Ok(Msg::Key(Key::Terminal(report))) => screen.heard(report),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    app.sync_composer(&mut d, true);
    drop(terminal.take());
    app.farewell();
    d.shutdown();
    Ok(())
}

/// Whether this window should hand itself over to the build installed at
/// `exe`: another file is installed there, or its host runs a later release
/// (one installed in place, under the same file). A host on an *earlier*
/// release is not this window's cue — that host takes the installed build over
/// itself when idle; a window that treated it as one exec'd itself over and
/// over meanwhile (measured: 6 times in 25 s, each one a redraw and a
/// reconnect), as every window does that moves before its host.
fn is_cli_outdated(exe: &std::path::Path, host_version: &str) -> bool {
    crate::build::replaced(exe) || crate::build::later(host_version, runeengine::VERSION)
}

pub(crate) fn perform_cli_handover(
    app: &mut App,
    d: &mut Driver,
    terminal: &mut Option<term::Term>,
) -> bool {
    #[cfg(test)]
    {
        let _ = (app, d, terminal);
        false
    }
    #[cfg(not(test))]
    {
        if app.handover_waits() {
            return false;
        }

        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let exe = std::path::PathBuf::from(
            exe.as_os_str()
                .to_string_lossy()
                .trim_end_matches(" (deleted)"),
        );

        if !is_cli_outdated(&exe, &app.host_version) {
            return false;
        }
        // Where a process cannot become another program (Windows), a newer
        // build takes over when Rune is opened again (owner, 2026-10-17): the
        // window says so once, and keeps working on this one.
        if !runeengine::platform::REPLACES_IN_PLACE {
            app.tell_newer_build();
            return false;
        }

        let now_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let Some(chain) = app.next_handover(now_sec) else {
            return false;
        };

        if !crate::build::ready(&exe, Some(&d.db_path)) {
            return false;
        }

        // From here to the `exec` the reader reads nothing: a key typed now
        // waits in the terminal for the next build. One it read before
        // stopping and this window has not applied yet would be lost with
        // this process, so the handover waits for the next look instead.
        let Some(parked) = input::park(std::time::Duration::from_millis(250)) else {
            return false;
        };
        if input::sent() != app.keys_applied {
            return false;
        }

        let _ = app.sync_composer(d, true);
        // The host is told this window comes back, so going away for the exec
        // does not stop the session's work (`session.returning`).
        if let Some(session) = app.session {
            let _ = d.call(|id| Command::SessionReturning { id, session });
        }
        if let Some(old) = d.client.take() {
            old.disconnect();
        }
        if let Some(term) = terminal.take() {
            term.hand_over();
        }

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("-project").arg(&app.project);
        if let Some(session) = app.session {
            cmd.arg("-resume").arg(session.to_string());
        }
        cmd.arg("-db").arg(&d.db_path);
        if d.read_only {
            cmd.arg("-read-only");
        }
        if let Some(ref script) = d.script {
            cmd.arg("-script").arg(script);
        }
        let mut window = app.handover_window(d.next, chain);
        window.children = d.children();
        let err = crate::handover::exec(
            crate::handover::Handover {
                window: Some(window),
                ..Default::default()
            },
            &mut cmd,
        );
        drop(parked);
        *terminal = term::enter_or_report(app.mouse).ok();
        app.note(format!("CLI handover could not exec new binary: {err}"));
        false
    }
}

impl App {
    /// A key was read from the terminal and is being applied.
    pub(crate) fn typed(&mut self) {
        self.keys_applied += 1;
        self.last_key = Some(std::time::Instant::now());
    }

    /// Whether this window must stay on its build for now: something it is in
    /// the middle of would not survive the `exec`.
    ///
    /// What is typed is not one of them (owner, 2026-10-09): the input, its
    /// cursor and its attachments travel with the handover, and a window that
    /// waited for an empty composer stayed on the old build for as long as a
    /// draft sat in it — hours, on a build whose bugs the release had fixed.
    /// A window that is being typed into waits until the hands stop
    /// ([`HANDOVER_IDLE`]) — unless its host no longer speaks its protocol,
    /// when it cannot send what is typed anyway; the keys themselves are kept
    /// either way ([`input::park`]).
    pub(crate) fn handover_waits(&self) -> bool {
        self.quit
            || self.quit_armed
            || self.busy
            || self.queued_prompt.is_some()
            || self.pending.is_some()
            || self.questions.is_some()
            || self.preferences.editor.is_some()
            || !self.protocol_skew
                && self
                    .last_key
                    .is_some_and(|at| at.elapsed() < HANDOVER_IDLE)
            || self.composition.flight.is_some()
            || self.composition.dirty_since.is_some()
            || self.composition.promote_pending
            // A draft's first message is stored (`DraftPromote`) but not yet
            // handed to its session's host (`PromptCommitted`), which is what
            // starts the turn that answers it. The token is this window's
            // alone and does not travel with the handover, so no `exec` may
            // fall between the two. (The failure measured 2026-10-09 — a draft
            // sent while a release was installed, stored and never answered —
            // was the host launch from a replaced file, `Driver::launcher`;
            // this keeps the other way to lose the token closed.)
            || self.composition.promotion.is_some()
    }

    /// What this window hands the build it activates.
    pub(crate) fn handover_window(
        &self,
        request_next: u64,
        chain: crate::handover::Chain,
    ) -> crate::handover::Window {
        use crate::handover::Panel as Handed;
        crate::handover::Window {
            draft: self
                .session
                .is_none()
                .then(|| self.composition.draft.as_ref().map(|d| d.token.clone()))
                .flatten(),
            request_next,
            input: self.input.clone(),
            cursor: self.cursor,
            attached: self.attached.iter().map(|a| a.path.clone()).collect(),
            scroll: self.scroll,
            panel: match self.panel {
                Panel::Settings => Some(Handed::Settings {
                    page: serde_json::to_value(&self.preferences.location.page).unwrap_or_default(),
                    row: self.preferences.location.row,
                }),
                Panel::Tasks => Some(Handed::Tasks),
                Panel::Help => Some(Handed::Help),
                _ => None,
            },
            chain,
            // Filled by the handover itself: only the driver knows them.
            children: Vec::new(),
        }
    }

    /// Take over what the previous window handed this one.
    pub(crate) fn restore_handover(&mut self, window: crate::handover::Window) {
        use crate::handover::Panel as Handed;
        self.input = window.input;
        self.cursor = window.cursor.min(self.input.len());
        while !self.input.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        for path in &window.attached {
            if let Ok(attachment) = runeengine::media::Attachment::at(std::path::Path::new(path)) {
                if !self.attached.iter().any(|a| a.path == attachment.path) {
                    self.attached.push(attachment);
                }
            }
        }
        self.scroll = window.scroll;
        match window.panel {
            Some(Handed::Settings { page, row }) => {
                self.panel = Panel::Settings;
                // The previous release handed over a connection's Service
                // page; its one-time destination is now the Tools provider page.
                let page = page
                    .get("Service")
                    .and_then(|s| s.get("service"))
                    .and_then(|s| {
                        serde_json::from_value::<runeengine::settings::services::Service>(s.clone())
                            .ok()
                    })
                    .map(|service| settings::Page::WebProviders { service })
                    .or_else(|| serde_json::from_value::<settings::Page>(page).ok());
                if let Some(page) = page {
                    self.preferences.navigate(page);
                }
                self.preferences.location.row = row;
            }
            Some(Handed::Tasks) => self.panel = Panel::Tasks,
            Some(Handed::Help) => self.panel = Panel::Help,
            None => {}
        }
        self.handover_chain = window.chain;
    }

    /// The chain the next handover carries, or `None` when this window was
    /// already handed over three times within ten seconds — a build that keeps
    /// handing over to itself is stopped rather than left to loop.
    pub(crate) fn next_handover(&self, now: u64) -> Option<crate::handover::Chain> {
        let chain = self.handover_chain;
        let count = if now.saturating_sub(chain.at) > 10 {
            0
        } else {
            chain.count
        };
        (count < 3).then_some(crate::handover::Chain {
            count: count + 1,
            at: now,
        })
    }
}

/// The host announced its upgrade. Stop talking to it, keep every unacked
/// command for its successor, and find that successor on a worker thread — the
/// screen keeps painting and taking keys meanwhile, and nothing typed is routed
/// anywhere but the next host.
fn start_reconnect(app: &mut App, d: &mut Driver, tx: &Sender<Msg>, session: i64) {
    if Some(session) != app.session || d.reconnecting {
        return;
    }
    d.begin_reconnect();
    app.note("Activating the installed Rune release; reconnecting…");
    #[cfg(not(test))]
    let launcher = d.launcher(session, &app.project).ok();
    #[cfg(not(test))]
    let store = d.db_path.parent().map(std::path::Path::to_path_buf);
    #[cfg(test)]
    let store: Option<std::path::PathBuf> = None;
    let tx = tx.clone();
    std::thread::Builder::new()
        .name(format!("rune-reconnect-{session}"))
        .spawn(move || {
            if let Some((client, events)) = crate::driver::find_host_through_upgrade(
                session,
                store.as_deref(),
                crate::driver::RESUME_DEADLINE,
            ) {
                let _ = tx.send(Msg::Reconnected(session, client, events));
                return;
            }
            // Nobody brought the host back: start one, as `/new` would.
            #[cfg(not(test))]
            let outcome = launcher
                .ok_or_else(|| "no way to start a session host".to_string())
                .and_then(|launcher| launcher.start());
            #[cfg(test)]
            let outcome: Result<
                (crate::session_ipc::SessionClient, Receiver<Event>),
                String,
            > = Err("no session host".into());
            let _ = tx.send(match outcome {
                Ok((client, events)) => Msg::Reconnected(session, client, events),
                Err(error) => Msg::ReconnectFailed(session, error),
            });
        })
        .ok();
}

/// A new host answers. The resume goes first, then everything that waited.
fn finish_reconnect(
    app: &mut App,
    d: &mut Driver,
    tx: &Sender<Msg>,
    session: i64,
    client: crate::session_ipc::SessionClient,
    events: Receiver<Event>,
) {
    if Some(session) != app.session || !d.reconnecting {
        return;
    }
    spawn_event_source(tx, events, Some(session));
    let id = d.next;
    d.next += 1;
    app.awaiting.insert(id, Awaiting::Resumed);
    d.finish_reconnect(
        client,
        Command::SessionResume {
            id,
            session,
            project: Some(app.project.clone()),
        },
    );
    // The window was away: what the stream said meanwhile it never heard. Its
    // screen is rebuilt from the engine's view, asked for after everything this
    // window resent, so the answer already holds their effects.
    app.awaiting_view = true;
    app.request(d, Awaiting::View, |id| Command::SessionView { id, session });
}

/// How a replayed tool call ended, read from the label kept beside it in the
/// audit trail — never from the text the model read, which says it in words.
fn state_of(status: &str) -> Option<ToolState> {
    match status {
        "cancelled" => Some(ToolState::Stopped),
        "denied"
        | "failed"
        | "not_executed"
        | "service_unavailable"
        | "timed_out"
        | "repeated_failure" => Some(ToolState::Failed),
        "backgrounded" => Some(ToolState::Backgrounded),
        _ => None,
    }
}

/// The number of the event stream this window listens to now. Written by the
/// UI thread when it starts one, read by the UI thread when a stream ends.
static EVENT_STREAM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn spawn_event_source(tx: &Sender<Msg>, events: Receiver<Event>, session: Option<i64>) {
    // Only a session's stream is one this window can lose its host on; the
    // private engine's stream ends only with the window.
    let stream = if session.is_some() {
        EVENT_STREAM.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    } else {
        0
    };
    let engine = tx.clone();
    std::thread::spawn(move || {
        for event in events {
            if let Event::HostUpgrading { session: from } = event {
                if session == Some(from) {
                    let _ = engine.send(Msg::HostUpgrade(from));
                    return;
                }
                continue;
            }
            if engine.send(Msg::Event(event)).is_err() {
                return;
            }
        }
        // The host's side closed without saying it was upgrading. A window
        // that kept a dead connection would talk to nobody while its session
        // ran on without it — find the session's host again.
        if let Some(session) = session {
            let _ = engine.send(Msg::HostGone(session, stream));
        }
    });
}

fn spawn_sources(tx: &Sender<Msg>, events: Receiver<Event>, session: Option<i64>) {
    let (ktx, krx) = channel::<Key>();
    input::spawn(ktx);
    let keys = tx.clone();
    std::thread::spawn(move || {
        for key in krx {
            if keys.send(Msg::Key(key)).is_err() {
                return;
            }
        }
    });

    spawn_event_source(tx, events, session);

    let ticker = tx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(runtime_configuration().duration("ui.frame_interval"));
        if ticker.send(Msg::Tick).is_err() {
            return;
        }
    });
}

impl App {
    fn new(
        session: Option<i64>,
        project: String,
        report: &runeengine::proto::SandboxReport,
    ) -> App {
        // Read from `~/.rune/rune.toml`, which the sandbox does not grant —
        // so nothing the agent runs can raise its own ceiling or turn the user's
        // pointer back on.
        let limits = settings_home()
            .and_then(|home| runeengine::limits::Limits::load(&home))
            .unwrap_or_default();
        let theme_mode =
            theme::ThemeMode::parse(&limits.theme()).unwrap_or_else(theme::current_theme);
        App {
            host_version: runeengine::VERSION.to_string(),
            told_newer_build: false,
            queued_prompt: None,
            pending_model_selection: None,
            blocks: Vec::new(),
            input: String::new(),
            input_editor: editing::State::default(),
            attached: Vec::new(),
            attachment_catalogues: std::collections::HashSet::new(),
            cursor: 0,
            composer_width: 80,
            composer_first_row: 0,
            composer_hit_rows: Vec::new(),
            scroll: 0,
            drawn_rows: 0,
            drawn_viewport: 0,
            scroll_anchor: None,
            panel_view: PanelView::default(),
            glide: None,
            transcript_cache: None,
            transcript_dirty: true,
            sidebar: limits.sidebar(),
            plan_scroll: usize::MAX,
            plan_anchor: None,
            questions_card: None,
            sidebar_cols: None,
            sidebar_last_row: 0,
            jump_hint: None,
            plan_max: 0,
            verbose: false,
            panel: Panel::None,
            capability_view: Value::Null,
            mcp_input: None,
            panel_row: 0,
            preferences: settings::State::default(),
            hits: Vec::new(),
            cursor_terminal: None,
            selection: None,
            selection_rows: Vec::new(),
            palette_row: 0,
            palette_scroll: 0,
            ask_choice: 0,
            permission: permission::State::default(),
            session,
            composition: composer::State::default(),
            project,
            mode: "ask".into(),
            settings: Value::Null,
            providers: Value::Null,
            pending_provider_refresh: false,
            config_generation: runtime_configuration().generation(),
            mouse_dirty: false,
            busy: false,
            context: 0,
            label: String::new(),
            frame: 0,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            context_used: 0,
            tok_in: 0,
            tok_out: 0,
            cache_in: 0,
            cost: 0.0,
            unpriced_calls: 0,
            // Read from `~/.rune/rune.toml`, which the sandbox does not grant
            // — so nothing the agent runs can raise its own ceiling.
            warn_spend: limits.warn_spend_usd(),
            mouse: limits.mouse(),
            theme: theme_mode,
            warned_spend: false,
            turn_started: None,
            pending: None,
            questions: None,
            tasks: Vec::new(),
            logs: Vec::new(),
            context_view: Value::Null,
            awaiting_view: false,
            shown_pending: std::collections::HashSet::new(),
            stored_turn: 0,
            terminal_keeps_up: true,
            log_floor: runeengine::proto::LogLevel::Info,
            log_open: None,
            session_picker: None,
            pending_session_switch: None,
            undo_diff: None,
            undo_armed: None,
            undo_refused: None,
            redo_files: Vec::new(),
            notified: Vec::new(),
            file_changes: Vec::new(),
            status: None,
            status_at: None,
            retry_status: None,
            sandbox: if report.enforced {
                format!("landlock abi {}", report.abi)
            } else {
                report
                    .reason
                    .clone()
                    .unwrap_or_else(|| "not enforced".into())
            },
            sandbox_ok: report.enforced,
            escapes: report.escapes.iter().map(|e| e.detail.clone()).collect(),
            awaiting: std::collections::HashMap::new(),
            title: None,
            started_at: 0,
            run_started_at: 0,
            accumulated_seconds: 0,
            steps: 0,
            turns: 0,
            quit_armed: false,
            quit_armed_at: None,
            stop_armed: false,
            stop_armed_at: None,
            resync: false,
            handover_due: false,
            protocol_skew: false,
            keys_applied: 0,
            last_key: None,
            handover_chain: crate::handover::Chain::default(),
            quit: false,
        }
    }

    pub fn model(&self) -> &str {
        self.settings["settings"]["model"].as_str().unwrap_or("")
    }

    pub fn provider(&self) -> &str {
        self.settings["settings"]["provider"].as_str().unwrap_or("")
    }

    /// What one call cost, from the prices the engine published for this model.
    ///
    /// Three rates, not two: a cached prompt token is charged at a fraction of a
    /// fresh one — for MiMo, under one percent of it — so a long session that
    /// reuses its prompt is priced several times too high without the
    /// distinction. Nothing is charged for a model nobody has priced.
    /// Say once, when the session's spend passes the mark the user set.
    ///
    /// Deliberately not a stop. A turn cut off partway has already been paid for
    /// and leaves a half-applied edit behind, which is worse than the spending;
    /// what a runaway loop actually needs is for a person to notice it, and a
    /// line in the transcript is how they do.
    fn check_spend(&mut self) {
        if self.warned_spend || self.warn_spend <= 0.0 || self.cost < self.warn_spend {
            return;
        }
        self.warned_spend = true;
        self.blocks.push(Block::Notice {
            text: format!(
                "This session has now spent ${:.2}, past the ${:.2} you asked to be told about.                  Nothing has been stopped — `esc` twice stops the turn; `ctrl+c` twice leaves Rune.",
                self.cost, self.warn_spend
            ),
            bad: false,
        });
    }

    pub fn lookup_model_info(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Option<runeengine::settings::providers::ModelInfo> {
        let direct = (self.providers["provider"].as_str() == Some(provider))
            .then(|| self.providers["configured"].as_array())
            .flatten();
        let registry = self.providers["providers"]
            .as_array()
            .and_then(|ps| ps.iter().find(|p| p["name"].as_str() == Some(provider)))
            .and_then(|p| p["models"].as_array());
        let active = (self.provider() == provider)
            .then(|| self.settings["models"].as_array())
            .flatten();
        [direct, registry, active]
            .into_iter()
            .flatten()
            .flat_map(|a| a.iter())
            .find(|m| m["id"].as_str() == Some(model_id))
            .and_then(|m| serde_json::from_value(m.clone()).ok())
    }

    /// Re-read the active provider's model list from the registry into the
    /// session's settings snapshot.
    ///
    /// The model picker draws the *active* provider from `settings["models"]` and
    /// every other provider from the registry. A model add, delete, edit, or a
    /// catalogue refresh mutates the registry, but only a refresh of the active
    /// provider used to copy the result back into the snapshot — so an add or a
    /// delete on the provider the user is actually working in stayed invisible
    /// until the engine re-read its settings on the next launch. One function,
    /// called after any registry mutation, so the two sources cannot drift.
    pub fn sync_active_models(&mut self) {
        let active = self.provider().to_string();
        let models = self.providers["providers"]
            .as_array()
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p["name"].as_str() == Some(active.as_str()))
            })
            .and_then(|p| p["models"].as_array().cloned());
        if let Some(models) = models {
            self.settings["models"] = serde_json::Value::Array(models);
        }
    }

    pub fn update_cached_model(
        &mut self,
        provider: &str,
        model: &runeengine::settings::providers::ModelInfo,
    ) {
        let val = serde_json::to_value(model).unwrap_or(serde_json::Value::Null);
        if self.providers["provider"].as_str() == Some(provider) {
            if let Some(configured) = self.providers["configured"].as_array_mut() {
                if let Some(m) = configured
                    .iter_mut()
                    .find(|m| m["id"].as_str() == Some(&model.id))
                {
                    *m = val.clone();
                } else {
                    configured.push(val.clone());
                }
            }
        }
        if let Some(providers) = self.providers["providers"].as_array_mut() {
            if let Some(p) = providers
                .iter_mut()
                .find(|p| p["name"].as_str() == Some(provider))
            {
                if let Some(models) = p["models"].as_array_mut() {
                    if let Some(m) = models
                        .iter_mut()
                        .find(|m| m["id"].as_str() == Some(&model.id))
                    {
                        *m = val.clone();
                    } else {
                        models.push(val.clone());
                    }
                }
            }
        }
        if self.provider() == provider {
            if let Some(arr) = self.settings["models"].as_array_mut() {
                if let Some(m) = arr.iter_mut().find(|m| m["id"].as_str() == Some(&model.id)) {
                    *m = val;
                }
            }
        }
    }

    pub fn switch_session(&mut self, target_session: i64, d: &mut Driver) {
        if Some(target_session) == self.session {
            self.note(format!("Already in session #{target_session}"));
            return;
        }
        // This runs between event-loop bursts, not inside a keyboard handler.
        // Keep the old socket alive until target validation/replay succeeds.
        if !self.sync_composer(d, true) {
            return;
        }
        let previous = d.client.take();
        #[cfg(not(test))]
        {
            if let Ok((client, events)) = crate::session_ipc::SessionClient::connect(target_session)
            {
                d.client = Some(client);
                d.events = events;
            } else {
                let target = Driver::start(crate::driver::Options {
                    db: Some(d.db_path.clone()),
                    script: d.script.clone(),
                    read_only: d.read_only,
                    request_next: None,
                })
                .and_then(|mut staged| {
                    staged.attach_detached(target_session, &self.project)?;
                    Ok(staged)
                });
                match target {
                    Ok(mut staged) => {
                        d.client = staged.client.take();
                        d.events = std::mem::replace(&mut staged.events, channel().1);
                    }
                    Err(e) => {
                        d.client = previous;
                        self.note(e);
                        return;
                    }
                }
            }
        }
        let (id, res) = match d.resume(target_session, Some(self.project.clone())) {
            Ok(pair) => pair,
            Err(e) => {
                d.client = previous;
                d.pending_events.clear();
                self.note(e);
                return;
            }
        };
        // Reuse the existing reset path, then restore the target's metadata.
        self.awaiting.insert(u64::MAX, Awaiting::NewSession);
        self.on_ack(u64::MAX, true, None, Some(res.clone()));
        self.session_picker = None;
        self.attached.clear();
        match d.view(id) {
            Ok(view) => self.apply_view(&view),
            Err(e) => self.note(format!("Could not read the session: {e}")),
        }
        if let Ok(saved) = d.call(|request| Command::ComposerGet {
            id: request,
            session: id,
        }) {
            if let Some(composer) = saved
                .get("composer")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
            {
                self.restore_composer(composer, true);
            }
        }
        self.composition.promote_pending = false;
        self.composition.promote_started = None;
        self.composition.flight = None;
        self.composition.flight_started = None;
        self.composition.submits.clear();
        // Dropping the final old client triggers host-side cancellation; peers
        // keep ownership. No count-then-cancel race and no guessed fallback.
        drop(previous);
        self.note(format!("Resumed session #{id}"));
        self.resync = true;
    }

    /// What the session has cost, or nothing at all when the model has no
    /// published price. A zero would be a claim, and the wrong one.
    pub fn spend(&self) -> Option<f64> {
        (self.unpriced_calls == 0
            && (self.settings["price"].is_object() || self.cost > 0.0 || self.input_tokens > 0))
            .then_some(self.cost)
    }

    pub fn is_suppressed(&self, key: &str) -> bool {
        self.settings["suppressed"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["key"].as_str() == Some(key) && s["permanent"].as_bool() == Some(true))
    }

    pub fn thinking(&self) -> bool {
        if self.settings["thinking_required"].as_bool() == Some(true) {
            return true;
        }
        if self.is_suppressed("thinking") {
            return false;
        }
        self.settings["settings"]["thinking"]
            .as_bool()
            .unwrap_or(false)
    }

    /// The effort choice the screen holds: `off`, a level, or — thinking on
    /// with no level, on a route that applies its own — the route default. One
    /// reading for the settings row, its arrows, Enter and Alt+T.
    pub fn effort_choice(&self, choices: &[String]) -> String {
        if !self.thinking() {
            return "off".into();
        }
        if let Some(level) = self.effort() {
            return level.to_string();
        }
        if choices.iter().any(|c| c == ROUTE_DEFAULT) {
            return ROUTE_DEFAULT.into();
        }
        choices
            .iter()
            .find(|c| *c != "off")
            .cloned()
            .unwrap_or_else(|| "on".into())
    }

    pub fn effort(&self) -> Option<&str> {
        if !self.thinking() || self.is_suppressed("effort") {
            return None;
        }
        self.settings["settings"]["effort"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "off")
    }

    /// Whether the classifier is switched on, as the engine says it.
    pub fn classifier_enabled(&self) -> bool {
        self.settings["classifier_status"]["enabled"]
            .as_bool()
            .unwrap_or(true)
    }

    /// Whether a policy file exists, as the engine says it — a fact about the
    /// installation and the project, the same in every mode. This screen never
    /// looks at the file system or reads a reason written for another mode.
    pub fn has_classifier_policy(&self) -> bool {
        self.settings["classifier_status"]["policy"]
            .as_bool()
            .unwrap_or(false)
    }

    pub fn classifier_active(&self) -> bool {
        if self.mode == "ask" {
            return false;
        }
        self.settings["classifier_status"]["active"]
            .as_bool()
            .unwrap_or(false)
    }

    pub fn show_classifier_status(&mut self) {
        let enabled = self.classifier_enabled();
        let has_policy = self.has_classifier_policy();
        let cfg = &self.settings["configuration"];
        let model_info = match cfg
            .get("classifier")
            .and_then(|c| c.get("model"))
            .and_then(Value::as_object)
            .or_else(|| cfg.get("classifier.model").and_then(Value::as_object))
        {
            Some(m) => format!(
                "{}/{}",
                m.get("provider").and_then(Value::as_str).unwrap_or(""),
                m.get("id").and_then(Value::as_str).unwrap_or("")
            ),
            None => format!("{}/{}", self.provider(), self.model()),
        };
        let status_reason = self.settings["classifier_status"]["reason"]
            .as_str()
            .unwrap_or(if !enabled {
                "Disabled by configuration"
            } else if !has_policy {
                "Inactive: no CLASSIFIER.md file found"
            } else if self.mode == "ask" {
                "Inactive in Ask mode"
            } else {
                "Active for proposed tool actions"
            });

        self.note(format!(
            "Classifier: {status_reason} · Model: {model_info} · /classifier [on|off|model|reload]"
        ));
    }

    /// Restore previous session token usage and spend into live state.
    pub fn restore_usage(&mut self, usage: &Value) {
        if let Some(u) = usage.get("used").and_then(|v| v.as_u64()) {
            self.context_used = u;
        }
        if let Some(i) = usage.get("tok_in").and_then(|v| v.as_u64()) {
            self.tok_in = i;
            self.input_tokens = i;
            if self.context_used == 0 {
                self.context_used = i;
            }
        }
        if let Some(o) = usage.get("tok_out").and_then(|v| v.as_u64()) {
            self.tok_out = o;
            self.output_tokens = o;
        }
        if let Some(c) = usage.get("cache_in").and_then(|v| v.as_u64()) {
            self.cache_in = c;
            self.cached_tokens = c;
        }
        self.unpriced_calls = usage
            .get("unpriced_calls")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(s) = usage.get("spend").and_then(|v| v.as_f64()) {
            self.cost = s;
        }
    }

    /// A message brought back from the draft's history is its words and files
    /// only: the model and mode are the ones on the screen now. Bringing back
    /// the model it was once sent with changed the model with no word of it —
    /// a model since deleted included (owner, 2026-10-15).
    fn recalled(
        &self,
        mut entry: runeengine::store::composer::Composer,
    ) -> runeengine::store::composer::Composer {
        let now = self.composer_snapshot();
        entry.settings = now.settings;
        entry.mode = now.mode;
        entry
    }

    /// Recall previous user prompt from history.
    pub fn history_prev(&mut self) {
        if let Some(draft) = self.composition.draft.as_ref() {
            let history: Vec<_> = draft
                .history
                .iter()
                .filter(|entry| entry.composer.has_user_state())
                .map(|entry| entry.composer.clone())
                .collect();
            if history.is_empty() {
                return;
            }
            let current = self.history_index;
            let index = match current {
                Some(0) => return,
                Some(index) => index - 1,
                None => history.len() - 1,
            };
            if self.history_index.is_none() {
                self.composition.history_saved = Some(self.composer_snapshot());
            }
            self.history_index = Some(index);
            let authoritative = self.composition.authoritative.clone();
            let recalled = self.recalled(history[index].clone());
            self.input_editor.reset();
            self.restore_composer(recalled, false);
            self.composition.authoritative = authoritative;
            self.cursor = 0;
            return;
        }
        if self.prompt_history.is_empty() {
            return;
        }
        let current_idx = self.history_index;
        if current_idx == Some(0) {
            return;
        }
        self.input_editor.reset();
        match current_idx {
            None => {
                self.history_draft = self.input.clone();
                let idx = self.prompt_history.len().saturating_sub(1);
                self.history_index = Some(idx);
                self.input = self.prompt_history[idx].clone();
                self.cursor = 0;
            }
            Some(idx) => {
                if idx > 0 {
                    let next_idx = idx - 1;
                    self.history_index = Some(next_idx);
                    self.input = self.prompt_history[next_idx].clone();
                    self.cursor = 0;
                }
            }
        }
    }

    /// Recall next user prompt from history or restore draft.
    pub fn history_next(&mut self) {
        if let Some(draft) = self.composition.draft.as_ref() {
            let history: Vec<_> = draft
                .history
                .iter()
                .filter(|entry| entry.composer.has_user_state())
                .map(|entry| entry.composer.clone())
                .collect();
            if let Some(index) = self.history_index {
                let next = if index + 1 < history.len() {
                    self.history_index = Some(index + 1);
                    Some(history[index + 1].clone())
                } else {
                    self.history_index = None;
                    self.composition.history_saved.take()
                };
                if let Some(next) = next {
                    let authoritative = self.composition.authoritative.clone();
                    let recalled = self.recalled(next);
                    self.input_editor.reset();
                    self.restore_composer(recalled, true);
                    self.composition.authoritative = authoritative;
                    self.cursor = self.input.len();
                }
            }
            return;
        }
        let current_idx = self.history_index;
        if let Some(idx) = current_idx {
            self.input_editor.reset();
            if idx + 1 < self.prompt_history.len() {
                let next_idx = idx + 1;
                self.history_index = Some(next_idx);
                self.input = self.prompt_history[next_idx].clone();
                self.cursor = self.input.len();
            } else {
                self.history_index = None;
                self.input = std::mem::take(&mut self.history_draft);
                self.cursor = self.input.len();
            }
        }
    }

    /// Make this screen show what the engine says the session shows: the state
    /// as it stands, the conversation up to the turn in flight, and that turn's
    /// events — through the same handlers a live stream goes through, so a
    /// window that joins now looks exactly like one that watched from the start.
    pub fn apply_view(&mut self, view: &Value) {
        if let Some(mode) = view["mode"].as_str() {
            self.mode = mode.to_string();
        }
        if !view["classifier"].is_null() {
            self.settings["classifier_status"] = view["classifier"].clone();
        }
        if !view["settings"].is_null() {
            self.take_settings(view["settings"].clone());
        }
        self.title = view["title"].as_str().map(String::from);
        self.started_at = view["created_at"].as_i64().unwrap_or(0);
        self.accumulated_seconds = view["active_seconds"].as_i64().unwrap_or(0).max(0) as u64;
        self.run_started_at = 0;
        // Counters are the view's, as of before the turn in flight; that turn's
        // own requests arrive in its events.
        self.tok_in = 0;
        self.tok_out = 0;
        self.cache_in = 0;
        self.input_tokens = 0;
        self.output_tokens = 0;
        self.cached_tokens = 0;
        self.context_used = 0;
        self.cost = 0.0;
        self.unpriced_calls = 0;
        if !view["usage"].is_null() {
            self.restore_usage(&view["usage"]);
        }
        self.file_changes = view["file_changes"].as_array().cloned().unwrap_or_default();

        // The transcript is rebuilt: nothing from before is kept.
        self.blocks.clear();
        self.prompt_history.clear();
        self.input_editor.reset();
        self.history_index = None;
        self.queued_prompt = None;
        self.busy = false;
        self.turn_started = None;
        self.label.clear();
        self.pending = None;
        self.questions = None;
        self.mcp_input = None;
        self.retry_status = None;
        self.tasks.clear();
        let empty = Vec::new();
        self.load_history(
            view["history"].as_array().unwrap_or(&empty),
            view["toolcalls"].as_array().unwrap_or(&empty),
            &view["tasks"],
        );
        for event in view["live"]["events"].as_array().unwrap_or(&empty) {
            if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
                self.on_event(event);
            }
        }
        // Messages waiting for the running turn, as the window that sent them
        // shows them. One the turn has already taken — its start is among the
        // events above, the composer not yet told — is a message on screen
        // already, and is not drawn a second time.
        self.shown_pending.clear();
        if let Ok(composer) = serde_json::from_value::<runeengine::store::composer::Composer>(
            view["composer"].clone(),
        ) {
            let mut taken: Vec<String> = view["live"]["events"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .skip(1)
                .filter(|e| e["t"] == "turn_start" && e["notification"] != true)
                .filter_map(|e| e["text"].as_str().map(|t| t.trim().to_string()))
                .collect();
            let waiting: Vec<_> = composer
                .pending
                .into_iter()
                .filter(
                    |entry| match taken.iter().position(|t| *t == entry.text.trim()) {
                        Some(at) => {
                            taken.remove(at);
                            false
                        }
                        None => true,
                    },
                )
                .collect();
            self.sync_pending(&waiting);
        }
        self.recount();
        self.transcript_cache = None;
        self.transcript_dirty = true;
        self.resync = true;
    }

    /// Put the messages a session already holds back on screen.
    ///
    /// Enough to read, not a re-simulation: tool calls come back as one line
    /// each with what the model was given, because replaying their streaming is
    /// a lie about when things happened. What is still being said — the turn in
    /// flight — is not here: it arrives as the view's events and goes through
    /// the same handlers a live stream does.
    fn load_history(&mut self, messages: &[Value], toolcalls: &[Value], tasks: &Value) {
        let mut tool_durations =
            std::collections::HashMap::<_, std::collections::VecDeque<_>>::new();
        for item in toolcalls {
            if let (Some(id), Some(start), Some(end)) = (
                item["id"].as_str(),
                item["started_at"].as_i64(),
                item["ended_at"].as_i64(),
            ) {
                let diff_ms = end.saturating_sub(start).max(0) as u64;
                let summary = item["summary"].as_str().map(String::from);
                let status = item["status"].as_str().map(String::from);
                tool_durations
                    .entry((id.to_string(), item["turn"].as_i64().unwrap_or(0)))
                    .or_default()
                    .push_front((
                        std::time::Duration::from_millis(diff_ms),
                        item["ok"].as_bool(),
                        summary,
                        status,
                        item["code"].as_str().map(String::from),
                    ));
            }
        }

        self.stored_turn = messages
            .iter()
            .filter_map(|m| m["turn"].as_i64())
            .max()
            .unwrap_or(0);
        for m in messages {
            let text = m["content"].as_str().unwrap_or("").to_string();
            match m["role"].as_str().unwrap_or("") {
                "user" => {
                    let time = m["created_at"]
                        .as_i64()
                        .map(term::epoch_hm)
                        .filter(|t| !t.is_empty());
                    let media = serde_json::from_value(m["media"].clone()).unwrap_or_default();
                    self.blocks
                        .push(Block::user_timed(text.clone(), time, media));
                    if !text.is_empty() && !text.starts_with('/') {
                        self.prompt_history.push(text);
                    }
                }
                // Rune's own rows, drawn by what the engine says they are. What
                // Rune told the model behind the conversation — the opening
                // context, a status update, a changed AGENTS.md or memory
                // index, a memory notice, a restore — is not drawn (owner,
                // 2026-10-09): it is still sent, and `/context` and `/logs`
                // show it. A background task's notification is drawn: it is
                // why the model speaks unasked.
                "context" => {
                    let name = m["context"]["name"].as_str().unwrap_or("");
                    let kind = m["context"]["kind"].as_str().unwrap_or("");
                    match kind {
                        "notification" => self.blocks.push(Block::Notification {
                            text,
                            time: m["created_at"]
                                .as_i64()
                                .map(term::epoch_hm)
                                .filter(|t| !t.is_empty()),
                        }),
                        "skill" => self.blocks.push(Block::Notice {
                            text: format!("Skill {name} activated for this conversation."),
                            bad: false,
                        }),
                        "mcp_prompt" => self.blocks.push(Block::Notice {
                            text: format!(
                                "Prompt from MCP server {name} added to the conversation."
                            ),
                            bad: false,
                        }),
                        _ => {}
                    }
                }
                "assistant" => {
                    if let Some(reasoning) =
                        m["reasoning"].as_str().filter(|r| !r.trim().is_empty())
                    {
                        self.blocks.push(Block::Thinking {
                            text: reasoning.to_string(),
                            open: false,
                        });
                    }
                    if !text.trim().is_empty() {
                        self.blocks.push(Block::Text { text });
                    }
                    for call in m["tool_calls"]
                        .as_array()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                    {
                        let id = call["id"].as_str().unwrap_or_default().to_string();
                        let name = call["name"].as_str().unwrap_or_default().to_string();
                        let mut tool = Tool::new(id.clone(), name, String::new());
                        if let Some(raw) = call["args"].as_str() {
                            tool.absorb(raw);
                            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                                tool.settle(&parsed);
                            }
                        } else if let Some(obj) = call["args"].as_object() {
                            let val = Value::Object(obj.clone());
                            tool.absorb(&val.to_string());
                            tool.settle(&val);
                        }
                        tool.state = ToolState::Ok;
                        tool.ended = Some(Instant::now());
                        if let Some((dur, ok, summary, status, code)) = tool_durations
                            .get_mut(&(id.clone(), m["turn"].as_i64().unwrap_or(0)))
                            .and_then(|list| list.pop_front())
                        {
                            tool.duration = Some(dur);
                            if ok == Some(false) {
                                tool.state = ToolState::Failed;
                            }
                            if let Some(s) = summary {
                                tool.summary = s;
                            }
                            // The kind of ending, as the live event told it.
                            if let Some(status) = status.as_deref() {
                                if let Some(state) = state_of(status) {
                                    tool.state = state;
                                }
                            }
                            if status.is_some() || code.is_some() {
                                tool.detail =
                                    Some(serde_json::json!({ "status": status, "code": code }));
                            }
                        }
                        tool.snap_counters();
                        self.blocks.push(Block::of(tool));
                    }
                }
                "tool" => {
                    let id = m["tool_call_id"].as_str().unwrap_or_default();
                    if let Some(tool) = self.tool_by_id(id) {
                        tool.content = text;
                        if tool.summary.is_empty() {
                            let parsed = runeengine::tools::notice::split(&tool.content);
                            let first_line = parsed
                                .notices
                                .first()
                                .copied()
                                .or_else(|| tool.content.lines().next())
                                .unwrap_or("");
                            tool.summary = if first_line.trim_start().starts_with('{')
                                || first_line.trim_start().starts_with('[')
                            {
                                String::new()
                            } else {
                                first_line.lines().next().unwrap_or("").to_string()
                            };
                        }
                    }
                }
                "error" => {
                    self.blocks.push(Block::Notice { text, bad: true });
                }
                _ => {}
            }
        }

        if let Some(list) = tasks.as_array() {
            self.tasks = list.clone();
        }
        if self.tasks.is_empty() {
            for b in self.blocks.iter().rev() {
                if let Block::Tool(t) = b {
                    if t.name == "track_tasks" {
                        let parsed = steps::tasks(t);
                        if !parsed.is_empty() {
                            self.tasks = parsed;
                            break;
                        }
                    }
                }
            }
        }
        self.transcript_dirty = true;
    }

    /// Keep one log line: for the panel, and said once in the status line when
    /// it is a warning or an error.
    pub fn log(&mut self, entry: runeengine::proto::LogEntry) {
        // The same line can arrive live and in the read that opened the panel.
        if self.logs.last() == Some(&entry) {
            return;
        }
        if entry.level >= runeengine::proto::LogLevel::Warn && self.panel != Panel::Logs {
            let preview: String = if entry.message.chars().count() > 50 {
                format!("{}…", entry.message.chars().take(49).collect::<String>())
            } else {
                entry.message.clone()
            };
            let note = match entry.level {
                runeengine::proto::LogLevel::Error => {
                    format!(
                        "Error ({}): {} · view in /logs error",
                        entry.source, preview
                    )
                }
                _ => {
                    format!("Warning ({}): {} · view in /logs", entry.source, preview)
                }
            };
            self.note(note);
        }
        self.logs.push(entry);
        let overflow = self.logs.len().saturating_sub(LOG_LINES);
        self.logs.drain(..overflow);
    }

    /// Open the context panel, asking the engine what the model holds.
    pub fn open_context(&mut self, d: &mut Driver) {
        let Some(session) = self.session else {
            self.note("An unsent draft has no context yet: the model is given one with the first message.");
            return;
        };
        self.show_panel(Panel::Context);
        self.context_view = Value::Null;
        self.panel_view.reset();
        self.request(d, Awaiting::Context, |id| Command::SessionContext {
            id,
            session,
        });
    }

    /// Open the undo list: every file the session changed, asked of the
    /// engine so it reads as the files stand now.
    pub fn open_undo(&mut self, d: &mut Driver) {
        undo::open(self, d, undo::Side::Undo);
    }

    /// Open the redo list: every file undone and not put back since.
    pub fn open_redo(&mut self, d: &mut Driver) {
        undo::open(self, d, undo::Side::Redo);
    }

    /// Open the log panel, reading the session's whole log from disk.
    pub fn open_logs(&mut self, d: &mut Driver, floor: runeengine::proto::LogLevel) {
        self.show_panel(Panel::Logs);
        self.log_floor = floor;
        self.log_open = None;
        self.panel_row = usize::MAX;
        if let Some(session) = self.session {
            self.request(d, Awaiting::Logs, |id| Command::SessionLogs {
                id,
                session,
                limit: LOG_LINES,
            });
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.status = Some(text.into());
        self.status_at = Some(Instant::now());
    }

    /// Say that a newer build is installed which this process cannot become
    /// in place (`platform::REPLACES_IN_PLACE`) — once per window, as it
    /// stays true until Rune is opened again.
    fn tell_newer_build(&mut self) {
        if !self.told_newer_build {
            self.told_newer_build = true;
            self.note("A newer Rune is installed; it is used when Rune is opened again.");
        }
    }

    pub fn clear_status(&mut self) {
        self.status = None;
        self.status_at = None;
    }

    /// Adjusts `palette_scroll` maintaining a 1-item scroll margin:
    /// stopping at the 2nd row from bottom when moving down, and the 2nd row from top
    /// when moving up.
    pub fn adjust_palette_scroll(&mut self, total: usize) {
        self.palette_scroll = draw::compute_palette_first(
            self.palette_row,
            self.palette_scroll,
            draw::PALETTE_ROWS,
            total,
        );
    }

    /// A question was answered — by this window or another — and the step that
    /// asked it goes on: it is running again, not still asking. Without this a
    /// window that did not give the answer kept showing the step as waiting.
    fn resume_asking_tools(&mut self) {
        let mut label = None;
        for tool in self.blocks.iter_mut().filter_map(Block::tool_mut) {
            if tool.state == ToolState::Asking {
                tool.state = ToolState::Running;
                tool.ask = None;
                label = Some(steps::label(&tool.name).to_string());
            }
        }
        if let Some(label) = label {
            self.label = label;
        } else if self.label == "Waiting on you" {
            self.label.clear();
        }
    }

    /// `Instant` for a moment of the engine's clock (Unix ms), so a timer that
    /// started before this window did still reads the time it has really run.
    pub fn instant_at(at_ms: i64) -> Instant {
        let now_ms = runeengine::store::now_ms();
        let ago = if at_ms > 0 {
            now_ms.saturating_sub(at_ms).max(0) as u64
        } else {
            0
        };
        Instant::now()
            .checked_sub(Duration::from_millis(ago))
            .unwrap_or_else(Instant::now)
    }

    /// Take a message this window queued back off the screen: its block and
    /// its place among the messages waiting for the engine.
    fn withdraw_steer(&mut self, text: &str) {
        let Some(queued) = &mut self.queued_prompt else {
            return;
        };
        let Some(k) = queued
            .steers
            .iter()
            .position(|steer| !steer.remote && steer.pending_id.is_none() && steer.text == text)
        else {
            return;
        };
        let at = queued.first_user_block + k;
        queued.steers.remove(k);
        if matches!(self.blocks.get(at), Some(Block::User { text: shown, .. }) if shown == text) {
            self.blocks.remove(at);
        }
        if queued.steers.is_empty() {
            self.queued_prompt = None;
        }
        self.transcript_dirty = true;
    }

    /// The engine has stored a message and says so: if this window already drew
    /// it — as its own steer, as a message it sent, or as another window's
    /// message waiting for the running turn — that block is the message, and it
    /// is kept where it is; otherwise it is drawn now, with its files.
    fn adopt_or_draw_user(
        &mut self,
        text: String,
        media: Vec<runeengine::media::Attachment>,
        time: String,
    ) {
        self.transcript_dirty = true;
        if !text.is_empty() && !text.starts_with('/') && self.prompt_history.last() != Some(&text) {
            self.prompt_history.push(text.clone());
        }
        // 1. A message waiting for the running turn, drawn before the engine
        //    stored it.
        if let Some(queued) = &mut self.queued_prompt {
            if let Some(k) = queued
                .steers
                .iter()
                .position(|steer| steer.text.trim() == text.trim())
            {
                let at = queued.first_user_block + k;
                if let Some(Block::User { media: shown, .. }) = self.blocks.get_mut(at) {
                    if shown.is_empty() {
                        *shown = media;
                    }
                }
                let steer = queued.steers.remove(k);
                if let Some(id) = steer.pending_id {
                    self.shown_pending.remove(&id);
                }
                if k == 0 {
                    queued.first_user_block += 1;
                }
                if queued.steers.is_empty() {
                    self.queued_prompt = None;
                }
                return;
            }
        }
        // 2. Another window's message: drawn now.
        self.blocks.push(Block::user_timed(text, Some(time), media));
    }

    /// The composer's messages waiting for the running turn, drawn on every
    /// window of the session — as the window that sent them drew them — so all
    /// show the same. A message this window sent is already there and is only
    /// recognised; another window's is drawn where queued messages go.
    pub fn sync_pending(&mut self, pending: &[runeengine::store::composer::PendingPrompt]) {
        let live: std::collections::HashSet<i64> = pending.iter().map(|p| p.id).collect();
        self.shown_pending.retain(|id| live.contains(id));
        for entry in pending {
            if self.shown_pending.contains(&entry.id) {
                continue;
            }
            let own = self.queued_prompt.as_mut().and_then(|queued| {
                queued.steers.iter_mut().find(|steer| {
                    !steer.remote
                        && steer.pending_id.is_none()
                        && steer.text.trim() == entry.text.trim()
                })
            });
            if let Some(steer) = own {
                steer.pending_id = Some(entry.id);
                self.shown_pending.insert(entry.id);
                continue;
            }
            let index = self.blocks.len();
            self.blocks.push(Block::user_timed(
                entry.text.clone(),
                Some(term::now_hm()),
                entry.media.clone(),
            ));
            let steer = QueuedSteer {
                text: entry.text.clone(),
                remote: true,
                pending_id: Some(entry.id),
            };
            match &mut self.queued_prompt {
                Some(queued) => queued.steers.push(steer),
                None => {
                    self.queued_prompt = Some(QueuedPrompt {
                        steers: vec![steer],
                        first_user_block: index,
                    })
                }
            }
            self.shown_pending.insert(entry.id);
            self.transcript_dirty = true;
        }
    }

    pub fn push_block(&mut self, block: Block) {
        if let Some(queued) = &mut self.queued_prompt {
            let idx = queued.first_user_block.min(self.blocks.len());
            self.blocks.insert(idx, block);
            queued.first_user_block += 1;
            self.transcript_dirty = true;
            return;
        }
        self.blocks.push(block);
        self.transcript_dirty = true;
    }

    fn tool_by_id(&mut self, id: &str) -> Option<&mut Tool> {
        self.blocks
            .iter_mut()
            .rev()
            .find_map(|b| b.tool_mut().filter(|t| t.id == id))
    }

    /// Follow `rune.toml` when it changed on disk — by this window, another
    /// one, or the owner's editor. Takes the handle it reads from, so a test can
    /// give it a home of its own; `true` when the screen has to be redrawn.
    ///
    /// A window that dies here dies with every other window of the session
    /// (they all see the same file change at once), so nothing in this function
    /// may panic: a key it reads is checked against the schema by a test
    /// (`every_literal_key_the_code_reads_is_a_field_of_the_right_kind`).
    pub(crate) fn reload_configuration(
        &mut self,
        config: &runeengine::settings::configuration::Shared,
    ) -> bool {
        let mut changed = false;
        // By generation, not by this call's own reload: the shared handle also
        // follows the file on its own, and a change one read took in first is
        // still this window's to show (owner, 2026-10-14).
        let _ = config.reload_if_changed();
        let generation = config.generation();
        if generation != self.config_generation {
            self.config_generation = generation;
            self.pending_provider_refresh = true;
            // `ui.color`: the depth is resolved once and cached; a choice saved
            // anywhere is a new depth here, and every cached ink with it.
            let depth = ansi::depth();
            ansi::invalidate_depth();
            if ansi::depth() != depth {
                changed = true;
                self.transcript_cache = None;
                self.transcript_dirty = true;
                self.resync = true;
            }
            let mouse = config.flag("ui.mouse");
            if self.mouse != mouse {
                self.mouse = mouse;
                self.mouse_dirty = true;
                changed = true;
            }
            if let Some(mode) = theme::ThemeMode::parse(&config.text("ui.theme")) {
                if self.theme != mode {
                    self.theme = mode;
                    theme::set_theme(mode);
                    changed = true;
                    self.transcript_cache = None;
                    self.transcript_dirty = true;
                    self.resync = true;
                }
            }
            let sidebar = config.flag("ui.sidebar");
            if self.sidebar != sidebar {
                self.sidebar = sidebar;
                changed = true;
                self.transcript_dirty = true;
                self.resync = true;
            }
        }
        // What the configuration was misread as, if anything: a bug in this
        // build, said once in /logs rather than guessed at silently.
        for line in runeengine::settings::configuration::take_misuse() {
            self.log(runeengine::proto::LogEntry {
                at: runeengine::store::now_ms(),
                level: runeengine::proto::LogLevel::Warn,
                source: "config".into(),
                code: "config_key".into(),
                message: format!("Rune read a configuration setting wrongly: {line}."),
                turn: None,
                detail: None,
            });
            changed = true;
        }
        changed
    }

    fn tick(&mut self) -> bool {
        let glided = self.glide_step();
        let before = self.frame;
        self.frame = self.frame.wrapping_add(1);
        let mut changed = glided
            || ((self.busy
                || (self.panel == Panel::Settings && self.preferences.pending.is_some()))
                && (before / 6 != self.frame / 6 || before / 38 != self.frame / 38));
        if self.sidebar && (before / 60 != self.frame / 60) {
            changed = true;
        }
        if self.panel == Panel::None
            && self
                .transcript_cache
                .as_ref()
                .is_some_and(draw::Transcript::head_pending)
        {
            changed = true;
        }
        if self.frame.is_multiple_of(6) {
            changed |= self.reload_configuration(runtime_configuration());
        }
        let now = Instant::now();
        changed |= self.preferences.expire_notice(now);
        changed |= undo::tick(self, now);
        for tool in self.blocks.iter_mut().filter_map(Block::tool_mut) {
            changed |= tool.advance_counters(now);
        }
        if self.busy && changed {
            self.transcript_dirty = true;
        }

        if let Some(at) = self.status_at {
            if now.duration_since(at) >= runtime_configuration().duration("ui.status_lifetime") {
                self.status = None;
                self.status_at = None;
                changed = true;
            }
        } else if self.status.is_some() {
            // Started by direct field assignment: measure TTL from now
            self.status_at = Some(now);
        }

        if let Some(at) = self.quit_armed_at {
            if now.duration_since(at) >= runtime_configuration().duration("ui.quit_lifetime") {
                self.quit_armed = false;
                self.quit_armed_at = None;
                changed = true;
            }
        }

        if let Some(at) = self.stop_armed_at {
            if now.duration_since(at) >= runtime_configuration().duration("ui.quit_lifetime") {
                self.stop_armed = false;
                self.stop_armed_at = None;
            }
        }

        if let Some(q) = &mut self.questions {
            if let Some(at) = q.esc_armed_at {
                if now.duration_since(at) >= Duration::from_secs(3) {
                    q.esc_armed = false;
                    q.esc_armed_at = None;
                    if self.status.as_deref() == Some("press esc again to close") {
                        self.clear_status();
                    }
                    changed = true;
                }
            }
        }

        if self.panel == Panel::Settings {
            if let Some(e) = self.preferences.editor.as_mut() {
                if let Some(at) = e.confirm_apply_at {
                    if now.duration_since(at) >= std::time::Duration::from_secs(3) {
                        e.confirm_apply_at = None;
                        changed = true;
                    } else if before / 15 != self.frame / 15 {
                        changed = true;
                    }
                }
            }
        }

        changed
    }

    /// One notch of the wheel over the transcript.
    ///
    /// A notch moves `ui.scroll_lines` rows, however fast the notches come:
    /// the owner asked for a fixed distance (2026-10-09), and a spin that
    /// grew on its own read as the page running away. With
    /// `ui.scroll_smooth` the rows are not jumped but owed: [`Glide`] holds the
    /// distance and every tick pays a share of it, so the page eases to where
    /// the wheel pointed — the first share is paid at once, so the reply to the
    /// hand is not a frame late. On a terminal too slow to show the glide as
    /// motion ([`App::terminal_keeps_up`]) the rows are jumped all the same.
    #[cfg(test)]
    pub fn wheel_scroll(&mut self, up: bool) {
        self.wheel_scroll_at(up, false);
    }

    /// A notch with Ctrl or Alt held: the same notch, [`FAST_WHEEL`] times as
    /// far.
    ///
    /// Only the distance changes, through the same door a plain notch uses:
    /// the glide still pays a third of what is owed a frame and the frames
    /// still wait for the terminal (`Screen::AHEAD`), so a fast notch is a
    /// longer glide, never a new kind of motion — and a terminal that cannot
    /// show a glide still jumps.
    pub fn wheel_scroll_at(&mut self, up: bool, fast: bool) {
        let config = runtime_configuration();
        let base = config.number("ui.scroll_lines") as usize;
        let rows = if fast { base * FAST_WHEEL } else { base };
        if !config.flag("ui.scroll_smooth") || !self.terminal_keeps_up {
            self.glide = None;
            self.scroll_by(up, rows);
            return;
        }
        let max = self.scroll_limit() as isize;
        let owed = self.glide.map_or(0, |g| g.owed);
        // Toward the top the page owes `+`, toward the live end `-`.
        let wanted = owed + if up { rows as isize } else { -(rows as isize) };
        // Never owe a place past either end: the rest would be a glide that
        // runs into the wall and shows nothing.
        let wanted = wanted.clamp(-(self.scroll as isize), (max - self.scroll as isize).max(0));
        self.glide = Some(Glide { owed: wanted });
        self.glide_step();
    }

    /// How far up the transcript goes, as the last frame measured it. Before
    /// any frame there is no measure, and the renderer is left to clamp.
    fn scroll_limit(&self) -> usize {
        if self.drawn_viewport == 0 {
            usize::MAX / 4
        } else {
            self.drawn_rows.saturating_sub(self.drawn_viewport)
        }
    }

    /// Pay one share of what the wheel owes the page. Returns whether the page
    /// moved. The share is a third of the distance (at least a row), which is
    /// what makes the end of a long gesture ease out rather than stop dead.
    pub fn glide_step(&mut self) -> bool {
        let max = self.scroll_limit();
        let Some(glide) = self.glide.as_mut() else {
            return false;
        };
        if glide.owed == 0 {
            self.glide = None;
            return false;
        }
        let step = (glide.owed.abs() / 3).max(1) * glide.owed.signum();
        glide.owed -= step;
        let moved = (self.scroll as isize + step).clamp(0, max as isize) as usize;
        let changed = moved != self.scroll;
        if moved == 0 || moved == max {
            // At an end the rest is not owed to anyone.
            glide.owed = 0;
        }
        if glide.owed == 0 {
            self.glide = None;
        }
        self.scroll = moved;
        self.scroll_anchor = None;
        changed
    }

    /// Back to the newest row of the conversation, followed from there on:
    /// Ctrl+↓, or a click on the hint that names it.
    pub fn jump_to_latest(&mut self) {
        self.glide = None;
        self.scroll = 0;
        self.scroll_anchor = None;
    }

    /// Move through transcript history and invalidate the content anchor so the
    /// next frame treats this as an intentional user move. Zero means follow.
    pub fn scroll_by(&mut self, up: bool, rows: usize) {
        self.glide = None;
        self.scroll = if up {
            self.scroll.saturating_add(rows)
        } else {
            self.scroll.saturating_sub(rows)
        };
        self.scroll_anchor = None;
    }

    /// Move through the plan inside the side panel.
    ///
    /// `max` is how far the plan can be scrolled at all, which the renderer
    /// knows and this does not: whether the plan is taller than the panel
    /// depends on the terminal and on how the items wrap. Clamping here means
    /// a plan that shortens — an item removed — cannot leave the panel showing
    /// empty rows below the last one.
    ///
    /// # `up` moves the window toward the top
    ///
    /// [`plan_scroll`](App::plan_scroll) counts rows dropped from the **top**,
    /// unlike [`scroll`](App::scroll), which counts rows raised from the
    /// bottom. So scrolling up *subtracts* here, and a reader pressing up at
    /// the live end lands on the newest rows first — which is the row they
    /// were already on. Getting this backwards is not a small slip: the panel
    /// refuses to reach its own first item.
    ///
    /// The anchor is dropped on a deliberate move, exactly as the transcript's
    /// is: the reader has chosen a new place, and the next frame should honour
    /// that rather than pull them back to the one they just left.
    pub fn plan_scroll_by(&mut self, up: bool, rows: usize, max: usize) {
        self.plan_scroll = if up {
            self.plan_scroll.saturating_sub(rows)
        } else {
            self.plan_scroll.saturating_add(rows)
        }
        .min(max);
        self.plan_anchor = None;
    }
    /// Put the plan back at its live end, and follow it there.
    ///
    /// Called wherever the plan is replaced rather than merely worked on: a new
    /// turn, a restored session, `/new`. `usize::MAX` parks the window past the
    /// end of any list, which the renderer reads as "follow the newest rows" —
    /// so a fresh plan opens at the work in progress rather than at the top of
    /// a list that has not been started.
    pub fn reset_plan_scroll(&mut self) {
        self.plan_scroll = usize::MAX;
        self.plan_anchor = None;
    }

    // ─── Events ──────────────────────────────────────────────────────────────

    pub fn on_event(&mut self, event: Event) {
        self.apply_event(event);
        self.recount();
    }

    /// The two counts the side panel shows, taken from what is on the screen —
    /// one definition for a window that watched and one that joined: a step is
    /// a tool call, a turn is a message that opened one.
    fn recount(&mut self) {
        self.steps = self
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Tool(_)))
            .count();
        // A message waiting for the running turn has not opened one yet.
        let waiting = self.queued_prompt.as_ref().map_or(0, |q| q.steers.len());
        self.turns = self
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::User { .. } | Block::Notification { .. }))
            .count()
            .saturating_sub(waiting);
    }

    fn apply_event(&mut self, event: Event) {
        // One gate for the whole stream. The engine's event channel is global —
        // one engine serves many sessions, and the session IPC host forwards the
        // stream to every connected window — so an event naming another session
        // is that session's state arriving here: its mode, its plan, its file
        // tallies, its counters, its status line. Applying any of it made this
        // screen show a mixture of two conversations, which is what a `/new` (or
        // a second window) made visible the moment any other session moved.
        // The mcp_input/questions branches above are ack-correlated state of
        // *this* screen's own request and answer nothing about another session.
        if let Some(owner) = event.session() {
            if Some(owner) != self.session {
                return;
            }
        }
        // Asked for the session's view and not given it yet: this event is
        // before the answer in the stream, so it is inside the answer.
        if self.awaiting_view
            && !matches!(
                event,
                Event::Ack { .. }
                    | Event::HostUpgrading { .. }
                    | Event::Upgrading
                    | Event::Ready { .. }
            )
        {
            return;
        }
        self.transcript_dirty = true;
        if let Event::Ack { id, ok, error, .. } = &event {
            if let Some(form) = self.mcp_input.as_mut().filter(|f| f.sending == Some(*id)) {
                if !ok {
                    form.sending = None;
                    form.error = error.clone();
                }
                return;
            }
            if let Some(state) = self
                .questions
                .as_mut()
                .filter(|state| state.sending == Some(*id))
            {
                if !ok {
                    state.sending = None;
                    state.error = Some(
                        error
                            .clone()
                            .unwrap_or_else(|| "Could not submit answers".into()),
                    );
                }
                return;
            }
        }
        match event {
            Event::Composer {
                request, composer, ..
            } => {
                self.sync_pending(&composer.pending);
                self.composer_event(request, *composer);
            }
            Event::McpInputRequest {
                req,
                server_id,
                request,
                ..
            } => {
                self.mcp_input = Some(mcp_input::Form::new(req, server_id, request));
                self.label = "Waiting on you".into();
            }
            Event::McpInputResolved { req, .. } => {
                if self.mcp_input.as_ref().is_some_and(|f| f.req == req) {
                    self.mcp_input = None;
                    self.resume_asking_tools();
                }
            }
            Event::QuestionsRequest {
                req, id, questions, ..
            } => {
                self.questions = Some(questions::State::new(req, questions));
                self.label = "Waiting on you".into();
                if let Some(tool) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .filter_map(Block::tool_mut)
                    .find(|tool| tool.id == id)
                {
                    tool.state = ToolState::Asking;
                }
            }
            Event::QuestionsResolved { req, .. } => {
                if self
                    .questions
                    .as_ref()
                    .is_some_and(|state| state.req == req)
                {
                    self.questions = None;
                }
                self.resume_asking_tools();
            }
            // A background task ended while the session was busy: shown now,
            // where it happened; the model reads it with its next request, and
            // that `turn_start` draws nothing new (`notified`).
            Event::Notification { text, at, .. } => {
                let time = if at > 0 {
                    term::epoch_hm(at)
                } else {
                    term::now_hm()
                };
                self.notified.push(text.clone());
                self.push_block(Block::Notification {
                    text,
                    time: Some(time).filter(|t| !t.is_empty()),
                });
            }
            // What Rune told the model behind the conversation draws nothing:
            // it stays in `/context` and `/logs` (the replay above agrees).
            Event::Context { .. } => {}
            Event::TurnStart {
                turn,
                text,
                notification,
                media,
                at,
                ..
            } => {
                self.busy = true;
                let now = runeengine::store::now_ms();
                // The engine's clock, so a window that joins late counts from
                // the same moment as the one that was here.
                self.turn_started
                    .get_or_insert_with(|| Self::instant_at(at));
                if self.started_at == 0 {
                    self.started_at = if at > 0 { at } else { now };
                }
                if self.run_started_at == 0 {
                    self.run_started_at = if at > 0 { at } else { now };
                }
                self.label.clear();
                self.clear_status();
                let time = if at > 0 {
                    term::epoch_hm(at)
                } else {
                    term::now_hm()
                };

                if turn <= self.stored_turn {
                    // Already drawn from the stored conversation (see
                    // `stored_turn`): the turn starts, its message is on screen.
                } else if let (Some(t), true) = (&text, notification) {
                    // Rune's own notification opens this turn: drawn as one, the
                    // engine's word on what it is, never the user's words — once:
                    // a notification shown when it was raised is the same one.
                    if let Some(at) = self.notified.iter().position(|shown| shown == t) {
                        self.notified.remove(at);
                    } else {
                        self.blocks.push(Block::Notification {
                            text: t.clone(),
                            time: Some(time),
                        });
                        self.transcript_dirty = true;
                    }
                } else if let Some(t) = text {
                    self.adopt_or_draw_user(t, media, time);
                }
            }
            Event::Text { delta, .. } => {
                self.retry_status = None;
                self.clear_status();
                self.append_text(&delta);
                self.label = "Writing".into();
            }
            Event::Thinking { delta, .. } => {
                self.retry_status = None;
                self.clear_status();
                if self.label.is_empty() {
                    self.label = "Thinking".into();
                }
                if let Some(queued) = &self.queued_prompt {
                    let limit = queued.first_user_block.min(self.blocks.len());
                    if limit > 0 && matches!(self.blocks[limit - 1], Block::Thinking { .. }) {
                        if let Block::Thinking { text, .. } = &mut self.blocks[limit - 1] {
                            text.push_str(&delta);
                            self.transcript_dirty = true;
                            return;
                        }
                    }
                    self.push_block(Block::Thinking {
                        text: delta,
                        open: false,
                    });
                    return;
                }
                // A delta is new text, always: appended verbatim, never compared
                // with what is shown — one that happens to equal the tail ("\n",
                // ".", a repeated word) is real.
                match self.blocks.last_mut() {
                    Some(Block::Thinking { text, .. }) => {
                        text.push_str(&delta);
                        self.transcript_dirty = true;
                    }
                    _ => {
                        self.push_block(Block::Thinking {
                            text: delta,
                            open: false,
                        });
                    }
                }
            }
            Event::ToolArgs {
                id, name, delta, ..
            } => {
                self.retry_status = None;
                self.clear_status();
                self.label = steps::label(&name).to_string();
                match self.tool_by_id(&id) {
                    Some(tool) => tool.absorb(&delta),
                    None => {
                        let mut tool = Tool::new(id, name, String::new());
                        tool.absorb(&delta);
                        self.push_block(Block::of(tool));
                    }
                }
            }
            Event::ToolCall {
                id,
                name,
                action,
                args,
                at,
                ..
            } => {
                self.clear_status();
                self.label = steps::label(&name).to_string();
                match self.tool_by_id(&id) {
                    Some(tool) => {
                        tool.action = action;
                        tool.settle(&args);
                        // The engine's clock: the timer reads the same in every window.
                        tool.started = Some(Self::instant_at(at));
                    }
                    None => {
                        let mut tool = Tool::new(id, name, action);
                        tool.settle(&args);
                        tool.started = Some(Self::instant_at(at));
                        self.push_block(Block::of(tool));
                    }
                }
            }
            Event::ToolOutput {
                id,
                chunk,
                rows,
                cols,
                ..
            } => {
                if let Some(tool) = self.tool_by_id(&id) {
                    tool.absorb_output(&chunk, rows, cols);
                }
            }
            Event::ToolResult {
                id,
                ok,
                summary,
                content,
                detail,
                at,
                ..
            } => {
                if let Some(tool) = self.tool_by_id(&id) {
                    // The engine's own word, not a phrase in the summary: the
                    // phrase this once matched was never produced, so a
                    // background launch never said it was one (audit D10).
                    tool.state = if detail.as_ref().and_then(|d| d["status"].as_str())
                        == Some("backgrounded")
                    {
                        ToolState::Backgrounded
                    } else if ok {
                        ToolState::Ok
                    } else if detail.as_ref().and_then(|d| d["status"].as_str())
                        == Some("cancelled")
                        || summary.contains("stopped by the user")
                    {
                        ToolState::Stopped
                    } else {
                        ToolState::Failed
                    };
                    tool.summary = summary;
                    tool.content = content;
                    tool.detail = detail;
                    tool.ended = Some(Self::instant_at(at));
                    tool.ask = None;
                    tool.trimmed_lines = 0;
                    tool.terminal = None;
                }
                // The tool is over: the footer must not go on saying what it was.
                self.label.clear();
            }
            Event::PermissionRequest {
                req,
                preview,
                remember_as,
                reach,
                local_services,
                full_access,
                action,
                reason,
                ..
            } => {
                let ask = blocks::Ask {
                    req,
                    preview,
                    remember_as,
                    reach,
                    local_services,
                    full_access,
                    action,
                    reason,
                };
                // Every question starts on its first answer. Carrying the last
                // one over would mean a card that opens on "Always allow"
                // because the previous card happened to end there.
                self.ask_choice = 0;
                self.pending = Some(ask.clone());
                // Not the tool's name. The card below says what is being asked
                // in the model's own words, and naming the registry entry here
                // was the one place an identifier still reached the screen.
                self.label = "Waiting on you".into();
                if let Some(t) = self.blocks.iter_mut().rev().find_map(|b| b.tool_mut()) {
                    t.state = ToolState::Asking;
                    t.ask = Some(ask);
                }
            }
            Event::PermissionResolved { .. } => {
                self.pending = None;
                self.resume_asking_tools();
            }
            Event::Tasks { tasks, .. } => {
                self.tasks = tasks
                    .iter()
                    .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                    .collect();
            }
            Event::AuxiliaryUsage {
                input,
                output,
                cached,
                cost,
                ..
            } => {
                self.tok_in += input;
                self.tok_out += output;
                self.cache_in += cached;
                self.input_tokens += input;
                self.output_tokens += output;
                self.cached_tokens += cached;
                self.cost += cost.unwrap_or(0.0);
                if cost.is_none() {
                    self.unpriced_calls += 1;
                }
            }
            Event::Usage {
                input,
                output,
                cached,
                cost,
                details,
                ..
            } => {
                self.cost += cost.unwrap_or(0.0);
                // Only a request the provider actually measured can be a
                // pricing gap. One that reported no usage failed or was
                // cancelled, and nothing was billed for it — counting it would
                // blank out a real spend for the rest of the session over a
                // request that cost nothing. Mirrors `session_usage`.
                if cost.is_none()
                    && (input > 0 || output > 0 || details.input_known || details.output_known)
                {
                    self.unpriced_calls += 1;
                }
                if details.input_known || details.native.is_null() && input > 0 {
                    self.context_used = input;
                }
                self.tok_in += input;
                self.tok_out += output;
                self.cache_in += cached;
                self.input_tokens += input;
                self.output_tokens += output;
                self.cached_tokens += cached;
                self.check_spend();
            }
            Event::Completion { .. } => {}
            Event::Retrying {
                attempt,
                of,
                wait_ms,
                rotated,
                reason,
                ..
            } => {
                let secs = wait_ms.div_ceil(1_000).max(1);
                let text = if rotated {
                    format!("{reason} — trying the next key ({attempt} of {of})")
                } else if wait_ms == 0 {
                    format!("{reason} — resuming ({attempt} of {of})")
                } else {
                    format!("{reason} — retrying in {secs}s ({attempt} of {of})")
                };
                if wait_ms > 0 {
                    self.retry_status = Some(text);
                } else {
                    self.retry_status = None;
                    self.note(text);
                }
            }
            // Capturing an edit is not a status message or an undo shortcut.
            // The person opens /undo when they want the checkpoint history.
            Event::Checkpoint { .. } => {}
            Event::FileChanges { changes, .. } => {
                // The engine only sends what a round actually settled — every
                // capture was pre-write and every call closed — so this is the
                // session's tally as it stands now, not a forecast.
                self.file_changes = changes
                    .iter()
                    .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
                    .collect();
            }
            // One thin line where the summary took over; what it summarised and
            // what it cost are in the log.
            Event::Compacted { through_turn, .. } => {
                self.push_block(Block::Divider {
                    text: format!("history summarised through turn {through_turn}"),
                });
            }
            Event::Done { stop, .. } => {
                // Every notification shown during the run was handed over (or
                // withdrawn, the model having read the whole report) before its
                // `done`.
                self.notified.clear();
                self.retry_status = None;
                self.busy = false;
                self.turn_started = None;
                self.pending = None;
                self.questions = None;
                self.label.clear();
                let now = Instant::now();
                for tool in self.blocks.iter_mut().filter_map(Block::tool_mut) {
                    if !tool.state.finished() {
                        tool.state = if stop == runeengine::proto::StopReason::Cancelled {
                            ToolState::Stopped
                        } else {
                            ToolState::Failed
                        };
                        if tool.ended.is_none() {
                            tool.ended = Some(now);
                        }
                        tool.ask = None;
                        tool.snap_counters();
                    }
                }
                self.status = match stop {
                    runeengine::proto::StopReason::Refused => {
                        Some("The model declined the request.".into())
                    }
                    runeengine::proto::StopReason::Cancelled if self.queued_prompt.is_none() => {
                        Some("stopped".into())
                    }
                    _ => None,
                };
                // How long the session worked is counted by the engine, which adds
                // this run's time to the session before it says `done`: this
                // window adds the same, rounded the same way, and stops its clock.
                if self.run_started_at > 0 {
                    let spent = (runeengine::store::now_ms().saturating_sub(self.run_started_at)
                        + 500)
                        / 1000;
                    self.accumulated_seconds =
                        self.accumulated_seconds.saturating_add(spent.max(0) as u64);
                    self.run_started_at = 0;
                }
            }
            // The attempt that streamed these failed and is being repeated: its
            // partial output goes, so the screen matches what the model will
            // be shown — the repeat streams it again from the start.
            Event::StreamReset { .. } => {
                // What the failed attempt said sits where the stream's blocks
                // go — before any message waiting for the turn, which is drawn
                // after them — so that is where it is taken away from.
                let mut end = self
                    .queued_prompt
                    .as_ref()
                    .map_or(self.blocks.len(), |queued| {
                        queued.first_user_block.min(self.blocks.len())
                    });
                while end > 0
                    && match &self.blocks[end - 1] {
                        Block::Text { .. } | Block::Thinking { .. } => true,
                        Block::Tool(tool) => tool.state == ToolState::Writing,
                        _ => false,
                    }
                {
                    self.blocks.remove(end - 1);
                    end -= 1;
                    if let Some(queued) = &mut self.queued_prompt {
                        queued.first_user_block = queued.first_user_block.saturating_sub(1);
                    }
                }
                self.label.clear();
                self.transcript_dirty = true;
            }
            // Consumed by the event source, which starts the reconnect.
            // A window talks to a session host over its socket; `upgrading`
            // belongs to the stdio host (`rune -host`) and never reaches it.
            Event::HostUpgrading { .. } | Event::Upgrading => {}
            // Behind the conversation: kept for the log panel, never drawn into
            // the transcript. A warning or an error is counted in the footer
            // until the panel has been looked at.
            Event::Log { entry, .. } => self.log(entry),
            Event::Error { message, .. } => {
                self.retry_status = None;
                self.clear_status();
                self.push_block(Block::Notice {
                    text: message,
                    bad: true,
                });
                self.busy = false;
                self.turn_started = None;
                self.pending = None;
                self.questions = None;
                self.label.clear();
                let now = Instant::now();
                for tool in self.blocks.iter_mut().filter_map(Block::tool_mut) {
                    if !tool.state.finished() {
                        tool.state = ToolState::Failed;
                        if tool.ended.is_none() {
                            tool.ended = Some(now);
                        }
                        tool.ask = None;
                        tool.snap_counters();
                    }
                }
            }
            Event::Title { title, .. } => {
                self.title = Some(title);
            }
            Event::Session {
                mode, classifier, ..
            } => {
                self.mode = mode;
                // The engine's own answer about the classifier, whichever
                // window changed the mode.
                if !classifier.is_null() {
                    self.settings["classifier_status"] = classifier;
                }
            }
            Event::Settings { settings, .. } => {
                if self.preferences.pending.is_none() {
                    self.take_settings(settings);
                    self.transcript_dirty = true;
                }
            }
            Event::Ack {
                id,
                ok,
                error,
                data,
            } => self.on_ack(id, ok, error, data),
            Event::Ready { version, .. } => {
                self.host_version = version;
            }
        }
    }

    /// The answer to something the screen asked for.
    fn on_ack(&mut self, id: u64, ok: bool, error: Option<String>, data: Option<Value>) {
        let Some(what) = self.awaiting.remove(&id) else {
            return;
        };
        if matches!(what, Awaiting::DraftSaved | Awaiting::ComposerEdit) {
            self.composer_ack(id, ok, &data.clone().unwrap_or(Value::Null));
            if !ok {
                self.status = error;
            }
            return;
        }
        if let Awaiting::Preferences(request) = what {
            settings::ack(self, request, ok, error, data);
            return;
        }
        if !ok {
            if matches!(what, Awaiting::Promoted) {
                self.composition.promote_pending = false;
            }
            if let Awaiting::Mode { previous, .. } = &what {
                self.mode = previous.clone();
            }
            // A refused restore or diff is said in the undo panel.
            if matches!(what, Awaiting::Restored { .. } | Awaiting::UndoDiff { .. }) {
                if matches!(what, Awaiting::Restored { .. }) {
                    self.status = None;
                }
                undo::refused(self, error.as_deref());
            }
            if let Awaiting::Prompt { text, media } = what {
                // Refused (an attachment gone, the session closed): the message
                // was never the engine's. It leaves the queue and the screen, and
                // its words go back to the composer — before anything typed
                // since, never instead of it.
                self.withdraw_steer(&text);
                if self.input.trim().is_empty() {
                    self.input = text;
                } else {
                    self.input = format!("{text}\n{}", self.input);
                }
                self.cursor = self.input.len();
                for file in media {
                    if !self.attached.iter().any(|a| a.path == file.path) {
                        self.attached.push(file);
                    }
                }
            }
            self.status = error;
            return;
        }
        let data = data.unwrap_or(Value::Null);
        match what {
            Awaiting::DraftOpened => {
                if let Some(draft) = data
                    .get("draft")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                {
                    self.take_draft(draft, None);
                }
            }
            Awaiting::Promoted => {
                self.composition.promote_pending = false;
                if let (Some(session), Some(token)) =
                    (data["session"].as_i64(), data["promotion"].as_str())
                {
                    self.session = Some(session);
                    self.composition.draft = None;
                    self.composition.flight = None;
                    self.composition.promotion = Some(token.to_string());
                    let committed = runeengine::store::composer::Composer {
                        settings: serde_json::from_value(self.settings["settings"].clone())
                            .unwrap_or_default(),
                        mode: self.mode.clone(),
                        ..Default::default()
                    };
                    let visible = self.composer_snapshot();
                    let first_prompt = self
                        .composition
                        .submitted
                        .as_ref()
                        .map(|sent| sent.text.clone());
                    let next = self.composition.submitted.take().map_or_else(
                        || committed.clone(),
                        |sent| composer::rebase(&sent, &visible, &committed),
                    );
                    self.restore_composer(next, false);
                    self.composition.authoritative = committed;
                    self.composition.ready = true;
                    self.prompt_history = first_prompt
                        .filter(|text| !text.is_empty())
                        .into_iter()
                        .collect();
                    self.history_index = None;
                    self.history_draft.clear();
                }
            }
            Awaiting::DraftSaved | Awaiting::ComposerEdit => {
                unreachable!("handled before general acknowledgements")
            }
            Awaiting::Settings { serial } => {
                if serial == self.preferences.serial {
                    self.take_settings(data);
                } else {
                    // Asked before a preference changed: read again once the
                    // change has its answer.
                    self.pending_provider_refresh = true;
                }
            }
            Awaiting::Providers => {
                self.providers = data;
                if !self.providers["web"].is_null() {
                    self.settings["web"] = self.providers["web"].clone();
                }
                self.sync_active_models();
            }
            Awaiting::Capabilities => {
                self.capability_view = data;
                self.note("Capabilities updated");
            }
            Awaiting::Prompt { .. } => {
                if let Some(saved) = data.get("composer").and_then(|value| {
                    serde_json::from_value::<runeengine::store::composer::Composer>(value.clone())
                        .ok()
                }) {
                    // A boundary flush on another subscribed connection can
                    // observe a newer revision before this prompt's queued ACK
                    // is applied. An ACK confirms that submit; it cannot rewind
                    // the editor's authoritative state.
                    if saved.revision >= self.composition.authoritative.revision {
                        self.composition.authoritative = saved;
                    }
                    self.composition.submitted = None;
                }
            }
            Awaiting::Catalogue(provider) => {
                if provider == self.provider() {
                    self.settings["models"] = data["configured"].clone();
                    self.status = Some("Attachment support checked".into());
                }
            }
            Awaiting::Resumed => {
                if let Some(version) = data.get("version").and_then(Value::as_str) {
                    self.host_version = version.to_string();
                }
                if let Some(settings) = data.get("settings").filter(|s| !s.is_null()) {
                    self.take_settings(settings.clone());
                }
                // The engine's mode is the truth; a switch the old host never
                // acknowledged must not survive on screen.
                if let Some(mode) = data.get("mode").and_then(Value::as_str) {
                    self.mode = mode.to_string();
                }
                self.protocol_skew = data.get("protocol").and_then(Value::as_u64)
                    != Some(u64::from(runeengine::proto::PROTOCOL));
                self.preferences.pending = None;
                self.pending_provider_refresh = true;
                self.transcript_dirty = true;
                self.resync = true;
                self.handover_due = true;
                self.note("Updated Rune engine is active; session reconnected.");
            }
            Awaiting::Mode { next, .. } => {
                self.mode = next.clone();
                if let Some(cs) = data.get("classifier_status") {
                    if !cs.is_null() {
                        self.settings["classifier_status"] = cs.clone();
                    }
                }
                self.clear_status();
            }
            Awaiting::Restored { file, side } => undo::restored(self, side, &file, &data),
            Awaiting::UndoFiles { side } => undo::listed(self, side, &data),
            Awaiting::UndoDiff { file, side } => undo::diffed(self, side, file, data),
            Awaiting::NewSession => {
                if data["session"].is_i64() || data["draft"].is_object() {
                    let session = data["session"].as_i64();
                    if session.is_some() && session == self.session {
                        self.note("Already in this session");
                        return;
                    }
                    self.session = session;
                    self.composition = composer::State::default();
                    self.input_editor.reset();
                    self.input.clear();
                    self.attached.clear();
                    self.cursor = 0;
                    self.retry_status = None;
                    self.mode = data["mode"].as_str().unwrap_or("ask").to_string();
                    self.busy = false;
                    self.label.clear();
                    self.queued_prompt = None;
                    self.pending_model_selection = None;
                    self.mcp_input = None;
                    self.questions = None;
                    self.awaiting.clear();
                    self.stop_armed = false;
                    self.stop_armed_at = None;
                    self.warned_spend = false;
                    self.scroll = 0;
                    self.scroll_anchor = None;
                    self.glide = None;
                    self.drawn_rows = 0;
                    self.drawn_viewport = 0;
                    self.transcript_cache = None;
                    self.transcript_dirty = true;
                    self.reset_plan_scroll();
                    self.plan_max = 0;
                    self.blocks.clear();
                    self.stored_turn = 0;
                    self.tasks.clear();
                    self.undo_diff = None;
                    self.undo_armed = None;
                    self.redo_files.clear();
                    self.file_changes.clear();
                    // The prompt history belongs to a session, like the blocks
                    // above: pgup after `/new` must not recall the previous
                    // session's prompts into the new one, and the draft held
                    // for the history walk is that session's draft too.
                    self.prompt_history.clear();
                    self.history_index = None;
                    self.history_draft.clear();
                    self.turn_started = None;
                    self.pending = None;
                    self.input_tokens = 0;
                    self.output_tokens = 0;
                    self.cached_tokens = 0;
                    self.context_used = 0;
                    self.tok_in = 0;
                    self.tok_out = 0;
                    self.cache_in = 0;
                    self.cost = 0.0;
                    self.unpriced_calls = 0;
                    self.title = data.get("title").and_then(|v| v.as_str()).map(String::from);
                    self.started_at = 0;
                    self.run_started_at = 0;
                    self.accumulated_seconds = 0;
                    self.steps = 0;
                    self.turns = 0;
                    if let Some(settings) = data.get("settings").cloned() {
                        if !settings.is_null() {
                            self.take_settings(settings);
                        }
                    }
                    if let Some(status) = data.get("classifier_status").filter(|v| !v.is_null()) {
                        self.settings["classifier_status"] = status.clone();
                    }
                    if let Some(draft) = data
                        .get("draft")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                    {
                        self.take_draft(draft, data.get("settings").cloned());
                    }
                    self.note(
                        session.map_or_else(
                            || "Project draft".into(),
                            |id| format!("New session #{id}"),
                        ),
                    );
                }
            }
            Awaiting::Sessions => {
                let sessions = data["sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(picker::SessionItem::from_json)
                    .collect();
                self.session_picker = Some(picker::SessionPicker::new(
                    sessions,
                    Some(self.project.clone()),
                ));
                self.resync = true;
            }
            Awaiting::Logs => {
                self.logs = data
                    .get("entries")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                self.transcript_dirty = true;
            }
            Awaiting::View => {
                self.awaiting_view = false;
                self.apply_view(&data);
            }
            Awaiting::Context => {
                if self.panel != Panel::Context {
                    return;
                }
                self.context_view = data;
                self.panel_view.reset();
                self.transcript_dirty = true;
            }
            Awaiting::Compact => {
                let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                match status {
                    "compacted" => {
                        self.note("Session history compacted");
                        self.transcript_dirty = true;
                    }
                    "queued" => {
                        self.note("Compaction queued for next API request boundary");
                    }
                    "nothing_to_compact" => {
                        self.note("History is already compacted or minimal");
                    }
                    _ => {}
                }
            }
            Awaiting::ModelConfig(provider, model, select_session) => {
                let configured_opt = data
                    .get("configured")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                if let Some(ref configured) = configured_opt {
                    self.update_cached_model(&provider, configured);
                }
                if let Some(registry) = data.get("registry") {
                    self.providers = registry.clone();
                }
                if let Some(settings) = data.get("settings").filter(|v| !v.is_null()) {
                    self.take_settings(settings.clone());
                } else if self.provider() == provider && self.model() == model {
                    let m_opt =
                        configured_opt.or_else(|| self.lookup_model_info(&provider, &model));
                    if let Some(m) = m_opt {
                        if let Some(ctx) = m.context {
                            let max_out = m.max_output.unwrap_or(0);
                            let reserve = if max_out > 0 && max_out < ctx {
                                max_out
                            } else {
                                0
                            };
                            self.context = ctx - reserve;
                        } else {
                            self.context = 0;
                        }
                    }
                }
                self.sync_active_models();
                self.preferences.pending = None;
                self.pending_provider_refresh = true;
                self.note(format!("Model '{provider}:{model}' configured and saved"));
                if select_session == self.session {
                    self.pending_model_selection = Some((self.session, provider, model));
                }
            }
            Awaiting::Preferences(_) => unreachable!("handled before general acknowledgements"),
        }
    }

    /// Ask the engine for something and remember what the answer is for.
    ///
    /// A settings change is always followed by re-reading the settings: the
    /// engine is the authority on what is now in force — it clamps, it drops
    /// what a provider ignores — and echoing back what was typed would show a
    /// value that is not the one being used.
    pub fn request(&mut self, d: &mut Driver, what: Awaiting, make: impl FnOnce(u64) -> Command) {
        let id = d.ask(make);
        self.awaiting.insert(id, what);
    }

    /// Re-read everything the panels draw.
    pub fn refresh(&mut self, d: &mut Driver) {
        if self.composition.draft.is_some()
            && self
                .awaiting
                .values()
                .any(|pending| matches!(pending, Awaiting::Preferences(_)))
        {
            // A draft's settings are what the screen holds, and the screen
            // takes a change in flight only from its answer: read them once
            // it has come (the main loop asks again).
            self.pending_provider_refresh = true;
            return;
        }
        let serial = self.preferences.serial;
        if let Some(session) = self.session {
            self.request(d, Awaiting::Settings { serial }, |id| {
                Command::SettingsGet { id, session }
            });
        } else if let Some(mut draft) = self.composition.draft.clone() {
            // What the screen holds, as every other draft request sends it:
            // the stored draft lags the screen until its save is answered, and
            // describing that copy put the model chosen before back.
            draft.composer = self.composer_snapshot();
            self.request(d, Awaiting::Settings { serial }, |id| {
                Command::DraftSettings {
                    id,
                    draft: Box::new(draft),
                    key: None,
                    value: String::new(),
                    provider: None,
                    custom: false,
                }
            });
        }
        self.request(d, Awaiting::Providers, |id| Command::ProviderList { id });
    }

    fn append_text(&mut self, delta: &str) {
        if let Some(queued) = &self.queued_prompt {
            let limit = queued.first_user_block.min(self.blocks.len());
            if limit > 0 && matches!(self.blocks[limit - 1], Block::Text { .. }) {
                if let Block::Text { text } = &mut self.blocks[limit - 1] {
                    text.push_str(delta);
                    self.transcript_dirty = true;
                    return;
                }
            }
            self.push_block(Block::Text {
                text: delta.to_string(),
            });
            return;
        }
        match self.blocks.last_mut() {
            Some(Block::Text { text, .. }) => text.push_str(delta),
            _ => self.push_block(Block::Text {
                text: delta.to_string(),
            }),
        }
    }

    /// Accept a settings payload, wherever it came from.
    ///
    /// One place, because the two doors — the synchronous load at startup and
    /// the ack after a change — must agree about what a payload means.
    pub fn take_settings(&mut self, mut data: Value) {
        // A settings event may carry only model/knob facts. Absence is not an
        // engine announcement that the policy stopped supervising this session.
        if data.get("classifier_status").is_none() {
            if let Some(status) = self.settings.get("classifier_status") {
                data["classifier_status"] = status.clone();
            }
        }
        if data["catalogue_complete"].as_bool() == Some(false) {
            let provider = data["provider"].as_str().unwrap_or("");
            let cached = self.providers["providers"]
                .as_array()
                .and_then(|providers| {
                    providers
                        .iter()
                        .find(|p| p["name"].as_str() == Some(provider))
                })
                .and_then(|p| p["models"].as_array())
                .or_else(|| {
                    (self.provider() == provider)
                        .then(|| self.settings["models"].as_array())
                        .flatten()
                });
            if let Some(cached) = cached {
                let mut models = cached.clone();
                for active in data["models"].as_array().into_iter().flatten() {
                    if let Some(existing) = models.iter_mut().find(|m| m["id"] == active["id"]) {
                        *existing = active.clone();
                    } else {
                        models.push(active.clone());
                    }
                }
                data["models"] = Value::Array(models);
            }
        }
        // Aligned 100% with Cipher ContextWindowGuard: usable = context_window - max_output
        self.context = data["context_usable"]
            .as_u64()
            .or_else(|| {
                let ctx = data["context"].as_u64()?;
                let max_out = data["settings"]["max_output"]
                    .as_u64()
                    .or_else(|| data["max_output_ceiling"].as_u64());
                let reserve = match max_out {
                    Some(m) if m > 0 && m < ctx => m,
                    _ => 0,
                };
                Some(ctx - reserve)
            })
            .unwrap_or(0);
        if let Some(registry) = data.get("registry") {
            self.providers = registry.clone();
        }
        self.settings = data;
    }

    /// What is left in the real terminal after the screen is given back.
    fn farewell(&self) {
        if self.turns == 0 && self.input_tokens == 0 && self.blocks.is_empty() {
            return;
        }
        let done = self
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Tool(_)))
            .count();
        let Some(session) = self.session else {
            return;
        };
        println!(
            "rune · session #{} · {} tool call(s) · {} tokens in, {} out",
            session, done, self.input_tokens, self.output_tokens
        );
        println!("resume it with: rune -resume {}", session);
    }
}

/// Returns the label to display for reasoning mode, or None if thinking is off
/// or already mandatory without a distinct effort level.
///
/// Only explicit effort is shown; binary reasoning has no extra badge.
pub use runeengine::settings::providers::ROUTE_DEFAULT;

/// An effort choice as the settings screen words it.
pub fn effort_label(choice: &str) -> &str {
    if choice == ROUTE_DEFAULT {
        "route default"
    } else {
        choice
    }
}

pub fn thinking_label(thinking: bool, effort: Option<&str>) -> Option<&str> {
    if thinking {
        effort.filter(|s| !s.is_empty() && *s != "off")
    } else {
        None
    }
}

#[cfg(test)]
mod animation_tests {
    use super::*;

    #[test]
    fn new_session_while_old_turn_streams_is_idle_and_rejects_old_events() {
        let mut app = App::for_test();
        let old = app.session.unwrap();
        app.busy = true;
        app.label = "Writing".into();
        app.turn_started = Some(Instant::now());
        app.blocks.push(Block::Text {
            text: "old answer".into(),
        });
        app.awaiting.insert(811, Awaiting::NewSession);
        app.on_ack(
            811,
            true,
            None,
            Some(serde_json::json!({
                "session": old + 1,
                "title": "new",
                "settings": {"settings":{"provider":"p","model":"m"}}
            })),
        );
        assert_eq!(app.session, Some(old + 1));
        assert!(!app.busy, "the old turn's spinner must not follow /new");
        assert!(app.label.is_empty());
        assert!(app.turn_started.is_none());
        assert!(app.blocks.is_empty());
        assert_eq!(app.scroll, 0);
        app.on_event(Event::Text {
            session: old,
            delta: "old stream".into(),
        });
        app.on_event(Event::Done {
            session: old,
            turn: 1,
            stop: runeengine::proto::StopReason::End,
        });
        assert!(app.blocks.is_empty());
        assert!(!app.busy);
        app.on_event(Event::TurnStart {
            session: old + 1,
            turn: 1,
            text: None,
            notification: false,
            media: Vec::new(),
            at: 0,
        });
        assert!(app.busy, "new session still receives its own live stream");
    }

    /// A repeated attempt starts over: the failed one's text, thinking and
    /// streaming arguments leave the screen, and committed blocks stay.
    #[test]
    fn a_stream_reset_drops_only_the_failed_attempts_output() {
        let mut app = App::for_test();
        let session = app.session.unwrap();
        app.blocks.push(Block::user_timed(
            "go".to_string(),
            None::<String>,
            Vec::new(),
        ));
        let mut done = blocks::Tool::new("t0".into(), "terminal_use".into(), "ran".into());
        done.state = ToolState::Ok;
        app.blocks.push(Block::Tool(Box::new(done)));
        app.blocks.push(Block::Thinking {
            text: "half a thought".into(),
            open: false,
        });
        app.blocks.push(Block::Text {
            text: "half an ans".into(),
        });
        app.blocks.push(Block::Tool(Box::new(blocks::Tool::new(
            "t1".into(),
            "create_file".into(),
            "writing".into(),
        ))));
        app.on_event(Event::StreamReset { session });
        assert_eq!(app.blocks.len(), 2);
        assert!(matches!(app.blocks.last(), Some(Block::Tool(t)) if t.id == "t0"));
    }

    /// Audit R17: a replayed result keeps how it ended — from the label beside
    /// the call, not from the words the model read — instead of turning green.
    #[test]
    fn replayed_results_keep_how_they_ended() {
        assert_eq!(state_of("cancelled"), Some(ToolState::Stopped));
        for failed in [
            "denied",
            "failed",
            "not_executed",
            "service_unavailable",
            "timed_out",
            "repeated_failure",
        ] {
            assert_eq!(state_of(failed), Some(ToolState::Failed), "{failed}");
        }
        assert_eq!(state_of("backgrounded"), Some(ToolState::Backgrounded));
        // A command that ran to its end is told by its exit code alone, and a
        // call with no label ran normally.
        assert_eq!(state_of(""), None);
        assert_eq!(state_of("anything else"), None);
    }

    #[test]
    fn checkpoints_do_not_advertise_undo_or_replace_the_composer_status() {
        let mut app = App::for_test();
        app.clear_status();
        app.on_event(Event::Checkpoint {
            session: app.session.unwrap(),
            turn: 3,
            files: 2,
        });
        assert!(app.status.is_none());
        assert_eq!(app.panel, Panel::None);
        app.note("existing status");
        app.on_event(Event::Checkpoint {
            session: app.session.unwrap(),
            turn: 4,
            files: 1,
        });
        assert_eq!(app.status.as_deref(), Some("existing status"));
    }

    /// Owner, 2026-10-09: what Rune tells the model behind the conversation
    /// — a status update, a changed AGENTS.md or memory index, a memory
    /// notice, a restore — is not drawn, live or replayed; `/context` and
    /// `/logs` keep it. A background task's notification still is.
    #[test]
    fn a_context_update_is_told_to_the_model_but_not_drawn() {
        let mut app = App::for_test();
        let session = app.session.unwrap();
        let before = app.blocks.len();
        for kind in ["status", "context_update", "memory", "restore"] {
            app.on_event(Event::Context {
                session,
                turn: 2,
                kind: kind.into(),
                note: "model other (was test)".into(),
                at: 1,
            });
        }
        assert_eq!(app.blocks.len(), before, "a context row drew a box");
        app.on_event(Event::Notification {
            session,
            text: "<system_notification>Background task bg_1 finished.</system_notification>"
                .into(),
            at: 1,
        });
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notification { .. })
        ));
    }

    #[test]
    fn a_late_panel_answer_never_reopens_a_closed_overlay() {
        let mut app = App::for_test();
        for request in [
            Awaiting::Context,
            Awaiting::UndoFiles {
                side: undo::Side::Undo,
            },
            Awaiting::UndoFiles {
                side: undo::Side::Redo,
            },
            Awaiting::UndoDiff {
                file: "a.rs".into(),
                side: undo::Side::Redo,
            },
        ] {
            app.panel = Panel::None;
            app.awaiting.insert(999, request);
            app.on_ack(
                999,
                true,
                None,
                Some(serde_json::json!({"files":[],"diff":{"change":"unchanged","hunks":[]}})),
            );
            assert_eq!(app.panel, Panel::None);
            assert!(app.undo_diff.is_none());
        }
    }

    /// A warning logged while the panel is closed is counted in the footer
    /// until the panel is opened; the conversation itself never shows it.
    #[test]
    fn a_logged_warning_waits_in_the_footer_not_in_the_transcript() {
        use runeengine::proto::{LogEntry, LogLevel};
        let mut app = App::for_test();
        let session = app.session;
        let blocks = app.blocks.len();
        for level in [LogLevel::Info, LogLevel::Warn] {
            app.on_event(Event::Log {
                session,
                entry: LogEntry {
                    at: 0,
                    level,
                    source: "title".into(),
                    code: "title_failed".into(),
                    message: format!("{level:?} line"),
                    turn: None,
                    detail: None,
                },
            });
        }
        assert_eq!(
            app.blocks.len(),
            blocks,
            "a log line is not a transcript block"
        );
        assert_eq!(app.logs.len(), 2);
        assert_eq!(
            app.status.as_deref(),
            Some("Warning (title): Warn line · view in /logs")
        );
        let mut d = test_driver("logsopen");
        app.open_logs(&mut d, LogLevel::Info);
        assert_eq!(app.panel, Panel::Logs);
    }

    /// A turn the engine opens on Rune's own notification is drawn as one, by
    /// the flag the engine sets — and a compaction is a thin line, not a box.
    #[test]
    fn a_notification_turn_and_a_compaction_are_drawn_as_what_they_are() {
        let mut app = App::for_test();
        let session = app.session.unwrap();
        app.on_event(Event::TurnStart {
            session,
            turn: 3,
            text: Some("<system_notification>Background task bg_1 finished: Completed.</system_notification>".into()),
            notification: true, media: Vec::new(), at: 0,
});
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Notification { .. })
        ));
        app.on_event(Event::Compacted {
            session,
            through_turn: 2,
            kept_turns: 1,
            summary: "s".into(),
        });
        assert!(matches!(
            app.blocks.last(),
            Some(Block::Divider { text }) if text.contains("turn 2")
        ));
    }

    /// A new session's first message is stored by the window that wrote it and
    /// announced by its host afterwards. The sender draws it from that
    /// announcement; a window whose view was taken between the two already
    /// shows it and does not draw it twice. Either way the turn runs.
    #[test]
    fn a_new_sessions_first_message_is_drawn_once_wherever_the_view_was_taken() {
        let start = |app: &mut App| {
            let session = app.session.unwrap();
            app.on_event(Event::TurnStart {
                session,
                turn: 1,
                text: Some("first words".into()),
                notification: false,
                media: Vec::new(),
                at: 1_791_000_000_000,
            });
        };
        let users = |app: &App| {
            app.blocks
                .iter()
                .filter(|b| matches!(b, Block::User { .. }))
                .count()
        };
        let mut sender = App::for_test();
        start(&mut sender);
        assert_eq!(users(&sender), 1, "the sender sees what it sent");
        assert!(sender.busy);

        let mut joiner = App::for_test();
        let session = joiner.session.unwrap();
        joiner.apply_view(&serde_json::json!({
            "session": session,
            "history": [{"id": 1, "turn": 1, "role": "user", "content": "first words",
                         "media": [], "created_at": 1_791_000_000_000i64}],
            "toolcalls": [],
            "live": {"first_turn": null, "events": []},
        }));
        assert_eq!(users(&joiner), 1);
        start(&mut joiner);
        assert_eq!(users(&joiner), 1, "drawn once");
        assert!(joiner.busy, "and the turn runs");
    }

    /// Audit R14: the footer showed Ask while the engine still enforced Full
    /// (or the reverse) after a refused switch. A refusal puts the old mode back.
    #[test]
    fn a_refused_mode_switch_restores_the_mode_the_engine_enforces() {
        let mut app = App::for_test();
        app.mode = "full".into();
        app.awaiting.insert(
            1,
            Awaiting::Mode {
                previous: "ask".into(),
                next: "full".into(),
            },
        );
        app.on_ack(1, false, Some("refused".into()), None);
        assert_eq!(app.mode, "ask");
    }

    /// Audit U3: an optimistic knob the engine refused is put back.
    #[test]
    fn a_refused_setting_puts_the_knob_back() {
        let mut app = App::for_test();
        app.settings["settings"] = serde_json::json!({ "thinking": false });
        let before = app.settings["settings"].clone();
        app.preferences.serial = 4;
        app.settings["settings"]["thinking"] = serde_json::json!(true);
        let session = app.session;
        settings::ack(
            &mut app,
            settings::Request {
                token: 4,
                session,
                origin: settings::Page::Overview,
                editor: None,
                operation: settings::Operation::Setting(Some("thinking".into())),
                before: Some(before),
            },
            false,
            Some("not accepted".into()),
            None,
        );
        assert_eq!(
            app.settings["settings"]["thinking"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn model_selection_waits_for_successful_configuration_ack() {
        let mut app = App::for_test();
        app.awaiting.insert(
            1,
            Awaiting::ModelConfig("p".into(), "m".into(), app.session),
        );
        assert!(app.pending_model_selection.is_none());
        app.on_ack(1, false, Some("rejected".into()), None);
        assert!(app.pending_model_selection.is_none());
        app.awaiting.insert(
            2,
            Awaiting::ModelConfig("p".into(), "m".into(), app.session),
        );
        app.on_ack(2,true,None,Some(serde_json::json!({"configured":{"id":"m","context":1000,"price":{"input":0,"cached":0,"output":0}}})));
        assert_eq!(
            app.pending_model_selection,
            Some((app.session, "p".into(), "m".into()))
        );
        app.providers = serde_json::json!({"providers":[{"name":"p","models":[
            {"id":"m","context":1000}, {"id":"other","context":2000}
        ]}]});
        app.take_settings(serde_json::json!({
            "catalogue_complete":false,"provider":"p",
            "settings":{"provider":"p","model":"m"},
            "models":[{"id":"m","context":3000}],"context_usable":2500
        }));
        assert_eq!(app.settings["models"].as_array().unwrap().len(), 2);
        assert_eq!(
            app.settings["models"][0]["context"],
            serde_json::json!(3000)
        );
        assert_eq!(app.settings["models"][1]["id"], serde_json::json!("other"));
        assert_eq!(app.context, 2500);
    }

    #[test]
    fn counter_clock_matches_browser_cadence_without_idle_repaints() {
        assert!(TICK <= Duration::from_millis(17));
        let mut app = App::for_test();
        assert!(!app.tick(), "an idle frame should not repaint");
        app.busy = true;
        for _ in 0..4 {
            assert!(
                !app.tick(),
                "the slow activity pulse repainted at counter cadence"
            );
        }
        assert!(
            app.tick(),
            "the activity pulse never reached its own slower cadence"
        );
    }

    /// What a window hands over is what the next window shows: the input and
    /// its cursor (on a character boundary), the panel with its page and row,
    /// the scroll, and the chain of handovers so far.
    #[test]
    fn a_window_hands_over_what_it_shows_and_the_next_one_shows_it() {
        let mut before = App::for_test();
        before.input = "draft ✓ prompt".into();
        before.cursor = 9;
        before.scroll = 4;
        before.panel = Panel::Settings;
        before.preferences.location.page = settings::Page::Connections;
        before.preferences.location.row = 3;
        let chain = before.next_handover(1_000).unwrap();
        let window = before.handover_window(77, chain);
        assert_eq!(window.request_next, 77);

        let mut after = App::for_test();
        after.restore_handover(window.clone());
        assert_eq!(after.input, "draft ✓ prompt");
        assert_eq!(after.cursor, 9);
        assert_eq!(after.scroll, 4);
        assert_eq!(after.panel, Panel::Settings);
        assert_eq!(after.preferences.location.page, settings::Page::Connections);
        assert_eq!(after.preferences.location.row, 3);
        assert_eq!(after.handover_chain, chain);

        // A nested page arrives with its ancestry: Esc goes back through it.
        let mut nested = App::for_test();
        nested.panel = Panel::Settings;
        nested.preferences.navigate(settings::Page::WebProviders {
            service: runeengine::settings::services::Service::Search,
        });
        let handed = nested.handover_window(78, nested.next_handover(1_000).unwrap());
        let mut resumed = App::for_test();
        resumed.restore_handover(handed);
        assert!(resumed.preferences.back());
        assert_eq!(resumed.preferences.location.page, settings::Page::Tools);
        assert!(resumed.preferences.back());
        assert_eq!(resumed.preferences.location.page, settings::Page::Overview);

        // A cursor inside a character lands on the boundary before it.
        let mut inside = window;
        inside.cursor = 7;
        after.restore_handover(inside);
        assert_eq!(after.cursor, 6);
    }

    #[test]
    fn a_previous_releases_service_page_hands_over_to_tools_with_its_new_ancestry() {
        let before = App::for_test();
        let mut handed = before.handover_window(77, before.next_handover(1_000).unwrap());
        handed.panel = Some(crate::handover::Panel::Settings {
            page: serde_json::json!({"Service":{"name":"9router","service":"search"}}),
            row: 2,
        });
        let mut resumed = App::for_test();
        resumed.restore_handover(handed);
        assert_eq!(
            resumed.preferences.location.page,
            settings::Page::WebProviders {
                service: runeengine::settings::services::Service::Search
            }
        );
        assert!(resumed.preferences.back());
        assert_eq!(resumed.preferences.location.page, settings::Page::Tools);
    }

    /// The loop guard holds across handovers: three in ten seconds stop the
    /// fourth, and a pause resets it. (0.1.5 cleared the count on arrival, so
    /// the guard never fired.)
    #[test]
    fn a_window_handing_over_in_a_loop_is_stopped() {
        let mut app = App::for_test();
        let mut now = 5_000;
        for _ in 0..3 {
            let chain = app.next_handover(now).expect("allowed");
            let window = app.handover_window(1, chain);
            app = App::for_test();
            app.restore_handover(window);
            now += 2;
        }
        assert_eq!(app.next_handover(now), None, "a fourth within ten seconds");
        assert!(app.next_handover(now + 11).is_some(), "a pause resets it");

        let exe = std::env::current_exe().unwrap();
        assert!(!is_cli_outdated(&exe, runeengine::VERSION));
        assert!(is_cli_outdated(&exe, "99.0.0"), "a host on a later release");
        assert!(
            !is_cli_outdated(&exe, "0.0.1"),
            "a host on an earlier release moves itself; the window waits"
        );
    }

    #[test]
    fn retry_backoff_owns_footer_but_resume_uses_transient_lifetime() {
        let mut app = App::for_test();
        for attempt in 1..=5 {
            app.on_event(Event::Retrying {
                session: app.session.unwrap(),
                attempt,
                of: 5,
                wait_ms: attempt as u64 * 3000,
                rotated: false,
                reason: "Rate limited".into(),
            });
            app.status_at = Some(Instant::now() - Duration::from_secs(30));
            app.tick();
            assert!(app
                .retry_status
                .as_ref()
                .unwrap()
                .contains(&format!("{attempt} of 5")));
            app.on_event(Event::Retrying {
                session: app.session.unwrap(),
                attempt,
                of: 5,
                wait_ms: 0,
                rotated: false,
                reason: "Rate limited".into(),
            });
            assert!(app.retry_status.is_none());
            assert!(app.status.as_ref().unwrap().contains("resuming"));
            app.status_at = Some(Instant::now() - Duration::from_secs(30));
            app.tick();
            assert!(
                app.status.is_none(),
                "API calling must not pin retry indefinitely"
            );
        }
        app.on_event(Event::Retrying {
            session: app.session.unwrap(),
            attempt: 1,
            of: 5,
            wait_ms: 9000,
            rotated: false,
            reason: "Rate limited".into(),
        });
        app.on_event(Event::Done {
            session: app.session.unwrap(),
            turn: 1,
            stop: runeengine::proto::StopReason::Cancelled,
        });
        assert!(app.retry_status.is_none());
    }

    #[test]
    fn status_expires_after_ttl_and_reverts_to_idle_state() {
        let mut app = App::for_test();
        app.note("temporary notification");
        assert!(app.status.is_some());
        assert!(app.status_at.is_some());

        // Fast forward status_at to 9 seconds ago
        app.status_at = Some(Instant::now() - Duration::from_secs(9));
        let repainted = app.tick();
        assert!(repainted, "expiring status must trigger repaint");
        assert!(app.status.is_none(), "status must be cleared after TTL");
        assert!(app.status_at.is_none());
    }

    #[test]
    fn quit_armed_expires_after_ttl() {
        let mut app = App::for_test();
        app.quit_armed = true;
        app.quit_armed_at = Some(Instant::now() - Duration::from_secs(5));
        let _ = app.tick();
        assert!(!app.quit_armed, "quit_armed must disarm after TTL");
        assert!(app.quit_armed_at.is_none());
    }

    #[test]
    fn questions_esc_armed_expires_after_ttl_and_clears_status() {
        let mut app = App::for_test();
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
        let mut state = crate::ui::questions::State::new("r1".into(), questions);
        state.esc_armed = true;
        state.esc_armed_at = Some(Instant::now() - Duration::from_secs(4));
        app.questions = Some(state);
        app.note("press esc again to close");

        let repainted = app.tick();
        assert!(repainted, "expiring esc_armed must trigger repaint");
        assert!(
            !app.questions.as_ref().unwrap().esc_armed,
            "esc_armed must be false after 3s"
        );
        assert!(app.questions.as_ref().unwrap().esc_armed_at.is_none());
        assert_eq!(app.status, None, "status must be cleared after 3s");
    }
}

#[cfg(test)]
mod spend_tests {
    use super::*;

    /// The guardrail warns and never blocks, and it says so once.
    #[test]
    fn spend_is_flagged_once_and_nothing_is_stopped() {
        let mut app = App::for_test();
        app.warn_spend = 1.00;

        app.cost = 0.40;
        app.check_spend();
        assert!(app.blocks.is_empty(), "under the mark, nothing is said");

        app.cost = 1.20;
        app.check_spend();
        let said = app.blocks.len();
        assert_eq!(said, 1, "the mark must be reported");
        match app.blocks.last() {
            Some(Block::Notice { text, bad }) => {
                assert!(!bad, "passing a spending mark is not an error");
                assert!(text.contains("1.20") && text.contains("1.00"), "{text}");
                assert!(text.contains("Nothing has been stopped"), "{text}");
            }
            _ => panic!("the mark must be reported as a notice, not as anything else"),
        }

        app.cost = 9.00;
        app.check_spend();
        assert_eq!(
            app.blocks.len(),
            said,
            "a guardrail that repeats is one nobody reads"
        );
    }

    /// Zero is off, and off must stay silent however much is spent.
    #[test]
    fn a_session_with_no_mark_set_is_never_interrupted_about_money() {
        let mut app = App::for_test();
        app.warn_spend = 0.0;
        app.cost = 500.0;
        app.check_spend();
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn replayed_session_with_track_tasks_restores_tasks_for_split_pane() {
        let mut app = App::for_test();
        let mut tool = Tool::new("t1".into(), "track_tasks".into(), "set plan".into());
        tool.settle(&serde_json::json!({
            "tasks": [
                { "text": "Task A", "status": "done" },
                { "text": "Task B", "status": "doing" }
            ]
        }));
        app.blocks.push(Block::of(tool));
        assert!(app.tasks.is_empty());
        for b in app.blocks.iter().rev() {
            if let Block::Tool(t) = b {
                if t.name == "track_tasks" {
                    let parsed = steps::tasks(t);
                    if !parsed.is_empty() {
                        app.tasks = parsed;
                        break;
                    }
                }
            }
        }
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.tasks[0]["text"], "Task A");
    }

    #[test]
    fn replayed_session_with_error_role_restores_notice_block() {
        let mut app = App::for_test();
        let history = serde_json::json!({
            "messages": [
                { "role": "user", "content": "tes", "created_at": 1000 },
                { "role": "error", "content": "provider: HTTP 400: invalid model ID" }
            ]
        });
        for m in history["messages"].as_array().unwrap() {
            let text = m["content"].as_str().unwrap().to_string();
            match m["role"].as_str().unwrap() {
                "user" => app.blocks.push(Block::user_timed(text, None, Vec::new())),
                "error" => app.blocks.push(Block::Notice { text, bad: true }),
                _ => {}
            }
        }
        assert_eq!(app.blocks.len(), 2);
        match &app.blocks[1] {
            Block::Notice { text, bad } => {
                assert!(bad);
                assert!(text.contains("HTTP 400"));
            }
            _ => panic!("expected notice block"),
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    /// `/new` starts a fresh session, and the composer's history walk belongs
    /// to a session exactly as the transcript does: pgup after it must offer
    /// nothing from the session that was just left.
    #[test]
    fn a_new_session_empties_the_prompt_history_with_the_transcript() {
        let mut app = App::for_test();
        app.prompt_history.push("pesan 1".into());
        app.prompt_history.push("pesan 2".into());
        app.history_index = Some(0);
        app.history_draft = "draft yang belum terkirim".into();
        app.awaiting.insert(7, Awaiting::NewSession);
        app.on_event(Event::Ack {
            id: 7,
            ok: true,
            error: None,
            data: Some(serde_json::json!({ "session": 42 })),
        });
        assert_eq!(app.session, Some(42), "the ack names the new session");
        assert!(
            app.prompt_history.is_empty(),
            "the old session's prompts must not be recallable in the new one"
        );
        assert!(
            app.history_index.is_none(),
            "the walk position belongs to the old session"
        );
        assert!(
            app.history_draft.is_empty(),
            "and so does the draft saved for that walk"
        );
    }

    /// The engine hands back the session the workspace already has when nobody
    /// has said anything in it — and that answer must not wipe a history when
    /// there was nothing to replace it with.
    #[test]
    fn an_already_empty_session_keeps_its_history() {
        let mut app = App::for_test();
        app.session = Some(1);
        app.prompt_history.push("pesan 1".into());
        app.awaiting.insert(7, Awaiting::NewSession);
        app.on_event(Event::Ack {
            id: 7,
            ok: true,
            error: None,
            data: Some(serde_json::json!({ "session": 1 })),
        });
        assert_eq!(
            app.prompt_history.len(),
            1,
            "the same session was handed back"
        );
    }

    #[test]
    fn usage_accumulates_tok_in_and_tok_out_across_turns() {
        let mut app = App::for_test();
        // Turn 1: 4,000 in, 500 out, 100 cached
        app.on_event(Event::Usage {
            cost: None,
            details: Default::default(),
            session: 1,
            input: 4000,
            output: 500,
            cached: 100,
        });
        assert_eq!(app.context_used, 4000);
        assert_eq!(app.tok_in, 4000);
        assert_eq!(app.tok_out, 500);
        assert_eq!(app.cache_in, 100);

        // Turn 2: 4,500 in, 300 out, 200 cached
        app.on_event(Event::Usage {
            cost: None,
            details: Default::default(),
            session: 1,
            input: 4500,
            output: 300,
            cached: 200,
        });
        assert_eq!(
            app.context_used, 4500,
            "context_used reflects latest turn input"
        );
        assert_eq!(
            app.tok_in, 8500,
            "tok_in accumulates all inputs across turns"
        );
        assert_eq!(
            app.tok_out, 800,
            "tok_out accumulates all outputs across turns"
        );
        assert_eq!(
            app.cache_in, 300,
            "cache_in accumulates all cached tokens across turns"
        );
    }

    #[test]
    fn new_session_resets_split_screen_token_metrics_and_applies_settings() {
        let mut app = App::for_test();
        app.session = Some(1);
        app.tok_in = 50_000;
        app.tok_out = 5_000;
        app.cache_in = 2_000;
        app.context_used = 45_000;
        app.cost = 1.25;
        app.tasks
            .push(serde_json::json!({ "text": "task 1", "status": "done" }));
        app.file_changes
            .push(serde_json::json!({ "path": "a.rs", "added": 1, "removed": 0 }));
        app.prompt_history.push("prompt 1".into());

        app.awaiting.insert(8, Awaiting::NewSession);
        app.on_event(Event::Ack {
            id: 8,
            ok: true,
            error: None,
            data: Some(serde_json::json!({
                "session": 2,
                "settings": {
                    "settings": { "provider": "test_prov", "model": "test_model" },
                    "context": 128_000,
                    "context_usable": 120_000
                }
            })),
        });

        assert_eq!(app.session, Some(2));
        assert_eq!(app.tok_in, 0, "tok_in must be cleared for new session");
        assert_eq!(app.tok_out, 0, "tok_out must be cleared for new session");
        assert_eq!(app.cache_in, 0, "cache_in must be cleared for new session");
        assert_eq!(
            app.context_used, 0,
            "context_used must be cleared for new session"
        );
        assert_eq!(app.cost, 0.0, "cost must be cleared for new session");
        assert!(
            app.tasks.is_empty(),
            "tasks must be cleared for new session"
        );
        assert!(
            app.file_changes.is_empty(),
            "the undo list must be cleared for new session"
        );
        assert!(
            app.prompt_history.is_empty(),
            "prompt history must be cleared for new session"
        );
        assert_eq!(
            app.model(),
            "test_model",
            "settings must be updated for new session"
        );
        assert_eq!(
            app.context, 120_000,
            "context usable must be updated for new session"
        );
    }

    #[test]
    fn thinking_label_behavior() {
        // 1. Thinking on without effort -> returns None (no bare "thinking" word)
        assert_eq!(thinking_label(true, None), None);

        // 2. Thinking on with explicit effort level -> shows effort level
        assert_eq!(thinking_label(true, Some("low")), Some("low"));
        assert_eq!(thinking_label(true, Some("high")), Some("high"));
        assert_eq!(thinking_label(true, Some("high")), Some("high"));

        // 3. Thinking off -> returns None
        assert_eq!(thinking_label(false, None), None);
        assert_eq!(thinking_label(false, Some("medium")), None);
        assert_eq!(thinking_label(false, None), None);

        // 4. thinking_required=true without effort -> None
        assert_eq!(thinking_label(true, None), None);
    }

    /// A delta is new text and is appended verbatim — even one that equals the
    /// tail, which an old heuristic silently dropped.
    #[test]
    fn thinking_deltas_are_never_dropped() {
        let mut app = App::for_test();
        let session = app.session.unwrap();
        app.blocks.push(Block::user_timed(
            "Plan something".to_string(),
            None::<String>,
            Vec::new(),
        ));
        for delta in ["Thinking part one", ".", ".", " and three"] {
            app.on_event(Event::Thinking {
                session,
                delta: delta.into(),
            });
        }
        let thinking: Vec<String> = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Thinking { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["Thinking part one.. and three"]);
    }
}

#[cfg(test)]
mod parity;

#[cfg(test)]
mod reload_tests {
    use super::*;
    use runeengine::settings::configuration::{Configuration, Shared};
    use serde_json::json;

    /// Where a newer build cannot be taken in place, the window says so
    /// once and not at every check after.
    #[test]
    fn a_newer_build_that_waits_for_the_next_start_is_told_once() {
        let mut app = App::for_test();
        app.tell_newer_build();
        assert_eq!(
            app.status.as_deref(),
            Some("A newer Rune is installed; it is used when Rune is opened again.")
        );
        app.clear_status();
        app.tell_newer_build();
        assert_eq!(app.status, None);
    }

    #[test]
    fn partial_settings_do_not_erase_the_engines_classifier_state() {
        let mut app = App::for_test();
        app.mode = "full".into();
        app.settings["classifier_status"] = json!({"enabled":true,"policy":true,"active":true});
        let mut partial = app.settings.clone();
        partial.as_object_mut().unwrap().remove("classifier_status");
        app.take_settings(partial);
        assert!(app.classifier_active());
        let mut next = app.settings.clone();
        next["classifier_status"] =
            json!({"enabled":true,"policy":true,"active":false,"reason":"invalid policy"});
        app.take_settings(next);
        assert!(
            !app.classifier_active(),
            "active is an engine fact, not inferred from policy existence"
        );
    }

    /// The crash that ended every window on a Ctrl+O: the toggle writes
    /// `ui.sidebar`, every window sees `rune.toml` change at once, and the one
    /// reading the file asked for settings by names that do not exist. A window
    /// has to follow the file, not die of it.
    #[test]
    fn a_window_follows_a_changed_rune_toml_instead_of_dying() {
        let home = std::env::temp_dir().join(format!("rune-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut app = App::for_test();
        app.sidebar = true;
        std::fs::write(
            home.join("rune.toml"),
            format!("[ui]\nsidebar = true\ntheme = \"{}\"\n", app.theme.name()),
        )
        .unwrap();
        let config = Shared::load(&home).unwrap();
        // This window follows the handle it is given, from where it stands.
        app.config_generation = config.generation();
        assert!(
            !app.reload_configuration(&config),
            "an unchanged file changes nothing"
        );

        // What Ctrl+O writes.
        Configuration::edit(&home, Some("ui.sidebar"), Some(json!(false)), false, None).unwrap();
        assert!(app.reload_configuration(&config));
        assert!(!app.sidebar, "the window follows the file");

        // And back: another window, or the owner's editor, switches it on.
        Configuration::edit(&home, Some("ui.sidebar"), Some(json!(true)), false, None).unwrap();
        assert!(app.reload_configuration(&config));
        assert!(app.sidebar);

        // Mouse reporting saved elsewhere reaches this terminal at the next
        // frame — `-config explain` calls it live (owner, 2026-10-14).
        let mouse = app.mouse;
        Configuration::edit(&home, Some("ui.mouse"), Some(json!(!mouse)), false, None).unwrap();
        assert!(app.reload_configuration(&config));
        assert_eq!(app.mouse, !mouse);
        assert!(app.mouse_dirty, "the terminal is told");

        // A change another reader of the same handle took in first is still
        // this window's to follow: it goes by generation.
        Configuration::edit(&home, Some("ui.sidebar"), Some(json!(false)), false, None).unwrap();
        config.reload_if_changed().unwrap();
        assert!(app.reload_configuration(&config));
        assert!(!app.sidebar);
        let _ = std::fs::remove_dir_all(&home);
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    fn long_transcript() -> App {
        let mut app = App::for_test();
        app.drawn_rows = 400;
        app.drawn_viewport = 20;
        app
    }

    /// A window holding a draft's first message, stored but not yet handed to
    /// its host, does not hand itself over: the token would not travel.
    #[test]
    fn a_window_does_not_hand_over_with_a_first_message_not_yet_handed_on() {
        let mut app = App::for_test();
        assert!(!app.handover_waits(), "an idle window may hand over");
        app.composition.promotion = Some("token".into());
        assert!(app.handover_waits());
    }

    /// Typed text and attachments no longer hold a window on its build
    /// (owner, 2026-10-09): they travel with it. Only hands still typing hold
    /// it — until they rest for [`HANDOVER_IDLE`] — and not when its host no
    /// longer speaks its protocol.
    #[test]
    fn typed_text_waits_only_for_the_hands_to_rest() {
        let mut app = App::for_test();
        app.input = "a draft half written".into();
        app.attached.push(runeengine::media::Attachment {
            name: "notes.txt".into(),
            path: "/tmp/notes.txt".into(),
            ..Default::default()
        });
        assert!(!app.handover_waits(), "text and attachments travel with it");

        app.typed();
        assert!(app.handover_waits(), "a key just typed: the hands are busy");
        app.protocol_skew = true;
        assert!(
            !app.handover_waits(),
            "a host it cannot talk to does not wait"
        );
        app.protocol_skew = false;

        app.last_key = Some(std::time::Instant::now() - HANDOVER_IDLE);
        assert!(!app.handover_waits(), "rested for the idle gap");

        let handed = app.handover_window(7, crate::handover::Chain::default());
        assert_eq!(handed.input, "a draft half written");
        assert_eq!(handed.attached, vec!["/tmp/notes.txt".to_string()]);
    }

    /// A tap is `ui.scroll_lines` rows in all, but they are not jumped: the
    /// first share is paid at once and the ticks pay the rest, easing out.
    #[test]
    fn a_tap_glides_to_where_the_wheel_pointed() {
        let mut app = long_transcript();
        app.wheel_scroll(true);
        assert!(app.scroll > 0, "the answer to the hand is not a frame late");
        let base = runtime_configuration().number("ui.scroll_lines") as usize;
        assert_eq!(base, 2, "a notch is two rows (owner, 2026-10-09)");
        assert!(
            app.scroll < base,
            "the first share is not the whole distance"
        );
        let mut seen = vec![app.scroll];
        while app.glide_step() {
            seen.push(app.scroll);
        }
        assert_eq!(app.scroll, base, "one notch is two rows: {seen:?}");
        assert!(app.glide.is_none());
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
    }

    /// A spun wheel goes no further a notch than a tap (owner, 2026-10-09:
    /// no acceleration): twelve quick notches are twelve notches.
    #[test]
    fn a_spun_wheel_goes_as_far_a_notch_as_a_tap() {
        let base = runtime_configuration().number("ui.scroll_lines") as usize;
        let mut app = long_transcript();
        app.terminal_keeps_up = false;
        for _ in 0..12 {
            app.wheel_scroll(true);
        }
        assert_eq!(app.scroll, 12 * base);
        let mut fast = long_transcript();
        fast.terminal_keeps_up = false;
        for _ in 0..12 {
            fast.wheel_scroll_at(true, true);
        }
        assert_eq!(fast.scroll, 12 * base * FAST_WHEEL);
    }

    /// Ctrl or Alt held makes a notch go three times as far (six rows) —
    /// jumped on a terminal that cannot show a glide, glided on one that can,
    /// and stopped at the top like any other notch.
    #[test]
    fn a_notch_with_ctrl_or_alt_goes_three_times_as_far() {
        let base = runtime_configuration().number("ui.scroll_lines") as usize;
        let fast = base * FAST_WHEEL;
        assert_eq!(fast, 6);
        let mut jumped = long_transcript();
        jumped.terminal_keeps_up = false;
        jumped.wheel_scroll_at(true, true);
        assert_eq!(
            jumped.scroll, fast,
            "a slow terminal jumps the whole fast notch"
        );
        let mut glided = long_transcript();
        glided.terminal_keeps_up = true;
        glided.wheel_scroll_at(true, true);
        assert!(glided.glide.is_some(), "a fast notch still glides");
        while glided.glide_step() {}
        assert_eq!(glided.scroll, fast);
        let mut plain = long_transcript();
        plain.terminal_keeps_up = false;
        plain.wheel_scroll_at(true, false);
        assert_eq!(plain.scroll, base, "without the modifier nothing changes");
        let mut top = long_transcript();
        top.terminal_keeps_up = true;
        top.scroll = 378;
        top.wheel_scroll_at(true, true);
        while top.glide_step() {}
        assert_eq!(top.scroll, 380, "never past the top");
    }

    /// The glide never owes a place beyond either end of the transcript.
    #[test]
    fn a_glide_stops_at_the_ends() {
        let mut app = long_transcript();
        app.scroll = 378; // the top is 400 - 20 = 380
        for _ in 0..5 {
            app.wheel_scroll(true);
        }
        while app.glide_step() {}
        assert_eq!(app.scroll, 380);
        let mut app = long_transcript();
        app.scroll = 2;
        for _ in 0..5 {
            app.wheel_scroll(false);
        }
        while app.glide_step() {}
        assert_eq!(app.scroll, 0, "the live end");
    }

    /// Any other move of the page — a key, a jump to the end — cancels what
    /// the wheel still owed; the page must not drift on after the hand left.
    #[test]
    fn another_move_cancels_the_glide() {
        let mut app = long_transcript();
        app.wheel_scroll(true);
        assert!(app.glide.is_some());
        app.scroll_by(true, 10);
        assert!(app.glide.is_none());
        let at = app.scroll;
        assert!(!app.glide_step());
        assert_eq!(app.scroll, at);
    }

    /// A terminal that shows a page frame slower than a glide needs (Konsole,
    /// about 80 ms a frame beside the panel) gets the notch's rows at once: the
    /// glide it could only show at a dozen frames a second was the lag felt
    /// after the hand stopped. One that keeps up still glides.
    #[test]
    fn a_slow_terminal_jumps_where_a_fast_one_glides() {
        let mut slow = long_transcript();
        slow.terminal_keeps_up = false;
        let before = slow.scroll;
        slow.wheel_scroll(true);
        assert!(slow.glide.is_none(), "no glide owed on a slow terminal");
        assert_eq!(
            slow.scroll - before,
            runtime_configuration().number("ui.scroll_lines") as usize,
            "the whole notch, at once"
        );

        let mut fast = long_transcript();
        fast.wheel_scroll(true);
        assert!(fast.glide.is_some(), "a terminal that keeps up glides");
    }
}
