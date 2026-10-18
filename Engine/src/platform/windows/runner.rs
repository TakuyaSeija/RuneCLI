//! Persistent per-process command runner executing under the local sandbox
//! account (`rune-sbx-<8hex>`).
//!
//! An unprivileged Windows process can start a process under another local
//! user account only through `CreateProcessWithLogonW` (`seclogon`), which
//! marshals a plain `STARTUPINFOW` over RPC and ignores extended startup
//! attributes (`STARTUPINFOEXW` / `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`).
//! Once running as that sandbox account, however, the process can build a
//! deny-only sibling token of its own (`Administrators` and
//! `Authenticated Users` marked deny-only, no restricting SIDs) and call
//! `CreateProcessAsUserW` with a `STARTUPINFOEXW` pseudoconsole and handle
//! list without holding `SeAssignPrimaryTokenPrivilege` (measured on Windows
//! Server 2025, 2026-10-18).
//!
//! Rune therefore starts one helper process (`-sandbox-runner <pipe>`) per
//! Rune process via `CreateProcessWithLogonW(LOGON_WITH_PROFILE)` and speaks
//! to it over a private named pipe whose DACL admits only the owner and the
//! sandbox account.

use std::collections::HashMap;
use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, GENERIC_READ, GENERIC_WRITE,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    SetKernelObjectSecurity, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess, GetCurrentProcessId,
    GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use super::account::SandboxAccount;
use super::security::{current_user, grant, Sid};
use super::wide::{check, owned, wide, wide_path, Local};

#[used]
#[link_section = ".CRT$XCU"]
static INIT_SANDBOX_HELPER: unsafe extern "C" fn() = init_sandbox_helper;

unsafe extern "C" fn init_sandbox_helper() {
    maybe_run_from_args();
}

/// Intercept `-sandbox-runner <pipe>` and `-sandbox account-setup <path>` at
/// process startup so both `rune.exe` and test binaries can serve as the
/// helper executable.
pub(super) fn maybe_run_from_args() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    let Some(first) = args.next() else {
        return;
    };
    if first == "-sandbox-runner" {
        let code = match args.next() {
            Some(pipe) => match run_server(&pipe) {
                Ok(()) => 0,
                Err(err) => {
                    eprintln!("rune -sandbox-runner: {err}");
                    1
                }
            },
            None => 2,
        };
        std::process::exit(code);
    }
    if first == "-sandbox" {
        if let Some(sub) = args.next() {
            if sub == "account-setup" {
                let code = match args.next() {
                    Some(path) => match super::account::setup_account_at(Path::new(&path)) {
                        Ok(()) => 0,
                        Err(err) => {
                            eprintln!("rune -sandbox account-setup: {err}");
                            1
                        }
                    },
                    None => 2,
                };
                std::process::exit(code);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Handshake {
    owner_sid: String,
}

#[derive(Serialize, Deserialize)]
struct HandshakeAck {
    ok: bool,
    error: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) enum SpawnStreams {
    Console {
        rows: u16,
        cols: u16,
    },
    Pipes {
        stdin: u64,
        stdout: u64,
        stderr: u64,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) struct SpawnSpec {
    pub(super) application: Vec<u16>,
    pub(super) command_line: Vec<u16>,
    pub(super) environment: Vec<u16>,
    pub(super) cwd: Option<Vec<u16>>,
    pub(super) flags: u32,
    pub(super) job_handle: u64,
    pub(super) streams: SpawnStreams,
}

#[derive(Serialize, Deserialize)]
pub(super) struct RemoteConsole {
    pub(super) id: u64,
    pub(super) keys_write: u64,
    pub(super) shown_read: u64,
}

#[derive(Serialize, Deserialize)]
enum RunnerRequest {
    Spawn(SpawnSpec),
    ResizeConsole { id: u64, rows: u16, cols: u16 },
    CloseConsole { id: u64 },
}

#[derive(Serialize, Deserialize)]
enum RunnerResponse {
    Spawned {
        pid: u32,
        process_handle: u64,
        console: Option<RemoteConsole>,
    },
    Ok,
    Error {
        raw_os_error: Option<i32>,
        message: String,
    },
}

impl RunnerResponse {
    fn error(err: io::Error) -> RunnerResponse {
        RunnerResponse::Error {
            raw_os_error: err.raw_os_error(),
            message: err.to_string(),
        }
    }
}

fn into_io_error(raw_os_error: Option<i32>, message: String) -> io::Error {
    match raw_os_error {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::other(message),
    }
}

/// Distinguishes a broken runner connection (which requires starting a new
/// runner process) from an ordinary `CreateProcessAsUserW` failure inside a
/// healthy runner (which must keep the runner and its live ConPTYs running).
enum SpawnAttemptError {
    Transport(io::Error),
    Remote(io::Error),
}

pub(super) struct RemoteStarted {
    pub(super) pid: u32,
    pub(super) process: OwnedHandle,
    pub(super) console: Option<(u64, OwnedHandle, OwnedHandle)>,
}

struct RunnerClient {
    pipe: File,
    process: OwnedHandle,
}

static CLIENT: OnceLock<Mutex<Option<RunnerClient>>> = OnceLock::new();

fn client_slot() -> &'static Mutex<Option<RunnerClient>> {
    CLIENT.get_or_init(Default::default)
}

/// Drop the active runner connection (called on `rune -sandbox reset`).
pub(super) fn stop_runner() {
    if let Some(client) = crate::panic::lock(client_slot()).take() {
        unsafe { TerminateProcess(client.process.as_raw_handle(), 0) };
    }
}

fn runner_alive(process: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) != WAIT_OBJECT_0 }
}

/// Spawn a command inside the persistent sandbox-account runner.
pub(super) fn spawn_in_runner(
    account: &SandboxAccount,
    staging_home: &Path,
    mut spec: SpawnSpec,
    local_job: HANDLE,
    local_pipes: Option<[HANDLE; 3]>,
) -> io::Result<RemoteStarted> {
    let mut guard = crate::panic::lock(client_slot());
    if guard
        .as_ref()
        .is_none_or(|client| !runner_alive(&client.process))
    {
        *guard = Some(RunnerClient::start(account, staging_home)?);
    }
    let result = {
        let client = guard.as_mut().unwrap();
        duplicate_inputs_and_spawn(client, &mut spec, local_job, local_pipes)
    };
    match result {
        Ok(started) => Ok(started),
        // The runner itself is healthy and reported that this command could
        // not be started (e.g., file not found or permission denied); never
        // restart the runner or drop its active ConPTYs.
        Err(SpawnAttemptError::Remote(err)) => Err(err),
        // The runner process or control pipe broke; start a fresh runner once.
        Err(SpawnAttemptError::Transport(first_err)) => {
            *guard = Some(RunnerClient::start(account, staging_home)?);
            let client = guard.as_mut().unwrap();
            match duplicate_inputs_and_spawn(client, &mut spec, local_job, local_pipes) {
                Ok(started) => Ok(started),
                Err(SpawnAttemptError::Remote(err)) => Err(err),
                Err(SpawnAttemptError::Transport(_)) => Err(first_err),
            }
        }
    }
}

/// Resize a remote pseudoconsole owned by the runner.
pub(super) fn resize_console(id: u64, rows: u16, cols: u16) -> io::Result<()> {
    let mut guard = crate::panic::lock(client_slot());
    let Some(client) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "sandbox runner is not connected",
        ));
    };
    send_frame(
        &mut client.pipe,
        &RunnerRequest::ResizeConsole { id, rows, cols },
    )?;
    match recv_frame::<RunnerResponse>(&mut client.pipe)? {
        RunnerResponse::Ok => Ok(()),
        RunnerResponse::Error {
            raw_os_error,
            message,
        } => Err(into_io_error(raw_os_error, message)),
        RunnerResponse::Spawned { .. } => Err(io::Error::other("unexpected runner reply")),
    }
}

/// Close a remote pseudoconsole owned by the runner.
pub(super) fn close_console(id: u64) -> io::Result<()> {
    let mut guard = crate::panic::lock(client_slot());
    let Some(client) = guard.as_mut() else {
        return Ok(());
    };
    send_frame(&mut client.pipe, &RunnerRequest::CloseConsole { id })?;
    match recv_frame::<RunnerResponse>(&mut client.pipe)? {
        RunnerResponse::Ok => Ok(()),
        RunnerResponse::Error {
            raw_os_error,
            message,
        } => Err(into_io_error(raw_os_error, message)),
        RunnerResponse::Spawned { .. } => Err(io::Error::other("unexpected runner reply")),
    }
}

fn duplicate_inputs_and_spawn(
    client: &mut RunnerClient,
    spec: &mut SpawnSpec,
    local_job: HANDLE,
    local_pipes: Option<[HANDLE; 3]>,
) -> Result<RemoteStarted, SpawnAttemptError> {
    let runner_proc = client.process.as_raw_handle();
    let mut pushed_handles: Vec<HANDLE> = Vec::new();
    let job_in_runner = dup_into(current_process_raw(), local_job, runner_proc, false)
        .map_err(SpawnAttemptError::Transport)?;
    pushed_handles.push(job_in_runner);
    spec.job_handle = job_in_runner as u64;

    if let Some([stdin, stdout, stderr]) = local_pipes {
        let dup_pipe = |h: HANDLE, pushed: &mut Vec<HANDLE>| -> Result<u64, SpawnAttemptError> {
            match dup_into(current_process_raw(), h, runner_proc, true) {
                Ok(remote) => {
                    pushed.push(remote);
                    Ok(remote as u64)
                }
                Err(err) => {
                    for &prev in pushed.iter() {
                        close_in_runner(runner_proc, prev);
                    }
                    Err(SpawnAttemptError::Transport(err))
                }
            }
        };
        let s_in = dup_pipe(stdin, &mut pushed_handles)?;
        let s_out = dup_pipe(stdout, &mut pushed_handles)?;
        let s_err = dup_pipe(stderr, &mut pushed_handles)?;
        spec.streams = SpawnStreams::Pipes {
            stdin: s_in,
            stdout: s_out,
            stderr: s_err,
        };
    }

    if let Err(err) = send_frame(&mut client.pipe, &RunnerRequest::Spawn(clone_spec(spec))) {
        for &remote in &pushed_handles {
            close_in_runner(runner_proc, remote);
        }
        return Err(SpawnAttemptError::Transport(err));
    }

    let response =
        recv_frame::<RunnerResponse>(&mut client.pipe).map_err(SpawnAttemptError::Transport)?;
    match response {
        RunnerResponse::Spawned {
            pid,
            process_handle,
            console,
        } => {
            let pulled_proc = pull_from_runner(runner_proc, process_handle as HANDLE);
            let pulled_console = match console {
                Some(c) => {
                    let keys = pull_from_runner(runner_proc, c.keys_write as HANDLE);
                    let shown = pull_from_runner(runner_proc, c.shown_read as HANDLE);
                    match (keys, shown) {
                        (Ok(k), Ok(s)) => Ok(Some((c.id, k, s))),
                        (Err(e), _) | (_, Err(e)) => {
                            let _ = send_frame(
                                &mut client.pipe,
                                &RunnerRequest::CloseConsole { id: c.id },
                            );
                            let _ = recv_frame::<RunnerResponse>(&mut client.pipe);
                            Err(e)
                        }
                    }
                }
                None => Ok(None),
            };
            match (pulled_proc, pulled_console) {
                (Ok(process), Ok(console)) => Ok(RemoteStarted {
                    pid,
                    process,
                    console,
                }),
                (Ok(process), Err(err)) => {
                    unsafe { TerminateProcess(process.as_raw_handle(), 1) };
                    Err(SpawnAttemptError::Transport(err))
                }
                (Err(err), _) => Err(SpawnAttemptError::Transport(err)),
            }
        }
        RunnerResponse::Error {
            raw_os_error,
            message,
        } => Err(SpawnAttemptError::Remote(into_io_error(
            raw_os_error,
            message,
        ))),
        RunnerResponse::Ok => Err(SpawnAttemptError::Transport(io::Error::other(
            "unexpected runner reply",
        ))),
    }
}

fn clone_spec(spec: &SpawnSpec) -> SpawnSpec {
    SpawnSpec {
        application: spec.application.clone(),
        command_line: spec.command_line.clone(),
        environment: spec.environment.clone(),
        cwd: spec.cwd.clone(),
        flags: spec.flags,
        job_handle: spec.job_handle,
        streams: match &spec.streams {
            SpawnStreams::Console { rows, cols } => SpawnStreams::Console {
                rows: *rows,
                cols: *cols,
            },
            SpawnStreams::Pipes {
                stdin,
                stdout,
                stderr,
            } => SpawnStreams::Pipes {
                stdin: *stdin,
                stdout: *stdout,
                stderr: *stderr,
            },
        },
    }
}

impl RunnerClient {
    fn start(account: &SandboxAccount, staging_home: &Path) -> io::Result<RunnerClient> {
        use windows_sys::Win32::System::Threading::{CreateProcessWithLogonW, LOGON_WITH_PROFILE};

        let staged_exe = stage_runner_binary(staging_home, &account.sid)?;
        let runner_dir = staged_exe
            .parent()
            .ok_or_else(|| io::Error::other("staged runner has no parent folder"))?;

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let pipe_name = format!(
            r"\\.\pipe\rune-runner-{}-{}-{:016x}",
            unsafe { GetCurrentProcessId() },
            NEXT.fetch_add(1, Ordering::Relaxed),
            rand::rngs::OsRng.next_u64()
        );
        let wide_pipe = wide(OsStr::new(&pipe_name))?;
        let owner = current_user()?;
        let owner_sid = owner.string()?;
        let account_sid = account.sid.string()?;
        let sddl = wide(OsStr::new(&format!(
            "D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;GRGW;;;{account_sid})"
        )))?;
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        })?;
        let descriptor = Local(descriptor);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let server_pipe = owned(unsafe {
            CreateNamedPipeW(
                wide_pipe.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            )
        })?;

        let user = wide(OsStr::new(&account.name))?;
        let domain = wide(OsStr::new("."))?;
        let password = wide(OsStr::new(&account.password))?;
        let application = wide_path(&staged_exe)?;
        let mut cmd_line = wide(OsStr::new(&format!(
            "\"{}\" -sandbox-runner \"{pipe_name}\"",
            staged_exe.display()
        )))?;
        let cwd = wide_path(runner_dir)?;
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        check(unsafe {
            CreateProcessWithLogonW(
                user.as_ptr(),
                domain.as_ptr(),
                password.as_ptr(),
                LOGON_WITH_PROFILE,
                application.as_ptr(),
                cmd_line.as_mut_ptr(),
                CREATE_NO_WINDOW,
                std::ptr::null(),
                cwd.as_ptr(),
                &startup,
                &mut proc_info,
            )
        })?;
        let runner_process = unsafe { OwnedHandle::from_raw_handle(proc_info.hProcess) };
        drop(unsafe { OwnedHandle::from_raw_handle(proc_info.hThread) });
        let ready = Arc::new(AtomicBool::new(false));
        watch_until_ready(&runner_process, &pipe_name, Arc::clone(&ready))?;

        if unsafe { ConnectNamedPipe(server_pipe.as_raw_handle(), std::ptr::null_mut()) } == 0 {
            let err = io::Error::last_os_error();
            const ERROR_PIPE_CONNECTED: i32 = 535;
            if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED) {
                unsafe { TerminateProcess(runner_process.as_raw_handle(), 1) };
                return Err(err);
            }
        }

        let mut client_pid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(server_pipe.as_raw_handle(), &mut client_pid) } == 0
            || client_pid != proc_info.dwProcessId
        {
            unsafe { TerminateProcess(runner_process.as_raw_handle(), 1) };
            return Err(not_ready(&runner_process, client_pid));
        }

        let mut pipe = File::from(server_pipe);
        send_frame(&mut pipe, &Handshake { owner_sid })?;
        let ack: HandshakeAck =
            recv_frame(&mut pipe).map_err(|_| not_ready(&runner_process, client_pid))?;
        ready.store(true, Ordering::SeqCst);
        if !ack.ok {
            unsafe { TerminateProcess(runner_process.as_raw_handle(), 1) };
            return Err(io::Error::other(
                ack.error
                    .unwrap_or_else(|| "runner handshake failed".into()),
            ));
        }

        Ok(RunnerClient {
            pipe,
            process: runner_process,
        })
    }
}

/// How long a runner may take from its logon to its handshake. A first logon
/// of the account makes its profile, which takes seconds.
const RUNNER_READY_MS: u32 = 120_000;

/// A runner that dies or hangs before its handshake would leave Rune waiting
/// for its connection, or its first reply, for ever — every confined command
/// of the process behind it. A watcher ends that wait: at the runner's end,
/// or once [`RUNNER_READY_MS`] passed (stopping it first), it connects to the
/// pipe itself, so the connection Rune waits for comes and is found not the
/// runner's; a stopped runner breaks the pipe a reply is awaited on.
fn watch_until_ready(
    runner: &OwnedHandle,
    pipe_name: &str,
    ready: Arc<AtomicBool>,
) -> io::Result<()> {
    let process = runner.try_clone()?;
    let pipe_name = pipe_name.to_string();
    std::thread::Builder::new()
        .name("rune-runner-watch".into())
        .spawn(move || {
            let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), RUNNER_READY_MS) };
            if ready.load(Ordering::SeqCst) {
                return;
            }
            if waited != WAIT_OBJECT_0 {
                unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            }
            if let Ok(name) = wide(OsStr::new(&pipe_name)) {
                let handle = unsafe {
                    CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if handle != INVALID_HANDLE_VALUE {
                    drop(unsafe { OwnedHandle::from_raw_handle(handle) });
                }
            }
        })
        .map(drop)
}

/// Why a runner never became ready: it ended (and how), or it was stopped
/// after [`RUNNER_READY_MS`], or another process connected in its place.
fn not_ready(runner: &OwnedHandle, client_pid: u32) -> io::Error {
    const STILL_ACTIVE: u32 = 259;
    let mut code = STILL_ACTIVE;
    unsafe { GetExitCodeProcess(runner.as_raw_handle(), &mut code) };
    let why = if client_pid == unsafe { GetCurrentProcessId() } || code != STILL_ACTIVE {
        if code == 1 || code == STILL_ACTIVE {
            format!(
                "the sandbox runner did not become ready within {} s",
                RUNNER_READY_MS / 1000
            )
        } else {
            format!("the sandbox runner ended before it was ready (exit code {code:#x})")
        }
    } else {
        "another process connected to the sandbox runner's pipe".to_string()
    };
    io::Error::new(io::ErrorKind::TimedOut, why)
}

/// Deterministic 16-hex fingerprint of a runner executable from the build
/// version, canonical path, byte length, and modification timestamp so an
/// upgraded build never collides with or falls back to an older runner image.
fn exe_fingerprint(version: &str, exe: &Path, len: u64, modified_ns: u128) -> String {
    let mut hasher = Sha256::new();
    hasher.update(version.as_bytes());
    hasher.update([0]);
    hasher.update(exe.as_os_str().to_string_lossy().to_uppercase().as_bytes());
    hasher.update([0]);
    hasher.update(len.to_le_bytes());
    hasher.update(modified_ns.to_le_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn stage_runner_binary(staging_home: &Path, account_sid: &Sid) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let canonical_exe = super::canonical(&exe).unwrap_or_else(|_| exe.clone());
    let src = std::fs::metadata(&exe)?;
    let modified_ns = src
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let fp = exe_fingerprint(crate::VERSION, &canonical_exe, src.len(), modified_ns);

    let runner_dir = staging_home.join("sandbox").join("runner");
    std::fs::create_dir_all(&runner_dir)?;
    let staged_name = format!("rune-runner-{}-{fp}.exe", crate::VERSION);
    let staged = runner_dir.join(&staged_name);

    let matches_source =
        |path: &Path| std::fs::metadata(path).is_ok_and(|dst| dst.len() == src.len());

    if !matches_source(&staged) {
        let temp = runner_dir.join(format!(
            "stage-{}-{:016x}.tmp",
            std::process::id(),
            rand::rngs::OsRng.next_u64()
        ));
        std::fs::copy(&exe, &temp)?;
        if let Err(rename_err) = std::fs::rename(&temp, &staged) {
            let _ = std::fs::remove_file(&temp);
            if !matches_source(&staged) {
                return Err(rename_err);
            }
        }
    }

    grant(
        &runner_dir,
        account_sid,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        true,
    )?;
    grant(
        &staged,
        account_sid,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        false,
    )?;

    // Best-effort cleanup of older staged runner binaries that are no longer
    // held open by a running process.
    if let Ok(entries) = std::fs::read_dir(&runner_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name != OsStr::new(&staged_name) {
                let text = name.to_string_lossy();
                if text.starts_with("rune-runner-") || text.starts_with("stage-") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    Ok(staged)
}

/// Lock down the runner process's own kernel DACL so only `SYSTEM`,
/// `Administrators`, and the owner's SID (`owner_sid`) may open the runner
/// process. Confined child commands run as the sandbox account and are
/// refused `OpenProcess` on the runner.
fn lock_runner_process_dacl(owner_sid: &str) -> Result<(), String> {
    let sddl = wide(OsStr::new(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{owner_sid})"
    )))
    .map_err(|e| e.to_string())?;
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    })
    .map_err(|e| format!("runner process security descriptor: {e}"))?;
    let sd = Local(sd);
    check(unsafe {
        SetKernelObjectSecurity(
            GetCurrentProcess(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sd.0,
        )
    })
    .map_err(|e| format!("SetKernelObjectSecurity(runner): {e}"))
}

/// Entry point when invoked as `rune -sandbox-runner <pipe>`.
pub fn run_server(pipe_name: &OsStr) -> Result<(), String> {
    let wide_pipe = wide(pipe_name).map_err(|e| e.to_string())?;
    let handle = owned(unsafe {
        CreateFileW(
            wide_pipe.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    })
    .map_err(|e| format!("open runner pipe: {e}"))?;
    let mut pipe = File::from(handle);

    let handshake: Handshake = recv_frame(&mut pipe).map_err(|e| format!("read handshake: {e}"))?;
    let owner_sid =
        Sid::from_string(&handshake.owner_sid).map_err(|e| format!("parse owner SID: {e}"))?;

    if let Err(err) = lock_runner_process_dacl(&handshake.owner_sid) {
        let _ = send_frame(
            &mut pipe,
            &HandshakeAck {
                ok: false,
                error: Some(err.clone()),
            },
        );
        return Err(err);
    }

    let token = match super::confine::runner_deny_only_token(&owner_sid) {
        Ok(tok) => tok,
        Err(err) => {
            let _ = send_frame(
                &mut pipe,
                &HandshakeAck {
                    ok: false,
                    error: Some(err.to_string()),
                },
            );
            return Err(err.to_string());
        }
    };

    let runner_sid = current_user()
        .map_err(|e| e.to_string())?
        .string()
        .map_err(|e| e.to_string())?;
    let child_sddl = wide(OsStr::new(&format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{runner_sid})(A;;GA;;;{})",
        handshake.owner_sid
    )))
    .map_err(|e| e.to_string())?;
    let mut child_sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            child_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut child_sd,
            std::ptr::null_mut(),
        )
    })
    .map_err(|e| format!("child security descriptor: {e}"))?;
    let child_sd = Local(child_sd);

    send_frame(
        &mut pipe,
        &HandshakeAck {
            ok: true,
            error: None,
        },
    )
    .map_err(|e| format!("write handshake ack: {e}"))?;

    let mut consoles: HashMap<u64, HPCON> = HashMap::new();
    let mut next_console_id = 1u64;

    while let Ok(request) = recv_frame::<RunnerRequest>(&mut pipe) {
        let response = match request {
            RunnerRequest::Spawn(spec) => handle_spawn(
                spec,
                token.as_raw_handle(),
                child_sd.0,
                &mut consoles,
                &mut next_console_id,
            ),
            RunnerRequest::ResizeConsole { id, rows, cols } => match consoles.get(&id) {
                Some(&hpc) => {
                    let hr = unsafe { ResizePseudoConsole(hpc, coord(rows, cols)) };
                    if hr >= 0 {
                        RunnerResponse::Ok
                    } else {
                        RunnerResponse::error(io::Error::from_raw_os_error(hr))
                    }
                }
                None => RunnerResponse::error(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown console id {id}"),
                )),
            },
            RunnerRequest::CloseConsole { id } => {
                if let Some(hpc) = consoles.remove(&id) {
                    std::thread::spawn(move || unsafe {
                        ClosePseudoConsole(hpc);
                    });
                }
                RunnerResponse::Ok
            }
        };
        if send_frame(&mut pipe, &response).is_err() {
            break;
        }
    }

    for (_, hpc) in consoles.drain() {
        unsafe { ClosePseudoConsole(hpc) };
    }
    Ok(())
}

fn handle_spawn(
    mut spec: SpawnSpec,
    token: HANDLE,
    child_sd: PSECURITY_DESCRIPTOR,
    consoles: &mut HashMap<u64, HPCON>,
    next_console_id: &mut u64,
) -> RunnerResponse {
    let job = unsafe { OwnedHandle::from_raw_handle(spec.job_handle as HANDLE) };
    let owned_pipes = match &spec.streams {
        SpawnStreams::Pipes {
            stdin,
            stdout,
            stderr,
        } => Some(unsafe {
            [
                OwnedHandle::from_raw_handle(*stdin as HANDLE),
                OwnedHandle::from_raw_handle(*stdout as HANDLE),
                OwnedHandle::from_raw_handle(*stderr as HANDLE),
            ]
        }),
        SpawnStreams::Console { .. } => None,
    };

    let result = spawn_child(
        &mut spec,
        token,
        child_sd,
        job.as_raw_handle(),
        owned_pipes.as_ref(),
        consoles,
        next_console_id,
    );
    match result {
        Ok(resp) => resp,
        Err(err) => RunnerResponse::error(err),
    }
}

fn spawn_child(
    spec: &mut SpawnSpec,
    token: HANDLE,
    child_sd: PSECURITY_DESCRIPTOR,
    job: HANDLE,
    owned_pipes: Option<&[OwnedHandle; 3]>,
    consoles: &mut HashMap<u64, HPCON>,
    next_console_id: &mut u64,
) -> io::Result<RunnerResponse> {
    let mut attributes = Attributes::new(1)?;
    let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;

    let mut created_console: Option<(HPCON, OwnedHandle, OwnedHandle)> = None;
    let raw_pipes: [HANDLE; 3];
    let inherit = match (&spec.streams, owned_pipes) {
        (SpawnStreams::Console { rows, cols }, _) => {
            let (keys_read, keys_write) = anon_pipe()?;
            let (shown_read, shown_write) = anon_pipe()?;
            let mut hpc: HPCON = 0;
            let hr = unsafe {
                CreatePseudoConsole(
                    coord(*rows, *cols),
                    keys_read.as_raw_handle(),
                    shown_write.as_raw_handle(),
                    0,
                    &mut hpc,
                )
            };
            if hr < 0 {
                return Err(io::Error::other(format!("CreatePseudoConsole: {hr:#010x}")));
            }
            drop((keys_read, shown_write));
            attributes.set(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const c_void,
                std::mem::size_of::<HPCON>(),
            )?;
            created_console = Some((hpc, keys_write, shown_read));
            info.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            0
        }
        (SpawnStreams::Pipes { .. }, Some(pipes)) => {
            raw_pipes = [
                pipes[0].as_raw_handle(),
                pipes[1].as_raw_handle(),
                pipes[2].as_raw_handle(),
            ];
            attributes.set(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                raw_pipes.as_ptr().cast(),
                std::mem::size_of_val(&raw_pipes),
            )?;
            info.StartupInfo.hStdInput = raw_pipes[0];
            info.StartupInfo.hStdOutput = raw_pipes[1];
            info.StartupInfo.hStdError = raw_pipes[2];
            1
        }
        _ => return Err(io::Error::other("missing pipe handles")),
    };
    info.lpAttributeList = attributes.list;

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: child_sd,
        bInheritHandle: 0,
    };
    let flags =
        spec.flags | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
    let cwd_ptr = spec
        .cwd
        .as_ref()
        .map_or(std::ptr::null(), |dir| dir.as_ptr());
    let mut proc_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let started = unsafe {
        CreateProcessAsUserW(
            token,
            spec.application.as_ptr(),
            spec.command_line.as_mut_ptr(),
            &sa,
            &sa,
            inherit,
            flags,
            spec.environment.as_ptr().cast(),
            cwd_ptr,
            &info.StartupInfo,
            &mut proc_info,
        )
    };
    if started == 0 {
        let err = io::Error::last_os_error();
        if let Some((hpc, _, _)) = created_console {
            unsafe { ClosePseudoConsole(hpc) };
        }
        return Err(err);
    }
    let child_proc = unsafe { OwnedHandle::from_raw_handle(proc_info.hProcess) };
    let child_thread = unsafe { OwnedHandle::from_raw_handle(proc_info.hThread) };

    let assigned = check(unsafe { AssignProcessToJobObject(job, child_proc.as_raw_handle()) })
        .and_then(|()| {
            if unsafe { ResumeThread(child_thread.as_raw_handle()) } == u32::MAX {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    if let Err(err) = assigned {
        unsafe { TerminateProcess(child_proc.as_raw_handle(), 1) };
        if let Some((hpc, _, _)) = created_console {
            unsafe { ClosePseudoConsole(hpc) };
        }
        return Err(err);
    }

    let remote_console = created_console.map(|(hpc, keys_write, shown_read)| {
        let id = *next_console_id;
        *next_console_id += 1;
        consoles.insert(id, hpc);
        RemoteConsole {
            id,
            keys_write: keys_write.into_raw_handle() as u64,
            shown_read: shown_read.into_raw_handle() as u64,
        }
    });

    Ok(RunnerResponse::Spawned {
        pid: proc_info.dwProcessId,
        process_handle: child_proc.into_raw_handle() as u64,
        console: remote_console,
    })
}

fn current_process_raw() -> HANDLE {
    unsafe { GetCurrentProcess() }
}

fn dup_into(
    source_process: HANDLE,
    source_handle: HANDLE,
    target_process: HANDLE,
    inherit: bool,
) -> io::Result<HANDLE> {
    let mut out: HANDLE = std::ptr::null_mut();
    check(unsafe {
        DuplicateHandle(
            source_process,
            source_handle,
            target_process,
            &mut out,
            0,
            i32::from(inherit),
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    Ok(out)
}

/// Pull a handle out of the runner process into the current Rune process and
/// atomically close the runner's copy (`DUPLICATE_CLOSE_SOURCE`).
fn pull_from_runner(runner_process: HANDLE, remote_handle: HANDLE) -> io::Result<OwnedHandle> {
    let mut out: HANDLE = std::ptr::null_mut();
    check(unsafe {
        DuplicateHandle(
            runner_process,
            remote_handle,
            current_process_raw(),
            &mut out,
            0,
            0,
            DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
        )
    })?;
    Ok(unsafe { OwnedHandle::from_raw_handle(out) })
}

/// Close a handle inside the runner process when a request fails before the
/// runner takes ownership of it.
fn close_in_runner(runner_process: HANDLE, remote_handle: HANDLE) {
    unsafe {
        DuplicateHandle(
            runner_process,
            remote_handle,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            0,
            DUPLICATE_CLOSE_SOURCE,
        );
    }
}

fn anon_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    check(unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) })?;
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD {
        X: cols.clamp(1, i16::MAX as u16) as i16,
        Y: rows.clamp(1, i16::MAX as u16) as i16,
    }
}

struct Attributes {
    _memory: Vec<usize>,
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Attributes {
    fn new(count: u32) -> io::Result<Attributes> {
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut memory = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let list = memory.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(unsafe { InitializeProcThreadAttributeList(list, count, 0, &mut size) })?;
        Ok(Attributes {
            _memory: memory,
            list,
        })
    }

    fn set(&mut self, attribute: u32, value: *const c_void, size: usize) -> io::Result<()> {
        check(unsafe {
            UpdateProcThreadAttribute(
                self.list,
                0,
                attribute as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list) };
    }
}

fn send_frame<T: Serialize>(stream: &mut File, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = (payload.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn recv_frame<T: for<'de> Deserialize<'de>>(stream: &mut File) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > (16 << 20) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runner frame exceeds 16 MiB",
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_fingerprint_changes_across_version_size_and_mtime() {
        let exe = Path::new(r"C:\Users\owner\.local\bin\rune.exe");
        let base = exe_fingerprint("0.3.2", exe, 1_048_576, 1_700_000_000);
        assert_eq!(base.len(), 16);
        assert_eq!(
            base,
            exe_fingerprint(
                "0.3.2",
                Path::new(r"c:\users\OWNER\.local\bin\rune.exe"),
                1_048_576,
                1_700_000_000,
            ),
            "case-insensitive path normalization must produce the same fingerprint"
        );
        assert_ne!(
            base,
            exe_fingerprint("0.3.3", exe, 1_048_576, 1_700_000_000)
        );
        assert_ne!(
            base,
            exe_fingerprint("0.3.2", exe, 1_048_577, 1_700_000_000)
        );
        assert_ne!(
            base,
            exe_fingerprint("0.3.2", exe, 1_048_576, 1_700_000_001)
        );
    }

    #[test]
    fn runner_error_roundtrips_raw_os_error() {
        let resp = RunnerResponse::error(io::Error::from_raw_os_error(5));
        match resp {
            RunnerResponse::Error {
                raw_os_error,
                message,
            } => {
                let err = into_io_error(raw_os_error, message);
                assert_eq!(err.raw_os_error(), Some(5));
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            }
            _ => panic!("expected RunnerResponse::Error"),
        }
    }
}
