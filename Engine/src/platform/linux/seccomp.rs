//! seccomp: the one door Landlock does not close for an MCP server — creating
//! a Unix socket, which can reach a daemon the confinement never sees.

use std::io;

/// Refuse the creation of pathname and abstract Unix sockets to the calling
/// thread and what it runs (seccomp). Called after fork; only stack data and
/// syscalls are used.
pub(super) fn refuse_unix_sockets() -> io::Result<()> {
    #[cfg(target_arch = "x86_64")]
    let architecture = 0xc000003eu32;
    #[cfg(target_arch = "aarch64")]
    let architecture = 0xc00000b7u32;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    let mut filter = [
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 4,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 1,
            jf: 0,
            k: architecture,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x80000000,
        },
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 1,
            jf: 0,
            k: libc::SYS_socket as u32,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: 3,
            k: libc::SYS_socketpair as u32,
        },
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 16,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: 1,
            k: libc::AF_UNIX as u32,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x00050000 | libc::EPERM as u32,
        },
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x7fff0000,
        },
    ];
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Launch, LinkFreeDir, Placement};
    use std::path::Path;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-seccomp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// An MCP server the person did not allow sockets is refused one; a
    /// process allowed them creates one.
    #[test]
    fn unix_sockets_are_refused_unless_allowed() {
        let python = ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists());
        let Some(python) = python else {
            eprintln!("note: no python3 here to open a socket with");
            return;
        };
        // Started as an MCP server is: piped, placed, its own session.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let opens = |allowed: bool| {
            runtime.block_on(async {
                let mut child = crate::platform::spawn_piped(
                    &Launch::new(python).args([
                        "-c",
                        "import socket,sys\ntry:\n socket.socket(socket.AF_UNIX)\nexcept PermissionError:\n sys.exit(9)",
                    ]),
                    Placement {
                        unix_sockets: allowed,
                        ..Placement::default()
                    },
                )
                .unwrap();
                Some(child.wait().await.unwrap())
            })
        };
        assert_eq!(opens(true), Some(0));
        assert_eq!(opens(false), Some(9));
    }

    /// Bug of 0.6.7: package files were read with `openat2`, which a kernel
    /// before 5.6 and many container profiles refuse (ENOSYS / EPERM) — every
    /// skill, Rune's own help included, then failed as if a link were found.
    /// Here `openat2` is refused for this thread, as such a system refuses it,
    /// and the read still works.
    #[test]
    fn package_files_read_where_the_kernel_refuses_openat2() {
        let dir = scratch("openat2");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/SKILL.md"), "skill").unwrap();
        let root = dir.join("pkg");
        let read = std::thread::spawn(move || {
            refuse_for_this_thread(libc::SYS_openat2 as u32, libc::ENOSYS as u32);
            let refused = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    libc::AT_FDCWD,
                    c"/".as_ptr(),
                    0usize,
                    0usize,
                )
            };
            assert_eq!(refused, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSYS)
            );
            let mut text = String::new();
            use std::io::Read;
            LinkFreeDir::open(&root)
                .and_then(|dir| dir.open_file(Path::new("SKILL.md")))
                .and_then(|mut file| file.read_to_string(&mut text))
                .map(|_| text)
        })
        .join()
        .unwrap();
        assert_eq!(read.unwrap(), "skill");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Install a seccomp filter on the calling thread alone (no TSYNC) that
    /// fails `syscall` with `errno`: how an old kernel or a container profile
    /// answers it. The thread may then end; nothing else is affected.
    fn refuse_for_this_thread(syscall: u32, errno: u32) {
        #[cfg(target_arch = "x86_64")]
        let architecture = 0xc000003eu32;
        #[cfg(target_arch = "aarch64")]
        let architecture = 0xc00000b7u32;
        let mut filter = [
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 4,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 1,
                jf: 0,
                k: architecture,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0x7fff0000,
            },
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: syscall,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0x00050000 | errno,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: 0x7fff0000,
            },
        ];
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_mut_ptr(),
        };
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(libc::prctl(libc::PR_SET_SECCOMP, 2, &program), 0);
        }
    }
}
