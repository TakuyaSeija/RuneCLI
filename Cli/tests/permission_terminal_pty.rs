//! Exercise the real UI binary, not just card helpers: a long permission must
//! retain its answers, wheel events must reach its body, and PTY spinner frames
//! must replace one cell without leaking terminal controls into Rune's frame.

use std::{
    io,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use runeengine::{platform, terminal_screen::Screen};
use serde_json::json;

struct Window {
    child: platform::Process,
    pty: platform::Pty,
    screen: Screen,
    raw: Vec<u8>,
    decoded: usize,
    answered: usize,
    base: PathBuf,
    live_box_height: Option<usize>,
    /// Bytes have arrived since a frame was last taken as complete.
    fresh: bool,
}

impl Window {
    fn start() -> Self {
        Self::start_with(None, 24, 100)
    }

    fn start_with(fixture_override: Option<serde_json::Value>, rows: u16, cols: u16) -> Self {
        Self::start_seeded(fixture_override, rows, cols, 0)
    }

    fn start_seeded(
        fixture_override: Option<serde_json::Value>,
        rows: u16,
        cols: u16,
        turns: usize,
    ) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rune-permission-vt-{}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let project = base.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            r#"
[host]
idle_lifetime = "1s"
[ui]
sidebar = true
[providers]
default = "fixture"
[[providers.list]]
name = "fixture"
base_url = "http://127.0.0.1:1"
api = "openai"
auth = { header = "Authorization", prefix = "Bearer " }
[[providers.list.models]]
id = "test"
thinking = false
context = 32768
"#,
        )
        .unwrap();
        // Its commands are POSIX: Git Bash runs them on Windows.
        runeengine::testing::posix_shell_for_tests(&home);
        let body: String = (0..380)
            .map(|i| format!("// source-row-{i:03}\n"))
            .collect();
        let command = format!(
            "printf '⠋'; sleep 0.3; printf '\\r\\033[2K'; sleep 0.3; printf '⠙'; sleep 0.3; printf '\\r\\033[2K'; sleep 0.3; printf '⠹'; sleep 0.3; printf '\\r\\033[2KSPINNER-DONE\\n'; # {}\n# PERMISSION-END",
            "permission-padding ".repeat(900)
        );
        let call = |id: &str, name: &str, args: serde_json::Value| json!({"chunks":[{"tool_call":{"id":id,"name":name,"args":args}}]});
        let fixture = json!({"responses":[
            call("create", "create_file", json!({"action":"create demo", "path":"demo.js", "text":body})),
            call("patch", "patch_file", json!({"action":"patch demo", "path":"demo.js", "old_str":"// source-row-000", "new_str":"// changed-row-000\n// inserted-row"})),
            call("terminal", "terminal_use", json!({"action":"run real spinner", "command":command})),
            {"chunks":[{"text":"Fixture completed."}]}
        ]});
        let fixture = fixture_override.unwrap_or(fixture);
        let script = home.join("fixture.json");
        std::fs::write(&script, fixture.to_string()).unwrap();
        let (pty, slave) = platform::open_pty(rows, cols).unwrap();
        let executable = std::env::var_os("RUNE_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_rune").into());
        if turns > 0 {
            use std::io::{BufRead, BufReader, Write};
            let mut host = Command::new(&executable)
                .env("RUNE_HOME", &home)
                .args(["-host", "-script"])
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut input = host.stdin.take().unwrap();
            let mut output = BufReader::new(host.stdout.take().unwrap());
            let mut until = |kind: &str, id: Option<u64>| loop {
                let mut line = String::new();
                assert!(output.read_line(&mut line).unwrap() > 0);
                let event: serde_json::Value = serde_json::from_str(&line).unwrap();
                if event["t"] == kind && id.is_none_or(|id| event["id"].as_u64() == Some(id)) {
                    return event;
                }
            };
            until("ready", None);
            writeln!(
                input,
                "{}",
                json!({"cmd":"session.open","id":1,"project":project})
            )
            .unwrap();
            let opened = until("ack", Some(1));
            assert_eq!(opened["ok"], true, "{opened}");
            let session = opened["data"]["session"].as_i64().unwrap();
            writeln!(
                input,
                "{}",
                json!({"cmd":"session.mode","id":2,"session":session,"mode":"full"})
            )
            .unwrap();
            assert_eq!(until("ack", Some(2))["ok"], true);
            for turn in 1..=turns {
                writeln!(input, "{}", json!({"cmd":"prompt","id":10+turn,"session":session,"text":format!("edit turn {turn}")})).unwrap();
                until("done", None);
            }
            writeln!(input, "{}", json!({"cmd":"shutdown","id":999})).unwrap();
            assert!(host.wait().unwrap().success());
        }
        let mut launch = platform::Launch::new(executable)
            .env("RUNE_HOME", &home)
            .env("TERM", "xterm-256color")
            .arg("-project")
            .arg(&project)
            .arg("-script")
            .arg(&script);
        if turns > 0 {
            launch = launch.args(["-resume", "1"]);
        }
        // On its own terminal, as a terminal emulator starts it.
        let child =
            platform::spawn_on_terminal(&launch, &slave, platform::Placement::default()).unwrap();
        drop(slave);
        Self {
            child,
            pty,
            screen: Screen::new(rows, cols, 10000),
            raw: Vec::new(),
            decoded: 0,
            answered: 0,
            base,
            live_box_height: None,
            fresh: false,
        }
    }

    fn read(&mut self) -> bool {
        let mut buf = [0; 65536];
        let mut quiet = false;
        match self.pty.read_timeout(&mut buf, 30) {
            Ok(Some(n)) if n > 0 => {
                self.raw.extend_from_slice(&buf[..n]);
                self.fresh = true;
            }
            Ok(_) => quiet = true,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => panic!("UI PTY closed: {error}"),
        }
        let complete = match std::str::from_utf8(&self.raw[self.decoded..]) {
            Ok(_) => self.raw.len(),
            Err(error) if error.error_len().is_none() => self.decoded + error.valid_up_to(),
            Err(_) => self.raw.len(),
        };
        self.screen
            .feed(&String::from_utf8_lossy(&self.raw[self.decoded..complete]));
        self.decoded = complete;
        // Answer after the entire preceding frame has been consumed, just as a
        // terminal does. Keep overlap so queries split across reads are found.
        let start = self.answered.saturating_sub(8);
        let fresh = &self.raw[start..];
        let reports = fresh
            .windows(4)
            .enumerate()
            .filter(|(index, bytes)| start + index + 4 > self.answered && *bytes == b"\x1b[5n")
            .count();
        for _ in 0..reports {
            self.send(b"\x1b[0n");
        }
        if fresh
            .windows(9)
            .enumerate()
            .any(|(index, bytes)| start + index + 9 > self.answered && bytes == b"\x1b[?2026$p")
        {
            self.send(b"\x1b[?2026;2$y");
        }
        self.answered = self.raw.len();
        // Windows's ConPTY is the UI's terminal: it answers the UI's status
        // query (`ESC [ 5 n`) itself, so the query never reaches this end. A
        // frame is complete there once its output went quiet.
        let settled =
            quiet && self.fresh && runeengine::platform::OS == runeengine::platform::Os::Windows;
        if reports > 0 || settled {
            self.fresh = false;
        }
        reports > 0 || settled
    }

    fn send(&self, bytes: &[u8]) {
        self.pty.write_all(bytes).unwrap();
    }

    fn until(&mut self, label: &str, test: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let complete_frame = self.read();
            let text = self.screen.text();
            if complete_frame
                && text.contains("run real spinner")
                && !text.contains("Fixture completed.")
                && !text.contains("Refuse")
                && !text.lines().any(|line| line.contains("│ SPINNER-DONE"))
            {
                let lines: Vec<_> = text.lines().collect();
                if let Some(top) = lines.iter().position(|line| line.contains("╭─ Bash")) {
                    if let Some(bottom) = lines
                        .iter()
                        .enumerate()
                        .skip(top + 1)
                        .find_map(|(row, line)| line.contains('╰').then_some(row))
                    {
                        let height = bottom - top + 1;
                        if let Some(previous) = self.live_box_height {
                            assert!(
                                height >= previous,
                                "live terminal box shrunk unexpectedly: {previous} -> {height}\n{text}"
                            );
                        }
                        self.live_box_height = Some(height);
                    }
                }
            }
            if complete_frame && test(&text) {
                return text;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "Rune exited during {label}: {text}"
            );
            assert!(Instant::now() < deadline, "timeout {label}: {text}");
        }
    }

    fn card(text: &str) -> &str {
        let first = text
            .lines()
            .find(|line| {
                line.starts_with('╭')
                    && (line.contains("Create ")
                        || line.contains("Patch ")
                        || line.contains("permission")
                        || line.contains("Bash"))
            })
            .unwrap();
        &text[text.find(first).unwrap()..]
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // Only this isolated fixture's child is collected. Its host notices the
        // last window leave and ends on its configured one-second idle lifetime.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let socket = self.base.join("home/run/session_1.sock");
        let deadline = Instant::now() + Duration::from_secs(3);
        while socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        // A failed fixture can still own a host. Keep its isolated home until
        // the host closes instead of making it reload a missing configuration.
        if !socket.exists() {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
}

#[test]
fn undo_and_context_are_readable_and_navigation_follows_split_reflow() {
    let path = |n: usize| {
        format!("p{n:02}/Engine/src/very-long-directory/another-long-directory/a-really-long-file-name-for-panel-wrap.rs")
    };
    let mut responses = Vec::new();
    for turn in 1..=18 {
        responses.push(json!({"chunks":[{"tool_call":{
            "id":format!("edit-{turn}"),"name":"create_file","args":{
                "action":format!("edit {turn}"),"path":path(turn),"text":format!("version {turn}\n")
            }
        }}]}));
        responses.push(json!({"chunks":[{"text":format!("Audit turn {turn} done.")}]}));
    }
    let mut window = Window::start_seeded(Some(json!({"responses":responses})), 24, 140, 18);
    let done = window.until("panel startup", |text| {
        text.contains("Audit turn 18 done.") && text.contains("test")
    });
    assert!(
        !done.contains("/undo to restore") && !done.contains("ctrl+u"),
        "checkpoint hint remains: {done}"
    );
    // The chosen row of the undo/redo list: its chevron, its file and its
    // button. Without the button a slash-palette row ("› /redo") that shares
    // its screen row with the side panel's "p15/…" was taken for it mid-typing.
    let chosen = |n: usize| {
        move |text: &str| {
            text.lines().any(|line| {
                line.trim_start().starts_with('›')
                    && line.contains(&format!("p{n:02}/"))
                    && (line.contains("[undo]") || line.contains("[redo]"))
            })
        }
    };
    window.send(b"/undo\r");
    let opened = window.until("undo opens on the first file", chosen(1));
    assert!(opened.contains("alt+s show diff"), "{opened}");
    assert!(
        !opened
            .lines()
            .any(|line| line.trim_start().starts_with('›') && line.contains("turn")),
        "no turn in the per-file list: {opened}"
    );
    for n in 2..=18 {
        window.send(b"\x1b[B");
        window.until("undo Down auto-scroll", chosen(n));
    }
    for n in [17, 16, 15] {
        window.send(b"\x1b[A");
        window.until("undo Up auto-scroll", chosen(n));
        for _ in 0..2 {
            window.send(b"\x0f");
            let frame = window.until("split preserves undo choice", chosen(n));
            assert!(!frame.contains('▸'));
        }
    }
    for cols in [80, 169, 110, 140] {
        window.pty.resize(24, cols).unwrap();
        window.screen.resize(24, cols);
        platform::fixture::signal_resize(window.child.id() as u32).unwrap();
        window.until("resize preserves undo choice", chosen(15));
    }
    // Alt+S: the file's diff, numbered; Esc goes back to the list.
    window.send(b"\x1bs");
    let diff = window.until("the diff opens", |text| {
        text.contains("+ version 15") && text.contains("alt+s back to the list")
    });
    assert!(diff.contains("1 │"), "a numbered gutter: {diff}");
    window.send(b"\x1b");
    window.until("back to the list", chosen(15));
    // Enter arms a countdown in the state line; a second Enter undoes.
    window.send(b"\r");
    window.until("countdown", |text| text.contains("Press Enter within"));
    window.send(b"\r");
    window.until("undone, the row stays at zero", |text| {
        text.lines().any(|line| {
            line.trim_start().starts_with('›') && line.contains("p15/") && line.contains("+0 −0")
        })
    });
    let created = window.base.join("project").join(path(15));
    let deadline = Instant::now() + Duration::from_secs(5);
    while created.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !created.exists(),
        "a file the session created goes away on undo"
    );
    window.send(b"\x1b");
    window.until("undo closed", |text| !text.contains("alt+s show diff"));
    // /redo: the undone file, and only it; Enter twice puts it back.
    window.send(b"/redo\r");
    let redo = window.until("redo lists the undone file", chosen(15));
    assert_eq!(
        redo.lines().filter(|line| line.contains("[redo]")).count(),
        1,
        "only the undone file: {redo}"
    );
    window.send(b"\r");
    window.until("redo countdown", |text| {
        text.contains("Press Enter within") && text.contains("to redo")
    });
    window.send(b"\r");
    window.until("redone, the list is empty", |text| {
        text.contains("Nothing to redo: undo a file first")
    });
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "version 15\n");
    window.send(b"\x1b");
    window.until("redo closed", |text| !text.contains("alt+s show diff"));
    // The redo's word leaves the state line in its own time.
    window.until("state line back to the mode", |text| {
        text.contains("[Full]")
    });
    window.send(b"/context\r");
    let context = window.until("context starts at header", |text| {
        text.contains("CONTEXT") && text.contains("SYSTEM PROMPT")
    });
    assert!(
        context.contains("│"),
        "source text has a visible boundary: {context}"
    );
    window.send(b"\x1b[6~");
    let scrolled = window.until("context PageDown", |text| {
        !text.contains("what the model holds besides")
    });
    assert!(!scrolled.contains("ask, or / for commands") || scrolled.contains("[Full]"));
    window.send(b"\x0f");
    window.until("context split toggle", |text| {
        text.contains("pgup/pgdn")
            || text.contains("<instruction>")
            || text.contains("You are Rune")
            || text.contains("working_method")
    });
}

// Kept with the shared real-window fixture; not a second terminal harness.
fn main_terminal_box(text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.starts_with(' ') && line.contains("╭─ Bash"))
        .expect("the transcript contains a terminal box");
    let bottom = lines
        .iter()
        .enumerate()
        .skip(top + 1)
        .find_map(|(row, line)| line.contains('╰').then_some(row))
        .expect("terminal box has a bottom edge");
    // Ignore the sidebar; compare the terminal's own cells only.
    let columns = lines[top]
        .chars()
        .position(|character| character == '╮')
        .unwrap()
        + 1;
    lines[top..=bottom]
        .iter()
        .map(|line| line.chars().take(columns).collect())
        .collect()
}

#[test]
fn ask_allow_and_full_have_the_same_four_row_preview_and_only_a_click_expands_it() {
    let command = "for n in $(seq 1 12); do printf 'preview-%02d\\n' \"$n\"; done; sleep 1; printf 'PREVIEW-FINAL\\n'";
    let fixture = json!({"responses":[
        {"chunks":[{"tool_call":{"id":"terminal","name":"terminal_use","args":{"action":"preview contract","command":command}}}]},
        {"chunks":[{"text":"Preview fixture completed."}]}
    ]});
    let mut completed = Vec::new();
    for mode in ["ask", "full"] {
        let mut window = Window::start_with(Some(fixture.clone()), 34, 140);
        window.until("mode fixture startup", |text| text.contains("test"));
        if mode == "full" {
            window.send(b"/mode full\r");
            window.until("Full selected", |text| {
                text.contains("\u{b7} Full") || text.contains("[Full]")
            });
        }
        window.send(b"exercise preview modes\r");
        if mode == "ask" {
            let permission = window.until("Ask permission reader", |text| {
                text.contains("Refuse") && text.contains("Bash")
            });
            assert!(Window::card(&permission).contains("for n"));
            window.send(b"\r");
        }
        let running = window.until("mode live preview", |text| {
            text.lines().any(|line| line.contains("│ preview-01"))
                && text.contains("lines cut")
                && !text.contains("Refuse")
        });
        let live = main_terminal_box(&running);
        assert_eq!(live.len(), 9, "{mode} live preview must have one command row, four output rows, footer and borders: {live:?}");
        assert!(
            live[7].contains("lines cut"),
            "footer is BELOW all output: {live:?}"
        );
        assert!(!live
            .iter()
            .any(|line| line.contains("rows hidden") || line.contains("wheel here")));
        let done = window.until("mode completed preview", |text| {
            text.contains("Preview fixture completed.")
        });
        let compact = main_terminal_box(&done);
        assert_eq!(
            compact.len(),
            live.len(),
            "completion must not change the folded size: {mode}"
        );
        assert!(compact[7].contains("lines cut"));
        completed.push(compact);
        // Click the footer in its actual frame, without a slash /verbose toggle.
        let row = done
            .lines()
            .position(|line| line.contains("lines cut"))
            .unwrap() as u16
            + 1;
        window.send(format!("\x1b[<0;20;{row}M\x1b[<0;20;{row}m").as_bytes());
        let expanded = window.until("explicit Open All", |text| {
            text.contains("preview-01")
                && !main_terminal_box(text)
                    .iter()
                    .any(|line| line.contains("lines cut"))
        });
        assert!(main_terminal_box(&expanded).len() > live.len());
    }
    assert_eq!(
        completed[0], completed[1],
        "the same command must yield identical non-open boxes regardless of permission path"
    );
}

#[test]
fn real_permission_viewports_and_terminal_redraws_keep_one_frame() {
    let mut window = Window::start();
    window.until("first paint", |text| text.contains("test"));
    window.send(b"exercise permission UI\r");
    let create = window.until("create permission", |text| {
        text.contains("Create demo.js +380")
    });
    let card = Window::card(&create);
    assert!(
        card.contains("Allow once") && card.contains("Refuse") && card.contains("rows below"),
        "{card}"
    );
    assert_eq!(card.matches('╭').count(), 1);
    assert!(!card.contains("diff") && !card.contains("lines cut"));
    window.send(b"\x1b[6~");
    window.until("create permission scroll", |text| {
        text.contains("Create demo.js") && text.contains("rows above")
    });
    window.send(b"\r");
    let patch = window.until("patch permission", |text| {
        text.contains("Patch demo.js -1 +2")
    });
    assert_eq!(Window::card(&patch).matches('╭').count(), 1);
    assert!(!Window::card(&patch).contains("diff"));
    window.send(b"\r");
    let terminal = window.until("long command permission", |text| {
        text.contains("permission-padding")
            && text.contains("Refuse")
            && text.contains("rows below")
    });
    assert!(terminal.lines().count() <= 24, "{terminal}");
    assert!(
        Window::card(&terminal).contains("Allow once")
            || Window::card(&terminal).contains("Full access once")
    );
    // Wheel down many rows over the card's own viewport (one mouse gesture each).
    for _ in 0..110 {
        window.send(b"\x1b[<65;30;17M");
    }
    let end = window.until("all command rows reachable", |text| {
        text.contains("PERMISSION-END")
    });
    assert!(
        end.contains("Refuse") && end.contains("rows above"),
        "{end}"
    );
    window.send(b"\r");
    let spinner = window.until("live spinner redraw", |text| {
        text.lines().any(|line| line.contains("│ ⠙"))
    });
    assert!(
        !spinner.lines().any(|line| line.contains("│ ⠋")),
        "spinner frames accumulated: {spinner}"
    );
    let done = window.until("terminal result", |text| {
        text.contains("SPINNER-DONE") && text.contains("Fixture completed.")
    });
    assert!(
        !done
            .lines()
            .any(|line| line.contains("│ ⠙") || line.contains("│ ⠹")),
        "{done}"
    );
}
