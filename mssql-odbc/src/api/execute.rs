// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use std::time::Instant;

use mssql_tds::connection::tds_client::{
    ExecuteOptions, PreparedBatchResult, PreparedBatchRowResult, StatementId, StatementResult,
    StreamedParamStatus,
};
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{
    ParamsWithDae, build_named_params, build_named_params_for_row, claim_connection,
    deduct_query_timeout, fail_with_tds, finish_execute, park_dae_client, publish_scalar_processed,
    query_timeout_expired_error, return_client_idle, snapshot_bound_params,
};
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use crate::api::odbc_types::{
    SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR,
    SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_BIND_BY_COLUMN, SQL_ERROR, SQL_INVALID_HANDLE,
    SQL_NO_ROWCOUNT_TOTAL, SQL_PARAM_ERROR, SQL_PARAM_IGNORE, SQL_PARAM_INPUT, SQL_PARAM_PROCEED,
    SQL_PARAM_SUCCESS, SQL_PARAM_SUCCESS_WITH_INFO, SQL_PARAM_UNUSED, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlReturn, SqlULen, SqlUSmallInt,
};
use crate::conversion::param_convert::is_data_at_exec_indicator;
use crate::error::free_errors;
use crate::error::post_sql_error;
use crate::handles::stmt::{
    DaeParam, PreparedPlan, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT,
    STMT_STATE_EXEC_STARTED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Executes the prepared statement on `statement_handle`.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// For each non-data-at-execution parameter, the currently bound value,
/// indicator, and octet-length buffers must remain readable according to the
/// bound C type and lengths. When `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is non-null,
/// these readable extents begin at each bound base plus the pointed-to signed
/// byte offset, which may be negative, so every allocation must cover that
/// displaced range. The offset pointer itself must remain readable for one
/// `SqlLen`.
pub(crate) unsafe fn sql_execute(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLExecute called");
    crate::ffi_entry!("SQLExecute", unsafe { sql_execute_impl(statement_handle) })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// For each non-data-at-execution parameter, the currently bound value,
/// indicator, and octet-length buffers must remain readable according to the
/// bound C type and lengths. When `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is non-null,
/// these readable extents begin at each bound base plus the pointed-to signed
/// byte offset, which may be negative, so every allocation must cover that
/// displaced range. The offset pointer itself must remain readable for one
/// `SqlLen`.
unsafe fn sql_execute_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLExecute: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLExecute: handle is not a STMT"
    );

    sql_execute_safe(statement_handle, stmt)
}

/// Values gathered under the STMT lock before any network I/O.
struct Execution {
    named_params: Vec<RpcParameter>,
    /// The prepared plan moved out of `StmtState` for the execute; written
    /// back afterward (possibly re-prepared with a fresh handle).
    prepared: PreparedPlan,
    /// A prepared statement's still-live handle, superseded by a prior rebind /
    /// re-prepare, dropped by piggyback on this execute.
    orphaned: Option<StatementId>,
    /// `SQL_ATTR_QUERY_TIMEOUT` in effect for this statement, in seconds; `0`
    /// means no timeout.
    query_timeout: u32,
}

/// Values gathered when at least one bound parameter carries a data-at-execution
/// indicator and the statement will be streamed via `begin_execute_prepared`.
struct DaeExecution {
    /// Full parameter list in original order; DAE entries have `data_at_exec()`
    /// set and carry a `None` value.
    params: Vec<RpcParameter>,
    /// The streamed parameters, in original parameter order.
    dae_params: Vec<DaeParam>,
    prepared: PreparedPlan,
    orphaned: Option<StatementId>,
    /// `SQL_ATTR_QUERY_TIMEOUT` in effect for this statement, in seconds; `0`
    /// means no timeout.
    query_timeout: u32,
}

struct BatchExecution {
    bound_params: Vec<Option<crate::params::BoundParam>>,
    active_rows: Vec<usize>,
    marker_count: usize,
    bind_offset: isize,
    param_bind_type: SqlULen,
    outputs: ParamArrayOutputs,
    prepared: PreparedPlan,
    orphaned: Option<StatementId>,
    query_timeout: u32,
}

#[derive(Clone, Copy)]
struct ParamArrayOutputs {
    paramset_size: SqlULen,
    param_status_ptr: *mut SqlUSmallInt,
    params_processed_ptr: *mut SqlULen,
}

enum ExecutionStaging {
    Ready(Execution),
    NeedData(DaeExecution),
    Batch(BatchExecution),
}

struct PreparedRows<'a> {
    bound_params: &'a [Option<crate::params::BoundParam>],
    active_rows: std::slice::Iter<'a, usize>,
    marker_count: usize,
    bind_offset: isize,
    param_bind_type: SqlULen,
    failures: Vec<(usize, DiagMsg)>,
}

impl Iterator for PreparedRows<'_> {
    type Item = Result<(usize, Vec<RpcParameter>), TdsError>;

    fn next(&mut self) -> Option<Self::Item> {
        let row = *self.active_rows.next()?;
        // Unnamed: every row of a batch serializes positionally, which writes a
        // zero-length name and never reads `RpcParameter::name`. The @P{n} names
        // the declaration needs are applied by `execute_prepared_batch`, which
        // renames its own clone of the first row that builds.
        let built = match unsafe {
            build_named_params_for_row(
                self.bound_params,
                self.marker_count,
                self.bind_offset,
                self.param_bind_type,
                row,
                false,
            )
        } {
            Ok(built) => built,
            Err(error) => {
                self.failures.push((row, error.diag()));
                return Some(Err(TdsError::UsageError(format!(
                    "Parameter-array row {} changed after validation: {}",
                    row + 1,
                    error.diag().text
                ))));
            }
        };
        if !built.dae_params.is_empty() {
            self.failures
                .push((row, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED));
            return Some(Err(TdsError::UsageError(format!(
                "Parameter-array row {} changed to data-at-execution after validation",
                row + 1
            ))));
        }
        Some(Ok((row, built.params)))
    }
}

fn sql_execute_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    let staging = match stage_execution(stmt) {
        Ok(s) => s,
        Err(rc) => return rc,
    };

    match staging {
        ExecutionStaging::Ready(Execution {
            named_params,
            mut prepared,
            mut orphaned,
            query_timeout,
        }) => {
            let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecute") {
                Ok(client) => client,
                Err(rc) => {
                    // Staging moved the prepared statement (and any pending orphan) out;
                    // a failed connection claim runs nothing, so put them back so the
                    // statement stays prepared and re-executable. `claim_connection`
                    // already cleared `EXEC_STARTED`.
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return rc;
                }
            };
            let started = Instant::now();

            if let Err(e) =
                begin_transaction_if_manual(dbc, &mut client, "SQLExecute", query_timeout)
            {
                // Nothing ran, so put the staged statement (and any pending orphan)
                // back before reporting, exactly as the failed-claim path does.
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                }
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

            // `query_timeout` (SQL_ATTR_QUERY_TIMEOUT) bounds every wire operation
            // this call makes, not just the final execute — matching msodbcsql's
            // `CheckOptions`, which charges the implicit transaction begin above
            // against the same budget the statement itself gets. The elapsed cost
            // of that begin is deducted before `execute_prepared` runs; an
            // already-exhausted budget fails immediately with HYT00 instead of
            // sending the execute unbounded.
            let query_timeout = match deduct_query_timeout(query_timeout, started.elapsed()) {
                Ok(remaining) => remaining,
                Err(()) => {
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return fail_with_tds(
                        dbc,
                        stmt,
                        statement_handle,
                        client,
                        &query_timeout_expired_error(),
                    );
                }
            };

            // `execute_prepared` owns the whole recovery sequence: reconnect once up
            // front (mirrors msodbcsql `GetBatchCtxOrRecover`), charge it against the
            // command timeout, then reuse the cached handle or transparently re-prepare
            // when it belongs to a superseded session (msodbcsql `FIsReprepareRequired`).
            // A still-live orphaned handle is released by piggyback on the re-prepare.
            //
            // `query_timeout` (already deducted above) bounds the whole call,
            // including any reconnect charged above; `0` means unlimited, matching
            // the ODBC default.
            let exec_result = dbc.runtime.block_on(client.execute_prepared(
                &mut prepared.stmt,
                named_params,
                &mut orphaned,
                ExecuteOptions::new().timeout_secs(query_timeout),
            ));

            // Write the statement back along with any orphan that was not consumed
            // because execution failed before the prepexec send boundary. The fresh
            // handle's RETURNVALUE arrives after the result set and is captured later.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = Some(prepared);
                stmt_state.pending_unprepare = orphaned;
            }

            let stmt_result = match exec_result {
                Ok(result) => result,
                Err(e) => {
                    error!(%e, "SQLExecute: prepared execution failed");
                    return fail_with_tds(dbc, stmt, statement_handle, client, &e);
                }
            };

            // A prepared statement runs a single SQL statement. If it produced no result
            // set (DML / no-row), drain its trailing tokens so the statement is left idle
            // and immediately re-executable (msodbcsql parity) instead of leaving a
            // 0-column cursor open. A row-returning statement keeps its cursor open for
            // SQLFetch; its `@handle` RETURNVALUE (sp_prepexec) is captured later at
            // drain time (SQLCloseCursor / the DDL finish path).
            if !matches!(stmt_result, StatementResult::Rows)
                && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
            {
                error!(%e, "SQLExecute: draining no-row prepared result failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

            finish_execute(dbc, stmt, statement_handle, client, "SQLExecute")
        }

        ExecutionStaging::NeedData(DaeExecution {
            params,
            dae_params,
            mut prepared,
            mut orphaned,
            query_timeout,
        }) => {
            let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecute") {
                Ok(client) => client,
                Err(rc) => {
                    // Same restore-on-failure contract as the non-streaming arm:
                    // nothing ran, so the statement must stay prepared.
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return rc;
                }
            };
            let started = Instant::now();

            if let Err(e) =
                begin_transaction_if_manual(dbc, &mut client, "SQLExecute", query_timeout)
            {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                }
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

            // See the non-streaming arm above: the implicit transaction begin is
            // charged against the same `SQL_ATTR_QUERY_TIMEOUT` budget as the
            // streamed execute that follows.
            let query_timeout = match deduct_query_timeout(query_timeout, started.elapsed()) {
                Ok(remaining) => remaining,
                Err(()) => {
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return fail_with_tds(
                        dbc,
                        stmt,
                        statement_handle,
                        client,
                        &query_timeout_expired_error(),
                    );
                }
            };

            // Data-at-execution keeps the prepared path: `begin_execute_prepared`
            // streams the values into the same `sp_execute` / `sp_prepexec` RPC a
            // materialized execute would have used, so the statement stays
            // prepared and reuses its handle across executes (msodbcsql parity).
            // The orphan is not piggybacked here — the request stays open for the
            // whole SQLPutData sequence and may never reach the server — so it
            // rides along with the parked state and is released by the next
            // execute or by SQLFreeHandle.
            let begin_result = dbc.runtime.block_on(client.begin_execute_prepared(
                &mut prepared.stmt,
                params,
                &mut orphaned,
                ExecuteOptions::new().timeout_secs(query_timeout),
            ));

            match begin_result {
                Ok(StreamedParamStatus::Complete(result)) => {
                    // All params happened to be materialized (shouldn't happen
                    // because staging only produces NeedData when dae_params is
                    // non-empty, but handle it defensively).
                    error!(
                        dae_param_count = dae_params.len(),
                        "SQLExecute: begin_execute_prepared completed despite data-at-execution parameters"
                    );
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    let _ = result; // result handled by finish_execute below
                    finish_execute(dbc, stmt, statement_handle, client, "SQLExecute")
                }
                Ok(StreamedParamStatus::NeedData { .. }) => park_dae_client(
                    stmt,
                    client,
                    Some(prepared),
                    orphaned,
                    dae_params,
                    "SQLExecute",
                ),
                Err(e) => {
                    error!(%e, "SQLExecute: begin_execute_prepared failed");
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    fail_with_tds(dbc, stmt, statement_handle, client, &e)
                }
            }
        }

        ExecutionStaging::Batch(BatchExecution {
            bound_params,
            active_rows,
            marker_count,
            bind_offset,
            param_bind_type,
            outputs,
            mut prepared,
            mut orphaned,
            query_timeout,
        }) => {
            if active_rows.is_empty() {
                unsafe {
                    write_params_processed(outputs.params_processed_ptr, outputs.paramset_size);
                }
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                    // No set ran, so there is no count at all.
                    stmt_state.row_count = SQL_NO_ROWCOUNT_TOTAL;
                    stmt_state.clear_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN);
                    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
                }
                return SQL_SUCCESS;
            }

            let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecute") {
                Ok(client) => client,
                Err(rc) => {
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return rc;
                }
            };
            let started = Instant::now();
            if let Err(e) =
                begin_transaction_if_manual(dbc, &mut client, "SQLExecute", query_timeout)
            {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                }
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }
            let query_timeout = match deduct_query_timeout(query_timeout, started.elapsed()) {
                Ok(remaining) => remaining,
                Err(()) => {
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        stmt_state.prepared = Some(prepared);
                        stmt_state.pending_unprepare = orphaned;
                    }
                    return fail_with_tds(
                        dbc,
                        stmt,
                        statement_handle,
                        client,
                        &query_timeout_expired_error(),
                    );
                }
            };

            let mut rows = PreparedRows {
                bound_params: &bound_params,
                active_rows: active_rows.iter(),
                marker_count,
                bind_offset,
                param_bind_type,
                failures: Vec::new(),
            };
            let batch_result = dbc.runtime.block_on(client.execute_prepared_batch(
                &mut prepared.stmt,
                &mut rows,
                &mut orphaned,
                ExecuteOptions::new().timeout_secs(query_timeout),
            ));
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = Some(prepared);
                stmt_state.pending_unprepare = orphaned;
            }
            // A row that fails to build is skipped by the writer rather than
            // aborting the batch: the request is serialized as it streams, so
            // the sets before the failure are already on the wire. msodbcsql
            // materializes first and sends nothing (measured: 0 rows written),
            // which is the one half of this divergence we keep - matching it
            // would mean a validation pre-pass over every set and giving up the
            // streaming serializer that buys the measured perf parity.
            let failures = std::mem::take(&mut rows.failures);
            let all_rows_failed = failures.len() == active_rows.len();
            if !failures.is_empty() {
                unsafe {
                    for (row, _) in &failures {
                        write_param_status(outputs.param_status_ptr, *row, SQL_PARAM_ERROR);
                    }
                }
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    for (_, diag) in &failures {
                        post_diag(&mut stmt_state, *diag);
                    }
                }
            }
            if all_rows_failed {
                // No set reached the wire, so nothing ran and nothing was
                // written. The status array above already describes each set.
                unsafe {
                    write_params_processed(outputs.params_processed_ptr, outputs.paramset_size);
                }
                return_client_idle(dbc, statement_handle, client);
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                }
                return SQL_ERROR;
            }

            let result = match batch_result {
                Ok(result) => result,
                Err(error) => {
                    error!(%error, "SQLExecute: parameter-array execution failed");
                    return fail_with_tds(dbc, stmt, statement_handle, client, &error);
                }
            };
            finish_parameter_array(
                dbc,
                stmt,
                statement_handle,
                client,
                result,
                outputs,
                failures.len(),
            )
        }
    }
}

/// # Safety
/// When non-null, `status_ptr` must point to an array containing `row`.
unsafe fn write_param_status(status_ptr: *mut SqlUSmallInt, row: usize, status: SqlUSmallInt) {
    if !status_ptr.is_null() {
        unsafe { status_ptr.wrapping_add(row).write_unaligned(status) };
    }
}

/// # Safety
/// When non-null, `processed_ptr` must point to one writable `SqlULen`.
unsafe fn write_params_processed(processed_ptr: *mut SqlULen, processed: SqlULen) {
    if !processed_ptr.is_null() {
        unsafe { processed_ptr.write_unaligned(processed) };
    }
}

/// Reports a finished parameter array.
///
/// `client_side_failures` counts sets that never reached the wire because they
/// could not be built. They are folded in with the sets the server rejected so
/// the return code is decided once, from one rule, rather than by the caller
/// second-guessing this function afterwards.
fn finish_parameter_array(
    dbc: &crate::handles::DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: mssql_tds::connection::tds_client::TdsClient,
    result: PreparedBatchResult,
    outputs: ParamArrayOutputs,
    client_side_failures: usize,
) -> SqlReturn {
    let mut failed_sets = client_side_failures;
    let mut had_info = false;
    let complete = result.complete;
    let total_rows = result
        .total_rows_affected()
        .unwrap_or(SQL_NO_ROWCOUNT_TOTAL);
    let processed = params_processed(complete, outputs.paramset_size, &result.rows);
    let info_messages = client.take_info_messages();

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        return_client_idle(dbc, statement_handle, client);
        return SQL_ERROR;
    };
    for row in result.rows {
        // A set that returned rows still ran: the OUTPUT rows are dropped
        // because one statement handle cannot hold N result sets (AB#47944),
        // but reporting it SQL_PARAM_ERROR would invite a retry that
        // double-inserts.
        let status = if row.has_result_set && row.errors.is_empty() {
            had_info = true;
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_01000,
                0,
                format!(
                    "Parameter-array row {} produced a result set; its rows were discarded",
                    row.row_index + 1
                ),
            );
            SQL_PARAM_SUCCESS_WITH_INFO
        } else if row.errors.is_empty() {
            if row.has_info {
                had_info = true;
                SQL_PARAM_SUCCESS_WITH_INFO
            } else {
                SQL_PARAM_SUCCESS
            }
        } else {
            failed_sets += 1;
            post_tds_error(
                &mut stmt_state,
                &TdsError::from_sql_errors(row.errors),
                SQLSTATE_HY000,
            );
            SQL_PARAM_ERROR
        };
        unsafe {
            write_param_status(outputs.param_status_ptr, row.row_index, status);
        }
    }
    if !complete {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_01000,
            0,
            format!(
                "Batched RPC reported through parameter set {processed} of {}; later sets kept SQL_PARAM_UNUSED",
                outputs.paramset_size
            ),
        );
    }
    post_tds_info_messages(&mut stmt_state, &info_messages);
    stmt_state.row_count = total_rows;
    stmt_state.clear_exhaustion_state();
    stmt_state.set_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.clear_state(STMT_STATE_CURSOR_OPEN | STMT_STATE_EXEC_STARTED);
    drop(stmt_state);

    unsafe {
        write_params_processed(outputs.params_processed_ptr, processed);
    }
    return_client_idle(dbc, statement_handle, client);

    // One rule, whether the set failed client-side before it reached the wire or
    // server-side after: a status array can carry the per-set detail, so the
    // call softens to SQL_SUCCESS_WITH_INFO; without one the caller cannot see
    // which set failed, so it stays SQL_ERROR (msodbcsql, sqlctokn.cpp:2341-2360).
    //
    // There is deliberately no all-failed special case here. Measured on
    // msodbcsql 18.6.2.1, a 3-set batch where every set violates a CHECK
    // constraint returns SQL_SUCCESS_WITH_INFO with a status array bound, not
    // SQL_ERROR - its downgrade has no such branch. Total *client-side* failure
    // is still SQL_ERROR, but that is decided by the caller, because nothing
    // reached the wire and no set ran.
    // A short batch is reported rather than rejected, so the sets the server did
    // report keep their statuses and count. msodbcsql returns SQL_SUCCESS here -
    // it never compares the reported count against SQL_ATTR_PARAMSET_SIZE
    // (sqlctokn.cpp OnDone) - and leaves the unreported statuses untouched
    // because it has no SQL_PARAM_UNUSED pre-fill. AB#47945.
    parameter_array_return_code(
        failed_sets,
        complete,
        had_info || !info_messages.is_empty(),
        !outputs.param_status_ptr.is_null(),
    )
}

/// `SQL_ATTR_PARAMS_PROCESSED_PTR` for a finished batch. ODBC counts the sets
/// the call reached, not the sets the server spoke about, so an ignored set
/// still advances it and a short batch stops at the last set reported.
fn params_processed(
    complete: bool,
    paramset_size: SqlULen,
    rows: &[PreparedBatchRowResult],
) -> SqlULen {
    if complete {
        paramset_size
    } else {
        rows.last().map_or(0, |row| row.row_index.saturating_add(1))
    }
}

/// The one rule deciding a parameter array's return code. Split out because
/// [`finish_parameter_array`] needs a live `TdsClient` and cannot be unit
/// tested; every branch below is reachable only from here.
fn parameter_array_return_code(
    failed_sets: usize,
    complete: bool,
    had_diagnostic: bool,
    has_status_array: bool,
) -> SqlReturn {
    if failed_sets > 0 {
        if has_status_array {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_ERROR
        }
    } else if !complete || had_diagnostic {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// Validates statement state and builds the parameter list under the STMT lock,
/// setting `EXEC_STARTED` on success. Application value buffers are read here by
/// reference (no network I/O).
fn stage_execution(stmt: &StmtHandle) -> Result<ExecutionStaging, SqlReturn> {
    // Snapshotted before the STMT lock below is taken — this crate never
    // holds a STMT lock while acquiring a DESC lock (see bind_col.rs's
    // rationale). Not applied to `stmt_state.bound_params` until every
    // early-return check below has passed: a statement already mid-DAE-
    // sequence must keep that sequence's own frozen snapshot if this call
    // turns out to be a rejected re-entry rather than a real new execute.
    //
    // A snapshot failure (poisoned mutex, or an explicit APD freed out from
    // under a concurrent reassociation) must still post a diagnostic —
    // mirroring `SQLExecDirectW`'s handling of the same failure — rather
    // than leave `SQLGetDiagRec` reporting `SQL_NO_DATA` or a stale record
    // from a previous call.
    let bound_params = match snapshot_bound_params(stmt) {
        Ok(params) => params,
        Err(rc) => {
            error!("SQLExecute: failed to snapshot parameter bindings");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                free_errors(&mut stmt_state);
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error reading parameter bindings",
                );
            }
            return Err(rc);
        }
    };

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLExecute: stmt mutex poisoned");
        return Err(SQL_ERROR);
    };
    free_errors(&mut stmt_state);

    // A statement awaiting data-at-execution input is in the ODBC "Need Data"
    // state, where every function other than SQLPutData/SQLParamData/SQLCancel
    // and the diagnostic calls is a sequence error rather than a cursor error.
    //
    // Checked before the prepared-plan guard below: parking a DAE sequence
    // moves the plan into `DaeState`, so a statement in Need Data has
    // `prepared == None` and would otherwise be reported as never prepared.
    if stmt_state.needs_data() {
        error!("SQLExecute: statement is awaiting data-at-execution input");
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        return Err(SQL_ERROR);
    }

    // SQLExecute on an unprepared statement is HY010 — a DM-enforced
    // precondition (the spec marks it "(DM)"), so assert rather than post.
    // The release-path fallback still returns SQL_ERROR since we have no SQL
    // to run, but it can't be reached through a conforming Driver Manager.
    debug_assert!(
        stmt_state.prepared.is_some(),
        "SQLExecute: statement not prepared — DM should have rejected this"
    );
    if stmt_state.prepared.is_none() {
        error!("SQLExecute: statement has not been prepared");
        return Err(SQL_ERROR);
    }

    if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
        error!("SQLExecute: statement has an active execute or open cursor");
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return Err(SQL_ERROR);
    }

    let marker_count = stmt_state
        .prepared
        .as_ref()
        .expect("prepared checked non-None above")
        .marker_count;

    // All state-sequencing checks passed: this is a real new execute, so the
    // fresh snapshot now becomes the one `build_named_params` and any DAE
    // sequence it opens will read for the rest of this execute.
    stmt_state.bound_params = bound_params;

    if stmt_state.paramset_size > 1 {
        let paramset_size = stmt_state.paramset_size;
        let row_count = paramset_size;
        let bind_offset = unsafe { stmt_state.inert_attrs.param_bind_offset() };
        let param_bind_type = stmt_state
            .inert_attrs
            .get(SQL_ATTR_PARAM_BIND_TYPE)
            .unwrap_or(SQL_BIND_BY_COLUMN);
        let operation_ptr = stmt_state
            .inert_attrs
            .get(SQL_ATTR_PARAM_OPERATION_PTR)
            .unwrap_or(0) as *const SqlUSmallInt;
        let param_status_ptr = stmt_state
            .inert_attrs
            .get(SQL_ATTR_PARAM_STATUS_PTR)
            .unwrap_or(0) as *mut SqlUSmallInt;
        let params_processed_ptr = stmt_state
            .inert_attrs
            .get(SQL_ATTR_PARAMS_PROCESSED_PTR)
            .unwrap_or(0) as *mut SqlULen;

        for parameter in 0..marker_count {
            let Some(Some(bound)) = stmt_state.bound_params.get(parameter) else {
                post_diag(&mut stmt_state, ERR_UNBOUND_PARAMETER);
                return Err(SQL_ERROR);
            };
            if bound.input_output_type != SQL_PARAM_INPUT {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HYC00,
                    0,
                    "Parameter arrays currently support input-only parameters",
                );
                return Err(SQL_ERROR);
            }
        }

        // Reserved before anything is written: `PARAMSET_SIZE` is any non-zero
        // SQLULEN, and an infallible `with_capacity` on an absurd one aborts
        // the host process instead of failing the call. Checked ahead of the
        // prefill so a size that cannot be serviced never walks the caller's
        // status array either.
        let mut active_rows = Vec::new();
        if active_rows.try_reserve(row_count).is_err() {
            error!("SQLExecute: failed to reserve {row_count} parameter sets (HY001)");
            post_diag(&mut stmt_state, ERR_MEMORY_ALLOCATION);
            return Err(SQL_ERROR);
        }

        unsafe {
            write_params_processed(params_processed_ptr, 0);
            if !param_status_ptr.is_null() {
                for row in 0..row_count {
                    write_param_status(param_status_ptr, row, SQL_PARAM_UNUSED);
                }
            }
        }

        for row in 0..row_count {
            let operation = if operation_ptr.is_null() {
                SQL_PARAM_PROCEED
            } else {
                unsafe { operation_ptr.wrapping_add(row).read_unaligned() }
            };
            // Only SQL_PARAM_IGNORE skips a set; every other value proceeds,
            // matching msodbcsql (sqlccmd.cpp:3218, :6605, sqlctokn.cpp:2396).
            if operation == SQL_PARAM_IGNORE {
                continue;
            }

            for parameter in 0..marker_count {
                let bound = stmt_state
                    .bound_params
                    .get(parameter)
                    .and_then(Option::as_ref)
                    .copied()
                    .ok_or_else(|| {
                        post_diag(&mut stmt_state, ERR_UNBOUND_PARAMETER);
                        SQL_ERROR
                    })?;
                let positioned = match bound.for_row(row, bind_offset, param_bind_type) {
                    Ok(positioned) => positioned,
                    Err(_) => {
                        unsafe {
                            write_param_status(param_status_ptr, row, SQL_PARAM_ERROR);
                            write_params_processed(params_processed_ptr, row + 1);
                        }
                        post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
                        return Err(SQL_ERROR);
                    }
                };
                let dae = !positioned.octet_length_ptr.is_null()
                    && is_data_at_exec_indicator(unsafe {
                        positioned.octet_length_ptr.read_unaligned()
                    });
                if dae {
                    unsafe {
                        write_param_status(param_status_ptr, row, SQL_PARAM_ERROR);
                        write_params_processed(params_processed_ptr, row + 1);
                    }
                    post_sql_error(
                        &mut stmt_state,
                        SQLSTATE_HYC00,
                        0,
                        "Data-at-execution parameters are not supported in parameter arrays",
                    );
                    return Err(SQL_ERROR);
                }
            }
            active_rows.push(row);
        }

        let Some(prepared) = stmt_state.prepared.take() else {
            error!("SQLExecute: prepared plan disappeared during array staging");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return Err(SQL_ERROR);
        };
        let orphaned = stmt_state.pending_unprepare.take();
        let query_timeout = stmt_state.query_timeout;
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.clear_result_metadata();
        stmt_state.reset_row_stream();
        stmt_state.row_count = SQL_NO_ROWCOUNT_TOTAL;
        stmt_state.pending_row_counts.clear();
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
        return Ok(ExecutionStaging::Batch(BatchExecution {
            bound_params: std::mem::take(&mut stmt_state.bound_params),
            active_rows,
            marker_count,
            bind_offset,
            param_bind_type,
            outputs: ParamArrayOutputs {
                paramset_size,
                param_status_ptr,
                params_processed_ptr,
            },
            prepared,
            orphaned,
            query_timeout,
        }));
    }

    // Scan for data-at-execution parameters.  If any are present, use the
    // streaming path; otherwise, go through the normal prepared-execute path.
    publish_scalar_processed(&stmt_state);
    let ParamsWithDae { params, dae_params } =
        unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecute") }?;

    // All fallible validation passed: move the prepared plan out (written
    // back after the execute) and take any orphaned handle for piggyback drop.
    let prepared = stmt_state
        .prepared
        .take()
        .expect("prepared checked non-None above");
    let orphaned = stmt_state.pending_unprepare.take();
    let query_timeout = stmt_state.query_timeout;
    stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.clear_result_metadata();
    stmt_state.reset_row_stream();
    stmt_state.row_count = SQL_NO_ROWCOUNT_TOTAL;
    stmt_state.pending_row_counts.clear();
    stmt_state.set_state(STMT_STATE_EXEC_STARTED);

    if dae_params.is_empty() {
        Ok(ExecutionStaging::Ready(Execution {
            named_params: params,
            prepared,
            orphaned,
            query_timeout,
        }))
    } else {
        Ok(ExecutionStaging::NeedData(DaeExecution {
            params,
            dae_params,
            prepared,
            orphaned,
            query_timeout,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::bind_param::sql_bind_parameter;
    use crate::api::odbc_types::{
        SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR,
        SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_BIND_BY_COLUMN, SQL_C_CHAR, SQL_C_SLONG,
        SQL_DATA_AT_EXEC, SQL_INTEGER, SQL_NULL_HANDLE, SQL_PARAM_ERROR, SQL_PARAM_IGNORE,
        SQL_PARAM_INPUT, SQL_PARAM_OUTPUT, SQL_PARAM_PROCEED, SQL_PARAM_UNUSED, SQL_SUCCESS,
        SQL_VARCHAR, SqlLen, SqlULen,
    };
    use crate::api::util::rewrite_param_markers;
    use crate::handles::DescHandle;
    use crate::test_support::TestHandles;
    use mssql_tds::connection::tds_client::{PreparedStatement, StatementId};

    fn set_prepared(stmt_raw: SqlHandle, sql: &str) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_raw) };
        let (rewritten, marker_count) = rewrite_param_markers(sql);
        let mut state = stmt.inner.lock().unwrap();
        state.prepared = Some(PreparedPlan {
            stmt: PreparedStatement::new(rewritten),
            marker_count,
        });
    }

    /// Panics while holding the APD lock, leaving the mutex poisoned —
    /// mirrors `bind_param.rs`'s own `poison_apd` test helper.
    fn poison_apd(apd: SqlHandle) {
        let handle = unsafe { handle_from_raw::<DescHandle>(apd) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.inner.lock().unwrap();
            panic!("poison the apd lock");
        }));
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_execute(SQL_NULL_HANDLE) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn unbound_parameter_marker_returns_07002() {
        let h = TestHandles::with_env_dbc_stmt();
        // Prepared SQL has one marker but no parameter is bound.
        set_prepared(h.stmt, "SELECT * FROM t WHERE id = ?");
        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07002);
        // EXEC_STARTED must not leak on this pre-I/O failure.
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    /// A `snapshot_bound_params` failure (here, a poisoned APD) must still
    /// post an HY000 diagnostic, and post it as record 1 — not leave
    /// `SQLGetDiagRec` reporting `SQL_NO_DATA`, and not append after a stale
    /// record a previous call left behind (`free_errors` must run first).
    #[test]
    fn snapshot_failure_posts_hy000_as_the_first_diagnostic_record() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .diag_records
            .push(crate::error::DiagRecord::new(SQLSTATE_07002, 0, "stale"));
        poison_apd(h.apd());

        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1, "stale record must be cleared");
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY000);
        assert!(
            state.diag_records[0]
                .message
                .contains("Internal error reading parameter bindings")
        );
    }

    #[test]
    fn prepared_but_disconnected_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        // No parameter markers, so gathering succeeds and we reach the
        // connection claim, which fails because the DBC is not connected.
        set_prepared(h.stmt, "SELECT 1");
        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_CONNECTION_DOES_NOT_EXIST.state
        );
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn open_cursor_returns_invalid_cursor_state() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);
        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_INVALID_CURSOR_STATE.state
        );
        // The pre-I/O guard must not set EXEC_STARTED.
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn execute_while_awaiting_data_returns_function_sequence_error() {
        // In the Need Data state the spec requires HY010, not the 24000 that a
        // merely-busy statement gets.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            // Exactly what `park_dae_client` leaves behind: the prepared plan
            // moves into `DaeState`, so the statement is Need Data *and*
            // `prepared == None`.
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_STARTED);
            state.dae = Some(crate::handles::stmt::DaeState::for_test(Vec::new(), None));
            assert!(state.prepared.is_none());
        }
        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
    }

    #[test]
    fn data_at_exec_disconnected_returns_connection_error() {
        // A DAE parameter is now supported: staging succeeds (produces
        // NeedData staging), connection is claimed, but the DBC is
        // disconnected so claim_connection fails with 08003.
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT ?");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let bind_ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(bind_ret, SQL_SUCCESS);

        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        // Connection is not connected → 08003, not HYC00.
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_CONNECTION_DOES_NOT_EXIST.state
        );
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
        // The prepared plan must be restored so SQLExecute remains retryable.
        assert!(state.prepared.is_some());
    }

    #[test]
    fn stage_execution_moves_prepared_out_and_threads_orphaned_handle() {
        // A handle orphaned by a prior rebind / re-prepare lives in
        // `pending_unprepare`. Staging must move the prepared statement out and
        // hand the orphan to `orphaned` for a piggyback drop, consuming it so it
        // can't be released twice.
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let orphan = StatementId::from_raw_for_test(42);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().pending_unprepare = Some(orphan);

        let staging = stage_execution(stmt).expect("staging should succeed");
        let (exec_prepared_sql, exec_orphaned) = match staging {
            ExecutionStaging::Ready(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
            ExecutionStaging::NeedData(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
            ExecutionStaging::Batch(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
        };
        assert_eq!(exec_orphaned, Some(orphan));
        assert_eq!(exec_prepared_sql, "SELECT 1");

        let state = stmt.inner.lock().unwrap();
        assert!(state.prepared.is_none(), "prepared moved out of state");
        assert!(state.pending_unprepare.is_none(), "orphan consumed");
        assert!(state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn stage_execution_without_pending_has_no_orphaned_handle() {
        // Nothing pending: staging threads no orphan, so the execute won't
        // piggyback a drop.
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let staging = stage_execution(stmt).expect("staging should succeed");
        let (exec_prepared_sql, exec_orphaned) = match staging {
            ExecutionStaging::Ready(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
            ExecutionStaging::NeedData(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
            ExecutionStaging::Batch(e) => (e.prepared.stmt.sql().to_string(), e.orphaned),
        };
        assert_eq!(exec_orphaned, None);
        assert_eq!(exec_prepared_sql, "SELECT 1");
        assert!(stmt.inner.lock().unwrap().prepared.is_none());
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` (`StmtState::query_timeout`) must be captured
    /// during staging so the execute call can bound the wait for a response —
    /// see mssql-rs#439: a statement blocked server-side has no client-side
    /// escape hatch when the timeout is silently dropped on the floor.
    #[test]
    fn stage_execution_captures_configured_query_timeout() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = 42;

        let staging = stage_execution(stmt).expect("staging should succeed");
        let query_timeout = match staging {
            ExecutionStaging::Ready(e) => e.query_timeout,
            ExecutionStaging::NeedData(e) => e.query_timeout,
            ExecutionStaging::Batch(e) => e.query_timeout,
        };
        assert_eq!(query_timeout, 42);
    }

    /// The ODBC default (`0`, "no timeout") must still stage as `0`, which
    /// `ExecuteOptions::timeout_secs` treats as unlimited — the common case
    /// must stay behaviorally unchanged by wiring the timeout through.
    #[test]
    fn stage_execution_default_query_timeout_is_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let staging = stage_execution(stmt).expect("staging should succeed");
        let query_timeout = match staging {
            ExecutionStaging::Ready(e) => e.query_timeout,
            ExecutionStaging::NeedData(e) => e.query_timeout,
            ExecutionStaging::Batch(e) => e.query_timeout,
        };
        assert_eq!(query_timeout, 0);
    }

    #[test]
    fn stage_parameter_array_preserves_original_indices_and_ignore_rows() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let mut values = [10i32, 20, 30];
        let mut indicators = [size_of::<i32>() as SqlLen; 3];
        let mut operations = [SQL_PARAM_PROCEED, SQL_PARAM_IGNORE, SQL_PARAM_PROCEED];
        let mut statuses = [99 as SqlUSmallInt; 3];
        let mut processed: SqlULen = 99;
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    0,
                    0,
                    values.as_mut_ptr().cast(),
                    size_of::<i32>() as SqlLen,
                    indicators.as_mut_ptr(),
                )
            },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.paramset_size = 3;
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_BIND_TYPE, SQL_BIND_BY_COLUMN);
            state.inert_attrs.set(
                SQL_ATTR_PARAM_OPERATION_PTR,
                operations.as_mut_ptr() as SqlULen,
            );
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr() as SqlULen);
            state.inert_attrs.set(
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut processed) as SqlULen,
            );
        }

        let ExecutionStaging::Batch(batch) =
            stage_execution(stmt).expect("array staging should succeed")
        else {
            panic!("expected array staging");
        };
        assert_eq!(batch.active_rows, vec![0, 2]);
        assert_eq!(statuses, [SQL_PARAM_UNUSED; 3]);
        assert_eq!(processed, 0);

        let rows = PreparedRows {
            bound_params: &batch.bound_params,
            active_rows: batch.active_rows.iter(),
            marker_count: batch.marker_count,
            bind_offset: batch.bind_offset,
            param_bind_type: batch.param_bind_type,
            failures: Vec::new(),
        }
        .collect::<Result<Vec<_>, _>>()
        .expect("validated parameter rows should build again");
        assert_eq!(
            rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    /// Stages four good rows, then invalidates some the way an application can
    /// between validation and execution - the only way a row still fails once
    /// the batch is under way.
    fn stage_three_rows(
        h: &TestHandles,
        values: &mut [i32; 4],
        indicators: &mut [SqlLen; 4],
        statuses: &mut [SqlUSmallInt; 4],
        processed: &mut SqlULen,
    ) -> BatchExecution {
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    0,
                    0,
                    values.as_mut_ptr().cast(),
                    size_of::<i32>() as SqlLen,
                    indicators.as_mut_ptr(),
                )
            },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.paramset_size = 4;
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_BIND_TYPE, SQL_BIND_BY_COLUMN);
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr() as SqlULen);
            state.inert_attrs.set(
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut *processed) as SqlULen,
            );
        }
        let ExecutionStaging::Batch(batch) =
            stage_execution(stmt).expect("array staging should succeed")
        else {
            panic!("expected array staging");
        };
        batch
    }

    #[test]
    fn prepared_rows_skips_a_failing_row_and_keeps_building_the_rest() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut values = [10i32, 20, 30, 40];
        let mut indicators = [size_of::<i32>() as SqlLen; 4];
        let mut statuses = [99 as SqlUSmallInt; 4];
        let mut processed: SqlULen = 99;
        let batch = stage_three_rows(
            &h,
            &mut values,
            &mut indicators,
            &mut statuses,
            &mut processed,
        );

        // Written through the pointer the binding captured, which is how the
        // driver reads it back.
        unsafe { indicators.as_mut_ptr().add(1).write(SQL_DATA_AT_EXEC) };
        unsafe { indicators.as_mut_ptr().add(3).write(SQL_DATA_AT_EXEC) };

        let mut rows = PreparedRows {
            bound_params: &batch.bound_params,
            active_rows: batch.active_rows.iter(),
            marker_count: batch.marker_count,
            bind_offset: batch.bind_offset,
            param_bind_type: batch.param_bind_type,
            failures: Vec::new(),
        };
        let built = rows.by_ref().collect::<Vec<_>>();

        assert_eq!(built.len(), 4);
        assert!(built[0].is_ok());
        assert!(built[1].is_err());
        assert!(
            built[2].is_ok(),
            "a row that fails to build must not stop the rows after it"
        );
        assert!(built[3].is_err());
        assert_eq!(
            rows.failures
                .iter()
                .map(|(row, _)| *row)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "every failing row has to be reportable, not just one of them"
        );
    }

    #[test]
    fn stage_parameter_array_rejects_data_at_execution_before_io() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let mut token = 0u8;
        let mut indicators = [SQL_DATA_AT_EXEC; 2];
        let mut statuses = [SQL_PARAM_UNUSED; 2];
        let mut processed: SqlULen = 0;
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_CHAR,
                    SQL_VARCHAR,
                    0,
                    0,
                    (&raw mut token).cast(),
                    0,
                    indicators.as_mut_ptr(),
                )
            },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.paramset_size = 2;
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr() as SqlULen);
            state.inert_attrs.set(
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut processed) as SqlULen,
            );
        }

        assert!(stage_execution(stmt).is_err());
        assert_eq!(statuses[0], SQL_PARAM_ERROR);
        assert_eq!(statuses[1], SQL_PARAM_UNUSED);
        assert_eq!(processed, 1);
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_HYC00
        );
    }

    /// Output parameters are refused by `SQLBindParameter` itself, so nothing
    /// non-input ever reaches `bound_params` and the array path's own
    /// input-only guard is unreachable. Pins that, so the guard is not mistaken
    /// for an array-specific deviation (AB#47945).
    #[test]
    fn output_parameters_are_refused_before_the_array_path_sees_them() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let mut values = [1i32, 2];
        let mut indicators = [size_of::<i32>() as SqlLen; 2];
        let rc = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_OUTPUT,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                0,
                values.as_mut_ptr().cast(),
                size_of::<i32>() as SqlLen,
                indicators.as_mut_ptr(),
            )
        };

        assert_eq!(rc, SQL_ERROR, "binding an output parameter must fail");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_HYC00
        );
        stmt.inner.lock().unwrap().paramset_size = 2;
        // The marker is still unbound, so staging stops at 07002 - it never
        // reaches the input-only check.
        assert!(stage_execution(stmt).is_err());
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_07002
        );
    }

    /// Only `SQL_PARAM_IGNORE` skips a set. msodbcsql tests for that one value
    /// and lets every other bit pattern proceed (`sqlccmd.cpp:3218`); measured
    /// against msodbcsql 18, an operation value of 7 inserts all three sets.
    #[test]
    fn stage_parameter_array_proceeds_on_an_unknown_operation_value() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let mut values = [1i32, 2, 3];
        let mut indicators = [size_of::<i32>() as SqlLen; 3];
        let mut operations = [SQL_PARAM_PROCEED, 42 as SqlUSmallInt, SQL_PARAM_PROCEED];
        let mut statuses = [0xFFFF as SqlUSmallInt; 3];
        let mut processed: SqlULen = 0xDEAD;
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    0,
                    0,
                    values.as_mut_ptr().cast(),
                    size_of::<i32>() as SqlLen,
                    indicators.as_mut_ptr(),
                )
            },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.paramset_size = 3;
            state.inert_attrs.set(
                SQL_ATTR_PARAM_OPERATION_PTR,
                operations.as_mut_ptr() as SqlULen,
            );
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr() as SqlULen);
            state.inert_attrs.set(
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut processed) as SqlULen,
            );
        }

        let ExecutionStaging::Batch(batch) =
            stage_execution(stmt).expect("an unknown operation value must not fail")
        else {
            panic!("expected array staging");
        };
        assert_eq!(
            batch.active_rows,
            vec![0, 1, 2],
            "7 is not SQL_PARAM_IGNORE, so every set runs"
        );
        assert_eq!(
            statuses, [SQL_PARAM_UNUSED; 3],
            "staging prefills the status array and must not mark the set in error"
        );
        assert_eq!(processed, 0, "nothing has run yet");
        assert!(stmt.inner.lock().unwrap().diag_records.is_empty());
    }

    /// The unknown-value acceptance above sits in the same loop as the
    /// `SQL_PARAM_IGNORE` skip. Pins that only the validation was removed and
    /// the skip still applies when both appear in one operation array.
    #[test]
    fn stage_parameter_array_still_skips_ignore_beside_an_unknown_value() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let mut values = [1i32, 2, 3];
        let mut indicators = [size_of::<i32>() as SqlLen; 3];
        let mut operations = [SQL_PARAM_IGNORE, 7 as SqlUSmallInt, SQL_PARAM_PROCEED];
        let mut statuses = [0xFFFF as SqlUSmallInt; 3];
        let mut processed: SqlULen = 0xDEAD;
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    0,
                    0,
                    values.as_mut_ptr().cast(),
                    size_of::<i32>() as SqlLen,
                    indicators.as_mut_ptr(),
                )
            },
            SQL_SUCCESS
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.paramset_size = 3;
            state.inert_attrs.set(
                SQL_ATTR_PARAM_OPERATION_PTR,
                operations.as_mut_ptr() as SqlULen,
            );
            state
                .inert_attrs
                .set(SQL_ATTR_PARAM_STATUS_PTR, statuses.as_mut_ptr() as SqlULen);
            state.inert_attrs.set(
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                (&raw mut processed) as SqlULen,
            );
        }

        let ExecutionStaging::Batch(batch) =
            stage_execution(stmt).expect("array staging should succeed")
        else {
            panic!("expected array staging");
        };
        assert_eq!(
            batch.active_rows,
            vec![1, 2],
            "only SQL_PARAM_IGNORE skips; the unknown value runs"
        );
        assert_eq!(
            statuses, [SQL_PARAM_UNUSED; 3],
            "staging prefills the status array and marks nothing in error"
        );
        assert_eq!(processed, 0, "nothing has run yet");
        assert!(stmt.inner.lock().unwrap().diag_records.is_empty());
    }

    /// A marker with no binding is 07002 on the array path too, and must be
    /// caught before any set is sent.
    #[test]
    fn stage_parameter_array_rejects_an_unbound_marker() {
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().paramset_size = 2;

        assert!(stage_execution(stmt).is_err());
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07002);
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    fn reported_row(row_index: usize) -> PreparedBatchRowResult {
        PreparedBatchRowResult {
            row_index,
            rows_affected: Some(1),
            errors: Vec::new(),
            has_result_set: false,
            has_info: false,
        }
    }

    /// A complete batch reports the whole array regardless of what the server
    /// said per set, and a short one stops at the last set reported.
    #[test]
    fn params_processed_counts_through_the_last_reported_set() {
        assert_eq!(params_processed(true, 5, &[reported_row(0)]), 5);
        assert_eq!(
            params_processed(false, 5, &[reported_row(0), reported_row(1)]),
            2
        );
        assert_eq!(params_processed(false, 5, &[]), 0);
    }

    /// Sets skipped by `SQL_PARAM_IGNORE` never reach the wire, so they are
    /// absent from `rows` while still counting towards the processed total.
    #[test]
    fn params_processed_counts_ignored_sets_it_never_saw() {
        // PARAMSET_SIZE 5, sets 0 and 1 ignored, only set 2 reported.
        assert_eq!(
            params_processed(false, 5, &[reported_row(2)]),
            3,
            "runs through set index 2, rather than counting the one set reported"
        );
    }

    /// The whole return-code rule, including the short-batch arm that no E2E
    /// test can reach: no server behaviour is known to shorten a batch without
    /// an error explaining it.
    #[test]
    fn parameter_array_return_code_covers_every_arm() {
        assert_eq!(
            parameter_array_return_code(0, true, false, true),
            SQL_SUCCESS,
            "clean batch"
        );
        assert_eq!(
            parameter_array_return_code(0, true, true, true),
            SQL_SUCCESS_WITH_INFO,
            "a diagnostic alone downgrades"
        );
        assert_eq!(
            parameter_array_return_code(0, false, false, true),
            SQL_SUCCESS_WITH_INFO,
            "a short batch downgrades even with no failure and no diagnostic"
        );
        assert_eq!(
            parameter_array_return_code(0, false, false, false),
            SQL_SUCCESS_WITH_INFO,
            "a short batch does not need a status array to downgrade"
        );
        assert_eq!(
            parameter_array_return_code(1, true, false, true),
            SQL_SUCCESS_WITH_INFO,
            "a status array carries the per-set detail"
        );
        assert_eq!(
            parameter_array_return_code(1, true, false, false),
            SQL_ERROR,
            "without one the caller cannot see which set failed"
        );
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` must actually bound the wait for a response,
    /// not just reach `ExecuteOptions` — see mssql-rs#439, where the timeout
    /// was silently dropped on the floor instead of bounding a statement
    /// blocked server-side (e.g. behind another session's row lock).
    ///
    /// Drives the real `SQLExecute` code path (`stage_execution`,
    /// `begin_transaction_if_manual`, the elapsed-time deduction, and
    /// `execute_prepared`'s `sp_prepexec` RPC) against a real `TdsClient`
    /// connected to a mock TDS server that holds its response for
    /// `RESPONSE_DELAY` — far longer than the statement's configured timeout.
    /// Reverting the timeout wiring back to `ExecuteOptions::default()` would
    /// make this test take the full `RESPONSE_DELAY` and return
    /// `SQL_SUCCESS`/`1222` instead of the prompt `HYT00` asserted here, so it
    /// fails if the plumbing regresses.
    #[test]
    fn execute_query_timeout_bounds_a_longer_server_delay() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::{QueryResponse, TerminalError};
        use std::time::{Duration, Instant};

        const RESPONSE_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        // Comfortably above STMT_TIMEOUT_SECS plus connection/RTT overhead,
        // comfortably below RESPONSE_DELAY — the gap is what proves the
        // statement timeout, not the server delay, ended the wait.
        const BOUND: Duration = Duration::from_secs(5);
        // All-uppercase: `get_by_contained_utf16_text` compares against the
        // registry's case-insensitive (upper-cased) key.
        const SELECT_SQL: &str = "SELECT * FROM T WHERE ID = 1";

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let _mock_server = crate::test_support::connect_mock_server(
            dbc,
            SELECT_SQL,
            QueryResponse::error_only(TerminalError::new(
                1222,
                16,
                "Lock request time out period exceeded.",
            ))
            .with_delay(RESPONSE_DELAY),
        );

        set_prepared(h.stmt, SELECT_SQL);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = unsafe { sql_execute(h.stmt) };
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLExecute took {elapsed:?} — a {STMT_TIMEOUT_SECS}s SQL_ATTR_QUERY_TIMEOUT must \
             bound the wait well below the server's {RESPONSE_DELAY:?} delay"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "a query-timeout expiry must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
    }

    #[test]
    fn failed_connection_claim_restores_prepared_statement() {
        // Staging moves the prepared statement (and any pending orphan) out
        // before the connection is claimed. When the claim fails (here: the DBC
        // is not connected) the statement must be restored so a retried
        // SQLExecute still sees it as prepared and re-executable, rather than
        // silently unprepared.
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT 1");
        let orphan = StatementId::from_raw_for_test(42);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().pending_unprepare = Some(orphan);

        let ret = unsafe { sql_execute(h.stmt) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_CONNECTION_DOES_NOT_EXIST.state
        );
        assert_eq!(
            state.prepared.as_ref().map(|p| p.stmt.sql()),
            Some("SELECT 1"),
            "the prepared statement must be restored after a failed connection claim"
        );
        assert_eq!(
            state.pending_unprepare,
            Some(orphan),
            "the pending orphan must be restored so its drop is not lost"
        );
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn dae_param_staging_produces_need_data_variant() {
        // A bound parameter with SQL_DATA_AT_EXEC indicator must produce
        // NeedData staging, not the Ready variant.
        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "INSERT INTO t VALUES (?)");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let bind_ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(bind_ret, SQL_SUCCESS);

        let staging = stage_execution(stmt).expect("staging should succeed");
        match staging {
            ExecutionStaging::NeedData(dae) => {
                // The single param is DAE: its index is in dae_indices.
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
                assert_eq!(dae.params.len(), 1, "one param in list");
            }
            ExecutionStaging::Ready(_) | ExecutionStaging::Batch(_) => {
                panic!("expected NeedData staging for DAE param")
            }
        }
    }
}
