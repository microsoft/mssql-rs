// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{
    claim_connection, fail_with_tds, finish_execute, flush_pending_unprepare,
};
use super::sqlstate::*;
use super::util::rewrite_param_markers;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use crate::params::convert::bound_param_to_rpc;

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

    // Release any handle orphaned by a prior rebind / re-prepare before we
    // (re)prepare or reuse the current handle.
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLExecute");

    match exec.handle {
        // Already prepared: reuse the cached server handle (msodbcsql
        // `cmdp.hPrepCurrent`) via sp_execute.
        Some(handle) => {
            if let Err(e) = dbc.runtime.block_on(client.execute_sp_execute(
                handle,
                None,
                Some(exec.named_params),
                None,
                None,
            )) {
                error!(%e, "SQLExecute: sp_execute failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }
        }
        // First execute: prepare and run in a single round trip via sp_prepexec
        // (msodbcsql's deferred-prepare path), then cache the returned handle
        // for subsequent sp_execute reuse.
        //
        // NOTE: msodbcsql falls back to sp_prepare + sp_execute for statements
        // with data-at-execution (DAE) parameters, which sp_prepexec can't
        // carry. Phase 1 rejects DAE params at bind time, so that case can't
        // occur here yet — add the sp_prepare branch when DAE support lands.
        None => {
            if let Err(e) = dbc.runtime.block_on(client.execute_sp_prepexec(
                exec.rewritten_sql,
                exec.named_params,
                None,
                None,
            )) {
                error!(%e, "SQLExecute: sp_prepexec failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }
            // The prepared handle is captured once the batch is drained (see
            // `capture_prepared_handle`): for a result-returning statement the
            // `@handle` RETURNVALUE arrives after the result set, so it is read
            // at drain time (SQLCloseCursor, or the DDL finish path), not here.
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

    let mut named_params = Vec::with_capacity(marker_count);
    for i in 0..marker_count {
        let Some(Some(bound_param)) = stmt_state.bound_params.get(i) else {
            error!(
                parameter = i + 1,
                "SQLExecute: parameter marker has no bound value"
            );
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_07002,
                0,
                "COUNT field incorrect or syntax error",
            );
            return Err(SQL_ERROR);
        };
        let name = format!("@P{}", i + 1);
        match unsafe { bound_param_to_rpc(name, bound_param) } {
            Ok(param) => named_params.push(param),
            Err(e) => {
                error!(
                    parameter = i + 1,
                    message = e.message(),
                    "SQLExecute: parameter conversion failed"
                );
                post_sql_error(&mut stmt_state, SQLSTATE_HYC00, 0, e.message());
                return Err(SQL_ERROR);
            }
        }
    }

    let handle = stmt_state.prepared_handle;
    stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
    stmt_state.column_metadata.clear();
    stmt_state.current_row = None;
    stmt_state.set_state(STMT_STATE_EXEC_STARTED);

    Ok(Execution {
        rewritten_sql,
        named_params,
        handle,
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
}
