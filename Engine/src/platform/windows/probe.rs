//! Which way of confining commands real Windows programs live under — a
//! measurement, not a contract. Each [`Mechanism`] is given the same grants
//! (the project and a scratch folder written, the tools' folders read) and
//! the same battery: the shells a person uses (cmd, both PowerShells, Git
//! Bash), the tools a build runs (git, Python, Node, npm, cargo), what must
//! work (writing in the project, the network, a server of the command's
//! own) and what must not (reading or writing elsewhere in the profile,
//! reaching a pipe of the user's). A table is printed; nothing is asserted.
//!
//! Run on a real Windows by CI when a commit names `[ci probe]`:
//! `cargo test -p runeengine --lib platform::windows::probe -- --ignored --nocapture`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::confine::{Confinement, Grants, Mechanism};
use super::process::spawn_on_terminal;
use super::pty::open_pty;
use crate::platform::{Launch, Placement};

/// How long one command may take before it counts as hung.
const LIMIT: Duration = Duration::from_secs(25);

/// What a check needs to see.
enum Expect {
    /// Exit 0, and the marker in what it printed.
    Runs(&'static str),
    /// Whatever it printed: the predicate says whether what must hold does
    /// (kept out, built).
    Holds(fn(&Probe, &str) -> bool),
}

struct Check {
    name: &'static str,
    program: PathBuf,
    args: Vec<String>,
    /// The end of its command line as written: what cmd reads after `/C`.
    tail: Option<String>,
    expect: Expect,
}

/// `line` run by cmd as Rune's terminal runs it: `/D /S /C "line"`.
fn cmd(name: &'static str, line: String, expect: Expect) -> Check {
    Check {
        name,
        program: PathBuf::from(r"C:\Windows\System32\cmd.exe"),
        args: vec!["/D".into(), "/S".into(), "/C".into()],
        tail: Some(format!("\"{line}\"")),
        expect,
    }
}

/// The folders and servers one battery runs against.
struct Probe {
    /// The person's own profile.
    person: PathBuf,
    /// A folder at the root of the system drive: what every signed-in user
    /// may read and change unless told otherwise.
    open: PathBuf,
    project: PathBuf,
    outside: PathBuf,
    pipe_name: String,
    pipe_reached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    port: u16,
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn git_bash() -> Option<PathBuf> {
    let git = on_path("git.exe")?;
    // …\Git\cmd\git.exe or …\Git\bin\git.exe or …\Git\mingw64\bin\git.exe
    git.ancestors()
        .map(|dir| dir.join("bin").join("bash.exe"))
        .find(|bash| bash.is_file())
}

fn checks(probe: &Probe) -> Vec<Check> {
    let outside = probe.outside.display();
    let mut checks = vec![
        cmd("cmd echo", "echo probe-ok".into(), Expect::Runs("probe-ok")),
        cmd(
            "cmd nested child",
            "cmd /D /C echo probe-ok".into(),
            Expect::Runs("probe-ok"),
        ),
        cmd(
            "cmd write in project",
            "echo x> written.txt && type written.txt && echo probe-ok".into(),
            Expect::Runs("probe-ok"),
        ),
        cmd(
            "cmd read outside control (must show)",
            format!("type \"{outside}\\secret.txt\" 2>&1 & dir /b \"{outside}\""),
            Expect::Holds(|_, _| true),
        ),
        cmd(
            "cmd read outside (must fail)",
            format!("type \"{outside}\\secret.txt\""),
            Expect::Holds(|_, out| !out.contains("probe-secret")),
        ),
        cmd(
            "cmd list profile (must fail)",
            format!(
                "dir /b \"{}\\Documents\" && echo probe-listed",
                probe.person.display()
            ),
            Expect::Holds(|_, out| !out.contains("probe-listed")),
        ),
        cmd(
            "cmd read open folder (must fail)",
            format!("type \"{}\\secret.txt\"", probe.open.display()),
            Expect::Holds(|_, out| !out.contains("probe-secret")),
        ),
        cmd(
            "cmd write open folder (must fail)",
            format!("echo x> \"{}\\written.txt\"", probe.open.display()),
            Expect::Holds(|p, _| !p.open.join("written.txt").exists()),
        ),
        cmd(
            "cmd write outside (must fail)",
            format!("echo x> \"{outside}\\written.txt\""),
            Expect::Holds(|p, _| !p.outside.join("written.txt").exists()),
        ),
        cmd(
            "cmd user pipe (must fail)",
            format!("echo x> {}", probe.pipe_name),
            Expect::Holds(|p, _| {
                std::thread::sleep(Duration::from_millis(300));
                !p.pipe_reached.load(std::sync::atomic::Ordering::SeqCst)
            }),
        ),
        cmd(
            "powershell web request",
            "powershell -NoLogo -NoProfile -Command \"(Invoke-WebRequest -UseBasicParsing https://example.com/).StatusCode\"".into(),
            Expect::Runs("200"),
        ),
        Check {
            name: "powershell 5.1",
            program: PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ('probe-' + 'ok')".into(),
            ],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        },
        Check {
            name: "curl https out",
            program: PathBuf::from(r"C:\Windows\System32\curl.exe"),
            args: vec![
                "-sS".into(),
                "-o".into(),
                "probe-body.txt".into(),
                "-w".into(),
                "probe-%{http_code}".into(),
                "https://example.com/".into(),
            ],
            tail: None,
            expect: Expect::Runs("probe-200"),
        },
        Check {
            name: "curl localhost server of the user",
            program: PathBuf::from(r"C:\Windows\System32\curl.exe"),
            args: vec!["-sS".into(), format!("http://127.0.0.1:{}/", probe.port)],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        },
    ];
    if let Some(pwsh) = on_path("pwsh.exe") {
        checks.push(Check {
            name: "pwsh 7",
            program: pwsh,
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ('probe-' + 'ok')".into(),
            ],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
    }
    if let Some(bash) = git_bash() {
        checks.push(Check {
            name: "git bash echo",
            program: bash.clone(),
            args: vec!["-c".into(), "echo probe-ok".into()],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
        checks.push(Check {
            name: "git bash fork+pipe",
            program: bash,
            args: vec![
                "-c".into(),
                "x=$(echo probe); echo \"$x-ok\" | cat; ls / >/dev/null".into(),
            ],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
    }
    if let Some(git) = on_path("git.exe") {
        checks.push(Check {
            name: "git status in project",
            program: git,
            args: vec![
                "-c".into(),
                "safe.directory=*".into(),
                "status".into(),
                "--short".into(),
                "--branch".into(),
            ],
            tail: None,
            expect: Expect::Runs("##"),
        });
    }
    if let Some(git) = on_path("git.exe") {
        checks.push(Check {
            name: "git https ls-remote",
            program: git.clone(),
            args: vec![
                "ls-remote".into(),
                "--heads".into(),
                "https://github.com/octocat/Hello-World".into(),
            ],
            tail: None,
            expect: Expect::Runs("refs/heads"),
        });
        if let Some(curl) = git
            .ancestors()
            .map(|dir| dir.join("mingw64").join("bin").join("curl.exe"))
            .find(|curl| curl.is_file())
        {
            checks.push(Check {
                name: "git's curl (openssl) https",
                program: curl,
                args: vec![
                    "-sS".into(),
                    "-o".into(),
                    "probe-body.txt".into(),
                    "-w".into(),
                    "probe-%{http_code}".into(),
                    "https://example.com/".into(),
                ],
                tail: None,
                expect: Expect::Runs("probe-200"),
            });
        }
    }
    if let Some(python) = on_path("python.exe") {
        checks.push(Check {
            name: "python",
            program: python.clone(),
            args: vec!["-c".into(), "print('probe-' + 'ok')".into()],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
        checks.push(Check {
            name: "python own localhost server",
            program: python,
            args: vec![
                "-c".into(),
                "import socket;s=socket.socket();s.bind(('127.0.0.1',0));s.listen();\
                 c=socket.create_connection(s.getsockname());a,_=s.accept();\
                 c.send(b'probe-ok');print(a.recv(8).decode())"
                    .into(),
            ],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
    }
    if let Some(node) = on_path("node.exe") {
        checks.push(Check {
            name: "node",
            program: node,
            args: vec!["-e".into(), "console.log('probe-' + 'ok')".into()],
            tail: None,
            expect: Expect::Runs("probe-ok"),
        });
    }
    if on_path("npm.cmd").is_some() {
        checks.push(cmd(
            "npm --version (batch)",
            "npm --version && echo probe-ok".into(),
            Expect::Runs("probe-ok"),
        ));
    }
    if let Some(cargo) = on_path("cargo.exe") {
        checks.push(Check {
            name: "cargo build (msvc link)",
            program: cargo,
            args: vec![
                "build".into(),
                "--offline".into(),
                "--quiet".into(),
                "--manifest-path".into(),
                probe
                    .project
                    .join("hello")
                    .join("Cargo.toml")
                    .display()
                    .to_string(),
            ],
            tail: None,
            expect: Expect::Holds(|p, _| {
                // Built: the program is there (a "kept out" that is a success).
                p.project
                    .join("hello")
                    .join("target")
                    .join("debug")
                    .join("hello.exe")
                    .is_file()
            }),
        });
    }
    checks
}

/// Where a check runs and what it is given beside this process's
/// environment.
struct Place<'a> {
    project: &'a std::path::Path,
    scratch: &'a std::path::Path,
    env: &'a [(String, String)],
}

/// Run one check under `confinement` (none: as this process runs): what it
/// printed, its exit code (none: hung and stopped), how long it took.
fn run(
    check: &Check,
    place: &Place<'_>,
    confinement: Option<&Confinement>,
) -> (String, Option<i32>, Duration) {
    let started = Instant::now();
    let (pty, slave) = match open_pty(30, 200) {
        Ok(pair) => pair,
        Err(e) => return (format!("open_pty: {e}"), None, started.elapsed()),
    };
    let mut launch = Launch::new(&check.program).args(&check.args);
    if let Some(tail) = &check.tail {
        launch = launch.verbatim_tail(tail);
    }
    let mut launch = launch
        .dir(place.project.to_path_buf())
        .env("TEMP", place.scratch)
        .env("TMP", place.scratch);
    for (key, value) in place.env {
        launch = launch.env(key, value);
    }
    let mut child = match spawn_on_terminal(
        &launch,
        &slave,
        Placement {
            confinement,
            tree: None,
        },
    ) {
        Ok(child) => child,
        Err(e) => return (format!("spawn: {e}"), None, started.elapsed()),
    };
    drop(slave);
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let mut code = None;
    while started.elapsed() < LIMIT {
        if let Ok(Some(n)) = pty.read_timeout(&mut buf, 100) {
            seen.extend_from_slice(&buf[..n]);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            code = Some(exit);
            // What is still on its way.
            let until = Instant::now() + Duration::from_millis(400);
            while Instant::now() < until {
                if let Ok(Some(n)) = pty.read_timeout(&mut buf, 50) {
                    seen.extend_from_slice(&buf[..n]);
                }
            }
            break;
        }
    }
    if code.is_none() {
        let _ = child.kill();
    }
    (
        plain(&String::from_utf8_lossy(&seen)),
        code,
        started.elapsed(),
    )
}

/// What a console showed, without its escape sequences, on one line.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if c.is_control() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A pipe of the user's, as a program of the user makes one (the default
/// security: the user, SYSTEM, Administrators), noting whether anything
/// connected.
fn user_pipe(name: &str, reached: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let wide = super::wide::wide(std::ffi::OsStr::new(name)).unwrap();
    std::thread::spawn(move || loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        let Ok(pipe) = super::wide::owned(pipe) else {
            return;
        };
        use std::os::windows::io::AsRawHandle;
        let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) };
        // ERROR_PIPE_CONNECTED: the client came before the wait.
        if connected != 0 || std::io::Error::last_os_error().raw_os_error() == Some(535) {
            reached.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
}

/// A web server of the user's on the loopback, answering `probe-ok`.
fn user_server() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 1024];
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 8\r\n\r\nprobe-ok");
        }
    });
    port
}

/// Where each tool of the battery is installed, for reading: what Rune would
/// have to grant a container for the tool to start.
fn tool_folders() -> Vec<PathBuf> {
    ["git.exe", "python.exe", "node.exe", "pwsh.exe", "cargo.exe"]
        .into_iter()
        .filter_map(on_path)
        .filter_map(|exe| {
            let dir = exe.parent()?;
            // The install root: one up from a bin/cmd/Scripts folder.
            let name = dir.file_name()?.to_string_lossy().to_lowercase();
            Some(if ["bin", "cmd", "scripts"].contains(&name.as_str()) {
                dir.parent()?.to_path_buf()
            } else {
                dir.to_path_buf()
            })
        })
        .collect()
}

/// One check as a runner started as another account reads it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Job {
    /// `plain`: as that account; `restricted`: under a restricted token of
    /// its own, granted what the battery grants.
    mode: String,
    program: PathBuf,
    args: Vec<String>,
    tail: Option<String>,
    project: PathBuf,
    scratch: PathBuf,
    env: Vec<(String, String)>,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
}

/// The local account a CI step made for the probe (`RUNE_PROBE_USER`,
/// `RUNE_PROBE_PASSWORD`), and its SID.
struct Account {
    name: String,
    password: String,
    sid: super::security::Sid,
}

impl Account {
    fn from_env() -> Option<Account> {
        let name = std::env::var("RUNE_PROBE_USER").ok()?;
        let password = std::env::var("RUNE_PROBE_PASSWORD").ok()?;
        let sid = match account_sid(&name) {
            Ok(sid) => sid,
            Err(e) => {
                println!("PROBE account {name}: no SID: {e}");
                return None;
            }
        };
        Some(Account {
            name,
            password,
            sid,
        })
    }
}

fn account_sid(name: &str) -> std::io::Result<super::security::Sid> {
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
    let wide = super::wide::wide(std::ffi::OsStr::new(name))?;
    let mut sid = vec![0u8; 256];
    let mut sid_len = sid.len() as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind: SID_NAME_USE = 0;
    super::wide::check(unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            wide.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    })?;
    super::security::Sid::copy(sid.as_mut_ptr().cast())
}

/// Start `exe` (this test binary) as `account`, its profile loaded, running
/// only the runner test; what it printed, and its exit code.
fn as_account(
    account: &Account,
    exe: &std::path::Path,
    dir: &std::path::Path,
) -> (String, Option<u32>) {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessWithLogonW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
        CREATE_NO_WINDOW, LOGON_WITH_PROFILE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOW,
    };
    let wide = |text: &str| super::wide::wide(std::ffi::OsStr::new(text)).unwrap();
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, &inherit, 0) } == 0 {
        return (
            format!("CreatePipe: {}", std::io::Error::last_os_error()),
            None,
        );
    }
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    let mut info: STARTUPINFOW = unsafe { std::mem::zeroed() };
    info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    info.dwFlags = STARTF_USESTDHANDLES;
    info.hStdOutput = write.as_raw_handle();
    info.hStdError = write.as_raw_handle();
    let application = wide(&exe.display().to_string());
    let mut line = wide(&format!(
        "\"{}\" platform::windows::probe::probe_runner --exact --ignored --nocapture --test-threads=1",
        exe.display()
    ));
    let (user, domain, password) = (wide(&account.name), wide("."), wide(&account.password));
    let folder = wide(&dir.display().to_string());
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let started = unsafe {
        CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON_WITH_PROFILE,
            application.as_ptr(),
            line.as_mut_ptr(),
            CREATE_NO_WINDOW,
            std::ptr::null(),
            folder.as_ptr(),
            &info,
            &mut process,
        )
    };
    if started == 0 {
        return (
            format!(
                "CreateProcessWithLogonW: {}",
                std::io::Error::last_os_error()
            ),
            None,
        );
    }
    drop(write);
    let handle = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    drop(unsafe { OwnedHandle::from_raw_handle(process.hThread) });
    let reader = std::thread::spawn(move || {
        let mut file = std::fs::File::from(read);
        let mut out = Vec::new();
        let _ = file.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    });
    let waited =
        unsafe { WaitForSingleObject(handle.as_raw_handle(), (LIMIT.as_millis() + 60_000) as u32) };
    let code = if waited == WAIT_OBJECT_0 {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) };
        Some(code)
    } else {
        unsafe { TerminateProcess(handle.as_raw_handle(), 1) };
        None
    };
    (reader.join().unwrap_or_default(), code)
}

/// The runner: run as another account by [`as_account`], one check from the
/// `job.json` beside this binary, said on one `RESULT` line. Without a job,
/// nothing.
#[test]
#[ignore = "started by the probe as another account"]
fn probe_runner() {
    let Some(job) = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("job.json"))
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Job>(&bytes).ok())
    else {
        return;
    };
    let check = Check {
        name: "job",
        program: job.program.clone(),
        args: job.args.clone(),
        tail: job.tail.clone(),
        expect: Expect::Runs(""),
    };
    let place = Place {
        project: &job.project,
        scratch: &job.scratch,
        env: &job.env,
    };
    let confinement = match job.mode.as_str() {
        "restricted" => match Confinement::build_as(
            Grants {
                project: &job.project,
                reads: &job.reads,
                writes: &job.writes,
                ledger: None,
                reach: &[],
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            Mechanism::RestrictedToken,
        ) {
            Ok(confinement) => Some(confinement),
            Err(why) => {
                println!("RESULT\tbuild\t{}", why.reason);
                return;
            }
        },
        _ => None,
    };
    let (out, code, _) = run(&check, &place, confinement.as_ref());
    println!(
        "RESULT\t{}\t{out}",
        code.map_or("hung".to_string(), |c| c.to_string())
    );
}

/// What the runner said: its output and exit code, or why it said nothing.
fn runner_result(said: &str) -> (String, Option<i32>) {
    match said.lines().find_map(|line| line.strip_prefix("RESULT\t")) {
        Some(rest) => {
            let (code, out) = rest.split_once('\t').unwrap_or((rest, ""));
            (out.to_string(), code.parse().ok())
        }
        None => (format!("no runner result: {}", plain(said)), None),
    }
}

#[test]
#[ignore = "a measurement for CI on a real Windows: [ci probe]"]
fn sandbox_mechanisms_measured() {
    use super::security::{grant, revoke, Token};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    };
    let profile = super::home::home_dir().expect("a profile");
    let base = profile.join(format!("rune-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    let scratch = base.join("scratch");
    let outside = base.join("outside");
    let runner = base.join("runner");
    for dir in [&project, &scratch, &outside, &runner] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(outside.join("secret.txt"), "probe-secret").unwrap();
    let open = PathBuf::from(format!(r"C:\rune-probe-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&open);
    std::fs::create_dir_all(&open).unwrap();
    std::fs::write(open.join("secret.txt"), "probe-secret").unwrap();
    let hello = project.join("hello");
    std::fs::create_dir_all(hello.join("src")).unwrap();
    std::fs::write(
        hello.join("Cargo.toml"),
        "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(hello.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status();
    let project = super::canonical(&project).unwrap();
    let scratch = super::canonical(&scratch).unwrap();
    let ledger = base.join("sandbox-grants.txt");

    let pipe_name = format!(r"\\.\pipe\rune-probe-{}", std::process::id());
    let pipe_reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    user_pipe(&pipe_name, pipe_reached.clone());
    let probe = Probe {
        person: profile.clone(),
        open: open.clone(),
        project: project.clone(),
        outside: outside.clone(),
        pipe_name,
        pipe_reached,
        port: user_server(),
    };

    let mut reads: Vec<PathBuf> = tool_folders();
    for name in [".cargo", ".rustup", ".gitconfig"] {
        reads.push(profile.join(name));
    }
    let writes = vec![scratch.clone()];
    // What a command of another account needs to find this one's tools.
    let env: Vec<(String, String)> = ["PATH", "PATHEXT"]
        .into_iter()
        .filter_map(|key| Some((key.to_string(), std::env::var(key).ok()?)))
        .chain([
            (
                "RUSTUP_HOME".to_string(),
                profile.join(".rustup").display().to_string(),
            ),
            (
                "CARGO_HOME".to_string(),
                profile.join(".cargo").display().to_string(),
            ),
        ])
        .collect();
    let place = Place {
        project: &project,
        scratch: &scratch,
        env: &env,
    };
    let elevated = Token::of_this_process(TOKEN_QUERY).is_ok_and(|t| t.elevated());
    println!("PROBE elevated={elevated} reads={reads:?}");

    // The account's own entries: the project and scratch written, the
    // tools read, this binary's copy run — each taken back at the end.
    let account = Account::from_env();
    let mut account_grants: Vec<PathBuf> = Vec::new();
    let runner_exe = runner.join("probe.exe");
    if let Some(account) = &account {
        std::fs::copy(std::env::current_exe().unwrap(), &runner_exe).unwrap();
        for (path, write) in [(&project, true), (&scratch, true), (&runner, false)]
            .into_iter()
            .chain(reads.iter().map(|path| (path, false)))
        {
            if !path.exists() {
                continue;
            }
            let mask = if write {
                FILE_ALL_ACCESS
            } else {
                FILE_GENERIC_READ | FILE_GENERIC_EXECUTE
            };
            match grant(path, &account.sid, mask, path.is_dir()) {
                Ok(_) => account_grants.push(path.clone()),
                Err(e) => println!("PROBE account grant {}: {e}", path.display()),
            }
        }
        println!("PROBE account {} = {:?}", account.name, account.sid);
    } else {
        println!("PROBE no account (RUNE_PROBE_USER unset): its candidates are skipped");
    }

    // (label, mechanism here, mode of the runner as the account)
    let candidates: [(&str, Mechanism, Option<&str>); 4] = [
        ("restricted-token", Mechanism::RestrictedToken, None),
        ("account", Mechanism::RestrictedToken, Some("plain")),
        (
            "account+restricted-token",
            Mechanism::RestrictedToken,
            Some("restricted"),
        ),
        (
            "appcontainer",
            Mechanism::AppContainer {
                profile: true,
                filtered: true,
            },
            None,
        ),
    ];
    let mut summary = Vec::new();
    for (label, mechanism, as_account_mode) in candidates {
        if as_account_mode.is_some() && account.is_none() {
            continue;
        }
        let confinement = match Confinement::build_as(
            Grants {
                project: &project,
                reads: &reads,
                writes: &writes,
                ledger: Some(&ledger),
                reach: &[],
                local_services: false,
                sockets: &[],
                ask_service: None,
            },
            mechanism,
        ) {
            Ok(confinement) => confinement,
            Err(why) => {
                println!("PROBE {label} | build | FAIL | {}", why.reason);
                summary.push(format!("{label}: build failed"));
                continue;
            }
        };
        // The entries written at the first start: how long a person waits.
        // A runner of the account finds those of its restricted token here.
        let granting = Instant::now();
        let granted = confinement.token_for_start().map(drop);
        println!(
            "PROBE {label} | grants written | {} | {} ms",
            granted.map_or_else(|e| format!("FAIL {e}"), |()| "ok".into()),
            granting.elapsed().as_millis()
        );
        let mut passed = 0;
        let all = checks(&probe);
        for check in &all {
            probe
                .pipe_reached
                .store(false, std::sync::atomic::Ordering::SeqCst);
            for leftover in [outside.join("written.txt"), open.join("written.txt")] {
                let _ = std::fs::remove_file(leftover);
            }
            let _ = std::fs::remove_dir_all(hello.join("target"));
            let started = Instant::now();
            let (out, code) = match (as_account_mode, &account) {
                (Some(mode), Some(account)) => {
                    let job = Job {
                        mode: mode.to_string(),
                        program: check.program.clone(),
                        args: check.args.clone(),
                        tail: check.tail.clone(),
                        project: project.clone(),
                        scratch: scratch.clone(),
                        env: env.clone(),
                        reads: reads.clone(),
                        writes: writes.clone(),
                    };
                    std::fs::write(runner.join("job.json"), serde_json::to_vec(&job).unwrap())
                        .unwrap();
                    let (said, _) = as_account(account, &runner_exe, &runner);
                    runner_result(&said)
                }
                _ => {
                    let (out, code, _) = run(check, &place, Some(&confinement));
                    (out, code)
                }
            };
            let took = started.elapsed();
            let pass = match check.expect {
                Expect::Runs(marker) => code == Some(0) && out.contains(marker),
                Expect::Holds(kept) => kept(&probe, &out),
            };
            passed += usize::from(pass);
            let snippet: String = out.chars().take(300).collect();
            println!(
                "PROBE {label} | {} | {} | {} ms | exit {} | {snippet}",
                check.name,
                if pass { "PASS" } else { "FAIL" },
                took.as_millis(),
                code.map_or("hung".to_string(), |c| c.to_string()),
            );
        }
        summary.push(format!("{label}: {passed}/{}", all.len()));
        confinement.forget_profile();
    }
    println!("PROBE summary: {}", summary.join("; "));
    if let Some(account) = &account {
        for path in &account_grants {
            let _ = revoke(path, &account.sid);
        }
    }
    let _ = super::confine::forget_grants(&ledger);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&open);
}
