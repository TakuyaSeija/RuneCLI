//! What Linux tells through `/proc`: which processes carry a tree's mark,
//! when the machine started, when a process started, and the file this
//! program runs from.

use std::io;
use std::path::PathBuf;

use crate::platform::{hand_on, FileIdentity, Handed};

/// Every live process whose environment holds `entry` (`NAME=value`) —
/// readable in `/proc/<pid>/environ` for this user's own processes — never
/// this process itself.
pub(in crate::platform) fn tree_members(entry: &str) -> Vec<libc::pid_t> {
    let me = std::process::id() as libc::pid_t;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| *pid != me && *pid > 0)
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|held| held == entry.as_bytes())
            })
        })
        .collect()
}

/// The processes of process group `group`, read from `/proc` (a process's
/// group is field 5 of its `stat`, counted after the parenthesised name,
/// which may hold spaces itself).
pub(super) fn group_members(group: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let rest = &stat[stat.rfind(')')? + 2..];
                    rest.split_whitespace().nth(2)?.parse::<libc::pid_t>().ok()
                })
                == Some(group)
        })
        .collect()
}

/// When this machine started: nothing a process of an earlier boot left can
/// still be in use. `None` when the system does not say.
pub fn boot_time() -> Option<std::time::SystemTime> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let secs: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
}

/// When process `pid` began, in the kernel's ticks since boot (`/proc/<pid>/stat`
/// field 22): with the pid, the process's identity — a number the kernel hands
/// out again later never comes back with the same start. `None` once it is
/// gone.
pub fn process_started(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces and parentheses
    // itself: the fields are counted from the last `)`, where field 3 begins.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// A descriptor on the program this process runs, open across `exec`: the
/// way back to this build for a successor that cannot take the home over —
/// still there once a release has replaced the file it was started from, and
/// on any machine, with no copy of the program kept anywhere.
pub fn own_program() -> io::Result<Handed> {
    hand_on(std::fs::File::open("/proc/self/exe")?)
}

/// The path that runs the program `handed` holds ([`own_program`]), when it
/// is one: a regular file this user may execute.
pub fn program_at(handed: Handed) -> io::Result<PathBuf> {
    let fd = handed.0;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if fd < 0 || unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no previous program was handed on",
        ));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "what was handed on is not a program",
        ));
    }
    Ok(PathBuf::from(format!("/proc/self/fd/{fd}")))
}

/// The file this process was started from, whatever is installed at its
/// path now — compared with [`FileIdentity::of_path`] of the installed path,
/// it tells that a release replaced it.
pub fn running_program() -> io::Result<FileIdentity> {
    FileIdentity::of_path(std::path::Path::new("/proc/self/exe"))
}

/// The path this process's program is installed at — where a release puts
/// the next build — as the kernel names it, without the " (deleted)" it
/// gains once that file was replaced.
pub fn installed_program() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(PathBuf::from(
        exe.as_os_str()
            .to_string_lossy()
            .trim_end_matches(" (deleted)"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{spawn_detached, terminate_group, terminate_tree, Launch, TREE_VAR};

    #[test]
    fn the_running_program_is_the_installed_one_until_replaced() {
        let running = running_program().unwrap();
        let installed = installed_program().unwrap();
        assert_eq!(FileIdentity::of_path(&installed).unwrap(), running);
    }

    #[test]
    fn a_tree_is_stopped_whole_even_what_left_its_group() {
        let dir = std::env::temp_dir().join(format!("rune-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let escaped_file = dir.join("escaped");
        let alive = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat[stat.rfind(')')? + 2..].chars().next())
                .is_some_and(|state| state != 'Z')
        };
        let run = |mark: &str| {
            let script = format!(
                "setsid sleep 300 & echo $! > {}; sleep 300",
                escaped_file.display()
            );
            let log = std::fs::File::create(dir.join("log")).unwrap();
            let child = spawn_detached(
                &Launch::new("/bin/bash")
                    .args(["-c", &script])
                    .env(TREE_VAR, mark),
                log,
            )
            .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let escaped = loop {
                if let Some(pid) = std::fs::read_to_string(&escaped_file)
                    .ok()
                    .and_then(|t| t.trim().parse::<i32>().ok())
                {
                    let detached = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .ok()
                        .is_some_and(|stat| {
                            let Some(end) = stat.rfind(')') else {
                                return false;
                            };
                            let fields: Vec<_> = stat[end + 2..].split_whitespace().collect();
                            fields.get(2).and_then(|v| v.parse::<i32>().ok()) == Some(pid)
                                && fields.get(3).and_then(|v| v.parse::<i32>().ok()) == Some(pid)
                        });
                    if detached {
                        break pid;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "setsid did not detach the child"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let _ = std::fs::remove_file(&escaped_file);
            (child, escaped)
        };
        let bystander = std::process::Command::new("/bin/sleep")
            .arg("300")
            .env(TREE_VAR, "someone-else")
            .spawn()
            .unwrap();

        // The group signal alone: the escaped process lives on.
        let mark = crate::platform::new_tree_mark();
        let (mut child, escaped) = run(&mark);
        terminate_group(child.id(), 200);
        let _ = child.wait();
        assert!(alive(escaped), "setsid really left the group");
        terminate_tree(0, Some(&mark), 500);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!alive(escaped), "the mark reached what the group did not");

        // The tree: both at once.
        let mark = crate::platform::new_tree_mark();
        let (mut child, escaped) = run(&mark);
        terminate_tree(child.id(), Some(&mark), 500);
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!alive(escaped));

        let mut bystander = bystander;
        assert!(
            bystander.try_wait().unwrap().is_none(),
            "another tree is never touched"
        );
        let _ = bystander.kill();
        let _ = bystander.wait();
        assert_ne!(
            crate::platform::new_tree_mark(),
            crate::platform::new_tree_mark()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
