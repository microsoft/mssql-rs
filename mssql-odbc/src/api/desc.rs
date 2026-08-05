// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Descriptor and parameter-metadata entry points.
//!
//! These are the minimum viable implementations required by applications that
//! bind `SQL_C_NUMERIC` parameters (which must set precision/scale on the APD)
//! or probe parameter types before binding.

use tracing::{debug, error};

use super::odbc_types::*;
use super::sqlstate::SQLSTATE_HY091;
use crate::error::{free_errors, post_sql_error};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements `SQLSetDescFieldW`.
///
/// The driver exposes implicit descriptors only, and the APD fields an
/// application sets for `SQL_C_NUMERIC` binding (type, precision, scale, data
/// pointer) are already captured by `SQLBindParameter`. Accepting them keeps
/// numeric binding working; anything else is reported as an unknown field so
/// callers are not silently misled.
///
/// # Safety
/// `descriptor_handle` must be a valid handle produced by
/// `SQLGetStmtAttr(SQL_ATTR_APP_PARAM_DESC)` — which this driver reports as the
/// statement handle itself — or null.
pub(crate) unsafe fn sql_set_desc_field_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    _value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?descriptor_handle,
        record_number, field_identifier, "SQLSetDescFieldW called"
    );
    crate::ffi_entry!("SQLSetDescFieldW", unsafe {
        if descriptor_handle.is_null() {
            error!("SQLSetDescFieldW: descriptor_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let field = field_identifier as SqlUSmallInt;
        if matches!(
            field,
            SQL_DESC_TYPE
                | SQL_DESC_CONCISE_TYPE
                | SQL_DESC_PRECISION
                | SQL_DESC_SCALE
                | SQL_DESC_DATA_PTR
                | SQL_DESC_LENGTH
                | SQL_DESC_OCTET_LENGTH
        ) {
            return SQL_SUCCESS;
        }

        let stmt = handle_from_raw::<StmtHandle>(descriptor_handle);
        if stmt.object_type != HandleType::Stmt {
            return SQL_INVALID_HANDLE;
        }
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLSetDescFieldW: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        post_sql_error(
            &mut state,
            SQLSTATE_HY091,
            0,
            "Invalid descriptor field identifier",
        );
        SQL_ERROR
    })
}

/// Implements `SQLDescribeParam`.
///
/// Server-side parameter description requires `sp_describe_undeclared_parameters`,
/// which is not yet wired up. Reporting the optional feature as unsupported is
/// the documented behaviour for drivers that cannot describe parameters, and
/// callers fall back to their own type inference.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null; the output pointers
/// are not written.
pub(crate) unsafe fn sql_describe_param(
    statement_handle: SqlHandle,
    parameter_number: SqlUSmallInt,
    _data_type_ptr: *mut SqlSmallInt,
    _parameter_size_ptr: *mut SqlULen,
    _decimal_digits_ptr: *mut SqlSmallInt,
    _nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        parameter_number, "SQLDescribeParam called"
    );
    crate::ffi_entry!("SQLDescribeParam", unsafe {
        if statement_handle.is_null() {
            error!("SQLDescribeParam: statement_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let stmt = handle_from_raw::<StmtHandle>(statement_handle);
        debug_assert_eq!(stmt.object_type, HandleType::Stmt);
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLDescribeParam: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        post_sql_error(
            &mut state,
            super::sqlstate::SQLSTATE_HYC00,
            0,
            "Optional feature not implemented: parameter description",
        );
        SQL_ERROR
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestHandles;

    #[test]
    fn set_desc_field_accepts_apd_numeric_fields() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.stmt,
                1,
                SQL_DESC_PRECISION as SqlSmallInt,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_desc_field_rejects_unknown_field() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_desc_field_w(h.stmt, 1, 9999, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn set_desc_field_null_handle() {
        let ret =
            unsafe { sql_set_desc_field_w(SQL_NULL_HANDLE, 1, 1002, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn describe_param_reports_unsupported() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_describe_param(
                h.stmt,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }
}
