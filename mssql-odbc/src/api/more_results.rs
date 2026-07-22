// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLMoreResults.
//!
//! Mirrors msodbcsql's `SQLMoreResults`: close the current
//! rowset's reading state and advance to the next result set in the batch,
//! if any. Returns `SQL_SUCCESS` when a new result set is positioned,
//! `SQL_NO_DATA` when the batch is exhausted, or `SQL_ERROR` on failure.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ResultSet, StatementResult};

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

    match dbc.runtime.block_on(client.move_to_next_statement()) {
        Ok(StatementResult::RowSet) => {
            // Positioned on a new row-returning result set. Refresh metadata,
            // clear row state, keep CURSOR_OPEN and active_stmt set.
            let metadata = client.get_metadata().clone();
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned advancing result set");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                }
                return SQL_ERROR;
            };
            stmt_state.column_metadata = metadata;
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
        Ok(StatementResult::NoRows { .. }) => {
            // Positioned on a no-row statement result (PRINT / low-severity
            // RAISERROR / DDL / DML): zero columns, so it is not fetchable
            // (SQLFetch returns 24000), but it is a navigable result and may
            // carry diagnostic messages. The connection stays busy so a further
            // SQLMoreResults can advance past it. Matches msodbcsql.
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLMoreResults: stmt mutex poisoned on no-row result");
                if let Ok(mut ds) = dbc.inner.lock() {
                    ds.client = Some(client);
                }
                return SQL_ERROR;
            };
            stmt_state.column_metadata.clear();
            stmt_state.current_row = None;
            let info_messages = client.take_info_messages();
            let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
            drop(stmt_state);
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                // active_stmt remains set — still positioned on this statement.
            }
            debug!("SQLMoreResults: advanced to a no-row statement result");
            if has_server_info {
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
        Ok(StatementResult::End) => {
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
            error!(%e, "SQLMoreResults: move_to_next_statement failed");
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
    use super::*;
    use crate::api::odbc_types::{SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO};
    use crate::api::sqlstate::ERR_NO_ACTIVE_TDS_CLIENT;
    use crate::handles::dbc::DbcHandle;
    use crate::test_support::TestHandles;
    use mssql_tds::test_client_support::{
        ScriptedToken, col_metadata_empty, done_more, done_no_more, info, tds_client_from_tokens,
    };

    /// Builds a scripted client, positions it on the batch's first statement,
    /// then injects it into `h`'s DBC as the busy client owning `h.stmt` with an
    /// open cursor — mirroring the state left by a successful `SQLExecDirect`.
    /// Returns the first statement's result so callers can assert on it.
    fn position_first_and_inject(h: &TestHandles, tokens: Vec<ScriptedToken>) -> StatementResult {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut client = tds_client_from_tokens(tokens);
        let first = dbc
            .runtime
            .block_on(client.execute_multi_statement("SELECT 1;".to_string(), None, None))
            .unwrap();
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
        }
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }
        first
    }

    /// SQLMoreResults advances from one row set to the next, keeping the cursor
    /// open and the connection busy on the same statement.
    #[test]
    fn more_results_advances_to_next_rowset() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(), // stmt1 row set
                done_more(),          // terminates stmt1, more to come
                col_metadata_empty(), // stmt2 row set
            ],
        );
        assert_eq!(first, StatementResult::RowSet);

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(h.stmt));
    }

    /// SQLMoreResults surfaces a no-row statement result (message-bearing, zero
    /// columns) as SQL_SUCCESS_WITH_INFO with the cursor kept open, then reports
    /// SQL_NO_DATA and releases the connection when the batch is exhausted.
    #[test]
    fn more_results_surfaces_norow_then_end() {
        let h = TestHandles::with_env_dbc_stmt();
        let first = position_first_and_inject(
            &h,
            vec![
                col_metadata_empty(),        // stmt1 row set
                done_more(),                 // terminates stmt1
                info(50000, 10, "raise me"), // stmt2 message
                done_no_more(),              // stmt2 no-row result, last in batch
            ],
        );
        assert_eq!(first, StatementResult::RowSet);

        // Advance onto the no-row statement result.
        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().column_metadata.is_empty());

        // Advance again: batch exhausted -> SQL_NO_DATA, cursor closed, released.
        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_NO_DATA);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
    }

    /// SQLMoreResults on an open cursor whose connection has no active client
    /// posts the no-active-client diagnostic and returns SQL_ERROR.
    #[test]
    fn more_results_no_active_client_errors() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.set_state(STMT_STATE_CURSOR_OPEN);
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = Some(h.stmt);

        let ret = unsafe { sql_more_results(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            ERR_NO_ACTIVE_TDS_CLIENT.state
        );
    }
}
