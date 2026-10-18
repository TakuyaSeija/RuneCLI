//! Does the kernel actually stop a command the engine allowed?
//!
//! Every other test proves the engine's own rules. This one proves the part the
//! engine cannot enforce itself: once `bash` is running, only the kernel decides
//! where it may write. If Landlock is silently not applied, these tests are the
//! ones that notice.
//!
//! On Windows they run in PowerShell, which the restricted token holds: Git
//! Bash is beyond it (`platform::unconfinable`), so a command of Git Bash's
//! runs only with full access once allowed — the last test here.

use std::path::PathBuf;
use std::sync::Arc;

use runeengine::engine::{Config, Engine};
use runeengine::proto::{Command, Decision, Event};
use runeengine::provider::scripted::Scripted;
use serde_json::json;

struct Probe {
    engine: Engine,
    events: std::sync::mpsc::Receiver<Event>,
    project: PathBuf,
    session: i64,
    enforced: bool,
    db_path: PathBuf,
}

/// The engine's own store lives outside the project's ancestor chain — see the
/// same note in `escape.rs`. With both in one directory, every "outside the
/// workspace" path in these tests would sit inside the engine's own store, which
/// no permission prompt may open, and the tests would be measuring that instead.
fn probe(name: &str) -> Probe {
    probe_in(name, false)
}

/// A probe whose commands run in the tests' POSIX shell on every OS — Git
/// Bash on Windows.
fn probe_in(name: &str, posix_on_windows: bool) -> Probe {
    let tag = format!("{}-{}", std::process::id(), name);
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-sb-{tag}"));
    let store = std::env::temp_dir().join(format!("rune-sb-engine-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&store);
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    if posix_on_windows {
        runeengine::testing::posix_shell_for_tests(&store.join("data"));
    }

    let cfg = Config::at(
        store.join("rune.db"),
        store.join("data"),
        Arc::new(Scripted::new(Vec::new())),
    )
    .unwrap();
    let (mut engine, events) = Engine::start(cfg).unwrap();
    let session = engine.open(project.to_str().unwrap()).unwrap().id();
    let enforced = engine.sandbox_of(session).unwrap().enforced();
    Probe {
        engine,
        events,
        project,
        session,
        enforced,
        db_path: store.join("rune.db"),
    }
}

/// Run one shell command through the real tool path and return (ok, transcript).
fn shell(p: &mut Probe, command: &str) -> (bool, String) {
    run(p, command, Decision::AllowOnce).0
}

/// The same, answering the permission prompt with `decision`, and reporting the
/// paths the prompt said it would open up.
fn run(p: &mut Probe, command: &str, decision: Decision) -> ((bool, String), Vec<String>) {
    p.engine.handle(Command::ToolInvoke {
        id: 1,
        session: p.session,
        name: "terminal_use".into(),
        args: json!({
            "action": "probe",
            "command": command
        }),
    });

    let mut asked: Vec<String> = Vec::new();
    loop {
        let e = p
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        match e {
            Event::PermissionRequest { req, reach, .. } => {
                asked.extend(reach);
                p.engine.handle(Command::PermissionReply {
                    id: 2,
                    req,
                    decision,
                });
            }
            Event::ToolResult {
                ok,
                summary,
                content,
                ..
            } => {
                return ((ok, format!("{summary}\n{content}")), asked);
            }
            _ => {}
        }
    }
}

/// A directory outside the project that the user could plausibly allow: not the
/// home directory itself, and one that really exists.
fn outside_dir(p: &Probe, name: &str) -> PathBuf {
    let dir = p.project.parent().unwrap().join(format!("outside-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn windows() -> bool {
    runeengine::platform::OS == runeengine::platform::Os::Windows
}

/// The command line these tests run: POSIX on Linux and macOS, PowerShell
/// on Windows.
fn script(posix: &str, powershell: &str) -> String {
    if windows() { powershell } else { posix }.to_string()
}

/// `path` quoted for PowerShell, which reads nothing inside single quotes.
fn in_powershell(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// A file read whole by the command line of this OS's test shell.
fn read_command(file: &std::path::Path) -> String {
    script(
        &format!("cat {}", in_shell(file)),
        &format!("Get-Content -Raw -Path {}", in_powershell(file)),
    )
}

/// The home folder, as the machine names it.
fn home() -> PathBuf {
    runeengine::platform::home_dir().expect("a home folder")
}

/// `path` as a POSIX shell command line spells it on every OS: Git Bash on
/// Windows reads `D:/a/b` as the folder, while the backslashes of `D:\a\b`
/// are escapes to it (and to Rune's reading of the line).
fn in_shell(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn exited_zero(transcript: &str) -> bool {
    transcript.starts_with("command ok") && !refused_by_the_kernel(transcript)
}

/// How a program reports the kernel's refusal of a path: `EACCES` (Landlock,
/// a restricted token) or `EPERM` (Seatbelt: "Operation not permitted").
fn refused_by_the_kernel(transcript: &str) -> bool {
    [
        "Permission denied",
        "Operation not permitted",
        "Access is denied",
        // PowerShell: "Access to the path '…' is denied."
        "Access to the path",
    ]
    .iter()
    .any(|said| transcript.contains(said))
}

#[test]
fn the_engine_says_out_loud_whether_it_is_enforcing() {
    let p = probe("report");
    let sandbox = p.engine.sandbox_of(p.session).unwrap();
    let r = sandbox.report();
    if r.enforced {
        use runeengine::platform::Os;
        let (kinds, abi): (&[&str], i32) = match runeengine::platform::OS {
            Os::Linux => (&["landlock"], 1),
            Os::MacOs => (&["seatbelt"], 0),
            Os::Windows => (&["restricted-token", "sandbox-account"], 0),
        };
        assert!(
            kinds.contains(&r.kind.as_str()),
            "unexpected kind {}",
            r.kind
        );
        assert!(r.abi >= abi);
        // CI sets Rune's sandbox account up before the tests: a confinement
        // that does not use it says why, and that is a failure there.
        if std::env::var_os("RUNE_EXPECT_SANDBOX_ACCOUNT").is_some() {
            assert_eq!(
                r.kind, "sandbox-account",
                "the sandbox account was set up but is not in use: {:?}",
                r.escapes
            );
        }
    } else {
        assert!(r.reason.is_some(), "a disabled sandbox must carry a reason");
        eprintln!(
            "note: no confinement enforced here — {}",
            r.reason.clone().unwrap()
        );
    }
}

#[test]
fn an_inline_awk_regex_does_not_ask_for_root_or_lift_the_sandbox() {
    if windows() {
        eprintln!("note: awk is a POSIX program; Windows's shells are read by their own rules");
        return;
    }
    let mut p = probe("awk-regex");
    std::fs::write(p.project.join("memory.txt"), "MemTotal: 1024\n").unwrap();
    p.engine.handle(Command::ToolInvoke {
        id: 71, session: p.session, name: "terminal_use".into(),
        args: json!({"action":"read the local fixture with awk", "command":"awk '/MemTotal/ {printf \"RAM: %d\\n\", $2}' memory.txt"}),
    });
    loop {
        match p
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
        {
            Event::PermissionRequest {
                req,
                reach,
                full_access,
                ..
            } => {
                assert!(
                    reach.is_empty(),
                    "awk program was mistaken for a path: {reach:?}"
                );
                assert_eq!(
                    full_access, !p.enforced,
                    "awk must not cause an unconfined call on a sandboxed host"
                );
                p.engine.handle(Command::PermissionReply {
                    id: 72,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::ToolResult { content, ok, .. } => {
                assert!(ok && content.contains("RAM: 1024"), "{content}");
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn a_command_can_write_inside_the_project() {
    let mut p = probe("inside");
    let (ok, out) = shell(
        &mut p,
        &script(
            "echo inside > probe.txt",
            "Set-Content -Path probe.txt -Value inside",
        ),
    );
    assert!(ok, "{out}");
    assert!(exited_zero(&out), "{out}");
    assert_eq!(
        std::fs::read_to_string(p.project.join("probe.txt"))
            .unwrap()
            .trim_end(),
        "inside"
    );
}

#[test]
fn a_command_can_still_read_the_rest_of_the_machine() {
    let mut p = probe("read");
    // A compiler needs its headers; a shell needs its binaries. Reading stays
    // open — a file of the system's own on each OS.
    let (ok, out) = shell(
        &mut p,
        &script(
            "head -1 /etc/hosts > /dev/null && echo readable",
            "Get-Content -TotalCount 1 \"$env:SystemRoot\\System32\\drivers\\etc\\hosts\" | Out-Null; echo readable",
        ),
    );
    assert!(ok, "{out}");
    assert!(exited_zero(&out), "{out}");
}

#[test]
fn a_command_cannot_write_outside_the_project() {
    let mut p = probe("outside");
    let target = home().join(format!(".rune-escape-probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);

    // The path is built by the shell rather than written as an argument, so the
    // reach scanner cannot see it and the *kernel* is what has to stop this.
    // That is the property under test: the scanner is convenience, Landlock is
    // the boundary, and a test that let the scanner answer would prove nothing.
    let name = target.file_name().unwrap().to_string_lossy().to_string();
    let (_, out) = shell(
        &mut p,
        &script(
            &format!("d=$HOME; echo escaped > \"$d/{name}\""),
            &format!(
                "$d=[Environment]::GetEnvironmentVariable('USER' + 'PROFILE'); Set-Content -Path \"$d\\{name}\" -Value escaped"
            ),
        ),
    );

    if p.enforced {
        assert!(!exited_zero(&out), "the write should have failed:\n{out}");
        assert!(
            !target.exists(),
            "the kernel let a write through to {}",
            target.display()
        );
    } else {
        eprintln!("note: sandbox not enforced, escape was expected to succeed");
    }
    let _ = std::fs::remove_file(&target);
}

#[test]
fn scratch_directories_stay_writable_so_ordinary_builds_work() {
    let mut p = probe("tmp");
    let (ok, out) = shell(
        &mut p,
        &script(
            "t=$(mktemp) && echo work > \"$t\" && rm -f \"$t\" && echo tmp-ok",
            "$t = New-TemporaryFile; Set-Content -Path $t -Value work; Remove-Item $t; echo tmp-ok",
        ),
    );
    assert!(ok, "{out}");
    assert!(
        exited_zero(&out),
        "a build that cannot use a temp file is a broken sandbox:\n{out}"
    );
}

#[test]
fn confinement_survives_a_child_of_the_command() {
    let mut p = probe("child");
    let target = home().join(format!(".rune-escape-child-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);

    // A sub-shell, two levels down: the restriction must be inherited, not
    // applied only to the process we spawned. The path is assembled at runtime
    // so the reach scanner cannot pre-empt the kernel here either.
    let name = target.file_name().unwrap().to_string_lossy().to_string();
    let (_, out) = shell(
        &mut p,
        &script(
            &format!("d=$HOME; bash -c \"bash -c 'echo nested > $d/{name}'\""),
            &format!(
                "$d=[Environment]::GetEnvironmentVariable('USER' + 'PROFILE'); powershell -NoProfile -Command \"cmd /c 'echo nested > $d\\{name}'\""
            ),
        ),
    );

    if p.enforced {
        assert!(!target.exists(), "a grandchild escaped the sandbox:\n{out}");
    }
    let _ = std::fs::remove_file(&target);
}

/// Run one tool call, allowing whatever it asks, and return its result.
fn invoke(p: &mut Probe, id: u64, name: &str, args: serde_json::Value) -> (bool, String) {
    p.engine.handle(Command::ToolInvoke {
        id,
        session: p.session,
        name: name.into(),
        args,
    });
    loop {
        match p
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
        {
            Event::PermissionRequest { req, .. } => {
                p.engine.handle(Command::PermissionReply {
                    id: id + 1000,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::ToolResult { content, ok, .. } => return (ok, content),
            _ => {}
        }
    }
}

fn task_id_in(content: &str) -> String {
    content
        .split_whitespace()
        .find(|word| word.starts_with("bg_"))
        .expect("a background task id")
        .to_string()
}

/// A command still running at the timeout is not killed: it moves to the
/// background, at the timeout, and the call that ran it is answered then.
#[test]
fn a_command_that_hangs_moves_to_the_background_at_the_timeout() {
    let mut p = probe("timeout");
    p.engine.handle(Command::SettingsSet {
        id: 0,
        session: p.session,
        key: "terminal.command_timeout".into(),
        value: "1s".into(),
    });
    let started = std::time::Instant::now();
    let (ok, content) = invoke(
        &mut p,
        1,
        "terminal_use",
        json!({ "action": "hang", "command": script("sleep 60", "Start-Sleep 60") }),
    );
    assert!(ok, "moving on is not a failure: {content}");
    assert!(
        content.starts_with("<tool_notice>Moved to the background as bg_"),
        "{content}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let (ok, stopped) = invoke(
        &mut p,
        2,
        "manage_process",
        json!({ "action": "stop", "process_id": task_id_in(&content), "kill": true }),
    );
    assert!(ok && stopped.contains("Process: Killed"), "{stopped}");
}

/// Stopping a command stops everything it started: a command that moved to
/// the background and is then stopped takes its own children with it.
#[test]
fn killing_a_command_kills_what_it_spawned() {
    let mut p = probe("group");
    p.engine.handle(Command::SettingsSet {
        id: 0,
        session: p.session,
        key: "terminal.command_timeout".into(),
        value: "1s".into(),
    });
    let marker = p.project.join("still-alive.txt");
    let cmd = script(
        &format!("( sleep 3; echo survived > {} ) & sleep 30", in_shell(&marker)),
        &format!(
            "Start-Process -NoNewWindow powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 3; Set-Content -Path {} -Value survived'; Start-Sleep 30",
            in_powershell(&marker).replace('\'', "''")
        ),
    );
    let (_, moved) = invoke(
        &mut p,
        1,
        "terminal_use",
        json!({ "action": "spawn a background child", "command": cmd }),
    );
    assert!(
        moved.starts_with("<tool_notice>Moved to the background as bg_"),
        "the command must have run: {moved}"
    );
    let (ok, stopped) = invoke(
        &mut p,
        2,
        "manage_process",
        json!({ "action": "stop", "process_id": task_id_in(&moved), "kill": true }),
    );
    assert!(ok && stopped.contains("Process: Killed"), "{stopped}");

    // The child had 3 seconds to write; its group was killed at about 1.
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert!(
        !marker.exists(),
        "a background child outlived the command that started it"
    );
}

#[test]
fn a_catastrophic_command_is_asked_and_stopped_by_the_kernel() {
    // The gate no longer refuses anything: `rm -rf /` is Severe, so it is put
    // to the user, and — answered "allow once" — the shell runs it under the
    // sandbox. The kernel, or rm's own root guard, is the wall. That is the
    // deny-list's own definition of itself: a tripwire, never a boundary.
    if windows() {
        // A whole drive's removal is not tried on a real Windows drive: the
        // token's refusal of writes outside is
        // `a_command_cannot_write_outside_the_project`.
        eprintln!("note: not tried on Windows");
        return;
    }
    let mut p = probe("severe");
    let ((ok, out), asked) = run(&mut p, "rm -rf /", Decision::AllowOnce);
    assert!(
        !asked.is_empty(),
        "rm -rf / was never put to the user: {out}"
    );
    assert!(
        ok,
        "a stopped command is not an engine refusal — the call completed: {out}"
    );
    assert!(!exited_zero(&out), "rm -rf / must not succeed: {out}");
}

// ─── Reaching outside the workspace, by permission ───────────────────────────
//
// The bug these exist for: an "allow" moved the permission gate and left the
// kernel ruleset exactly where it was, so a user watched a command they had
// just authorised fail with `Permission denied`.

#[test]
fn an_allowed_path_outside_the_workspace_really_becomes_reachable() {
    let mut p = probe("reach-allow");
    let dir = outside_dir(&p, "allow");
    std::fs::write(dir.join("note.txt"), "hello from outside\n").unwrap();

    let ((ok, out), asked) = run(
        &mut p,
        &read_command(&dir.join("note.txt")),
        Decision::AllowOnce,
    );

    assert!(
        asked.iter().any(|a| a.contains("outside-allow")),
        "the prompt must name the path it is about to open up, got {asked:?}"
    );
    assert!(
        ok,
        "an allowed read outside the workspace still failed:\n{out}"
    );
    assert!(out.contains("hello from outside"), "{out}");
    assert!(exited_zero(&out), "{out}");
}

#[test]
fn a_refused_path_outside_the_workspace_stays_unreachable() {
    let mut p = probe("reach-deny");
    let dir = outside_dir(&p, "deny");
    std::fs::write(dir.join("note.txt"), "secret\n").unwrap();

    let ((ok, out), _) = run(&mut p, &read_command(&dir.join("note.txt")), Decision::Deny);
    assert!(!ok, "a refused call must not run:\n{out}");
    assert!(
        !out.contains("secret"),
        "the contents leaked despite the refusal:\n{out}"
    );
}

#[test]
fn a_one_call_grant_does_not_outlive_the_call() {
    let mut p = probe("reach-once");
    let dir = outside_dir(&p, "once");
    std::fs::write(dir.join("note.txt"), "one-shot\n").unwrap();
    let read = read_command(&dir.join("note.txt"));

    let ((ok, out), _) = run(&mut p, &read, Decision::AllowOnce);
    assert!(ok, "{out}");

    // The second call must be asked about again. Refusing it proves the first
    // grant was handed back rather than quietly kept for the session.
    let ((ok, out), asked) = run(&mut p, &read, Decision::Deny);
    assert!(
        !asked.is_empty(),
        "the second call was never put to the user again"
    );
    assert!(!ok, "a grant for one call survived it:\n{out}");
}

/// The key a session host's windows present (`runeengine::access`) is out of
/// reach of every command the agent runs: a command that knows where it lies
/// and hides the path from the gate — as text steered by an injection would —
/// raises no question, runs confined, and the kernel refuses the read.
/// Holding that key is control of the session (its mode, its permission
/// cards), so this is the boundary the host's socket rests on.
#[test]
fn a_confined_command_cannot_read_the_session_hosts_key() {
    let mut p = probe("host-key");
    if !p.enforced {
        eprintln!("skipped: no kernel sandbox on this machine");
        return;
    }
    let store = p.db_path.parent().unwrap().to_path_buf();
    let socket = runeengine::store::jsonl::JsonlStore::new(&store).session_socket_path(p.session);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _key = runeengine::access::HostKey::issue(&socket).unwrap();
    let key = std::fs::read_to_string(runeengine::access::key_path(&socket)).unwrap();
    let path = runeengine::access::key_path(&socket).display().to_string();
    let read = if windows() {
        let codes: Vec<String> = path.encode_utf16().map(|c| c.to_string()).collect();
        format!(
            "Get-Content -Raw -Path ([string]::new([char[]]@({})))",
            codes.join(",")
        )
    } else {
        let hidden: String = path.bytes().map(|b| format!("\\{b:03o}")).collect();
        format!("cat \"$(printf '{hidden}')\"")
    };
    let ((_, out), asked) = run(&mut p, &read, Decision::AllowOnce);
    assert!(
        asked.is_empty(),
        "the gate saw the path, so this measured the gate, not the kernel: {asked:?}"
    );
    assert!(!out.contains(key.trim()), "the key leaked:\n{out}");
    assert!(
        refused_by_the_kernel(&out),
        "the read was not refused by the kernel:\n{out}"
    );

    // The control: the same command, unconfined, does read it — so the
    // refusal above was the sandbox's, not a path the command got wrong.
    p.engine.handle(Command::SessionMode {
        id: 9,
        session: p.session,
        mode: "full".into(),
    });
    let ((_, out), _) = run(&mut p, &read, Decision::AllowOnce);
    assert!(
        out.contains(key.trim()),
        "the control did not read the key:\n{out}"
    );
}

#[test]
fn allow_always_survives_into_the_next_call_without_asking_again() {
    let mut p = probe("reach-always");
    let dir = outside_dir(&p, "always");
    std::fs::write(dir.join("note.txt"), "remembered\n").unwrap();
    let read = read_command(&dir.join("note.txt"));

    let ((ok, out), _) = run(&mut p, &read, Decision::AllowAlways);
    assert!(ok, "{out}");

    // The second call is still put to the user — this is `Ask` mode, and every
    // command is read by a person there. What must not happen again is the
    // question about the *path*: `asked` collects only the paths a prompt said
    // it would open, so an empty list is the proof the grant was remembered.
    let ((ok, out), asked) = run(&mut p, &read, Decision::AllowOnce);
    assert!(
        asked.is_empty(),
        "a remembered path was put to the user again: {asked:?}"
    );
    assert!(ok, "{out}");
    assert!(out.contains("remembered"), "{out}");
}

#[test]
fn the_home_directory_is_asked_with_no_always_and_cannot_be_remembered() {
    // The owner's rule, 2026-09-28: `ls ~` is a question, never a refusal.
    // The whole home is full access under another name: the card offers no
    // "always" — so answering
    // AllowAlways anyway stores nothing, and the same question comes back.
    let mut p = probe("reach-home");
    let ((ok, out), asked) = run(&mut p, "ls ~", Decision::AllowAlways);
    assert!(
        !asked.is_empty(),
        "the whole home directory was never asked: {out}"
    );
    assert!(ok, "{out}");

    let ((ok2, out2), asked2) = run(&mut p, "ls ~", Decision::AllowAlways);
    assert!(
        !asked2.is_empty(),
        "AllowAlways was remembered for the home directory"
    );
    assert!(ok2, "{out2}");
}

#[test]
fn a_system_path_every_command_already_uses_is_not_put_to_the_user() {
    let mut p = probe("reach-system");
    // /usr/bin is readable under the base policy. Prompting for it would put a
    // dialog in front of half the commands an agent runs.
    let system_program = if windows() {
        format!(
            "& {} | Out-Null; echo fine",
            in_powershell(
                &PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
                    .join(r"System32\hostname.exe")
            )
        )
    } else {
        "/usr/bin/env true && echo fine".to_string()
    };
    let ((ok, out), asked) = run(&mut p, &system_program, Decision::AllowOnce);
    assert!(
        asked.is_empty(),
        "an already-reachable path was put to the user: {asked:?}"
    );
    assert!(ok, "{out}");
    assert!(exited_zero(&out), "{out}");
}

#[test]
fn chained_commands_run_successfully() {
    let mut p = probe("chained");
    p.engine.handle(Command::ToolInvoke {
        id: 1,
        session: p.session,
        name: "terminal_use".into(),
        args: json!({
            "action": "run chained commands",
            "command": script("echo first && echo second", "echo first; echo second")
        }),
    });

    loop {
        let e = p
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        match e {
            Event::PermissionRequest { req, .. } => {
                p.engine.handle(Command::PermissionReply {
                    id: 2,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::ToolResult { content, ok, .. } => {
                assert!(ok);
                assert!(content.contains("first"));
                assert!(content.contains("second"));
                return;
            }
            _ => {}
        }
    }
}

#[test]
fn a_path_grant_persisted_by_another_process_reaches_this_session() {
    // The owner's rule, 2026-09-30: a grant is a decision about the *project*,
    // not about the window that happened to ask. "Always allow" answered in one
    // Rune process must make the path reachable in another without a restart —
    // the same answer to the same question, everywhere, at once.
    let p = probe("reseed-grant");
    let dir = outside_dir(&p, "reseed");
    std::fs::write(dir.join("note.txt"), "granted elsewhere\n").unwrap();

    // Before any grant, the path is outside everything this session may reach.
    let state = p.engine.session(p.session).unwrap().state.clone();
    assert!(
        !state.reach().contains(&dir),
        "the path was reachable before anyone granted it"
    );

    // A second Db handle on the same store stands in for the other process: it is
    // where "always allow" was answered and the row landed.
    let db = runeengine::store::Db::open(&p.db_path, std::time::Duration::from_secs(10)).unwrap();
    db.grant(
        p.project.to_str().unwrap(),
        runeengine::perm::reach::GRANT_TOOL,
        dir.to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(
        db.grants(p.project.to_str().unwrap()).unwrap().len(),
        1,
        "the grant must be on disk for the other process to see"
    );

    // What the engine reads back on every round of a running turn.
    let standing = runeengine::engine::standing_reach(
        &std::sync::Arc::new(std::sync::Mutex::new(db)),
        p.project.to_str().unwrap(),
    );
    assert!(
        standing.contains(&dir),
        "the grant persisted by the other process must resolve to a reachable path: {standing:?}"
    );

    state.reseed_reach(&standing);
    assert!(
        state.reach().contains(&dir),
        "a grant made in another process did not reach this session"
    );
    assert!(
        state.jail().contains(&dir),
        "the rebuilt jail does not cover the granted path"
    );
}

#[test]
fn terminal_use_respects_path_parameter() {
    let mut p = probe("term_path");
    let sub = p.project.join("subdir");
    std::fs::create_dir_all(&sub).unwrap();

    p.engine.handle(Command::ToolInvoke {
        id: 10,
        session: p.session,
        name: "terminal_use".into(),
        args: json!({
            "action": "check pwd with path",
            "command": "pwd",
            "path": "subdir"
        }),
    });
    loop {
        let e = p
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        match e {
            Event::PermissionRequest { req, .. } => {
                p.engine.handle(Command::PermissionReply {
                    id: 11,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::ToolResult { content, ok, .. } => {
                assert!(ok);
                assert!(
                    content.contains("subdir"),
                    "pwd should be inside subdir: {content}"
                );
                break;
            }
            _ => {}
        }
    }
}

// ─── Local services ─────────────────────────────────────────────────────────
//
// A program already running on this machine, reached through a Unix socket,
// acts outside the workspace with the user's rights. A confined command
// reaches its own sockets freely; another program's only once the person
// allowed it — asked at the moment it connects (owner, 2026-10-17).

/// A stand-in for such a program, outside the project and outside the
/// command's tree: it notes in a file beside its socket every connection it
/// is given — the act a command must not get done on its behalf unasked.
fn service(p: &Probe) -> (PathBuf, PathBuf) {
    // Short: a socket's path has at most 104 bytes (macOS), and the test
    // folder sits deep.
    let dir = p.project.parent().unwrap().join("o");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s");
    let marker = dir.join("served.txt");
    let _ = std::fs::remove_file(&socket);
    let listener = runeengine::platform::LocalListener::bind(&socket).unwrap();
    let noted = marker.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                let mut seen = std::fs::read_to_string(&noted).unwrap_or_default();
                seen.push_str("served\n");
                std::fs::write(&noted, seen).unwrap();
            }
        }
    });
    (socket, marker)
}

fn served(marker: &std::path::Path) -> usize {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .count()
}

/// A command that reaches the socket by a path it does not name on its
/// command line (an environment variable): only the connection itself can
/// tell.
fn connect(socket: &std::path::Path) -> String {
    let python = runeengine::testing::python();
    let code = "import os,socket,sys\ntry:\n s=socket.socket(socket.AF_UNIX)\n s.connect(os.environ[\"SOCK\"])\nexcept OSError as e:\n print(\"refused\", e.errno); sys.exit(7)\nprint(\"connected\")";
    format!(
        "SOCK={} {} -c '{code}'",
        in_shell(socket),
        in_shell(&python)
    )
}

/// Run `command` (its own card allowed: the probe is in Ask), answering
/// every question about a local service with `decision`; the result and
/// those questions (their actions).
fn run_services(p: &mut Probe, command: &str, decision: Decision) -> (String, Vec<String>) {
    p.engine.handle(Command::ToolInvoke {
        id: 1,
        session: p.session,
        name: "terminal_use".into(),
        args: json!({"action": "probe", "command": command}),
    });
    let mut asked = Vec::new();
    loop {
        match p
            .events
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap()
        {
            Event::PermissionRequest {
                req, action, tool, ..
            } => {
                let service = tool == runeengine::session::SERVICE_TOOL;
                if service {
                    asked.push(action);
                }
                p.engine.handle(Command::PermissionReply {
                    id: 2,
                    req,
                    decision: if service {
                        decision
                    } else {
                        Decision::AllowOnce
                    },
                });
            }
            Event::ToolResult {
                summary, content, ..
            } => return (format!("{summary}\n{content}"), asked),
            _ => {}
        }
    }
}

/// Where a connection is put to the person as it happens: Linux, where Rune
/// supervises it. Elsewhere the confinement refuses another program's
/// socket outright (macOS) or never sees one (Windows).
fn supervised(p: &Probe) -> bool {
    if !p.enforced || runeengine::platform::OS != runeengine::platform::Os::Linux {
        eprintln!("note: no supervised Unix-socket confinement here");
        return false;
    }
    true
}

#[test]
fn a_service_outside_the_workspace_is_asked_as_the_command_connects_and_a_refusal_holds() {
    let mut p = probe("sv1");
    if !supervised(&p) {
        return;
    }
    let (socket, marker) = service(&p);
    let (out, asked) = run_services(&mut p, &connect(&socket), Decision::Deny);
    assert_eq!(asked.len(), 1, "{out}");
    assert!(
        asked[0].contains(&socket.display().to_string()),
        "{asked:?}"
    );
    assert!(
        out.contains("refused 13"),
        "EACCES, as the kernel says it: {out}"
    );
    assert_eq!(
        served(&marker),
        0,
        "the service acted for a confined command"
    );
}

#[test]
fn a_service_the_person_allows_once_is_reached_by_that_connection_alone() {
    let mut p = probe("sv2");
    if !supervised(&p) {
        return;
    }
    let (socket, marker) = service(&p);
    let (out, asked) = run_services(&mut p, &connect(&socket), Decision::AllowOnce);
    assert_eq!(asked.len(), 1, "{out}");
    assert!(out.contains("connected"), "{out}");
    assert_eq!(served(&marker), 1);

    let (out, asked) = run_services(&mut p, &connect(&socket), Decision::Deny);
    assert_eq!(asked.len(), 1, "asked again: {out}");
    assert!(out.contains("refused"), "{out}");
    assert_eq!(served(&marker), 1);
}

#[test]
fn a_service_allowed_always_is_reached_by_the_projects_later_commands_unasked() {
    let mut p = probe("sv3");
    if !supervised(&p) {
        return;
    }
    let (socket, marker) = service(&p);
    let (out, _) = run_services(&mut p, &connect(&socket), Decision::AllowAlways);
    assert!(out.contains("connected"), "{out}");
    let (out, asked) = run_services(&mut p, &connect(&socket), Decision::Deny);
    assert!(asked.is_empty(), "a kept grant asked again: {asked:?}");
    assert!(
        out.contains("connected"),
        "a kept grant did not hold: {out}"
    );
    assert_eq!(served(&marker), 2);
}

/// What builds do: a server of the command's own — on a socket file in the
/// project, on an abstract name (Python's process pools) — and a pair of
/// connected sockets: all reached with no question. (The scratch folder is
/// one of the confinement's own too, but its path here is longer than a
/// socket address holds.)
#[test]
fn a_commands_own_sockets_are_reached_with_no_question() {
    let mut p = probe("sv4");
    if !supervised(&p) {
        return;
    }
    let python = runeengine::testing::python();
    let script = format!(
        r#"import os,socket,sys,threading
def serve(address):
    s=socket.socket(socket.AF_UNIX); s.bind(address); s.listen()
    threading.Thread(target=lambda: s.accept(), daemon=True).start()
    c=socket.socket(socket.AF_UNIX); c.connect(address); c.close()
for address in ["inside.sock", "\0rune-test-{pid}"]:
    try:
        serve(address)
    except OSError as e:
        print("refused", address, e.errno); sys.exit(7)
a,b=socket.socketpair(); a.send(b"x"); assert b.recv(1)==b"x"
print("all own")"#,
        pid = std::process::id(),
    );
    std::fs::write(p.project.join("own.py"), script).unwrap();
    let (out, asked) = run_services(
        &mut p,
        &format!("{} own.py", in_shell(&python)),
        Decision::Deny,
    );
    assert!(
        asked.is_empty(),
        "the command's own socket was asked: {asked:?}"
    );
    assert!(out.contains("all own"), "{out}");
}

/// Off Linux nobody asks at the moment of connecting: the confinement itself
/// keeps a program of the person's out of reach — Seatbelt opens no Unix
/// socket beyond the command's own folders (macOS), and a pipe of the user's
/// alone admits neither the sandbox account nor a restricted token's SIDs
/// (Windows; Rune's own session pipes are made so). The address is hidden from
/// the gate, as text steered by an injection would hide it, so the refusal
/// measured is the kernel's; the same command run in Full is the control.
#[test]
fn a_service_of_the_persons_is_out_of_a_confined_commands_reach_off_linux() {
    let mut p = probe("sv8");
    if !p.enforced || runeengine::platform::OS == runeengine::platform::Os::Linux {
        eprintln!("note: the supervised tests above hold this on Linux");
        return;
    }
    let (socket, marker) = service(&p);
    let reach = if windows() {
        let text = std::fs::read_to_string(&socket).unwrap();
        let name = text
            .lines()
            .nth(1)
            .and_then(|pipe| pipe.strip_prefix(r"\\.\pipe\"))
            .expect("a meeting file names its pipe")
            .to_string();
        let codes: Vec<String> = name.encode_utf16().map(|c| c.to_string()).collect();
        format!(
            "$c = [System.IO.Pipes.NamedPipeClientStream]::new('.', [string]::new([char[]]@({})), [System.IO.Pipes.PipeDirection]::InOut); try {{ $c.Connect(5000); 'connected' }} catch {{ 'refused: ' + $_.Exception.Message; exit 7 }}",
            codes.join(",")
        )
    } else {
        let hidden: String = socket
            .display()
            .to_string()
            .bytes()
            .map(|b| format!("\\{b:03o}"))
            .collect();
        let code = "import os,socket,sys\ntry:\n s=socket.socket(socket.AF_UNIX)\n s.connect(os.environ[\"SOCK\"])\nexcept OSError as e:\n print(\"refused:\", os.strerror(e.errno)); sys.exit(7)\nprint(\"connected\")";
        format!(
            "SOCK=\"$(printf '{hidden}')\" {} -c '{code}'",
            in_shell(&runeengine::testing::python())
        )
    };

    let ((_, out), asked) = run(&mut p, &reach, Decision::AllowOnce);
    assert!(
        asked.is_empty(),
        "the gate saw the address, so this measured the gate, not the kernel: {asked:?}"
    );
    assert!(
        out.contains("refused:") && refused_by_the_kernel(&out),
        "the connection was not refused by the kernel:\n{out}"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(served(&marker), 0, "a confined command reached the service");

    p.engine.handle(Command::SessionMode {
        id: 9,
        session: p.session,
        mode: "full".into(),
    });
    let ((_, out), _) = run(&mut p, &reach, Decision::AllowOnce);
    assert!(
        out.contains("connected"),
        "the control did not connect:\n{out}"
    );
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while served(&marker) == 0 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        served(&marker),
        1,
        "the control's connection was not served"
    );
}

/// A pipe another program of the person's makes with Windows's default
/// security (no descriptor of its own: most programs) — measured, not read
/// from documentation: what a confined command gets when it opens one to
/// read, and to read and write. Full is the control.
#[test]
fn a_default_secured_pipe_of_the_persons_as_a_confined_command_meets_it() {
    let mut p = probe("pipe-gap");
    if !p.enforced || !windows() {
        eprintln!("note: Windows only");
        return;
    }
    let dir = outside_dir(&p, "pipe");
    let marker = dir.join("served.txt");
    let server = dir.join("server.py");
    std::fs::write(
        &server,
        r#"import _winapi, sys, threading
name, marker = sys.argv[1], sys.argv[2]
def note(text):
    with open(marker, "a") as f:
        f.write(text + "\n")
def serve(h):
    try:
        _winapi.WriteFile(h, b"from the service\n")
        data, _ = _winapi.ReadFile(h, 64)
        note("told " + data.decode(errors="replace").strip())
    except OSError:
        pass
first = True
while True:
    mode = _winapi.PIPE_ACCESS_DUPLEX | (_winapi.FILE_FLAG_FIRST_PIPE_INSTANCE if first else 0)
    h = _winapi.CreateNamedPipe(name, mode, _winapi.PIPE_TYPE_BYTE | _winapi.PIPE_READMODE_BYTE | _winapi.PIPE_WAIT, 255, 4096, 4096, 0, _winapi.NULL)
    if first:
        note("ready")
        first = False
    try:
        _winapi.ConnectNamedPipe(h, False)
    except OSError as e:
        if e.winerror != 535:
            continue
    note("served")
    threading.Thread(target=serve, args=(h,), daemon=True).start()
"#,
    )
    .unwrap();
    let name = format!("rune-gap-{}", std::process::id());
    let mut service = std::process::Command::new(runeengine::testing::python())
        .arg(&server)
        .arg(format!(r"\\.\pipe\{name}"))
        .arg(&marker)
        .spawn()
        .unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !std::fs::read_to_string(&marker)
        .unwrap_or_default()
        .contains("ready")
        && std::time::Instant::now() < until
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let codes: Vec<String> = name.encode_utf16().map(|c| c.to_string()).collect();
    let open = format!(
        "$n = [string]::new([char[]]@({})); foreach ($d in 'In','InOut') {{ $c = [System.IO.Pipes.NamedPipeClientStream]::new('.', $n, [System.IO.Pipes.PipeDirection]$d); try {{ $c.Connect(5000); $r = [System.IO.StreamReader]::new($c); \"$d opened: \" + $r.ReadLine(); if ($d -eq 'InOut') {{ $w = [System.IO.StreamWriter]::new($c); $w.WriteLine('hi'); $w.Flush(); 'InOut wrote' }} }} catch {{ \"$d refused: \" + $_.Exception.Message }} finally {{ $c.Dispose() }} }}",
        codes.join(",")
    );
    let served = || {
        std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "served")
            .count()
    };

    let ((_, confined), asked) = run(&mut p, &open, Decision::AllowOnce);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let served_confined = served();
    let kind = p.engine.sandbox_of(p.session).unwrap().report().kind.clone();
    eprintln!("MEASURED ({kind}) confined:\n{confined}\nserved {served_confined}");

    p.engine.handle(Command::SessionMode {
        id: 9,
        session: p.session,
        mode: "full".into(),
    });
    let ((_, full), _) = run(&mut p, &open, Decision::AllowOnce);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = service.kill();
    let _ = service.wait();
    eprintln!(
        "MEASURED full:\n{full}\nmarker:\n{}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );

    assert!(asked.is_empty(), "the gate saw the pipe: {asked:?}");
    assert!(
        full.contains("In opened: from the service") && full.contains("InOut wrote"),
        "the control did not reach the pipe:\n{full}"
    );
    assert!(
        confined.contains("InOut refused:"),
        "a confined command opened another program's pipe to write:\n{confined}"
    );
}

/// A datagram stand-in, run as a program of its own outside the sandbox: a
/// message addressed to it, with no connection, is the act a command must
/// not get done unasked. Stopped when dropped.
struct DatagramService {
    child: std::process::Child,
    socket: PathBuf,
    marker: PathBuf,
}

impl Drop for DatagramService {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn datagram_service(p: &Probe) -> DatagramService {
    let dir = p.project.parent().unwrap().join("d");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s");
    let marker = dir.join("served.txt");
    let ready = dir.join("ready");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&ready);
    let code = "import socket,sys\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ns.bind(sys.argv[1])\nopen(sys.argv[3],\"w\").close()\nwhile True:\n s.recv(65536)\n open(sys.argv[2],\"a\").write(\"served\\n\")";
    let child = std::process::Command::new(runeengine::testing::python())
        .args(["-c", code])
        .arg(&socket)
        .arg(&marker)
        .arg(&ready)
        .spawn()
        .unwrap();
    let service = DatagramService {
        child,
        socket,
        marker,
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !ready.exists() {
        assert!(std::time::Instant::now() < until, "the service never bound");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    service
}

/// A command that sends to `SOCK` with no connection, `how` being `sendmsg`
/// or `sendmmsg` (two messages in one call; Python has no binding, so libc).
const SEND: &str = r#"import ctypes,os,socket,struct,sys
how=sys.argv[1]; to=os.environ["SOCK"]
s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)
try:
    if how=="sendmsg":
        s.sendmsg([b"x"],[],0,to)
    else:
        libc=ctypes.CDLL(None,use_errno=True)
        class iovec(ctypes.Structure): _fields_=[("base",ctypes.c_void_p),("len",ctypes.c_size_t)]
        class msghdr(ctypes.Structure): _fields_=[("name",ctypes.c_void_p),("namelen",ctypes.c_uint32),("iov",ctypes.POINTER(iovec)),("iovlen",ctypes.c_size_t),("control",ctypes.c_void_p),("controllen",ctypes.c_size_t),("flags",ctypes.c_int)]
        class mmsghdr(ctypes.Structure): _fields_=[("hdr",msghdr),("len",ctypes.c_uint)]
        path=to.encode()
        address=ctypes.create_string_buffer(struct.pack("H",socket.AF_UNIX)+path)
        data=ctypes.create_string_buffer(b"xy",2)
        parts=(iovec*1)(iovec(ctypes.cast(data,ctypes.c_void_p),2))
        messages=(mmsghdr*2)()
        for m in messages:
            m.hdr.name=ctypes.cast(address,ctypes.c_void_p); m.hdr.namelen=2+len(path)
            m.hdr.iov=parts; m.hdr.iovlen=1
        sent=libc.sendmmsg(s.fileno(),messages,2,0)
        if sent<0:
            raise OSError(ctypes.get_errno(),"sendmmsg")
        if sent!=2 or messages[0].len!=2 or messages[1].len!=2:
            print("short",sent,messages[0].len,messages[1].len); sys.exit(8)
except OSError as e:
    print("refused",e.errno); sys.exit(7)
print("sent")"#;

fn send(p: &Probe, service: &DatagramService, how: &str) -> String {
    std::fs::write(p.project.join("send.py"), SEND).unwrap();
    format!(
        "SOCK={} {} send.py {how}",
        in_shell(&service.socket),
        in_shell(&runeengine::testing::python())
    )
}

/// Every way a message names an address is the connection's question:
/// `sendmsg` and `sendmmsg` carry it inside the message, and a refusal holds
/// for both — nothing reaches the service.
#[test]
fn a_message_addressed_to_a_service_is_asked_and_a_refusal_holds() {
    let mut p = probe("sv5");
    if !supervised(&p) {
        return;
    }
    let service = datagram_service(&p);
    for how in ["sendmsg", "sendmmsg"] {
        let (out, asked) = {
            let command = send(&p, &service, how);
            run_services(&mut p, &command, Decision::Deny)
        };
        assert_eq!(asked.len(), 1, "{how}: {out}");
        assert!(
            asked[0].contains(&service.socket.display().to_string()),
            "{how}: {asked:?}"
        );
        assert!(out.contains("refused 13"), "{how}: EACCES: {out}");
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        served(&service.marker),
        0,
        "the service was reached by a confined command's message"
    );
}

/// Allowed, the messages arrive whole: one question for a batch to one
/// address, each message's length reported back as the kernel reports it.
#[test]
fn messages_the_person_allows_arrive_and_a_batch_asks_once() {
    let mut p = probe("sv6");
    if !supervised(&p) {
        return;
    }
    let service = datagram_service(&p);
    let (out, asked) = {
        let command = send(&p, &service, "sendmsg");
        run_services(&mut p, &command, Decision::AllowOnce)
    };
    assert_eq!(asked.len(), 1, "{out}");
    assert!(out.contains("sent"), "{out}");
    let (out, asked) = {
        let command = send(&p, &service, "sendmmsg");
        run_services(&mut p, &command, Decision::AllowOnce)
    };
    assert_eq!(asked.len(), 1, "a batch to one address asks once: {out}");
    assert!(out.contains("sent"), "{out}");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while served(&service.marker) < 3 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(served(&service.marker), 3);
}

/// What programs send among their own sockets goes on unasked and unchanged
/// now that Rune sends it: a descriptor passed over a pair of sockets is the
/// same open file on the other side, and a message on a connected socket
/// arrives as written.
#[test]
fn a_commands_own_messages_and_passed_descriptors_arrive_unasked() {
    let mut p = probe("sv7");
    if !supervised(&p) {
        return;
    }
    let script = r#"import os,socket,sys
a,b=socket.socketpair()
r,w=os.pipe()
socket.send_fds(a,[b"fd"],[w])
data,fds,_,_=socket.recv_fds(b,16,1)
os.write(fds[0],b"through")
assert os.read(r,7)==b"through", "not the same pipe"
assert data==b"fd"
c,d=socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM)
c.sendmsg([b"one",b"two"])
assert d.recv(16)==b"onetwo"
print("all own")"#;
    std::fs::write(p.project.join("own.py"), script).unwrap();
    let (out, asked) = run_services(
        &mut p,
        &format!("{} own.py", in_shell(&runeengine::testing::python())),
        Decision::Deny,
    );
    assert!(
        asked.is_empty(),
        "the command's own messages were asked: {asked:?}"
    );
    assert!(out.contains("all own"), "{out}");
}

/// A shell the sandbox cannot hold (Git Bash on Windows: MSYS2's shared
/// memory is refused to a restricted token) is put to the person in Auto
/// with that reason, and once allowed runs with full access for that call
/// alone; refused, it does not run. Where every program can be confined
/// (Linux, macOS) the same command runs unasked, confined.
#[test]
fn a_shell_the_sandbox_cannot_hold_is_asked_in_auto_and_runs_with_full_access_once_allowed() {
    let mut p = probe_in("beyond", true);
    if !p.enforced {
        eprintln!("note: no kernel sandbox on this machine");
        return;
    }
    if windows() && runeengine::platform::fixture::posix_shell().is_none() {
        eprintln!("note: no Git Bash on this machine");
        return;
    }
    p.engine.handle(Command::SessionMode {
        id: 3,
        session: p.session,
        mode: "auto".into(),
    });
    let invoke = |p: &mut Probe, decision: Decision| {
        p.engine.handle(Command::ToolInvoke {
            id: 4,
            session: p.session,
            name: "terminal_use".into(),
            // `cat` is a program of its own: on Windows an MSYS2 child, which
            // starts only when the whole tree runs unconfined.
            args: json!({"action": "probe", "command": "echo lifted | cat > lifted.txt"}),
        });
        let mut asked = Vec::new();
        loop {
            match p
                .events
                .recv_timeout(std::time::Duration::from_secs(60))
                .unwrap()
            {
                Event::PermissionRequest {
                    req,
                    full_access,
                    remember_as,
                    reason,
                    ..
                } => {
                    asked.push((full_access, remember_as, reason));
                    p.engine.handle(Command::PermissionReply {
                        id: 5,
                        req,
                        decision,
                    });
                }
                Event::ToolResult { ok, content, .. } => return (ok, content, asked),
                _ => {}
            }
        }
    };
    let kind = p
        .engine
        .sandbox_of(p.session)
        .unwrap()
        .report()
        .kind
        .clone();
    let written = p.project.join("lifted.txt");

    let (ok, out, asked) = invoke(&mut p, Decision::AllowOnce);
    assert!(ok, "{out}");
    assert!(written.is_file(), "the command did not run: {out}");
    if !windows() || kind == "sandbox-account" {
        assert!(
            asked.is_empty(),
            "a confinable shell was asked in Auto: {asked:?}"
        );
        return;
    }
    let [(full_access, remember_as, reason)] = asked.as_slice() else {
        panic!("one question, got {asked:?}");
    };
    assert!(full_access, "the answer must say it gives full access");
    assert_eq!(remember_as, &None, "nothing about it may be remembered");
    let reason = reason.as_deref().unwrap_or_default();
    assert!(reason.contains("msys-2.0.dll"), "{reason}");

    std::fs::remove_file(&written).unwrap();
    let (ok, out, asked) = invoke(&mut p, Decision::Deny);
    assert!(!ok, "a refused command ran: {out}");
    assert_eq!(asked.len(), 1);
    assert!(!written.exists(), "a refused command ran: {out}");
}
