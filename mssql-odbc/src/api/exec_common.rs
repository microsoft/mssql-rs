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
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::sqlstate::*;
use crate::api::odbc_types::{SQL_ERROR, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn};
use crate::error::post_sql_error;
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, StmtState,
};
use crate::handles::{DbcHandle, StmtHandle};
use crate::params::convert::{ParamConvError, bound_param_to_rpc};

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

/// Claims the TDS client only if the connection is live and **idle** (no
/// statement currently holds it), marking `active_stmt` so the claim is visible
/// to concurrent threads. Returns `None` — without side effects — when
/// disconnected, busy, or the client is unavailable. Pairs with
/// [`return_client_idle`].
///
/// Unlike [`claim_connection`], this posts no diagnostics and never sets
/// `EXEC_STARTED`: it backs internal best-effort operations (e.g. releasing a
/// prepared handle on statement free) that must not disturb a busy connection.
pub(super) fn try_claim_idle_client(
    dbc: &DbcHandle,
    statement_handle: SqlHandle,
) -> Option<TdsClient> {
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        return None;
    };
    if dbc_state.connection_state != ConnectionState::Connected || dbc_state.active_stmt.is_some() {
        return None;
    }
    let client = dbc_state.client.take()?;
    dbc_state.active_stmt = Some(statement_handle);
    Some(client)
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

/// Releases a statement's pending orphaned prepared handle (from a re-prepare,
/// rebind, or `SQLExecDirect` supersede) via `sp_unprepare`, using the already
/// claimed `client`. Best-effort: any failure is logged and swallowed — a
/// leaked handle is freed when the connection closes, and must not fail the
/// caller's execution.
///
/// No lock is held across the network I/O.
pub(super) fn flush_pending_unprepare(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    client: &mut TdsClient,
    op: &str,
) {
    let handle = match stmt.inner.lock() {
        Ok(mut stmt_state) => stmt_state.pending_unprepare.take(),
        Err(_) => {
            error!("{op}: stmt mutex poisoned taking pending unprepare");
            return;
        }
    };
    let Some(handle) = handle else {
        return;
    };
    if let Err(e) = dbc
        .runtime
        .block_on(client.execute_sp_unprepare(handle, None, None))
    {
        error!(%e, handle, "{op}: sp_unprepare failed — handle leaked until disconnect");
    }
}

/// Builds the ordered `@P1..@Pn` RPC parameter list from the statement's bound
/// parameters, reading application value buffers by reference. Posts the
/// matching diagnostic and returns `Err(SQL_ERROR)` when a marker is unbound
/// (`07002`) or a value cannot be converted (`HYC00`). Shared by `SQLExecute`
/// and `SQLExecDirect`; `op` names the entry point for traceable diagnostics.
///
/// # Safety
/// Each bound parameter's value/indicator pointers must still satisfy the
/// `SQLBindParameter` contract; the buffers are read here.
pub(super) unsafe fn build_named_params(
    stmt_state: &mut StmtState,
    marker_count: usize,
    op: &str,
) -> Result<Vec<RpcParameter>, SqlReturn> {
    let mut named_params = Vec::with_capacity(marker_count);
    for i in 0..marker_count {
        let Some(Some(bound_param)) = stmt_state.bound_params.get(i) else {
            error!("{op}: parameter {} has no bound value", i + 1);
            post_diag(stmt_state, ERR_UNBOUND_PARAMETER);
            return Err(SQL_ERROR);
        };
        let name = format!("@P{}", i + 1);
        match unsafe { bound_param_to_rpc(name, bound_param) } {
            Ok(param) => named_params.push(param),
            Err(ParamConvError::InvalidLength(len)) => {
                error!("{op}: parameter {} has invalid StrLen_or_Ind {len}", i + 1);
                post_diag(stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
                return Err(SQL_ERROR);
            }
            Err(e) => {
                error!(
                    "{op}: parameter {} conversion failed: {}",
                    i + 1,
                    e.message()
                );
                post_sql_error(stmt_state, SQLSTATE_HYC00, 0, e.message());
                return Err(SQL_ERROR);
            }
        }
    }
    Ok(named_params)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NTS, SQL_PARAM_INPUT, SQL_VARCHAR, SqlLen};
    use crate::handles::handle_from_raw;
    use crate::params::BoundParam;
    use crate::test_support::TestHandles;
    use std::ffi::c_void;

    // The success path of `try_claim_idle_client` needs a real `TdsClient`,
    // which unit tests can't construct; these cover the guard branches (each
    // returns `None` without claiming `active_stmt`).

    #[test]
    fn try_claim_idle_client_none_when_disconnected() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // Default state is not connected.
        assert!(try_claim_idle_client(dbc, h.dbc).is_none());
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
    }

    #[test]
    fn try_claim_idle_client_none_when_busy() {
        let h = TestHandles::with_env_dbc();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // A different statement already holds the connection.
        let other = 0x1234 as SqlHandle;
        dbc.inner.lock().unwrap().active_stmt = Some(other);
        assert!(try_claim_idle_client(dbc, h.dbc).is_none());
        // The existing claim must be left untouched.
        assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(other));
    }

    /// Builds a `BoundParam` over the given char buffer and NTS indicator.
    fn char_param(buf: &mut [u8], ind: &mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: buf.as_mut_ptr() as *mut c_void,
            buffer_length: buf.len() as SqlLen,
            strlen_or_ind_ptr: ind as *mut SqlLen,
        }
    }

    #[test]
    fn build_named_params_zero_markers_yields_empty() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        let params = unsafe { build_named_params(&mut state, 0, "test") }.unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn build_named_params_builds_one_per_marker() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut buf1: Vec<u8> = b"abc\0".to_vec();
        let mut ind1: SqlLen = SQL_NTS as SqlLen;
        let mut buf2: Vec<u8> = b"de\0".to_vec();
        let mut ind2: SqlLen = SQL_NTS as SqlLen;

        let mut state = stmt.inner.lock().unwrap();
        state
            .bound_params
            .push(Some(char_param(&mut buf1, &mut ind1)));
        state
            .bound_params
            .push(Some(char_param(&mut buf2, &mut ind2)));

        let params = unsafe { build_named_params(&mut state, 2, "test") }.unwrap();
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn build_named_params_unbound_marker_posts_07002() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // One marker expected, but nothing bound.
        let mut state = stmt.inner.lock().unwrap();
        let ret = unsafe { build_named_params(&mut state, 1, "test") };
        assert_eq!(ret.unwrap_err(), SQL_ERROR);
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07002);
    }
}
