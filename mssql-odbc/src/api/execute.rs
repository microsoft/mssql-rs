// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLExecute — execute a prepared statement with the
//! currently bound parameter values.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;

use super::exec_common::{build_named_params, claim_connection, fail_with_tds, finish_execute};
use super::sqlstate::*;
use super::util::rewrite_param_markers;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn};
use crate::error::free_errors;
use crate::handles::stmt::{
    PreparedHandle, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED,
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
    handle: Option<PreparedHandle>,
    /// A superseded prepared handle (from a prior rebind / re-prepare) to be
    /// dropped on the server
    drop_handle: Option<PreparedHandle>,
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

    // Command timeout (SQL_ATTR_QUERY_TIMEOUT) isn't wired up yet; None = no
    // limit. TODO: thread the statement's query-timeout attribute here.
    //
    // When wiring it: `deduct_timeout` below returns `Some(0)` when recovery
    // consumed the whole budget, but the execute RPCs convert their timeout via
    // `TdsClient::timeout_to_duration`, which reads `Some(0)` as infinite. So an
    // exhausted budget must be turned into an immediate `HYT00` here (don't
    // issue the RPC) rather than passed down. See `TdsClient::deduct_timeout`.
    let timeout_sec: Option<u32> = None;

    // Single recovery point (msodbcsql `GetBatchCtxOrRecover`): reconnect
    // *before* the liveness decision below. No execute RPC recovers again, so a
    // reconnect can't slip in and make the chosen handle stale mid-send.
    let reconnect_elapsed = match dbc
        .runtime
        .block_on(client.check_and_reconnect(timeout_sec, None))
    {
        Ok(elapsed) => elapsed,
        Err(e) => {
            error!(%e, "SQLExecute: reconnect failed");
            return fail_with_tds(dbc, stmt, statement_handle, client, &e);
        }
    };
    // Keep recovery + execution within one command-timeout budget.
    let timeout_sec = TdsClient::deduct_timeout(timeout_sec, reconnect_elapsed);
    let session_epoch = client.connection_recovery_count();

    // msodbcsql `FIsReprepareRequired`: reuse vs. reprepare, judged by epoch.
    match plan_execution(exec.handle, exec.drop_handle, session_epoch) {
        // Reuse the cached handle via sp_execute.
        ExecPlan::Reuse { handle_id } => {
            if let Err(e) = dbc.runtime.block_on(client.execute_sp_execute(
                handle_id,
                None,
                Some(exec.named_params),
                timeout_sec,
                None,
            )) {
                error!(%e, "SQLExecute: sp_execute failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }
        }
        // Prepare + run in one round trip; `drop_id` piggybacks a superseded
        // handle's release onto the same sp_prepexec (no separate sp_unprepare).
        //
        // NOTE: msodbcsql falls back to sp_prepare + sp_execute for data-at-exec
        // params, which sp_prepexec can't carry. Phase 1 rejects DAE at bind
        // time; add that branch when DAE support lands.
        ExecPlan::Reprepare {
            drop_id,
            scrub_cached,
        } => {
            if scrub_cached {
                // Drop the stale cached handle so capture-if-absent records the
                // one from this fresh prepare.
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared_handle = None;
                }
            }
            if let Err(e) = dbc.runtime.block_on(client.execute_sp_prepexec(
                exec.rewritten_sql,
                exec.named_params,
                drop_id,
                timeout_sec,
                None,
            )) {
                error!(%e, "SQLExecute: sp_prepexec failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }
            // The new handle's `@handle` RETURNVALUE arrives after the result
            // set, so it's captured at drain time, not here.
        }
    }

    finish_execute(dbc, stmt, statement_handle, client, "SQLExecute")
}

/// The action `SQLExecute` takes after resolving cached-handle liveness against
/// the connection's current session epoch (msodbcsql `FIsReprepareRequired`).
#[derive(Debug, PartialEq, Eq)]
enum ExecPlan {
    /// The cached handle is valid for this session: reuse it via `sp_execute`.
    Reuse { handle_id: i32 },
    /// Prepare fresh via `sp_prepexec`. `drop_id` piggybacks a still-live
    /// superseded handle as the by-ref `@handle` drop (a stale one is `None`).
    /// `scrub_cached` is `true` when a *stale* cached handle must first be
    /// cleared from `StmtState` so capture-if-absent records the fresh handle.
    Reprepare {
        drop_id: Option<i32>,
        scrub_cached: bool,
    },
}

/// Decides the execution plan by comparing the staged handles' `session_epoch`
/// against the connection's current epoch. A handle from a superseded session
/// is dead server-side, so it is neither executed nor sent as a drop target.
fn plan_execution(
    handle: Option<PreparedHandle>,
    drop_handle: Option<PreparedHandle>,
    session_epoch: u32,
) -> ExecPlan {
    if let Some(h) = handle.filter(|h| h.session_epoch == session_epoch) {
        return ExecPlan::Reuse { handle_id: h.id };
    }
    ExecPlan::Reprepare {
        drop_id: drop_handle
            .filter(|h| h.session_epoch == session_epoch)
            .map(|h| h.id),
        // In this branch the cached handle is either absent (first execute) or
        // present-but-stale (its epoch didn't match above); the latter must be
        // scrubbed before the fresh prepare.
        scrub_cached: handle.is_some(),
    }
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
            state.pending_unprepare = Some(PreparedHandle {
                id: 42,
                session_epoch: 0,
            });
        }

        let exec = stage_execution(stmt).expect("staging should succeed");
        assert_eq!(exec.handle, None);
        assert_eq!(
            exec.drop_handle,
            Some(PreparedHandle {
                id: 42,
                session_epoch: 0,
            })
        );

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
            state.prepared_handle = Some(PreparedHandle {
                id: 7,
                session_epoch: 0,
            });
        }

        let exec = stage_execution(stmt).expect("staging should succeed");
        assert_eq!(
            exec.handle,
            Some(PreparedHandle {
                id: 7,
                session_epoch: 0,
            })
        );
        assert_eq!(exec.drop_handle, None);
    }

    #[test]
    fn plan_execution_reuses_live_handle() {
        // Same epoch as the connection: reuse the cached handle via sp_execute.
        assert_eq!(
            plan_execution(
                Some(PreparedHandle {
                    id: 7,
                    session_epoch: 3,
                }),
                None,
                3
            ),
            ExecPlan::Reuse { handle_id: 7 }
        );
    }

    #[test]
    fn plan_execution_reprepares_and_scrubs_stale_handle() {
        // The cached handle was created in an older session (epoch 2) than the
        // reconnected connection (epoch 3): it is dead, so we reprepare fresh
        // and scrub the stale cached handle. No drop to piggyback.
        assert_eq!(
            plan_execution(
                Some(PreparedHandle {
                    id: 7,
                    session_epoch: 2,
                }),
                None,
                3
            ),
            ExecPlan::Reprepare {
                drop_id: None,
                scrub_cached: true,
            }
        );
    }

    #[test]
    fn plan_execution_first_execute_reprepares_without_scrub() {
        // No cached handle (first execute): reprepare fresh, nothing to scrub,
        // no drop.
        assert_eq!(
            plan_execution(None, None, 0),
            ExecPlan::Reprepare {
                drop_id: None,
                scrub_cached: false,
            }
        );
    }

    #[test]
    fn plan_execution_keeps_live_drop_for_piggyback() {
        // A superseded handle from the current session is a valid piggyback drop
        // target on the next sp_prepexec.
        assert_eq!(
            plan_execution(
                None,
                Some(PreparedHandle {
                    id: 9,
                    session_epoch: 5,
                }),
                5
            ),
            ExecPlan::Reprepare {
                drop_id: Some(9),
                scrub_cached: false,
            }
        );
    }

    #[test]
    fn plan_execution_drops_stale_drop() {
        // The pending drop belongs to a session a reconnect already tore down:
        // it is gone server-side, so it is not sent as a drop target.
        assert_eq!(
            plan_execution(
                None,
                Some(PreparedHandle {
                    id: 9,
                    session_epoch: 4,
                }),
                5
            ),
            ExecPlan::Reprepare {
                drop_id: None,
                scrub_cached: false,
            }
        );
    }

    #[test]
    fn plan_execution_scrubs_stale_handle_and_drops_stale_drop() {
        // Both a stale cached handle and a stale pending drop across a reconnect:
        // reprepare fresh, scrub the cached handle, and send no drop.
        assert_eq!(
            plan_execution(
                Some(PreparedHandle {
                    id: 7,
                    session_epoch: 1,
                }),
                Some(PreparedHandle {
                    id: 9,
                    session_epoch: 1,
                }),
                2
            ),
            ExecPlan::Reprepare {
                drop_id: None,
                scrub_cached: true,
            }
        );
    }

    #[test]
    fn plan_execution_epoch_zero_never_reconnected_reuses() {
        // A connection that never reconnected stays at epoch 0; a handle captured
        // there stays live — the gate is a no-op when recovery is off.
        assert_eq!(
            plan_execution(
                Some(PreparedHandle {
                    id: 1,
                    session_epoch: 0,
                }),
                None,
                0
            ),
            ExecPlan::Reuse { handle_id: 1 }
        );
    }
}
