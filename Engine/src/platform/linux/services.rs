//! Rune's supervisor of a confined command's connections: how a command
//! reaches its own Unix sockets freely while a program already running on
//! the machine (a container engine, the service manager, an agent holding
//! keys) is reached only once the person allowed it — with nothing for the
//! person to set up (owner, 2026-10-17: "everything automatic").
//!
//! # Why a supervisor
//!
//! Landlock mediates no `connect()` on a socket's path before ABI 9, and an
//! abstract socket has no path at all; refusing every Unix socket instead
//! broke what builds do inside the workspace (a test server, Python's
//! process pools, which listen on abstract names). So the command's
//! `connect` — and a `sendto` that names an address — waits on a seccomp
//! user notification (`seccomp::Supervised`), and this supervisor answers.
//!
//! # Race-free by construction
//!
//! The supervisor never lets the call go on with what it read: a second
//! thread of the command could rewrite the address, or put another socket
//! under the same descriptor number, between the judging and the kernel's
//! reading. It takes the command's socket itself (`pidfd_getfd`), copies the
//! address once, opens a pathname socket once (`O_PATH`, its real path read
//! from that descriptor) and makes the connection on that very socket, to
//! that very file (`/proc/self/fd/N`), replying with the call's result. What
//! it judged is what is connected.
//!
//! # Whose socket
//!
//! A socket is the command's own when its file lies in a folder the
//! confinement names (`Grants::sockets`: the project, the scratch, what the
//! person granted) or when the program serving it belongs to the command's
//! own tree — a descendant of the command, or carrying its tree's mark —
//! learnt without touching anyone: the kernel's list of bound Unix sockets
//! (`/proc/net/unix`: each socket's inode and the name it was bound to) and
//! the descriptors the tree's processes hold. No connection reaches another
//! program before the person said so. Anything else is put to the person
//! (`Grants::ask_service`); refused, the command reads `EACCES`, as Landlock
//! answers.

use std::ffi::CString;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use super::seccomp::Supervised;
use crate::platform::{ServiceAsk, ServiceCall};

/// What one confinement's supervisor judges by.
pub(super) struct Rules {
    /// Folders whose sockets are the command's own.
    pub own: Vec<PathBuf>,
    /// Who is asked about any other program's socket; `None`: refused.
    pub ask: Option<ServiceAsk>,
}

impl Rules {
    /// Every connection through: nothing confines the command any more.
    pub(super) fn everything() -> Arc<Rules> {
        Arc::new(Rules {
            own: Vec::new(),
            ask: Some(Arc::new(|_: &ServiceCall| true)),
        })
    }

    /// The command's own sockets alone (served by its own tree).
    pub(super) fn own_only() -> Arc<Rules> {
        Arc::new(Rules {
            own: Vec::new(),
            ask: None,
        })
    }
}

/// Whether this kernel lets Rune supervise connections: user notifications
/// with a listener that says when the last command left (`POLLHUP`), and
/// `pidfd_getfd`. Found by doing it once, in a child of this process — never
/// assumed from a version.
pub(super) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe().unwrap_or(false))
}

fn probe() -> io::Result<bool> {
    let filter = Supervised::of_connections()?;
    let (mut handoff, receiver) = handoff()?;
    let taker = receiver.take()?;
    // SAFETY: the child runs syscalls on data built above, then `_exit`s.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        let sent = filter.install().and_then(|listener| handoff.send(listener));
        unsafe { libc::_exit(if sent.is_ok() { 0 } else { 1 }) }
    }
    drop(handoff);
    let listener = taker.listener(child);
    let mut status = 0;
    unsafe { libc::waitpid(child, &mut status, 0) };
    let listener = listener?;
    if !(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0) {
        return Ok(false);
    }
    let mut poll = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // The child is gone: a kernel that says so lets a supervisor end.
    let hung_up = unsafe { libc::poll(&mut poll, 1, 0) } == 1 && poll.revents & libc::POLLHUP != 0;
    Ok(hung_up)
}

/// How a listener crosses from the child that installed its filter to Rune.
/// The child cannot send it in a message: its own sends wait on the very
/// listener it would send. So Rune takes it from the child (`pidfd_getfd`):
/// the child writes its pid and the listener's number on a socket pair made
/// before `fork`, and waits for Rune's answer before it goes on to `exec`
/// (where the listener would close). `spawn` itself waits for that `exec`, so
/// a thread of Rune's started before it ([`Taker`]) does the taking. The
/// child keeps the [`Handoff`] end.
pub(super) struct Handoff {
    child: OwnedFd,
}

/// Rune's end of a [`Handoff`], before the child is started.
pub(super) struct Receiver {
    parent: OwnedFd,
}

/// The thread taking one child's listener: its pid and the listener.
pub(super) struct Taker(std::thread::JoinHandle<io::Result<(i32, OwnedFd)>>);

/// How long a taker waits for a child that never says (its spawn failed).
const HANDOFF_WAIT_MS: i32 = 60_000;

pub(super) fn handoff() -> io::Result<(Handoff, Receiver)> {
    let mut fds = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let (child, parent) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((Handoff { child }, Receiver { parent }))
}

impl Handoff {
    /// In the child: say where `listener` is, wait until Rune holds it, then
    /// close it. Syscalls only; refused by Rune, the child does not go on.
    pub(super) fn send(&mut self, listener: libc::c_int) -> io::Result<()> {
        let mut said = [0u8; 8];
        let me = unsafe { libc::getpid() };
        said[..4].copy_from_slice(&me.to_ne_bytes());
        said[4..].copy_from_slice(&listener.to_ne_bytes());
        let fd = self.child.as_raw_fd();
        if unsafe { libc::write(fd, said.as_ptr().cast(), said.len()) } != said.len() as isize {
            return Err(io::Error::last_os_error());
        }
        let mut answer = [0u8; 1];
        let read = loop {
            let read = unsafe { libc::read(fd, answer.as_mut_ptr().cast(), 1) };
            if read < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break read;
        };
        unsafe { libc::close(listener) };
        if read != 1 || answer[0] != 1 {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        Ok(())
    }
}

impl Receiver {
    /// Start taking the listener the child will say, before it is started.
    pub(super) fn take(self) -> io::Result<Taker> {
        std::thread::Builder::new()
            .name("rune-handoff".into())
            .spawn(move || self.taken())
            .map(Taker)
    }

    fn taken(&self) -> io::Result<(i32, OwnedFd)> {
        let fd = self.parent.as_raw_fd();
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let ready = unsafe { libc::poll(&mut poll, 1, HANDOFF_WAIT_MS) };
            if ready < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if ready != 1 || poll.revents & libc::POLLIN == 0 {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            break;
        }
        let mut said = [0u8; 8];
        let read = unsafe { libc::read(fd, said.as_mut_ptr().cast(), said.len()) };
        let taken = if read == said.len() as isize {
            let pid = libc::pid_t::from_ne_bytes(said[..4].try_into().unwrap_or([0; 4]));
            let number = libc::c_int::from_ne_bytes(said[4..].try_into().unwrap_or([0; 4]));
            pidfd_open(pid)
                .and_then(|process| pidfd_getfd(&process, number))
                .map(|listener| (pid, listener))
        } else {
            Err(io::Error::from_raw_os_error(libc::EPROTO))
        };
        let answer = [u8::from(taken.is_ok())];
        unsafe { libc::write(fd, answer.as_ptr().cast(), 1) };
        taken
    }
}

impl Taker {
    /// The listener of the child `root`, once it was taken.
    pub(super) fn listener(self, root: i32) -> io::Result<OwnedFd> {
        let (pid, listener) = self
            .0
            .join()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))??;
        if pid != root {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        Ok(listener)
    }
}

/// Answer the connections of the command `root` (its tree marked `mark`)
/// arriving on `listener`, until its last process is gone — or until this
/// process lets go of it for the build it `exec`s into ([`release`]).
pub(super) fn supervise(listener: OwnedFd, rules: Arc<Rules>, root: i32, mark: Option<String>) {
    let tree = Arc::new(Tree { root, mark });
    let listener = Arc::new(listener);
    let Ok(stop) = eventfd() else {
        // Not stoppable, still supervised: only a live upgrade would wait.
        let _ = std::thread::Builder::new()
            .name("rune-services".into())
            .spawn(move || serve(listener, rules, tree, None, Arc::default()));
        return;
    };
    let stop = Arc::new(stop);
    let busy: Arc<Busy> = Arc::default();
    let (l, r, t, s, b) = (
        Arc::clone(&listener),
        Arc::clone(&rules),
        Arc::clone(&tree),
        Arc::clone(&stop),
        Arc::clone(&busy),
    );
    let Ok(thread) = std::thread::Builder::new()
        .name("rune-services".into())
        .spawn(move || serve(l, r, t, Some(s), b))
    else {
        return;
    };
    if let Ok(mut running) = running().lock() {
        running.retain(|_, held| !held.thread.is_finished());
        running.insert(
            root,
            Running {
                listener,
                rules,
                tree,
                stop,
                busy,
                thread,
            },
        );
    }
}

/// Answers still being made: a supervisor is let go of only once none is.
#[derive(Default)]
struct Busy {
    count: std::sync::Mutex<usize>,
    idle: std::sync::Condvar,
}

/// One supervisor at work, as this process lets go of it.
struct Running {
    listener: Arc<OwnedFd>,
    rules: Arc<Rules>,
    tree: Arc<Tree>,
    stop: Arc<OwnedFd>,
    busy: Arc<Busy>,
    thread: std::thread::JoinHandle<()>,
}

fn running() -> &'static std::sync::Mutex<std::collections::HashMap<i32, Running>> {
    static RUNNING: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, Running>>> =
        OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

fn eventfd() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A supervisor let go of: no call is being answered, none is taken, and the
/// calls the command makes meanwhile wait in the kernel for the build that
/// takes the listener over. Taken back with [`Released::resume`].
pub struct Released {
    listener: Arc<OwnedFd>,
    rules: Arc<Rules>,
    root: i32,
    mark: Option<String>,
}

/// Let go of the supervisor of the command `root`, once every answer it is
/// making is made — within `within`; `None` when it has none (its command
/// asks nothing of Rune, or has ended). An answer still waiting then (a
/// question to the person, a connection still being made) keeps it here,
/// supervising, and the error says so.
pub(super) fn release(root: i32, within: std::time::Duration) -> io::Result<Option<Released>> {
    let held = running()
        .lock()
        .ok()
        .and_then(|mut running| running.remove(&root));
    let Some(held) = held else {
        return Ok(None);
    };
    if held.thread.is_finished() {
        // Its command has ended: nothing is left to hand on.
        return Ok(None);
    }
    let one = 1u64.to_ne_bytes();
    unsafe { libc::write(held.stop.as_raw_fd(), one.as_ptr().cast(), one.len()) };
    let _ = held.thread.join();
    let deadline = std::time::Instant::now() + within;
    let mut count = held.busy.count.lock().map_err(|_| io::ErrorKind::Other)?;
    while *count > 0 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        count = held
            .busy
            .idle
            .wait_timeout(count, left)
            .map_err(|_| io::ErrorKind::Other)?
            .0;
    }
    let answering = *count;
    drop(count);
    let released = Released {
        listener: held.listener,
        rules: held.rules,
        root,
        mark: held.tree.mark.clone(),
    };
    if answering > 0 {
        released.resume();
        return Err(io::Error::other(format!(
            "the connections of process {root} are still being answered"
        )));
    }
    Ok(Some(released))
}

impl Released {
    /// Supervise again, here, as before.
    pub fn resume(self) {
        if let Ok(listener) = Arc::try_unwrap(self.listener).or_else(|shared| shared.try_clone()) {
            supervise(listener, self.rules, self.root, self.mark);
        }
    }

    /// The listener, open across `exec` for the build that takes it over
    /// ([`adopt`]); this process's own copy still closes on `exec`.
    pub fn into_handed(self) -> io::Result<crate::platform::Handed> {
        let fd = unsafe { libc::fcntl(self.listener.as_raw_fd(), libc::F_DUPFD, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(crate::platform::Handed(fd))
    }
}

/// Take over the supervisor a build before this one handed on for the
/// command `root`: its listener, checked to be one, closes on `exec` again
/// and is answered by `rules`. A number that is not a listener is left alone.
pub(super) fn adopt(
    handed: crate::platform::Handed,
    rules: Arc<Rules>,
    root: i32,
    mark: Option<String>,
) -> io::Result<()> {
    let fd = handed.0;
    let named = std::fs::read_link(format!("/proc/self/fd/{fd}"))?;
    if named.as_os_str().as_bytes() != b"anon_inode:seccomp notify" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not a supervisor's listener"),
        ));
    }
    let listener = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    supervise(listener, rules, root, mark);
    Ok(())
}

/// A command's own processes: its root, every descendant, and whatever
/// carries its tree's mark (one that left for `init`).
pub(super) struct Tree {
    root: i32,
    mark: Option<String>,
}

impl Tree {
    fn holds(&self, pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        let mut at = pid;
        for _ in 0..256 {
            if at == self.root {
                return true;
            }
            match parent_of(at) {
                Some(parent) if parent > 1 => at = parent,
                _ => break,
            }
        }
        self.marked(pid)
    }

    fn marked(&self, pid: i32) -> bool {
        self.mark.as_ref().is_some_and(|mark| {
            std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|held| held == mark.as_bytes())
            })
        })
    }
}

impl Tree {
    /// Whether a process of this tree holds one of `inodes`.
    fn serves(&self, inodes: &[u64]) -> bool {
        processes()
            .into_iter()
            .filter(|pid| self.holds(*pid))
            .any(|pid| socket_inodes(pid).iter().any(|i| inodes.contains(i)))
    }
}

fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

fn serve(
    listener: Arc<OwnedFd>,
    rules: Arc<Rules>,
    tree: Arc<Tree>,
    stop: Option<Arc<OwnedFd>>,
    busy: Arc<Busy>,
) {
    loop {
        let mut polls = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_ref().map_or(-1, |stop| stop.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(polls.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if polls[1].revents & libc::POLLIN != 0 {
            // Let go of for another build: nothing more is taken here.
            return;
        }
        if polls[0].revents & libc::POLLIN == 0 {
            // Every process of the command has ended.
            return;
        }
        let mut request: libc::seccomp_notif = unsafe { zeroed() };
        if unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV as _,
                &mut request,
            )
        } != 0
        {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) | Some(libc::ENOENT) => continue,
                _ => return,
            }
        }
        // Each connection on a thread of its own: a blocking connect, or a
        // question to the person, holds only the call that waits on it.
        if let Ok(mut count) = busy.count.lock() {
            *count += 1;
        }
        let (l, r, t, b) = (
            Arc::clone(&listener),
            Arc::clone(&rules),
            Arc::clone(&tree),
            Arc::clone(&busy),
        );
        let answered = std::thread::Builder::new()
            .name("rune-connect".into())
            .spawn(move || {
                answer(l.as_raw_fd(), &request, &r, &t);
                drop(l);
                b.done();
            });
        if answered.is_err() {
            reply(listener.as_raw_fd(), request.id, Err(libc::EAGAIN));
            busy.done();
        }
    }
}

impl Busy {
    fn done(&self) {
        if let Ok(mut count) = self.count.lock() {
            *count = count.saturating_sub(1);
            self.idle.notify_all();
        }
    }
}

fn answer(listener: RawFd, request: &libc::seccomp_notif, rules: &Rules, tree: &Tree) {
    let result = handle(listener, request, rules, tree);
    reply(listener, request.id, result);
}

fn reply(listener: RawFd, id: u64, result: Result<i64, i32>) {
    let mut response: libc::seccomp_notif_resp = unsafe { zeroed() };
    response.id = id;
    match result {
        Ok(value) => response.val = value,
        Err(errno) => response.error = -errno,
    }
    unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_SEND as _, &mut response) };
}

pub(super) fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// The largest data a `sendto` is carried for (a datagram cannot be larger).
pub(super) const MOST_SENT: u64 = 8 << 20;

fn handle(
    listener: RawFd,
    request: &libc::seccomp_notif,
    rules: &Rules,
    tree: &Tree,
) -> Result<i64, i32> {
    let pid = request.pid as i32;
    let args = request.data.args;
    let call = request.data.nr as libc::c_long;
    if call == libc::SYS_sendmsg || call == libc::SYS_sendmmsg {
        return send_messages(listener, request, rules, tree);
    }
    let sending = call == libc::SYS_sendto;
    let (descriptor, at, length) = if sending {
        (args[0], args[4], args[5])
    } else {
        (args[0], args[1], args[2])
    };
    if length > size_of::<libc::sockaddr_storage>() as u64 {
        return Err(libc::EINVAL);
    }
    let process = pidfd_open(thread_group(pid)).map_err(code)?;
    let socket = pidfd_getfd(&process, descriptor as i32).map_err(code)?;
    let address = read_memory(pid, at, length as usize)?;
    let data = if sending {
        if args[2] > MOST_SENT {
            return Err(libc::EMSGSIZE);
        }
        read_memory(pid, args[1], args[2] as usize)?
    } else {
        Vec::new()
    };
    still_waiting(listener, request.id)?;
    let target = if option(&socket, libc::SO_DOMAIN) == Some(libc::AF_UNIX) {
        judge(&socket, pid, &address, rules, tree)?
    } else {
        Target::Address(address)
    };
    let (pointer, size) = target.as_sockaddr();
    let result = unsafe {
        if sending {
            libc::sendto(
                socket.as_raw_fd(),
                data.as_ptr().cast(),
                data.len(),
                args[3] as i32 | libc::MSG_NOSIGNAL,
                pointer,
                size,
            ) as i64
        } else {
            libc::connect(socket.as_raw_fd(), pointer, size) as i64
        }
    };
    if result < 0 {
        let failed = errno();
        if sending {
            broken_pipe(pid, failed, args[3] as i32);
        }
        Err(failed)
    } else {
        Ok(result)
    }
}

/// The most messages one `sendmmsg` sends (the kernel's `UIO_MAXIOV`).
const MOST_MESSAGES: usize = 1024;

/// One `sendmsg`/`sendmmsg` of the command, sent from Rune on its socket.
struct Sender<'a> {
    listener: RawFd,
    id: u64,
    pid: i32,
    process: &'a OwnedFd,
    socket: &'a OwnedFd,
    unix: bool,
    stream: bool,
    flags: i32,
    rules: &'a Rules,
    tree: &'a Tree,
}

/// `sendmsg` and `sendmmsg`: each message copied once (`message`), the Unix
/// address it names judged as a connection's is, then sent by Rune on the
/// command's own socket. `sendmmsg` reports how many went, each one's length
/// written back where the command reads it, as the kernel does.
fn send_messages(
    listener: RawFd,
    request: &libc::seccomp_notif,
    rules: &Rules,
    tree: &Tree,
) -> Result<i64, i32> {
    let pid = request.pid as i32;
    let args = request.data.args;
    let many = request.data.nr as libc::c_long == libc::SYS_sendmmsg;
    let process = pidfd_open(thread_group(pid)).map_err(code)?;
    let socket = pidfd_getfd(&process, args[0] as i32).map_err(code)?;
    let sender = Sender {
        listener,
        id: request.id,
        pid,
        process: &process,
        socket: &socket,
        unix: option(&socket, libc::SO_DOMAIN) == Some(libc::AF_UNIX),
        stream: option(&socket, libc::SO_TYPE) == Some(libc::SOCK_STREAM),
        flags: (if many { args[3] } else { args[2] }) as i32,
        rules,
        tree,
    };
    let mut judged = None;
    if !many {
        return sender.send(args[1], &mut judged);
    }
    let entry = size_of::<libc::mmsghdr>() as u64;
    let mut sent = 0;
    for index in 0..(args[2] as usize).min(MOST_MESSAGES) {
        let at = args[1] + index as u64 * entry;
        match sender.send(at, &mut judged) {
            Ok(length) => {
                let written = (length as libc::c_uint).to_ne_bytes();
                let place = at + std::mem::offset_of!(libc::mmsghdr, msg_len) as u64;
                let _ = write_memory(pid, place, &written);
                sent += 1;
            }
            Err(failed) if sent == 0 => return Err(failed),
            Err(_) => break,
        }
    }
    Ok(sent)
}

impl Sender<'_> {
    /// Send the message at `at`. `judged` keeps the last address allowed in
    /// this call, so a batch to one address puts it to the person once.
    fn send(&self, at: u64, judged: &mut Option<(Vec<u8>, Target)>) -> Result<i64, i32> {
        let message = super::message::Message::copy(self.pid, self.process, at, self.stream)?;
        still_waiting(self.listener, self.id)?;
        let (name, name_length) = if self.unix && !message.name.is_empty() {
            if judged
                .as_ref()
                .is_none_or(|(name, _)| *name != message.name)
            {
                let target = judge(self.socket, self.pid, &message.name, self.rules, self.tree)?;
                *judged = Some((message.name.clone(), target));
            }
            judged
                .as_ref()
                .map_or((std::ptr::null(), 0), |(_, t)| t.as_sockaddr())
        } else if message.name.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (
                message.name.as_ptr().cast(),
                message.name.len() as libc::socklen_t,
            )
        };
        let mut part = libc::iovec {
            iov_base: message.data.as_ptr() as *mut libc::c_void,
            iov_len: message.data.len(),
        };
        let mut header: libc::msghdr = unsafe { zeroed() };
        header.msg_name = name as *mut libc::c_void;
        header.msg_namelen = name_length;
        header.msg_iov = &mut part;
        header.msg_iovlen = 1;
        if !message.control.is_empty() {
            header.msg_control = message.control.as_ptr() as *mut libc::c_void;
            header.msg_controllen = message.control.len() as _;
        }
        let result = unsafe {
            libc::sendmsg(
                self.socket.as_raw_fd(),
                &header,
                self.flags | libc::MSG_NOSIGNAL,
            )
        };
        if result < 0 {
            let failed = errno();
            broken_pipe(self.pid, failed, self.flags);
            Err(failed)
        } else {
            Ok(result as i64)
        }
    }
}

/// Whether the command still waits on this call: everything it needs was
/// copied, and from here on the command's memory is not read again.
pub(super) fn still_waiting(listener: RawFd, id: u64) -> Result<(), i32> {
    let mut id = id;
    if unsafe { libc::ioctl(listener, libc::SECCOMP_IOCTL_NOTIF_ID_VALID as _, &mut id) } != 0 {
        return Err(libc::EINTR);
    }
    Ok(())
}

/// A send Rune made for the command found the other end gone: the command
/// gets the signal it would have got itself, unless it asked not to. Rune
/// sends with `MSG_NOSIGNAL`, so the signal is never Rune's.
pub(super) fn broken_pipe(thread: i32, failed: i32, flags: i32) {
    if failed != libc::EPIPE || flags & libc::MSG_NOSIGNAL != 0 {
        return;
    }
    unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            thread_group(thread),
            thread,
            libc::SIGPIPE,
        )
    };
}

/// The process a thread belongs to (its thread group). A notification names
/// the calling thread, and only a process has a pidfd whose descriptors can
/// be taken: a call from any thread but the first — a resolver's, a pool's —
/// is answered through its process (`pidfd_open` refuses a thread).
fn thread_group(thread: i32) -> i32 {
    std::fs::read_to_string(format!("/proc/{thread}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Tgid:"))
                .and_then(|value| value.trim().parse::<i32>().ok())
        })
        .unwrap_or(thread)
}

pub(super) fn code(error: io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

/// Where the connection goes: an address as it was copied, or a socket file
/// opened once and reached through its descriptor.
pub(super) enum Target {
    Address(Vec<u8>),
    /// The file held open for as long as its address names it.
    Opened {
        _file: OwnedFd,
        address: Vec<u8>,
    },
}

impl Target {
    fn opened(file: OwnedFd) -> Target {
        let spelt = format!("/proc/self/fd/{}", file.as_raw_fd());
        let address = unix_address(spelt.as_bytes());
        Target::Opened {
            _file: file,
            address,
        }
    }

    pub(super) fn as_sockaddr(&self) -> (*const libc::sockaddr, libc::socklen_t) {
        let bytes = match self {
            Target::Address(bytes) | Target::Opened { address: bytes, .. } => bytes,
        };
        (bytes.as_ptr().cast(), bytes.len() as libc::socklen_t)
    }
}

/// A `sockaddr_un` naming `path` (its bytes, no terminating zero needed).
fn unix_address(path: &[u8]) -> Vec<u8> {
    let mut bytes = (libc::AF_UNIX as libc::sa_family_t).to_ne_bytes().to_vec();
    bytes.extend_from_slice(path);
    bytes
}

pub(super) fn judge(
    socket: &OwnedFd,
    pid: i32,
    address: &[u8],
    rules: &Rules,
    tree: &Tree,
) -> Result<Target, i32> {
    let family_bytes = size_of::<libc::sa_family_t>();
    let unix = address.len() > family_bytes
        && libc::sa_family_t::from_ne_bytes(address[..family_bytes].try_into().unwrap_or([0; 2]))
            == libc::AF_UNIX as libc::sa_family_t;
    if !unix {
        // Unnamed, or not a Unix address: the kernel answers it as it would.
        return Ok(Target::Address(address.to_vec()));
    }
    let path = &address[family_bytes..];
    let kind = option(socket, libc::SO_TYPE).unwrap_or(libc::SOCK_STREAM);
    if path[0] == 0 {
        // As the kernel lists it: `@`, and `@` for every zero byte.
        let listed: Vec<u8> = std::iter::once(b'@')
            .chain(path[1..].iter().map(|b| if *b == 0 { b'@' } else { *b }))
            .collect();
        let spelt = String::from_utf8_lossy(&listed).into_owned();
        return settle(
            Target::Address(address.to_vec()),
            kind,
            &listed,
            &spelt,
            rules,
            tree,
        );
    }
    let end = path.iter().position(|b| *b == 0).unwrap_or(path.len());
    let name = &path[..end];
    let file = open_in(pid, name)?;
    let real = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .unwrap_or_else(|_| PathBuf::from(std::ffi::OsStr::from_bytes(name)));
    let target = Target::opened(file);
    if rules.own.iter().any(|root| real.starts_with(root)) {
        return Ok(target);
    }
    let listed = real.as_os_str().as_bytes().to_vec();
    settle(
        target,
        kind,
        &listed,
        &real.display().to_string(),
        rules,
        tree,
    )
}

/// A socket outside the command's folders: its own when its own tree
/// serves it; otherwise the person's to allow.
fn settle(
    target: Target,
    kind: i32,
    listed: &[u8],
    spelt: &str,
    rules: &Rules,
    tree: &Tree,
) -> Result<Target, i32> {
    let bound = sockets_bound_at(listed, kind);
    if !bound.is_empty() && tree.serves(&bound) {
        return Ok(target);
    }
    let call = ServiceCall {
        address: spelt.to_string(),
        server: server_of(&bound),
    };
    match &rules.ask {
        Some(ask) if ask(&call) => Ok(target),
        _ => Err(libc::EACCES),
    }
}

/// The inodes of the sockets bound to `listed` (as `/proc/net/unix` spells
/// it) that a connection of `kind` could reach: a listening one for a stream,
/// any for a datagram.
fn sockets_bound_at(listed: &[u8], kind: i32) -> Vec<u64> {
    const LISTENING: u32 = 1 << 16;
    let Ok(table) = std::fs::read("/proc/net/unix") else {
        return Vec::new();
    };
    table
        .split(|b| *b == b'\n')
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.splitn(8, |b| *b == b' ').filter(|f| !f.is_empty());
            let _num = fields.next()?;
            let _refs = fields.next()?;
            let _protocol = fields.next()?;
            let flags = u32::from_str_radix(std::str::from_utf8(fields.next()?).ok()?, 16).ok()?;
            let _type = fields.next()?;
            let _state = fields.next()?;
            let inode: u64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            let path = fields.next()?;
            (path == listed && (kind == libc::SOCK_DGRAM || flags & LISTENING != 0))
                .then_some(inode)
        })
        .collect()
}

/// The descriptors of `pid` that are sockets, by inode.
fn socket_inodes(pid: i32) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let link = std::fs::read_link(entry.path()).ok()?;
            let link = link.to_str()?;
            link.strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

fn processes() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect()
}

/// The program holding one of `inodes`, named for the person.
fn server_of(inodes: &[u64]) -> Option<String> {
    if inodes.is_empty() {
        return None;
    }
    processes()
        .into_iter()
        .find(|pid| socket_inodes(*pid).iter().any(|i| inodes.contains(i)))
        .and_then(program_name)
}

fn program_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Open the socket file `name` as the calling thread `pid` resolves it —
/// a relative name from its own working folder — following links once.
fn open_in(pid: i32, name: &[u8]) -> Result<OwnedFd, i32> {
    let name = CString::new(name).map_err(|_| libc::EINVAL)?;
    let base = if name.as_bytes().first() == Some(&b'/') {
        None
    } else {
        let cwd = CString::new(format!("/proc/{pid}/cwd")).map_err(|_| libc::EINVAL)?;
        let fd = unsafe {
            libc::open(
                cwd.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(errno());
        }
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let fd = unsafe {
        libc::openat(
            base.as_ref().map_or(libc::AT_FDCWD, |b| b.as_raw_fd()),
            name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(super) fn option(socket: &OwnedFd, name: i32) -> Option<i32> {
    let mut value: libc::c_int = 0;
    let mut size = size_of::<libc::c_int>() as libc::socklen_t;
    (unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
        )
    } == 0)
        .then_some(value)
}

pub(super) fn read_memory(pid: i32, at: u64, length: usize) -> Result<Vec<u8>, i32> {
    let mut buffer = vec![0u8; length];
    if length == 0 {
        return Ok(buffer);
    }
    let local = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: length,
    };
    let remote = libc::iovec {
        iov_base: at as *mut libc::c_void,
        iov_len: length,
    };
    let read = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if read != length as isize {
        return Err(libc::EFAULT);
    }
    Ok(buffer)
}

/// Write `bytes` at `at` in the thread `pid`'s memory.
fn write_memory(pid: i32, at: u64, bytes: &[u8]) -> Result<(), i32> {
    let local = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: at as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let written = unsafe { libc::process_vm_writev(pid, &local, 1, &remote, 1, 0) };
    if written != bytes.len() as isize {
        return Err(libc::EFAULT);
    }
    Ok(())
}

pub(super) fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

pub(super) fn pidfd_getfd(process: &OwnedFd, descriptor: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_getfd, process.as_raw_fd(), descriptor, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // pidfd_getfd sets close-on-exec on what it hands over.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

#[cfg(test)]
mod tests {
    use crate::platform::{Launch, Placement, ServiceAsk, ServiceCall};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::canonical(&dir).unwrap()
    }

    /// This kernel (5.8 or later) supervises; the probe says so by doing it.
    #[test]
    fn the_probe_finds_what_this_kernel_offers() {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let mut parts = release
            .split(['.', '-'])
            .map(|p| p.parse::<u32>().unwrap_or(0));
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if (major, minor) >= (5, 8) {
            assert!(
                super::available(),
                "a {release} kernel supervises connections"
            );
        }
    }

    /// A program outside the confinement's folders and its tree, reached by
    /// a confined command: asked; refused when the answer is no or nobody
    /// is there to ask; connected when yes — and the confinement that grants
    /// local services is never asked.
    #[test]
    fn another_programs_socket_is_the_persons_to_allow() {
        if super::super::landlock::abi() <= 0 || !super::available() {
            eprintln!("note: nothing to supervise on this kernel");
            return;
        }
        let Some(python) = ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        let base = scratch("svc");
        let project = base.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let socket = base.join("s");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || for _ in listener.incoming() {});
        let reads = ["/usr", "/bin", "/lib", "/lib64", "/etc"].map(PathBuf::from);
        let asked: Arc<Mutex<Vec<ServiceCall>>> = Arc::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let connects = |open: bool, answer: Option<bool>| {
            let seen = Arc::clone(&asked);
            let ask: Option<ServiceAsk> = answer.map(|yes| {
                Arc::new(move |call: &ServiceCall| {
                    seen.lock().unwrap().push(call.clone());
                    yes
                }) as ServiceAsk
            });
            let sockets = [project.clone()];
            let confinement = crate::platform::Confinement::build(crate::platform::Grants {
                project: &project,
                ledger: None,
                reads: &reads,
                writes: &[],
                reach: &[],
                local_services: open,
                sockets: &sockets,
                ask_service: ask,
            })
            .unwrap();
            let code = format!(
                "import socket,sys\ns=socket.socket(socket.AF_UNIX)\ntry:\n s.connect({:?})\nexcept PermissionError:\n sys.exit(9)",
                socket.display().to_string()
            );
            runtime.block_on(async {
                let mut child = crate::platform::spawn_piped(
                    &Launch::new(python).args(["-c", &code]),
                    Placement {
                        confinement: Some(&confinement),
                        ..Placement::default()
                    },
                )
                .unwrap();
                child.wait().await.unwrap()
            })
        };
        assert_eq!(connects(false, None), 9, "nobody to ask: refused");
        assert_eq!(connects(false, Some(false)), 9, "the person said no");
        assert_eq!(connects(false, Some(true)), 0, "the person said yes");
        let calls = asked.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].address, socket.display().to_string());
        assert_eq!(connects(true, Some(false)), 0, "local services granted");
        assert_eq!(asked.lock().unwrap().len(), 2, "a grant asks nobody");
        let _ = std::fs::remove_dir_all(base);
    }

    /// A live upgrade: the supervisor is let go of, its listener handed on
    /// by number and taken over — and the command's next connection is
    /// answered by the build that took it (refused here, as it now says),
    /// never left without a supervisor (`ENOSYS`) nor waiting forever.
    #[test]
    fn a_supervisor_let_go_of_is_taken_over_and_answers_on() {
        if super::super::landlock::abi() <= 0 || !super::available() {
            eprintln!("note: nothing to supervise on this kernel");
            return;
        }
        let Some(python) = ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        assert!(super::release(i32::MAX, std::time::Duration::ZERO)
            .unwrap()
            .is_none());
        let base = scratch("svc-handover");
        let project = base.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let socket = base.join("s");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || for _ in listener.incoming() {});
        let go = project.join("go");
        let reads = ["/usr", "/bin", "/lib", "/lib64", "/etc"].map(PathBuf::from);
        let sockets = [project.clone()];
        let yes: ServiceAsk = Arc::new(|_: &ServiceCall| true);
        let confinement = crate::platform::Confinement::build(crate::platform::Grants {
            project: &project,
            ledger: None,
            reads: &reads,
            writes: &[],
            reach: &[],
            local_services: false,
            sockets: &sockets,
            ask_service: Some(yes),
        })
        .unwrap();
        let code = format!(
            "import os,socket,sys,time\nwhile not os.path.exists({go:?}): time.sleep(0.02)\ns=socket.socket(socket.AF_UNIX)\ntry:\n s.connect({socket:?})\nexcept OSError as e:\n sys.exit(e.errno)",
            go = go.display().to_string(),
            socket = socket.display().to_string(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime.block_on(async {
            let mut child = crate::platform::spawn_piped(
                &Launch::new(python).args(["-c", &code]),
                Placement {
                    confinement: Some(&confinement),
                    ..Placement::default()
                },
            )
            .unwrap();
            let root = child.id().unwrap();
            let released = super::release(root, std::time::Duration::from_secs(2))
                .unwrap()
                .expect("a confined command's supervisor is let go of");
            let handed = released.into_handed().unwrap();
            // The build that takes over refuses what this one allowed.
            super::adopt(handed, super::Rules::own_only(), root, None).unwrap();
            std::fs::write(&go, "").unwrap();
            child.wait().await.unwrap()
        });
        assert_eq!(code, libc::EACCES, "answered by the build that took over");
        let _ = std::fs::remove_dir_all(base);
    }

    /// What is not a supervisor's listener is never taken for one.
    #[test]
    fn a_number_that_is_not_a_listener_is_not_adopted() {
        let file = std::fs::File::open("/proc/self/stat").unwrap();
        let handed = crate::platform::Handed(std::os::fd::AsRawFd::as_raw_fd(&file));
        assert!(super::adopt(handed, super::Rules::own_only(), 1, None).is_err());
        drop(file);
    }

    /// A connection made by a thread that is not the command's first — a
    /// resolver's (curl looks names up on one), a pool's — is answered like
    /// any other (2026-10-18: `pidfd_open` refused the thread, and every such
    /// connection failed: curl could resolve no name).
    #[test]
    fn a_connection_from_another_thread_of_the_command_is_answered() {
        if super::super::landlock::abi() <= 0 || !super::available() {
            eprintln!("note: nothing to supervise on this kernel");
            return;
        }
        let Some(python) = ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        let base = scratch("svc-thread");
        let project = base.join("p");
        std::fs::create_dir_all(&project).unwrap();
        let reads = ["/usr", "/bin", "/lib", "/lib64", "/etc"].map(PathBuf::from);
        let sockets = [project.clone()];
        let confinement = crate::platform::Confinement::build(crate::platform::Grants {
            project: &project,
            ledger: None,
            reads: &reads,
            writes: &[],
            reach: &[],
            local_services: false,
            sockets: &sockets,
            ask_service: None,
        })
        .unwrap();
        // Its own socket, in the project: reached at once, from a thread.
        let code = format!(
            "import socket,sys,threading\nserver=socket.socket(socket.AF_UNIX)\nserver.bind({own:?})\nserver.listen()\nfailed=[]\ndef reach():\n  c=socket.socket(socket.AF_UNIX)\n  try:\n    c.connect({own:?})\n  except OSError as e:\n    failed.append(e.errno)\nt=threading.Thread(target=reach)\nt.start()\nt.join()\nsys.exit(failed[0] if failed else 0)",
            own = project.join("own.sock").display().to_string(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime.block_on(async {
            let mut child = crate::platform::spawn_piped(
                &Launch::new(python).args(["-c", &code]),
                Placement {
                    confinement: Some(&confinement),
                    ..Placement::default()
                },
            )
            .unwrap();
            child.wait().await.unwrap()
        });
        assert_eq!(code, 0, "a thread's connection failed with errno {code}");
        let _ = std::fs::remove_dir_all(base);
    }
}
