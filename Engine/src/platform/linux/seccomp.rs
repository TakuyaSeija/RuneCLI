//! seccomp: the door Landlock does not close — creating a Unix socket, which
//! can reach a program already running outside the confinement (a container
//! engine, the user's service manager) that then acts on the command's
//! behalf. Landlock mediates opening a file, never `connect()` on a socket's
//! path (before `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`), so a confinement that does
//! not grant local services refuses the socket itself.
//!
//! What stays allowed, and why: `socketpair(AF_UNIX)` makes two connected
//! ends inside the process and reaches nobody — Python's event loop and
//! Node's child pipes are built on it. What is refused besides: `io_uring`,
//! whose queued operations (a `socket`, a `connect`) run without passing this
//! filter, refused as a kernel without it answers (`ENOSYS`) so programs fall
//! back to plain calls; and on x86_64 the x32 call numbers, another spelling
//! of the same calls.

use std::io;

/// One step of a classic BPF program; `jt`/`jf` are resolved to offsets by
/// [`assemble`].
#[derive(Clone, Copy)]
enum To {
    Next,
    Label(Label),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    Allow,
    Absent,
    Domain,
    Foreign,
    Notify,
    Address,
}

enum Op {
    Load(u32),
    JumpIfEqual(u32, To, To),
    JumpIfAtLeast(u32, To, To),
    Return(u32),
    Mark(Label),
}

const LOAD_WORD: u16 = 0x20;
const JUMP_EQUAL: u16 = 0x15;
const JUMP_AT_LEAST: u16 = 0x35;
const RETURN: u16 = 0x06;

const KILL_PROCESS: u32 = 0x8000_0000;
const ALLOW: u32 = 0x7fff_0000;
const ERRNO: u32 = 0x0005_0000;
const USER_NOTIF: u32 = 0x7fc0_0000;

/// Offsets into `struct seccomp_data`.
const NR: u32 = 0;
const ARCH: u32 = 4;
const FIRST_ARGUMENT: u32 = 16;

/// x86_64's x32 call numbers carry this bit.
const X32_BIT: u32 = 0x4000_0000;

#[cfg(target_arch = "x86_64")]
const ARCHITECTURE: Option<u32> = Some(0xc000_003e);
#[cfg(target_arch = "aarch64")]
const ARCHITECTURE: Option<u32> = Some(0xc000_00b7);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ARCHITECTURE: Option<u32> = None;

/// What every filter of Rune's begins with: another architecture's calls
/// end the process (their numbers mean other calls), x32's numbers and
/// `io_uring` are answered as absent; the call's number is then loaded.
fn prelude() -> io::Result<Vec<Op>> {
    let architecture = ARCHITECTURE.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOTSUP))?;
    let mut ops = vec![
        Op::Load(ARCH),
        Op::JumpIfEqual(architecture, To::Next, To::Label(Label::Foreign)),
        Op::Load(NR),
    ];
    if cfg!(target_arch = "x86_64") {
        ops.push(Op::JumpIfAtLeast(
            X32_BIT,
            To::Label(Label::Absent),
            To::Next,
        ));
    }
    for call in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        ops.push(Op::JumpIfEqual(
            call as u32,
            To::Label(Label::Absent),
            To::Next,
        ));
    }
    Ok(ops)
}

/// The refusal: every Unix socket a command would create is refused — for a
/// kernel where Rune cannot supervise its connections (`services`).
fn program() -> io::Result<Vec<libc::sock_filter>> {
    let mut ops = prelude()?;
    ops.extend([
        Op::JumpIfEqual(
            libc::SYS_socket as u32,
            To::Label(Label::Domain),
            To::Label(Label::Allow),
        ),
        Op::Mark(Label::Domain),
        Op::Load(FIRST_ARGUMENT),
        Op::JumpIfEqual(libc::AF_UNIX as u32, To::Next, To::Label(Label::Allow)),
        Op::Return(ERRNO | libc::EPERM as u32),
        Op::Mark(Label::Allow),
        Op::Return(ALLOW),
        Op::Mark(Label::Absent),
        Op::Return(ERRNO | libc::ENOSYS as u32),
        Op::Mark(Label::Foreign),
        Op::Return(KILL_PROCESS),
    ]);
    assemble(&ops)
}

/// The supervision: every `connect`, and every `sendto` that names an
/// address, waits for Rune's supervisor (`services`), which makes the
/// connection itself — a command's own socket at once, another program's
/// only once the person allowed it. Sockets are created freely. A `sendto`
/// without an address (an ordinary send on a connected socket) is not
/// stopped: the filter reads its fifth argument, a pointer, as two words
/// (little-endian on both architectures Rune confines). `sendmsg` and
/// `sendmmsg` carry their address inside the message, which a filter cannot
/// read, so every one of them waits and the supervisor sends it.
fn supervision_program() -> io::Result<Vec<libc::sock_filter>> {
    let address = FIRST_ARGUMENT + 4 * 8;
    let mut ops = prelude()?;
    ops.extend([
        Op::JumpIfEqual(libc::SYS_connect as u32, To::Label(Label::Notify), To::Next),
        Op::JumpIfEqual(libc::SYS_sendmsg as u32, To::Label(Label::Notify), To::Next),
        Op::JumpIfEqual(
            libc::SYS_sendmmsg as u32,
            To::Label(Label::Notify),
            To::Next,
        ),
        Op::JumpIfEqual(
            libc::SYS_sendto as u32,
            To::Label(Label::Address),
            To::Label(Label::Allow),
        ),
        Op::Mark(Label::Address),
        Op::Load(address),
        Op::JumpIfEqual(0, To::Next, To::Label(Label::Notify)),
        Op::Load(address + 4),
        Op::JumpIfEqual(0, To::Label(Label::Allow), To::Label(Label::Notify)),
        Op::Mark(Label::Allow),
        Op::Return(ALLOW),
        Op::Mark(Label::Notify),
        Op::Return(USER_NOTIF),
        Op::Mark(Label::Absent),
        Op::Return(ERRNO | libc::ENOSYS as u32),
        Op::Mark(Label::Foreign),
        Op::Return(KILL_PROCESS),
    ]);
    assemble(&ops)
}

/// Lay `ops` out as BPF, each jump resolved to the instruction after its
/// label.
fn assemble(ops: &[Op]) -> io::Result<Vec<libc::sock_filter>> {
    let mut at = Vec::new();
    let mut index = 0usize;
    for op in ops {
        match op {
            Op::Mark(label) => at.push((*label, index)),
            _ => index += 1,
        }
    }
    let place = |label: Label| {
        at.iter()
            .find(|(l, _)| *l == label)
            .map(|(_, i)| *i)
            .expect("every label is marked")
    };
    let mut out = Vec::new();
    for op in ops {
        let here = out.len();
        let offset = |to: To| -> io::Result<u8> {
            match to {
                To::Next => Ok(0),
                To::Label(label) => u8::try_from(place(label) - here - 1)
                    .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG)),
            }
        };
        let step = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
        match *op {
            Op::Mark(_) => {}
            Op::Load(offset_in_data) => out.push(step(LOAD_WORD, offset_in_data, 0, 0)),
            Op::JumpIfEqual(value, yes, no) => {
                out.push(step(JUMP_EQUAL, value, offset(yes)?, offset(no)?))
            }
            Op::JumpIfAtLeast(value, yes, no) => {
                out.push(step(JUMP_AT_LEAST, value, offset(yes)?, offset(no)?))
            }
            Op::Return(action) => out.push(step(RETURN, action, 0, 0)),
        }
    }
    Ok(out)
}

/// The refusal of Unix sockets, built before `fork` — a child between `fork`
/// and `exec` may not allocate — and installed in the child by
/// [`Refusal::install`].
pub(super) struct Refusal(Vec<libc::sock_filter>);

impl Refusal {
    pub(super) fn of_unix_sockets() -> io::Result<Refusal> {
        program().map(Refusal)
    }

    /// Refuse the creation of pathname and abstract Unix sockets to the
    /// calling thread and what it runs. Called after fork: only syscalls, on
    /// data built before it.
    pub(super) fn install(&self) -> io::Result<()> {
        let program = libc::sock_fprog {
            len: self.0.len() as u16,
            filter: self.0.as_ptr() as *mut libc::sock_filter,
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
            || unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// The supervision filter, built before `fork` and installed in the child
/// by [`Supervised::install`], which hands back the listener the supervisor
/// reads.
pub(super) struct Supervised(Vec<libc::sock_filter>);

impl Supervised {
    pub(super) fn of_connections() -> io::Result<Supervised> {
        supervision_program().map(Supervised)
    }

    /// Install the filter on the calling thread (and what it runs) with a
    /// listener; its descriptor. Called after fork: only syscalls.
    ///
    /// Once the supervisor has taken a call, only a kill ends the wait
    /// (`WAIT_KILLABLE_RECV`, Linux 5.19): a signal that interrupted it would
    /// restart the call, and what the supervisor already sent would be sent
    /// twice. An older kernel refuses the flag, and the wait stays as it was.
    pub(super) fn install(&self) -> io::Result<libc::c_int> {
        let program = libc::sock_fprog {
            len: self.0.len() as u16,
            filter: self.0.as_ptr() as *mut libc::sock_filter,
        };
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let with = |flags: libc::c_ulong| unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                flags,
                &program,
            )
        };
        let mut listener = with(
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER | libc::SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV,
        );
        if listener < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            listener = with(libc::SECCOMP_FILTER_FLAG_NEW_LISTENER);
        }
        if listener < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(listener as libc::c_int)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::LinkFreeDir;
    use std::path::Path;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-seccomp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// What the refusal answers, on a thread of this test alone: a Unix
    /// socket is refused, a connected pair is made, `io_uring` is answered
    /// as a kernel without it answers, and the network's sockets are left
    /// to the network.
    #[test]
    fn a_confinement_without_local_services_refuses_unix_sockets_and_nothing_it_needs() {
        let refusal = Refusal::of_unix_sockets().unwrap();
        let seen = std::thread::spawn(move || {
            refusal.install().unwrap();
            let errno = || io::Error::last_os_error().raw_os_error();
            let unix = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            let unix_errno = errno();
            let datagram = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
            let mut pair = [-1; 2];
            let paired =
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) };
            for fd in pair {
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
            }
            let mut params = [0u8; 120];
            let ring = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) };
            let ring_errno = errno();
            let network = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            if network >= 0 {
                unsafe { libc::close(network) };
            }
            (
                unix, unix_errno, datagram, paired, ring, ring_errno, network,
            )
        })
        .join()
        .unwrap();
        let (unix, unix_errno, datagram, paired, ring, ring_errno, network) = seen;
        assert_eq!((unix, unix_errno), (-1, Some(libc::EPERM)), "a Unix socket");
        assert_eq!(datagram, -1, "a Unix datagram socket");
        assert_eq!(paired, 0, "a connected pair reaches no other program");
        assert_eq!((ring, ring_errno), (-1, Some(libc::ENOSYS)), "io_uring");
        assert!(network >= 0, "the network is not this filter's");
    }

    /// x86_64's x32 numbers are another spelling of the same calls: refused
    /// whole, so `socket` cannot be reached through them.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_x32_spelling_of_a_call_is_refused() {
        let refusal = Refusal::of_unix_sockets().unwrap();
        let (result, errno) = std::thread::spawn(move || {
            refusal.install().unwrap();
            let result = unsafe {
                libc::syscall(
                    X32_BIT as libc::c_long | libc::SYS_socket,
                    libc::AF_UNIX,
                    libc::SOCK_STREAM,
                    0,
                )
            };
            (result, io::Error::last_os_error().raw_os_error())
        })
        .join()
        .unwrap();
        assert_eq!((result, errno), (-1, Some(libc::ENOSYS)));
    }

    /// Every jump of the program lands on an instruction inside it, and
    /// every path through it ends in a return.
    #[test]
    fn the_program_is_well_formed() {
        for program in [program().unwrap(), supervision_program().unwrap()] {
            well_formed(&program);
        }
    }

    fn well_formed(program: &[libc::sock_filter]) {
        for (index, step) in program.iter().enumerate() {
            if step.code == JUMP_EQUAL || step.code == JUMP_AT_LEAST {
                for offset in [step.jt, step.jf] {
                    assert!(index + 1 + (offset as usize) < program.len(), "{index}");
                }
            }
        }
        assert_eq!(program.last().unwrap().code, RETURN);
        let foreign = &program[1 + 1 + program[1].jf as usize];
        assert_eq!(
            (foreign.code, foreign.k),
            (RETURN, KILL_PROCESS),
            "another architecture ends the process"
        );
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
