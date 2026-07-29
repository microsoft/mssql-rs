// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation with incremental row materialization.

use tracing::{debug, error};

use super::odbc_row_writer::OdbcRowWriter;
use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_TOTAL, SQL_NULL_DATA,
    SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::datatypes::column_values::ColumnValues;
#[cfg(test)]
use mssql_tds::datatypes::sql_string::SqlString;

/// Implements SQLGetData for current-row retrieval.
///
/// Current scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports `SQL_C_CHAR` and `SQL_C_WCHAR` for text retrieval.
/// - Supports incremental row resume and chunked PLP retrieval via
///   `read_active_plp_bytes` + `active_plp_reached_end`.
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
        statement_handle,
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_get_data_safe(
    statement_handle: SqlHandle,
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

    let col_index = usize::from(column_number);
    let metadata_len = if stmt_state.column_metadata.is_empty() {
        stmt_state
            .current_row
            .as_ref()
            .map(|row| row.len())
            .unwrap_or(0)
    } else {
        stmt_state.column_metadata.len()
    };
    if col_index == 0 || col_index > metadata_len {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // Continuation: app is calling SQLGetData again on the same PLP column to
    // get the next chunk from the active wire stream.
    if stmt_state.active_plp_column == Some(col_index) {
        drop(stmt_state);
        return stream_active_plp_chunk(
            stmt,
            statement_handle,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
            false,
        );
    }

    // If the app jumps to a different column while a PLP stream was open —
    // incorrect usage per the ODBC spec — clear the stale stream state.
    if stmt_state.active_plp_column.is_some() {
        stmt_state.active_plp_column = None;
        stmt_state.active_plp_target_type = None;
    }

    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    let need_resume = row.len() < col_index && !stmt_state.current_row_complete;

    if need_resume {
        drop(stmt_state);
        let rc = resume_row_to_column(stmt, statement_handle, col_index);
        if rc != SQL_SUCCESS {
            return rc;
        }
        let Ok(mut reopened_stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned after row resume");
            return SQL_ERROR;
        };
        // Column still missing from row -> decoder paused on a PLP column.
        // Stream this column directly from the active PLP reader.
        let col_in_row = reopened_stmt_state
            .current_row
            .as_ref()
            .map(|r| r.len() >= col_index)
            .unwrap_or(false);
        if !col_in_row && !reopened_stmt_state.current_row_complete {
            drop(reopened_stmt_state);
            return stream_active_plp_chunk(
                stmt,
                statement_handle,
                col_index,
                target_type,
                target_value_ptr,
                buffer_length,
                strlen_or_ind_ptr,
                true,
            );
        }
        return write_column_as_text(
            &mut reopened_stmt_state,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
    }

    write_column_as_text(
        &mut stmt_state,
        col_index,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn write_column_as_text(
    stmt_state: &mut crate::handles::stmt::StmtState,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    if row.len() < col_index {
        post_sql_error(
            stmt_state,
            SQLSTATE_24000,
            0,
            "Requested column is not available in the current row",
        );
        return SQL_ERROR;
    }

    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
        );
        return SQL_ERROR;
    }

    // Output buffer capacity in element units (u8 for SQL_C_CHAR, SqlWChar for
    // SQL_C_WCHAR). buffer_length is always in bytes per the ODBC spec.
    let buf_elements = if target_type == SQL_C_WCHAR {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };

    let value = &row[col_index - 1];
    if matches!(value, ColumnValues::Null) {
        unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
        // Write a NUL terminator into the caller buffer when there's room. The
        // helper handles null `dst` and zero-length uniformly.
        if target_type == SQL_C_WCHAR {
            unsafe {
                copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
            }
        } else {
            unsafe {
                copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]);
            }
        }
        return SQL_SUCCESS;
    }

    let Some(as_text) = column_value_to_text(value) else {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Column type conversion not yet implemented",
        );
        return SQL_ERROR;
    };

    if target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = as_text.encode_utf16().collect();
        write_string_result(
            stmt_state,
            &utf16,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
        )
    } else {
        write_string_result(
            stmt_state,
            as_text.as_bytes(),
            target_value_ptr as *mut u8,
            buf_elements,
            strlen_or_ind_ptr,
        )
    }
}

fn resume_row_to_column(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    column_number: usize,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    let (col_count, current_row) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while preparing row resume");
            return SQL_ERROR;
        };

        let Some(row) = stmt_state.current_row.take() else {
            post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
            return SQL_ERROR;
        };

        (stmt_state.column_metadata.len(), row)
    };

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLGetData: dbc mutex poisoned while resuming row");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
            }
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
                post_diag(&mut stmt_state, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
                post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    let mut writer = OdbcRowWriter::from_row(current_row, col_count);
    writer.request_pause_after_column(column_number);

    let row_read = dbc.runtime.block_on(client.next_row_into(&mut writer));
    let row_complete = writer.row_complete();
    let updated_row = writer.into_row();

    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        dbc_state.active_stmt = Some(statement_handle);
    }

    match row_read {
        Ok(true) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(updated_row);
                stmt_state.current_row_complete = row_complete;
                return SQL_SUCCESS;
            }
            SQL_ERROR
        }
        Ok(false) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(updated_row);
                stmt_state.current_row_complete = true;
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_24000,
                    0,
                    "Result set exhausted while resuming current row",
                );
            }
            SQL_ERROR
        }
        Err(e) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = None;
                stmt_state.current_row_complete = false;
                stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
            }
            SQL_ERROR
        }
    }
}

/// Reads and returns one SQLGetData chunk directly from the active PLP stream.
///
/// This never buffers the full PLP payload in ODBC-layer memory. The TDS
/// client remains the owner of stream state between repeated calls.
#[allow(clippy::too_many_arguments)]
fn stream_active_plp_chunk(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
    starting_new_stream: bool,
) -> SqlReturn {
    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        if let Ok(mut s) = stmt.inner.lock() {
            post_sql_error(&mut s, SQLSTATE_HYC00, 0, "Target type not yet implemented");
        }
        return SQL_ERROR;
    }

    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while preparing PLP stream read");
            return SQL_ERROR;
        };

        if starting_new_stream {
            stmt_state.active_plp_column = Some(col_index);
            stmt_state.active_plp_target_type = None;
            stmt_state.active_plp_is_unicode = stmt_state
                .column_metadata
                .get(col_index - 1)
                .is_some_and(|m| m.is_unicode_text());
        }

        if stmt_state.active_plp_column != Some(col_index) {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_24000,
                0,
                "No active PLP stream for this column",
            );
            return SQL_ERROR;
        }

        if let Some(existing_target) = stmt_state.active_plp_target_type
            && existing_target != target_type
        {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Switching SQLGetData target type during PLP streaming is not supported",
            );
            return SQL_ERROR;
        }
    }

    let payload_capacity = if target_type == SQL_C_WCHAR {
        (buffer_length as usize).saturating_sub(std::mem::size_of::<SqlWChar>())
    } else {
        (buffer_length as usize).saturating_sub(1)
    };
    let max_read = if target_type == SQL_C_WCHAR {
        payload_capacity & !1
    } else {
        payload_capacity
    };
    let mut payload = vec![0u8; max_read];

    let is_unicode_plp = {
        let Ok(ss) = stmt.inner.lock() else {
            return SQL_ERROR;
        };
        ss.active_plp_is_unicode
    };

    let dbc = stmt.parent_dbc();
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLGetData: dbc mutex poisoned while reading PLP stream");
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut s) = stmt.inner.lock() {
                post_diag(&mut s, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut s) = stmt.inner.lock() {
                post_diag(&mut s, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    let read_result = dbc
        .runtime
        .block_on(client.read_active_plp_bytes(&mut payload));
    let reached_end = client.active_plp_reached_end();

    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        dbc_state.active_stmt = Some(statement_handle);
    }

    let read = match read_result {
        Ok(n) => n,
        Err(e) => {
            if let Ok(mut s) = stmt.inner.lock() {
                s.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut s, &e, SQLSTATE_HY000);
            }
            return SQL_ERROR;
        }
    };

    if target_type == SQL_C_WCHAR {
        let usable = read & !1;
        let units: Vec<u16> = payload[..usable]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        unsafe {
            copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &units);
            write_if_some(strlen_or_ind_ptr, usable as SqlLen);
        }
    } else if is_unicode_plp {
        // NVARCHAR PLP wire bytes are UTF-16LE; convert to UTF-8 for SQL_C_CHAR.
        let usable = read & !1;
        let units: Vec<u16> = payload[..usable]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let utf8 = String::from_utf16_lossy(&units);
        let utf8_bytes = utf8.as_bytes();
        unsafe {
            copy_with_nul(
                target_value_ptr as *mut u8,
                buffer_length as usize,
                utf8_bytes,
            );
            write_if_some(strlen_or_ind_ptr, utf8_bytes.len() as SqlLen);
        }
    } else {
        unsafe {
            copy_with_nul(
                target_value_ptr as *mut u8,
                buffer_length as usize,
                &payload[..read],
            );
            write_if_some(strlen_or_ind_ptr, read as SqlLen);
        }
    }

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned while finalizing PLP stream read");
        return SQL_ERROR;
    };

    if reached_end {
        stmt_state.active_plp_column = None;
        stmt_state.active_plp_target_type = None;
        return SQL_SUCCESS;
    }

    stmt_state.active_plp_column = Some(col_index);
    stmt_state.active_plp_target_type = Some(target_type);
    unsafe { write_if_some(strlen_or_ind_ptr, SQL_NO_TOTAL) };
    post_diag(&mut stmt_state, ERR_STRING_RIGHT_TRUNCATION);

    SQL_SUCCESS_WITH_INFO
}
fn write_string_result<T: Copy + Default>(
    stmt_state: &mut crate::handles::stmt::StmtState,
    src: &[T],
    target_value_ptr: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let byte_len = std::mem::size_of_val(src) as SqlLen;
    unsafe { write_if_some(strlen_or_ind_ptr, byte_len) };
    let truncated = unsafe { copy_with_nul(target_value_ptr, buf_elements, src) };
    if truncated {
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
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
        ColumnValues::String(s) => Some(s.to_utf8_string()),
        ColumnValues::Uuid(u) => Some(u.to_string()),
        ColumnValues::Null => Some(String::new()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

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
