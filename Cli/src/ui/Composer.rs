//! Frontend composition coordination. Draft UI is local; committed-session
//! edits are host-owned. Only keyboard/attachment actions create outgoing edits.

use super::{App, Awaiting};
use crate::driver::Driver;
use runeengine::{
    store::{
        composer::{Composer, Edit},
        drafts::Draft,
    },
    Command,
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct State {
    pub draft: Option<Draft>,
    pub authoritative: Composer,
    pub flight: Option<(u64, Composer, Composer)>,
    pub deferred: std::collections::VecDeque<Composer>,
    pub dirty_since: Option<Instant>,
    pub last_poll: Option<Instant>,
    pub history_saved: Option<Composer>,
    pub promotion: Option<String>,
    pub promote_pending: bool,
    pub promote_started: Option<Instant>,
    pub flight_started: Option<Instant>,
    pub submitted: Option<Composer>,
    pub ready: bool,
    /// Messages the person has sent (Enter) that wait for the host to confirm
    /// an edit still in flight, oldest first: each the composer as it stood at
    /// its Enter. A message leaves the composer at its Enter — what is typed
    /// next belongs to the next message, never to this one.
    pub submits: std::collections::VecDeque<Submit>,
    /// Send the next session edit without waiting out the debounce (a submit
    /// must find the host's composer equal to what it sends).
    pub urgent: bool,
    /// When the draft's generation was last checked against the registry.
    pub generation_checked: Option<Instant>,
}

/// One message as it was sent: its words, its attachments, and the composer
/// the host must hold when it is submitted (the host consumes the composer
/// that equals the prompt).
pub struct Submit {
    pub text: String,
    pub media: Vec<runeengine::media::Attachment>,
    pub composer: Composer,
}

/// How long a session edit may wait to be batched with the next keys. Every
/// edit is a host round trip with an fsync; one per keystroke was the cost,
/// not the typing (audit T1).
const SESSION_EDIT_DEBOUNCE: Duration = Duration::from_millis(150);

impl App {
    pub fn composer_snapshot(&self) -> Composer {
        Composer {
            text: self.input.clone(),
            cursor: self.cursor,
            attachments: self
                .attached
                .iter()
                .map(|attachment| attachment.path.clone())
                .collect(),
            settings: serde_json::from_value(self.settings["settings"].clone())
                .unwrap_or_else(|_| self.composition.authoritative.settings.clone()),
            mode: self.mode.clone(),
            preference_changed: self.composition.authoritative.preference_changed,
            revision: self.composition.authoritative.revision,
            operations: self.composition.authoritative.operations.clone(),
            pending: self.composition.authoritative.pending.clone(),
            cleared: self.composition.authoritative.cleared
                || self.composition.draft.is_some()
                    && self.input.is_empty()
                    && self.attached.is_empty()
                    && self.composition.authoritative.has_user_state(),
        }
    }

    pub fn restore_composer(&mut self, mut composer: Composer, cursor: bool) {
        composer.restore(std::path::Path::new(&self.project));
        self.input = composer.text.clone();
        // Reuse what is already attached: re-reading every file (canonicalize,
        // metadata, header) on each ack ran on the UI thread (audit T2).
        let previous = std::mem::take(&mut self.attached);
        self.attached = composer
            .attachments
            .iter()
            .filter_map(|path| {
                previous
                    .iter()
                    .find(|attached| &attached.path == path)
                    .cloned()
                    .or_else(|| runeengine::media::Attachment::at(std::path::Path::new(path)).ok())
            })
            .collect();
        // A mode or setting the user just changed is still on its way to the
        // engine; a composer snapshot written before it must not flip the pill
        // back for a frame. The engine's answer settles it either way.
        if !self
            .awaiting
            .values()
            .any(|pending| matches!(pending, Awaiting::Preferences(_)))
        {
            self.settings["settings"] =
                serde_json::to_value(&composer.settings).unwrap_or_default();
        }
        if !self
            .awaiting
            .values()
            .any(|pending| matches!(pending, Awaiting::Mode { .. }))
        {
            self.mode = composer.mode.clone();
        }
        self.cursor = if cursor {
            composer.cursor
        } else {
            self.cursor.min(self.input.len())
        };
        while !self.input.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
        self.input_editor.observe(&self.input, self.cursor);
        self.composition.authoritative = composer;
        self.composition.ready = true;
    }

    /// Called after a local key action. A maximum-age disk debounce is not
    /// extended by continuous typing; session edits dispatch without debounce.
    pub fn sync_composer(&mut self, d: &mut Driver, flush: bool) -> bool {
        if !self.composition.ready {
            return true;
        }
        if self.composition.promote_pending {
            return !flush;
        }
        if self
            .awaiting
            .values()
            .any(|pending| matches!(pending, Awaiting::Prompt { .. }))
        {
            return !flush;
        }
        let mut visible = self.composer_snapshot();
        // A message still waiting to be submitted is what the host must hold
        // first; what was typed after it follows once it has gone.
        if let Some(message) = self.composition.submits.front() {
            visible.text = message.composer.text.clone();
            visible.cursor = message.composer.cursor;
            visible.attachments = message.composer.attachments.clone();
        }
        if flush {
            if let Some(mut draft) = self.composition.draft.clone() {
                let opened = d
                    .local(Command::DraftOpen {
                        id: 0,
                        project: self.project.clone(),
                        token: Some(draft.token.clone()),
                    })
                    .and_then(|data| {
                        serde_json::from_value::<Draft>(data["draft"].clone())
                            .map_err(|error| error.to_string())
                    });
                match opened {
                    Ok(saved) => {
                        if saved.generation != draft.generation {
                            self.take_draft(saved, None);
                            return true;
                        }
                        draft.composer.revision = saved.composer.revision;
                    }
                    Err(error) => {
                        self.note(error);
                        return false;
                    }
                }
                match d.local(Command::DraftSave {
                    id: 0,
                    base: draft.composer.revision,
                    draft: Box::new(draft),
                    composer: Box::new(visible),
                }) {
                    Ok(data) => {
                        if let Ok(saved) = serde_json::from_value::<Draft>(data["draft"].clone()) {
                            self.composition.authoritative = saved.composer.clone();
                            self.composition.draft = Some(saved);
                            self.composition.flight = None;
                            self.composition.dirty_since = None;
                            return true;
                        }
                        self.note("Draft save did not return authoritative state; composition was retained.");
                    }
                    Err(error) => self.note(error),
                }
                return false;
            }
        }
        if let Some(draft) = self.composition.draft.as_ref() {
            if self.composition.flight.is_some() {
                return !flush;
            }
            let mut stored = self.composition.authoritative.clone();
            stored.cursor = visible.cursor;
            let changed = visible != stored || self.composition.dirty_since.is_some();
            if changed {
                let since = *self
                    .composition
                    .dirty_since
                    .get_or_insert_with(Instant::now);
                if flush || since.elapsed() >= Duration::from_millis(250) {
                    let draft = draft.clone();
                    let base = draft.composer.revision;
                    let id = d.ask(|id| Command::DraftSave {
                        id,
                        draft: Box::new(draft),
                        base,
                        composer: Box::new(visible.clone()),
                    });
                    self.awaiting.insert(id, Awaiting::DraftSaved);
                    self.composition.flight =
                        Some((id, self.composition.authoritative.clone(), visible));
                    self.composition.flight_started = Some(Instant::now());
                    self.composition.dirty_since = None;
                }
            } else if self.input.is_empty()
                && self.attached.is_empty()
                && self
                    .composition
                    .last_poll
                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(1000))
            {
                self.composition.last_poll = Some(Instant::now());
                let project = self.project.clone();
                let token = Some(draft.token.clone());
                self.request(d, Awaiting::DraftOpened, |id| Command::DraftOpen {
                    id,
                    project,
                    token,
                });
            }
            return true;
        }
        let Some(session) = self.session else {
            return !flush;
        };
        if flush && d.client.is_some() {
            let mut wanted = visible;
            let mut base = self.composition.authoritative.clone();
            for _ in 0..16 {
                let result = d
                    .call(|id| Command::ComposerGet { id, session })
                    .and_then(|data| {
                        serde_json::from_value::<Composer>(data["composer"].clone())
                            .map_err(|error| error.to_string())
                    });
                let latest = match result {
                    Ok(latest) => latest,
                    Err(error) => {
                        self.note(error);
                        return !flush;
                    }
                };
                if let Some((id, _, sent)) = &self.composition.flight {
                    if latest
                        .operations
                        .get(&(id >> 32).to_string())
                        .is_some_and(|accepted| *accepted >= *id)
                    {
                        base = sent.clone();
                    }
                }
                wanted = rebase(&base, &wanted, &latest);
                base = latest.clone();
                let changes = edits(&latest, &wanted);
                if changes.is_empty() {
                    self.composition.authoritative = latest;
                    self.composition.flight = None;
                    self.composition.dirty_since = None;
                    return true;
                }
                let sent = d.call(|id| Command::ComposerEdit {
                    id,
                    session,
                    base: latest.revision,
                    changes,
                });
                match sent {
                    Ok(data) if data["applied"].as_bool() == Some(true) => {
                        if let Ok(saved) =
                            serde_json::from_value::<Composer>(data["composer"].clone())
                        {
                            self.restore_composer(saved, false);
                            self.composition.flight = None;
                            self.composition.dirty_since = None;
                            return true;
                        }
                        self.note("Composer save did not return authoritative state; composition was retained.");
                        return false;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.note(error);
                        return !flush;
                    }
                }
            }
            self.note("Composer flush is contended; the latest local input was retained.");
            return !flush;
        }
        if self.composition.flight.is_some() {
            return !flush;
        }
        let changes = edits(&self.composition.authoritative, &visible);
        if changes.is_empty() {
            self.composition.dirty_since = None;
            return true;
        }
        let since = *self
            .composition
            .dirty_since
            .get_or_insert_with(Instant::now);
        if !self.composition.urgent && since.elapsed() < SESSION_EDIT_DEBOUNCE {
            return !flush;
        }
        self.composition.dirty_since = None;
        let base = self.composition.authoritative.clone();
        let id = d.ask(|id| Command::ComposerEdit {
            id,
            session,
            base: base.revision,
            changes,
        });
        self.awaiting.insert(id, Awaiting::ComposerEdit);
        self.composition.flight = Some((id, base, visible));
        self.composition.flight_started = Some(Instant::now());
        !flush
    }

    pub fn composer_event(&mut self, request: u64, composer: Composer) {
        if composer.revision < self.composition.authoritative.revision {
            return;
        }
        if self.composition.flight.is_some() {
            if composer.revision > self.composition.authoritative.revision
                && self
                    .composition
                    .deferred
                    .back()
                    .is_none_or(|saved| saved.revision < composer.revision)
            {
                self.composition.deferred.push_back(composer);
            }
            return;
        }
        let visible = self.composer_snapshot();
        let next = if self
            .awaiting
            .values()
            .any(|pending| matches!(pending, Awaiting::Prompt { .. }))
        {
            let mut next = composer.clone();
            next.text = visible.text;
            next.cursor = visible.cursor;
            next.attachments = visible.attachments;
            next
        } else {
            rebase(&self.composition.authoritative, &visible, &composer)
        };
        self.restore_composer(next, false);
        self.composition.authoritative = composer;
        let _ = request;
    }

    pub fn composer_ack(&mut self, id: u64, ok: bool, data: &serde_json::Value) {
        let Some((flight_id, base, sent)) = self.composition.flight.take() else {
            return;
        };
        let started = self.composition.flight_started.take();
        if flight_id != id {
            self.composition.flight = Some((flight_id, base, sent));
            self.composition.flight_started = started;
            return;
        }
        let visible = self.composer_snapshot();
        if !ok {
            for state in std::mem::take(&mut self.composition.deferred) {
                self.composer_event(0, state);
            }
            return;
        }
        if let Some(saved) = data
            .get("draft")
            .and_then(|draft| serde_json::from_value::<Draft>(draft.clone()).ok())
        {
            let generation_changed = self
                .composition
                .draft
                .as_ref()
                .is_some_and(|draft| draft.generation != saved.generation);
            let mut next = if generation_changed {
                if !visible.text.is_empty() {
                    let mut preserved = saved.composer.clone();
                    preserved.text = visible.text.clone();
                    preserved.cursor = visible.cursor;
                    preserved
                } else {
                    saved.composer.clone()
                }
            } else {
                let rebased = rebase(&sent, &visible, &saved.composer);
                if rebased.text.is_empty() && !visible.text.is_empty() {
                    let mut preserved = rebased;
                    preserved.text = visible.text.clone();
                    preserved.cursor = visible.cursor;
                    preserved
                } else {
                    rebased
                }
            };
            next.revision = saved.composer.revision;
            self.restore_composer(next, false);
            self.composition.authoritative = saved.composer.clone();
            self.composition.draft = Some(saved);
            self.composition.dirty_since = None;
        } else if let Some(saved) = data
            .get("composer")
            .and_then(|state| serde_json::from_value::<Composer>(state.clone()).ok())
        {
            let applied = data["applied"].as_bool().unwrap_or(true);
            let mut states = std::mem::take(&mut self.composition.deferred);
            if states
                .back()
                .is_none_or(|state| state.revision < saved.revision)
            {
                states.push_back(saved.clone());
            }
            let accepted = applied
                .then(|| {
                    states.iter().position(|state| {
                        state
                            .operations
                            .get(&(id >> 32).to_string())
                            .is_some_and(|accepted| *accepted >= id)
                    })
                })
                .flatten();
            let mut anchor = base;
            let mut next = visible.clone();
            let mut latest = self.composition.authoritative.clone();
            let first = if applied {
                let state = accepted
                    .map(|index| states[index].clone())
                    .unwrap_or_else(|| saved.clone());
                next = rebase(&sent, &next, &state);
                anchor = state.clone();
                latest = state;
                accepted.map_or(states.len(), |index| index + 1)
            } else {
                0
            };
            for state in states.into_iter().skip(first) {
                if state.revision <= latest.revision {
                    continue;
                }
                next = rebase(&anchor, &next, &state);
                anchor = state.clone();
                latest = state;
            }
            if next.text.is_empty() && !visible.text.is_empty() {
                next.text = visible.text.clone();
                next.cursor = visible.cursor;
            }
            self.restore_composer(next, false);
            self.composition.authoritative = latest;
        }
    }

    /// History updates never replace another frontend's local draft, except a
    /// consumed generation, which prevents pre-commit state from resurrecting.
    pub fn take_draft(&mut self, mut draft: Draft, settings: Option<serde_json::Value>) {
        if self
            .composition
            .draft
            .as_ref()
            .is_some_and(|current| current.generation > draft.generation)
        {
            return;
        }
        draft.history.sort_by_key(|entry| entry.order);
        let initial = self.composition.draft.is_none();
        let consumed = self
            .composition
            .draft
            .as_ref()
            .is_some_and(|old| old.generation != draft.generation);
        if initial || consumed {
            self.restore_composer(draft.composer.clone(), true);
            self.history_index = None;
            self.history_draft.clear();
            self.composition.history_saved = None;
        }
        if let Some(settings) = settings {
            self.take_settings(settings);
        }
        self.composition.draft = Some(draft);
    }
}

fn edits(base: &Composer, next: &Composer) -> Vec<Edit> {
    let mut changes = Vec::new();
    if base.text != next.text {
        let (start, end, text) = splice(&base.text, &next.text);
        changes.push(Edit::Splice { start, end, text });
    }
    for path in &base.attachments {
        if !next.attachments.contains(path) {
            changes.push(Edit::Remove { path: path.clone() });
        }
    }
    for path in &next.attachments {
        if !base.attachments.contains(path) {
            changes.push(Edit::Attach { path: path.clone() });
        }
    }
    changes
}

fn splice(before: &str, after: &str) -> (usize, usize, String) {
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
    (
        prefix,
        before.len() - suffix,
        after[prefix..after.len() - suffix].into(),
    )
}

/// Transform the local splice through the accepted remote splice. Concurrent
/// inserts survive; deletion targets old bytes, never newly inserted peer text.
pub(super) fn rebase(base: &Composer, local: &Composer, remote: &Composer) -> Composer {
    let mut result = remote.clone();
    if base.text != local.text {
        let (start, end, insert) = splice(&base.text, &local.text);
        let (rstart, rend, rinsert) = splice(&base.text, &remote.text);
        let shift = rinsert.len() as isize - (rend - rstart) as isize;
        let map = |offset: usize| -> usize {
            if offset < rstart {
                offset
            } else if offset >= rend {
                offset.saturating_add_signed(shift)
            } else {
                rstart + rinsert.len()
            }
        };
        let mut removals = Vec::new();
        if start < rstart {
            removals.push((start, end.min(rstart)));
        }
        if end > rend {
            removals.push((map(start.max(rend)), map(end)));
        }
        if rstart == rend && end == start {
            removals.clear();
        }
        let mapped = map(start);
        let removed_before: usize = removals
            .iter()
            .map(|(first, last)| mapped.min(*last).saturating_sub(*first))
            .sum();
        for (first, last) in removals.iter().rev() {
            if first < last
                && *last <= result.text.len()
                && result.text.is_char_boundary(*first)
                && result.text.is_char_boundary(*last)
            {
                result.text.replace_range(*first..*last, "");
            }
        }
        let position = mapped.saturating_sub(removed_before).min(result.text.len());
        if result.text.is_char_boundary(position) {
            result.text.insert_str(position, &insert);
        }
    }
    for path in &base.attachments {
        if !local.attachments.contains(path) {
            result.attachments.retain(|saved| saved != path);
        }
    }
    for path in &local.attachments {
        if !base.attachments.contains(path) && !result.attachments.contains(path) {
            result.attachments.push(path.clone());
        }
    }
    if local.settings != base.settings {
        result.settings = local.settings.clone();
    }
    if local.mode != base.mode {
        result.mode = local.mode.clone();
    }
    result.cursor = local.cursor.min(result.text.len());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduced under load with real windows (RuneLiveProviderPTY): an Enter
    /// pressed while the host had not yet confirmed an edit kept the composer,
    /// so `/undo` typed next was glued on and sent as `patch both/undo`. The
    /// message is what was there at its Enter; what follows is the next one.
    #[test]
    fn a_message_is_what_was_there_at_its_enter_whatever_the_host_still_confirms() {
        let mut d = crate::ui::test_driver("submit-while-edit-in-flight");
        let mut app = App::for_test_with(serde_json::json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro" }
        }));
        app.composition.ready = true;
        app.input = "patch both".into();
        app.cursor = app.input.len();
        let in_flight = app.composer_snapshot();
        let base = app.composition.authoritative.clone();
        app.composition.flight = Some((1u64 << 32, base.clone(), in_flight.clone()));
        app.composition.flight_started = Some(Instant::now());

        app.on_key(super::super::input::Key::Enter, &mut d);
        assert!(
            app.input.is_empty(),
            "the message left the composer at its Enter"
        );
        assert_eq!(app.composition.submits.len(), 1);
        for c in "/undo".chars() {
            app.on_key(super::super::input::Key::Char(c), &mut d);
        }

        let confirmed = Composer {
            revision: base.revision + 1,
            ..in_flight.clone()
        };
        app.composer_ack(
            1u64 << 32,
            true,
            &serde_json::json!({ "applied": true, "composer": confirmed }),
        );
        assert_eq!(app.input, "/undo", "what was typed after the Enter stays");
        app.send_submits(&mut d);
        let prompts: Vec<String> = app
            .awaiting
            .values()
            .filter_map(|awaiting| match awaiting {
                Awaiting::Prompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, vec!["patch both".to_string()]);
        assert!(app.composition.submits.is_empty());
        assert_eq!(app.input, "/undo");
    }

    /// The owner's report: typing `abc` then a remote snapshot with a **higher
    /// revision but empty text** (a peer's clear or an engine-side consume)
    /// arrives. The empty remote must not erase visible local typing — the
    /// engine-side consume path clears the composer only when the submission
    /// actually matched, and a live window that never submitted must keep its
    /// own unsent text.
    #[test]
    fn a_remote_empty_snapshot_with_a_newer_revision_never_wipes_visible_local_typing() {
        let mut app = App::for_test();
        app.restore_composer(
            Composer {
                text: String::new(),
                ..Default::default()
            },
            true,
        );
        app.input = "abc".into();
        app.cursor = 3;
        let remote = Composer {
            revision: 7,
            ..Default::default()
        };
        app.composer_event(0, remote);
        assert_eq!(app.input, "abc", "local typing must survive a remote clear");
        // And the authoritative state is advanced so the remote is not replayed.
        assert_eq!(app.composition.authoritative.revision, 7);
    }

    #[test]
    fn a_consumed_prompt_never_returns_through_late_composer_events_or_edit_ack() {
        // Real engine order: accepted composer edit -> submit -> TurnStart ->
        // consumed Composer -> prompt Ack. Exercise both event/ack delivery
        // orders with subsequent local typing; old revisions cannot resurrect.
        let request = 51u64 << 32;
        let base = Composer {
            revision: 10,
            text: "old".into(),
            ..Default::default()
        };
        let sent = Composer {
            revision: 11,
            text: "patch both".into(),
            ..base.clone()
        };
        let consumed = Composer {
            revision: 12,
            text: String::new(),
            ..sent.clone()
        };
        for event_first in [true, false] {
            for next_text in ["", "/undo", "new typing"] {
                let mut app = App::for_test();
                app.restore_composer(sent.clone(), true);
                app.composition.submitted = Some(sent.clone());
                app.awaiting.insert(
                    request,
                    Awaiting::Prompt {
                        text: sent.text.clone(),
                        media: Vec::new(),
                    },
                );
                app.input = next_text.into();
                app.cursor = next_text.len();
                if event_first {
                    app.composer_event(0, consumed.clone());
                }
                app.on_event(runeengine::Event::Ack {
                    id: request,
                    ok: true,
                    error: None,
                    data: Some(serde_json::json!({"composer":consumed})),
                });
                if !event_first {
                    app.composer_event(0, consumed.clone());
                }
                app.composer_event(request - 1, sent.clone());
                app.composer_event(0, consumed.clone());
                assert_eq!(app.input, next_text, "event_first={event_first}");
                assert_eq!(app.composition.authoritative.revision, 12);
            }
        }
    }

    #[test]
    fn a_late_prompt_ack_never_rolls_back_a_newer_composer_revision() {
        let request = 71u64 << 32;
        let committed = Composer {
            revision: 12,
            ..Default::default()
        };
        let newer = Composer {
            revision: 13,
            text: "next input".into(),
            cursor: 10,
            ..committed.clone()
        };
        let mut app = App::for_test();
        app.restore_composer(newer.clone(), true);
        app.awaiting.insert(
            request,
            Awaiting::Prompt {
                text: "sent prompt".into(),
                media: Vec::new(),
            },
        );
        app.on_event(runeengine::Event::Ack {
            id: request,
            ok: true,
            error: None,
            data: Some(serde_json::json!({"composer":committed})),
        });
        assert_eq!(
            app.composition.authoritative.revision, 13,
            "an older prompt ACK is not a newer editor state"
        );
        app.composer_event(0, newer);
        assert_eq!(
            app.input, "next input",
            "a replay after the ACK must not duplicate newer typing"
        );
    }

    /// Audit T1: keys are batched into one host edit per debounce window; a
    /// submit sends at once so the host's composer matches the prompt.
    #[test]
    fn session_edits_are_debounced_and_a_submit_sends_at_once() {
        let mut driver = crate::ui::test_driver("composer-debounce");
        let mut app = App::for_test();
        app.restore_composer(Composer::default(), true);
        let sent = |app: &App| {
            app.awaiting
                .values()
                .filter(|a| matches!(a, Awaiting::ComposerEdit))
                .count()
        };
        app.input = "a".into();
        app.sync_composer(&mut driver, false);
        app.input = "ab".into();
        app.sync_composer(&mut driver, false);
        assert_eq!(sent(&app), 0, "nothing goes out inside the window");
        app.composition.urgent = true;
        app.sync_composer(&mut driver, false);
        app.composition.urgent = false;
        assert_eq!(sent(&app), 1, "a submit does not wait");
        assert!(app.composition.dirty_since.is_none());
    }

    #[test]
    fn rejected_edit_rebases_through_each_remote_revision_not_one_aggregate_replacement() {
        let base = Composer {
            text: "abc".into(),
            ..Default::default()
        };
        let mut app = App::for_test();
        app.restore_composer(base.clone(), true);
        app.input = "aLc".into();
        let local = app.composer_snapshot();
        app.composition.flight = Some((1u64 << 32, base.clone(), local));
        let first = Composer {
            text: "Rabc".into(),
            revision: 1,
            ..base.clone()
        };
        let second = Composer {
            text: "RabcQ".into(),
            revision: 2,
            ..base.clone()
        };
        app.composer_event(0, first);
        app.composer_event(0, second.clone());
        app.composer_ack(
            1u64 << 32,
            true,
            &serde_json::json!({"applied":false,"composer":second}),
        );
        assert_eq!(
            app.input, "RaLcQ",
            "remote insertion must not turn untouched old bytes into a remote replacement"
        );
    }

    #[test]
    fn overlapping_clear_and_insert_converge_for_ordered_ack_schedules() {
        use serde_json::json;
        use std::collections::VecDeque;
        // Both windows clear a slash command and type independent next input.
        // The server serializes edits, while each socket preserves FIFO events.
        for seed in 1..=512u64 {
            let initial = Composer {
                text: "/detach all".into(),
                ..Default::default()
            };
            let mut host = initial.clone();
            let mut apps = [App::for_test(), App::for_test()];
            for app in &mut apps {
                app.restore_composer(initial.clone(), true);
            }
            let mut messages = [VecDeque::new(), VecDeque::new()];
            let mut requests = VecDeque::<(usize, u64, u64, Vec<Edit>)>::new();
            // Capture both slash-command clears before either window receives
            // new peer text. An explicit later user clear of visible Y would
            // legitimately delete Y and is not a lost-update reproduction.
            let mut actions = [1, 1];
            let mut serial = [(1u64 << 32) + 1, (2u64 << 32) + 1];
            for client in 0..2 {
                apps[client].input.clear();
                requests.push_back((
                    client,
                    serial[client],
                    initial.revision,
                    edits(&initial, &apps[client].composer_snapshot()),
                ));
                apps[client].composition.flight = Some((
                    serial[client],
                    initial.clone(),
                    apps[client].composer_snapshot(),
                ));
            }
            let mut random = seed;
            for _ in 0..200 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let choice = (random >> 32) as usize % 5;
                if choice < 2 {
                    let app = &mut apps[choice];
                    match actions[choice] {
                        0 => {
                            app.input.clear();
                            actions[choice] += 1;
                        }
                        1 => {
                            app.input.push(if choice == 0 { 'X' } else { 'Y' });
                            actions[choice] += 1;
                        }
                        _ => {}
                    }
                } else if choice == 2 {
                    if let Some((client, id, base, changes)) = requests.pop_front() {
                        let applied = host.revision == base;
                        if applied {
                            host = host
                                .edit(base, &changes, std::path::Path::new("/tmp"))
                                .unwrap();
                            host.operations.insert((id >> 32).to_string(), id);
                            for pending in &mut messages {
                                pending.push_back((None, host.clone(), true));
                            }
                        }
                        messages[client].push_back((Some(id), host.clone(), applied));
                    }
                } else {
                    let client = choice - 3;
                    if let Some((id, state, applied)) = messages[client].pop_front() {
                        if let Some(id) = id {
                            apps[client].composer_ack(
                                id,
                                true,
                                &json!({"applied": applied, "composer": state}),
                            );
                        } else {
                            apps[client].composer_event(0, state);
                        }
                    }
                }
                for client in 0..2 {
                    let app = &mut apps[client];
                    if app.composition.flight.is_none() {
                        let visible = app.composer_snapshot();
                        let changes = edits(&app.composition.authoritative, &visible);
                        if !changes.is_empty() {
                            serial[client] += 1;
                            let base = app.composition.authoritative.clone();
                            requests.push_back((client, serial[client], base.revision, changes));
                            app.composition.flight = Some((serial[client], base, visible));
                        }
                    }
                }
            }
            assert_eq!(
                actions,
                [2, 2],
                "schedule {seed} did not exercise both inputs"
            );
            assert!(
                host.text.contains('X') && host.text.contains('Y'),
                "schedule {seed}: lost input, host={:?}, peers={:?}",
                host.text,
                [&apps[0].input, &apps[1].input]
            );
        }
    }

    #[test]
    fn failed_draft_flush_retains_input_and_blocks_submit_or_switch() {
        let name = "composer-flush-failure";
        let mut driver = super::super::test_driver(name);
        let project = runeengine::platform::canonical(&std::env::temp_dir()).unwrap();
        let opened = driver
            .local(Command::DraftOpen {
                id: 0,
                project: project.to_string_lossy().into_owned(),
                token: None,
            })
            .unwrap();
        let mut app = App::for_test();
        app.session = None;
        app.project = project.to_string_lossy().into_owned();
        app.take_draft(
            serde_json::from_value(opened["draft"].clone()).unwrap(),
            opened.get("settings").cloned(),
        );
        app.settings["settings"]["model"] = "fixture".into();
        app.input = "keep this unsent text".into();
        app.cursor = app.input.len();
        let root = std::env::temp_dir()
            .join("rune-test")
            .join(format!("rune-{name}-{}", std::process::id()));
        // A directory where the draft registry lives: every save of it fails.
        std::fs::create_dir_all(root.join("drafts.json")).unwrap();
        assert!(!app.sync_composer(&mut driver, true));
        app.submit(&mut driver);
        assert!(!app.composition.promote_pending);
        app.switch_session(42, &mut driver);
        assert_eq!(app.session, None);
        assert_eq!(app.input, "keep this unsent text");
        driver.shutdown();
    }

    /// A draft window as one opens: the stored draft taken in, the screen on it.
    fn draft_window(d: &mut Driver) -> App {
        let project = runeengine::platform::canonical(&std::env::temp_dir()).unwrap();
        let opened = d
            .local(Command::DraftOpen {
                id: 0,
                project: project.to_string_lossy().into_owned(),
                token: None,
            })
            .unwrap();
        let mut app = App::for_test();
        app.session = None;
        app.project = project.to_string_lossy().into_owned();
        app.take_draft(
            serde_json::from_value(opened["draft"].clone()).unwrap(),
            opened.get("settings").cloned(),
        );
        app.composition.ready = true;
        app
    }

    /// Hand the window every answer to what it asked until nothing is left.
    fn answered(app: &mut App, d: &mut Driver) {
        while !app.awaiting.is_empty() {
            let event = d
                .events
                .recv_timeout(Duration::from_secs(10))
                .expect("the engine answers");
            app.on_event(event);
        }
    }

    /// Reproduced 2026-10-15 in a real window: a model chosen in a draft was
    /// said saved, then the screen went back to the model before it. The
    /// settings read that followed every change described the stored draft,
    /// which lags the screen until its save is answered.
    #[test]
    fn a_drafts_settings_are_read_as_the_screen_holds_them() {
        let mut d = super::super::test_driver("draft-settings-read");
        let mut app = draft_window(&mut d);
        let stored = app
            .composition
            .draft
            .as_ref()
            .unwrap()
            .composer
            .settings
            .model
            .clone();
        app.settings["settings"]["model"] = "chosen-model".into();
        assert_ne!(stored, "chosen-model");

        app.refresh(&mut d);
        answered(&mut app, &mut d);

        assert_eq!(
            app.model(),
            "chosen-model",
            "the read put the model before back"
        );
        assert_eq!(app.composer_snapshot().settings.model, "chosen-model");
        d.shutdown();
    }

    /// A settings read asked before a preference changed answers for the
    /// state before it: it never replaces the change's own answer, and the
    /// window reads again once the change is settled.
    #[test]
    fn a_settings_read_older_than_a_preference_change_never_replaces_it() {
        let mut d = super::super::test_driver("draft-settings-older-read");
        let mut app = draft_window(&mut d);
        app.settings["settings"]["model"] = "first-model".into();
        app.refresh(&mut d);
        // A change asked (and answered) while that read was on its way.
        app.preferences.serial += 1;
        app.settings["settings"]["model"] = "second-model".into();
        app.pending_provider_refresh = false;

        answered(&mut app, &mut d);

        assert_eq!(app.model(), "second-model");
        assert!(
            app.pending_provider_refresh,
            "the window reads again once settled"
        );
        app.pending_provider_refresh = false;
        app.refresh(&mut d);
        answered(&mut app, &mut d);
        assert_eq!(app.model(), "second-model");
        d.shutdown();
    }

    /// A settings page opened while a draft's model change is still on its
    /// way would read the screen as it stood before the change; the read waits
    /// for the change's answer instead.
    #[test]
    fn a_drafts_settings_are_not_read_while_a_change_is_on_its_way() {
        let mut d = super::super::test_driver("draft-settings-in-flight");
        let mut app = draft_window(&mut d);
        app.preferences.serial += 1;
        app.awaiting.insert(
            u64::MAX,
            Awaiting::Preferences(super::super::settings::Request {
                token: app.preferences.serial,
                session: None,
                origin: super::super::settings::Page::Models,
                editor: None,
                operation: super::super::settings::Operation::Setting(Some("model".into())),
                before: None,
            }),
        );
        app.pending_provider_refresh = false;
        app.refresh(&mut d);
        assert!(
            !app.awaiting
                .values()
                .any(|pending| matches!(pending, Awaiting::Settings { .. })),
            "a read was sent with the change still on its way"
        );
        assert!(
            app.pending_provider_refresh,
            "the read is asked again once settled"
        );
        app.awaiting.clear();
        d.shutdown();
    }

    /// A message brought back from the draft's history brings its words and
    /// files, not the model and mode it was once sent with.
    #[test]
    fn a_recalled_draft_message_keeps_the_model_and_mode_on_the_screen() {
        let mut d = super::super::test_driver("draft-history-recall");
        let mut app = draft_window(&mut d);
        let mut old = app.composer_snapshot();
        old.text = "an older message".into();
        old.settings.model = "deleted-model".into();
        old.mode = "full".into();
        let draft = app.composition.draft.as_mut().unwrap();
        draft.history.push(runeengine::store::drafts::Entry {
            token: "older".into(),
            order: 1,
            composer: old,
        });
        app.settings["settings"]["model"] = "current-model".into();
        app.mode = "ask".into();
        app.input = "typing now".into();

        app.history_prev();
        assert_eq!(app.input, "an older message");
        assert_eq!(app.model(), "current-model");
        assert_eq!(app.mode, "ask");

        app.history_next();
        assert_eq!(app.input, "typing now");
        assert_eq!(app.model(), "current-model");
        assert_eq!(app.mode, "ask");
        d.shutdown();
    }

    #[test]
    fn concurrent_insert_and_delete_preserve_acknowledged_peer_insert() {
        let base = Composer {
            text: "abc".into(),
            ..Default::default()
        };
        let local = Composer {
            text: "aLc".into(),
            ..base.clone()
        };
        let remote = Composer {
            text: "aRbc".into(),
            revision: 1,
            ..base.clone()
        };
        assert_eq!(rebase(&base, &local, &remote).text, "aRLc");
        let local = Composer {
            text: "Zc".into(),
            ..base.clone()
        };
        assert_eq!(rebase(&base, &local, &remote).text, "ZRc");
        let unicode = Composer {
            text: "é🙂x".into(),
            ..Default::default()
        };
        let local = Composer {
            text: "Qx".into(),
            ..unicode.clone()
        };
        let remote = Composer {
            text: "éR🙂x".into(),
            ..unicode.clone()
        };
        assert_eq!(rebase(&unicode, &local, &remote).text, "QRx");
        let a = Composer {
            text: "a".into(),
            ..Default::default()
        };
        let b = Composer {
            text: "b".into(),
            revision: 1,
            ..Default::default()
        };
        assert_eq!(rebase(&Composer::default(), &a, &b).text, "ba");
    }
}
