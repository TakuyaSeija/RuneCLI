//! Starting the engine, and talking to it the same way a GUI would.
//!
//! Every frontend in this program — the interactive screen, the one-shot prompt,
//! the `-provider` commands — goes through [`Command`] and [`Event`]. None of
//! them reaches into the engine's internals, even though they are linked into
//! the same binary and easily could.
//!
//! That is not ceremony. `RuneStudio` will speak this protocol over a pipe, and
//! the only way to be sure it can do everything the terminal can is for the
//! terminal to have no other door. When a command is missing from the protocol,
//! it is missing for both.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use runeengine::engine::{Config, Engine};
use runeengine::provider::live::Live;
use runeengine::settings::Settings;
use runeengine::store::Db;
use runeengine::{Command, Event};
use serde_json::Value;

/// How long a remote-session resume may wait for its host's ack before the
/// caller is told, instead of hanging forever behind a wedged host.
pub(crate) const RESUME_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Look for `session`'s host for up to `patience`: a host between two builds
/// answers again within moments, and starting another beside it meanwhile is
/// a race the second one loses (`session_ipc::OWNED_ELSEWHERE`).
pub(crate) fn find_host(
    session: i64,
    patience: std::time::Duration,
) -> Option<(crate::session_ipc::SessionClient, Receiver<Event>)> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        if let Ok(found) = crate::session_ipc::SessionClient::connect(session) {
            return Some(found);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// [`find_host`], for as long as the home's upgrade keeps the host away
/// (`runeengine::upgrade::underway`, `store` the home): `patience` is one
/// look's. A host taking a newer build over serves again only once the home
/// is upgraded, and the upgrade waits for every session of the previous build
/// to close — a turn still running in one included. A host started beside it
/// would only queue behind the same upgrade; a window that did so, and then
/// gave up, closed in the middle of one (2026-10-09).
pub(crate) fn find_host_through_upgrade(
    session: i64,
    store: Option<&std::path::Path>,
    patience: std::time::Duration,
) -> Option<(crate::session_ipc::SessionClient, Receiver<Event>)> {
    loop {
        if let Some(found) = find_host(session, patience) {
            return Some(found);
        }
        if !store.is_some_and(runeengine::upgrade::underway) {
            return None;
        }
    }
}

pub struct Driver {
    pub engine: Engine,
    pub events: Receiver<Event>,
    pub local_events: Option<Receiver<Event>>,
    #[cfg(not(test))]
    pub(crate) db_path: std::path::PathBuf,
    #[cfg(not(test))]
    pub(crate) script: Option<std::path::PathBuf>,
    #[cfg(not(test))]
    pub(crate) read_only: bool,
    pub client: Option<crate::session_ipc::SessionClient>,
    pub host: Option<crate::session_ipc::SessionHost>,
    pub pending_events: Vec<Event>,
    pub(crate) next: u64,
    /// Commands sent to the session host and not yet acked, by request id.
    /// Kept so a host handover cannot lose one: whatever is still here when
    /// `host_upgrading` arrives was never accepted and is sent again.
    pub(crate) inflight: std::collections::BTreeMap<u64, Command>,
    /// Commands waiting for the next host while this window reconnects. Never
    /// handed to the private engine — that would be a second writer.
    pub(crate) outbox: Vec<Command>,
    pub(crate) reconnecting: bool,
    /// Session hosts this process started and has not seen end. A window hands
    /// them to the build it `exec`s into (`handover::Window::children`): they
    /// stay its children, and the thread that was waiting for each does not
    /// survive the exec.
    pub(crate) launched: Arc<Mutex<Vec<u32>>>,
}

pub struct Options {
    pub db: Option<std::path::PathBuf>,
    pub read_only: bool,
    /// A recorded script to replay instead of calling a model.
    pub script: Option<std::path::PathBuf>,
    /// The next request id a window handed over (`crate::handover`).
    pub request_next: Option<u64>,
}

impl Driver {
    pub fn start(options: Options) -> Result<Driver, String> {
        let db_path = match options.db {
            Some(p) => p,
            None => Db::default_path().map_err(|e| e.to_string())?,
        };
        #[cfg(not(test))]
        let data_dir = Db::home_dir().map_err(|e| e.to_string())?;
        #[cfg(test)]
        let data_dir = db_path
            .parent()
            .ok_or("Test database has no parent")?
            .join("Configuration");

        let settings = Arc::new(Mutex::new(
            Settings::load(&data_dir, &db_path).map_err(|e| e.to_string())?,
        ));
        let provider: Arc<dyn runeengine::provider::Provider> = match &options.script {
            Some(path) => Arc::new(
                runeengine::provider::scripted::Scripted::from_file(path)
                    .map_err(|e| e.to_string())?,
            ),
            None => Arc::new(Live::new(Arc::clone(&settings))),
        };

        let cfg = Config {
            db_path: db_path.clone(),
            data_dir,
            provider,
            settings: Arc::clone(&settings),
            read_only: options.read_only,
        };
        let (engine, events) = Engine::start(cfg).map_err(|e| e.to_string())?;
        Ok(Driver {
            engine,
            events,
            local_events: None,
            #[cfg(not(test))]
            db_path,
            #[cfg(not(test))]
            script: options.script,
            #[cfg(not(test))]
            read_only: options.read_only,
            client: None,
            host: None,
            pending_events: Vec::new(),
            inflight: std::collections::BTreeMap::new(),
            outbox: Vec::new(),
            reconnecting: false,
            launched: Arc::default(),
            // IPC forwards acks through session sockets. A per-process high
            // word makes request IDs disjoint between live windows, so a
            // sibling's ack cannot satisfy this window's pending request.
            next: {
                let prefix = (std::process::id() as u64) << 32;
                let inherited = options
                    .request_next
                    .filter(|value| *value >> 32 == std::process::id() as u64);
                inherited.unwrap_or(prefix | 1)
            },
        })
    }

    /// Dispatch one command, routing through remote client if attached.
    pub fn handle(&mut self, cmd: Command) {
        if matches!(
            cmd,
            Command::DraftOpen { .. }
                | Command::DraftCheck { .. }
                | Command::DraftSave { .. }
                | Command::DraftSettings { .. }
                | Command::DraftPromote { .. }
        ) {
            self.engine.handle(cmd);
        } else if self.reconnecting {
            self.outbox.push(cmd);
        } else if let Some(ref client) = self.client {
            let id = cmd.request_id();
            match client.send(&cmd) {
                Ok(()) => {
                    self.inflight.insert(id, cmd);
                }
                Err(error) => self.pending_events.push(Event::Ack {
                    id,
                    ok: false,
                    error: Some(error),
                    data: None,
                }),
            }
        } else {
            if matches!(cmd, Command::SessionOpen { fresh: true, .. }) {
                if let Some(host) = &self.host {
                    host.register_open(cmd.request_id());
                }
            }
            self.engine.handle(cmd);
        }
    }

    /// Account for one event from the session host. Returns `false` when the
    /// event is the host refusing a command because it is retiring: that
    /// command is queued for the next host and the refusal is not the UI's
    /// business.
    pub fn observe(&mut self, event: &Event) -> bool {
        let Event::Ack { id, ok, data, .. } = event else {
            return true;
        };
        let Some(cmd) = self.inflight.remove(id) else {
            return true;
        };
        let retry = !ok
            && data
                .as_ref()
                .and_then(|d| d.get("retry"))
                .and_then(Value::as_bool)
                == Some(true);
        if retry {
            self.outbox.push(cmd);
        }
        !retry
    }

    /// The host announced its upgrade: stop talking to it, and keep every
    /// command it never acked for the next one, oldest first.
    pub fn begin_reconnect(&mut self) {
        if let Some(old) = self.client.take() {
            old.disconnect();
        }
        self.reconnecting = true;
        let mut waiting: Vec<Command> = std::mem::take(&mut self.inflight).into_values().collect();
        waiting.append(&mut self.outbox);
        self.outbox = waiting;
    }

    /// Talk to the new host. `first` (the resume) goes ahead of everything
    /// that waited, so the host knows the window before it hears from it.
    pub fn finish_reconnect(&mut self, client: crate::session_ipc::SessionClient, first: Command) {
        self.client = Some(client);
        self.reconnecting = false;
        self.handle(first);
        for cmd in std::mem::take(&mut self.outbox) {
            self.handle(cmd);
        }
    }

    /// Start the session's engine in another local process and attach this
    /// terminal via its Unix socket. The UI owns no in-flight turn after this:
    /// Ctrl+C may close the terminal without calling Cancel or shutting down
    /// the active engine. Explicit `-resume` connects to the same host directly.
    #[cfg(not(test))]
    pub fn attach_detached(&mut self, session: i64, project: &str) -> Result<(), String> {
        let (client, events) = self.launcher(session, project)?.start()?;
        self.client = Some(client);
        let local = std::mem::replace(&mut self.events, events);
        if self.local_events.is_none() {
            self.local_events = Some(local);
        }
        Ok(())
    }

    /// Everything needed to start this session's host elsewhere — a worker
    /// thread, while the screen keeps painting.
    ///
    /// The host is the build installed where this one runs from, not this
    /// process's own file: once a release replaced it, that file is gone (the
    /// kernel names it `… (deleted)`), and a window still on the build before
    /// could start no host at all — a draft sent then was stored as a session
    /// that nothing answered (measured 2026-10-09: "could not start session
    /// host: No such file or directory"). A host of the newer build is what
    /// this window follows anyway, when idle.
    #[cfg(not(test))]
    pub(crate) fn launcher(&self, session: i64, project: &str) -> Result<HostLauncher, String> {
        Ok(HostLauncher {
            launched: Arc::clone(&self.launched),
            exe: crate::build::installed()?,
            db_path: self.db_path.clone(),
            script: self.script.clone(),
            read_only: self.read_only,
            session,
            project: project.to_string(),
        })
    }

    /// The hosts this process started and has not seen end, for the build it
    /// `exec`s into (`handover::Window::children`).
    pub(crate) fn children(&self) -> Vec<u32> {
        runeengine::panic::lock(&self.launched).clone()
    }

    /// Wait for the hosts the program this process was before had started:
    /// they are still its children, and nothing else will collect them.
    pub(crate) fn adopt_children(&self, pids: &[u32]) {
        for &pid in pids {
            runeengine::panic::lock(&self.launched).push(pid);
            let launched = Arc::clone(&self.launched);
            std::thread::spawn(move || {
                let _ = runeengine::platform::reap(pid as i32, true);
                runeengine::panic::lock(&launched).retain(|p| *p != pid);
            });
        }
    }

    pub fn shutdown(&mut self) {
        // What the engine still holds, and what a host served and has already
        // reaped from it (its own session stays owned until the host goes).
        let mut sessions = self.engine.open_sessions();
        if let Some(host) = self.host.as_ref() {
            sessions.extend(host.sessions());
        }
        sessions.sort_unstable();
        sessions.dedup();
        self.client = None;
        self.engine.shutdown();
        self.host = None;
        // A host's ownership locks went with it, so its sessions can be seen
        // closed only now; the engine's own pass could not clear their
        // scratch while this process still held them.
        self.engine.release_scratch(&sessions);
    }

    /// Send one command and wait for its answer.
    ///
    /// Events that arrive meanwhile are handed to `watch`, so a caller that
    /// wants to show progress can, and one that does not can ignore them —
    /// but they are never silently dropped on the floor.
    pub fn call_with(
        &mut self,
        make: impl FnOnce(u64) -> Command,
        mut watch: impl FnMut(&Event),
    ) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let command = make(id);
        #[cfg(not(test))]
        let host_diagnostic = if let Command::SessionResume { session, .. } = &command {
            self.db_path
                .parent()
                .map(|root| runeengine::store::run_files::host_log(root, *session))
        } else {
            None
        };
        // A composer boundary flush has its own subscribed receiver; the UI's
        // active event stream remains owned by its forwarder. Other UI commands
        // continue to use asynchronous ask rather than this boundary path.
        let barrier = if let Command::ComposerGet { session, .. }
        | Command::ComposerEdit { session, .. } = &command
        {
            if self.client.is_some() {
                Some(crate::session_ipc::SessionClient::connect(*session)?)
            } else {
                None
            }
        } else {
            None
        };
        let deadline = (barrier.is_some()
            || self.client.is_some()
                && matches!(
                    command,
                    Command::SessionResume { .. } | Command::SessionClients { .. }
                ))
        .then(|| std::time::Instant::now() + RESUME_DEADLINE);
        if let Some((client, _)) = &barrier {
            client.send(&command)?;
        } else {
            self.handle(command);
        }
        loop {
            if deadline.is_some_and(|at| std::time::Instant::now() >= at) {
                return Err(format!(
                    "Session host did not acknowledge resume within {} seconds",
                    RESUME_DEADLINE.as_secs()
                ));
            }
            let receiver = barrier.as_ref().map_or(&self.events, |(_, events)| events);
            let event = match receiver.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(ev) => ev,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    #[cfg(not(test))]
                    if let Some(path) = &host_diagnostic {
                        return Err(format!(
                            "the engine stopped; diagnostic log: {}",
                            path.display()
                        ));
                    }
                    return Err("the engine stopped".to_string());
                }
            };
            if let Event::Ack {
                id: got,
                ok,
                error,
                data,
            } = &event
            {
                if *got == id {
                    self.inflight.remove(&id);
                    return if *ok {
                        Ok(data.clone().unwrap_or(Value::Null))
                    } else {
                        Err(error.clone().unwrap_or_else(|| "failed".into()))
                    };
                }
            }
            watch(&event);
            self.pending_events.push(event);
        }
    }

    pub fn call(&mut self, make: impl FnOnce(u64) -> Command) -> Result<Value, String> {
        self.call_with(make, |_| {})
    }

    /// The session's view, as the engine holds it at one place in the event
    /// stream. Everything that arrived before the answer is inside it, so it is
    /// dropped rather than applied a second time; what comes after is left in
    /// the stream, where it belongs. See `runeengine::session::view`.
    pub fn view(&mut self, session: i64) -> Result<Value, String> {
        let before = self.pending_events.len();
        let view = self.call(|id| Command::SessionView { id, session });
        self.pending_events.truncate(before);
        view
    }

    /// Send a command without waiting, and answer the ack when it arrives.
    ///
    /// The screen cannot use [`Driver::call`]: its event stream belongs to a
    /// forwarding thread, so a blocking wait for one ack would sit on a channel
    /// that will never deliver it — and, worse, would freeze the frame while the
    /// engine is mid-turn. Everything the screen asks for is asked this way and
    /// answered in the event loop.
    /// Generation ordering is a local registry operation, not a wait on the
    /// receiver already owned by the UI. Commands remain the frontend boundary.
    pub fn local(&mut self, command: Command) -> Result<Value, String> {
        self.engine
            .dispatch_local(command)
            .map_err(|error| error.to_string())
            .map(|value| value.unwrap_or(Value::Null))
    }

    pub fn ask(&mut self, make: impl FnOnce(u64) -> Command) -> u64 {
        let id = self.next;
        self.next += 1;
        self.handle(make(id));
        id
    }

    /// Start a new interactive session; only explicit `-resume` attaches to an
    /// existing one, even if an older window left its session empty.
    pub fn open(&mut self, project: &str) -> Result<i64, String> {
        // Inspect the draft before opening another identity. A live host with
        // no existing watcher can be reused; an occupied terminal stays isolated.
        let canonical = runeengine::platform::canonical(std::path::Path::new(project))
            .map_err(|e| e.to_string())?;
        let list = self.call(|id| Command::SessionList {
            id,
            project: Some(canonical.to_string_lossy().into_owned()),
        })?;
        let candidate = list["drafts"]
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|row| row["id"].as_i64());
        if let Some(session) = candidate {
            if let Ok((client, events)) = crate::session_ipc::SessionClient::connect(session) {
                self.client = Some(client);
                let local_events = std::mem::replace(&mut self.events, events);
                let count = self.call(|id| Command::SessionClients { id, session })?;
                if count["count"].as_u64() == Some(1) {
                    return Ok(session);
                }
                self.client = None;
                self.events = local_events;
                self.pending_events.clear();
            } else {
                let (_, data) = self.resume(session, Some(project.into()))?;
                return session_id(&data);
            }
        }
        let out = self.call(|id| Command::SessionOpen {
            id,
            project: project.to_string(),
            fresh: true,
        })?;
        session_id(&out)
    }

    pub fn resume(
        &mut self,
        session: i64,
        project: Option<String>,
    ) -> Result<(i64, Value), String> {
        let out = self.call(|id| Command::SessionResume {
            id,
            session,
            project,
        })?;
        let id = session_id(&out)?;
        Ok((id, out))
    }

    /// The newest session of a project, if there is one.
    pub fn newest(&mut self, project: &str) -> Result<Option<i64>, String> {
        let out = self.call(|id| Command::SessionList {
            id,
            project: Some(project.to_string()),
        })?;
        Ok(out
            .get("sessions")
            .and_then(|s| s.as_array())
            .and_then(|list| list.first())
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_i64()))
    }
}

/// What it takes to start a detached session host: plain data, so it can run
/// on a worker thread.
#[cfg(not(test))]
pub(crate) struct HostLauncher {
    launched: Arc<Mutex<Vec<u32>>>,
    exe: std::path::PathBuf,
    db_path: std::path::PathBuf,
    script: Option<std::path::PathBuf>,
    read_only: bool,
    session: i64,
    project: String,
}

#[cfg(not(test))]
impl HostLauncher {
    /// Spawn the host and wait until its socket answers.
    pub(crate) fn start(
        self,
    ) -> Result<(crate::session_ipc::SessionClient, Receiver<Event>), String> {
        let log_path = runeengine::store::run_files::host_log(
            self.db_path.parent().ok_or("Database path has no parent")?,
            self.session,
        );
        std::fs::create_dir_all(log_path.parent().unwrap()).map_err(|error| error.to_string())?;
        let log =
            runeengine::platform::open_private(&log_path, runeengine::platform::Private::Append)
                .map_err(|error| error.to_string())?;
        let mut launch = runeengine::platform::program_launch(&self.exe)
            .arg("-detached-host")
            .arg(self.session.to_string())
            .arg("-project")
            .arg(&self.project)
            .arg("-db")
            .arg(&self.db_path);
        if let Some(script) = &self.script {
            launch = launch.arg("-script").arg(script);
        }
        if self.read_only {
            launch = launch.arg("-read-only");
        }
        // The host owns the turn, so it must outlive the window that started it.
        // Left in this window's process group it would take the terminal's
        // `Ctrl+C` — a SIGINT to the whole group — and die mid-tool, which is
        // exactly how a session ends up holding an assistant message whose tool
        // call has no result. Detached, the only signals that reach it are the
        // ones the engine sends deliberately.
        let mut child = runeengine::platform::spawn_detached(&launch, log)
            .map_err(|e| format!("could not start session host: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        // The host started here stepped aside: the session already has one —
        // most often its own host between builds, handing it on. Not a failure:
        // that host is the one to reach (`session_ipc::OWNED_ELSEWHERE`).
        let mut elsewhere = false;
        loop {
            if let Ok((client, events)) = crate::session_ipc::SessionClient::connect(self.session) {
                if !elsewhere {
                    self.watch(child, log_path);
                } else {
                    // The host that stepped aside said nothing worth keeping.
                    forget_quiet_log(&log_path);
                }
                return Ok((client, events));
            }
            if !elsewhere {
                if let Some(code) = child.try_wait().map_err(|e| e.to_string())? {
                    if code != crate::session_ipc::OWNED_ELSEWHERE {
                        return Err(format!(
                            "session host exited during startup (exit code {code}); diagnostic log: {}",
                            log_path.display()
                        ));
                    }
                    elsewhere = true;
                }
            }
            if std::time::Instant::now() >= deadline {
                if elsewhere {
                    return Err(format!(
                        "session #{} has a host that did not answer within 30 seconds",
                        self.session
                    ));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err("session host did not become ready within 30 seconds".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// Wait for a host started here, off the UI thread, and remember it
    /// meanwhile: dropping a `Child` does not stop its process, and a host
    /// that ends uncollected stays a zombie under this window.
    fn watch(&self, mut child: runeengine::platform::Process, log: std::path::PathBuf) {
        let pid = child.id() as u32;
        runeengine::panic::lock(&self.launched).push(pid);
        let launched = Arc::clone(&self.launched);
        std::thread::spawn(move || {
            let _ = child.wait();
            runeengine::panic::lock(&launched).retain(|p| *p != pid);
            forget_quiet_log(&log);
        });
    }
}

/// A host's log is kept only when the host had something to say: one that
/// ended in silence leaves no empty file behind (owner, 2026-10-08).
#[cfg(not(test))]
fn forget_quiet_log(log: &std::path::Path) {
    if std::fs::metadata(log).is_ok_and(|meta| meta.len() == 0) {
        let _ = std::fs::remove_file(log);
    }
}

fn session_id(out: &Value) -> Result<i64, String> {
    out.get("session")
        .or_else(|| out.get("id"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "the engine did not name the session it opened".to_string())
}

/// The workspace to act in: what was asked for, made absolute.
pub fn workspace(project: Option<std::path::PathBuf>) -> Result<String, String> {
    let path = match project {
        Some(p) => p,
        None => here()?,
    };
    let path =
        runeengine::platform::canonical(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    path.into_os_string()
        .into_string()
        .map_err(|_| "that path is not valid UTF-8".to_string())
}

/// The folder the terminal is in.
///
/// A shell keeps standing in a folder that was deleted under it; asked for
/// its path, the system says only "No such file or directory (os error 2)",
/// which named neither the folder nor the way out (owner, 2026-10-15: a
/// project deleted and made again at the same path). The folder now at the
/// shell's `$PWD` is a different one, so it is named, never entered.
pub fn here() -> Result<std::path::PathBuf, String> {
    std::env::current_dir().map_err(|error| gone_folder(&error, std::env::var_os("PWD")))
}

fn gone_folder(error: &std::io::Error, pwd: Option<std::ffi::OsString>) -> String {
    if error.kind() != std::io::ErrorKind::NotFound {
        return format!("cannot read the current folder: {error}");
    }
    let was = pwd
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute());
    match was {
        Some(path) if path.is_dir() => format!(
            "the folder this terminal is in was deleted or moved; {} is now a different folder. \
             To work there, run `cd {}` and start Rune again.",
            path.display(),
            shell_quoted(&path)
        ),
        Some(path) => format!(
            "the folder this terminal is in ({}) was deleted or moved. \
             `cd` into the project folder and start Rune again.",
            path.display()
        ),
        None => "the folder this terminal is in was deleted or moved. \
                 `cd` into the project folder and start Rune again."
            .into(),
    }
}

/// A path as a POSIX shell reads it back, quoted only when it has to be.
fn shell_quoted(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%~,".contains(c))
    {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduced 2026-10-15: `rune` started in a shell standing in a folder
    /// deleted and made again said only "No such file or directory (os error
    /// 2)". It names the folder and the command that enters the new one.
    #[test]
    fn a_terminal_in_a_deleted_folder_is_told_which_folder_and_how_to_enter_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        let base = std::env::temp_dir().join(format!("cwd-gone-{}", std::process::id()));
        let again = base.join("Project Testing");
        std::fs::create_dir_all(&again).unwrap();

        let said = gone_folder(&gone, Some(again.clone().into_os_string()));
        assert!(said.contains("is now a different folder"), "{said}");
        assert!(
            said.contains(&format!("cd '{}'", again.display())),
            "the way in is a command the shell reads back: {said}"
        );
        assert!(!said.contains("os error"), "{said}");

        let moved = base.join("never-made-again");
        let said = gone_folder(&gone, Some(moved.clone().into_os_string()));
        assert!(said.contains(&moved.display().to_string()), "{said}");
        assert!(said.contains("deleted or moved"), "{said}");

        let said = gone_folder(&gone, None);
        assert!(said.contains("deleted or moved"), "{said}");
        let said = gone_folder(&gone, Some("relative/path".into()));
        assert!(
            !said.contains("relative/path"),
            "a relative $PWD is no folder: {said}"
        );

        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(gone_folder(&other, None).starts_with("cannot read the current folder"));
        assert_eq!(
            shell_quoted(std::path::Path::new("/home/a/Testing")),
            "/home/a/Testing"
        );
        assert_eq!(
            shell_quoted(std::path::Path::new("/x/it's")),
            "'/x/it'\\''s'"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A socket whose host accepts the connection and then never answers is the
    /// exact shape of the wedged-host hang. Resume must give up within the
    /// deadline and say so — not block the caller forever.
    /// A window whose host is away for the home's upgrade waits for it as long
    /// as the upgrade runs, a look at a time — before, one look's patience
    /// ran out, it started a host that queued behind the same upgrade, and
    /// then it gave up and closed. With no upgrade running, one look is all.
    #[test]
    fn a_host_away_for_the_homes_upgrade_is_waited_for_as_long_as_it_runs() {
        let _d = crate::ui::test_driver("host-through-upgrade");
        let session = 987_655i64;
        let path = crate::session_ipc::session_socket_path(session).unwrap();
        let _ = std::fs::remove_file(&path);
        let store = std::env::temp_dir()
            .join("rune-test")
            .join(format!("upgrade-underway-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(store.join("run")).unwrap();
        let patience = std::time::Duration::from_millis(200);

        let started = std::time::Instant::now();
        assert!(find_host_through_upgrade(session, Some(&store), patience).is_none());
        assert!(
            started.elapsed() < patience * 4,
            "with no upgrade running it looked more than once"
        );

        // An upgrade holds its lock; the host is back after several looks.
        let upgrading = runeengine::lock::exclusive(
            &store.join("run/Upgrade.lock"),
            runeengine::lock::Wait::No,
        )
        .unwrap();
        assert!(runeengine::upgrade::underway(&store));
        let host = {
            let path = path.clone();
            std::thread::spawn(move || {
                use std::io::{BufRead, Write};
                std::thread::sleep(patience * 5);
                let key = runeengine::access::HostKey::issue(&path).unwrap();
                let listener = runeengine::platform::LocalListener::bind(&path).unwrap();
                let mut stream = listener.accept().unwrap();
                let mut hello = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut hello)
                    .unwrap();
                if let runeengine::access::Presented::Hello(Some(presented)) =
                    runeengine::access::presented(hello.trim_end())
                {
                    assert!(key.admits(&presented));
                }
                writeln!(stream, "{}", runeengine::access::admitted_line()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(500));
            })
        };
        let found = find_host_through_upgrade(session, Some(&store), patience);
        assert!(found.is_some(), "it gave up while the upgrade ran");
        drop(upgrading);
        assert!(!runeengine::upgrade::underway(&store));
        host.join().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn resume_to_a_silent_host_times_out_instead_of_hanging() {
        let mut d = crate::ui::test_driver("resume-silent-host");
        let session = 987_654i64;

        // Bind the session's socket with a host that admits the window and
        // then never writes another byte, and attach the driver's client to
        // it. `RUNE_HOME` is left alone by construction: `session_socket_path`
        // uses the engine home, which the test driver already points at its
        // own temp directory.
        let path = crate::session_ipc::session_socket_path(session).unwrap();
        let _ = std::fs::remove_file(&path);
        let key = runeengine::access::HostKey::issue(&path).unwrap();
        let silent = runeengine::platform::LocalListener::bind(&path).unwrap();
        let _host = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut stream = silent.accept().unwrap();
            let mut hello = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut hello)
                .unwrap();
            if let runeengine::access::Presented::Hello(Some(presented)) =
                runeengine::access::presented(hello.trim_end())
            {
                assert!(key.admits(&presented));
            }
            writeln!(stream, "{}", runeengine::access::admitted_line()).unwrap();
            std::thread::sleep(RESUME_DEADLINE * 2);
        });
        let (client, _events) = crate::session_ipc::SessionClient::connect(session).unwrap();
        d.client = Some(client);

        let started = std::time::Instant::now();
        let err = d
            .resume(session, None)
            .expect_err("silent host must time out");
        assert!(
            err.contains("did not acknowledge resume"),
            "the error must name the deadline: {err}"
        );
        assert!(
            started.elapsed() >= RESUME_DEADLINE,
            "the failure came from the deadline, not an early error: {started:?}"
        );
    }

    /// A host the program before this one started — still this process's
    /// child after the exec, with nothing waiting for it — is waited for once
    /// adopted: when it ends it is collected, not left a zombie, and forgotten.
    #[test]
    fn an_adopted_child_is_collected_when_it_ends() {
        let d = crate::ui::test_driver("adopt-children");
        let child = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let pid = child.id();
        // As after an exec: no handle in this program waits for it.
        std::mem::forget(child);
        d.adopt_children(&[pid]);
        assert_eq!(d.children(), vec![pid]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !d.children().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the child was never collected"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let state = runeengine::platform::fixture::process_state(pid as i32);
        assert_ne!(
            state,
            Some(runeengine::platform::fixture::ProcessState::Zombie),
            "left a zombie"
        );
    }

    fn cancel(id: u64) -> Command {
        Command::Cancel {
            id,
            session: 1,
            call: None,
        }
    }

    /// Audit R1: during a handover nothing is lost and nothing reaches the
    /// private engine — unacked and refused commands wait, oldest first, for
    /// the next host.
    #[test]
    fn a_handover_keeps_every_unaccepted_command_in_order() {
        let mut d = crate::ui::test_driver("handover-queue");
        d.inflight.insert(5, cancel(5));
        d.inflight.insert(3, cancel(3));
        d.begin_reconnect();
        d.handle(cancel(9));
        assert!(d.reconnecting);
        let ids = |d: &Driver| d.outbox.iter().map(Command::request_id).collect::<Vec<_>>();
        assert_eq!(ids(&d), vec![3, 5, 9]);

        d.reconnecting = false;
        d.inflight.insert(11, cancel(11));
        let refused = Event::Ack {
            id: 11,
            ok: false,
            error: Some("retiring".into()),
            data: Some(serde_json::json!({"retry": true})),
        };
        assert!(!d.observe(&refused), "a retry refusal is not shown");
        assert_eq!(ids(&d), vec![3, 5, 9, 11]);
        d.inflight.insert(12, cancel(12));
        let accepted = Event::Ack {
            id: 12,
            ok: true,
            error: None,
            data: None,
        };
        assert!(d.observe(&accepted));
        assert!(d.inflight.is_empty());
    }
}
