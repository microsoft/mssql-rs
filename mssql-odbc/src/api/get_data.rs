// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal SQLGetData implementation for Phase 1.

use mssql_tds::datatypes::column_values::ColumnValues;
use tracing::{debug, error};

use super::cdata::{
    StreamPayload, WriteError, WriteOutcome, stream_payload, wchar_capacity, write_c_value,
};
use super::odbc_types::{
    SQL_C_CHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NULL_DATA, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlUSmallInt,
    SqlWChar,
};
use super::sqlstate::*;
use super::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{STMT_STATE_CURSOR_OPEN, StmtState};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

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

    let value = row[col_index - 1].clone();

    // A NULL buffer is the "how long is this value?" probe (and, for
    // sql_variant, the call that primes SQL_CA_SS_VARIANT_TYPE). It must report
    // the length without consuming any of the value, and must succeed even when
    // the requested C type could not actually render the value — clients probe
    // with SQL_C_BINARY before they know the underlying type. `BufferLength` is
    // ignored for fixed-width C types, so a zero length only means "probe" for
    // the character and binary targets.
    let streamable = stream_payload(&value, target_type).is_some();
    if target_value_ptr.is_null() || (buffer_length == 0 && streamable) {
        let indicator = if matches!(value, ColumnValues::Null) {
            SQL_NULL_DATA
        } else {
            probe_length(&value, target_type)
        };
        unsafe { write_if_some(strlen_or_ind_ptr, indicator) };
        return SQL_SUCCESS;
    }

    if let Some(payload) = stream_payload(&value, target_type) {
        let payload = match payload {
            Ok(payload) => payload,
            Err(WriteError::RestrictedConversion) => {
                post_diag(&mut stmt_state, ERR_RESTRICTED_DATA_TYPE);
                return SQL_ERROR;
            }
            Err(_) => {
                post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
                return SQL_ERROR;
            }
        };
        return get_data_chunk(
            &mut stmt_state,
            col_index,
            &payload,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
    }

    // Fixed-width targets are delivered whole; a repeat call yields SQL_NO_DATA.
    if stmt_state.getdata_col == Some(col_index) && stmt_state.getdata_done {
        return SQL_NO_DATA;
    }
    stmt_state.getdata_col = Some(col_index);
    stmt_state.getdata_offset = 0;
    stmt_state.getdata_done = true;

    match unsafe {
        write_c_value(
            &value,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    } {
        Ok(WriteOutcome::Complete) => SQL_SUCCESS,
        Ok(WriteOutcome::Truncated) => {
            post_diag(&mut stmt_state, ERR_STRING_RIGHT_TRUNCATION);
            SQL_SUCCESS_WITH_INFO
        }
        Err(WriteError::InvalidCType) => {
            post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
            SQL_ERROR
        }
        Err(WriteError::RestrictedConversion) => {
            post_diag(&mut stmt_state, ERR_RESTRICTED_DATA_TYPE);
            SQL_ERROR
        }
        Err(WriteError::OutOfRange) => {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_22003,
                0,
                "Numeric value out of range",
            );
            SQL_ERROR
        }
    }
}

/// Byte length reported for a zero-length `SQLGetData` probe.
///
/// Falls back to the narrow rendering when the requested C type cannot express
/// the value, so that a probe never fails.
fn probe_length(value: &ColumnValues, target_type: SqlSmallInt) -> SqlLen {
    match stream_payload(value, target_type) {
        Some(Ok(StreamPayload::Narrow(b))) | Some(Ok(StreamPayload::Binary(b))) => {
            b.len() as SqlLen
        }
        Some(Ok(StreamPayload::Wide(w))) => (w.len() * size_of::<SqlWChar>()) as SqlLen,
        _ => match stream_payload(value, SQL_C_CHAR) {
            Some(Ok(StreamPayload::Narrow(b))) => b.len() as SqlLen,
            _ => 0,
        },
    }
}

/// Copies the next chunk of a streamable column value into the caller's buffer,
/// advancing the per-column offset.
///
/// Returns `SQL_SUCCESS_WITH_INFO` (01004) while data remains, `SQL_SUCCESS` on
/// the chunk that completes the value, and `SQL_NO_DATA` on any call after that.
/// The indicator always reports the number of bytes still available *before*
/// this call, which is what ODBC clients use to size the next read.
fn get_data_chunk(
    stmt_state: &mut StmtState,
    col_index: usize,
    payload: &StreamPayload,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if stmt_state.getdata_col != Some(col_index) {
        stmt_state.getdata_col = Some(col_index);
        stmt_state.getdata_offset = 0;
        stmt_state.getdata_done = false;
    }

    let (total_units, unit_size) = match payload {
        StreamPayload::Narrow(b) | StreamPayload::Binary(b) => (b.len(), 1usize),
        StreamPayload::Wide(w) => (w.len(), size_of::<SqlWChar>()),
    };

    if stmt_state.getdata_done {
        return SQL_NO_DATA;
    }

    let offset = stmt_state.getdata_offset;
    let remaining = total_units.saturating_sub(offset);
    unsafe { write_if_some(strlen_or_ind_ptr, (remaining * unit_size) as SqlLen) };

    // Character targets reserve one unit for the terminator; binary does not.
    let capacity = match payload {
        StreamPayload::Narrow(_) => (buffer_length.max(0) as usize).saturating_sub(1),
        StreamPayload::Wide(_) => wchar_capacity(buffer_length).saturating_sub(1),
        StreamPayload::Binary(_) => buffer_length.max(0) as usize,
    };
    let copied = remaining.min(capacity);

    unsafe {
        match payload {
            StreamPayload::Narrow(b) => {
                let dst = target_value_ptr as *mut u8;
                std::ptr::copy_nonoverlapping(b[offset..offset + copied].as_ptr(), dst, copied);
                *dst.add(copied) = 0;
            }
            StreamPayload::Wide(w) => {
                let dst = target_value_ptr as *mut SqlWChar;
                std::ptr::copy_nonoverlapping(w[offset..offset + copied].as_ptr(), dst, copied);
                *dst.add(copied) = 0;
            }
            StreamPayload::Binary(b) => {
                std::ptr::copy_nonoverlapping(
                    b[offset..offset + copied].as_ptr(),
                    target_value_ptr as *mut u8,
                    copied,
                );
            }
        }
    }

    stmt_state.getdata_offset = offset + copied;
    if stmt_state.getdata_offset >= total_units {
        stmt_state.getdata_done = true;
        SQL_SUCCESS
    } else {
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_LONG, SQL_C_WCHAR, SQL_NULL_DATA, SQL_NULL_HANDLE, SqlWChar,
    };
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::column_values::ColumnValues;
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
                12345,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_long_target_type_succeeds() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(7)]);
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
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 7);
        assert_eq!(ind, 4);
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
