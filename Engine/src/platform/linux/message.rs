//! A message a confined command hands to `sendmsg`/`sendmmsg`, copied out of
//! its memory once so the supervisor (`services`) sends exactly what it
//! judged: a second thread of the command may rewrite the message, its
//! address or its descriptors while the call waits, and nothing is read from
//! the command again once the copy is made.
//!
//! Descriptors the command passes (`SCM_RIGHTS`) are its numbers, meaningless
//! in Rune: each is taken from the command (`pidfd_getfd`) and the copy names
//! Rune's descriptor for the same open file. Credentials (`SCM_CREDENTIALS`)
//! name the sender, and the sender is now Rune: the kernel accepts only the
//! sending process's own, so they say Rune's process — as the credentials the
//! kernel attaches by itself to whatever Rune sends for the command already do.

use std::mem::size_of;
use std::os::fd::{AsRawFd, OwnedFd};

use super::services::{pidfd_getfd, read_memory, MOST_SENT};

/// The most iovecs one message may have (the kernel's `UIO_MAXIOV`).
const MOST_PARTS: usize = 1024;
/// The most control data copied; the kernel's own allowance (`optmem_max`)
/// is smaller on every kernel Rune knows.
const MOST_CONTROL: usize = 1 << 17;

/// One message as the command wrote it, ready to send from Rune.
pub(super) struct Message {
    /// The address it names; empty when it names none.
    pub name: Vec<u8>,
    pub data: Vec<u8>,
    pub control: Vec<u8>,
    /// The command's passed descriptors, open in Rune until the send.
    _passed: Vec<OwnedFd>,
}

impl Message {
    /// Copy the `msghdr` at `at` in the thread `pid`, whose process is
    /// `process`. A stream socket's data is copied up to [`MOST_SENT`], and a
    /// send of that much reports what it sent, as any short send does; a
    /// larger datagram is refused, as no socket carries one.
    pub(super) fn copy(pid: i32, process: &OwnedFd, at: u64, stream: bool) -> Result<Message, i32> {
        let raw = read_memory(pid, at, size_of::<libc::msghdr>())?;
        // SAFETY: `raw` holds exactly one `msghdr`'s bytes; any bytes are a
        // valid `msghdr` (pointers and lengths, read here as numbers).
        let header: libc::msghdr = unsafe { std::ptr::read_unaligned(raw.as_ptr().cast()) };

        let name_length = header.msg_namelen as usize;
        if name_length > size_of::<libc::sockaddr_storage>() {
            return Err(libc::EINVAL);
        }
        let name = if header.msg_name.is_null() || name_length == 0 {
            Vec::new()
        } else {
            read_memory(pid, header.msg_name as u64, name_length)?
        };

        let parts = header.msg_iovlen as usize;
        if parts > MOST_PARTS {
            return Err(libc::EMSGSIZE);
        }
        let mut data = Vec::new();
        if parts > 0 {
            let listed = read_memory(pid, header.msg_iov as u64, parts * size_of::<libc::iovec>())?;
            let mut room = MOST_SENT as usize;
            for part in listed.chunks_exact(size_of::<libc::iovec>()) {
                // SAFETY: one `iovec`'s bytes, read as numbers.
                let part: libc::iovec = unsafe { std::ptr::read_unaligned(part.as_ptr().cast()) };
                if part.iov_len > room {
                    if !stream {
                        return Err(libc::EMSGSIZE);
                    }
                    data.extend(read_memory(pid, part.iov_base as u64, room)?);
                    break;
                }
                data.extend(read_memory(pid, part.iov_base as u64, part.iov_len)?);
                room -= part.iov_len;
            }
        }

        let control_length = header.msg_controllen as usize;
        if control_length > MOST_CONTROL {
            return Err(libc::ENOBUFS);
        }
        let mut control = if header.msg_control.is_null() || control_length == 0 {
            Vec::new()
        } else {
            read_memory(pid, header.msg_control as u64, control_length)?
        };
        let passed = translate(&mut control, process)?;
        Ok(Message {
            name,
            data,
            control,
            _passed: passed,
        })
    }
}

/// Make the command's control messages Rune's: its passed descriptors become
/// Rune's for the same files, its credentials Rune's process. A malformed
/// list is left for the kernel to refuse; walking it stops where it ends.
fn translate(control: &mut [u8], process: &OwnedFd) -> Result<Vec<OwnedFd>, i32> {
    let head = size_of::<libc::cmsghdr>();
    let align = |n: usize| (n + size_of::<usize>() - 1) & !(size_of::<usize>() - 1);
    let mut passed = Vec::new();
    let mut at = 0;
    while at + head <= control.len() {
        // SAFETY: a `cmsghdr`'s bytes within `control`, read as numbers.
        let entry: libc::cmsghdr =
            unsafe { std::ptr::read_unaligned(control[at..].as_ptr().cast()) };
        let length = entry.cmsg_len as usize;
        if length < head || at + length > control.len() {
            break;
        }
        let body = at + align(head)..at + length;
        if entry.cmsg_level == libc::SOL_SOCKET && entry.cmsg_type == libc::SCM_RIGHTS {
            for number in control[body].chunks_exact_mut(size_of::<libc::c_int>()) {
                let theirs = libc::c_int::from_ne_bytes(number.try_into().unwrap_or([0; 4]));
                let ours = pidfd_getfd(process, theirs).map_err(|_| libc::EBADF)?;
                number.copy_from_slice(&ours.as_raw_fd().to_ne_bytes());
                passed.push(ours);
            }
        } else if entry.cmsg_level == libc::SOL_SOCKET
            && entry.cmsg_type == libc::SCM_CREDENTIALS
            && body.len() >= size_of::<libc::ucred>()
        {
            let pid = control[body.start..].as_mut_ptr().cast::<libc::pid_t>();
            // SAFETY: `ucred.pid` is the first field, inside `body`.
            unsafe { pid.write_unaligned(std::process::id() as libc::pid_t) };
        }
        at += align(length);
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One control message of `level`/`kind` carrying `body`, laid out as
    /// the kernel reads it.
    fn entry(level: i32, kind: i32, body: &[u8]) -> Vec<u8> {
        let space = unsafe { libc::CMSG_SPACE(body.len() as u32) } as usize;
        let mut bytes = vec![0u8; space];
        let header = libc::cmsghdr {
            cmsg_len: unsafe { libc::CMSG_LEN(body.len() as u32) } as _,
            cmsg_level: level,
            cmsg_type: kind,
        };
        unsafe { std::ptr::write_unaligned(bytes.as_mut_ptr().cast(), header) };
        let start = size_of::<libc::cmsghdr>();
        bytes[start..start + body.len()].copy_from_slice(body);
        bytes
    }

    /// A passed descriptor becomes Rune's own for the same file; credentials
    /// name Rune's process; anything else is left as written.
    #[test]
    fn passed_descriptors_and_credentials_become_runes() {
        let me = super::super::services::pidfd_open(std::process::id() as i32).unwrap();
        let file = std::fs::File::open("/proc/self/stat").unwrap();
        let theirs = file.as_raw_fd();
        let credentials = libc::ucred {
            pid: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        let credential_bytes = unsafe {
            std::slice::from_raw_parts(
                (&credentials as *const libc::ucred).cast::<u8>(),
                size_of::<libc::ucred>(),
            )
        };
        let other = entry(libc::SOL_IP, libc::IP_TOS, &[7, 0, 0, 0]);
        let mut control = entry(libc::SOL_SOCKET, libc::SCM_RIGHTS, &theirs.to_ne_bytes());
        control.extend(entry(
            libc::SOL_SOCKET,
            libc::SCM_CREDENTIALS,
            credential_bytes,
        ));
        control.extend(&other);
        let written = control.clone();

        let passed = translate(&mut control, &me).unwrap();
        assert_eq!(passed.len(), 1);
        let body = size_of::<libc::cmsghdr>();
        let ours = libc::c_int::from_ne_bytes(control[body..body + 4].try_into().unwrap());
        assert_eq!(ours, passed[0].as_raw_fd());
        assert_ne!(
            ours, theirs,
            "Rune's own descriptor, not the number written"
        );
        let same = |fd: i32| std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
        assert_eq!(same(ours), same(theirs));

        let second = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
        let pid = libc::pid_t::from_ne_bytes(
            control[second + body..second + body + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(pid, std::process::id() as libc::pid_t);
        assert_eq!(&control[written.len() - other.len()..], &other[..]);
    }

    /// A control list that claims more than it holds is walked no further.
    #[test]
    fn a_malformed_control_list_is_left_to_the_kernel() {
        let me = super::super::services::pidfd_open(std::process::id() as i32).unwrap();
        let mut control = entry(libc::SOL_SOCKET, libc::SCM_RIGHTS, &3i32.to_ne_bytes());
        control[0] = 0xff;
        let written = control.clone();
        assert!(translate(&mut control, &me).unwrap().is_empty());
        assert_eq!(control, written);
    }
}
