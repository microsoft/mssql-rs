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

use mssql_tds::connection::tds_client::{ExecuteOptions, StreamedParamStatus};

use super::exec_common::{
    abort_dae_with_diag, clear_exec_started, fail_with_tds, finish_execute_with_param_warning,
    rebuild_deferred_params, return_client_idle,
};
use super::sqlstate::*;
use super::util::write_if_some;
use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NEED_DATA, SqlHandle, SqlPointer, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::{DaeTarget, STMT_STATE_EXEC_STARTED};
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
    crate::ffi_entry!(
        "SQLParamData",
        statement_handle,
        debug!(?statement_handle, ?value_ptr_ptr, "SQLParamData called"),
        unsafe { sql_param_data_impl(statement_handle, value_ptr_ptr) }
    )
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
    let operation = match stmt.begin_operation() {
        Ok(operation) => operation,
        Err(rc) => return rc,
    };
    let rc = sql_param_data_operation(statement_handle, stmt, value_ptr_ptr);
    super::exec_common::finish_operation(operation, stmt, statement_handle, rc)
}

fn sql_param_data_operation(
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

    let deferred_tail_error = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned validating buffered transcoder tail");
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished validating buffered transcoder tail");
            return SQL_ERROR;
        };
        if !dae.deferred {
            None
        } else if let Some(limit) = dae
            .current_param()
            .and_then(|param| param.length_limit)
            .filter(|limit| limit.requires_encoded_input())
        {
            let trailing = match dae.current_param().and_then(|param| param.transcode) {
                Some(transcode) => transcode.finish(&mut dae.progress.carry).bytes,
                None => Vec::new(),
            };
            let mut no_carry = Vec::new();
            match limit.fit_chunk(&mut no_carry, &trailing, dae.progress.retained_units) {
                Ok((_, consumed)) => {
                    dae.progress.retained_units =
                        dae.progress.retained_units.saturating_add(consumed);
                    None
                }
                Err(error) => Some(error.diag()),
            }
        } else {
            None
        }
    };
    if let Some(diag) = deferred_tail_error {
        error!("SQLParamData: buffered transcoder tail exceeds the parameter length");
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
        let (has_more, buffered_phase_done, next_ptr, cell_error) = {
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
            let Some((bound_index, target)) = dae
                .current_param()
                .map(|param| (param.bound_index, param.target))
            else {
                error!("SQLParamData: no open parameter to close");
                dae.return_client(client);
                return SQL_ERROR;
            };
            // A trailing partial SQL_C_WCHAR unit is deliberately omitted here;
            // close-time conversion floors odd byte counts just like the
            // materialized path.
            let bytes = std::mem::take(&mut dae.progress.buffer);
            let is_null = dae.progress.is_null;
            match target {
                DaeTarget::Parameter => dae.buffered.push((bound_index, bytes, is_null)),
                DaeTarget::TvpCell {
                    tvp_index,
                    row_index,
                    column_index,
                } => dae.tvp_cells.push(crate::handles::stmt::BufferedTvpCell {
                    tvp_index,
                    row_index,
                    column_index,
                    bytes,
                    is_null,
                    value: None,
                }),
                DaeTarget::TvpRowControl {
                    tvp_index,
                    row_index,
                } => {
                    let Some(batch_count) = dae.progress.tvp_row_batch_count else {
                        error!("SQLParamData: TVP row-control value was not captured");
                        dae.return_client(client);
                        return SQL_ERROR;
                    };
                    if batch_count > 0 {
                        let Some(batch_end) = row_index.checked_add(batch_count) else {
                            error!("SQLParamData: TVP row batch count overflowed");
                            dae.return_client(client);
                            return SQL_ERROR;
                        };
                        let row_count = dae.tvp_row_counts.entry(tvp_index).or_default();
                        let Some(total) = row_count.checked_add(batch_count) else {
                            error!("SQLParamData: accumulated TVP row count overflowed");
                            dae.return_client(client);
                            return SQL_ERROR;
                        };
                        *row_count = total;
                        dae.tvp_batches.insert(tvp_index, (row_index, batch_end));
                        dae.advance();
                    } else {
                        dae.advance_past_tvp(tvp_index);
                    }
                }
            }
            if !matches!(target, DaeTarget::TvpRowControl { .. }) {
                dae.advance();
            }
            let mut cell_error = None;
            loop {
                match dae.current_param().map(|param| param.target) {
                    Some(DaeTarget::TvpRowControl {
                        tvp_index,
                        row_index,
                    }) if dae
                        .tvp_batches
                        .get(&tvp_index)
                        .is_some_and(|(_, batch_end)| row_index < *batch_end) =>
                    {
                        dae.advance();
                        continue;
                    }
                    Some(DaeTarget::TvpCell { .. }) => {}
                    _ => break,
                }
                let Some(DaeTarget::TvpCell {
                    tvp_index,
                    row_index,
                    column_index,
                }) = dae.current_param().map(|param| param.target)
                else {
                    break;
                };
                if dae.position_current_tvp_cell().is_err() {
                    cell_error = Some(super::exec_common::ParamRowBuildError::Conversion {
                        parameter: tvp_index + 1,
                        source:
                            crate::conversion::param_convert::ParamBuildError::InvalidBufferLength,
                    });
                    break;
                }
                let Some(param) = dae.current_param() else {
                    break;
                };
                let is_null = !param.binding.strlen_or_ind_ptr.is_null()
                    && unsafe {
                        param.binding.strlen_or_ind_ptr.read_unaligned()
                            == crate::api::odbc_types::SQL_NULL_DATA
                    };
                if !is_null
                    && let Some(indicator) = unsafe {
                        crate::conversion::param_convert::data_at_exec_indicator(&param.binding)
                    }
                {
                    if let Some(param) = dae.current_param_mut() {
                        param.expected_len = super::exec_common::dae_expected_length(indicator);
                    }
                    break;
                }
                // Ordinary buffers remain application-owned throughout Need Data.
                let value = unsafe {
                    crate::conversion::param_convert::bound_tvp_cell_to_value(&param.binding)
                };
                match value {
                    Ok(value) => {
                        dae.tvp_cells.push(crate::handles::stmt::BufferedTvpCell {
                            tvp_index,
                            row_index,
                            column_index,
                            bytes: Vec::new(),
                            is_null: false,
                            value: Some(value),
                        });
                        dae.advance();
                    }
                    Err(error) => {
                        cell_error = Some(super::exec_common::ParamRowBuildError::Conversion {
                            parameter: tvp_index + 1,
                            source: crate::conversion::param_convert::ParamBuildError::TvpCellConversion {
                                row: row_index,
                                column: column_index,
                                diagnostic: error.diag(),
                            },
                        });
                        break;
                    }
                }
            }
            let has_more = dae.current_param().is_some();
            let buffered_done = dae.buffered_phase_complete();
            dae.return_client(client);
            (
                has_more,
                buffered_done,
                stmt_state.dae_current_value_ptr(),
                cell_error,
            )
        };

        if let Some(error) = cell_error {
            let rc = abort_dae_with_diag(dbc, stmt, statement_handle, error.diag());
            if let Ok(mut state) = stmt.inner.lock() {
                free_errors(&mut state);
                super::exec_common::post_param_row_build_error(&mut state, &error, 0);
            }
            return rc;
        }
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
        //
        // The length bound's own carry is released first: a value whose last
        // chunk ended mid-code-unit has that half unit held there, and it has to
        // rejoin the value before the transcoder sees it, or the tail character
        // is silently lost.
        let mut trailing = stmt_state
            .dae
            .as_mut()
            .map(|dae| {
                let mut pending = dae
                    .current_param()
                    .and_then(|p| p.length_limit)
                    .map(|_| std::mem::take(&mut dae.progress.unit_carry))
                    .unwrap_or_default();
                match dae.current_param().and_then(|p| p.transcode) {
                    Some(transcode) => {
                        let mut carry = std::mem::take(&mut dae.progress.carry);
                        carry.extend_from_slice(&pending);
                        let out = transcode.finish(&mut carry);
                        dae.progress.carry = carry;
                        (out.bytes, out.had_loss)
                    }
                    None => (std::mem::take(&mut pending), false),
                }
            })
            .unwrap_or_default();

        if let Some(dae) = stmt_state.dae.as_mut()
            && let Some(limit) = dae
                .current_param()
                .and_then(|param| param.length_limit)
                .filter(|limit| limit.requires_encoded_input())
        {
            let mut no_carry = Vec::new();
            match limit.fit_chunk(&mut no_carry, &trailing.0, dae.progress.retained_units) {
                Ok((kept, consumed)) => {
                    trailing.0 = kept;
                    dae.progress.retained_units =
                        dae.progress.retained_units.saturating_add(consumed);
                }
                Err(error) => {
                    if let Some(dae) = stmt_state.dae.as_mut() {
                        dae.return_client(client);
                    }
                    drop(stmt_state);
                    return abort_dae_with_diag(dbc, stmt, statement_handle, error.diag());
                }
            }
        }

        (client, trailing)
    };
    let (trailing, tail_had_loss) = trailing;

    // As in `SQLPutData`: recorded on the connection and reported when the
    // statement completes, not from this call.
    if tail_had_loss {
        client.note_code_page_conversion_loss();
    }

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

    let (mut prepared, mut orphaned) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned taking the streamed plan");
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished before closing the parameter");
            drop(stmt_state);
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        (dae.take_prepared(), dae.take_orphaned())
    };
    let end_result = match prepared.as_mut() {
        Some(plan) => dbc
            .runtime
            .block_on(client.end_execute_prepared_param(&mut plan.stmt, &mut orphaned)),
        None => dbc.runtime.block_on(client.end_streamed_param()),
    };
    // The send may have completed even when its response failed. Restore the
    // updated ownership before either completion or error tears down DAE.
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned restoring the streamed plan");
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished after closing the parameter");
            stmt_state.prepared = prepared;
            stmt_state.pending_unprepare = orphaned;
            drop(stmt_state);
            return_client_idle(dbc, statement_handle, client);
            return SQL_ERROR;
        };
        dae.restore_plan(prepared, orphaned);
    }

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

        Ok(StreamedParamStatus::Complete(_)) => {
            // All DAE parameters are done.  `take_dae` recovers the prepared
            // plan and orphan in the same critical section that ends the
            // sequence: a statement observed between the two would look idle
            // but unprepared, and a concurrent SQLExecute would report 07002
            // instead of re-running the plan. `SQLExecDirect` parks no plan, so
            // a `None` plan is legitimate there.
            let fractional_truncated = {
                let Ok(mut stmt_state) = stmt.inner.lock() else {
                    error!("SQLParamData: stmt mutex poisoned on completion");
                    return_client_idle(dbc, statement_handle, client);
                    return SQL_ERROR;
                };
                debug_assert!(
                    stmt_state.dae.is_some(),
                    "SQLParamData: DAE sequence vanished before completion"
                );
                let fractional_truncated = stmt_state
                    .dae
                    .as_ref()
                    .is_some_and(|dae| dae.fractional_truncated);
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
                fractional_truncated
            };

            finish_execute_with_param_warning(
                dbc,
                stmt,
                statement_handle,
                client,
                "SQLParamData",
                fractional_truncated,
            )
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
    let cancel_handle = match stmt.execution_cancel() {
        Ok(handle) => handle,
        Err(rc) => return Some(rc),
    };
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
        let mut tvp_cells = std::mem::take(&mut dae.tvp_cells);
        let tvp_row_counts = std::mem::take(&mut dae.tvp_row_counts);
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

        let (params, fractional_truncated) = match rebuild_deferred_params(
            &mut stmt_state,
            prebuilt,
            &collected,
            &dae_params,
            &mut tvp_cells,
            &tvp_row_counts,
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
        (
            client,
            params,
            prepared,
            orphaned,
            sql,
            timeout_secs,
            fractional_truncated,
        )
    };

    let (mut client, params, mut prepared, mut orphaned, sql, timeout_secs, fractional_truncated) =
        taken;
    let collation = client.get_collation();
    let options = ExecuteOptions::new()
        .timeout_secs(timeout_secs)
        .cancel(&cancel_handle);

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
            let Some(dae) = stmt_state.dae.as_mut() else {
                error!("SQLParamData: DAE sequence vanished with its RPC open");
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
                drop(stmt_state);
                return_client_idle(dbc, statement_handle, client);
                return Some(SQL_ERROR);
            };
            // Back onto the *sequence*, not the statement: the sequence outlives
            // this call and its closing `take_dae` restores `StmtState` from
            // these fields, so writing them to the statement here would have
            // that restore overwrite the live plan with `None` -- leaving a
            // successful mixed execute unprepared and skipping the no-row drain
            // its `was_prepared` check gates.
            dae.restore_plan(prepared, orphaned);
            dae.fractional_truncated = fractional_truncated;
            dae.begin_streaming_phase(client, collation);
            None
        }
        Ok(StreamedParamStatus::Complete(_)) => {
            // Unreachable: this runs only while a streamed parameter is still
            // open, so the RPC cannot have completed. Torn down all the same --
            // the sequence is a husk by now, its client checked out and its plan
            // and collected values taken, so leaving it installed would keep the
            // statement in "Need Data" over state nothing can complete.
            error!("SQLParamData: deferred RPC completed despite a streamed parameter");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
            }
            Some(finish_execute_with_param_warning(
                dbc,
                stmt,
                statement_handle,
                client,
                "SQLParamData",
                fractional_truncated,
            ))
        }
        Err(e) => {
            error!(%e, "SQLParamData: opening the deferred RPC failed");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                // The sequence cannot be resumed: its client is checked out and
                // its plan, collected values and prebuilt parameters were all
                // taken to open the RPC. Leaving it installed would keep
                // `needs_data()` true after this call returns `SQL_ERROR`, so
                // the application could drive `SQLPutData` against a husk.
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.prepared = prepared;
                stmt_state.pending_unprepare = orphaned;
                stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
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
    let cancel_handle = match stmt.execution_cancel() {
        Ok(handle) => handle,
        Err(rc) => return rc,
    };
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
        let mut tvp_cells = std::mem::take(&mut dae.tvp_cells);
        let tvp_row_counts = std::mem::take(&mut dae.tvp_row_counts);
        let dae_params = dae.params().to_vec();
        let prebuilt = std::mem::take(&mut dae.prebuilt);
        let sql = dae.sql.take();
        let timeout_secs = dae.timeout_secs;
        let prepared = dae.take_prepared();
        let mut orphaned = dae.take_orphaned();

        let (params, fractional_truncated) = match rebuild_deferred_params(
            &mut stmt_state,
            prebuilt,
            &collected,
            &dae_params,
            &mut tvp_cells,
            &tvp_row_counts,
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
        (
            client,
            params,
            prepared,
            orphaned,
            sql,
            timeout_secs,
            fractional_truncated,
        )
    };

    let (client, params, mut prepared, mut orphaned, sql, timeout_secs, fractional_truncated) =
        taken;
    let Some(mut client) = client else {
        error!("SQLParamData: deferred sequence has no client to execute on");
        clear_exec_started(stmt);
        return SQL_ERROR;
    };

    let options = ExecuteOptions::new()
        .timeout_secs(timeout_secs)
        .cancel(&cancel_handle);
    let exec_result = match (prepared.as_mut(), sql) {
        (Some(plan), _) => dbc
            .runtime
            .block_on(client.execute_prepared(&mut plan.stmt, params, &mut orphaned, options))
            .map(|_| ()),
        (None, Some(sql)) => dbc
            .runtime
            .block_on(client.execute_sp_executesql(sql, params, options))
            .map(|_| ()),
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

    if let Err(e) = exec_result {
        error!(%e, "SQLParamData: deferred execute failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    finish_execute_with_param_warning(
        dbc,
        stmt,
        statement_handle,
        client,
        "SQLParamData",
        fractional_truncated,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NULL_HANDLE, SQL_VARCHAR, SqlLen};
    use crate::handles::stmt::{DaeParam, DaeState, DaeTarget};
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
    fn mixed_tvp_rows_enforce_each_cells_declared_dae_length() {
        use crate::api::exports::{SQLParamData, SQLPutData};
        use crate::api::odbc_types::{SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_SUCCESS};
        use mssql_tds::test_client_support::tds_client_from_tokens;

        for supplied in [0usize, 2, 3, 4] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let dbc = stmt.parent_dbc();
            let client = {
                let mut state = dbc.inner.lock().unwrap();
                state.wire.install(tds_client_from_tokens(Vec::new()));
                state.wire.take_lease_for_test(h.stmt)
            };
            let mut indicators = [1, SQL_LEN_DATA_AT_EXEC_OFFSET - 3];
            let mut first_value = b'a';
            let row = |index| {
                let mut param = DaeParam::unbounded(0, std::ptr::null_mut(), None);
                param.binding.column_size = 2;
                param.with_target(DaeTarget::TvpRowControl {
                    tvp_index: 0,
                    row_index: index,
                })
            };
            let mut bound = DaeParam::unbounded(0, (&raw mut first_value).cast(), None)
                .with_binding_types(SQL_C_CHAR, crate::api::odbc_types::SQL_VARCHAR)
                .with_target(DaeTarget::TvpCell {
                    tvp_index: 0,
                    row_index: 0,
                    column_index: 0,
                });
            bound.binding.column_size = 10;
            bound.binding.strlen_or_ind_ptr = &raw mut indicators[0];
            bound.binding.octet_length_ptr = &raw mut indicators[0];
            let mut deferred = bound.clone().with_target(DaeTarget::TvpCell {
                tvp_index: 0,
                row_index: 1,
                column_index: 0,
            });
            deferred.binding.strlen_or_ind_ptr = &raw mut indicators[1];
            deferred.binding.octet_length_ptr = &raw mut indicators[1];
            let mut params = vec![row(0), bound, row(1), deferred, row(2)];
            for param in &mut params {
                param.set_buffered(true);
            }
            stmt.inner.lock().unwrap().dae =
                Some(DaeState::new(client, None, None, params).deferred(Vec::new(), None, 0));
            let mut token = std::ptr::null_mut();
            assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
            assert_eq!(
                unsafe { SQLPutData(h.stmt, 1usize as SqlPointer, 2) },
                SQL_SUCCESS
            );
            assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
            assert_eq!(
                stmt.inner
                    .lock()
                    .unwrap()
                    .dae
                    .as_ref()
                    .unwrap()
                    .current_param()
                    .unwrap()
                    .expected_len,
                Some(3)
            );
            let bytes = [b'x'; 4];
            let rc = unsafe {
                SQLPutData(
                    h.stmt,
                    bytes.as_ptr().cast_mut().cast(),
                    SqlLen::try_from(supplied).unwrap(),
                )
            };
            if supplied > 3 {
                assert_eq!(rc, SQL_ERROR);
            } else {
                assert_eq!(rc, SQL_SUCCESS);
                assert_eq!(
                    unsafe { SQLParamData(h.stmt, &mut token) },
                    if supplied == 3 {
                        SQL_NEED_DATA
                    } else {
                        SQL_ERROR
                    }
                );
            }
            if supplied != 3 {
                assert_eq!(
                    stmt.inner.lock().unwrap().diag_records[0].sql_state,
                    ERR_DAE_LENGTH_MISMATCH.state
                );
            }
        }
    }

    fn tvp_array_dae(
        h: &TestHandles,
        values: &mut [i32; 2],
        indicators: &mut [SqlLen; 2],
        lengths: &mut [SqlLen; 2],
    ) -> DaeState {
        let tvp = crate::params::BoundParam {
            input_output_type: crate::api::odbc_types::SQL_PARAM_INPUT,
            c_type: crate::api::odbc_types::SQL_C_BINARY,
            sql_type: crate::api::odbc_types::SQL_SS_TABLE,
            column_size: 2,
            decimal_digits: 0,
            app_precision: 0,
            app_scale: 0,
            parameter_value_ptr: std::ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        };
        let column = crate::params::BoundParam {
            c_type: crate::api::odbc_types::SQL_C_SLONG,
            sql_type: crate::api::odbc_types::SQL_INTEGER,
            column_size: 10,
            buffer_length: std::mem::size_of::<i32>() as SqlLen,
            parameter_value_ptr: values.as_mut_ptr().cast(),
            strlen_or_ind_ptr: indicators.as_mut_ptr(),
            octet_length_ptr: indicators.as_mut_ptr(),
            ..tvp
        };
        let mut params = crate::api::exec_common::build_tvp_dae_params(
            0,
            &tvp,
            None,
            &[Some(column)],
            0,
            crate::api::odbc_types::SQL_BIND_BY_COLUMN,
        )
        .unwrap();
        for param in &mut params {
            if let Some(cell_binding) = param.tvp_cell_binding.as_mut() {
                cell_binding.binding.strlen_or_ind_ptr = indicators.as_mut_ptr();
                cell_binding.binding.octet_length_ptr = lengths.as_mut_ptr();
            }
        }
        params.push(DaeParam::unbounded(1, std::ptr::null_mut(), None));
        DaeState::new(
            crate::handles::wire::WireLease::detached_for_test(
                mssql_tds::test_client_support::tds_client_from_tokens(Vec::new()),
                h.stmt,
            ),
            None,
            None,
            params,
        )
        .deferred(Vec::new(), Some("SELECT ?".into()), 0)
    }

    #[test]
    fn tvp_dae_accepts_a_two_row_batch() {
        use crate::api::exports::{SQLParamData, SQLPutData};
        use crate::api::odbc_types::SQL_SUCCESS;
        use mssql_tds::datatypes::sqltypes::SqlType;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut values = [11, 999];
        let mut indicators = [std::mem::size_of::<i32>() as SqlLen; 2];
        let mut lengths = indicators;
        stmt.inner.lock().unwrap().dae = Some(tvp_array_dae(
            &h,
            &mut values,
            &mut indicators,
            &mut lengths,
        ));

        let mut token = std::ptr::null_mut();
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
        assert_eq!(
            unsafe { SQLPutData(h.stmt, 1usize as SqlPointer, 2) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().unwrap();
        assert_eq!(dae.tvp_row_counts.get(&0), Some(&2));
        assert_eq!(dae.tvp_cells[0].value, Some(SqlType::Int(Some(11))));
        assert_eq!(dae.tvp_cells[1].value, Some(SqlType::Int(Some(999))));
        assert_eq!(dae.tvp_cells[0].row_index, 0);
        assert_eq!(dae.tvp_cells[1].row_index, 1);
    }

    #[test]
    fn tvp_dae_refilled_single_row_batches_restart_at_array_row_zero() {
        use crate::api::exports::{SQLParamData, SQLPutData};
        use crate::api::odbc_types::SQL_SUCCESS;
        use mssql_tds::datatypes::sqltypes::SqlType;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut values = [11, 999];
        let mut indicators = [std::mem::size_of::<i32>() as SqlLen; 2];
        let mut lengths = indicators;
        stmt.inner.lock().unwrap().dae = Some(tvp_array_dae(
            &h,
            &mut values,
            &mut indicators,
            &mut lengths,
        ));

        let mut token = std::ptr::null_mut();
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
        assert_eq!(
            unsafe { SQLPutData(h.stmt, 1usize as SqlPointer, 1) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
        values[0] = 22;
        assert_eq!(values[0], 22);
        assert_eq!(
            unsafe { SQLPutData(h.stmt, 1usize as SqlPointer, 1) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().unwrap();
        assert_eq!(dae.tvp_row_counts.get(&0), Some(&2));
        assert_eq!(dae.tvp_cells[0].value, Some(SqlType::Int(Some(11))));
        assert_eq!(dae.tvp_cells[1].value, Some(SqlType::Int(Some(22))));
        assert_eq!(dae.tvp_cells[0].row_index, 0);
        assert_eq!(dae.tvp_cells[1].row_index, 1);
    }

    #[test]
    fn tvp_null_indicator_overrides_separate_dae_length_marker() {
        use crate::api::exports::{SQLParamData, SQLPutData};
        use crate::api::odbc_types::{SQL_DATA_AT_EXEC, SQL_NEED_DATA, SQL_SUCCESS};
        use mssql_tds::datatypes::sqltypes::SqlType;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut values = [11, 999];
        let mut indicators = [crate::api::odbc_types::SQL_NULL_DATA, 0];
        let mut lengths = [SQL_DATA_AT_EXEC, 0];
        stmt.inner.lock().unwrap().dae = Some(tvp_array_dae(
            &h,
            &mut values,
            &mut indicators,
            &mut lengths,
        ));

        let mut token = std::ptr::null_mut();
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);
        assert_eq!(
            unsafe { SQLPutData(h.stmt, 1usize as SqlPointer, 1) },
            SQL_SUCCESS
        );
        assert_eq!(unsafe { SQLParamData(h.stmt, &mut token) }, SQL_NEED_DATA);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().unwrap();
        assert_eq!(
            dae.tvp_cells[0].value,
            Some(SqlType::Int(None)),
            "the independent NULL indicator must win over SQL_DATA_AT_EXEC"
        );
        assert!(matches!(
            dae.current_param().map(|param| param.target),
            Some(DaeTarget::TvpRowControl { row_index: 1, .. })
        ));
    }

    #[test]
    fn tvp_dae_sequence_collects_mixed_cells_and_stops_at_false_row_marker() {
        use crate::api::exports::{SQLParamData as sql_param_data, SQLPutData as sql_put_data};
        use mssql_tds::test_client_support::tds_client_from_tokens;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut bound_value = 42i32;
        let mut null_indicator = crate::api::odbc_types::SQL_NULL_DATA;
        let mut cell_indicator = crate::api::odbc_types::SQL_DATA_AT_EXEC;
        let mut bound_cell = DaeParam::unbounded(0, (&raw mut bound_value).cast(), None)
            .with_binding_types(
                crate::api::odbc_types::SQL_C_SLONG,
                crate::api::odbc_types::SQL_INTEGER,
            )
            .with_target(DaeTarget::TvpCell {
                tvp_index: 0,
                row_index: 0,
                column_index: 0,
            });
        bound_cell.set_buffered(true);
        let mut null_cell = bound_cell.clone().with_target(DaeTarget::TvpCell {
            tvp_index: 0,
            row_index: 0,
            column_index: 1,
        });
        null_cell.binding.parameter_value_ptr = std::ptr::null_mut();
        null_cell.binding.strlen_or_ind_ptr = &mut null_indicator;
        null_cell.binding.octet_length_ptr = &mut null_indicator;
        let mut row0 = DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(
            DaeTarget::TvpRowControl {
                tvp_index: 0,
                row_index: 0,
            },
        );
        row0.binding.column_size = 2;
        row0.set_buffered(true);
        let mut cell0 =
            DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(DaeTarget::TvpCell {
                tvp_index: 0,
                row_index: 0,
                column_index: 2,
            });
        cell0.set_buffered(true);
        cell0.binding.strlen_or_ind_ptr = &mut cell_indicator;
        cell0.binding.octet_length_ptr = &mut cell_indicator;
        let mut row1 = DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(
            DaeTarget::TvpRowControl {
                tvp_index: 0,
                row_index: 1,
            },
        );
        row1.binding.column_size = 2;
        row1.set_buffered(true);
        let mut cell1 =
            DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(DaeTarget::TvpCell {
                tvp_index: 0,
                row_index: 1,
                column_index: 0,
            });
        cell1.set_buffered(true);
        let mut end = DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(
            DaeTarget::TvpRowControl {
                tvp_index: 0,
                row_index: 2,
            },
        );
        end.binding.column_size = 2;
        end.set_buffered(true);
        let mut next_parameter = DaeParam::unbounded(1, std::ptr::null_mut(), None);
        next_parameter.set_buffered(true);
        let dae = DaeState::new(
            crate::handles::wire::WireLease::detached_for_test(
                tds_client_from_tokens(Vec::new()),
                h.stmt,
            ),
            None,
            None,
            vec![
                row0,
                bound_cell,
                null_cell,
                cell0,
                row1,
                cell1,
                end,
                next_parameter,
            ],
        )
        .deferred(Vec::new(), Some("SELECT ?".into()), 0);
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(dae);
        }

        let mut token = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        assert_eq!(
            unsafe { sql_put_data(h.stmt, 1usize as SqlPointer, 1) },
            crate::api::odbc_types::SQL_SUCCESS
        );

        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        let cell_bytes = [42u8];
        assert_eq!(
            unsafe {
                sql_put_data(
                    h.stmt,
                    cell_bytes.as_ptr().cast_mut().cast(),
                    SqlLen::try_from(cell_bytes.len()).unwrap(),
                )
            },
            crate::api::odbc_types::SQL_SUCCESS
        );

        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);
        assert_eq!(
            unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), 0) },
            crate::api::odbc_types::SQL_SUCCESS
        );
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut token) }, SQL_NEED_DATA);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().unwrap();
        assert!(matches!(
            dae.current_param().unwrap().target,
            DaeTarget::Parameter
        ));
        assert_eq!(dae.tvp_row_counts.get(&0), Some(&1));
        assert_eq!(dae.tvp_cells.len(), 3);
        assert_eq!(
            dae.tvp_cells[0].value,
            Some(mssql_tds::datatypes::sqltypes::SqlType::Int(Some(42)))
        );
        assert_eq!(
            dae.tvp_cells[1].value,
            Some(mssql_tds::datatypes::sqltypes::SqlType::Int(None))
        );
        assert_eq!(dae.tvp_cells[2].bytes, cell_bytes);
        assert_eq!(dae.tvp_cells[0].row_index, 0);
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
    fn buffered_partial_utf8_overflow_is_reported_at_close() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let limit =
                crate::conversion::param_convert::dae_length_limit(SQL_C_CHAR, SQL_VARCHAR, 1)
                    .unwrap()
                    .unwrap();
            let collation = mssql_tds::token::tokens::SqlCollation {
                info: 0x0409,
                lcid_language_id: 0,
                col_flags: 0x40,
                sort_id: 0,
            };
            let mut param = DaeParam::unbounded(0, std::ptr::null_mut(), None)
                .with_binding_types(SQL_C_CHAR, SQL_VARCHAR)
                .with_length_limit(limit);
            param.transcode = Some(crate::conversion::param_convert::DaeTranscode::new(
                SQL_C_CHAR,
                SQL_VARCHAR,
                collation,
            ));
            let mut dae = DaeState::for_test(vec![param], Some(0));
            dae.deferred = true;
            dae.progress.put_data_called = true;
            dae.progress.bytes_sent = 1;
            dae.progress.buffer = vec![0xC3];
            dae.progress.carry = vec![0xC3];
            state.dae = Some(dae);
        }

        let mut p: SqlPointer = std::ptr::null_mut();
        assert_eq!(unsafe { sql_param_data(h.stmt, &mut p) }, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_PARAM_STRING_TRUNCATION.state
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
