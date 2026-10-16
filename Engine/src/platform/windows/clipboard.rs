//! The Windows clipboard, set directly: the console window does not take the
//! terminal clipboard sequence (OSC 52), and every program of the person's
//! desktop may set the clipboard itself.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// `CF_UNICODETEXT`: UTF-16 text ending in a NUL.
const UNICODE_TEXT: u32 = 13;

/// Put `text` on the clipboard as Unicode text. `false` when Windows would
/// not open it — another program holding it a moment — or take the text.
pub fn set_clipboard(text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // Another program may hold the clipboard a moment: asked a few times.
    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !opened {
        return false;
    }
    let set = unsafe {
        EmptyClipboard() != 0 && {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if memory.is_null() {
                false
            } else {
                let at = GlobalLock(memory) as *mut u16;
                if at.is_null() {
                    GlobalFree(memory);
                    false
                } else {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), at, units.len());
                    GlobalUnlock(memory);
                    // The clipboard owns the memory once it took it.
                    let taken = !SetClipboardData(UNICODE_TEXT, memory as HANDLE).is_null();
                    if !taken {
                        GlobalFree(memory);
                    }
                    taken
                }
            }
        }
    };
    unsafe { CloseClipboard() };
    set
}
