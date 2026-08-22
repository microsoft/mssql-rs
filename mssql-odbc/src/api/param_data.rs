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

use tracing::{debug, error};

use mssql_tds::connection::tds_client::StreamedParamStatus;

use super::exec_common::{abort_dae_with_diag, fail_with_tds, finish_execute, return_client_idle};
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NEED_DATA, SqlHandle, SqlPointer, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::{STMT_STATE_EXEC_STARTED, STMT_STATE_NEED_DATA};
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
    let (is_first_call, current_ptr) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if !stmt_state.has_state(STMT_STATE_NEED_DATA) {
            error!("SQLParamData: called without an active data-at-execution sequence (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }

        let first = stmt_state.dae_param_data_first;
        let idx = stmt_state.dae_current_idx;
        let param_idx = stmt_state
            .dae_param_indices
            .get(idx)
            .copied()
            .unwrap_or(usize::MAX);
        let ptr = stmt_state
            .bound_params
            .get(param_idx)
            .and_then(|p| p.as_ref())
            .map(|p| p.parameter_value_ptr)
            .unwrap_or(std::ptr::null_mut());

        (first, ptr)
    };

    // Write the current parameter's application token to *value_ptr_ptr.
    if !value_ptr_ptr.is_null() {
        unsafe { *value_ptr_ptr = current_ptr };
    }

    if is_first_call {
        // First SQLParamData: just deliver the pointer and wait for SQLPutData.
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned clearing first-call flag");
            return SQL_ERROR;
        };
        stmt_state.dae_param_data_first = false;
        return SQL_NEED_DATA;
    }

    let close_error = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned validating current DAE parameter");
            return SQL_ERROR;
        };

        if !stmt_state.dae_current_put_data_called {
            Some(ERR_FUNCTION_SEQUENCE)
        } else if !stmt_state.dae_current_is_null {
            match stmt_state
                .dae_expected_lengths
                .get(stmt_state.dae_current_idx)
                .and_then(|v| *v)
            {
                Some(expected) if expected != stmt_state.dae_current_bytes_sent => {
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
        match stmt_state.dae_client.take() {
            Some(c) => c,
            None => {
                error!("SQLParamData: dae_client is None — internal state corruption");
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
                let Ok(mut stmt_state) = stmt.inner.lock() else {
                    error!("SQLParamData: stmt mutex poisoned advancing DAE index");
                    // Put client back in DBC idle since we can't store it.
                    return_client_idle(dbc, statement_handle, client);
                    return SQL_ERROR;
                };
                stmt_state.dae_client = Some(client);
                stmt_state.dae_current_idx += 1;
                stmt_state.dae_current_bytes_sent = 0;
                stmt_state.dae_current_put_data_called = false;
                stmt_state.dae_current_is_null = false;
                let next_idx = stmt_state.dae_current_idx;
                let param_idx = stmt_state
                    .dae_param_indices
                    .get(next_idx)
                    .copied()
                    .unwrap_or(usize::MAX);
                stmt_state
                    .bound_params
                    .get(param_idx)
                    .and_then(|p| p.as_ref())
                    .map(|p| p.parameter_value_ptr)
                    .unwrap_or(std::ptr::null_mut())
            };

            if !value_ptr_ptr.is_null() {
                unsafe { *value_ptr_ptr = next_ptr };
            }
            SQL_NEED_DATA
        }

        Ok(StreamedParamStatus::Complete(_result)) => {
            // All DAE parameters are done.  Recover the prepared plan and
            // orphan, then run the standard finish path.
            let (prepared, orphaned) = {
                let Ok(mut stmt_state) = stmt.inner.lock() else {
                    error!("SQLParamData: stmt mutex poisoned on completion");
                    return_client_idle(dbc, statement_handle, client);
                    return SQL_ERROR;
                };
                let p = stmt_state.dae_prepared.take();
                let o = stmt_state.dae_orphaned.take();
                stmt_state.reset_dae();
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                (p, o)
            };

            // Write the prepared plan back so the statement remains prepared
            // and re-executable.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
            }

            finish_execute(dbc, stmt, statement_handle, client, "SQLParamData")
        }

        Err(e) => {
            error!(%e, "SQLParamData: end_streamed_param failed");
            // The streaming was aborted by the TDS layer; abort_streamed_write
            // was already called internally.  Clean up ODBC state and return
            // the client idle.
            let (prepared, orphaned) = {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    let p = stmt_state.dae_prepared.take();
                    let o = stmt_state.dae_orphaned.take();
                    stmt_state.reset_dae();
                    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                    (p, o)
                } else {
                    (None, None)
                }
            };
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
            }
            fail_with_tds(dbc, stmt, statement_handle, client, &e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::handles::stmt::STMT_STATE_NEED_DATA;
    use crate::test_support::TestHandles;

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
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
        assert!(!state.has_state(STMT_STATE_NEED_DATA));
    }

    #[test]
    fn short_declared_dae_length_returns_22026() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
            state.dae_current_put_data_called = true;
            state.dae_current_bytes_sent = 2;
            state.dae_expected_lengths.push(Some(3));
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        let ret = unsafe { sql_param_data(h.stmt, &mut p) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_DAE_LENGTH_MISMATCH.state
        );
        assert!(!state.has_state(STMT_STATE_NEED_DATA));
    }
}
