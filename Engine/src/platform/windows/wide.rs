//! What every Windows file here needs: UTF-16 strings for the `W`
//! functions, Win32 results as `io::Result`, handles that close themselves,
//! and memory Windows allocated that it must free.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR};

/// `text` as a NUL-terminated UTF-16 string; an error when it holds a NUL,
/// which would end it early and name something else.
pub(super) fn wide(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut out: Vec<u16> = text.encode_wide().collect();
    if out.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a name holds a NUL character",
        ));
    }
    out.push(0);
    Ok(out)
}

/// How long a path Windows takes as written: past it (`MAX_PATH`, less the
/// room `CreateDirectoryW` keeps), only a verbatim path (`\\?\`) is
/// accepted. The standard library draws the same line.
const LONGEST_PLAIN: usize = 248;

/// `path` as a NUL-terminated UTF-16 string a `W` function opens whatever its
/// length: a path of `LONGEST_PLAIN` units or more is made whole
/// (`GetFullPathNameW` — `.`, `..` and `/` resolved) and written verbatim,
/// `\\?\C:\…` or `\\?\UNC\server\share\…`, as the standard library
/// writes it — a project deep in folders was refused with "path not found"
/// by Rune's own calls while every `std::fs` call reached it (CI,
/// 2026-10-17: no checkpoint could be taken).
pub(super) fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let plain = wide(path.as_os_str())?;
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let device: Vec<u16> = r"\\.\".encode_utf16().collect();
    if plain.len() - 1 < LONGEST_PLAIN || plain.starts_with(&verbatim) || plain.starts_with(&device)
    {
        return Ok(plain);
    }
    let full = full_path(&plain)?;
    let unc: Vec<u16> = r"\\".encode_utf16().collect();
    let mut out: Vec<u16> = if full.starts_with(&unc) {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(full[2..].iter().copied())
            .collect()
    } else {
        verbatim.into_iter().chain(full).collect()
    };
    out.push(0);
    Ok(out)
}

/// `name` (NUL-terminated) as Windows makes it whole: absolute, `.` and `..`
/// gone, every separator a backslash. Without the NUL.
fn full_path(name: &[u16]) -> io::Result<Vec<u16>> {
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    let mut buffer = vec![0u16; 512];
    loop {
        let n = unsafe {
            GetFullPathNameW(
                name.as_ptr(),
                buffer.len() as u32,
                buffer.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buffer.len() {
            buffer.truncate(n);
            return Ok(buffer);
        }
        buffer.resize(n + 1, 0);
    }
}

/// The text of a UTF-16 buffer up to its first NUL.
pub(super) fn from_wide(buffer: &[u16]) -> OsString {
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..end])
}

/// A `BOOL` result: the call's error when it failed.
pub(super) fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A `WIN32_ERROR` result (the security functions return their error).
pub(super) fn win32(code: WIN32_ERROR) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// A handle a function returned, owned — or its error when it returned none
/// (functions disagree on null or `INVALID_HANDLE_VALUE`; both are none).
pub(super) fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// Memory Windows allocated with `LocalAlloc` for us (a security
/// descriptor, an ACL), freed when dropped.
pub(super) struct Local<T>(pub(super) *mut T);

impl<T> Local<T> {
    pub(super) fn null() -> Local<T> {
        Local(std::ptr::null_mut())
    }
}

impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0.cast()) };
        }
    }
}
