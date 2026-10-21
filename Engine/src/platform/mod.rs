//! Everything Rune asks of the operating system, behind one contract.
//!
//! Outside this folder the workspace is portable Rust: no `libc`, no
//! `std::os`, no `extern "C"`, no `cfg` on an OS, no reading `/proc` — the
//! engine, the CLI and both crates' tests alike. The scanner test
//! `boundary::os_specific_code_lives_only_in_platform` holds that, so a leak
//! fails the gate instead of waiting for a port to find it; the CLI does not
//! even depend on `libc`. Each OS answers the contract below — same names,
//! same meaning: `unix/` the part Linux and macOS share, `linux/` and
//! `macos/` what only one of them has, `windows/` all of Windows. Where an
//! OS cannot give a fact, the contract says so by a value (`INPUT_WAIT_DETECTION`,
//! `REPLACES_IN_PLACE`), never by a stub that pretends.
//!
//! # The contract
//!
//! ```text
//! The machine
//!   OS: Os                                       which table a decision above reads
//!                                                (sandbox paths, protected prefixes, …)
//!
//! Terminals of commands (pty)
//!   open_pty(rows, cols) -> (Pty, PtySlave)      both ends private to Rune (never inherited)
//!   Pty: read_timeout, write_all, size, resize
//!        input_wait() -> Option<InputWait>       its program blocked reading it (a fact, never parsed)
//!        line_mode() -> Option<bool>             reads a line at Enter
//!        keep_across_exec(bool), into_handed() -> Handed, adopt(Handed)
//!   INPUT_WAIT_DETECTION                         how much input_wait can see: Every (Linux),
//!                                                HiddenOnly (macOS), Never (Windows)
//!
//! Processes
//!   Launch { program, args, verbatim_tail, dir, clear_env, env }
//!   Placement { confinement, tree }              where it runs and under what
//!   spawn_on_terminal(&Launch, &PtySlave, Placement) -> Process
//!                                                its own session, the terminal its own
//!   spawn_piped(&Launch, Placement) -> PipedProcess
//!                                                a server spoken to over pipes, stopped when dropped
//!   spawn_detached(&Launch, log) -> Process      outlives this process and its terminal
//!   Process: id, try_wait, wait, kill, into_pid  exit codes as a shell reports them
//!   PipedProcess: id, take_stdin/stdout/stderr, stop, wait
//!   reap(pid, wait), reap_exited()               by pid: a child inherited across exec has no handle
//!   group_alive(pid), terminate_group(pid, grace)
//!   TREE_VAR, new_tree_mark(), terminate_tree(pid, mark, grace)
//!                                                the group and every process carrying the mark
//!   boot_time(), process_started(pid)            a process's identity: pid + start
//!   REPLACES_IN_PLACE                            whether a process can become a newer build (exec)
//!   replace_process(&mut Command, arg0) -> Error become a newer build in place
//!   own_program() -> Handed, program_at(Handed)  the way back to the build that handed over
//!   running_program() -> FileIdentity            the file this process was started from
//!   installed_program() -> PathBuf               where a release puts the next build
//!   program_launch(path) -> Launch, program_command(path) -> Command
//!                                                start Rune again as this process was started:
//!                                                through the same loader and its options when
//!                                                it was (Linux `ld-linux … ./rune`)
//!   exit_code(ExitStatus) -> i32
//!   at_start()                                   first in main: a broken pipe ends quietly;
//!                                                what the OS must note before anything changes
//!
//! Confinement
//!   Confinement::build(Grants) -> Result<Confinement, Unconfined>
//!   Confinement: kind(), abi(), local_services() applied to each placed command
//!                gaps() -> [Gap]                 what it leaves reachable beyond its grants
//!                                                (Windows: what every user may read and
//!                                                ProgramData's create right; Landlock,
//!                                                Seatbelt: nothing)
//!                revoke_paths([PathBuf]), revoke_session_grants()
//!   revoke_sandbox_paths([PathBuf])              take back a session's or one-call grants
//!   Grants.local_services                        Unix sockets reach programs outside the
//!                                                confinement: open when granted; otherwise a
//!                                                command's own (`Grants.sockets`, or served by
//!                                                its own tree) connect freely and the rest go
//!                                                to `Grants.ask_service` at the moment they
//!                                                connect (Linux: seccomp user notification;
//!                                                macOS: Seatbelt, own folders only; Windows:
//!                                                another account or restricted token opens no
//!                                                other's pipe)
//!   release_supervision(root, within) -> Option<Supervision>
//!   Supervision: resume(), into_handed() -> Handed
//!   adopt_supervision(Handed, root, tree, SupervisedAs)
//!                                                a command's connection supervisor across a
//!                                                live upgrade (`exec`): let go of once no answer
//!                                                is pending, its calls waiting in the kernel, and
//!                                                answered by the next build as its session
//!                                                confines now (Linux; elsewhere nothing
//!                                                supervises: `None`)
//!   unconfinable(program) -> Option<String>      why a command `program` starts cannot run
//!                                                confined (Windows without the sandbox account:
//!                                                an MSYS2/Cygwin runtime's shared memory;
//!                                                Linux, macOS, Windows sandbox account: never)
//!   DEVICES                                      what a terminal needs, granted with every confinement
//!   Grants.ledger                                where what a confinement leaves on disk is
//!                                                recorded before it is written (Windows's ACL
//!                                                entries; Landlock and Seatbelt leave nothing)
//!   recorded_grants(ledger) -> [RecordedGrant]   what is recorded — the same on every OS
//!   forget_grants(ledger) -> Forgotten           take every recorded entry back
//!   set_up_sandbox_account(), prepare_sandbox_account() (interactive start, once),
//!   remove_sandbox_account(), setup_sandbox_account(path) (elevated helper),
//!   run_sandbox_runner(pipe)                     Windows's local sandbox account and runner;
//!                                                a confinement only finds the account, never
//!                                                sets it up, and says why when not in use
//!
//! Files
//!   canonical(path) -> PathBuf                   every link resolved, spelt one way
//!                                                (Windows: without `\\?\` where it reads the same)
//!   windows_name_trouble(name)                   why Windows reads a name as something else
//!                                                (a device, a stream, a trimmed name) — pure
//!   FileIdentity::of_path / of_entry / of_file   which file, not which spelling
//!   same_file(a, b)                              an atomic replace is a new file
//!   file_mode(&Metadata), set_file_mode(path)    what an undo puts back
//!   open_private(path, Private) -> File          a file this user's alone, or none at all
//!   is_private(path)                             nobody else may reach it (mode bits / ACL)
//!   restrict_to_owner(path)                      tighten a file or folder anyone else may reach
//!   replace(from, to)                            `to` replaced by `from` in one step, on disk
//!   sync_dir(dir)                                the names a folder holds on disk (no-op on Windows)
//!   writable(path), free_space(path)
//!   remove_entry(path)                           whatever is there, a link as a link
//!   open_read_nonblocking(path)                  never waits on a FIFO's peer
//!   LinkFreeDir::open(path).open_file(relative)  no symbolic link anywhere on the way
//!   Handed, hand_on(File), take_handed_file(Handed, expected)
//!                                                kept open across an exec, named by number
//!   keep_from_children(&File) -> KeptFromChildren  no child holds it, not even before its
//!                                                exec (a lock); dropped before the file
//!                                                closes
//!
//! Local sockets
//!   LocalListener: bind(path), accept, incoming, try_clone, wake
//!   LocalStream: connect(path), pair, try_clone, set_read/write_timeout, shutdown,
//!                peer_is_same_user() — the kernel's word, not the peer's; Read + Write
//!   is_socket(path)
//!
//! This process's terminal and input
//!   ConsoleMode: current(), raw(), apply(Typed)  Typed::Keep never loses a key typed meanwhile
//!   console_mouse(on)                            the mouse the program's or the console's
//!                                                (Windows: the console's input mode; a VT
//!                                                terminal is told by what the program writes)
//!   set_clipboard(text) -> bool                  where the OS lets a terminal program set it
//!                                                itself (Windows, macOS); else the terminal
//!                                                is asked (OSC 52)
//!   console_size(), watch_console_resize(), console_resized(), take_console_resized()
//!   InputWaker, wait_stdin(timeout, waker), read_stdin(buf), read_stdin_timeout(buf, ms)
//!
//! The person's machine
//!   home_dir()                                   absolute, or none
//!   running_elevated()                           root, or an Administrator's full rights
//!   system_caches()                              the system's folders for this user's tool caches
//!   default_shell()                              the shell commands run in when none was chosen
//!   find_program(name) -> Option<PathBuf>        what a bare program name runs, on this
//!                                                process's PATH (Windows: with PATHEXT)
//!   local_time(epoch_secs) -> LocalTime, local_date()
//!
//! Tests only (fixture, `test-support`)
//!   process_state(pid), running(pid), parent_of(pid), program_of(pid), symlink,
//!   handed_number, kept_across_exec, failing_file, signal_resize, share_with_others,
//!   set_modified, posix_shell (Windows: Git Bash where installed)
//! ```
//!
//! Every descriptor Rune opens is private to it unless handed on by name:
//! a live upgrade `exec`s a newer build in place, and a background command
//! keeps running through it only because its terminal is handed on and
//! adopted (`session::background`).
//!
//! # What is not here
//!
//! Security policy that names an OS's paths stays with its owner: which
//! system folders a confined command may read and which caches it may write
//! (`sandbox::Policy`), which paths a command may never reach
//! (`perm::reach::Forbidden`, `perm::denylist`). They are decisions, not
//! mechanisms; each owner keeps one table per OS, chosen by [`OS`] — never by
//! a `cfg` — so every table is compiled and tested on every machine.

#[cfg(test)]
mod boundary;
mod ledger;

pub use ledger::{recorded_grants, Forgotten, RecordedGrant};

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The operating systems Rune runs on. Decisions above the platform that
/// name an OS's paths keep one table per OS and read the one in force
/// through [`OS`]: a runtime value, so every table is compiled and tested
/// everywhere — a `cfg` would hide the others from every machine but one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

/// How much [`Pty::input_wait`] can see on this OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputWaitDetection {
    /// Every wait: the kernel tells which syscall a process is blocked in
    /// and which descriptor it reads (Linux).
    Every,
    /// Only a hidden entry read by line — the mode a password prompt sets —
    /// while every process of the terminal's foreground group waits; a
    /// visible prompt cannot be told from a program waiting on something
    /// else (macOS: the kernel gives a thread's state, not what it waits on).
    HiddenOnly,
    /// None: no public interface tells that a console program waits to read
    /// (Windows).
    Never,
}

/// A program blocked reading its terminal, as the platform tells it — a fact
/// read from the terminal and the kernel, never from what the program printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputWait {
    /// The terminal does not echo what is typed: a password or passphrase.
    pub hidden: bool,
    /// The terminal hands the program a line at Enter (canonical mode);
    /// otherwise every key as it is pressed.
    pub line: bool,
}

/// A program to start: what runs, with which arguments, where, and with
/// what environment. A description, not a process — the platform starts it
/// ([`spawn_on_terminal`], [`spawn_piped`], [`spawn_detached`]) the way this
/// OS starts one placed as asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Text that ends the command line exactly as written, after `args` —
    /// how `cmd.exe /D /S /C "…"` takes a command, which no argument quoting
    /// can spell. Only Windows has command lines; elsewhere a launch carrying
    /// one is refused.
    pub verbatim_tail: Option<OsString>,
    /// The folder it starts in; this process's when `None`.
    pub dir: Option<PathBuf>,
    /// Start from an empty environment instead of this process's.
    pub clear_env: bool,
    /// Set in order, after clearing.
    pub env: Vec<(OsString, OsString)>,
}

impl Launch {
    pub fn new(program: impl Into<PathBuf>) -> Launch {
        Launch {
            program: program.into(),
            ..Launch::default()
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Launch {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Launch
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    pub fn verbatim_tail(mut self, tail: impl AsRef<OsStr>) -> Launch {
        self.verbatim_tail = Some(tail.as_ref().to_owned());
        self
    }

    pub fn dir(mut self, dir: impl Into<PathBuf>) -> Launch {
        self.dir = Some(dir.into());
        self
    }

    pub fn clear_env(mut self) -> Launch {
        self.clear_env = true;
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Launch {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }
}

/// A confined command reaching, through a Unix socket, a program its
/// confinement does not hold: what is put to the person
/// ([`Grants::ask_service`](Grants)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCall {
    /// The socket as the person reads it: its real path, or `@name` for an
    /// abstract one.
    pub address: String,
    /// The program serving it, when the system names it (`dockerd`).
    pub server: Option<String>,
}

/// The person's answer to a [`ServiceCall`], asked on the thread that
/// supervises the command while it waits in `connect`: `true` lets this one
/// connection through. Called once per connection; a lasting answer is the
/// caller's to remember.
pub type ServiceAsk = std::sync::Arc<dyn Fn(&ServiceCall) -> bool + Send + Sync>;

/// Where a command Rune starts runs, and under what. Every placed command
/// leads a session (Windows: a process group and a job) of its own — out of
/// reach of the terminal's `Ctrl+C` and stoppable as one tree.
#[derive(Clone, Copy, Default)]
pub struct Placement<'a> {
    /// The confinement it runs under; `None` for none. Whether it may reach
    /// local services through Unix sockets is the confinement's own grant
    /// (`Grants::local_services`), never the placement's: a confined command
    /// cannot be placed with a door its confinement did not open.
    pub confinement: Option<&'a Confinement>,
    /// The mark every process of its tree carries ([`new_tree_mark`]), so a
    /// stop reaches what left its group too ([`terminate_tree`]).
    pub tree: Option<&'a str>,
}

/// How a file that must be this user's alone is opened ([`open_private`]):
/// made private, or not made at all — never first readable by others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Private {
    /// Read and written, made when missing, its content kept: a lock.
    Lock,
    /// Written, and only when it does not exist yet: a key.
    New,
    /// Appended to, made when missing: a log.
    Append,
}

/// Why Windows would read the path component `name` as something other than
/// a file of that name — a device (`CON`, `NUL`, `COM1`, whatever ending
/// follows), a stream of a file (`file:stream`), a name it trims (`name.`
/// and `name ` are `name`), a character it refuses — or `None` for a plain
/// name. Pure, the same answer on every machine, so the rules are tested
/// everywhere; it matters where Windows reads the path (`OS`).
pub fn windows_name_trouble(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some(
            "Windows drops a trailing dot or space from a name, so it would name another file",
        );
    }
    if name.contains(':') {
        return Some("on Windows a colon in a name names a hidden stream of a file, not a file");
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*') || (c as u32) < 32)
    {
        return Some("Windows does not allow this character in a name");
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "0" | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
        })
    };
    let device = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT");
    device.then_some("on Windows this name is a device (CON, NUL, COM1 …), not a file")
}

/// A moment on the person's own clock, in their time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1–12.
    pub month: u32,
    /// 1–31.
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// [`program_launch`] as a command of the standard library's, for the
/// places that `exec` or wait on a Rune build themselves.
pub fn program_command(program: &Path) -> std::process::Command {
    let launch = program_launch(program);
    let mut command = std::process::Command::new(&launch.program);
    command.args(&launch.args);
    command
}

/// Today's civil date in the user's own time zone, as `YYYY-MM-DD`.
pub fn local_date() -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let today = local_time(i64::try_from(now.as_secs()).ok()?)?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        today.year, today.month, today.day
    ))
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use os::{
    boot_time, installed_program, own_program, process_started, program_at, program_launch,
    running_program, set_clipboard, system_caches, Confinement, Grants, Unconfined, DEVICES,
    INPUT_WAIT_DETECTION, OS, REPLACES_IN_PLACE,
};

/// What a confinement leaves reachable that its grants do not say, named so
/// a person can act on it (`Confinement::gaps`; a report's escapes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// Stable identifier.
    pub id: &'static str,
    /// What stays reachable.
    pub detail: String,
    /// What the person can do about it.
    pub fix: &'static str,
}

/// How a connection supervisor taken over from the build before answers for
/// its command (`adopt_supervision`).
#[derive(Clone, Copy)]
pub enum SupervisedAs<'a> {
    /// As the command's session confines now.
    Confined(&'a Confinement),
    /// Nothing confines the session now (Full): every connection goes through.
    Unconfined,
    /// No session holds the command any more: its own sockets alone.
    Orphaned,
}

#[cfg(target_os = "linux")]
pub use os::{adopt_supervision, release_supervision, Supervision};

#[cfg(not(target_os = "linux"))]
mod unsupervised;
#[cfg(not(target_os = "linux"))]
pub use unsupervised::{adopt_supervision, release_supervision, Supervision};

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(test, feature = "test-support")
))]
pub use os::fixture;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!(
    "Rune implements Linux, macOS and Windows. Add Engine/src/platform/<os>/ \
     satisfying the contract documented in platform/mod.rs, then extend the cfg here. \
     No stub is provided on purpose: an untested stub that compiles is worse than a \
     build error that tells you exactly what is missing."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_date_is_a_civil_date() {
        let date = super::local_date().unwrap();
        assert_eq!(date.len(), 10);
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
    }

    /// Windows reads devices, streams and trimmed names where a file's name
    /// was meant; a plain name is a plain name.
    #[test]
    fn windows_names_that_mean_something_else_are_told() {
        for trouble in [
            "CON",
            "nul",
            "Nul.txt",
            "com1",
            "LPT9.log",
            "COM\u{b9}",
            "conin$",
            "aux .c",
            ".git.",
            "file ",
            "a:stream",
            "x?y",
            "a<b",
            "tab\t",
        ] {
            assert!(windows_name_trouble(trouble).is_some(), "{trouble:?}");
        }
        for plain in [
            "console",
            "nullable.rs",
            "com",
            "com10",
            "lpt",
            ".git",
            "a.b.c",
            "README",
            ".",
            "..",
            "con-tents",
            "名前",
        ] {
            assert_eq!(windows_name_trouble(plain), None, "{plain:?}");
        }
    }

    /// A launch says what it was told, in order.
    #[test]
    fn a_launch_is_built_as_written() {
        let launch = Launch::new("prog")
            .arg("a")
            .args(["b", "c"])
            .dir("/work")
            .clear_env()
            .env("K", "v")
            .env("K", "w");
        assert_eq!(launch.program, PathBuf::from("prog"));
        assert_eq!(launch.args, ["a", "b", "c"].map(OsString::from));
        assert_eq!(launch.dir, Some(PathBuf::from("/work")));
        assert!(launch.clear_env);
        assert_eq!(
            launch.env,
            vec![("K".into(), "v".into()), ("K".into(), "w".into())]
        );
        assert_eq!(launch.verbatim_tail, None);
        assert!(Placement::default().confinement.is_none());
    }
}
