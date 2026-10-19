//! The host, exercised as a real process.
//!
//! Everything else tests the engine in-process. This spawns the actual `rune`
//! binary and speaks the protocol down a pipe, because that is what a frontend
//! will do — and it is the only way to catch a stdout stream that has been
//! corrupted by a stray print, or a host that stops reading stdin during a turn.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Host {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    project: PathBuf,
}

fn start(name: &str, fixture: Option<Value>) -> Host {
    let base = std::env::temp_dir().join("rune-test").join(format!(
        "rune-host-{}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();

    let script = base.join("fixture.json");
    let fixture = fixture.unwrap_or_else(|| serde_json::json!({ "responses": [] }));
    if let Some(providers) = fixture.get("test_providers") {
        // Where a person configures them: the engine home's rune.toml.
        runeengine::settings::configuration::Configuration::edit(
            &base,
            Some("providers"),
            Some(providers.clone()),
            false,
            None,
        )
        .unwrap();
    }

    std::fs::write(&script, serde_json::to_string_pretty(&fixture).unwrap()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env("RUNE_HOME", &base)
        .arg("-host")
        .arg("-db")
        .arg(base.join("rune.db"))
        .arg("-script")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("rune binary should start");

    let stdin = child.stdin.take().unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    Host {
        child,
        stdin,
        out,
        project,
    }
}

impl Host {
    fn send(&mut self, cmd: Value) {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Next event, parsed. Every line the host writes must be valid JSON — that
    /// is the assertion that matters most here.
    fn next(&mut self) -> Value {
        let mut line = String::new();
        let n = self.out.read_line(&mut line).expect("host stdout");
        assert!(n > 0, "the host closed its output early");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("host wrote a non-JSON line: {line:?} ({e})"))
    }

    /// Read until an event of this type arrives, answering permission requests.
    fn until(&mut self, kind: &str) -> Value {
        loop {
            let e = self.next();
            if e["t"] == "permission_request" {
                let req = e["req"].as_str().unwrap().to_string();
                self.send(json!({ "cmd": "permission.reply", "id": 90, "req": req,
                                  "decision": "allow_once" }));
            }
            if e["t"] == kind {
                return e;
            }
        }
    }

    fn finish(mut self) {
        self.send(json!({ "cmd": "shutdown", "id": 999 }));
        let status = self.child.wait().expect("host should exit");
        assert!(status.success(), "host exited with {status}");
    }
}

/// What a test that copies the binary leaves behind goes when the test ends,
/// passed or not: the host it started (its background tasks end with it, their
/// terminals closed) and the folders — each holding a copy of a debug build,
/// hundreds of megabytes of a RAM-backed `/tmp`.
struct Cleanup {
    pid: Option<u32>,
    dirs: Vec<std::path::PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .status();
        }
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Only copied fixture executables need this barrier. Parallel tests may fork
/// while another thread copies a binary: the child briefly holds that writable
/// close-on-exec descriptor until its exec, and Linux refuses the new executable
/// with ETXTBSY meanwhile. Retry that errno only, never an arbitrary test error.
fn fixture_spawn(command: &mut Command) -> std::io::Result<Child> {
    fixture_exec_retry(|| command.spawn())
}

fn fixture_exec_retry<T>(mut attempt: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match attempt() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

#[test]
fn copied_fixture_spawn_retries_only_executable_file_busy() {
    let mut calls = 0;
    let result = fixture_exec_retry(|| {
        calls += 1;
        if calls == 1 {
            Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
        } else {
            Ok(7)
        }
    })
    .unwrap();
    assert_eq!((result, calls), (7, 2));
    let mut calls = 0;
    let result: std::io::Result<()> = fixture_exec_retry(|| {
        calls += 1;
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    });
    assert_eq!(
        result.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(calls, 1, "never retry a permission or test failure");
}

/// A detached session host serving one session, as a window starts one: the
/// session made through `-host`, then moved to a home short enough for its
/// socket path, and served by a copy of the binary that a test can replace
/// the way an install does.
struct Served {
    home: std::path::PathBuf,
    /// The scripted model's answers, read when a host starts.
    fixture: std::path::PathBuf,
    project: std::path::PathBuf,
    session: i64,
    executable: std::path::PathBuf,
    socket: std::path::PathBuf,
    child: std::process::Child,
    client: runeengine::platform::LocalStream,
    /// Reads the host's lines from where its answer to the hello ended.
    reader: BufReader<runeengine::platform::LocalStream>,
    /// Ends the host and removes both homes when the test is done.
    cleanup: Cleanup,
}

/// Connect to the host listening at `socket` as a window does: present the
/// host's key (`runeengine::access`) and hand back the stream with the reader
/// that holds whatever the host said after its answer.
fn window(
    socket: &std::path::Path,
) -> std::io::Result<(
    runeengine::platform::LocalStream,
    BufReader<runeengine::platform::LocalStream>,
)> {
    let stream = runeengine::platform::LocalStream::connect(socket)?;
    let introduction = runeengine::access::introduce(&stream, socket)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    assert_eq!(
        introduction.answer,
        runeengine::access::Answer::Admitted,
        "the host admits a window that presents its key"
    );
    Ok((stream, introduction.reader))
}

fn served(name: &str, fixture: Value) -> Served {
    served_with(name, fixture, None)
}

/// [`served`], its host reading `config` as its `rune.toml`.
fn served_with(name: &str, fixture: Value, config: Option<&str>) -> Served {
    use std::time::{Duration, Instant};
    let mut setup = start(name, Some(fixture));
    setup.next();
    setup.send(json!({"cmd":"session.open","id":1,"project":setup.project,"fresh":true}));
    let ack = setup.until("ack");
    let session = ack["data"]["session"].as_i64().unwrap();
    let base = setup.project.parent().unwrap().to_path_buf();
    let project = setup.project.clone();
    setup.finish();
    let short_home = std::path::PathBuf::from(format!(
        "/tmp/rune-dc-{}-{name}-{}",
        std::process::id(),
        runeengine::store::now_ms()
    ));
    std::fs::create_dir_all(&short_home).unwrap();
    std::fs::copy(base.join("rune.db"), short_home.join("rune.db")).unwrap();
    std::fs::copy(base.join("fixture.json"), short_home.join("fixture.json")).unwrap();
    // The session is filed under its project's folder, which moves with it.
    let folder = runeengine::project::find(&base, &project).expect("the project has its folder");
    let short_folder = short_home
        .join("projects")
        .join(folder.file_name().unwrap());
    let short_session = short_folder.join("sessions").join(session.to_string());
    std::fs::create_dir_all(&short_session).unwrap();
    std::fs::copy(
        folder.join("project.json"),
        short_folder.join("project.json"),
    )
    .unwrap();
    std::fs::copy(
        folder
            .join("sessions")
            .join(session.to_string())
            .join("meta.json"),
        short_session.join("meta.json"),
    )
    .unwrap();
    if let Some(config) = config {
        std::fs::write(short_home.join("rune.toml"), config).unwrap();
    }
    runeengine::testing::posix_shell_for_tests(&short_home);
    let test_executable = short_home.join("Rune");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &test_executable).unwrap();
    let child = fixture_spawn(
        Command::new(&test_executable)
            .env("RUNE_HOME", &short_home)
            .args(["-detached-host", &session.to_string(), "-project"])
            .arg(&project)
            .arg("-db")
            .arg(short_home.join("rune.db"))
            .arg("-script")
            .arg(base.join("fixture.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .unwrap();
    let socket = short_home
        .join("run")
        .join(format!("session_{session}.sock"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let (client, reader) = loop {
        if let Ok(window) = window(&socket) {
            break window;
        }
        assert!(Instant::now() < deadline, "host socket startup deadline");
        std::thread::yield_now();
    };
    let cleanup = Cleanup {
        pid: Some(child.id()),
        dirs: vec![short_home.clone(), base.clone()],
    };
    Served {
        cleanup,
        home: short_home,
        fixture: base.join("fixture.json"),
        project,
        session,
        executable: test_executable,
        socket,
        child,
        client,
        reader,
    }
}

#[test]
fn detached_host_cancels_unwatched_tool_without_waiting_idle_lifetime() {
    // A release is taken over in place by an exec, which Windows has not:
    // there a newer build is used at the next start, and a running program's
    // file cannot be replaced under it.
    if !runeengine::platform::REPLACES_IN_PLACE {
        return;
    }
    use std::time::{Duration, Instant};
    let Served {
        cleanup: _cleanup,
        home: short_home,
        fixture: _,
        project,
        session,
        executable: test_executable,
        socket,
        mut child,
        mut client,
        mut reader,
    } = served(
        "detached-last-client",
        json!({"responses": [{"chunks": [
            {"tool_call": {"id": "long-shell", "name": "terminal_use", "args": {
                "action": "run cancellable shell", "command": "echo started; sleep 30; echo unexpected", "run_in_background": false
            }}}
        ]}]}),
    );
    // How long the first output may take to come, not how fast the work
    // stops (measured below): a confined start on a loaded CI runner took
    // more than 5 s (2026-10-19).
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"cmd":"prompt","id":2,"session":session,"text":"run shell"})
    )
    .unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "permission_request" {
            writeln!(
                client,
                "{}",
                json!({"cmd":"permission.reply","id":3,"req":event["req"],"decision":"allow_once"})
            )
            .unwrap();
        }
        if event["t"] == "tool_output" && event["chunk"].as_str().unwrap_or("").contains("started")
        {
            break;
        }
        if event["t"] == "tool_output" {
            break;
        }
    }
    let closed_at = Instant::now();
    client.shutdown().unwrap();
    drop(reader);
    drop(client);
    let db =
        runeengine::store::Db::open(&short_home.join("rune.db"), Duration::from_secs(5)).unwrap();
    loop {
        let rows = db.history(session).unwrap();
        if rows
            .iter()
            .any(|row| row.tool_call_id.as_deref() == Some("long-shell"))
        {
            break;
        }
        assert!(
            closed_at.elapsed() < Duration::from_secs(3),
            "last-client cancellation must close tool history promptly"
        );
        std::thread::yield_now();
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "idle host may remain, but unwatched work must stop"
    );
    let (mut reconnected, mut resumed_reader) = window(&socket).unwrap();
    reconnected
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        reconnected,
        "{}",
        json!({"cmd":"session.resume","id":5,"session":session})
    )
    .unwrap();
    let mut old_running = false;
    loop {
        let mut line = String::new();
        resumed_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "turn_start" {
            old_running = true;
        }
        if event["t"] == "done" {
            old_running = false;
        }
        if event["t"] == "ack" && event["id"] == 5 {
            assert!(
                event["data"]["settings"].is_object(),
                "resume payload must include settings"
            );
            break;
        }
    }
    while old_running {
        let mut line = String::new();
        resumed_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "done" {
            old_running = false;
        }
    }
    writeln!(
        reconnected,
        "{}",
        json!({"cmd":"prompt","id":4,"session":session,"text":"continue after reconnect"})
    )
    .unwrap();
    loop {
        let mut line = String::new();
        resumed_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "done" && event["turn"].as_i64().is_some_and(|turn| turn > 1) {
            assert_eq!(event["stop"], "end", "reconnect must reset cancellation");
            break;
        }
    }
    writeln!(
        reconnected,
        "{}",
        json!({"cmd":"session.open","id":6,"project":project,"fresh":true})
    )
    .unwrap();
    let child_session = loop {
        let mut line = String::new();
        resumed_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "ack" && event["id"] == 6 {
            break event["data"]["session"].as_i64().unwrap();
        }
    };
    let updated = short_home.join("RuneNext");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &updated).unwrap();
    std::fs::rename(&updated, &test_executable).unwrap();
    let wanted = runeengine::platform::FileIdentity::of_path(&test_executable).unwrap();
    loop {
        let mut line = String::new();
        resumed_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "host_upgrading" {
            break;
        }
    }
    reconnected.shutdown().unwrap();
    drop(resumed_reader);
    drop(reconnected);
    let upgrade_at = Instant::now();
    loop {
        let current = runeengine::platform::fixture::program_of(child.id()).unwrap();
        if current == wanted {
            break;
        }
        assert!(
            upgrade_at.elapsed() < Duration::from_secs(5),
            "idle host must detect installed update without a client trigger"
        );
        std::thread::yield_now();
    }
    let child_socket = short_home
        .join("run")
        .join(format!("session_{child_session}.sock"));
    let (mut child_client, mut child_reader) = loop {
        if let Ok(window) = window(&child_socket) {
            break window;
        }
        assert!(
            upgrade_at.elapsed() < Duration::from_secs(10),
            "child-session socket must survive handover"
        );
        std::thread::yield_now();
    };
    child_client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(
        child_client,
        "{}",
        json!({"cmd":"session.resume","id":7,"session":child_session})
    )
    .unwrap();
    loop {
        let mut line = String::new();
        child_reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["t"] == "ack" && event["id"] == 7 {
            assert_eq!(event["ok"], true);
            break;
        }
    }
    child_client.shutdown().unwrap();
    // Cleanup only the isolated test process, after cancellation and reexec proof.
    child.kill().unwrap();
    child.wait().unwrap();
    drop(db);
    std::fs::remove_dir_all(short_home).unwrap();
}

#[test]
fn the_first_line_reports_the_version_and_the_sandbox() {
    let mut h = start("ready", None);
    let ready = h.next();
    assert_eq!(ready["t"], "ready");
    assert!(!ready["version"].as_str().unwrap().is_empty());
    let sandbox = &ready["sandbox"];
    assert!(sandbox["enforced"].is_boolean());
    if sandbox["enforced"] == json!(false) {
        assert!(
            sandbox["reason"].is_string(),
            "a disabled sandbox must say why"
        );
    }
    h.finish();
}

/// A host with no session yet reports what confinement would apply without
/// leaving anything behind. 0.1.5's probe made `scratch/probe` at the top of
/// the home: a folder no format keeps there, and the very shape by which an
/// unmarked home is recognised as format 1.
#[test]
fn reporting_the_sandbox_leaves_nothing_in_the_home() {
    let mut h = start("probe", None);
    assert_eq!(h.next()["t"], "ready");
    let home = h.project.parent().unwrap().to_path_buf();
    h.finish();
    assert!(
        !home.join("scratch").exists(),
        "the sandbox report created {}",
        home.join("scratch").display()
    );
}

#[test]
fn question_forms_use_the_host_protocol_in_every_mode_and_cancel_cleanly() {
    let mut form: Value = serde_json::from_str(include_str!("Questions.json")).unwrap();
    // The system names the questions — the model sends prose only, so the form
    // may repeat its two templates as often as it likes without any id.
    form["questions"] = Value::Array((0..5).map(|i| form["questions"][i % 2].clone()).collect());
    for mode in ["ask", "auto", "full"] {
        let mut h = start(&format!("question-{mode}"), None);
        h.next();
        std::fs::write(
            h.project.join("CLASSIFIER.md"),
            "Review all proposed actions.",
        )
        .unwrap();
        h.send(json!({"cmd":"session.open","id":1,"project":h.project}));
        h.until("ack");
        h.send(json!({"cmd":"session.mode","id":2,"session":1,"mode":mode}));
        h.until("ack");
        h.send(
            json!({"cmd":"tool.invoke","id":3,"session":1,"name":"ask_user_questions","args":form}),
        );
        let request = h.until("questions_request");
        assert_eq!(request["questions"].as_array().unwrap().len(), 5);
        let answers: Vec<_> = request["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| json!({"id":q["id"],"selected":[q["options"][0]["id"]],"custom":""}))
            .collect();
        h.send(json!({"cmd":"questions.reply","id":4,"session":1,"req":request["req"],"reply":{"status":"answered","answers":answers}}));
        let result = h.until("tool_result");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["detail"]["status"], "answered");
        assert_eq!(result["detail"]["answers"].as_array().unwrap().len(), 5);
        h.send(
            json!({"cmd":"tool.invoke","id":5,"session":1,"name":"ask_user_questions","args":form}),
        );
        let request = h.until("questions_request");
        h.send(json!({"cmd":"cancel","id":6,"session":1}));
        let result = h.until("tool_result");
        assert_eq!(result["ok"], false);
        h.send(json!({"cmd":"questions.reply","id":7,"session":1,"req":request["req"],"reply":{"status":"unavailable"}}));
        loop {
            let event = h.next();
            if event["t"] == "ack" && event["id"] == 7 {
                assert_eq!(event["ok"], false);
                break;
            }
        }
        // Shutdown must also release a form that never received any answer.
        h.send(
            json!({"cmd":"tool.invoke","id":8,"session":1,"name":"ask_user_questions","args":form}),
        );
        h.until("questions_request");
        h.finish();
    }
}

#[test]
fn one_shot_questions_report_unavailable_without_waiting_for_stdin() {
    let form: Value = serde_json::from_str(include_str!("Questions.json")).unwrap();
    let fixture = json!({"responses":[{"chunks":[{"tool_call":{"id":"q1","name":"ask_user_questions","args":form}}]},{"chunks":[{"text":"No user answer was provided."}]}]});
    let h = start("questions-once", Some(fixture));
    let base = h.project.parent().unwrap().to_path_buf();
    let project = h.project.clone();
    h.finish();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env("RUNE_HOME", &base)
        .arg("-project")
        .arg(project)
        .arg("-script")
        .arg(base.join("fixture.json"))
        .arg("-p")
        .arg("Ask for preferences")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("one-shot question hung waiting for input");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("User input unavailable"));
}

#[test]
fn every_command_is_acknowledged_by_its_own_id() {
    let mut h = start("ack", None);
    h.next();
    let project = h.project.to_string_lossy().to_string();
    h.send(json!({ "cmd": "session.open", "id": 11, "project": project }));

    let ack = h.until("ack");
    assert_eq!(ack["id"], 11);
    assert_eq!(ack["ok"], true);
    assert_eq!(ack["data"]["session"], 1);
    h.finish();
}

/// `rune -storage` lists every project folder, and `prune` removes only the
/// folders whose project is gone — and only after the person says yes.
#[test]
fn storage_prunes_only_a_gone_project_and_only_after_yes() {
    let mut h = start("storage", None);
    h.next();
    let base = h.project.parent().unwrap().to_path_buf();
    let gone = base.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    for (id, project) in [(1, h.project.clone()), (2, gone.clone())] {
        h.send(json!({ "cmd": "session.open", "id": id,
                       "project": project.to_string_lossy() }));
        let ack = h.until("ack");
        assert_eq!(ack["ok"], true, "{ack}");
    }
    let kept = runeengine::platform::canonical(&h.project).unwrap();
    h.finish();
    std::fs::remove_dir_all(&gone).unwrap();

    let rune = |args: &[&str], answer: &str| -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
            .env("RUNE_HOME", &base)
            .arg("-db")
            .arg(base.join("rune.db"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let folders = base.join("projects");
    let count = || std::fs::read_dir(&folders).unwrap().count();

    let listed = rune(&["-storage"], "");
    assert!(listed.contains("missing"), "{listed}");
    assert!(listed.contains(&*gone.to_string_lossy()), "{listed}");
    assert!(listed.contains(&*kept.to_string_lossy()), "{listed}");
    assert_eq!(count(), 2);

    let declined = rune(&["-storage", "prune"], "n\n");
    assert!(declined.contains("Nothing was deleted."), "{declined}");
    assert_eq!(count(), 2);

    let confirmed = rune(&["-storage", "prune"], "y\n");
    assert!(confirmed.contains("Deleted 1 folder"), "{confirmed}");
    assert_eq!(count(), 1);
    let after = rune(&["-storage"], "");
    assert!(after.contains(&*kept.to_string_lossy()), "{after}");
    assert!(!after.contains("missing"), "{after}");
}

/// A home as the previous release (format 1) left it: a session at the top
/// of the home, its memory keyed by project, an old configuration.
fn old_home(base: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = base.join("home");
    let project = base.join("work/app");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let project = runeengine::platform::canonical(&project).unwrap();
    {
        let db =
            runeengine::store::Db::open(&home.join("rune.db"), std::time::Duration::from_secs(5))
                .unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (id, project, title, created_at, updated_at) VALUES (1, ?1, 'one', 1, 1)",
                [project.to_string_lossy()],
            )
            .unwrap();
        db.conn().pragma_update(None, "user_version", 19).unwrap();
    }
    std::fs::create_dir_all(home.join("sessions/1")).unwrap();
    std::fs::write(home.join("sessions/1/session.jsonl"), "{}\n").unwrap();
    let key = runeengine::project::project_key(&project);
    std::fs::create_dir_all(home.join(format!("memory/{key}"))).unwrap();
    std::fs::write(home.join(format!("memory/{key}/MEMORY.md")), "- a note\n").unwrap();
    std::fs::write(
        home.join("rune.toml"),
        "version = 1\n[inspect_files]\nmax_output_chars = 1234\n",
    )
    .unwrap();
    (home, project)
}

/// The previous release's home is brought up to this build by the first
/// command that touches it; the check a running build asks before handing
/// over changes nothing; a newer build's home is refused untouched.
#[test]
fn an_old_home_is_upgraded_by_the_first_command_that_touches_it() {
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (home, project) = old_home(&base);
    let rune = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .env("RUNE_HOME", &home)
            .args(args)
            .output()
            .unwrap()
    };

    let checked = rune(&["-upgrade-check"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(
        home.join("sessions/1").is_dir(),
        "the check changed nothing"
    );

    let listed = rune(&["-storage"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let folder = runeengine::project::find(&home, &project).expect("its folder");
    assert!(folder.join("sessions/1/session.jsonl").is_file());
    assert!(folder.join("memory/MEMORY.md").is_file());
    assert!(!home.join("sessions").exists());
    let config = std::fs::read_to_string(home.join("rune.toml")).unwrap();
    assert!(config.contains("[inspect_file]"), "{config}");
    assert_eq!(
        std::fs::read_to_string(home.join("version.json")).unwrap(),
        format!(r#"{{"version":{}}}"#, runeengine::upgrade::VERSION)
    );

    std::fs::write(home.join("version.json"), r#"{"version":99}"#).unwrap();
    let refused = rune(&["-storage"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("newer Rune"));
    assert!(!rune(&["-upgrade-check"]).status.success());
}

/// An upgrade that cannot finish is undone and the person is told why; the
/// installed build is never replaced (owner, 2026-10-16: a person's machine
/// has no `rune.prev`, and an isolated home once replaced the owner's build).
/// A file named like the old rollback is neither run nor moved.
#[test]
fn a_failed_upgrade_leaves_the_home_and_the_installed_build_as_they_were() {
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-rollback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let _cleanup = Cleanup {
        pid: None,
        dirs: vec![base.clone()],
    };
    let (home, project) = old_home(&base);
    // The project's folder already holds a session 1: the upgrade cannot
    // move the old one there.
    let folder = runeengine::project::open(&home, &project).unwrap();
    std::fs::create_dir_all(folder.join("sessions/1")).unwrap();
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let installed = bin.join("rune");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &installed).unwrap();
    let decoy = "#!/bin/sh\necho \"previous build: $*\"\n";
    std::fs::write(bin.join("rune.prev"), decoy).unwrap();
    runeengine::platform::set_file_mode(&bin.join("rune.prev"), 0o755).unwrap();
    let before = std::fs::read(&installed).unwrap();

    let ran = Command::new(&installed)
        .env("RUNE_HOME", &home)
        .env_remove("RUNE_HANDOVER")
        .arg("-storage")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(!ran.status.success());
    assert!(!stdout.contains("previous build"), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("could not be updated") && stderr.contains("Nothing of yours was changed"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&installed).unwrap(), before, "never replaced");
    assert_eq!(
        std::fs::read_to_string(bin.join("rune.prev")).unwrap(),
        decoy
    );
    assert!(!bin.join("rune.failed").exists());
    assert!(!bin.join("rune.rollback").exists());
    // The home is the previous build's, as it was.
    assert!(home.join("sessions/1/session.jsonl").is_file());
    assert!(home.join("rune.toml").is_file());
    assert!(std::fs::read_to_string(home.join("rune.toml"))
        .unwrap()
        .contains("[inspect_files]"));
    assert!(!home.join("version.json").exists());
}

#[test]
fn a_malformed_line_is_reported_and_the_host_keeps_going() {
    let mut h = start("malformed", None);
    h.next();
    writeln!(h.stdin, "this is not json").unwrap();
    h.stdin.flush().unwrap();

    let err = h.until("error");
    assert_eq!(err["code"], "protocol");

    // Still alive and still answering.
    h.send(json!({ "cmd": "tool.list", "id": 12 }));
    let ack = h.until("ack");
    assert_eq!(ack["id"], 12);
    assert_eq!(ack["ok"], true);
    h.finish();
}

#[test]
fn an_unknown_session_is_refused_without_killing_the_host() {
    let mut h = start("unknown", None);
    h.next();
    h.send(json!({ "cmd": "prompt", "id": 13, "session": 404, "text": "hello" }));
    let ack = h.until("ack");
    assert_eq!(ack["id"], 13);
    assert_eq!(ack["ok"], false);
    assert!(ack["error"].as_str().unwrap().contains("not found"));
    h.finish();
}

#[test]
fn tool_declarations_are_clean_and_flat() {
    let mut h = start("tools", None);
    h.next();
    h.send(json!({ "cmd": "tool.list", "id": 14 }));
    let ack = h.until("ack");

    let tools = ack["data"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    for t in tools {
        let props = t["parameters"]["properties"].as_object().unwrap();
        assert!(props.contains_key("action"), "{} lacks action", t["name"]);
        assert_eq!(t["parameters"]["required"][0], "action");
    }
    h.finish();
}

#[test]
fn web_fetch_is_a_separate_tool_in_the_real_host_without_search_credentials() {
    let mut h = start("web-fetch-tool", None);
    h.next();
    h.send(json!({"cmd":"tool.list", "id":14}));
    let ack = h.until("ack");
    let tools = ack["data"]["tools"].as_array().unwrap();
    let search = tools.iter().find(|t| t["name"] == "web_search").unwrap();
    let fetch = tools.iter().find(|t| t["name"] == "web_fetch").unwrap();
    assert_eq!(search["parameters"]["required"], json!(["action", "query"]));
    assert!(search["parameters"]["properties"].get("url").is_none());
    // Owner, 2026-10-14: no mode — image search is gone — and no vendor.
    let mut props: Vec<&String> = search["parameters"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    props.sort();
    assert_eq!(props, ["action", "count", "query"]);
    assert_eq!(fetch["parameters"]["required"], json!(["action", "url"]));
    assert_eq!(
        fetch["parameters"]["properties"].as_object().unwrap().len(),
        2
    );
    assert_eq!(fetch["mutates"], false);
    h.send(json!({"cmd":"session.open", "id":1, "project":h.project}));
    let ack = h.until("ack");
    assert_eq!(ack["ok"], true);
    h.send(json!({"cmd":"tool.invoke", "id":2, "session":1, "name":"web_fetch", "args":{"action":"read page", "url":"https://127.0.0.1/"}}));
    let result = h.until("tool_result");
    assert_eq!(result["ok"], false);
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("internal/private"), "{content}");
    assert!(!content.contains("serper"), "{content}");
    h.finish();
}

#[test]
fn a_full_turn_streams_the_expected_event_sequence() {
    let fixture = json!({
        "responses": [
            { "chunks": [
                { "tool_call": { "id": "t1", "name": "search_text",
                                 "args": { "action": "find the adder",
                                           "pattern": "fn add" } } }
            ]},
            { "chunks": [
                { "text": "It is in src/lib.rs." },
                { "usage": { "input": 800, "output": 9 } }
            ]}
        ]
    });
    let mut h = start("turn", Some(fixture));
    h.next();
    let project = h.project.to_string_lossy().to_string();
    h.send(json!({ "cmd": "session.open", "id": 1, "project": project }));
    h.until("ack");
    h.send(json!({ "cmd": "prompt", "id": 2, "session": 1, "text": "where is add?" }));

    let mut kinds = Vec::new();
    loop {
        let e = h.until_any();
        kinds.push(e["t"].as_str().unwrap().to_string());
        if e["t"] == "done" {
            assert_eq!(e["stop"], "end");
            break;
        }
    }
    assert!(kinds.contains(&"turn_start".to_string()));
    assert!(kinds.contains(&"tool_call".to_string()));
    assert!(kinds.contains(&"tool_result".to_string()));
    assert!(kinds.contains(&"text".to_string()));
    assert!(kinds.contains(&"usage".to_string()));
    h.finish();
}

#[test]
fn a_write_turn_asks_and_then_writes() {
    let fixture = json!({
        "responses": [
            { "chunks": [
                { "tool_call": { "id": "w1", "name": "create_file",
                                 "args": { "action": "add notes",
                                           "path": "Notes.md", "text": "note\n" } } }
            ]},
            { "chunks": [ { "text": "Added." } ]}
        ]
    });
    let mut h = start("write", Some(fixture));
    h.next();
    let project = h.project.to_string_lossy().to_string();
    h.send(json!({ "cmd": "session.open", "id": 1, "project": project }));
    h.until("ack");
    h.send(json!({ "cmd": "prompt", "id": 2, "session": 1, "text": "add notes" }));

    let done = h.until("done");
    assert_eq!(done["stop"], "end");
    assert_eq!(
        std::fs::read_to_string(h.project.join("Notes.md")).unwrap(),
        "note\n"
    );
    h.finish();
}

impl Host {
    /// Like `until`, but returns every event rather than skipping to one kind.
    fn until_any(&mut self) -> Value {
        let e = self.next();
        if e["t"] == "permission_request" {
            let req = e["req"].as_str().unwrap().to_string();
            self.send(json!({ "cmd": "permission.reply", "id": 90, "req": req,
                              "decision": "allow_once" }));
        }
        e
    }
}

#[test]
fn starting_without_a_project_leaves_nothing_behind_in_the_directory() {
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-host-{}-noproject", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cwd = base.join("somewhere");
    std::fs::create_dir_all(&cwd).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env("RUNE_HOME", &base)
        .arg("-host")
        .arg("-db")
        .arg(base.join("rune.db"))
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("rune binary should start");

    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(
        ready["t"], "ready",
        "the sandbox is still reported with no project open"
    );

    writeln!(stdin, "{}", json!({ "cmd": "shutdown", "id": 1 })).unwrap();
    stdin.flush().unwrap();
    let _ = child.wait();

    // The sandbox probe needs a scratch directory to grant. It used to make one
    // here, in whatever directory the user started the host from, and leave it.
    let left: Vec<String> = std::fs::read_dir(&cwd)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        left.is_empty(),
        "the host littered the working directory: {left:?}"
    );
}

#[test]
fn model_selection_is_atomic_remembers_each_provider_and_persists() {
    let fixture = json!({"responses":[],"test_providers":{"default":"alpha","list":[
        {"name":"alpha","base_url":"http://127.0.0.1:1/v1","auth":{"header":"Authorization","prefix":"Bearer "},"models":[{"id":"a1"},{"id":"a2"}]},
        {"name":"beta","base_url":"http://127.0.0.1:1/v1","auth":{"header":"Authorization","prefix":"Bearer "},"models":[{"id":"b1"},{"id":"b2"}]}
    ]}});
    let mut h = start("preferences-atomic", Some(fixture));
    let project = h.project.clone();
    h.send(json!({"cmd":"session.open","id":1,"project":project}));
    let opened = h.until("ack");
    let session = opened["data"]["session"].as_i64().unwrap();
    for (id, provider, model) in [(2, "beta", "b2"), (3, "alpha", "a2")] {
        h.send(json!({"cmd":"settings.select","id":id,"session":session,"provider":provider,"model":model}));
        let ack = h.until("ack");
        assert_eq!(ack["ok"], true, "{ack}");
        assert_eq!(ack["data"]["settings"]["provider"], provider);
        assert_eq!(ack["data"]["settings"]["model"], model);
    }
    h.send(json!({"cmd":"settings.set","id":4,"session":session,"key":"provider","value":"beta"}));
    let restored = h.until("ack");
    assert_eq!(restored["data"]["settings"]["model"], "b2");
    h.send(json!({"cmd":"settings.select","id":5,"session":session,"provider":"alpha","model":"not-in-catalog"}));
    assert_eq!(h.until("ack")["ok"], false);
    h.send(json!({"cmd":"settings.get","id":6,"session":session}));
    let unchanged = h.until("ack");
    assert_eq!(unchanged["data"]["settings"]["provider"], "beta");
    assert_eq!(unchanged["data"]["settings"]["model"], "b2");
    h.send(json!({"cmd":"settings.select","id":7,"session":session,"provider":"beta","model":"custom-model","custom":true}));
    assert_eq!(h.until("ack")["ok"], true);
    h.finish();
    let db = runeengine::store::Db::open(
        &project.parent().unwrap().join("rune.db"),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let stored: Value = serde_json::from_str(&db.settings_json(session).unwrap().unwrap()).unwrap();
    assert_eq!(stored["provider"], "beta");
    assert_eq!(stored["model"], "custom-model");
}

#[test]
fn slow_connection_test_keeps_host_responsive_and_returns_auth_failure() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = [0; 8192];
        let n = socket.read(&mut bytes).unwrap();
        assert!(n > 0);
        started_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(3));
        let body = r#"{"error":{"message":"fixture credential rejected"}}"#;
        write!(socket,"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let mut h = start(
        "preferences-responsive",
        Some(
            json!({"responses":[],"test_providers":{"default":"local","list":[
                {"name":"local","base_url":format!("http://{address}/v1"),"auth":{"header":"Authorization"},"models":[{"id":"fixture-model"}]}
            ]}}),
        ),
    );
    h.send(json!({"cmd":"provider.test","id":11,"name":"local"}));
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    h.send(json!({"cmd":"provider.list","id":12}));
    let quick = h.until("ack");
    assert_eq!(quick["id"], 12, "the host waited for network I/O: {quick}");
    assert_eq!(quick["ok"], true);
    release_tx.send(()).unwrap();
    let tested = h.until("ack");
    assert_eq!(tested["id"], 11);
    assert_eq!(tested["ok"], false);
    assert!(tested["error"].as_str().unwrap().contains("HTTP 401"));
    server.join().unwrap();
    h.finish();
}

/// Every row of session `session`'s transcript under `home`, oldest first.
fn transcript(home: &std::path::Path, session: i64) -> Vec<Value> {
    let Ok(projects) = std::fs::read_dir(home.join("projects")) else {
        return Vec::new();
    };
    for project in projects.flatten() {
        let path = project
            .path()
            .join(format!("sessions/{session}/session.jsonl"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return text
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
        }
    }
    Vec::new()
}

/// One line from the host, parsed.
fn event(reader: &mut BufReader<runeengine::platform::LocalStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
}

/// Read until `done`, answering every permission request; the events seen.
fn through_turn(
    client: &mut runeengine::platform::LocalStream,
    reader: &mut BufReader<runeengine::platform::LocalStream>,
) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let e = event(reader);
        if e["t"] == "permission_request" {
            writeln!(
                client,
                "{}",
                json!({"cmd":"permission.reply","id":90,"req":e["req"],"decision":"allow_once"})
            )
            .unwrap();
        }
        let done = e["t"] == "done";
        seen.push(e);
        if done {
            return seen;
        }
    }
}

#[test]
fn a_manage_process_result_keeps_additional_information_inside_its_report() {
    let mut h = start(
        "one-process-report",
        Some(json!({"responses":[
            {"chunks":[{"text":"completion noted"}]},
            {"chunks":[{"text":"completion noted"}]}
        ]})),
    );
    h.next();
    h.send(json!({"cmd":"session.open","id":1,"project":h.project}));
    let opened = h.until("ack");
    let session = opened["data"]["session"].as_i64().unwrap();
    for (i, command) in ["printf 'short output\\n'", "printf '%040000d\\n' 0"]
        .iter()
        .enumerate()
    {
        h.send(
            json!({"cmd":"tool.invoke","id":10 + i,"session":session,"name":"terminal_use",
            "args":{"action":"print report fixture", "command":command, "run_in_background":true}}),
        );
        let mut asked = Vec::new();
        let launch = loop {
            let e = h.next();
            if e["t"] == "permission_request" {
                asked.push(e["full_access"].clone());
                h.send(json!({"cmd":"permission.reply","id":90,"req":e["req"],"decision":"allow_once"}));
            }
            if e["t"] == "tool_result" {
                break e;
            }
        };
        assert_eq!(launch["ok"], true, "{launch}");
        let task = launch["content"]
            .as_str()
            .unwrap()
            .split("Running in the background as ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        // An idle completion starts a notification turn, rather than emitting
        // the busy-only Notification event. Its Done releases admission.
        h.until("done");
        h.send(
            json!({"cmd":"tool.invoke","id":20 + i,"session":session,"name":"manage_process",
            "args":{"action":"read report fixture", "process_id":task}}),
        );
        let result = h.until("tool_result");
        assert_eq!(result["ok"], true, "{result}");
        let content = result["content"].as_str().unwrap();
        assert_eq!(content.matches("<tool_notice>").count(), 1, "{content}");
        assert_eq!(content.matches("</tool_notice>").count(), 1, "{content}");
        let split = runeengine::tools::notice::split(content);
        assert_eq!(split.notices.len(), 1);
        assert!(
            split.notices[0].contains("Process: Completed"),
            "asked with full access: {asked:?}\n{content}"
        );
        assert_eq!(split.notices[0].contains("Additional information:"), i == 1);
        assert!(!split.notices[0].contains("Capture:"));
        if i == 1 {
            assert!(split.notices[0].contains("over the 30000 limit"));
            assert!(split.rest.starts_with("<preview_head"));
            assert!(split.rest.contains("<preview_tail"));
        } else {
            assert_eq!(split.rest.trim(), "short output");
        }
    }
    h.finish();
}

/// `manage_process` through the frontend's door — the same one call the
/// model's path runs — and its result.
fn manage(
    client: &mut runeengine::platform::LocalStream,
    reader: &mut BufReader<runeengine::platform::LocalStream>,
    session: i64,
    id: u64,
    args: Value,
) -> Value {
    let invoke =
        json!({"cmd":"tool.invoke","id":id,"session":session,"name":"manage_process","args":args});
    writeln!(client, "{invoke}").unwrap();
    let mut refused = false;
    loop {
        let e = event(reader);
        // Refused while a turn still holds the session: sent again at its
        // `done`, as a frontend does.
        if e["t"] == "ack" && e["id"] == id && e["ok"] == false {
            let error = e["error"].as_str().unwrap_or_default();
            assert!(error.contains("already running a turn"), "{e}");
            refused = true;
            continue;
        }
        if refused && e["t"] == "done" {
            refused = false;
            writeln!(client, "{invoke}").unwrap();
            continue;
        }
        if e["t"] == "permission_request" {
            writeln!(
                client,
                "{}",
                json!({"cmd":"permission.reply","id":91,"req":e["req"],"decision":"allow_once"})
            )
            .unwrap();
        }
        if e["t"] == "tool_result" {
            return e;
        }
    }
}

/// What the kernel says of `pid`: its state and its parent.
fn process(pid: i64) -> Option<(runeengine::platform::fixture::ProcessState, i64)> {
    let pid = i32::try_from(pid).ok()?;
    let state = runeengine::platform::fixture::process_state(pid)?;
    let parent = runeengine::platform::fixture::parent_of(pid)?;
    Some((state, i64::from(parent)))
}

/// What `ps` says of `pid` — its state, group, terminal, what it waits on —
/// for a failure's message.
fn seen_by_ps(pid: i64) -> String {
    let state = process(pid);
    let ps = Command::new("ps")
        .args([
            "-o",
            "pid,ppid,pgid,tpgid,stat,wchan,tty,command",
            "-p",
            &pid.to_string(),
        ])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_else(|e| format!("ps: {e}"));
    format!("{state:?}\n{ps}")
}

/// The owner's case (2026-10-06): a dev server running in the background
/// keeps running through a live upgrade — the same process, never stopped,
/// still the host's child — its log goes on, a task that ends after the
/// upgrade is reported by the new build, and the new build can stop it.
#[test]
fn a_background_task_runs_on_through_a_live_upgrade() {
    // A release is taken over in place by an exec, which Windows has not:
    // there a newer build is used at the next start, and a running program's
    // file cannot be replaced under it.
    if !runeengine::platform::REPLACES_IN_PLACE {
        return;
    }
    use std::time::{Duration, Instant};
    let marker = format!("rune-upgrade-tick-{}", std::process::id());
    // The second task ends when the test says so — after the new build has
    // taken the host over, however long a loaded machine takes to get there.
    let release = std::env::temp_dir().join(format!("rune-upgrade-release-{}", std::process::id()));
    let _ = std::fs::remove_file(&release);
    let Served {
        cleanup: _cleanup,
        home,
        fixture,
        project: _,
        session,
        executable,
        socket,
        mut child,
        mut client,
        mut reader,
    } = served(
        "upgrade-background",
        json!({"responses": [
            {"chunks": [
                {"tool_call": {"id": "serve", "name": "terminal_use", "args": {
                    "action": "serve", "run_in_background": true,
                    "command": format!("while true; do echo {marker}; sleep 0.05; done")}}},
                {"tool_call": {"id": "later", "name": "terminal_use", "args": {
                    "action": "end after the upgrade", "run_in_background": true,
                    "command": format!(
                        "while [ ! -e '{}' ]; do sleep 0.05; done; echo bye; exit 3",
                        release.display()
                    )}}}
            ]},
            {"chunks": [{"text": "Both started."}]}
        ]}),
    );
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"cmd":"prompt","id":2,"session":session,"text":"start the server"})
    )
    .unwrap();
    through_turn(&mut client, &mut reader);

    let listed = manage(
        &mut client,
        &mut reader,
        session,
        3,
        json!({"action":"list them","process_id":"all"}),
    );
    let tasks = listed["detail"]["processes"].as_array().unwrap().clone();
    let of = |needle: &str| {
        tasks
            .iter()
            .find(|t| t["command"].as_str().unwrap().contains(needle))
            .unwrap()
            .clone()
    };
    let (serve, later) = (of(&marker), of("exit 3"));
    let serve_pid = serve["pid"].as_i64().unwrap();
    let later_pid = later["pid"].as_i64().unwrap();
    let log = std::path::PathBuf::from(serve["out_file"].as_str().unwrap());
    assert!(log.starts_with(home.join("projects")), "{}", log.display());
    let host = child.id() as i64;
    assert_eq!(process(serve_pid).map(|p| p.1), Some(host));
    // Its log has begun under the old build: what goes on after the upgrade
    // is measured against output that was already there.
    let begun = Instant::now();
    while std::fs::metadata(&log).map_or(0, |m| m.len()) == 0 {
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "the log never began before the upgrade: {}",
            seen_by_ps(serve_pid)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The scripted model is read afresh by the build that takes over, the
    // way a real model holds no state of its own: what it answers next.
    std::fs::write(
        &fixture,
        json!({"responses": [
            {"chunks": [{"text": "Noted that it ended."}]},
            {"chunks": [{"text": "Noted that it was stopped."}]}
        ]})
        .to_string(),
    )
    .unwrap();

    // Installed the way a release is: a new file renamed over the old one.
    let next = home.join("RuneNext");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &next).unwrap();
    std::fs::rename(&next, &executable).unwrap();
    let installed = runeengine::platform::FileIdentity::of_path(&executable).unwrap();
    loop {
        if event(&mut reader)["t"] == "host_upgrading" {
            break;
        }
    }
    client.shutdown().unwrap();
    drop(reader);
    let swapped = Instant::now();
    while runeengine::platform::fixture::program_of(host as u32).unwrap() != installed {
        assert!(
            swapped.elapsed() < Duration::from_secs(10),
            "the host never took the new build"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The same process, never stopped, still the host's child.
    let (state, parent) = process(serve_pid).expect("the server is gone");
    assert_eq!(
        state,
        runeengine::platform::fixture::ProcessState::Live,
        "the server ended"
    );
    assert_eq!(parent, host);
    // Its log goes on under the new build.
    let size = std::fs::metadata(&log).unwrap().len();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&log).unwrap().len() <= size {
        assert!(
            Instant::now() < deadline,
            "the log stopped at {size} bytes: {}",
            seen_by_ps(serve_pid)
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    std::fs::write(&release, "").unwrap();
    let (mut client, mut reader) = loop {
        if let Ok(window) = window(&socket) {
            break window;
        }
        assert!(
            swapped.elapsed() < Duration::from_secs(10),
            "no socket after the upgrade"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"cmd":"session.resume","id":4,"session":session})
    )
    .unwrap();

    // The task that ends after the upgrade is reported by the new build: its
    // wake starts a turn of its own, which the transcript holds whether it ran
    // before this window was back (a slow upgrade) or after.
    let later_id = later["task_id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = transcript(&home, session);
        let woke = rows.iter().find(|row| {
            row["role"] == "context" && row["content"].as_str().unwrap_or("").contains(&later_id)
        });
        if let Some(woke) = woke {
            let said = woke["content"].as_str().unwrap();
            assert!(said.contains("exit code 3"), "{said}");
            if rows
                .iter()
                .any(|row| row["role"] == "assistant" && row["content"] == "Noted that it ended.")
            {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the ended task was never told: {rows:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(process(later_pid), None, "the ended task was reaped");

    // And the new build stops the server it took over. The model was told it
    // would hear when its server ends, and it does: the wake is answered by a
    // turn of its own once the call lets the session go — never stored for
    // nobody. Which of the two is said first is the threads' business.
    let kill = json!({"cmd":"tool.invoke","id":5,"session":session,"name":"manage_process",
                      "args":{"action":"stop the server","process_id":serve["task_id"],"kill":true}});
    writeln!(client, "{kill}").unwrap();
    let mut refused = false;
    let (mut stopped, mut woke, mut answered) = (None, None, Vec::new());
    let mut wake_done = false;
    let mut seen = Vec::new();
    loop {
        let mut line = String::new();
        if let Err(error) = reader.read_line(&mut line) {
            panic!("{error}: stopped {stopped:?}, woke {woke:?}, after it {answered:?}, every event {seen:?}");
        }
        let e: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        seen.push(e.clone());
        // The wake's answer is in the transcript before its turn lets the
        // session go (`done` comes after): a direct call sent in between is
        // refused, as for any frontend, and sent again once it is done
        // (Linux CI, 2026-10-19: no kill, so no result and no wake).
        if e["t"] == "ack" && e["id"] == 5 && e["ok"] == false {
            let error = e["error"].as_str().unwrap_or_default();
            assert!(error.contains("already running a turn"), "{e}");
            refused = true;
            continue;
        }
        if refused && e["t"] == "done" {
            refused = false;
            writeln!(client, "{kill}").unwrap();
            continue;
        }
        if e["t"] == "permission_request" {
            writeln!(
                client,
                "{}",
                json!({"cmd":"permission.reply","id":91,"req":e["req"],"decision":"allow_once"})
            )
            .unwrap();
        }
        // The turn the earlier wake started may still be on its way here.
        let of_the_kill = e["text"]
            .as_str()
            .is_some_and(|text| text.contains(serve["task_id"].as_str().unwrap()));
        if e["t"] == "tool_result" && stopped.is_none() {
            stopped = Some(e);
        } else if e["t"] == "turn_start" && e["notification"] == true && of_the_kill {
            woke = Some(e);
        } else if woke.is_some() && !wake_done {
            wake_done = e["t"] == "done";
            answered.push(e);
        }
        // The wake's turn may end before the kill's own result is read
        // (Linux CI, 2026-10-19: waiting on for a second `done` that never
        // came timed the read out).
        if wake_done && stopped.is_some() {
            break;
        }
    }
    let stopped = stopped.unwrap();
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert_eq!(stopped["detail"]["status"], "Killed", "{stopped}");
    let said = woke.unwrap()["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(said.contains(serve["task_id"].as_str().unwrap()), "{said}");
    assert!(said.contains("Killed"), "{said}");
    assert!(
        answered
            .iter()
            .any(|e| e["t"] == "text" && e["delta"] == "Noted that it was stopped."),
        "{answered:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while process(serve_pid).is_some() {
        assert!(
            Instant::now() < deadline,
            "the server was not stopped and reaped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let listed = manage(
        &mut client,
        &mut reader,
        session,
        6,
        json!({"action":"list them","process_id":"all"}),
    );
    let after = listed["detail"]["processes"].as_array().unwrap();
    assert_eq!(after.len(), 2, "both tasks are still known: {after:?}");

    client.shutdown().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_file(&release);
}

/// What a GUI sees of a live upgrade of `rune -host` (owner, 2026-10-06): the
/// process becomes the installed release in place — same pid, same pipes —
/// after `upgrading`; the successor says `ready` again, holds the same
/// session, and reads on from the middle of a command the frontend was still
/// writing; a background task runs on through it.
#[test]
fn the_stdio_host_becomes_the_installed_release_in_place() {
    // A release is taken over in place by an exec, which Windows has not:
    // there a newer build is used at the next start, and a running program's
    // file cannot be replaced under it.
    if !runeengine::platform::REPLACES_IN_PLACE {
        return;
    }
    use std::time::{Duration, Instant};
    let base = std::env::temp_dir()
        .join("rune-test")
        .join(format!("rune-stdio-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let marker = format!("rune-stdio-tick-{}", std::process::id());
    let fixture = base.join("fixture.json");
    std::fs::write(
        &fixture,
        json!({"responses": [
            {"chunks": [{"tool_call": {"id": "serve", "name": "terminal_use", "args": {
                "action": "serve", "run_in_background": true,
                "command": format!("while true; do echo {marker}; sleep 0.05; done")}}}]},
            {"chunks": [{"text": "Started."}]}
        ]})
        .to_string(),
    )
    .unwrap();
    let executable = base.join("Rune");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &executable).unwrap();
    let mut child = fixture_spawn(
        Command::new(&executable)
            .env("RUNE_HOME", &base)
            .arg("-host")
            .arg("-project")
            .arg(&project)
            .arg("-db")
            .arg(base.join("rune.db"))
            .arg("-script")
            .arg(&fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()),
    )
    .unwrap();
    let _cleanup = Cleanup {
        pid: Some(child.id()),
        dirs: vec![base.clone()],
    };
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut next = || {
        let mut line = String::new();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the host closed its output"
        );
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let mut until = |kind: &str, stdin: &mut std::process::ChildStdin| loop {
        let e = next();
        if e["t"] == "permission_request" {
            writeln!(
                stdin,
                "{}",
                json!({"cmd":"permission.reply","id":90,"req":e["req"],"decision":"allow_once"})
            )
            .unwrap();
        }
        if e["t"] == kind {
            return e;
        }
    };
    let ready = until("ready", &mut stdin);
    assert_eq!(ready["protocol"], runeengine::proto::PROTOCOL);
    writeln!(
        stdin,
        "{}",
        json!({"cmd":"prompt","id":2,"session":1,"text":"start the server"})
    )
    .unwrap();
    until("done", &mut stdin);
    writeln!(
        stdin,
        "{}",
        json!({"cmd":"tool.invoke","id":3,"session":1,"name":"manage_process",
               "args":{"action":"list","process_id":"all"}})
    )
    .unwrap();
    let listed = until("tool_result", &mut stdin);
    let task = listed["detail"]["processes"][0].clone();
    let pid = task["pid"].as_i64().unwrap();
    let log = std::path::PathBuf::from(task["out_file"].as_str().unwrap());

    // Half a command, then the release is installed.
    write!(stdin, r#"{{"cmd":"session.resume","id":41,"#).unwrap();
    stdin.flush().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let next_build = base.join("RuneNext");
    std::fs::copy(env!("CARGO_BIN_EXE_rune"), &next_build).unwrap();
    std::fs::rename(&next_build, &executable).unwrap();
    let installed = runeengine::platform::FileIdentity::of_path(&executable).unwrap();
    until("upgrading", &mut stdin);
    let again = until("ready", &mut stdin);
    assert_eq!(again["protocol"], runeengine::proto::PROTOCOL);
    let host = child.id();
    assert_eq!(
        runeengine::platform::fixture::program_of(host).unwrap(),
        installed,
        "the same process runs the installed build"
    );

    // The rest of the command: the successor reads on from its start.
    writeln!(stdin, r#""session":1}}"#).unwrap();
    let acked = loop {
        let e = until("ack", &mut stdin);
        if e["id"] == 41 {
            break e;
        }
    };
    assert_eq!(acked["ok"], true, "{acked}");

    // The background task ran on, still this process's child, its log going on.
    let parent = runeengine::platform::fixture::parent_of(pid as i32).unwrap();
    assert_eq!(parent, host);
    let size = std::fs::metadata(&log).unwrap().len();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&log).unwrap().len() <= size {
        assert!(Instant::now() < deadline, "the log stopped");
        std::thread::sleep(Duration::from_millis(50));
    }
    writeln!(
        stdin,
        "{}",
        json!({"cmd":"tool.invoke","id":5,"session":1,"name":"manage_process",
               "args":{"action":"stop","process_id":task["task_id"],"kill":true}})
    )
    .unwrap();
    let stopped = until("tool_result", &mut stdin);
    assert_eq!(stopped["detail"]["status"], "Killed", "{stopped}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while runeengine::platform::fixture::process_state(pid as i32).is_some() {
        assert!(
            Instant::now() < deadline,
            "the task was not stopped and reaped"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    writeln!(stdin, "{}", json!({"cmd":"shutdown","id":999})).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    let _ = std::fs::remove_dir_all(base);
}

/// A window that `exec`s a newer build of itself says first that it is
/// coming back (`session.returning`): the session's background task runs on
/// while it is gone, and is still running when it reconnects. A window that
/// just goes away stops the session's runtime at once, but its background
/// task is other work: it runs on for `host.idle_lifetime` — a person who
/// quit by accident finds it running — and is stopped once that has passed
/// with no window (owner, 2026-10-11).
#[test]
fn a_window_coming_back_keeps_the_sessions_work_running() {
    use std::time::{Duration, Instant};
    let marker = format!("rune-returning-tick-{}", std::process::id());
    let Served {
        cleanup: _cleanup,
        home: _,
        fixture: _,
        project: _,
        session,
        executable: _,
        socket,
        child: _,
        mut client,
        mut reader,
    } = served_with(
        "returning",
        json!({"responses": [
            {"chunks": [{"tool_call": {"id": "serve", "name": "terminal_use", "args": {
                "action": "serve", "run_in_background": true,
                "command": format!("while true; do echo {marker}; sleep 0.05; done")}}}]},
            {"chunks": [{"text": "Started."}]}
        ]}),
        Some("[host]\nidle_lifetime = \"3s\"\n"),
    );
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"cmd":"prompt","id":2,"session":session,"text":"serve"})
    )
    .unwrap();
    through_turn(&mut client, &mut reader);
    let listed = manage(
        &mut client,
        &mut reader,
        session,
        3,
        json!({"action":"list","process_id":"all"}),
    );
    let pid = listed["detail"]["processes"][0]["pid"].as_i64().unwrap();
    let running = |pid: i64| {
        process(pid)
            .is_some_and(|(state, _)| state == runeengine::platform::fixture::ProcessState::Live)
    };

    writeln!(
        client,
        "{}",
        json!({"cmd":"session.returning","id":4,"session":session})
    )
    .unwrap();
    loop {
        let e = event(&mut reader);
        if e["t"] == "ack" && e["id"] == 4 {
            assert_eq!(e["ok"], true);
            break;
        }
    }
    client.shutdown().unwrap();
    drop(reader);
    std::thread::sleep(Duration::from_millis(700));
    // Starting again, a window connects and lets go before it resumes: each
    // of those looks like the last window leaving.
    for _ in 0..2 {
        let (probe, _) = window(&socket).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        probe.shutdown().unwrap();
    }
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        running(pid),
        "the task was stopped while its window was coming back"
    );

    let (mut client, mut reader) = window(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"cmd":"session.resume","id":5,"session":session})
    )
    .unwrap();
    loop {
        let e = event(&mut reader);
        if e["t"] == "ack" && e["id"] == 5 {
            break;
        }
    }
    assert!(
        running(pid),
        "the task did not survive the window coming back"
    );

    // Going away without saying so: the runtime stops, the task runs on
    // through the idle lifetime, then is stopped.
    client.shutdown().unwrap();
    drop(reader);
    let left = Instant::now();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        running(pid),
        "the task was stopped before the idle lifetime passed"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(pid) {
        assert!(
            Instant::now() < deadline,
            "an unwatched task was left running past the idle lifetime"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        left.elapsed() >= Duration::from_secs(3),
        "stopped after {:?}, before the idle lifetime",
        left.elapsed()
    );
}
