// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation for current-row retrieval.
//!
//! Delegates value conversion to the shared [`fetch_convert`](super::fetch_convert)
//! core and covers:
//! - integer C targets (`SQL_C_TINYINT`..`SQL_C_SBIGINT`, `SQL_C_BIT`, signed
//!   and unsigned), range-checked;
//! - floating-point targets (`SQL_C_FLOAT` / `SQL_C_DOUBLE`);
//! - `SQL_C_GUID`;
//! - the date/time C structs (`SQL_C_TYPE_DATE` / `TIME` / `TIMESTAMP`,
//!   `SQL_C_SS_TIME2`, `SQL_C_SS_TIMESTAMPOFFSET`);
//! - character targets (`SQL_C_CHAR` / `SQL_C_WCHAR`) for every scalar type via
//!   text formatting, and `SQL_C_BINARY` for binary/xml/json;
//! - chunked-offset streaming for the variable-length (character / binary)
//!   targets: repeated calls advance a per-column offset, report the remaining
//!   length, warn with `01004`, and yield `SQL_NO_DATA` once exhausted.
//!
//! A `NULL` column reports `SQL_NULL_DATA` for any target (this also serves the
//! `sql_variant` `SQL_C_BINARY` NULL probe). Full `sql_variant` underlying-type
//! resolution depends on `SQLColAttributeW` (P2).

use tracing::{debug, error};

use super::fetch_convert::{
    ConvError, convert_datetime_c, convert_float_c, convert_guid_c, convert_integer_c,
    extract_datetime_parts, format_datetime_parts, is_datetime_c_target, is_float_c_target,
    is_integer_c_target,
};
use super::odbc_types::{
    SQL_C_BINARY, SQL_C_CHAR, SQL_C_GUID, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA,
    SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::datatypes::column_values::ColumnValues;

/// Implements SQLGetData for current-row retrieval.
///
/// Phase 1 scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports only `SQL_C_CHAR` output.
/// - Supports basic scalar conversion to UTF-8 text.
/// - Repeated calls on the same column do not advance an offset; each call
///   returns the same prefix for the current value (no chunked streaming yet).
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

    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    let col_index = usize::from(column_number);
    if col_index == 0 || col_index > row.len() {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // A repeated SQLGetData on a column whose value was already fully returned
    // yields SQL_NO_DATA (the streaming cursor is exhausted).
    if stmt_state.getdata_col == column_number && stmt_state.getdata_offset == usize::MAX {
        return SQL_NO_DATA;
    }

    let value = &row[col_index - 1];
    if matches!(value, ColumnValues::Null) {
        let ret = write_null_result(
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
        mark_column_done(&mut stmt_state, column_number);
        return ret;
    }

    // Fixed / typed C targets go through the shared conversion core. They return
    // the whole value in one call, so the column is marked done on success.
    let typed = if is_integer_c_target(target_type) {
        Some(unsafe { convert_integer_c(value, target_type, target_value_ptr, strlen_or_ind_ptr) })
    } else if is_float_c_target(target_type) {
        Some(unsafe { convert_float_c(value, target_type, target_value_ptr, strlen_or_ind_ptr) })
    } else if target_type == SQL_C_GUID {
        Some(unsafe { convert_guid_c(value, target_type, target_value_ptr, strlen_or_ind_ptr) })
    } else if is_datetime_c_target(target_type) {
        Some(unsafe { convert_datetime_c(value, target_type, target_value_ptr, strlen_or_ind_ptr) })
    } else {
        None
    };
    if let Some(r) = typed {
        let ret = finish_typed_conv(&mut stmt_state, r);
        if ret == SQL_SUCCESS || ret == SQL_SUCCESS_WITH_INFO {
            mark_column_done(&mut stmt_state, column_number);
        }
        return ret;
    }

    // Variable-length targets (character and binary) stream across repeated
    // calls, advancing a per-column offset.
    let resuming = stmt_state.getdata_col == column_number;
    let offset = if resuming {
        stmt_state.getdata_offset
    } else {
        0
    };

    if target_type == SQL_C_BINARY {
        let Some(bytes) = column_value_to_bytes(value) else {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            );
            return SQL_ERROR;
        };
        let outcome = stream_binary(
            offset,
            bytes,
            target_value_ptr as *mut u8,
            buffer_length as usize,
            strlen_or_ind_ptr,
        );
        return finish_stream(&mut stmt_state, column_number, outcome);
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

    let Some(as_text) = column_value_to_text(value) else {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HYC00,
            0,
            "Column type conversion not yet implemented",
        );
        return SQL_ERROR;
    };

    // buffer_length is always in bytes per the ODBC spec; SQL_C_WCHAR streams in
    // units of SqlWChar.
    let outcome = if target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = as_text.encode_utf16().collect();
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        stream_text(
            offset,
            &utf16,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
        )
    } else {
        stream_text(
            offset,
            as_text.as_bytes(),
            target_value_ptr as *mut u8,
            buffer_length as usize,
            strlen_or_ind_ptr,
        )
    };
    finish_stream(&mut stmt_state, column_number, outcome)
}

/// Marks `column_number` as fully returned so a subsequent `SQLGetData` on the
/// same column yields `SQL_NO_DATA`.
fn mark_column_done(stmt_state: &mut crate::handles::stmt::StmtState, column_number: SqlUSmallInt) {
    stmt_state.getdata_col = column_number;
    stmt_state.getdata_offset = usize::MAX;
}

/// Maps a [`ConvError`] result from the shared conversion core to an ODBC
/// return code, posting the appropriate diagnostic on the statement.
fn finish_typed_conv(
    stmt_state: &mut crate::handles::stmt::StmtState,
    r: Result<SqlReturn, ConvError>,
) -> SqlReturn {
    match r {
        Ok(ret) => ret,
        Err(ConvError::OutOfRange) => {
            post_diag(stmt_state, ERR_NUMERIC_OUT_OF_RANGE);
            SQL_ERROR
        }
        Err(ConvError::Unsupported) => {
            post_sql_error(
                stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            );
            SQL_ERROR
        }
    }
}

/// Writes the NULL indicator and a NUL terminator (for character targets) for a
/// SQL `NULL` column value.
fn write_null_result(
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
    // Write a NUL terminator into the caller buffer when there's room. The
    // helper handles null `dst` and zero-length uniformly. Only meaningful for
    // character targets; fixed-width targets leave the buffer untouched on NULL.
    if target_type == SQL_C_WCHAR {
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        unsafe {
            copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
        }
    } else if target_type == SQL_C_CHAR {
        unsafe {
            copy_with_nul(target_value_ptr as *mut u8, buffer_length as usize, &[]);
        }
    }
    SQL_SUCCESS
}

/// Result of a single streaming chunk: the new per-column offset and whether
/// the value was truncated (more remains). Carries no borrow so the caller can
/// update the statement state after the row borrow is released.
struct StreamOutcome {
    new_offset: usize,
    truncated: bool,
}

/// Applies a [`StreamOutcome`] to the statement's streaming cursor and returns
/// the ODBC code: `01004` + `SQL_SUCCESS_WITH_INFO` on truncation, otherwise the
/// column is marked done and `SQL_SUCCESS` is returned.
fn finish_stream(
    stmt_state: &mut crate::handles::stmt::StmtState,
    column_number: SqlUSmallInt,
    outcome: StreamOutcome,
) -> SqlReturn {
    stmt_state.getdata_col = column_number;
    if outcome.truncated {
        stmt_state.getdata_offset = outcome.new_offset;
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        stmt_state.getdata_offset = usize::MAX;
        SQL_SUCCESS
    }
}

/// Streams a character value (`src` in units of `T` = `u8` for `SQL_C_CHAR` or
/// `SqlWChar` for `SQL_C_WCHAR`) from `offset` (in elements). Reports the
/// remaining post-offset byte length in the indicator and NUL-terminates the
/// chunk within the buffer.
///
/// The caller-provided pointers are obligations of the FFI caller (validated
/// against the buffer length passed by the DM).
fn stream_text<T: Copy + Default>(
    offset: usize,
    src: &[T],
    target_value_ptr: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> StreamOutcome {
    let remaining = &src[offset.min(src.len())..];
    let byte_len = std::mem::size_of_val(remaining) as SqlLen;
    unsafe { write_if_some(strlen_or_ind_ptr, byte_len) };

    // `copy_with_nul` reserves one element for the terminator; it copies
    // `min(remaining, buf_elements - 1)` data elements.
    let data_fit = buf_elements.saturating_sub(1);
    let truncated = unsafe { copy_with_nul(target_value_ptr, buf_elements, remaining) };
    StreamOutcome {
        new_offset: offset + data_fit,
        truncated,
    }
}

/// Streams a binary value from `offset` (in bytes). Binary data is not
/// NUL-terminated: copies up to `buffer_len` bytes and reports the remaining
/// byte length in the indicator.
fn stream_binary(
    offset: usize,
    src: &[u8],
    target_value_ptr: *mut u8,
    buffer_len: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> StreamOutcome {
    let remaining = &src[offset.min(src.len())..];
    unsafe { write_if_some(strlen_or_ind_ptr, remaining.len() as SqlLen) };

    let copy_n = remaining.len().min(buffer_len);
    if copy_n > 0 && !target_value_ptr.is_null() {
        unsafe {
            std::ptr::copy_nonoverlapping(remaining.as_ptr(), target_value_ptr, copy_n);
        }
    }
    StreamOutcome {
        new_offset: offset + copy_n,
        truncated: copy_n < remaining.len(),
    }
}

/// Formats a SQL Server `vector` value as a JSON-style array of its float
/// elements (e.g. `[1,2.5,3]`), matching the textual form SQL Server produces
/// when a vector is cast to a character type.
fn format_vector(v: &mssql_tds::datatypes::sql_vector::SqlVector) -> String {
    use mssql_tds::datatypes::sql_vector::VectorData;
    let floats = match &v.data {
        VectorData::Float32(xs) | VectorData::Float16(xs) => xs,
    };
    let mut s = String::from("[");
    for (i, f) in floats.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&f.to_string());
    }
    s.push(']');
    s
}

/// Raw bytes for a binary (`SQL_C_BINARY`) target. `binary` / `varbinary` /
/// `image` arrive as [`ColumnValues::Bytes`]; `xml` / `json` expose their
/// encoded bytes. Returns `None` for sources without a binary representation.
fn column_value_to_bytes(v: &ColumnValues) -> Option<&[u8]> {
    match v {
        ColumnValues::Bytes(b) => Some(b),
        ColumnValues::Xml(x) => Some(&x.bytes),
        ColumnValues::Json(j) => Some(&j.bytes),
        _ => None,
    }
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
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Some(d.to_string()),
        ColumnValues::Money(m) => Some(money_scaled_to_string(
            (i64::from(m.lsb_part) & 0xFFFF_FFFF) | (i64::from(m.msb_part) << 32),
        )),
        ColumnValues::SmallMoney(m) => Some(money_scaled_to_string(i64::from(m.int_val))),
        ColumnValues::String(s) => Some(s.to_utf8_string()),
        ColumnValues::Xml(x) => Some(x.as_string()),
        ColumnValues::Json(j) => Some(j.as_string()),
        ColumnValues::Uuid(u) => Some(u.to_string()),
        ColumnValues::Vector(vec) => Some(format_vector(vec)),
        ColumnValues::Date(_)
        | ColumnValues::Time(_)
        | ColumnValues::DateTime(_)
        | ColumnValues::DateTime2(_)
        | ColumnValues::DateTimeOffset(_)
        | ColumnValues::SmallDateTime(_) => {
            extract_datetime_parts(v).map(|p| format_datetime_parts(&p))
        }
        ColumnValues::Null => Some(String::new()),
        _ => None,
    }
}

/// Formats a SQL Server `money` / `smallmoney` value (stored as an integer
/// scaled by 10^4) as a fixed 4-decimal string, without the precision loss of
/// an intermediate `f64`.
fn money_scaled_to_string(scaled: i64) -> String {
    let neg = scaled < 0;
    let abs = scaled.unsigned_abs();
    let int_part = abs / 10_000;
    let frac = abs % 10_000;
    format!("{}{int_part}.{frac:04}", if neg { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_BINARY, SQL_C_BIT, SQL_C_DOUBLE, SQL_C_SLONG, SQL_C_TYPE_TIMESTAMP, SQL_NO_DATA,
        SQL_NULL_HANDLE,
    };
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::SqlString;

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
    fn get_data_unsupported_conversion() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Bytes(vec![1, 2, 3])]);
        }

        // A binary column cannot be delivered to a floating-point C target.
        let mut out: f64 = 0.0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_DOUBLE,
                (&mut out as *mut f64).cast(),
                std::mem::size_of::<f64>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_int_to_slong_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(-2_000_000)]);
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = -99;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, -2_000_000);
        assert_eq!(ind, std::mem::size_of::<i32>() as SqlLen);
    }

    #[test]
    fn get_data_bigint_out_of_range_for_slong_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::BigInt(i64::from(i32::MAX) + 1)]);
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_bit_to_bit_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Bit(true)]);
        }

        let mut out: u8 = 0xFF;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_BIT,
                (&mut out as *mut u8).cast(),
                std::mem::size_of::<u8>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 1);
        assert_eq!(ind, 1);
    }

    #[test]
    fn get_data_null_int_column_leaves_buffer_and_sets_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut out: i32 = 0x5A5A_5A5A;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
        // Fixed-width targets leave the caller buffer untouched on NULL.
        assert_eq!(out, 0x5A5A_5A5A);
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

    fn open_row(stmt: SqlHandle, value: ColumnValues) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        s.set_state(STMT_STATE_CURSOR_OPEN);
        s.current_row = Some(vec![value]);
    }

    #[test]
    fn get_data_binary_full_read_then_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        open_row(h.stmt, ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 4);
        assert_eq!(&buf[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);

        // A second call on the fully-read column yields SQL_NO_DATA.
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }

    #[test]
    fn get_data_binary_chunked_streaming() {
        let h = TestHandles::with_env_dbc_stmt();
        open_row(h.stmt, ColumnValues::Bytes(vec![1, 2, 3, 4, 5]));

        // First 2-byte chunk: 5 remaining reported, truncation warning.
        let mut buf = [0u8; 2];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
        assert_eq!(buf, [1, 2]);

        // Second chunk.
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 3);
        assert_eq!(buf, [3, 4]);

        // Final byte: fits, SUCCESS.
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 1);
        assert_eq!(buf[0], 5);

        // Exhausted.
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                2,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }

    #[test]
    fn get_data_char_chunked_streaming() {
        let h = TestHandles::with_env_dbc_stmt();
        open_row(
            h.stmt,
            ColumnValues::String(SqlString::from_utf8_string("abcde".to_string())),
        );

        // 3-byte buffer holds 2 data chars + NUL each chunk.
        let mut buf = [0u8; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                3,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
        assert_eq!(&buf[..2], b"ab");
        assert_eq!(buf[2], 0);

        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                3,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(&buf[..2], b"cd");

        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                3,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(buf[0], b'e');
        assert_eq!(buf[1], 0);
    }

    #[test]
    fn get_data_timestamp_struct_via_entry() {
        use crate::api::odbc_types::SqlTimestampStruct;
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
        let h = TestHandles::with_env_dbc_stmt();
        open_row(
            h.stmt,
            ColumnValues::DateTime2(SqlDateTime2 {
                days: 738_685, // 2023-06-15
                time: SqlTime {
                    time_nanoseconds: 0,
                    scale: 7,
                },
            }),
        );

        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                std::mem::size_of::<SqlTimestampStruct>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out.year, 2023);
        assert_eq!(out.month, 6);
        assert_eq!(out.day, 15);
    }

    #[test]
    fn get_data_decimal_to_char() {
        use mssql_tds::datatypes::decoder::DecimalParts;
        let h = TestHandles::with_env_dbc_stmt();
        let d = DecimalParts::from_string("123.45", 5, 2).unwrap();
        open_row(h.stmt, ColumnValues::Numeric(d));

        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(&buf[..ind as usize], b"123.45");
    }

    #[test]
    fn get_data_vector_to_char() {
        use mssql_tds::datatypes::sql_vector::SqlVector;
        let h = TestHandles::with_env_dbc_stmt();
        open_row(
            h.stmt,
            ColumnValues::Vector(SqlVector::try_from_f32(vec![1.0, 2.5, 3.0]).unwrap()),
        );

        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(&buf[..ind as usize], b"[1,2.5,3]");
    }

    #[test]
    fn get_data_fixed_repeat_yields_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        open_row(h.stmt, ColumnValues::Int(7));

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 7);

        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }
}
