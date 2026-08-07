// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal SQLGetData implementation for Phase 1.

use tracing::{debug, error};

use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NULL_DATA,
    SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{GetDataCursor, GetDataPayload, STMT_STATE_CURSOR_OPEN};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::datatypes::column_values::{ColumnValues, SqlTime};
use mssql_tds::datatypes::sql_string::EncodingType;

/// Implements SQLGetData for current-row retrieval.
///
/// Scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports `SQL_C_CHAR` and `SQL_C_WCHAR` output.
/// - Supports chunked reads: repeated calls on the same column advance through
///   the value and report `SQL_NO_DATA` once it has been fully delivered.
pub(crate) unsafe fn sql_get_data(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number,
        target_type,
        ?target_value_ptr,
        buffer_length,
        ?strlen_or_ind_ptr,
        "SQLGetData called",
    );

    crate::ffi_entry!("SQLGetData", unsafe {
        sql_get_data_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

unsafe fn sql_get_data_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetData: handle is not a STMT"
    );

    sql_get_data_safe(
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_get_data_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug_assert!(
        buffer_length >= 0,
        "SQLGetData: DM should reject negative buffer_length (HY090)"
    );

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    let Some(row_len) = stmt_state.current_row.as_ref().map(Vec::len) else {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    let col_index = usize::from(column_number);
    if col_index == 0 || col_index > row_len {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
        );
        return SQL_ERROR;
    }

    // Output buffer capacity in element units (u8 for SQL_C_CHAR, SqlWChar for
    // SQL_C_WCHAR). buffer_length is always in bytes per the ODBC spec.
    let wide = target_type == SQL_C_WCHAR;
    let buf_elements = if wide {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };

    // Continue an in-progress chunked read of this column, if any. A different
    // column or a different target type restarts the read.
    match stmt_state.get_data_cursor.take() {
        Some(cursor) if cursor.column == column_number && cursor.wide == wide => {
            let Some(payload) = cursor.payload else {
                // The value was already delivered in full.
                return SQL_NO_DATA;
            };
            return continue_chunked_read(
                &mut stmt_state,
                column_number,
                wide,
                payload,
                cursor.offset,
                target_value_ptr,
                buf_elements,
                strlen_or_ind_ptr,
            );
        }
        _ => {}
    }

    // Zero-copy fast path: an all-ASCII `varchar`/`char` value that fits the
    // caller's buffer is copied straight out of the row. This covers the bulk
    // of `SQL_C_CHAR` reads and skips both the code-page transcode and the
    // intermediate payload allocation that the general path needs for chunking.
    if !wide && !target_value_ptr.is_null() && buf_elements > 0 {
        let direct_len = stmt_state.current_row.as_ref().and_then(|row| {
            let ColumnValues::String(s) = &row[col_index - 1] else {
                return None;
            };
            let single_byte = matches!(
                s.encoding_type(),
                EncodingType::Utf8 | EncodingType::LcidBased(_)
            );
            if !single_byte || s.bytes.len() >= buf_elements || !s.bytes.is_ascii() {
                return None;
            }
            unsafe { copy_with_nul(target_value_ptr as *mut u8, buf_elements, &s.bytes) };
            Some(s.bytes.len())
        });
        if let Some(len) = direct_len {
            unsafe { write_if_some(strlen_or_ind_ptr, len as SqlLen) };
            stmt_state.get_data_cursor = Some(GetDataCursor::exhausted(column_number, wide));
            return SQL_SUCCESS;
        }
    }

    // Convert the cell before touching `stmt_state` mutably, so the borrow of
    // the current row ends here.
    let converted = {
        let Some(row) = stmt_state.current_row.as_ref() else {
            post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
            return SQL_ERROR;
        };
        let value = &row[col_index - 1];
        if matches!(value, ColumnValues::Null) {
            Converted::Null
        } else if wide {
            match column_value_to_utf16(value) {
                Some(v) => Converted::Wide(v),
                None => Converted::Unsupported,
            }
        } else {
            match column_value_to_utf8_bytes(value) {
                Some(v) => Converted::Narrow(v),
                None => Converted::Unsupported,
            }
        }
    };

    match converted {
        Converted::Unsupported => {
            post_unsupported_conversion(&mut stmt_state);
            SQL_ERROR
        }
        Converted::Null => {
            unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
            // Write a NUL terminator into the caller buffer when there's room.
            // The helper handles null `dst` and zero-length uniformly.
            if wide {
                unsafe {
                    copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
                }
            } else {
                unsafe {
                    copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]);
                }
            }
            stmt_state.get_data_cursor = Some(GetDataCursor::exhausted(column_number, wide));
            SQL_SUCCESS
        }
        Converted::Wide(utf16) => deliver_first_chunk(
            &mut stmt_state,
            column_number,
            wide,
            utf16,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
            GetDataPayload::Wide,
        ),
        Converted::Narrow(bytes) => deliver_first_chunk(
            &mut stmt_state,
            column_number,
            wide,
            bytes,
            target_value_ptr as *mut u8,
            buf_elements,
            strlen_or_ind_ptr,
            GetDataPayload::Narrow,
        ),
    }
}

/// A cell converted to the element width requested by the caller.
enum Converted {
    Null,
    Narrow(Vec<u8>),
    Wide(Vec<u16>),
    /// The column type has no text conversion yet.
    Unsupported,
}

fn post_unsupported_conversion(stmt_state: &mut crate::handles::stmt::StmtState) {
    post_sql_error(
        stmt_state,
        SQLSTATE_HYC00,
        0,
        "Column type conversion not yet implemented",
    );
}

/// Writes the first chunk of a freshly converted value and records how much of
/// it remains, so a truncated value can be resumed by the next call.
///
/// The full payload is buffered only when the value did not fit; a value that
/// fits leaves behind a cursor with no buffer.
#[allow(clippy::too_many_arguments)]
fn deliver_first_chunk<T: Copy + Default>(
    stmt_state: &mut crate::handles::stmt::StmtState,
    column_number: SqlUSmallInt,
    wide: bool,
    payload: Vec<T>,
    target_value_ptr: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
    wrap: fn(Vec<T>) -> GetDataPayload,
) -> SqlReturn {
    let written = write_chunk(
        stmt_state,
        &payload,
        target_value_ptr,
        buf_elements,
        strlen_or_ind_ptr,
    );
    match written {
        ChunkOutcome::Complete => {
            stmt_state.get_data_cursor = Some(GetDataCursor::exhausted(column_number, wide));
            SQL_SUCCESS
        }
        ChunkOutcome::Truncated(delivered) => {
            stmt_state.get_data_cursor = Some(GetDataCursor {
                column: column_number,
                wide,
                payload: Some(wrap(payload)),
                offset: delivered,
            });
            SQL_SUCCESS_WITH_INFO
        }
    }
}

/// Resumes a chunked read from `offset` using the already-converted payload, so
/// a long value is converted once rather than once per chunk.
#[allow(clippy::too_many_arguments)]
fn continue_chunked_read(
    stmt_state: &mut crate::handles::stmt::StmtState,
    column_number: SqlUSmallInt,
    wide: bool,
    payload: GetDataPayload,
    offset: usize,
    target_value_ptr: SqlPointer,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    macro_rules! resume {
        ($buf:expr, $ptr_ty:ty, $wrap:expr) => {{
            let outcome = write_chunk(
                stmt_state,
                &$buf[offset..],
                target_value_ptr as *mut $ptr_ty,
                buf_elements,
                strlen_or_ind_ptr,
            );
            match outcome {
                ChunkOutcome::Complete => {
                    stmt_state.get_data_cursor =
                        Some(GetDataCursor::exhausted(column_number, wide));
                    SQL_SUCCESS
                }
                ChunkOutcome::Truncated(delivered) => {
                    stmt_state.get_data_cursor = Some(GetDataCursor {
                        column: column_number,
                        wide,
                        payload: Some($wrap($buf)),
                        offset: offset + delivered,
                    });
                    SQL_SUCCESS_WITH_INFO
                }
            }
        }};
    }

    match payload {
        GetDataPayload::Narrow(buf) => resume!(buf, u8, GetDataPayload::Narrow),
        GetDataPayload::Wide(buf) => resume!(buf, SqlWChar, GetDataPayload::Wide),
    }
}

/// Result of copying one chunk into the application buffer.
enum ChunkOutcome {
    /// The remaining value fit entirely.
    Complete,
    /// The buffer filled up; the payload element count that was delivered.
    Truncated(usize),
}

/// Writes `src` to the caller's output buffer with ODBC string semantics:
/// the indicator (when present) reports the untruncated byte length of what
/// remains, the payload is NUL-terminated within the buffer, and truncation is
/// reported via SQLSTATE 01004 + `SQL_SUCCESS_WITH_INFO`.
///
/// `buf_elements` is the buffer capacity in units of `T` (not bytes).
///
/// The caller-provided pointers are written through small `unsafe` blocks
/// inside this function; both pointer arguments are obligations of the FFI
/// caller (validated against the buffer length passed by the DM).
fn write_chunk<T: Copy + Default>(
    stmt_state: &mut crate::handles::stmt::StmtState,
    src: &[T],
    target_value_ptr: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> ChunkOutcome {
    let byte_len = std::mem::size_of_val(src) as SqlLen;
    unsafe { write_if_some(strlen_or_ind_ptr, byte_len) };
    let truncated = unsafe { copy_with_nul(target_value_ptr, buf_elements, src) };
    if truncated {
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        // `copy_with_nul` reserves the final element for the terminator.
        ChunkOutcome::Truncated(buf_elements.saturating_sub(1))
    } else {
        ChunkOutcome::Complete
    }
}

/// Converts a column value to UTF-8 bytes for `SQL_C_CHAR` output.
fn column_value_to_utf8_bytes(v: &ColumnValues) -> Option<Vec<u8>> {
    match v {
        // Already UTF-8 on the wire: hand back the bytes without transcoding.
        ColumnValues::String(s) if matches!(s.encoding_type(), EncodingType::Utf8) => {
            Some(s.bytes.clone())
        }
        // Every single-byte SQL Server code page agrees with US-ASCII below
        // 0x80, so all-ASCII payloads are already their own UTF-8 encoding.
        // The check is vectorized and skips a full code-page transcode, which
        // is the common case for `varchar` columns.
        ColumnValues::String(s)
            if matches!(s.encoding_type(), EncodingType::LcidBased(_)) && s.bytes.is_ascii() =>
        {
            Some(s.bytes.clone())
        }
        _ => column_value_to_text(v).map(String::into_bytes),
    }
}

/// Converts a column value to UTF-16 code units for `SQL_C_WCHAR` output.
///
/// Character data arrives from SQL Server as UTF-16LE, which is exactly what
/// `SQL_C_WCHAR` wants. Reinterpreting those bytes avoids a UTF-16 -> UTF-8 ->
/// UTF-16 round trip through an intermediate `String`.
fn column_value_to_utf16(v: &ColumnValues) -> Option<Vec<u16>> {
    if let ColumnValues::String(s) = v
        && let Some(bytes) = s.as_utf16_bytes()
        && bytes.len() % 2 == 0
    {
        return Some(
            bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
        );
    }
    column_value_to_text(v).map(|t| t.encode_utf16().collect())
}

fn column_value_to_text(v: &ColumnValues) -> Option<String> {
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
        ColumnValues::Null => Some(String::new()),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Some(d.to_string()),
        ColumnValues::Date(d) => Some(format_date(d.get_days())),
        ColumnValues::Time(t) => Some(format_time(t)),
        ColumnValues::DateTime2(dt) => Some(format!(
            "{} {}",
            format_date(dt.days),
            format_time(&dt.time)
        )),
        ColumnValues::DateTimeOffset(dto) => {
            let (sign, mins) = if dto.offset < 0 {
                ('-', (-(dto.offset as i32)) as u32)
            } else {
                ('+', dto.offset as u32)
            };
            Some(format!(
                "{} {} {sign}{:02}:{:02}",
                format_date(dto.datetime2.days),
                format_time(&dto.datetime2.time),
                mins / 60,
                mins % 60
            ))
        }
        // `datetime` counts days from 1900-01-01 in 1/300-second ticks.
        ColumnValues::DateTime(dt) => {
            let days = DAYS_0001_TO_1900 as i64 + dt.days as i64;
            let nanos = (dt.time as u64) * 10_000_000 / 3;
            Some(format!(
                "{} {}",
                format_date(days.clamp(0, u32::MAX as i64) as u32),
                format_nanos(nanos, 3)
            ))
        }
        ColumnValues::SmallDateTime(dt) => {
            let days = DAYS_0001_TO_1900 + u32::from(dt.days);
            let nanos = u64::from(dt.time) * 60 * 1_000_000_000;
            Some(format!("{} {}", format_date(days), format_nanos(nanos, 0)))
        }
        ColumnValues::SmallMoney(m) => Some(format_money(i64::from(m.int_val))),
        ColumnValues::Money(m) => {
            let scaled = (i64::from(m.msb_part) << 32) | (i64::from(m.lsb_part) & 0xFFFF_FFFF);
            Some(format_money(scaled))
        }
        ColumnValues::Bytes(b) => {
            let mut s = String::with_capacity(b.len() * 2);
            for byte in b {
                use std::fmt::Write;
                let _ = write!(s, "{byte:02X}");
            }
            Some(s)
        }
        ColumnValues::Xml(x) => Some(x.as_string()),
        _ => None,
    }
}

/// Days from 0001-01-01 to 1900-01-01, the epoch used by `datetime` and
/// `smalldatetime`.
const DAYS_0001_TO_1900: u32 = 693_595;

/// Formats a day count from 0001-01-01 as `YYYY-MM-DD`.
///
/// Uses the civil-from-days algorithm, shifting the year to start in March so
/// the leap day falls at the end of the cycle and month lengths follow a
/// regular pattern.
fn format_date(days_since_year_one: u32) -> String {
    // Re-base onto the 1970-01-01 era the algorithm is defined against.
    const DAYS_0001_TO_1970: i64 = 719_162;
    let z = days_since_year_one as i64 - DAYS_0001_TO_1970 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a [`SqlTime`] as `HH:MM:SS[.fffffff]`, honouring its scale.
fn format_time(t: &SqlTime) -> String {
    format_nanos(t.time_nanoseconds, t.scale)
}

/// Formats nanoseconds since midnight as `HH:MM:SS[.fffffff]`, emitting
/// `scale` fractional digits (none when `scale` is 0).
fn format_nanos(nanos: u64, scale: u8) -> String {
    let secs = nanos / 1_000_000_000;
    let frac = nanos % 1_000_000_000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if scale == 0 {
        return format!("{h:02}:{m:02}:{s:02}");
    }
    let scale = scale.min(9) as u32;
    let divisor = 10u64.pow(9 - scale);
    format!(
        "{h:02}:{m:02}:{s:02}.{:0width$}",
        frac / divisor,
        width = scale as usize
    )
}

/// Formats a money value stored as a ×10⁴ scaled integer.
fn format_money(scaled: i64) -> String {
    let sign = if scaled < 0 { "-" } else { "" };
    let abs = scaled.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::SqlString;

    #[test]
    fn format_date_known_epochs() {
        // 1900-01-01 is the SQL Server datetime epoch.
        assert_eq!(format_date(DAYS_0001_TO_1900), "1900-01-01");
        assert_eq!(format_date(0), "0001-01-01");
        assert_eq!(format_date(719_162), "1970-01-01");
        // 2000 is a leap year; day 60 of that year is 2000-02-29.
        assert_eq!(format_date(DAYS_0001_TO_1900 + 36_583), "2000-02-29");
    }

    #[test]
    fn format_money_scales_and_signs() {
        assert_eq!(format_money(0), "0.0000");
        assert_eq!(format_money(1), "0.0001");
        assert_eq!(format_money(123_456), "12.3456");
        assert_eq!(format_money(-123_456), "-12.3456");
        assert_eq!(
            format_money(9_223_372_036_854_775_807),
            "922337203685477.5807"
        );
    }

    #[test]
    fn money_mixed_endian_reassembly() {
        // TDS transmits `money` as MSB i32 then LSB i32. A negative value has an
        // all-ones MSB word that must not sign-extend the LSB word.
        let reassemble =
            |msb: i32, lsb: i32| (i64::from(msb) << 32) | (i64::from(lsb) & 0xFFFF_FFFF);
        let expected: i64 = -123_456;
        let msb = (expected >> 32) as i32;
        let lsb = expected as i32;
        assert_eq!(reassemble(msb, lsb), expected);
        assert_eq!(format_money(reassemble(msb, lsb)), "-12.3456");

        let big: i64 = 9_223_372_036_854_775_807;
        assert_eq!(reassemble((big >> 32) as i32, big as i32), big);
    }

    #[test]
    fn format_nanos_honours_scale() {
        assert_eq!(format_nanos(0, 0), "00:00:00");
        assert_eq!(format_nanos(3_723_000_000_000, 0), "01:02:03");
        assert_eq!(format_nanos(3_723_123_456_700, 7), "01:02:03.1234567");
        assert_eq!(format_nanos(3_723_123_456_700, 3), "01:02:03.123");
    }

    #[test]
    fn get_data_null_handle() {
        let ret = unsafe {
            sql_get_data(
                SQL_NULL_HANDLE,
                1,
                SQL_C_CHAR,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_data_without_cursor_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_string_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "hello".to_string(),
            ))]);
        }

        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 5);
        assert_eq!(std::str::from_utf8(&buf[..5]).unwrap(), "hello");
    }

    #[test]
    fn get_data_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        let mut buf = [0u8; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
    }

    #[test]
    fn get_data_empty_string_zero_buffer_no_truncation() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                String::new(),
            ))]);
        }

        let mut ind: SqlLen = -1;
        let ret = unsafe { sql_get_data(stmt, 1, SQL_C_CHAR, std::ptr::null_mut(), 0, &mut ind) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 0);
    }

    #[test]
    fn get_data_null_column_writes_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0u8; 4];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
    }

    #[test]
    fn get_data_unsupported_target_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_LONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_invalid_column_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                2,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    /// Helper: read a NUL-terminated UTF-16 buffer back to a Rust String.
    fn read_until_nul(buf: &[u16]) -> String {
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..len]).unwrap()
    }

    #[test]
    fn get_data_wchar_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "héllo".to_string(),
            ))]);
        }

        let mut buf = [0u16; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        // Indicator is byte length of untruncated value, excluding NUL.
        // "héllo" → 5 u16 units → 10 bytes.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "héllo");
    }

    #[test]
    fn get_data_wchar_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        // 3 u16 slots = 6 bytes. "12345" needs 6 units (5 chars + NUL) → truncated.
        let mut buf = [0u16; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        // Untruncated byte length: 5 chars × 2 bytes = 10.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "12");
    }

    #[test]
    fn get_data_wchar_null_column_writes_nul_and_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0xDEADu16; 4];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
        // First slot must be NUL; nothing else touched.
        assert_eq!(buf[0], 0);
        assert_eq!(&buf[1..], &[0xDEAD; 3]);
    }
}
