use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use runeengine::platform::{FileIdentity, LocalListener, LocalStream};
use runeengine::proto::{Command, Event};

// Protect the accept -> connection-count publication against host retirement.
// Shared across child sockets because one detached engine owns the whole tree.
pub(crate) static ADMISSION_GATE: Mutex<()> = Mutex::new(());

/// Bytes a window may fall behind by before it is dropped. A stalled window is
/// found by its writer's own write timeout; this cap only bounds the memory it
/// can pin meanwhile. Every other window keeps streaming either way.
const CLIENT_BACKLOG_BYTES: usize = 64 * 1024 * 1024;

/// The exit status of a session host that found its session already owned —
/// most often by the session's own host, handing itself on to a newer build.
/// Not a failure: the window that started it connects to that host instead
/// (`driver::HostLauncher::start`).
pub(crate) const OWNED_ELSEWHERE: i32 = 3;

/// Why a session host did not start.
#[derive(Debug)]
pub(crate) enum NotStarted {
    /// Another process holds the session's ownership lock.
    Owned,
    Failed(String),
}

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotStarted::Owned => f.write_str("this session already has a host"),
            NotStarted::Failed(why) => f.write_str(why),
        }
    }
}

impl From<String> for NotStarted {
    fn from(why: String) -> NotStarted {
        NotStarted::Failed(why)
    }
}

/// Ownership locks the program this process was before handed on across its
/// exec (`handover::Handover::owners`), each waiting for its session's host
/// to start — and when it was taken, so one nobody takes is let go.
static ADOPTED: Mutex<Vec<(i64, std::fs::File, std::time::Instant)>> = Mutex::new(Vec::new());

/// Take over the ownership locks handed across the exec. A descriptor is taken
/// only when it is the lock file of the session it names; any other number is
/// left exactly as it was — never closed, since it may be something else this
/// process holds — and that session's host locks anew.
pub(crate) fn adopt_ownership(owners: &[crate::handover::Owner]) {
    for owner in owners {
        let Ok(lock) = session_socket_path(owner.session).map(|p| p.with_extension("Host.lock"))
        else {
            continue;
        };
        if let Some(file) = runeengine::platform::take_handed_file(owner.fd, &lock) {
            runeengine::panic::lock(&ADOPTED).push((
                owner.session,
                file,
                std::time::Instant::now(),
            ));
        }
    }
}

/// The ownership lock handed on for `session`, if one waits — still holding the
/// lock, or it is let go and `None` is answered.
fn adopted(session: i64) -> Option<std::fs::File> {
    let file = {
        let mut waiting = runeengine::panic::lock(&ADOPTED);
        let at = waiting.iter().position(|(id, ..)| *id == session)?;
        waiting.remove(at).1
    };
    // Asking again for the exclusive lock this description already holds
    // succeeds and changes nothing (`flock`); it fails only if another one
    // holds it.
    file.try_lock().is_ok().then_some(file)
}

/// Let go of handed-on locks no host took within `patience`: a session that
/// did not come back must not stay owned by a process that will never serve it.
pub(crate) fn expire_adopted(patience: std::time::Duration) {
    runeengine::panic::lock(&ADOPTED).retain(|(_, _, at)| at.elapsed() < patience);
}

/// Where a retiring host's socket goes, out of its successor's way.
fn retired(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".retired");
    PathBuf::from(name)
}

pub(crate) fn session_socket_path(session: i64) -> Result<PathBuf, String> {
    let home = runeengine::store::Db::home_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(home.join("run")).map_err(|e| e.to_string())?;
    Ok(runeengine::store::jsonl::JsonlStore::new(home).session_socket_path(session))
}

pub(crate) struct SessionClient {
    stream: Arc<Mutex<LocalStream>>,
}

/// Connect to the host at `path` and introduce this window
/// ([`runeengine::access`]). A key read just before the host was replaced is
/// refused by the successor; it is read again, a few times, before giving up.
fn introduced(
    path: &std::path::Path,
) -> Result<(LocalStream, runeengine::access::Introduction), String> {
    use runeengine::access::Answer;
    let mut attempts = 3;
    loop {
        attempts -= 1;
        let stream = LocalStream::connect(path).map_err(|e| e.to_string())?;
        let introduction =
            runeengine::access::introduce(&stream, path).map_err(|e| e.to_string())?;
        match introduction.answer {
            Answer::Admitted | Answer::Older => return Ok((stream, introduction)),
            Answer::Refused if attempts > 0 => continue,
            Answer::Refused => return Err("the session host refused this window's key".to_string()),
            Answer::Retiring => return Err("the session host is activating an update".to_string()),
        }
    }
}

impl SessionClient {
    pub(crate) fn connect(session: i64) -> Result<(SessionClient, Receiver<Event>), String> {
        let path = session_socket_path(session)?;
        let (stream, introduction) = introduced(&path)?;
        let (tx, rx) = channel();
        let runeengine::access::Introduction { early, reader, .. } = introduction;

        std::thread::Builder::new()
            .name(format!("rune-client-rx-{session}"))
            .spawn(move || {
                let mut warned = false;
                for line in early.into_iter().map(Ok).chain(reader.lines()) {
                    let Ok(text) = line else { break };
                    let event = match serde_json::from_str::<Event>(&text) {
                        Ok(event) => event,
                        // A line this build cannot read comes from a host of
                        // another build. Skipping it silently loses deltas and
                        // acks; say so once, and the resume handshake hands the
                        // window over to the installed build.
                        Err(_) if !warned => {
                            warned = true;
                            Event::Log {
                                session: Some(session),
                                entry: runeengine::proto::LogEntry {
                                    at: runeengine::store::now_ms(),
                                    level: runeengine::proto::LogLevel::Warn,
                                    source: "host".into(),
                                    code: "protocol_skew".into(),
                                    message: "This window and its session host are different Rune builds; some updates may be missing until the window is restarted.".into(),
                                    turn: None,
                                    detail: None,
                                },
                            }
                        }
                        Err(_) => continue,
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        Ok((
            SessionClient {
                stream: Arc::new(Mutex::new(stream)),
            },
            rx,
        ))
    }

    pub(crate) fn disconnect(&self) {
        if let Ok(stream) = self.stream.lock() {
            let _ = stream.shutdown();
        }
    }

    pub(crate) fn send(&self, cmd: &Command) -> Result<(), String> {
        let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        let mut stream = self.stream.lock().map_err(|e| e.to_string())?;
        stream
            .write_all(json.as_bytes())
            .map_err(|e| e.to_string())?;
        stream.write_all(b"\n").map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// One connected window, as the host writes to it: a queue drained by the
/// window's own writer thread, so a slow window never blocks the others.
struct Link {
    id: u64,
    queue: Queue,
    stream: LocalStream,
}

/// The sending half of a window's queue, with the bytes still unwritten.
#[derive(Clone)]
struct Queue {
    lines: Sender<Arc<[u8]>>,
    backlog: Arc<std::sync::atomic::AtomicUsize>,
}

impl Queue {
    /// `false` means this window is gone or too far behind.
    fn push(&self, line: &Arc<[u8]>) -> bool {
        if self.backlog.load(Ordering::Acquire) + line.len() > CLIENT_BACKLOG_BYTES {
            return false;
        }
        self.backlog.fetch_add(line.len(), Ordering::AcqRel);
        self.lines.send(Arc::clone(line)).is_ok()
    }
}

impl Link {
    fn open(id: u64, stream: LocalStream) -> std::io::Result<Link> {
        let (lines, queued) = channel::<Arc<[u8]>>();
        let backlog = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut writer = stream.try_clone()?;
        let drained = Arc::clone(&backlog);
        std::thread::Builder::new()
            .name(format!("rune-host-tx-{id}"))
            .spawn(move || {
                for line in queued {
                    // The empty line is `finish`: everything before it is out.
                    if line.is_empty()
                        || writer.write_all(&line).is_err()
                        || writer.flush().is_err()
                    {
                        break;
                    }
                    drained.fetch_sub(line.len(), Ordering::AcqRel);
                }
                let _ = writer.shutdown();
            })?;
        Ok(Link {
            id,
            queue: Queue { lines, backlog },
            stream,
        })
    }

    fn send(&self, line: &Arc<[u8]>) -> bool {
        self.queue.push(line)
    }

    /// Close once every queued line has been written.
    fn finish(self) {
        let _ = self.queue.lines.send(Arc::from(&[][..]));
    }

    /// Close now.
    fn close(self) {
        let _ = self.stream.shutdown();
    }
}

fn line_of(event: &Event) -> Option<Arc<[u8]>> {
    let mut bytes = serde_json::to_vec(event).ok()?;
    bytes.push(b'\n');
    Some(bytes.into())
}

/// Send to every window, dropping the ones that cannot keep up.
fn fan_out(links: &mut Vec<Link>, line: &Arc<[u8]>) {
    let mut kept = Vec::with_capacity(links.len());
    for link in links.drain(..) {
        if link.send(line) {
            kept.push(link);
        } else {
            link.close();
        }
    }
    *links = kept;
}

/// Whether a turn is running, as the events passing through say it. Nothing
/// else about the turn is kept here: what a window that joins needs to be told
/// is the engine's to say (`session.view`), not a host's to reassemble.
#[derive(Default)]
struct LiveTurnState {
    active_turn: Option<i64>,
}

pub(crate) struct SessionHost {
    session: i64,
    /// What a window presents to be heard here (`runeengine::access`).
    key: Arc<runeengine::access::HostKey>,
    /// Where this host listens: the session's socket path, until it retires
    /// and moves out of the way ([`SessionHost::retire`]).
    socket_path: Mutex<PathBuf>,
    socket_identity: FileIdentity,
    ownership: Option<std::fs::File>,
    /// The listening socket, for waking the accept loop on the way out — by
    /// the socket, never by its path, which may be gone (`LocalListener::wake`).
    waker: LocalListener,
    accept_thread: Option<std::thread::JoinHandle<()>>,
    forward_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Admits new commands. Cleared by `retire`; commands arriving afterwards
    /// are refused with a retry ack rather than dropped.
    active: Arc<AtomicBool>,
    /// The host object still exists. Cleared on drop, which is what ends the
    /// forwarder of a host nobody owns any more.
    alive: Arc<AtomicBool>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    children: Arc<Mutex<Vec<SessionHost>>>,
    pending_open: Arc<Mutex<std::collections::HashSet<u64>>>,
    live_turn: Arc<Mutex<LiveTurnState>>,
}

impl SessionHost {
    pub(crate) fn register_open(&self, id: u64) {
        runeengine::panic::lock(&self.pending_open).insert(id);
    }

    /// Whether any process is currently attached to this session.
    pub(crate) fn has_clients(&self) -> bool {
        self.connections.load(Ordering::Relaxed) > 0
            || runeengine::panic::lock(&self.children)
                .iter()
                .any(Self::has_clients)
    }

    pub(crate) fn sessions(&self) -> Vec<i64> {
        let mut result = vec![self.session];
        for child in runeengine::panic::lock(&self.children).iter() {
            result.extend(child.sessions());
        }
        result
    }

    pub(crate) fn is_busy(&self) -> bool {
        runeengine::panic::lock(&self.live_turn)
            .active_turn
            .is_some()
            || runeengine::panic::lock(&self.children)
                .iter()
                .any(Self::is_busy)
    }

    /// Stop admitting commands, here and in every child. Called under
    /// `ADMISSION_GATE` once the host's queue is empty.
    /// Stop admitting commands, and windows. The socket leaves the session's
    /// path for a private name before any window hears `HostUpgrading`, so a
    /// window reconnecting meets the successor or nobody — never this host
    /// on its way out, whose connection would die with its `exec` and leave
    /// the window talking to no one while the successor saw no window at all.
    /// Kept under that name until the host goes, which removes it.
    pub(crate) fn retire(&self) {
        self.active.store(false, Ordering::Release);
        let mut path = runeengine::panic::lock(&self.socket_path);
        let retired = retired(&path);
        if FileIdentity::of_entry(&path).is_ok_and(|entry| entry == self.socket_identity)
            && std::fs::rename(&*path, &retired).is_ok()
        {
            *path = retired;
        }
        drop(path);
        for child in runeengine::panic::lock(&self.children).iter() {
            child.retire();
        }
    }

    /// Wait until every forwarder in the tree has delivered its
    /// `HostUpgrading` and closed its windows, or `deadline` passes.
    pub(crate) fn drain(&self, deadline: std::time::Instant) {
        for child in runeengine::panic::lock(&self.children).iter() {
            child.drain(deadline);
        }
        if let Some(thread) = runeengine::panic::lock(&self.forward_thread).take() {
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }

    /// Take the ownership locks out of this host and its children, for the
    /// build it `exec`s into: the sessions stay owned through the exec, and
    /// dropping the host afterwards releases none of them.
    pub(crate) fn hand_over_ownership(&mut self) -> Vec<(i64, std::fs::File)> {
        let mut owned: Vec<_> = self
            .ownership
            .take()
            .map(|lock| (self.session, lock))
            .into_iter()
            .collect();
        for child in runeengine::panic::lock(&self.children).iter_mut() {
            owned.extend(child.hand_over_ownership());
        }
        owned
    }

    pub(crate) fn release_children(&self, sessions: &[i64]) {
        let mut children = runeengine::panic::lock(&self.children);
        children.retain(|child| {
            child.release_children(sessions);
            !sessions.contains(&child.session) || child.has_clients()
        });
    }

    pub(crate) fn owns_clients(&self, session: i64) -> bool {
        if self.session == session {
            self.connections.load(Ordering::Acquire) > 0
        } else {
            self.children
                .lock()
                .unwrap()
                .iter()
                .any(|child| child.owns_clients(session))
        }
    }

    pub(crate) fn start(
        session: i64,
        msg_tx: Sender<crate::ui::Msg>,
        incoming: Receiver<Event>,
    ) -> Result<(SessionHost, Receiver<Event>), NotStarted> {
        let path = session_socket_path(session)?;
        // The lock the build before this one held, handed on across its exec,
        // or a new one: the session is never without an owner in between.
        let ownership = match adopted(session) {
            Some(handed) => handed,
            None => match runeengine::lock::exclusive(
                &path.with_extension("Host.lock"),
                runeengine::lock::Wait::No,
            ) {
                Ok(lock) => lock,
                Err(runeengine::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(NotStarted::Owned)
                }
                Err(e) => return Err(NotStarted::Failed(e.to_string())),
            },
        };
        if let Ok(found) = FileIdentity::of_entry(&path) {
            if !runeengine::platform::is_socket(&path) || LocalStream::connect(&path).is_ok() {
                return Err(format!("Session #{session} socket is already in use").into());
            }
            // The host lock is ours, so no host owns this socket any more.
            // Re-check its identity; never unlink an artifact replaced meanwhile.
            if FileIdentity::of_entry(&path).is_ok_and(|current| current == found) {
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            } else {
                return Err(NotStarted::Failed(
                    "Session socket changed during ownership reconciliation".into(),
                ));
            }
        }
        // A predecessor that retired and then `exec`d into this build never
        // got to remove its socket: nobody listens there any more.
        let stale = retired(&path);
        if runeengine::platform::is_socket(&stale) && LocalStream::connect(&stale).is_err() {
            let _ = std::fs::remove_file(&stale);
        }
        // Issued before the socket exists: a window that reaches this host can
        // already read the key it must present (`runeengine::access`).
        let key = Arc::new(runeengine::access::HostKey::issue(&path).map_err(|e| e.to_string())?);
        let listener = LocalListener::bind(&path).map_err(|e| e.to_string())?;
        // Kept to wake the accept loop by the socket itself (`Drop`).
        let waker = listener.try_clone().map_err(|e| e.to_string())?;
        let socket_identity = FileIdentity::of_entry(&path).map_err(|e| e.to_string())?;
        let links: Arc<Mutex<Vec<Link>>> = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live_turn = Arc::new(Mutex::new(LiveTurnState::default()));
        let children = Arc::new(Mutex::new(Vec::new()));
        let siblings: Arc<Mutex<Vec<Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let pending_open = Arc::new(Mutex::new(std::collections::HashSet::new()));
        // Which window asked for which request: its ack goes to that window only.
        let owners: Arc<Mutex<std::collections::HashMap<u64, u64>>> = Arc::default();

        let (ui_tx, ui_rx) = channel();

        let forward_thread = {
            let links = Arc::clone(&links);
            let alive = Arc::clone(&alive);
            let live_turn = Arc::clone(&live_turn);
            let siblings = Arc::clone(&siblings);
            let children = Arc::clone(&children);
            let pending_open = Arc::clone(&pending_open);
            let owners = Arc::clone(&owners);
            let msg_tx = msg_tx.clone();
            std::thread::Builder::new()
                .name(format!("rune-host-events-{session}"))
                .spawn(move || {
                    for event in incoming {
                        if !alive.load(Ordering::Acquire) {
                            break;
                        }
                        // A fresh open creates another session in this engine.
                        // Start its socket before the opener sees the ack and
                        // sends its first prompt. The original host keeps its
                        // own stream and clients: moving one window via `/new`
                        // never stops anyone still watching the original session.
                        let mut new_child = None;
                        if let Event::Ack { id, ok, data, .. } = &event {
                            if runeengine::panic::lock(&pending_open).remove(id) && *ok {
                                if let Some(child_id) = data
                                    .as_ref()
                                    .and_then(|d| d.get("session"))
                                    .and_then(serde_json::Value::as_i64)
                                    .filter(|s| *s != session)
                                {
                                    let path = session_socket_path(child_id);
                                    if !path.as_ref().map(|p| p.exists()).unwrap_or(false) {
                                        let (child_tx, child_rx) = channel();
                                        if let Ok((child, _)) =
                                            SessionHost::start(child_id, msg_tx.clone(), child_rx)
                                        {
                                            new_child = Some((child_tx, child));
                                        }
                                    }
                                }
                            }
                        }
                        // The engine's event channel is global, but this socket
                        // is owned by exactly one session. Only that session's
                        // events may enter its live snapshot, local UI, or
                        // connected clients; the rest go down the host tree.
                        if let Some(owner) = event.session() {
                            if owner != session {
                                let mut siblings = runeengine::panic::lock(&siblings);
                                siblings.retain(|tx| tx.send(event.clone()).is_ok());
                                if let Some((child_tx, child)) = new_child {
                                    siblings.push(child_tx);
                                    runeengine::panic::lock(&children).push(child);
                                }
                                continue;
                            }
                        }
                        // An ack goes to the window that asked. One this host
                        // did not route came through a child socket, or from
                        // this process's own UI, so it travels down the tree —
                        // to the children that existed before it: the ack that
                        // created a child is not that child's to show.
                        let requester = match &event {
                            Event::Ack { id, .. } => {
                                let owner = runeengine::panic::lock(&owners).remove(id);
                                if owner.is_none() {
                                    let mut siblings = runeengine::panic::lock(&siblings);
                                    siblings.retain(|tx| tx.send(event.clone()).is_ok());
                                }
                                owner
                            }
                            _ => None,
                        };
                        if let Some((child_tx, child)) = new_child {
                            runeengine::panic::lock(&siblings).push(child_tx);
                            runeengine::panic::lock(&children).push(child);
                        }
                        let upgrading = matches!(event, Event::HostUpgrading { .. });
                        let mut st = runeengine::panic::lock(&live_turn);
                        match &event {
                            Event::TurnStart { turn, .. } => st.active_turn = Some(*turn),
                            Event::Done { .. } => st.active_turn = None,
                            _ => {}
                        }
                        let _ = ui_tx.send(event.clone());
                        if let Some(line) = line_of(&event) {
                            let mut held = runeengine::panic::lock(&links);
                            match requester {
                                Some(owner) => {
                                    if let Some(at) = held.iter().position(|l| l.id == owner) {
                                        if !held[at].send(&line) {
                                            held.remove(at).close();
                                        }
                                    }
                                }
                                None => fan_out(&mut held, &line),
                            }
                            if upgrading {
                                // Every window has its last line queued; each
                                // writer flushes it and closes the window.
                                for link in held.drain(..) {
                                    link.finish();
                                }
                            }
                        }
                        drop(st);
                        if upgrading {
                            break;
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };

        let accept_thread = {
            let links = Arc::clone(&links);
            let active = Arc::clone(&active);
            let connections = Arc::clone(&connections);
            let pending_open = Arc::clone(&pending_open);
            let owners = Arc::clone(&owners);
            let next_link = Arc::new(AtomicU64::new(1));
            let unadmitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let key = Arc::clone(&key);
            std::thread::Builder::new()
                .name(format!("rune-host-accept-{session}"))
                .spawn(move || {
                    for stream in listener.incoming() {
                        if !active.load(Ordering::Acquire) {
                            break;
                        }
                        let Ok(client) = stream else { continue };
                        let _ =
                            client.set_write_timeout(Some(std::time::Duration::from_millis(500)));
                        // A window that joins asks for the session's view and
                        // drops what it hears before the answer; nothing is
                        // handed to it here.
                        admit(Admission {
                            session,
                            client,
                            link: next_link.fetch_add(1, Ordering::Relaxed),
                            key: Arc::clone(&key),
                            links: Arc::clone(&links),
                            unadmitted: Arc::clone(&unadmitted),
                            msg_tx: msg_tx.clone(),
                            pending_open: Arc::clone(&pending_open),
                            active: Arc::clone(&active),
                            connections: Arc::clone(&connections),
                            owners: Arc::clone(&owners),
                        });
                    }
                })
                .map_err(|e| e.to_string())?
        };

        Ok((
            SessionHost {
                session,
                key,
                socket_path: Mutex::new(path),
                socket_identity,
                ownership: Some(ownership),
                waker,
                accept_thread: Some(accept_thread),
                forward_thread: Mutex::new(Some(forward_thread)),
                active,
                alive,
                connections,
                children,
                pending_open,
                live_turn,
            },
            ui_rx,
        ))
    }
}

/// Everything one window's command reader needs.
struct Reader {
    session: i64,
    link: u64,
    own: Queue,
    msg_tx: Sender<crate::ui::Msg>,
    pending_open: Arc<Mutex<std::collections::HashSet<u64>>>,
    active: Arc<AtomicBool>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    owners: Arc<Mutex<std::collections::HashMap<u64, u64>>>,
}

/// A connection not yet admitted, and what admitting it takes.
struct Admission {
    session: i64,
    client: LocalStream,
    link: u64,
    key: Arc<runeengine::access::HostKey>,
    links: Arc<Mutex<Vec<Link>>>,
    unadmitted: Arc<std::sync::atomic::AtomicUsize>,
    msg_tx: Sender<crate::ui::Msg>,
    pending_open: Arc<Mutex<std::collections::HashSet<u64>>>,
    active: Arc<AtomicBool>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    owners: Arc<Mutex<std::collections::HashMap<u64, u64>>>,
}

/// Windows of the build before keys a host keeps answering at once; past it a
/// connection that cannot introduce itself is closed.
const MAX_UNADMITTED: usize = 8;

/// Prove the peer, then serve it — on the connection's own thread, so a peer
/// that never speaks holds up nobody. Until the peer is admitted it is in no
/// fan-out, counts as no window and reaches no engine
/// ([`runeengine::access`]).
fn admit(a: Admission) {
    use runeengine::access::{self, Presented};
    std::thread::spawn(move || {
        if !a.client.peer_is_same_user() {
            return;
        }
        let answer = |line: String| {
            let _ = (&a.client).write_all(format!("{line}\n").as_bytes());
        };
        let _ = a.client.set_read_timeout(Some(access::PATIENCE));
        let Ok(read_half) = a.client.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(read_half);
        let mut first = String::new();
        if !matches!(reader.read_line(&mut first), Ok(n) if n > 0) {
            return;
        }
        match access::presented(first.trim_end()) {
            Presented::Hello(Some(key)) if a.key.admits(&key) => {}
            Presented::Hello(_) => return answer(access::refused_line()),
            Presented::Other => {
                if a.unadmitted.fetch_add(1, Ordering::AcqRel) < MAX_UNADMITTED {
                    let _ = a.client.set_read_timeout(None);
                    answer_older_window(&a.client, first, reader);
                }
                a.unadmitted.fetch_sub(1, Ordering::AcqRel);
                return;
            }
        }
        let _ = a.client.set_read_timeout(None);
        let admission = runeengine::panic::lock(&ADMISSION_GATE);
        if !a.active.load(Ordering::Acquire) {
            return answer(access::retiring_line());
        }
        let Ok(link) = a.client.try_clone().and_then(|s| Link::open(a.link, s)) else {
            return;
        };
        // The answer is this window's first line; the stream follows it.
        let own = link.queue.clone();
        own.push(&Arc::from(
            format!("{}\n", access::admitted_line()).into_bytes(),
        ));
        a.connections.fetch_add(1, Ordering::AcqRel);
        runeengine::panic::lock(&a.links).push(link);
        drop(admission);
        serve(
            Reader {
                session: a.session,
                link: a.link,
                own,
                msg_tx: a.msg_tx,
                pending_open: a.pending_open,
                active: a.active,
                connections: a.connections,
                owners: a.owners,
            },
            reader,
        );
    });
}

/// A window of the build before keys (0.2.1), which cannot introduce itself.
/// Nothing it sends goes further and it hears nothing of the session. Its
/// resume alone is answered as a success carrying only this host's protocol
/// and version — public facts, `rune -version` tells anyone — and
/// `resumed: false`: that build reads the protocol only from a successful
/// resume (`protocol_skew`), and a mismatch is what makes it hand itself over
/// to the installed build at once, carrying its input, instead of waiting for
/// an empty input that its refused composer edits never let it reach. Every
/// other command is refused. Only for the previous release, like the handover
/// record's previous format (`handover.rs`): removable once no window of the
/// build before keys can still be running.
///
/// The connection is kept, not cut: a window that loses its host looks for it
/// again at once, and would reconnect in a loop (`Msg::HostGone`).
fn answer_older_window(client: &LocalStream, first: String, reader: BufReader<LocalStream>) {
    for line in std::iter::once(Ok(first)).chain(reader.lines()) {
        let Ok(text) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            break;
        };
        let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
            break;
        };
        let answer = if value["cmd"] == "session.resume" {
            Event::Ack {
                id,
                ok: true,
                error: None,
                data: Some(serde_json::json!({
                    "protocol": runeengine::proto::PROTOCOL,
                    "version": runeengine::VERSION,
                    "resumed": false,
                })),
            }
        } else {
            Event::Ack {
                id,
                ok: false,
                error: Some(
                    "This session host runs a newer Rune build; this window is switching to it."
                        .into(),
                ),
                data: Some(serde_json::json!({ "protocol": runeengine::proto::PROTOCOL })),
            }
        };
        let Some(line) = line_of(&answer) else {
            break;
        };
        if (&*client).write_all(&line).is_err() {
            break;
        }
    }
}

/// Run an admitted window's commands, on its connection's thread.
fn serve(r: Reader, reader: BufReader<LocalStream>) {
    {
        struct ConnGuard {
            count: Arc<std::sync::atomic::AtomicUsize>,
            sender: Sender<crate::ui::Msg>,
            session: i64,
        }
        impl Drop for ConnGuard {
            fn drop(&mut self) {
                if self.count.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let _ = self
                        .sender
                        .send(crate::ui::Msg::SessionDetached(self.session));
                }
            }
        }
        let _guard = ConnGuard {
            count: Arc::clone(&r.connections),
            sender: r.msg_tx.clone(),
            session: r.session,
        };
        // Answer this window directly — never through the fan-out.
        let reply = |id: u64, ok: bool, error: Option<String>, data: Option<serde_json::Value>| {
            if let Some(line) = line_of(&Event::Ack {
                id,
                ok,
                error,
                data,
            }) {
                r.own.push(&line);
            }
        };
        for line in reader.lines() {
            let Ok(text) = line else { break };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            // The socket represents exactly one session; do not dispatch
            // another session's command into this engine.
            if value
                .get("session")
                .is_some_and(|v| v.as_i64() != Some(r.session))
            {
                continue;
            }
            let id = value.get("id").and_then(serde_json::Value::as_u64);
            let Ok(cmd) = serde_json::from_value::<Command>(value) else {
                if let Some(id) = id {
                    reply(
                        id,
                        false,
                        Some("This session host is a different Rune build and does not know this command; restart the window.".into()),
                        Some(serde_json::json!({"protocol": runeengine::proto::PROTOCOL})),
                    );
                }
                continue;
            };
            if let Command::SessionClients { id, session } = cmd {
                let count = r.connections.load(Ordering::Acquire);
                reply(
                    id,
                    true,
                    None,
                    Some(serde_json::json!({ "count": count, "session": session })),
                );
                continue;
            }
            let request = cmd.request_id();
            let _admission = runeengine::panic::lock(&ADMISSION_GATE);
            if !r.active.load(Ordering::Acquire) {
                // Retiring: this command was not accepted. Say so, so the window
                // sends it again to the host that replaces this one.
                reply(
                    request,
                    false,
                    Some(
                        "The session host is activating an update; the command will be resent."
                            .into(),
                    ),
                    Some(serde_json::json!({ "retry": true })),
                );
                continue;
            }
            if matches!(cmd, Command::SessionOpen { fresh: true, .. }) {
                runeengine::panic::lock(&r.pending_open).insert(request);
            }
            runeengine::panic::lock(&r.owners).insert(request, r.link);
            if r.msg_tx.send(crate::ui::Msg::RemoteCommand(cmd)).is_err() {
                break;
            }
        }
    }
}

impl Drop for SessionHost {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.alive.store(false, Ordering::Release);
        runeengine::panic::lock(&self.children).clear();
        // Wake blocking accept, then join before releasing exclusive ownership.
        let path = runeengine::panic::lock(&self.socket_path).clone();
        let _ = self.waker.wake();
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        if FileIdentity::of_entry(&path).is_ok_and(|entry| entry == self.socket_identity) {
            let _ = std::fs::remove_file(&path);
        }
        // Still under the ownership lock: no successor has issued its key.
        self.key.withdraw();
        self.ownership.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A retiring host is out of the way before any window hears it is
    /// upgrading: connecting to the session's path reaches nobody until the
    /// successor binds it, and dropping the host still ends its accept loop
    /// and removes its socket.
    #[test]
    fn a_retired_host_cannot_be_reached_at_the_sessions_path() {
        let session = 987_669;
        let (msg_tx, _msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let path = session_socket_path(session).unwrap();
        assert!(LocalStream::connect(&path).is_ok());
        host.retire();
        assert!(
            LocalStream::connect(&path).is_err(),
            "a window can reach the retiring host"
        );
        assert!(retired(&path).exists());
        drop(host);
        assert!(
            !retired(&path).exists(),
            "the retired socket was left behind"
        );
        // The successor binds the session's path as if nothing had been there.
        let (msg_tx, _msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (next, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        assert!(LocalStream::connect(&path).is_ok());
        drop(next);
    }

    /// A host whose socket file is gone still ends: it used to wake its accept
    /// loop by connecting to that file, and hung in its shutdown forever when
    /// the file had been removed (a test host found alive 29 minutes after its
    /// home was deleted, its main thread joining the accept thread).
    #[test]
    fn a_host_whose_socket_file_is_gone_still_ends() {
        let session = 987_671;
        let (msg_tx, _msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let path = session_socket_path(session).unwrap();
        std::fs::remove_file(&path).unwrap();
        let (done_tx, done_rx) = channel();
        std::thread::spawn(move || {
            drop(host);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok(),
            "the host hung in its shutdown"
        );
    }

    #[test]
    fn retired_host_does_not_admit_a_new_writer() {
        let session = 987_661;
        let (msg_tx, msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        {
            let _admission = runeengine::panic::lock(&ADMISSION_GATE);
            assert!(!host.has_clients());
            host.retire();
        }
        if let Ok((client, events)) = SessionClient::connect(session) {
            let _ = client.send(&Command::Cancel {
                id: 1,
                session,
                call: None,
            });
            assert!(events.recv_timeout(Duration::from_secs(2)).is_err());
        }
        assert!(msg_rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(!host.has_clients());
    }

    /// Audit R1: a command that reaches a retiring host is refused with a retry
    /// ack — never dropped in silence, which left the window waiting forever.
    #[test]
    fn a_command_after_retirement_is_refused_with_a_retry() {
        let session = 987_670;
        let (msg_tx, msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let (client, events) = SessionClient::connect(session).unwrap();
        // A first command proves the window was admitted.
        client
            .send(&Command::Cancel {
                id: 1,
                session,
                call: None,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(_)
        ));
        {
            let _admission = runeengine::panic::lock(&ADMISSION_GATE);
            host.retire();
        }
        client
            .send(&Command::Cancel {
                id: 2,
                session,
                call: None,
            })
            .unwrap();
        let Event::Ack { id, ok, data, .. } = events.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("the refusal is an ack");
        };
        assert_eq!((id, ok), (2, false));
        assert_eq!(data.unwrap()["retry"], serde_json::json!(true));
        assert!(
            msg_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "not admitted"
        );
    }

    /// Audit R1: the upgrade notice travels behind the acks of every command the
    /// host accepted, and then the window is closed.
    #[test]
    fn the_upgrade_notice_follows_every_accepted_ack_then_closes() {
        let session = 987_671;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let (client, events) = SessionClient::connect(session).unwrap();
        client
            .send(&Command::Cancel {
                id: 7,
                session,
                call: None,
            })
            .unwrap();
        msg_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        event_tx
            .send(Event::Ack {
                id: 7,
                ok: true,
                error: None,
                data: None,
            })
            .unwrap();
        event_tx.send(Event::HostUpgrading { session }).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Ack {
                id: 7,
                ok: true,
                ..
            }
        ));
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::HostUpgrading { session: s } if s == session
        ));
        host.drain(std::time::Instant::now() + Duration::from_secs(2));
        assert!(
            matches!(
                events.recv_timeout(Duration::from_secs(2)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "the window is closed after the notice"
        );
    }

    /// Audit R9: a window that stops reading falls behind alone and is dropped;
    /// it never holds back the stream of the others.
    #[test]
    fn a_stalled_window_is_dropped_without_stalling_the_others() {
        let (stalled, _never_read) = LocalStream::pair().unwrap();
        stalled
            .set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let (healthy, healthy_end) = LocalStream::pair().unwrap();
        let mut links = vec![
            Link::open(1, stalled).unwrap(),
            Link::open(2, healthy).unwrap(),
        ];
        const LINES: usize = 50_000;
        let reader = std::thread::spawn(move || BufReader::new(healthy_end).lines().count());
        let line: Arc<[u8]> = Arc::from(&b"{\"t\":\"x\"}\n"[..]);
        let started = std::time::Instant::now();
        for _ in 0..LINES {
            fan_out(&mut links, &line);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fan-out never blocks"
        );
        // The stalled writer's own write timeout is what finds it.
        std::thread::sleep(Duration::from_millis(300));
        fan_out(&mut links, &line);
        assert_eq!(links.iter().map(|l| l.id).collect::<Vec<_>>(), vec![2]);
        for link in links {
            link.finish();
        }
        assert_eq!(
            reader.join().unwrap(),
            LINES + 1,
            "the healthy window got every line"
        );
    }

    #[test]
    fn dropping_last_client_notifies_only_its_session() {
        let session = 987_660;
        let (msg_tx, msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let (first, _first_events) = SessionClient::connect(session).unwrap();
        let (second, _second_events) = SessionClient::connect(session).unwrap();
        // A command barrier proves both connections have been accepted.
        second
            .send(&Command::Cancel {
                id: 1,
                session,
                call: None,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(_)
        ));
        drop(first);
        assert!(
            msg_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "another watcher must preserve the turn"
        );
        assert!(host.owns_clients(session));
        assert!(!host.owns_clients(session + 1));
        drop(second);
        assert!(
            matches!(msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(), crate::ui::Msg::SessionDetached(id) if id == session)
        );
        assert!(!host.owns_clients(session));
    }

    /// The attack, at the socket: a process of this user that cannot read the
    /// host's key — a command the agent ran, confined — connects anyway.
    /// Whatever it sends first, it hears nothing of the session, reaches no
    /// engine and counts as no window; a window with the key is served as
    /// before, beside it.
    #[test]
    fn only_a_window_holding_the_hosts_key_is_heard_or_told_anything() {
        let session = 987_700;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let path = session_socket_path(session).unwrap();
        let lines = |stream: &LocalStream| {
            stream
                .set_read_timeout(Some(Duration::from_millis(600)))
                .unwrap();
            let mut seen = Vec::new();
            for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                match line {
                    Ok(line) => seen.push(line),
                    Err(_) => break,
                }
            }
            seen
        };

        // Straight to a command, as a confined command would.
        let mut blind = LocalStream::connect(&path).unwrap();
        writeln!(
            blind,
            "{}",
            serde_json::json!({"cmd":"session.mode","id":7,"session":session,"mode":"full"})
        )
        .unwrap();
        // A hello with a guessed key. The host may hang up on it before the
        // second line is written — that refusal is what is tested, so a write
        // the closed connection turns down is not a failure.
        let mut guess = LocalStream::connect(&path).unwrap();
        writeln!(
            guess,
            "{}",
            serde_json::json!({"cmd": runeengine::access::HELLO, "id": 0, "key": "0".repeat(64)})
        )
        .unwrap();
        let _ = writeln!(
            guess,
            "{}",
            serde_json::json!({"cmd":"session.mode","id":8,"session":session,"mode":"full"})
        );

        // The window that holds the key, and something the session says.
        let (client, client_rx) = SessionClient::connect(session).unwrap();
        event_tx
            .send(Event::Title {
                session,
                title: "only for windows".into(),
            })
            .unwrap();
        assert!(matches!(
            client_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Title { .. }
        ));

        let heard_blind = lines(&blind);
        assert_eq!(heard_blind.len(), 1, "{heard_blind:?}");
        let answer: Event = serde_json::from_str(&heard_blind[0]).unwrap();
        assert!(
            matches!(
                answer,
                Event::Ack {
                    id: 7,
                    ok: false,
                    ..
                }
            ),
            "{answer:?}"
        );
        let heard_guess = lines(&guess);
        assert_eq!(heard_guess, vec![runeengine::access::refused_line()]);
        for heard in heard_blind.iter().chain(&heard_guess) {
            assert!(!heard.contains("only for windows"), "{heard}");
        }
        // Nothing they sent reached the engine; only the real window counts.
        while let Ok(msg) = msg_rx.try_recv() {
            assert!(
                !matches!(msg, crate::ui::Msg::RemoteCommand(_)),
                "a command without the key reached the engine"
            );
        }
        assert_eq!(host.connections.load(Ordering::Acquire), 1);
        drop(client);
        drop(host);
        assert!(
            !runeengine::access::key_path(&path).exists(),
            "a host that is gone leaves no key behind"
        );
    }

    /// A window of the build before keys is told what moves it on and
    /// nothing else: its resume, this host's protocol and version only (that
    /// build hands itself over on the mismatch, carrying its input); anything
    /// else, refused. It hears nothing of the session, reaches no engine and
    /// counts as no window.
    #[test]
    fn a_window_of_the_build_before_keys_is_told_only_what_moves_it_on() {
        let session = 987_701;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let path = session_socket_path(session).unwrap();
        let mut old = LocalStream::connect(&path).unwrap();
        writeln!(
            old,
            "{}",
            serde_json::json!({"cmd":"session.resume","id":5,"session":session})
        )
        .unwrap();
        writeln!(
            old,
            "{}",
            serde_json::json!({"cmd":"session.mode","id":6,"session":session,"mode":"full"})
        )
        .unwrap();
        event_tx
            .send(Event::Title {
                session,
                title: "only for windows".into(),
            })
            .unwrap();

        old.set_read_timeout(Some(Duration::from_millis(800)))
            .unwrap();
        let mut heard = Vec::new();
        for line in BufReader::new(old.try_clone().unwrap()).lines() {
            match line {
                Ok(line) => heard.push(line),
                Err(_) => break,
            }
        }
        assert_eq!(heard.len(), 2, "{heard:?}");
        match serde_json::from_str::<Event>(&heard[0]).unwrap() {
            Event::Ack {
                id: 5,
                ok: true,
                data: Some(data),
                ..
            } => assert_eq!(
                data,
                serde_json::json!({
                    "protocol": runeengine::proto::PROTOCOL,
                    "version": runeengine::VERSION,
                    "resumed": false,
                }),
                "the resume carries nothing of the session"
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            serde_json::from_str::<Event>(&heard[1]).unwrap(),
            Event::Ack {
                id: 6,
                ok: false,
                ..
            }
        ));
        assert!(heard.iter().all(|line| !line.contains("only for windows")));
        while let Ok(msg) = msg_rx.try_recv() {
            assert!(
                !matches!(msg, crate::ui::Msg::RemoteCommand(_)),
                "a command without the key reached the engine"
            );
        }
        assert_eq!(host.connections.load(Ordering::Acquire), 0);
        drop(host);
    }

    /// A session's ownership lock goes from host to host and never lapses:
    /// handed out of a retiring host, it outlives that host; handed across the
    /// exec, the next host takes it over while any other host is refused. A
    /// number that is not the lock is neither taken nor closed, and a lock no
    /// host takes is let go.
    #[test]
    fn a_sessions_ownership_is_handed_on_and_never_lapses() {
        use runeengine::lock::{exclusive, Wait};
        use runeengine::platform::{fixture::handed_number, hand_on};
        let session = 987_710;
        let path = session_socket_path(session).unwrap();
        let lock = path.with_extension("Host.lock");
        let start = || {
            let (msg_tx, _msg_rx) = channel();
            let (_event_tx, event_rx) = channel();
            SessionHost::start(session, msg_tx, event_rx)
        };

        // Out of a retiring host: dropping the host releases nothing.
        let (mut first, _ui) = start().unwrap();
        let handed = first.hand_over_ownership();
        assert_eq!(handed.len(), 1);
        drop(first);
        assert!(
            matches!(start(), Err(NotStarted::Owned)),
            "owned through the gap"
        );

        // A stray number is not taken, and stays open.
        let stray = std::fs::File::create(path.with_extension("stray")).unwrap();
        adopt_ownership(&[crate::handover::Owner {
            session,
            fd: handed_number(&stray),
        }]);
        assert!((&stray).write_all(b"still open").is_ok());

        // Across the exec: the next host takes it over; another is refused.
        let (_, held) = handed.into_iter().next().unwrap();
        adopt_ownership(&[crate::handover::Owner {
            session,
            fd: hand_on(held).unwrap(),
        }]);
        let (next, _ui) = start().unwrap();
        assert!(matches!(start(), Err(NotStarted::Owned)));
        drop(next);
        assert!(exclusive(&lock, Wait::No).is_ok(), "released when it ends");

        // Handed on and never taken: let go.
        let waiting = exclusive(&lock, Wait::No).unwrap();
        adopt_ownership(&[crate::handover::Owner {
            session,
            fd: hand_on(waiting).unwrap(),
        }]);
        assert!(exclusive(&lock, Wait::No).is_err(), "held while it waits");
        expire_adopted(Duration::ZERO);
        assert!(
            exclusive(&lock, Wait::No).is_ok(),
            "let go once nobody took it"
        );
        let _ = std::fs::remove_file(path.with_extension("stray"));
    }

    #[test]
    fn test_ipc_host_and_client_roundtrip() {
        let session = 999_999;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();

        let (host, ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let (client, client_rx) = SessionClient::connect(session).unwrap();

        client
            .send(&Command::Cancel {
                id: 42,
                session,
                call: None,
            })
            .unwrap();

        let msg = msg_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match msg {
            crate::ui::Msg::RemoteCommand(Command::Cancel { id, session: s, .. }) => {
                assert_eq!(id, 42);
                assert_eq!(s, session);
            }
            _ => panic!("unexpected msg"),
        }

        event_tx
            .send(Event::Title {
                session,
                title: "Test Sync".into(),
            })
            .unwrap();

        let host_event = ui_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(host_event, Event::Title { session: s, title: t } if s == session && t == "Test Sync")
        );

        let client_event = client_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(client_event, Event::Title { session: s, title: t } if s == session && t == "Test Sync")
        );

        drop(client);
        drop(host);
    }

    #[test]
    fn child_client_keeps_parent_host_owned_after_new() {
        let parent_id = 987_651;
        let child_id = 987_652;
        let (msg_tx, _msg_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (parent, _ui_rx) = SessionHost::start(parent_id, msg_tx, event_rx).unwrap();
        parent.register_open(42);
        event_tx
            .send(Event::Ack {
                id: 42,
                ok: true,
                error: None,
                data: Some(serde_json::json!({"session": child_id})),
            })
            .unwrap();
        let (child, _events) = SessionClient::connect(child_id).unwrap_or_else(|_| {
            // The host event forwarder creates the child socket asynchronously.
            for _ in 0..100 {
                if let Ok(client) = SessionClient::connect(child_id) {
                    return client;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("child socket was not created")
        });
        for _ in 0..100 {
            if parent.has_clients() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            parent.has_clients(),
            "child window must own the parent engine"
        );
        assert!(
            !parent.owns_clients(parent_id),
            "child watcher must not own parent session"
        );
        assert!(parent.owns_clients(child_id));
        child.disconnect();
        for _ in 0..100 {
            if !parent.has_clients() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !parent.has_clients(),
            "closed child must not retain the host"
        );
    }

    #[test]
    fn socket_rejects_commands_naming_another_session() {
        let session = 987_653;
        let (msg_tx, msg_rx) = channel();
        let (_event_tx, event_rx) = channel();
        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        let (client, _rx) = SessionClient::connect(session).unwrap();
        client
            .send(&Command::Cancel {
                id: 1,
                session: session + 1,
                call: None,
            })
            .unwrap();
        client
            .send(&Command::Prompt {
                id: 2,
                session: session + 1,
                text: "foreign".into(),
                attach: vec![],
            })
            .unwrap();
        client
            .send(&Command::Cancel {
                id: 3,
                session,
                call: None,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::Cancel { id: 3, session: s, .. }) if s == session
        ));
        assert!(msg_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(client);
        drop(host);
    }

    /// A window that connects mid-turn is handed nothing by the host: it asks
    /// the engine for the session's view and drops what it hears before the
    /// answer. What the host does is carry the live stream on to it.
    #[test]
    fn a_window_that_connects_mid_turn_hears_the_stream_from_then_on() {
        let session = 999_998;
        let (msg_tx, _msg_rx) = channel();
        let (event_tx, event_rx) = channel();

        let (host, _ui_rx) = SessionHost::start(session, msg_tx, event_rx).unwrap();
        event_tx
            .send(Event::Text {
                session,
                delta: "before".into(),
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let (client, client_rx) = SessionClient::connect(session).unwrap();
        assert!(
            client_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing is replayed to a joiner: that is the view's job"
        );
        event_tx
            .send(Event::Text {
                session,
                delta: " after".into(),
            })
            .unwrap();
        let next = client_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(next, Event::Text { delta, .. } if delta == " after"));

        drop(client);
        drop(host);
    }

    #[test]
    fn two_windows_stay_isolated_after_one_opens_a_new_session() {
        let old = 998_991;
        let next = 998_992;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();
        let (host, old_ui) = SessionHost::start(old, msg_tx, event_rx).unwrap();
        let (moving, moving_rx) = SessionClient::connect(old).unwrap();
        let (watcher, watcher_rx) = SessionClient::connect(old).unwrap();
        watcher
            .send(&Command::SessionTouch {
                id: 400,
                session: old,
                active_seconds: 0,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionTouch { id: 400, .. })
        ));

        moving
            .send(&Command::SessionOpen {
                id: 401,
                project: "/tmp".into(),
                fresh: true,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionOpen {
                id: 401,
                fresh: true,
                ..
            })
        ));
        event_tx
            .send(Event::Ack {
                id: 401,
                ok: true,
                error: None,
                data: Some(serde_json::json!({"session":next})),
            })
            .unwrap();
        let _ = moving_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            watcher_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "an ack goes to the window that asked, not to every window"
        );
        let (new_client, new_rx) = SessionClient::connect(next).unwrap();
        moving.disconnect();
        drop(moving);

        // The old session keeps streaming to its other window, but neither
        // the new client nor its live snapshot contains any part of that turn.
        event_tx
            .send(Event::TurnStart {
                session: old,
                turn: 1,
                text: Some("old".into()),
                notification: false,
                media: Vec::new(),
                at: 0,
            })
            .unwrap();
        event_tx
            .send(Event::Text {
                session: old,
                delta: "old output".into(),
            })
            .unwrap();
        assert!(matches!(
            watcher_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::TurnStart { session, .. } if session == old
        ));
        assert!(matches!(
            watcher_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Text { session, .. } if session == old
        ));
        let stray = new_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            stray.is_err(),
            "new session received an old event: {stray:?}"
        );
        event_tx
            .send(Event::Error {
                session: Some(old),
                code: "tool".into(),
                message: "old session error".into(),
            })
            .unwrap();
        assert!(matches!(
            watcher_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Error { session: Some(id), .. } if id == old
        ));
        assert!(new_rx.recv_timeout(Duration::from_millis(100)).is_err());

        event_tx
            .send(Event::Text {
                session: next,
                delta: "new output".into(),
            })
            .unwrap();
        assert!(matches!(
            new_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Text { session, delta } if session == next && delta == "new output"
        ));
        assert!(watcher_rx.recv_timeout(Duration::from_millis(100)).is_err());
        new_client
            .send(&Command::SessionMode {
                id: 402,
                session: next,
                mode: "auto".into(),
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionMode { session, .. }) if session == next
        ));
        event_tx
            .send(Event::Session {
                session: next,
                project: "/tmp".into(),
                mode: "auto".into(),
                confined: true,
                sandbox: runeengine::proto::SandboxReport {
                    kind: "none".into(),
                    abi: 0,
                    enforced: false,
                    reason: None,
                    escapes: vec![],
                },
                classifier: serde_json::Value::Null,
            })
            .unwrap();
        assert!(matches!(
            new_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Session { session, mode, .. } if session == next && mode == "auto"
        ));
        assert!(watcher_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(host);
        let _ = old_ui;
    }

    #[test]
    fn child_session_receives_new_ack_and_multi_hop_handover_succeeds() {
        let root_id = 988_001;
        let child_1_id = 988_002;
        let child_2_id = 988_003;
        let (msg_tx, msg_rx) = channel();
        let (event_tx, event_rx) = channel();

        let (root_host, _root_ui) = SessionHost::start(root_id, msg_tx, event_rx).unwrap();
        let (root_client, root_rx) = SessionClient::connect(root_id).unwrap();

        // 1. Root client opens child 1
        root_client
            .send(&Command::SessionOpen {
                id: 101,
                project: "/tmp".into(),
                fresh: true,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionOpen {
                id: 101,
                fresh: true,
                ..
            })
        ));
        event_tx
            .send(Event::Ack {
                id: 101,
                ok: true,
                error: None,
                data: Some(serde_json::json!({ "session": child_1_id })),
            })
            .unwrap();
        let ack1 = root_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            ack1,
            Event::Ack {
                id: 101,
                ok: true,
                ..
            }
        ));

        // Connect to child 1 socket
        let (child_1_client, child_1_rx) =
            SessionClient::connect(child_1_id).unwrap_or_else(|_| {
                for _ in 0..100 {
                    if let Ok(c) = SessionClient::connect(child_1_id) {
                        return c;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("child 1 socket was not created")
            });

        // 2. Client on Child 1 sends /new to create child 2 (THE BUG SCENARIO)
        child_1_client
            .send(&Command::SessionOpen {
                id: 102,
                project: "/tmp".into(),
                fresh: true,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionOpen {
                id: 102,
                fresh: true,
                ..
            })
        ));
        // Supervisor registers open on root host (as driver.handle does in host.rs)
        root_host.register_open(102);

        // Engine acknowledges creation of child 2
        event_tx
            .send(Event::Ack {
                id: 102,
                ok: true,
                error: None,
                data: Some(serde_json::json!({ "session": child_2_id })),
            })
            .unwrap();

        // 3. Child 1 client MUST receive the Ack (unconditional fanout verification)
        let ack2 = child_1_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("child 1 client must receive Ack for /new");
        assert!(
            matches!(
                ack2,
                Event::Ack {
                    id: 102,
                    ok: true,
                    ..
                }
            ),
            "expected Ack for request 102 on child 1 socket, got {ack2:?}"
        );

        // 4. Verify child 2 socket was created and can be connected to
        let (child_2_client, child_2_rx) =
            SessionClient::connect(child_2_id).unwrap_or_else(|_| {
                for _ in 0..100 {
                    if let Ok(c) = SessionClient::connect(child_2_id) {
                        return c;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("child 2 socket was not created")
            });

        // 5. Multi-hop: Child 2 client sends /new to create child 3
        let child_3_id = 988_004;
        child_2_client
            .send(&Command::SessionOpen {
                id: 103,
                project: "/tmp".into(),
                fresh: true,
            })
            .unwrap();
        assert!(matches!(
            msg_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            crate::ui::Msg::RemoteCommand(Command::SessionOpen {
                id: 103,
                fresh: true,
                ..
            })
        ));
        root_host.register_open(103);

        event_tx
            .send(Event::Ack {
                id: 103,
                ok: true,
                error: None,
                data: Some(serde_json::json!({ "session": child_3_id })),
            })
            .unwrap();

        let ack3 = child_2_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("child 2 client must receive Ack for /new");
        assert!(
            matches!(
                ack3,
                Event::Ack {
                    id: 103,
                    ok: true,
                    ..
                }
            ),
            "expected Ack for request 103 on child 2 socket, got {ack3:?}"
        );

        let (child_3_client, _child_3_rx) =
            SessionClient::connect(child_3_id).unwrap_or_else(|_| {
                for _ in 0..100 {
                    if let Ok(c) = SessionClient::connect(child_3_id) {
                        return c;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("child 3 socket was not created")
            });

        drop(child_3_client);
        drop(child_2_client);
        drop(child_1_client);
        drop(root_client);
        drop(root_host);
    }
}
