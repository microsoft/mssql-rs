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
//! [`ConvError::Unsupported`] so callers can fall back to their existing paths
//! (e.g. the character conversion in `get_data`).

use super::odbc_types::{
    SQL_C_BIT, SQL_C_DATE, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_LONG, SQL_C_SBIGINT,
    SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_STINYINT, SQL_C_TIME, SQL_C_TIMESTAMP, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME,
    SQL_C_TYPE_TIMESTAMP, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_SUCCESS,
    SqlDateStruct, SqlGuid, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlSsTime2Struct,
    SqlSsTimestampoffsetStruct, SqlTimeStruct, SqlTimestampStruct,
};
use super::util::write_if_some;
use mssql_tds::datatypes::column_values::ColumnValues;

/// Why a value-level conversion could not be completed. The caller maps each
/// variant to the appropriate SQLSTATE on its own diagnostic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvError {
    /// The value does not fit the requested C type (SQLSTATE `22003`).
    OutOfRange,
    /// This source/target pairing is not handled here (SQLSTATE `HYC00`), and
    /// the caller should try another path or report "not implemented".
    Unsupported,
}

/// Widen any integer-valued column to `i128`, which losslessly holds every
/// integer `ColumnValues` variant. Returns `None` for non-integer sources.
fn integer_source_as_i128(value: &ColumnValues) -> Option<i128> {
    match value {
        ColumnValues::TinyInt(x) => Some(i128::from(*x)),
        ColumnValues::SmallInt(x) => Some(i128::from(*x)),
        ColumnValues::Int(x) => Some(i128::from(*x)),
        ColumnValues::BigInt(x) => Some(i128::from(*x)),
        ColumnValues::Bit(b) => Some(i128::from(*b)),
        _ => None,
    }
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
unsafe fn write_fixed<T: Copy>(ptr: SqlPointer, value: T, ind: *mut SqlLen) -> SqlReturn {
    if !ptr.is_null() {
        unsafe { (ptr as *mut T).write_unaligned(value) };
    }
    unsafe { write_if_some(ind, std::mem::size_of::<T>() as SqlLen) };
    SQL_SUCCESS
}

/// Converts an integer column value to a fixed-width integer C target,
/// range-checking against the target type.
///
/// Returns [`ConvError::Unsupported`] when either the source is not an integer
/// column or the target is not a fixed-width integer C type, letting the caller
/// fall back to another conversion path.
///
/// # Safety
/// `target_value_ptr`, when non-null, must be valid for a write of the target
/// C type's size, and `strlen_or_ind_ptr` must be null or valid for a
/// `SqlLen` write.
pub(crate) unsafe fn convert_integer_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<SqlReturn, ConvError> {
    let Some(v) = integer_source_as_i128(value) else {
        return Err(ConvError::Unsupported);
    };

    // Helper: narrow `v` to the target's range or fail with OutOfRange.
    macro_rules! narrow {
        ($ty:ty) => {{ <$ty>::try_from(v).map_err(|_| ConvError::OutOfRange)? }};
    }

    let ret = match target_type {
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
        // A bit target only accepts 0 or 1; any other integer is out of range.
        SQL_C_BIT => {
            let b: u8 = match v {
                0 => 0,
                1 => 1,
                _ => return Err(ConvError::OutOfRange),
            };
            unsafe { write_fixed(target_value_ptr, b, strlen_or_ind_ptr) }
        }
        _ => return Err(ConvError::Unsupported),
    };
    Ok(ret)
}

/// Widen a numeric column (integer or floating) to `f64`. Returns `None` for
/// non-numeric sources.
fn numeric_source_as_f64(value: &ColumnValues) -> Option<f64> {
    match value {
        ColumnValues::TinyInt(x) => Some(f64::from(*x)),
        ColumnValues::SmallInt(x) => Some(f64::from(*x)),
        ColumnValues::Int(x) => Some(f64::from(*x)),
        ColumnValues::BigInt(x) => Some(*x as f64),
        ColumnValues::Bit(b) => Some(if *b { 1.0 } else { 0.0 }),
        ColumnValues::Real(x) => Some(f64::from(*x)),
        ColumnValues::Float(x) => Some(*x),
        _ => None,
    }
}

/// Returns `true` if `target_type` is one of the floating-point C types handled
/// by [`convert_float_c`].
pub(crate) fn is_float_c_target(target_type: SqlSmallInt) -> bool {
    matches!(target_type, SQL_C_FLOAT | SQL_C_DOUBLE)
}

/// Converts a numeric column value to a floating-point C target.
///
/// Returns [`ConvError::Unsupported`] when the source is not numeric or the
/// target is not a floating-point C type.
///
/// # Safety
/// Same pointer contract as [`convert_integer_c`].
pub(crate) unsafe fn convert_float_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<SqlReturn, ConvError> {
    let Some(v) = numeric_source_as_f64(value) else {
        return Err(ConvError::Unsupported);
    };
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
        _ => return Err(ConvError::Unsupported),
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
) -> Result<SqlReturn, ConvError> {
    if target_type != SQL_C_GUID {
        return Err(ConvError::Unsupported);
    }
    let ColumnValues::Uuid(u) = value else {
        return Err(ConvError::Unsupported);
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

/// (hour, minute, second, fraction_ns) from nanoseconds since midnight.
fn hms_from_nanos(nanos: u64) -> (u16, u16, u16, u32) {
    let secs = nanos / 1_000_000_000;
    let fraction_ns = (nanos % 1_000_000_000) as u32;
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
            let (h, mi, s, f) = hms_from_nanos(t.time_nanoseconds);
            p.hour = h;
            p.minute = mi;
            p.second = s;
            p.fraction_ns = f;
            p.has_time = true;
        }
        ColumnValues::DateTime2(dt) => {
            let (y, m, day) = civil_from_days_since_0001(i64::from(dt.days));
            let (h, mi, s, f) = hms_from_nanos(dt.time.time_nanoseconds);
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
            let (y, m, day) = civil_from_days_since_0001(i64::from(dto.datetime2.days));
            let (h, mi, s, f) = hms_from_nanos(dto.datetime2.time.time_nanoseconds);
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
pub(crate) unsafe fn convert_datetime_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<SqlReturn, ConvError> {
    let Some(p) = extract_datetime_parts(value) else {
        return Err(ConvError::Unsupported);
    };
    let ret = match target_type {
        SQL_C_TYPE_DATE | SQL_C_DATE if p.has_date => unsafe {
            write_fixed(
                target_value_ptr,
                SqlDateStruct {
                    year: p.year,
                    month: p.month,
                    day: p.day,
                },
                strlen_or_ind_ptr,
            )
        },
        SQL_C_TYPE_TIME | SQL_C_TIME if p.has_time => unsafe {
            write_fixed(
                target_value_ptr,
                SqlTimeStruct {
                    hour: p.hour,
                    minute: p.minute,
                    second: p.second,
                },
                strlen_or_ind_ptr,
            )
        },
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
        _ => return Err(ConvError::Unsupported),
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
        if p.fraction_ns != 0 {
            // Render in 100 ns units (7 digits) and trim trailing zeros.
            let hundred_ns = p.fraction_ns / 100;
            let frac = format!("{hundred_ns:07}");
            let frac = frac.trim_end_matches('0');
            if !frac.is_empty() {
                s.push('.');
                s.push_str(frac);
            }
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
    ) -> Result<SqlReturn, ConvError> {
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
        assert_eq!(ret, SQL_SUCCESS);
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
        assert_eq!(ret, SQL_SUCCESS);
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
    fn non_integer_source_is_unsupported() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Real(1.5),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::Unsupported);
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
        assert_eq!(err, ConvError::Unsupported);
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
        assert_eq!(ret, SQL_SUCCESS);
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
    ) -> Result<SqlReturn, ConvError> {
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
        assert_eq!(ret, SQL_SUCCESS);
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

    #[test]
    fn non_numeric_source_float_unsupported() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        let err = conv_f(
            &ColumnValues::Bytes(vec![1, 2, 3]),
            SQL_C_DOUBLE,
            (&mut out as *mut f64).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::Unsupported);
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
        assert_eq!(ret, SQL_SUCCESS);
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
        assert_eq!(ret, SQL_SUCCESS);
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
        assert_eq!(err, ConvError::Unsupported);
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
        assert_eq!(ret, SQL_SUCCESS);
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
        // 13:45:30.123456700 -> nanoseconds since midnight.
        let nanos = ((13 * 3600 + 45 * 60 + 30) as u64) * 1_000_000_000 + 123_456_700;
        let mut out = SqlSsTime2Struct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::Time(SqlTime {
                    time_nanoseconds: nanos,
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
        let nanos = ((3600 + 2 * 60 + 3) as u64) * 1_000_000_000 + 500_000_000;
        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        unsafe {
            convert_datetime_c(
                &ColumnValues::DateTime2(SqlDateTime2 {
                    days: 738_685,
                    time: SqlTime {
                        time_nanoseconds: nanos,
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
        assert_eq!(out.year, 2000);
        assert_eq!(out.month, 2);
        assert_eq!(out.day, 29);
        assert_eq!(out.timezone_hour, -5);
        assert_eq!(out.timezone_minute, -30);
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
    fn time_into_date_target_unsupported() {
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
        assert_eq!(err, ConvError::Unsupported);
    }

    #[test]
    fn format_datetime_parts_timestamp_with_fraction() {
        let p = extract_datetime_parts(&{
            use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
            ColumnValues::DateTime2(SqlDateTime2 {
                days: 738_685,
                time: SqlTime {
                    time_nanoseconds: 123_456_700,
                    scale: 7,
                },
            })
        })
        .unwrap();
        assert_eq!(format_datetime_parts(&p), "2023-06-15 00:00:00.1234567");
    }
}
