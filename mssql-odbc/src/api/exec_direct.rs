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
#[allow(clippy::too_many_arguments)]
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

    // TODO(Phase2): In Phase 1, SQLExecDirect drains the entire result set and calls
    // close_query() before returning, so the TDS connection is always clean — silently
    // overwriting pending_rows on a re-execute is safe. In Phase 2 (streaming), rows
    // stay live on the wire and re-executing without closing first would corrupt TDS.
    // msodbcsql checks lpstmt->wStatus & (STMT_ST_EXECSTARTED|STMT_ST_CURS_OPEN).

    let sql = unsafe { read_utf16(statement_text, text_length) };
    debug!(sql = %sql, "SQLExecDirectW: executing");

    // Access parent DBC
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

    // TODO(SQLDriverConnect): Holding the full DBC lock for the duration of execute +
    // row drain is a Phase 1 simplification. It prevents concurrent STMT operations on
    // the same DBC and makes SQLCancel impossible. Fixing this requires changes to how
    // TdsClient is owned inside DbcState.
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLExecDirectW: dbc mutex poisoned");
        return SQL_ERROR;
    };

    if dbc_state.connection_state != ConnectionState::Connected {
        error!("SQLExecDirectW: DBC is not connected");
        return SQL_ERROR;
    }

    let Some(client) = dbc_state.client.as_mut() else {
        error!("SQLExecDirectW: no active TDS client");
        return SQL_ERROR;
    };

    // Execute the SQL batch.
    let exec_result = dbc.runtime.block_on(client.execute(sql, None, None));
    if let Err(e) = exec_result {
        error!(%e, "SQLExecDirectW: execution failed");
        // TODO: post diagnostic record with SQLSTATE 42000 or HY000
        return SQL_ERROR;
    }

    // Buffer metadata and all rows from the result set.
    let metadata = client.get_metadata().clone();

    let mut rows = Vec::new();
    loop {
        let row_result = dbc.runtime.block_on(client.next_row());
        match row_result {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => break,
            Err(e) => {
                error!(%e, "SQLExecDirectW: error reading row");
                let _ = dbc.runtime.block_on(client.close_query());
                return SQL_ERROR;
            }
        }
    }

    // Close the server-side cursor so the connection is ready for the next query.
    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        error!(%e, "SQLExecDirectW: failed to close query");
        return SQL_ERROR;
    }

    // Store results in the statement handle.
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLExecDirectW: stmt mutex poisoned");
        return SQL_ERROR;
    };

    stmt_state.column_metadata = metadata;
    // Fields consumed by SQLFetch — see TODO(SQLFetch) in stmt.rs for the contract.
    stmt_state.pending_rows = rows;
    stmt_state.row_cursor = 0;
    stmt_state.diag_records.clear();

    debug!(
        rows = stmt_state.pending_rows.len(),
        "SQLExecDirectW: execution complete"
    );

    if stmt_state.pending_rows.is_empty() {
        SQL_NO_DATA
    } else {
        SQL_SUCCESS
    }
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
