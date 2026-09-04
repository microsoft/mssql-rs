// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLParamData` — advances the data-at-execution streaming
//! protocol.
//!
//! # ODBC data-at-execution protocol
//!
//! ```text
//!   SQLExecute         → SQL_NEED_DATA     (DAE params detected, streaming started)
//!   SQLParamData(&p1)  → SQL_NEED_DATA     (p1 = ParameterValuePtr for param 1)
//!   SQLPutData(…)      → SQL_SUCCESS       (supply zero or more chunks)
//!   SQLParamData(&p2)  → SQL_NEED_DATA     (closes param 1, p2 = ParameterValuePtr for param 2)
//!   SQLPutData(…)      → SQL_SUCCESS
//!   SQLParamData(…)    → SQL_SUCCESS / … (closes last param; returns statement result)
//! ```
//!
//! `SQLParamData` is both the "what parameter needs data" query and the
//! "I'm done with the current parameter, advance" signal.  The first call after
//! `SQLExecute` just delivers the pointer; every subsequent call closes the
//! current parameter on the wire and either opens the next or completes the
//! execution.
//!
//! Each parameter requires at least one `SQLPutData` call before the
//! `SQLParamData` that closes it; closing a parameter that received none is a
//! sequence error (`HY010`). An empty — as opposed to NULL — value is supplied
//! with a non-null pointer and a length of zero.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{ExecuteOptions, StatementResult, StreamedParamStatus};

use super::exec_common::{
    abort_dae_with_diag, clear_exec_started, fail_with_tds, finish_execute,
    rebuild_deferred_params, return_client_idle,
};
use super::sqlstate::*;
use super::util::write_if_some;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NEED_DATA, SqlHandle, SqlPointer, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_EXEC_STARTED;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
/// Advances the data-at-execution parameter protocol.
///
/// `value_ptr_ptr`, when non-null, receives the `ParameterValuePtr` that was
/// supplied to `SQLBindParameter` for the parameter currently awaiting data.
/// Applications use this token to identify which buffer to stream.
///
/// # Safety
/// - `statement_handle` must be a valid `STMT` handle allocated by
///   `SQLAllocHandle`.
/// - `value_ptr_ptr`, if non-null, must be a writable `SqlPointer`.
pub(crate) unsafe fn sql_param_data(
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    debug!(?statement_handle, ?value_ptr_ptr, "SQLParamData called");
    crate::ffi_entry!("SQLParamData", unsafe {
        sql_param_data_impl(statement_handle, value_ptr_ptr)
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `value_ptr_ptr`, when non-null, must be writable for one `SqlPointer`.
unsafe fn sql_param_data_impl(
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLParamData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLParamData: handle is not a STMT"
    );

    sql_param_data_safe(statement_handle, stmt, value_ptr_ptr)
}

fn sql_param_data_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    // ── Validate state ──────────────────────────────────────────────────────
    // The first call has no open parameter to close: it opens parameter 0 and
    // hands the application its token. Every later call closes the parameter
    // the cursor is on before advancing.
    let (is_first_call, current_ptr) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if !stmt_state.needs_data() {
            error!("SQLParamData: called without an active data-at-execution sequence (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }

        let Some(dae) = stmt_state.dae.as_mut() else {
            // Unreachable while `needs_data()` implies an open sequence, but this
            // runs behind an FFI boundary: a panic here would unwind into the
            // application and poison the statement mutex. Report it instead.
            error!("SQLParamData: DAE sequence missing despite needs_data (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        };
        let first = dae.cursor.is_none();
        if first {
            dae.advance();
        }
        (first, stmt_state.dae_current_value_ptr())
    };

    // Write the current parameter's application token to *value_ptr_ptr.
    unsafe { write_if_some(value_ptr_ptr, current_ptr) };

    if is_first_call {
        // First SQLParamData: just deliver the pointer and wait for SQLPutData.
        return SQL_NEED_DATA;
    }

    let close_error = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned validating current DAE parameter");
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_ref() else {
            error!("SQLParamData: DAE sequence vanished between locks");
            return SQL_ERROR;
        };

        if !dae.progress.put_data_called {
            Some(ERR_FUNCTION_SEQUENCE)
        } else if !dae.progress.is_null {
            match dae.current_param().and_then(|param| param.expected_len) {
                Some(expected) if expected != dae.progress.bytes_sent => {
                    Some(ERR_DAE_LENGTH_MISMATCH)
                }
                _ => None,
            }
        } else {
            None
        }
    };

    if let Some(diag) = close_error {
        error!("SQLParamData: current DAE parameter failed close-time validation");
        return abort_dae_with_diag(dbc, stmt, statement_handle, diag);
    }

    // ── Deferred sequence: no request is open ───────────────────────────────
    // The parameter closes into its buffer rather than onto the wire, and the
    // execute runs once the last one is in (AB#47590).
    let is_deferred = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned checking deferred mode");
            return SQL_ERROR;
        };
        stmt_state.dae.as_ref().is_some_and(|dae| dae.deferred)
    };
    if is_deferred {
        let (has_more, buffered_phase_done, next_ptr) = {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLParamData: stmt mutex poisoned closing a buffered parameter");
                return SQL_ERROR;
            };
            // The close validation above ran under its own lock and released it,
            // so two concurrent calls can both reach here having judged the same
            // parameter complete. Checking the client out claims the sequence
            // for this call, exactly as the streamed path below does: the loser
            // gets `None` and fails, rather than recording an empty value for
            // the next parameter and advancing past input the application never
            // supplied. Returned immediately -- nothing here does I/O -- so the
            // window is only as wide as the capture itself.
            let client = match stmt_state
                .dae
                .as_mut()
                .and_then(|dae| dae.checkout_client())
            {
                Some(client) => client,
                None => {
                    error!("SQLParamData: DAE sequence is already being closed by another call");
                    post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                    return SQL_ERROR;
                }
            };
            let Some(dae) = stmt_state.dae.as_mut() else {
                error!("SQLParamData: DAE sequence vanished closing a buffered parameter");
                return SQL_ERROR;
            };
            let Some(bound_index) = dae.current_param().map(|param| param.bound_index) else {
                error!("SQLParamData: no open parameter to close");
                dae.return_client(client);
                return SQL_ERROR;
            };
            let bytes = std::mem::take(&mut dae.progress.buffer);
            let is_null = dae.progress.is_null;
            dae.buffered.push((bound_index, bytes, is_null));
            dae.advance();
            let has_more = dae.current_param().is_some();
            let buffered_done = dae.buffered_phase_complete();
            dae.return_client(client);
            (has_more, buffered_done, stmt_state.dae_current_value_ptr())
        };

        // Every remaining parameter streams, so the collected values are
        // complete and the RPC can be opened now. The rest of the sequence goes
        // onto the wire as it arrives instead of being collected whole, which is
        // the whole point of data-at-execution for a LOB.
        if buffered_phase_done && let Some(rc) = open_deferred_rpc(dbc, stmt, statement_handle) {
            return rc;
        }

        // More parameters to collect: hand back the next token.
        if has_more {
            unsafe { write_if_some(value_ptr_ptr, next_ptr) };
            return SQL_NEED_DATA;
        }
        return run_deferred_execute(dbc, stmt, statement_handle);
    }

    // ── Subsequent calls: close current parameter and advance ───────────────
    // Take the TDS client out of stmt_state while we do I/O.
    let (mut client, trailing) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned taking dae_client");
            return SQL_ERROR;
        };
        // Checked out before the carry is drained: if this fails (a concurrent
        // call already holds the client), the sequence stays open for a retry
        // with the partial character still intact rather than silently
        // discarded.
        let client = match stmt_state
            .dae
            .as_mut()
            .and_then(|dae| dae.checkout_client())
        {
            Some(client) => client,
            None => {
                error!("SQLParamData: DAE client is unavailable — internal state corruption");
                post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                return SQL_ERROR;
            }
        };

        // A value that ended part-way through a character leaves bytes in the
        // carry that no further chunk will complete. Flush them before the
        // terminator so they reach the wire lossily rather than vanishing.
        let trailing = stmt_state
            .dae
            .as_mut()
            .map(|dae| match dae.current_param().and_then(|p| p.transcode) {
                Some(transcode) => {
                    let mut carry = std::mem::take(&mut dae.progress.carry);
                    let out = transcode.finish(&mut carry);
                    dae.progress.carry = carry;
                    out
                }
                None => Vec::new(),
            })
            .unwrap_or_default();
        (client, trailing)
    };

    if !trailing.is_empty()
        && let Err(e) = dbc.runtime.block_on(client.write_streamed_chunk(&trailing))
    {
        error!(%e, "SQLParamData: flushing the transcoder tail failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            let parked = stmt_state.take_dae();
            debug_assert!(parked.is_none(), "the client is checked out by this call");
            stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
        }
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let end_result = dbc.runtime.block_on(client.end_streamed_param());

    match end_result {
        Ok(StreamedParamStatus::NeedData { param_name: _ }) => {
            // Advance to the next DAE parameter.
            let next_ptr = {
                let mut stmt_state = match stmt.inner.lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        error!("SQLParamData: stmt mutex poisoned advancing DAE index");
                        // The RPC is still open on the next parameter and the
                        // statement can no longer hold the client. Discard the
                        // half-written request before the client goes back to
                        // the idle pool, or the next command on it fails as
                        // already executing.
                        dbc.runtime.block_on(client.cancel_streamed_write());
                        return_client_idle(dbc, statement_handle, client);
                        return SQL_ERROR;
                    }
                };
                match stmt_state.dae.as_mut() {
                    Some(dae) => {
                        dae.return_client(client);
                        dae.advance();
                        stmt_state.dae_current_value_ptr()
                    }
                    None => {
                        // Drop the STMT guard before touching the DBC lock in
                        // `return_client_idle`: `SQLFreeHandle(SQL_HANDLE_DESC)`
                        // locks DBC then STMT (to reset any statement whose
                        // active ARD/APD is the freed descriptor), so holding
                        // STMT across a DBC-lock call here would be the
                        // reverse order and risk an ABBA deadlock against it.
                        drop(stmt_state);
                        error!("SQLParamData: DAE sequence vanished while advancing");
                        dbc.runtime.block_on(client.cancel_streamed_write());
                        return_client_idle(dbc, statement_handle, client);
                        return SQL_ERROR;
                    }
                }
            };

            unsafe { write_if_some(value_ptr_ptr, next_ptr) };
            SQL_NEED_DATA
        }

        Ok(StreamedParamStatus::Complete(result)) => {
            // All DAE parameters are done.  `take_dae` recovers the prepared
            // plan and orphan in the same critical section that ends the
            // sequence: a statement observed between the two would look idle
            // but unprepared, and a concurrent SQLExecute would report 07002
            // instead of re-running the plan. `SQLExecDirect` parks no plan, so
            // a `None` plan is legitimate there.
            let was_prepared = {
                let Ok(mut stmt_state) = stmt.inner.lock() else {
                    error!("SQLParamData: stmt mutex poisoned on completion");
                    return_client_idle(dbc, statement_handle, client);
                    return SQL_ERROR;
                };
                debug_assert!(
                    stmt_state.dae.is_some(),
                    "SQLParamData: DAE sequence vanished before completion"
                );
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                stmt_state.prepared.is_some()
            };

            // Same contract as the non-streaming `SQLExecute` arm: a prepared
            // statement runs one SQL statement, so a no-row result must have its
            // trailing tokens drained (including `sp_prepexec`'s `@handle`
            // RETURNVALUE, which is what materializes the handle for reuse)
            // instead of leaving a 0-column cursor open. `SQLExecDirect` streams
            // ad-hoc `sp_executesql` with no parked plan and no trailing handle,
            // so it keeps the batch-navigation behaviour `finish_execute` gives
            // it.
            if was_prepared
                && !matches!(result, StatementResult::Rows)
                && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
            {
                error!(%e, "SQLParamData: draining no-row prepared result failed");
                return fail_with_tds(dbc, stmt, statement_handle, client, &e);
            }

            finish_execute(dbc, stmt, statement_handle, client, "SQLParamData")
        }

        Err(e) => {
            error!(%e, "SQLParamData: end_streamed_param failed");
            // The streaming was aborted by the TDS layer; abort_streamed_write
            // was already called internally.  Clean up ODBC state and return
            // the client idle.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
            }
            fail_with_tds(dbc, stmt, statement_handle, client, &e)
        }
    }
}

/// Opens the RPC for a deferred sequence whose buffered parameters are all
/// collected, so the streamed ones that remain go onto the wire as their chunks
/// arrive.
///
/// Returns `Some(rc)` only on failure; success leaves the sequence open with its
/// cursor untouched, so the caller hands back the next parameter's token exactly
/// as it would have.
///
/// Without this the whole sequence stays deferred and every parameter is
/// collected whole, so one fixed-width value alongside a `varbinary(max)` would
/// cost memory proportional to the LOB — the opposite of what data-at-execution
/// is for (AB#47590).
fn open_deferred_rpc(
    dbc: &crate::handles::DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
) -> Option<SqlReturn> {
    let taken = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned opening the deferred RPC");
            return Some(SQL_ERROR);
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished opening the deferred RPC");
            return Some(SQL_ERROR);
        };
        let collected = std::mem::take(&mut dae.buffered);
        let dae_params = dae.params().to_vec();
        let prebuilt = std::mem::take(&mut dae.prebuilt);
        let sql = dae.sql.take();
        let timeout_secs = dae.timeout_secs;
        let prepared = dae.take_prepared();
        let orphaned = dae.take_orphaned();
        let Some(client) = dae.checkout_client() else {
            error!("SQLParamData: DAE sequence has no client to open its RPC on");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return Some(SQL_ERROR);
        };

        let params = match rebuild_deferred_params(
            &mut stmt_state,
            prebuilt,
            &collected,
            &dae_params,
            "SQLParamData",
        ) {
            Ok(params) => params,
            Err(rc) => {
                // Nothing was sent, so the statement goes back to being merely
                // prepared rather than needing a cancel.
                //
                // The client this call checked out is returned explicitly:
                // `take_dae` cannot produce it, because the checkout already
                // removed it from the sequence. Binding its `None` over this
                // one would drop the connection's only client and leave the DBC
                // permanently busy.
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                drop(stmt_state);
                return_client_idle(dbc, statement_handle, client);
                return Some(rc);
            }
        };
        (client, params, prepared, orphaned, sql, timeout_secs)
    };

    let (mut client, params, mut prepared, mut orphaned, sql, timeout_secs) = taken;
    let collation = client.get_collation();
    let options = ExecuteOptions::new().timeout_secs(timeout_secs);

    let begin_result = match (prepared.as_mut(), sql) {
        (Some(plan), _) => dbc.runtime.block_on(client.begin_execute_prepared(
            &mut plan.stmt,
            params,
            &mut orphaned,
            options,
        )),
        (None, Some(sql)) => dbc
            .runtime
            .block_on(client.begin_sp_executesql(sql, params, options)),
        (None, None) => {
            error!("SQLParamData: deferred sequence has neither a plan nor SQL text");
            return_client_idle(dbc, statement_handle, client);
            clear_exec_started(stmt);
            return Some(SQL_ERROR);
        }
    };

    // The plan goes back before either outcome is reported, exactly as the
    // immediate path does: a failure must still leave the statement prepared.
    match begin_result {
        Ok(StreamedParamStatus::NeedData { .. }) => {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                // The RPC is open and the client is in hand, so it cannot be
                // parked back on a statement whose lock is unusable. Hand it to
                // the DBC rather than dropping it, or the connection is left
                // busy with no client for the rest of its life.
                error!("SQLParamData: stmt mutex poisoned parking the opened RPC");
                return_client_idle(dbc, statement_handle, client);
                return Some(SQL_ERROR);
            };
            stmt_state.prepared = prepared;
            stmt_state.pending_unprepare = orphaned;
            let Some(dae) = stmt_state.dae.as_mut() else {
                error!("SQLParamData: DAE sequence vanished with its RPC open");
                drop(stmt_state);
                return_client_idle(dbc, statement_handle, client);
                return Some(SQL_ERROR);
            };
            dae.begin_streaming_phase(client, collation);
            None
        }
        Ok(StreamedParamStatus::Complete(_)) => {
            // Unreachable: this runs only while a streamed parameter is still
            // open, so the RPC cannot have completed.
            error!("SQLParamData: deferred RPC completed despite a streamed parameter");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
            }
            Some(finish_execute(
                dbc,
                stmt,
                statement_handle,
                client,
                "SQLParamData",
            ))
        }
        Err(e) => {
            error!(%e, "SQLParamData: opening the deferred RPC failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
            }
            Some(fail_with_tds(dbc, stmt, statement_handle, client, &e))
        }
    }
}

/// Runs the execute a data-at-execution sequence deferred, now that every value
/// has been collected.
///
/// The whole parameter list is rebuilt from the application's bindings and the
/// collected buffers, so the values go out declared and bounded by the same
/// conversion a materialized execute uses; the request itself is then the
/// ordinary `sp_execute` / `sp_executesql` one, not a streamed variant
/// (AB#47590).
fn run_deferred_execute(
    dbc: &crate::handles::DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
) -> SqlReturn {
    // Take everything the execute needs, and end the sequence, in one critical
    // section: a statement observed between the two would look idle but
    // unprepared.
    let taken = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned starting the deferred execute");
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished before the deferred execute");
            return SQL_ERROR;
        };
        let collected = std::mem::take(&mut dae.buffered);
        let dae_params = dae.params().to_vec();
        let prebuilt = std::mem::take(&mut dae.prebuilt);
        let sql = dae.sql.take();
        let timeout_secs = dae.timeout_secs;
        let prepared = dae.take_prepared();
        let mut orphaned = dae.take_orphaned();

        let params = match rebuild_deferred_params(
            &mut stmt_state,
            prebuilt,
            &collected,
            &dae_params,
            "SQLParamData",
        ) {
            Ok(params) => params,
            Err(rc) => {
                // Nothing was sent, so the statement goes back to being merely
                // prepared rather than needing a cancel.
                let client = stmt_state.take_dae();
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned.take();
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                drop(stmt_state);
                if let Some(client) = client {
                    return_client_idle(dbc, statement_handle, client);
                }
                return rc;
            }
        };

        let client = stmt_state.take_dae();
        // `EXEC_STARTED` deliberately stays set across the execute below, exactly
        // as the immediate path holds it for its whole round trip and lets
        // `finish_execute` / `fail_with_tds` clear it. Clearing it here would
        // open a window in which a concurrent `SQLPrepareW` passes its
        // active-execute guard and installs a plan that the `prepared` restore
        // after the execute would then silently overwrite.
        (client, params, prepared, orphaned, sql, timeout_secs)
    };

    let (client, params, mut prepared, mut orphaned, sql, timeout_secs) = taken;
    let Some(mut client) = client else {
        error!("SQLParamData: deferred sequence has no client to execute on");
        clear_exec_started(stmt);
        return SQL_ERROR;
    };

    let options = ExecuteOptions::new().timeout_secs(timeout_secs);
    let was_prepared = prepared.is_some();
    let exec_result: Result<Option<StatementResult>, mssql_tds::error::Error> =
        match (prepared.as_mut(), sql) {
            (Some(plan), _) => dbc
                .runtime
                .block_on(client.execute_prepared(&mut plan.stmt, params, &mut orphaned, options))
                .map(Some),
            (None, Some(sql)) => dbc
                .runtime
                .block_on(client.execute_sp_executesql(sql, params, options))
                .map(|_| None),
            (None, None) => {
                error!("SQLParamData: deferred sequence has neither a plan nor SQL text");
                return_client_idle(dbc, statement_handle, client);
                clear_exec_started(stmt);
                return SQL_ERROR;
            }
        };

    // Give the plan back before reporting either outcome, exactly as the
    // immediate path does: a failure must still leave the statement prepared.
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.prepared = prepared;
        stmt_state.pending_unprepare = orphaned;
    }

    let stmt_result = match exec_result {
        Ok(result) => result,
        Err(e) => {
            error!(%e, "SQLParamData: deferred execute failed");
            return fail_with_tds(dbc, stmt, statement_handle, client, &e);
        }
    };

    // Same contract as the immediate prepared arm: a no-row prepared result has
    // its trailing tokens drained rather than leaving a 0-column cursor open.
    if was_prepared
        && !matches!(stmt_result, Some(StatementResult::Rows))
        && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
    {
        error!(%e, "SQLParamData: draining a no-row deferred result failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    finish_execute(dbc, stmt, statement_handle, client, "SQLParamData")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NULL_HANDLE};
    use crate::handles::stmt::{DaeParam, DaeState};
    use crate::test_support::TestHandles;

    fn open_dae(expected_len: Option<usize>) -> DaeState {
        DaeState::for_test(
            vec![DaeParam::unbounded(0, std::ptr::null_mut(), expected_len)],
            Some(0),
        )
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(SQL_NULL_HANDLE, &mut p) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn without_need_data_state_returns_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
    }

    #[test]
    fn first_call_returns_the_staged_dae_token() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut token = 0u8;
        let token_ptr = (&raw mut token).cast();
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(DaeState::for_test(
                vec![DaeParam::unbounded(0, token_ptr, None)],
                None,
            ));
        }

        let mut returned = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut returned) };
        assert_eq!(ret, SQL_NEED_DATA);
        assert_eq!(returned, token_ptr);
    }

    #[test]
    fn second_call_without_put_data_returns_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(open_dae(None));
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
        assert!(!state.needs_data());
    }

    /// A concurrent call on the same statement can hold the client when this
    /// one tries to close a transcoded parameter (`checkout_client` returns
    /// `None`, the same condition `DaeState::for_test`'s parked-client-free
    /// setup exercises here). The client is checked out *before* the carry is
    /// drained, so this failure must not lose the partial character a retry
    /// would need: it has to see the same bytes it would have seen if this
    /// call had never happened.
    #[test]
    fn failed_checkout_leaves_the_carry_and_the_sequence_intact() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut param = DaeParam::unbounded(0, std::ptr::null_mut(), None);
            param.transcode = Some(crate::conversion::param_convert::DaeTranscode::new(
                SQL_C_CHAR,
                crate::api::odbc_types::SQL_WVARCHAR,
                mssql_tds::token::tokens::SqlCollation::default(),
            ));
            let mut dae = DaeState::for_test(vec![param], Some(0));
            dae.progress.put_data_called = true;
            // A lead byte whose continuation has not arrived yet.
            dae.progress.carry = vec![0xC3];
            state.dae = Some(dae);
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
        assert!(state.needs_data(), "sequence must stay open for a retry");
        assert_eq!(
            state.dae.as_ref().unwrap().progress.carry,
            vec![0xC3],
            "the pending partial character must survive a failed checkout"
        );
    }

    #[test]
    fn short_declared_dae_length_returns_22026() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut dae = open_dae(Some(3));
            dae.progress.put_data_called = true;
            dae.progress.bytes_sent = 2;
            state.dae = Some(dae);
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_DAE_LENGTH_MISMATCH.state
        );
        assert!(!state.needs_data());
    }
}
