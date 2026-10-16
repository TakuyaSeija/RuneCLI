//! Interactive terminal session picker.
//!
//! Provides an open, full-width terminal interface matching Foto 1 layout:
//! live search filter, 2-line session cards with rich metadata, Ctrl+R in-place
//! rename, and streamlined shortcut keys.

use std::io::Write;
use std::time::Duration;

use runeengine::Command;

use crate::driver::Driver;
use crate::ui::ansi::{
    at, pad, truncate, ACCENT, BOLD, CLEAR_LINE, CLEAR_SCREEN, FAINT, MUTED, RESET, RULE, TEXT,
};
use crate::ui::draw::format_duration;
#[cfg(test)]
use crate::ui::input::Mouse;
use crate::ui::input::{Click, Key};
use crate::ui::term::{self, epoch_ymd_hm};

/// Representation of a stored session row in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionItem {
    pub id: i64,
    pub title: String,
    pub project: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub active_seconds: i64,
    pub model: Option<String>,
    pub size_bytes: u64,
}

pub fn format_session_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

impl SessionItem {
    pub fn from_json(v: &serde_json::Value) -> Self {
        Self {
            id: v["id"].as_i64().unwrap_or(0),
            title: v["title"].as_str().unwrap_or("(untitled)").to_string(),
            project: v["project"].as_str().unwrap_or("").to_string(),
            created_at: v["created_at"].as_i64().unwrap_or(0),
            updated_at: v["updated_at"].as_i64().unwrap_or(0),
            active_seconds: v["active_seconds"].as_i64().unwrap_or(0),
            model: v["model"].as_str().map(String::from),
            size_bytes: v["size_bytes"].as_u64().unwrap_or(0),
        }
    }
}

/// Interactive picker state.
pub struct SessionPicker {
    pub sessions: Vec<SessionItem>,
    pub selected: usize,
    pub scroll_top: usize,
    pub project: Option<String>,
    pub done: bool,
    pub selected_id: Option<i64>,
    pub query: String,
    pub renaming: Option<String>,
    query_cursor: usize,
    query_editor: super::editing::State,
    rename_cursor: usize,
    rename_editor: super::editing::State,
    text_row: Option<(u16, super::editor::Row)>,
    pub(super) caret: Option<(u16, u16)>,
    pub rename_session_id: Option<i64>,
    pub pending_rename: Option<(i64, String)>,
    prev_frame: Vec<String>,
    /// Last drawn row → filtered item; never guess layout in the input path.
    hit_rows: Vec<Option<usize>>,
    mouse_anchor: Option<(u16, u16)>,
    mouse_dragged: bool,
}

impl SessionPicker {
    /// Margin from visible top/bottom boundary where auto-scrolling begins.
    #[cfg(test)]
    pub const SCROLL_MARGIN: usize = 3;

    pub fn new(sessions: Vec<SessionItem>, project: Option<String>) -> Self {
        Self {
            sessions,
            selected: 0,
            scroll_top: 0,
            project,
            done: false,
            selected_id: None,
            query: String::new(),
            renaming: None,
            query_cursor: 0,
            query_editor: super::editing::State::default(),
            rename_cursor: 0,
            rename_editor: super::editing::State::default(),
            text_row: None,
            caret: None,
            rename_session_id: None,
            pending_rename: None,
            prev_frame: Vec::new(),
            hit_rows: Vec::new(),
            mouse_anchor: None,
            mouse_dragged: false,
        }
    }

    /// Returns session indices that match the active search query.
    pub fn filtered_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.sessions.len()).collect();
        }
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, s)| {
                let title_matches = s.title.to_lowercase().contains(&q);
                let id_matches = s.id.to_string().contains(&q) || format!("#{}", s.id).contains(&q);
                let model_matches = s
                    .model
                    .as_deref()
                    .map(|m| m.to_lowercase().contains(&q))
                    .unwrap_or(false);
                let date_matches = epoch_ymd_hm(s.updated_at).contains(&q);
                if title_matches || id_matches || model_matches || date_matches {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Calculates how many sessions fit in the available terminal height.
    pub fn visible_capacity(rows: u16) -> usize {
        let overhead = 10;
        let avail = rows.saturating_sub(overhead) as usize;
        let spacing = if rows >= 22 { 3 } else { 2 };
        (avail / spacing).max(1)
    }

    /// Moves cursor down by one item, auto-scrolling before reaching the bottom edge.
    pub fn move_down(&mut self, visible_capacity: usize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        if self.selected + 1 < filtered.len() {
            self.selected += 1;
            self.adjust_scroll(visible_capacity, filtered.len());
        }
    }

    /// Moves cursor up by one item, auto-scrolling before reaching the top edge.
    pub fn move_up(&mut self, visible_capacity: usize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
            self.adjust_scroll(visible_capacity, filtered.len());
        }
    }

    /// Advances by a page down.
    pub fn page_down(&mut self, visible_capacity: usize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let step = visible_capacity.saturating_sub(2).max(1);
        self.selected = (self.selected + step).min(filtered.len().saturating_sub(1));
        self.adjust_scroll(visible_capacity, filtered.len());
    }

    /// Advances by a page up.
    pub fn page_up(&mut self, visible_capacity: usize) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let step = visible_capacity.saturating_sub(2).max(1);
        self.selected = self.selected.saturating_sub(step);
        self.adjust_scroll(visible_capacity, filtered.len());
    }

    /// Tests the shared scroll clamp independently of input editing.
    #[cfg(test)]
    pub fn move_home(&mut self, visible_capacity: usize) {
        self.selected = 0;
        let total = self.filtered_indices().len();
        self.adjust_scroll(visible_capacity, total);
    }

    /// Tests the shared scroll clamp independently of input editing.
    #[cfg(test)]
    pub fn move_end(&mut self, visible_capacity: usize) {
        let filtered = self.filtered_indices();
        if !filtered.is_empty() {
            self.selected = filtered.len() - 1;
            self.adjust_scroll(visible_capacity, filtered.len());
        }
    }

    /// Adjusts `scroll_top` maintaining `SCROLL_MARGIN` distance from edges.
    pub fn adjust_scroll(&mut self, visible_capacity: usize, total: usize) {
        if visible_capacity == 0 || total == 0 {
            self.scroll_top = 0;
            return;
        }
        if total <= visible_capacity {
            self.scroll_top = 0;
            return;
        }
        let max_scroll = total.saturating_sub(visible_capacity);
        let m = (super::runtime_configuration().number("ui.scroll_margin") as usize)
            .min(visible_capacity.saturating_sub(1) / 2);

        // Scrolling down: trigger when selection reaches visible_capacity - m
        if self.selected >= self.scroll_top + visible_capacity.saturating_sub(m) {
            let target = (self.selected + m + 1).saturating_sub(visible_capacity);
            self.scroll_top = target.min(max_scroll);
        }
        // Scrolling up: trigger when selection reaches scroll_top + m
        if self.selected < self.scroll_top + m {
            self.scroll_top = self.selected.saturating_sub(m);
        }
        self.scroll_top = self.scroll_top.min(max_scroll);
    }

    /// Handles a key event in the picker.
    pub fn on_key(&mut self, key: Key, rows: u16, _cols: u16) {
        let cap = Self::visible_capacity(rows);

        if let Key::Mouse(mouse) = &key {
            match mouse.kind {
                Click::Press => {
                    self.mouse_anchor = Some((mouse.row, mouse.col));
                    self.mouse_dragged = false;
                }
                Click::Drag => {
                    self.mouse_dragged |= self
                        .mouse_anchor
                        .is_some_and(|(row, col)| row != mouse.row || col.abs_diff(mouse.col) > 1);
                }
                Click::Release if self.mouse_dragged => {
                    let anchor = self.mouse_anchor.take();
                    self.mouse_dragged = false;
                    if let Some((row, col)) = anchor {
                        self.text_select(row, col, mouse.row, mouse.col);
                    }
                    return;
                }
                _ => {}
            }
        }
        // Rename mode handling
        if self.renaming.is_some() {
            match key {
                Key::Enter => {
                    if let Some(new_title) = self.renaming.take() {
                        let trimmed = new_title.trim().to_string();
                        if !trimmed.is_empty() {
                            if let Some(sid) = self.rename_session_id.take() {
                                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                                    s.title = trimmed.clone();
                                }
                                self.pending_rename = Some((sid, trimmed));
                            }
                        }
                    }
                    self.renaming = None;
                    self.rename_session_id = None;
                }
                Key::Esc => {
                    self.renaming = None;
                    self.rename_session_id = None;
                }
                Key::Mouse(m) if m.kind == Click::Release => {
                    self.text_click(m.row, m.col);
                }
                _ => {
                    if let Some(text) = self.renaming.as_mut() {
                        self.rename_editor.key(
                            text,
                            &mut self.rename_cursor,
                            &key,
                            super::editing::Options::single(200, 4096),
                        );
                    }
                }
            }
            return;
        }

        // Normal browsing mode handling
        match key {
            Key::Ctrl('r') => {
                let filtered = self.filtered_indices();
                if let Some(&idx) = filtered.get(self.selected) {
                    let s = &self.sessions[idx];
                    self.rename_session_id = Some(s.id);
                    self.rename_cursor = s.title.len();
                    self.rename_editor.reset();
                    self.renaming = Some(s.title.clone());
                }
            }
            Key::Up => self.move_up(cap),
            Key::Down => self.move_down(cap),
            Key::PageUp => self.page_up(cap),
            Key::PageDown => self.page_down(cap),
            Key::Mouse(m) if matches!(m.kind, Click::WheelUp | Click::WheelDown) => {
                for _ in 0..m.modifiers.wheel_multiplier() {
                    if m.kind == Click::WheelUp {
                        self.move_up(cap);
                    } else {
                        self.move_down(cap);
                    }
                }
            }
            Key::Mouse(m) => match m.kind {
                Click::Press => {
                    self.mouse_anchor = Some((m.row, m.col));
                    self.mouse_dragged = false;
                }
                Click::Drag => {
                    self.mouse_dragged |= self
                        .mouse_anchor
                        .is_some_and(|(row, col)| row != m.row || col.abs_diff(m.col) > 1);
                }
                Click::Release => {
                    let anchor = self.mouse_anchor.take().unwrap_or((m.row, m.col));
                    if !self.mouse_dragged && anchor.0 == m.row && anchor.1.abs_diff(m.col) <= 1 {
                        if self.text_click(m.row, m.col) {
                            return;
                        }
                        if let Some(Some(item)) =
                            self.hit_rows.get(m.row.saturating_sub(1) as usize)
                        {
                            let item = *item;
                            if item == self.selected {
                                self.selected_id = self
                                    .filtered_indices()
                                    .get(item)
                                    .map(|&idx| self.sessions[idx].id);
                                self.done = self.selected_id.is_some();
                            } else {
                                self.selected = item;
                            }
                        }
                    }
                    self.mouse_dragged = false;
                }
                _ => {}
            },
            Key::Enter => {
                let filtered = self.filtered_indices();
                if let Some(&idx) = filtered.get(self.selected) {
                    self.selected_id = Some(self.sessions[idx].id);
                }
                self.done = true;
            }
            Key::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.query_cursor = 0;
                    self.query_editor.reset();
                    self.selected = 0;
                    self.scroll_top = 0;
                } else {
                    self.selected_id = None;
                    self.done = true;
                }
            }
            Key::Ctrl('c') => {
                self.selected_id = None;
                self.done = true;
            }
            _ => {
                let before = self.query.clone();
                self.query_editor.key(
                    &mut self.query,
                    &mut self.query_cursor,
                    &key,
                    super::editing::Options::single(4096, 16_384),
                );
                if before != self.query {
                    self.selected = 0;
                    self.scroll_top = 0;
                }
            }
        }
    }

    fn text_select(&mut self, first_row: u16, first: u16, last_row: u16, last: u16) -> bool {
        let Some((row, geometry)) = self.text_row.as_ref() else {
            return false;
        };
        if first_row != *row || last_row != *row {
            return false;
        }
        let (start, end) = (geometry.caret_at(first), geometry.caret_at(last));
        if let Some(text) = self.renaming.as_ref() {
            self.rename_editor
                .select(text, &mut self.rename_cursor, start, end);
        } else {
            self.query_editor
                .select(&self.query, &mut self.query_cursor, start, end);
        }
        true
    }

    fn text_click(&mut self, row: u16, col: u16) -> bool {
        let Some((line, geometry)) = self.text_row.as_ref() else {
            return false;
        };
        if row != *line {
            return false;
        }
        let at = geometry.caret_at(col);
        if let Some(text) = self.renaming.as_ref() {
            self.rename_editor.place(text, &mut self.rename_cursor, at);
        } else {
            self.query_editor
                .place(&self.query, &mut self.query_cursor, at);
        }
        true
    }

    /// Composes the terminal output lines matching Foto 1 aesthetic.
    pub fn compose(&mut self, rows: u16, cols: u16) -> Vec<String> {
        let cols = cols.max(20) as usize;
        let filtered = self.filtered_indices();
        let total_filtered = filtered.len();
        if self.selected >= total_filtered && total_filtered > 0 {
            self.selected = total_filtered - 1;
        }
        let current_num = if total_filtered == 0 {
            0
        } else {
            self.selected + 1
        };

        let cap = Self::visible_capacity(rows);
        let spacing = if rows >= 22 { 3 } else { 2 };
        self.adjust_scroll(cap, total_filtered);

        let mut lines = Vec::new();
        self.hit_rows.clear();
        self.text_row = None;
        self.caret = None;
        let indent = "  ";

        // 1. Top header line: Resume session (1 of 50) ───────────────────────────
        let title_prefix = format!("{ACCENT}{BOLD}Resume session{RESET}");
        let count_str = format!("{MUTED}({} of {}){RESET}", current_num, total_filtered);
        let visible_prefix_w =
            14 + 1 + format!("({} of {})", current_num, total_filtered).len() + 1;
        let rule_len = cols.saturating_sub(visible_prefix_w + 1);
        lines.push(format!(
            "{title_prefix} {count_str} {RULE}{}{RESET}",
            "─".repeat(rule_len)
        ));

        // 2. Blank line
        lines.push(String::new());

        // 3. Search / Rename box container
        let box_w = cols.saturating_sub(4).max(8);
        let inner_w = box_w.saturating_sub(4);

        if self.renaming.is_some() {
            let tag = if cols < 40 {
                " Rename ".to_owned()
            } else {
                format!(" Rename session #{} ", self.rename_session_id.unwrap_or(0))
            };
            let top_rule_len = box_w.saturating_sub(2 + tag.len() + 1);
            lines.push(format!(
                "{indent}{RULE}╭─{RESET}{ACCENT}{BOLD}{tag}{RESET}{RULE}{}╮{RESET}",
                "─".repeat(top_rule_len)
            ));
            let cur_val = self.renaming.as_deref().unwrap_or("");
            let projection = super::editor::Projection::single_line(cur_val);
            let label = if cols < 40 { "Title: " } else { "New title: " };
            let label_width = super::ansi::width(label);
            let (geometry, col) = projection.window(
                self.rename_cursor,
                inner_w.saturating_sub(label_width),
                5 + label_width,
            );
            let value = geometry.styled(self.rename_editor.selected(cur_val, self.rename_cursor));
            let disp = format!("{label}{value}");
            self.caret = Some((lines.len() as u16, col.saturating_sub(1) as u16));
            self.text_row = Some((lines.len() as u16 + 1, geometry));
            let padded = pad(&format!("{TEXT}{BOLD}{disp}{RESET}"), inner_w);
            lines.push(format!("{indent}{RULE}│ {RESET}{padded}{RULE} │{RESET}"));
            lines.push(format!(
                "{indent}{RULE}╰{}╯{RESET}",
                "─".repeat(box_w.saturating_sub(2))
            ));
        } else {
            lines.push(format!(
                "{indent}{RULE}╭{}╮{RESET}",
                "─".repeat(box_w.saturating_sub(2))
            ));
            let projection = super::editor::Projection::single_line(&self.query);
            let (geometry, col) =
                projection.window(self.query_cursor, inner_w.saturating_sub(3), 7);
            let search_content = if self.query.is_empty() {
                format!("{FAINT}{}{RESET}", truncate("⌕ Search...", inner_w))
            } else {
                format!(
                    "{TEXT}⌕ {}{RESET}",
                    geometry.styled(self.query_editor.selected(&self.query, self.query_cursor))
                )
            };
            self.caret = Some((lines.len() as u16, col.saturating_sub(1) as u16));
            self.text_row = Some((lines.len() as u16 + 1, geometry));
            let padded = pad(&search_content, inner_w);
            lines.push(format!("{indent}{RULE}│ {RESET}{padded}{RULE} │{RESET}"));
            lines.push(format!(
                "{indent}{RULE}╰{}╯{RESET}",
                "─".repeat(box_w.saturating_sub(2))
            ));
        }

        // 4. Blank line
        lines.push(String::new());

        // 5. Workspace label
        let ws_label = match self.project.as_deref() {
            Some(p) => std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p),
            None => "Clode",
        };
        lines.push(format!("{indent}{MUTED}{ws_label}{RESET}"));
        lines.push(String::new());

        // 6. Session cards list
        if filtered.is_empty() {
            lines.push(format!("{indent}{MUTED}No matching sessions found.{RESET}"));
            if !self.query.is_empty() {
                lines.push(format!(
                    "{indent}{FAINT}Press Esc to clear search query.{RESET}"
                ));
            } else {
                lines.push(format!(
                    "{indent}{FAINT}Start a new conversation with: rune{RESET}"
                ));
            }
        } else {
            let end_idx = (self.scroll_top + cap).min(total_filtered);
            for (idx, &item_idx) in filtered
                .iter()
                .enumerate()
                .take(end_idx)
                .skip(self.scroll_top)
            {
                let s = &self.sessions[item_idx];
                let is_selected = idx == self.selected;

                // Line 1: Title Row
                let arrow_prefix = if idx == self.scroll_top && self.scroll_top > 0 {
                    format!("{FAINT}↑ {RESET}")
                } else if idx == end_idx - 1 && end_idx < total_filtered {
                    format!("{FAINT}↓ {RESET}")
                } else if is_selected {
                    format!("{ACCENT}{BOLD}❯ {RESET}")
                } else {
                    "  ".to_string()
                };

                let max_title_w = cols.saturating_sub(8);
                let title_text = truncate(&s.title, max_title_w);
                let title_fmt = if is_selected {
                    format!("{TEXT}{BOLD}{title_text}{RESET}")
                } else {
                    format!("{TEXT}{title_text}{RESET}")
                };
                self.hit_rows.resize(lines.len(), None);
                self.hit_rows.extend([Some(idx), Some(idx)]);
                lines.push(format!("{indent}{arrow_prefix}{title_fmt}"));

                // Line 2: Metadata Row (Indented 2 spaces, separated by ' · ')
                // Attributes: ID · Date-time (YYYY-MM-DD HH:MM) · Model · Active
                let dt = if s.updated_at > 0 {
                    epoch_ymd_hm(s.updated_at)
                } else {
                    "-".to_string()
                };
                let model_str = s.model.as_deref().unwrap_or("-");
                let act_str = if s.active_seconds > 0 {
                    format!("active {}", format_duration(s.active_seconds as u64))
                } else {
                    "active -".to_string()
                };
                let size_str = format_session_size(s.size_bytes);

                let meta_content = format!(
                    "#{id} · {size} · {dt} · {model} · {act}",
                    id = s.id,
                    size = size_str,
                    dt = dt,
                    model = model_str,
                    act = act_str
                );
                let meta_line = if is_selected {
                    format!("{indent}  {MUTED}{meta_content}{RESET}")
                } else {
                    format!("{indent}  {FAINT}{meta_content}{RESET}")
                };
                lines.push(meta_line);

                // Breathing room between cards if space allows
                if spacing == 3 && idx + 1 < end_idx {
                    lines.push(String::new());
                }
            }
        }

        // 7. Footer bar anchored towards bottom
        while lines.len() < rows.saturating_sub(2) as usize {
            lines.push(String::new());
        }

        if self.renaming.is_some() {
            lines.push(format!(
                "{indent}{MUTED}[Enter]{RESET} {FAINT}Save title{RESET}   {MUTED}[Esc]{RESET} {FAINT}Cancel rename{RESET}"
            ));
        } else {
            lines.push(format!(
                "{indent}{MUTED}↑/↓{RESET} {FAINT}to navigate{RESET} · {MUTED}Enter{RESET} {FAINT}to resume{RESET} · {MUTED}Ctrl+R{RESET} {FAINT}to rename{RESET} · {MUTED}Type{RESET} {FAINT}to search{RESET} · {MUTED}Esc{RESET} {FAINT}to cancel{RESET}"
            ));
        }

        lines
    }

    /// Paints the current picker state to the terminal using differential row updates.
    pub fn paint(&mut self, rows: u16, cols: u16, force: bool) {
        let frame = self.compose(rows, cols);
        let mut out = String::new();
        if force || self.prev_frame.len() != frame.len() {
            out.push_str(CLEAR_SCREEN);
            self.prev_frame = vec![String::new(); frame.len()];
        }
        for (i, line) in frame.iter().enumerate() {
            if self.prev_frame.get(i).map(|p| p != line).unwrap_or(true) {
                out.push_str(&at(i as u16 + 1, 1));
                out.push_str(CLEAR_LINE);
                out.push_str(line);
            }
        }
        out.push_str(RESET);
        if let Some((row, col)) = self.caret {
            out.push_str(&at(row + 1, col + 1));
            out.push_str("\x1b[6 q\x1b[?25h");
        }
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
        self.prev_frame = frame;
    }
}

/// Runs the interactive terminal picker. Returns `Some(id)` if a session was chosen.
pub fn pick(d: &mut Driver, project: Option<&str>) -> Result<Option<i64>, String> {
    let out = d.call(|id| Command::SessionList {
        id,
        project: project.map(String::from),
    })?;
    let raw_list = out["sessions"].as_array().cloned().unwrap_or_default();
    let sessions: Vec<SessionItem> = raw_list.iter().map(SessionItem::from_json).collect();

    if sessions.is_empty() {
        println!("No sessions yet in this workspace.");
        return Ok(None);
    }

    super::theme::init();
    term::guard_panics();
    let mut term = term::enter_or_report(true)?;
    let (tx, rx) = std::sync::mpsc::channel();
    crate::ui::input::spawn(tx);

    let mut picker = SessionPicker::new(sessions, project.map(String::from));
    let (mut rows, mut cols) = term.size();
    picker.paint(rows, cols, true);

    let mut ticks: usize = 0;
    let mut activation_failed = false;
    // Keys taken from the reader, counted as it counts them (`input::sent`).
    let mut applied: u64 = 0;
    while !picker.done {
        ticks = ticks.wrapping_add(1);
        let key_opt = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(k) => Some(k),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        let (r, c) = term.size();
        let resized = r != rows || c != cols;
        if resized {
            rows = r;
            cols = c;
        }

        if let Some(key) = key_opt {
            if crate::ui::input::is_typed(&key) {
                applied += 1;
            }
            picker.on_key(key, rows, cols);
            if let Some((sid, new_title)) = picker.pending_rename.take() {
                let _ = d.call(|id| Command::SessionRename {
                    id,
                    session: sid,
                    title: new_title,
                });
            }
            picker.paint(rows, cols, false);
        } else {
            // A release came while the list was up: the picker activates it as
            // a window does, when nothing typed here would be lost — and comes
            // back as the same list on the new build.
            if !activation_failed
                && ticks.is_multiple_of(20)
                && picker.renaming.is_none()
                && picker.query.is_empty()
            {
                // The reader stopped, and every key it read applied here: a
                // key typed from now on waits in the terminal for the new
                // build.
                let parked = || {
                    crate::ui::input::park(Duration::from_millis(250))
                        .filter(|_| crate::ui::input::sent() == applied)
                };
                if let Some((exe, parked)) =
                    release_to_activate(d).and_then(|exe| Some((exe, parked()?)))
                {
                    // Back here only when the `exec` failed: not tried again
                    // every second, this build goes on serving the list.
                    term = activate(term, &exe)?;
                    drop(parked);
                    activation_failed = true;
                    picker.paint(rows, cols, true);
                    continue;
                }
            }
            if ticks.is_multiple_of(20) && picker.renaming.is_none() && picker.query.is_empty() {
                if let Ok(out) = d.call(|id| Command::SessionList {
                    id,
                    project: project.map(String::from),
                }) {
                    let raw_list = out["sessions"].as_array().cloned().unwrap_or_default();
                    let fresh: Vec<SessionItem> =
                        raw_list.iter().map(SessionItem::from_json).collect();
                    if !fresh.is_empty() && fresh != picker.sessions {
                        picker.sessions = fresh;
                        picker.selected =
                            picker.selected.min(picker.sessions.len().saturating_sub(1));
                        picker.paint(rows, cols, false);
                    }
                }
            }
            if resized {
                picker.paint(rows, cols, true);
            }
        }
    }

    // What was typed after the choice belongs to the window it opens, which
    // takes the reader only once its session is reached: kept for it.
    crate::ui::input::release(&rx);
    drop(term);
    Ok(picker.selected_id)
}

/// The build to activate, when a release installed another one where this one
/// runs from and it says it could take this home over (`-upgrade-check`).
///
/// The picker used to have no such check (found 2026-10-09: a `rune -session`
/// list stayed on 0.3.0 after 0.3.1 was installed, reading a home the new
/// build had upgraded), so a release reached it only once a session was
/// picked and the window that opened handed itself over.
#[cfg(not(test))]
fn release_to_activate(d: &Driver) -> Option<std::path::PathBuf> {
    // Where a process cannot become another program (Windows), the list is
    // the next start's to show in the newer build.
    if !runeengine::platform::REPLACES_IN_PLACE {
        return None;
    }
    let exe = crate::build::installed().ok()?;
    (crate::build::replaced(&exe) && crate::build::ready(&exe, Some(&d.db_path))).then_some(exe)
}

/// A unit test runs from a test binary, never an installed build.
#[cfg(test)]
fn release_to_activate(_: &Driver) -> Option<std::path::PathBuf> {
    None
}

/// `exec` the build at `exe` with this process's own arguments, the terminal
/// handed over with the alternate screen kept up, so the list is simply drawn
/// again by the new build. Only an `exec` that failed comes back — a file that
/// had just answered `-upgrade-check` and then would not start — and the list
/// has no line to say so on: the terminal is taken again and this build goes
/// on serving it.
fn activate(term: term::Term, exe: &std::path::Path) -> Result<term::Term, String> {
    #[cfg(not(test))]
    {
        term.hand_over();
        let mut command = std::process::Command::new(exe);
        command.args(std::env::args_os().skip(1));
        let _ = crate::handover::exec(crate::handover::Handover::default(), &mut command);
        term::enter_or_report(true)
    }
    #[cfg(test)]
    {
        let _ = exe;
        Ok(term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_sessions(count: usize) -> Vec<SessionItem> {
        (0..count)
            .map(|i| SessionItem {
                id: (i + 1) as i64,
                title: format!("Session {}", i + 1),
                project: "/workspace".to_string(),
                created_at: 1000,
                updated_at: 1756994400000, // known epoch ms
                active_seconds: (i * 60) as i64,
                model: Some("glm-5.3".to_string()),
                size_bytes: (i * 1024) as u64,
            })
            .collect()
    }

    #[test]
    fn canonical_editing_matrix_covers_picker_search_and_rename_without_page_aliases() {
        for rename in [false, true] {
            for case in super::super::editing::cases() {
                let mut picker = SessionPicker::new(dummy_sessions(1), None);
                if rename {
                    picker.renaming = Some(case.text.into());
                    picker.rename_cursor = case.at;
                    picker.rename_session_id = Some(1);
                } else {
                    picker.query = case.text.into();
                    picker.query_cursor = case.at;
                }
                for key in case.keys {
                    picker.on_key(key, 34, 120);
                }
                let got = if rename {
                    picker.renaming.as_deref().unwrap()
                } else {
                    &picker.query
                };
                assert_eq!(got, case.expected, "{} rename={rename}", case.name);
                picker.compose(34, 120);
                assert!(picker.caret.is_some());
                assert!(!picker.done);
                assert!(picker.pending_rename.is_none());
            }
        }
        let mut picker = SessionPicker::new(dummy_sessions(20), None);
        picker.query = "Session".into();
        picker.query_cursor = 7;
        picker.on_key(Key::Ctrl('u'), 34, 120);
        assert_eq!(picker.query, "");
        assert_eq!(picker.selected, 0);
        picker.on_key(Key::PageDown, 34, 120);
        assert!(picker.selected > 0);
    }

    #[test]
    fn picker_text_geometry_and_drag_selection_survive_narrow_and_wide_fields() {
        use super::super::input::Modifiers;
        for cols in [20, 32, 60, 120] {
            for rename in [false, true] {
                let mut picker = SessionPicker::new(dummy_sessions(1), None);
                if rename {
                    picker.renaming = Some("abcdef".into());
                    picker.rename_cursor = 6;
                } else {
                    picker.query = "abcdef".into();
                    picker.query_cursor = 6;
                }
                let frame = picker.compose(24, cols);
                let (row, geometry) = picker.text_row.as_ref().unwrap();
                assert!(super::super::ansi::width(&frame[*row as usize - 1]) <= cols as usize);
                let (caret_row, caret_col) = picker.caret.unwrap();
                assert!(caret_row < 24 && caret_col < cols);
                if cols >= 32 {
                    let line = *row;
                    let first = geometry.first_col as u16;
                    for (kind, col) in [
                        (Click::Press, first + 1),
                        (Click::Drag, first + 3),
                        (Click::Release, first + 3),
                    ] {
                        picker.on_key(
                            Key::Mouse(Mouse {
                                kind,
                                row: line,
                                col,
                                modifiers: Modifiers::NONE,
                            }),
                            24,
                            cols,
                        );
                    }
                    picker.on_key(Key::Char('X'), 24, cols);
                    let value = picker.renaming.as_deref().unwrap_or(&picker.query);
                    assert_eq!(value, "aXdef", "cols={cols} rename={rename}");
                    assert!(!picker.done);
                    assert!(picker.pending_rename.is_none());
                }
            }
        }
    }

    #[test]
    fn picker_clicks_follow_drawn_filtered_cards_and_never_resume_a_drag() {
        use crate::ui::input::Modifiers;
        let mut picker = SessionPicker::new(dummy_sessions(30), None);
        picker.query = "Session 2".into();
        let _ = picker.compose(34, 120);
        let row = picker
            .hit_rows
            .iter()
            .position(|hit| *hit == Some(1))
            .unwrap() as u16
            + 1;
        let send = |picker: &mut SessionPicker, kind, row, col| {
            picker.on_key(
                Key::Mouse(Mouse {
                    kind,
                    row,
                    col,
                    modifiers: Modifiers::NONE,
                }),
                34,
                120,
            )
        };
        send(&mut picker, Click::Press, row, 8);
        send(&mut picker, Click::Release, row, 8);
        assert_eq!(picker.selected, 1);
        assert!(!picker.done);
        let _ = picker.compose(34, 120);
        send(&mut picker, Click::Press, row, 8);
        send(&mut picker, Click::Drag, row, 30);
        send(&mut picker, Click::Release, row, 30);
        assert!(!picker.done);
        // Clicking the metadata of the already-selected card resumes it.
        send(&mut picker, Click::Press, row + 1, 8);
        send(&mut picker, Click::Release, row + 1, 8);
        assert!(picker.done);
        assert_eq!(picker.selected_id, Some(20));
    }

    #[test]
    fn picker_navigation_and_selection() {
        let sessions = dummy_sessions(5);
        let mut picker = SessionPicker::new(sessions, Some("/workspace".into()));

        assert_eq!(picker.selected, 0);
        picker.move_down(10);
        assert_eq!(picker.selected, 1);
        picker.move_up(10);
        assert_eq!(picker.selected, 0);
        picker.move_up(10);
        assert_eq!(picker.selected, 0); // clamp at 0

        picker.move_end(10);
        assert_eq!(picker.selected, 4);
        picker.move_home(10);
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn auto_scroll_triggers_at_margin_distance() {
        let sessions = dummy_sessions(20);
        let mut picker = SessionPicker::new(sessions, None);
        let cap = 10;
        let m = SessionPicker::SCROLL_MARGIN; // 3

        assert_eq!(picker.scroll_top, 0);

        // Move cursor down to index 6 (distance from bottom 9 is 3 rows)
        for _ in 0..6 {
            picker.move_down(cap);
        }
        assert_eq!(picker.selected, 6);
        assert_eq!(picker.scroll_top, 0);

        // Reaching index 7 triggers scroll margin (cap - m = 10 - 3 = 7)
        picker.move_down(cap);
        assert_eq!(picker.selected, cap - m);
        assert_eq!(picker.scroll_top, 1); // Auto-scrolled ahead!

        picker.move_down(cap);
        assert_eq!(picker.selected, 8);
        assert_eq!(picker.scroll_top, 2);

        // Now move up: moving up triggers auto-scroll when reaching scroll_top + m = 2 + 3 = 5
        picker.move_up(cap);
        assert_eq!(picker.selected, 7);
        assert_eq!(picker.scroll_top, 2);

        picker.move_up(cap);
        assert_eq!(picker.selected, 6);
        assert_eq!(picker.scroll_top, 2);

        picker.move_up(cap);
        assert_eq!(picker.selected, 5);
        assert_eq!(picker.scroll_top, 2);

        // At index 4 (< scroll_top + m = 5), auto-scrolling upward triggers!
        picker.move_up(cap);
        assert_eq!(picker.selected, 4);
        assert_eq!(picker.scroll_top, 1);

        picker.move_up(cap);
        assert_eq!(picker.selected, 3);
        assert_eq!(picker.scroll_top, 0);
    }

    #[test]
    fn page_navigation_and_clamping() {
        let sessions = dummy_sessions(30);
        let mut picker = SessionPicker::new(sessions, None);
        let cap = 10;

        picker.page_down(cap);
        assert_eq!(picker.selected, 8);
        assert!(picker.scroll_top > 0);

        picker.page_up(cap);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.scroll_top, 0);
    }

    #[test]
    fn search_filtering_and_clamping() {
        let mut sessions = dummy_sessions(5);
        sessions[2].title = "Fix premature compaction".to_string();
        sessions[2].model = Some("claude-3-7-sonnet".to_string());

        let mut picker = SessionPicker::new(sessions, None);
        assert_eq!(picker.filtered_indices().len(), 5);

        // Search by title keyword
        picker.on_key(Key::Char('p'), 24, 80);
        picker.on_key(Key::Char('r'), 24, 80);
        picker.on_key(Key::Char('e'), 24, 80);
        assert_eq!(picker.query, "pre");
        let filtered = picker.filtered_indices();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], 2);

        // Clear query with Backspace
        picker.on_key(Key::Backspace, 24, 80);
        picker.on_key(Key::Backspace, 24, 80);
        picker.on_key(Key::Backspace, 24, 80);
        assert_eq!(picker.filtered_indices().len(), 5);

        // Search by model name
        picker.on_key(Key::Char('s'), 24, 80);
        picker.on_key(Key::Char('o'), 24, 80);
        picker.on_key(Key::Char('n'), 24, 80);
        let filtered = picker.filtered_indices();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], 2);

        // Esc clears search query
        picker.on_key(Key::Esc, 24, 80);
        assert!(picker.query.is_empty());
        assert_eq!(picker.filtered_indices().len(), 5);
        assert!(!picker.done);
    }

    #[test]
    fn rename_mode_lifecycle() {
        let sessions = dummy_sessions(3);
        let mut picker = SessionPicker::new(sessions, None);

        // Select item 1
        picker.move_down(10);
        assert_eq!(picker.selected, 1);

        // Press Ctrl+R to start renaming
        picker.on_key(Key::Ctrl('r'), 24, 80);
        assert_eq!(picker.rename_session_id, Some(2));
        assert_eq!(picker.renaming.as_deref(), Some("Session 2"));

        // Type characters
        picker.on_key(Key::Backspace, 24, 80); // removes '2'
        picker.on_key(Key::Char('X'), 24, 80); // adds 'X'
        assert_eq!(picker.renaming.as_deref(), Some("Session X"));

        // Commit with Enter
        picker.on_key(Key::Enter, 24, 80);
        assert!(picker.renaming.is_none());
        assert_eq!(picker.pending_rename, Some((2, "Session X".to_string())));
        assert_eq!(picker.sessions[1].title, "Session X");

        // Esc cancels without committing
        picker.on_key(Key::Ctrl('r'), 24, 80);
        picker.on_key(Key::Char('!'), 24, 80);
        picker.on_key(Key::Esc, 24, 80);
        assert!(picker.renaming.is_none());
        assert_eq!(picker.sessions[1].title, "Session X");
    }

    #[test]
    fn compose_renders_foto1_layout() {
        let sessions = dummy_sessions(3);
        let mut picker = SessionPicker::new(sessions, Some("/home/user/Clode".into()));
        let lines = picker.compose(24, 80);

        assert!(!lines.is_empty());
        let full = lines.join("\n");

        // Header line matching Foto 1
        assert!(full.contains("Resume session"));
        assert!(full.contains("(1 of 3)"));

        // Search container
        assert!(full.contains("⌕ Search..."));

        // Workspace label
        assert!(full.contains("Clode"));

        // Session list content
        assert!(full.contains("Session 1"));
        assert!(full.contains("#1"));
        assert!(full.contains("glm-5.3"));
        assert!(full.contains("active"));

        // Footer shortcuts matching Foto 1 requested subset
        assert!(full.contains("to navigate"));
        assert!(full.contains("to resume"));
        assert!(full.contains("to rename"));
        assert!(full.contains("to search"));
        assert!(full.contains("to cancel"));

        // Verify mode is NOT displayed
        assert!(!full.contains("[ask]"));
        assert!(!full.contains("[auto]"));
        assert!(!full.contains("[full]"));
    }

    #[test]
    fn compose_reflects_active_theme() {
        use crate::ui::ansi::{set_depth, Depth, ACCENT};
        use crate::ui::theme::{self, ThemeMode};

        set_depth(Depth::True);
        let sessions = dummy_sessions(1);
        let mut picker = SessionPicker::new(sessions, Some("/workspace".into()));

        // Amber theme
        theme::set_theme(ThemeMode::Amber);
        let lines_amber = picker.compose(24, 80).join("\n");
        let amber_code = format!("{ACCENT}");
        assert!(lines_amber.contains(&amber_code));

        // Cyan theme
        theme::set_theme(ThemeMode::Cyan);
        let lines_cyan = picker.compose(24, 80).join("\n");
        let cyan_code = format!("{ACCENT}");
        assert!(lines_cyan.contains(&cyan_code));
        assert_ne!(amber_code, cyan_code);

        // Reset to Violet
        theme::set_theme(ThemeMode::Violet);
    }
}
