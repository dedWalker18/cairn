//! Time handling. Rules (see docs/DECISIONS.md, D-016):
//!
//! * Filesystem times are UTC instants (ns since epoch). They are converted to
//!   civil time in the *system* time zone only when compared with a query.
//! * EXIF capture times are civil times of the camera's location. They are
//!   compared as civil times directly: a photo taken 23:30 on 31 Dec in
//!   Reykjavik is "December" to the person who took it, wherever they are now.
//! * Implausible timestamps (before 1990, more than a day in the future,
//!   EXIF "0000:00:00") are treated as unknown, never as real dates.

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

pub const MIN_PLAUSIBLE_YEAR: i16 = 1990;

/// Half-open civil interval [start, end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilRange {
    pub start: DateTime,
    pub end: DateTime,
}

impl CivilRange {
    pub fn contains(&self, t: DateTime) -> bool {
        t >= self.start && t < self.end
    }

    pub fn days(start: Date, end_exclusive: Date) -> CivilRange {
        CivilRange { start: start.to_datetime(jiff::civil::Time::midnight()), end: end_exclusive.to_datetime(jiff::civil::Time::midnight()) }
    }
}

pub fn now_ns() -> i64 {
    Timestamp::now().as_nanosecond() as i64
}

/// Civil time of a filesystem timestamp in the system zone, if plausible.
pub fn fs_ns_to_civil(ns: i64, tz: &TimeZone, now: Timestamp) -> Option<DateTime> {
    if ns <= 0 {
        return None;
    }
    let ts = Timestamp::from_nanosecond(ns as i128).ok()?;
    if ts > now + jiff::SignedDuration::from_hours(24) {
        return None;
    }
    let dt = ts.to_zoned(tz.clone()).datetime();
    if dt.year() < MIN_PLAUSIBLE_YEAR {
        return None;
    }
    Some(dt)
}

/// Parse an EXIF "YYYY:MM:DD HH:MM:SS" value. Returns None for zeroed or
/// malformed values instead of guessing.
pub fn parse_exif_datetime(s: &str) -> Option<DateTime> {
    let s = s.trim().trim_end_matches('\0');
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i32> { std::str::from_utf8(&b[r]).ok()?.trim().parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if y < MIN_PLAUSIBLE_YEAR as i32 || y > 2200 {
        return None;
    }
    DateTime::new(y as i16, mo as i8, d as i8, h as i8, mi as i8, se.min(59) as i8, 0).ok()
}

/// Parse an EXIF OffsetTime value like "+05:30" into seconds east of UTC.
pub fn parse_exif_offset(s: &str) -> Option<i32> {
    let s = s.trim().trim_end_matches('\0');
    let b = s.as_bytes();
    if b.len() < 6 || b[3] != b':' {
        return None;
    }
    let sign = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let h: i32 = s[1..3].parse().ok()?;
    let m: i32 = s[4..6].parse().ok()?;
    if h > 14 || m > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60))
}

pub fn civil_to_string(dt: DateTime) -> String {
    dt.strftime("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn civil_from_string(s: &str) -> Option<DateTime> {
    s.parse().ok()
}

pub fn system_tz() -> TimeZone {
    TimeZone::system()
}

pub fn now_zoned() -> Zoned {
    Zoned::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_parsing() {
        let dt = parse_exif_datetime("2025:12:14 09:31:02").unwrap();
        assert_eq!(civil_to_string(dt), "2025-12-14T09:31:02");
        assert!(parse_exif_datetime("0000:00:00 00:00:00").is_none());
        assert!(parse_exif_datetime("    :  :     :  :  ").is_none());
        assert!(parse_exif_datetime("2025:13:40 00:00:00").is_none());
        assert_eq!(parse_exif_offset("+05:30"), Some(19800));
        assert_eq!(parse_exif_offset("-03:00"), Some(-10800));
        assert_eq!(parse_exif_offset("garbage"), None);
    }

    #[test]
    fn implausible_fs_times() {
        let tz = TimeZone::UTC;
        let now = Timestamp::from_second(1_790_000_000).unwrap();
        assert!(fs_ns_to_civil(0, &tz, now).is_none());
        assert!(fs_ns_to_civil(1_000_000_000, &tz, now).is_none()); // 1970
        let future = (1_790_000_000i64 + 3 * 86400) * 1_000_000_000;
        assert!(fs_ns_to_civil(future, &tz, now).is_none());
        let ok = 1_700_000_000i64 * 1_000_000_000;
        assert!(fs_ns_to_civil(ok, &tz, now).is_some());
    }
}
