//! This process's standard input on Windows.
//!
//! A console is read record by record (`ReadConsoleInputW`) and turned into
//! the bytes a Unix terminal would send: in raw mode every key arrives as the
//! characters of its VT sequence; a mouse record, while the program has the
//! mouse (`console::MOUSE`), becomes the SGR report a VT terminal sends for
//! it (`ESC [ < b ; x ; y M`, press, release, motion with a button held, the
//! wheel — what `?1002h ?1006h` asks for); everything else — focus, the
//! window's size, a key released — is let go. A console handle is signalled
//! for any record, so a wait reads what is there, never waiting, and says
//! input waits only when those records made bytes: a read that found only
//! records that make none would block the reader with nothing typed. A pipe
//! or file (a frontend speaking to `rune -host`) is read as bytes.

use std::collections::VecDeque;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileType, ReadFile, FILE_TYPE_CHAR, FILE_TYPE_PIPE,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, GetStdHandle,
    ReadConsoleInputW, CONSOLE_SCREEN_BUFFER_INFO, FROM_LEFT_1ST_BUTTON_PRESSED,
    FROM_LEFT_2ND_BUTTON_PRESSED, INPUT_RECORD, KEY_EVENT, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED,
    MOUSE_EVENT, MOUSE_EVENT_RECORD, MOUSE_MOVED, MOUSE_WHEELED, RIGHTMOST_BUTTON_PRESSED,
    RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED, STD_INPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE,
};

use super::wide::{check, owned};

fn stdin() -> HANDLE {
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
}

fn is_console(handle: HANDLE) -> bool {
    let mut mode = 0;
    unsafe { GetFileType(handle) == FILE_TYPE_CHAR && GetConsoleMode(handle, &mut mode) != 0 }
}

/// Wakes a thread waiting on stdin ([`wait_stdin`]) from another thread.
pub struct InputWaker {
    event: OwnedHandle,
}

impl InputWaker {
    pub fn new() -> io::Result<InputWaker> {
        Ok(InputWaker {
            event: owned(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?,
        })
    }

    /// Wake whoever waits, now or at its next wait.
    pub fn wake(&self) {
        unsafe { SetEvent(self.event.as_raw_handle()) };
    }

    fn drain(&self) {
        unsafe { ResetEvent(self.event.as_raw_handle()) };
    }
}

/// What was read from the console but not yet handed to a reader: the bytes
/// its records made, half a surrogate pair, and the mouse buttons held at
/// the last mouse record (a press or a release is told by the change).
struct Typed {
    bytes: VecDeque<u8>,
    high: Option<u16>,
    buttons: u32,
}

static TYPED: Mutex<Typed> = Mutex::new(Typed {
    bytes: VecDeque::new(),
    high: None,
    buttons: 0,
});

/// Where the console's window starts in its buffer: a mouse record names a
/// cell of the buffer, a VT report a cell of the window.
fn window_origin() -> (i32, i32) {
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(super::console::output(), &mut info) } == 0 {
        return (0, 0);
    }
    (i32::from(info.srWindow.Left), i32::from(info.srWindow.Top))
}

/// The bytes the records in `records` make, into `typed`.
fn take_input(records: &[INPUT_RECORD], typed: &mut Typed) {
    let mouse = super::console::MOUSE.load(Ordering::Relaxed);
    for record in records {
        match u32::from(record.EventType) {
            KEY_EVENT => {
                let key = unsafe { record.Event.KeyEvent };
                if key.bKeyDown == 0 {
                    continue;
                }
                let unit = unsafe { key.uChar.UnicodeChar };
                if unit == 0 {
                    continue;
                }
                for _ in 0..key.wRepeatCount.max(1) {
                    take_unit(unit, typed);
                }
            }
            MOUSE_EVENT if mouse => {
                let event = unsafe { record.Event.MouseEvent };
                take_mouse(&event, window_origin(), typed);
            }
            _ => {}
        }
    }
}

fn take_unit(unit: u16, typed: &mut Typed) {
    let mut utf8 = [0u8; 4];
    match (typed.high.take(), unit) {
        (None, 0xD800..=0xDBFF) => typed.high = Some(unit),
        (Some(high), 0xDC00..=0xDFFF) => {
            for c in char::decode_utf16([high, unit]).flatten() {
                typed.bytes.extend(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
        (_, unit) => {
            let c = char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}');
            typed.bytes.extend(c.encode_utf8(&mut utf8).as_bytes());
        }
    }
}

/// A mouse record as the SGR reports (`?1006h`) a VT terminal sends for
/// button-event tracking (`?1002h`): each button pressed or let go, motion
/// while one is held, a notch of the wheel; Shift, Alt and Ctrl added as
/// xterm adds them. Motion with no button held is not reported.
fn take_mouse(event: &MOUSE_EVENT_RECORD, (left, top): (i32, i32), typed: &mut Typed) {
    let x = i32::from(event.dwMousePosition.X) - left + 1;
    let y = i32::from(event.dwMousePosition.Y) - top + 1;
    let state = event.dwControlKeyState;
    let mut modifiers = 0;
    if state & SHIFT_PRESSED != 0 {
        modifiers += 4;
    }
    if state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        modifiers += 8;
    }
    if state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
        modifiers += 16;
    }
    let mut report = |code: u32, press: bool| {
        if x >= 1 && y >= 1 {
            let end = if press { 'M' } else { 'm' };
            typed
                .bytes
                .extend(format!("\x1b[<{};{x};{y}{end}", code + modifiers).as_bytes());
        }
    };
    // The buttons as SGR numbers them: left 0, middle 1, right 2.
    const BUTTONS: [(u32, u32); 3] = [
        (FROM_LEFT_1ST_BUTTON_PRESSED, 0),
        (FROM_LEFT_2ND_BUTTON_PRESSED, 1),
        (RIGHTMOST_BUTTON_PRESSED, 2),
    ];
    let held = event.dwButtonState
        & (FROM_LEFT_1ST_BUTTON_PRESSED | FROM_LEFT_2ND_BUTTON_PRESSED | RIGHTMOST_BUTTON_PRESSED);
    if event.dwEventFlags & MOUSE_WHEELED != 0 {
        // The high word is the wheel's turn: forward (up) above zero.
        let turn = (event.dwButtonState >> 16) as u16 as i16;
        if turn != 0 {
            report(if turn > 0 { 64 } else { 65 }, true);
        }
        return;
    }
    if event.dwEventFlags & MOUSE_MOVED != 0 && held == typed.buttons {
        if let Some((_, code)) = BUTTONS.iter().find(|(bit, _)| held & bit != 0) {
            report(code + 32, true);
        }
        return;
    }
    let before = std::mem::replace(&mut typed.buttons, held);
    for (bit, code) in BUTTONS {
        match (before & bit != 0, held & bit != 0) {
            (false, true) => report(code, true),
            (true, false) => report(code, false),
            _ => {}
        }
    }
}

/// Read every record the console holds now — never waiting for one — into
/// [`TYPED`].
fn drain_console(console: HANDLE) {
    let mut records: [INPUT_RECORD; 64] = unsafe { std::mem::zeroed() };
    loop {
        let mut waiting = 0u32;
        if unsafe { GetNumberOfConsoleInputEvents(console, &mut waiting) } == 0 || waiting == 0 {
            return;
        }
        let mut read = 0u32;
        if unsafe { ReadConsoleInputW(console, records.as_mut_ptr(), waiting.min(64), &mut read) }
            == 0
            || read == 0
        {
            return;
        }
        take_input(&records[..read as usize], &mut crate::panic::lock(&TYPED));
    }
}

/// Wait until stdin has something to say (bytes, or its end), `timeout`
/// passes (`None`: no limit) or `waker` is woken. `true` when stdin can be
/// read without waiting; a wake is used up by the wait it ends.
pub fn wait_stdin(timeout: Option<Duration>, waker: Option<&InputWaker>) -> bool {
    if !crate::panic::lock(&TYPED).bytes.is_empty() {
        return true;
    }
    let handle = stdin();
    let deadline = timeout.map(|t| Instant::now() + t);
    let left = || {
        deadline.map_or(INFINITE, |d| {
            d.saturating_duration_since(Instant::now())
                .as_millis()
                .min(u128::from(INFINITE - 1)) as u32
        })
    };
    if is_console(handle) {
        loop {
            let mut handles = vec![handle];
            if let Some(waker) = waker {
                handles.push(waker.event.as_raw_handle());
            }
            let waited = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, left())
            };
            if waited == WAIT_OBJECT_0 + 1 {
                if let Some(waker) = waker {
                    waker.drain();
                }
                return false;
            }
            if waited != WAIT_OBJECT_0 {
                return false;
            }
            drain_console(handle);
            if !crate::panic::lock(&TYPED).bytes.is_empty() {
                return true;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return false;
            }
        }
    }
    // A pipe or a file: bytes, or its end, are what a read finds.
    loop {
        if waker.is_some_and(|w| unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(w.event.as_raw_handle(), 0)
        } == WAIT_OBJECT_0)
        {
            if let Some(waker) = waker {
                waker.drain();
            }
            return false;
        }
        if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
            return true;
        }
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || available > 0 {
            // Its end, or bytes.
            return true;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Read what stdin holds into `buf`: `Ok(0)` at its end.
pub fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    let handle = stdin();
    if is_console(handle) {
        loop {
            {
                let mut typed = crate::panic::lock(&TYPED);
                if !typed.bytes.is_empty() {
                    let n = buf.len().min(typed.bytes.len());
                    for (slot, byte) in buf.iter_mut().zip(typed.bytes.drain(..n)) {
                        *slot = byte;
                    }
                    return Ok(n);
                }
            }
            let mut records: [INPUT_RECORD; 64] = unsafe { std::mem::zeroed() };
            let mut count = 0u32;
            check(unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), 64, &mut count) })?;
            take_input(&records[..count as usize], &mut crate::panic::lock(&TYPED));
        }
    }
    let mut read = 0u32;
    let len = buf.len().min(u32::MAX as usize) as u32;
    if unsafe {
        ReadFile(
            handle,
            buf.as_mut_ptr(),
            len,
            &mut read,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            return Ok(0);
        }
        return Err(error);
    }
    Ok(read as usize)
}

/// Wait up to `timeout_ms` for stdin, then read what is there — the way a
/// `-host` reads the commands a frontend writes it.
///
/// `Ok(None)`     nothing arrived in the window
/// `Ok(Some(0))`  stdin ended
/// `Ok(Some(n))`  `n` bytes
pub fn read_stdin_timeout(buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);
    if !wait_stdin(Some(timeout), None) {
        return Ok(None);
    }
    let handle = stdin();
    if unsafe { GetFileType(handle) } == FILE_TYPE_PIPE && !is_console(handle) {
        let mut available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Ok(Some(0));
        }
        let take = (available as usize).min(buf.len()).max(1);
        return read_stdin(&mut buf[..take]).map(Some);
    }
    read_stdin(buf).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Console::{
        COORD, INPUT_RECORD_0, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
    };

    fn typed() -> Typed {
        Typed {
            bytes: VecDeque::new(),
            high: None,
            buttons: 0,
        }
    }

    fn text(typed: Typed) -> String {
        String::from_utf8(typed.bytes.into_iter().collect()).unwrap()
    }

    fn key(down: bool, unit: u16, repeat: u16) -> INPUT_RECORD {
        INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: i32::from(down),
                    wRepeatCount: repeat,
                    wVirtualKeyCode: 0,
                    wVirtualScanCode: 0,
                    uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                    dwControlKeyState: 0,
                },
            },
        }
    }

    fn mouse(x: i16, y: i16, buttons: u32, flags: u32, keys: u32) -> MOUSE_EVENT_RECORD {
        MOUSE_EVENT_RECORD {
            dwMousePosition: COORD { X: x, Y: y },
            dwButtonState: buttons,
            dwControlKeyState: keys,
            dwEventFlags: flags,
        }
    }

    /// Keys pressed become their characters, a pair of surrogates one
    /// character, repeats repeated; keys released and modifiers alone are
    /// nothing.
    #[test]
    fn keys_become_their_characters() {
        let mut typed = typed();
        let records = [
            key(true, u16::from(b'\x1b'), 1),
            key(true, u16::from(b'['), 1),
            key(true, u16::from(b'A'), 1),
            key(false, u16::from(b'A'), 1),
            key(true, 0, 1),
            key(true, 0xD83D, 1),
            key(true, 0xDE00, 1),
            key(true, u16::from(b'x'), 3),
        ];
        take_input(&records, &mut typed);
        assert_eq!(text(typed), "\x1b[A\u{1F600}xxx");
    }

    /// A click, a drag and a release become the SGR reports a VT terminal
    /// sends; cells counted from the window's corner, from 1.
    #[test]
    fn a_click_and_a_drag_are_reported_as_a_vt_terminal_reports_them() {
        let mut typed = typed();
        let origin = (0, 100);
        let left = FROM_LEFT_1ST_BUTTON_PRESSED;
        take_mouse(&mouse(4, 105, left, 0, 0), origin, &mut typed);
        take_mouse(&mouse(6, 105, left, MOUSE_MOVED, 0), origin, &mut typed);
        take_mouse(&mouse(7, 105, 0, MOUSE_MOVED, 0), origin, &mut typed);
        take_mouse(&mouse(7, 105, 0, 0, 0), origin, &mut typed);
        // Motion with nothing held is not reported.
        take_mouse(&mouse(9, 109, 0, MOUSE_MOVED, 0), origin, &mut typed);
        take_mouse(
            &mouse(2, 101, RIGHTMOST_BUTTON_PRESSED, 0, LEFT_CTRL_PRESSED),
            origin,
            &mut typed,
        );
        assert_eq!(
            text(typed),
            "\x1b[<0;5;6M\x1b[<32;7;6M\x1b[<0;8;6m\x1b[<18;3;2M"
        );
    }

    /// A notch of the wheel is 64 forward and 65 back, Alt added as 8.
    #[test]
    fn the_wheel_is_reported_by_its_turn() {
        let mut typed = typed();
        take_mouse(
            &mouse(0, 0, 120 << 16, MOUSE_WHEELED, 0),
            (0, 0),
            &mut typed,
        );
        take_mouse(
            &mouse(
                0,
                0,
                (-120i16 as u16 as u32) << 16,
                MOUSE_WHEELED,
                LEFT_ALT_PRESSED,
            ),
            (0, 0),
            &mut typed,
        );
        assert_eq!(text(typed), "\x1b[<64;1;1M\x1b[<73;1;1M");
    }

    #[test]
    fn a_wake_ends_the_wait_and_is_used_up() {
        let waker = InputWaker::new().unwrap();
        waker.wake();
        let started = Instant::now();
        wait_stdin(Some(Duration::from_secs(5)), Some(&waker));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
