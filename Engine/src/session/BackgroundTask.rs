//! Background task registry, process lifecycle management, and notification dispatch.
//!
//! # A task outlives the program watching it
//!
//! A live upgrade `exec`s a newer build of Rune in place — same process, same
//! pid — and a background command (a dev server, a watcher) must keep running
//! through it, untouched (owner, 2026-10-06). So a task is watched by pid, not
//! through a handle only one program can hold, and its terminal can be let go
//! of ([`BackgroundTaskManager::release`]) and taken over by the next build
//! ([`BackgroundTaskManager::adopt`]): the process never notices, its output
//! waits in the terminal for the few hundred milliseconds nobody reads it, and
//! its log continues where it stopped.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::terminal_screen::{Screen, Shown};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    Completed {
        exit_code: i32,
        finished_at: i64,
    },
    Killed {
        finished_at: i64,
    },
    /// It was running when Rune stopped watching it, and nobody saw it end:
    /// what it did and its exit code are unknown, never guessed.
    Lost {
        reason: LostReason,
        finished_at: i64,
    },
}

/// Why a task's end was never seen — the one thing about it that is known.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LostReason {
    /// The computer started again since the task was launched: the process
    /// went with the machine.
    Restart,
    /// Rune stopped (a crash, a kill) while the machine ran on; a process of
    /// the task still alive then is stopped, since nothing could watch it.
    RuneStopped,
    /// Nothing says which: the task's start carries no identity (a record an
    /// earlier build wrote), or its exit status could not be read.
    Unknown,
}

impl LostReason {
    /// Why it is lost, as the end of a sentence about the task.
    pub fn text(self) -> &'static str {
        match self {
            LostReason::Restart => {
                "the computer restarted while it was running, so how it ended is unknown"
            }
            LostReason::RuneStopped => {
                "Rune stopped while it was running, so how it ended is unknown"
            }
            LostReason::Unknown => {
                "Rune stopped tracking it before it ended (Rune or the computer stopped), so how it ended is unknown"
            }
        }
    }
}

/// What tells this task's process apart from any other that later gets its
/// number: the boot it ran in and when it began in that boot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    /// When the machine started, in seconds since the Unix epoch.
    pub boot: u64,
    /// When the process began, in the kernel's ticks since that boot.
    pub started: u64,
}

impl Identity {
    /// The identity of running process `pid`, when the system tells it.
    pub fn of(pid: i32) -> Option<Identity> {
        let boot = crate::platform::boot_time()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let started = crate::platform::process_started(u32::try_from(pid).ok()?)?;
        Some(Identity { boot, started })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTaskInfo {
    pub task_id: String,
    pub pid: i32,
    pub command: String,
    pub out_file: PathBuf,
    pub started_at: i64,
    pub status: BackgroundTaskStatus,
    /// Whether the wake for this completion has been settled already.
    #[serde(default)]
    pub surfaced: bool,
    /// A tool result has reported this completion in full, so the wake has
    /// nothing left to say and is withdrawn before it is spoken.
    #[serde(default)]
    pub answered: bool,
    /// The wake text queued for this task, so it can be recognised and
    /// withdrawn out of a steer that carries more than one notification.
    #[serde(default)]
    pub wake_text: Option<String>,
    /// Capture failure or buffer-limit termination, never mixed into stdout.
    #[serde(default)]
    pub capture_note: Option<String>,
    /// What the process's screen shows now, as text: the rows that have not
    /// scrolled off yet, which it may still redraw (a progress bar, a status
    /// block). Its log holds only the rows that left the screen
    /// ([`Screen::drain`]); this is shown after them, and goes into the log
    /// when the process ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tail: String,
    /// The same screen, exactly, for the build that takes the task over across
    /// an upgrade. A build that handed on only `tail` is resumed from that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<Shown>,
    /// What `manage_process` last reported of it; `None` until the first report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported: Option<Reported>,
    /// It is running and blocked reading its terminal, as the platform saw
    /// it (`Pty::input_wait`); `None` while it runs on, or nobody can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<crate::platform::InputWait>,
    /// The mark every process of its tree carries (`platform::TREE_VAR`), so
    /// a stop reaches what left its process group; `None` for a task an
    /// earlier build started, stopped by its group alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
}

/// How far the model has been given a task's output, so a report holds only
/// what is new: the log up to `log` bytes, and the screen as `tail` was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    pub log: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tail: String,
}

/// What the session's `background.jsonl` keeps about its background tasks,
/// so a task whose end the model was never told is told when the session is
/// opened again — a promise ("you will receive a <system_notification>") that
/// a crash, a power cut or a close with no window open must not break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BackgroundRecord {
    Started {
        task_id: String,
        pid: i32,
        command: String,
        out_file: PathBuf,
        started_at: i64,
        /// Absent in a record written before identities were kept: such a
        /// task, if lost, is lost for an unknown reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<Identity>,
        /// Its tree's mark, so what is left of a lost task can be found.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tree: Option<String>,
    },
    /// How it ended, as Rune saw it.
    Ended {
        task_id: String,
        status: BackgroundTaskStatus,
    },
    /// The model was told how it ended: a wake was sent, a `manage_process`
    /// report said so, or the session's opening said it was lost.
    Told { task_id: String },
}

impl BackgroundRecord {
    fn task_id(&self) -> &str {
        match self {
            BackgroundRecord::Started { task_id, .. }
            | BackgroundRecord::Ended { task_id, .. }
            | BackgroundRecord::Told { task_id } => task_id,
        }
    }
}

/// Where a session's [`BackgroundRecord`]s are kept.
pub trait BackgroundJournal: Send + Sync {
    fn append(&self, record: &BackgroundRecord);
    fn read(&self) -> Vec<BackgroundRecord>;
}

/// A process id as the model may name it: `bg_` and the letters
/// [`crate::tools::unique_persist_id`] draws from — so a name read back as a
/// file under the session's results can never be a path of its own.
pub fn is_task_id(id: &str) -> bool {
    id.strip_prefix("bg_").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 32
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

pub trait SessionWakeNotifier: Send + Sync {
    fn notify(&self, text: String);
    /// Take back a wake [`Self::notify`] gave, while it still waits unread for
    /// the work in progress: what it said is over. One already read, or one
    /// that began a turn of its own, stays where it is.
    fn withdraw(&self, _text: &str) {}
    /// Messages are waiting and the work that held the session ended without
    /// taking them: start the work that answers them.
    fn waiting(&self) {}
}

/// A task as one build hands it to the next across `exec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandedTask {
    pub info: BackgroundTaskInfo,
    /// Its terminal's master, open across the exec — `None` for a task that
    /// had already ended, handed over so it can still be reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty: Option<crate::platform::Handed>,
    /// Bytes of output already in its log, counted against the log's limit.
    #[serde(default)]
    pub written: usize,
    /// The supervisor of its connections, open across the exec — `None` when
    /// nothing supervises it (`platform::release_supervision`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<crate::platform::Handed>,
}

/// How long letting go of a task waits for the answers its connection
/// supervisor is still making; a question to the person outlasts it, and the
/// upgrade waits for a later idle moment.
const SUPERVISION_GRACE: Duration = Duration::from_secs(2);

/// Every task of one session, as one build hands them to the next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTasks {
    pub session: i64,
    pub tasks: Vec<HandedTask>,
}

/// How a monitor watches its task: the configuration as it stands at each
/// look, so a change to `rune.toml` reaches a task already running (hot
/// reload, never a snapshot taken at its start).
#[derive(Debug, Clone)]
pub struct Watch(WatchSource);

#[derive(Debug, Clone)]
enum WatchSource {
    Live(crate::settings::configuration::Shared),
    /// Fixed values, for a test that needs them exact.
    #[cfg(test)]
    Fixed {
        max_bytes: usize,
        cancel_grace: u64,
        drain: Duration,
    },
}

impl Watch {
    /// The watch the configuration asks for, read live.
    pub fn of(config: &crate::settings::configuration::Shared) -> Watch {
        Watch(WatchSource::Live(config.clone()))
    }

    #[cfg(test)]
    pub(crate) fn fixed(max_bytes: usize, cancel_grace: u64, drain: Duration) -> Watch {
        Watch(WatchSource::Fixed {
            max_bytes,
            cancel_grace,
            drain,
        })
    }

    /// How large the task's log may grow on disk before the task is stopped.
    fn max_bytes(&self) -> usize {
        match &self.0 {
            WatchSource::Live(config) => config.number("terminal.max_background_log_size") as usize,
            #[cfg(test)]
            WatchSource::Fixed { max_bytes, .. } => *max_bytes,
        }
    }

    /// How long a stopped task gets before it is killed, in milliseconds.
    fn cancel_grace(&self) -> u64 {
        match &self.0 {
            WatchSource::Live(config) => config.number("terminal.cancel_grace"),
            #[cfg(test)]
            WatchSource::Fixed { cancel_grace, .. } => *cancel_grace,
        }
    }

    /// How long a task must be quiet before it is asked whether it waits.
    fn input_wait_delay(&self) -> Duration {
        match &self.0 {
            WatchSource::Live(config) => config.duration("terminal.input_wait_delay"),
            #[cfg(test)]
            WatchSource::Fixed { drain, .. } => *drain * 4,
        }
    }

    /// How long output is still drained after the task exited.
    fn drain(&self) -> Duration {
        match &self.0 {
            WatchSource::Live(config) => config.duration("terminal.drain_after_exit"),
            #[cfg(test)]
            WatchSource::Fixed { drain, .. } => *drain,
        }
    }
}

/// A monitor thread, and the flag that asks it to let go of its task.
struct Monitor {
    release: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Ending>,
    watch: Watch,
    /// Set when input was typed into the task: whether it waits is asked
    /// again once it has been quiet, even if it printed nothing (a hidden
    /// entry echoes nothing).
    rearm: Arc<AtomicBool>,
    /// The task's terminal, to type into. Weak: the monitor holds it, and a
    /// handover needs to be its only holder.
    pty: std::sync::Weak<crate::platform::Pty>,
}

/// What the session was told of tasks waiting for input.
///
/// One task asks the same words at every wait ("bg_x is waiting for input"),
/// so a wake is taken back by the notifier the moment its wait is over or a
/// result said it ([`SessionWakeNotifier::withdraw`]) — never by matching its
/// text later, which took the next wait's wake, word for word the same, with
/// it. Sending and taking back happen under this one lock, so a wake is never
/// queued after it was taken back.
#[derive(Default)]
struct Waits {
    /// Tasks whose present wait the model knows: a wake said it, or a
    /// result did (`terminal_use` moving a waiting command, a
    /// `manage_process` report).
    told: std::collections::HashSet<String>,
    /// The wake sent for each present wait, until it is taken back.
    wakes: HashMap<String, String>,
}

/// How a monitor thread ended.
enum Ending {
    /// The task ended and was reported.
    Finished,
    /// Asked to let go while the task still ran: its terminal and log, as
    /// they stand.
    Released {
        pty: Arc<crate::platform::Pty>,
        file: std::fs::File,
        written: usize,
    },
}

pub struct BackgroundTaskManager {
    tasks: Mutex<HashMap<String, BackgroundTaskInfo>>,
    notifier: Mutex<Option<Arc<dyn SessionWakeNotifier>>>,
    stopped: Arc<AtomicBool>,
    monitors: Mutex<HashMap<String, Monitor>>,
    journal: Mutex<Option<Arc<dyn BackgroundJournal>>>,
    /// The session's unreported tasks were settled once already.
    settled: AtomicBool,
    waits: Mutex<Waits>,
}

impl Default for BackgroundTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskManager {
    pub fn new() -> Self {
        BackgroundTaskManager {
            tasks: Mutex::new(HashMap::new()),
            notifier: Mutex::new(None),
            stopped: Arc::new(AtomicBool::new(false)),
            monitors: Mutex::new(HashMap::new()),
            journal: Mutex::new(None),
            settled: AtomicBool::new(false),
            waits: Mutex::new(Waits::default()),
        }
    }

    /// The notifier's word, when the session has one.
    fn notifier(&self) -> Option<Arc<dyn SessionWakeNotifier>> {
        self.notifier.lock().ok().and_then(|lock| lock.clone())
    }

    /// The model was told, in a result, that `task_id` waits for input: a
    /// wake still unread about that wait would say it a second time, so it
    /// is taken back.
    pub fn wait_told(&self, task_id: &str) {
        let Ok(mut waits) = self.waits.lock() else {
            return;
        };
        waits.told.insert(task_id.to_string());
        if let Some(text) = waits.wakes.remove(task_id) {
            if let Some(notifier) = self.notifier() {
                notifier.withdraw(&text);
            }
        }
    }

    /// Tell the model by a wake that `task_id` waits, when it does: a wait
    /// found by a call whose result the model does not read.
    pub fn tell_waiting(&self, task_id: &str) {
        if let Some(wait) = self.get(task_id).and_then(|task| task.waiting) {
            self.tell_wait(task_id, wait);
        }
    }

    /// `task_id` is blocked reading its terminal. A wait that begins is told
    /// once, by a wake — unless a result already said it, or nobody watches
    /// the session now (the wake is then told when it is watched again,
    /// [`Self::deliver_pending`]).
    pub fn set_waiting(&self, task_id: &str, wait: crate::platform::InputWait) {
        let began = {
            let Ok(mut tasks) = self.tasks.lock() else {
                return;
            };
            let Some(task) = tasks.get_mut(task_id) else {
                return;
            };
            if !matches!(task.status, BackgroundTaskStatus::Running) {
                return;
            }
            let began = task.waiting.is_none();
            task.waiting = Some(wait);
            began
        };
        if began {
            self.tell_wait(task_id, wait);
        }
    }

    /// The wake for `task_id`'s present wait, unless it was told already.
    /// It is a ping, as an end's wake is: what the process asks is in its
    /// output, which `manage_process` reports.
    fn tell_wait(&self, task_id: &str, wait: crate::platform::InputWait) {
        if self.is_stopped() {
            return;
        }
        let hidden = if wait.hidden {
            format!(" {}", crate::tools::notice::HIDDEN_INPUT)
        } else {
            String::new()
        };
        let text = format!(
            "<system_notification>Background task {task_id} is waiting for input.{hidden} \
             Call manage_process with process_id: {task_id} for what it asks, then answer it with terminal_send_input.</system_notification>"
        );
        // Sent under the lock that takes it back, so a wait that ends while
        // its wake is on its way never leaves that wake queued behind it.
        let Ok(mut waits) = self.waits.lock() else {
            return;
        };
        if !waits.told.insert(task_id.to_string()) {
            return;
        }
        waits.wakes.insert(task_id.to_string(), text.clone());
        if let Some(notifier) = self.notifier() {
            notifier.notify(text);
        }
    }

    /// `task_id` no longer waits: it is Running again. A wait `over` (typed
    /// into, gone by itself, or the task ended) is forgotten, and its wake not
    /// read yet is taken back. One that only printed meanwhile — a countdown,
    /// a reminder — and waits again is the same wait, still told: a read ends
    /// only by input, so nothing new was asked, and nothing is said twice.
    pub fn clear_waiting(&self, task_id: &str, over: bool) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.waiting = None;
            }
        }
        if over {
            self.forget_wait(task_id);
        }
    }

    /// Forget that `task_id`'s wait was told — input was typed into it, or it
    /// ended — and take its wake back if it is still unread: the next wait,
    /// whose wake says the very same words, is a new one.
    fn forget_wait(&self, task_id: &str) {
        let Ok(mut waits) = self.waits.lock() else {
            return;
        };
        waits.told.remove(task_id);
        if let Some(text) = waits.wakes.remove(task_id) {
            if let Some(notifier) = self.notifier() {
                notifier.withdraw(&text);
            }
        }
    }

    /// Whether running task `task_id`'s terminal reads a line at Enter;
    /// `None` when it has no terminal here or the platform cannot tell.
    pub fn line_mode(&self, task_id: &str) -> Option<bool> {
        let pty = self.monitors.lock().ok()?.get(task_id)?.pty.upgrade()?;
        pty.line_mode()
    }

    /// Type `bytes` into running task `task_id`'s terminal, and have its
    /// monitor ask again, once it is quiet, whether it waits.
    pub fn send_input(&self, task_id: &str, bytes: &[u8]) -> crate::error::Result<()> {
        let (pty, rearm) = {
            let monitors = self.monitors.lock().map_err(|_| {
                crate::error::Error::Internal("background monitors poisoned".into())
            })?;
            let monitor = monitors.get(task_id).ok_or_else(|| {
                crate::error::Error::invalid(format!(
                    "background process '{task_id}' has no terminal to type into: it is not running here"
                ))
            })?;
            (monitor.pty.upgrade(), Arc::clone(&monitor.rearm))
        };
        let pty = pty.ok_or_else(|| {
            crate::error::Error::invalid(format!(
                "background process '{task_id}' has no terminal to type into: it has ended"
            ))
        })?;
        pty.write_all(bytes)?;
        rearm.store(true, Ordering::Release);
        Ok(())
    }

    pub fn set_journal(&self, journal: Arc<dyn BackgroundJournal>) {
        if let Ok(mut lock) = self.journal.lock() {
            *lock = Some(journal);
        }
    }

    fn record(&self, record: BackgroundRecord) {
        let journal = self.journal.lock().ok().and_then(|lock| lock.clone());
        if let Some(journal) = journal {
            journal.append(&record);
        }
    }

    /// A task this session started: listed, and kept in its journal.
    pub fn register_started(&self, info: BackgroundTaskInfo) {
        self.record(BackgroundRecord::Started {
            task_id: info.task_id.clone(),
            pid: info.pid,
            command: info.command.clone(),
            out_file: info.out_file.clone(),
            started_at: info.started_at,
            identity: Identity::of(info.pid),
            tree: info.tree.clone(),
        });
        self.register(info);
    }

    /// The model was told how `task_id` ended.
    pub fn told(&self, task_id: &str) {
        self.record(BackgroundRecord::Told {
            task_id: task_id.to_string(),
        });
    }

    /// Rebuild, once per opening of the session, every task its journal
    /// knows that this program does not hold (a task handed on by an upgrade
    /// is watched here already), with the last state the journal saw — so
    /// `manage_process` answers for a task of an earlier opening exactly as it
    /// did then: Completed stays Completed with its exit code, whatever
    /// happened to the machine after. A task whose end nobody saw (the host
    /// crashed, the power went) is Lost, recorded as such once, with the
    /// reason its identity tells. The model is told of every end it was never
    /// told, once.
    pub fn settle_unreported(&self) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        let records = match self.journal.lock().ok().and_then(|lock| lock.clone()) {
            Some(journal) => journal.read(),
            None => return,
        };
        let mut order: Vec<String> = Vec::new();
        let mut started: HashMap<String, (BackgroundTaskInfo, Option<Identity>)> = HashMap::new();
        let mut ended: HashMap<String, BackgroundTaskStatus> = HashMap::new();
        let mut told: std::collections::HashSet<String> = Default::default();
        for record in records {
            let id = record.task_id().to_string();
            match record {
                BackgroundRecord::Started {
                    task_id,
                    pid,
                    command,
                    out_file,
                    started_at,
                    identity,
                    tree,
                } => {
                    order.push(id);
                    let info = BackgroundTaskInfo {
                        task_id: task_id.clone(),
                        pid,
                        command,
                        out_file,
                        started_at,
                        status: BackgroundTaskStatus::Running,
                        surfaced: true,
                        answered: false,
                        wake_text: None,
                        capture_note: None,
                        tail: String::new(),
                        screen: None,
                        reported: None,
                        waiting: None,
                        tree,
                    };
                    started.insert(task_id, (info, identity));
                }
                BackgroundRecord::Ended { status, .. } => {
                    // The first end recorded is the end: a kill's own exit
                    // that follows it is never a second one.
                    ended.entry(id).or_insert(status);
                }
                BackgroundRecord::Told { .. } => {
                    told.insert(id);
                }
            }
        }
        for id in order {
            if self.get(&id).is_some() {
                continue;
            }
            let Some((mut info, identity)) = started.remove(&id) else {
                continue;
            };
            let status = match ended.remove(&id) {
                Some(status) => status,
                None => {
                    let status = BackgroundTaskStatus::Lost {
                        reason: lost_reason(info.pid, identity, info.tree.clone()),
                        finished_at: crate::store::now_ms(),
                    };
                    self.record(BackgroundRecord::Ended {
                        task_id: id.clone(),
                        status: status.clone(),
                    });
                    status
                }
            };
            let was_told = told.contains(&id);
            info.status = status.clone();
            info.answered = was_told;
            self.register(info);
            if was_told {
                continue;
            }
            let text = match &status {
                BackgroundTaskStatus::Lost { reason, .. } => format!(
                    "<system_notification>Background task {id} was lost: {}. \
                     Call manage_process with process_id: {id} for the output it left.</system_notification>",
                    reason.text()
                ),
                status => format!(
                    "<system_notification>Background task {id} ended while this session was not open: {}. \
                     Call manage_process with process_id: {id} for its result.</system_notification>",
                    outcome_of(status)
                ),
            };
            self.told(&id);
            if let Ok(lock) = self.notifier.lock() {
                if let Some(notifier) = lock.as_ref() {
                    notifier.notify(text);
                }
            }
        }
    }

    pub fn set_notifier(&self, notifier: Arc<dyn SessionWakeNotifier>) {
        if let Ok(mut lock) = self.notifier.lock() {
            *lock = Some(notifier);
        }
    }

    pub fn register(&self, info: BackgroundTaskInfo) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.insert(info.task_id.clone(), info);
        }
    }

    pub fn list(&self) -> Vec<BackgroundTaskInfo> {
        self.tasks
            .lock()
            .map(|t| t.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, task_id: &str) -> Option<BackgroundTaskInfo> {
        self.tasks.lock().ok()?.get(task_id).cloned()
    }

    /// `task_id` ended as `status`. The first end is the end — the one the
    /// journal keeps and the model was told: a kill's own exit that follows it
    /// (Completed), or an exit status that could not be read after it (Lost),
    /// never replaces it.
    pub fn update_status(&self, task_id: &str, status: BackgroundTaskStatus) {
        let ended = {
            let Ok(mut tasks) = self.tasks.lock() else {
                return;
            };
            let Some(task) = tasks.get_mut(task_id) else {
                return;
            };
            if !matches!(task.status, BackgroundTaskStatus::Running) {
                return;
            }
            let ended = !matches!(status, BackgroundTaskStatus::Running);
            task.status = status.clone();
            ended
        };
        // Written outside the lock: a journal write waits for the disk.
        if ended {
            self.record(BackgroundRecord::Ended {
                task_id: task_id.to_string(),
                status,
            });
        }
    }

    /// Stop running task `task_id`: it is Killed, and a wait it had is over
    /// (its wake, unread, taken back). `told` when the model reads the result
    /// of the call that stops it: that end is then told in the same step that
    /// makes it, so the monitor that sees the process go has nothing to wake
    /// the session for (the bug of 2026-10-11: a `manage_process` kill was
    /// followed by "finished: Killed"). A stop the model does not read — a
    /// person's own call — is still the model's to hear, by that wake. False
    /// when it was not running.
    pub fn stop(&self, task_id: &str, told: bool) -> bool {
        let (pid, tree, status) = {
            let Ok(mut tasks) = self.tasks.lock() else {
                return false;
            };
            let Some(task) = tasks.get_mut(task_id) else {
                return false;
            };
            if !matches!(task.status, BackgroundTaskStatus::Running) {
                return false;
            }
            task.status = BackgroundTaskStatus::Killed {
                finished_at: crate::store::now_ms(),
            };
            task.answered |= told;
            (task.pid, task.tree.clone(), task.status.clone())
        };
        self.record(BackgroundRecord::Ended {
            task_id: task_id.to_string(),
            status,
        });
        if told {
            self.told(task_id);
        }
        self.clear_waiting(task_id, true);
        crate::platform::terminate_tree(pid, tree.as_deref(), 200);
        true
    }

    /// The session's runtime stops — its last window went away — while its
    /// background tasks run on: wakes are held, so no model request starts
    /// for a session nobody watches, and no task is touched. A task that ends
    /// meanwhile keeps its wake for [`Self::deliver_pending`]; the tasks
    /// themselves end with [`Self::stop_all`] when the session is let go.
    pub fn suspend(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Wakes held while the session was suspended, said now that it is
    /// watched again: every task that ended unannounced, once each.
    pub fn deliver_pending(&self) {
        if self.is_stopped() {
            return;
        }
        let ended: Vec<String> = self
            .tasks
            .lock()
            .map(|tasks| {
                tasks
                    .values()
                    .filter(|task| {
                        !matches!(task.status, BackgroundTaskStatus::Running)
                            && !task.surfaced
                            && !task.answered
                    })
                    .map(|task| task.task_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in ended {
            self.notify_completed(&id);
        }
        let waiting: Vec<(String, crate::platform::InputWait)> = self
            .list()
            .into_iter()
            .filter(|task| matches!(task.status, BackgroundTaskStatus::Running))
            .filter_map(|task| task.waiting.map(|wait| (task.task_id, wait)))
            .collect();
        for (id, wait) in waiting {
            self.tell_wait(&id, wait);
        }
    }

    pub fn stop_all(&self) {
        self.suspend();
        let running: Vec<(String, i32, Option<String>)> = {
            if let Ok(tasks) = self.tasks.lock() {
                tasks
                    .values()
                    .filter(|t| matches!(t.status, BackgroundTaskStatus::Running))
                    .map(|t| (t.task_id.clone(), t.pid, t.tree.clone()))
                    .collect()
            } else {
                Vec::new()
            }
        };

        let now = crate::store::now_ms();
        for (id, pid, tree) in running {
            self.update_status(&id, BackgroundTaskStatus::Killed { finished_at: now });
            crate::platform::terminate_tree(pid, tree.as_deref(), 200);
        }
    }

    pub fn resume(&self) {
        self.stopped.store(false, Ordering::Relaxed);
    }

    /// The session let go with messages still queued: have them answered —
    /// unless the session's wakes are suspended (its last window left).
    pub fn waiting(&self) {
        if self.is_stopped() {
            return;
        }
        if let Ok(lock) = self.notifier.lock() {
            if let Some(notifier) = lock.as_ref() {
                notifier.waiting();
            }
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Settle this task's completion wake. Returns true for whoever gets here
    /// first, false for everyone after.
    ///
    /// The wake is a ping, not the report, so this is not about who says the
    /// result — `manage_process` always prints it. It is about the wake itself
    /// being said once: the claim stops a second wake for one process.
    fn claim_surface(&self, task_id: &str) -> bool {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                // Lost too: an end whose exit status could not be read is
                // still an end the model was promised a wake for.
                let finished = !matches!(task.status, BackgroundTaskStatus::Running);
                if finished && !task.surfaced {
                    task.surfaced = true;
                    return true;
                }
            }
        }
        false
    }

    /// Record that a tool result has reported this task's completion in full.
    ///
    /// The wake exists to tell the model that something finished. Once the model
    /// has the whole report, the wake is a second telling of the same fact, so
    /// it is withdrawn rather than delivered.
    pub fn answer_wake(&self, task_id: &str) {
        let wake = self.tasks.lock().ok().and_then(|mut tasks| {
            let task = tasks.get_mut(task_id)?;
            task.answered = true;
            task.wake_text.clone()
        });
        self.told(task_id);
        // Taken back now while it waits unread; one queued after this, in the
        // instant between the wake's claim and its sending, is taken out when
        // the queue is read ([`Self::without_answered_wakes`]).
        if let (Some(text), Some(notifier)) = (wake, self.notifier()) {
            notifier.withdraw(&text);
        }
    }

    /// A queued steer with every answered wake taken back out of it.
    ///
    /// One steer can carry several wakes, and only some of them may have been
    /// answered, so the text is edited rather than the whole message dropped.
    /// An empty result means there is nothing left to say.
    pub fn without_answered_wakes(&self, text: &str) -> String {
        let Ok(tasks) = self.tasks.lock() else {
            return text.trim().to_string();
        };
        let mut out = text.to_string();
        for task in tasks.values() {
            if task.answered {
                if let Some(wake) = &task.wake_text {
                    out = out.replace(wake.as_str(), "");
                }
            }
        }
        out.trim().to_string()
    }

    /// Wake the session that a background task has ended, unless something has
    /// already settled it.
    pub fn notify_completed(&self, task_id: &str) {
        // Esc/cancel ends a turn, not the active session's completion wakes.
        // Only client-last-exit / teardown calls stop_all to suspend them.
        if self.is_stopped() {
            return;
        }

        if !self.claim_surface(task_id) {
            return;
        }

        // A ping, not a report. It says the task finished and how it ended, and
        // points at the tool that answers for it. The id / pid / status / log
        // block is `manage_process`'s to print, and printing it here too put the
        // same four lines on screen a second time.
        let Some(outcome) = self.get(task_id).map(|task| outcome_of(&task.status)) else {
            return;
        };

        let notification = format!(
            "<system_notification>Background task {task_id} finished: {outcome}. \
             Call manage_process with process_id: {task_id} for its result.</system_notification>"
        );

        // A `manage_process` that has already printed the whole report leaves
        // the wake nothing to say, so it is not sent at all. Otherwise it is
        // remembered by its text: a steer can carry more than one wake, and a
        // later `manage_process` must be able to find and withdraw only its
        // own without touching the others.
        {
            let Ok(mut tasks) = self.tasks.lock() else {
                return;
            };
            let Some(task) = tasks.get_mut(task_id) else {
                return;
            };
            if task.answered {
                return;
            }
            task.wake_text = Some(notification.clone());
        }

        self.told(task_id);
        if let Ok(lock) = self.notifier.lock() {
            if let Some(notifier) = lock.as_ref() {
                notifier.notify(notification);
            }
        }
    }

    /// Watch task `task_id` (process group `pid`, terminal `pty`): drain its
    /// output into `file`, `written` bytes of which are already there, wait for
    /// it to end, record how, and wake the session. The task is this
    /// process's child, waited for by pid ([`crate::platform::reap`]).
    pub fn spawn_monitor(
        self: &Arc<Self>,
        task_id: String,
        pid: i32,
        file: std::fs::File,
        pty: Arc<crate::platform::Pty>,
        written: usize,
        watch: Watch,
    ) {
        let manager = Arc::clone(self);
        let release = Arc::new(AtomicBool::new(false));
        let asked = Arc::clone(&release);
        let id = task_id.clone();
        let watching = watch.clone();
        let rearm = Arc::new(AtomicBool::new(false));
        let rearmed = Arc::clone(&rearm);
        let terminal = Arc::downgrade(&pty);
        // Registered before the thread starts, so typing into a task that has
        // just started finds its terminal.
        let Ok(mut monitors) = self.monitors.lock() else {
            return;
        };
        let thread = std::thread::spawn(move || {
            manager.watch(id, pid, file, pty, written, &watching, &asked, &rearmed)
        });
        monitors.insert(
            task_id,
            Monitor {
                release,
                thread,
                watch,
                rearm,
                pty: terminal,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn watch(
        self: &Arc<Self>,
        task_id: String,
        pid: i32,
        mut file: std::fs::File,
        pty: Arc<crate::platform::Pty>,
        mut written: usize,
        watch: &Watch,
        release: &AtomicBool,
        rearm: &AtomicBool,
    ) -> Ending {
        let mut buf = [0u8; 8192];
        let mut exited: Option<(Instant, i32)> = None;
        let mut capture_note = None;
        // Whether it waits for typed input is asked once per quiet spell, as
        // the foreground asks it; a task already known to wait (it moved
        // because it did) is not asked again until it prints or is typed into.
        let mut last_output = Instant::now();
        let mut input_asked = self
            .get(&task_id)
            .is_some_and(|task| task.waiting.is_some());
        let mut waits = input_asked;
        // The task's own terminal, read the way the foreground reads one: the
        // grid is as large as the terminal the process was given, and goes on
        // from the screen a build before handed over.
        let (rows, cols) = pty.size().unwrap_or((
            crate::tools::terminal::DEFAULT_ROWS,
            crate::tools::terminal::DEFAULT_COLS,
        ));
        let mut screen = match self.get(&task_id) {
            Some(BackgroundTaskInfo {
                screen: Some(shown),
                ..
            }) => Screen::resume(rows, cols, &shown),
            Some(task) => {
                let mut screen = Screen::log(rows, cols);
                screen.feed(&task.tail);
                screen
            }
            None => Screen::log(rows, cols),
        };
        let mut carry: Vec<u8> = Vec::new();
        loop {
            // Let go only of a task that still runs: one that has exited is
            // drained and reported here, by the monitor that saw it end.
            if exited.is_none() && release.load(Ordering::Acquire) {
                return Ending::Released { pty, file, written };
            }
            match pty.read_timeout(&mut buf, 200) {
                Ok(Some(0)) => break,
                Ok(Some(n)) => {
                    last_output = Instant::now();
                    input_asked = false;
                    if std::mem::take(&mut waits) {
                        self.clear_waiting(&task_id, false);
                    }
                    screen.feed(&crate::terminal_screen::decode_utf8(&mut carry, &buf[..n]));
                    if let Err(note) =
                        append_capped(&mut file, &screen.drain(), &mut written, watch)
                    {
                        capture_note = Some(note);
                        break;
                    }
                    self.set_screen(&task_id, screen.shown());
                }
                Ok(None) => {
                    if exited.is_none() {
                        if let Ok(Some(code)) = crate::platform::reap(pid, false) {
                            exited = Some((Instant::now(), code));
                        }
                    }
                    if exited.is_some_and(|(at, _)| at.elapsed() >= watch.drain()) {
                        break;
                    }
                    if rearm.swap(false, Ordering::AcqRel) {
                        // Typed into: the wait in force is answered. Whether
                        // it waits again is asked once it is quiet.
                        last_output = Instant::now();
                        input_asked = false;
                        if std::mem::take(&mut waits) {
                            self.clear_waiting(&task_id, true);
                        } else {
                            // Typed into while it printed: whatever it waits
                            // for next is a new question.
                            self.forget_wait(&task_id);
                        }
                    }
                    if exited.is_none()
                        && !input_asked
                        && last_output.elapsed() >= watch.input_wait_delay()
                    {
                        input_asked = true;
                        match pty.input_wait() {
                            Some(wait) => {
                                waits = true;
                                self.set_waiting(&task_id, wait);
                            }
                            None if std::mem::take(&mut waits) => {
                                self.clear_waiting(&task_id, true)
                            }
                            None => {}
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    capture_note = Some(format!(
                        "Output capture failed ({error}); process terminated."
                    ));
                    break;
                }
            }
        }
        // What the screen still showed is the log's last lines.
        if capture_note.is_none() {
            let mut last = screen.drain();
            let shown = screen.shown().text();
            if !shown.is_empty() {
                last.push_str(&shown);
                last.push('\n');
            }
            if let Err(note) = append_capped(&mut file, &last, &mut written, watch) {
                capture_note = Some(note);
            }
        }
        self.set_screen(&task_id, Shown::default());
        self.clear_waiting(&task_id, true);
        if let Some(note) = capture_note {
            if let Ok(mut tasks) = self.tasks.lock() {
                if let Some(task) = tasks.get_mut(&task_id) {
                    task.capture_note = Some(note);
                }
            }
            let tree = self.get(&task_id).and_then(|task| task.tree);
            crate::platform::terminate_tree(pid, tree.as_deref(), watch.cancel_grace());
        }

        // Reaped here, so the process never lingers as a zombie. An exit
        // status that cannot be read is not invented: the end is Lost.
        let exit_code = match exited {
            Some((_, code)) => Some(code),
            None => crate::platform::reap(pid, true).ok().flatten(),
        };
        let finished_at = crate::store::now_ms();
        let status = match exit_code {
            Some(exit_code) => BackgroundTaskStatus::Completed {
                exit_code,
                finished_at,
            },
            None => BackgroundTaskStatus::Lost {
                reason: LostReason::Unknown,
                finished_at,
            },
        };
        self.update_status(&task_id, status);
        self.notify_completed(&task_id);
        Ending::Finished
    }

    /// What the task's screen shows now: its text for a report, the screen
    /// itself for a build that takes the task over.
    fn set_screen(&self, task_id: &str, shown: Shown) {
        let tail = shown.text();
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.tail = tail;
                task.screen = (shown != Shown::default()).then_some(shown);
            }
        }
    }

    /// `manage_process` gave the model `task_id`'s output this far.
    pub fn mark_reported(&self, task_id: &str, reported: Reported) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(task_id) {
                task.reported = Some(reported);
            }
        }
    }

    /// Let go of every task for the program this process is about to `exec`
    /// into, and say what that program takes over: each running task with
    /// its terminal handed on, each ended one so it can still be reported.
    ///
    /// Every monitor stops at its next look (within one read window); a task
    /// that exits meanwhile is reported here first. All or nothing: when one
    /// terminal cannot be handed on, every monitor is started again, the
    /// tasks run on here as if nothing had happened, and the error says why.
    pub fn release(self: &Arc<Self>) -> crate::error::Result<Vec<HandedTask>> {
        let monitors: Vec<(String, Monitor)> = self
            .monitors
            .lock()
            .map(|mut monitors| monitors.drain().collect())
            .unwrap_or_default();
        for (_, monitor) in &monitors {
            monitor.release.store(true, Ordering::Release);
        }
        // What each monitor gave back: a task still running, its terminal,
        // its log and how it was watched.
        let mut released = Vec::new();
        for (id, monitor) in monitors {
            if let Ok(Ending::Released { pty, file, written }) = monitor.thread.join() {
                released.push((id, pty, file, written, monitor.watch));
            }
        }
        let mut failure = None;
        for (id, pty, _, _, _) in &released {
            let kept = if Arc::strong_count(pty) == 1 {
                pty.keep_across_exec(true).map_err(|e| e.to_string())
            } else {
                Err(format!("the terminal of {id} is still in use"))
            };
            if let Err(why) = kept {
                failure = Some(why);
                break;
            }
        }
        // Each supervisor let go of only once every terminal could be: its
        // command's calls then wait in the kernel for the next build.
        let mut supervisions = HashMap::new();
        if failure.is_none() {
            for (id, ..) in &released {
                let Some(pid) = self.get(id).map(|task| task.pid) else {
                    continue;
                };
                match crate::platform::release_supervision(pid, SUPERVISION_GRACE) {
                    Ok(Some(supervision)) => {
                        supervisions.insert(id.clone(), supervision);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                }
            }
        }
        if let Some(why) = failure {
            for (_, supervision) in supervisions {
                supervision.resume();
            }
            // Taken back: every terminal closes on exec again and is watched
            // as before.
            for (id, pty, file, written, watch) in released {
                let _ = pty.keep_across_exec(false);
                if let Some(task) = self.get(&id) {
                    self.spawn_monitor(id, task.pid, file, pty, written, watch);
                }
            }
            return Err(crate::error::Error::Internal(format!(
                "background tasks could not be handed over: {why}"
            )));
        }
        let mut terminals: HashMap<String, (crate::platform::Handed, usize)> = HashMap::new();
        for (id, pty, _file, written, _) in released {
            if let Ok(pty) = Arc::try_unwrap(pty) {
                terminals.insert(id, (pty.into_handed(), written));
            }
        }
        let handed: Vec<HandedTask> = self
            .list()
            .into_iter()
            .map(|info| {
                let (pty, written) = terminals
                    .remove(&info.task_id)
                    .map_or((None, 0), |(fd, written)| (Some(fd), written));
                // One that cannot be handed on closes at the exec, and the
                // kernel refuses what its command then asks (`ENOSYS`).
                let services = supervisions
                    .remove(&info.task_id)
                    .and_then(|supervision| supervision.into_handed().ok());
                HandedTask {
                    info,
                    pty,
                    written,
                    services,
                }
            })
            .collect();
        // A task handed on is the successor's from here: this manager neither
        // watches nor stops it — not on `stop_all`, not when its session is
        // dropped on the way to the exec.
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in &handed {
                if task.pty.is_some() {
                    tasks.remove(&task.info.task_id);
                }
            }
        }
        Ok(handed)
    }

    /// Take over what the previous build handed on: every task is listed
    /// again, and every running one watched again, its log continued, its
    /// connections supervised again as `answers` says. A terminal that cannot
    /// be taken over leaves a process nobody could see or stop, so that
    /// process is stopped, and the error says which.
    pub fn adopt(
        self: &Arc<Self>,
        tasks: Vec<HandedTask>,
        watch: Watch,
        answers: crate::platform::SupervisedAs<'_>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        for task in tasks {
            let id = task.info.task_id.clone();
            let pid = task.info.pid;
            let out_file = task.info.out_file.clone();
            if let Some(handed) = task.services {
                let tree = task.info.tree.as_deref();
                if let Err(error) = crate::platform::adopt_supervision(handed, pid, tree, answers) {
                    errors.push(format!(
                        "the connections of background task {id} could not be supervised again \
                         and are refused: {error}"
                    ));
                }
            }
            self.register(task.info);
            let Some(handed) = task.pty else {
                continue;
            };
            let taken = crate::platform::Pty::adopt(handed).and_then(|pty| {
                let file = std::fs::OpenOptions::new().append(true).open(&out_file)?;
                Ok((pty, file))
            });
            match taken {
                Ok((pty, file)) => {
                    self.spawn_monitor(id, pid, file, Arc::new(pty), task.written, watch.clone());
                }
                Err(error) => {
                    // Stopped, and reaped off this path: taking over runs as a
                    // host starts, which must not wait on a process ending.
                    let tree = self.get(&id).and_then(|task| task.tree);
                    crate::platform::terminate_tree(pid, tree.as_deref(), watch.cancel_grace());
                    std::thread::spawn(move || crate::platform::reap(pid, true));
                    self.update_status(
                        &id,
                        BackgroundTaskStatus::Killed {
                            finished_at: crate::store::now_ms(),
                        },
                    );
                    errors.push(format!(
                        "background task {id} could not be taken over and was stopped: {error}"
                    ));
                }
            }
        }
        errors
    }
}

/// How a task stands, in the words its wake, the task list and
/// `manage_process` use — one word each, capitalised, the same everywhere.
pub fn outcome_of(status: &BackgroundTaskStatus) -> String {
    match status {
        BackgroundTaskStatus::Running => "Running".to_string(),
        BackgroundTaskStatus::Completed { exit_code: 0, .. } => "Completed".to_string(),
        BackgroundTaskStatus::Completed { exit_code, .. } => {
            format!("Failed (exit code {exit_code})")
        }
        BackgroundTaskStatus::Killed { .. } => "Killed".to_string(),
        BackgroundTaskStatus::Lost { .. } => "Lost".to_string(),
    }
}

/// How `task` stands now: its status, or `Waiting for input` while it runs
/// blocked reading its terminal.
pub fn state_of(task: &BackgroundTaskInfo) -> String {
    match (&task.status, task.waiting) {
        (BackgroundTaskStatus::Running, Some(_)) => "Waiting for input".to_string(),
        (status, _) => outcome_of(status),
    }
}

/// Why a task whose end was never seen is lost, from what its start
/// recorded: another boot took the process with it; in this boot Rune
/// stopped watching it, and a process of it still alive is stopped, since
/// nothing could see or stop it any more (as `adopt` stops one it cannot
/// take over). Its number is only trusted with its start: a process that got
/// the number since is never signalled.
fn lost_reason(pid: i32, identity: Option<Identity>, tree: Option<String>) -> LostReason {
    let Some(identity) = identity else {
        return LostReason::Unknown;
    };
    let boot = crate::platform::boot_time()
        .and_then(|boot| boot.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|boot| boot.as_secs());
    match boot {
        // The kernel's boot time can drift by a second between reads.
        Some(boot) if boot.abs_diff(identity.boot) <= 1 => {
            // The group only when its leader is the very process the task
            // started; the tree's mark is its own, whatever pid it found.
            let leader = if Identity::of(pid).is_some_and(|now| now.started == identity.started) {
                pid
            } else {
                0
            };
            if leader > 0 || tree.is_some() {
                std::thread::spawn(move || {
                    crate::platform::terminate_tree(leader, tree.as_deref(), 200)
                });
            }
            LostReason::RuneStopped
        }
        Some(_) => LostReason::Restart,
        None => LostReason::Unknown,
    }
}

/// Append `text` to a task's log, `written` bytes of which are already there,
/// up to the log's limit; past it, the reason the process is stopped.
fn append_capped(
    file: &mut std::fs::File,
    text: &str,
    written: &mut usize,
    watch: &Watch,
) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    let max_bytes = watch.max_bytes();
    let room = max_bytes.saturating_sub(*written);
    let mut allowed = text.len().min(room);
    while !text.is_char_boundary(allowed) {
        allowed -= 1;
    }
    if let Err(error) = file
        .write_all(&text.as_bytes()[..allowed])
        .and_then(|_| file.flush())
    {
        return Err(format!(
            "Output capture failed ({error}); process terminated."
        ));
    }
    *written += allowed;
    if allowed < text.len() {
        return Err(format!(
            "Output capture exceeded {} bytes; process terminated. Log contains the captured prefix only.",
            max_bytes
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// The POSIX shell these tests run their scripts in (Git Bash on
    /// Windows); `None`, said once, where the machine has none — a test
    /// that needs one has nothing to run then.
    fn sh() -> Option<PathBuf> {
        let shell = crate::platform::fixture::posix_shell();
        if shell.is_none() {
            eprintln!("note: no POSIX shell on this machine; a background task test is skipped");
        }
        shell
    }

    #[test]
    fn failed_capture_terminates_and_reaps_the_real_background_process() {
        let Some(sh) = sh() else {
            return;
        };
        let manager = Arc::new(finished("capture-failure"));
        // More lines than the screen holds: a line reaches the log once it
        // has scrolled off, and that is the write that fails.
        let (pty, pid) = on_terminal(&sh, "seq 1 100; sleep 30");
        manager.spawn_monitor(
            "capture-failure".into(),
            pid,
            crate::platform::fixture::failing_file().unwrap(),
            Arc::new(pty),
            0,
            Watch::fixed(1024, 20, Duration::from_millis(20)),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while crate::platform::group_alive(pid) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !crate::platform::group_alive(pid),
            "capture failure left a running process"
        );
        let task = manager.get("capture-failure").unwrap();
        assert!(task.capture_note.unwrap().contains("capture failed"));
    }

    /// `script` run by `shell` on a terminal of its own, as `terminal_use`
    /// starts a command: the terminal and the pid it is watched by.
    fn on_terminal(shell: &std::path::Path, script: &str) -> (crate::platform::Pty, i32) {
        let (pty, slave) = crate::platform::open_pty(24, 80).unwrap();
        let child = crate::platform::spawn_on_terminal(
            &crate::platform::Launch::new(shell).args(["-c", script]),
            &slave,
            crate::platform::Placement::default(),
        )
        .unwrap();
        let pid = child.into_pid();
        drop(slave);
        (pty, pid)
    }

    /// A real task on a real terminal, watched by `manager`: a shell running
    /// `script`, its log in a fresh file under `dir`.
    fn launch(
        manager: &Arc<BackgroundTaskManager>,
        dir: &std::path::Path,
        id: &str,
        script: &str,
    ) -> (i32, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.join(format!("{id}.txt"));
        let file = std::fs::File::create(&log).unwrap();
        let shell = sh().expect("a test that launches a task checks for a shell first");
        let (pty, pid) = on_terminal(&shell, script);
        manager.register(BackgroundTaskInfo {
            task_id: id.into(),
            pid,
            command: script.into(),
            out_file: log.clone(),
            started_at: 0,
            status: BackgroundTaskStatus::Running,
            surfaced: false,
            answered: false,
            wake_text: None,
            capture_note: None,
            tail: String::new(),
            screen: None,
            reported: None,
            waiting: None,
            tree: None,
        });
        manager.spawn_monitor(id.into(), pid, file, Arc::new(pty), 0, watch());
        (pid, log)
    }

    fn watch() -> Watch {
        Watch::fixed(1 << 20, 50, Duration::from_millis(50))
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("bg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn state_of(pid: i32) -> Option<crate::platform::fixture::ProcessState> {
        crate::platform::fixture::process_state(pid)
    }

    use crate::platform::fixture::ProcessState::Zombie;

    /// What a live upgrade does, in one process: the task is let go of with
    /// its terminal kept open, taken over by another manager (the next
    /// build's), and runs on untouched — its log continuing where it was.
    #[test]
    fn a_task_handed_over_runs_on_and_its_log_continues() {
        // A task is handed to a newer build only across an exec, which
        // Windows has not (a newer build is taken at the next start).
        if sh().is_none() || !crate::platform::REPLACES_IN_PLACE {
            return;
        }
        let dir = scratch("handover");
        let before = Arc::new(BackgroundTaskManager::new());
        let (pid, log) = launch(
            &before,
            &dir,
            "bg_tick",
            "while true; do echo tick; sleep 0.02; done",
        );
        until("first output", || {
            std::fs::metadata(&log).unwrap().len() > 0
        });

        let handed = before.release().unwrap();
        assert_eq!(handed.len(), 1);
        let task = handed[0].clone();
        let terminal = task.pty.expect("a running task hands on its terminal");
        assert!(
            crate::platform::fixture::kept_across_exec(terminal),
            "the terminal stays open across exec"
        );
        assert!(task.written > 0);
        assert_eq!(
            state_of(pid).map(|s| s != Zombie),
            Some(true),
            "it never stopped"
        );

        // The old manager lets the task be: stopping everything it still
        // holds — as dropping its session does — leaves the task running.
        before.stop_all();
        drop(before);
        assert_eq!(
            state_of(pid).map(|s| s != Zombie),
            Some(true),
            "stop_all reached it"
        );

        let after = Arc::new(BackgroundTaskManager::new());
        assert!(after
            .adopt(handed, watch(), crate::platform::SupervisedAs::Orphaned)
            .is_empty());
        assert!(
            !crate::platform::fixture::kept_across_exec(terminal),
            "closes on exec again once taken over"
        );
        let size = std::fs::metadata(&log).unwrap().len();
        until("the log to go on", || {
            std::fs::metadata(&log).unwrap().len() > size
        });
        assert_eq!(
            after.get("bg_tick").unwrap().status,
            BackgroundTaskStatus::Running
        );

        assert!(after.stop("bg_tick", true));
        until("the task to be reaped", || state_of(pid).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A task that ends while nobody watches it — between the old build
    /// letting go and the new one taking over — is reported by the one that
    /// takes over, with its real exit code, once, and leaves no zombie.
    #[test]
    fn a_task_that_ends_while_handed_over_is_reported_by_the_successor() {
        // A task is handed to a newer build only across an exec, which
        // Windows has not (a newer build is taken at the next start).
        if sh().is_none() || !crate::platform::REPLACES_IN_PLACE {
            return;
        }
        let dir = scratch("ended");
        let before = Arc::new(BackgroundTaskManager::new());
        let (pid, log) = launch(&before, &dir, "bg_end", "sleep 0.2; echo bye; exit 7");
        let handed = before.release().unwrap();
        let ended = Instant::now() + Duration::from_secs(5);
        while state_of(pid) != Some(Zombie) {
            assert!(
                Instant::now() < ended,
                "timed out waiting for it to end unwatched: {:?}\n{}",
                state_of(pid),
                std::process::Command::new("ps")
                    .args(["-o", "pid,stat,wchan,command", "-p", &pid.to_string()])
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                    .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let after = Arc::new(BackgroundTaskManager::new());
        let notifier = Arc::new(RecordingNotifier::default());
        after.set_notifier(notifier.clone());
        assert!(after
            .adopt(handed, watch(), crate::platform::SupervisedAs::Orphaned)
            .is_empty());
        until("it to be reported", || {
            matches!(
                after.get("bg_end").unwrap().status,
                BackgroundTaskStatus::Completed { exit_code: 7, .. }
            )
        });
        until("the wake", || notifier.0.lock().unwrap().len() == 1);
        assert!(notifier.text(0).contains("exit code 7"));
        assert!(std::fs::read_to_string(&log).unwrap().contains("bye"));
        until("no zombie", || state_of(pid).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A number that is not a terminal master is never adopted as one, and
    /// the process behind it — which nothing could watch or stop — is stopped.
    #[test]
    fn a_terminal_that_cannot_be_taken_over_stops_its_task() {
        let dir = scratch("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        let Some(sh) = sh() else {
            return;
        };
        let pid = crate::platform::spawn_detached(
            &crate::platform::Launch::new(&sh).args(["-c", "sleep 30"]),
            std::fs::File::create(dir.join("log")).unwrap(),
        )
        .unwrap()
        .into_pid();
        let file = std::fs::File::create(dir.join("x")).unwrap();
        let not_a_terminal = crate::platform::fixture::handed_number(&file);
        let after = Arc::new(BackgroundTaskManager::new());
        let errors = after.adopt(
            vec![HandedTask {
                info: BackgroundTaskInfo {
                    task_id: "bg_orphan".into(),
                    pid,
                    command: "sleep 30".into(),
                    out_file: dir.join("bg_orphan.txt"),
                    started_at: 0,
                    status: BackgroundTaskStatus::Running,
                    surfaced: false,
                    answered: false,
                    wake_text: None,
                    capture_note: None,
                    tail: String::new(),
                    screen: None,
                    reported: None,
                    waiting: None,
                    tree: None,
                },
                pty: Some(not_a_terminal),
                written: 0,
                services: None,
            }],
            watch(),
            crate::platform::SupervisedAs::Orphaned,
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(matches!(
            after.get("bg_orphan").unwrap().status,
            BackgroundTaskStatus::Killed { .. }
        ));
        until("the task to be stopped and reaped", || {
            state_of(pid).is_none()
        });
        assert!(!crate::platform::group_alive(pid));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// All or nothing: a terminal that cannot be handed on (here: still held
    /// elsewhere) leaves every task watched here as before, nothing let go.
    #[test]
    fn a_hand_over_that_cannot_finish_leaves_every_task_watched() {
        if sh().is_none() {
            return;
        }
        let dir = scratch("partial");
        let manager = Arc::new(BackgroundTaskManager::new());
        let (pid, log) = launch(
            &manager,
            &dir,
            "bg_a",
            "while true; do echo a; sleep 0.02; done",
        );
        // A second task whose terminal is still held by somebody else.
        let (pty, _slave) = crate::platform::open_pty(24, 80).unwrap();
        let held = Arc::new(pty);
        manager.register(BackgroundTaskInfo {
            task_id: "bg_b".into(),
            pid: 0,
            command: "held".into(),
            out_file: dir.join("bg_b.txt"),
            started_at: 0,
            status: BackgroundTaskStatus::Running,
            surfaced: false,
            answered: false,
            wake_text: None,
            capture_note: None,
            tail: String::new(),
            screen: None,
            reported: None,
            waiting: None,
            tree: None,
        });
        manager.spawn_monitor(
            "bg_b".into(),
            i32::MAX,
            std::fs::File::create(dir.join("bg_b.txt")).unwrap(),
            Arc::clone(&held),
            0,
            watch(),
        );
        assert!(manager.release().is_err());
        let size = std::fs::metadata(&log).unwrap().len();
        until("bg_a to be watched again", || {
            std::fs::metadata(&log).unwrap().len() > size
        });
        assert!(manager.stop("bg_a", true));
        until("bg_a to be reaped", || state_of(pid).is_none());
        drop(held);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Counts wakes instead of sending them, so a test can ask how many times
    /// the model would have been interrupted by one completion.
    struct CountingNotifier(AtomicUsize);

    impl SessionWakeNotifier for CountingNotifier {
        fn notify(&self, _text: String) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Keeps the wakes it is given, so a test can look at the exact text a
    /// steer would have carried.
    #[derive(Default)]
    struct RecordingNotifier(Mutex<Vec<String>>);

    impl RecordingNotifier {
        fn text(&self, at: usize) -> String {
            self.0.lock().expect("notifier lock")[at].clone()
        }
    }

    impl SessionWakeNotifier for RecordingNotifier {
        fn notify(&self, text: String) {
            self.0.lock().expect("notifier lock").push(text);
        }
    }

    /// A journal kept in memory.
    #[derive(Default)]
    struct MemoryJournal(Mutex<Vec<BackgroundRecord>>);

    impl BackgroundJournal for MemoryJournal {
        fn append(&self, record: &BackgroundRecord) {
            self.0.lock().unwrap().push(record.clone());
        }
        fn read(&self) -> Vec<BackgroundRecord> {
            self.0.lock().unwrap().clone()
        }
    }

    fn started(id: &str) -> BackgroundRecord {
        BackgroundRecord::Started {
            task_id: id.into(),
            pid: 77,
            command: "cargo test".into(),
            out_file: PathBuf::from(format!("/x/{id}.txt")),
            started_at: 1,
            identity: None,
            tree: None,
        }
    }

    /// The promise of a wake survives the program that made it: opened again,
    /// a session tells the model of every task it started and never heard the
    /// end of — lost when nobody saw it end, with its end when one was seen —
    /// and of nothing it was told, nothing still held here, and nothing twice.
    #[test]
    fn an_opened_session_tells_of_every_task_whose_end_it_never_told() {
        let journal = Arc::new(MemoryJournal::default());
        for record in [
            started("bg_lost"),
            started("bg_ended"),
            BackgroundRecord::Ended {
                task_id: "bg_ended".into(),
                status: BackgroundTaskStatus::Completed {
                    exit_code: 0,
                    finished_at: 5,
                },
            },
            started("bg_told"),
            BackgroundRecord::Told {
                task_id: "bg_told".into(),
            },
            started("bg_held"),
        ] {
            journal.append(&record);
        }
        let manager = finished("bg_held");
        manager.set_journal(journal.clone());
        let notifier = Arc::new(RecordingNotifier::default());
        manager.set_notifier(notifier.clone());

        manager.settle_unreported();
        let texts = notifier.0.lock().unwrap().clone();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(
            texts[0].contains("bg_lost was lost") && texts[0].contains("unknown"),
            "{}",
            texts[0]
        );
        assert!(
            texts[1].contains("bg_ended ended while this session was not open: Completed"),
            "{}",
            texts[1]
        );
        // The one whose end was seen is listed again, so its report is whole.
        let ended = manager.get("bg_ended").expect("listed again");
        assert!(matches!(
            ended.status,
            BackgroundTaskStatus::Completed { exit_code: 0, .. }
        ));
        // Lost is listed too, with the reason its start allows: a record with
        // no identity cannot say which, so Unknown.
        assert!(matches!(
            manager.get("bg_lost").unwrap().status,
            BackgroundTaskStatus::Lost {
                reason: LostReason::Unknown,
                ..
            }
        ));
        // What the model was told of is still listed, as it ended.
        assert!(manager.get("bg_told").is_some());
        for id in ["bg_lost", "bg_ended"] {
            assert!(journal
                .read()
                .contains(&BackgroundRecord::Told { task_id: id.into() }));
        }

        manager.settle_unreported();
        assert_eq!(notifier.0.lock().unwrap().len(), 2, "once per opening");
        // Opened again by a program that does not hold `bg_held` either: that
        // one is lost now; what was told is never told again.
        let reopened = BackgroundTaskManager::new();
        reopened.set_journal(journal.clone());
        reopened.set_notifier(notifier.clone());
        reopened.settle_unreported();
        let texts = notifier.0.lock().unwrap().clone();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts[2].contains("bg_held was lost"), "{}", texts[2]);
    }

    /// What the journal learns as a task lives: its start, its end once (not
    /// again when a kill's own exit follows), and that the model was told.
    #[test]
    fn the_journal_keeps_a_start_one_end_and_the_telling() {
        let journal = Arc::new(MemoryJournal::default());
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_journal(journal.clone());
        manager.set_notifier(Arc::new(RecordingNotifier::default()));
        let mut info = finished("x").get("x").unwrap();
        info.task_id = "bg_a".into();
        info.status = BackgroundTaskStatus::Running;
        // No process group has this number: the kill below reaches nobody.
        info.pid = i32::MAX;
        manager.register_started(info);
        assert!(manager.stop("bg_a", true));
        manager.update_status(
            "bg_a",
            BackgroundTaskStatus::Completed {
                exit_code: 143,
                finished_at: 9,
            },
        );
        manager.notify_completed("bg_a");
        let records = journal.read();
        assert!(matches!(records[0], BackgroundRecord::Started { .. }));
        let ends: Vec<_> = records
            .iter()
            .filter(|r| matches!(r, BackgroundRecord::Ended { .. }))
            .collect();
        assert_eq!(ends.len(), 1, "{records:?}");
        assert!(matches!(
            ends[0],
            BackgroundRecord::Ended {
                status: BackgroundTaskStatus::Killed { .. },
                ..
            }
        ));
        assert_eq!(
            records.last(),
            Some(&BackgroundRecord::Told {
                task_id: "bg_a".into()
            })
        );
    }

    fn started_as(id: &str, pid: i32, identity: Option<Identity>) -> BackgroundRecord {
        BackgroundRecord::Started {
            task_id: id.into(),
            pid,
            command: "cargo build".into(),
            out_file: PathBuf::from(format!("/x/{id}.txt")),
            started_at: 1,
            identity,
            tree: None,
        }
    }

    fn reopened(journal: &Arc<MemoryJournal>) -> (BackgroundTaskManager, Arc<RecordingNotifier>) {
        let manager = BackgroundTaskManager::new();
        manager.set_journal(journal.clone());
        let notifier = Arc::new(RecordingNotifier::default());
        manager.set_notifier(notifier.clone());
        manager.settle_unreported();
        (manager, notifier)
    }

    /// The bug of 2026-10-11: a task that Completed (exit 0) and whose end
    /// the model was told was forgotten when the session was opened again —
    /// after a restart, say — and `manage_process` then called it failed with
    /// PID unknown. Its end is in the journal, and it stays that end.
    #[test]
    fn a_task_that_ended_before_a_restart_keeps_its_end_and_is_not_told_again() {
        let journal = Arc::new(MemoryJournal::default());
        for record in [
            started_as("bg_ok", 77, None),
            BackgroundRecord::Ended {
                task_id: "bg_ok".into(),
                status: BackgroundTaskStatus::Completed {
                    exit_code: 0,
                    finished_at: 2,
                },
            },
            BackgroundRecord::Told {
                task_id: "bg_ok".into(),
            },
            started_as("bg_bad", 78, None),
            BackgroundRecord::Ended {
                task_id: "bg_bad".into(),
                status: BackgroundTaskStatus::Completed {
                    exit_code: 3,
                    finished_at: 2,
                },
            },
            BackgroundRecord::Told {
                task_id: "bg_bad".into(),
            },
        ] {
            journal.append(&record);
        }
        for _ in 0..2 {
            let (manager, notifier) = reopened(&journal);
            let ok = manager.get("bg_ok").expect("still listed after reopening");
            assert_eq!(super::state_of(&ok), "Completed");
            assert_eq!(ok.pid, 77);
            assert!(ok.answered, "its end was told already");
            assert_eq!(
                super::state_of(&manager.get("bg_bad").unwrap()),
                "Failed (exit code 3)"
            );
            assert_eq!(manager.list().len(), 2);
            assert!(notifier.0.lock().unwrap().is_empty(), "nothing told twice");
        }
    }

    /// A task still running when its watcher went away is Lost, with the
    /// reason its identity gives: another boot is a restart; this boot is
    /// Rune stopping — and a process of it still alive, the very one its
    /// start names, is stopped. Lost is recorded once: every later opening
    /// reads the same end, and a process that got the number since is never
    /// touched.
    #[test]
    fn a_task_lost_mid_run_says_why_and_its_orphan_is_stopped() {
        let Some(sh) = sh() else {
            return;
        };
        let log = std::env::temp_dir().join(format!("rune-lost-{}.log", std::process::id()));
        let child = crate::platform::spawn_detached(
            &crate::platform::Launch::new(&sh).args(["-c", "sleep 30"]),
            std::fs::File::create(&log).unwrap(),
        )
        .unwrap();
        let _ = std::fs::remove_file(&log);
        let pid = child.id();
        let alive = Identity::of(pid).expect("identity of a live process");
        let journal = Arc::new(MemoryJournal::default());
        journal.append(&started_as(
            "bg_restart",
            4242,
            Some(Identity {
                boot: 1,
                started: 9,
            }),
        ));
        journal.append(&started_as("bg_orphan", pid, Some(alive)));
        // Same boot, same number, another start: a stranger that got the pid.
        let stranger = std::process::Command::new(&sh)
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let stranger_pid = stranger.id() as i32;
        journal.append(&started_as(
            "bg_reused",
            stranger_pid,
            Some(Identity {
                boot: alive.boot,
                started: alive.started.wrapping_sub(1),
            }),
        ));

        let (manager, notifier) = reopened(&journal);
        let reason = |id: &str| match manager.get(id).unwrap().status {
            BackgroundTaskStatus::Lost { reason, .. } => reason,
            other => panic!("{id} is {other:?}"),
        };
        assert_eq!(reason("bg_restart"), LostReason::Restart);
        assert_eq!(reason("bg_orphan"), LostReason::RuneStopped);
        assert_eq!(reason("bg_reused"), LostReason::RuneStopped);
        let texts = notifier.0.lock().unwrap().clone();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts
            .iter()
            .any(|t| t.contains("bg_restart was lost: the computer restarted")));

        let mut child = child;
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.is_some(), "the orphan its start names was stopped");
        let mut stranger = stranger;
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "a process that only shares the number is never signalled"
        );
        let _ = stranger.kill();
        let _ = stranger.wait();

        // Recorded once; the next opening reads it and tells nothing.
        let ends = journal
            .read()
            .iter()
            .filter(|r| matches!(r, BackgroundRecord::Ended { .. }))
            .count();
        assert_eq!(ends, 3);
        let (again, notifier) = reopened(&journal);
        assert_eq!(super::state_of(&again.get("bg_restart").unwrap()), "Lost");
        assert!(notifier.0.lock().unwrap().is_empty());
        assert_eq!(
            journal
                .read()
                .iter()
                .filter(|r| matches!(r, BackgroundRecord::Ended { .. }))
                .count(),
            3,
            "Lost is recorded once"
        );
    }

    /// Every surface says a task's state in the same capitalised words.
    #[test]
    fn a_state_is_one_capitalised_word_everywhere() {
        let mut task = finished("bg_w").get("bg_w").unwrap();
        task.status = BackgroundTaskStatus::Running;
        assert_eq!(super::state_of(&task), "Running");
        task.waiting = Some(crate::platform::InputWait {
            hidden: false,
            line: true,
        });
        assert_eq!(super::state_of(&task), "Waiting for input");
        task.status = BackgroundTaskStatus::Killed { finished_at: 1 };
        assert_eq!(
            super::state_of(&task),
            "Killed",
            "only a running task waits"
        );
        task.status = BackgroundTaskStatus::Completed {
            exit_code: 0,
            finished_at: 1,
        };
        assert_eq!(super::state_of(&task), "Completed");
        task.status = BackgroundTaskStatus::Lost {
            reason: LostReason::Restart,
            finished_at: 1,
        };
        assert_eq!(super::state_of(&task), "Lost");
    }

    /// A suspended session (its last window gone) holds a wake without
    /// touching the task; the window back hears it, once.
    #[test]
    fn a_wake_held_while_suspended_is_told_once_when_watched_again() {
        let manager = finished("bg_quiet");
        let wakes = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        manager.set_notifier(wakes.clone());
        manager.suspend();
        announce(&manager, "bg_quiet");
        manager.deliver_pending();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0, "nobody watches yet");
        manager.resume();
        manager.deliver_pending();
        manager.deliver_pending();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    }

    /// Suspending leaves a running task running; only stopping ends it.
    #[test]
    fn suspending_never_stops_a_task() {
        if sh().is_none() {
            return;
        }
        let dir = scratch("suspend");
        let manager = Arc::new(BackgroundTaskManager::new());
        let (pid, _log) = launch(&manager, &dir, "bg_run", "sleep 30");
        manager.suspend();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(state_of(pid).map(|s| s != Zombie), Some(true));
        manager.stop_all();
        until("the task to end", || state_of(pid).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A background log has its own limit, on disk, apart from the
    /// foreground's in-memory one — 100 MiB by default — and a change to it
    /// reaches a task already watched (hot reload, never a snapshot).
    #[test]
    fn the_background_log_limit_is_its_own_and_read_live() {
        use crate::settings::configuration::{Configuration, Shared};
        let shared = Shared::new(Configuration::default());
        let watch = Watch::of(&shared);
        assert_eq!(watch.max_bytes(), 100 * 1024 * 1024);
        assert_ne!(
            watch.max_bytes() as u64,
            shared.number("terminal.max_output_size"),
            "not the foreground's limit"
        );
        let mut values = Configuration::default().values().clone();
        values["terminal"]["max_background_log_size"] = serde_json::json!("1KiB");
        values["terminal"]["cancel_grace"] = serde_json::json!("1s");
        shared.store(Configuration::from_values(values).unwrap());
        assert_eq!(
            watch.max_bytes(),
            1024,
            "the watch already running reads it"
        );
        assert_eq!(watch.cancel_grace(), 1000);
    }

    /// A wait that only printed meanwhile (a countdown) and waits again is
    /// the same wait: told once. Typed into, the next wait is a new one.
    #[test]
    fn a_wait_is_told_once_until_it_is_answered() {
        let manager = running("bg_ask");
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        let wait = crate::platform::InputWait {
            hidden: false,
            line: true,
        };
        manager.set_waiting("bg_ask", wait);
        manager.clear_waiting("bg_ask", false);
        manager.set_waiting("bg_ask", wait);
        assert_eq!(queue.sent(), 1, "a countdown is not a new question");
        assert_eq!(
            super::state_of(&manager.get("bg_ask").unwrap()),
            "Waiting for input"
        );
        manager.clear_waiting("bg_ask", true);
        assert!(
            queue.unread().is_empty(),
            "a wake about a wait that is over is taken back"
        );
        manager.set_waiting("bg_ask", wait);
        assert_eq!(queue.sent(), 2, "answered, the next wait is new");
        assert!(queue.unread()[0].contains("bg_ask is waiting for input"));
    }

    /// The bug behind "is terminal_send_input woken?": every wait of one task
    /// is told in the very same words, and the words of a wait that was over
    /// used to be kept to strip later steers by — so the next wait's wake,
    /// word for word the same, was stripped too and the model never heard it.
    /// Read, answered, and asked again: the second wake reaches the model.
    #[test]
    fn the_next_wait_after_an_answer_is_told_though_its_words_are_the_same() {
        let manager = running("bg_twice");
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        let wait = crate::platform::InputWait {
            hidden: true,
            line: true,
        };
        manager.set_waiting("bg_twice", wait);
        let first = queue.read_all();
        assert_eq!(first.len(), 1);
        assert!(
            first[0].contains("Its input is hidden (what is typed is not shown)."),
            "{}",
            first[0]
        );
        assert!(
            first[0].contains("Call manage_process with process_id: bg_twice for what it asks"),
            "the wake says where the question is read: {}",
            first[0]
        );
        // Typed into: that wait is over, then the process asks again.
        manager.clear_waiting("bg_twice", true);
        manager.set_waiting("bg_twice", wait);
        let second = queue.unread();
        assert_eq!(second, first, "the same words, a new wait");
        assert_eq!(
            manager.without_answered_wakes(&second[0]),
            second[0],
            "and nothing strips it on its way to the model"
        );
    }

    /// A report that says the task waits is the telling: a wake about that
    /// wait still unread is taken back, so the model does not hear it twice.
    #[test]
    fn a_report_of_a_wait_takes_its_unread_wake_back() {
        let manager = running("bg_seen");
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        let wait = crate::platform::InputWait {
            hidden: false,
            line: true,
        };
        manager.set_waiting("bg_seen", wait);
        assert_eq!(queue.unread().len(), 1);
        manager.wait_told("bg_seen");
        assert!(queue.unread().is_empty());
        manager.clear_waiting("bg_seen", false);
        manager.set_waiting("bg_seen", wait);
        assert!(queue.unread().is_empty(), "the same wait is never retold");
    }

    #[test]
    fn a_task_id_is_never_a_path() {
        assert!(is_task_id("bg_dRHEv1-r"));
        for bad in ["bg_", "../bg_x", "bg_../x", "bg_a/b", "x_abc", "bg_a.txt"] {
            assert!(!is_task_id(bad), "{bad}");
        }
    }

    fn finished(id: &str) -> BackgroundTaskManager {
        let manager = BackgroundTaskManager::new();
        manager.register(BackgroundTaskInfo {
            task_id: id.to_string(),
            pid: 4242,
            command: "true".into(),
            out_file: PathBuf::from("/tmp/rune-test-bg.log"),
            started_at: 0,
            status: BackgroundTaskStatus::Running,
            surfaced: false,
            answered: false,
            wake_text: None,
            capture_note: None,
            tail: String::new(),
            screen: None,
            reported: None,
            waiting: None,
            tree: None,
        });
        manager.update_status(
            id,
            BackgroundTaskStatus::Completed {
                exit_code: 1,
                finished_at: 0,
            },
        );
        manager
    }

    fn announce(manager: &BackgroundTaskManager, id: &str) {
        manager.notify_completed(id);
    }

    /// A manager holding one task, still running, whose pid reaches nobody.
    fn running(id: &str) -> BackgroundTaskManager {
        let manager = BackgroundTaskManager::new();
        manager.register(BackgroundTaskInfo {
            status: BackgroundTaskStatus::Running,
            pid: i32::MAX,
            ..finished(id).get(id).unwrap()
        });
        manager
    }

    /// The session's queue as the notifier sees it: a wake is queued until
    /// it is read, and one taken back before then never reaches the model.
    #[derive(Default)]
    struct QueueNotifier {
        queue: Mutex<Vec<String>>,
        sent: AtomicUsize,
    }

    impl QueueNotifier {
        fn sent(&self) -> usize {
            self.sent.load(Ordering::Relaxed)
        }
        fn unread(&self) -> Vec<String> {
            self.queue.lock().unwrap().clone()
        }
        /// The work in progress reads every queued wake.
        fn read_all(&self) -> Vec<String> {
            std::mem::take(&mut *self.queue.lock().unwrap())
        }
    }

    impl SessionWakeNotifier for QueueNotifier {
        fn notify(&self, text: String) {
            self.sent.fetch_add(1, Ordering::Relaxed);
            self.queue.lock().unwrap().push(text);
        }
        fn withdraw(&self, text: &str) {
            let mut queue = self.queue.lock().unwrap();
            if let Some(at) = queue.iter().position(|queued| queued == text) {
                queue.remove(at);
            }
        }
    }

    /// The bug of 2026-10-11: `manage_process` killed a task, and a moment
    /// later the model was woken with "finished: Killed" — the monitor saw
    /// the process go before the kill's result marked its end told. A stop
    /// is told by the call that stops it, in the same step: no wake, ever,
    /// however the monitor's own end lands; and a wait it had is taken back.
    #[test]
    fn a_task_stopped_by_a_call_is_never_woken_for() {
        let journal = Arc::new(MemoryJournal::default());
        let manager = running("bg_kill");
        manager.set_journal(journal.clone());
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        manager.set_waiting(
            "bg_kill",
            crate::platform::InputWait {
                hidden: false,
                line: true,
            },
        );
        assert_eq!(queue.unread().len(), 1);

        assert!(manager.stop("bg_kill", true));
        assert!(queue.unread().is_empty(), "the wait's wake is taken back");
        assert_eq!(super::state_of(&manager.get("bg_kill").unwrap()), "Killed");
        // What the monitor does when it sees the process go.
        manager.clear_waiting("bg_kill", true);
        manager.update_status(
            "bg_kill",
            BackgroundTaskStatus::Completed {
                exit_code: 143,
                finished_at: 2,
            },
        );
        manager.notify_completed("bg_kill");
        manager.deliver_pending();
        assert!(queue.unread().is_empty(), "{:?}", queue.unread());
        assert_eq!(queue.sent(), 1, "only the wait was ever told");
        assert!(matches!(
            manager.get("bg_kill").unwrap().status,
            BackgroundTaskStatus::Killed { .. }
        ));
        assert!(journal.read().contains(&BackgroundRecord::Told {
            task_id: "bg_kill".into()
        }));
        assert!(
            !manager.stop("bg_kill", true),
            "an ended task is not stopped again"
        );
    }

    /// The first end is the end: an exit status that could not be read after
    /// a kill does not turn Killed into Lost, nor Completed into anything.
    #[test]
    fn the_first_end_is_the_end() {
        let manager = running("bg_end");
        assert!(manager.stop("bg_end", true));
        manager.update_status(
            "bg_end",
            BackgroundTaskStatus::Lost {
                reason: LostReason::Unknown,
                finished_at: 3,
            },
        );
        assert_eq!(super::state_of(&manager.get("bg_end").unwrap()), "Killed");
        let done = finished("bg_done");
        done.update_status("bg_done", BackgroundTaskStatus::Killed { finished_at: 4 });
        assert_eq!(
            super::state_of(&done.get("bg_done").unwrap()),
            "Failed (exit code 1)"
        );
    }

    /// A task whose exit status could not be read ends Lost, and the model —
    /// promised a wake when it ends — is woken for it, once.
    #[test]
    fn a_task_that_ends_lost_still_wakes_the_session_once() {
        let manager = running("bg_unread");
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        manager.update_status(
            "bg_unread",
            BackgroundTaskStatus::Lost {
                reason: LostReason::Unknown,
                finished_at: 5,
            },
        );
        manager.notify_completed("bg_unread");
        manager.notify_completed("bg_unread");
        let wakes = queue.unread();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(
            wakes[0].contains("bg_unread finished: Lost"),
            "{}",
            wakes[0]
        );
    }

    /// A report that answers an end's wake takes it back out of the queue
    /// while it is still unread.
    #[test]
    fn an_answered_wake_is_taken_back_out_of_the_queue() {
        let manager = finished("bg_report");
        let queue = Arc::new(QueueNotifier::default());
        manager.set_notifier(queue.clone());
        announce(&manager, "bg_report");
        assert_eq!(queue.unread().len(), 1);
        manager.answer_wake("bg_report");
        assert!(queue.unread().is_empty());
    }

    #[test]
    fn killed_task_keeps_its_status_and_wakes_an_active_session_once() {
        let manager = running("bg_killed");
        let notifier = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        manager.set_notifier(notifier.clone());
        manager.update_status("bg_killed", BackgroundTaskStatus::Killed { finished_at: 1 });
        manager.resume();
        manager.update_status(
            "bg_killed",
            BackgroundTaskStatus::Completed {
                exit_code: 137,
                finished_at: 2,
            },
        );
        announce(&manager, "bg_killed");
        assert!(matches!(
            manager.get("bg_killed").unwrap().status,
            BackgroundTaskStatus::Killed { .. }
        ));
        assert_eq!(notifier.0.load(Ordering::Relaxed), 1);
        announce(&manager, "bg_killed");
        assert_eq!(
            notifier.0.load(Ordering::Relaxed),
            1,
            "completion wakes remain exactly once"
        );
    }

    #[test]
    fn stopped_registry_suppresses_wake_and_new_turn_can_resume_it() {
        let manager = finished("bg_reconnect");
        let notifier = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        manager.set_notifier(notifier.clone());
        manager.stop_all();
        announce(&manager, "bg_reconnect");
        assert_eq!(notifier.0.load(Ordering::Relaxed), 0);
        manager.resume();
        announce(&manager, "bg_reconnect");
        assert_eq!(notifier.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_wake_is_settled_by_the_first_caller_and_by_nobody_after() {
        let manager = finished("bg_one");
        assert!(
            manager.claim_surface("bg_one"),
            "the first caller settles it"
        );
        assert!(!manager.claim_surface("bg_one"), "and never a second time");
    }

    #[test]
    fn a_running_task_has_nothing_to_settle() {
        let manager = BackgroundTaskManager::new();
        manager.register(BackgroundTaskInfo {
            task_id: "bg_two".to_string(),
            pid: 1,
            command: "sleep 1".into(),
            out_file: PathBuf::from("/tmp/x"),
            started_at: 0,
            status: BackgroundTaskStatus::Running,
            surfaced: false,
            answered: false,
            wake_text: None,
            capture_note: None,
            tail: String::new(),
            screen: None,
            reported: None,
            waiting: None,
            tree: None,
        });
        assert!(!manager.claim_surface("bg_two"));
    }

    #[test]
    fn a_wake_is_announced_once_and_only_once() {
        let manager = finished("bg_three");
        let wakes = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        manager.set_notifier(wakes.clone());

        announce(&manager, "bg_three");
        announce(&manager, "bg_three");

        assert_eq!(
            wakes.0.load(Ordering::Relaxed),
            1,
            "one finished process is one wake, however many times it is asked for"
        );
    }

    /// A steer can carry two wakes, and only one of them may have been
    /// answered. The answered one is taken out; the other survives, because it
    /// still has something to say.
    #[test]
    fn an_answered_wake_is_withdrawn_from_a_steer_that_carries_others() {
        let manager = finished("bg_answered");
        let other = finished("bg_untouched");
        let wakes = Arc::new(RecordingNotifier::default());
        manager.set_notifier(wakes.clone());
        other.set_notifier(wakes.clone());

        announce(&manager, "bg_answered");
        announce(&other, "bg_untouched");
        manager.answer_wake("bg_answered");

        let steer = format!(
            "model said something\n{}\n{}\n",
            wakes.text(0),
            wakes.text(1)
        );
        let left = manager.without_answered_wakes(&steer);

        assert!(
            left.contains("bg_untouched"),
            "the other wake survives: {left}"
        );
        assert!(
            !left.contains("bg_answered"),
            "the answered one is gone: {left}"
        );
        assert!(
            left.contains("model said something"),
            "and so does the model's own words"
        );
    }

    #[test]
    fn a_steer_that_was_only_an_answered_wake_has_nothing_left_to_say() {
        let manager = finished("bg_only");
        let wakes = Arc::new(RecordingNotifier::default());
        manager.set_notifier(wakes.clone());
        announce(&manager, "bg_only");
        manager.answer_wake("bg_only");

        assert_eq!(manager.without_answered_wakes(&wakes.text(0)), "");
    }

    /// Once a tool has answered, the monitor does not send a wake at all.
    #[test]
    fn a_task_answered_before_its_wake_is_never_woken_for() {
        let manager = finished("bg_race");
        let wakes = Arc::new(CountingNotifier(AtomicUsize::new(0)));
        manager.set_notifier(wakes.clone());
        manager.answer_wake("bg_race");

        announce(&manager, "bg_race");

        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    }
}
