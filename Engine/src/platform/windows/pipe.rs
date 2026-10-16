//! Local sockets on Windows: named pipes, met at a path.
//!
//! A session host listens on a pipe with a random name, and writes that
//! name into a file of the user's alone at the socket's path — the place a
//! Unix socket would be — so everything above meets it by the same path, the
//! same identity, the same rename and removal: a host retiring moves its
//! file, a window reconnecting finds the successor or nobody. The pipe is
//! the user's alone (its ACL; above medium integrity its label too), the
//! first instance of its name, and refuses other machines.
//!
//! Who is at the other end is the kernel's word: the host reads the
//! window's own token (impersonation at identification level, which the
//! window allows and nothing more) — the same user, and never less elevated
//! than the host, so a window of an ordinary terminal never drives a host
//! started in an Administrator one — and the window asks which process
//! serves the pipe and whose it is.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    ImpersonateNamedPipeClient, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken, SetEvent,
    WaitForMultipleObjects, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::security::{current_user, Token};
use super::wide::{check, owned, wide};

/// What a meeting file starts with; the pipe's name follows.
const MAGIC: &str = "rune-local-pipe 1\n";

/// A local socket a session host listens on.
pub struct LocalListener(Arc<Listening>);

struct Listening {
    name: Vec<u16>,
    /// The instance waiting for the next window; made again after each.
    next: Mutex<Option<OwnedHandle>>,
    /// Set by `wake`, for good: every accept after it fails at once.
    woken: AtomicBool,
    wake: OwnedHandle,
}

/// One connection between a window and a host.
pub struct LocalStream(Arc<Connection>);

struct Connection {
    handle: OwnedHandle,
    server: bool,
    shut: AtomicBool,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
}

/// A manual-reset event, for one overlapped operation or a wake.
fn event() -> io::Result<OwnedHandle> {
    owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })
}

/// The security a session's pipe and meeting file have: the user alone, and
/// above medium integrity a label no lower process may write through.
fn pipe_attributes() -> io::Result<(SECURITY_ATTRIBUTES, super::wide::Local<std::ffi::c_void>)> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    let user = current_user()?.string()?;
    let raised = Token::of_this_process(TOKEN_QUERY)?
        .integrity()
        .is_some_and(|level| level > 0x2000);
    let sddl = if raised {
        format!("D:P(A;;GA;;;{user})S:(ML;;NW;;;HI)")
    } else {
        format!("D:P(A;;GA;;;{user})")
    };
    let sddl = wide(OsStr::new(&sddl))?;
    let mut descriptor = std::ptr::null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    })?;
    Ok((
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        },
        super::wide::Local(descriptor),
    ))
}

/// A new instance of the pipe `name` — the first one, when `first`.
fn instance(name: &[u16], first: bool) -> io::Result<OwnedHandle> {
    let (attributes, _descriptor) = pipe_attributes()?;
    owned(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX
                | FILE_FLAG_OVERLAPPED
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    })
}

fn random_name() -> String {
    use rand::RngCore;
    format!(
        r"\\.\pipe\rune-{}-{:016x}{:016x}",
        std::process::id(),
        rand::rngs::OsRng.next_u64(),
        rand::rngs::OsRng.next_u64()
    )
}

/// Wait for the overlapped operation `ov` on `handle` to finish, up to
/// `timeout`, or for `wake`; a timeout or a wake cancels it.
fn finish(
    handle: HANDLE,
    ov: &mut OVERLAPPED,
    timeout: Option<Duration>,
    wake: Option<HANDLE>,
) -> io::Result<u32> {
    let mut events = vec![ov.hEvent];
    events.extend(wake);
    let ms = timeout.map_or(INFINITE, |t| {
        t.as_millis().min(u128::from(INFINITE - 1)) as u32
    });
    let waited = unsafe { WaitForMultipleObjects(events.len() as u32, events.as_ptr(), 0, ms) };
    if waited != WAIT_OBJECT_0 {
        unsafe { CancelIoEx(handle, ov) };
        let mut done = 0u32;
        unsafe { GetOverlappedResult(handle, ov, &mut done, 1) };
        return Err(if waited == WAIT_TIMEOUT {
            io::Error::new(io::ErrorKind::WouldBlock, "timed out")
        } else {
            io::Error::new(io::ErrorKind::ConnectionAborted, "woken to stop")
        });
    }
    let mut done = 0u32;
    check(unsafe { GetOverlappedResult(handle, ov, &mut done, 0) })?;
    Ok(done)
}

impl LocalListener {
    /// Listen at `path`: a pipe of a new random name, met through a file of
    /// the user's alone written at `path` — one already there is an address
    /// in use, as a socket's would be.
    pub fn bind(path: &Path) -> io::Result<LocalListener> {
        let name_text = random_name();
        let name = wide(OsStr::new(&name_text))?;
        let first = instance(&name, true)?;
        let mut file =
            super::files::open_private(path, crate::platform::Private::New).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{} is in use", path.display()),
                    )
                } else {
                    e
                }
            })?;
        file.write_all(MAGIC.as_bytes())?;
        file.write_all(name_text.as_bytes())?;
        file.sync_all()?;
        Ok(LocalListener(Arc::new(Listening {
            name,
            next: Mutex::new(Some(first)),
            woken: AtomicBool::new(false),
            wake: event()?,
        })))
    }

    /// The next connection.
    pub fn accept(&self) -> io::Result<LocalStream> {
        let listening = &self.0;
        if listening.woken.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the listener was woken to stop",
            ));
        }
        let pipe = match crate::panic::lock(&listening.next).take() {
            Some(pipe) => pipe,
            None => instance(&listening.name, false)?,
        };
        let signal = event()?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = signal.as_raw_handle();
        let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), &mut ov) };
        if connected == 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|e| e as u32) {
                Some(ERROR_PIPE_CONNECTED) => {}
                Some(ERROR_IO_PENDING) => {
                    finish(
                        pipe.as_raw_handle(),
                        &mut ov,
                        None,
                        Some(listening.wake.as_raw_handle()),
                    )?;
                }
                _ => return Err(error),
            }
        }
        // The next window finds an instance waiting.
        if let Ok(next) = instance(&listening.name, false) {
            *crate::panic::lock(&listening.next) = Some(next);
        }
        Ok(LocalStream(Arc::new(Connection {
            handle: pipe,
            server: true,
            shut: AtomicBool::new(false),
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
        })))
    }

    /// Every connection, as it comes; ends once [`LocalListener::wake`] was
    /// called (each accept after it fails).
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<LocalStream>> + '_ {
        std::iter::repeat_with(move || self.accept())
    }

    /// Another handle on the same listener — kept to wake the accept loop by.
    pub fn try_clone(&self) -> io::Result<LocalListener> {
        Ok(LocalListener(Arc::clone(&self.0)))
    }

    /// Wake a thread blocked accepting: it returns at once, with an error,
    /// and so does every accept after — whatever became of the path.
    pub fn wake(&self) -> io::Result<()> {
        self.0.woken.store(true, Ordering::Release);
        check(unsafe { SetEvent(self.0.wake.as_raw_handle()) })
    }
}

impl LocalStream {
    /// Connect to the host met at `path`. Nobody there — no meeting file, or
    /// its pipe gone — is a refused connection, as for a socket left by a
    /// host that ended.
    pub fn connect(path: &Path) -> io::Result<LocalStream> {
        let refused = || {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("nobody listens at {}", path.display()),
            )
        };
        let name = meeting_name(path)?.ok_or_else(refused)?;
        let name = wide(OsStr::new(&name))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            match owned(handle) {
                Ok(handle) => {
                    return Ok(LocalStream(Arc::new(Connection {
                        handle,
                        server: false,
                        shut: AtomicBool::new(false),
                        read_timeout: Mutex::new(None),
                        write_timeout: Mutex::new(None),
                    })))
                }
                Err(error) => match error.raw_os_error().map(|e| e as u32) {
                    Some(ERROR_PIPE_BUSY) if std::time::Instant::now() < deadline => {
                        unsafe { WaitNamedPipeW(name.as_ptr(), 250) };
                    }
                    Some(ERROR_FILE_NOT_FOUND) => return Err(refused()),
                    _ => return Err(error),
                },
            }
        }
    }

    /// Two ends connected to each other — for tests of what a connection
    /// carries.
    pub fn pair() -> io::Result<(LocalStream, LocalStream)> {
        let name = wide(OsStr::new(&random_name()))?;
        let server = instance(&name, true)?;
        let client = owned(unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        })?;
        let end = |handle, server| {
            LocalStream(Arc::new(Connection {
                handle,
                server,
                shut: AtomicBool::new(false),
                read_timeout: Mutex::new(None),
                write_timeout: Mutex::new(None),
            }))
        };
        Ok((end(server, true), end(client, false)))
    }

    /// Another handle on the same connection: a reader beside a writer.
    pub fn try_clone(&self) -> io::Result<LocalStream> {
        Ok(LocalStream(Arc::clone(&self.0)))
    }

    /// How long a read waits; `None` for as long as it takes.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *crate::panic::lock(&self.0.read_timeout) = timeout;
        Ok(())
    }

    /// How long a write waits; `None` for as long as it takes.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *crate::panic::lock(&self.0.write_timeout) = timeout;
        Ok(())
    }

    /// End the connection both ways: every handle on it reads its end, and
    /// the host's side lets the window go at once.
    pub fn shutdown(&self) -> io::Result<()> {
        self.0.shut.store(true, Ordering::Release);
        unsafe { CancelIoEx(self.0.handle.as_raw_handle(), std::ptr::null()) };
        if self.0.server {
            unsafe { DisconnectNamedPipe(self.0.handle.as_raw_handle()) };
        }
        Ok(())
    }

    /// Whether the process at the other end runs as this process's user, as
    /// the kernel says — for a host, from the window's own token, which must
    /// also be at least as elevated as the host's; for a window, from the
    /// process serving the pipe.
    pub fn peer_is_same_user(&self) -> bool {
        let Ok(me) = current_user() else {
            return false;
        };
        let peer = if self.0.server {
            client_token(self.0.handle.as_raw_handle())
        } else {
            server_token(self.0.handle.as_raw_handle())
        };
        let Some(peer) = peer else {
            return false;
        };
        let same = peer.user().is_ok_and(|user| &user == me);
        if !self.0.server {
            return same;
        }
        let mine = Token::of_this_process(TOKEN_QUERY)
            .ok()
            .and_then(|token| token.integrity());
        same && matches!((peer.integrity(), mine), (Some(theirs), Some(mine)) if theirs >= mine)
    }

    fn transfer(&self, buf: &mut [u8], writing: bool) -> io::Result<usize> {
        let connection = &self.0;
        if connection.shut.load(Ordering::Acquire) {
            return if writing {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            } else {
                Ok(0)
            };
        }
        let timeout = *crate::panic::lock(if writing {
            &connection.write_timeout
        } else {
            &connection.read_timeout
        });
        let signal = event()?;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = signal.as_raw_handle();
        let handle = connection.handle.as_raw_handle();
        let len = buf.len().min(u32::MAX as usize) as u32;
        let started = unsafe {
            if writing {
                WriteFile(handle, buf.as_ptr(), len, std::ptr::null_mut(), &mut ov)
            } else {
                ReadFile(handle, buf.as_mut_ptr(), len, std::ptr::null_mut(), &mut ov)
            }
        };
        let result = if started != 0 {
            let mut done = 0u32;
            check(unsafe { GetOverlappedResult(handle, &ov, &mut done, 0) }).map(|()| done)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                finish(handle, &mut ov, timeout, None)
            } else {
                Err(error)
            }
        };
        match result {
            Ok(n) => Ok(n as usize),
            Err(error) => {
                let code = error.raw_os_error().map(|e| e as u32);
                let ended = matches!(
                    code,
                    Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
                ) || (code == Some(ERROR_OPERATION_ABORTED)
                    && connection.shut.load(Ordering::Acquire));
                match (ended, writing) {
                    (true, false) => Ok(0),
                    (true, true) => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                    _ => Err(error),
                }
            }
        }
    }
}

/// The token of the window at the other end of the host's pipe `handle`.
fn client_token(handle: HANDLE) -> Option<Token> {
    if unsafe { ImpersonateNamedPipeClient(handle) } == 0 {
        return None;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
    // Back to this process's own identity before anything else is done.
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        // A thread left impersonating must not go on: nothing safe remains.
        std::process::abort();
    }
    (opened != 0).then(|| Token(unsafe { OwnedHandle::from_raw_handle(token) }))
}

/// The token of the process serving the window's pipe `handle`.
fn server_token(handle: HANDLE) -> Option<Token> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } == 0 {
        return None;
    }
    let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }).ok()?;
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    Some(Token(unsafe { OwnedHandle::from_raw_handle(token) }))
}

impl Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.transfer(buf, false)
    }
}

impl Read for &LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.transfer(buf, false)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut copy = buf.to_vec();
        self.transfer(&mut copy, true)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut copy = buf.to_vec();
        self.transfer(&mut copy, true)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The pipe name a meeting file at `path` holds; `None` when there is no
/// such file, and an error when something else is there.
fn meeting_name(path: &Path) -> io::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !meta.is_file() || meta.len() > 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a local socket", path.display()),
        ));
    }
    let mut text = String::new();
    File::open(path)?.take(512).read_to_string(&mut text)?;
    let name = text.strip_prefix(MAGIC).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a local socket", path.display()),
        )
    })?;
    if !name.starts_with(r"\\.\pipe\rune-") || name.contains(['\r', '\n', '\0']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} names no pipe of Rune's", path.display()),
        ));
    }
    Ok(Some(name.to_string()))
}

/// Whether the entry at `path` is a local socket — a meeting file (not
/// followed if a link).
pub fn is_socket(path: &Path) -> bool {
    matches!(meeting_name(path), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-pipe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_host_is_met_at_its_path() {
        let dir = scratch("meet");
        let path = dir.join("Session1Host.sock");
        let listener = LocalListener::bind(&path).unwrap();
        assert!(is_socket(&path));
        assert_eq!(
            LocalListener::bind(&path).err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        let accepting = std::thread::spawn(move || {
            let mut accepted = listener.accept().unwrap();
            assert!(accepted.peer_is_same_user(), "the window is this user's");
            let mut got = String::new();
            accepted.read_to_string(&mut got).unwrap();
            got
        });
        let mut client = LocalStream::connect(&path).unwrap();
        assert!(client.peer_is_same_user(), "the host is this user's");
        client.write_all(b"ping").unwrap();
        client.shutdown().unwrap();
        drop(client);
        assert_eq!(accepting.join().unwrap(), "ping");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            LocalStream::connect(&path).err().map(|e| e.kind()),
            Some(io::ErrorKind::ConnectionRefused),
            "nobody is met where the file was"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wake_ends_an_accept_even_when_the_path_is_gone() {
        let dir = scratch("wake");
        let path = dir.join("Session2Host.sock");
        let listener = LocalListener::bind(&path).unwrap();
        let waker = listener.try_clone().unwrap();
        let accepting = std::thread::spawn(move || {
            let first = listener.accept().is_err();
            (first, listener.accept().is_err())
        });
        std::thread::sleep(Duration::from_millis(50));
        std::fs::remove_file(&path).unwrap();
        waker.wake().unwrap();
        assert_eq!(accepting.join().unwrap(), (true, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_times_out_and_a_shutdown_reads_its_end() {
        let (a, b) = LocalStream::pair().unwrap();
        a.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(
            (&a).read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        (&b).write_all(b"abcd").unwrap();
        assert_eq!((&a).read(&mut buf).unwrap(), 4);
        a.shutdown().unwrap();
        assert_eq!(
            (&b).read(&mut buf).unwrap(),
            0,
            "the other end reads its end"
        );
        assert!(!is_socket(&std::env::temp_dir()), "a folder is no socket");
    }
}
