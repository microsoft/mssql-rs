// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{
    ExecuteOptions, StatementId, StatementResult, StreamedParamStatus,
};
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{
    ParamsWithDae, build_named_params, claim_connection, fail_with_tds, finish_execute,
    park_dae_client,
};
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use crate::error::free_errors;
use crate::handles::stmt::{
    DaeParam, PreparedPlan, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT,
    STMT_STATE_EXEC_STARTED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Executes the prepared statement on `statement_handle`.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
pub(crate) unsafe fn sql_execute(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLExecute called");
    crate::ffi_entry!("SQLExecute", unsafe { sql_execute_impl(statement_handle) })
}

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
}

enum ExecutionStaging {
    Ready(Execution),
    NeedData(DaeExecution),
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

            if let Err(e) = begin_transaction_if_manual(dbc, &mut client, "SQLExecute") {
                // Nothing ran, so put the staged statement (and any pending orphan)
                // back before reporting, exactly as the failed-claim path does.
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                }
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

            // `execute_prepared` owns the whole recovery sequence: reconnect once up
            // front (mirrors msodbcsql `GetBatchCtxOrRecover`), charge it against the
            // command timeout, then reuse the cached handle or transparently re-prepare
            // when it belongs to a superseded session (msodbcsql `FIsReprepareRequired`).
            // A still-live orphaned handle is released by piggyback on the re-prepare.
            //
            // Command timeout (SQL_ATTR_QUERY_TIMEOUT) isn't wired up yet; the default
            // `ExecuteOptions` means no per-command limit.
            let exec_result = dbc.runtime.block_on(client.execute_prepared(
                &mut prepared.stmt,
                named_params,
                &mut orphaned,
                ExecuteOptions::default(),
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

            if let Err(e) = begin_transaction_if_manual(dbc, &mut client, "SQLExecute") {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = Some(prepared);
                    stmt_state.pending_unprepare = orphaned;
                }
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

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
                ExecuteOptions::default(),
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

/// Validates statement state and builds the parameter list under the STMT lock,
/// setting `EXEC_STARTED` on success. Application value buffers are read here by
/// reference (no network I/O).
fn stage_execution(stmt: &StmtHandle) -> Result<ExecutionStaging, SqlReturn> {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLExecute: stmt mutex poisoned");
        return Err(SQL_ERROR);
    };
    free_errors(&mut stmt_state);

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

    // A statement awaiting data-at-execution input is in the ODBC "Need Data"
    // state, where every function other than SQLPutData/SQLParamData/SQLCancel
    // and the diagnostic calls is a sequence error rather than a cursor error.
    if stmt_state.needs_data() {
        error!("SQLExecute: statement is awaiting data-at-execution input");
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
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

    // Scan for data-at-execution parameters.  If any are present, use the
    // streaming path; otherwise, go through the normal prepared-execute path.
    let ParamsWithDae { params, dae_params } =
        unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecute") }?;

    // All fallible validation passed: move the prepared plan out (written
    // back after the execute) and take any orphaned handle for piggyback drop.
    let prepared = stmt_state
        .prepared
        .take()
        .expect("prepared checked non-None above");
    let orphaned = stmt_state.pending_unprepare.take();
    stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.column_metadata.clear();
    stmt_state.reset_row_stream();
    stmt_state.row_count = -1;
    stmt_state.pending_row_counts.clear();
    stmt_state.set_state(STMT_STATE_EXEC_STARTED);

    if dae_params.is_empty() {
        Ok(ExecutionStaging::Ready(Execution {
            named_params: params,
            prepared,
            orphaned,
        }))
    } else {
        Ok(ExecutionStaging::NeedData(DaeExecution {
            params,
            dae_params,
            prepared,
            orphaned,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_DATA_AT_EXEC, SQL_NULL_HANDLE, SQL_PARAM_INPUT, SQL_VARCHAR, SqlLen,
    };
    use crate::api::util::rewrite_param_markers;
    use crate::params::BoundParam;
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
        set_prepared(h.stmt, "SELECT ?");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_STARTED);
            state.dae = Some(crate::handles::stmt::DaeState::for_test(Vec::new(), None));
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
        stmt.inner
            .lock()
            .unwrap()
            .bound_params
            .push(Some(BoundParam {
                input_output_type: SQL_PARAM_INPUT,
                c_type: SQL_C_CHAR,
                sql_type: SQL_VARCHAR,
                column_size: 0,
                decimal_digits: 0,
                parameter_value_ptr: std::ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: &mut ind as *mut SqlLen,
            }));

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
        stmt.inner
            .lock()
            .unwrap()
            .bound_params
            .push(Some(BoundParam {
                input_output_type: SQL_PARAM_INPUT,
                c_type: SQL_C_CHAR,
                sql_type: SQL_VARCHAR,
                column_size: 0,
                decimal_digits: 0,
                parameter_value_ptr: std::ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: &mut ind as *mut SqlLen,
            }));

        let staging = stage_execution(stmt).expect("staging should succeed");
        match staging {
            ExecutionStaging::NeedData(dae) => {
                // The single param is DAE: its index is in dae_indices.
                assert_eq!(
                    dae.dae_params,
                    vec![DaeParam {
                        bound_index: 0,
                        expected_len: None
                    }]
                );
                assert_eq!(dae.params.len(), 1, "one param in list");
            }
            ExecutionStaging::Ready(_) => panic!("expected NeedData staging for DAE param"),
        }
    }
}
