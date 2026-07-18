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

use super::odbc_types::{
    SQL_CLOSE, SQL_SUCCESS, SqlHWnd, SqlHandle, SqlInteger, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt, SqlWChar,
};

// ---- Handle allocation and management ---------------------------------------

/// Allocates an environment, connection, statement, or descriptor handle.
///
/// # Safety
/// - `output_handle_ptr` must be a valid, aligned, writable pointer to [`SqlHandle`].
/// - For `SQL_HANDLE_ENV`, `input_handle` must be `SQL_NULL_HANDLE`.
/// - For other handle types, `input_handle` must be a valid parent handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLAllocHandle(
    handle_type: SqlSmallInt,
    input_handle: SqlHandle,
    output_handle_ptr: *mut SqlHandle,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::alloc_handle::sql_alloc_handle(handle_type, input_handle, output_handle_ptr) }
}

/// Frees an environment, connection, statement, or descriptor handle
/// previously allocated by [`SQLAllocHandle`].
///
/// # Safety
/// - `handle` must have been allocated by [`SQLAllocHandle`] and not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFreeHandle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::free_handle::sql_free_handle(handle_type, handle) }
}

// ---- Attribute management --------------------------------------------------
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
    crate::init_tracing();
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
/// - `handle` must be a valid handle of type `handle_type`.
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
    crate::init_tracing();
    unsafe {
        super::get_diag::sql_get_diag_rec_w(
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

/// Retrieves a single diagnostic field value for a given record on a handle.
///
/// Supports `SQL_DIAG_NUMBER` (header field, record count) and the per-record
/// fields `SQL_DIAG_SQLSTATE`, `SQL_DIAG_NATIVE`, and `SQL_DIAG_MESSAGE_TEXT`.
/// Unrecognized identifiers return `SQL_ERROR`.
///
/// # Safety
/// - `handle` must be a valid handle of type `handle_type`.
/// - `diag_info_ptr` and `string_length_ptr` must be valid for the requested field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetDiagFieldW(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    rec_number: SqlSmallInt,
    diag_identifier: SqlSmallInt,
    diag_info_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_diag::sql_get_diag_field_w(
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

// ---- Connection management --------------------------------------------------

/// Establishes a connection to a data source.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle from [`SQLAllocHandle`].
/// - `in_connection_string` must point to a valid UTF-16 buffer of at least
///   `string_length1` characters (or null-terminated if `string_length1` is `SQL_NTS`).
/// - `out_connection_string` (if non-null) must be writable for `buffer_length` wide chars.
/// - `string_length2_ptr` (if non-null) must be a writable `SqlSmallInt` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLDriverConnectW(
    connection_handle: SqlHandle,
    window_handle: SqlHWnd,
    in_connection_string: *const SqlWChar,
    string_length1: SqlSmallInt,
    out_connection_string: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length2_ptr: *mut SqlSmallInt,
    driver_completion: SqlUSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::driver_connect::sql_driver_connect_w(
            connection_handle,
            window_handle,
            in_connection_string,
            string_length1,
            out_connection_string,
            buffer_length,
            string_length2_ptr,
            driver_completion,
        )
    }
}

/// Disconnects from the data source associated with a connection handle.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle that is currently connected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLDisconnect(connection_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::disconnect::sql_disconnect(connection_handle) }
}

// ---- Cursor management ------------------------------------------------------

/// Closes the open cursor on a statement handle and discards any pending rows.
///
/// Returns `SQL_ERROR` (SQLSTATE 24000) if no cursor is open on this statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLCloseCursor(statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::close_cursor::sql_close_cursor(statement_handle) }
}

/// Frees resources associated with a statement handle.
///
/// Only `SQL_CLOSE` is implemented; it closes the open cursor (no-op if none).
/// Other options (`SQL_DROP`, `SQL_UNBIND`, `SQL_RESET_PARAMS`) are not yet implemented.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFreeStmt(
    statement_handle: SqlHandle,
    option: SqlUSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    match option {
        SQL_CLOSE => unsafe { super::close_cursor::sql_free_stmt_close(statement_handle) },
        _ => {
            // TODO: SQL_DROP, SQL_UNBIND, SQL_RESET_PARAMS
            SQL_SUCCESS
        }
    }
}

// ---- Statement execution ---------------------------------------------------

/// Prepares a SQL statement for later execution with `SQLExecute`.
///
/// The server-side prepare is deferred and bundled into `SQLExecute`
/// (`sp_prepexec`), matching msodbcsql. No network I/O happens at prepare time.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `statement_text`, if non-null, must be readable for `text_length` `SQLWCHAR`s.
///   If `text_length` is `SQL_NTS`, the string must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLPrepareW(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::prepare::sql_prepare_w(statement_handle, statement_text, text_length) }
}

/// Executes a preparable statement, using the current values of the parameter
/// marker variables if any parameter markers exist in the statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `statement_text`, if non-null, must be readable for `text_length` `SQLWCHAR`s.
///   If `text_length` is `SQL_NTS`, the string must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLExecDirectW(
    statement_handle: SqlHandle,
    statement_text: *const SqlWChar,
    text_length: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::exec_direct::sql_exec_direct_w(statement_handle, statement_text, text_length) }
}

// ---- Result set processing --------------------------------
/// Fetches the next row from the current result set.
///
/// Returns `SQL_SUCCESS` when a row is available or `SQL_NO_DATA` when the
/// result set is exhausted.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFetch(statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::fetch::sql_fetch(statement_handle) }
}

/// Returns the number of columns in the result set.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `column_count_ptr` must be a valid, writable pointer to [`SqlSmallInt`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLNumResultCols(
    statement_handle: SqlHandle,
    column_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::num_result_cols::sql_num_result_cols(statement_handle, column_count_ptr) }
}

/// Gets metadata for a result set column.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `column_number` must be a valid column index (1-based).
/// - Output pointers must be writable for their respective types.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLDescribeColW(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    column_name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    name_length_ptr: *mut SqlSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    column_size_ptr: *mut u64,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::describe_col::sql_describe_col_w(
            statement_handle,
            column_number,
            column_name,
            buffer_length,
            name_length_ptr,
            data_type_ptr,
            column_size_ptr,
            decimal_digits_ptr,
            nullable_ptr,
        )
    }
}

/// Retrieves data for a single column in the current fetched row.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `target_value_ptr`, when non-null, must be writable for `buffer_length` bytes.
/// - `strlen_or_ind_ptr`, when non-null, must be writable for one `SqlLen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetData(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_data::sql_get_data(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    }
}

/// Moves to the next result set in a batch.
///
/// Returns `SQL_SUCCESS` when positioned on the next result set,
/// `SQL_NO_DATA` when the batch is exhausted (cursor is closed), or
/// `SQL_ERROR` on failure.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLMoreResults(statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::more_results::sql_more_results(statement_handle) }
}

// ---- Result set processing (TO-BE-IMPLEMENTED) --------------------------------

/// Returns the row count from the last INSERT, UPDATE, or DELETE statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `row_count_ptr` must be a valid, writable pointer to [`i64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLRowCount(
    _statement_handle: SqlHandle,
    row_count_ptr: *mut i64,
) -> SqlReturn {
    crate::init_tracing();
    if !row_count_ptr.is_null() {
        unsafe { *row_count_ptr = 0 };
    }
    SQL_SUCCESS
}

// ---- Attribute management (TO-BE-IMPLEMENTED) --------------------------------

/// Sets a connection attribute.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle.
/// - `attribute` must be a valid connection attribute identifier.
/// - `value_ptr` validity depends on the attribute type.
/// - `string_length` is used only for string-type attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLSetConnectAttrW(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::set_connect_attr::sql_set_connect_attr_w(
            connection_handle,
            attribute,
            value_ptr,
            string_length,
        )
    }
}

/// Retrieves a connection attribute.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle.
/// - `attribute` must be a valid connection attribute identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetConnectAttrW(
    _connection_handle: SqlHandle,
    _attribute: SqlInteger,
    _value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    _string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

/// Sets a statement attribute.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `attribute` must be a valid statement attribute identifier.
/// - `value_ptr` validity depends on the attribute type.
/// - `string_length` is used only for string-type attributes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLSetStmtAttrW(
    _statement_handle: SqlHandle,
    _attribute: SqlInteger,
    _value_ptr: SqlPointer,
    _string_length: SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

/// Retrieves a statement attribute.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `attribute` must be a valid statement attribute identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetStmtAttrW(
    _statement_handle: SqlHandle,
    _attribute: SqlInteger,
    _value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    _string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

// ---- Descriptor and parameter management (TO-BE-IMPLEMENTED) -----------------

/// Gets a descriptor field.
///
/// # Safety
/// - `descriptor_handle` must be a valid descriptor handle.
/// - `record_number` must be valid for the descriptor.
/// - `field_identifier` must be a valid field identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetDescFieldW(
    _descriptor_handle: SqlHandle,
    _record_number: SqlSmallInt,
    _field_identifier: SqlSmallInt,
    _value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    _string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

/// Binds a parameter marker to a memory buffer.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `parameter_number` must be valid.
/// - `value_ptr` must be a valid, aligned pointer (if non-null).
/// - `str_len_or_ind_ptr` (if non-null) must point to a valid [`i64`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLBindParameter(
    _statement_handle: SqlHandle,
    _parameter_number: SqlUSmallInt,
    _input_output_type: SqlSmallInt,
    _value_type: SqlSmallInt,
    _parameter_type: SqlSmallInt,
    _column_size: u64,
    _decimal_digits: SqlSmallInt,
    _value_ptr: SqlPointer,
    _buffer_length: i64,
    _str_len_or_ind_ptr: *mut i64,
) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

/// Cancels the processing of the statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLCancel(_statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{SQL_DROP, SQL_HANDLE_ENV, SQL_INVALID_HANDLE, SQL_NULL_HANDLE};

    /// Every delegating export forwards a null handle to its impl, which
    /// uniformly reports `SQL_INVALID_HANDLE`.
    #[test]
    fn delegating_exports_reject_null_handle() {
        let sql: Vec<u16> = "SELECT 1".encode_utf16().chain(Some(0)).collect();
        unsafe {
            assert_eq!(
                SQLFreeHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLSetEnvAttr(SQL_NULL_HANDLE, 0, ptr::null_mut(), 0),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLGetDiagRecW(
                    SQL_HANDLE_ENV,
                    SQL_NULL_HANDLE,
                    1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                ),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLGetDiagFieldW(
                    SQL_HANDLE_ENV,
                    SQL_NULL_HANDLE,
                    1,
                    0,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                ),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLDriverConnectW(
                    SQL_NULL_HANDLE,
                    ptr::null_mut(),
                    ptr::null(),
                    0,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    0,
                ),
                SQL_INVALID_HANDLE
            );
            assert_eq!(SQLDisconnect(SQL_NULL_HANDLE), SQL_INVALID_HANDLE);
            assert_eq!(
                SQLSetConnectAttrW(SQL_NULL_HANDLE, 0, ptr::null_mut(), 0),
                SQL_INVALID_HANDLE
            );
            assert_eq!(SQLCloseCursor(SQL_NULL_HANDLE), SQL_INVALID_HANDLE);
            assert_eq!(SQLFreeStmt(SQL_NULL_HANDLE, SQL_CLOSE), SQL_INVALID_HANDLE);
            assert_eq!(
                SQLPrepareW(SQL_NULL_HANDLE, ptr::null(), 0),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLExecDirectW(SQL_NULL_HANDLE, sql.as_ptr(), 0),
                SQL_INVALID_HANDLE
            );
            assert_eq!(SQLFetch(SQL_NULL_HANDLE), SQL_INVALID_HANDLE);
            assert_eq!(
                SQLNumResultCols(SQL_NULL_HANDLE, ptr::null_mut()),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLDescribeColW(
                    SQL_NULL_HANDLE,
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                SQL_INVALID_HANDLE
            );
            assert_eq!(
                SQLGetData(SQL_NULL_HANDLE, 1, 0, ptr::null_mut(), 0, ptr::null_mut()),
                SQL_INVALID_HANDLE
            );
            assert_eq!(SQLMoreResults(SQL_NULL_HANDLE), SQL_INVALID_HANDLE);
        }
    }

    /// `SQLAllocHandle` validates its output pointer before touching the parent
    /// handle; a null output pointer is `SQL_INVALID_HANDLE`.
    #[test]
    fn alloc_handle_rejects_null_output() {
        let ret = unsafe { SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, ptr::null_mut()) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    /// Round-trips an ENV handle through the exported alloc/free wrappers.
    #[test]
    fn alloc_and_free_env_handle() {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert!(!env.is_null());
        assert_eq!(unsafe { SQLFreeHandle(SQL_HANDLE_ENV, env) }, SQL_SUCCESS);
    }

    /// Not-yet-implemented stubs succeed unconditionally regardless of handle.
    #[test]
    fn stub_exports_return_success() {
        unsafe {
            let mut row_count: i64 = -1;
            assert_eq!(SQLRowCount(SQL_NULL_HANDLE, &mut row_count), SQL_SUCCESS);
            assert_eq!(row_count, 0);

            assert_eq!(
                SQLGetConnectAttrW(SQL_NULL_HANDLE, 0, ptr::null_mut(), 0, ptr::null_mut()),
                SQL_SUCCESS
            );
            assert_eq!(
                SQLSetStmtAttrW(SQL_NULL_HANDLE, 0, ptr::null_mut(), 0),
                SQL_SUCCESS
            );
            assert_eq!(
                SQLGetStmtAttrW(SQL_NULL_HANDLE, 0, ptr::null_mut(), 0, ptr::null_mut()),
                SQL_SUCCESS
            );
            assert_eq!(
                SQLGetDescFieldW(SQL_NULL_HANDLE, 0, 0, ptr::null_mut(), 0, ptr::null_mut()),
                SQL_SUCCESS
            );
            assert_eq!(
                SQLBindParameter(
                    SQL_NULL_HANDLE,
                    1,
                    0,
                    0,
                    0,
                    0,
                    0,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                ),
                SQL_SUCCESS
            );
            assert_eq!(SQLCancel(SQL_NULL_HANDLE), SQL_SUCCESS);
        }
    }

    /// `SQLRowCount` skips the write when the output pointer is null.
    #[test]
    fn row_count_tolerates_null_out_pointer() {
        assert_eq!(
            unsafe { SQLRowCount(SQL_NULL_HANDLE, ptr::null_mut()) },
            SQL_SUCCESS
        );
    }

    /// `SQLFreeStmt` only implements `SQL_CLOSE`; other options hit the default
    /// arm and succeed without delegating.
    #[test]
    fn free_stmt_non_close_option_succeeds() {
        assert_eq!(
            unsafe { SQLFreeStmt(SQL_NULL_HANDLE, SQL_DROP) },
            SQL_SUCCESS
        );
    }
}
