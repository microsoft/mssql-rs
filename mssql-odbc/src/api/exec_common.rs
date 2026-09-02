// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared execution helpers used by `SQLExecDirect` and `SQLExecute`.
//!
//! These factor out the connection-claim / client-restore dance so the two
//! execution paths stay in lockstep. None of these helpers hold a lock across
//! network I/O.

use tracing::error;

use std::collections::VecDeque;
use std::time::Duration;

use mssql_tds::connection::tds_client::{
    CursorPoll, ExecuteOptions, ResultSet, StatementId, TdsClient,
};
use mssql_tds::error::{Error as TdsError, TimeoutErrorType};
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::ird::populate_ird;
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_DATA_AT_EXEC, SQL_ERROR, SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_NEED_DATA, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlReturn,
};
use crate::conversion::param_convert::{
    ParamBuildError, bound_param_to_rpc, dae_placeholder_type, is_data_at_exec_indicator,
};
use crate::error::post_sql_error;
use crate::handles::dbc::ConnectionState;
use crate::handles::stmt::{
    DaeParam, DaeState, PreparedPlan, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT,
    STMT_STATE_EXEC_STARTED, StmtState,
};
use crate::handles::{DbcHandle, DescHandle, StmtHandle, handle_from_raw};
use crate::params::BoundParam;

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
        let client = stmt_state.take_dae();
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        client
    };

    if let Some(mut client) = client {
        dbc.runtime.block_on(client.cancel_streamed_write());
        return_client_idle(dbc, statement_handle, client);
    }
}

/// Parks the streaming client on the statement so `SQLParamData` / `SQLPutData`
/// can drive the sequence, and enters the ODBC "Need Data" state. The DBC keeps
/// `active_stmt` set, so the connection stays busy for the duration.
///
/// `prepared` is `None` for `SQLExecDirect`, which runs ad-hoc `sp_executesql`
/// and has no plan to restore when the sequence completes.
pub(super) fn park_dae_client(
    stmt: &StmtHandle,
    client: TdsClient,
    prepared: Option<PreparedPlan>,
    orphaned: Option<StatementId>,
    dae_params: Vec<DaeParam>,
    op: &str,
) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        // The client has nowhere to go: the statement that owns it is
        // unreachable and the DBC still records it as busy.
        error!("{op}: stmt mutex poisoned while parking DAE client");
        return SQL_ERROR;
    };
    stmt_state.dae = Some(DaeState::new(client, prepared, orphaned, dae_params));
    SQL_NEED_DATA
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

/// Peeks whether another row follows the one just fully consumed, and
/// releases the DBC's busy claim (`active_stmt`) once both the peek and
/// `client.has_open_batch()` agree nothing remains for this statement to
/// read — matching msodbcsql's wire-state busy gate rather than holding the
/// connection for the statement's entire cursor lifetime (AB#47508). A row
/// the peek finds is parked (see [`TdsClient::peek_past_current_row`]) so
/// the next fetch consumes it without re-reading the wire. Also sets
/// `StmtState::batch_exhausted` alongside the release, so `SQLMoreResults`
/// can fast-path to `SQL_NO_DATA` the same way `SQLFetch` already does.
///
/// A peek failure that ends the batch (a SQL Server `ERROR` token) is
/// stashed on `stmt` as `pending_fetch_error` rather than posted here: this
/// call has already committed to its own success return for the row it
/// delivered, so posting now would never reach the caller. The next call
/// that would otherwise short-circuit past the wire believing there is
/// nothing left (`SQLFetch`'s `result_set_exhausted` fast path,
/// `SQLMoreResults`) drains and reports it instead.
///
/// If an RPC row set ends on `DONEINPROC` with MORE, the TDS layer consumes
/// only trailing RPC control tokens and parks the first non-tail token for
/// `SQLMoreResults`. A failure while consuming that completion tail abandons
/// the batch: timeout/cancellation has already drained through ATTENTION, and
/// other failures retire the connection. The error is deferred to the next
/// statement operation because this call has already committed to returning
/// the row successfully.
///
/// `row_delivered` tells this call whether it actually delivered data —
/// `true` for `SQLGetData` (a column was just captured) and for a
/// `SQLFetch`/`SQLFetchScroll` whose rowset held at least one row, `false`
/// for a zero-row `SQLFetchScroll`. Info messages are always drained from
/// `client` once the claim is released, regardless of `row_delivered` —
/// leaving them on `client` would otherwise leak into whichever statement
/// claims the connection next and get posted under its unrelated
/// diagnostics. Where they are *posted* still depends on `row_delivered`:
/// with a row delivered, this call's own `SQL_SUCCESS`/`SQL_SUCCESS_WITH_INFO`
/// return can carry them, so they are posted here directly. A zero-row
/// fetch's `SQL_NO_DATA` return cannot carry `SQL_SUCCESS_WITH_INFO` (and few
/// callers inspect diagnostics after it), so `fill_rowset` deliberately
/// leaves them out of its own post — they are stashed on
/// `StmtState::pending_fetch_info` instead, for `SQLMoreResults`'s
/// `batch_exhausted` fast path or a cursor close to surface later, exactly
/// like the deferred-error twin above.
///
/// # Caller obligation
/// Only call this once every column of the row positioned when `client` was
/// claimed has been read — like `next_row_cursor`, the peek discards
/// anything left unread on that row.
pub(super) fn release_busy_if_row_exhausted(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: TdsClient,
    row_delivered: bool,
) {
    let peek_result = match client.try_peek_past_current_row() {
        Ok(CursorPoll::Ready(has_row)) => Ok(has_row),
        Ok(CursorPoll::Pending) => dbc.runtime.block_on(client.peek_past_current_row()),
        Err(error) => Err(error),
    };

    // `Ok(true)`: another row, already parked for the next fetch — never
    // exhausted. `Ok(false)`: the current result set's own DONE token was
    // reached cleanly — always exhausted, regardless of whether a further
    // result set remains pending elsewhere in the batch (`SQLFetch` never
    // auto-advances across result sets, so reporting `SQL_NO_DATA` for a
    // fully-read one is correct on its own terms). `Err`: only exhausted if
    // the wire itself has given up on the whole batch (see doc comment).
    let result_set_exhausted = match &peek_result {
        Ok(has_more) => !has_more,
        Err(_) => !client.has_open_batch(),
    };

    // A row-returning RPC can end its visible row set with DONEINPROC MORE because
    // RETURNVALUE, RETURNSTATUS, and a terminal DONE still follow. Consume
    // those protocol-only tokens now. The first non-tail token is parked inside
    // TdsClient, so SQLMoreResults still observes it in order. Plain batch DONE
    // tokens return immediately without probing the next result.
    let completion_result = if result_set_exhausted && peek_result.is_ok() {
        Some(dbc.runtime.block_on(client.complete_current_result()))
    } else {
        None
    };
    let batch_done = match &completion_result {
        Some(Ok(done)) => *done,
        _ => !client.has_open_batch(),
    };
    let completion_failed = matches!(&completion_result, Some(Err(_)));
    let release = result_set_exhausted && batch_done;

    let mut read_error = peek_result.err();
    if let Some(Err(error)) = completion_result {
        read_error = Some(error);
    }

    let drained_info = if release {
        client.take_info_messages()
    } else {
        Vec::new()
    };

    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        dbc_state.active_stmt = if release {
            None
        } else {
            Some(statement_handle)
        };
    }

    if let Ok(mut stmt_state) = stmt.inner.lock() {
        if row_delivered {
            post_tds_info_messages(&mut stmt_state, &drained_info);
        } else {
            stmt_state.pending_fetch_info = drained_info;
        }
        if let Some(e) = read_error {
            error!(%e, "release_busy_if_row_exhausted: finishing current result failed");
            if batch_done || completion_failed {
                stmt_state.pending_fetch_error = Some(e);
            }
        }
        if result_set_exhausted {
            stmt_state.result_set_exhausted = true;
        }
        if release {
            stmt_state.batch_exhausted = true;
        }
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
/// `timeout_secs` bounds the wait the same way `SQL_ATTR_QUERY_TIMEOUT` bounds
/// the execute that follows — `0` means unlimited. Being best-effort, a
/// timeout here is logged like any other failure rather than propagated: the
/// caller's own budget (deducted by its elapsed wall-clock time) still gates
/// the execute that follows.
///
/// No lock is held across the network I/O.
pub(super) fn flush_pending_unprepare(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    client: &mut TdsClient,
    op: &str,
    timeout_secs: u32,
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
    if let Err(e) = dbc
        .runtime
        .block_on(client.unprepare(handle, ExecuteOptions::new().timeout_secs(timeout_secs)))
    {
        error!(%e, "{op}: sp_unprepare failed — handle leaked until disconnect");
    }
}

/// Deducts elapsed wall-clock time from a `SQL_ATTR_QUERY_TIMEOUT` budget
/// spent across multiple wire operations performed in sequence before the
/// caller's own execute — e.g. releasing an orphaned prepared handle, then
/// beginning an implicit transaction, then the real execute. Mirrors
/// msodbcsql's `DropPrepHandle` / `CheckOptions`, which charge the same
/// deducted timeout to each step (`sqlcfunc.cpp:787-828`, `sqlccmd.cpp:10572-10586`).
///
/// `0` means unlimited and passes through unchanged. A positive budget is
/// reduced by `elapsed`, truncated *down* to whole seconds — unlike
/// `mssql-tds`'s own internal `deduct_timeout`, which rounds a *measured*
/// recovery duration up to charge it conservatively, `elapsed` here is
/// measured across steps that may have done no I/O at all (e.g. an
/// autocommit-on connection skips the transaction begin entirely), so its
/// value is often a few microseconds of local bookkeeping (mutex locks,
/// staging). Rounding that up would charge a full second against the budget
/// for every step regardless of whether it touched the network, spuriously
/// exhausting a small timeout (e.g. `1`) before any wire wait ever happened.
/// Truncating instead only ever under-charges by less than one second, and
/// elapsed time that genuinely meets or exceeds the budget still exhausts it.
/// Returns `Err(())` once exhausted; the caller must fail with a timeout
/// rather than send the next step unbounded.
pub(super) fn deduct_query_timeout(timeout_secs: u32, elapsed: Duration) -> Result<u32, ()> {
    if timeout_secs == 0 {
        return Ok(0);
    }
    let elapsed_secs = u32::try_from(elapsed.as_secs()).unwrap_or(u32::MAX);
    match timeout_secs.checked_sub(elapsed_secs) {
        Some(remaining) if remaining > 0 => Ok(remaining),
        _ => Err(()),
    }
}

/// Builds the [`TdsError`] reported when [`deduct_query_timeout`] finds the
/// budget already exhausted ahead of the caller's own execute.
pub(super) fn query_timeout_expired_error() -> TdsError {
    TdsError::TimeoutError(TimeoutErrorType::String(
        "SQL_ATTR_QUERY_TIMEOUT expired before the statement could be sent".to_string(),
    ))
}

/// Result of [`build_named_params`]: the full RPC parameter list (with
/// data-at-execution placeholders) and a description of those placeholders.
pub(super) struct ParamsWithDae {
    /// All `@P1..@Pn` parameters in original order.  DAE entries carry a
    /// `data_at_exec()` flag and a `None` value; their data arrives later via
    /// `SQLPutData`.
    pub(super) params: Vec<RpcParameter>,
    /// Every DAE entry, in original parameter order.
    pub(super) dae_params: Vec<DaeParam>,
}

/// The byte total an application declared with `SQL_LEN_DATA_AT_EXEC(n)`, or
/// `None` when it declared no total and the length is whatever gets streamed.
///
/// `SQL_LEN_DATA_AT_EXEC(0)` is "unspecified", not "must be empty": msodbcsql
/// guards both of its length checks with `cbDAEDataTotal > 0`
/// (`sqlccmd.cpp:4548` per-put, `sqlccmd.cpp:6010` at close) and treats a zero
/// total the same as `NO_PARAM_LENGTH` (`sqlccmd.cpp:4160`). Folding it into
/// `None` here keeps that behaviour instead of rejecting the first byte with
/// `22026`.
fn dae_expected_length(indicator: SqlLen) -> Option<usize> {
    if indicator == SQL_DATA_AT_EXEC {
        return None;
    }
    match (SQL_LEN_DATA_AT_EXEC_OFFSET - indicator) as usize {
        0 => None,
        n => Some(n),
    }
}

/// Snapshots every parameter position currently bound on `stmt`'s effective
/// APD/IPD, in ordinal order.
///
/// Read once, immediately before an execute's main STMT-locked critical
/// section — never while that lock is held (see
/// ".github/instructions/mssql-odbc.instructions.md", "Locking rules": a
/// STMT lock must never be held while acquiring a DESC lock) — since ODBC
/// requires bindings to stay stable across one execute.
/// `SQLBindParameter` and `SQLSetDescFieldW` write into these same descriptor
/// records (AB#47437), so this is the one place `build_named_params` and the
/// data-at-execution lookups need to look, regardless of which API produced
/// the binding; the snapshot itself is what lets the rest of the execute path
/// (`build_named_params`, `StmtState::dae_current_c_type`) stay unchanged,
/// reading `StmtState::bound_params` exactly as it did before AB#47437.
///
/// `Err(SQL_ERROR)` on a poisoned STMT/env/APD/IPD mutex: the caller posts a
/// diagnostic against the statement and fails the execute outright, rather
/// than silently snapshotting "every parameter unbound" — which used to let
/// a zero-marker statement execute successfully despite the internal
/// failure, and reported a misleading `07002` for one with markers.
pub(super) fn snapshot_bound_params(
    stmt: &StmtHandle,
) -> Result<Vec<Option<BoundParam>>, SqlReturn> {
    // Read before the STMT lock below, matching bind_param.rs's own
    // parent-before-child lock ordering for the same lookup.
    let odbc_version = {
        let env = stmt.parent_dbc().parent_env();
        let Ok(env_state) = env.inner.lock() else {
            error!("snapshotting parameters: env mutex poisoned");
            return Err(SQL_ERROR);
        };
        env_state.odbc_version
    };

    let (apd, ipd) = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("snapshotting parameters: stmt mutex poisoned");
            return Err(SQL_ERROR);
        };
        (stmt_state.effective_apd(stmt), stmt.ipd)
    };

    // `apd` can be an explicit descriptor resolved under the STMT lock,
    // already dropped by now — re-check liveness right before dereferencing
    // to narrow (not fully close) the race against a concurrent
    // `SQLFreeHandle(SQL_HANDLE_DESC)` on that same descriptor. `ipd` is
    // always `stmt.ipd`, freed only with the statement itself.
    if crate::handles::live_type(apd) != Some(crate::handles::HandleType::Desc) {
        error!("snapshotting parameters: apd freed concurrently");
        return Err(SQL_ERROR);
    }
    let apd_desc = unsafe { handle_from_raw::<DescHandle>(apd) };
    let Ok(apd_state) = apd_desc.inner.lock() else {
        error!("snapshotting parameters: apd mutex poisoned");
        return Err(SQL_ERROR);
    };
    let ipd_desc = unsafe { handle_from_raw::<DescHandle>(ipd) };
    let Ok(ipd_state) = ipd_desc.inner.lock() else {
        error!("snapshotting parameters: ipd mutex poisoned");
        return Err(SQL_ERROR);
    };
    Ok(BoundParam::all_from_descriptor_states(
        &apd_state,
        &ipd_state,
        odbc_version,
    ))
}

/// Builds the ordered `@P1..@Pn` RPC parameter list from the statement's bound
/// parameters, reading application value buffers by reference. Shared by
/// `SQLExecute` and `SQLExecDirect`; `op` names the entry point for traceable
/// diagnostics.
///
/// Parameters with `SQL_DATA_AT_EXEC` or `SQL_LEN_DATA_AT_EXEC(n)` indicators
/// become streaming placeholders instead of having their value buffer read
/// eagerly, and are recorded in [`ParamsWithDae::dae_params`]; all others are
/// converted immediately. Posts the matching diagnostic and returns
/// `Err(SQL_ERROR)` when a marker is unbound (`07002`) or a parameter cannot be
/// built.
///
/// # Safety
/// Each bound parameter's value/indicator pointers must still satisfy the
/// `SQLBindParameter` contract; the buffers are read here.
pub(super) unsafe fn build_named_params(
    stmt_state: &mut StmtState,
    marker_count: usize,
    op: &str,
) -> Result<ParamsWithDae, SqlReturn> {
    use mssql_tds::message::parameters::rpc_parameters::StatusFlags;

    let mut params = Vec::with_capacity(marker_count);
    let mut dae_params = Vec::new();
    // Read once per execution: the attribute holds a pointer, and every
    // binding shifts by the same amount.
    let bind_offset = unsafe { stmt_state.inert_attrs.param_bind_offset() };

    for i in 0..marker_count {
        let Some(Some(bound_param)) = stmt_state.bound_params.get(i) else {
            error!("{op}: parameter {} has no bound value", i + 1);
            post_diag(stmt_state, ERR_UNBOUND_PARAMETER);
            return Err(SQL_ERROR);
        };
        // Applied before anything reads the binding: ODBC shifts the
        // indicator pointer alongside the value pointer, so the
        // data-at-execution check below has to see the shifted indicator.
        let bound_param = bound_param.with_bind_offset(bind_offset);

        let name = format!("@P{}", i + 1);

        // Check for a data-at-execution indicator before dereferencing the
        // value buffer: DAE params carry no value at bind time. Read from
        // `octet_length_ptr`, not `strlen_or_ind_ptr`: per ODBC's "Deferred
        // Fields" spec, SQL_DESC_OCTET_LENGTH_PTR carries the length or a DAE
        // sentinel, while SQL_DESC_INDICATOR_PTR carries only SQL_NULL_DATA
        // status. `SQLBindParameter` writes the same pointer to both, so this
        // is unchanged for the common case; `SQLSetDescFieldW`/`SQLSetDescRec`
        // can set them independently.
        let dae_indicator = if !bound_param.octet_length_ptr.is_null() {
            let ind = unsafe { bound_param.octet_length_ptr.read_unaligned() };
            is_data_at_exec_indicator(ind).then_some(ind)
        } else {
            None
        };

        if let Some(indicator) = dae_indicator {
            let dae_stream = match dae_placeholder_type(bound_param.c_type, bound_param.sql_type) {
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
            let rpc =
                RpcParameter::data_at_exec(Some(name), StatusFlags::NONE, dae_stream.sql_type);
            dae_params.push(DaeParam {
                value_ptr: bound_param.parameter_value_ptr,
                expected_len: dae_expected_length(indicator),
                needs_transcode: dae_stream.needs_transcode,
                c_type: bound_param.c_type,
                sql_type: bound_param.sql_type,
            });
            params.push(rpc);
        } else {
            match unsafe { bound_param_to_rpc(name, &bound_param) } {
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

    Ok(ParamsWithDae { params, dae_params })
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

    // Populated before `metadata` is moved into `stmt_state.begin_batch`
    // below (each arm), while it's still owned locally — avoids a clone per
    // arm purely to keep a copy alive across the STMT lock drop.
    // `populate_ird` only ever touches the IRD's own DescHandle, with no
    // dependency on `stmt_state`/`client`/`dbc`, so running it before the
    // STMT-locked bookkeeping instead of after doesn't change what state ends
    // up where — only whether an IRD failure is checked before or after that
    // bookkeeping runs, and every arm already runs the bookkeeping
    // unconditionally and reports the IRD failure (if any) in its return
    // value regardless.
    let ird_ok = populate_ird(stmt, &metadata).is_ok();

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
        stmt_state.begin_batch(metadata); // empty (0 columns)
        // Statement-wise: report this no-row (DML/PRINT/RAISERROR) statement's
        // own affected-row count for SQLRowCount. Later statements' counts are
        // surfaced as SQLMoreResults advances onto each in turn (not pre-queued).
        stmt_state.row_count = client.last_rows_affected();
        stmt_state.clear_exhaustion_state();
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_OPEN);
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
        drop(stmt_state);
        return_client_busy(dbc, client);
        if !ird_ok {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error refreshing result-set metadata",
                );
            }
            return SQL_ERROR;
        }
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
        stmt_state.begin_batch(metadata); // empty
        stmt_state.row_count = first_count;
        stmt_state.pending_row_counts = dml_counts;
        stmt_state.clear_exhaustion_state();
        stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_STARTED);
        let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
        drop(stmt_state);
        return_client_idle(dbc, statement_handle, client);
        if !ird_ok {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error refreshing result-set metadata",
                );
            }
            return SQL_ERROR;
        }
        return if has_server_info {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
    }

    // SQL Server can send COLMETADATA before the statement has produced its
    // first row. Wait for that row, end-of-set, or an ERROR token so execution
    // errors surface from SQLExecDirect/SQLExecute instead of a later SQLFetch.
    // A row is only positioned and parked; SQLFetch still receives it normally.
    if let Err(e) = dbc.runtime.block_on(client.peek_past_current_row()) {
        error!(%e, "{op}: failed before the first result row");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    // Result-bearing query: leave the cursor open for SQLFetch. This must stay
    // below the peek: the peek drains any INFO token in the post-metadata
    // window, and taking the messages first would leave them for a later fetch.
    let info_messages = client.take_info_messages();
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("{op}: stmt mutex poisoned");
        return_client_busy(dbc, client);
        return SQL_ERROR;
    };
    stmt_state.begin_batch(metadata);
    stmt_state.row_count = client.last_rows_affected();
    stmt_state.pending_row_counts.clear();
    stmt_state.clear_exhaustion_state();
    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT | STMT_STATE_CURSOR_OPEN);
    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);
    drop(stmt_state);
    return_client_busy(dbc, client);
    if !ird_ok {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error refreshing result-set metadata",
            );
        }
        return SQL_ERROR;
    }
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
        SQL_ATTR_PARAM_BIND_OFFSET_PTR, SQL_C_CHAR, SQL_C_LONG, SQL_DEFAULT_PARAM, SQL_INTEGER,
        SQL_NTS, SQL_PARAM_INPUT, SQL_VARCHAR, SqlLen, SqlULen, sql_len_data_at_exec,
    };
    use crate::handles::handle_from_raw;
    use crate::params::BoundParam;
    use crate::test_support::TestHandles;
    use mssql_tds::test_client_support::{
        ScriptedToken, col_metadata, col_metadata_empty, done_in_proc_more, done_more,
        done_no_more, done_proc_no_more, int_columns, sql_error, tds_client_from_tokens,
    };
    use std::ffi::c_void;

    // The success path of `try_claim_idle_client` needs a real `TdsClient`,
    // which unit tests can't construct; these cover the guard branches (each
    // returns `None` without claiming `active_stmt`).

    /// `SQL_LEN_DATA_AT_EXEC(0)` declares no total rather than an empty value,
    /// matching msodbcsql's `cbDAEDataTotal > 0` guards. Treating it as
    /// `Some(0)` would reject the first byte with `22026`.
    #[test]
    fn dae_expected_length_treats_zero_as_unspecified() {
        assert_eq!(dae_expected_length(SQL_DATA_AT_EXEC), None);
        assert_eq!(dae_expected_length(sql_len_data_at_exec(0)), None);
        assert_eq!(dae_expected_length(sql_len_data_at_exec(1)), Some(1));
        assert_eq!(dae_expected_length(sql_len_data_at_exec(4)), Some(4));
    }

    #[test]
    fn deduct_query_timeout_zero_is_unlimited() {
        assert_eq!(deduct_query_timeout(0, Duration::from_secs(1_000)), Ok(0));
    }

    #[test]
    fn deduct_query_timeout_truncates_sub_second_elapsed() {
        // 1.9s truncates to 1s, so a 10s budget leaves 9s, not 8s.
        assert_eq!(
            deduct_query_timeout(10, Duration::from_millis(1_900)),
            Ok(9)
        );
    }

    #[test]
    fn deduct_query_timeout_exhausted_at_or_past_budget_errs() {
        assert_eq!(deduct_query_timeout(5, Duration::from_secs(5)), Err(()));
        assert_eq!(deduct_query_timeout(5, Duration::from_secs(6)), Err(()));
    }

    /// The error `execute.rs`/`exec_direct.rs` report when a pre-execute
    /// `deduct_query_timeout` call finds the budget already exhausted — a
    /// `TimeoutError`, matching every other query-timeout expiry, so
    /// `post_tds_error` maps it to `HYT00` (see `sqlstate.rs`'s
    /// `post_tds_error_timeout_maps_to_hyt00_regardless_of_default`) the same
    /// way whether the budget ran out before or during the wire call.
    #[test]
    fn query_timeout_expired_error_is_a_timeout_error() {
        match query_timeout_expired_error() {
            TdsError::TimeoutError(TimeoutErrorType::String(msg)) => {
                assert!(
                    msg.contains("SQL_ATTR_QUERY_TIMEOUT"),
                    "message should name the attribute that expired: {msg}"
                );
            }
            other => panic!("expected TimeoutError(String(_)), got: {other:?}"),
        }
    }

    /// `SQLExecDirectW` calls `deduct_query_timeout` twice in sequence — once
    /// after `flush_pending_unprepare`, once after `begin_transaction_if_manual`
    /// — each time measuring the *cumulative* elapsed time since the call
    /// began against the *original, fixed* budget (not the previous call's
    /// return value). Composing them must charge each step's elapsed time
    /// exactly once and must not floor away sub-second remainders
    /// independently at each step: a 10s budget with a 3s unprepare and a 2s
    /// implicit transaction begin must leave 5s (10 - (3+2)), not the 2s an
    /// earlier version of this code produced by re-deducting from its own
    /// shrinking result (`(10-3)-(3+2)=2`, double-charging the first step), or
    /// the loss a per-step-floored version would suffer with sub-second steps
    /// — both caught in mssql-rs#442 review by an independent reviewer tracing
    /// the exact arithmetic; the numbers here are theirs.
    #[test]
    fn deduct_query_timeout_composed_twice_charges_each_step_once() {
        let budget = 10;
        let after_unprepare = deduct_query_timeout(budget, Duration::from_secs(3)).unwrap();
        assert_eq!(
            after_unprepare, 7,
            "remaining allowance handed to the next step"
        );
        let after_begin = deduct_query_timeout(budget, Duration::from_secs(3 + 2)).unwrap();
        assert_eq!(
            after_begin, 5,
            "budget minus total elapsed, not double-charged"
        );
    }

    /// Composing from a *fixed* original budget with *cumulative* elapsed
    /// (what `SQLExecDirectW` now does) is stricter than composing from a
    /// shrinking budget with each step's own elapsed measured in isolation:
    /// the latter floors sub-second remainders away independently at every
    /// step, so two 0.99s pre-execute steps against a 1s budget would each
    /// charge nothing, letting the following execute start with a fresh 1s —
    /// about 3x the configured timeout in wall-clock terms before any
    /// network wait even begins. Deducting cumulatively from the original
    /// budget catches this instead: composing across the same two steps
    /// exhausts a 1s budget, matching msodbcsql's own millisecond-granularity
    /// deduction (`dwQueryTimeoutInMS` in `DropPrepHandle`) not losing
    /// sub-second remainders.
    #[test]
    fn deduct_query_timeout_cumulative_composition_catches_accumulated_sub_second_cost() {
        let budget = 1;
        let step_1 = Duration::from_millis(990);
        let step_2 = Duration::from_millis(990);

        let per_step_reseeded =
            deduct_query_timeout(deduct_query_timeout(budget, step_1).unwrap(), step_2);
        assert_eq!(
            per_step_reseeded,
            Ok(1),
            "per-step composition floors each sub-second cost away independently"
        );

        let cumulative_fixed_budget = deduct_query_timeout(budget, step_1)
            .and_then(|_| deduct_query_timeout(budget, step_1 + step_2));
        assert_eq!(
            cumulative_fixed_budget,
            Err(()),
            "cumulative composition against the fixed budget must see the combined cost"
        );
    }

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

    #[test]
    fn finish_execute_surfaces_an_error_before_the_first_row() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut client = tds_client_from_tokens(vec![
            col_metadata(int_columns(1)),
            sql_error(1222, 16, "Lock request time out period exceeded."),
            done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT blocked".to_string(), ()))
            .unwrap();

        let rc = finish_execute(dbc, stmt, h.stmt, client, "SQLExecDirectW");

        assert_eq!(rc, SQL_ERROR);
        let stmt_state = stmt.inner.lock().unwrap();
        assert!(
            stmt_state
                .diag_records
                .iter()
                .any(|record| record.native_error == 1222)
        );
    }

    /// The peek does not stop at rows and errors: `handle_row_read_token`'s
    /// `Tokens::Info` arm captures and continues, so an INFO token sitting
    /// between `COLMETADATA` and the first `ROW`/`DONE` is now drained during
    /// execution rather than by the first `SQLFetch`. The success tail posts it,
    /// which turns `SQL_SUCCESS` into `SQL_SUCCESS_WITH_INFO` for that window.
    ///
    /// This matches msodbcsql, whose parse loop covers the same window before
    /// parking at the first row. It is pinned here because the behaviour is
    /// otherwise invisible: moving the peek below `take_info_messages()` would
    /// silently restore execute-time `SQL_SUCCESS` and defer the diagnostic
    /// back to `SQLFetch`, and no other test would fail.
    #[test]
    fn finish_execute_reports_an_info_message_arriving_before_the_first_row() {
        use mssql_tds::test_client_support::info;

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut client = tds_client_from_tokens(vec![
            col_metadata(int_columns(1)),
            info(
                8153,
                10,
                "Null value is eliminated by an aggregate or other SET operation.",
            ),
            done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT SUM(col) FROM t".to_string(), ()))
            .unwrap();

        let rc = finish_execute(dbc, stmt, h.stmt, client, "SQLExecDirectW");

        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        let stmt_state = stmt.inner.lock().unwrap();
        assert!(
            stmt_state
                .diag_records
                .iter()
                .any(|record| record.native_error == 8153),
            "the warning must be posted under the execute that drained it"
        );
    }

    /// Builds a scripted client positioned on a row-returning result (empty
    /// metadata; column data is irrelevant to `release_busy_if_row_exhausted`,
    /// which only peeks past it), then injects it as the busy client owning
    /// `h.stmt` — mirroring the state left by a fetch that has just consumed a
    /// row's last column.
    fn position_and_inject(h: &TestHandles, tokens: Vec<ScriptedToken>) {
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_tokens(tokens);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        let mut ds = dbc.inner.lock().unwrap();
        ds.client = Some(client);
        ds.active_stmt = Some(h.stmt);
    }

    #[test]
    fn release_busy_if_row_exhausted_releases_when_wire_is_done() {
        // The peek finds the terminating DONE: the wire is provably idle for
        // this statement even though its cursor stays open, so the busy claim
        // is released immediately (AB#47508) and the statement is marked so a
        // later SQLFetch can report SQL_NO_DATA without touching the
        // connection at all.
        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(&h, vec![col_metadata_empty(), done_no_more()]);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        assert!(dbc.inner.lock().unwrap().client.is_some());
        let ss = stmt.inner.lock().unwrap();
        assert!(ss.result_set_exhausted);
        assert!(
            ss.batch_exhausted,
            "the whole batch is done here (single-statement, no MORE), so \
             SQLMoreResults must also be able to fast-path without touching \
             the connection"
        );
    }

    #[test]
    fn release_busy_if_row_exhausted_drains_a_protocol_only_rpc_tail() {
        // A SELECT inside sp_executesql/sp_prepexec ends with DONE_MORE even
        // when no application-visible result follows. The RPC's terminal DONE
        // must be consumed before another statement can safely use the wire.
        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                done_in_proc_more(),
                done_proc_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        let ss = stmt.inner.lock().unwrap();
        assert!(ss.result_set_exhausted);
        assert!(ss.batch_exhausted);
    }

    #[test]
    fn release_busy_if_row_exhausted_defers_a_truncated_rpc_tail() {
        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(&h, vec![col_metadata_empty(), done_in_proc_more()]);

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        let ss = stmt.inner.lock().unwrap();
        assert!(ss.result_set_exhausted);
        assert!(ss.batch_exhausted);
        assert!(matches!(
            ss.pending_fetch_error,
            Some(TdsError::ConnectionClosed(_))
        ));
    }

    /// The reviewer-flagged AB#47508 regression: `SELECT 1; SELECT 2;` as one
    /// batch. The peek reaches result set 1's own terminating DONE — which
    /// carries the MORE flag, since result set 2 is still to come — so
    /// `result_set_exhausted` is correctly set (a further `SQLFetch` on this
    /// statement, without an intervening `SQLMoreResults`, must report
    /// `SQL_NO_DATA` per ODBC's per-result-set fetch semantics), but the busy
    /// claim must NOT be released: `client.has_open_batch()` is still true,
    /// so a different statement claiming the connection now would desync
    /// whichever statement later reads result set 2's COLMETADATA/DONE via
    /// `SQLMoreResults`.
    #[test]
    fn release_busy_if_row_exhausted_keeps_busy_when_a_further_result_set_is_pending() {
        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                done_more(),
                col_metadata_empty(),
                done_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(h.stmt),
            "a pending second result set must keep this statement's busy claim"
        );
        assert!(dbc.inner.lock().unwrap().client.is_some());
        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.result_set_exhausted,
            "result set 1 itself has no more rows, independent of result set 2 being pending"
        );
        assert!(
            !ss.batch_exhausted,
            "the batch is not done — SQLMoreResults must still genuinely \
             advance to result set 2, not fast-path to SQL_NO_DATA"
        );
    }

    /// An INFO token the peek reads on its way to the terminating DONE must be
    /// attributed to the statement that produced it, not left to leak onto
    /// whichever statement next touches the client (e.g. a different one that
    /// claims the now-idle connection).
    #[test]
    fn release_busy_if_row_exhausted_attributes_a_peeked_info_message_to_this_statement() {
        use mssql_tds::test_client_support::info;

        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                info(50000, 10, "trailing message"),
                done_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        let ss = stmt.inner.lock().unwrap();
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("trailing message")),
            "the peeked INFO message must land on this statement's own diagnostics"
        );
    }

    /// A trailing SQL Server `ERROR` token during the peek must not be
    /// silently swallowed — it collapses the whole batch on the wire
    /// (`handle_row_read_token`'s error arm forces `has_open_batch` false), so
    /// the busy claim is released same as an ordinary exhausted result set.
    /// But this call has already committed to a success return for the row
    /// it delivered, so the diagnostic cannot be posted here — no return
    /// code would tell the caller to look. It is deferred via
    /// `pending_fetch_error` for the next call that would otherwise
    /// silently short-circuit past the wire (`SQLFetch`'s fast path,
    /// `SQLMoreResults`) to drain and report instead (AB#47508 follow-up).
    #[test]
    fn release_busy_if_row_exhausted_defers_a_trailing_sql_error() {
        use mssql_tds::test_client_support::sql_error;

        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                sql_error(547, 16, "FOREIGN KEY constraint violation"),
                done_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert!(
            dbc.inner.lock().unwrap().active_stmt.is_none(),
            "the ERROR token ends the batch, so the busy claim must still be released"
        );
        let ss = stmt.inner.lock().unwrap();
        assert!(
            !ss.diag_records
                .iter()
                .any(|d| d.message.contains("FOREIGN KEY constraint violation")),
            "posting it here would attach it to this call's own SQL_SUCCESS return, \
             which the caller has no reason to inspect"
        );
        match &ss.pending_fetch_error {
            Some(TdsError::SqlServerError { diagnostics }) => {
                assert!(
                    diagnostics
                        .errors
                        .iter()
                        .any(|e| e.message.contains("FOREIGN KEY constraint violation")),
                    "the deferred error must be the one the peek actually found"
                );
            }
            other => panic!("expected a stashed SqlServerError, got {other:?}"),
        }
        assert!(ss.result_set_exhausted);
    }

    // The "peek finds another row and keeps the connection busy" branch is
    // covered in `mssql_tds::connection::tds_client::tests` instead: it needs
    // `TdsClient::row_already_positioned`, a private field the scripted
    // transport in `test_client_support` cannot reach from outside the crate
    // (see its module docs — there are no real row bytes to manufacture one
    // through a fresh read either).

    #[test]
    fn release_busy_if_row_exhausted_swallows_peek_failure_and_stays_busy() {
        // A transport failure during the peek must not be silently reported
        // as "exhausted" — that would tell a later SQLFetch there is nothing
        // left when the connection may in fact be broken. The failure isn't
        // posted here either: `release` is false, so this statement's state
        // is untouched, and its very next real operation on this connection
        // takes the normal route and organically rediscovers the same
        // failure through its own existing error handling — unlike the
        // batch-ending-in-a-SQL-Server-ERROR case, nothing here has already
        // committed to a success return that would hide a diagnostic.
        let h = TestHandles::with_env_dbc_stmt();
        // No tokens queued: the peek's `next_row_cursor` read fails immediately.
        position_and_inject(&h, vec![col_metadata_empty()]);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(h.stmt));
        assert!(dbc.inner.lock().unwrap().client.is_some());
        let ss = stmt.inner.lock().unwrap();
        assert!(!ss.result_set_exhausted);
        assert!(!ss.batch_exhausted);
        assert!(
            ss.diag_records.is_empty(),
            "a non-exhausting failure must not be posted here — this call \
             may have already returned success for the row it delivered, \
             and the connection's next real user rediscovers the same \
             failure fresh anyway"
        );
        assert!(ss.pending_fetch_error.is_none());
    }

    /// A zero-row fetch discovering the current result set is done, with a
    /// further result set still pending in the batch, must leave any info
    /// message already on the client alone — that message belongs to
    /// whichever call (`SQLMoreResults`) actually reads the client next, not
    /// to this one, since the claim was not released.
    #[test]
    fn release_busy_if_row_exhausted_leaves_info_messages_when_the_claim_is_not_released() {
        use mssql_tds::test_client_support::info;

        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                info(50000, 10, "leave me for SQLMoreResults"),
                done_more(),
                col_metadata_empty(),
                done_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, true);

        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(h.stmt),
            "a pending second result set means the claim is not released"
        );
        assert!(
            !stmt
                .inner
                .lock()
                .unwrap()
                .diag_records
                .iter()
                .any(|d| d.message.contains("leave me for SQLMoreResults")),
            "the message must not be posted under this call, which returns a \
             code the caller may never inspect diagnostics for"
        );
        let dbc_state = dbc.inner.lock().unwrap();
        assert!(
            dbc_state
                .client
                .as_ref()
                .unwrap()
                .info_messages()
                .iter()
                .any(|m| m.message.contains("leave me for SQLMoreResults")),
            "the message must still be resident on the client for \
             SQLMoreResults to find and surface"
        );
    }

    /// The other half of the `release` gate: even when the claim *is*
    /// released (a single-statement zero-row batch — nothing pending after
    /// it), a fetch that filled zero rows must still not post any drained
    /// info message under its own return. `fill_rowset` deliberately does
    /// not drain its own info messages for a zero-row fetch (its
    /// `SQL_NO_DATA` return can't carry `SQL_SUCCESS_WITH_INFO`) — posting
    /// them here anyway, just because `release` happens to be true, would
    /// work against that. But leaving them resident on `client` isn't safe
    /// either once the claim is released: a different statement could claim
    /// the now-idle connection next and have its own unrelated diagnostics
    /// contaminated by them (or, if nothing else claims it first,
    /// `SQLMoreResults`'s `batch_exhausted` fast path wouldn't even look at
    /// `client` to find them — see AB#47508 follow-up). So this drains the
    /// message off `client` right away and stashes it on
    /// `StmtState::pending_fetch_info` instead, for `SQLMoreResults` or a
    /// cursor close to surface later.
    #[test]
    fn release_busy_if_row_exhausted_stashes_info_messages_when_no_row_was_delivered() {
        use mssql_tds::test_client_support::info;

        let h = TestHandles::with_env_dbc_stmt();
        position_and_inject(
            &h,
            vec![
                col_metadata_empty(),
                info(50000, 10, "leave me for the next call"),
                done_no_more(),
            ],
        );

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let client = dbc.inner.lock().unwrap().client.take().unwrap();

        release_busy_if_row_exhausted(dbc, stmt, h.stmt, client, false);

        assert!(
            dbc.inner.lock().unwrap().active_stmt.is_none(),
            "the batch is genuinely done here, so the claim is still released"
        );
        let ss = stmt.inner.lock().unwrap();
        assert!(
            !ss.diag_records
                .iter()
                .any(|d| d.message.contains("leave me for the next call")),
            "row_delivered == false must suppress posting under this call's own return"
        );
        assert!(
            ss.pending_fetch_info
                .iter()
                .any(|m| m.message.contains("leave me for the next call")),
            "must be drained off the client and stashed for SQLMoreResults/close to surface"
        );
        drop(ss);
        let dbc_state = dbc.inner.lock().unwrap();
        assert!(
            dbc_state
                .client
                .as_ref()
                .unwrap()
                .info_messages()
                .is_empty(),
            "must not stay resident on the client, where a different statement \
             claiming the connection next could have it misattributed to its \
             own diagnostics"
        );
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
            octet_length_ptr: ind as *mut SqlLen,
        }
    }

    /// The narrow race `snapshot_bound_params`'s liveness check guards
    /// against: `effective_apd` resolves an explicit descriptor under the
    /// STMT lock, which is dropped before the descriptor is actually locked
    /// and read. If a concurrent `SQLFreeHandle(SQL_HANDLE_DESC)` completes
    /// in that window, the stale pointer must fail cleanly (`Err`), not
    /// dereference freed memory. Reassociating the APD and then freeing it
    /// reproduces the state that window leaves behind — `active_apd` still
    /// points at the freed handle, since only a *subsequent* `SQLBindCol`/
    /// `SQLBindParameter`-family call re-resolves it via `free_desc`'s
    /// association reset.
    #[test]
    fn snapshot_bound_params_fails_cleanly_on_a_freed_apd() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_apd = h.alloc_explicit_desc();
        unsafe {
            crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                h.stmt,
                crate::api::odbc_types::SQL_ATTR_APP_PARAM_DESC,
                explicit_apd as crate::api::odbc_types::SqlPointer,
                0,
            )
        };
        {
            // Directly clears the tracked association without going through
            // `free_desc`'s association-reset walk, so `active_apd` is left
            // dangling exactly as it would be mid-race — `h.free_explicit_desc`
            // goes through the real `SQLFreeHandle` path, which already resets
            // the association and would defeat the point of this test.
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut ss = stmt.inner.lock().unwrap();
            ss.active_apd = None;
        }
        unsafe {
            crate::handles::free_handle::<DescHandle>(explicit_apd);
        }
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut ss = stmt.inner.lock().unwrap();
            ss.active_apd = Some(explicit_apd);
        }
        assert!(snapshot_bound_params(stmt).is_err());
    }

    #[test]
    fn build_named_params_zero_markers_yields_empty() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        let built = unsafe { build_named_params(&mut state, 0, "test") }.unwrap();
        assert!(built.params.is_empty());
        assert!(built.dae_params.is_empty());
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

        let built = unsafe { build_named_params(&mut state, 2, "test") }.unwrap();
        assert_eq!(built.params.len(), 2);
        assert!(built.dae_params.is_empty());
    }

    #[test]
    fn build_named_params_unbound_marker_posts_07002() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // One marker expected, but nothing bound.
        let mut state = stmt.inner.lock().unwrap();
        let ret = unsafe { build_named_params(&mut state, 1, "test") };
        assert!(ret.is_err());
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
        assert!(ret.is_err());
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07S01);
    }

    /// A data-at-execution marker between two ordinary binds keeps its ordinal
    /// position: the `@P1..@Pn` list still carries one entry per marker in
    /// order, and only the middle one is staged for streaming. Pins the
    /// interleaving that `DataAtExecutionInterleavesWithBoundParams` can only
    /// observe through the concatenated server result.
    #[test]
    fn build_named_params_keeps_streamed_marker_in_position() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut first: Vec<u8> = b"a\0".to_vec();
        let mut first_ind: SqlLen = SQL_NTS as SqlLen;
        let mut streamed_ind: SqlLen = SQL_DATA_AT_EXEC;
        let mut last: Vec<u8> = b"d\0".to_vec();
        let mut last_ind: SqlLen = SQL_NTS as SqlLen;

        let mut state = stmt.inner.lock().unwrap();
        state
            .bound_params
            .push(Some(char_param(&mut first, &mut first_ind)));
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: std::ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: &mut streamed_ind as *mut SqlLen,
            octet_length_ptr: &mut streamed_ind as *mut SqlLen,
        }));
        state
            .bound_params
            .push(Some(char_param(&mut last, &mut last_ind)));

        let dae = unsafe { build_named_params(&mut state, 3, "test") }.unwrap();
        assert_eq!(dae.params.len(), 3);
        assert_eq!(
            dae.dae_params,
            vec![DaeParam {
                value_ptr: std::ptr::null_mut(),
                expected_len: None,
                needs_transcode: false,
                c_type: SQL_C_CHAR,
                sql_type: SQL_VARCHAR
            }]
        );
    }

    /// `SQL_LEN_DATA_AT_EXEC(n)` promises `n` bytes, which the closing
    /// `SQLParamData` enforces; `SQL_DATA_AT_EXEC` promises nothing.
    #[test]
    fn build_named_params_records_declared_length() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut ind: SqlLen = sql_len_data_at_exec(7);

        let mut state = stmt.inner.lock().unwrap();
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: std::ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: &mut ind as *mut SqlLen,
            octet_length_ptr: &mut ind as *mut SqlLen,
        }));

        let dae = unsafe { build_named_params(&mut state, 1, "test") }.unwrap();
        assert_eq!(
            dae.dae_params,
            vec![DaeParam {
                value_ptr: std::ptr::null_mut(),
                expected_len: Some(7),
                needs_transcode: false,
                c_type: SQL_C_CHAR,
                sql_type: SQL_VARCHAR
            }]
        );
    }

    /// Without an indicator pointer there is nothing to carry a
    /// data-at-execution request, so the binding is an ordinary value even
    /// though its buffer is empty.
    #[test]
    fn build_named_params_without_indicator_is_never_streamed() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut buf: Vec<u8> = b"abc".to_vec();
        let mut state = stmt.inner.lock().unwrap();
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: buf.as_mut_ptr() as *mut c_void,
            buffer_length: buf.len() as SqlLen,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        }));

        let built = unsafe { build_named_params(&mut state, 1, "test") }.unwrap();
        assert_eq!(built.params.len(), 1);
        assert!(built.dae_params.is_empty());
    }

    /// `SQLBindParameter` accepts a data-at-execution indicator on any C type,
    /// so a type `SQLPutData` cannot stream is only caught here, at execute
    /// time.
    #[test]
    fn build_named_params_unstreamable_dae_c_type_posts_hyc00() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut value: i32 = 0;
        let mut ind: SqlLen = SQL_DATA_AT_EXEC;

        let mut state = stmt.inner.lock().unwrap();
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_LONG,
            sql_type: SQL_INTEGER,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: &mut value as *mut i32 as *mut c_void,
            buffer_length: 4,
            strlen_or_ind_ptr: &mut ind as *mut SqlLen,
            octet_length_ptr: &mut ind as *mut SqlLen,
        }));

        let ret = unsafe { build_named_params(&mut state, 1, "test") };
        assert!(ret.is_err());
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_PARAM_C_TYPE_NOT_IMPLEMENTED.state
        );
    }

    /// The bind offset displaces the indicator pointer as well as the value
    /// pointer, so a data-at-execution marker can sit in a slot only the
    /// offset reaches. The offset therefore has to be applied before the
    /// marker is read, not just before the value is converted.
    ///
    /// Applying it later makes the two reads of the indicator disagree: the
    /// check sees row 0 and routes the parameter down the ordinary path, then
    /// the conversion sees row 1's marker and rejects it as unstaged with
    /// `HY000`. Confirmed by reproducing that ordering, which turns the
    /// `HYC00` below into `HY000`.
    ///
    /// Discriminating because row 0 holds an ordinary length: with no offset
    /// this same binding converts cleanly, as its companion test shows.
    #[test]
    fn build_named_params_reads_the_dae_indicator_through_the_bind_offset() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        // Two rows of a parameter array. Row 0 is an ordinary bound value;
        // only row 1 carries the data-at-execution marker.
        let mut values: [i32; 4] = [7; 4];
        let mut inds: [SqlLen; 2] = [4, SQL_DATA_AT_EXEC];
        let mut offset: SqlLen = size_of::<SqlLen>() as SqlLen;

        let mut state = stmt.inner.lock().unwrap();
        state
            .inert_attrs
            .set(SQL_ATTR_PARAM_BIND_OFFSET_PTR, &raw mut offset as SqlULen);
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_LONG,
            sql_type: SQL_INTEGER,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: values.as_mut_ptr() as *mut c_void,
            buffer_length: 4,
            strlen_or_ind_ptr: inds.as_mut_ptr(),
            octet_length_ptr: inds.as_mut_ptr(),
        }));

        // `SQL_C_LONG` is not streamable, so reaching the DAE branch is
        // reported as `HYC00`. That rejection is the observable proof the
        // offset indicator was the one consulted.
        let ret = unsafe { build_named_params(&mut state, 1, "test") };
        assert!(ret.is_err(), "the offset indicator marks this param as DAE");
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_PARAM_C_TYPE_NOT_IMPLEMENTED.state
        );
    }

    #[test]
    fn bind_offset_preserves_a_misaligned_dae_indicator_and_shifted_token() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut value = [0u8; 8];
        let mut indicator_storage: [SqlLen; 2] = [0; 2];
        let shifted_indicator = unsafe {
            indicator_storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(1)
                .cast::<SqlLen>()
        };
        assert_ne!(
            shifted_indicator as usize % std::mem::align_of::<SqlLen>(),
            0,
            "test pointer must be misaligned"
        );
        unsafe { shifted_indicator.write_unaligned(SQL_DATA_AT_EXEC) };
        let mut offset: SqlLen = 1;

        let mut state = stmt.inner.lock().unwrap();
        state
            .inert_attrs
            .set(SQL_ATTR_PARAM_BIND_OFFSET_PTR, &raw mut offset as SqlULen);
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            sql_type: SQL_VARCHAR,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: value.as_mut_ptr().cast(),
            buffer_length: value.len() as SqlLen,
            strlen_or_ind_ptr: indicator_storage.as_mut_ptr(),
            octet_length_ptr: indicator_storage.as_mut_ptr(),
        }));

        let built = unsafe { build_named_params(&mut state, 1, "test") }
            .expect("the shifted DAE marker is streamable");
        assert_eq!(built.dae_params.len(), 1);
        assert_eq!(built.dae_params.first().unwrap().value_ptr, unsafe {
            value.as_mut_ptr().add(1).cast()
        });
    }

    /// The companion to the test above: with no offset set, the same binding
    /// reads row 0's ordinary length and converts normally. Without this, a
    /// driver that treated *every* parameter as data-at-execution would still
    /// pass the offset test.
    #[test]
    fn build_named_params_without_a_bind_offset_reads_the_unshifted_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut values: [i32; 4] = [7; 4];
        let mut inds: [SqlLen; 2] = [4, SQL_DATA_AT_EXEC];

        let mut state = stmt.inner.lock().unwrap();
        state.bound_params.push(Some(BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_LONG,
            sql_type: SQL_INTEGER,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: values.as_mut_ptr() as *mut c_void,
            buffer_length: 4,
            strlen_or_ind_ptr: inds.as_mut_ptr(),
            octet_length_ptr: inds.as_mut_ptr(),
        }));

        let built = unsafe { build_named_params(&mut state, 1, "test") }
            .expect("row 0 is an ordinary length, not a DAE marker");
        assert!(
            built.dae_params.is_empty(),
            "nothing should be staged for streaming"
        );
    }
}
