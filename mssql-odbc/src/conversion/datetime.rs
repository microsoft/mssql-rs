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

/// A calendar date, shared by the day-number decoder and the `YYYY-MM-DD`
/// parser so neither hands back an unlabelled triple of same-typed fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CivilDate {
    /// Proleptic Gregorian year.
    pub year: i16,
    /// Calendar month in `1..=12`.
    pub month: u16,
    /// Calendar day in `1..=31`.
    pub day: u16,
}

/// A wall-clock time of day, carrying no date and no offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimeOfDay {
    /// Hour in `0..=23`.
    pub hour: u16,
    /// Minute in `0..=59`.
    pub minute: u16,
    /// Second in `0..=59`.
    pub second: u16,
    /// Fractional seconds in nanoseconds.
    pub fraction_ns: u32,
}

/// A parsed time literal, paired with the scale that literal itself declared.
///
/// The scale is a property of the text, not of the instant, which is why it
/// sits here rather than on [`TimeOfDay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParsedTime {
    pub time: TimeOfDay,
    /// Count of fractional-seconds digits the literal supplied.
    pub scale: u8,
}

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
    /// Fractional-seconds scale. For a fetched value this is the source
    /// column's declared scale (0-7), which character rendering pads to
    /// exactly, matching msodbcsql. A value parsed from a literal instead
    /// carries that literal's own digit count, which
    /// `parse_time_literal` bounds at 9 rather than 7 -- so do not treat
    /// this as an index into a 7-digit string. `format_datetime_parts` is
    /// the only renderer and clamps with `.min(frac.len())`.
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

/// The calendar date for a day count where day 0 = 0001-01-01, using Howard
/// Hinnant's `civil_from_days` algorithm rebased from its 1970 epoch.
pub(crate) fn civil_from_days_since_0001(days_since_0001: i64) -> CivilDate {
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
    CivilDate {
        year: year as i16,
        month: m as u16,
        day: d as u16,
    }
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

/// The clock fields for a count of 100-nanosecond ticks since midnight.
///
/// `SqlTime::time_nanoseconds` is a misnomer: the decoder normalizes every
/// fractional-seconds scale to 100 ns ticks, not nanoseconds.
pub(crate) fn hms_from_ticks_100ns(ticks: u64) -> TimeOfDay {
    let secs = ticks / 10_000_000;
    let fraction_ns = ((ticks % 10_000_000) * 100) as u32;
    TimeOfDay {
        hour: (secs / 3600) as u16,
        minute: ((secs % 3600) / 60) as u16,
        second: (secs % 60) as u16,
        fraction_ns,
    }
}

/// Parses `YYYY-MM-DD`.
fn parse_date_literal(s: &str) -> Option<CivilDate> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 {
        return None;
    }
    // `str::parse` accepts a leading `+`, which would make `+123-01-01` a valid
    // date; require plain digits.
    if !y
        .bytes()
        .chain(m.bytes())
        .chain(d.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let year: i16 = y.parse().ok()?;
    let month: u16 = m.parse().ok()?;
    let day: u16 = d.parse().ok()?;
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    // Reject impossible days (2023-02-31, or 02-29 outside a leap year) rather
    // than writing them into a date struct as a successful conversion.
    if !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    Some(CivilDate { year, month, day })
}

/// Parses `HH:MM[:SS[.f{1,9}]]`, returning the components plus the number of
/// fractional digits supplied (the effective scale).
fn parse_time_literal(s: &str) -> Option<ParsedTime> {
    let mut it = s.split(':');
    let hour_s = it.next()?;
    let minute_s = it.next()?;
    let sec_part = it.next().unwrap_or("0");
    if it.next().is_some() {
        return None;
    }
    let (sec_digits, frac_digits) = match sec_part.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_part, ""),
    };
    // `str::parse` accepts a leading `+`, which would make `+1:00:00` a valid
    // time; require plain digits.
    if !hour_s
        .bytes()
        .chain(minute_s.bytes())
        .chain(sec_digits.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let hour: u16 = hour_s.parse().ok()?;
    let minute: u16 = minute_s.parse().ok()?;
    let second: u16 = sec_digits.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if !frac_digits.is_empty() && !frac_digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // `SQL_TIMESTAMP_STRUCT.fraction` is nanoseconds, so a character literal can
    // carry 9 exact digits; msodbcsql rejects anything longer rather than
    // truncating it, and a character source has no server-side scale to cap it.
    if frac_digits.len() > 9 {
        return None;
    }
    let mut nanos: u32 = 0;
    for i in 0..9 {
        let digit = frac_digits
            .as_bytes()
            .get(i)
            .map_or(0, |b| u32::from(b - b'0'));
        nanos = nanos * 10 + digit;
    }
    Some(ParsedTime {
        time: TimeOfDay {
            hour,
            minute,
            second,
            fraction_ns: nanos,
        },
        scale: frac_digits.len() as u8,
    })
}

/// Port of msodbcsql's `IsValidTimezoneOffsetValue` (`dataconv.cpp:118`).
///
/// The mixed-sign rules are the non-obvious part: `+5h -30m` is rejected even
/// though it totals a legal +4:30, because the two components must agree in
/// sign. Checking only the total would silently accept it.
///
/// Those arms are reachable only from the `SQL_C_SS_TIMESTAMPOFFSET` struct
/// path, where the caller supplies each component independently.
/// `parse_datetime_literal` applies one sign to both, so its tests
/// (`an_offset_is_bounded_on_the_total`) exercise the total bound only.
pub(crate) fn is_valid_timezone_offset(tz_hour: i16, tz_minute: i16) -> bool {
    let total = i32::from(tz_hour) * 60 + i32::from(tz_minute);
    !((tz_hour > 0 && tz_minute < 0)
        || (tz_hour < 0 && tz_minute > 0)
        || !(-59..=59).contains(&tz_minute)
        || total.abs() > 14 * 60)
}

/// Parses the character forms of `date`, `time`, `datetime2` and
/// `datetimeoffset` into [`DateTimeParts`].
///
/// Shared by both directions so a literal a fetch would accept is one a
/// parameter binding accepts too.
pub(crate) fn parse_datetime_literal(text: &str) -> Option<DateTimeParts> {
    let mut s = text.trim();
    let mut p = DateTimeParts::default();

    // A trailing "+HH:MM" / "-HH:MM" is a UTC offset. Match it only in that
    // exact shape so the hyphens inside a date are never mistaken for one.
    // Compared as bytes: slicing the `str` would panic when a multi-byte
    // character straddles the boundary, and the payload is server data.
    if let Some(tail) = s.len().checked_sub(6).and_then(|i| s.as_bytes().get(i..))
        && (tail[0] == b'+' || tail[0] == b'-')
        && tail[3] == b':'
        && tail[1..3].iter().chain(&tail[4..6]).all(u8::is_ascii_digit)
    {
        let sign: i16 = if tail[0] == b'+' { 1 } else { -1 };
        let hh = i16::from(tail[1] - b'0') * 10 + i16::from(tail[2] - b'0');
        let mm = i16::from(tail[4] - b'0') * 10 + i16::from(tail[5] - b'0');
        // Bounding the components separately would admit +14:30, which no
        // target validates once the offset is parsed.
        if !is_valid_timezone_offset(sign * hh, sign * mm) {
            return None;
        }
        p.tz_hour = sign * hh;
        p.tz_minute = sign * mm;
        p.has_tz = true;
        // The matched tail is all ASCII, so this boundary is a char boundary.
        s = s[..s.len() - 6].trim_end();
    }

    // An empty right-hand side is left for `parse_time_literal` to reject
    // rather than filtered out below: dropping it would make a dangling
    // separator such as `2024-05-20T` parse as a date-only literal and bind
    // against a date or timestamp target.
    let (date_str, time_str) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (Some(d), Some(t.trim())),
        None if s.contains(':') => (None, Some(s)),
        None => (Some(s), None),
    };

    if let Some(d) = date_str {
        let date = parse_date_literal(d)?;
        p.year = date.year;
        p.month = date.month;
        p.day = date.day;
        p.has_date = true;
    }
    if let Some(t) = time_str {
        let parsed = parse_time_literal(t)?;
        p.hour = parsed.time.hour;
        p.minute = parsed.time.minute;
        p.second = parsed.time.second;
        p.fraction_ns = parsed.time.fraction_ns;
        p.scale = parsed.scale;
        p.has_time = true;
    }
    if !p.has_date && !p.has_time {
        return None;
    }
    // An offset is only meaningful alongside a date and time.
    if p.has_tz && !(p.has_date && p.has_time) {
        return None;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_anchor_dates() {
        let civil = |days| {
            let d = civil_from_days_since_0001(days);
            (d.year, d.month, d.day)
        };
        assert_eq!(civil(0), (1, 1, 1));
        assert_eq!(civil(693_595), (1900, 1, 1));
        assert_eq!(civil(730_178), (2000, 2, 29));
        assert_eq!(civil(738_685), (2023, 6, 15));
        assert_eq!(civil(3_652_058), (9999, 12, 31));
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
            let d = civil_from_days_since_0001(days);
            assert_eq!(
                days_since_0001_from_civil(d.year, d.month, d.day),
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

    /// The shapes both directions have to accept. Pinned directly now that the
    /// parameter path shares this parser and not only the fetch path reaches it.
    #[test]
    fn the_accepted_literal_shapes_parse() {
        let date_only = parse_datetime_literal("2024-05-20").unwrap();
        assert!(date_only.has_date && !date_only.has_time);
        assert_eq!(
            (date_only.year, date_only.month, date_only.day),
            (2024, 5, 20)
        );

        let time_only = parse_datetime_literal("12:34:56").unwrap();
        assert!(time_only.has_time && !time_only.has_date);
        assert_eq!(
            (time_only.hour, time_only.minute, time_only.second),
            (12, 34, 56)
        );

        // Both separators reach the same value.
        assert_eq!(
            parse_datetime_literal("2024-05-20 12:34:56"),
            parse_datetime_literal("2024-05-20T12:34:56")
        );

        // Seconds are optional; the fraction sets the effective scale.
        let hhmm = parse_datetime_literal("12:34").unwrap();
        assert_eq!((hhmm.second, hhmm.fraction_ns, hhmm.scale), (0, 0, 0));
        let frac = parse_datetime_literal("12:34:56.000000").unwrap();
        assert_eq!((frac.fraction_ns, frac.scale), (0, 6));
        let ns = parse_datetime_literal("12:34:56.123456789").unwrap();
        assert_eq!((ns.fraction_ns, ns.scale), (123_456_789, 9));

        // Surrounding whitespace is not part of the value.
        assert_eq!(
            parse_datetime_literal("  2024-05-20  "),
            parse_datetime_literal("2024-05-20")
        );
    }

    #[test]
    fn an_offset_is_signed_and_bounded() {
        let plus = parse_datetime_literal("2024-05-20 12:34:56+05:30").unwrap();
        assert!(plus.has_tz);
        assert_eq!((plus.tz_hour, plus.tz_minute), (5, 30));

        let minus = parse_datetime_literal("2024-05-20 12:34:56-08:00").unwrap();
        assert_eq!((minus.tz_hour, minus.tz_minute), (-8, 0));

        // Past the legal range, and an offset with nothing to anchor it.
        assert_eq!(parse_datetime_literal("2024-05-20 12:34:56+15:00"), None);
        assert_eq!(parse_datetime_literal("2024-05-20 12:34:56+05:60"), None);
        assert_eq!(parse_datetime_literal("12:34:56+05:30"), None);
        assert_eq!(parse_datetime_literal("2024-05-20+05:30"), None);
    }

    /// The total is what is bounded, not each component. Bounding them
    /// separately admitted `+14:01`..`+14:59`, which only the
    /// `SQL_SS_TIMESTAMPOFFSET` target would have caught afterwards - every
    /// other temporal target discards the offset and would have accepted the
    /// literal.
    #[test]
    fn an_offset_is_bounded_on_the_total() {
        assert!(parse_datetime_literal("2024-05-20 12:34:56+14:00").is_some());
        assert!(parse_datetime_literal("2024-05-20 12:34:56-14:00").is_some());
        for bad in [
            "2024-05-20 12:34:56+14:01",
            "2024-05-20 12:34:56+14:30",
            "2024-05-20 12:34:56+14:59",
            "2024-05-20 12:34:56-14:01",
            "2024-05-20 12:34:56-14:30",
        ] {
            assert_eq!(parse_datetime_literal(bad), None, "{bad:?} was accepted");
        }
    }

    /// A hyphen inside a date must never be read as the offset sign, and a
    /// leading `+` must not sneak past `str::parse`.
    #[test]
    fn malformed_literals_are_rejected() {
        for bad in [
            "",
            "   ",
            "abc",
            "2024-05-20 25:00:00",
            "2024-05-20 12:60:00",
            "2024-05-20 12:00:60",
            "2023-02-31",
            "+123-01-01",
            "+1:00:00",
            "2024-05-20 12:34:56.1234567890",
            "2024-05-20 12:34:56:78",
            "2024-05-20-01",
            // A separator with nothing after it names no time. Dropping the
            // empty right-hand side instead would let these bind against a
            // date or timestamp target as if the separator were not there.
            "2024-05-20T",
            "2024-05-20T   ",
        ] {
            assert_eq!(parse_datetime_literal(bad), None, "{bad:?} was accepted");
        }
    }

    /// Unpadded month/day fields, a seconds-less time, a trailing empty
    /// fraction and the ISO `T` separator are accepted where msodbcsql's
    /// fixed-length token grammar refuses them. Deliberate, recorded in
    /// `docs/parameters_plan.md`, and pinned here so it cannot be dropped by
    /// accident.
    ///
    /// `1:01:01` and `18:01:0` are `22018` rows in msodbcsql's own regression
    /// table (`KatmaiDatetimeODBC.cpp`); `12:00:00.` reaches its `ECODE_TIME2`
    /// grammar, whose `'.' NUM(-9)` needs at least one digit; the `T` form was
    /// measured `22018` on retail 18.6.2.1 via the compare leg.
    #[test]
    fn the_permissive_shapes_stay_accepted() {
        for text in [
            "2024-5-20",
            "2023-6-5",
            "12:34",
            "2024-5-20 1:2:3",
            "12:00:00.",
            "2024-05-20T12:34:56.123",
        ] {
            assert!(
                parse_datetime_literal(text).is_some(),
                "{text:?} was rejected"
            );
        }
        // The year is still fixed-width, which is what stops `+123-01-01`.
        assert_eq!(parse_datetime_literal("999-01-01"), None);
    }
}
