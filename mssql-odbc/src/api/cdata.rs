// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion of TDS column values into ODBC C buffers.
//!
//! Shared by `SQLGetData` (single value into a caller buffer) and the bound
//! column path used by `SQLFetch` / `SQLFetchScroll`. The two entry points
//! differ only in how the destination pointer is computed, so all of the
//! SQL-type → C-type conversion policy lives here.

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::EncodingType;

use super::odbc_types::*;
use super::util::{copy_with_nul, write_if_some};

/// Outcome of a successful write.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum WriteOutcome {
    /// The whole value was written.
    Complete,
    /// The value did not fit; a truncated prefix was written (01004).
    Truncated,
}

/// Why a conversion could not be performed.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum WriteError {
    /// The requested C type is not a valid ODBC C type (HY003).
    InvalidCType,
    /// The SQL type cannot be converted to the requested C type (07006).
    RestrictedConversion,
    /// The value does not fit the target C type (22003).
    OutOfRange,
}

/// Normalized view of a column value, decoupled from the TDS representation.
pub(crate) enum Cell {
    Int(i64),
    UInt(u64),
    Double(f64),
    /// `money` / `smallmoney` as its raw scaled integer (4 decimal places).
    /// Kept exact so character and `SQL_C_NUMERIC` targets don't inherit `f64`
    /// rounding at the edges of the `money` range.
    Money(i64),
    Bool(bool),
    Text(String),
    Binary(Vec<u8>),
    Guid([u8; 16]),
    Decimal(DecimalParts),
    Date(CivilDate),
    Time(CivilTime),
    Timestamp(CivilDate, CivilTime),
    TimestampOffset(CivilDate, CivilTime, i16),
}

#[derive(Clone, Copy)]
pub(in crate::api) struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

#[derive(Clone, Copy, Default)]
pub(in crate::api) struct CivilTime {
    hour: u32,
    minute: u32,
    second: u32,
    /// Fractional seconds in nanoseconds.
    nanos: u32,
    /// Fractional-seconds scale (0–7), used when rendering to text.
    scale: u8,
}

/// Days from 0001-01-01 to 1970-01-01.
const DAYS_YEAR_ONE_TO_EPOCH: i64 = 719_162;
/// Days from 1900-01-01 to 1970-01-01.
const DAYS_1900_TO_EPOCH: i64 = 25_567;

/// Howard Hinnant's `civil_from_days`: converts days since 1970-01-01 into a
/// proleptic Gregorian calendar date.
fn civil_from_days(z: i64) -> CivilDate {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    CivilDate {
        year: (y + i64::from(m <= 2)) as i32,
        month: m as u32,
        day: d as u32,
    }
}

/// TDS `SqlTime::time_nanoseconds` is actually a count of 100ns ticks (the
/// decoder normalizes every scale to that unit), so callers converting a TDS
/// time must go through [`time_from_ticks`], not this helper.
fn time_from_nanos(total_nanos: u64, scale: u8) -> CivilTime {
    let secs = total_nanos / 1_000_000_000;
    CivilTime {
        hour: (secs / 3600) as u32,
        minute: ((secs / 60) % 60) as u32,
        second: (secs % 60) as u32,
        nanos: (total_nanos % 1_000_000_000) as u32,
        scale,
    }
}

/// Converts TDS 100ns ticks since midnight into a civil time.
fn time_from_ticks(ticks_100ns: u64, scale: u8) -> CivilTime {
    time_from_nanos(ticks_100ns.saturating_mul(100), scale)
}

impl CivilDate {
    fn to_text(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl CivilTime {
    fn to_text(self) -> String {
        let base = format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second);
        if self.scale == 0 {
            return base;
        }
        let scale = usize::from(self.scale.min(9));
        let frac = format!("{:09}", self.nanos);
        format!("{base}.{}", &frac[..scale])
    }
}

fn money_scaled(lsb: i32, msb: i32) -> i64 {
    ((msb as i64) << 32) | ((lsb as u32) as i64)
}

/// Renders a scaled `money` integer with its four implied decimal places.
fn money_text(scaled: i64) -> String {
    let sign = if scaled < 0 { "-" } else { "" };
    let abs = scaled.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

/// Projects a TDS column value onto the normalized [`Cell`] model.
pub(crate) fn to_cell(v: &ColumnValues) -> Option<Cell> {
    Some(match v {
        ColumnValues::TinyInt(x) => Cell::UInt(u64::from(*x)),
        ColumnValues::SmallInt(x) => Cell::Int(i64::from(*x)),
        ColumnValues::Int(x) => Cell::Int(i64::from(*x)),
        ColumnValues::BigInt(x) => Cell::Int(*x),
        ColumnValues::Real(x) => Cell::Double(f64::from(*x)),
        ColumnValues::Float(x) => Cell::Double(*x),
        ColumnValues::Bit(x) => Cell::Bool(*x),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Cell::Decimal(d.clone()),
        ColumnValues::String(s) => Cell::Text(s.to_utf8_string()),
        ColumnValues::Xml(x) => Cell::Text(x.as_string()),
        ColumnValues::Json(j) => Cell::Text(j.as_string()),
        ColumnValues::Bytes(b) => Cell::Binary(b.clone()),
        ColumnValues::Uuid(u) => Cell::Guid(*u.as_bytes()),
        ColumnValues::SmallMoney(m) => Cell::Money(i64::from(m.int_val)),
        ColumnValues::Money(m) => Cell::Money(money_scaled(m.lsb_part, m.msb_part)),
        ColumnValues::Date(d) => Cell::Date(civil_from_days(
            i64::from(d.get_days()) - DAYS_YEAR_ONE_TO_EPOCH,
        )),
        ColumnValues::Time(t) => Cell::Time(time_from_ticks(t.time_nanoseconds, t.scale)),
        ColumnValues::DateTime2(dt) => Cell::Timestamp(
            civil_from_days(i64::from(dt.days) - DAYS_YEAR_ONE_TO_EPOCH),
            time_from_ticks(dt.time.time_nanoseconds, dt.time.scale),
        ),
        ColumnValues::DateTimeOffset(dto) => Cell::TimestampOffset(
            civil_from_days(i64::from(dto.datetime2.days) - DAYS_YEAR_ONE_TO_EPOCH),
            time_from_ticks(
                dto.datetime2.time.time_nanoseconds,
                dto.datetime2.time.scale,
            ),
            dto.offset,
        ),
        ColumnValues::DateTime(dt) => {
            // 1/300 s ticks since midnight; msodbcsql rounds to 3 fractional digits.
            let nanos = (u64::from(dt.time) * 1_000_000_000).div_euclid(300);
            let millis = (nanos + 500_000) / 1_000_000;
            Cell::Timestamp(
                civil_from_days(i64::from(dt.days) - DAYS_1900_TO_EPOCH),
                time_from_nanos(millis * 1_000_000, 3),
            )
        }
        ColumnValues::SmallDateTime(dt) => Cell::Timestamp(
            civil_from_days(i64::from(dt.days) - DAYS_1900_TO_EPOCH),
            time_from_nanos(u64::from(dt.time) * 60 * 1_000_000_000, 0),
        ),
        ColumnValues::Null | ColumnValues::Vector(_) => return None,
    })
}

impl Cell {
    /// Renders the value the way msodbcsql renders it for character targets.
    fn to_text(&self) -> String {
        match self {
            Cell::Int(x) => x.to_string(),
            Cell::UInt(x) => x.to_string(),
            Cell::Double(x) => x.to_string(),
            Cell::Money(x) => money_text(*x),
            Cell::Bool(x) => (if *x { "1" } else { "0" }).to_string(),
            Cell::Text(s) => s.clone(),
            Cell::Binary(b) => b.iter().map(|byte| format!("{byte:02X}")).collect(),
            Cell::Guid(g) => guid_text(g),
            Cell::Decimal(d) => d.to_string(),
            Cell::Date(d) => d.to_text(),
            Cell::Time(t) => t.to_text(),
            Cell::Timestamp(d, t) => format!("{} {}", d.to_text(), t.to_text()),
            Cell::TimestampOffset(d, t, off) => {
                let sign = if *off < 0 { '-' } else { '+' };
                let abs = off.unsigned_abs();
                format!(
                    "{} {} {sign}{:02}:{:02}",
                    d.to_text(),
                    t.to_text(),
                    abs / 60,
                    abs % 60
                )
            }
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Int(x) => Some(*x),
            Cell::UInt(x) => i64::try_from(*x).ok(),
            Cell::Double(x) => Some(x.round() as i64),
            Cell::Money(x) => Some(x / 10_000),
            Cell::Bool(x) => Some(i64::from(*x)),
            Cell::Decimal(d) => d.to_decimal_string().parse::<f64>().ok().map(|f| f as i64),
            Cell::Text(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Cell::Int(x) => Some(*x as f64),
            Cell::UInt(x) => Some(*x as f64),
            Cell::Double(x) => Some(*x),
            Cell::Money(x) => Some(*x as f64 / 10_000.0),
            Cell::Bool(x) => Some(f64::from(u8::from(*x))),
            Cell::Decimal(d) => d.to_decimal_string().parse::<f64>().ok(),
            Cell::Text(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Cell::Binary(b) => Some(b.clone()),
            Cell::Guid(g) => Some(g.to_vec()),
            Cell::Text(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }
}

fn guid_text(g: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        g[0],
        g[1],
        g[2],
        g[3],
        g[4],
        g[5],
        g[6],
        g[7],
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// Writes a fixed-size POD value to `dst` and reports its byte size through the
/// indicator.
///
/// # Safety
/// `dst` must be valid for writes of `size_of::<T>()` bytes (possibly
/// unaligned), or null; `ind` must be null or a valid `SqlLen` pointer.
unsafe fn write_pod<T>(dst: SqlPointer, ind: *mut SqlLen, value: T) -> WriteOutcome {
    if !dst.is_null() {
        unsafe { std::ptr::write_unaligned(dst as *mut T, value) };
    }
    unsafe { write_if_some(ind, std::mem::size_of::<T>() as SqlLen) };
    WriteOutcome::Complete
}

/// Converts `value` into `target_type` and writes it to the caller's buffer.
///
/// # Safety
/// `target_value_ptr` must be valid for writes of `buffer_length` bytes (or
/// null), and `strlen_or_ind_ptr` must be null or point to a writable `SqlLen`.
pub(crate) unsafe fn write_c_value(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<WriteOutcome, WriteError> {
    if matches!(value, ColumnValues::Null) {
        unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
        // Character/binary targets still get a terminator so naive callers that
        // ignore the indicator read an empty value rather than stale memory.
        match target_type {
            SQL_C_WCHAR => unsafe {
                copy_with_nul(
                    target_value_ptr as *mut SqlWChar,
                    wchar_capacity(buffer_length),
                    &[],
                );
            },
            SQL_C_CHAR => unsafe {
                copy_with_nul(
                    target_value_ptr as *mut u8,
                    buffer_length.max(0) as usize,
                    &[],
                );
            },
            _ => {}
        }
        return Ok(WriteOutcome::Complete);
    }

    let Some(cell) = to_cell(value) else {
        return Err(WriteError::RestrictedConversion);
    };

    match target_type {
        SQL_C_CHAR | SQL_C_DEFAULT => {
            let ansi = narrow_bytes(value).ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_text(
                    &ansi,
                    target_value_ptr as *mut u8,
                    buffer_length.max(0) as usize,
                    strlen_or_ind_ptr,
                )
            })
        }
        SQL_C_WCHAR => {
            let utf16: Vec<SqlWChar> = cell.to_text().encode_utf16().collect();
            Ok(unsafe {
                write_text(
                    &utf16,
                    target_value_ptr as *mut SqlWChar,
                    wchar_capacity(buffer_length),
                    strlen_or_ind_ptr,
                )
            })
        }
        SQL_C_BINARY => {
            let bytes = cell.as_bytes().ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_binary(
                    &bytes,
                    target_value_ptr as *mut u8,
                    buffer_length.max(0) as usize,
                    strlen_or_ind_ptr,
                )
            })
        }
        SQL_C_BIT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe { write_pod::<u8>(target_value_ptr, strlen_or_ind_ptr, u8::from(v != 0)) })
        }
        SQL_C_STINYINT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = i8::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        // SQL Server's `tinyint` is unsigned 0..=255 and msodbcsql treats the
        // unqualified `SQL_C_TINYINT` alias as unsigned for it, so 128..=255
        // round-trips instead of overflowing a signed byte.
        SQL_C_UTINYINT | SQL_C_TINYINT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = u8::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_SSHORT | SQL_C_SHORT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = i16::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_USHORT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = u16::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_SLONG | SQL_C_LONG => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = i32::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_ULONG => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = u32::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_SBIGINT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_UBIGINT => {
            let v = cell.as_i64().ok_or(WriteError::RestrictedConversion)?;
            let v = u64::try_from(v).map_err(|_| WriteError::OutOfRange)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_FLOAT => {
            let v = cell.as_f64().ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v as f32) })
        }
        SQL_C_DOUBLE => {
            let v = cell.as_f64().ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, v) })
        }
        SQL_C_NUMERIC => {
            let d = match &cell {
                Cell::Decimal(d) => d.clone(),
                _ => return Err(WriteError::RestrictedConversion),
            };
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, numeric_struct(&d)) })
        }
        SQL_C_GUID => {
            let Cell::Guid(g) = cell else {
                return Err(WriteError::RestrictedConversion);
            };
            Ok(unsafe { write_pod(target_value_ptr, strlen_or_ind_ptr, guid_struct(&g)) })
        }
        SQL_C_TYPE_DATE | SQL_C_DATE => {
            let d = cell_date(&cell).ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_pod(
                    target_value_ptr,
                    strlen_or_ind_ptr,
                    SqlDateStruct {
                        year: d.year as SqlSmallInt,
                        month: d.month as SqlUSmallInt,
                        day: d.day as SqlUSmallInt,
                    },
                )
            })
        }
        SQL_C_TYPE_TIME | SQL_C_TIME => {
            let t = cell_time(&cell).ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_pod(
                    target_value_ptr,
                    strlen_or_ind_ptr,
                    SqlTimeStruct {
                        hour: t.hour as SqlUSmallInt,
                        minute: t.minute as SqlUSmallInt,
                        second: t.second as SqlUSmallInt,
                    },
                )
            })
        }
        SQL_C_SS_TIME2 => {
            let t = cell_time(&cell).ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_pod(
                    target_value_ptr,
                    strlen_or_ind_ptr,
                    SqlSsTime2Struct {
                        hour: t.hour as SqlUSmallInt,
                        minute: t.minute as SqlUSmallInt,
                        second: t.second as SqlUSmallInt,
                        fraction: t.nanos,
                    },
                )
            })
        }
        SQL_C_TYPE_TIMESTAMP | SQL_C_TIMESTAMP => {
            let (d, t) = cell_timestamp(&cell).ok_or(WriteError::RestrictedConversion)?;
            Ok(unsafe {
                write_pod(
                    target_value_ptr,
                    strlen_or_ind_ptr,
                    SqlTimestampStruct {
                        year: d.year as SqlSmallInt,
                        month: d.month as SqlUSmallInt,
                        day: d.day as SqlUSmallInt,
                        hour: t.hour as SqlUSmallInt,
                        minute: t.minute as SqlUSmallInt,
                        second: t.second as SqlUSmallInt,
                        fraction: t.nanos,
                    },
                )
            })
        }
        SQL_C_SS_TIMESTAMPOFFSET => {
            let (d, t) = cell_timestamp(&cell).ok_or(WriteError::RestrictedConversion)?;
            let offset = match &cell {
                Cell::TimestampOffset(_, _, off) => *off,
                _ => 0,
            };
            Ok(unsafe {
                write_pod(
                    target_value_ptr,
                    strlen_or_ind_ptr,
                    SqlSsTimestampoffsetStruct {
                        year: d.year as SqlSmallInt,
                        month: d.month as SqlUSmallInt,
                        day: d.day as SqlUSmallInt,
                        hour: t.hour as SqlUSmallInt,
                        minute: t.minute as SqlUSmallInt,
                        second: t.second as SqlUSmallInt,
                        fraction: t.nanos,
                        timezone_hour: offset / 60,
                        timezone_minute: offset % 60,
                    },
                )
            })
        }
        _ => Err(WriteError::InvalidCType),
    }
}

fn cell_date(cell: &Cell) -> Option<CivilDate> {
    match cell {
        Cell::Date(d) => Some(*d),
        Cell::Timestamp(d, _) | Cell::TimestampOffset(d, _, _) => Some(*d),
        _ => None,
    }
}

fn cell_time(cell: &Cell) -> Option<CivilTime> {
    match cell {
        Cell::Time(t) => Some(*t),
        Cell::Timestamp(_, t) | Cell::TimestampOffset(_, t, _) => Some(*t),
        _ => None,
    }
}

fn cell_timestamp(cell: &Cell) -> Option<(CivilDate, CivilTime)> {
    match cell {
        Cell::Timestamp(d, t) | Cell::TimestampOffset(d, t, _) => Some((*d, *t)),
        Cell::Date(d) => Some((*d, CivilTime::default())),
        _ => None,
    }
}

fn guid_struct(g: &[u8; 16]) -> SqlGuid {
    SqlGuid {
        data1: u32::from_be_bytes([g[0], g[1], g[2], g[3]]),
        data2: u16::from_be_bytes([g[4], g[5]]),
        data3: u16::from_be_bytes([g[6], g[7]]),
        data4: [g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]],
    }
}

/// Builds a `SQL_NUMERIC_STRUCT` from the decimal's digit string. Going through
/// the rendered digits keeps this independent of the TDS mantissa layout.
fn numeric_struct(d: &DecimalParts) -> SqlNumericStruct {
    let text = d.to_decimal_string();
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut mantissa = digits.parse::<u128>().unwrap_or(0);
    let mut val = [0u8; SQL_MAX_NUMERIC_LEN];
    for slot in val.iter_mut() {
        *slot = (mantissa & 0xFF) as u8;
        mantissa >>= 8;
    }
    SqlNumericStruct {
        precision: d.precision,
        scale: d.scale as i8,
        sign: u8::from(d.is_positive),
        val,
    }
}

pub(crate) fn wchar_capacity(buffer_length: SqlLen) -> usize {
    (buffer_length.max(0) as usize) / std::mem::size_of::<SqlWChar>()
}

/// NUL-terminated character write with ODBC truncation semantics: the indicator
/// reports the untruncated length in bytes.
///
/// # Safety
/// `dst` must be valid for `capacity` elements of `T`, or null.
unsafe fn write_text<T: Copy + Default>(
    src: &[T],
    dst: *mut T,
    capacity: usize,
    ind: *mut SqlLen,
) -> WriteOutcome {
    unsafe { write_if_some(ind, std::mem::size_of_val(src) as SqlLen) };
    if unsafe { copy_with_nul(dst, capacity, src) } {
        WriteOutcome::Truncated
    } else {
        WriteOutcome::Complete
    }
}

/// Binary write: no NUL terminator, indicator reports the untruncated length.
///
/// # Safety
/// `dst` must be valid for `capacity` bytes, or null.
unsafe fn write_binary(
    src: &[u8],
    dst: *mut u8,
    capacity: usize,
    ind: *mut SqlLen,
) -> WriteOutcome {
    unsafe { write_if_some(ind, src.len() as SqlLen) };
    if dst.is_null() || capacity == 0 {
        return if src.is_empty() {
            WriteOutcome::Complete
        } else {
            WriteOutcome::Truncated
        };
    }
    let n = src.len().min(capacity);
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, n) };
    if n < src.len() {
        WriteOutcome::Truncated
    } else {
        WriteOutcome::Complete
    }
}

/// Character/binary payload for a column, ready to be streamed by `SQLGetData`.
///
/// `SQLGetData` returns long values in chunks, so the payload has to be
/// materialized once and then sliced at a byte offset across calls. Fixed-width
/// C types are never chunked and are served directly by [`write_c_value`].
pub(crate) enum StreamPayload {
    /// Narrow character bytes, terminated with a single NUL when copied out.
    Narrow(Vec<u8>),
    /// UTF-16LE code units.
    Wide(Vec<SqlWChar>),
    /// Raw bytes, copied without a terminator.
    Binary(Vec<u8>),
}

/// Materializes the streamable payload for a column value, or `None` when the
/// target C type is fixed-width.
pub(crate) fn stream_payload(
    value: &ColumnValues,
    target_type: SqlSmallInt,
) -> Option<Result<StreamPayload, WriteError>> {
    match target_type {
        SQL_C_CHAR | SQL_C_DEFAULT => Some(Ok(StreamPayload::Narrow(narrow_bytes(value)?))),
        SQL_C_WCHAR => {
            let cell = to_cell(value)?;
            Some(Ok(StreamPayload::Wide(
                cell.to_text().encode_utf16().collect(),
            )))
        }
        SQL_C_BINARY => {
            let Some(cell) = to_cell(value) else {
                // Vectors and other values without a Cell projection still have
                // a byte form on the wire; treat them as opaque binary.
                return Some(Err(WriteError::RestrictedConversion));
            };
            Some(match cell.as_bytes() {
                Some(bytes) => Ok(StreamPayload::Binary(bytes)),
                None => Err(WriteError::RestrictedConversion),
            })
        }
        _ => None,
    }
}

/// Produces the bytes msodbcsql hands back for a narrow (`SQL_C_CHAR`) target.
///
/// Character columns with a collation-derived encoding are passed through in
/// their original code page rather than transcoded to UTF-8: that is what the
/// native driver does on Windows, and clients decode using the column collation.
/// Everything else is rendered as text and encoded into the client ANSI code
/// page, which is what `SQL_C_CHAR` means to an ODBC application.
fn narrow_bytes(value: &ColumnValues) -> Option<Vec<u8>> {
    if let ColumnValues::String(s) = value
        && matches!(s.encoding_type(), EncodingType::LcidBased(_))
    {
        return Some(s.bytes.clone());
    }
    Some(crate::api::ansi::encode(&to_cell(value)?.to_text()))
}

/// Maps a column value to the `SQL_C_*` code msodbcsql reports through
/// `SQL_CA_SS_VARIANT_TYPE` for a `sql_variant` column.
pub(crate) fn variant_c_type(value: &ColumnValues) -> SqlSmallInt {
    match value {
        ColumnValues::TinyInt(_) => SQL_C_UTINYINT,
        ColumnValues::SmallInt(_) => SQL_C_SSHORT,
        ColumnValues::Int(_) => SQL_C_SLONG,
        ColumnValues::BigInt(_) => SQL_C_SBIGINT,
        ColumnValues::Real(_) => SQL_C_FLOAT,
        ColumnValues::Float(_) => SQL_C_DOUBLE,
        ColumnValues::Bit(_) => SQL_C_BIT,
        ColumnValues::Decimal(_)
        | ColumnValues::Numeric(_)
        | ColumnValues::Money(_)
        | ColumnValues::SmallMoney(_) => SQL_C_NUMERIC,
        ColumnValues::Bytes(_) => SQL_C_BINARY,
        ColumnValues::Uuid(_) => SQL_C_GUID,
        ColumnValues::Date(_) => SQL_C_TYPE_DATE,
        ColumnValues::Time(_) => SQL_C_TYPE_TIME,
        ColumnValues::DateTime2(_) | ColumnValues::DateTime(_) | ColumnValues::SmallDateTime(_) => {
            SQL_C_TYPE_TIMESTAMP
        }
        ColumnValues::String(s) if s.is_utf16() => SQL_C_WCHAR,
        ColumnValues::String(_) => SQL_C_CHAR,
        _ => SQL_C_WCHAR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::column_values::{SqlDate, SqlTime};
    use mssql_tds::datatypes::sql_string::SqlString;

    fn write<T>(
        value: &ColumnValues,
        ctype: SqlSmallInt,
        out: &mut T,
        ind: &mut SqlLen,
    ) -> WriteOutcome {
        unsafe {
            write_c_value(
                value,
                ctype,
                (out as *mut T).cast(),
                std::mem::size_of::<T>() as SqlLen,
                ind,
            )
        }
        .expect("conversion should succeed")
    }

    #[test]
    fn int_to_slong() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        assert_eq!(
            write(&ColumnValues::Int(42), SQL_C_SLONG, &mut out, &mut ind),
            WriteOutcome::Complete
        );
        assert_eq!(out, 42);
        assert_eq!(ind, 4);
    }

    #[test]
    fn bigint_to_slong_out_of_range() {
        let mut out: i32 = 0;
        let err = unsafe {
            write_c_value(
                &ColumnValues::BigInt(i64::MAX),
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(err, Err(WriteError::OutOfRange));
    }

    #[test]
    fn bit_to_bit() {
        let mut out: u8 = 9;
        let mut ind: SqlLen = 0;
        write(&ColumnValues::Bit(true), SQL_C_BIT, &mut out, &mut ind);
        assert_eq!(out, 1);
    }

    #[test]
    fn float_to_double() {
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        write(&ColumnValues::Float(1.5), SQL_C_DOUBLE, &mut out, &mut ind);
        assert!((out - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn date_to_date_struct() {
        // 1970-01-01 is day 719162 since 0001-01-01.
        let value = ColumnValues::Date(SqlDate::create(719_162).unwrap());
        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        write(&value, SQL_C_TYPE_DATE, &mut out, &mut ind);
        assert_eq!(
            out,
            SqlDateStruct {
                year: 1970,
                month: 1,
                day: 1
            }
        );
    }

    #[test]
    fn time_to_ss_time2() {
        // The TDS field counts 100-nanosecond ticks, and SQL_SS_TIME2 reports
        // nanoseconds.
        let value = ColumnValues::Time(SqlTime {
            time_nanoseconds: (13 * 3600 + 45 * 60 + 7) * 10_000_000 + 1_234_567,
            scale: 7,
        });
        let mut out = SqlSsTime2Struct::default();
        let mut ind: SqlLen = 0;
        write(&value, SQL_C_SS_TIME2, &mut out, &mut ind);
        assert_eq!(out.hour, 13);
        assert_eq!(out.minute, 45);
        assert_eq!(out.second, 7);
        assert_eq!(out.fraction, 123_456_700);
    }

    #[test]
    fn string_to_wchar_truncates() {
        let value = ColumnValues::String(SqlString::from_utf8_string("abcdef".into()));
        let mut buf = [0u16; 3];
        let mut ind: SqlLen = 0;
        let outcome = unsafe {
            write_c_value(
                &value,
                SQL_C_WCHAR,
                buf.as_mut_ptr().cast(),
                (buf.len() * 2) as SqlLen,
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Truncated);
        assert_eq!(ind, 12);
        assert_eq!(String::from_utf16(&buf[..2]).unwrap(), "ab");
    }

    #[test]
    fn bytes_to_binary() {
        let value = ColumnValues::Bytes(vec![1, 2, 3]);
        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let outcome = unsafe {
            write_c_value(
                &value,
                SQL_C_BINARY,
                buf.as_mut_ptr().cast(),
                buf.len() as SqlLen,
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(outcome, WriteOutcome::Complete);
        assert_eq!(ind, 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
    }

    #[test]
    fn null_writes_indicator() {
        let mut buf = [0u8; 4];
        let mut ind: SqlLen = 0;
        unsafe {
            write_c_value(
                &ColumnValues::Null,
                SQL_C_CHAR,
                buf.as_mut_ptr().cast(),
                4,
                &mut ind,
            )
        }
        .unwrap();
        assert_eq!(ind, SQL_NULL_DATA);
    }

    #[test]
    fn unsupported_ctype_is_rejected() {
        let err = unsafe {
            write_c_value(
                &ColumnValues::Int(1),
                12345,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(err, Err(WriteError::InvalidCType));
    }
}
