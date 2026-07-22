// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLPrepareW — prepare a SQL statement for later execution.

use tracing::{debug, error};

use super::sqlstate::*;
use super::util::read_utf16;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
    SqlWChar,
};
use crate::error::free_errors;
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implementation of `SQLPrepareW`.
///
/// Stores the SQL text on the statement for later execution by `SQLExecute`.
/// The server-side prepare is **deferred** to `SQLExecute`. No network I/O happens
/// here.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// - `statement_text` must point to a valid UTF-16 buffer readable for `text_length`
///   characters. If `text_length` is `SQL_NTS`, the string must be NUL-terminated.
pub(crate) unsafe fn sql_prepare_w(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?statement_text,
        text_length,
        "SQLPrepareW called",
    );

    crate::ffi_entry!("SQLPrepareW", unsafe {
        sql_prepare_w_impl(statement_handle, statement_text, text_length)
    })
}

unsafe fn sql_prepare_w_impl(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLPrepareW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLPrepareW: handle is not a STMT"
    );

    // The DM rejects null statement_text before calling the driver; see SQLPrepare spec.
    debug_assert!(
        !statement_text.is_null(),
        "SQLPrepareW: statement_text is null — DM should have rejected this"
    );
    debug_assert!(
        text_length == SQL_NTS || text_length >= 0,
        "SQLPrepareW: invalid text_length ({text_length}) — DM should have rejected this"
    );

    let sql = unsafe { read_utf16(statement_text, text_length) };
    sql_prepare_w_safe(stmt, sql)
}

fn sql_prepare_w_safe(stmt: &StmtHandle, sql: String) -> SqlReturn {
    debug!(sql = %sql, "SQLPrepareW: preparing");

    let dbc = stmt.parent_dbc();

    // Lock parent (DBC) before child (STMT) per the crate's lock-ordering rule,
    // and hold both for the whole body: the state check and the store happen
    // under one continuous STMT lock, so there is no TOCTOU window between them,
    // and the connection-liveness read stays valid through the store.
    let Ok(dbc_state) = dbc.inner.lock() else {
        error!("SQLPrepareW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLPrepareW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);

    if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
        error!("SQLPrepareW: statement has an active execute or open cursor");
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    // The prepare requires a live connection.
    if dbc_state.connection_state != ConnectionState::Connected {
        error!("SQLPrepareW: DBC is not connected");
        post_diag(&mut stmt_state, ERR_CONNECTION_DOES_NOT_EXIST);
        return SQL_ERROR;
    }

    // Store the SQL text and defer the server-side prepare to SQLExecute.
    // Re-preparing discards any prior prepared text and stale result metadata.
    // A prior prepared handle is orphaned for release at the next execute.
    stmt_state.prepared_sql = Some(sql);
    stmt_state.orphan_prepared_handle();
    stmt_state.column_metadata.clear();
    stmt_state.current_row = None;
    stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.set_state(STMT_STATE_PREPARED);

    debug!("SQLPrepareW: statement prepared (deferred)");
    SQL_SUCCESS
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
        let ret = unsafe { sql_prepare_w(SQL_NULL_HANDLE, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn prepare_stores_sql_and_sets_prepared_state() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_prepare_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        assert_eq!(ret, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.prepared_sql.as_deref(), Some("SELECT 1"));
        assert!(state.has_state(STMT_STATE_PREPARED));
    }

    #[test]
    fn reprepare_orphans_prior_handle_for_unprepare() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();

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
        assert_eq!(
            unsafe { sql_prepare_w(h.stmt, sql.as_ptr(), SQL_NTS) },
            SQL_SUCCESS
        );

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.prepared_sql.as_deref(), Some("SELECT 2"));
        assert!(state.prepared_handle.is_none());
        // The old handle is queued for release at the next execute.
        assert_eq!(state.pending_unprepare, Some(42));
    }

    #[test]
    fn null_statement_text_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();

        let ret = unsafe { sql_prepare_w(h.stmt, std::ptr::null(), SQL_NTS) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn disconnected_dbc_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();

        let sql: Vec<u16> = "SELECT 1"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ret = unsafe { sql_prepare_w(h.stmt, sql.as_ptr(), SQL_NTS) };
        // DBC is not connected — prepare needs a live connection.
        assert_eq!(ret, SQL_ERROR);
    }
}
