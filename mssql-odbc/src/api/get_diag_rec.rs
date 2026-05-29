// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetDiagRecW.
//!
//! Mirrors msodbcsql's `ExportImp::SQLGetDiagRecW` in `sqlcerr.cpp`:
//! validate handle/rec number, walk the per-handle diagnostic list, copy
//! SQLSTATE + native error + message into caller-supplied buffers, and return
//! `SQL_NO_DATA` past the end of the list.
//!
//! Only the `W` (UTF-16) variant is exported — modern DMs (unixODBC, iODBC,
//! Windows) translate ANSI calls to `W` for the driver.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_NO_DATA,
    SQL_SQLSTATE_SIZE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlReturn,
    SqlSmallInt, SqlWChar,
};
use crate::error::DiagRecord;
use crate::handles::{DbcHandle, EnvHandle, HandleType, StmtHandle, handle_from_raw};

/// Implementation of [`SQLGetDiagRecW`](super::exports::SQLGetDiagRecW).
///
/// # Safety
/// See the exported function's doc for caller requirements.
#[allow(clippy::too_many_arguments)] // arity is fixed by the ODBC spec
pub(crate) unsafe fn sql_get_diag_rec_w(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    sql_state: *mut SqlWChar,
    native_error_ptr: *mut SqlInteger,
    message_text: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    text_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        handle_type,
        ?handle,
        rec_number,
        buffer_length,
        "SQLGetDiagRecW called"
    );

    let result = panic::catch_unwind(|| {
        if handle.is_null() {
            error!("SQLGetDiagRecW: handle is null");
            return SQL_INVALID_HANDLE;
        }
        if rec_number < 1 || buffer_length < 0 {
            error!(
                rec_number,
                buffer_length, "SQLGetDiagRecW: invalid argument"
            );
            return SQL_ERROR;
        }

        // Per spec, the text-length out-param is initialized to 0.
        if !text_length_ptr.is_null() {
            unsafe { text_length_ptr.write(0) };
        }

        let snapshot = match unsafe { snapshot_record(handle_type, handle, rec_number) } {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let Some(rec) = snapshot else {
            return SQL_NO_DATA;
        };

        unsafe { write_sql_state(sql_state, &rec.sql_state) };
        if !native_error_ptr.is_null() {
            unsafe { native_error_ptr.write(rec.native_error) };
        }
        unsafe { write_message(message_text, buffer_length, text_length_ptr, &rec.message) }
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLGetDiagRecW: panic caught at FFI boundary");
        SQL_ERROR
    });
    trace!(?ret, "SQLGetDiagRecW returning");
    ret
}

/// Clones the requested record out from under the handle's diag mutex.
/// Returns `Ok(None)` if the index is past the end of the list.
/// Returns `Err(SQL_INVALID_HANDLE)` for unsupported handle types.
///
/// TODO: For ODBC 3.x parity with msodbcsql, diagnostic records should be
/// sorted by priority before indexing (msodbcsql `SortErrors`). We currently
/// return in posting order.
/// TODO: Add an OOM-resilient fallback diagnostic path like msodbcsql's
/// `ERR_STATUS_OOM`: store a non-allocating OOM flag on the handle and return
/// static HY001 text for record 1 without heap allocation.
unsafe fn snapshot_record(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
) -> Result<Option<DiagRecord>, SqlReturn> {
    let idx = (rec_number - 1) as usize;

    macro_rules! read_from {
        ($ty:ty, $expected:expr) => {{
            let h = unsafe { handle_from_raw::<$ty>(handle) };
            debug_assert_eq!(
                h.object_type, $expected,
                "handle type mismatch — possible DM bug or memory corruption"
            );
            let Ok(state) = h.inner.lock() else {
                return Err(SQL_ERROR);
            };
            Ok(state.diag_records.get(idx).cloned())
        }};
    }

    match handle_type {
        SQL_HANDLE_ENV => read_from!(EnvHandle, HandleType::Env),
        SQL_HANDLE_DBC => read_from!(DbcHandle, HandleType::Dbc),
        SQL_HANDLE_STMT => read_from!(StmtHandle, HandleType::Stmt),
        _ => {
            error!(handle_type, "SQLGetDiagRecW: unsupported handle type");
            Err(SQL_INVALID_HANDLE)
        }
    }
}

/// Writes SQLSTATE as 5 UTF-16 chars + NUL terminator. SQLSTATEs are ASCII,
/// so a zero-extending widen is sufficient — no `encode_utf16` needed.
///
/// # Safety
/// `dst` must be writable for `SQL_SQLSTATE_SIZE + 1` `SqlWChar`s or null.
unsafe fn write_sql_state(dst: *mut SqlWChar, src: &[u8; SQL_SQLSTATE_SIZE]) {
    if dst.is_null() {
        return;
    }
    for (i, b) in src.iter().enumerate() {
        unsafe { dst.add(i).write(SqlWChar::from(*b)) };
    }
    unsafe { dst.add(SQL_SQLSTATE_SIZE).write(0) };
}

/// Encodes `message` to UTF-16, writes it to `buffer` (NUL-terminated), and
/// reports the full untruncated length via `text_length_ptr`. Returns
/// `SQL_SUCCESS_WITH_INFO` if the buffer was too small.
///
/// # Safety
/// `message_dst` must be writable for `buffer_length` `SqlWChar`s or null.
/// `text_length_ptr` must be writable for one `SqlSmallInt` or null.
unsafe fn write_message(
    message_dst: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    text_length_ptr: *mut SqlSmallInt,
    message_src: &str,
) -> SqlReturn {
    let utf16: Vec<u16> = message_src.encode_utf16().collect();
    let total = utf16.len();

    if !text_length_ptr.is_null() {
        let len = SqlSmallInt::try_from(total).unwrap_or(SqlSmallInt::MAX);
        unsafe { text_length_ptr.write(len) };
    }

    let truncated = if !message_dst.is_null() {
        if buffer_length <= 0 {
            // Caller provided a buffer pointer but no space — cannot write any
            // characters or the NUL terminator, so a non-empty message is truncated.
            total > 0
        } else {
            let buf_len = usize::try_from(buffer_length).unwrap_or(0);
            // Reserve one slot for the NUL terminator.
            let copy_len = total.min(buf_len.saturating_sub(1));
            for (i, ch) in utf16.iter().copied().take(copy_len).enumerate() {
                unsafe { message_dst.add(i).write(ch) };
            }
            unsafe { message_dst.add(copy_len).write(0) };
            copy_len < total
        }
    } else {
        // No output buffer: report length only; this is not truncation.
        false
    };

    if truncated {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{SQL_HANDLE_DESC, SQL_NULL_HANDLE};
    use crate::error::DiagRecord;
    use crate::handles::handle_from_raw;

    fn alloc_env() -> SqlHandle {
        let mut h: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut h) };
        assert_eq!(ret, SQL_SUCCESS);
        h
    }

    fn push_diag(env: SqlHandle, sql_state: [u8; 5], native: i32, msg: &str) {
        let env_ref = unsafe { handle_from_raw::<EnvHandle>(env) };
        env_ref
            .inner
            .lock()
            .unwrap()
            .diag_records
            .push(DiagRecord::new(sql_state, native, msg));
    }

    fn utf16_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..len]).unwrap()
    }

    #[test]
    fn no_records_returns_no_data() {
        let env = alloc_env();
        let mut state = [0u16; 6];
        let mut native: SqlInteger = 0;
        let mut msg = [0u16; 64];
        let mut text_len: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as SqlSmallInt,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
        assert_eq!(text_len, 0);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn reads_first_record() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 42, "Invalid attribute value");

        let mut state = [0u16; 6];
        let mut native: SqlInteger = 0;
        let mut msg = [0u16; 64];
        let mut text_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                &mut native,
                msg.as_mut_ptr(),
                msg.len() as SqlSmallInt,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(utf16_to_string(&state), "HY024");
        assert_eq!(native, 42);
        assert_eq!(utf16_to_string(&msg), "Invalid attribute value");
        assert_eq!(text_len, "Invalid attribute value".len() as SqlSmallInt);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn truncation_returns_success_with_info() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "This message is long");

        let mut state = [0u16; 6];
        let mut msg = [0u16; 8]; // 7 chars + NUL fits "This me"
        let mut text_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                ptr::null_mut(),
                msg.as_mut_ptr(),
                msg.len() as SqlSmallInt,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(utf16_to_string(&msg), "This me");
        // Full untruncated length is reported.
        assert_eq!(text_len, "This message is long".len() as SqlSmallInt);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn zero_length_buffer_returns_success_with_info() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "Some message");

        let mut state = [0u16; 6];
        let mut msg = [0u16; 1]; // non-null but zero usable space
        let mut text_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                ptr::null_mut(),
                msg.as_mut_ptr(),
                0,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(text_len, "Some message".len() as SqlSmallInt);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                ptr::null_mut(),
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn rec_number_zero_returns_error() {
        let env = alloc_env();
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn unsupported_handle_type_returns_invalid_handle() {
        let env = alloc_env();
        // DESC handles are not yet implemented; passing SQL_HANDLE_DESC on a
        // non-DESC pointer must be rejected before we dereference.
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_DESC,
                env,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn null_message_buffer_reports_length_without_truncation() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "Invalid attribute value");

        let mut state = [0u16; 6];
        let mut text_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut text_len,
            )
        };

        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(utf16_to_string(&state), "HY024");
        assert_eq!(text_len, "Invalid attribute value".len() as SqlSmallInt);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn rec_number_past_end_returns_no_data() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "first");
        let mut text_len: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                2,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
        assert_eq!(text_len, 0);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn loop_until_no_data_reads_all_records() {
        let env = alloc_env();
        push_diag(env, *b"HY000", 1, "first error");
        push_diag(env, *b"HY001", 2, "second error");
        push_diag(env, *b"HY024", 3, "third error");

        let expected = [
            ("HY000", 1i32, "first error"),
            ("HY001", 2i32, "second error"),
            ("HY024", 3i32, "third error"),
        ];

        let mut rec_number: SqlSmallInt = 1;
        let mut records: Vec<(String, i32, String)> = Vec::new();
        loop {
            let mut state = [0u16; 6];
            let mut native: SqlInteger = 0;
            let mut msg = [0u16; 64];
            let ret = unsafe {
                sql_get_diag_rec_w(
                    SQL_HANDLE_ENV,
                    env,
                    rec_number,
                    state.as_mut_ptr(),
                    &mut native,
                    msg.as_mut_ptr(),
                    msg.len() as SqlSmallInt,
                    ptr::null_mut(),
                )
            };
            if ret == SQL_NO_DATA {
                break;
            }
            assert_eq!(ret, SQL_SUCCESS);
            records.push((utf16_to_string(&state), native, utf16_to_string(&msg)));
            rec_number += 1;
        }

        assert_eq!(records.len(), expected.len());
        for (i, (exp_state, exp_native, exp_msg)) in expected.iter().enumerate() {
            assert_eq!(&records[i].0, exp_state);
            assert_eq!(records[i].1, *exp_native);
            assert_eq!(&records[i].2, exp_msg);
        }

        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }
}
