//! `rune -host` — the protocol on stdin and stdout, one JSON object per line.
//!
//! This is what a GUI frontend spawns. It was the whole of this binary before
//! there was a terminal UI, and it stays exactly as it was: the screen is one
//! more speaker of this protocol, not a replacement for it.
//!
//! Two threads, for one reason: the event writer must keep running while a turn
//! is in flight, and stdin must keep being read so `cancel` and
//! `permission.reply` can arrive *during* that turn.

use std::io::Write;

use runeengine::Command;

use crate::driver::Driver;

/// Serve the protocol on stdin and stdout. `handed`, `tasks` and `input` are
/// what the previous `rune -host` in this process handed over when it
/// activated this build (`crate::handover`): its sessions, opened again here
/// instead of a new one for `project`; their background tasks, taken over
/// still running; and the bytes it had read of a command not yet whole.
pub fn run(
    mut driver: Driver,
    project: Option<String>,
    handed: Vec<i64>,
    tasks: Vec<runeengine::session::background::SessionTasks>,
    input: Vec<u8>,
) -> Result<(), String> {
    let events = std::mem::replace(&mut driver.events, std::sync::mpsc::channel().1);

    // One writer thread owns stdout. Nothing else may print there: a stray line
    // would corrupt the stream for the frontend parsing it.
    let writer = std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for event in events {
            match serde_json::to_string(&event) {
                Ok(line) => {
                    if writeln!(out, "{line}").is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
                Err(e) => eprintln!("rune: could not encode event: {e}"),
            }
        }
    });

    // Open the project first when one was given, so `ready` reports the sandbox
    // that will actually apply to it rather than a guess. A host that took over
    // from the previous build opens the sessions it held instead.
    let first = if handed.is_empty() {
        match &project {
            Some(dir) => match driver.engine.open(dir) {
                Ok(session) => Some(session.id()),
                Err(e) => {
                    driver.engine.report_protocol_error(&e.to_string());
                    None
                }
            },
            None => None,
        }
    } else {
        handed.first().copied()
    };
    for session in first.into_iter().chain(handed.iter().skip(1).copied()) {
        driver.engine.handle(Command::SessionResume {
            id: 0,
            session,
            project: None,
        });
    }
    match first.and_then(|session| driver.engine.sandbox_of(session).ok()) {
        Some(s) => driver.engine.announce(&s),
        None => {
            // With no project open, report what confinement *would* apply, using
            // this directory as the stand-in. A probe creates nothing — not in
            // the directory the user started in, not in the engine's home — so
            // its scratch is the stand-in itself, which already exists and is
            // granted anyway.
            let cwd = crate::driver::here()?;
            let probe = runeengine::sandbox::Sandbox::probe(
                &runeengine::sandbox::Policy::for_project(&cwd, &cwd),
            );
            driver.engine.announce(&probe);
        }
    }
    for error in driver.engine.adopt_tasks(tasks) {
        eprintln!("rune: {error}");
    }
    // Only now, with what the build before handed on taken over: a task
    // that is neither here nor handed on was lost, and the model is told.
    driver.engine.settle_background();

    let mut installed = Installed::here(&driver)?;
    let served = serve(&mut driver, &mut installed, input);
    let (handed_tasks, pending) = match served {
        Ok(Served::Ended) => (None, Vec::new()),
        Ok(Served::Upgrading { tasks, pending }) => {
            // After the ack of every command accepted so far: the frontend
            // sends again whatever has none, once the successor is `ready`.
            driver.engine.notify(runeengine::Event::Upgrading);
            (Some(tasks), pending)
        }
        Err(error) => {
            driver.engine.shutdown();
            drop(driver);
            let _ = writer.join();
            return Err(error);
        }
    };
    let sessions = driver.engine.open_sessions();
    driver.engine.shutdown();
    drop(driver);
    // Every line written before the successor writes its first.
    let _ = writer.join();
    match handed_tasks {
        None => Ok(()),
        // The stdio host owns no session socket, so it holds no lock to hand on.
        Some(tasks) => Err(installed.exec(
            crate::handover::Handover {
                sessions,
                tasks,
                input: pending,
                ..Default::default()
            },
            Vec::new(),
        )),
    }
}

/// How serving stdin ended.
enum Served {
    /// The frontend closed stdin, or asked the host to shut down.
    Ended,
    /// An installed release is to take over: the tasks let go of for it, and
    /// the bytes read of a command not yet whole.
    Upgrading {
        tasks: Vec<runeengine::session::background::SessionTasks>,
        pending: Vec<u8>,
    },
}

/// Read commands off stdin and run them, looking every half second for an
/// installed release to hand over to while nothing is in flight. Read
/// straight from the descriptor, never through a buffer of the standard
/// library's: what has been read and not yet run is exactly `pending`, which
/// the successor is handed — nothing the frontend wrote is lost in a buffer
/// an `exec` throws away.
fn serve(
    driver: &mut Driver,
    installed: &mut Installed,
    mut pending: Vec<u8>,
) -> Result<Served, String> {
    let limit = runeengine::limits::MAX_TOOL_ARG_BYTES;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut closed = false;
    loop {
        // Every whole line first; at the end of input, what is left is one.
        while let Some(end) = pending
            .iter()
            .position(|b| *b == b'\n')
            .map(|at| at + 1)
            .or((closed && !pending.is_empty()).then_some(pending.len()))
        {
            let line: Vec<u8> = pending.drain(..end).collect();
            if line.len() > limit {
                return Err("Protocol input exceeds configured limit".into());
            }
            let line = String::from_utf8(line).map_err(|_| "Protocol input must be UTF-8")?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Command>(&line) {
                Ok(cmd) => {
                    if !driver.engine.handle(cmd) {
                        return Ok(Served::Ended);
                    }
                }
                Err(e) => driver.engine.report_protocol_error(&e.to_string()),
            }
        }
        if closed {
            return Ok(Served::Ended);
        }
        if pending.len() > limit {
            return Err("Protocol input exceeds configured limit".into());
        }
        if driver.engine.idle() && installed.newer() {
            match driver.engine.release_tasks() {
                Ok(tasks) => return Ok(Served::Upgrading { tasks, pending }),
                Err(error) => {
                    eprintln!("rune: not upgrading yet: {error}");
                    installed.hold_back();
                }
            }
        }
        match runeengine::platform::read_stdin_timeout(&mut chunk, 500) {
            Ok(Some(0)) => closed = true,
            Ok(Some(n)) => pending.extend_from_slice(&chunk[..n]),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Commands that answer or stop something already running. Served ahead of
/// anything queued with them.
fn is_control(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Cancel { .. } | Command::PermissionReply { .. } | Command::QuestionsReply { .. }
    )
}

/// Private Unix domain socket IPC host running independently of the interactive terminal.
///
/// Dispatched by `-detached-host <session>`. Owns the session's live engine,
/// dispatches commands from connected clients, and continues streaming in-flight turns
/// across client disconnects (such as when the user exits with Ctrl+C). Exits after
/// an inactivity timeout when no clients remain connected and no turns are running.
/// Serve one session (and its children) to every window attached to it.
/// `handed` lists the sessions a previous host held open when it activated
/// this build (`crate::handover`); they are opened again here, and `tasks`,
/// their background tasks, taken over still running.
pub fn detached(
    mut driver: Driver,
    project: String,
    session: i64,
    handed: Vec<i64>,
    tasks: Vec<runeengine::session::background::SessionTasks>,
    owners: Vec<crate::handover::Owner>,
) -> Result<(), String> {
    crate::session_ipc::adopt_ownership(&owners);
    let (msg_tx, msg_rx) = std::sync::mpsc::channel();
    let (host, _ui_rx) = match crate::session_ipc::SessionHost::start(
        session,
        msg_tx,
        std::mem::replace(&mut driver.events, std::sync::mpsc::channel().1),
    ) {
        Ok(started) => started,
        // Another process owns the session — its own host, most often, on its
        // way to a newer build. Stepping aside is the answer, not a failure:
        // the window that started this one connects to that host instead.
        Err(crate::session_ipc::NotStarted::Owned) => {
            eprintln!("rune: session #{session} already has a host; this one steps aside");
            std::process::exit(crate::session_ipc::OWNED_ELSEWHERE);
        }
        Err(crate::session_ipc::NotStarted::Failed(why)) => return Err(why),
    };
    driver.host = Some(host);
    driver.local(Command::SessionResume {
        id: 0,
        session,
        project: Some(project),
    })?;
    driver
        .engine
        .recover_session(session)
        .map_err(|error| error.to_string())?;
    {
        for id in handed.into_iter().filter(|id| *id != session) {
            let token = (id as u64) | (1u64 << 63);
            driver.host.as_ref().unwrap().register_open(token);
            driver.handle(Command::SessionResume {
                id: token,
                session: id,
                project: None,
            });
            // A child session handed over mid-composition has a journal to
            // reconcile, exactly like the main one.
            if let Err(error) = driver.engine.recover_session(id) {
                eprintln!("session #{id}: could not recover its composer: {error}");
            }
        }
    }
    for error in driver.engine.adopt_tasks(tasks) {
        eprintln!("rune: {error}");
    }
    // Only now, with what the build before handed on taken over: a task
    // that is neither here nor handed on was lost, and the model is told.
    driver.engine.settle_background();

    let mut last_activity = std::time::Instant::now();
    let mut installed = Installed::here(&driver)?;
    let mut upgrade = false;
    let mut handed_tasks = Vec::new();
    let mut departed = Departed::default();
    while let Some(ref host) = driver.host {
        // A turn in flight is waited out, and so is a provider or catalogue
        // worker — joined only after the windows were told to reconnect, it
        // could outlast their patience. A background task is not waited out:
        // it is handed to the successor still running.
        if !host.is_busy() && driver.engine.idle() && installed.newer() {
            let _admission = runeengine::panic::lock(&crate::session_ipc::ADMISSION_GATE);
            match msg_rx.try_recv() {
                Ok(crate::ui::Msg::RemoteCommand(cmd)) => {
                    drop(_admission);
                    departed.serve(&mut driver, cmd);
                    continue;
                }
                Ok(crate::ui::Msg::SessionDetached(id)) => {
                    drop(_admission);
                    departed.left(&driver, id);
                    continue;
                }
                Ok(_) => continue,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Let go of the background tasks first: if one cannot
                    // be handed on, every task runs on here and this host
                    // tries again later, still serving.
                    match driver.engine.release_tasks() {
                        Ok(tasks) => handed_tasks = tasks,
                        Err(error) => {
                            eprintln!("rune: not upgrading yet: {error}");
                            installed.hold_back();
                            continue;
                        }
                    }
                    // From here a command is refused with a retry ack, never
                    // dropped; the windows resend it to the next host.
                    host.retire();
                    upgrade = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        match msg_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(crate::ui::Msg::RemoteCommand(cmd)) => {
                // Everything already waiting, control first: one window's heavy
                // command must not hold back another window's Stop or its
                // answer to a permission prompt.
                let mut batch = vec![cmd];
                let mut other = Vec::new();
                while let Ok(next) = msg_rx.try_recv() {
                    match next {
                        crate::ui::Msg::RemoteCommand(cmd) => batch.push(cmd),
                        message => other.push(message),
                    }
                }
                batch.sort_by_key(|cmd| !is_control(cmd));
                for cmd in batch {
                    departed.serve(&mut driver, cmd);
                }
                for message in other {
                    if let crate::ui::Msg::SessionDetached(id) = message {
                        departed.left(&driver, id);
                    }
                }
                last_activity = std::time::Instant::now();
            }
            Ok(crate::ui::Msg::SessionDetached(id)) => departed.left(&driver, id),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // A session handed on that has not come back by now is not
                // coming: its lock is let go, so the session is not held by a
                // host that will never serve it.
                crate::session_ipc::expire_adopted(std::time::Duration::from_secs(30));
                let admission = runeengine::panic::lock(&crate::session_ipc::ADMISSION_GATE);
                let lifetime = crate::ui::runtime_configuration().duration("host.idle_lifetime");
                let detached: Vec<_> = host
                    .sessions()
                    .into_iter()
                    .filter(|id| {
                        if host.owns_clients(*id) {
                            departed.back(*id);
                            false
                        } else {
                            departed.since(*id).elapsed() >= lifetime
                        }
                    })
                    .collect();
                match msg_rx.try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        let released = driver.engine.reap(&detached);
                        drop(admission);
                        host.release_children(&released);
                        // Their hosts' locks are gone with the children: a
                        // session nobody holds now takes its scratch with it.
                        driver.engine.release_scratch(&released);
                    }
                    Ok(crate::ui::Msg::RemoteCommand(command)) => {
                        drop(admission);
                        departed.serve(&mut driver, command);
                        last_activity = std::time::Instant::now();
                        continue;
                    }
                    Ok(crate::ui::Msg::SessionDetached(id)) => {
                        drop(admission);
                        departed.left(&driver, id);
                        continue;
                    }
                    _ => drop(admission),
                }
                // A turn is worth staying alive for only while somebody can still
                // be watching it. Another process holding this same session —
                // a second window, or a `-resume` that found this host — is that
                // somebody: the work is theirs to collect, so it runs on. With
                // no such process there is nobody left to hand the answer to,
                // and the clock is what separates "closed the window and is
                // coming back" from "closed the window and is done".
                if host.has_clients() {
                    last_activity = std::time::Instant::now();
                } else if last_activity.elapsed()
                    >= crate::ui::runtime_configuration().duration("host.idle_lifetime")
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Leaving the loop while a turn is still running would end the process with
    // an assistant message whose tool call has no result — a transcript no
    // provider will accept, and one that poisons the session permanently. Stop
    // the turn first so the engine writes the closing result for every call it
    // opened, then let `shutdown` join it.
    driver
        .engine
        .session(session)
        .map(|s| {
            s.state.request_cancel();
            // On an upgrade the tasks were handed on, still running.
            if !upgrade {
                s.state.background.stop_all();
            }
        })
        .ok();
    let sessions = driver
        .host
        .as_ref()
        .map(|host| host.sessions())
        .unwrap_or_default();
    if upgrade {
        // Through the engine's own channel, children first: each window hears
        // the notice after the ack of every command its host accepted, so an
        // unacked command is one it may safely send again.
        for id in sessions.iter().rev() {
            driver
                .engine
                .notify(runeengine::proto::Event::HostUpgrading { session: *id });
        }
        if let Some(host) = driver.host.as_ref() {
            host.drain(std::time::Instant::now() + std::time::Duration::from_secs(5));
        }
    }
    // Taken out before the host goes, so its going releases none of them.
    let owners = match driver.host.as_mut() {
        Some(host) if upgrade => host.hand_over_ownership(),
        _ => Vec::new(),
    };
    driver.shutdown();
    if upgrade {
        return Err(installed.exec(
            crate::handover::Handover {
                sessions,
                tasks: handed_tasks,
                ..Default::default()
            },
            owners,
        ));
    }
    Ok(())
}

/// Sessions whose windows have all gone, and the ones a window said it is
/// coming back to.
///
/// A session's last window going away leaves nobody to collect its runtime,
/// so its turn is cancelled and its wakes held at once — no model request is
/// made for a session nobody watches. Its background tasks are other work: they
/// run on until the session is reaped, once it has had no window for
/// `host.idle_lifetime` (a person who quit by accident and comes back finds
/// them still running, and hears of any that ended meanwhile). A window that
/// said first that it is only `exec`ing a newer build of itself
/// (`session.returning`) leaves the runtime running too while it comes back.
#[derive(Default)]
struct Departed {
    since: std::collections::HashMap<i64, std::time::Instant>,
    returning: std::collections::HashMap<i64, std::time::Instant>,
}

impl Departed {
    /// Run `command`, noting first a window that says it is coming back —
    /// and one that has: a window back resumes its session.
    fn serve(&mut self, driver: &mut Driver, command: Command) {
        match &command {
            Command::SessionReturning { session, .. } => {
                self.returning.insert(*session, std::time::Instant::now());
            }
            Command::SessionResume { session, .. } => {
                self.returning.remove(session);
                self.back(*session);
            }
            _ => {}
        }
        driver.handle(command);
    }

    /// A window of session `id` went away.
    fn left(&mut self, driver: &Driver, id: i64) {
        if driver
            .host
            .as_ref()
            .is_some_and(|host| host.owns_clients(id))
        {
            return;
        }
        self.since.entry(id).or_insert_with(std::time::Instant::now);
        let lifetime = crate::ui::runtime_configuration().duration("host.idle_lifetime");
        // Until the window is back — it resumes its session — or the time an
        // idle session is kept has passed. Starting again, a window connects
        // and lets go more than once before it resumes, and each of those
        // looks like the last window leaving.
        match self.returning.get(&id) {
            Some(said) if said.elapsed() < lifetime => return,
            Some(_) => {
                self.returning.remove(&id);
            }
            None => {}
        }
        if let Ok(session) = driver.engine.session(id) {
            session.state.files.stop();
            session.state.background.suspend();
            session.state.request_cancel();
        }
    }

    /// Session `id` has a window again.
    fn back(&mut self, id: i64) {
        self.since.remove(&id);
    }

    /// Since when session `id` has had no window.
    fn since(&mut self, id: i64) -> std::time::Instant {
        *self.since.entry(id).or_insert_with(std::time::Instant::now)
    }
}

/// The build this process runs, against what is installed at its path now.
///
/// A host hands itself over to the installed build once that is another file
/// — a release installed over this one — which answers that it can take this
/// home over (`rune -upgrade-check`, asked once per installed file).
struct Installed {
    executable: std::path::PathBuf,
    /// The file this process was started from.
    running: Option<runeengine::platform::FileIdentity>,
    /// The home the installed build is asked about: this process's own.
    database: Option<std::path::PathBuf>,
    /// After a hand-over could not start, the next try waits until then.
    held_back_until: Option<std::time::Instant>,
}

impl Installed {
    fn here(driver: &Driver) -> Result<Installed, String> {
        let executable = crate::build::installed()?;
        let running = Some(runeengine::platform::running_program().map_err(|e| e.to_string())?);
        #[cfg(not(test))]
        let database = Some(driver.db_path.clone());
        #[cfg(test)]
        let database = {
            let _ = driver;
            None
        };
        Ok(Installed {
            executable,
            running,
            database,
            held_back_until: None,
        })
    }

    /// Whether another build is installed and ready to take over. Asked only
    /// when nothing is in flight, as often as the host looks.
    fn newer(&self) -> bool {
        // Where a process cannot become another program (Windows), the host
        // serves on, and the newer build is the next host's.
        if !runeengine::platform::REPLACES_IN_PLACE {
            return false;
        }
        let Some(running) = self.running else {
            return false;
        };
        self.held_back_until
            .is_none_or(|until| std::time::Instant::now() >= until)
            && runeengine::platform::FileIdentity::of_path(&self.executable)
                .is_ok_and(|installed| installed != running)
            && crate::build::ready(&self.executable, self.database.as_deref())
    }

    /// A hand-over could not start: try again in a while, serving meanwhile.
    fn hold_back(&mut self) {
        self.held_back_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
    }

    /// Become the installed build, in this process, handing it `record` — and
    /// the sessions' ownership locks, open across the exec, when that build
    /// says it takes them over. One that does not would find its own session
    /// locked by the descriptor it inherited: it gets none, and the locks are
    /// let go here as they always were. Returns only when the exec failed.
    fn exec(
        &self,
        mut record: crate::handover::Handover,
        owners: Vec<(i64, runeengine::lock::Lock)>,
    ) -> String {
        if crate::build::readiness(&self.executable, self.database.as_deref()).adopts_owners {
            for (session, lock) in owners {
                // Last thing before the exec: no program started meanwhile can
                // inherit it, and the successor closes it on exec again. One
                // that cannot be kept open is let go here: the successor's
                // host locks its session anew, as for a build that adopts none.
                if let Ok(fd) = runeengine::platform::hand_on(lock.into_file()) {
                    record.owners.push(crate::handover::Owner { session, fd });
                }
            }
        } else {
            drop(owners);
        }
        let mut successor = runeengine::platform::program_command(&self.executable);
        successor.args(std::env::args_os().skip(1));
        let error = crate::handover::exec(record, &mut successor);
        format!("could not activate the installed Rune: {error}")
    }
}
