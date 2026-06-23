// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLCloseCursor and the SQL_CLOSE path of SQLFreeStmt.
//!
//! Both operations close the cursor on a statement handle and release the
//! connection-level "busy" claim, allowing other statements on the same DBC
//! to execute.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
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

    drain_and_release(stmt, statement_handle);

    debug!("SQLCloseCursor: cursor closed");
    SQL_SUCCESS
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

    drain_and_release(stmt, statement_handle);

    debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
    SQL_SUCCESS
}

/// Resets cursor state on the statement (cursor is no longer open, metadata cleared).
pub(super) fn reset_cursor_state(stmt_state: &mut crate::handles::stmt::StmtState) {
    stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_CONTEXT);
    stmt_state.current_row = None;
    stmt_state.column_metadata.clear();
}

/// Takes the TDS client from the DBC, drains any pending tokens, and clears `active_stmt`.
/// `active_stmt` is kept set until the drain finishes so concurrent threads see the
/// connection as busy (HY000) throughout — not just until `client` is taken.
/// No locks are held during the network I/O.
pub(super) fn drain_and_release(stmt: &StmtHandle, statement_handle: SqlHandle) {
    let dbc = stmt.parent_dbc();

    // Take the client; intentionally leave active_stmt set while draining.
    let client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("drain_and_release: dbc mutex poisoned");
            return;
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
        return;
    };

    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        error!(%e, "drain_and_release: failed to drain TDS stream — connection may be broken");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
            if ds.active_stmt == Some(statement_handle) {
                ds.active_stmt = None;
            }
        }
        return;
    }

    // Drain complete: return client and release busy claim atomically.
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        if dbc_state.active_stmt == Some(statement_handle) {
            dbc_state.active_stmt = None;
        }
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
}
