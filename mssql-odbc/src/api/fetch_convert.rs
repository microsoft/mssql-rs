// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared fetch conversion core: `ColumnValues` → a requested `SQL_C_*` target
//! buffer.
//!
//! This is the value-level conversion used by both `SQLGetData` (row-by-row)
//! and, later, `SQLBindCol` block fetch (P3). It is deliberately free of any
//! statement-handle or diagnostic-list coupling: it writes the converted value
//! into the caller's buffer and reports success/failure through [`ConvError`],
//! leaving the SQLSTATE posting to the caller (whose diagnostic target differs
//! between the two fetch paths).
//!
//! Scope: the fixed-width integer C targets, floating-point targets
//! (`SQL_C_FLOAT` / `SQL_C_DOUBLE`), `SQL_C_GUID`, and the date/time C structs
//! (`SQL_C_TYPE_DATE` / `TIME` / `TIMESTAMP`, `SQL_C_SS_TIME2`,
//! `SQL_C_SS_TIMESTAMPOFFSET`), plus an ISO-style text formatter for date/time
//! character output. Unhandled source/target pairs return
//! [`ConvError::NotHandledHere`] so callers can fall back to their existing paths
//! (e.g. the character conversion in `get_data`).

use super::odbc_types::{
    SQL_C_BIT, SQL_C_DATE, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_LONG, SQL_C_SBIGINT,
    SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_STINYINT, SQL_C_TIME, SQL_C_TIMESTAMP, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME,
    SQL_C_TYPE_TIMESTAMP, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SqlDateStruct,
    SqlGuid, SqlLen, SqlPointer, SqlSmallInt, SqlSsTime2Struct, SqlSsTimestampoffsetStruct,
    SqlTimeStruct, SqlTimestampStruct,
};
use super::util::write_if_some;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};

/// Decodes a character column without the panicking paths in
/// `SqlString::to_utf8_string` (its UTF-8 branch unwraps); the UTF-16 and LCID
/// branches decode through `encoding_rs`, which substitutes replacement
/// characters rather than failing.
pub(crate) fn sql_string_to_text(s: &SqlString) -> Option<String> {
    match s.encoding_type() {
        EncodingType::Utf8 => String::from_utf8(s.bytes.clone()).ok(),
        _ => Some(s.to_utf8_string()),
    }
}

/// Outcome of a successful conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvOk {
    /// The value was represented exactly.
    Exact,
    /// Precision was lost (e.g. a date/time component the target cannot hold).
    /// The caller posts `01S07` and returns `SQL_SUCCESS_WITH_INFO`.
    Truncated,
}

/// Why a value-level conversion could not be completed. The caller maps each
/// variant to the appropriate SQLSTATE on its own diagnostic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvError {
    /// The value does not fit the requested C type (SQLSTATE `22003`).
    OutOfRange,
    /// This source/target pairing is not handled by this converter; the caller
    /// should try another path. Never surfaced to the application directly.
    NotHandledHere,
    /// The requested C type is not a legal target for this SQL type
    /// (SQLSTATE `07006`). Terminal.
    Restricted,
    /// A character column's text is not a valid literal for the requested target
    /// (SQLSTATE `22018`). Terminal.
    InvalidCharacterValue,
}

/// Returns `true` if `target_type` is one of the fixed-width integer C types
/// handled by [`convert_integer_c`]. Lets a caller decide whether to route a
/// request here before it has a value in hand.
pub(crate) fn is_integer_c_target(target_type: SqlSmallInt) -> bool {
    matches!(
        target_type,
        SQL_C_STINYINT
            | SQL_C_TINYINT
            | SQL_C_UTINYINT
            | SQL_C_SSHORT
            | SQL_C_SHORT
            | SQL_C_USHORT
            | SQL_C_SLONG
            | SQL_C_LONG
            | SQL_C_ULONG
            | SQL_C_SBIGINT
            | SQL_C_UBIGINT
            | SQL_C_BIT
    )
}

/// Writes a `Copy` value of type `T` to `ptr` (when non-null) and sets the
/// indicator to `size_of::<T>()`.
///
/// # Safety
/// `ptr`, when non-null, must be valid for a write of `size_of::<T>()` bytes.
/// The write is unaligned-safe. `ind` follows the same contract as
/// [`write_if_some`].
unsafe fn write_fixed<T: Copy>(ptr: SqlPointer, value: T, ind: *mut SqlLen) -> ConvOk {
    if !ptr.is_null() {
        unsafe { (ptr as *mut T).write_unaligned(value) };
    }
    unsafe { write_if_some(ind, std::mem::size_of::<T>() as SqlLen) };
    ConvOk::Exact
}

/// Converts an integer column value to a fixed-width integer C target,
/// range-checking against the target type.
///
/// Returns [`ConvError::NotHandledHere`] when either the source is not an integer
/// column or the target is not a fixed-width integer C type, letting the caller
/// fall back to another conversion path.
///
/// # Safety
/// `target_value_ptr`, when non-null, must be valid for a write of the target
/// C type's size, and `strlen_or_ind_ptr` must be null or valid for a
/// `SqlLen` write.
/// A numeric column value in a form that keeps exact sources exact, so an
/// integer target can report truncation instead of silently dropping a
/// fraction.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NumericSource {
    Int(i128),
    /// `mantissa / 10^scale` — the exact decimal types (`decimal`, `numeric`,
    /// `money`, `smallmoney`) and decimal literals in character columns.
    Scaled {
        mantissa: i128,
        scale: u32,
    },
    Float(f64),
}

impl NumericSource {
    fn as_f64(&self) -> f64 {
        match self {
            NumericSource::Int(v) => *v as f64,
            NumericSource::Scaled { mantissa, scale } => {
                *mantissa as f64 / 10f64.powi(*scale as i32)
            }
            NumericSource::Float(f) => *f,
        }
    }

    /// Value truncated toward zero plus whether a fractional part was dropped.
    /// `None` when the value cannot be represented as an integer at all.
    fn to_i128_truncating(self) -> Option<(i128, bool)> {
        match self {
            NumericSource::Int(v) => Some((v, false)),
            NumericSource::Scaled { mantissa, scale } => {
                let divisor = 10i128.checked_pow(scale)?;
                Some((mantissa / divisor, mantissa % divisor != 0))
            }
            NumericSource::Float(f) => {
                if !f.is_finite() || !(-1.7e38..=1.7e38).contains(&f) {
                    return None;
                }
                Some((f.trunc() as i128, f.fract() != 0.0))
            }
        }
    }
}

/// Parses a plain decimal literal (`-12.34`, `+7`, `.5`) into an exact
/// [`NumericSource::Scaled`]. Exponent forms are left to the `f64` fallback.
fn parse_decimal_literal(text: &str) -> Option<NumericSource> {
    let t = text.trim();
    let (negative, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_digits, frac_digits) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    if !int_digits
        .bytes()
        .chain(frac_digits.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mantissa: i128 = format!("{int_digits}{frac_digits}").parse().ok()?;
    Some(NumericSource::Scaled {
        mantissa: if negative { -mantissa } else { mantissa },
        scale: frac_digits.len() as u32,
    })
}

/// The `money` / `smallmoney` wire value as an integer scaled by 10^4.
fn money_scaled(lsb: i32, msb: i32) -> i128 {
    i128::from((i64::from(lsb) & 0xFFFF_FFFF) | (i64::from(msb) << 32))
}

/// Interprets a column as a number, or `None` when the column type has no
/// numeric interpretation.
fn numeric_source(value: &ColumnValues) -> Option<NumericSource> {
    match value {
        ColumnValues::TinyInt(x) => Some(NumericSource::Int(i128::from(*x))),
        ColumnValues::SmallInt(x) => Some(NumericSource::Int(i128::from(*x))),
        ColumnValues::Int(x) => Some(NumericSource::Int(i128::from(*x))),
        ColumnValues::BigInt(x) => Some(NumericSource::Int(i128::from(*x))),
        ColumnValues::Bit(b) => Some(NumericSource::Int(i128::from(*b))),
        ColumnValues::Real(x) => Some(NumericSource::Float(f64::from(*x))),
        ColumnValues::Float(x) => Some(NumericSource::Float(*x)),
        // `DecimalParts` renders itself exactly; parse that back rather than
        // reassembling its base-2^32 limbs.
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => {
            parse_decimal_literal(&d.to_string())
        }
        ColumnValues::Money(m) => Some(NumericSource::Scaled {
            mantissa: money_scaled(m.lsb_part, m.msb_part),
            scale: 4,
        }),
        ColumnValues::SmallMoney(m) => Some(NumericSource::Scaled {
            mantissa: i128::from(m.int_val),
            scale: 4,
        }),
        ColumnValues::String(_) => None,
        _ => None,
    }
}

/// Interprets a column as a number, including character columns holding a
/// numeric literal. `Err` distinguishes "not a numeric column" from "character
/// column whose text is not a valid number" (`22018`).
fn numeric_source_or_parse(value: &ColumnValues) -> Result<NumericSource, ConvError> {
    if let ColumnValues::String(s) = value {
        let text = sql_string_to_text(s).ok_or(ConvError::InvalidCharacterValue)?;
        return parse_decimal_literal(&text)
            .or_else(|| text.trim().parse::<f64>().ok().map(NumericSource::Float))
            .ok_or(ConvError::InvalidCharacterValue);
    }
    numeric_source(value).ok_or(ConvError::Restricted)
}

pub(crate) unsafe fn convert_integer_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<ConvOk, ConvError> {
    // Reject an unhandled target before interpreting the value, so the caller
    // can still route elsewhere.
    if !is_integer_c_target(target_type) {
        return Err(ConvError::NotHandledHere);
    }
    let source = numeric_source_or_parse(value)?;
    let (v, truncated) = source.to_i128_truncating().ok_or(ConvError::OutOfRange)?;

    // Helper: narrow `v` to the target's range or fail with OutOfRange.
    macro_rules! narrow {
        ($ty:ty) => {{ <$ty>::try_from(v).map_err(|_| ConvError::OutOfRange)? }};
    }

    match target_type {
        // SQL_C_TINYINT maps to an unsigned SQLCHAR (SQL Server `tinyint` is
        // 0-255 and mssql-python fetches it unsigned); only SQL_C_STINYINT is
        // the signed form.
        SQL_C_STINYINT => unsafe { write_fixed(target_value_ptr, narrow!(i8), strlen_or_ind_ptr) },
        SQL_C_TINYINT | SQL_C_UTINYINT => unsafe {
            write_fixed(target_value_ptr, narrow!(u8), strlen_or_ind_ptr)
        },
        SQL_C_SSHORT | SQL_C_SHORT => unsafe {
            write_fixed(target_value_ptr, narrow!(i16), strlen_or_ind_ptr)
        },
        SQL_C_USHORT => unsafe { write_fixed(target_value_ptr, narrow!(u16), strlen_or_ind_ptr) },
        SQL_C_SLONG | SQL_C_LONG => unsafe {
            write_fixed(target_value_ptr, narrow!(i32), strlen_or_ind_ptr)
        },
        SQL_C_ULONG => unsafe { write_fixed(target_value_ptr, narrow!(u32), strlen_or_ind_ptr) },
        SQL_C_SBIGINT => unsafe { write_fixed(target_value_ptr, narrow!(i64), strlen_or_ind_ptr) },
        SQL_C_UBIGINT => unsafe { write_fixed(target_value_ptr, narrow!(u64), strlen_or_ind_ptr) },
        // A bit target only accepts 0 or 1; any other value is out of range.
        SQL_C_BIT => {
            let b: u8 = match v {
                0 => 0,
                1 => 1,
                _ => return Err(ConvError::OutOfRange),
            };
            unsafe { write_fixed(target_value_ptr, b, strlen_or_ind_ptr) }
        }
        _ => return Err(ConvError::NotHandledHere),
    };
    Ok(if truncated {
        ConvOk::Truncated
    } else {
        ConvOk::Exact
    })
}

/// Widen a numeric column (integer or floating) to `f64`. Returns `None` for
/// non-numeric sources.
/// Returns `true` if `target_type` is one of the floating-point C types handled
/// by [`convert_float_c`].
pub(crate) fn is_float_c_target(target_type: SqlSmallInt) -> bool {
    matches!(target_type, SQL_C_FLOAT | SQL_C_DOUBLE)
}

/// Converts a numeric column value to a floating-point C target.
///
/// Returns [`ConvError::NotHandledHere`] when the source is not numeric or the
/// target is not a floating-point C type.
///
/// # Safety
/// Same pointer contract as [`convert_integer_c`].
pub(crate) unsafe fn convert_float_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<ConvOk, ConvError> {
    if !is_float_c_target(target_type) {
        return Err(ConvError::NotHandledHere);
    }
    let v = numeric_source_or_parse(value)?.as_f64();
    let ret = match target_type {
        // SQL_C_FLOAT is 32-bit. A finite value outside the f32 range must be
        // reported as an overflow (22003) rather than silently becoming
        // infinity; a source that is already infinite passes through.
        SQL_C_FLOAT => {
            if v.is_finite() && v.abs() > f64::from(f32::MAX) {
                return Err(ConvError::OutOfRange);
            }
            unsafe { write_fixed(target_value_ptr, v as f32, strlen_or_ind_ptr) }
        }
        SQL_C_DOUBLE => unsafe { write_fixed(target_value_ptr, v, strlen_or_ind_ptr) },
        _ => return Err(ConvError::NotHandledHere),
    };
    Ok(ret)
}

/// Converts a `uniqueidentifier` column to a `SQL_C_GUID` (`SQLGUID`) target.
///
/// # Safety
/// Same pointer contract as [`convert_integer_c`].
pub(crate) unsafe fn convert_guid_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<ConvOk, ConvError> {
    if target_type != SQL_C_GUID {
        return Err(ConvError::NotHandledHere);
    }
    // The target is SQL_C_GUID but the column is not a uniqueidentifier: an
    // illegal cast rather than a gap in this converter.
    let ColumnValues::Uuid(u) = value else {
        return Err(ConvError::Restricted);
    };
    // `Uuid::as_fields` yields the GUID components in the same host-order layout
    // as `SQLGUID` (data1/data2/data3 native-endian, data4 as raw bytes).
    let (data1, data2, data3, data4) = u.as_fields();
    let guid = SqlGuid {
        data1,
        data2,
        data3,
        data4: *data4,
    };
    Ok(unsafe { write_fixed(target_value_ptr, guid, strlen_or_ind_ptr) })
}

/// Days from 0001-01-01 (proleptic Gregorian) to 1900-01-01, used to rebase the
/// `datetime` / `smalldatetime` epoch onto the common day-0 = 0001-01-01 axis.
const DAYS_0001_TO_1900: i64 = 693_595;

/// A normalized calendar breakdown shared by every date/time column type, so
/// each target C struct can be filled from a single representation.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DateTimeParts {
    pub year: i16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    /// Fractional seconds in nanoseconds.
    pub fraction_ns: u32,
    /// Declared fractional-seconds scale (0-7) of the source column. Character
    /// rendering pads to exactly this many digits, matching msodbcsql.
    pub scale: u8,
    pub tz_hour: i16,
    pub tz_minute: i16,
    pub has_date: bool,
    pub has_time: bool,
    pub has_tz: bool,
}

/// (year, month, day) from a day count where day 0 = 0001-01-01, using Howard
/// Hinnant's `civil_from_days` algorithm rebased from its 1970 epoch.
fn civil_from_days_since_0001(days_since_0001: i64) -> (i16, u16, u16) {
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

/// Number of 100 ns ticks in one day.
const TICKS_PER_DAY: i64 = 864_000_000_000;

/// Day number of `9999-12-31`, the maximum SQL Server date. Used to reject a
/// `datetimeoffset` whose offset adjustment would leave the representable range.
const MAX_DAYS_SINCE_0001: i64 = 3_652_058;

/// (hour, minute, second, fraction_ns) from 100-nanosecond ticks since midnight.
///
/// `SqlTime::time_nanoseconds` is a misnomer: the decoder normalizes every
/// fractional-seconds scale to 100 ns ticks, not nanoseconds.
fn hms_from_ticks_100ns(ticks: u64) -> (u16, u16, u16, u32) {
    let secs = ticks / 10_000_000;
    let fraction_ns = ((ticks % 10_000_000) * 100) as u32;
    (
        (secs / 3600) as u16,
        ((secs % 3600) / 60) as u16,
        (secs % 60) as u16,
        fraction_ns,
    )
}

/// Extracts a [`DateTimeParts`] from any date/time column value, or `None` for
/// non-temporal sources.
pub(crate) fn extract_datetime_parts(value: &ColumnValues) -> Option<DateTimeParts> {
    let mut p = DateTimeParts::default();
    match value {
        ColumnValues::Date(d) => {
            let (y, m, day) = civil_from_days_since_0001(i64::from(d.get_days()));
            p.year = y;
            p.month = m;
            p.day = day;
            p.has_date = true;
        }
        ColumnValues::Time(t) => {
            let (h, mi, s, f) = hms_from_ticks_100ns(t.time_nanoseconds);
            p.scale = t.scale;
            p.hour = h;
            p.minute = mi;
            p.second = s;
            p.fraction_ns = f;
            p.has_time = true;
        }
        ColumnValues::DateTime2(dt) => {
            let (y, m, day) = civil_from_days_since_0001(i64::from(dt.days));
            let (h, mi, s, f) = hms_from_ticks_100ns(dt.time.time_nanoseconds);
            p.scale = dt.time.scale;
            p.year = y;
            p.month = m;
            p.day = day;
            p.hour = h;
            p.minute = mi;
            p.second = s;
            p.fraction_ns = f;
            p.has_date = true;
            p.has_time = true;
        }
        ColumnValues::DateTimeOffset(dto) => {
            // The wire value is UTC; `offset` is what must be added to reach the
            // local wall clock the application wrote, which is what the ODBC
            // struct and the character rendering report.
            let utc_ticks = dto.datetime2.time.time_nanoseconds as i64
                + i64::from(dto.offset) * 60 * 10_000_000;
            // Euclidean division so a negative offset borrows a day rather than
            // producing a negative time-of-day.
            let days = i64::from(dto.datetime2.days) + utc_ticks.div_euclid(TICKS_PER_DAY);
            if !(0..=MAX_DAYS_SINCE_0001).contains(&days) {
                return None;
            }
            let (y, m, day) = civil_from_days_since_0001(days);
            let (h, mi, s, f) = hms_from_ticks_100ns(utc_ticks.rem_euclid(TICKS_PER_DAY) as u64);
            p.scale = dto.datetime2.time.scale;
            p.year = y;
            p.month = m;
            p.day = day;
            p.hour = h;
            p.minute = mi;
            p.second = s;
            p.fraction_ns = f;
            p.tz_hour = dto.offset / 60;
            p.tz_minute = dto.offset % 60;
            p.has_date = true;
            p.has_time = true;
            p.has_tz = true;
        }
        ColumnValues::DateTime(dt) => {
            let (y, m, day) = civil_from_days_since_0001(i64::from(dt.days) + DAYS_0001_TO_1900);
            // `datetime` time is counted in 1/300-second ticks since midnight.
            let ticks = u64::from(dt.time);
            let secs = ticks / 300;
            let fraction_ns = ((ticks % 300) * 1_000_000_000 / 300) as u32;
            p.year = y;
            p.month = m;
            p.day = day;
            p.hour = (secs / 3600) as u16;
            p.minute = ((secs % 3600) / 60) as u16;
            p.second = (secs % 60) as u16;
            p.fraction_ns = fraction_ns;
            // `datetime` always renders 3 fractional digits.
            p.scale = 3;
            p.has_date = true;
            p.has_time = true;
        }
        ColumnValues::SmallDateTime(dt) => {
            let (y, m, day) = civil_from_days_since_0001(i64::from(dt.days) + DAYS_0001_TO_1900);
            p.year = y;
            p.month = m;
            p.day = day;
            p.hour = dt.time / 60;
            p.minute = dt.time % 60;
            p.has_date = true;
            p.has_time = true;
        }
        _ => return None,
    }
    Some(p)
}

/// Returns `true` if `target_type` is one of the date/time C struct targets
/// handled by [`convert_datetime_c`].
pub(crate) fn is_datetime_c_target(target_type: SqlSmallInt) -> bool {
    matches!(
        target_type,
        SQL_C_TYPE_DATE
            | SQL_C_DATE
            | SQL_C_TYPE_TIME
            | SQL_C_TIME
            | SQL_C_SS_TIME2
            | SQL_C_TYPE_TIMESTAMP
            | SQL_C_TIMESTAMP
            | SQL_C_SS_TIMESTAMPOFFSET
    )
}

/// Converts a date/time column value to the requested date/time C struct.
///
/// # Safety
/// Same pointer contract as [`convert_integer_c`]; `target_value_ptr` must be
/// valid for a write of the target struct's size.
/// Parses `YYYY-MM-DD`.
fn parse_date_literal(s: &str) -> Option<(i16, u16, u16)> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 {
        return None;
    }
    let year: i16 = y.parse().ok()?;
    let month: u16 = m.parse().ok()?;
    let day: u16 = d.parse().ok()?;
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Parses `HH:MM[:SS[.fffffff]]`, returning the components plus the number of
/// fractional digits written (the effective scale).
fn parse_time_literal(s: &str) -> Option<(u16, u16, u16, u32, u8)> {
    let mut it = s.split(':');
    let hour: u16 = it.next()?.parse().ok()?;
    let minute: u16 = it.next()?.parse().ok()?;
    let sec_part = it.next().unwrap_or("0");
    if it.next().is_some() {
        return None;
    }
    let (sec_digits, frac_digits) = match sec_part.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_part, ""),
    };
    let second: u16 = sec_digits.parse().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if !frac_digits.is_empty() && !frac_digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Normalize the fraction to 100 ns resolution (7 digits).
    let scale = frac_digits.len().min(7);
    let mut hundred_ns: u32 = 0;
    for i in 0..7 {
        let digit = frac_digits
            .as_bytes()
            .get(i)
            .map_or(0, |b| u32::from(b - b'0'));
        hundred_ns = hundred_ns * 10 + digit;
    }
    Some((hour, minute, second, hundred_ns * 100, scale as u8))
}

/// Parses the character forms of `date`, `time`, `datetime2` and
/// `datetimeoffset` into [`DateTimeParts`].
fn parse_datetime_literal(text: &str) -> Option<DateTimeParts> {
    let mut s = text.trim();
    let mut p = DateTimeParts::default();

    // A trailing "+HH:MM" / "-HH:MM" is a UTC offset. Match it only in that
    // exact shape so the hyphens inside a date are never mistaken for one.
    if s.len() >= 6 {
        let tail = &s[s.len() - 6..];
        let tb = tail.as_bytes();
        if (tb[0] == b'+' || tb[0] == b'-') && tb[3] == b':' {
            let sign: i16 = if tb[0] == b'+' { 1 } else { -1 };
            let hh: i16 = tail[1..3].parse().ok()?;
            let mm: i16 = tail[4..6].parse().ok()?;
            if hh > 14 || mm > 59 {
                return None;
            }
            p.tz_hour = sign * hh;
            p.tz_minute = sign * mm;
            p.has_tz = true;
            s = s[..s.len() - 6].trim_end();
        }
    }

    let (date_str, time_str) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (Some(d), Some(t.trim())),
        None if s.contains(':') => (None, Some(s)),
        None => (Some(s), None),
    };

    if let Some(d) = date_str {
        let (y, m, day) = parse_date_literal(d)?;
        p.year = y;
        p.month = m;
        p.day = day;
        p.has_date = true;
    }
    if let Some(t) = time_str.filter(|t| !t.is_empty()) {
        let (h, mi, sec, frac_ns, scale) = parse_time_literal(t)?;
        p.hour = h;
        p.minute = mi;
        p.second = sec;
        p.fraction_ns = frac_ns;
        p.scale = scale;
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

pub(crate) unsafe fn convert_datetime_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<ConvOk, ConvError> {
    let p = match value {
        // A character column must hold a valid literal for the target.
        ColumnValues::String(s) => {
            let text = sql_string_to_text(s).ok_or(ConvError::InvalidCharacterValue)?;
            parse_datetime_literal(&text).ok_or(ConvError::InvalidCharacterValue)?
        }
        // A date/time C target for a non-temporal column is illegal.
        _ => extract_datetime_parts(value).ok_or(ConvError::Restricted)?,
    };
    let ret = match target_type {
        SQL_C_TYPE_DATE | SQL_C_DATE if p.has_date => {
            let written = unsafe {
                write_fixed(
                    target_value_ptr,
                    SqlDateStruct {
                        year: p.year,
                        month: p.month,
                        day: p.day,
                    },
                    strlen_or_ind_ptr,
                )
            };
            // Dropping a non-zero time component is a truncation.
            if p.has_time && ((p.hour | p.minute | p.second) != 0 || p.fraction_ns != 0) {
                ConvOk::Truncated
            } else {
                written
            }
        }
        SQL_C_TYPE_TIME | SQL_C_TIME if p.has_time => {
            let written = unsafe {
                write_fixed(
                    target_value_ptr,
                    SqlTimeStruct {
                        hour: p.hour,
                        minute: p.minute,
                        second: p.second,
                    },
                    strlen_or_ind_ptr,
                )
            };
            // SQL_TIME_STRUCT has no fractional field.
            if p.fraction_ns != 0 {
                ConvOk::Truncated
            } else {
                written
            }
        }
        SQL_C_SS_TIME2 if p.has_time => unsafe {
            write_fixed(
                target_value_ptr,
                SqlSsTime2Struct {
                    hour: p.hour,
                    minute: p.minute,
                    second: p.second,
                    fraction: p.fraction_ns,
                },
                strlen_or_ind_ptr,
            )
        },
        SQL_C_TYPE_TIMESTAMP | SQL_C_TIMESTAMP if p.has_date => unsafe {
            write_fixed(
                target_value_ptr,
                SqlTimestampStruct {
                    year: p.year,
                    month: p.month,
                    day: p.day,
                    hour: p.hour,
                    minute: p.minute,
                    second: p.second,
                    fraction: p.fraction_ns,
                },
                strlen_or_ind_ptr,
            )
        },
        SQL_C_SS_TIMESTAMPOFFSET if p.has_date => unsafe {
            write_fixed(
                target_value_ptr,
                SqlSsTimestampoffsetStruct {
                    year: p.year,
                    month: p.month,
                    day: p.day,
                    hour: p.hour,
                    minute: p.minute,
                    second: p.second,
                    fraction: p.fraction_ns,
                    timezone_hour: p.tz_hour,
                    timezone_minute: p.tz_minute,
                },
                strlen_or_ind_ptr,
            )
        },
        // Reached when the value lacks the component the target needs (e.g. a
        // `time` column into `SQL_C_TYPE_DATE`).
        _ => return Err(ConvError::Restricted),
    };
    Ok(ret)
}

/// Formats a [`DateTimeParts`] as an ISO-8601-style string for character
/// targets. Fractional seconds are rendered in 100 ns units (SQL Server's max
/// scale of 7 digits) with trailing zeros trimmed; a zero fraction is omitted.
pub(crate) fn format_datetime_parts(p: &DateTimeParts) -> String {
    let mut s = String::new();
    if p.has_date {
        s.push_str(&format!("{:04}-{:02}-{:02}", p.year, p.month, p.day));
    }
    if p.has_time {
        if p.has_date {
            s.push(' ');
        }
        s.push_str(&format!("{:02}:{:02}:{:02}", p.hour, p.minute, p.second));
        // Pad to the column's declared scale so every row of a column renders
        // the same width, matching msodbcsql (which applications rely on when
        // sizing buffers from SQLDescribeCol).
        if p.scale > 0 {
            let hundred_ns = p.fraction_ns / 100;
            let frac = format!("{hundred_ns:07}");
            s.push('.');
            s.push_str(&frac[..usize::from(p.scale).min(frac.len())]);
        }
    }
    if p.has_tz {
        let sign = if p.tz_hour < 0 || p.tz_minute < 0 {
            '-'
        } else {
            '+'
        };
        s.push_str(&format!(
            " {sign}{:02}:{:02}",
            p.tz_hour.unsigned_abs(),
            p.tz_minute.unsigned_abs()
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SqlPointer;

    fn conv(
        v: &ColumnValues,
        target: SqlSmallInt,
        ptr: SqlPointer,
        ind: *mut SqlLen,
    ) -> Result<ConvOk, ConvError> {
        unsafe { convert_integer_c(v, target, ptr, ind) }
    }

    #[test]
    fn int_to_slong_roundtrip() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = conv(
            &ColumnValues::Int(-123456),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(out, -123456);
        assert_eq!(ind, 4);
    }

    #[test]
    fn tinyint_to_utinyint() {
        let mut out: u8 = 0;
        let mut ind: SqlLen = 0;
        let ret = conv(
            &ColumnValues::TinyInt(200),
            SQL_C_UTINYINT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(out, 200);
        assert_eq!(ind, 1);
    }

    #[test]
    fn bigint_to_sbigint() {
        let mut out: i64 = 0;
        let mut ind: SqlLen = 0;
        conv(
            &ColumnValues::BigInt(i64::MIN),
            SQL_C_SBIGINT,
            (&mut out as *mut i64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, i64::MIN);
        assert_eq!(ind, 8);
    }

    #[test]
    fn bit_true_to_bit() {
        let mut out: u8 = 0xFF;
        let mut ind: SqlLen = 0;
        conv(
            &ColumnValues::Bit(true),
            SQL_C_BIT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, 1);
        assert_eq!(ind, 1);
    }

    #[test]
    fn int_out_of_range_for_smallint() {
        let mut out: i16 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(40000),
            SQL_C_SSHORT,
            (&mut out as *mut i16).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn negative_into_unsigned_is_out_of_range() {
        let mut out: u32 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(-1),
            SQL_C_ULONG,
            (&mut out as *mut u32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn real_into_integer_target_truncates() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &ColumnValues::Real(1.5),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out, 1);
    }

    #[test]
    fn non_integer_target_is_unsupported() {
        let mut out: [u8; 8] = [0; 8];
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(1),
            super::super::odbc_types::SQL_C_DOUBLE,
            out.as_mut_ptr().cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::NotHandledHere);
    }

    #[test]
    fn null_target_pointer_still_sets_indicator() {
        let mut ind: SqlLen = -99;
        let ret = conv(
            &ColumnValues::Int(7),
            SQL_C_SLONG,
            std::ptr::null_mut(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(ind, 4);
    }

    #[test]
    fn bit_out_of_range_rejected() {
        let mut out: u8 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(2),
            SQL_C_BIT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    fn conv_f(
        v: &ColumnValues,
        target: SqlSmallInt,
        ptr: SqlPointer,
        ind: *mut SqlLen,
    ) -> Result<ConvOk, ConvError> {
        unsafe { convert_float_c(v, target, ptr, ind) }
    }

    #[test]
    fn real_to_float_target() {
        let mut out: f32 = 0.0;
        let mut ind: SqlLen = 0;
        let ret = conv_f(
            &ColumnValues::Real(1.5),
            SQL_C_FLOAT,
            (&mut out as *mut f32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(out, 1.5);
        assert_eq!(ind, 4);
    }

    #[test]
    fn float_to_double_target() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        conv_f(
            &ColumnValues::Float(2.5),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, 2.5);
        assert_eq!(ind, 8);
    }

    #[test]
    fn int_to_double_target() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        conv_f(
            &ColumnValues::Int(42),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, 42.0);
    }

    // ---- P1a: mandatory source-type conversions --------------------------
    /// Builds a `decimal`/`numeric` column value from a literal.
    fn dec(s: &str, precision: u8, scale: u8) -> ColumnValues {
        use mssql_tds::datatypes::decoder::DecimalParts;
        ColumnValues::Numeric(DecimalParts::from_string(s, precision, scale).unwrap())
    }

    fn utf8_col(s: &str) -> ColumnValues {
        ColumnValues::String(SqlString::from_utf8_string(s.to_string()))
    }

    #[test]
    fn decimal_into_double_target() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        let ok = conv_f(
            &dec("12345.6789", 18, 4),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Exact);
        assert!((out - 12345.6789).abs() < 1e-9);
    }

    #[test]
    fn decimal_into_bigint_target_truncates() {
        let mut out: i64 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &dec("12345.6789", 18, 4),
            SQL_C_SBIGINT,
            (&mut out as *mut i64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out, 12345);
    }

    #[test]
    fn money_into_double_target() {
        use mssql_tds::datatypes::column_values::SqlMoney;
        // 1234.5678 scaled by 10^4 = 12_345_678.
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        let ok = conv_f(
            &ColumnValues::Money(SqlMoney::from(12_345_678i32)),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Exact);
        assert!((out - 1234.5678).abs() < 1e-9);
    }

    #[test]
    fn smallmoney_into_integer_target_truncates() {
        use mssql_tds::datatypes::column_values::SqlSmallMoney;
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &ColumnValues::SmallMoney(SqlSmallMoney::from(12_345_678i32)),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out, 1234);
    }

    #[test]
    fn float_into_integer_target_truncates() {
        // msodbcsql18: CAST(1234.99 AS float) -> SQL_C_SLONG gives 1234 + 01S07.
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &ColumnValues::Float(1234.99),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out, 1234);
    }

    #[test]
    fn character_source_into_integer_target() {
        // msodbcsql18: CAST('123' AS varchar(10)) -> SQL_C_SLONG gives 123.
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &utf8_col("123"),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Exact);
        assert_eq!(out, 123);
    }

    #[test]
    fn character_source_with_fraction_truncates() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ok = conv(
            &utf8_col(" -42.75 "),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out, -42);
    }

    #[test]
    fn character_source_into_double_target() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        conv_f(
            &utf8_col("1.5e3"),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, 1500.0);
    }

    #[test]
    fn non_numeric_character_source_is_invalid_character_value() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &utf8_col("not-a-number"),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::InvalidCharacterValue);
    }

    #[test]
    fn decimal_out_of_range_for_target_is_rejected() {
        let mut out: i16 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &dec("99999.5", 18, 1),
            SQL_C_SSHORT,
            (&mut out as *mut i16).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn parse_decimal_literal_forms() {
        assert_eq!(
            parse_decimal_literal("12.34"),
            Some(NumericSource::Scaled {
                mantissa: 1234,
                scale: 2
            })
        );
        assert_eq!(
            parse_decimal_literal("-0.01"),
            Some(NumericSource::Scaled {
                mantissa: -1,
                scale: 2
            })
        );
        assert_eq!(
            parse_decimal_literal("+7"),
            Some(NumericSource::Scaled {
                mantissa: 7,
                scale: 0
            })
        );
        assert_eq!(parse_decimal_literal("1e5"), None);
        assert_eq!(parse_decimal_literal("abc"), None);
        assert_eq!(parse_decimal_literal(""), None);
    }

    #[test]
    fn character_source_into_date_target() {
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ok = unsafe {
            convert_datetime_c(
                &utf8_col("2023-06-15"),
                SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ok, ConvOk::Exact);
        assert_eq!(
            out,
            SqlDateStruct {
                year: 2023,
                month: 6,
                day: 15
            }
        );
    }

    #[test]
    fn character_source_into_timestamp_target() {
        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &utf8_col("2023-06-15 12:34:56.1234567"),
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!((out.year, out.month, out.day), (2023, 6, 15));
        assert_eq!((out.hour, out.minute, out.second), (12, 34, 56));
        assert_eq!(out.fraction, 123_456_700);
    }

    #[test]
    fn character_source_iso_t_separator_and_offset() {
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &utf8_col("2023-01-01T12:34:56.1234567+05:30"),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        // A character literal already carries the local wall clock, so the
        // fields are used as written.
        assert_eq!((out.year, out.month, out.day), (2023, 1, 1));
        assert_eq!((out.hour, out.minute, out.second), (12, 34, 56));
        assert_eq!((out.timezone_hour, out.timezone_minute), (5, 30));
    }

    #[test]
    fn character_source_time_only_into_time_target() {
        let mut out = SqlSsTime2Struct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &utf8_col("13:45:30.1234567"),
                SQL_C_SS_TIME2,
                (&mut out as *mut SqlSsTime2Struct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(
            out,
            SqlSsTime2Struct {
                hour: 13,
                minute: 45,
                second: 30,
                fraction: 123_456_700
            }
        );
    }

    #[test]
    fn character_source_into_date_target_truncates_time() {
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ok = unsafe {
            convert_datetime_c(
                &utf8_col("2023-06-15 12:34:56"),
                SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out.day, 15);
    }

    #[test]
    fn invalid_datetime_literals_are_rejected() {
        for bad in [
            "not-a-date",
            "2023-13-01",       // month out of range
            "2023-06-32",       // day out of range
            "2023-06-15 25:00", // hour out of range
            "23-06-15",         // year not 4 digits
            "",
        ] {
            let mut out = SqlTimestampStruct::default();
            let mut ind: SqlLen = 0;
            let err = unsafe {
                convert_datetime_c(
                    &utf8_col(bad),
                    SQL_C_TYPE_TIMESTAMP,
                    (&mut out as *mut SqlTimestampStruct).cast(),
                    &mut ind,
                )
            }
            .unwrap_err();
            assert_eq!(err, ConvError::InvalidCharacterValue, "input: {bad:?}");
        }
    }

    #[test]
    fn binary_source_into_numeric_target_is_restricted() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        let err = conv_f(
            &ColumnValues::Bytes(vec![1, 2, 3]),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::Restricted);
    }

    #[test]
    fn tinyint_c_target_is_unsigned() {
        // SQL_C_TINYINT is unsigned: 200 (> i8::MAX) must round-trip.
        let mut out: u8 = 0;
        let mut ind: SqlLen = 0;
        let ret = conv(
            &ColumnValues::TinyInt(200),
            SQL_C_TINYINT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(out, 200);
    }

    #[test]
    fn signed_tinyint_target_rejects_over_127() {
        let mut out: i8 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::TinyInt(200),
            SQL_C_STINYINT,
            (&mut out as *mut i8).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn float_target_overflow_is_out_of_range() {
        // A finite f64 beyond the f32 range must report 22003, not infinity.
        let mut out: f32 = 0.0;
        let mut ind: SqlLen = 0;
        let err = conv_f(
            &ColumnValues::Float(1.0e40),
            SQL_C_FLOAT,
            (&mut out as *mut f32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    // ---- GUID ------------------------------------------------------------
    #[test]
    fn uuid_to_guid_struct() {
        use uuid::Uuid;
        let u = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let mut out = SqlGuid::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            convert_guid_c(
                &ColumnValues::Uuid(u),
                SQL_C_GUID,
                (&mut out as *mut SqlGuid).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(out.data1, 0x0011_2233);
        assert_eq!(out.data2, 0x4455);
        assert_eq!(out.data3, 0x6677);
        assert_eq!(out.data4, [0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(ind, std::mem::size_of::<SqlGuid>() as SqlLen);
    }

    #[test]
    fn guid_wrong_target_unsupported() {
        use uuid::Uuid;
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let err = unsafe {
            convert_guid_c(
                &ColumnValues::Uuid(Uuid::nil()),
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                &mut ind,
            )
        }
        .unwrap_err();
        assert_eq!(err, ConvError::NotHandledHere);
    }

    // ---- Date / time -----------------------------------------------------
    #[test]
    fn civil_anchor_dates() {
        assert_eq!(civil_from_days_since_0001(0), (1, 1, 1));
        assert_eq!(civil_from_days_since_0001(693_595), (1900, 1, 1));
        assert_eq!(civil_from_days_since_0001(730_178), (2000, 2, 29));
        assert_eq!(civil_from_days_since_0001(738_685), (2023, 6, 15));
        assert_eq!(civil_from_days_since_0001(3_652_058), (9999, 12, 31));
    }

    #[test]
    fn date_to_date_struct() {
        use mssql_tds::datatypes::column_values::SqlDate;
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            convert_datetime_c(
                &ColumnValues::Date(SqlDate::create(738_685).unwrap()),
                SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ret, ConvOk::Exact);
        assert_eq!(
            out,
            SqlDateStruct {
                year: 2023,
                month: 6,
                day: 15
            }
        );
        assert_eq!(ind, std::mem::size_of::<SqlDateStruct>() as SqlLen);
    }

    #[test]
    fn time_to_ss_time2_struct() {
        use mssql_tds::datatypes::column_values::SqlTime;
        // 13:45:30.1234567 in 100 ns ticks since midnight.
        let ticks = ((13 * 3600 + 45 * 60 + 30) as u64) * 10_000_000 + 1_234_567;
        let mut out = SqlSsTime2Struct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::Time(SqlTime {
                    time_nanoseconds: ticks,
                    scale: 7,
                }),
                SQL_C_SS_TIME2,
                (&mut out as *mut SqlSsTime2Struct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(
            out,
            SqlSsTime2Struct {
                hour: 13,
                minute: 45,
                second: 30,
                fraction: 123_456_700
            }
        );
    }

    #[test]
    fn datetime2_to_timestamp_struct() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
        // 01:02:03.5 in 100 ns ticks since midnight.
        let ticks = ((3600 + 2 * 60 + 3) as u64) * 10_000_000 + 5_000_000;
        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTime2(SqlDateTime2 {
                    days: 738_685,
                    time: SqlTime {
                        time_nanoseconds: ticks,
                        scale: 7,
                    },
                }),
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(
            out,
            SqlTimestampStruct {
                year: 2023,
                month: 6,
                day: 15,
                hour: 1,
                minute: 2,
                second: 3,
                fraction: 500_000_000,
            }
        );
    }

    #[test]
    fn datetimeoffset_to_ss_struct() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 730_178,
                        time: SqlTime {
                            time_nanoseconds: 0,
                            scale: 7,
                        },
                    },
                    offset: -330, // -05:30
                }),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        // UTC 2000-02-29 00:00 at -05:30 is local 2000-02-28 18:30, so the
        // negative offset must borrow a day.
        assert_eq!(out.year, 2000);
        assert_eq!(out.month, 2);
        assert_eq!(out.day, 28);
        assert_eq!(out.hour, 18);
        assert_eq!(out.minute, 30);
        assert_eq!(out.timezone_hour, -5);
        assert_eq!(out.timezone_minute, -30);
    }

    #[test]
    fn datetimeoffset_matches_msodbcsql_wall_clock() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        // SELECT CAST('2023-01-01 12:34:56.1234567 +05:30' AS datetimeoffset(7))
        // msodbcsql18 -> 2023-01-01 12:34:56.123456700 +05:30
        // Stored UTC is 2023-01-01 07:04:56.1234567 with offset +330.
        let utc_ticks = ((7 * 3600 + 4 * 60 + 56) as u64) * 10_000_000 + 1_234_567;
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 738_520, // 2023-01-01
                        time: SqlTime {
                            time_nanoseconds: utc_ticks,
                            scale: 7,
                        },
                    },
                    offset: 330,
                }),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!((out.year, out.month, out.day), (2023, 1, 1));
        assert_eq!((out.hour, out.minute, out.second), (12, 34, 56));
        assert_eq!(out.fraction, 123_456_700);
        assert_eq!((out.timezone_hour, out.timezone_minute), (5, 30));
    }

    #[test]
    fn datetimeoffset_negative_offset_rolls_day_backward() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        // UTC 2023-06-15 02:00 at -05:00 is local 2023-06-14 21:00.
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 738_685,
                        time: SqlTime {
                            time_nanoseconds: 2 * 3600 * 10_000_000,
                            scale: 7,
                        },
                    },
                    offset: -300,
                }),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!((out.year, out.month, out.day), (2023, 6, 14));
        assert_eq!((out.hour, out.minute), (21, 0));
        assert_eq!((out.timezone_hour, out.timezone_minute), (-5, 0));
    }

    #[test]
    fn datetime2_into_date_target_reports_truncation() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ok = unsafe {
            convert_datetime_c(
                &ColumnValues::DateTime2(SqlDateTime2 {
                    days: 738_685,
                    time: SqlTime {
                        time_nanoseconds: ((12 * 3600 + 34 * 60 + 56) as u64) * 10_000_000,
                        scale: 7,
                    },
                }),
                SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!(out.day, 15);
    }

    #[test]
    fn datetime2_into_time_target_reports_fraction_truncation() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
        let mut out = SqlTimeStruct::default();
        let mut ind: SqlLen = 0;
        let ok = unsafe {
            convert_datetime_c(
                &ColumnValues::DateTime2(SqlDateTime2 {
                    days: 738_685,
                    time: SqlTime {
                        time_nanoseconds: ((12 * 3600 + 34 * 60 + 56) as u64) * 10_000_000
                            + 1_234_567,
                        scale: 7,
                    },
                }),
                SQL_C_TYPE_TIME,
                (&mut out as *mut SqlTimeStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        // SQL_TIME_STRUCT cannot carry the fraction.
        assert_eq!(ok, ConvOk::Truncated);
        assert_eq!((out.hour, out.minute, out.second), (12, 34, 56));
    }

    #[test]
    fn character_rendering_pads_to_declared_scale() {
        use mssql_tds::datatypes::column_values::{
            SqlDateTime, SqlDateTime2, SqlSmallDateTime, SqlTime,
        };
        // datetime2(7) with a whole-second value still renders 7 digits.
        let p = extract_datetime_parts(&ColumnValues::DateTime2(SqlDateTime2 {
            days: 738_685,
            time: SqlTime {
                time_nanoseconds: 12 * 3600 * 10_000_000,
                scale: 7,
            },
        }))
        .unwrap();
        let s = format_datetime_parts(&p);
        assert_eq!(s, "2023-06-15 12:00:00.0000000");
        assert_eq!(s.len(), 27);

        // datetime2(3) renders exactly 3.
        let p = extract_datetime_parts(&ColumnValues::DateTime2(SqlDateTime2 {
            days: 738_685,
            time: SqlTime {
                time_nanoseconds: 12 * 3600 * 10_000_000 + 1_000_000,
                scale: 3,
            },
        }))
        .unwrap();
        assert_eq!(format_datetime_parts(&p), "2023-06-15 12:00:00.100");

        // Legacy `datetime` is always scale 3.
        let p = extract_datetime_parts(&ColumnValues::DateTime(SqlDateTime {
            days: 45_090, // 2023-06-15
            time: 12 * 3600 * 300,
        }))
        .unwrap();
        assert_eq!(format_datetime_parts(&p), "2023-06-15 12:00:00.000");

        // `smalldatetime` is scale 0: no fractional part at all.
        let p = extract_datetime_parts(&ColumnValues::SmallDateTime(SqlSmallDateTime {
            days: 45_090,
            time: 12 * 60,
        }))
        .unwrap();
        assert_eq!(format_datetime_parts(&p), "2023-06-15 12:00:00");
    }

    #[test]
    fn datetimeoffset_positive_offset_rolls_day_forward() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 738_685, // 2023-06-15 UTC
                        time: SqlTime {
                            // 22:00:00 UTC
                            time_nanoseconds: 22 * 3600 * 10_000_000,
                            scale: 7,
                        },
                    },
                    offset: 330, // +05:30 -> local 2023-06-16 03:30
                }),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!((out.year, out.month, out.day), (2023, 6, 16));
        assert_eq!((out.hour, out.minute), (3, 30));
        assert_eq!((out.timezone_hour, out.timezone_minute), (5, 30));
    }

    #[test]
    fn datetimeoffset_out_of_range_after_offset_is_rejected() {
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlDateTimeOffset, SqlTime};
        let mut out = SqlSsTimestampoffsetStruct::default();
        let mut ind: SqlLen = 0;
        // 0001-01-01 00:00 UTC with a negative offset falls before the minimum
        // representable date.
        let err = unsafe {
            convert_datetime_c(
                &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 0,
                        time: SqlTime {
                            time_nanoseconds: 0,
                            scale: 7,
                        },
                    },
                    offset: -60,
                }),
                SQL_C_SS_TIMESTAMPOFFSET,
                (&mut out as *mut SqlSsTimestampoffsetStruct).cast(),
                &mut ind,
            )
        }
        .unwrap_err();
        assert_eq!(err, ConvError::Restricted);
    }

    #[test]
    fn datetime_legacy_epoch_and_ticks() {
        use mssql_tds::datatypes::column_values::SqlDateTime;
        // days = 0 -> 1900-01-01; time = 300 ticks -> 1 second past midnight.
        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTime(SqlDateTime { days: 0, time: 300 }),
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(out.year, 1900);
        assert_eq!(out.month, 1);
        assert_eq!(out.day, 1);
        assert_eq!(out.second, 1);
    }

    #[test]
    fn time_into_date_target_is_restricted() {
        use mssql_tds::datatypes::column_values::SqlTime;
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let err = unsafe {
            convert_datetime_c(
                &ColumnValues::Time(SqlTime {
                    time_nanoseconds: 0,
                    scale: 0,
                }),
                SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                &mut ind,
            )
        }
        .unwrap_err();
        assert_eq!(err, ConvError::Restricted);
    }

    #[test]
    fn format_datetime_parts_timestamp_with_fraction() {
        let p = extract_datetime_parts(&{
            use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
            ColumnValues::DateTime2(SqlDateTime2 {
                days: 738_685,
                time: SqlTime {
                    time_nanoseconds: 1_234_567,
                    scale: 7,
                },
            })
        })
        .unwrap();
        assert_eq!(format_datetime_parts(&p), "2023-06-15 00:00:00.1234567");
    }
}
