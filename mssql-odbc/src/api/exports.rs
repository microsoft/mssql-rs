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
    SQL_CLOSE, SQL_RESET_PARAMS, SQL_SUCCESS, SQL_UNBIND, SqlHWnd, SqlHandle, SqlInteger, SqlLen,
    SqlPointer, SqlReturn, SqlSmallInt, SqlULen, SqlUSmallInt, SqlWChar,
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

/// Retrieves an attribute from an environment handle.
///
/// # Safety
/// - `environment_handle` must be a valid ENV handle.
/// - Output pointers must be valid and writable for the requested attribute.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetEnvAttr(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_env_attr::sql_get_env_attr(
            environment_handle,
            attribute,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    }
}

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

/// Sets a connection attribute through the Unix ANSI entry point.
///
/// unixODBC converts character-valued attributes set before driver selection to
/// ANSI, then replays them through this symbol when the driver is loaded.
#[cfg(not(windows))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLSetConnectAttr(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::set_connect_attr::sql_set_connect_attr(
            connection_handle,
            attribute,
            value_ptr,
            string_length,
        )
    }
}

/// Retrieves a statement attribute.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `attribute` must be a valid statement attribute identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetStmtAttrW(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::set_stmt_attr::sql_get_stmt_attr_w(
            statement_handle,
            attribute,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    }
}

/// Reports whether a specific ODBC function is supported by this driver.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle.
/// - `supported_ptr` must be writable as required by `function_id`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetFunctions(
    connection_handle: SqlHandle,
    function_id: SqlUSmallInt,
    supported_ptr: *mut SqlUSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_functions::sql_get_functions(connection_handle, function_id, supported_ptr)
    }
}

/// Retrieves driver/data-source capability information.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle.
/// - Output pointers must be valid and writable for the requested info type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetInfoW(
    connection_handle: SqlHandle,
    info_type: SqlUSmallInt,
    info_value_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_info::sql_get_info_w(
            connection_handle,
            info_type,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
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

/// Establishes a connection to a data source using a DSN, user, and password.
///
/// Exists mainly so the Windows Driver Manager can resolve this mandatory core
/// function; the driver's primary connect path is [`SQLDriverConnectW`].
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle from [`SQLAllocHandle`].
/// - `server_name`, `user_name`, and `authentication` (if non-null) must each point
///   to a valid UTF-16 buffer of the corresponding length (or be null-terminated
///   when the length is `SQL_NTS`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLConnectW(
    connection_handle: SqlHandle,
    server_name: *const SqlWChar,
    name_length1: SqlSmallInt,
    user_name: *const SqlWChar,
    name_length2: SqlSmallInt,
    authentication: *const SqlWChar,
    name_length3: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::connect::sql_connect_w(
            connection_handle,
            server_name,
            name_length1,
            user_name,
            name_length2,
            authentication,
            name_length3,
        )
    }
}

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

// ---- Transactions -----------------------------------------------------------

/// Commits or rolls back the transaction on a connection, or on every
/// connection under an environment.
///
/// `handle_type` must be `SQL_HANDLE_DBC` or `SQL_HANDLE_ENV`; `completion_type`
/// must be `SQL_COMMIT` or `SQL_ROLLBACK`. In autocommit mode, or when no
/// transaction has been started, this succeeds without contacting the server.
///
/// # Safety
/// - `handle` must be a valid DBC or ENV handle returned by `SQLAllocHandle`,
///   matching `handle_type`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLEndTran(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    completion_type: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::end_tran::sql_end_tran(handle_type, handle, completion_type) }
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
/// `SQL_CLOSE` closes the open cursor (no-op if none); `SQL_RESET_PARAMS`
/// releases all parameter bindings; `SQL_UNBIND` releases all column bindings.
/// `SQL_DROP` is not yet implemented.
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
        SQL_RESET_PARAMS => unsafe {
            super::bind_param::sql_free_stmt_reset_params(statement_handle)
        },
        SQL_UNBIND => unsafe { super::bind_col::sql_free_stmt_unbind(statement_handle) },
        _ => {
            // TODO: SQL_DROP
            SQL_SUCCESS
        }
    }
}

// ---- Statement execution ---------------------------------------------------

/// Binds an application buffer to a parameter marker in an SQL statement.
///
/// The value is read by reference at `SQLExecute` time; the bound buffers must
/// stay valid until execution.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `parameter_value_ptr` / `strlen_or_ind_ptr`, if non-null, must remain valid
///   and readable until the statement is executed.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLBindParameter(
    statement_handle: SqlHandle,
    parameter_number: SqlUSmallInt,
    input_output_type: SqlSmallInt,
    value_type: SqlSmallInt,
    parameter_type: SqlSmallInt,
    column_size: SqlULen,
    decimal_digits: SqlSmallInt,
    parameter_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::bind_param::sql_bind_parameter(
            statement_handle,
            parameter_number,
            input_output_type,
            value_type,
            parameter_type,
            column_size,
            decimal_digits,
            parameter_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    }
}

/// Prepares a SQL statement for later execution with `SQLExecute`.
///
/// Only the SQL text is stored here - the server-side prepare is deferred to
/// `SQLExecute`. No network I/O happens at prepare time.
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

/// Returns the inferred SQL type, size, scale, and nullability of a prepared
/// statement parameter.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each output pointer, if non-null, must be writable for its declared type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLDescribeParam(
    statement_handle: SqlHandle,
    parameter_number: SqlUSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    parameter_size_ptr: *mut SqlULen,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::describe_param::sql_describe_param(
            statement_handle,
            parameter_number,
            data_type_ptr,
            parameter_size_ptr,
            decimal_digits_ptr,
            nullable_ptr,
        )
    }
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

/// Returns information about the data types supported by the data source as an
/// open result set (fetchable via `SQLFetch` / `SQLGetData`).
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetTypeInfoW(
    statement_handle: SqlHandle,
    data_type: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::get_type_info::sql_get_type_info_w(statement_handle, data_type) }
}

/// Executes a prepared statement using the current bound parameter values.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLExecute(statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::execute::sql_execute(statement_handle) }
}

/// Advances the data-at-execution parameter protocol.
///
/// Called by the application after `SQLExecute` returns `SQL_NEED_DATA`.
/// `value_ptr_ptr`, if non-null, receives the `ParameterValuePtr` that
/// was bound for the parameter currently awaiting data.  Returns
/// `SQL_NEED_DATA` when more parameters still need data, or the statement
/// result when all parameters have been supplied.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `value_ptr_ptr`, if non-null, must be a writable `SqlPointer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLParamData(
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::param_data::sql_param_data(statement_handle, value_ptr_ptr) }
}

/// Supplies a data chunk for a data-at-execution parameter.
///
/// Must be called between `SQLParamData` calls during a data-at-execution
/// sequence.  `strlen_or_ind` is the number of bytes in `data_ptr`, or
/// `SQL_NULL_DATA` to mark the parameter as SQL NULL.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - `data_ptr`, when `strlen_or_ind` is a positive byte count, must be
///   readable for that many bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLPutData(
    statement_handle: SqlHandle,
    data_ptr: SqlPointer,
    strlen_or_ind: SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::put_data::sql_put_data(statement_handle, data_ptr, strlen_or_ind) }
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

/// Binds an application buffer to a result-set column.
///
/// Passing null for both `target_value_ptr` and `strlen_or_ind_ptr` unbinds the
/// column; a null data pointer with a live indicator keeps it bound for lengths
/// only.
///
/// # Safety
/// `statement_handle` must be a valid statement handle or null. The buffers must
/// stay valid until the column is unbound or the statement is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLBindCol(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::bind_col::sql_bind_col(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    }
}

/// Fetches a rowset of rows into the columns bound by `SQLBindCol`.
///
/// The cursor is forward-only, so `SQL_FETCH_NEXT` is the only orientation
/// served; anything else returns `HY106`.
///
/// # Safety
/// `statement_handle` must be a valid statement handle or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFetchScroll(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::fetch_scroll::sql_fetch_scroll(statement_handle, fetch_orientation, fetch_offset)
    }
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

/// Gets a descriptor field for a result set column.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `column_number` must be a valid column index (1-based), except for
///   `SQL_DESC_COUNT`, which describes the result set.
/// - `character_attribute_ptr` must be writable for `buffer_length` bytes when
///   non-null; `string_length_ptr` and `numeric_attribute_ptr` must be null or
///   writable for their types.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLColAttributeW(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::col_attribute::sql_col_attribute_w(
            statement_handle,
            column_number,
            field_identifier,
            character_attribute_ptr,
            buffer_length,
            string_length_ptr,
            numeric_attribute_ptr,
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

// ---- Result set processing --------------------------------------------------

/// Returns the row count from the last INSERT, UPDATE, or DELETE statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
/// - `row_count_ptr` must be a valid, writable pointer to [`SqlLen`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLRowCount(
    statement_handle: SqlHandle,
    row_count_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::row_count::sql_row_count(statement_handle, row_count_ptr) }
}

// ---- Catalog functions -------------------------------------------------------

/// Returns table, catalog, or schema information.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLTablesW(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    table_type: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_tables_w(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            table_type,
            name_length_4,
        )
    }
}

/// Returns column information for one or more tables.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLColumnsW(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    column_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_columns_w(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            column_name,
            name_length_4,
        )
    }
}

/// Returns the columns that make up a table's primary key.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLPrimaryKeysW(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_primary_keys_w(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
        )
    }
}

/// Returns the foreign keys in, or referencing, a table.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLForeignKeysW(
    statement_handle: SqlHandle,
    pk_catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    pk_schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    pk_table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    fk_catalog_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
    fk_schema_name: *const SqlWChar,
    name_length_5: SqlSmallInt,
    fk_table_name: *const SqlWChar,
    name_length_6: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_foreign_keys_w(
            statement_handle,
            pk_catalog_name,
            name_length_1,
            pk_schema_name,
            name_length_2,
            pk_table_name,
            name_length_3,
            fk_catalog_name,
            name_length_4,
            fk_schema_name,
            name_length_5,
            fk_table_name,
            name_length_6,
        )
    }
}

/// Returns index and column statistics for a table.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLStatisticsW(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    unique: SqlUSmallInt,
    reserved: SqlUSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_statistics_w(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            unique,
            reserved,
        )
    }
}

/// Returns the best-fit unique row identifier, or the columns automatically
/// updated when any value in the row changes.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn SQLSpecialColumnsW(
    statement_handle: SqlHandle,
    identifier_type: SqlUSmallInt,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: SqlUSmallInt,
    nullable: SqlUSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_special_columns_w(
            statement_handle,
            identifier_type,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            scope,
            nullable,
        )
    }
}

/// Returns the stored procedures registered in a data source.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle returned by `SQLAllocHandle`.
/// - Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLProceduresW(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    proc_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::catalog::sql_procedures_w(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            proc_name,
            name_length_3,
        )
    }
}

// ---- Attribute management (TO-BE-IMPLEMENTED) --------------------------------

/// Retrieves a connection attribute.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle.
/// - `attribute` must be a valid connection attribute identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetConnectAttrW(
    connection_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_connect_attr::sql_get_connect_attr_w(
            connection_handle,
            attribute,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    }
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
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::set_stmt_attr::sql_set_stmt_attr_w(
            statement_handle,
            attribute,
            value_ptr,
            string_length,
        )
    }
}

// ---- Descriptor and parameter management ------------------------------------

/// Gets a descriptor field.
///
/// # Safety
/// - `descriptor_handle` must be a valid descriptor handle.
/// - `record_number` must be valid for the descriptor.
/// - `field_identifier` must be a valid field identifier.
/// - Output pointers must be valid and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLGetDescFieldW(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::get_desc_field::sql_get_desc_field_w(
            descriptor_handle,
            record_number,
            field_identifier,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    }
}

/// Sets a descriptor field.
///
/// # Safety
/// - `descriptor_handle` must be a valid descriptor handle.
/// - `record_number` must be valid for the descriptor.
/// - `field_identifier` must be a valid field identifier.
/// - `value_ptr`, when the field is a pointer field, is stored verbatim and
///   not dereferenced by this call; the caller must keep it valid for as
///   long as it remains bound. When the field is a character field,
///   `value_ptr` must be readable for `buffer_length` bytes (or
///   NUL-terminated when `buffer_length == SQL_NTS`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLSetDescFieldW(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    crate::init_tracing();
    unsafe {
        super::set_desc_field::sql_set_desc_field_w(
            descriptor_handle,
            record_number,
            field_identifier,
            value_ptr,
            buffer_length,
        )
    }
}

/// Cancels the processing of the statement.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLCancel(statement_handle: SqlHandle) -> SqlReturn {
    crate::init_tracing();
    unsafe { super::cancel::sql_cancel(statement_handle) }
}
