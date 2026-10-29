//! A reviewed HTTP(S) URL is opened by the user's desktop, never by cmd.
use std::{ffi::OsStr, io};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW};

pub fn open_browser(url: &crate::platform::BrowserUrl) -> io::Result<()> {
    let operation = super::wide::wide(OsStr::new("open"))?;
    let target = super::wide::wide(OsStr::new(url.as_str()))?;
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.lpVerb = operation.as_ptr();
    info.lpFile = target.as_ptr();
    info.nShow = 1;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
