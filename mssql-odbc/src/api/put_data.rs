// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLPutData` — supplies a data chunk for the
//! currently-open data-at-execution parameter.
//!
//! This function is called zero or more times between consecutive
//! `SQLParamData` calls.  Each call appends bytes to the streaming PLP value
//! on the TDS wire.  A `strlen_or_ind` of `SQL_NULL_DATA` marks the entire
//! parameter as SQL `NULL` (no chunks); any other valid value is a byte count
//! for `data_ptr`.

use tracing::{debug, error};

use super::exec_common::{fail_with_tds, return_client_idle};
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_NULL_DATA, SQL_SUCCESS, SqlHandle,
    SqlLen, SqlPointer, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_NEED_DATA;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Supplies a data chunk for the current data-at-execution parameter.
///
/// # Safety
/// - `statement_handle` must be a valid `STMT` handle allocated by
///   `SQLAllocHandle`.
/// - `data_ptr`, when `strlen_or_ind` is a positive byte count, must be
///   readable for that many bytes and must remain valid for the duration of
///   this call.
pub(crate) unsafe fn sql_put_data(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    strlen_or_ind: SqlLen,
) -> SqlReturn {
    debug!(?statement_handle, strlen_or_ind, "SQLPutData called");
    crate::ffi_entry!("SQLPutData", unsafe {
        sql_put_data_impl(statement_handle, data_ptr, strlen_or_ind)
    })
}

unsafe fn sql_put_data_impl(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    strlen_or_ind: SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLPutData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLPutData: handle is not a STMT"
    );

    unsafe { sql_put_data_safe(statement_handle, stmt, data_ptr, strlen_or_ind) }
}

fn current_dae_c_type(stmt_state: &crate::handles::stmt::StmtState) -> Option<i16> {
    let param_idx = stmt_state
        .dae_param_indices
        .get(stmt_state.dae_current_idx)
        .copied()?;
    stmt_state
        .bound_params
        .get(param_idx)?
        .as_ref()
        .map(|p| p.c_type)
}

unsafe fn nts_byte_count(data_ptr: SqlPointer, c_type: i16) -> usize {
    if c_type == SQL_C_WCHAR {
        let ptr = data_ptr as *const u16;
        let mut units = 0usize;
        while unsafe { *ptr.add(units) } != 0 {
            units += 1;
        }
        units * std::mem::size_of::<u16>()
    } else {
        let ptr = data_ptr as *const u8;
        let mut bytes = 0usize;
        while unsafe { *ptr.add(bytes) } != 0 {
            bytes += 1;
        }
        bytes
    }
}

fn abort_dae_with_diag(
    dbc: &crate::handles::DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    diag: DiagMsg,
) -> SqlReturn {
    let (client, prepared, orphaned) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLPutData: stmt mutex poisoned aborting DAE sequence");
            return SQL_ERROR;
        };
        post_diag(&mut stmt_state, diag);
        let client = stmt_state.dae_client.take();
        let prepared = stmt_state.dae_prepared.take();
        let orphaned = stmt_state.dae_orphaned.take();
        stmt_state.reset_dae();
        stmt_state.clear_state(crate::handles::stmt::STMT_STATE_EXEC_STARTED);
        (client, prepared, orphaned)
    };

    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.prepared = prepared;
        stmt_state.pending_unprepare = orphaned;
    }

    if let Some(mut client) = client {
        dbc.runtime.block_on(client.cancel_streamed_write());
        return_client_idle(dbc, statement_handle, client);
    }
    SQL_ERROR
}

unsafe fn sql_put_data_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    data_ptr: SqlPointer,
    strlen_or_ind: SqlLen,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    // ── Validate state ──────────────────────────────────────────────────────
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLPutData: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if !stmt_state.has_state(STMT_STATE_NEED_DATA) {
            error!("SQLPutData: called without an active data-at-execution sequence (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }

        if stmt_state.dae_param_data_first {
            // SQLPutData was called before the first SQLParamData — invalid
            // sequencing.
            error!("SQLPutData: called before the first SQLParamData (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
    }

    let is_null_put = strlen_or_ind == SQL_NULL_DATA || (data_ptr.is_null() && strlen_or_ind == 0);

    // ── Null data: mark the parameter as SQL NULL ───────────────────────────
    if is_null_put {
        let write_result = {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned taking dae_client for null write");
                return SQL_ERROR;
            };
            stmt_state.dae_current_put_data_called = true;
            stmt_state.dae_current_is_null = true;
            match stmt_state.dae_client.as_mut() {
                Some(c) => match c.write_streamed_null() {
                    Ok(()) => Some(Ok(())),
                    Err(error) => {
                        // If the null write fails, move the client into the
                        // error path after its stream was aborted.
                        let client = stmt_state.dae_client.take().unwrap();
                        let prepared = stmt_state.dae_prepared.take();
                        let orphaned = stmt_state.dae_orphaned.take();
                        stmt_state.reset_dae();
                        stmt_state.clear_state(crate::handles::stmt::STMT_STATE_EXEC_STARTED);
                        Some(Err((client, prepared, orphaned, error)))
                    }
                },
                None => {
                    // DAE client missing — internal error.
                    None
                }
            }
        };

        return match write_result {
            Some(Ok(())) => SQL_SUCCESS,
            Some(Err((client, prepared, orphaned, e))) => {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    stmt_state.prepared = prepared;
                    stmt_state.pending_unprepare = orphaned;
                }
                error!(%e, "SQLPutData: write_streamed_null failed");
                fail_with_tds(dbc, stmt, statement_handle, client, &e)
            }
            None => {
                error!("SQLPutData: dae_client is None — internal state corruption");
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                }
                SQL_ERROR
            }
        };
    }

    // ── Positive (or zero) length: stream the bytes ─────────────────────────
    let byte_count = if strlen_or_ind == SQL_NTS as SqlLen {
        if data_ptr.is_null() {
            error!("SQLPutData: SQL_NTS with null data pointer");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_diag(&mut stmt_state, ERR_INVALID_NULL_POINTER);
            }
            return SQL_ERROR;
        }
        let c_type = {
            let Ok(stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned resolving current C type");
                return SQL_ERROR;
            };
            current_dae_c_type(&stmt_state).unwrap_or_default()
        };
        unsafe { nts_byte_count(data_ptr, c_type) }
    } else if strlen_or_ind < 0 {
        // Negative values other than SQL_NULL_DATA are invalid for input.
        error!("SQLPutData: invalid strlen_or_ind {strlen_or_ind}");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
        }
        return SQL_ERROR;
    } else {
        strlen_or_ind as usize
    };

    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLPutData: stmt mutex poisoned updating DAE byte count");
            return SQL_ERROR;
        };
        let new_total = stmt_state.dae_current_bytes_sent.saturating_add(byte_count);
        if let Some(expected) = stmt_state
            .dae_expected_lengths
            .get(stmt_state.dae_current_idx)
            .and_then(|v| *v)
            && new_total > expected
        {
            drop(stmt_state);
            error!("SQLPutData: DAE data exceeds SQL_LEN_DATA_AT_EXEC length");
            return abort_dae_with_diag(dbc, stmt, statement_handle, ERR_DAE_LENGTH_MISMATCH);
        }
        stmt_state.dae_current_bytes_sent = new_total;
        stmt_state.dae_current_put_data_called = true;
    }

    if byte_count == 0 {
        // Zero-length chunk with a non-null pointer supplies an empty value.
        // NULL/0 is handled above as SQL NULL to match msodbcsql.
        return SQL_SUCCESS;
    }

    // Safety: caller guarantees data_ptr is readable for byte_count bytes.
    let chunk = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, byte_count) };

    // Take the client out of stmt for the async write, then put it back.
    let mut client = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLPutData: stmt mutex poisoned taking dae_client");
            return SQL_ERROR;
        };
        match stmt_state.dae_client.take() {
            Some(c) => c,
            None => {
                error!("SQLPutData: dae_client is None — internal state corruption");
                post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                return SQL_ERROR;
            }
        }
    };

    let write_result = dbc.runtime.block_on(client.write_streamed_chunk(chunk));

    match write_result {
        Ok(()) => {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned returning dae_client after write");
                // Client is now floating — return it to idle to avoid a leak.
                return_client_idle(dbc, statement_handle, client);
                return SQL_ERROR;
            };
            stmt_state.dae_client = Some(client);
            SQL_SUCCESS
        }
        Err(e) => {
            // The write failed; abort_streamed_write was called internally.
            // Clean up the DAE sequence.
            error!(%e, "SQLPutData: write_streamed_chunk failed");
            let (prepared, orphaned) = {
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    let p = stmt_state.dae_prepared.take();
                    let o = stmt_state.dae_orphaned.take();
                    stmt_state.reset_dae();
                    stmt_state.clear_state(crate::handles::stmt::STMT_STATE_EXEC_STARTED);
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
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NULL_HANDLE};
    use crate::handles::stmt::STMT_STATE_NEED_DATA;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_put_data(SQL_NULL_HANDLE, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn without_need_data_state_returns_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), SQL_NULL_DATA) };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
    }

    #[test]
    fn before_first_param_data_returns_hy010() {
        // STMT_STATE_NEED_DATA is set but dae_param_data_first is true —
        // SQLPutData must still reject this as a sequencing error.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = true;
        }
        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), SQL_NULL_DATA) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
    }

    #[test]
    fn invalid_negative_strlen_returns_hy090() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
        }
        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), -5) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_INVALID_STRING_OR_BUFFER_LENGTH.state
        );
    }

    #[test]
    fn zero_length_non_null_chunk_marks_current_param_supplied() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
        }

        let mut byte = 0u8;
        let ret = unsafe { sql_put_data(h.stmt, (&mut byte as *mut u8).cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);

        let state = stmt.inner.lock().unwrap();
        assert!(state.dae_current_put_data_called);
        assert!(!state.dae_current_is_null);
        assert_eq!(state.dae_current_bytes_sent, 0);
    }

    #[test]
    fn nts_chunk_length_is_counted_before_terminator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
            state.dae_param_indices.push(0);
            state.bound_params.push(Some(crate::params::BoundParam {
                input_output_type: crate::api::odbc_types::SQL_PARAM_INPUT,
                c_type: SQL_C_CHAR,
                c_type_defaulted: false,
                sql_type: crate::api::odbc_types::SQL_VARCHAR,
                column_size: 0,
                decimal_digits: 0,
                parameter_value_ptr: std::ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: std::ptr::null_mut(),
            }));
        }

        let mut bytes = b"abc\0".to_vec();
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), SQL_NTS as SqlLen) };
        assert_eq!(
            ret, SQL_ERROR,
            "no TDS client is present for the actual write"
        );

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.dae_current_bytes_sent, 3);
        assert!(state.dae_current_put_data_called);
    }

    #[test]
    fn over_declared_dae_length_returns_22026() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA);
            state.dae_param_data_first = false;
            state.dae_expected_lengths.push(Some(2));
        }

        let mut bytes = *b"abc";
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), 3) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_DAE_LENGTH_MISMATCH.state
        );
        assert!(!state.has_state(STMT_STATE_NEED_DATA));
    }
}
