// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLMoreResults.
//!
//! Mirrors msodbcsql's `SQLMoreResults` (`sqlcresl.cpp`): close the current
//! rowset's reading state and advance to the next result set in the batch,
//! if any. Returns `SQL_SUCCESS` when a new result set is positioned,
//! `SQL_NO_DATA` when the batch is exhausted, or `SQL_ERROR` on failure.

use std::panic;

use tracing::{debug, error, trace};

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};

use super::close_cursor::reset_cursor_state;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SqlHandle, SqlReturn,
};
use crate::api::sqlstate::SQLSTATE_HY000;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{DbcHandle, HandleType, StmtHandle, handle_from_raw};

/// Advances to the next result set on a statement.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_more_results(statement_handle: SqlHandle) -> SqlReturn {
    debug!("SQLMoreResults called");

    let result = panic::catch_unwind(|| unsafe { sql_more_results_impl(statement_handle) });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLMoreResults: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLMoreResults returning");
    ret
}

unsafe fn sql_more_results_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLMoreResults: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

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

    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

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
                post_sql_error(
                    &mut ss,
                    SQLSTATE_HY000,
                    0,
                    "Connection is busy with results for another hstmt",
                );
            }
            return SQL_ERROR;
        }
        let Some(client) = dbc_state.client.take() else {
            error!("SQLMoreResults: no active TDS client");
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_sql_error(&mut ss, SQLSTATE_HY000, 0, "No active TDS client");
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
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.column_metadata = metadata;
                stmt_state.current_row = None;
            }
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                dbc_state.client = Some(client);
                // active_stmt remains set — cursor still open on this statement.
            }
            debug!("SQLMoreResults: advanced to next result set");
            SQL_SUCCESS
        }
        Ok(false) => {
            // Batch exhausted. Close cursor state and release the connection.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                reset_cursor_state(&mut stmt_state);
            }
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
            let msg = e.to_string();
            error!(%e, "SQLMoreResults: move_to_next failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                // Treat as terminal: clear cursor state and post diagnostic.
                reset_cursor_state(&mut stmt_state);
                post_sql_error(&mut stmt_state, SQLSTATE_HY000, 0, msg);
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
