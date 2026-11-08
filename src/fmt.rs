//! Display formatting shared by GUI and CLI.

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::volume::unix_to_ft;

/// `1234567` -> `1,234,567`
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (k, c) in s.chars().enumerate() {
        if k > 0 && (s.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Explorer-style size in KB, rounded up.
pub fn size_kb(bytes: u64) -> String {
    format!("{} KB", group(bytes.div_ceil(1024)))
}

/// Local time `YYYY-MM-DD HH:MM`; empty for unknown.
pub fn date(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let ft = unix_to_ft(unix);
    let f = FILETIME {
        dwLowDateTime: ft as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    // SAFETY: POD structs, valid pointers.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0
            || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0
        {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            loc.wYear, loc.wMonth, loc.wDay, loc.wHour, loc.wMinute
        )
    }
}

/// Fast local-time formatter for bulk output: asks Windows for the UTC
/// offset once per hour of time (DST-correct), then formats with integer
/// arithmetic. Same output as [`date`].
#[derive(Default)]
pub struct DateCache {
    offsets: std::collections::HashMap<u32, i64>,
}

impl DateCache {
    fn offset(&mut self, unix: u32) -> Option<i64> {
        let hour = unix / 3600;
        if let Some(&o) = self.offsets.get(&hour) {
            return Some(o);
        }
        let ft = unix_to_ft(hour * 3600);
        let f = FILETIME {
            dwLowDateTime: ft as u32,
            dwHighDateTime: (ft >> 32) as u32,
        };
        // SAFETY: POD structs, valid pointers.
        let loc = unsafe {
            let mut utc: SYSTEMTIME = std::mem::zeroed();
            let mut loc: SYSTEMTIME = std::mem::zeroed();
            if FileTimeToSystemTime(&f, &mut utc) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0
            {
                return None;
            }
            loc
        };
        let local = days_from_civil(loc.wYear as i64, loc.wMonth as i64, loc.wDay as i64) * 86_400
            + loc.wHour as i64 * 3600
            + loc.wMinute as i64 * 60
            + loc.wSecond as i64;
        let o = local - (hour as i64 * 3600);
        self.offsets.insert(hour, o);
        Some(o)
    }

    /// Append `YYYY-MM-DD HH:MM` (nothing for unknown) to `out`.
    pub fn write(&mut self, unix: u32, out: &mut String) {
        use std::fmt::Write;
        if unix == 0 {
            return;
        }
        let Some(o) = self.offset(unix) else { return };
        let t = unix as i64 + o;
        let (y, m, d) = civil_from_days(t.div_euclid(86_400));
        let sec = t.rem_euclid(86_400);
        let _ = write!(
            out,
            "{y:04}-{m:02}-{d:02} {:02}:{:02}",
            sec / 3600,
            sec % 3600 / 60
        );
    }
}

// Howard Hinnant's civil-calendar algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}
