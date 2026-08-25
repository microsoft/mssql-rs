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

use super::exec_common::{abort_dae_with_diag, fail_with_tds, return_client_idle};
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_NULL_DATA, SQL_SUCCESS, SqlHandle,
    SqlLen, SqlPointer, SqlReturn,
};
use crate::error::free_errors;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Supplies a data chunk for the current data-at-execution parameter.
///
/// # Safety
/// - `statement_handle` must be a valid `STMT` handle allocated by
///   `SQLAllocHandle`.
/// - `data_ptr`, when `strlen_or_ind` is a positive byte count, must be
///   readable for that many bytes and must remain valid for the duration of
///   this call.
/// - `data_ptr`, when `strlen_or_ind` is `SQL_NTS`, must be non-null, aligned
///   for the bound parameter's C type, and NUL-terminated within an allocation
///   it owns: the terminator search reads `u16` units for `SQL_C_WCHAR` and
///   `u8` units otherwise, and runs off the end of the allocation if no
///   terminator is present.
pub(crate) unsafe fn sql_put_data(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    strlen_or_ind: SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?data_ptr,
        strlen_or_ind,
        "SQLPutData called"
    );
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

        // A parameter is only open once `SQLParamData` has handed the
        // application its token; calling before that is invalid sequencing.
        if stmt_state
            .dae
            .as_ref()
            .is_none_or(|dae| dae.cursor.is_none())
        {
            error!("SQLPutData: called outside an open data-at-execution parameter (HY010)");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
    }

    let is_null_put = strlen_or_ind == SQL_NULL_DATA || (data_ptr.is_null() && strlen_or_ind == 0);

    // ── Null data: mark the parameter as SQL NULL ───────────────────────────
    if is_null_put {
        // A parameter that already received a value contribution cannot become
        // NULL. Any prior `SQLPutData` in this window supplied a present value —
        // including a zero-length one, which commits the parameter to empty
        // rather than NULL — so the test is whether a chunk call happened at
        // all, not whether it carried bytes.
        {
            let Ok(stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned checking null concatenation");
                return SQL_ERROR;
            };
            if stmt_state
                .dae
                .as_ref()
                .is_some_and(|dae| dae.progress.put_data_called)
            {
                drop(stmt_state);
                error!("SQLPutData: SQL_NULL_DATA after a value contribution (HY020)");
                return abort_dae_with_diag(
                    dbc,
                    stmt,
                    statement_handle,
                    ERR_ATTEMPT_TO_CONCATENATE_NULL,
                );
            }
        }

        let write_result = {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned taking the DAE client for a null write");
                return SQL_ERROR;
            };
            // Taken rather than borrowed so the failure arm can move the client
            // into the error path without a second lookup that would have to be
            // unwrapped: a panic here would poison this mutex and strand the
            // parked client.
            let checked_out = stmt_state.dae.as_mut().and_then(|dae| {
                // Checked out before the progress flags move. If the client is
                // unavailable nothing is written, and a rejected call must not
                // leave the parameter marked NULL — same contract as the
                // byte-total guards below.
                let client = dae.checkout_client()?;
                dae.progress.put_data_called = true;
                dae.progress.is_null = true;
                Some(client)
            });
            let Some(mut client) = checked_out else {
                error!("SQLPutData: DAE client is unavailable — internal state corruption");
                post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                return SQL_ERROR;
            };
            match client.write_streamed_null() {
                Ok(()) => {
                    if let Some(dae) = stmt_state.dae.as_mut() {
                        dae.return_client(client);
                    }
                    Ok(())
                }
                Err(error) => {
                    // If the null write fails, end the sequence and carry the
                    // client out to the error path.
                    let parked = stmt_state.take_dae();
                    debug_assert!(parked.is_none(), "the client was checked out above");
                    stmt_state.clear_state(crate::handles::stmt::STMT_STATE_EXEC_STARTED);
                    Err((client, error))
                }
            }
        };

        return match write_result {
            Ok(()) => SQL_SUCCESS,
            Err((mut client, e)) => {
                error!(%e, "SQLPutData: write_streamed_null failed");
                // Not every rejection tears the stream down — a sequencing
                // error re-parks the message and leaves the write active. The
                // client is about to go back to the idle pool, so discard any
                // half-written request first; this is a no-op once the stream
                // has already aborted itself.
                dbc.runtime.block_on(client.cancel_streamed_write());
                fail_with_tds(dbc, stmt, statement_handle, client, &e)
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
            stmt_state.dae_current_c_type().unwrap_or_default()
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

    // (NULL, 0) and SQL_NULL_DATA are the only legal null-pointer forms and
    // both returned above, so a null pointer with bytes to send would reach
    // `from_raw_parts(null, n)`. That is undefined behavior in Rust before a
    // single byte is read, so it cannot be left to the driver manager the way
    // msodbcsql does. Checked ahead of the counters below: a rejected call must
    // not move the parameter's byte total.
    if data_ptr.is_null() && byte_count > 0 {
        error!("SQLPutData: null data pointer with length {strlen_or_ind} (HY009)");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_INVALID_NULL_POINTER);
        }
        return SQL_ERROR;
    }

    // The client is checked out in the same lock as the counter update, and
    // before it, so a call that cannot write leaves the parameter's byte total
    // where it was. A zero-length chunk writes nothing and needs no client.
    let checked_out = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLPutData: stmt mutex poisoned updating DAE byte count");
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLPutData: DAE sequence ended between locks");
            return SQL_ERROR;
        };
        // The parameter was already signalled NULL; appending data to it is the
        // mirror of the guard above.
        if dae.progress.is_null {
            drop(stmt_state);
            error!("SQLPutData: data chunk after SQL_NULL_DATA (HY020)");
            return abort_dae_with_diag(
                dbc,
                stmt,
                statement_handle,
                ERR_ATTEMPT_TO_CONCATENATE_NULL,
            );
        }
        let new_total = dae.progress.bytes_sent.saturating_add(byte_count);
        if let Some(expected) = dae.current_param().and_then(|param| param.expected_len)
            && new_total > expected
        {
            drop(stmt_state);
            error!("SQLPutData: DAE data exceeds SQL_LEN_DATA_AT_EXEC length");
            return abort_dae_with_diag(dbc, stmt, statement_handle, ERR_DAE_LENGTH_MISMATCH);
        }

        let client = if byte_count == 0 {
            None
        } else {
            match stmt_state
                .dae
                .as_mut()
                .and_then(|dae| dae.checkout_client())
            {
                Some(client) => Some(client),
                None => {
                    error!("SQLPutData: DAE client is unavailable — internal state corruption");
                    post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                    return SQL_ERROR;
                }
            }
        };

        if let Some(dae) = stmt_state.dae.as_mut() {
            dae.progress.bytes_sent = new_total;
            dae.progress.put_data_called = true;
        }
        client
    };

    let Some(mut client) = checked_out else {
        // Zero-length chunk with a non-null pointer supplies an empty value.
        // NULL/0 is handled above as SQL NULL to match msodbcsql.
        return SQL_SUCCESS;
    };

    // Safety: caller guarantees data_ptr is readable for byte_count bytes.
    let chunk = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, byte_count) };

    let write_result = dbc.runtime.block_on(client.write_streamed_chunk(chunk));

    match write_result {
        Ok(()) => {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned returning the DAE client after a write");
                // Client is now floating — return it to idle to avoid a leak.
                dbc.runtime.block_on(client.cancel_streamed_write());
                return_client_idle(dbc, statement_handle, client);
                return SQL_ERROR;
            };
            match stmt_state.dae.as_mut() {
                Some(dae) => {
                    dae.return_client(client);
                    SQL_SUCCESS
                }
                None => {
                    error!("SQLPutData: DAE sequence ended while the client was checked out");
                    drop(stmt_state);
                    dbc.runtime.block_on(client.cancel_streamed_write());
                    return_client_idle(dbc, statement_handle, client);
                    SQL_ERROR
                }
            }
        }
        Err(e) => {
            // A mid-stream I/O failure aborts the write internally, but the
            // caller-error rejections (oversized chunk, no active parameter)
            // return before touching the stream and leave it active. Cancel
            // unconditionally so the client never re-enters the idle pool with
            // a half-written request parked on it; this is a no-op when the
            // stream already aborted.
            error!(%e, "SQLPutData: write_streamed_chunk failed");
            dbc.runtime.block_on(client.cancel_streamed_write());
            // `take_dae` restores the prepared plan under the same lock that
            // ends the sequence, so the statement is never observable as idle
            // but unprepared.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                let parked = stmt_state.take_dae();
                debug_assert!(parked.is_none(), "the client is checked out by this call");
                stmt_state.clear_state(crate::handles::stmt::STMT_STATE_EXEC_STARTED);
            }
            fail_with_tds(dbc, stmt, statement_handle, client, &e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NULL_HANDLE};
    use crate::handles::stmt::{DaeParam, DaeState};
    use crate::test_support::TestHandles;

    /// A sequence with one parameter, already opened by `SQLParamData`.
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
        // The sequence is active but no parameter is open yet — SQLPutData must
        // reject this as a sequencing error.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(DaeState::for_test(Vec::new(), None));
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
            state.dae = Some(open_dae(None));
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
    fn null_pointer_with_positive_length_returns_hy009() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(open_dae(None));
        }
        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), 5) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_INVALID_NULL_POINTER.state
        );
        // Rejecting must not charge the parameter for bytes never sent.
        let dae = state.dae.as_ref().expect("sequence still active");
        assert_eq!(dae.progress.bytes_sent, 0);
        assert!(!dae.progress.put_data_called);
    }

    #[test]
    fn zero_length_non_null_chunk_marks_current_param_supplied() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(open_dae(None));
        }

        let mut byte = 0u8;
        let ret = unsafe { sql_put_data(h.stmt, (&mut byte as *mut u8).cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().expect("sequence still active");
        assert!(dae.progress.put_data_called);
        assert!(!dae.progress.is_null);
        assert_eq!(dae.progress.bytes_sent, 0);
    }

    #[test]
    fn nts_chunk_length_is_counted_before_terminator() {
        // Counting the terminator would make "abc\0" four bytes. The length
        // guard is what makes that observable without a live client: declared
        // at 3, a correct count fits and a terminator-inclusive one overruns.
        // A mismatch aborts the sequence, so `dae` being gone is the tell.
        for (declared, expect_overrun) in [(3usize, false), (2usize, true)] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            {
                let mut state = stmt.inner.lock().unwrap();
                state.dae = Some(open_dae(Some(declared)));
                state.bound_params.push(Some(crate::params::BoundParam {
                    input_output_type: crate::api::odbc_types::SQL_PARAM_INPUT,
                    c_type: SQL_C_CHAR,
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
            assert_eq!(ret, SQL_ERROR, "declared {declared}: no client to write to");

            let state = stmt.inner.lock().unwrap();
            assert_eq!(
                state.dae.is_none(),
                expect_overrun,
                "declared {declared}: three data bytes must fit in 3 but not in 2"
            );
        }
    }

    /// ODBC states `SQLPutData` lengths in bytes for every C type, so a
    /// two-unit `SQL_C_WCHAR` string is four bytes, not two. Observed the same
    /// way as the narrow case above: declared at 4 it fits, declared at 3 it
    /// overruns and the sequence is torn down.
    #[test]
    fn wide_nts_chunk_length_is_counted_in_bytes() {
        for (declared, expect_overrun) in [(4usize, false), (3usize, true)] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            {
                let mut state = stmt.inner.lock().unwrap();
                state.dae = Some(open_dae(Some(declared)));
                state.bound_params.push(Some(crate::params::BoundParam {
                    input_output_type: crate::api::odbc_types::SQL_PARAM_INPUT,
                    c_type: crate::api::odbc_types::SQL_C_WCHAR,
                    sql_type: crate::api::odbc_types::SQL_WVARCHAR,
                    column_size: 0,
                    decimal_digits: 0,
                    parameter_value_ptr: std::ptr::null_mut(),
                    buffer_length: 0,
                    strlen_or_ind_ptr: std::ptr::null_mut(),
                }));
            }

            let mut units: Vec<u16> = "hi".encode_utf16().chain(std::iter::once(0)).collect();
            let ret = unsafe { sql_put_data(h.stmt, units.as_mut_ptr().cast(), SQL_NTS as SqlLen) };
            assert_eq!(ret, SQL_ERROR, "declared {declared}: no client to write to");

            let state = stmt.inner.lock().unwrap();
            assert_eq!(
                state.dae.is_none(),
                expect_overrun,
                "declared {declared}: two wide units must count as four bytes"
            );
        }
    }

    #[test]
    fn failed_client_checkout_leaves_progress_untouched() {
        // A call that cannot write must not move the parameter's counters, or a
        // retry would resume from a byte total the server never received.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(open_dae(None));
        }

        let mut bytes = b"abc".to_vec();
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), 3) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().expect("sequence still active");
        assert_eq!(dae.progress.bytes_sent, 0);
        assert!(!dae.progress.put_data_called);
    }

    #[test]
    fn over_declared_dae_length_returns_22026() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(open_dae(Some(2)));
        }

        let mut bytes = *b"abc";
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), 3) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_DAE_LENGTH_MISMATCH.state
        );
        assert!(!state.needs_data());
    }

    #[test]
    fn null_after_value_chunks_returns_hy020() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut dae = open_dae(None);
            dae.progress.put_data_called = true;
            dae.progress.bytes_sent = 3;
            state.dae = Some(dae);
        }

        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), SQL_NULL_DATA) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_ATTEMPT_TO_CONCATENATE_NULL.state
        );
        assert!(!state.needs_data());
    }

    /// A zero-length chunk with a non-null pointer is a present empty value, so
    /// the parameter is already committed to being non-NULL even though no
    /// bytes were sent.
    #[test]
    fn null_after_empty_value_chunk_returns_hy020() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut dae = open_dae(None);
            dae.progress.put_data_called = true;
            state.dae = Some(dae);
        }

        let ret = unsafe { sql_put_data(h.stmt, std::ptr::null_mut(), SQL_NULL_DATA) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_ATTEMPT_TO_CONCATENATE_NULL.state
        );
        assert!(!state.needs_data());
    }

    #[test]
    fn value_chunk_after_null_returns_hy020() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut dae = open_dae(None);
            dae.progress.is_null = true;
            state.dae = Some(dae);
        }

        let mut bytes = *b"abc";
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), 3) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_ATTEMPT_TO_CONCATENATE_NULL.state
        );
        assert!(!state.needs_data());
    }
}
