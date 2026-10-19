//! Windows: the platform contract answered by Win32 — ConPTY consoles for
//! commands, processes in job objects (with a restricted token when
//! confined), ACL entries for what a confined command may reach, named pipes
//! met at a path for local sockets, and the console's virtual-terminal modes
//! for the window's own terminal. Windows 10 1809 and later.

mod account;
mod clipboard;
mod confine;
mod console;
mod files;
mod home;
mod pipe;
#[cfg(test)]
mod probe;
mod process;
pub(in crate::platform) mod pty;
mod runner;
mod security;
mod stdio;
mod time;
mod wide;

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;

use super::{InputWaitDetection, Os};

pub use clipboard::set_clipboard;
pub use confine::{
    forget_grants, revoke_sandbox_paths, unconfinable, Confinement, Grants, Unconfined, DEVICES,
};
pub use console::{
    console_mouse, console_resized, console_size, take_console_resized, watch_console_resize,
    ConsoleMode, Typed,
};
pub use files::{
    canonical, file_mode, free_space, hand_on, is_private, keep_from_children, open_private,
    open_read_nonblocking, remove_entry, replace, restrict_to_owner, same_file, set_file_mode,
    sync_dir, take_handed_file, writable, FileIdentity, Handed, KeptFromChildren, LinkFreeDir,
};
pub use home::{default_shell, home_dir, system_caches};
pub use pipe::{is_socket, LocalListener, LocalStream};
pub use process::{
    at_start, boot_time, exit_code, find_program, group_alive, installed_program, new_tree_mark,
    own_program, process_started, program_at, reap, reap_exited, replace_process, running_elevated,
    running_program, spawn_detached, spawn_on_terminal, spawn_piped, terminate_group,
    terminate_tree, PipeReader, PipeWriter, PipedProcess, Process, TREE_VAR,
};
pub use pty::{open_pty, Pty, PtySlave};
pub use stdio::{read_stdin, read_stdin_timeout, wait_stdin, InputWaker};
pub use time::local_time;

/// Provision the local sandbox account a record at `path` names: what the
/// elevated `-sandbox account-setup <path>` does.
pub fn setup_sandbox_account(path: &std::path::Path) -> Result<(), String> {
    account::setup_account_at(path).map_err(|refusal| refusal.to_string())
}

/// Set this user's sandbox account up (`rune -sandbox setup`), confirming
/// with an administrator when this process may not add accounts.
pub fn set_up_sandbox_account() -> Result<(), String> {
    account::set_up()
}

/// At an interactive start: set the sandbox account up unless it is, or a
/// setup was declined before.
pub fn prepare_sandbox_account() {
    account::set_up_once()
}

/// Remove this user's sandbox account and what Rune keeps for it.
pub fn remove_sandbox_account() -> Result<(), String> {
    account::remove()
}

/// Run the persistent sandbox-account runner connected to `pipe`.
pub fn run_sandbox_runner(pipe: &std::ffi::OsStr) -> Result<(), String> {
    runner::run_server(pipe).map_err(|e| e.to_string())
}

/// This build runs on Windows.
pub const OS: Os = Os::Windows;

/// No public interface tells that a console program waits to read.
pub const INPUT_WAIT_DETECTION: InputWaitDetection = InputWaitDetection::Never;

/// Windows cannot make a running process another program: a newer build is
/// taken at the next start (owner, 2026-10-17).
pub const REPLACES_IN_PLACE: bool = false;
