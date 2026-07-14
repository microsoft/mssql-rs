// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecDirectW — execute a SQL statement directly.

use tracing::{debug, error};

use super::exec_common::{
    build_named_params, claim_connection, fail_with_tds, finish_execute, flush_pending_unprepare,
};
use super::sqlstate::*;
use super::util::{read_utf16, rewrite_param_markers};
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn, SqlSmallInt, SqlWChar,
};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

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

    let dbc = stmt.parent_dbc();

    // Check STMT state, gather parameter values, and reset prior context.
    let (named_params, rewritten_sql, marker_count) = {
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
        // Rewrite markers and read the bound parameter buffers before mutating
        // any state, so a binding error (07002 / HYC00) leaves the statement
        // unchanged.
        let (rewritten_sql, marker_count) = rewrite_param_markers(&sql);
        let named_params =
            match unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecDirectW") } {
                Ok(params) => params,
                Err(rc) => return rc,
            };
        // A new execute invalidates prior metadata/context immediately, so a
        // later execute failure cannot expose stale SQLNumResultCols/DescribeCol state.
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.current_row = None;
        stmt_state.prepared_sql = None;
        // Superseding a prepared plan orphans its server handle; release it
        // (deferred) once we hold the client below.
        stmt_state.orphan_prepared_handle();
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
        (named_params, rewritten_sql, marker_count)
    };

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecDirectW") {
        Ok(client) => client,
        Err(rc) => return rc,
    };

    // Release any handle orphaned by the reset above before running the batch.
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLExecDirectW");

    // Parameterized text runs via sp_executesql (direct execution, no cached
    // handle); unparameterized text runs as a plain SQL batch. Neither DBC nor
    // STMT lock is held during I/O.
    let exec_result: Result<(), mssql_tds::error::Error> = if marker_count > 0 {
        dbc.runtime
            .block_on(client.execute_sp_executesql(rewritten_sql, named_params, None, None))
    } else {
        dbc.runtime.block_on(client.execute(sql, None, None))
    };
    if let Err(e) = exec_result {
        error!(%e, "SQLExecDirectW: execution failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    finish_execute(dbc, stmt, statement_handle, client, "SQLExecDirectW")
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

    #[test]
    fn exec_direct_clears_stale_prepared_plan() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared_sql = Some("SELECT 1".to_string());
            state.prepared_handle = Some(42);
            state.set_state(STMT_STATE_PREPARED);
        }

        let sql: Vec<u16> = "SELECT 2"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // Fails (not connected), but the prepared plan must already be reset.
        assert_eq!(
            unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_ERROR
        );

        let state = stmt.inner.lock().unwrap();
        assert!(state.prepared_sql.is_none());
        assert!(state.prepared_handle.is_none());
        assert!(!state.has_state(STMT_STATE_PREPARED));
        // The superseded handle is queued for sp_unprepare. The flush never ran
        // here because the connection claim failed, so it remains pending.
        assert_eq!(state.pending_unprepare, Some(42));
    }

    #[test]
    fn unbound_parameter_marker_returns_07002() {
        let h = TestHandles::with_env_dbc_stmt();
        // SQL has one marker but no parameter is bound; the failure must be
        // posted before any state mutation and before the connection claim.
        let sql: Vec<u16> = "SELECT ? AS v"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07002);
        // A binding error must leave the statement unchanged — no EXEC_STARTED.
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }
}
