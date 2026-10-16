//! Counts are content, not a timestamp or an arithmetic stamp. One publisher
//! serialises settled writes, external refreshes and restore results through Bus.
use super::{Runtime, SessionState};
use crate::{checkpoint::FileChange, error::Result, panic, proto::Event};
use std::sync::{Arc, Mutex, Weak};

pub struct Publisher {
    shown: Mutex<Vec<FileChange>>,
    writing: Mutex<()>,
    last_error: Mutex<Option<String>>,
    watcher: Mutex<Option<Watcher>>,
}

struct Watcher {
    stop: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Default for Publisher {
    fn default() -> Self {
        Self {
            shown: Mutex::new(Vec::new()),
            writing: Mutex::new(()),
            last_error: Mutex::new(None),
            watcher: Mutex::new(None),
        }
    }
}

impl Publisher {
    pub fn write(&self) -> std::sync::MutexGuard<'_, ()> {
        panic::lock(&self.writing)
    }

    pub fn refresh(&self, rt: &Runtime, st: &SessionState) -> Result<Vec<FileChange>> {
        let _writes = self.write();
        self.refresh_locked(rt, st)
    }

    fn refresh_locked(&self, rt: &Runtime, st: &SessionState) -> Result<Vec<FileChange>> {
        // Keep this hold through publication: an older tally can never arrive
        // after a newer restore, regardless of which thread started first.
        let mut shown = panic::lock(&self.shown);
        let changes = st.checkpoints.session_changes(st.id, &st.jail())?;
        if *shown != changes {
            let _ = rt.events.send(Event::FileChanges {
                session: st.id,
                changes: changes.clone(),
            });
            *shown = changes.clone();
        }
        *panic::lock(&self.last_error) = None;
        Ok(changes)
    }

    fn failed(&self, rt: &Runtime, st: &SessionState, error: crate::Error) {
        let reason = error.to_string();
        let mut last = panic::lock(&self.last_error);
        if last.as_ref() != Some(&reason) {
            super::log::warn(
                rt,
                st.id,
                "checkpoint",
                error.code(),
                format!("Could not tally session file changes: {reason}"),
            );
            *last = Some(reason);
        }
    }

    pub fn settled(&self, rt: &Runtime, st: &SessionState) {
        if let Err(error) = self.refresh(rt, st) {
            self.failed(rt, st, error);
        }
    }

    pub fn watch(&self, rt: &Runtime, state: &Arc<SessionState>) {
        let mut watcher = panic::lock(&self.watcher);
        if watcher.is_some() {
            return;
        }
        let (stop, receiver) = std::sync::mpsc::channel();
        let weak: Weak<SessionState> = Arc::downgrade(state);
        let rt = rt.clone();
        let thread = std::thread::spawn(move || {
            // Content checks are portable and bounded to the session ledger;
            // no platform watcher or project-wide file inventory is assumed.
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                receiver.recv_timeout(rt.limits.config.duration("checkpoint.refresh_interval"))
            {
                let Some(st) = weak.upgrade() else {
                    break;
                };
                // A read never opens a busy period, clears cancel, or resumes tasks.
                // Known writes hold this barrier; external changes are sampled boundedly.
                if let Ok(_writes) = st.files.writing.try_lock() {
                    if let Err(error) = st.files.refresh_locked(&rt, &st) {
                        st.files.failed(&rt, &st, error);
                    }
                };
            }
        });
        *watcher = Some(Watcher { stop, thread });
    }

    pub fn stop(&self) {
        let watcher = panic::lock(&self.watcher).take();
        if let Some(watcher) = watcher {
            let _ = watcher.stop.send(());
            if watcher.thread.thread().id() != std::thread::current().id() {
                let _ = watcher.thread.join();
            }
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stop();
    }
}
