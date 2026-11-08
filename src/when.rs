//! Date filter values (`dm:`): calendar dates in local time, relative ages
//! and ranges, turned into a half-open range of Unix seconds.
//!
//! ```text
//! dm:2024            the whole year          dm:today   dm:yesterday
//! dm:2024-05         the whole month         dm:7d      last 7 days (also h, w)
//! dm:2024-05-01      that day                dm:>2024-01-01   after that day
//! dm:<7d  dm:7d      newer than 7 days       dm:>7d   older than 7 days
//! dm:2024-01..2024-03   January through March (inclusive)
//! ```

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};

use crate::volume::ft_to_unix;

/// Converts local midnight of (year, month, day) to Unix seconds.
pub trait Clock {
    fn midnight(&self, y: i32, m: u32, d: u32) -> Option<i64>;
    /// Today's local date.
    fn today(&self) -> (i32, u32, u32);
    fn now(&self) -> i64;
}

/// The system clock and time zone.
pub struct Local;

impl Clock for Local {
    fn midnight(&self, y: i32, m: u32, d: u32) -> Option<i64> {
        let local = SYSTEMTIME {
            wYear: u16::try_from(y).ok()?,
            wMonth: m as u16,
            wDayOfWeek: 0,
            wDay: d as u16,
            wHour: 0,
            wMinute: 0,
            wSecond: 0,
            wMilliseconds: 0,
        };
        // SAFETY: POD structs and valid pointers; failures are reported via the return value.
        unsafe {
            let mut utc: SYSTEMTIME = std::mem::zeroed();
            let mut ft: FILETIME = std::mem::zeroed();
            if TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) == 0
                || SystemTimeToFileTime(&utc, &mut ft) == 0
            {
                return None;
            }
            Some(ft_to_unix(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64) as i64)
        }
    }
    fn today(&self) -> (i32, u32, u32) {
        // SAFETY: POD out struct.
        let st = unsafe {
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            st
        };
        (st.wYear as i32, st.wMonth as u32, st.wDay as u32)
    }
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn next_day(y: i32, m: u32, d: u32) -> (i32, u32, u32) {
    if d < days_in_month(y, m) {
        (y, m, d + 1)
    } else if m < 12 {
        (y, m + 1, 1)
    } else {
        (y + 1, 1, 1)
    }
}

fn prev_day(y: i32, m: u32, d: u32) -> (i32, u32, u32) {
    if d > 1 {
        (y, m, d - 1)
    } else if m > 1 {
        (y, m - 1, days_in_month(y, m - 1))
    } else {
        (y - 1, 12, 31)
    }
}

/// `end` of a relative age span: "from then until now and beyond".
const AGE: i64 = i64::MAX - 1;

/// `[start, end)` of one value (a date, a relative age, or a keyword).
fn span(s: &str, c: &dyn Clock) -> Result<(i64, i64), String> {
    let err = || format!("invalid date: {s} (use YYYY, YYYY-MM, YYYY-MM-DD, today, yesterday, 7d)");
    let at = |(y, m, d): (i32, u32, u32)| c.midnight(y, m, d).ok_or_else(err);
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "today" => return Ok((at(c.today())?, at(next_day_t(c.today()))?)),
        "yesterday" => {
            let t = c.today();
            return Ok((at(prev_day(t.0, t.1, t.2))?, at(t)?));
        }
        _ => {}
    }
    if let Some(unit) = lower.chars().last().filter(|ch| "hdw".contains(*ch)) {
        if let Ok(n) = lower[..lower.len() - 1].parse::<u32>() {
            let secs = match unit {
                'h' => 3600,
                'd' => 86_400,
                _ => 7 * 86_400,
            };
            // Marked as an age: comparisons on it compare ages (see parse_range).
            return Ok((c.now() - n as i64 * secs, AGE));
        }
    }
    let parts: Vec<&str> = lower.split(['-', '/', '.']).collect();
    let num = |p: &str, lo: u32, hi: u32| {
        p.parse::<u32>()
            .ok()
            .filter(|v| (lo..=hi).contains(v))
            .ok_or_else(err)
    };
    match parts.as_slice() {
        [y] if y.len() == 4 => {
            let y = num(y, 1601, 30827)? as i32;
            Ok((at((y, 1, 1))?, at((y + 1, 1, 1))?))
        }
        [y, m] if y.len() == 4 => {
            let (y, m) = (num(y, 1601, 30827)? as i32, num(m, 1, 12)?);
            let end = if m == 12 {
                (y + 1, 1, 1)
            } else {
                (y, m + 1, 1)
            };
            Ok((at((y, m, 1))?, at(end)?))
        }
        [y, m, d] if y.len() == 4 => {
            let (y, m) = (num(y, 1601, 30827)? as i32, num(m, 1, 12)?);
            let d = num(d, 1, days_in_month(y, m))?;
            Ok((at((y, m, d))?, at(next_day(y, m, d))?))
        }
        _ => Err(err()),
    }
}

fn next_day_t((y, m, d): (i32, u32, u32)) -> (i32, u32, u32) {
    next_day(y, m, d)
}

/// Parse a `dm:` value into `[lo, hi)` Unix seconds (clamped to u32).
/// Maps a span (start, end) to the range a comparison operator selects.
type Bound = fn((i64, i64)) -> (i64, i64);

pub fn parse_range(s: &str, c: &dyn Clock) -> Result<(u32, u32), String> {
    let clamp = |x: i64| x.clamp(0, u32::MAX as i64) as u32;
    let (lo, hi) = if let Some((a, b)) = s.split_once("..") {
        let hi = span(b, c)?.1;
        (span(a, c)?.0, if hi == AGE { i64::MAX } else { hi })
    } else {
        let ops: [(&str, Bound); 5] = [
            (">=", |(s, _)| (s, i64::MAX)),
            ("<=", |(_, e)| (i64::MIN, e)),
            (">", |(_, e)| (e, i64::MAX)),
            ("<", |(s, _)| (i64::MIN, s)),
            ("=", |r| r),
        ];
        match ops
            .iter()
            .find_map(|(p, f)| s.strip_prefix(p).map(|rest| (p, f, rest)))
        {
            Some((op, f, rest)) => match span(rest, c)? {
                // Ages: `>7d` = older than 7 days, `<7d` = within 7 days.
                (start, AGE) if op.starts_with('>') => (i64::MIN, start),
                (start, AGE) => (start, i64::MAX),
                r => f(r),
            },
            None => match span(s, c)? {
                (start, AGE) => (start, i64::MAX),
                r => r,
            },
        }
    };
    Ok((clamp(lo), clamp(hi)))
}
