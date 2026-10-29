//! A changing retry countdown is one live source, not a stream of new notices.
//! Keep a notice readable before yielding, and pause its visible time beneath
//! a confirmation. Coalesce waiting notices so obsolete updates cannot grow an
//! unbounded UI backlog. The App's existing status fields remain authoritative.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct State {
    pending: Option<String>,
    retry_at: Option<Instant>,
    paused_at: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Timing {
    lifetime: Duration,
    contested: Duration,
}

impl Timing {
    fn current() -> Self {
        let config = super::runtime_configuration();
        Self {
            lifetime: config.duration("ui.status_lifetime"),
            contested: config.duration("ui.status_replace_lifetime"),
        }
    }
}

impl super::App {
    pub(super) fn footer_text(&self) -> Option<&str> {
        self.status.as_deref().or(self.retry_status.as_deref())
    }

    pub(super) fn confirmation_status(&self, now: Instant) -> Option<&'static str> {
        let config = super::runtime_configuration();
        if self.stop_armed
            && self.working()
            && self.stop_armed_at.is_some_and(|at| {
                now.saturating_duration_since(at) < config.duration("ui.stop_lifetime")
            })
        {
            return Some("press esc again to stop");
        }
        if self.questions.as_ref().is_some_and(|state| {
            state.esc_armed
                && state.esc_armed_at.is_some_and(|at| {
                    now.saturating_duration_since(at) < config.duration("ui.stop_lifetime")
                })
        }) {
            return Some("press esc again to cancel");
        }
        if self.quit_armed
            && self.quit_armed_at.is_some_and(|at| {
                now.saturating_duration_since(at) < config.duration("ui.quit_lifetime")
            })
        {
            return Some(if self.working() {
                "press ctrl+c again to leave Rune · esc twice to stop AI"
            } else {
                "press ctrl+c again to leave"
            });
        }
        None
    }

    fn footer_covered(&self, now: Instant) -> bool {
        self.confirmation_status(now).is_some() || super::undo::countdown(self).is_some()
    }

    pub fn note(&mut self, text: impl Into<String>) {
        let now = Instant::now();
        self.note_at(
            text.into(),
            now,
            Timing::current(),
            self.footer_covered(now),
        );
    }

    pub(super) fn note_control(&mut self, text: impl Into<String>) {
        self.clear_status();
        self.footer.paused_at = None;
        self.show_status(text.into(), Instant::now());
    }

    pub fn clear_status(&mut self) {
        self.status = None;
        self.status_at = None;
        self.footer.pending = None;
    }

    fn show_status(&mut self, text: String, now: Instant) {
        self.status = Some(text);
        self.status_at = Some(now);
        self.footer.retry_at = None;
    }

    fn note_at(&mut self, text: String, now: Instant, timing: Timing, covered: bool) {
        self.footer_tick_at(now, timing, covered);
        if self.status.as_ref() == Some(&text) {
            self.footer.pending = None;
            return;
        }
        if self.footer.pending.as_ref() == Some(&text) {
            return;
        }
        self.footer.pending = Some(text);
        self.footer_tick_at(now, timing, covered);
    }

    #[cfg(test)]
    pub(super) fn show_waiting_status_for_test(&mut self) {
        if self.footer.pending.is_some() {
            let now = Instant::now();
            let timing = Timing::current();
            self.status_at = Some(now - timing.lifetime);
            self.footer.retry_at = Some(now - timing.contested);
            self.footer_tick_at(now, timing, false);
        }
    }

    pub(super) fn set_retry_status(&mut self, text: Option<String>) {
        let now = Instant::now();
        self.retry_at(text, now);
        self.tick_footer(now);
    }

    fn retry_at(&mut self, text: Option<String>, now: Instant) {
        if self.retry_status.is_none() && text.is_some() && self.status.is_none() {
            self.footer.retry_at = Some(now);
        }
        self.retry_status = text;
        if self.retry_status.is_none() {
            self.footer.retry_at = None;
        }
    }

    pub(super) fn tick_footer(&mut self, now: Instant) -> bool {
        self.footer_tick_at(now, Timing::current(), self.footer_covered(now))
    }

    fn footer_tick_at(&mut self, now: Instant, timing: Timing, covered: bool) -> bool {
        if covered {
            self.footer.paused_at.get_or_insert(now);
            return false;
        }
        if let Some(paused) = self.footer.paused_at.take() {
            let visible_start = |at: Instant| now.checked_sub(paused.saturating_duration_since(at));
            self.status_at = self.status_at.and_then(visible_start);
            self.footer.retry_at = self.footer.retry_at.and_then(visible_start);
        }
        let before = self.footer_text().map(str::to_owned);
        if self.retry_status.is_none() {
            self.footer.retry_at = None;
        }
        if self.status.is_some() {
            let at = *self.status_at.get_or_insert(now);
            let lifetime = if self.footer.pending.is_some() || self.retry_status.is_some() {
                timing.lifetime.min(timing.contested)
            } else {
                timing.lifetime
            };
            if now.saturating_duration_since(at) >= lifetime {
                self.status = None;
                self.status_at = None;
            }
        }
        if self.status.is_none() {
            let retry_can_yield = self.retry_status.is_none()
                || self
                    .footer
                    .retry_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= timing.contested);
            if retry_can_yield {
                if let Some(text) = self.footer.pending.take() {
                    self.show_status(text, now);
                }
            }
            if self.status.is_none() && self.retry_status.is_some() {
                self.footer.retry_at.get_or_insert(now);
            }
        }
        before.as_deref() != self.footer_text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TIMING: Timing = Timing {
        lifetime: Duration::from_secs(8),
        contested: Duration::from_secs(3),
    };

    #[test]
    fn unopposed_notice_lasts_eight_seconds_but_a_contender_waits_three() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.note_at("first".into(), now, TIMING, false);
        app.footer_tick_at(now + Duration::from_secs(7), TIMING, false);
        assert_eq!(app.footer_text(), Some("first"));
        app.footer_tick_at(now + Duration::from_secs(8), TIMING, false);
        assert_eq!(app.footer_text(), None);
        app.note_at("second".into(), now, TIMING, false);
        app.note_at("third".into(), now + Duration::from_secs(1), TIMING, false);
        app.footer_tick_at(now + Duration::from_millis(2999), TIMING, false);
        assert_eq!(app.footer_text(), Some("second"));
        app.footer_tick_at(now + Duration::from_secs(3), TIMING, false);
        assert_eq!(app.footer_text(), Some("third"));
    }

    #[test]
    fn retry_updates_do_not_overwrite_a_notice_or_restart_its_visible_time() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.note_at("notice".into(), now, TIMING, false);
        for second in 1..=2 {
            app.retry_at(
                Some(format!("retry in {}s", 9 - second)),
                now + Duration::from_secs(second),
            );
            app.footer_tick_at(now + Duration::from_secs(second), TIMING, false);
            assert_eq!(app.footer_text(), Some("notice"));
        }
        app.footer_tick_at(now + Duration::from_secs(3), TIMING, false);
        assert_eq!(app.footer_text(), Some("retry in 7s"));
        app.note_at(
            "new notice".into(),
            now + Duration::from_secs(4),
            TIMING,
            false,
        );
        app.retry_at(Some("retry in 4s".into()), now + Duration::from_secs(6));
        app.footer_tick_at(now + Duration::from_secs(6), TIMING, false);
        assert_eq!(app.footer_text(), Some("new notice"));
        app.footer_tick_at(now + Duration::from_secs(9), TIMING, false);
        assert_eq!(app.footer_text(), Some("retry in 4s"));
    }

    #[test]
    fn confirmations_pause_notices_and_retry_keeps_its_latest_value() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.note_at("notice".into(), now, TIMING, false);
        app.footer_tick_at(now + Duration::from_secs(1), TIMING, true);
        app.retry_at(Some("retry in 2s".into()), now + Duration::from_secs(3));
        app.footer_tick_at(now + Duration::from_secs(4), TIMING, false);
        assert_eq!(app.footer_text(), Some("notice"));
        app.footer_tick_at(now + Duration::from_secs(6), TIMING, false);
        assert_eq!(app.footer_text(), Some("retry in 2s"));
    }

    #[test]
    fn retry_started_under_confirmation_begins_its_visible_interval_after_uncovering() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.footer_tick_at(now, TIMING, true);
        app.retry_at(Some("retry countdown".into()), now + Duration::from_secs(2));
        app.note_at(
            "waiting notice".into(),
            now + Duration::from_secs(2),
            TIMING,
            true,
        );
        app.footer_tick_at(now + Duration::from_secs(3), TIMING, false);
        app.footer_tick_at(now + Duration::from_millis(5999), TIMING, false);
        assert_eq!(app.footer_text(), Some("retry countdown"));
        app.footer_tick_at(now + Duration::from_secs(6), TIMING, false);
        assert_eq!(app.footer_text(), Some("waiting notice"));
    }

    #[test]
    fn streamed_chunks_do_not_clear_a_readable_notice_or_armed_confirmation() {
        let mut app = crate::ui::App::for_test();
        app.busy = true;
        app.note("read this notice");
        let at = app.status_at;
        app.stop_armed = true;
        app.stop_armed_at = Some(Instant::now());
        app.on_event(runeengine::proto::Event::Thinking {
            session: app.session.unwrap(),
            delta: "thinking".into(),
        });
        app.on_event(runeengine::proto::Event::Text {
            session: app.session.unwrap(),
            delta: "answer".into(),
        });
        assert_eq!(app.status.as_deref(), Some("read this notice"));
        assert_eq!(app.status_at, at);
        assert_eq!(
            app.confirmation_status(Instant::now()),
            Some("press esc again to stop")
        );
    }

    #[test]
    fn a_return_to_the_visible_value_discards_an_obsolete_waiting_notice() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.note_at("same".into(), now, TIMING, false);
        app.note_at(
            "obsolete".into(),
            now + Duration::from_secs(1),
            TIMING,
            false,
        );
        app.note_at("same".into(), now + Duration::from_secs(2), TIMING, false);
        app.footer_tick_at(now + Duration::from_secs(3), TIMING, false);
        assert_eq!(app.footer_text(), Some("same"));
        assert!(app.footer.pending.is_none());
        app.clear_status();
        app.note_at(
            "hidden update".into(),
            now + Duration::from_secs(4),
            TIMING,
            true,
        );
        app.footer_tick_at(now + Duration::from_secs(7), TIMING, false);
        assert_eq!(app.footer_text(), Some("hidden update"));
        app.footer_tick_at(now + Duration::from_secs(14), TIMING, false);
        assert_eq!(app.footer_text(), Some("hidden update"));
        app.footer_tick_at(now + Duration::from_secs(15), TIMING, false);
        assert!(app.footer_text().is_none());
    }

    #[test]
    fn latest_waiting_notice_coalesces_and_shorter_explicit_lifetime_is_respected() {
        let now = Instant::now();
        let mut app = crate::ui::App::for_test();
        app.note_at("shown".into(), now, TIMING, false);
        app.note_at("obsolete pending".into(), now, TIMING, false);
        app.note_at("latest pending".into(), now, TIMING, false);
        app.footer_tick_at(now + Duration::from_secs(3), TIMING, false);
        assert_eq!(app.footer_text(), Some("latest pending"));
        app.clear_status();
        let short = Timing {
            lifetime: Duration::from_secs(1),
            ..TIMING
        };
        app.note_at("short".into(), now, short, false);
        app.note_at("next".into(), now, short, false);
        app.footer_tick_at(now + Duration::from_secs(1), short, false);
        assert_eq!(app.footer_text(), Some("next"));
    }
}
