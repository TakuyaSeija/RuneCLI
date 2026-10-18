//! Unix — Linux and macOS: the part of the platform contract both kernels
//! answer the same way, POSIX all through. What only one of them has is in
//! its own folder (`platform/linux/`, `platform/macos/`), reached here as
//! `os`: one implementation of what is common, never two copies drifting.

mod console;
mod files;
mod home;
mod process;
pub(in crate::platform) mod pty;
mod socket;
mod stdio;
mod time;

pub use console::{
    console_mouse, console_resized, console_size, take_console_resized, watch_console_resize,
    ConsoleMode, Typed,
};
pub use files::{
    canonical, file_mode, free_space, hand_on, is_private, open_private, open_read_nonblocking,
    remove_entry, replace, restrict_to_owner, same_file, set_file_mode, sync_dir, take_handed_file,
    writable, FileIdentity, Handed, LinkFreeDir,
};
pub use home::{default_shell, find_program, home_dir};
pub use process::{
    at_start, exit_code, group_alive, new_tree_mark, reap, reap_exited, replace_process,
    running_elevated, spawn_detached, spawn_on_terminal, spawn_piped, terminate_group,
    terminate_tree, PipeReader, PipeWriter, PipedProcess, Process, TREE_VAR,
};
pub use pty::{open_pty, Pty, PtySlave};
pub use socket::{is_socket, LocalListener, LocalStream};
pub use stdio::{read_stdin, read_stdin_timeout, wait_stdin, InputWaker};
pub use time::local_time;

/// Landlock and Seatbelt hold every program a command starts alike: none
/// is beyond them.
pub fn unconfinable(_program: &std::path::Path) -> Option<String> {
    None
}

/// Landlock and Seatbelt hold a confinement in the kernel and leave nothing
/// on disk: there is nothing to take back.
pub fn forget_grants(_ledger: &std::path::Path) -> std::io::Result<crate::platform::Forgotten> {
    Ok(crate::platform::Forgotten::default())
}

/// Landlock and Seatbelt hold their rules in the kernel and leave nothing on
/// disk to revoke.
pub fn revoke_sandbox_paths(_session_scratch: &std::path::Path, _paths: &[std::path::PathBuf]) {}

/// Only Windows uses a local sandbox account.
pub fn setup_sandbox_account(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

/// Only Windows uses a local sandbox account.
pub fn remove_sandbox_account(_home: &std::path::Path) -> Result<(), String> {
    Ok(())
}

/// Only Windows uses a sandbox runner process.
pub fn run_sandbox_runner(_pipe: &std::ffi::OsStr) -> Result<(), String> {
    Ok(())
}
