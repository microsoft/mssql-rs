// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecDirectW — execute a SQL statement directly.

use tracing::{debug, error};

use super::sqlstate::*;
use super::util::read_utf16;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn,
    SqlSmallInt, SqlWChar,
};
use crate::error::free_errors;
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::ResultSet;

/// Implementation of `SQLExecDirectW`.
///
/// Executes a SQL statement directly on the connection associated with `statement_handle`.
/// `SQLFreeStmt(SQL_CLOSE)` to drain the wire and release the connection.
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
    debug!(
        ?statement_handle,
        ?statement_text,
        text_length,
        "SQLExecDirectW called",
    );

    crate::ffi_entry!("SQLExecDirectW", unsafe {
        sql_exec_direct_w_impl(statement_handle, statement_text, text_length)
    })
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

    // The DM rejects null statement_text before calling the driver; see SQLExecDirect spec.
    debug_assert!(
        !statement_text.is_null(),
        "SQLExecDirectW: statement_text is null — DM should have rejected this"
    );

    let sql = unsafe { read_utf16(statement_text, text_length) };
    sql_exec_direct_w_safe(statement_handle, stmt, sql)
}

fn sql_exec_direct_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    sql: String,
) -> SqlReturn {
    debug!(sql = %sql, "SQLExecDirectW: executing");

    // Access parent DBC
    let dbc = stmt.parent_dbc();

    // Check STMT state first.
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLExecDirectW: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
            error!("SQLExecDirectW: statement has an active execute or open cursor");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        // A new execute invalidates prior metadata/context immediately, so a
        // later execute failure cannot expose stale SQLNumResultCols/DescribeCol state.
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.current_row = None;
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
    }

    // Take TdsClient out of DbcState. Lock is held only briefly — no I/O inside.
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLExecDirectW: dbc mutex poisoned");
            return SQL_ERROR;
        };

        if dbc_state.connection_state != ConnectionState::Connected {
            error!("SQLExecDirectW: DBC is not connected");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_DOES_NOT_EXIST);
            }
            clear_exec_started(stmt);
            return SQL_ERROR;
        }

        // HY000: another statement on this DBC already holds an open cursor.
        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            error!("SQLExecDirectW: connection is busy with results for another statement");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
            }
            clear_exec_started(stmt);
            return SQL_ERROR;
        }

        // Claim the connection before releasing the lock. Concurrent threads will
        // now see active_stmt and get HY000 instead of "no active TDS client".
        dbc_state.active_stmt = Some(statement_handle);
        let Some(client) = dbc_state.client.take() else {
            error!("SQLExecDirectW: no active TDS client");
            dbc_state.active_stmt = None;
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            clear_exec_started(stmt);
            return SQL_ERROR;
        };

        client
        // dbc_state dropped here — DBC lock released before any network I/O.
    };

    // Execute the SQL batch. Neither DBC nor STMT lock is held during I/O.
    if let Err(e) = dbc.runtime.block_on(client.execute(sql, None, None)) {
        error!(%e, "SQLExecDirectW: execution failed");
        let info_messages = client.take_info_messages();
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
            ds.active_stmt = None;
        }
        if let Ok(mut ss) = stmt.inner.lock() {
            post_tds_error(&mut ss, &e, SQLSTATE_HY000);
            post_tds_info_messages(&mut ss, &info_messages);
        }
        clear_exec_started(stmt);
        return SQL_ERROR;
    }

    // Capture metadata for SQLNumResultCols / SQLDescribeCol.
    let metadata = client.get_metadata().clone();
    // A non-empty metadata vec means COLMETADATA was received — there is a result set
    // (SELECT or similar). DDL / DML produces no COLMETADATA, so metadata is empty.
    let has_result_set = !metadata.is_empty();

    if !has_result_set {
        // DDL / DML: drain trailing DONE tokens and return the connection to idle.
        // No cursor is opened — the app can re-execute immediately without SQLCloseCursor.
        if let Err(e) = dbc.runtime.block_on(client.close_query()) {
            error!(%e, "SQLExecDirectW: failed to drain after DDL/DML");
            let info_messages = client.take_info_messages();
            if let Ok(mut ds) = dbc.inner.lock() {
                ds.client = Some(client);
                ds.active_stmt = None;
            }
            if let Ok(mut ss) = stmt.inner.lock() {
                post_tds_error(&mut ss, &e, SQLSTATE_HY000);
                post_tds_info_messages(&mut ss, &info_messages);
            }
            clear_exec_started(stmt);
            return SQL_ERROR;
        }
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLExecDirectW: stmt mutex poisoned");
            if let Ok(mut ds) = dbc.inner.lock() {
                ds.client = Some(client);
                ds.active_stmt = None;
            }
            clear_exec_started(stmt);
            return SQL_ERROR;
        };
        stmt_state.column_metadata = metadata; // empty vec
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_STARTED);
        let info_messages = client.take_info_messages();
        let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
        drop(stmt_state);
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
            ds.active_stmt = None;
        }
        debug!("SQLExecDirectW: DDL/DML complete");
        return if has_server_info {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
    }

    // Result-bearing query: leave the result set open for SQLFetch.
    // SQLCloseCursor / SQLFreeStmt(SQL_CLOSE) will drain the wire.
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLExecDirectW: stmt mutex poisoned");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
            ds.active_stmt = None;
        }
        clear_exec_started(stmt);
        return SQL_ERROR;
    };
    stmt_state.column_metadata = metadata;
    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_OPEN);
    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    let info_messages = client.take_info_messages();
    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
    drop(stmt_state);

    // Return client to DBC. active_stmt is already set from the claim above.
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLExecDirectW: dbc mutex poisoned storing client");
        clear_exec_started(stmt);
        return SQL_ERROR;
    };
    dbc_state.client = Some(client);
    drop(dbc_state);

    debug!("SQLExecDirectW: execution complete");
    if has_server_info {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

fn clear_exec_started(stmt: &StmtHandle) {
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_NTS, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

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
        let h = TestHandles::with_env_dbc_stmt();

        let ret = unsafe { sql_exec_direct_w(h.stmt, std::ptr::null(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn disconnected_dbc_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        // DBC is not connected
        assert_eq!(ret, SQL_ERROR);
    }
}
