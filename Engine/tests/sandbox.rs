//! Does the kernel actually stop a command the engine allowed?
//!
//! Every other test proves the engine's own rules. This one proves the part the
//! engine cannot enforce itself: once `bash` is running, only the kernel decides
//! where it may write. If Landlock is silently not applied, these tests are the
//! ones that notice.

use std::path::PathBuf;
use std::sync::Arc;

use runeengine::engine::{Config, Engine};
use runeengine::proto::{Command, Decision, Event};
use runeengine::provider::scripted::Scripted;
use serde_json::json;

mod support;

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
    support::posix_shell_for_tests(&store.join("data"));

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

fn exited_zero(transcript: &str) -> bool {
    transcript.starts_with("command ok") && !transcript.contains("Permission denied")
}

#[test]
fn the_engine_says_out_loud_whether_it_is_enforcing() {
    let p = probe("report");
    let sandbox = p.engine.sandbox_of(p.session).unwrap();
    let r = sandbox.report();
    if r.enforced {
        use runeengine::platform::Os;
        let (kind, abi) = match runeengine::platform::OS {
            Os::Linux => ("landlock", 1),
            Os::MacOs => ("seatbelt", 0),
            Os::Windows => ("restricted-token", 0),
        };
        assert_eq!(r.kind, kind);
        assert!(r.abi >= abi);
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
    let (ok, out) = shell(&mut p, "echo inside > probe.txt");
    assert!(ok, "{out}");
    assert!(exited_zero(&out), "{out}");
    assert_eq!(
        std::fs::read_to_string(p.project.join("probe.txt")).unwrap(),
        "inside\n"
    );
}

#[test]
fn a_command_can_still_read_the_rest_of_the_machine() {
    let mut p = probe("read");
    // A compiler needs its headers; a shell needs its binaries. Reading stays
    // open — a file of the system's own on each OS.
    let system_file = match runeengine::platform::OS {
        runeengine::platform::Os::Windows => "\"$SYSTEMROOT/System32/drivers/etc/hosts\"",
        _ => "/etc/hosts",
    };
    let (ok, out) = shell(
        &mut p,
        &format!("head -1 {system_file} > /dev/null && echo readable"),
    );
    assert!(ok, "{out}");
    assert!(exited_zero(&out), "{out}");
}

#[test]
fn a_command_cannot_write_outside_the_project() {
    let mut p = probe("outside");
    let home = std::env::var("HOME").expect("HOME");
    let target = PathBuf::from(&home).join(format!(".rune-escape-probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);

    // The path is built by the shell rather than written as an argument, so the
    // reach scanner cannot see it and the *kernel* is what has to stop this.
    // That is the property under test: the scanner is convenience, Landlock is
    // the boundary, and a test that let the scanner answer would prove nothing.
    let (_, out) = shell(
        &mut p,
        &format!(
            "d=$HOME; echo escaped > \"$d/{}\"",
            target.file_name().unwrap().to_string_lossy()
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
        "t=$(mktemp) && echo work > \"$t\" && rm -f \"$t\" && echo tmp-ok",
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
    let home = std::env::var("HOME").expect("HOME");
    let target = PathBuf::from(&home).join(format!(".rune-escape-child-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);

    // A sub-shell, two levels down: the restriction must be inherited, not
    // applied only to the process we spawned. The path is assembled at runtime
    // so the reach scanner cannot pre-empt the kernel here either.
    let name = target.file_name().unwrap().to_string_lossy().to_string();
    let (_, out) = shell(
        &mut p,
        &format!("d=$HOME; bash -c \"bash -c 'echo nested > $d/{name}'\""),
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
        json!({ "action": "hang", "command": "sleep 60" }),
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
    let cmd = format!(
        "( sleep 3; echo survived > {} ) & sleep 30",
        marker.display()
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
        &format!("cat {}/note.txt", dir.display()),
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

    let ((ok, out), _) = run(
        &mut p,
        &format!("cat {}/note.txt", dir.display()),
        Decision::Deny,
    );
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
    let read = format!("cat {}/note.txt", dir.display());

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
    let hidden: String = runeengine::access::key_path(&socket)
        .display()
        .to_string()
        .bytes()
        .map(|b| format!("\\{b:03o}"))
        .collect();

    let read = format!("cat \"$(printf '{hidden}')\"");
    let ((_, out), asked) = run(&mut p, &read, Decision::AllowOnce);
    assert!(
        asked.is_empty(),
        "the gate saw the path, so this measured the gate, not the kernel: {asked:?}"
    );
    assert!(!out.contains(key.trim()), "the key leaked:\n{out}");
    assert!(
        out.contains("Permission denied"),
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
    let read = format!("cat {}/note.txt", dir.display());

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
    // The card flags it sensitive and offers no "always" — so answering
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
    let ((ok, out), asked) = run(
        &mut p,
        "/usr/bin/env true && echo fine",
        Decision::AllowOnce,
    );
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
            "command": "echo first && echo second"
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
        &state.forbidden,
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
