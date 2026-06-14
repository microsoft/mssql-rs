// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFetch for forward-only, firehose result sets.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SqlHandle, SqlReturn,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::dbc::DbcHandle;
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::ResultSet;

/// Implements SQLFetch for the current forward-only result set.
///
/// This is the Phase 1 firehose-only path (FetchScroll with `SQL_FETCH_NEXT`
/// TODO: Add server-side cursor RPC fetch path and async re-entry handling.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_fetch(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFetch called");
    crate::ffi_entry!("SQLFetch", unsafe { sql_fetch_impl(statement_handle) })
}

unsafe fn sql_fetch_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFetch: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFetch: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            error!("SQLFetch: no open cursor on this statement");
            post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "Invalid cursor state");
            return SQL_ERROR;
        }
    }

    fetch_rows_next(statement_handle, stmt)
}

/// Row materialization step for one forward fetch operation.
fn fetch_rows_next(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetch: dbc mutex poisoned");
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            error!("SQLFetch: connection is busy with results for another statement");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_sql_error(
                    &mut ss,
                    SQLSTATE_HY000,
                    0,
                    "Connection is busy with results for another hstmt",
                );
            }
            return SQL_ERROR;
        }

        if dbc_state.active_stmt.is_none() {
            // End-of-set was already reached on a previous fetch: the connection
            // was drained and `active_stmt` cleared, but `CURSOR_OPEN` stays set
            // until SQLCloseCursor / SQLFreeStmt(SQL_CLOSE). Subsequent fetches
            // legitimately return SQL_NO_DATA rather than an error.
            debug!("SQLFetch: cursor already drained; returning SQL_NO_DATA");
            return SQL_NO_DATA;
        }

        let Some(client) = dbc_state.client.take() else {
            error!("SQLFetch: no active TDS client");
            // Keep active_stmt unchanged here. If this statement is in-flight,
            // clearing it would briefly hide the busy state from other statements.
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_sql_error(&mut ss, SQLSTATE_HY000, 0, "No active TDS client");
            }
            return SQL_ERROR;
        };

        client
    };

    let fetch_result = dbc.runtime.block_on(client.next_row());

    match fetch_result {
        Ok(Some(row)) => {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLFetch: stmt mutex poisoned storing row");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                    if ds.active_stmt == Some(statement_handle) {
                        ds.active_stmt = None;
                    }
                }
                return SQL_ERROR;
            };
            stmt_state.current_row = Some(row);
            drop(stmt_state);

            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                dbc_state.active_stmt = Some(statement_handle);
            }

            debug!("SQLFetch: row fetched");
            SQL_SUCCESS
        }
        Ok(None) => {
            // End of current rowset. Do NOT drain the rest of the batch — the
            // application may call SQLMoreResults to advance to a subsequent
            // result set (msodbcsql behaviour). Cursor stays open; active_stmt
            // stays set so the connection remains "busy" with this statement.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = None;
                // Dont clear CURSOR_OPEN here
                // Cursor stays open until SQLMoreResults / SQLCloseCursor / SQLFreeStmt(SQL_CLOSE)
            }
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
            }

            debug!("SQLFetch: no more rows in current result set");
            SQL_NO_DATA
        }
        Err(e) => {
            error!(%e, "SQLFetch: row fetch failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = None;
                stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
            }
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                if dbc_state.active_stmt == Some(statement_handle) {
                    dbc_state.active_stmt = None;
                }
            }
            SQL_ERROR
        }
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
    use crate::handles::dbc::DbcHandle;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;

    unsafe fn alloc_env_dbc_stmt() -> (SqlHandle, SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            0
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
            0
        );
        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            0
        );
        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            0
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
    fn fetch_null_handle() {
        let ret = unsafe { sql_fetch(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fetch_without_open_cursor_returns_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };
        let ret = unsafe { sql_fetch(stmt) };
        assert_eq!(ret, SQL_ERROR);
        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }

    #[test]
    fn fetch_busy_with_other_statement_returns_hy000() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };
        let mut other_stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut other_stmt) },
            0
        );

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        {
            let mut dbc_state = dbc_handle.inner.lock().unwrap();
            dbc_state.active_stmt = Some(other_stmt);
        }

        let ret = unsafe { sql_fetch(stmt) };
        assert_eq!(ret, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(
            stmt_state.diag_records[0].message,
            "Connection is busy with results for another hstmt"
        );
        drop(stmt_state);

        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert_eq!(dbc_state.active_stmt, Some(other_stmt));
        drop(dbc_state);

        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, other_stmt);
            free_env_dbc_stmt(env, dbc, stmt);
        };
    }

    /// CURSOR_OPEN is set but `active_stmt` is `None` — i.e. a previous fetch
    /// already drained the result set and cleared connection ownership, but
    /// the cursor hasn't been explicitly closed yet. Subsequent fetches must
    /// return `SQL_NO_DATA`, not an error.
    #[test]
    fn fetch_after_cursor_drained_returns_no_data() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }
        // Leave dbc.active_stmt as None and dbc.client as None — this mirrors
        // the post-drain state that fetch_rows_next produces on Ok(None).

        let ret = unsafe { sql_fetch(stmt) };
        assert_eq!(ret, SQL_NO_DATA);

        // No diagnostic should be posted on the drained-cursor path.
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert!(stmt_state.diag_records.is_empty());
        assert!(stmt_state.has_state(STMT_STATE_CURSOR_OPEN));
        drop(stmt_state);

        unsafe { free_env_dbc_stmt(env, dbc, stmt) };
    }
}
