// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLCloseCursor and the SQL_CLOSE path of SQLFreeStmt.
//!
//! Both operations close the cursor on a statement handle and release the
//! connection-level "busy" claim, allowing other statements on the same DBC
//! to execute.

use std::panic;

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
use crate::error::DiagRecord;
use crate::handles::{DbcHandle, HandleType, StmtHandle, handle_from_raw};

/// Closes the cursor on `statement_handle` and discards any pending rows.
///
/// Mirrors msodbcsql's `SQLCloseCursor` → `SQLFreeStmt(SQL_CLOSE)` path.
/// Returns `SQL_ERROR` (SQLSTATE 24000) if no cursor is open, matching
/// msodbcsql's behaviour.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_close_cursor(statement_handle: SqlHandle) -> SqlReturn {
    let result = panic::catch_unwind(|| unsafe { sql_close_cursor_impl(statement_handle) });
    result.unwrap_or_else(|_| {
        error!("SQLCloseCursor: panic caught at FFI boundary");
        SQL_ERROR
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
    let result = panic::catch_unwind(|| unsafe { sql_free_stmt_close_impl(statement_handle) });
    result.unwrap_or_else(|_| {
        error!("SQLFreeStmt(SQL_CLOSE): panic caught at FFI boundary");
        SQL_ERROR
    })
}

unsafe fn sql_close_cursor_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLCloseCursor: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLCloseCursor: stmt mutex poisoned");
        return SQL_ERROR;
    };

    if !stmt_state.cursor_open {
        error!("SQLCloseCursor: no cursor is open — SQLSTATE 24000");
        stmt_state.diag_records.clear();
        stmt_state
            .diag_records
            .push(DiagRecord::new(SQLSTATE_24000, 0, "Invalid cursor state"));
        return SQL_ERROR;
    }

    close_cursor_on_stmt(&mut stmt_state);
    drop(stmt_state);

    release_active_stmt(stmt, statement_handle);

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

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFreeStmt(SQL_CLOSE): stmt mutex poisoned");
        return SQL_ERROR;
    };

    // No-op if cursor is already closed.
    if !stmt_state.cursor_open {
        return SQL_SUCCESS;
    }

    close_cursor_on_stmt(&mut stmt_state);
    drop(stmt_state);

    release_active_stmt(stmt, statement_handle);

    debug!("SQLFreeStmt(SQL_CLOSE): cursor closed");
    SQL_SUCCESS
}

/// Resets cursor state on the statement.
fn close_cursor_on_stmt(stmt_state: &mut crate::handles::stmt::StmtState) {
    stmt_state.cursor_open = false;
    stmt_state.column_metadata.clear();
}

/// Clears `dbc.active_stmt` if it points to this statement.
fn release_active_stmt(stmt: &StmtHandle, statement_handle: SqlHandle) {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLCloseCursor: dbc mutex poisoned when releasing active_stmt");
        return;
    };
    if dbc_state.active_stmt == Some(statement_handle) {
        dbc_state.active_stmt = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NULL_HANDLE,
        SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    unsafe fn alloc_env_dbc_stmt() -> (SqlHandle, SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                sql_set_env_attr(
                    env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                    0,
                )
            },
            SQL_SUCCESS
        );
        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            SQL_SUCCESS
        );
        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );
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
    fn close_cursor_null_handle() {
        let ret = unsafe { sql_close_cursor(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn close_cursor_no_cursor_open_returns_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };
        // No execute has been called — cursor_open is false.
        let ret = unsafe { sql_close_cursor(stmt) };
        assert_eq!(ret, SQL_ERROR);
        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }

    #[test]
    fn free_stmt_close_null_handle() {
        let ret = unsafe { sql_free_stmt_close(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn free_stmt_close_no_cursor_is_noop() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };
        // No execute has been called — cursor_open is false; SQL_CLOSE is a no-op.
        let ret = unsafe { sql_free_stmt_close(stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }
}
