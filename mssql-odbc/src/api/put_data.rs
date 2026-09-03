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

use std::borrow::Cow;

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

        // A deferred sequence has no open request to signal NULL on: the flag is
        // recorded and `SQLParamData` builds a typed NULL from it, exactly as a
        // materialized parameter with a `SQL_NULL_DATA` indicator produces one.
        {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned marking a buffered parameter NULL");
                return SQL_ERROR;
            };
            if let Some(dae) = stmt_state.dae.as_mut()
                && dae.deferred
            {
                dae.progress.put_data_called = true;
                dae.progress.is_null = true;
                return SQL_SUCCESS;
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
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLPutData: stmt mutex poisoned resolving current C type");
                return SQL_ERROR;
            };
            // Sizing an SQL_NTS chunk needs the parameter's bound C type, and
            // there is no safe default: 0 is not "unknown" but a value that
            // scans the buffer for a single terminating byte, so a lost
            // SQL_C_WCHAR binding would silently stream one byte of a wide
            // string. `dae_current_c_type()` reads the `DaeParam` snapshot
            // taken at execute time, so `SQLFreeStmt(SQL_RESET_PARAMS)`
            // clearing `bound_params` mid-sequence no longer reaches this
            // guard at all (`nts_uses_the_snapshotted_c_type_with_bound_params_cleared`
            // asserts that). What remains reachable here is no current
            // parameter -- the sequence ended or never had one -- so refuse
            // rather than guess.
            let Some(c_type) = stmt_state.dae_current_c_type() else {
                error!("SQLPutData: open data-at-execution parameter has no snapshotted C type");
                post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                return SQL_ERROR;
            };
            c_type
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
        let app_total = dae.progress.bytes_sent.saturating_add(byte_count);
        if let Some(expected) = dae.current_param().and_then(|param| param.expected_len)
            && app_total > expected
        {
            drop(stmt_state);
            error!("SQLPutData: DAE data exceeds SQL_LEN_DATA_AT_EXEC length");
            return abort_dae_with_diag(dbc, stmt, statement_handle, ERR_DAE_LENGTH_MISMATCH);
        }

        let plan = dae.current_param().map(|param| param.plan);
        let Some(plan) = plan else {
            error!("SQLPutData: open data-at-execution parameter has no plan");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        };

        let will_buffer = plan.is_buffered() || dae.deferred;

        // `SQL_DATA_AT_EXEC` declares no total, so nothing bounds how large a
        // chunk can claim to be, and every allocation that follows --
        // `extend_from_slice` on the accumulator, the conversion buffer inside
        // `DaeTranscode::push` -- allocates infallibly and would abort the whole
        // host process on a failure it cannot report. `try_reserve` turns that
        // into a diagnostic the application can act on instead. Checked against
        // `byte_count` directly and before the slice below is built, so a length
        // this process could never satisfy never has to construct a slice
        // claiming that many bytes are valid just to read its length back out,
        // and before any client is checked out, so an unsatisfiable reservation
        // abandons the sequence the way the `is_null` and `expected_len` guards
        // above do rather than looking like the retriable "something else holds
        // this sequence" failure further down. Reserved on whichever accumulator
        // receives the bytes: `buffer` for a value converted whole at close,
        // `carry` for one converted on the way out -- `push` moves that capacity
        // into its own conversion buffer.
        if byte_count > 0 {
            let target = if will_buffer {
                &mut dae.progress.buffer
            } else {
                &mut dae.progress.carry
            };
            if target.try_reserve(byte_count).is_err() {
                drop(stmt_state);
                error!(
                    "SQLPutData: failed to reserve {byte_count} bytes for a data-at-execution value (HY001)"
                );
                return abort_dae_with_diag(dbc, stmt, statement_handle, ERR_MEMORY_ALLOCATION);
            }
        }

        // Safety: the caller guarantees `data_ptr` is readable for `byte_count`
        // bytes; the null and zero cases returned above.
        let chunk: &[u8] = if byte_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(data_ptr as *const u8, byte_count) }
        };

        // `ColumnSize` bounds the accumulated value, and it is applied here -
        // before the streamed/buffered split and against the *application*
        // buffer - exactly where msodbcsql applies `ValidatePutDataLength`
        // (`odbc/sqlccmd.cpp:4571`), so the `22001` lands on the `SQLPutData`
        // that overflows rather than at close (AB#47590).
        //
        // Measured against the *retained* running total, not the application's:
        // trimmed padding must not consume the declaration's budget. The two
        // totals are tracked separately because the declared-length promise
        // above counts what the application supplied.
        //
        // Counted in the bound's own unit, which is not always bytes -- a
        // `SQL_C_CHAR` buffer is measured in the UTF-16 units the materialized
        // path uses -- so the running total accumulates what `fit` reports
        // rather than the length of what it kept.
        let retained_before = dae.progress.retained_units;
        // Borrowed, not copied: `fit` returns a prefix of `chunk`, and the
        // buffered branch copies it straight into the capacity reserved above,
        // so a chunk is never materialized into a second heap buffer that the
        // `try_reserve` guard does not cover and that would double peak usage.
        let (fitted, consumed): (&[u8], usize) =
            match dae.current_param().and_then(|param| param.length_limit) {
                Some(limit) if !chunk.is_empty() => match limit.fit(chunk, retained_before) {
                    Ok(fitted) => fitted,
                    Err(e) => {
                        drop(stmt_state);
                        error!("SQLPutData: value exceeds ColumnSize (22001)");
                        return abort_dae_with_diag(dbc, stmt, statement_handle, e.diag());
                    }
                },
                _ => (chunk, chunk.len()),
            };
        let retained_total = retained_before.saturating_add(consumed);

        // A buffered parameter never touches the wire here: it accumulates and
        // is converted whole when `SQLParamData` closes it. In a deferred
        // sequence there is no open request at all, so every parameter
        // accumulates, whatever its own plan says.
        if will_buffer {
            if let Some(dae) = stmt_state.dae.as_mut() {
                dae.progress.buffer.extend_from_slice(fitted);
                dae.progress.bytes_sent = app_total;
                dae.progress.retained_units = retained_total;
                dae.progress.put_data_called = true;
            }
            return SQL_SUCCESS;
        }

        // A transcoded stream converts on the way out, holding back a character
        // that this chunk ended part-way through. An untranscoded one is already
        // the wire's bytes, so it is forwarded borrowed.
        let transcode = dae.current_param().and_then(|param| param.transcode);
        // `push` consumes the carry it is handed, so the checkout below can fail
        // after the partial character it held is already gone -- and that
        // failure is the retriable "something else holds this sequence" one, so
        // the retry would decode the continuation bytes alone and emit U+FFFD.
        // The carry is at most one incomplete character, so keeping a copy
        // across the checkout costs a few bytes and makes the failure free of
        // side effects.
        let mut carry_restore: Option<Vec<u8>> = None;
        let outgoing: Cow<'_, [u8]> = match transcode {
            Some(transcode) => match stmt_state.dae.as_mut() {
                Some(dae) => {
                    let mut carry = std::mem::take(&mut dae.progress.carry);
                    carry_restore = Some(carry.clone());
                    let out = transcode.push(&mut carry, fitted);
                    dae.progress.carry = carry;
                    Cow::Owned(out)
                }
                None => {
                    error!("SQLPutData: DAE sequence ended between locks");
                    return SQL_ERROR;
                }
            },
            None => Cow::Borrowed(fitted),
        };

        let client = if outgoing.is_empty() {
            None
        } else {
            match stmt_state
                .dae
                .as_mut()
                .and_then(|dae| dae.checkout_client())
            {
                Some(client) => Some(client),
                None => {
                    // Put the partial character back, so a retry sees exactly
                    // the state this call found.
                    if let (Some(restore), Some(dae)) = (carry_restore, stmt_state.dae.as_mut()) {
                        dae.progress.carry = restore;
                    }
                    error!("SQLPutData: DAE client is unavailable — internal state corruption");
                    post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
                    return SQL_ERROR;
                }
            }
        };

        if let Some(dae) = stmt_state.dae.as_mut() {
            dae.progress.bytes_sent = app_total;
            dae.progress.retained_units = retained_total;
            dae.progress.put_data_called = true;
        }
        client.map(|client| (client, outgoing))
    };

    let Some((mut client, outgoing)) = checked_out else {
        // Zero-length chunk with a non-null pointer supplies an empty value, as
        // does one held entirely in the transcoder pending its continuation.
        // NULL/0 is handled above as SQL NULL to match msodbcsql.
        return SQL_SUCCESS;
    };

    let write_result = dbc.runtime.block_on(client.write_streamed_chunk(&outgoing));

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
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_NULL_HANDLE, SQL_VARCHAR};
    use crate::handles::stmt::{DaeParam, DaeState};
    use crate::test_support::TestHandles;

    /// A sequence with one parameter, already opened by `SQLParamData`.
    ///
    /// Bound narrow-to-narrow so `SQL_NTS` sizing has a single-byte C type to
    /// read, which is what `nts_uses_the_snapshotted_c_type_with_bound_params_cleared`
    /// checks the snapshot supplies.
    fn open_dae(expected_len: Option<usize>) -> DaeState {
        DaeState::for_test(
            vec![
                DaeParam::unbounded(0, std::ptr::null_mut(), expected_len)
                    .with_binding_types(SQL_C_CHAR, SQL_VARCHAR),
            ],
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

    /// The hazard the missing-binding guard exists to prevent: a defaulted C
    /// type of 0 is not "unknown", it takes the narrow branch. On a UTF-16
    /// buffer the scan stops at the first zero byte, so `"AB"` measures 1 byte
    /// instead of 4 and the rest of the string is dropped with no diagnostic.
    /// `SQLPutData` must refuse the call rather than default the C type.
    #[test]
    fn nts_byte_count_narrows_a_wide_buffer_when_the_c_type_defaults() {
        let wide: [u16; 3] = [0x0041, 0x0042, 0x0000];
        let ptr = wide.as_ptr() as *mut std::ffi::c_void;
        assert_eq!(unsafe { nts_byte_count(ptr, SQL_C_WCHAR) }, 4);
        assert_eq!(unsafe { nts_byte_count(ptr, 0) }, 1);
    }

    /// `SQLFreeStmt(SQL_RESET_PARAMS)` can clear `bound_params` while a
    /// data-at-execution sequence is still open. Sizing an `SQL_NTS` chunk no
    /// longer needs that live binding: `dae_current_c_type()` reads
    /// `DaeParam::c_type`, snapshotted at execute time, so the call reaches
    /// the same "no client parked" state
    /// `failed_client_checkout_leaves_progress_untouched` covers, rather than
    /// being rejected earlier for a binding that no longer exists. The
    /// truncation a lost snapshot would risk is pinned by
    /// `nts_byte_count_narrows_a_wide_buffer_when_the_c_type_defaults`.
    #[test]
    fn nts_uses_the_snapshotted_c_type_with_bound_params_cleared() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            // `open_dae`'s DaeParam snapshots SQL_C_CHAR; bound_params stays
            // empty, as SQL_RESET_PARAMS would leave it.
            state.dae = Some(open_dae(None));
            assert!(state.bound_params.is_empty());
            assert_eq!(state.dae_current_c_type(), Some(SQL_C_CHAR));
        }
        let narrow = b"AB\0";
        let ret = unsafe {
            sql_put_data(
                h.stmt,
                narrow.as_ptr() as *mut std::ffi::c_void,
                SQL_NTS as SqlLen,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_FUNCTION_SEQUENCE.state);
        assert_eq!(
            state.dae.as_ref().unwrap().progress.bytes_sent,
            0,
            "a rejected call must not advance the parameter's byte total"
        );
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
                state.dae = Some(DaeState::for_test(
                    vec![
                        DaeParam::unbounded(0, std::ptr::null_mut(), Some(declared))
                            .with_binding_types(
                                crate::api::odbc_types::SQL_C_WCHAR,
                                crate::api::odbc_types::SQL_WVARCHAR,
                            ),
                    ],
                    Some(0),
                ));
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

    /// A parameter whose C type and SQL type disagree on wideness still needs
    /// the checked-out client as this sequence's only mutual-exclusion signal
    /// against a concurrent `SQLParamData` -- the transcoded write still needs
    /// the client, because its output goes to the wire like any other chunk.
    /// No client to check out is therefore the same "something else is using
    /// this sequence" state `failed_client_checkout_leaves_progress_untouched`
    /// covers for the untranscoded path, and a rejected call must leave the
    /// parameter's counters and carry where they were.
    /// `SQL_LEN_DATA_AT_EXEC(n)` promises `n` *application* bytes, while
    /// `ColumnSize` bounds what survives trimming. Conflating the two makes a
    /// value that satisfies both fail at close: `"ab  "` is four bytes the
    /// application really did supply, and two after the overflowing blanks are
    /// trimmed to `varchar(2)`, so a single counter would report `2 != 4` and
    /// reject a correct sequence. The totals are tracked separately.
    #[test]
    fn trimmed_padding_does_not_break_the_declared_length_promise() {
        let limit = crate::conversion::param_convert::dae_length_limit(
            crate::api::odbc_types::SQL_C_CHAR,
            SQL_VARCHAR,
            2,
        )
        .expect("varchar(2) is a bounded declaration")
        .expect("and therefore has a limit");

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(DaeState::for_test(
                vec![
                    DaeParam::unbounded(0, std::ptr::null_mut(), Some(4))
                        .with_binding_types(crate::api::odbc_types::SQL_C_CHAR, SQL_VARCHAR)
                        .with_length_limit(limit),
                ],
                Some(0),
            ));
        }
        // Buffered rather than streamed, so the chunk lands in the accumulator
        // without needing a parked client.
        {
            let mut state = stmt.inner.lock().unwrap();
            let dae = state.dae.as_mut().unwrap();
            dae.deferred = true;
        }

        let mut chunk = *b"ab  ";
        let ret = unsafe { sql_put_data(h.stmt, chunk.as_mut_ptr().cast(), 4) };
        assert_eq!(ret, SQL_SUCCESS, "four supplied bytes satisfy the promise");

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().expect("sequence still active");
        assert_eq!(
            dae.progress.bytes_sent, 4,
            "the declared-length promise counts what the application supplied"
        );
        assert_eq!(
            dae.progress.retained_units, 2,
            "the ColumnSize budget counts only what survived trimming"
        );
        assert_eq!(dae.progress.buffer, b"ab", "the blanks were trimmed away");
    }

    #[test]
    fn transcoded_param_without_a_client_returns_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            let mut param = DaeParam::unbounded(0, std::ptr::null_mut(), None)
                .with_binding_types(crate::api::odbc_types::SQL_C_WCHAR, SQL_VARCHAR);
            param.transcode = Some(crate::conversion::param_convert::DaeTranscode::new(
                crate::api::odbc_types::SQL_C_WCHAR,
                SQL_VARCHAR,
                mssql_tds::token::tokens::SqlCollation::default(),
            ));
            state.dae = Some(DaeState::for_test(vec![param], Some(0)));
        }

        let mut bytes: Vec<u8> = "ab".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = bytes.len() as SqlLen;
        let ret = unsafe { sql_put_data(h.stmt, bytes.as_mut_ptr().cast(), len) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        let dae = state.dae.as_ref().expect("sequence still active");
        assert_eq!(dae.progress.bytes_sent, 0);
        assert!(!dae.progress.put_data_called);
        assert!(dae.progress.carry.is_empty());
    }

    /// `SQL_DATA_AT_EXEC` declares no total, so nothing bounds how large
    /// The accumulator behind a data-at-execution parameter can grow to
    /// whatever `SQLPutData` claims. A reservation this call can never satisfy
    /// must fail cleanly with `HY001` instead of letting `Vec`'s default
    /// infallible allocation abort the process this driver is loaded into.
    /// Checked before any client is checked out, so this doesn't need one
    /// parked: a byte count near `usize::MAX` fails `try_reserve` on any real
    /// system without actually exhausting its memory, and `data_ptr` is never
    /// read at that length -- the call returns before the unsafe slice is
    /// constructed.
    #[test]
    fn oversized_transcoded_chunk_returns_hy001_without_aborting() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.dae = Some(DaeState::for_test(
                vec![
                    DaeParam::unbounded(0, std::ptr::null_mut(), None)
                        .with_binding_types(crate::api::odbc_types::SQL_C_WCHAR, SQL_VARCHAR),
                ],
                Some(0),
            ));
        }

        let mut token = 0u8;
        let ret =
            unsafe { sql_put_data(h.stmt, (&mut token as *mut u8).cast(), isize::MAX as SqlLen) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, ERR_MEMORY_ALLOCATION.state);
        assert!(
            !state.needs_data(),
            "an unsatisfiable reservation must abandon the sequence, not leave it retriable"
        );
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
