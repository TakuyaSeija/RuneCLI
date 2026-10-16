//! A screen replaces a screen, never a stack of responses that happen to arrive.
//! Pending forms retain their own field state while a foreground reader owns its keys.
use super::{App, Awaiting, Panel};

impl App {
    pub(super) fn show_panel(&mut self, panel: Panel) {
        self.awaiting.retain(|_, request| {
            !matches!(
                request,
                Awaiting::Context
                    | Awaiting::Logs
                    | Awaiting::Capabilities
                    | Awaiting::Sessions
                    | Awaiting::UndoFiles { .. }
                    | Awaiting::UndoDiff { .. }
            )
        });
        self.panel = panel;
        self.panel_row = 0;
        self.panel_view.reset();
        self.log_open = None;
        self.undo_diff = None;
        self.undo_armed = None;
        self.undo_refused = None;
        self.selection = None;
        self.transcript_dirty = true;
    }

    pub(super) fn composer_visible(&self) -> bool {
        self.panel != Panel::Settings
            && self.session_picker.is_none()
            && self.questions.is_none()
            && self.mcp_input.is_none()
            && self.pending.is_none()
    }
}
