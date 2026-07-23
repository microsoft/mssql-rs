// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::StatementResult;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{build_named_params, claim_connection, fail_with_tds, finish_execute};
use super::sqlstate::*;
use super::util::rewrite_param_markers;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED,
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
    rewritten_sql: String,
    named_params: Vec<RpcParameter>,
    handle: Option<i32>,
    /// A superseded prepared handle (from a prior rebind / re-prepare) to be dropped
    /// on the server
    drop_handle: Option<i32>,
}

fn sql_execute_safe(statement_handle: SqlHandle, stmt: &StmtHandle) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    let exec = match stage_execution(stmt) {
        Ok(exec) => exec,
        Err(rc) => return rc,
    };

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLExecute") {
        Ok(client) => client,
        Err(rc) => return rc,
    };

    let exec_result = match exec.handle {
        // Already prepared: reuse the cached server handle (msodbcsql
        // `cmdp.hPrepCurrent`) via sp_execute. No pending prepared handle drop can
        // exist here. (StmtState invariant: `pending_unprepare` is set only when
        // `prepared_handle` is None), so nothing to release.
        Some(handle) => dbc.runtime.block_on(client.execute_sp_execute(
            handle,
            None,
            Some(exec.named_params),
            (),
        )),
        // First execute / re-prepare: prepare and run in one round trip via
        // sp_prepexec (deferred-prepare path). A prepared handle superseded
        // by a prior rebind / re-prepare is dropped in the same RPC by passing
        // it as sp_prepexec's `@handle` input (`drop_handle`). This avoids
        // a separate `sp_unprepare` round trip.
        //
        // NOTE: msodbcsql falls back to sp_prepare + sp_execute for statements
        // with data-at-execution (DAE) parameters, which sp_prepexec can't
        // carry. Phase 1 rejects DAE params at bind time, so that case can't
        // occur here yet - add the sp_prepare branch when DAE support lands.
        None => dbc.runtime.block_on(client.execute_sp_prepexec(
            exec.rewritten_sql,
            exec.named_params,
            exec.drop_handle,
            (),
        )),
    };

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
    // 0-column cursor open — matching how the pre-statement-wise path collapsed
    // no-row results. A row-returning statement keeps its cursor open for
    // SQLFetch; its `@handle` RETURNVALUE (sp_prepexec) is captured later at
    // drain time (SQLCloseCursor / the DDL finish path).
    if !matches!(stmt_result, StatementResult::Rows) {
        if let Err(e) = dbc.runtime.block_on(client.advance_to_rows()) {
            error!(%e, "SQLExecute: draining no-row prepared result failed");
            return fail_with_tds(dbc, stmt, statement_handle, client, &e);
        }
    }

    finish_execute(dbc, stmt, statement_handle, client, "SQLExecute")
}

/// Validates statement state and builds the parameter list under the STMT lock,
/// setting `EXEC_STARTED` on success. Application value buffers are read here by
/// reference (no network I/O).
fn stage_execution(stmt: &StmtHandle) -> Result<Execution, SqlReturn> {
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
        stmt_state.prepared_sql.is_some(),
        "SQLExecute: statement not prepared — DM should have rejected this"
    );
    let Some(sql) = stmt_state.prepared_sql.clone() else {
        error!("SQLExecute: statement has not been prepared");
        return Err(SQL_ERROR);
    };

    if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
        error!("SQLExecute: statement has an active execute or open cursor");
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return Err(SQL_ERROR);
    }

    let (rewritten_sql, marker_count) = rewrite_param_markers(&sql);

    let named_params = unsafe { build_named_params(&mut stmt_state, marker_count, "SQLExecute") }?;

    let handle = stmt_state.prepared_handle;
    let drop_handle = stmt_state.pending_unprepare.take();
    stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.column_metadata.clear();
    stmt_state.current_row = None;
    stmt_state.set_state(STMT_STATE_EXEC_STARTED);

    Ok(Execution {
        rewritten_sql,
        named_params,
        handle,
        drop_handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    fn set_prepared(stmt_raw: SqlHandle, sql: &str) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_raw) };
        let mut state = stmt.inner.lock().unwrap();
        state.prepared_sql = Some(sql.to_string());
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
    fn data_at_exec_parameter_returns_hyc00() {
        use crate::api::odbc_types::{
            SQL_C_CHAR, SQL_DATA_AT_EXEC, SQL_PARAM_INPUT, SQL_VARCHAR, SqlLen,
        };
        use crate::params::BoundParam;

        let h = TestHandles::with_env_dbc_stmt();
        set_prepared(h.stmt, "SELECT ?");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        // Bind passes (SQL_C_CHAR → SQL_VARCHAR), but the data-at-execution
        // indicator is only seen at execute time and is unsupported in Phase 1.
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
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn stage_execution_threads_pending_unprepare_as_drop_handle() {
        // A handle orphaned by a prior rebind / re-prepare lives in
        // `pending_unprepare` with `prepared_handle == None`. Staging must move
        // it into `drop_handle` (to piggyback onto sp_prepexec) and consume it
        // so it can't be dropped twice.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared_sql = Some("SELECT 1".to_string());
            state.pending_unprepare = Some(42);
        }

        let exec = stage_execution(stmt).expect("staging should succeed");
        assert_eq!(exec.handle, None);
        assert_eq!(exec.drop_handle, Some(42));

        let state = stmt.inner.lock().unwrap();
        assert!(state.pending_unprepare.is_none());
    }

    #[test]
    fn stage_execution_reuse_path_has_no_drop_handle() {
        // With a cached `prepared_handle`, the next execute reuses it via
        // sp_execute; the invariant guarantees no pending drop, so `drop_handle`
        // is None.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared_sql = Some("SELECT 1".to_string());
            state.prepared_handle = Some(7);
        }

        let exec = stage_execution(stmt).expect("staging should succeed");
        assert_eq!(exec.handle, Some(7));
        assert_eq!(exec.drop_handle, None);
    }
}
