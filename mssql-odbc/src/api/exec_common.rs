// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared execution helpers used by `SQLExecDirect` and `SQLExecute`.
//!
//! These factor out the connection-claim / client-restore dance so the two
//! execution paths stay in lockstep. None of these helpers hold a lock across
//! network I/O.

use tracing::error;

use std::collections::VecDeque;

use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_DATA_AT_EXEC, SQL_ERROR, SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SqlHandle, SqlLen, SqlReturn,
};
use crate::conversion::param_convert::{
    ParamBuildError, bound_param_to_rpc, dae_placeholder_type, is_data_at_exec_indicator,
};
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, StmtState,
};
use crate::handles::{DbcHandle, StmtHandle};

/// Clears the in-flight `EXEC_STARTED` flag on an execution failure so the
/// statement is reusable.
pub(super) fn clear_exec_started(stmt: &StmtHandle) {
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    }
}

/// Tears down an in-progress data-at-execution sequence and returns the
/// connection to the idle pool.
///
/// The transport is mid-write when a DAE sequence is abandoned, so the parked
/// request must be discarded via `cancel_streamed_write` before the client can
/// serve another command. `prepared` and `pending_unprepare` are restored so the
/// statement can simply be executed again, which is what the ODBC spec requires
/// after `SQLCancel`.
///
/// `diag`, when supplied, is posted against the statement before the state is
/// cleared.
pub(super) fn unwind_dae(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    diag: Option<DiagMsg>,
) {
    let client = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("stmt mutex poisoned unwinding DAE sequence");
            return;
        };
        if let Some(diag) = diag {
            post_diag(&mut stmt_state, diag);
        }
        let client = stmt_state.dae_client.take();
        stmt_state.prepared = stmt_state.dae_prepared.take();
        stmt_state.pending_unprepare = stmt_state.dae_orphaned.take();
        stmt_state.reset_dae();
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        client
    };

    if let Some(mut client) = client {
        dbc.runtime.block_on(client.cancel_streamed_write());
        return_client_idle(dbc, statement_handle, client);
    }
}

/// Aborts a data-at-execution sequence with a diagnostic. Always `SQL_ERROR`.
pub(super) fn abort_dae_with_diag(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    diag: DiagMsg,
) -> SqlReturn {
    unwind_dae(dbc, stmt, statement_handle, Some(diag));
    SQL_ERROR
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
/// A statement the client no longer holds a handle for is skipped inside
/// `unprepare`: a transparent reconnect already discarded it server-side, so an
/// `sp_unprepare` would target a nonexistent handle on the new session.
///
/// No lock is held across the network I/O.
pub(super) fn flush_pending_unprepare(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    client: &mut TdsClient,
    op: &str,
) {
    let pending = match stmt.inner.lock() {
        Ok(mut stmt_state) => stmt_state.pending_unprepare.take(),
        Err(_) => {
            error!("{op}: stmt mutex poisoned taking pending unprepare");
            return;
        }
    };
    let Some(handle) = pending else {
        return;
    };
    // `unprepare` recovers a dead connection first, then drops the handle only
    // if it still belongs to the (recovered) session — a superseded handle is
    // already gone server-side and is skipped without an RPC.
    if let Err(e) = dbc.runtime.block_on(client.unprepare(handle, ())) {
        error!(%e, "{op}: sp_unprepare failed — handle leaked until disconnect");
    }
}

/// Builds the ordered `@P1..@Pn` RPC parameter list from the statement's bound
/// parameters, reading application value buffers by reference. Posts the
/// matching diagnostic and returns `Err(SQL_ERROR)` when a marker is unbound
/// (`07002`) or a parameter cannot be built. Shared by `SQLExecute`
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
            Err(e) => {
                if let ParamBuildError::InvalidLength(len) = e {
                    error!("{op}: parameter {} has invalid StrLen_or_Ind {len}", i + 1);
                } else {
                    error!("{op}: parameter {} rejected: {}", i + 1, e.diag().text);
                }
                post_diag(stmt_state, e.diag());
                return Err(SQL_ERROR);
            }
        }
    }
    Ok(named_params)
}

/// Result of [`build_params_with_dae`]: the full RPC parameter list (with
/// data-at-execution placeholders) and the 0-based indices of those parameters.
pub(super) struct ParamsWithDae {
    /// All `@P1..@Pn` parameters in original order.  DAE entries carry a
    /// `data_at_exec()` flag and a `None` value; their data arrives later via
    /// `SQLPutData`.
    pub(super) params: Vec<RpcParameter>,
    /// 0-based indices (into `params`) of every DAE entry.
    pub(super) dae_indices: Vec<usize>,
    /// Expected byte counts from `SQL_LEN_DATA_AT_EXEC(n)`, parallel to
    /// `dae_indices`; `None` means `SQL_DATA_AT_EXEC`.
    pub(super) dae_expected_lengths: Vec<Option<usize>>,
}

fn dae_expected_length(indicator: SqlLen) -> Option<usize> {
    if indicator == SQL_DATA_AT_EXEC {
        None
    } else {
        Some((SQL_LEN_DATA_AT_EXEC_OFFSET - indicator) as usize)
    }
}

/// Variant of [`build_named_params`] that recognises data-at-execution
/// indicators and returns them as streaming-placeholder parameters instead of
/// converting the value buffer eagerly.
///
/// Parameters with `SQL_DATA_AT_EXEC` or `SQL_LEN_DATA_AT_EXEC(n)` indicators
/// are recorded in [`ParamsWithDae::dae_indices`]; all others are converted
/// immediately.  Returns `Err(SQL_ERROR)` for unbound markers or unsupported
/// types.
///
/// # Safety
/// Same contract as [`build_named_params`].
pub(super) unsafe fn build_params_with_dae(
    stmt_state: &mut StmtState,
    marker_count: usize,
    op: &str,
) -> Result<ParamsWithDae, SqlReturn> {
    use mssql_tds::message::parameters::rpc_parameters::StatusFlags;

    let mut params = Vec::with_capacity(marker_count);
    let mut dae_indices = Vec::new();
    let mut dae_expected_lengths = Vec::new();

    for i in 0..marker_count {
        let Some(Some(bound_param)) = stmt_state.bound_params.get(i) else {
            error!("{op}: parameter {} has no bound value", i + 1);
            post_diag(stmt_state, ERR_UNBOUND_PARAMETER);
            return Err(SQL_ERROR);
        };

        let name = format!("@P{}", i + 1);

        // Check for a data-at-execution indicator before dereferencing the
        // value buffer: DAE params carry no value at bind time.
        let dae_indicator = if !bound_param.strlen_or_ind_ptr.is_null() {
            let ind = unsafe { *bound_param.strlen_or_ind_ptr };
            is_data_at_exec_indicator(ind).then_some(ind)
        } else {
            None
        };

        if let Some(indicator) = dae_indicator {
            let placeholder_type = match dae_placeholder_type(bound_param.c_type) {
                Ok(t) => t,
                Err(e) => {
                    error!(
                        "{op}: parameter {} DAE type not streamable: {}",
                        i + 1,
                        e.diag().text
                    );
                    post_diag(stmt_state, e.diag());
                    return Err(SQL_ERROR);
                }
            };
            let rpc = RpcParameter::data_at_exec(Some(name), StatusFlags::NONE, placeholder_type);
            dae_indices.push(i);
            dae_expected_lengths.push(dae_expected_length(indicator));
            params.push(rpc);
        } else {
            match unsafe { bound_param_to_rpc(name, bound_param) } {
                Ok(param) => params.push(param),
                Err(ParamBuildError::InvalidLength(len)) => {
                    error!("{op}: parameter {} has invalid StrLen_or_Ind {len}", i + 1);
                    post_diag(stmt_state, ParamBuildError::InvalidLength(len).diag());
                    return Err(SQL_ERROR);
                }
                Err(e) => {
                    error!(
                        "{op}: parameter {} conversion failed: {}",
                        i + 1,
                        e.diag().text
                    );
                    post_diag(stmt_state, e.diag());
                    return Err(SQL_ERROR);
                }
            }
        }
    }

    Ok(ParamsWithDae {
        params,
        dae_indices,
        dae_expected_lengths,
    })
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

    if !has_result_set && client.has_open_batch() {
        // Statement-wise navigation: positioned on a no-row statement result
        // (PRINT / low-severity RAISERROR / DDL / DML) with more statements still
        // pending on the wire. Keep the connection busy and leave a 0-column
        // cursor open so SQLMoreResults can advance past it (and SQLFetch returns
        // 24000). Do NOT drain the wire — that would collapse the rest of the
        // batch. Matches msodbcsql.
        let info_messages = client.take_info_messages();
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("{op}: stmt mutex poisoned on no-row result");
            return_client_busy(dbc, client);
            return SQL_ERROR;
        };
        stmt_state.column_metadata = metadata; // empty (0 columns)
        // Statement-wise: report this no-row (DML/PRINT/RAISERROR) statement's
        // own affected-row count for SQLRowCount. Later statements' counts are
        // surfaced as SQLMoreResults advances onto each in turn (not pre-queued).
        stmt_state.row_count = client.last_rows_affected();
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_OPEN);
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
        drop(stmt_state);
        return_client_busy(dbc, client);
        return if has_server_info {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
    }

    if !has_result_set {
        // DDL / DML (last / only statement): drain trailing DONE tokens and
        // return to idle so the statement can re-execute without an explicit
        // close.
        if let Err(e) = dbc.runtime.block_on(client.close_query()) {
            error!(%e, "{op}: failed to drain after DDL/DML");
            return fail_with_tds(dbc, stmt, statement_handle, client, &e);
        }
        let info_messages = client.take_info_messages();
        // A pure-DML batch (UPDATE; DELETE; INSERT) yields one count per
        // statement. Report the first here; queue the rest for SQLMoreResults to
        // step through, matching msodbcsql's one result set per DML statement.
        let mut dml_counts: VecDeque<i64> = client.take_dml_result_counts().into();
        let first_count = dml_counts.pop_front().unwrap_or(-1);
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("{op}: stmt mutex poisoned");
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        stmt_state.column_metadata = metadata; // empty
        stmt_state.row_count = first_count;
        stmt_state.pending_row_counts = dml_counts;
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
    stmt_state.row_count = client.last_rows_affected();
    stmt_state.pending_row_counts.clear();
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
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_DEFAULT_PARAM, SQL_NTS, SQL_PARAM_INPUT, SQL_VARCHAR, SqlLen,
    };
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
            c_type_defaulted: false,
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

    /// `SQL_DEFAULT_PARAM` is terminal 07S01, not "not yet implemented" —
    /// asserted through the poster, since the enum-level check cannot catch a
    /// dropped `post_diag` or the wrong `DiagMsg`.
    #[test]
    fn build_named_params_default_param_posts_07s01() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut buf: Vec<u8> = b"x\0".to_vec();
        let mut ind: SqlLen = SQL_DEFAULT_PARAM;

        let mut state = stmt.inner.lock().unwrap();
        state
            .bound_params
            .push(Some(char_param(&mut buf, &mut ind)));

        let ret = unsafe { build_named_params(&mut state, 1, "test") };
        assert_eq!(ret.unwrap_err(), SQL_ERROR);
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07S01);
    }
}
