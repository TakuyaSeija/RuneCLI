//! Processes on Unix: how a command Rune starts is launched and placed, how
//! its tree is stopped, how this process becomes a newer build. What differs
//! between Unix kernels — confining a command, finding a tree's members, a
//! process's start, this program's own file — is the OS's (`os::…`).

use std::borrow::Cow;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};

use super::super::os;
use super::super::{Launch, Placement};
use super::pty::PtySlave;

/// The `std` command `launch` describes, confined as `placement` says where
/// confining means starting another program (macOS wraps it in its sandbox;
/// Linux confines in the child, [`in_child`]), carrying the tree's mark.
fn command_for(launch: &Launch, placement: &Placement<'_>) -> io::Result<Command> {
    if launch.verbatim_tail.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a command line written out verbatim is a Windows notion; Unix takes arguments",
        ));
    }
    let launch: Cow<'_, Launch> = os::confined_launch(launch, placement)?;
    let mut command = Command::new(&launch.program);
    command.args(&launch.args);
    if let Some(dir) = &launch.dir {
        command.current_dir(dir);
    }
    if launch.clear_env {
        command.env_clear();
    }
    for (key, value) in &launch.env {
        command.env(key, value);
    }
    if let Some(mark) = placement.tree {
        command.env(TREE_VAR, mark);
    }
    Ok(command)
}

/// What must happen in the child — a session of its own, the terminal
/// claimed, the confinement applied — runs there between `fork` and `exec`,
/// syscalls only.
fn in_child(
    command: &mut Command,
    terminal: bool,
    placement: &Placement<'_>,
) -> io::Result<os::Watch> {
    // Held by the command itself, so what the child applies is open there
    // whatever the caller drops before it spawns.
    let (mut steps, watch) = os::ChildSteps::new(placement)?;
    unsafe {
        command.pre_exec(move || {
            // First: no lock of Rune's stays taken by a command still
            // starting (`kept.rs`).
            super::kept::close_in_child();
            leave_session()?;
            // The terminal is the child's stdin by now (std sets up stdio
            // before running this): it becomes its controlling terminal, so
            // the command leads the terminal's foreground group.
            if terminal {
                os::claim_terminal()?;
            }
            steps.run()
        });
    }
    Ok(watch)
}

/// Start `launch` on `terminal` — its stdin, stdout, stderr and controlling
/// terminal — leading a session of its own (out of reach of the terminal's
/// `Ctrl+C`, a SIGINT to the whole foreground group, and stoppable as one
/// group: [`terminate_tree`]), under `placement`.
pub fn spawn_on_terminal(
    launch: &Launch,
    terminal: &PtySlave,
    placement: Placement<'_>,
) -> io::Result<Process> {
    let mut command = command_for(launch, &placement)?;
    command
        .stdin(Stdio::from(terminal.0.try_clone()?))
        .stdout(Stdio::from(terminal.0.try_clone()?))
        .stderr(Stdio::from(terminal.0.try_clone()?));
    let watch = in_child(&mut command, true, &placement)?;
    let child = command.spawn()?;
    watch.started(child.id() as i32);
    Ok(Process(child))
}

/// Start `launch` with pipes for its stdin, stdout and stderr — a server Rune
/// speaks to — leading a session of its own, under `placement`. Stopped when
/// dropped. Called inside the async runtime its pipes are read on.
pub fn spawn_piped(launch: &Launch, placement: Placement<'_>) -> io::Result<PipedProcess> {
    let mut command = command_for(launch, &placement)?;
    let watch = in_child(&mut command, false, &placement)?;
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn()?;
    if let Some(root) = child.id() {
        watch.started(root as i32);
    }
    Ok(PipedProcess(child))
}

/// Start `launch` on its own: no terminal, its stdin and stdout to nothing,
/// its stderr to `log` — a session host, which must outlive the window that
/// started it and the terminal that window runs in: left in the window's
/// process group it would take the terminal's `Ctrl+C` and die mid-tool.
pub fn spawn_detached(launch: &Launch, log: std::fs::File) -> io::Result<Process> {
    let placement = Placement::default();
    let mut command = command_for(launch, &placement)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    let watch = in_child(&mut command, false, &placement)?;
    let child = command.spawn()?;
    watch.started(child.id() as i32);
    Ok(Process(child))
}

/// A process Rune started and watches through its handle.
pub struct Process(std::process::Child);

impl Process {
    pub fn id(&self) -> i32 {
        self.0.id() as i32
    }

    /// Its exit code once it ended (a shell's number, [`exit_code`]), or
    /// `None` while it runs.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(exit_code))
    }

    /// Wait for it to end; its exit code.
    pub fn wait(&mut self) -> io::Result<i32> {
        self.0.wait().map(exit_code)
    }

    /// Kill this process alone — what it started is [`terminate_tree`]'s.
    pub fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }

    /// Watched by pid from here on, never through this handle ([`reap`],
    /// [`group_alive`], [`terminate_tree`]): a task outlives the program that
    /// started it when Rune upgrades in place. Nothing is stopped or waited
    /// for here.
    pub fn into_pid(self) -> i32 {
        self.id()
    }
}

/// A server process Rune speaks to over pipes ([`spawn_piped`]).
pub struct PipedProcess(tokio::process::Child);

/// Rune's end of a server's stdin.
pub struct PipeWriter(tokio::process::ChildStdin);

/// Rune's end of a server's stdout or stderr.
pub struct PipeReader(Reading);

enum Reading {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

impl PipedProcess {
    /// Its pid while it runs.
    pub fn id(&self) -> Option<i32> {
        self.0.id().map(|pid| pid as i32)
    }

    pub fn take_stdin(&mut self) -> Option<PipeWriter> {
        self.0.stdin.take().map(PipeWriter)
    }

    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.0
            .stdout
            .take()
            .map(|out| PipeReader(Reading::Out(out)))
    }

    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.0
            .stderr
            .take()
            .map(|err| PipeReader(Reading::Err(err)))
    }

    /// Stop it and what it started: its group asked, then insisted after
    /// `grace_ms`, then the process itself killed.
    pub fn stop(&mut self, grace_ms: u64) {
        if let Some(pid) = self.id() {
            terminate_group(pid, grace_ms);
        }
        let _ = self.0.start_kill();
    }

    /// Wait for it to end; its exit code.
    pub async fn wait(&mut self) -> io::Result<i32> {
        self.0.wait().await.map(exit_code)
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
        match &mut self.0 {
            Reading::Out(out) => std::pin::Pin::new(out).poll_read(cx, buf),
            Reading::Err(err) => std::pin::Pin::new(err).poll_read(cx, buf),
        }
    }
}

/// Leave the caller's process group and start a new session, so the child ends
/// up in a session of its own with no controlling terminal.
///
/// # Safety
/// Async-signal-safe only: one syscall, no allocation, no locks. Run in the
/// child between `fork` and `exec`.
fn leave_session() -> io::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The exit code a shell would report for `status`: its own code, or
/// `128 + signal` for one a signal ended — the same number [`reap`] gives.
pub fn exit_code(status: ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    }
}

/// Become the program `command` names, in this process — its pid, its
/// terminal and what it keeps open across the exec stay; only the program
/// changes. `arg0`: the name it is started under, when not its path.
/// Returns only when that could not be done.
pub fn replace_process(command: &mut Command, arg0: Option<&std::ffi::OsStr>) -> io::Error {
    if let Some(name) = arg0 {
        command.arg0(name);
    }
    command.exec()
}

/// Whether this process runs with the machine's highest rights (root).
pub fn running_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// What this process notes about itself first thing in `main`, while it
/// still holds: a broken pipe ends it quietly ([`exit_on_broken_pipe`]), and
/// what the OS must take before anything changes (macOS: the file this
/// program runs from, before a release can put another at its path).
pub fn at_start() {
    exit_on_broken_pipe();
    os::at_start();
}

/// A command-line program is expected to die quietly when its reader goes
/// away: `rune -session | head` prints three lines and stops. Rust ignores
/// `SIGPIPE` at startup, which turns that ordinary shell idiom into a crash
/// report; this puts the default back.
fn exit_on_broken_pipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// The exit code of child `pid` once it has ended, reaping it: `Ok(None)`
/// while it runs (with `wait` false), the shell's `128 + signal` for one a
/// signal ended. By pid rather than through a `Child` handle, because a
/// process this program inherited across `exec` — still its child, the pid
/// unchanged — has no handle in it.
pub fn reap(pid: i32, wait: bool) -> io::Result<Option<i32>> {
    // waitpid(0/-1/negative) selects a group/all children, not this task. A
    // malformed handover or fixture must never steal another child's status.
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one positive child PID",
        ));
    }
    let mut status = 0;
    loop {
        let flags = if wait { 0 } else { libc::WNOHANG };
        let rc = unsafe { libc::waitpid(pid, &mut status, flags) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            return Ok(None);
        }
        return Ok(Some(if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            -1
        }));
    }
}

/// Collect every child of this process that has already ended, waiting for
/// none. Only for a process that has started no child of its own yet: one that
/// `exec`d keeps the children the program before it started, and one that
/// ended unseen before the exec is a zombie nothing else will collect. Taken
/// any later, it would steal an exit status a `Child` handle is waiting for.
pub fn reap_exited() -> usize {
    let mut collected = 0;
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } <= 0 {
            return collected;
        }
        collected += 1;
    }
}

/// Signal an entire process group. `pid` is the group leader — the child we
/// spawned, which leads a group of its own ([`spawn_on_terminal`]).
pub(in crate::platform) fn kill_group(pid: i32, signal: i32) -> io::Result<()> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one positive process-group leader PID",
        ));
    }
    if unsafe { libc::killpg(pid, signal) } != 0 {
        let e = io::Error::last_os_error();
        // ESRCH: already gone. Nothing to do, and not an error worth surfacing.
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

/// Whether a process group still exists.
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything, so this is the cheap way to ask "is it still running?" — including
/// for a group Rune no longer holds a handle to.
pub fn group_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::killpg(pid, 0) == 0 }
}

/// Ask a command tree to stop, then insist. `grace_ms` is how long the polite
/// request gets before `SIGKILL`.
pub fn terminate_group(pid: i32, grace_ms: u64) {
    let _ = kill_group(pid, libc::SIGTERM);
    let step = 20;
    let mut waited = 0;
    while waited < grace_ms {
        if !group_alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(step));
        waited += step;
    }
    let _ = kill_group(pid, libc::SIGKILL);
}

/// The environment variable that marks every process of one command's tree:
/// set on the command ([`Placement::tree`]), inherited by everything it
/// starts — including what leaves its process group or session (`setsid`,
/// `nohup … &`, a daemon's double fork), which a signal to the group never
/// reaches.
pub const TREE_VAR: &str = "RUNE_PROCESS_TREE";

/// A mark no other tree carries: this process, the time, and randomness — so
/// a number reused since, or a tree of another session, never matches.
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

/// Stop a command's whole tree: its process group (`pid` leads it) and every
/// process carrying its tree `mark`, wherever it moved — SIGTERM, then SIGKILL
/// after `grace_ms` for what is left. A tree without a mark (one started by an
/// earlier build) is its group alone, as [`terminate_group`] stops it.
pub fn terminate_tree(pid: i32, mark: Option<&str>, grace_ms: u64) {
    let Some(mark) = mark.filter(|mark| !mark.is_empty()) else {
        terminate_group(pid, grace_ms);
        return;
    };
    let wanted = format!("{TREE_VAR}={mark}");
    let signal_all = |signal: i32| {
        if pid > 0 {
            let _ = kill_group(pid, signal);
        }
        let members = os::tree_members(&wanted);
        for member in &members {
            unsafe { libc::kill(*member, signal) };
        }
        members
    };
    signal_all(libc::SIGTERM);
    let step = 20;
    let mut waited = 0;
    while waited < grace_ms {
        // A zombie lists no environment: only the living count.
        if !group_alive(pid) && os::tree_members(&wanted).is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(step));
        waited += step;
    }
    signal_all(libc::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Launch {
        Launch::new("/bin/sh").args(["-c", script])
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("rune-process-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_task_pid_never_means_a_process_selector() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 17"])
            .spawn()
            .unwrap();
        for pid in [0, -1, -2, i32::MIN] {
            for wait in [false, true] {
                assert_eq!(
                    reap(pid, wait).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput
                );
            }
            assert_eq!(
                kill_group(pid, 0).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!group_alive(pid));
        }
        assert_eq!(
            child.wait().unwrap().code(),
            Some(17),
            "another child's status is untouched"
        );
    }

    #[test]
    fn signalling_a_dead_group_is_not_an_error() {
        // A pid that cannot exist as a group leader.
        assert!(kill_group(i32::MAX - 1, libc::SIGTERM).is_ok());
    }

    /// A command ended by a signal reports what a shell would: 128 + signal.
    #[test]
    fn an_exit_code_is_the_shells() {
        let code = |script: &str| {
            exit_code(
                std::process::Command::new("/bin/sh")
                    .args(["-c", script])
                    .status()
                    .unwrap(),
            )
        };
        assert_eq!(code("exit 3"), 3);
        assert_eq!(code("kill -9 $$"), 128 + libc::SIGKILL);
    }

    /// A detached process leads a session of its own, out of reach of the
    /// group that started it.
    #[test]
    fn a_detached_process_leads_its_own_session() {
        let dir = scratch("detached");
        let log = std::fs::File::create(dir.join("log")).unwrap();
        let mut child = spawn_detached(&sh("sleep 30"), log).unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { libc::getsid(pid) } != pid {
            assert!(
                std::time::Instant::now() < deadline,
                "never its own session"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(group_alive(pid));
        terminate_group(pid, 200);
        assert_eq!(child.wait().unwrap(), 128 + libc::SIGTERM);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A launch carries what it says: its arguments, its folder, its
    /// environment kept or cleared, and its tree's mark — and a command line
    /// written out verbatim, a Windows notion, is refused.
    #[test]
    fn a_launch_carries_its_arguments_folder_environment_and_mark() {
        let dir = scratch("launch");
        let out = dir.join("out");
        let script = format!(
            "printf '%s|%s|%s|%s|%s' \"$0\" \"$(pwd -P)\" \"${{RUNE_TEST_SET-unset}}\" \"${{HOME-gone}}\" \"${{{TREE_VAR}-none}}\" > '{}'",
            out.display()
        );
        let run = |launch: Launch, placement: Placement<'_>| {
            // The master stays open while it runs: closing it hangs the
            // terminal up, and its program with it.
            let (pty, slave) = super::super::pty::open_pty(24, 80).unwrap();
            let mut child = spawn_on_terminal(&launch, &slave, placement).unwrap();
            drop(slave);
            assert_eq!(child.wait().unwrap(), 0);
            drop(pty);
            std::fs::read_to_string(&out).unwrap()
        };
        let base = || {
            sh(&script)
                .arg("first arg")
                .dir(&dir)
                .env("RUNE_TEST_SET", "set")
        };
        let kept = run(base(), Placement::default());
        assert!(
            kept.starts_with(&format!("first arg|{}|set|", dir.display())),
            "{kept}"
        );
        assert!(
            !kept.contains("|gone|") && kept.ends_with("|none"),
            "the environment was kept, no mark: {kept}"
        );
        let cleared = run(
            base().clear_env().env("RUNE_TEST_SET", "set"),
            Placement::default(),
        );
        assert_eq!(
            cleared,
            format!("first arg|{}|set|gone|none", dir.display())
        );
        let marked = run(
            base(),
            Placement {
                tree: Some("mark-1"),
                ..Placement::default()
            },
        );
        assert!(marked.ends_with("|mark-1"), "{marked}");
        let refused = spawn_detached(
            &Launch::new("/bin/sh").verbatim_tail("/C x"),
            std::fs::File::create(dir.join("log")).unwrap(),
        );
        assert_eq!(
            refused.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A piped process is spoken to and heard, and stops with its group.
    #[test]
    fn a_piped_process_is_heard_and_stopped() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut child = spawn_piped(
                &sh("read line; echo \"got $line\"; echo oops >&2; sleep 30"),
                Placement::default(),
            )
            .unwrap();
            let mut stdin = child.take_stdin().unwrap();
            let mut stdout = child.take_stdout().unwrap();
            let mut stderr = child.take_stderr().unwrap();
            stdin.write_all(b"hello\n").await.unwrap();
            stdin.flush().await.unwrap();
            let mut heard = [0u8; 9];
            stdout.read_exact(&mut heard).await.unwrap();
            assert_eq!(&heard, b"got hello");
            let mut said = [0u8; 4];
            stderr.read_exact(&mut said).await.unwrap();
            assert_eq!(&said, b"oops");
            let pid = child.id().unwrap();
            assert!(group_alive(pid));
            child.stop(200);
            assert_ne!(child.wait().await.unwrap(), 0);
            // What the group left behind is collected by whoever adopts it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while group_alive(pid) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "its group never went with it"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
    }
}
