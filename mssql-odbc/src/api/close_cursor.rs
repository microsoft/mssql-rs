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
use crate::handles::stmt::{
    STMT_STATE_CURSOR_EXHAUSTED, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT,
};
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
    {
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
    }

    match close_open_cursor(stmt, statement_handle) {
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
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFreeStmt(SQL_CLOSE): stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        // No-op if cursor is already closed.
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            return SQL_SUCCESS;
        }
    }

    match close_open_cursor(stmt, statement_handle) {
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
    stmt_state.clear_state(
        STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_EXHAUSTED,
    );
    stmt_state.reset_row_stream();
    stmt_state.column_metadata.clear();
    stmt_state.pending_row_counts.clear();
}

/// Shared tail of every cursor close: reset the statement's cursor state, then
/// drain the TDS stream and release the connection. Used by `SQLCloseCursor`,
/// `SQLFreeStmt(SQL_CLOSE)`, and the implicit close on re-execute; each caller
/// keeps its own "no cursor open" handling and `DrainOutcome` → `SqlReturn`
/// mapping.
pub(super) fn close_open_cursor(stmt: &StmtHandle, statement_handle: SqlHandle) -> DrainOutcome {
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        reset_cursor_state(&mut stmt_state);
    }
    drain_and_release(stmt, statement_handle)
}

/// If the statement's cursor is open but its whole batch has been consumed
/// (`SQLFetch` returned `SQL_NO_DATA` on the final result set), implicitly close
/// it via the shared close path so a re-execute (`SQLExecute` / `SQLExecDirect`)
/// starts from a clean cursor state. A cursor with un-read rows or still-pending
/// result sets is left untouched, so the caller's 24000 guard still rejects that
/// re-execute — msodbcsql only allows re-execute once the batch is fully read.
pub(super) fn implicit_cursor_close_if_exhausted(stmt: &StmtHandle, statement_handle: SqlHandle) {
    {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("implicit cursor close: stmt mutex poisoned checking cursor state");
            return;
        };
        if !stmt_state.cursor_is_exhausted() {
            return;
        }
    }
    close_open_cursor(stmt, statement_handle);
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
        dbc_state.client.take()
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

    // A cursor that fetched to end-of-batch (CURSOR_OPEN + CURSOR_EXHAUSTED) is
    // implicitly closed: state cleared, wire drained, connection released — the
    // precondition that lets SQLExecute / SQLExecDirect re-run without 24000.
    #[test]
    fn implicit_close_closes_exhausted_cursor() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_CURSOR_EXHAUSTED);
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(tds_client_from_tokens(vec![done_no_more()]));
            ds.active_stmt = Some(h.stmt);
        }

        implicit_cursor_close_if_exhausted(stmt, h.stmt);

        let ss = stmt.inner.lock().unwrap();
        assert!(!ss.has_state(STMT_STATE_CURSOR_OPEN));
        assert!(!ss.has_state(STMT_STATE_CURSOR_EXHAUSTED));
        drop(ss);
        let ds = dbc.inner.lock().unwrap();
        assert!(ds.client.is_some(), "client restored after drain");
        assert_eq!(ds.active_stmt, None, "busy claim released");
    }

    // A cursor that is open but NOT exhausted (rows or result sets still pending)
    // is left untouched, so the caller's 24000 guard still fires.
    #[test]
    fn implicit_close_noop_when_not_exhausted() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(tds_client_from_tokens(vec![done_no_more()]));
            ds.active_stmt = Some(h.stmt);
        }

        implicit_cursor_close_if_exhausted(stmt, h.stmt);

        let ss = stmt.inner.lock().unwrap();
        assert!(ss.has_state(STMT_STATE_CURSOR_OPEN), "cursor left open");
        drop(ss);
        let ds = dbc.inner.lock().unwrap();
        assert_eq!(ds.active_stmt, Some(h.stmt), "connection still busy");
    }

    #[test]
    fn cursor_is_exhausted_requires_both_open_and_exhausted() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut ss = stmt.inner.lock().unwrap();

        ss.set_state(STMT_STATE_CURSOR_OPEN);
        assert!(!ss.cursor_is_exhausted(), "open alone is not exhausted");

        ss.clear_state(STMT_STATE_CURSOR_OPEN);
        ss.set_state(STMT_STATE_CURSOR_EXHAUSTED);
        assert!(
            !ss.cursor_is_exhausted(),
            "exhausted flag without open is not exhausted"
        );

        ss.set_state(STMT_STATE_CURSOR_OPEN);
        assert!(ss.cursor_is_exhausted(), "both bits set");
    }
}
