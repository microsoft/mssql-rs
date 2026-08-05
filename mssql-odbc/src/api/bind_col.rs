// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindCol — bind a result column to an application buffer.

use tracing::{debug, error};

use super::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::{ERR_INVALID_DESCRIPTOR_INDEX, post_diag};
use crate::error::free_errors;
use crate::handles::stmt::BoundCol;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements `SQLBindCol`.
///
/// A null `target_value_ptr` unbinds the column, matching the ODBC contract.
/// Bindings are consumed by the block-fetch path in `SQLFetchScroll` and by
/// `SQLFetch`.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. The application
/// buffers referenced by `target_value_ptr` / `strlen_or_ind_ptr` must remain
/// valid until the column is unbound or the statement is freed.
pub(crate) unsafe fn sql_bind_col(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number, target_type, buffer_length, "SQLBindCol called"
    );
    crate::ffi_entry!("SQLBindCol", unsafe {
        sql_bind_col_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

unsafe fn sql_bind_col_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLBindCol: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLBindCol: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    if column_number == 0 {
        // Bookmark column binding is not supported.
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    let idx = usize::from(column_number) - 1;
    if state.bound_cols.len() <= idx {
        state.bound_cols.resize(idx + 1, None);
    }

    state.bound_cols[idx] = if target_value_ptr.is_null() && strlen_or_ind_ptr.is_null() {
        None
    } else {
        Some(BoundCol {
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        })
    };

    SQL_SUCCESS
}

/// Implements `SQLFreeStmt(SQL_UNBIND)` — releases every column binding.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_unbind(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFreeStmt(SQL_UNBIND) called");
    crate::ffi_entry!("SQLFreeStmt", unsafe {
        if statement_handle.is_null() {
            error!("SQLFreeStmt(SQL_UNBIND): statement_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let stmt = handle_from_raw::<StmtHandle>(statement_handle);
        debug_assert_eq!(stmt.object_type, HandleType::Stmt);
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLFreeStmt(SQL_UNBIND): stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        state.bound_cols.clear();
        SQL_SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_SLONG, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;

    #[test]
    fn bind_col_null_handle() {
        let ret = unsafe {
            sql_bind_col(
                SQL_NULL_HANDLE,
                1,
                SQL_C_SLONG,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn bind_col_records_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: i32 = 0;
        let ret = unsafe {
            sql_bind_col(
                h.stmt,
                2,
                SQL_C_SLONG,
                (&mut buf as *mut i32).cast(),
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.bound_cols.len(), 2);
        assert!(state.bound_cols[0].is_none());
        assert_eq!(state.bound_cols[1].unwrap().target_type, SQL_C_SLONG);
    }

    #[test]
    fn bind_col_null_pointers_unbind() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: i32 = 0;
        unsafe {
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut buf as *mut i32).cast(),
                4,
                std::ptr::null_mut(),
            );
            sql_bind_col(
                h.stmt,
                1,
                SQL_C_SLONG,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
        }

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert!(state.bound_cols[0].is_none());
    }

    #[test]
    fn bind_col_zero_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_bind_col(
                h.stmt,
                0,
                SQL_C_SLONG,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }
}
