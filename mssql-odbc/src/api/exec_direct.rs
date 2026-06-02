// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecDirectW — execute a SQL statement directly.

use std::panic;

use tracing::{debug, error, trace};

use super::util::read_utf16;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
    SqlWChar,
};
use super::sqlstate::*;
use crate::error::DiagRecord;
use crate::handles::dbc::{ConnectionState, DbcHandle};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::ResultSet;

/// Implementation of `SQLExecDirectW`.
///
/// Executes a SQL statement directly on the connection associated with `statement_handle`.
/// Buffers the complete result set in the statement handle for subsequent `SQLFetch` calls.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// - `statement_text` must point to a valid UTF-16 buffer readable for `text_length` characters.
///   If `text_length` is `SQL_NTS`, the string must be NUL-terminated.
pub(crate) unsafe fn sql_exec_direct_w(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    debug!("SQLExecDirectW called");

    let result = panic::catch_unwind(|| unsafe {
        sql_exec_direct_w_impl(statement_handle, statement_text, text_length)
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLExecDirectW: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLExecDirectW returning");
    ret
}

unsafe fn sql_exec_direct_w_impl(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLExecDirectW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLExecDirectW: handle is not a STMT"
    );

    if statement_text.is_null() {
        error!("SQLExecDirectW: statement_text is null");
        return SQL_ERROR;
    }

    let sql = unsafe { read_utf16(statement_text, text_length) };
    debug!(sql = %sql, "SQLExecDirectW: executing");

    // Access parent DBC
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

    // Precondition checks + take TdsClient out of DbcState.
    // Lock is held only briefly — no I/O inside this block.
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLExecDirectW: dbc mutex poisoned");
            return SQL_ERROR;
        };

        if dbc_state.connection_state != ConnectionState::Connected {
            error!("SQLExecDirectW: DBC is not connected");
            return SQL_ERROR;
        }

        // SQLSTATE 24000: re-executing on a statement that already has an open cursor.
        {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLExecDirectW: stmt mutex poisoned");
                return SQL_ERROR;
            };
            if stmt_state.cursor_open {
                error!("SQLExecDirectW: cursor is already open on this statement");
                stmt_state.diag_records.clear();
                stmt_state.diag_records.push(DiagRecord::new(
                    SQLSTATE_24000,
                    0,
                    "Invalid cursor state",
                ));
                return SQL_ERROR;
            }
        }

        // SQLSTATE HY000: another statement on this DBC already holds an open cursor.
        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            error!("SQLExecDirectW: connection is busy with results for another statement");
            if let Ok(mut ss) = stmt.inner.lock() {
                ss.diag_records.clear();
                ss.diag_records.push(DiagRecord::new(
                    SQLSTATE_HY000,
                    0,
                    "Connection is busy with results for another hstmt",
                ));
            }
            return SQL_ERROR;
        }

        let Some(client) = dbc_state.client.take() else {
            error!("SQLExecDirectW: no active TDS client");
            return SQL_ERROR;
        };

        client
        // dbc_state dropped here — DBC lock released before any network I/O.
    };

    // Execute the SQL batch. DBC lock is NOT held during network I/O, so SQLCancel
    // and other DBC-level operations on concurrent threads can proceed.
    if let Err(e) = dbc.runtime.block_on(client.execute(sql, None, None)) {
        error!(%e, "SQLExecDirectW: execution failed");
        // TODO: post diagnostic record with SQLSTATE 42000 or HY000
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
        }
        return SQL_ERROR;
    }

    // Buffer metadata for SQLNumResultCols / SQLDescribeCol.
    // Row buffering is deferred to SQLFetch (next PR); close_query drains remaining tokens.
    let metadata = client.get_metadata().clone();
    let has_rows = !metadata.is_empty();

    // Drain remaining tokens and close the server-side cursor.
    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        error!(%e, "SQLExecDirectW: failed to close query");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
        }
        return SQL_ERROR;
    }

    // Store results in the statement handle.
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLExecDirectW: stmt mutex poisoned");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
        }
        return SQL_ERROR;
    };

    stmt_state.column_metadata = metadata;
    stmt_state.cursor_open = true;
    stmt_state.diag_records.clear();
    drop(stmt_state);

    // Return client to DbcState and record this stmt as the active cursor owner.
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLExecDirectW: dbc mutex poisoned storing active_stmt");
        return SQL_ERROR;
    };
    dbc_state.client = Some(client);
    dbc_state.active_stmt = Some(statement_handle);
    drop(dbc_state);

    debug!("SQLExecDirectW: execution complete");

    if has_rows { SQL_SUCCESS } else { SQL_NO_DATA }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NTS,
        SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    unsafe fn alloc_env_dbc_stmt() -> (SqlHandle, SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe {
            sql_set_env_attr(
                env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        (env, dbc, stmt)
    }

    unsafe fn free_env_dbc_stmt(env: SqlHandle, dbc: SqlHandle, stmt: SqlHandle) {
        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, stmt);
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(SQL_NULL_HANDLE, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn null_statement_text_returns_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let ret = unsafe { sql_exec_direct_w(stmt, std::ptr::null(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);

        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }

    #[test]
    fn disconnected_dbc_returns_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(stmt, sql.as_ptr(), SQL_NTS) };
        // DBC is not connected
        assert_eq!(ret, SQL_ERROR);

        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }
}
