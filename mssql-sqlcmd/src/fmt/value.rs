// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rendering a decoded column value as the text the reference prints.
//!
//! The awkward cases — `float`'s 17 significant digits, uppercase hex and
//! GUIDs, `money`'s four decimals — were all read off the shipped binary.

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;

use crate::compat::Compat;

/// What the reference prints for a NULL, in every type.
pub const NULL_TEXT: &str = "NULL";

const SECONDS_PER_DAY: i64 = 86_400;
/// `SqlTime::time_nanoseconds` is named for nanoseconds but the driver stores
/// 100-nanosecond ticks, as its own decoding comment says.
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Days between 0001-01-01 and 1900-01-01. `date` and `datetime2` count from
/// the former, `datetime` and `smalldatetime` from the latter.
const DAYS_FROM_YEAR_ONE_TO_1900: i64 = 693_595;

pub fn render(value: &ColumnValues, column: &ColumnMetadata, compat: Compat) -> String {
    match value {
        ColumnValues::Null => NULL_TEXT.to_string(),
        ColumnValues::Bit(b) => i32::from(*b).to_string(),
        ColumnValues::TinyInt(v) => v.to_string(),
        ColumnValues::SmallInt(v) => v.to_string(),
        ColumnValues::Int(v) => v.to_string(),
        ColumnValues::BigInt(v) => v.to_string(),
        // ODBC prints a fixed number of significant digits; go-sqlcmd prints the
        // shortest text that reads back as the same value.
        ColumnValues::Real(v) => {
            if compat.is_go() {
                shortest_float(*v as f64)
            } else {
                float_text(*v as f64, 9)
            }
        }
        ColumnValues::Float(v) => {
            if compat.is_go() {
                shortest_float(*v)
            } else {
                float_text(*v, 17)
            }
        }
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => d.to_string(),
        ColumnValues::Money(m) => money_text(money_units(m), compat),
        ColumnValues::SmallMoney(m) => money_text(m.int_val as i64, compat),
        // `SqlString`'s `Display` prints raw bytes for collation-encoded data,
        // so decode explicitly.
        ColumnValues::String(s) => s.to_utf8_string(),
        ColumnValues::Xml(x) => x.as_string(),
        ColumnValues::Json(j) => j.as_string(),
        ColumnValues::Uuid(u) => {
            let text = u.to_string();
            if compat.is_go() {
                text
            } else {
                text.to_ascii_uppercase()
            }
        }
        ColumnValues::Bytes(bytes) => binary_text(bytes, fixed_binary_len(column)),
        ColumnValues::Date(d) => date_text(d.get_days() as i64),
        ColumnValues::Time(t) => time_text(t.time_nanoseconds, t.scale),
        ColumnValues::DateTime(dt) => datetime_text(dt.days as i64, dt.time),
        ColumnValues::SmallDateTime(dt) => smalldatetime_text(dt.days as i64, dt.time),
        ColumnValues::DateTime2(dt) => datetime2_text(dt.days as i64, &dt.time),
        ColumnValues::DateTimeOffset(dto) => {
            // The driver reports the instant in UTC; the reference shows local
            // time alongside the offset that produced it.
            let ticks = dto.datetime2.time.time_nanoseconds as i64
                + dto.offset as i64 * 60 * TICKS_PER_SECOND as i64;
            let day_ticks = seconds_per_day() * TICKS_PER_SECOND as i64;
            let days = dto.datetime2.days as i64 + ticks.div_euclid(day_ticks);
            let local = ticks.rem_euclid(day_ticks) as u64;
            format!(
                "{} {} {}",
                date_text(days),
                time_text(local, dto.datetime2.time.scale),
                offset_text(dto.offset)
            )
        }
        ColumnValues::Vector(v) => format!("{v:?}"),
    }
}

/// `binary(n)` is zero-padded to its declared length; `varbinary` is not.
fn fixed_binary_len(column: &ColumnMetadata) -> Option<usize> {
    matches!(
        column.data_type,
        TdsDataType::Binary | TdsDataType::BigBinary
    )
    .then_some(column.type_info.length)
}

fn binary_text(bytes: &[u8], pad_to: Option<usize>) -> String {
    let mut out = String::from("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    if let Some(len) = pad_to {
        while out.len() < len * 2 + 2 {
            out.push('0');
        }
    }
    out
}

/// SQL Server stores money scaled by 10^4 in two 32-bit halves, high word first.
fn money_units(m: &mssql_tds::datatypes::column_values::SqlMoney) -> i64 {
    ((m.msb_part as i64) << 32) | (m.lsb_part as u32 as i64)
}

/// Four decimals always. ODBC drops the leading zero on values below one —
/// `.0000` rather than `0.0000` — where go-sqlcmd keeps it.
fn money_text(units: i64, compat: Compat) -> String {
    let negative = units < 0;
    let magnitude = units.unsigned_abs();
    let whole = magnitude / 10_000;
    let frac = magnitude % 10_000;
    let sign = if negative { "-" } else { "" };
    if whole == 0 && !compat.is_go() {
        format!("{sign}.{frac:04}")
    } else {
        format!("{sign}{whole}.{frac:04}")
    }
}

/// The shortest decimal text that parses back to the same `f64`, which is what
/// Go's `strconv.FormatFloat(_, 'g', -1, 64)` produces.
fn shortest_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    // Rust's `Display` for floats is already shortest-roundtrip, but it writes
    // an integral value as `1` where Go's `%g` agrees, so it can be used as-is.
    let text = format!("{value}");
    // Go switches to exponent form outside the same range `%g` uses.
    let exponent = if value == 0.0 {
        0
    } else {
        value.abs().log10().floor() as i32
    };
    if !(-4..21).contains(&exponent) {
        return format!("{value:e}");
    }
    text
}

/// Reproduces the C `%.*g` rendering the reference uses, including its habit of
/// writing an integral float as `1.0` and its `E+300` exponent form.
fn float_text(value: f64, significant: usize) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Inf" } else { "-Inf" }.to_string();
    }
    if value == 0.0 {
        return "0.0".to_string();
    }

    // Round to the requested significant digits first, then decide on the form,
    // so that a value which rounds up into a new decade is classified the way C
    // classifies it.
    let scientific = format!("{:.*e}", significant.saturating_sub(1), value);
    let (mantissa, exponent) = split_exponent(&scientific);

    if exponent < -4 || exponent >= significant as i32 {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{}E{sign}{}", trim_fraction(mantissa), exponent.abs());
    }

    let decimals = (significant as i32 - 1 - exponent).max(0) as usize;
    trim_fraction(&format!("{value:.decimals$}"))
}

fn split_exponent(scientific: &str) -> (&str, i32) {
    match scientific.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse().unwrap_or(0)),
        None => (scientific, 0),
    }
}

/// Drops trailing zeros but keeps one decimal place, matching `1.0` and `0.5`.
fn trim_fraction(text: &str) -> String {
    if !text.contains('.') {
        return format!("{text}.0");
    }
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

/// Days are counted from 0001-01-01 for `date`/`datetime2` and from 1900-01-01
/// for `datetime`/`smalldatetime`.
fn date_text(days_from_year_one: i64) -> String {
    let (y, m, d) = civil_from_days(days_from_year_one);
    format!("{y:04}-{m:02}-{d:02}")
}

fn time_text(ticks: u64, scale: u8) -> String {
    let total_seconds = ticks / TICKS_PER_SECOND;
    let h = total_seconds / 3600;
    let m = (total_seconds % 3600) / 60;
    let s = total_seconds % 60;
    let base = format!("{h:02}:{m:02}:{s:02}");
    if scale == 0 {
        return base;
    }
    let fraction = ticks % TICKS_PER_SECOND;
    let digits = fraction / 10u64.pow(7 - scale.min(7) as u32);
    format!("{base}.{digits:0width$}", width = scale as usize)
}

/// `datetime` counts time in 1/300-second ticks and always shows milliseconds.
fn datetime_text(days_from_1900: i64, ticks: u32) -> String {
    let date = date_text(days_from_1900 + DAYS_FROM_YEAR_ONE_TO_1900);
    let total_ms = (ticks as u64 * 1000 + 150) / 300;
    let h = total_ms / 3_600_000;
    let m = (total_ms % 3_600_000) / 60_000;
    let s = (total_ms % 60_000) / 1000;
    let ms = total_ms % 1000;
    format!("{date} {h:02}:{m:02}:{s:02}.{ms:03}")
}

fn smalldatetime_text(days_from_1900: i64, minutes: u16) -> String {
    let date = date_text(days_from_1900 + DAYS_FROM_YEAR_ONE_TO_1900);
    format!("{date} {:02}:{:02}:00", minutes / 60, minutes % 60)
}

fn datetime2_text(
    days_from_year_one: i64,
    time: &mssql_tds::datatypes::column_values::SqlTime,
) -> String {
    format!(
        "{} {}",
        date_text(days_from_year_one),
        time_text(time.time_nanoseconds, time.scale)
    )
}

fn offset_text(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let magnitude = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", magnitude / 60, magnitude % 60)
}

/// Howard Hinnant's civil-from-days, shifted so day 0 is 0001-01-01.
fn civil_from_days(days_from_year_one: i64) -> (i64, u32, u32) {
    // 719_162 days separate 0001-01-01 from the 1970-01-01 epoch this uses.
    let z = days_from_year_one - 719_162 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Total seconds in a day, used to fold a datetimeoffset across midnight.
const fn seconds_per_day() -> i64 {
    SECONDS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_carry_seventeen_significant_digits() {
        assert_eq!(float_text(3.25, 17), "3.25");
        assert_eq!(float_text(0.1, 17), "0.10000000000000001");
        assert_eq!(float_text(1.0, 17), "1.0");
        assert_eq!(float_text(0.5, 17), "0.5");
        assert_eq!(float_text(1e300, 17), "1.0000000000000001E+300");
        assert_eq!(float_text(-0.000001, 17), "-9.9999999999999995E-7");
    }

    #[test]
    fn reals_carry_nine() {
        assert_eq!(float_text(3.140000104904175, 9), "3.1400001");
    }
    #[test]
    fn money_shows_four_decimals_and_drops_a_leading_zero() {
        assert_eq!(money_text(15_000, Compat::Odbc), "1.5000");
        assert_eq!(money_text(-15_000, Compat::Odbc), "-1.5000");
        assert_eq!(money_text(0, Compat::Odbc), ".0000");
        assert_eq!(money_text(5_000, Compat::Odbc), ".5000");
    }

    #[test]
    fn binary_is_uppercase_and_fixed_width_is_zero_padded() {
        assert_eq!(binary_text(&[0xDE, 0xAD, 0xBE, 0xEF], None), "0xDEADBEEF");
        assert_eq!(binary_text(&[0xAB], Some(4)), "0xAB000000");
    }

    #[test]
    fn dates_round_trip_through_the_civil_calendar() {
        // Day 0 is 0001-01-01, which is how SQL Server counts `date`.
        assert_eq!(date_text(0), "0001-01-01");
        assert_eq!(date_text(738_886), "2024-01-02");
        assert_eq!(date_text(730_119), "2000-01-01");
    }

    #[test]
    fn time_honours_its_scale() {
        let ticks = (12 * 3600 + 34 * 60 + 56) as u64 * TICKS_PER_SECOND + 1_234_567;
        assert_eq!(time_text(ticks, 7), "12:34:56.1234567");
        assert_eq!(time_text(ticks, 3), "12:34:56.123");
        assert_eq!(time_text(ticks, 0), "12:34:56");
    }

    #[test]
    fn datetime_rounds_ticks_to_milliseconds() {
        // 1/300s ticks: 300 ticks == 1 second.
        assert_eq!(datetime_text(0, 0), "1900-01-01 00:00:00.000");
        assert_eq!(datetime_text(0, 300), "1900-01-01 00:00:01.000");
    }

    #[test]
    fn offsets_are_signed_and_padded() {
        assert_eq!(offset_text(330), "+05:30");
        assert_eq!(offset_text(-330), "-05:30");
        assert_eq!(offset_text(0), "+00:00");
    }
}
