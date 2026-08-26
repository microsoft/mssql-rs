// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLCloseCursor and the SQL_CLOSE path of SQLFreeStmt.
//!
//! Both operations close the cursor on a statement handle and release the
//! connection-level "busy" claim, allowing other statements on the same DBC
//! to execute.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::{STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Closes the cursor on `statement_handle` and discards any pending rows.
///
/// Mirrors msodbcsql's `SQLCloseCursor` → `SQLFreeStmt(SQL_CLOSE)` path.
/// Returns `SQL_ERROR` (SQLSTATE 24000) if no cursor is open, matching
/// msodbcsql's behaviour.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_close_cursor(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLCloseCursor called");
    crate::ffi_entry!("SQLCloseCursor", unsafe {
        sql_close_cursor_impl(statement_handle)
    })
}

/// Implements the `SQL_CLOSE` option of `SQLFreeStmt` — closes the cursor
/// without dropping the statement handle or its bound parameters.
///
/// Unlike `SQLCloseCursor`, `SQLFreeStmt(SQL_CLOSE)` succeeds even when no
/// cursor is open (it is a no-op in that case).
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_close(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFreeStmt(SQL_CLOSE) called");
    crate::ffi_entry!("SQLFreeStmt(SQL_CLOSE)", unsafe {
        sql_free_stmt_close_impl(statement_handle)
    })
}

unsafe fn sql_close_cursor_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLCloseCursor: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_close_cursor_safe(statement_handle, stmt)
}

fn sql_close_cursor_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLCloseCursor: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);
    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        error!("SQLCloseCursor: no cursor is open — SQLSTATE 24000");
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    reset_cursor_state(&mut stmt_state);
    drop(stmt_state);

    match drain_and_release(stmt, statement_handle) {
        DrainOutcome::Failed => {
            error!("SQLCloseCursor: failed to drain TDS stream on close");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted => {
            debug!("SQLCloseCursor: cursor closed");
            SQL_SUCCESS_WITH_INFO
        }
        DrainOutcome::Clean => {
            debug!("SQLCloseCursor: cursor closed");
            SQL_SUCCESS
        }
    }
}

unsafe fn sql_free_stmt_close_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFreeStmt(SQL_CLOSE): statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_free_stmt_close_safe(statement_handle, stmt)
}

fn sql_free_stmt_close_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFreeStmt(SQL_CLOSE): stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);
    // No-op if cursor is already closed.
    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        return SQL_SUCCESS;
    }

    reset_cursor_state(&mut stmt_state);
    drop(stmt_state);

    match drain_and_release(stmt, statement_handle) {
        DrainOutcome::Failed => {
            error!("SQLFreeStmt(SQL_CLOSE): failed to drain TDS stream on close");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted => {
            debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
            SQL_SUCCESS_WITH_INFO
        }
        DrainOutcome::Clean => {
            debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
            SQL_SUCCESS
        }
    }
}

/// Closes the cursor on a statement as part of a *connection*-scoped operation
/// (`SQLEndTran`, an autocommit switch, an isolation change, disconnect).
///
/// Reproduces what msodbcsql's sweep does to each statement. `CommitAbortTran`
/// calls `SQLFreeStmt(lpstmt, SQL_CLOSE)` on every statement it visits
/// (`sqlctran.cpp:302-323`), and that entry point calls `FreeErrors(lpstmt)`
/// before it looks at either the option or the cursor state
/// (`sqlccmd.cpp:379-380`). Statement diagnostics are therefore discarded even
/// on a statement that never opened a cursor — the failed-statement case — so
/// [`free_errors`] runs here unconditionally, ahead of the cursor check.
///
/// Kept separate from the public `SQLFreeStmt(SQL_CLOSE)` path only for cost:
/// statements with no open cursor return straight after the diagnostics reset,
/// with no FFI entry and no drain.
pub(super) fn close_cursor_for_connection_op(stmt: &StmtHandle, handle: SqlHandle) -> SqlReturn {
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("close_cursor_for_connection_op: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            return SQL_SUCCESS;
        }
        reset_cursor_state(&mut stmt_state);
    }

    match drain_and_release(stmt, handle) {
        DrainOutcome::Failed => {
            error!("close_cursor_for_connection_op: failed to drain TDS stream");
            SQL_ERROR
        }
        DrainOutcome::InfoPosted | DrainOutcome::Clean => SQL_SUCCESS,
    }
}

/// Resets cursor state on the statement (cursor is no longer open, metadata cleared).
pub(super) fn reset_cursor_state(stmt_state: &mut crate::handles::stmt::StmtState) {
    stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_CONTEXT);
    stmt_state.reset_row_stream();
    stmt_state.column_metadata.clear();
    stmt_state.pending_row_counts.clear();
    stmt_state.result_set_exhausted = false;
}

/// Outcome of draining the TDS stream and releasing the connection on cursor close.
pub(super) enum DrainOutcome {
    /// Drain completed cleanly; no server INFO messages were posted.
    Clean,
    /// Drain completed and one or more server INFO messages were posted.
    InfoPosted,
    /// Draining the TDS stream failed (I/O error or a lost/poisoned client).
    /// A diagnostic is posted where possible; the connection may be broken.
    Failed,
}

/// Takes the TDS client from the DBC, drains any pending tokens, and clears `active_stmt`.
/// `active_stmt` is kept set until the drain finishes so concurrent threads see the
/// connection as busy (HY000) throughout — not just until `client` is taken.
/// No locks are held during the network I/O.
///
/// Returns a [`DrainOutcome`] so callers can distinguish a clean close from one
/// that surfaced server INFO messages, and — importantly — from a drain failure,
/// which must not be reported to the app as success.
pub(super) fn drain_and_release(stmt: &StmtHandle, statement_handle: SqlHandle) -> DrainOutcome {
    let dbc = stmt.parent_dbc();

    // Take the client; intentionally leave active_stmt set while draining.
    let client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("drain_and_release: dbc mutex poisoned");
            return DrainOutcome::Failed;
        };
        // A fetch may have already released `active_stmt` early once this
        // statement's own result set was exhausted (AB#47508), independently
        // of `STMT_STATE_CURSOR_OPEN`, which callers here check instead. If a
        // *different* statement has since claimed the connection, this
        // statement has nothing left on the wire to drain — taking the client
        // here would steal and corrupt whatever the other statement is mid-fetch
        // on. `active_stmt` being `None` (idle) or still `Some(statement_handle)`
        // is the ordinary case and proceeds as before.
        match dbc_state.active_stmt {
            Some(other) if other != statement_handle => return DrainOutcome::Clean,
            _ => dbc_state.client.take(),
        }
    };

    let Some(mut client) = client else {
        error!("drain_and_release: no TDS client to drain — this is a bug");
        if let Ok(mut ds) = dbc.inner.lock()
            && ds.active_stmt == Some(statement_handle)
        {
            ds.active_stmt = None;
        }
        return DrainOutcome::Failed;
    };

    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        error!(%e, "drain_and_release: failed to drain TDS stream — connection may be broken");
        // Surface the failure as a diagnostic so the app is not told the close
        // succeeded when the stream did not drain cleanly.
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        }
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
            if ds.active_stmt == Some(statement_handle) {
                ds.active_stmt = None;
            }
        }
        return DrainOutcome::Failed;
    }

    let has_server_info = match stmt.inner.lock() {
        Ok(mut stmt_state) => {
            // Drain INFO only after the lock is held so a poisoned mutex cannot
            // silently drop the messages.
            let info_messages = client.take_info_messages();
            post_tds_info_messages(&mut stmt_state, &info_messages)
        }
        Err(_) => {
            error!("drain_and_release: stmt mutex poisoned while posting info messages");
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                if dbc_state.active_stmt == Some(statement_handle) {
                    dbc_state.active_stmt = None;
                }
            }
            return DrainOutcome::Failed;
        }
    };

    // Drain complete: return client and release busy claim atomically.
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        if dbc_state.active_stmt == Some(statement_handle) {
            dbc_state.active_stmt = None;
        }
    }

    if has_server_info {
        DrainOutcome::InfoPosted
    } else {
        DrainOutcome::Clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    #[test]
    fn close_cursor_null_handle() {
        let ret = unsafe { sql_close_cursor(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn close_cursor_no_cursor_open_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        // No execute has been called — cursor_open is false.
        let ret = unsafe { sql_close_cursor(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn free_stmt_close_null_handle() {
        let ret = unsafe { sql_free_stmt_close(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn free_stmt_close_no_cursor_is_noop() {
        let h = TestHandles::with_env_dbc_stmt();
        // No execute has been called — cursor_open is false; SQL_CLOSE is a no-op.
        let ret = unsafe { sql_free_stmt_close(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    /// Statement A's cursor stays `STMT_STATE_CURSOR_OPEN` after its own fetch
    /// released `active_stmt` early (AB#47508) — a different statement B may
    /// have since claimed the connection and be mid-fetch on its own live,
    /// still-open result set. Closing A's cursor in that window must not touch
    /// B's client at all: taking it here would drain/corrupt B's pending rows.
    #[test]
    fn drain_and_release_does_not_touch_a_different_statements_client() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{col_metadata_empty, tds_client_from_tokens};

        let mut h = TestHandles::with_env_dbc_stmt();
        let stmt_b = h.alloc_extra_stmt();
        h.mark_dbc_connected();

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // B's live client, positioned on an open result set (has_open_batch) so
        // `close_query()` would genuinely try to drain it if this stole the
        // client — with an empty token queue left, that drain would error.
        let mut client_b = tds_client_from_tokens(vec![col_metadata_empty()]);
        dbc.runtime
            .block_on(client_b.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client_b);
            ds.active_stmt = Some(stmt_b);
        }
        {
            let stmt_a = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt_a
                .inner
                .lock()
                .unwrap()
                .set_state(STMT_STATE_CURSOR_OPEN);
        }

        let outcome = drain_and_release(unsafe { handle_from_raw::<StmtHandle>(h.stmt) }, h.stmt);

        assert!(matches!(outcome, DrainOutcome::Clean));
        let ds = dbc.inner.lock().unwrap();
        assert_eq!(
            ds.active_stmt,
            Some(stmt_b),
            "B's claim on the connection must be left untouched"
        );
        assert!(
            ds.client.as_ref().is_some_and(|c| c.has_open_batch()),
            "B's result set must still be open — not drained by A's close"
        );
    }
}
