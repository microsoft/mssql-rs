// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLGetDiagRecW` and `SQLGetDiagFieldW`.
//!
//! Validates handle/rec number, walks the per-handle diagnostic list, copies
//! SQLSTATE + native error + message into caller-supplied buffers, and returns
//! `SQL_NO_DATA` past the end of the list.
//!
//! Only the `W` (UTF-16) variant is exported — modern DMs (unixODBC, iODBC,
//! Windows) translate ANSI calls to `W` for the driver.

use std::{mem, panic};

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_DIAG_MESSAGE_TEXT, SQL_DIAG_NATIVE, SQL_DIAG_NUMBER, SQL_DIAG_SQLSTATE, SQL_ERROR,
    SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_NO_DATA,
    SQL_SQLSTATE_SIZE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlPointer,
    SqlReturn, SqlSmallInt, SqlWChar,
};
use crate::api::util::copy_utf16_with_nul;
use crate::error::{DiagRecord, HasDiagnostics};
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

        // TODO: Do we need to snapshot here? Copy to user buffer directly?
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

/// Implementation of [`SQLGetDiagFieldW`](super::exports::SQLGetDiagFieldW).
///
/// Mirrors msodbcsql's `SQLGetDiagFieldW` semantics:
/// - Header fields  require `RecNumber == 0`; any other value returns `SQL_ERROR`.
/// - Per-record fields require `RecNumber >= 1` and return `SQL_NO_DATA` past
///   the end of the diag list.
/// - String fields (`SQL_DIAG_SQLSTATE`, `SQL_DIAG_MESSAGE_TEXT`) honor the
///   caller's `BufferLength` (in bytes), NUL-terminate inside the buffer, and
///   report the full untruncated byte length via `*StringLengthPtr`. Truncation
///   yields `SQL_SUCCESS_WITH_INFO`.
/// - Fixed length fields (Ex: `SQL_DIAG_NATIVE`) ignore `BufferLength`.
///
/// # Safety
/// See the exported function's doc for caller requirements.
pub(crate) unsafe fn sql_get_diag_field_w(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    diag_identifier: SqlSmallInt,
    diag_info_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        handle_type,
        ?handle,
        rec_number,
        diag_identifier,
        buffer_length,
        "SQLGetDiagFieldW called"
    );

    let result = panic::catch_unwind(|| {
        if handle.is_null() {
            error!("SQLGetDiagFieldW: handle is null");
            return SQL_INVALID_HANDLE;
        }

        if is_diag_header_field(diag_identifier) {
            unsafe {
                handle_header_field(
                    handle_type,
                    handle,
                    rec_number,
                    diag_identifier,
                    diag_info_ptr,
                )
            }
        } else {
            unsafe {
                handle_record_field(
                    handle_type,
                    handle,
                    rec_number,
                    diag_identifier,
                    diag_info_ptr,
                    buffer_length,
                    string_length_ptr,
                )
            }
        }
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLGetDiagFieldW: panic caught at FFI boundary");
        SQL_ERROR
    });
    trace!(?ret, "SQLGetDiagFieldW returning");
    ret
}

/// Returns `true` if `diag_identifier` is a header diagnostic field. Header
/// fields are scoped to the handle (not a specific record).
fn is_diag_header_field(diag_identifier: SqlSmallInt) -> bool {
    matches!(diag_identifier, SQL_DIAG_NUMBER)
}

/// Handles header diagnostic fields (`SQL_DIAG_NUMBER`, etc.). Validates
/// `rec_number == 0` and dispatches by identifier. Caller must ensure
/// `is_diag_header_field(diag_identifier)` is true.
///
/// # Safety
/// `handle` must be a valid, non-null handle of `handle_type`.
unsafe fn handle_header_field(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    diag_identifier: SqlSmallInt,
    diag_info_ptr: SqlPointer,
) -> SqlReturn {
    if rec_number != 0 {
        error!(
            rec_number,
            "SQLGetDiagFieldW: header field requires RecNumber=0"
        );
        return SQL_ERROR;
    }

    match diag_identifier {
        SQL_DIAG_NUMBER => {
            if !diag_info_ptr.is_null() {
                let count = match unsafe { diag_record_count(handle_type, handle) } {
                    Ok(c) => c,
                    Err(rc) => return rc,
                };
                unsafe { (diag_info_ptr as *mut SqlInteger).write(count) };
            }
            SQL_SUCCESS
        }
        _ => {
            error!(
                diag_identifier,
                "SQLGetDiagFieldW: unsupported header diag identifier"
            );
            SQL_ERROR
        }
    }
}

/// Handles per-record diagnostic fields (`SQL_DIAG_SQLSTATE`,
/// `SQL_DIAG_NATIVE`, `SQL_DIAG_MESSAGE_TEXT`). Validates `rec_number >= 1`,
/// snapshots the record, and dispatches by identifier.
///
/// # Safety
/// `handle` must be a valid, non-null handle of `handle_type`. `diag_info_ptr`
/// and `string_length_ptr` must satisfy the contract of the underlying writers.
unsafe fn handle_record_field(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    diag_identifier: SqlSmallInt,
    diag_info_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if rec_number < 1 {
        error!(
            rec_number,
            "SQLGetDiagFieldW: invalid RecNumber for record field"
        );
        return SQL_ERROR;
    }

    let snapshot = match unsafe { snapshot_record(handle_type, handle, rec_number) } {
        Ok(s) => s,
        Err(rc) => return rc,
    };
    let Some(rec) = snapshot else {
        return SQL_NO_DATA;
    };

    match diag_identifier {
        SQL_DIAG_SQLSTATE => {
            let mut utf16 = [0u16; SQL_SQLSTATE_SIZE];
            for (i, b) in rec.sql_state.iter().enumerate() {
                utf16[i] = SqlWChar::from(*b);
            }
            unsafe {
                write_utf16_field_bytes(diag_info_ptr, buffer_length, string_length_ptr, &utf16)
            }
        }
        SQL_DIAG_NATIVE => {
            if !diag_info_ptr.is_null() {
                unsafe { (diag_info_ptr as *mut SqlInteger).write(rec.native_error) };
            }
            SQL_SUCCESS
        }
        SQL_DIAG_MESSAGE_TEXT => {
            let utf16: Vec<u16> = rec.message.encode_utf16().collect();
            unsafe {
                write_utf16_field_bytes(diag_info_ptr, buffer_length, string_length_ptr, &utf16)
            }
        }
        _ => {
            error!(
                diag_identifier,
                "SQLGetDiagFieldW: unsupported diag identifier"
            );
            SQL_ERROR
        }
    }
}

/// `SQLGetDiagFieldW` string-field writer. Buffer size and reported length are
/// in **bytes** (per the field-API convention). Returns `SQL_ERROR` (HY090)
/// if `buffer_length_bytes` is negative.
///
/// # Safety
/// `dst` must be writable for `buffer_length_bytes` bytes if non-null.
/// `string_length_ptr` must be writable for one `SqlSmallInt` if non-null.
unsafe fn write_utf16_field_bytes(
    dst: SqlPointer,
    buffer_length_bytes: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    src_utf16: &[u16],
) -> SqlReturn {
    if buffer_length_bytes < 0 {
        error!(
            buffer_length_bytes,
            "SQLGetDiagFieldW: negative BufferLength (HY090)"
        );
        return SQL_ERROR;
    }

    let total_bytes = src_utf16.len().saturating_mul(mem::size_of::<SqlWChar>());
    if !string_length_ptr.is_null() {
        let len = SqlSmallInt::try_from(total_bytes).unwrap_or(SqlSmallInt::MAX);
        unsafe { string_length_ptr.write(len) };
    }

    let buf_chars = (buffer_length_bytes as usize) / mem::size_of::<SqlWChar>();
    let truncated = unsafe { copy_utf16_with_nul(dst as *mut SqlWChar, buf_chars, src_utf16) };

    if truncated {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// Returns the number of diagnostic records stored on a handle.
///
/// # Safety
/// `handle` must be a valid, non-null handle pointer of the given `handle_type`.
unsafe fn diag_record_count(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
) -> Result<SqlInteger, SqlReturn> {
    unsafe { with_locked_diag_records(handle_type, handle, |records| records.len() as SqlInteger) }
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
    unsafe { with_locked_diag_records(handle_type, handle, |records| records.get(idx).cloned()) }
}

/// Dispatches to the correct handle type, locks its state, and passes the
/// diagnostic records to `f`. Returns `Err(SQL_INVALID_HANDLE)` for
/// unrecognized handle types, `Err(SQL_ERROR)` on poisoned mutex.
///
/// # Safety
/// `handle` must be a valid, non-null handle pointer of the given `handle_type`.
unsafe fn with_locked_diag_records<T>(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    f: impl Fn(&[DiagRecord]) -> T,
) -> Result<T, SqlReturn> {
    match handle_type {
        SQL_HANDLE_ENV => {
            let h = unsafe { handle_from_raw::<EnvHandle>(handle) };
            debug_assert_eq!(
                h.object_type,
                HandleType::Env,
                "with_locked_diag_records: handle is not ENV"
            );
            let guard = h.inner.lock().map_err(|_| {
                error!("with_locked_diag_records: ENV mutex poisoned");
                SQL_ERROR
            })?;
            Ok(f(guard.diag_records()))
        }
        SQL_HANDLE_DBC => {
            let h = unsafe { handle_from_raw::<DbcHandle>(handle) };
            debug_assert_eq!(
                h.object_type,
                HandleType::Dbc,
                "with_locked_diag_records: handle is not DBC"
            );
            let guard = h.inner.lock().map_err(|_| {
                error!("with_locked_diag_records: DBC mutex poisoned");
                SQL_ERROR
            })?;
            Ok(f(guard.diag_records()))
        }
        SQL_HANDLE_STMT => {
            let h = unsafe { handle_from_raw::<StmtHandle>(handle) };
            debug_assert_eq!(
                h.object_type,
                HandleType::Stmt,
                "with_locked_diag_records: handle is not STMT"
            );
            let guard = h.inner.lock().map_err(|_| {
                error!("with_locked_diag_records: STMT mutex poisoned");
                SQL_ERROR
            })?;
            Ok(f(guard.diag_records()))
        }
        _ => {
            error!(
                handle_type,
                "with_locked_diag_records: unsupported handle type"
            );
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

/// `SQLGetDiagRecW` message writer. Buffer size and reported length are in
/// **characters** (per the rec-API convention). Returns `SQL_ERROR` (HY090)
/// if `buffer_length` is negative.
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
    if buffer_length < 0 {
        error!(
            buffer_length,
            "SQLGetDiagRecW: negative BufferLength (HY090)"
        );
        return SQL_ERROR;
    }

    let utf16: Vec<u16> = message_src.encode_utf16().collect();

    if !text_length_ptr.is_null() {
        let len = SqlSmallInt::try_from(utf16.len()).unwrap_or(SqlSmallInt::MAX);
        unsafe { text_length_ptr.write(len) };
    }

    let buf_chars = buffer_length as usize;
    let truncated = unsafe { copy_utf16_with_nul(message_dst, buf_chars, &utf16) };

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
    fn negative_buffer_length_returns_error() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "Some message");

        let mut state = [0u16; 6];
        let mut msg = [0u16; 64];
        let mut text_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_rec_w(
                SQL_HANDLE_ENV,
                env,
                1,
                state.as_mut_ptr(),
                ptr::null_mut(),
                msg.as_mut_ptr(),
                -1,
                &mut text_len,
            )
        };
        assert_eq!(ret, SQL_ERROR);
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

    #[test]
    fn diag_field_message_text_reports_byte_length() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 42, "hello");

        let mut msg = [0u16; 64];
        let buf_bytes = (msg.len() * mem::size_of::<SqlWChar>()) as SqlSmallInt;
        let mut string_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_MESSAGE_TEXT,
                msg.as_mut_ptr() as SqlPointer,
                buf_bytes,
                &mut string_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(utf16_to_string(&msg), "hello");
        // SQLGetDiagFieldW reports string lengths in bytes, not characters.
        assert_eq!(string_len, 10); // 5 chars × 2 bytes/char
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_number_returns_record_count() {
        let env = alloc_env();
        let mut count: SqlInteger = -1;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                0,
                SQL_DIAG_NUMBER,
                &mut count as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(count, 0);

        push_diag(env, *b"HY000", 1, "first");
        push_diag(env, *b"HY001", 2, "second");

        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                0,
                SQL_DIAG_NUMBER,
                &mut count as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(count, 2);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_sqlstate_returns_state_and_byte_length() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 42, "some error");

        let mut state = [0u16; 6];
        let mut string_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_SQLSTATE,
                state.as_mut_ptr() as SqlPointer,
                (6 * mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut string_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(utf16_to_string(&state), "HY024");
        assert_eq!(string_len, 10); // 5 chars × 2 bytes
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_native_returns_error_code() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 42, "some error");

        let mut native: SqlInteger = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_NATIVE,
                &mut native as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(native, 42);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_message_text_truncation_reports_byte_length() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "This message is long");

        let mut msg = [0u16; 8]; // 7 chars + NUL fits "This me"
        let buf_bytes = (msg.len() * mem::size_of::<SqlWChar>()) as SqlSmallInt;
        let mut string_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_MESSAGE_TEXT,
                msg.as_mut_ptr() as SqlPointer,
                buf_bytes,
                &mut string_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(utf16_to_string(&msg), "This me");
        // Full untruncated byte length.
        assert_eq!(string_len, 40); // 20 chars × 2 bytes
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_no_records_returns_no_data() {
        let env = alloc_env();
        let mut state = [0u16; 6];
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_SQLSTATE,
                state.as_mut_ptr() as SqlPointer,
                (6 * mem::size_of::<SqlWChar>()) as SqlSmallInt,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_unsupported_identifier_returns_error() {
        // Need at least one record so we get past rec-validation and reach the
        // identifier dispatch (msodbcsql validates RecNumber/record existence
        // first, returning SQL_NO_DATA for missing records regardless of id).
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "msg");
        let mut dummy: SqlInteger = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                9999, // unsupported diag identifier
                &mut dummy as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_null_handle_returns_invalid_handle() {
        let mut count: SqlInteger = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                ptr::null_mut(),
                1,
                SQL_DIAG_NUMBER,
                &mut count as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn diag_field_sqlstate_truncation_honors_buffer_length() {
        // Per spec (and msodbcsql), SQLGetDiagFieldW must honor the caller's
        // BufferLength for SQLSTATE — truncating, NUL-terminating within the
        // buffer, and reporting the full untruncated byte length.
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "msg");

        let mut state = [0xFFFFu16; 4]; // 8 bytes — only fits 3 chars + NUL
        let mut string_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_SQLSTATE,
                state.as_mut_ptr() as SqlPointer,
                (state.len() * mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut string_len,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(utf16_to_string(&state), "HY0");
        assert_eq!(string_len, 10); // full untruncated bytes
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_negative_buffer_length_returns_error() {
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "msg");

        let mut buf = [0u16; 8];
        let mut string_len: SqlSmallInt = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1,
                SQL_DIAG_MESSAGE_TEXT,
                buf.as_mut_ptr() as SqlPointer,
                -1,
                &mut string_len,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_number_rejects_nonzero_rec_number() {
        // Header field: msodbcsql requires RecNumber == 0.
        let env = alloc_env();
        let mut count: SqlInteger = 0;
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                1, // non-zero — invalid for header field
                SQL_DIAG_NUMBER,
                &mut count as *mut SqlInteger as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn diag_field_record_field_rejects_zero_rec_number() {
        // Per-record field: RecNumber must be >= 1.
        let env = alloc_env();
        push_diag(env, *b"HY024", 0, "msg");
        let mut state = [0u16; 6];
        let ret = unsafe {
            sql_get_diag_field_w(
                SQL_HANDLE_ENV,
                env,
                0,
                SQL_DIAG_SQLSTATE,
                state.as_mut_ptr() as SqlPointer,
                (state.len() * mem::size_of::<SqlWChar>()) as SqlSmallInt,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }
}
