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
    expect: Expect,
}

/// The folders and servers one battery runs against.
struct Probe {
    project: PathBuf,
    scratch: PathBuf,
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
    let cmd = PathBuf::from(r"C:\Windows\System32\cmd.exe");
    let mut checks = vec![
        Check {
            name: "cmd echo",
            program: cmd.clone(),
            args: vec!["/D".into(), "/C".into(), "echo probe-ok".into()],
            expect: Expect::Runs("probe-ok"),
        },
        Check {
            name: "cmd nested child",
            program: cmd.clone(),
            args: vec!["/D".into(), "/C".into(), "cmd /D /C echo probe-ok".into()],
            expect: Expect::Runs("probe-ok"),
        },
        Check {
            name: "cmd write in project",
            program: cmd.clone(),
            args: vec![
                "/D".into(),
                "/C".into(),
                "echo x> written.txt && type written.txt && echo probe-ok".into(),
            ],
            expect: Expect::Runs("probe-ok"),
        },
        Check {
            name: "cmd read outside (must fail)",
            program: cmd.clone(),
            args: vec![
                "/D".into(),
                "/C".into(),
                format!("type \"{}\"", probe.outside.join("secret.txt").display()),
            ],
            expect: Expect::Holds(|_, out| !out.contains("probe-secret")),
        },
        Check {
            name: "cmd write outside (must fail)",
            program: cmd.clone(),
            args: vec![
                "/D".into(),
                "/C".into(),
                format!(
                    "echo x> \"{}\"",
                    probe.outside.join("written.txt").display()
                ),
            ],
            expect: Expect::Holds(|p, _| !p.outside.join("written.txt").exists()),
        },
        Check {
            name: "cmd user pipe (must fail)",
            program: cmd.clone(),
            args: vec![
                "/D".into(),
                "/C".into(),
                format!("echo x> {}", probe.pipe_name),
            ],
            expect: Expect::Holds(|p, _| {
                std::thread::sleep(Duration::from_millis(300));
                !p.pipe_reached.load(std::sync::atomic::Ordering::SeqCst)
            }),
        },
        Check {
            name: "powershell 5.1",
            program: PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "Write-Output ('probe-' + 'ok')".into(),
            ],
            expect: Expect::Runs("probe-ok"),
        },
        Check {
            name: "curl https out",
            program: PathBuf::from(r"C:\Windows\System32\curl.exe"),
            args: vec![
                "-sS".into(),
                "-o".into(),
                "NUL".into(),
                "-w".into(),
                "probe-%{http_code}".into(),
                "https://example.com/".into(),
            ],
            expect: Expect::Runs("probe-200"),
        },
        Check {
            name: "curl localhost server of the user",
            program: PathBuf::from(r"C:\Windows\System32\curl.exe"),
            args: vec!["-sS".into(), format!("http://127.0.0.1:{}/", probe.port)],
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
            expect: Expect::Runs("probe-ok"),
        });
    }
    if let Some(bash) = git_bash() {
        checks.push(Check {
            name: "git bash echo",
            program: bash.clone(),
            args: vec!["-c".into(), "echo probe-ok".into()],
            expect: Expect::Runs("probe-ok"),
        });
        checks.push(Check {
            name: "git bash fork+pipe",
            program: bash,
            args: vec![
                "-c".into(),
                "x=$(echo probe); echo \"$x-ok\" | cat; ls / >/dev/null".into(),
            ],
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
            expect: Expect::Runs("##"),
        });
    }
    if let Some(python) = on_path("python.exe") {
        checks.push(Check {
            name: "python",
            program: python.clone(),
            args: vec!["-c".into(), "print('probe-' + 'ok')".into()],
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
            expect: Expect::Runs("probe-ok"),
        });
    }
    if let Some(node) = on_path("node.exe") {
        checks.push(Check {
            name: "node",
            program: node,
            args: vec!["-e".into(), "console.log('probe-' + 'ok')".into()],
            expect: Expect::Runs("probe-ok"),
        });
    }
    if on_path("npm.cmd").is_some() {
        checks.push(Check {
            name: "npm --version (batch)",
            program: cmd.clone(),
            args: vec![
                "/D".into(),
                "/C".into(),
                "npm --version && echo probe-ok".into(),
            ],
            expect: Expect::Runs("probe-ok"),
        });
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

/// Run one check under `confinement`: what it printed, its exit code (none:
/// hung and stopped), how long it took.
fn run(check: &Check, probe: &Probe, confinement: &Confinement) -> (String, Option<i32>, Duration) {
    let started = Instant::now();
    let (pty, slave) = match open_pty(30, 200) {
        Ok(pair) => pair,
        Err(e) => return (format!("open_pty: {e}"), None, started.elapsed()),
    };
    let launch = Launch::new(&check.program)
        .args(&check.args)
        .dir(probe.project.clone())
        .env("TEMP", &probe.scratch)
        .env("TMP", &probe.scratch);
    let mut child = match spawn_on_terminal(
        &launch,
        &slave,
        Placement {
            confinement: Some(confinement),
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

#[test]
#[ignore = "a measurement for CI on a real Windows: [ci probe]"]
fn sandbox_mechanisms_measured() {
    use super::security::Token;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    let profile = super::home::home_dir().expect("a profile");
    let base = profile.join(format!("rune-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    let scratch = base.join("scratch");
    let outside = base.join("outside");
    for dir in [&project, &scratch, &outside] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(outside.join("secret.txt"), "probe-secret").unwrap();
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
        project: project.clone(),
        scratch: scratch.clone(),
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
    let elevated = Token::of_this_process(TOKEN_QUERY).is_ok_and(|t| t.elevated());
    println!("PROBE elevated={elevated} reads={reads:?}");

    let mechanisms = [
        ("restricted-token", Mechanism::RestrictedToken),
        (
            "appcontainer+profile+filtered",
            Mechanism::AppContainer {
                profile: true,
                filtered: true,
            },
        ),
        (
            "appcontainer+filtered",
            Mechanism::AppContainer {
                profile: false,
                filtered: true,
            },
        ),
        (
            "appcontainer+profile",
            Mechanism::AppContainer {
                profile: true,
                filtered: false,
            },
        ),
    ];
    let mut summary = Vec::new();
    for (label, mechanism) in mechanisms {
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
        let mut passed = 0;
        let all = checks(&probe);
        for check in &all {
            probe
                .pipe_reached
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = std::fs::remove_file(outside.join("written.txt"));
            let _ = std::fs::remove_dir_all(hello.join("target"));
            let (out, code, took) = run(check, &probe, &confinement);
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
    let _ = super::confine::forget_grants(&ledger);
    let _ = std::fs::remove_dir_all(&base);
}
