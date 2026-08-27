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

use mssql_tds::connection::tds_client::{StatementResult, StreamedParamStatus};

use super::exec_common::{abort_dae_with_diag, fail_with_tds, finish_execute, return_client_idle};
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

    // ── Subsequent calls: close current parameter and advance ───────────────
    // Take the TDS client out of stmt_state while we do I/O.
    let mut client = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned taking dae_client");
            return SQL_ERROR;
        };
        match stmt_state
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
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::handles::stmt::{DaeParam, DaeState};
    use crate::test_support::TestHandles;

    fn open_dae(expected_len: Option<usize>) -> DaeState {
        DaeState::for_test(
            vec![DaeParam {
                bound_index: 0,
                expected_len,
            }],
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
