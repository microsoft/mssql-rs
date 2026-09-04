// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The calendar and the normalized date/time value model, shared by both
//! directions.
//!
//! Sibling to [`super::numeric`]: it holds the representation and the
//! arithmetic, and neither direction's pointer I/O. `civil_from_days_since_0001`
//! and [`days_since_0001_from_civil`] are an inverse pair and live together so
//! fetch and parameters cannot disagree about what day a date is.

/// Days from 0001-01-01 (proleptic Gregorian) to 1900-01-01, used to rebase the
/// `datetime` / `smalldatetime` epoch onto the common day-0 = 0001-01-01 axis.
pub(crate) const DAYS_0001_TO_1900: i64 = 693_595;

/// Number of 100 ns ticks in one day.
pub(crate) const TICKS_PER_DAY: i64 = 864_000_000_000;

/// Day number of `9999-12-31`, the maximum SQL Server date. Used to reject a
/// `datetimeoffset` whose offset adjustment would leave the representable range.
pub(crate) const MAX_DAYS_SINCE_0001: i64 = 3_652_058;

/// A normalized calendar breakdown shared by every date/time column type, so
/// each target C struct can be filled from a single representation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DateTimeParts {
    /// Proleptic Gregorian year.
    pub year: i16,
    /// Calendar month in `1..=12`.
    pub month: u16,
    /// Calendar day in `1..=31`.
    pub day: u16,
    /// Hour in `0..=23`.
    pub hour: u16,
    /// Minute in `0..=59`.
    pub minute: u16,
    /// Second in `0..=59`.
    pub second: u16,
    /// Fractional seconds in nanoseconds.
    pub fraction_ns: u32,
    /// Declared fractional-seconds scale (0-7) of the source column. Character
    /// rendering pads to exactly this many digits, matching msodbcsql.
    pub scale: u8,
    /// Signed timezone hour component.
    pub tz_hour: i16,
    /// Signed timezone minute component.
    pub tz_minute: i16,
    /// Whether the source carries a date component.
    pub has_date: bool,
    /// Whether the source carries a time component.
    pub has_time: bool,
    /// Whether the source carries a timezone offset.
    pub has_tz: bool,
}

/// Days in `month` of `year` under the proleptic Gregorian leap rule. `0` for a
/// month outside `1..=12`.
pub(crate) fn days_in_month(year: i16, month: u16) -> u16 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let y = i32::from(year);
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// (year, month, day) from a day count where day 0 = 0001-01-01, using Howard
/// Hinnant's `civil_from_days` algorithm rebased from its 1970 epoch.
pub(crate) fn civil_from_days_since_0001(days_since_0001: i64) -> (i16, u16, u16) {
    // Hinnant's algorithm works in days since 1970-01-01 with a +719468 shift.
    let z = days_since_0001 - 719_162 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as i16, m as u16, d as u16)
}

/// Inverse of [`civil_from_days_since_0001`], for the parameter direction.
///
/// `None` for a date outside SQL Server's `0001-01-01`..`9999-12-31` range or
/// for a day that does not exist in the given month, so a caller can report
/// `22007` rather than sending a wrong day. The month-length check is what makes
/// this a validator and not just arithmetic: the algorithm itself happily maps
/// 31 February onto 3 March.
pub(crate) fn days_since_0001_from_civil(year: i16, month: u16, day: u16) -> Option<i64> {
    if !(1..=9999).contains(&year) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    // Hinnant's `days_from_civil`, rebased from its 1970 epoch onto day 0 = 0001-01-01.
    let y = i64::from(year) - i64::from(month <= 2);
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468 + 719_162)
}

/// (hour, minute, second, fraction_ns) from 100-nanosecond ticks since midnight.
///
/// `SqlTime::time_nanoseconds` is a misnomer: the decoder normalizes every
/// fractional-seconds scale to 100 ns ticks, not nanoseconds.
pub(crate) fn hms_from_ticks_100ns(ticks: u64) -> (u16, u16, u16, u32) {
    let secs = ticks / 10_000_000;
    let fraction_ns = ((ticks % 10_000_000) * 100) as u32;
    (
        (secs / 3600) as u16,
        ((secs % 3600) / 60) as u16,
        (secs % 60) as u16,
        fraction_ns,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_anchor_dates() {
        assert_eq!(civil_from_days_since_0001(0), (1, 1, 1));
        assert_eq!(civil_from_days_since_0001(693_595), (1900, 1, 1));
        assert_eq!(civil_from_days_since_0001(730_178), (2000, 2, 29));
        assert_eq!(civil_from_days_since_0001(738_685), (2023, 6, 15));
        assert_eq!(civil_from_days_since_0001(3_652_058), (9999, 12, 31));
    }

    /// The two directions must agree on every representable day, which they can
    /// only be relied on to do while they share this module.
    #[test]
    fn the_calendar_round_trips_at_its_boundaries() {
        for days in [
            0,
            1,
            DAYS_0001_TO_1900,
            730_178,
            738_685,
            MAX_DAYS_SINCE_0001,
        ] {
            let (y, m, d) = civil_from_days_since_0001(days);
            assert_eq!(
                days_since_0001_from_civil(y, m, d),
                Some(days),
                "day {days}"
            );
        }
    }

    /// A month length rejected here would otherwise be silently rolled forward
    /// by the arithmetic: 31 April becomes 1 May, 29 February a common year
    /// becomes 1 March.
    #[test]
    fn impossible_days_are_rejected_rather_than_rolled_forward() {
        for (y, m, d) in [
            (2024, 4u16, 31u16),
            (2023, 2, 29),
            (1900, 2, 29),
            (2024, 1, 0),
            (2024, 13, 1),
            (2024, 0, 1),
            (0, 1, 1),
            (10000, 1, 1),
        ] {
            assert_eq!(days_since_0001_from_civil(y, m, d), None, "{y}-{m}-{d}");
        }
        // The 100/400 split, both directions. Rejecting an impossible day is not
        // the same as getting the century rule right: 1700/1800/1900 are common
        // years, 1600/2000 are leap.
        for y in [1700i16, 1800, 1900] {
            assert_eq!(days_since_0001_from_civil(y, 2, 29), None, "{y}-02-29");
        }
        for y in [1600i16, 2000, 1804, 2004, 2024, 2028] {
            assert!(days_since_0001_from_civil(y, 2, 29).is_some(), "{y}-02-29");
        }
        assert!(days_since_0001_from_civil(1601, 2, 28).is_some());
    }
}
