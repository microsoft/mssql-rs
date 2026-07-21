// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFetch for forward-only, firehose result sets.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlReturn,
};
use crate::error::free_errors;
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
    sql_fetch_safe(statement_handle, stmt)
}

fn sql_fetch_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFetch: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            error!("SQLFetch: no open cursor on this statement");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
    }

    fetch_rows_next(statement_handle, stmt)
}

/// Row materialization step for one forward fetch operation.
fn fetch_rows_next(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let dbc = stmt.parent_dbc();

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
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
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
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    // At this point the connection is owned by this statement (`active_stmt`
    // was `Some(self)`) and the client has been taken. A no-row statement
    // result (PRINT / low-severity RAISERROR / DDL / DML) is positioned with
    // zero columns: there is nothing to fetch, so return 24000 (invalid cursor
    // state), matching msodbcsql. This is checked only after the busy-with-
    // other-statement (HY000) and already-drained (SQL_NO_DATA) cases above,
    // because those take precedence even when the column metadata is empty.
    {
        let no_columns = match stmt.inner.lock() {
            Ok(ss) => ss.column_metadata.is_empty(),
            Err(_) => {
                error!("SQLFetch: stmt mutex poisoned checking no-row result");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                }
                return SQL_ERROR;
            }
        };
        if no_columns {
            error!("SQLFetch: current result has no columns (no-row statement)");
            // Restore the client so the connection stays busy on this statement;
            // the application can still call SQLMoreResults to advance.
            if let Ok(mut ds) = dbc.inner.lock() {
                ds.client = Some(client);
            }
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_INVALID_CURSOR_STATE);
            }
            return SQL_ERROR;
        }
    }

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
            // Drain INFO only after the lock is held so a poisoned mutex cannot
            // silently drop the messages.
            let info_messages = client.take_info_messages();
            let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);

            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                dbc_state.active_stmt = Some(statement_handle);
            }

            debug!("SQLFetch: row fetched");
            if has_server_info {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        Ok(None) => {
            // End of current rowset. SQLFetch must return SQL_NO_DATA here per the
            // cursor contract, and SQL_NO_DATA cannot be upgraded to
            // SQL_SUCCESS_WITH_INFO — so this call has no way to signal "there are
            // diagnostic records worth reading", and many applications only pump
            // diagnostics after SQL_SUCCESS_WITH_INFO or SQL_ERROR.
            //
            // Therefore any INFO captured while reaching this DONE (e.g. a warning
            // emitted after the last row but before the result set's DONE) is
            // intentionally LEFT on the client's info buffer instead of being
            // posted here under SQL_NO_DATA. It is surfaced — with a return-code
            // hint — by whichever call the application makes next:
            //   * SQLMoreResults advancing to a further result set returns
            //     SQL_SUCCESS_WITH_INFO (its Ok(true) arm), or
            //   * SQLCloseCursor / SQLFreeStmt(SQL_CLOSE) returns
            //     SQL_SUCCESS_WITH_INFO (DrainOutcome::InfoPosted).
            // Neither `move_to_column_metadata`/`move_to_next` nor `close_query`
            // resets the info buffer, so nothing is lost by deferring; the
            // messages are simply attributed to the call that reports on the
            // batch boundary, mirroring msodbcsql's between-result surfacing.
            //
            // (If the batch has no further result set and the application calls
            // SQLMoreResults rather than closing the cursor, that call also
            // returns SQL_NO_DATA — an unavoidable consequence of the ODBC
            // contract, not message loss: the records are still posted there.)
            //
            // Do NOT drain the rest of the batch here either: the application may
            // call SQLMoreResults to advance to a subsequent result set. Cursor
            // stays open; active_stmt stays set so the connection remains "busy"
            // with this statement.
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLFetch: stmt mutex poisoned at end of rowset");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                }
                return SQL_ERROR;
            };
            stmt_state.current_row = None;
            // Don't clear CURSOR_OPEN here: the cursor stays open until
            // SQLMoreResults / SQLCloseCursor / SQLFreeStmt(SQL_CLOSE).
            drop(stmt_state);
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
                let info_messages = client.take_info_messages();
                post_tds_info_messages(&mut stmt_state, &info_messages);
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
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::handles::dbc::DbcHandle;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::test_support::TestHandles;

    #[test]
    fn fetch_null_handle() {
        let ret = unsafe { sql_fetch(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fetch_without_open_cursor_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn fetch_busy_with_other_statement_returns_hy000() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut dbc_state = dbc_handle.inner.lock().unwrap();
            dbc_state.active_stmt = Some(other_stmt);
        }

        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_HY000);
        assert_eq!(
            stmt_state.diag_records[0].message,
            "Connection is busy with results for another command"
        );
        drop(stmt_state);

        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert_eq!(dbc_state.active_stmt, Some(other_stmt));
    }

    /// CURSOR_OPEN is set but `active_stmt` is `None` — i.e. a previous fetch
    /// already drained the result set and cleared connection ownership, but
    /// the cursor hasn't been explicitly closed yet. Subsequent fetches must
    /// return `SQL_NO_DATA`, not an error.
    #[test]
    fn fetch_after_cursor_drained_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
        }
        // Leave dbc.active_stmt as None and dbc.client as None — this mirrors
        // the post-drain state that fetch_rows_next produces on Ok(None).

        let ret = unsafe { sql_fetch(h.stmt) };
        assert_eq!(ret, SQL_NO_DATA);

        // No diagnostic should be posted on the drained-cursor path.
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert!(stmt_state.diag_records.is_empty());
        assert!(stmt_state.has_state(STMT_STATE_CURSOR_OPEN));
    }
}
