// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use std::time::Instant;

use mssql_tds::connection::tds_client::{
    ExecuteOptions, StatementId, StatementResult, StreamedParamStatus,
};
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{
    ParamsWithDae, build_named_params, build_named_params_row, claim_connection,
    deduct_query_timeout, fail_with_tds, finish_execute, park_dae_client,
    query_timeout_expired_error, snapshot_bound_params,
};
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use crate::api::close_cursor::sql_free_stmt_close;
use crate::api::odbc_types::{
    SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR,
    SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NEED_DATA,
    SQL_PARAM_BIND_BY_COLUMN, SQL_PARAM_ERROR, SQL_PARAM_IGNORE, SQL_PARAM_SUCCESS,
    SQL_PARAM_SUCCESS_WITH_INFO, SQL_PARAM_UNUSED, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlReturn, SqlULen, SqlUSmallInt,
};
use crate::api::util::write_if_some;
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

enum ExecutionStaging {
    Ready(Execution),
    NeedData(DaeExecution),
}

fn sql_execute_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let paramset_size = match stmt.inner.lock() {
        Ok(state) => state.paramset_size,
        Err(_) => {
            error!("SQLExecute: stmt mutex poisoned");
            return SQL_ERROR;
        }
    };
    if paramset_size > 1 {
        return execute_param_array(statement_handle, stmt, paramset_size);
    }

    let dbc = stmt.parent_dbc();

    let staging = match stage_execution(stmt) {
        Ok(s) => s,
        Err(rc) => return rc,
    };

    execute_staged(statement_handle, stmt, dbc, staging)
}

fn execute_staged(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    dbc: &crate::handles::DbcHandle,
    staging: ExecutionStaging,
) -> SqlReturn {
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
    }
}

fn execute_param_array(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    paramset_size: SqlULen,
) -> SqlReturn {
    let (bind_type, operation_ptr, status_ptr, processed_ptr) = match stmt.inner.lock() {
        Ok(state) => (
            state
                .inert_attrs
                .get(SQL_ATTR_PARAM_BIND_TYPE)
                .unwrap_or(SQL_PARAM_BIND_BY_COLUMN),
            state
                .inert_attrs
                .get(SQL_ATTR_PARAM_OPERATION_PTR)
                .unwrap_or(0) as *const SqlUSmallInt,
            state
                .inert_attrs
                .get(SQL_ATTR_PARAM_STATUS_PTR)
                .unwrap_or(0) as *mut SqlUSmallInt,
            state
                .inert_attrs
                .get(SQL_ATTR_PARAMS_PROCESSED_PTR)
                .unwrap_or(0) as *mut SqlULen,
        ),
        Err(_) => {
            error!("SQLExecute: stmt mutex poisoned while reading parameter-array attributes");
            return SQL_ERROR;
        }
    };

    unsafe {
        write_if_some(processed_ptr, 0);
        for row in 0..paramset_size {
            write_param_status(status_ptr, row, SQL_PARAM_UNUSED);
        }
    }

    let dbc = stmt.parent_dbc();
    let mut worst = SQL_SUCCESS;
    let mut total_rows = -1_i64;

    for row in 0..paramset_size {
        unsafe { write_if_some(processed_ptr, row + 1) };
        if !operation_ptr.is_null()
            && unsafe { operation_ptr.wrapping_add(row).read_unaligned() } == SQL_PARAM_IGNORE
        {
            continue;
        }

        let staging = match stage_execution_row(stmt, row, bind_type, true) {
            Ok(staging) => staging,
            Err(rc) => {
                unsafe { write_param_status(status_ptr, row, SQL_PARAM_ERROR) };
                if let Ok(mut state) = stmt.inner.lock() {
                    state.row_count = total_rows;
                }
                return rc;
            }
        };
        let rc = execute_staged(statement_handle, stmt, dbc, staging);
        let status = match rc {
            SQL_SUCCESS => SQL_PARAM_SUCCESS,
            SQL_SUCCESS_WITH_INFO => {
                worst = SQL_SUCCESS_WITH_INFO;
                SQL_PARAM_SUCCESS_WITH_INFO
            }
            SQL_NEED_DATA | SQL_ERROR | SQL_INVALID_HANDLE => SQL_PARAM_ERROR,
            _ => SQL_PARAM_ERROR,
        };
        unsafe { write_param_status(status_ptr, row, status) };
        if !matches!(rc, SQL_SUCCESS | SQL_SUCCESS_WITH_INFO) {
            if let Ok(mut state) = stmt.inner.lock() {
                state.row_count = total_rows;
            }
            return if rc == SQL_NEED_DATA { SQL_ERROR } else { rc };
        }

        if let Ok(state) = stmt.inner.lock()
            && state.row_count >= 0
        {
            total_rows = if total_rows < 0 {
                state.row_count
            } else {
                total_rows.saturating_add(state.row_count)
            };
        }

        if row + 1 < paramset_size {
            let cursor_open = stmt
                .inner
                .lock()
                .map(|state| state.has_state(STMT_STATE_CURSOR_OPEN))
                .unwrap_or(false);
            if cursor_open {
                let close_rc = unsafe { sql_free_stmt_close(statement_handle) };
                if close_rc == SQL_ERROR || close_rc == SQL_INVALID_HANDLE {
                    unsafe { write_param_status(status_ptr, row, SQL_PARAM_ERROR) };
                    return close_rc;
                }
                if close_rc == SQL_SUCCESS_WITH_INFO {
                    worst = SQL_SUCCESS_WITH_INFO;
                    unsafe { write_param_status(status_ptr, row, SQL_PARAM_SUCCESS_WITH_INFO) };
                }
            }
        }
    }

    if let Ok(mut state) = stmt.inner.lock() {
        state.row_count = total_rows;
        state.pending_row_counts.clear();
    }
    worst
}

/// # Safety
/// When non-null, `status_ptr` must address an array containing `row + 1`
/// writable `SqlUSmallInt` elements.
unsafe fn write_param_status(status_ptr: *mut SqlUSmallInt, row: usize, status: SqlUSmallInt) {
    if !status_ptr.is_null() {
        unsafe { write_if_some(status_ptr.wrapping_add(row), status) };
    }
}

/// Validates statement state and builds the parameter list under the STMT lock,
/// setting `EXEC_STARTED` on success. Application value buffers are read here by
/// reference (no network I/O).
fn stage_execution(stmt: &StmtHandle) -> Result<ExecutionStaging, SqlReturn> {
    stage_execution_row(stmt, 0, SQL_PARAM_BIND_BY_COLUMN, false)
}

fn stage_execution_row(
    stmt: &StmtHandle,
    row: usize,
    bind_type: SqlULen,
    parameter_array: bool,
) -> Result<ExecutionStaging, SqlReturn> {
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

    // Scan for data-at-execution parameters.  If any are present, use the
    // streaming path; otherwise, go through the normal prepared-execute path.
    let ParamsWithDae { params, dae_params } = if parameter_array {
        unsafe {
            build_named_params_row(&mut stmt_state, marker_count, "SQLExecute", row, bind_type)
        }?
    } else {
        unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecute") }?
    };
    if parameter_array && !dae_params.is_empty() {
        post_diag(&mut stmt_state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
        return Err(SQL_ERROR);
    }

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
    stmt_state.row_count = -1;
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
        SQL_C_CHAR, SQL_DATA_AT_EXEC, SQL_NULL_HANDLE, SQL_PARAM_INPUT, SQL_SUCCESS, SQL_VARCHAR,
        SqlLen,
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
        };
        assert_eq!(query_timeout, 0);
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
            ExecutionStaging::Ready(_) => panic!("expected NeedData staging for DAE param"),
        }
    }
}
