//! The user's local time.

use crate::platform::LocalTime;

/// `epoch_secs` (seconds since 1970, UTC) in the user's own time zone.
///
/// Local, not UTC: the model is told "today" to reason about the user's day,
/// and a window shows the hour on the person's clock. `localtime_r` reads the
/// zone the user configured; nothing here assumes a distro layout.
pub fn local_time(epoch_secs: i64) -> Option<LocalTime> {
    let secs = libc::time_t::try_from(epoch_secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` fills the struct it is given and reads only `secs`.
    let filled = unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some(LocalTime {
        year: tm.tm_year + 1900,
        month: u32::try_from(tm.tm_mon + 1).ok()?,
        day: u32::try_from(tm.tm_mday).ok()?,
        hour: u32::try_from(tm.tm_hour).ok()?,
        minute: u32::try_from(tm.tm_min).ok()?,
        second: u32::try_from(tm.tm_sec).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moment_is_a_civil_time() {
        let t = local_time(1_700_000_000).unwrap();
        assert!((2023..=2023).contains(&t.year), "{t:?}");
        assert!((1..=12).contains(&t.month) && (1..=31).contains(&t.day));
        assert!(t.hour < 24 && t.minute < 60 && t.second < 61);
    }
}
