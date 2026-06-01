// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exported ODBC entry points for the msodbcsql18 shared library.
//!
//! Every `#[unsafe(no_mangle)] pub extern "C"` function that appears in the
//! driver's symbol table is listed here. Implementations live in sibling
//! modules (e.g. `alloc_handle.rs`) as `pub(crate)` functions.
//!
//! This file acts as the driver's export manifest — the Rust equivalent of a
//! Windows `.def` file or a C header listing the public API surface.

use super::odbc_types::{SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlSmallInt, SqlWChar};

// ---- Handle allocation and management ---------------------------------------

/// Allocates an environment, connection, statement, or descriptor handle.
///
/// # Safety
/// - `output_handle` must be a valid, aligned, writable pointer to [`SqlHandle`].
/// - For `SQL_HANDLE_ENV`, `input_handle` must be `SQL_NULL_HANDLE`.
/// - For other handle types, `input_handle` must be a valid parent handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLAllocHandle(
    handle_type: SqlSmallInt,
    input_handle: SqlHandle,
    output_handle: *mut SqlHandle,
) -> SqlReturn {
    unsafe { super::alloc_handle::sql_alloc_handle(handle_type, input_handle, output_handle) }
}

/// Frees an environment, connection, statement, or descriptor handle
/// previously allocated by [`SQLAllocHandle`].
///
/// # Safety
/// - `handle` must have been allocated by [`SQLAllocHandle`] and not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFreeHandle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    unsafe { super::free_handle::sql_free_handle(handle_type, handle) }
}

/// See [`set_env_attr::sql_set_env_attr`] for full safety requirements.
///
/// # Safety
/// Called from C via the ODBC Driver Manager. `environment_handle` must be a
/// valid ENV handle previously returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLSetEnvAttr(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    unsafe {
        super::set_env_attr::sql_set_env_attr(
            environment_handle,
            attribute,
            value_ptr,
            string_length,
        )
    }
}

// ---- Diagnostics -----------------------------------------------------------

/// Retrieves a diagnostic record (SQLSTATE, native error, message) previously
/// posted on the given handle.
///
/// # Safety
/// - `handle` must be a valid handle of type `handle_type`, or null.
/// - `sql_state`, if non-null, must be writable for at least
///   `SQL_SQLSTATE_SIZE + 1` `SQLWCHAR`s (6 code units including NUL).
/// - `message_text`, if non-null, must be writable for `buffer_length` `SQLWCHAR`s.
/// - `native_error_ptr` and `text_length_ptr`, if non-null, must point to
///   writable, aligned storage for one value of their respective types.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)] // arity is fixed by the ODBC spec
pub unsafe extern "C" fn SQLGetDiagRecW(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    sql_state: *mut SqlWChar,
    native_error_ptr: *mut SqlInteger,
    message_text: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    text_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    unsafe {
        super::get_diag_rec::sql_get_diag_rec_w(
            handle_type,
            handle,
            rec_number,
            sql_state,
            native_error_ptr,
            message_text,
            buffer_length,
            text_length_ptr,
        )
    }
}
