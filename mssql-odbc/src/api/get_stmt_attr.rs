// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetStmtAttr.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC, SQL_ATTR_IMP_PARAM_DESC, SQL_ATTR_IMP_ROW_DESC,
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::api::sqlstate::{ERR_INVALID_ATTRIBUTE_IDENTIFIER, post_diag};
use crate::error::free_errors;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Retrieves a statement attribute.
///
/// Currently only the four implicit-descriptor attributes are supported. The
/// Driver Manager queries these while allocating a statement to obtain the
/// driver's descriptor handles; returning them is required to avoid a null
/// descriptor dereference inside the DM's `SQLExecDirectW`.
///
/// # Safety
/// - `statement_handle` must be a valid STMT handle from `SQLAllocHandle`.
/// - For descriptor attributes, `value_ptr` must be writable for one `SQLHANDLE`.
pub(crate) unsafe fn sql_get_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        buffer_length,
        ?string_length_ptr,
        "SQLGetStmtAttrW called",
    );

    crate::ffi_entry!("SQLGetStmtAttr", unsafe {
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

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetStmtAttrW: handle is not a STMT"
    );
    sql_get_stmt_attr_w_safe(stmt, attribute, value_ptr)
}

fn sql_get_stmt_attr_w_safe(
    stmt: &StmtHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLGetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    let desc = match attribute {
        SQL_ATTR_APP_ROW_DESC => stmt.ard,
        SQL_ATTR_APP_PARAM_DESC => stmt.apd,
        SQL_ATTR_IMP_ROW_DESC => stmt.ird,
        SQL_ATTR_IMP_PARAM_DESC => stmt.ipd,
        other => {
            debug!(attribute = other, "SQLGetStmtAttrW: unsupported attribute");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_IDENTIFIER);
            return SQL_ERROR;
        }
    };

    if !value_ptr.is_null() {
        unsafe { (value_ptr as *mut SqlHandle).write(desc) };
    }
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    // An attribute the driver does not implement (SQL_ATTR_CURSOR_TYPE = 6).
    const UNSUPPORTED_ATTR: SqlInteger = 6;

    fn read_desc(stmt: SqlHandle, attribute: SqlInteger) -> (SqlReturn, SqlHandle) {
        let mut out: SqlHandle = SQL_NULL_HANDLE;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                &mut out as *mut SqlHandle as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        (rc, out)
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let (rc, _) = read_desc(SQL_NULL_HANDLE, SQL_ATTR_APP_ROW_DESC);
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn returns_the_four_implicit_descriptors() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        for (attr, expected) in [
            (SQL_ATTR_APP_ROW_DESC, stmt_ref.ard),
            (SQL_ATTR_APP_PARAM_DESC, stmt_ref.apd),
            (SQL_ATTR_IMP_ROW_DESC, stmt_ref.ird),
            (SQL_ATTR_IMP_PARAM_DESC, stmt_ref.ipd),
        ] {
            let (rc, out) = read_desc(h.stmt, attr);
            assert_eq!(rc, SQL_SUCCESS);
            assert!(!out.is_null());
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn implicit_descriptors_are_distinct() {
        let h = TestHandles::with_env_dbc_stmt();
        let all = [
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_PARAM_DESC).1,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "descriptors {i} and {j} alias");
            }
        }
    }

    #[test]
    fn null_value_ptr_is_noop_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_APP_ROW_DESC,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }

    #[test]
    fn unsupported_attribute_returns_error_and_posts_hy092() {
        let h = TestHandles::with_env_dbc_stmt();
        let (rc, _) = read_desc(h.stmt, UNSUPPORTED_ATTR);
        assert_eq!(rc, SQL_ERROR);

        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(
            state.diag_records[0].sql_state,
            ERR_INVALID_ATTRIBUTE_IDENTIFIER.state
        );
    }

    #[test]
    fn successful_call_clears_prior_diag_records() {
        let h = TestHandles::with_env_dbc_stmt();
        let (rc, _) = read_desc(h.stmt, UNSUPPORTED_ATTR);
        assert_eq!(rc, SQL_ERROR);
        {
            let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            assert_eq!(stmt_ref.inner.lock().unwrap().diag_records.len(), 1);
        }
        let (rc, _) = read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC);
        assert_eq!(rc, SQL_SUCCESS);
        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt_ref.inner.lock().unwrap().diag_records.is_empty());
    }
}
