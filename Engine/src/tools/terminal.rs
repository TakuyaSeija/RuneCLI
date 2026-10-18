//! `terminal_use` — run a shell command on the real machine.
//!
//! This is the tool that makes everything else in `perm` and `sandbox` matter.
//! It runs `bash` on a pseudo-terminal, in the project directory, with the
//! kernel sandbox applied to the child and to everything the child spawns.
//!
//! Three properties worth stating explicitly, because each one is a bug if it
//! is ever lost:
//!
//!  * **The child leads its own process group.** Killing a command that spawned
//!    a build kills the build too, not just the shell.
//!  * **Output is streamed raw to the frontend and sanitised for the model.**
//!    A terminal wants the escape codes; a model does not.
//!  * **Streaming is throttled by time, never by byte count.** A command that
//!    prints one byte a second must still appear alive.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{field_bool, field_opt_str, field_str, flat_schema, Outcome, Tool, ToolCtx, ToolDecl};
use crate::error::{Error, Result};
use crate::panic;
use crate::perm::denylist::{self, Risk};
use crate::platform;
use crate::proto::Event;

/// How often live output is pushed to the frontend. Time-based on purpose: a
/// byte-based throttle makes a slow command look hung.
/// After the child exits, wait this long for anything still in flight.
/// Terminal geometry. Wide enough that tools do not wrap their own output.
/// The size a terminal opens at when nobody has said otherwise.
///
/// A headless frontend has no size to offer; a GUI does, and sends `tool.resize`.
/// Anything that draws a box draws it against these numbers until it does.
pub const DEFAULT_ROWS: u16 = 40;
pub const DEFAULT_COLS: u16 = 200;

pub struct TerminalUse(pub std::sync::Arc<crate::limits::Limits>);

impl TerminalUse {
    /// The language commands are written in: the configured shell's.
    fn shell_kind(&self) -> crate::shell::ShellKind {
        crate::shell::ShellKind::of(&shell(&self.0.config.text("terminal.shell")))
    }
}

impl Tool for TerminalUse {
    fn name(&self) -> &'static str {
        "terminal_use"
    }

    fn runs_commands(&self) -> bool {
        true
    }

    fn mutates(&self) -> bool {
        true
    }

    fn declaration(&self) -> ToolDecl {
        ToolDecl {
            name: "terminal_use".into(),
            // Static on purpose: the declarations are the first bytes of every
            // request. The shell and the timeout are status values; a result
            // that hit the timeout says so.
            description: "Run a command on this machine, in the shell named in <status>. Each call starts a new shell, so cd does not carry over: pass path instead. A command still running at the command timeout named in <status> moves to the background and goes on there."
                .into(),
            parameters: flat_schema(
                json!({
                    "command": {
                        "type": "string",
                        "description": "The command line to run."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to run in, relative to the workspace. Default: the workspace root."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Start the command in the background and return at once; a <system_notification> arrives when it ends. Use it for builds, test suites, servers and anything that may outlast the command timeout. Default false."
                    }
                }),
                &["command"],
            ),
        }
    }

    fn risk(&self, args: &Value) -> Risk {
        let cmd = field_opt_str(args, "command").unwrap_or("");
        denylist::classify_for(cmd, self.shell_kind())
    }

    fn reaches(&self, args: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut add = |path: String| {
            if !out.contains(&path) {
                out.push(path);
            }
        };
        // A POSIX shell on Windows has its own `/` (Git Bash: Git's folder),
        // so its `/usr/bin/env` is read there and not on the current drive.
        let shell = shell(&self.0.config.text("terminal.shell"));
        let posix_root = (platform::OS == platform::Os::Windows
            && self.shell_kind() == crate::shell::ShellKind::Posix)
            .then(|| crate::perm::reach::posix_root_of(&shell))
            .flatten();
        if let Some(cmd) = field_opt_str(args, "command") {
            for path in crate::perm::reach::in_command_for(cmd, self.shell_kind()) {
                add(match &posix_root {
                    Some(root) => crate::perm::reach::under_posix_root(&path, root),
                    None => path,
                });
            }
        }
        if let Some(path) = field_opt_str(args, "path").and_then(crate::perm::reach::in_argument) {
            add(path);
        }
        out
    }

    fn local_services(&self, args: &Value) -> bool {
        let cmd = field_opt_str(args, "command").unwrap_or("");
        denylist::reaches_a_service_for(cmd, self.shell_kind())
    }

    fn unconfinable(&self, _args: &Value) -> Option<String> {
        platform::unconfinable(&shell(&self.0.config.text("terminal.shell")))
    }

    fn grant_key(&self, args: &Value) -> String {
        let cmd = field_opt_str(args, "command").unwrap_or("");
        let program = match self.shell_kind() {
            crate::shell::ShellKind::Posix => denylist::program_of(&denylist::tokens(cmd)),
            kind => crate::perm::windows_shells::first_program(cmd, kind),
        };
        program.unwrap_or_else(|| "shell".into())
    }

    fn preview(&self, args: &Value) -> String {
        let cmd = field_opt_str(args, "command").unwrap_or("");
        if cmd.is_empty() {
            "run command".into()
        } else {
            format!("$ {cmd}")
        }
    }

    fn run(&self, ctx: &mut ToolCtx<'_>, args: &Value) -> Result<Outcome> {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let command = field_str(args, "command")?;
        let run_in_bg = field_bool(args, "run_in_background", false);
        let target_dir = match super::field_given_str(args, "path") {
            Some(c) => ctx.jail.resolve(c)?,
            None => ctx.jail.root().to_path_buf(),
        };
        if !target_dir.is_dir() {
            return Err(Error::invalid(format!(
                "directory {} is not a directory",
                ctx.jail.display(&target_dir)
            )));
        }

        let timeout = Duration::from_millis(self.0.command_timeout_ms());
        let run = execute(ctx, command, &target_dir, timeout, run_in_bg)?;

        // The kind, as a label for a frontend: kept beside the call, never in
        // the text the model reads (see `notice`).
        let outcome_status = match &run.ending {
            Ending::Exited | Ending::OutputLimit => None,
            Ending::Cancelled => Some(super::notice::status::CANCELLED),
            Ending::TimedOut { .. } => Some(super::notice::status::TIMED_OUT),
            Ending::Started(_) | Ending::Moved(_) | Ending::Waiting(..) => {
                Some(super::notice::status::BACKGROUNDED)
            }
        };
        let summary = match &run.ending {
            Ending::Started(task) => format!(
                "started in background (task: {}, PID: {})",
                task.task_id, task.pid
            ),
            Ending::Moved(task) => format!(
                "moved to background after {}s (task: {}, PID: {})",
                timeout.as_secs(),
                task.task_id,
                task.pid
            ),
            Ending::Waiting(task, _) => format!(
                "waiting for input in background (task: {}, PID: {})",
                task.task_id, task.pid
            ),
            Ending::Cancelled => "command cancelled".into(),
            Ending::TimedOut { .. } => format!(
                "command timed out after {}s and was killed",
                timeout.as_secs()
            ),
            Ending::OutputLimit => "command output exceeded the configured byte limit".into(),
            Ending::Exited if run.code == 0 => "command ok".into(),
            Ending::Exited => format!("exit {}", run.code),
        };
        let backgrounded = matches!(
            run.ending,
            Ending::Started(_) | Ending::Moved(_) | Ending::Waiting(..)
        );
        let metadata = json!({ "status": outcome_status, "exit_code": run.code, "backgrounded": backgrounded });

        match &run.ending {
            // The launch is its own notice; there is no command output yet.
            Ending::Started(task) => {
                let notice = super::notice::background_started(
                    &task.task_id,
                    task.pid,
                    &task.log,
                    platform::INPUT_WAIT_DETECTION,
                );
                return Ok(Outcome::new(summary, notice).with_detail(metadata));
            }
            // Nothing is known of it yet but that it outlived the timeout: the
            // notice alone. Its output, from the start, is `manage_process`'s
            // first report.
            Ending::Moved(task) => {
                let notice = super::notice::moved_to_background(
                    timeout.as_secs(),
                    &task.task_id,
                    task.pid,
                    &task.log,
                    platform::INPUT_WAIT_DETECTION,
                );
                return Ok(Outcome::new(summary, notice).with_detail(metadata));
            }
            _ => {}
        }

        let mut transcript = run.text;
        if !transcript.ends_with('\n') && !transcript.is_empty() {
            transcript.push('\n');
        }
        let empty = transcript.trim().is_empty();
        // Rune speaks only when it has something to say: how the command was
        // stopped or where it went on, or its exit code when the output cannot
        // say it — it is empty, or the command failed. A command that succeeded
        // and printed its result needs no notice (the model reads the result).
        let notice = match &run.ending {
            // Known to wait for input: the notice, then what it printed, so
            // the question it asks is read where it stands.
            Ending::Waiting(task, wait) => Some(super::notice::waiting_for_input(
                &task.task_id,
                task.pid,
                &task.log,
                wait.hidden,
            )),
            Ending::Cancelled => Some(super::notice::command_stopped_by_user()),
            Ending::TimedOut { why } => {
                Some(super::notice::command_timed_out(timeout.as_secs(), why))
            }
            Ending::OutputLimit => Some(super::notice::command_output_limit(&crate::media::human(
                self.0.config.number("terminal.max_output_size"),
            ))),
            Ending::Exited if empty || run.code != 0 => Some(super::notice::exit_code(run.code)),
            Ending::Exited | Ending::Started(_) | Ending::Moved(_) => None,
        };
        let mut outcome = Outcome::process_output(
            summary,
            [Ok(transcript)],
            metadata,
            &self.0.config.snapshot(),
            "terminal",
            "terminal_use",
            ctx.results,
        );
        outcome.content = super::notice::prepend(notice.as_slice(), &outcome.content)
            .trim_end()
            .to_string();
        if matches!(run.ending, Ending::TimedOut { .. } | Ending::Cancelled) {
            outcome.ok = false;
        }
        Ok(outcome)
    }
}

struct Run {
    code: i32,
    /// Sanitised output, for the model: all of it, or what a command moved to
    /// the background had printed by then.
    text: String,
    ending: Ending,
}

/// How a call to the command ended.
enum Ending {
    /// It ran to its end.
    Exited,
    /// The user stopped it.
    Cancelled,
    /// It printed more than the output limit, and was stopped.
    OutputLimit,
    /// Still running at the timeout and could not be moved to the background,
    /// so it was stopped: `why` it could not move.
    TimedOut { why: String },
    /// Started in the background (`run_in_background`).
    Started(Background),
    /// Still running at the timeout: it goes on in the background.
    Moved(Background),
    /// Blocked reading its terminal — waiting for someone to type — so it
    /// went to the background at once rather than at the timeout, where
    /// `terminal_send_input` can answer it.
    Waiting(Background, platform::InputWait),
}

/// A command that runs on as a background task.
struct Background {
    task_id: String,
    pid: i32,
    log: std::path::PathBuf,
}

fn execute(
    ctx: &mut ToolCtx<'_>,
    command: &str,
    dir: &std::path::Path,
    timeout: Duration,
    run_in_background: bool,
) -> Result<Run> {
    let (rows, cols) = ctx.term_size;
    let (pty, slave) = platform::open_pty(rows, cols)?;
    let pty = Arc::new(pty);

    // Every process this command starts carries its tree's mark, so a stop
    // reaches the ones that left its process group too.
    let tree = platform::new_tree_mark();
    let mut launch =
        crate::shell::launch(&shell(&ctx.limits.config.text("terminal.shell")), command)?.dir(dir);
    if !ctx.limits.config.flag("terminal.inherit_environment") {
        launch = launch.clear_env();
        for key in kept_environment(platform::OS) {
            if let Some(value) = std::env::var_os(key) {
                launch = launch.env(key, value);
            }
        }
    }
    let launch = launch
        .env("TERM", ctx.limits.config.text("terminal.term"))
        // The sandbox grants this directory and not the machine's shared /tmp,
        // so anything that honours TMPDIR keeps working and anything that does
        // not gets a clear permission error instead of silently sharing space.
        .env("TMPDIR", ctx.sandbox.scratch())
        .env("TMP", ctx.sandbox.scratch())
        .env("TEMP", ctx.sandbox.scratch());

    // A background launch has its log before the command starts: a launch
    // that cannot record its output does not run.
    let log = if run_in_background {
        Some(open_log(ctx)?)
    } else {
        None
    };
    // Its own session on its own terminal, which it leads — so a stop
    // reaches the whole tree — under the session's kernel confinement.
    let child = platform::spawn_on_terminal(
        &launch,
        &slave,
        platform::Placement {
            confinement: ctx.sandbox.confinement(),
            tree: Some(&tree),
        },
    )?;
    drop(slave);
    let pid = child.id();

    if let Some((task_id, out_file, file)) = log {
        // Watched by pid from here on, never through this handle: a task
        // outlives the program that started it when Rune upgrades in place.
        child.into_pid();
        let task = to_background(
            ctx,
            command,
            pid,
            &pty,
            (task_id, out_file, file),
            &mut crate::terminal_screen::Screen::log(rows, cols),
            None,
            &tree,
        )?;
        return Ok(Run {
            code: 0,
            text: String::new(),
            ending: Ending::Started(task),
        });
    }

    let mut child_opt = Some(child);

    // Publish the terminal so `tool.input` can type into it while it runs.
    *panic::lock(ctx.input) = Some(Arc::clone(&pty));

    let started = Instant::now();
    let mut buf = [0u8; 8192];
    let mut pending = String::new();
    let mut output_bytes = 0usize;
    let mut output_size = (rows, cols);
    let mut screen = crate::terminal_screen::Screen::new(
        rows,
        cols,
        ctx.limits.config.number("terminal.max_output_size") as usize,
    );
    let mut last_flush = Instant::now();
    let mut exited_at: Option<Instant> = None;
    let mut ending: Option<Ending> = None;
    // Whether it waits for typed input is asked once per quiet spell: when
    // it has printed nothing for `terminal.input_wait_delay`, and again only
    // after it prints something new.
    let mut last_output = Instant::now();
    let mut input_asked = false;
    let mut waiting: Option<platform::InputWait> = None;
    // The start of a character the last read cut in half.
    let mut carry: Vec<u8> = Vec::new();

    loop {
        if ctx.cancelled() && ending.is_none() {
            platform::terminate_tree(
                pid,
                Some(&tree),
                ctx.limits.config.number("terminal.cancel_grace"),
            );
            ending = Some(Ending::Cancelled);
        }
        let timed_out = started.elapsed() > timeout;
        if ending.is_none() && exited_at.is_none() && (timed_out || waiting.is_some()) {
            // Everything printed so far reaches the frontend before the
            // command leaves its box.
            if !pending.is_empty() {
                flush(ctx, &mut pending, output_size);
            }
            let text = screen.text();
            match open_log(ctx).and_then(|log| {
                to_background(ctx, command, pid, &pty, log, &mut screen, waiting, &tree)
            }) {
                Ok(task) => {
                    // Watched by pid from here on, like any background task.
                    if let Some(child) = child_opt.take() {
                        child.into_pid();
                    }
                    return Ok(Run {
                        code: 0,
                        text,
                        ending: match waiting {
                            Some(wait) => Ending::Waiting(task, wait),
                            None => Ending::Moved(task),
                        },
                    });
                }
                // Waiting but unable to move: it stays here, where it can
                // still be typed into, until the timeout decides.
                Err(_) if !timed_out => waiting = None,
                Err(error) => {
                    platform::terminate_tree(
                        pid,
                        Some(&tree),
                        ctx.limits.config.number("terminal.timeout_grace"),
                    );
                    ending = Some(Ending::TimedOut {
                        why: error.to_string(),
                    });
                }
            }
        }

        let wait_ms = if ending.is_some() { 20 } else { 100 };
        match pty.read_timeout(&mut buf, wait_ms) {
            Ok(Some(0)) => break,
            Ok(Some(n)) => {
                let chunk = crate::terminal_screen::decode_utf8(&mut carry, &buf[..n]);
                let current_size = pty.size()?;
                if current_size != output_size {
                    if !pending.is_empty() {
                        flush(ctx, &mut pending, output_size);
                    }
                    screen.resize(current_size.0, current_size.1);
                    output_size = current_size;
                }
                if output_bytes.saturating_add(chunk.len())
                    > ctx.limits.config.number("terminal.max_output_size") as usize
                {
                    if ending.is_none() {
                        ending = Some(Ending::OutputLimit);
                        platform::terminate_tree(
                            pid,
                            Some(&tree),
                            ctx.limits.config.number("terminal.cancel_grace"),
                        );
                    }
                } else {
                    output_bytes = output_bytes.saturating_add(chunk.len());
                    screen.feed(&chunk);
                    pending.push_str(&chunk);
                }
                last_output = Instant::now();
                input_asked = false;
            }
            Ok(None) => {
                if exited_at.is_none() {
                    if let Some(ref mut c) = child_opt {
                        if c.try_wait()?.is_some() {
                            exited_at = Some(Instant::now());
                        }
                    }
                }
                if let Some(t) = exited_at {
                    if t.elapsed() > ctx.limits.config.duration("terminal.drain_after_exit")
                        || ending.is_some()
                    {
                        break;
                    }
                }
                if ending.is_none()
                    && exited_at.is_none()
                    && !input_asked
                    && last_output.elapsed()
                        >= ctx.limits.config.duration("terminal.input_wait_delay")
                {
                    input_asked = true;
                    waiting = pty.input_wait();
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }

        if !pending.is_empty()
            && last_flush.elapsed() >= ctx.limits.config.duration("terminal.flush_interval")
        {
            flush(ctx, &mut pending, output_size);
            last_flush = Instant::now();
        }
        // A descendant can keep the PTY open after the process group is killed
        // (for example by creating a new session). Never let its output extend
        // the foreground command beyond its stop indefinitely.
        if ending.is_some() {
            break;
        }
    }
    if !pending.is_empty() {
        flush(ctx, &mut pending, output_size);
    }

    // Nothing may type into a terminal that has finished.
    *panic::lock(ctx.input) = None;

    let mut child = child_opt.expect("child must be present when not backgrounded");
    if ending.is_some() && child.try_wait()?.is_none() {
        // The shell may have changed its process group. A group signal alone
        // cannot guarantee the direct child has exited before wait().
        let _ = child.kill();
    }
    // Killed by a signal: reported the way a shell does.
    let c = child.wait()?;
    let text = screen.text();

    Ok(Run {
        code: c,
        text,
        ending: ending.unwrap_or(Ending::Exited),
    })
}

/// A new log under the session's results, and the task id it names.
fn open_log(ctx: &ToolCtx<'_>) -> Result<(String, std::path::PathBuf, std::fs::File)> {
    let out_dir = ctx.results.join("terminal_use");
    std::fs::create_dir_all(&out_dir)?;
    let task_id = format!("bg_{}", super::unique_persist_id(&out_dir, ".txt"));
    let path = out_dir.join(format!("{task_id}.txt"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((task_id, path, file))
}

/// Make the running command `pid` a background task — at its launch, when it
/// outlived the timeout, or when it was found `waiting` for typed input — the
/// one way a command becomes one: what its screen let go of goes into the log,
/// what it still shows is the task's screen, and a monitor watches it from
/// here.
#[allow(clippy::too_many_arguments)]
fn to_background(
    ctx: &mut ToolCtx<'_>,
    command: &str,
    pid: i32,
    pty: &Arc<platform::Pty>,
    (task_id, out_file, mut file): (String, std::path::PathBuf, std::fs::File),
    screen: &mut crate::terminal_screen::Screen,
    waiting: Option<platform::InputWait>,
    tree: &str,
) -> Result<Background> {
    use crate::session::background::{BackgroundTaskInfo, BackgroundTaskStatus, Reported, Watch};
    use std::io::Write;
    let finished = screen.drain();
    file.write_all(finished.as_bytes())?;
    let shown = screen.shown();
    let tail = shown.text();
    // Nobody types into a background task, and its terminal must be the
    // monitor's alone when an upgrade hands it on.
    *panic::lock(ctx.input) = None;
    ctx.background.register_started(BackgroundTaskInfo {
        task_id: task_id.clone(),
        pid,
        command: command.to_string(),
        out_file: out_file.clone(),
        started_at: crate::store::now_ms(),
        status: BackgroundTaskStatus::Running,
        surfaced: false,
        answered: false,
        wake_text: None,
        capture_note: None,
        // A command known to wait hands the model what it printed in this
        // call's result, so its first report starts after it; every other
        // command's first report — and every one of a call the model does not
        // read — holds its output from the start.
        reported: (waiting.is_some() && ctx.reader == super::Reader::Model).then(|| Reported {
            log: finished.len() as u64,
            tail: tail.clone(),
        }),
        tail,
        screen: (shown != Default::default()).then_some(shown),
        waiting,
        tree: Some(tree.to_string()),
    });
    if waiting.is_some() {
        match ctx.reader {
            // This call's result says it waits: no wake says it again.
            super::Reader::Model => ctx.background.wait_told(&task_id),
            // A person's own command: the model hears of the wait by a wake.
            super::Reader::Person => ctx.background.tell_waiting(&task_id),
        }
    }
    ctx.background.spawn_monitor(
        task_id.clone(),
        pid,
        file,
        Arc::clone(pty),
        finished.len(),
        Watch::of(&ctx.limits.config),
    );
    Ok(Background {
        task_id,
        pid,
        log: out_file,
    })
}

fn flush(ctx: &mut ToolCtx<'_>, pending: &mut String, (rows, cols): (u16, u16)) {
    let chunk = std::mem::take(pending);
    let event = Event::ToolOutput {
        session: ctx.session,
        id: ctx.call_id.to_string(),
        chunk,
        rows,
        cols,
    };
    (ctx.emit)(event);
}

/// Simulates terminal carriage returns (\r) by overwriting characters in the current line
/// from column 0, properly folding progress bars (e.g. git clone, curl, npm install).
///
/// For legacy plain-text background log rows only. Foreground PTY output and
/// the CLI's live screen use `terminal_screen::Screen`, which also interprets
/// backspace, cursor movement, erase operations and split escape sequences.
pub fn collapse_cr(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_line: Vec<char> = Vec::new();
    let mut cursor = 0;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {
                // Windows CRLF: ignore \r and let \n commit the line.
                continue;
            }
            '\n' => {
                lines.push(current_line.into_iter().collect::<String>());
                current_line = Vec::new();
                cursor = 0;
            }
            '\r' => {
                // Bare CR: progress update rewriting the line from column 0.
                cursor = 0;
            }
            _ => {
                if cursor < current_line.len() {
                    current_line[cursor] = c;
                } else {
                    current_line.push(c);
                }
                cursor += 1;
            }
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line.into_iter().collect::<String>());
    }
    lines
}

/// Remove terminal control sequences so the model reads text, not escape codes.
/// The frontend already received the raw bytes and can render them properly.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: ends at the first byte in @..~
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ends at BEL or ESC \
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // ESC intermediate sequences (e.g. ESC ( B charset selection)
                // end at a final byte in 0x30..0x7e, not at the intermediate.
                Some(n) if ('\x20'..='\x2f').contains(&n) => {
                    for n in chars.by_ref() {
                        if ('\x30'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\x07' => {}
            _ => out.push(c),
        }
    }
    out
}

/// What a command keeps of Rune's environment when it does not inherit all of
/// it (`terminal.inherit_environment` off): what finds programs and names the
/// user, the language and the toolchains — and on Windows what every program
/// needs to start at all (`SystemRoot`: no network without it) and to find
/// the user's folders.
fn kept_environment(os: platform::Os) -> &'static [&'static str] {
    const UNIX: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "SHELL",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
    ];
    const WINDOWS: &[&str] = &[
        "PATH",
        "PATHEXT",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "ComSpec",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_IDENTIFIER",
        "NUMBER_OF_PROCESSORS",
        "USERPROFILE",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        // Not `PSModulePath`: PowerShell 7's, handed to Windows PowerShell,
        // makes it load modules it cannot run; each finds its own unset.
        "CARGO_HOME",
        "RUSTUP_HOME",
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
    ];
    match os {
        platform::Os::Windows => WINDOWS,
        platform::Os::Linux | platform::Os::MacOs => UNIX,
    }
}

/// `terminal.shell`'s value meaning "the shell this machine has" (owner,
/// 2026-10-16: `/bin/bash` is not on every Linux — NixOS and Alpine have none).
pub const AUTO_SHELL: &str = "auto";

/// The shell commands run in: the person's absolute choice, or for
/// [`AUTO_SHELL`] what the platform finds on this machine.
pub fn shell(value: &str) -> std::path::PathBuf {
    if value == AUTO_SHELL {
        platform::default_shell()
    } else {
        std::path::PathBuf::from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_cr_folds_progress_bars() {
        let raw = "Receiving objects:  72% (119/164)\rReceiving objects:  73% (120/164)\rReceiving objects: 100% (164/164), done.\nResolving deltas:   0%\rResolving deltas: 100%, done.\n";
        let lines = collapse_cr(raw);
        assert_eq!(
            lines,
            vec![
                "Receiving objects: 100% (164/164), done.",
                "Resolving deltas: 100%, done."
            ]
        );
        assert_eq!(
            collapse_cr("line 1\r\nline 2\r\n"),
            vec!["line 1", "line 2"]
        );
    }
    use serde_json::json;

    #[test]
    fn ansi_is_stripped_for_the_model() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(
            strip_ansi("\x1b[32m.\x1b(B\x1b[m\x1b[32m.\x1b(B\x1b[m"),
            ".."
        );
        assert_eq!(
            strip_ansi("ok\x1b(B\x1b[m B is program text"),
            "ok B is program text"
        );
        assert_eq!(strip_ansi("\x1b)0text\x1b(B"), "text");
        assert_eq!(collapse_cr(&strip_ansi("a\r\nb")), vec!["a", "b"]);
        assert_eq!(collapse_cr(&strip_ansi("50%\r100%")), vec!["100%"]);
    }

    #[test]
    fn risk_is_the_worst_command_in_the_batch() {
        let t = TerminalUse(std::sync::Arc::new(crate::limits::Limits::default()));
        let args = json!({
            "action": "x",
            "command": "sudo rm -rf /"
        });
        assert!(matches!(t.risk(&args), Risk::Severe { .. }));

        let ok = json!({ "action": "x", "command": "cargo test" });
        assert_eq!(t.risk(&ok), Risk::Normal);
    }

    #[test]
    fn grant_key_is_the_set_of_programs() {
        let t = TerminalUse(std::sync::Arc::new(crate::limits::Limits::default()));
        let args = json!({
            "action": "x",
            "command": "cargo build"
        });
        assert_eq!(t.grant_key(&args), "cargo");

        let npm = json!({
            "action": "x",
            "command": "npm ci"
        });
        assert_eq!(t.grant_key(&npm), "npm");
    }

    #[test]
    fn preview_shows_the_commands_verbatim() {
        let t = TerminalUse(std::sync::Arc::new(crate::limits::Limits::default()));
        let args = json!({ "action": "x", "command": "ls -la" });
        assert_eq!(t.preview(&args), "$ ls -la");
    }

    #[test]
    fn schema_exposes_run_in_background_and_path() {
        let t = TerminalUse(std::sync::Arc::new(crate::limits::Limits::default()));
        let decl = t.declaration();
        let schema_str = decl.parameters.to_string();
        assert!(
            schema_str.contains("run_in_background"),
            "schema must contain run_in_background"
        );
        assert!(schema_str.contains("path"), "schema must contain path");
        assert!(
            !schema_str.contains("timeout_ms"),
            "schema must NOT contain timeout_ms"
        );
    }

    #[test]
    fn atomic_command_validates_successfully() {
        let t = TerminalUse(std::sync::Arc::new(crate::limits::Limits::default()));
        let args = json!({
            "action": "test",
            "command": "ps -ef",
            "path": "src"
        });
        assert!(t.validate(&args).is_ok());
    }
}
