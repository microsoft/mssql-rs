// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLNumResultCols.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::api::sqlstate::SQLSTATE_HY010;
use crate::api::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Returns the number of columns in the current result set metadata.
///
/// # Safety
/// `statement_handle` must be a valid STMT handle or null.
/// `column_count_ptr`, when non-null, must be writable for one `SqlSmallInt`.
pub(crate) unsafe fn sql_num_result_cols(
    statement_handle: SqlHandle,
    column_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?column_count_ptr,
        "SQLNumResultCols called",
    );

    crate::ffi_entry!("SQLNumResultCols", unsafe {
        sql_num_result_cols_impl(statement_handle, column_count_ptr)
    })
}

unsafe fn sql_num_result_cols_impl(
    statement_handle: SqlHandle,
    column_count_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLNumResultCols: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLNumResultCols: handle is not a STMT"
    );

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLNumResultCols: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_EXEC_CONTEXT) {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HY010,
            0,
            "Function sequence error",
        );
        return SQL_ERROR;
    }

    let column_count =
        SqlSmallInt::try_from(stmt_state.column_metadata.len()).unwrap_or(SqlSmallInt::MAX);
    unsafe { write_if_some(column_count_ptr, column_count) };

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NULL_HANDLE,
        SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;
    use crate::handles::handle_from_raw;
    use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;

    unsafe fn alloc_env_dbc_stmt() -> (SqlHandle, SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                sql_set_env_attr(
                    env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                    0,
                )
            },
            SQL_SUCCESS
        );

        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            SQL_SUCCESS
        );

        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        (env, dbc, stmt)
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut count = -1;
        let rc = unsafe { sql_num_result_cols(ptr::null_mut(), &mut count) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn null_out_ptr_is_tolerated() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        stmt_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_CONTEXT);

        let rc: i16 = unsafe { sql_num_result_cols(stmt, ptr::null_mut()) };
        assert_eq!(rc, SQL_SUCCESS);

        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, stmt);
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    #[test]
    fn dml_or_ddl_returns_zero_columns() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        stmt_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_CONTEXT);

        let mut count: SqlSmallInt = -1;
        let rc = unsafe { sql_num_result_cols(stmt, &mut count) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(count, 0);

        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, stmt);
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    #[test]
    fn fresh_stmt_returns_sequence_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let mut count: SqlSmallInt = -1;
        let rc = unsafe { sql_num_result_cols(stmt, &mut count) };
        assert_eq!(rc, SQL_ERROR);

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_HY010);
        drop(stmt_state);

        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, stmt);
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }
}
