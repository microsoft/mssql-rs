// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared execution helpers used by `SQLExecDirect` and `SQLExecute`.
//!
//! These factor out the connection-claim / client-restore dance so the two
//! execution paths stay in lockstep. None of these helpers hold a lock across
//! network I/O.

use tracing::error;

use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::Error as TdsError;

use super::sqlstate::*;
use crate::api::odbc_types::{SQL_ERROR, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn};
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED,
};
use crate::handles::{DbcHandle, StmtHandle};

/// Clears the in-flight `EXEC_STARTED` flag on an execution failure so the
/// statement is reusable.
pub(super) fn clear_exec_started(stmt: &StmtHandle) {
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    }
}

/// Acquires the connection's TDS client for an execution, enforcing the
/// connection-busy / not-connected invariants and claiming `active_stmt`.
pub(super) fn claim_connection(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    op: &str,
) -> Result<TdsClient, SqlReturn> {
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned");
        clear_exec_started(stmt);
        return Err(SQL_ERROR);
    };

    if dbc_state.connection_state != ConnectionState::Connected {
        error!("{op}: DBC is not connected");
        drop(dbc_state);
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_CONNECTION_DOES_NOT_EXIST);
        }
        clear_exec_started(stmt);
        return Err(SQL_ERROR);
    }

    if let Some(busy_stmt) = dbc_state.active_stmt
        && busy_stmt != statement_handle
    {
        error!("{op}: connection is busy with results for another statement");
        drop(dbc_state);
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_CONNECTION_BUSY);
        }
        clear_exec_started(stmt);
        return Err(SQL_ERROR);
    }

    // Claim the connection before releasing the lock so concurrent threads see
    // active_stmt and get HY000 rather than "no active TDS client".
    dbc_state.active_stmt = Some(statement_handle);
    let Some(client) = dbc_state.client.take() else {
        error!("{op}: no active TDS client");
        dbc_state.active_stmt = None;
        drop(dbc_state);
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
        }
        clear_exec_started(stmt);
        return Err(SQL_ERROR);
    };

    Ok(client)
}

/// Returns `client` to the DBC and releases the busy claim. Used on the
/// DDL/DML success path and on error recovery.
pub(super) fn return_client_idle(dbc: &DbcHandle, statement_handle: SqlHandle, client: TdsClient) {
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        if dbc_state.active_stmt == Some(statement_handle) {
            dbc_state.active_stmt = None;
        }
    }
}

/// Returns `client` to the DBC but **keeps** the busy claim — used when a
/// cursor is left open for `SQLFetch`.
pub(super) fn return_client_busy(dbc: &DbcHandle, client: TdsClient) {
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
    }
}

/// Restores the client to idle, posts a TDS error to `stmt`, clears
/// `EXEC_STARTED`, and returns `SQL_ERROR`. The common failure tail for an
/// execution I/O error.
pub(super) fn fail_with_tds(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: TdsClient,
    err: &TdsError,
) -> SqlReturn {
    let info_messages = client.take_info_messages();
    return_client_idle(dbc, statement_handle, client);
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        post_tds_error(&mut stmt_state, err, SQLSTATE_HY000);
        post_tds_info_messages(&mut stmt_state, &info_messages);
    } else {
        error!("stmt mutex poisoned — could not post TDS error");
    }
    clear_exec_started(stmt);
    SQL_ERROR
}

/// Captures the server-side prepared-statement handle from `sp_prepexec`'s
/// `@handle` RETURNVALUE once the batch has been drained.
///
/// For a result-returning statement the handle arrives *after* the result set,
/// so it only lands in the client's return values once the stream is fully
/// drained via `close_query`. Capture-if-absent: the handle is stable for the
/// prepared plan, and `sp_execute` re-runs don't re-issue it.
pub(super) fn capture_prepared_handle(stmt: &StmtHandle, client: &mut TdsClient) {
    let Some(handle) = client.take_prepared_statement_handle() else {
        return;
    };
    if let Ok(mut stmt_state) = stmt.inner.lock()
        && stmt_state.prepared_handle.is_none()
    {
        stmt_state.prepared_handle = Some(handle);
    }
}

/// Captures result metadata after a successful execution and finalizes the
/// statement/connection state.
///
/// - **Result set** (non-empty `COLMETADATA`): the cursor is left open for
///   `SQLFetch`; the connection stays busy.
/// - **DDL/DML** (no `COLMETADATA`): the wire is drained via `close_query` and
///   the connection returns to idle so the statement can re-execute.
///
/// `EXEC_STARTED` is always cleared. No lock is held across the drain I/O.
pub(super) fn finish_execute(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: TdsClient,
    op: &str,
) -> SqlReturn {
    let metadata = client.get_metadata().clone();
    let has_result_set = !metadata.is_empty();

    if !has_result_set {
        // DDL / DML: drain trailing DONE tokens and return to idle.
        if let Err(e) = dbc.runtime.block_on(client.close_query()) {
            error!(%e, "{op}: failed to drain after DDL/DML");
            return fail_with_tds(dbc, stmt, statement_handle, client, &e);
        }
        capture_prepared_handle(stmt, &mut client);
        let info_messages = client.take_info_messages();
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("{op}: stmt mutex poisoned");
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        stmt_state.column_metadata = metadata; // empty
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_STARTED);
        let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
        drop(stmt_state);
        return_client_idle(dbc, statement_handle, client);
        return if has_server_info {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
    }

    // Result-bearing query: leave the cursor open for SQLFetch.
    let info_messages = client.take_info_messages();
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("{op}: stmt mutex poisoned");
        return_client_busy(dbc, client);
        return SQL_ERROR;
    };
    stmt_state.column_metadata = metadata;
    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_OPEN);
    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
    drop(stmt_state);
    return_client_busy(dbc, client);
    if has_server_info {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}
