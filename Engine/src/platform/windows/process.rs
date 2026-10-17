//! Processes on Windows: how a command Rune starts is launched and placed,
//! how its tree is stopped, what identifies a process, and the file this
//! program runs from.
//!
//! A command on a console or speaking over pipes is started by
//! `CreateProcessW` — or `CreateProcessAsUserW` with a confinement's
//! restricted token — suspended, put in a job object, and only then let run:
//! nothing it starts can leave the job, so a stop reaches the whole tree
//! (`terminate_tree`). Rune finds the program itself, on `PATH` with
//! `PATHEXT`, and never in the folder it was started from, where Windows
//! would look first: a `pwsh.exe` left in a project is never run in place
//! of the real one.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcessId,
    GetExitCodeProcess, GetProcessTimes, InitializeProcThreadAttributeList, OpenProcess,
    ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use super::files::{FileIdentity, Handed};
use super::pty::{PseudoConsole, PtySlave};
use super::wide::{check, owned, wide, wide_path};
use crate::platform::{Launch, Placement};

/// The environment variable that marks every process of one command's tree —
/// set as on every OS, though on Windows the tree is its job.
pub const TREE_VAR: &str = "RUNE_PROCESS_TREE";

/// A mark no other tree carries: this process, the time, and randomness.
pub fn new_tree_mark() -> String {
    use rand::RngCore;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos:x}-{:016x}",
        std::process::id(),
        rand::rngs::OsRng.next_u64()
    )
}

/// The name of the job a tree marked `mark` runs in, in this logon
/// session's namespace — how a stop finds it by its mark alone.
fn job_name(mark: &str) -> io::Result<Vec<u16>> {
    wide(OsStr::new(&format!(r"Local\RuneTree-{mark}")))
}

/// The value of `name` in `launch`'s environment as the program will see
/// it: the launch's own last setting, else this process's unless cleared.
/// Windows names compare without case.
fn launch_var(launch: &Launch, name: &str) -> Option<OsString> {
    launch
        .env
        .iter()
        .rev()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .or_else(|| {
            (!launch.clear_env)
                .then(|| std::env::var_os(name))
                .flatten()
        })
}

/// The file `program` names: itself when it has a path (relative to this
/// process's folder, as Windows reads it); otherwise the first match on the
/// launch's `PATH`, each folder tried with `PATHEXT`'s endings. Never the
/// current folder, and never a relative `PATH` entry.
pub(super) fn resolve(program: &Path, launch: &Launch) -> io::Result<PathBuf> {
    let endings: Vec<String> = launch_var(launch, "PATHEXT")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|ending| ending.starts_with('.'))
        .map(str::to_string)
        .collect();
    let found = |base: PathBuf| -> Option<PathBuf> {
        if base.extension().is_some() && base.is_file() {
            return Some(base);
        }
        endings
            .iter()
            .map(|ending| {
                let mut name = base.clone().into_os_string();
                name.push(ending);
                PathBuf::from(name)
            })
            .find(|candidate| candidate.is_file())
    };
    let has_folder = program.is_absolute()
        || program
            .as_os_str()
            .encode_wide()
            .any(|c| c == u16::from(b'\\') || c == u16::from(b'/'));
    if has_folder {
        let base = if program.is_absolute() {
            program.to_path_buf()
        } else {
            std::env::current_dir()?.join(program)
        };
        return found(base.clone()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", base.display()),
            )
        });
    }
    let search = launch_var(launch, "PATH").unwrap_or_default();
    std::env::split_paths(&search)
        .filter(|folder| folder.is_absolute())
        .find_map(|folder| found(folder.join(program)))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} was not found on PATH", program.display()),
            )
        })
}

/// Whether `program` is a batch file, which Windows runs through `cmd.exe`.
fn is_batch(program: &Path) -> bool {
    program
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ending| {
            ending.eq_ignore_ascii_case("bat") || ending.eq_ignore_ascii_case("cmd")
        })
}

/// One argument as the C runtime and `CommandLineToArgvW` read it back:
/// quoted when it holds a space, tab, newline or quote, or is empty;
/// backslashes doubled only where they precede a quote.
fn push_argument(argument: &OsStr, line: &mut Vec<u16>) {
    let text: Vec<u16> = argument.encode_wide().collect();
    let plain = !text.is_empty()
        && !text
            .iter()
            .any(|c| matches!(*c, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
    if plain {
        line.extend(text);
        return;
    }
    line.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for c in text {
        if c == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if c == u16::from(b'"') {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        line.push(c);
    }
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'"'));
}

/// The program Windows starts and the command line it is given. A batch
/// file is run by `cmd.exe` explicitly, and its arguments may then hold
/// nothing `cmd.exe` would read as its own syntax — refused rather than
/// guessed at.
pub(super) fn command_line(
    program: &Path,
    args: &[OsString],
    tail: Option<&OsStr>,
) -> io::Result<(PathBuf, Vec<u16>)> {
    if program
        .as_os_str()
        .encode_wide()
        .any(|c| c == u16::from(b'"'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a program's path cannot hold a quote",
        ));
    }
    let quoted_program = |line: &mut Vec<u16>| {
        line.push(u16::from(b'"'));
        line.extend(program.as_os_str().encode_wide());
        line.push(u16::from(b'"'));
    };
    if is_batch(program) {
        const UNSAFE: &[char] = &['"', '%', '^', '&', '|', '<', '>', '!', '\r', '\n'];
        if tail.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a batch file takes arguments, not a command line written out",
            ));
        }
        for arg in args {
            if arg.to_string_lossy().contains(UNSAFE) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "an argument for a batch file cannot hold any of {UNSAFE:?}: {}",
                        arg.to_string_lossy()
                    ),
                ));
            }
        }
        let shell = command_interpreter();
        let mut line: Vec<u16> = Vec::new();
        line.push(u16::from(b'"'));
        line.extend(shell.as_os_str().encode_wide());
        line.extend(r#"" /e:ON /v:OFF /d /c ""#.encode_utf16());
        quoted_program(&mut line);
        for arg in args {
            line.push(u16::from(b' '));
            line.push(u16::from(b'"'));
            line.extend(arg.encode_wide());
            line.push(u16::from(b'"'));
        }
        line.push(u16::from(b'"'));
        line.push(0);
        return Ok((shell, line));
    }
    let mut line: Vec<u16> = Vec::new();
    quoted_program(&mut line);
    for arg in args {
        line.push(u16::from(b' '));
        push_argument(arg, &mut line);
    }
    if let Some(tail) = tail {
        line.push(u16::from(b' '));
        line.extend(tail.encode_wide());
    }
    if line.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a command line cannot hold a NUL character",
        ));
    }
    line.push(0);
    Ok((program.to_path_buf(), line))
}

/// `cmd.exe` in this Windows's system folder — never one found elsewhere.
pub(super) fn command_interpreter() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join(r"System32\cmd.exe")
}

/// The environment block `launch` asks for: this process's unless cleared,
/// then its own settings, then the tree's mark — sorted by name without
/// case, as `CreateProcess` requires.
fn environment(launch: &Launch, tree: Option<&str>) -> io::Result<Vec<u16>> {
    let mut vars: BTreeMap<String, (OsString, OsString)> = BTreeMap::new();
    let mut set = |key: &OsStr, value: &OsStr| {
        vars.insert(
            key.to_string_lossy().to_uppercase(),
            (key.to_owned(), value.to_owned()),
        );
    };
    if !launch.clear_env {
        for (key, value) in std::env::vars_os() {
            set(&key, &value);
        }
    }
    for (key, value) in &launch.env {
        set(key, value);
    }
    if let Some(mark) = tree {
        set(OsStr::new(TREE_VAR), OsStr::new(mark));
    }
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in vars.values() {
        let key: Vec<u16> = key.encode_wide().collect();
        let value: Vec<u16> = value.encode_wide().collect();
        if key.is_empty()
            || key.contains(&0)
            || key.iter().skip(1).any(|c| *c == u16::from(b'='))
            || value.contains(&0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an environment variable's name or value cannot be passed to Windows",
            ));
        }
        block.extend(key);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// A process-thread attribute list, deleted when dropped. What its entries
/// point at must outlive the `CreateProcess` call.
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

/// A job object for one tree: named after its mark when it has one, so a
/// stop finds it by the mark; a process in it that crashes ends instead of
/// waiting on an error dialog nobody sees; `kill_on_close` ends the tree with
/// the last handle (a server, stopped with its connection); `confined` keeps
/// it from other programs' windows, the clipboard and the desktop.
fn new_job(mark: Option<&str>, kill_on_close: bool, confined: bool) -> io::Result<OwnedHandle> {
    let name = mark.map(job_name).transpose()?;
    let job = owned(unsafe {
        CreateJobObjectW(
            std::ptr::null(),
            name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
        )
    })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | if kill_on_close {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        } else {
            0
        };
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    if confined {
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };
        check(unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        })?;
    }
    Ok(job)
}

/// How a started process's standard streams are connected.
enum Streams<'a> {
    /// A pseudoconsole: its console, and through it stdin, stdout, stderr.
    Console(&'a PseudoConsole),
    /// The child's ends of three pipes, inherited by it alone.
    Pipes([HANDLE; 3]),
}

/// A process started, in its job.
struct Started {
    pid: u32,
    process: OwnedHandle,
    job: OwnedHandle,
}

fn start(
    launch: &Launch,
    placement: &Placement<'_>,
    streams: Streams<'_>,
    flags: u32,
    kill_on_close: bool,
) -> io::Result<Started> {
    let program = resolve(&launch.program, launch)?;
    let (application, mut line) =
        command_line(&program, &launch.args, launch.verbatim_tail.as_deref())?;
    let application = wide_path(&application)?;
    let environment = environment(launch, placement.tree)?;
    // The folder a process starts in as written: Windows keeps no verbatim
    // (`\\?\`) current folder, and cmd refuses to run in one.
    let folder = launch
        .dir
        .as_deref()
        .map(|dir| wide(dir.as_os_str()))
        .transpose()?;

    let mut attributes = Attributes::new(1)?;
    let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let console_handle: HPCON;
    let pipes: [HANDLE; 3];
    let inherit = match streams {
        Streams::Console(console) => {
            console_handle = console.handle();
            attributes.set(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console_handle as *const c_void,
                std::mem::size_of::<HPCON>(),
            )?;
            // Never this process's own standard handles, which a session
            // host has pointed at a log: the console's are the command's.
            info.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            info.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            0
        }
        Streams::Pipes(handles) => {
            pipes = handles;
            attributes.set(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                pipes.as_ptr().cast(),
                std::mem::size_of_val(&pipes),
            )?;
            info.StartupInfo.hStdInput = pipes[0];
            info.StartupInfo.hStdOutput = pipes[1];
            info.StartupInfo.hStdError = pipes[2];
            1
        }
    };
    info.lpAttributeList = attributes.list;
    let flags =
        flags | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let folder_ptr = folder.as_ref().map_or(std::ptr::null(), |f| f.as_ptr());
    let started = match placement.confinement {
        Some(confinement) => unsafe {
            CreateProcessAsUserW(
                confinement.token_for_start()?,
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                inherit,
                flags,
                environment.as_ptr().cast(),
                folder_ptr,
                &info.StartupInfo,
                &mut process,
            )
        },
        None => unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                inherit,
                flags,
                environment.as_ptr().cast(),
                folder_ptr,
                &info.StartupInfo,
                &mut process,
            )
        },
    };
    check(started)?;
    let handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    let placed = new_job(
        placement.tree,
        kill_on_close,
        placement.confinement.is_some(),
    )
    .and_then(|job| {
        check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), handle.as_raw_handle()) })
            .map(|()| job)
    })
    .and_then(|job| {
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            Err(io::Error::last_os_error())
        } else {
            Ok(job)
        }
    });
    match placed {
        Ok(job) => Ok(Started {
            pid: process.dwProcessId,
            process: handle,
            job,
        }),
        Err(error) => {
            // Never left suspended, or running outside its job.
            unsafe { TerminateProcess(handle.as_raw_handle(), 1) };
            Err(error)
        }
    }
}

/// Start `launch` on `terminal` — its console — in a job of its own (its
/// tree, stoppable as one: [`terminate_tree`]), under `placement`.
pub fn spawn_on_terminal(
    launch: &Launch,
    terminal: &PtySlave,
    placement: Placement<'_>,
) -> io::Result<Process> {
    let started = start(launch, &placement, Streams::Console(&terminal.0), 0, false)?;
    Ok(Process::new(
        started.pid,
        started.process,
        Some(started.job),
    ))
}

/// One pipe for a server's standard stream: Rune's end overlapped (read and
/// written asynchronously), the child's end inheritable. Named uniquely, the
/// first and only instance, refusing other machines, and only this user's.
fn server_pipe(rune_writes: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    use rand::RngCore;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        r"\\.\pipe\rune-{}-{}-{:016x}",
        unsafe { GetCurrentProcessId() },
        NEXT.fetch_add(1, Ordering::Relaxed),
        rand::rngs::OsRng.next_u64()
    );
    let name = wide(OsStr::new(&name))?;
    let direction = if rune_writes {
        PIPE_ACCESS_OUTBOUND
    } else {
        PIPE_ACCESS_INBOUND
    };
    let ours = owned(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            direction | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            super::security::private_attributes()?,
        )
    })?;
    let inherited = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let theirs = owned(unsafe {
        CreateFileW(
            name.as_ptr(),
            if rune_writes {
                GENERIC_READ
            } else {
                GENERIC_WRITE
            },
            0,
            &inherited,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    })?;
    Ok((ours, theirs))
}

/// Start `launch` with pipes for its stdin, stdout and stderr — a server
/// Rune speaks to — in a job that ends with it, under `placement`. Called
/// inside the async runtime its pipes are read on.
pub fn spawn_piped(launch: &Launch, placement: Placement<'_>) -> io::Result<PipedProcess> {
    let (stdin_ours, stdin_theirs) = server_pipe(true)?;
    let (stdout_ours, stdout_theirs) = server_pipe(false)?;
    let (stderr_ours, stderr_theirs) = server_pipe(false)?;
    let started = start(
        launch,
        &placement,
        Streams::Pipes([
            stdin_theirs.as_raw_handle(),
            stdout_theirs.as_raw_handle(),
            stderr_theirs.as_raw_handle(),
        ]),
        CREATE_NO_WINDOW,
        true,
    )?;
    // The child holds its ends now; ours are Rune's alone.
    drop((stdin_theirs, stdout_theirs, stderr_theirs));
    let pipe =
        |handle: OwnedHandle| -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
            unsafe {
                tokio::net::windows::named_pipe::NamedPipeServer::from_raw_handle(
                    handle.into_raw_handle(),
                )
            }
        };
    Ok(PipedProcess {
        pid: started.pid,
        process: started.process,
        job: started.job,
        stdin: Some(PipeWriter(pipe(stdin_ours)?)),
        stdout: Some(PipeReader(pipe(stdout_ours)?)),
        stderr: Some(PipeReader(pipe(stderr_ours)?)),
    })
}

/// Start `launch` on its own — no console, stdin and stdout to nothing,
/// stderr to `log` — a session host, which must outlive the window that
/// started it and the terminal that window runs in: out of its console
/// (`DETACHED_PROCESS`), out of its Ctrl+C (`CREATE_NEW_PROCESS_GROUP`), and
/// out of the terminal's job where that job lets it go.
pub fn spawn_detached(launch: &Launch, log: File) -> io::Result<Process> {
    let program = resolve(&launch.program, launch)?;
    let build = |breakaway: bool| -> io::Result<std::process::Child> {
        let mut command = std::process::Command::new(&program);
        command.args(&launch.args);
        if let Some(tail) = &launch.verbatim_tail {
            command.raw_arg(tail);
        }
        if let Some(dir) = &launch.dir {
            command.current_dir(dir);
        }
        if launch.clear_env {
            command.env_clear();
        }
        for (key, value) in &launch.env {
            command.env(key, value);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log.try_clone()?))
            .creation_flags(
                DETACHED_PROCESS
                    | CREATE_NEW_PROCESS_GROUP
                    | if breakaway {
                        CREATE_BREAKAWAY_FROM_JOB
                    } else {
                        0
                    },
            );
        command.spawn()
    };
    let child = match build(true) {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => build(false)?,
        other => other?,
    };
    let pid = child.id();
    let process = unsafe { OwnedHandle::from_raw_handle(child.into_raw_handle()) };
    Ok(Process::new(pid, process, None))
}

/// The processes Rune started and no longer watches through a [`Process`]:
/// kept by pid with their handles until they are reaped, as a Unix kernel
/// keeps an ended child until its parent collects it — so a pid is never
/// another process's before Rune has read how its own ended.
fn children() -> &'static Mutex<HashMap<u32, Child>> {
    static CHILDREN: OnceLock<Mutex<HashMap<u32, Child>>> = OnceLock::new();
    CHILDREN.get_or_init(Default::default)
}

struct Child {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
}

/// A process Rune started and watches through its handle.
pub struct Process {
    pid: u32,
    child: Option<Child>,
}

impl Process {
    fn new(pid: u32, process: OwnedHandle, job: Option<OwnedHandle>) -> Process {
        Process {
            pid,
            child: Some(Child { process, job }),
        }
    }

    pub fn id(&self) -> i32 {
        self.pid as i32
    }

    /// Its exit code once it ended, or `None` while it runs.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        match &self.child {
            Some(child) => ended(child.process.as_raw_handle(), 0),
            None => Ok(None),
        }
    }

    /// Wait for it to end; its exit code.
    pub fn wait(&mut self) -> io::Result<i32> {
        match &self.child {
            Some(child) => Ok(ended(child.process.as_raw_handle(), INFINITE)?.unwrap_or(1)),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this process is watched by pid now",
            )),
        }
    }

    /// Kill this process alone — what it started is [`terminate_tree`]'s.
    pub fn kill(&mut self) -> io::Result<()> {
        if let Some(child) = &self.child {
            if unsafe { TerminateProcess(child.process.as_raw_handle(), 1) } == 0
                && ended(child.process.as_raw_handle(), 0)?.is_none()
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Watched by pid from here on ([`reap`], [`group_alive`],
    /// [`terminate_tree`]); nothing is stopped or waited for here.
    pub fn into_pid(mut self) -> i32 {
        if let Some(child) = self.child.take() {
            crate::panic::lock(children()).insert(self.pid, child);
        }
        self.pid as i32
    }
}

impl Drop for Process {
    /// Dropped while it may still run, it is kept by pid, as on Unix a
    /// dropped child stays the parent's to collect.
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            if ended(child.process.as_raw_handle(), 0)
                .ok()
                .flatten()
                .is_none()
            {
                crate::panic::lock(children()).insert(self.pid, child);
            }
        }
    }
}

/// `process`'s exit code once it has ended, waiting up to `ms`.
fn ended(process: HANDLE, ms: u32) -> io::Result<Option<i32>> {
    match unsafe { WaitForSingleObject(process, ms) } {
        WAIT_OBJECT_0 => {
            let mut code = 0u32;
            check(unsafe { GetExitCodeProcess(process, &mut code) })?;
            Ok(Some(code as i32))
        }
        WAIT_TIMEOUT => Ok(None),
        _ => Err(io::Error::last_os_error()),
    }
}

/// A server process Rune speaks to over pipes ([`spawn_piped`]); its tree
/// ends when this is dropped.
pub struct PipedProcess {
    pid: u32,
    process: OwnedHandle,
    job: OwnedHandle,
    stdin: Option<PipeWriter>,
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
}

/// Rune's end of a server's stdin.
pub struct PipeWriter(tokio::net::windows::named_pipe::NamedPipeServer);

/// Rune's end of a server's stdout or stderr.
pub struct PipeReader(tokio::net::windows::named_pipe::NamedPipeServer);

impl PipedProcess {
    pub fn id(&self) -> Option<i32> {
        Some(self.pid as i32)
    }

    pub fn take_stdin(&mut self) -> Option<PipeWriter> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.stderr.take()
    }

    /// Stop it and what it started: its job ended. Windows has no polite
    /// request a process outside Rune's console would hear, so `grace_ms`
    /// waits for nothing.
    pub fn stop(&mut self, _grace_ms: u64) {
        unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
    }

    /// Wait for it to end; its exit code.
    pub async fn wait(&mut self) -> io::Result<i32> {
        let process = self.process.try_clone()?;
        tokio::task::spawn_blocking(move || {
            Ok(ended(process.as_raw_handle(), INFINITE)?.unwrap_or(1))
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl tokio::io::AsyncWrite for PipeWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl tokio::io::AsyncRead for PipeReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

/// The exit code a shell would report for `status`: Windows always has one.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Windows cannot make a running process another program
/// (`REPLACES_IN_PLACE` is false): a newer build is taken at the next start.
pub fn replace_process(
    _command: &mut std::process::Command,
    _arg0: Option<&std::ffi::OsStr>,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows cannot replace a running program: a newer build is taken at the next start",
    )
}

/// The exit code of child `pid` once it has ended, reaping it (`Ok(None)`
/// while it runs, with `wait` false). Only a process Rune started and keeps
/// by pid is its to reap.
pub fn reap(pid: i32, wait: bool) -> io::Result<Option<i32>> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one positive child PID",
        ));
    }
    let process = {
        let kept = crate::panic::lock(children());
        let Some(child) = kept.get(&(pid as u32)) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("process {pid} is not a child Rune keeps"),
            ));
        };
        child.process.try_clone()?
    };
    let code = ended(process.as_raw_handle(), if wait { INFINITE } else { 0 })?;
    if code.is_some() {
        crate::panic::lock(children()).remove(&(pid as u32));
    }
    Ok(code)
}

/// Whether `pid`, which Rune keeps, has ended (`Some(true)`: ended, not yet
/// reaped); `None` when Rune does not keep it. For tests.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn kept_ended(pid: i32) -> Option<bool> {
    let kept = crate::panic::lock(children());
    let child = kept.get(&(pid as u32))?;
    Some(
        ended(child.process.as_raw_handle(), 0)
            .ok()
            .flatten()
            .is_some(),
    )
}

/// Whether process `pid` runs. For tests.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn alive(pid: i32) -> bool {
    process_alive(pid)
}

/// Nothing to collect on Windows: no process is inherited across an `exec`.
pub fn reap_exited() -> usize {
    0
}

/// The job Rune keeps `pid` in, or the process itself, opened for `access`.
fn job_of(pid: i32) -> Option<OwnedHandle> {
    crate::panic::lock(children())
        .get(&(pid as u32))
        .and_then(|child| child.job.as_ref())
        .and_then(|job| job.try_clone().ok())
}

fn open_process(pid: i32, access: u32) -> Option<OwnedHandle> {
    if pid <= 0 {
        return None;
    }
    owned(unsafe { OpenProcess(access, 0, pid as u32) }).ok()
}

fn job_alive(job: &OwnedHandle) -> bool {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    let asked = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    asked != 0 && info.ActiveProcesses > 0
}

fn process_alive(pid: i32) -> bool {
    let Some(process) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return false;
    };
    let mut code = 0u32;
    let asked = unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) };
    asked != 0 && code == STILL_ACTIVE as u32
}

/// Whether the tree `pid` leads still runs: anything left in its job, or the
/// process itself.
pub fn group_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match job_of(pid) {
        Some(job) => job_alive(&job),
        None => process_alive(pid),
    }
}

/// Stop the tree `pid` leads: its job ended (or the process, when it has
/// none). Windows has no polite request a process outside Rune's console
/// would hear, so the stop is immediate; `grace_ms` is how long its end is
/// waited for.
pub fn terminate_group(pid: i32, grace_ms: u64) {
    match job_of(pid) {
        Some(job) => unsafe {
            TerminateJobObject(job.as_raw_handle(), 1);
        },
        None => {
            if let Some(process) = open_process(pid, PROCESS_TERMINATE) {
                unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            }
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    while group_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Stop a command's whole tree: its job, found by `pid` or by its `mark`
/// (named after it) — what left the command's process included, since
/// nothing leaves a job. A tree without a mark is its `pid`'s job alone.
pub fn terminate_tree(pid: i32, mark: Option<&str>, grace_ms: u64) {
    let named = mark
        .filter(|mark| !mark.is_empty())
        .and_then(|mark| job_name(mark).ok())
        .and_then(|name| {
            owned(unsafe {
                OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, 0, name.as_ptr())
            })
            .ok()
        });
    if let Some(job) = &named {
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    }
    if pid > 0 {
        terminate_group(pid, 0);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    while std::time::Instant::now() < deadline
        && (named.as_ref().is_some_and(job_alive) || (pid > 0 && group_alive(pid)))
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// When this machine started, as the kernel keeps it
/// (`SystemTimeOfDayInformation`): the same instant for every process of this
/// boot. `None` when the system does not say.
pub fn boot_time() -> Option<std::time::SystemTime> {
    use windows_sys::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemTimeOfDayInformation,
    };
    // Its first field is the boot time, in 100 ns since 1601.
    let mut info = [0i64; 6];
    let status = unsafe {
        NtQuerySystemInformation(
            SystemTimeOfDayInformation,
            info.as_mut_ptr().cast(),
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 || info[0] <= 0 {
        return None;
    }
    filetime_to_system(info[0] as u64)
}

/// Windows's 100 ns since 1601 as a moment.
fn filetime_to_system(ticks: u64) -> Option<std::time::SystemTime> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(since.saturating_mul(100)))
}

/// When process `pid` began, in Windows's 100 ns since 1601: with the pid,
/// the process's identity. `None` once it is gone.
pub fn process_started(pid: u32) -> Option<u64> {
    let process = open_process(
        pid as i32,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
    )?;
    if ended(process.as_raw_handle(), 0).ok()?.is_some() {
        return None;
    }
    let mut created: windows_sys::Win32::Foundation::FILETIME = unsafe { std::mem::zeroed() };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    check(unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    })
    .ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// The file this process was started from, opened when it started
/// ([`at_start`]): a release may rename it and put another at its path.
static OWN_PROGRAM: OnceLock<Option<File>> = OnceLock::new();

fn own() -> Option<&'static File> {
    OWN_PROGRAM
        .get_or_init(|| {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            // Shared every way: an installer may still rename it.
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(std::env::current_exe().ok()?)
                .ok()
        })
        .as_ref()
}

/// What this process notes about itself first thing in `main`: the file it
/// runs from, before a release can put another at its path.
pub fn at_start() {
    let _ = own();
}

/// Whether this process runs elevated — an Administrator's full rights.
pub fn running_elevated() -> bool {
    super::security::Token::of_this_process(windows_sys::Win32::Security::TOKEN_QUERY)
        .map(|token| token.elevated())
        .unwrap_or(false)
}

/// Windows hands no program on (`REPLACES_IN_PLACE` is false).
pub fn own_program() -> io::Result<Handed> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows hands no program on: a newer build is taken at the next start",
    ))
}

/// Windows hands no program on (`REPLACES_IN_PLACE` is false).
pub fn program_at(_handed: Handed) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no previous program was handed on",
    ))
}

/// The file this process was started from, whatever is installed at its
/// path now — compared with [`FileIdentity::of_path`] of the installed path,
/// it tells that a release replaced it.
pub fn running_program() -> io::Result<FileIdentity> {
    match own() {
        Some(file) => FileIdentity::of_file(file),
        None => FileIdentity::of_path(&std::env::current_exe()?),
    }
}

/// The path this process's program is installed at — where a release puts
/// the next build.
pub fn installed_program() -> io::Result<PathBuf> {
    std::env::current_exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(program: &str, args: &[&str], tail: Option<&str>) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let (_, wide) = command_line(Path::new(program), &args, tail.map(OsStr::new)).unwrap();
        String::from_utf16_lossy(&wide[..wide.len() - 1])
    }

    /// Arguments reach the program exactly as given, as the C runtime reads
    /// them back.
    #[test]
    fn arguments_are_quoted_as_the_c_runtime_reads_them() {
        assert_eq!(
            line(r"C:\p\x.exe", &["a", "b c", ""], None),
            r#""C:\p\x.exe" a "b c" """#
        );
        assert_eq!(
            line(r"C:\x.exe", &[r#"say "hi""#], None),
            r#""C:\x.exe" "say \"hi\"""#
        );
        assert_eq!(
            line(r"C:\x.exe", &[r"C:\dir\ x\"], None),
            r#""C:\x.exe" "C:\dir\ x\\""#
        );
        assert_eq!(line(r"C:\x.exe", &[r"a\\b"], None), r#""C:\x.exe" a\\b"#);
        assert_eq!(
            line(
                r"C:\Windows\System32\cmd.exe",
                &["/D", "/S", "/C"],
                Some(r#""echo a & echo b""#)
            ),
            r#""C:\Windows\System32\cmd.exe" /D /S /C "echo a & echo b""#
        );
    }

    /// A batch file runs through cmd.exe explicitly; an argument cmd.exe
    /// would read as its own syntax is refused.
    #[test]
    fn a_batch_file_is_run_by_cmd_and_never_given_its_syntax() {
        let args = [OsString::from("serve"), OsString::from("--port 3000")];
        let (program, wide) = command_line(Path::new(r"C:\tools\npx.cmd"), &args, None).unwrap();
        assert!(
            program.ends_with(r"System32\cmd.exe"),
            "{}",
            program.display()
        );
        let text = String::from_utf16_lossy(&wide[..wide.len() - 1]);
        assert!(
            text.ends_with(r#"/c ""C:\tools\npx.cmd" "serve" "--port 3000"""#),
            "{text}"
        );
        for bad in ["a&b", "%PATH%", "x|y", "q\"q"] {
            assert!(
                command_line(Path::new(r"C:\t\x.bat"), &[OsString::from(bad)], None).is_err(),
                "{bad}"
            );
        }
    }

    /// The environment block is sorted without case, the launch's settings
    /// replacing what they name whatever its case, and the mark added.
    #[test]
    fn the_environment_block_is_sorted_and_carries_the_mark() {
        let launch = Launch::new("x")
            .clear_env()
            .env("Path", r"C:\bin")
            .env("b", "2")
            .env("A", "1")
            .env("PATH", r"C:\other");
        let block = environment(&launch, Some("m1")).unwrap();
        let text = String::from_utf16_lossy(&block);
        assert_eq!(text, "A=1\0b=2\0PATH=C:\\other\0RUNE_PROCESS_TREE=m1\0\0");
        assert!(environment(&Launch::new("x").clear_env().env("BAD", "a\0b"), None).is_err());
    }

    /// A program is found on PATH with PATHEXT; a relative PATH folder —
    /// which would mean wherever Rune was started — is never searched.
    #[test]
    fn a_program_is_found_on_path_never_in_a_relative_folder() {
        let dir = std::env::temp_dir().join(format!("rune-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("tool.exe"), "").unwrap();
        let search = std::env::join_paths([PathBuf::from("bin"), dir.join("bin")]).unwrap();
        let launch = Launch::new("tool")
            .clear_env()
            .env("PATH", &search)
            .env("PATHEXT", ".EXE");
        // Found with PATHEXT's spelling of its ending: the same file, as the
        // volume compares names without case.
        let found = resolve(Path::new("tool"), &launch).unwrap();
        assert!(
            super::super::files::same_file(&found, &dir.join("bin").join("tool.exe")),
            "{}",
            found.display()
        );
        let relative_only = Launch::new("tool").clear_env().env("PATH", "bin");
        assert!(resolve(Path::new("tool"), &relative_only).is_err());
        assert!(resolve(Path::new("absent"), &launch).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_detached_process_is_reaped_by_pid_once() {
        let dir = std::env::temp_dir().join(format!("rune-detached-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = File::create(dir.join("log")).unwrap();
        let child =
            spawn_detached(&Launch::new("cmd.exe").args(["/D", "/C", "exit 7"]), log).unwrap();
        let pid = child.into_pid();
        assert_eq!(reap(pid, true).unwrap(), Some(7));
        assert!(reap(pid, false).is_err(), "reaped once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tree_is_stopped_by_its_mark() {
        let (pty, slave) = crate::platform::open_pty(24, 80).unwrap();
        let mark = new_tree_mark();
        let child = spawn_on_terminal(
            &Launch::new("cmd.exe").args(["/D", "/C", "ping -n 30 127.0.0.1 >NUL"]),
            &slave,
            Placement {
                tree: Some(&mark),
                ..Placement::default()
            },
        )
        .unwrap();
        let pid = child.into_pid();
        assert!(group_alive(pid));
        terminate_tree(0, Some(&mark), 2000);
        assert!(!group_alive(pid), "the mark reached the tree");
        assert!(reap(pid, true).unwrap().is_some());
        drop(pty);
    }

    #[test]
    fn this_machine_and_this_process_have_a_start() {
        let boot = boot_time().unwrap();
        assert!(boot < std::time::SystemTime::now());
        assert_eq!(boot_time(), Some(boot), "the same instant every time");
        assert!(process_started(std::process::id()).is_some());
        at_start();
        assert_eq!(
            running_program().unwrap(),
            FileIdentity::of_path(&installed_program().unwrap()).unwrap()
        );
    }
}
