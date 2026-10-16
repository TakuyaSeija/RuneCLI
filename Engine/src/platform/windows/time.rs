//! The user's local time.

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{
    FileTimeToSystemTime, GetDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTimeEx,
    DYNAMIC_TIME_ZONE_INFORMATION,
};

use crate::platform::LocalTime;

/// `epoch_secs` (seconds since 1970, UTC) in the user's own time zone, with
/// that year's daylight saving rules.
pub fn local_time(epoch_secs: i64) -> Option<LocalTime> {
    const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
    let ticks = epoch_secs
        .checked_mul(10_000_000)?
        .checked_add(UNIX_EPOCH_TICKS)?;
    let ticks = u64::try_from(ticks).ok()?;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { FileTimeToSystemTime(&file, &mut utc) } == 0 {
        return None;
    }
    // The zone the person set, with its daylight saving rules for every
    // year: the `Ex` conversion takes it named, never as "whatever is
    // current" (only the older function reads a null that way).
    let mut zone: DYNAMIC_TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetDynamicTimeZoneInformation(&mut zone) } == u32::MAX {
        return None;
    }
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { SystemTimeToTzSpecificLocalTimeEx(&zone, &utc, &mut local) } == 0 {
        return None;
    }
    Some(LocalTime {
        year: i32::from(local.wYear),
        month: u32::from(local.wMonth),
        day: u32::from(local.wDay),
        hour: u32::from(local.wHour),
        minute: u32::from(local.wMinute),
        second: u32::from(local.wSecond),
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
