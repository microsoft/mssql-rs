// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLMoreResults.
//!
//! Mirrors msodbcsql's `SQLMoreResults`: close the current
//! rowset's reading state and advance to the next result set in the batch,
//! if any. Returns `SQL_SUCCESS` when a new result set is positioned,
//! `SQL_NO_DATA` when the batch is exhausted, or `SQL_ERROR` on failure.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};

use super::close_cursor::reset_cursor_state;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlReturn,
};
use crate::api::sqlstate::{
    ERR_CONNECTION_BUSY, ERR_NO_ACTIVE_TDS_CLIENT, SQLSTATE_HY000, post_diag, post_tds_error,
    post_tds_info_messages,
};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Advances to the next result set on a statement.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_more_results(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLMoreResults called");
    crate::ffi_entry!("SQLMoreResults", unsafe {
        sql_more_results_impl(statement_handle)
    })
}

unsafe fn sql_more_results_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLMoreResults: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_more_results_safe(statement_handle, stmt)
}

fn sql_more_results_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    // Free any stale diagnostics and observe cursor state.
    let cursor_open = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLMoreResults: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        // A pure-DML batch queued one row count per statement; step through them
        // in memory (no cursor or connection) before falling back to the wire.
        if let Some(next) = stmt_state.pending_row_counts.pop_front() {
            stmt_state.row_count = next;
            debug!("SQLMoreResults: advanced to next DML result set");
            return SQL_SUCCESS;
        }
        stmt_state.has_state(STMT_STATE_CURSOR_OPEN)
    };

    if !cursor_open {
        debug!("SQLMoreResults: no cursor open; no more result sets");
        return SQL_NO_DATA;
    }

    let dbc = stmt.parent_dbc();

    // Take the client; keep active_stmt set so concurrent statements continue
    // to see the connection as busy throughout the advance.
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLMoreResults: dbc mutex poisoned");
            return SQL_ERROR;
        };
        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            error!("SQLMoreResults: connection is busy with results for another statement");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }
        let Some(client) = dbc_state.client.take() else {
            error!("SQLMoreResults: no active TDS client");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };
        client
    };

    match dbc.runtime.block_on(client.move_to_next()) {
        Ok(true) => {
            // Positioned on a new result set. Refresh metadata, clear row state,
            // keep CURSOR_OPEN and active_stmt set.
            let metadata = client.get_metadata().clone();
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned advancing result set");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                }
                return SQL_ERROR;
            };
            stmt_state.column_metadata = metadata;
            // Refresh the count for the newly-positioned result set (-1 for a SELECT).
            stmt_state.row_count = client.last_rows_affected();
            stmt_state.current_row = None;
            // Drain INFO only after the lock is held.
            let info_messages = client.take_info_messages();
            let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                // active_stmt remains set — cursor still open on this statement.
            }
            debug!("SQLMoreResults: advanced to next result set");
            if has_server_info {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        Ok(false) => {
            // Batch exhausted. Close cursor state and release the connection.
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned at batch end");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                    if ds.active_stmt == Some(statement_handle) {
                        ds.active_stmt = None;
                    }
                }
                return SQL_ERROR;
            };
            reset_cursor_state(&mut stmt_state);
            // Drain INFO only after the lock is held.
            let info_messages = client.take_info_messages();
            post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            // The batch is fully drained, so the `sp_prepexec` `@handle` (if any)
            // has arrived; capture it so the next execute reuses it via
            // `sp_execute` instead of re-preparing.
            super::exec_common::capture_prepared_handle(stmt, &mut client);
            // TODO: surface output-param availability here once output
            // params land.
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                if dbc_state.active_stmt == Some(statement_handle) {
                    dbc_state.active_stmt = None;
                }
            }
            debug!("SQLMoreResults: no more result sets");
            SQL_NO_DATA
        }
        Err(e) => {
            error!(%e, "SQLMoreResults: move_to_next failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                // Treat as terminal: clear cursor state and post diagnostic.
                reset_cursor_state(&mut stmt_state);
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
    use std::collections::VecDeque;
    use std::ptr;

    use super::*;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe { sql_more_results(ptr::null_mut()) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn no_cursor_and_no_pending_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe { sql_more_results(h.stmt) };
        assert_eq!(rc, SQL_NO_DATA);
    }

    #[test]
    fn pending_dml_counts_stepped_then_exhausted() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt.inner.lock().unwrap();
            s.row_count = 3;
            s.pending_row_counts = VecDeque::from(vec![2, 1]);
        }

        // Each SQLMoreResults surfaces the next DML statement's count in memory.
        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().row_count, 2);

        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_SUCCESS);
        assert_eq!(stmt.inner.lock().unwrap().row_count, 1);

        // Queue drained and no cursor open -> end of batch.
        assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
    }
}
