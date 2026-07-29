// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `SQLGetData` — msodbcsql-style column-wise retrieval.
//!
//! `SQLFetch` positions the cursor on a row without decoding any column (see
//! [`OdbcRowWriter`](crate::api::row_writer::OdbcRowWriter)). Each `SQLGetData`
//! then decodes exactly the requested column, draining any intervening columns
//! off the wire. Columns must be requested in non-decreasing order; a column
//! already consumed reports `SQL_NO_DATA`. PLP (`*(MAX)` / `xml`) values are
//! streamed chunk-by-chunk across repeated `SQLGetData` calls for the same
//! column (see [`plp_stream`](crate::api::plp_stream)).

use tracing::{debug, error};

use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NO_TOTAL,
    SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::plp_stream::{PlpStream, PlpTarget, pump_wire};
use crate::api::row_writer::OdbcRowWriter;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::{PlpEncoding, ResultSet};
use mssql_tds::datatypes::column_values::ColumnValues;

/// Implements SQLGetData for column-wise retrieval on the current row.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
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

/// A resolved output request after validation.
struct OutReq {
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    /// Buffer capacity in element units (`u8` for CHAR, `SqlWChar` for WCHAR).
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
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

    // ---- Validation phase (stmt lock only) ----
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        if !stmt_state.row_active {
            post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
            return SQL_ERROR;
        }
        if column_number == 0 {
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
    }

    let buf_elements = if target_type == SQL_C_WCHAR {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };
    let req = OutReq {
        target_type,
        target_value_ptr,
        buf_elements,
        strlen_or_ind_ptr,
    };

    // ---- PLP continuation / backward-column rejection ----
    // Checked before taking the client so a repeat call on a scalar column can
    // short-circuit without I/O.
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        if let Some(stream) = stmt_state.plp_stream.as_ref() {
            if stream.column == column_number {
                drop(stmt_state);
                return continue_plp_stream(statement_handle, stmt, column_number, &req);
            }
            // A different column than the in-progress PLP stream: that streamed
            // column is abandoned. Fall through to new-column handling, which
            // drains it while advancing.
        }
        let col0 = usize::from(column_number) - 1;
        if col0 < stmt_state.getdata_next_col {
            // This driver does not advertise SQL_GD_ANY_ORDER, so columns must be
            // retrieved in non-decreasing order. `getdata_next_col` holds the
            // 1-based ordinal of the most recently consumed column.
            if usize::from(column_number) == stmt_state.getdata_next_col {
                // Re-requesting the column just retrieved: the value is already
                // exhausted, so report end-of-data per the SQLGetData contract.
                debug!(column_number, "SQLGetData: column already exhausted");
                return SQL_NO_DATA;
            }
            // A strictly earlier column: backward retrieval is not supported.
            debug!(column_number, "SQLGetData: backward column access rejected");
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }
    }

    // ---- New column: decode column `column_number` ----
    read_new_column(statement_handle, stmt, column_number, &req)
}

/// Delivers the next chunk of an in-progress PLP stream for `column_number`.
fn continue_plp_stream(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    req: &OutReq,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();
    let Some(mut client) = take_client(statement_handle, stmt) else {
        return SQL_ERROR;
    };

    // Pump wire bytes until something is deliverable or the value ends.
    let pump_result: Result<Result<(), mssql_tds::error::Error>, bool> =
        dbc.runtime.block_on(async {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                return Err(true); // poisoned
            };
            let Some(mut stream) = stmt_state.plp_stream.take() else {
                return Err(false); // vanished; treat as no-data
            };
            drop(stmt_state);
            let mut io_err = None;
            while stream.needs_pump() {
                if let Err(e) = pump_wire(&mut stream, &mut client).await {
                    io_err = Some(e);
                    break;
                }
            }
            if let Ok(mut ss) = stmt.inner.lock() {
                ss.plp_stream = Some(stream);
            }
            Ok(match io_err {
                Some(e) => Err(e),
                None => Ok(()),
            })
        });

    match pump_result {
        Err(true) => {
            error!("SQLGetData: stmt mutex poisoned during PLP pump");
            return_client(&dbc, client);
            SQL_ERROR
        }
        Err(false) => {
            return_client(&dbc, client);
            SQL_NO_DATA
        }
        Ok(Err(e)) => {
            error!(%e, "SQLGetData: PLP wire read failed");
            if let Ok(mut ss) = stmt.inner.lock() {
                ss.plp_stream = None;
                post_tds_error(&mut ss, &e, SQLSTATE_HY000);
            }
            return_client(&dbc, client);
            SQL_ERROR
        }
        Ok(Ok(())) => {
            let ret = deliver_plp(stmt, column_number, req);
            return_client(&dbc, client);
            ret
        }
    }
}

/// Positions on `column_number`, draining intervening columns, and delivers the
/// value (scalar in one shot, or the first chunk of a PLP stream).
fn read_new_column(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    req: &OutReq,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();
    let col0 = usize::from(column_number) - 1;

    let Some(mut client) = take_client(statement_handle, stmt) else {
        return SQL_ERROR;
    };

    let mut writer = OdbcRowWriter::new();
    writer.request(col0);

    let decode = dbc
        .runtime
        .block_on(async { client.next_row_into(&mut writer).await });

    match decode {
        Err(e) => {
            error!(%e, "SQLGetData: column decode failed");
            if let Ok(mut ss) = stmt.inner.lock() {
                ss.reset_row_stream();
                post_tds_error(&mut ss, &e, SQLSTATE_HY000);
            }
            return_client(&dbc, client);
            SQL_ERROR
        }
        Ok(false) => {
            // Row ended before reaching the requested column — invalid index.
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_INVALID_DESCRIPTOR_INDEX);
            }
            return_client(&dbc, client);
            SQL_ERROR
        }
        Ok(true) => {
            if writer.end_row_fired() {
                // The requested column index is past the last column of the row.
                if let Ok(mut ss) = stmt.inner.lock() {
                    post_diag(&mut ss, ERR_INVALID_DESCRIPTOR_INDEX);
                }
                return_client(&dbc, client);
                return SQL_ERROR;
            }
            if let Some(value) = writer.take_captured() {
                // Non-PLP scalar (or NULL): fully materialized, one-shot delivery.
                let ret = deliver_scalar(stmt, col0, &value, req);
                return_client(&dbc, client);
                ret
            } else {
                // The requested column is a non-null PLP value: begin streaming.
                let ret = begin_plp_stream(&dbc, stmt, column_number, &mut client, req);
                return_client(&dbc, client);
                ret
            }
        }
    }
}

/// Begins a PLP stream for the just-paused column and delivers its first chunk.
fn begin_plp_stream(
    dbc: &crate::handles::dbc::DbcHandle,
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    client: &mut mssql_tds::connection::tds_client::TdsClient,
    req: &OutReq,
) -> SqlReturn {
    let Some(encoding) = client.active_plp_encoding() else {
        if let Ok(mut ss) = stmt.inner.lock() {
            post_sql_error(&mut ss, SQLSTATE_HYC00, 0, "Unsupported PLP column encoding");
        }
        return SQL_ERROR;
    };

    if matches!(encoding, PlpEncoding::Binary) {
        // Binary(max) → SQL_C_CHAR/WCHAR conversion is not implemented yet. Leave
        // the PLP paused; the next SQLFetch drains it while advancing.
        if let Ok(mut ss) = stmt.inner.lock() {
            post_sql_error(
                &mut ss,
                SQLSTATE_HYC00,
                0,
                "Binary(max) to character conversion not yet implemented",
            );
        }
        return SQL_ERROR;
    }

    let target = if req.target_type == SQL_C_WCHAR {
        PlpTarget::WChar
    } else {
        PlpTarget::Char
    };
    let mut stream = PlpStream::new(column_number, encoding, target);

    let pump = dbc.runtime.block_on(async {
        while stream.needs_pump() {
            pump_wire(&mut stream, &mut *client).await?;
        }
        Ok::<(), mssql_tds::error::Error>(())
    });

    if let Err(e) = pump {
        error!(%e, "SQLGetData: initial PLP wire read failed");
        if let Ok(mut ss) = stmt.inner.lock() {
            post_tds_error(&mut ss, &e, SQLSTATE_HY000);
        }
        return SQL_ERROR;
    }

    if let Ok(mut ss) = stmt.inner.lock() {
        ss.plp_stream = Some(stream);
    }
    deliver_plp(stmt, column_number, req)
}

/// Copies the next PLP chunk into the caller buffer and updates stream state.
/// On exhaustion, clears the stream and advances the get-data cursor.
fn deliver_plp(stmt: &StmtHandle, column_number: SqlUSmallInt, req: &OutReq) -> SqlReturn {
    let Ok(mut ss) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned delivering PLP");
        return SQL_ERROR;
    };
    let Some(mut stream) = ss.plp_stream.take() else {
        return SQL_NO_DATA;
    };

    // Length indicator: exact remaining byte count once the wire is drained,
    // otherwise SQL_NO_TOTAL (length not yet known for a *(MAX) value).
    let indicator = if stream.wire_done() {
        stream.pending_bytes() as SqlLen
    } else {
        SQL_NO_TOTAL
    };
    unsafe { write_if_some(req.strlen_or_ind_ptr, indicator) };

    let delivery = match req.target_type {
        SQL_C_WCHAR => stream.deliver_wchar(req.target_value_ptr as *mut SqlWChar, req.buf_elements),
        _ => stream.deliver_char(req.target_value_ptr as *mut u8, req.buf_elements),
    };

    if stream.is_exhausted() {
        // Whole value delivered: advance the get-data cursor past this column.
        ss.getdata_next_col = usize::from(column_number);
        if delivery.truncated {
            post_diag(&mut ss, ERR_STRING_RIGHT_TRUNCATION);
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        }
    } else {
        // More chunks remain: keep the stream and report truncation.
        ss.plp_stream = Some(stream);
        post_diag(&mut ss, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    }
}

/// Delivers a fully materialized scalar/NULL value in a single call and advances
/// the get-data cursor past `col0`.
fn deliver_scalar(stmt: &StmtHandle, col0: usize, value: &ColumnValues, req: &OutReq) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned delivering scalar");
        return SQL_ERROR;
    };
    stmt_state.getdata_next_col = col0 + 1;

    if matches!(value, ColumnValues::Null) {
        unsafe { write_if_some(req.strlen_or_ind_ptr, SQL_NULL_DATA) };
        if req.target_type == SQL_C_WCHAR {
            unsafe {
                copy_with_nul(req.target_value_ptr as *mut SqlWChar, req.buf_elements, &[]);
            }
        } else {
            unsafe {
                copy_with_nul(req.target_value_ptr as *mut u8, req.buf_elements, &[]);
            }
        }
        return SQL_SUCCESS;
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

    if req.target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = as_text.encode_utf16().collect();
        write_string_result(
            &mut stmt_state,
            &utf16,
            req.target_value_ptr as *mut SqlWChar,
            req.buf_elements,
            req.strlen_or_ind_ptr,
        )
    } else {
        write_string_result(
            &mut stmt_state,
            as_text.as_bytes(),
            req.target_value_ptr as *mut u8,
            req.buf_elements,
            req.strlen_or_ind_ptr,
        )
    }
}

/// Borrows the connection's TDS client for one get-data I/O step. The connection
/// must already be busy with this statement (set by `SQLFetch`). Posts a
/// diagnostic and returns `None` on any inconsistency.
fn take_client(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
) -> Option<mssql_tds::connection::tds_client::TdsClient> {
    let dbc = stmt.parent_dbc();
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLGetData: dbc mutex poisoned");
        return None;
    };
    if dbc_state.active_stmt != Some(statement_handle) {
        drop(dbc_state);
        if let Ok(mut ss) = stmt.inner.lock() {
            post_diag(&mut ss, ERR_INVALID_CURSOR_STATE);
        }
        return None;
    }
    match dbc_state.client.take() {
        Some(client) => Some(client),
        None => {
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            None
        }
    }
}

/// Returns a borrowed client to the connection, leaving the cursor open and the
/// connection busy with the same statement.
fn return_client(
    dbc: &crate::handles::dbc::DbcHandle,
    client: mssql_tds::connection::tds_client::TdsClient,
) {
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
    }
}

/// Writes `src` to the caller's output buffer with ODBC string semantics:
/// the indicator (when present) reports the untruncated byte length, the
/// payload is NUL-terminated within the buffer, and truncation is reported via
/// SQLSTATE 01004 + `SQL_SUCCESS_WITH_INFO`.
///
/// `buf_elements` is the buffer capacity in units of `T` (not bytes).
///
/// The caller-provided pointers are written through small `unsafe` blocks
/// inside this function; both pointer arguments are obligations of the FFI
/// caller (validated against the buffer length passed by the DM).
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
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NO_DATA, SQL_NULL_HANDLE};
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
    fn get_data_without_cursor_returns_error() {
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
    fn get_data_cursor_open_but_no_active_row_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            // row_active stays false: SQLGetData before a successful SQLFetch.
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
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_column_zero_is_invalid() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.row_active = true;
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                0,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_unsupported_target_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.row_active = true;
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
    fn get_data_backward_column_is_rejected() {
        // Columns 1..=3 were consumed (cursor at 3). Requesting an earlier column
        // (2) is backward retrieval, which this driver rejects with 07009.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.row_active = true;
            s.getdata_next_col = 3; // columns 1..=3 already consumed
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

    #[test]
    fn get_data_reread_just_consumed_column_returns_no_data() {
        // Re-requesting the column that was most recently retrieved (cursor == its
        // ordinal) reports end-of-data, matching the SQLGetData streaming contract.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.row_active = true;
            s.getdata_next_col = 3; // column 3 was the last consumed
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                3,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }
}
