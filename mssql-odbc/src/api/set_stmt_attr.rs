// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLSetStmtAttrW` / `SQLGetStmtAttrW`.
//!
//! Only the attributes exercised by the fetch path are given real semantics:
//! the block-fetch rowset controls (`SQL_ATTR_ROW_ARRAY_SIZE`,
//! `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_ROW_STATUS_PTR`,
//! `SQL_ATTR_ROW_BIND_TYPE`). Everything else the Driver Manager or
//! mssql-python sets — cursor type, concurrency, param-set controls — is
//! accepted as a no-op so the handshake is not broken. This driver only
//! supports forward-only, read-only cursors, which is exactly what
//! mssql-python requests.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC, SQL_ATTR_CONCURRENCY, SQL_ATTR_CURSOR_TYPE,
    SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_STATUS_PTR, SQL_ATTR_PARAMS_PROCESSED_PTR,
    SQL_ATTR_PARAMSET_SIZE, SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_BIND_OFFSET_PTR,
    SQL_ATTR_ROW_BIND_TYPE, SQL_ATTR_ROW_STATUS_PTR, SQL_ATTR_ROWS_FETCHED_PTR, SQL_ERROR,
    SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlULen,
    SqlUSmallInt,
};
use crate::error::free_errors;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Sets a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. For the pointer
/// attributes the caller-supplied `value_ptr` must remain valid for the
/// lifetime it is used by later fetches.
pub(crate) unsafe fn sql_set_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        string_length,
        "SQLSetStmtAttrW called",
    );
    crate::ffi_entry!("SQLSetStmtAttrW", unsafe {
        sql_set_stmt_attr_w_impl(statement_handle, attribute, value_ptr)
    })
}

unsafe fn sql_set_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLSetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLSetStmtAttrW: handle is not a STMT"
    );

    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLSetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => {
            // The value is a `SQLULEN` passed by value in the pointer slot. Zero
            // is invalid; clamp to one so downstream fetches never divide by it.
            let n = value_ptr as SqlULen;
            state.row_array_size = n.max(1);
            debug!(
                row_array_size = state.row_array_size,
                "SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE set"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_ROWS_FETCHED_PTR => {
            state.rows_fetched_ptr = value_ptr as *mut SqlULen;
            SQL_SUCCESS
        }
        SQL_ATTR_ROW_STATUS_PTR => {
            state.row_status_ptr = value_ptr as *mut SqlUSmallInt;
            SQL_SUCCESS
        }
        SQL_ATTR_ROW_BIND_TYPE => {
            state.row_bind_type = value_ptr as SqlULen;
            SQL_SUCCESS
        }
        // Accepted as no-ops: the driver already behaves as forward-only /
        // read-only, and the remaining param-set / descriptor controls are not
        // yet acted upon. mssql-python sets these to values consistent with the
        // implemented behavior, so accept silently rather than fail the DM.
        SQL_ATTR_CURSOR_TYPE
        | SQL_ATTR_CONCURRENCY
        | SQL_ATTR_PARAMSET_SIZE
        | SQL_ATTR_PARAM_BIND_TYPE
        | SQL_ATTR_PARAM_STATUS_PTR
        | SQL_ATTR_PARAMS_PROCESSED_PTR
        | SQL_ATTR_ROW_BIND_OFFSET_PTR
        | SQL_ATTR_APP_ROW_DESC
        | SQL_ATTR_APP_PARAM_DESC => {
            debug!(attribute, "SQLSetStmtAttrW: attribute accepted as no-op");
            SQL_SUCCESS
        }
        _ => {
            // Unknown statement attributes are accepted as no-ops: the Driver
            // Manager probes many attributes and rejecting them breaks the
            // handshake.
            debug!(
                attribute,
                "SQLSetStmtAttrW: unrecognized attribute accepted as no-op"
            );
            SQL_SUCCESS
        }
    }
}

/// Retrieves a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. `value_ptr`, when
/// non-null, must be writable for the size of the attribute (pointer-sized for
/// every attribute handled here).
pub(crate) unsafe fn sql_get_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(?statement_handle, attribute, "SQLGetStmtAttrW called");
    let _ = (buffer_length, string_length_ptr); // no string-valued attrs here
    crate::ffi_entry!("SQLGetStmtAttrW", unsafe {
        sql_get_stmt_attr_w_impl(statement_handle, attribute, value_ptr)
    })
}

unsafe fn sql_get_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    if value_ptr.is_null() {
        // Nothing to write into; treat as a successful no-op.
        return SQL_SUCCESS;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetStmtAttrW: handle is not a STMT"
    );

    let Ok(state) = stmt.inner.lock() else {
        error!("SQLGetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };

    match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => unsafe {
            *(value_ptr as *mut SqlULen) = state.row_array_size;
        },
        SQL_ATTR_ROWS_FETCHED_PTR => unsafe {
            *(value_ptr as *mut *mut SqlULen) = state.rows_fetched_ptr;
        },
        SQL_ATTR_ROW_STATUS_PTR => unsafe {
            *(value_ptr as *mut *mut SqlUSmallInt) = state.row_status_ptr;
        },
        SQL_ATTR_ROW_BIND_TYPE => unsafe {
            *(value_ptr as *mut SqlULen) = state.row_bind_type;
        },
        _ => unsafe {
            // Report a benign zero for attributes we don't track rather than
            // failing; callers reading an unset attribute get the ODBC default.
            debug!(
                attribute,
                "SQLGetStmtAttrW: unrecognized attribute; returning 0"
            );
            *(value_ptr as *mut SqlULen) = 0;
        },
    }

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_BIND_BY_COLUMN, SQL_NULL_HANDLE};
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn set_stmt_attr_null_handle() {
        let ret = unsafe {
            sql_set_stmt_attr_w(
                SQL_NULL_HANDLE,
                SQL_ATTR_ROW_ARRAY_SIZE,
                10 as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn set_row_array_size_stored_and_readback() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 128 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_array_size, 128);

        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 128);
    }

    #[test]
    fn set_row_array_size_zero_clamps_to_one() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().row_array_size, 1);
    }

    #[test]
    fn set_rows_fetched_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut rows_fetched: SqlULen = 0;
        let ptr = &mut rows_fetched as *mut SqlULen;
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROWS_FETCHED_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(stmt.inner.lock().unwrap().rows_fetched_ptr, ptr);
    }

    #[test]
    fn default_row_bind_type_is_column_wise() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_BIND_BY_COLUMN);
    }

    #[test]
    fn unknown_attribute_accepted_as_noop() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 9999, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
    }
}
