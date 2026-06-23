// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLMoreResults.
//!
//! Mirrors msodbcsql's `SQLMoreResults` (`sqlcresl.cpp`): close the current
//! rowset's reading state and advance to the next result set in the batch,
//! if any. Returns `SQL_SUCCESS` when a new result set is positioned,
//! `SQL_NO_DATA` when the batch is exhausted, or `SQL_ERROR` on failure.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};

use super::close_cursor::reset_cursor_state;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SqlHandle, SqlReturn,
};
use crate::api::sqlstate::{
    ERR_CONNECTION_BUSY, ERR_NO_ACTIVE_TDS_CLIENT, SQLSTATE_HY000, post_diag, post_tds_error,
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
            error!(%e, "SQLMoreResults: move_to_next failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                // Treat as terminal: clear cursor state and post diagnostic.
                reset_cursor_state(&mut stmt_state);
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
