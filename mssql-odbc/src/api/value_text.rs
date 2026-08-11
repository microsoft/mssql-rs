// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Character rendering of decoded TDS values for `SQL_C_CHAR` / `SQL_C_WCHAR`.
//!
//! ODBC prescribes the literal form each SQL type takes when converted to a
//! character type (see the ODBC "Converting Data from SQL to C Data Types"
//! appendix); the formats below follow it so applications parsing driver output
//! see the same text as `msodbcsql18`.

use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Days between `0001-01-01` and the Unix epoch.
const DAYS_0001_TO_UNIX: i64 = 719_162;
/// Days between `1900-01-01` and the Unix epoch (negative: 1900 precedes 1970).
const DAYS_1900_TO_UNIX: i64 = -25_567;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Converts a day count relative to the Unix epoch into `(year, month, day)`.
///
/// Uses the shifted-era algorithm (civil calendar epoch moved to `0000-03-01`)
/// so leap years fall at the end of an era and need no special casing.
fn civil_from_unix_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_pos = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_pos + 2) / 5 + 1) as u32;
    let month = if month_pos < 10 {
        month_pos + 3
    } else {
        month_pos - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn format_date(days_since_unix: i64) -> String {
    let (y, m, d) = civil_from_unix_days(days_since_unix);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Renders nanoseconds-since-midnight as `HH:MM:SS[.f{scale}]`, matching the
/// column's declared fractional-second scale.
fn format_time(time_nanoseconds: u64, scale: u8) -> String {
    let secs = time_nanoseconds / NANOS_PER_SEC;
    let nanos = time_nanoseconds % NANOS_PER_SEC;
    let base = format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    if scale == 0 {
        return base;
    }
    let scale = scale.min(9) as u32;
    let divisor = 10u64.pow(9 - scale);
    format!("{base}.{:0width$}", nanos / divisor, width = scale as usize)
}

fn format_datetime2(v: &SqlDateTime2) -> String {
    format!(
        "{} {}",
        format_date(i64::from(v.days) - DAYS_0001_TO_UNIX),
        format_time(v.time.time_nanoseconds, v.time.scale)
    )
}

/// `datetime` counts 1/300 s ticks, which ODBC renders with millisecond
/// precision, so the tick count is converted to whole milliseconds.
fn format_datetime(v: &SqlDateTime) -> String {
    let millis = (u64::from(v.time) * 1000).div_ceil(300);
    format!(
        "{} {}",
        format_date(i64::from(v.days) + DAYS_1900_TO_UNIX),
        format_time(millis * 1_000_000, 3)
    )
}

fn format_small_datetime(v: &SqlSmallDateTime) -> String {
    format!(
        "{} {}",
        format_date(i64::from(v.days) + DAYS_1900_TO_UNIX),
        format_time(u64::from(v.time) * 60 * NANOS_PER_SEC, 0)
    )
}

fn format_datetimeoffset(v: &SqlDateTimeOffset) -> String {
    let sign = if v.offset < 0 { '-' } else { '+' };
    let abs = v.offset.unsigned_abs();
    format!(
        "{} {sign}{:02}:{:02}",
        format_datetime2(&v.datetime2),
        abs / 60,
        abs % 60
    )
}

/// Renders a `money`/`smallmoney` scaled integer with its fixed 4-digit scale.
fn format_scaled_4(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

fn money_units(v: &SqlMoney) -> i64 {
    ((i64::from(v.msb_part)) << 32) | i64::from(v.lsb_part as u32)
}

fn format_bytes_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out
}

/// Stack buffer that renders fixed-width values without touching the allocator.
///
/// 64 bytes clears every fixed-width form by a wide margin — the longest is a
/// 36-character GUID — so the overflow path exists only for soundness.
pub(super) struct TextScratch {
    buf: [u8; 64],
    len: usize,
}

impl TextScratch {
    pub(super) fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.buf[..self.len]).ok()
    }
}

impl std::fmt::Write for TextScratch {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Allocation-free overlay on [`column_value_to_text`] for the types whose
/// rendered length is bounded.
///
/// Every other type — and any fixed-width value that somehow overflows the
/// scratch buffer — falls through to `column_value_to_text`, so the two agree
/// by construction.
pub(super) fn column_value_to_text_in<'s>(
    v: &ColumnValues,
    scratch: &'s mut TextScratch,
) -> Option<Cow<'s, str>> {
    scratch.len = 0;
    let rendered = match v {
        ColumnValues::TinyInt(x) => write!(scratch, "{x}"),
        ColumnValues::SmallInt(x) => write!(scratch, "{x}"),
        ColumnValues::Int(x) => write!(scratch, "{x}"),
        ColumnValues::BigInt(x) => write!(scratch, "{x}"),
        ColumnValues::Real(x) => write!(scratch, "{x}"),
        ColumnValues::Float(x) => write!(scratch, "{x}"),
        ColumnValues::Uuid(u) => write!(scratch, "{u}"),
        ColumnValues::Bit(x) => return Some(Cow::Borrowed(if *x { "1" } else { "0" })),
        ColumnValues::Null => return Some(Cow::Borrowed("")),
        _ => return column_value_to_text(v).map(Cow::Owned),
    };
    if rendered.is_ok()
        && let Some(s) = scratch.as_str()
    {
        return Some(Cow::Borrowed(s));
    }
    column_value_to_text(v).map(Cow::Owned)
}

/// Renders `v` in the character form ODBC defines for its SQL type, or `None`
/// when no character conversion is defined for the value.
pub(super) fn column_value_to_text(v: &ColumnValues) -> Option<String> {
    match v {
        ColumnValues::TinyInt(x) => Some(x.to_string()),
        ColumnValues::SmallInt(x) => Some(x.to_string()),
        ColumnValues::Int(x) => Some(x.to_string()),
        ColumnValues::BigInt(x) => Some(x.to_string()),
        ColumnValues::Real(x) => Some(x.to_string()),
        ColumnValues::Float(x) => Some(x.to_string()),
        ColumnValues::Bit(x) => Some(if *x { "1".into() } else { "0".into() }),
        ColumnValues::String(s) => Some(s.to_utf8_string()),
        ColumnValues::Uuid(u) => Some(u.to_string()),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Some(d.to_string()),
        ColumnValues::Date(d) => Some(format_date(i64::from(d.get_days()) - DAYS_0001_TO_UNIX)),
        ColumnValues::Time(t) => Some(format_time(t.time_nanoseconds, t.scale)),
        ColumnValues::DateTime2(v) => Some(format_datetime2(v)),
        ColumnValues::DateTime(v) => Some(format_datetime(v)),
        ColumnValues::SmallDateTime(v) => Some(format_small_datetime(v)),
        ColumnValues::DateTimeOffset(v) => Some(format_datetimeoffset(v)),
        ColumnValues::Money(m) => Some(format_scaled_4(money_units(m))),
        ColumnValues::SmallMoney(m) => Some(format_scaled_4(i64::from(m.int_val))),
        ColumnValues::Bytes(b) => Some(format_bytes_hex(b)),
        ColumnValues::Xml(x) => Some(x.as_string()),
        ColumnValues::Null => Some(String::new()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::column_values::{SqlDate, SqlSmallMoney, SqlTime};

    fn dt2(days: u32, nanos: u64, scale: u8) -> SqlDateTime2 {
        SqlDateTime2 {
            days,
            time: SqlTime {
                time_nanoseconds: nanos,
                scale,
            },
        }
    }

    #[test]
    fn civil_conversion_matches_known_epochs() {
        assert_eq!(civil_from_unix_days(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_days(-DAYS_0001_TO_UNIX), (1, 1, 1));
        assert_eq!(civil_from_unix_days(DAYS_1900_TO_UNIX), (1900, 1, 1));
    }

    #[test]
    fn date_renders_iso_form() {
        // 2024-02-29 exercises the leap-day path.
        let days = 738_944_u32; // days since 0001-01-01
        assert_eq!(
            format_date(i64::from(days) - DAYS_0001_TO_UNIX),
            "2024-02-29"
        );
    }

    #[test]
    fn time_honours_declared_scale() {
        let nanos = (13 * 3600 + 45 * 60 + 30) * NANOS_PER_SEC + 123_456_700;
        assert_eq!(format_time(nanos, 0), "13:45:30");
        assert_eq!(format_time(nanos, 3), "13:45:30.123");
        assert_eq!(format_time(nanos, 7), "13:45:30.1234567");
    }

    #[test]
    fn datetime2_joins_date_and_time() {
        let v = dt2(738_944, 3_600 * NANOS_PER_SEC, 7);
        assert_eq!(format_datetime2(&v), "2024-02-29 01:00:00.0000000");
    }

    #[test]
    fn datetimeoffset_appends_signed_offset() {
        let v = SqlDateTimeOffset {
            datetime2: dt2(738_944, 0, 0),
            offset: -330,
        };
        assert_eq!(format_datetimeoffset(&v), "2024-02-29 00:00:00 -05:30");
    }

    #[test]
    fn money_keeps_four_decimal_places() {
        assert_eq!(format_scaled_4(12_345_678), "1234.5678");
        assert_eq!(format_scaled_4(-1), "-0.0001");
        assert_eq!(format_scaled_4(0), "0.0000");
    }

    #[test]
    fn money_reassembles_split_halves() {
        let m = SqlMoney {
            msb_part: 1,
            lsb_part: -1,
        };
        assert_eq!(money_units(&m), (1i64 << 32) | 0xFFFF_FFFF);
    }

    #[test]
    fn bytes_render_as_uppercase_hex() {
        assert_eq!(format_bytes_hex(&[0x00, 0x0f, 0xab]), "000FAB");
    }

    #[test]
    fn small_money_uses_same_scale() {
        let v = ColumnValues::SmallMoney(SqlSmallMoney { int_val: 25_000 });
        assert_eq!(column_value_to_text(&v).unwrap(), "2.5000");
    }

    #[test]
    fn date_value_routes_through_conversion() {
        let v = ColumnValues::Date(SqlDate::create(738_944).unwrap());
        assert_eq!(column_value_to_text(&v).unwrap(), "2024-02-29");
    }
}
