// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindParameter — bind an application buffer to a
//! statement parameter marker.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_PARAM_INPUT, SQL_SUCCESS, SqlHandle, SqlLen,
    SqlPointer, SqlReturn, SqlSmallInt, SqlULen, SqlUSmallInt,
};
use crate::api::type_rules::{
    SqlTypeSupport, classify_sql_type, is_valid_c_type, resolve_default_c_type,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use crate::params::BoundParam;
use crate::params::conversion_matrix::is_supported_conversion;

/// Binds a buffer to a parameter marker in an SQL statement.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// - `parameter_value_ptr` / `strlen_or_ind_ptr`, if non-null, must remain valid
///   until the statement is executed (ODBC binds by reference).
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_bind_parameter(
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
    debug!(
        ?statement_handle,
        parameter_number,
        input_output_type,
        value_type,
        parameter_type,
        column_size,
        decimal_digits,
        ?parameter_value_ptr,
        buffer_length,
        ?strlen_or_ind_ptr,
        "SQLBindParameter called",
    );

    crate::ffi_entry!("SQLBindParameter", unsafe {
        sql_bind_parameter_impl(
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
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_bind_parameter_impl(
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
    if statement_handle.is_null() {
        error!("SQLBindParameter: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLBindParameter: handle is not a STMT"
    );

    debug_assert!(
        parameter_number >= 1,
        "SQLBindParameter: parameter number less than 1 - DM should have rejected this"
    );

    sql_bind_parameter_safe(
        stmt,
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

#[allow(clippy::too_many_arguments)]
fn sql_bind_parameter_safe(
    stmt: &StmtHandle,
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
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLBindParameter: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);

    // ValueType (C type) and ParameterType (SQL type) must be known type
    // identifiers (HY003 / HY004).
    if !is_valid_c_type(value_type) {
        error!(
            value_type,
            "SQLBindParameter: invalid application buffer type"
        );
        post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
        return SQL_ERROR;
    }
    match classify_sql_type(parameter_type) {
        SqlTypeSupport::Supported => {}
        SqlTypeSupport::NotImplemented => {
            error!(
                parameter_type,
                "SQLBindParameter: unsupported SQL data type"
            );
            post_diag(&mut stmt_state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
            return SQL_ERROR;
        }
        SqlTypeSupport::Invalid => {
            error!(parameter_type, "SQLBindParameter: invalid SQL data type");
            post_diag(&mut stmt_state, ERR_INVALID_SQL_DATA_TYPE);
            return SQL_ERROR;
        }
    }

    // Resolve SQL_C_DEFAULT here so the execute path never sees the placeholder,
    // matching msodbcsql, which stores the resolved type in the APD.
    let c_type = if value_type == SQL_C_DEFAULT {
        let Some(resolved) = resolve_default_c_type(parameter_type) else {
            error!(
                parameter_type,
                "SQLBindParameter: no default C type for this SQL type"
            );
            post_diag(&mut stmt_state, ERR_RESTRICTED_DATA_TYPE);
            return SQL_ERROR;
        };
        resolved
    } else {
        value_type
    };

    // The C type → SQL type conversion must be one we support (07006).
    if !is_supported_conversion(c_type, parameter_type) {
        error!(
            c_type,
            parameter_type, "SQLBindParameter: unsupported C/SQL type conversion"
        );
        post_diag(&mut stmt_state, ERR_RESTRICTED_DATA_TYPE);
        return SQL_ERROR;
    }

    // Phase 1: input parameters only. Output / input-output binding is a
    // deferred feature.
    if input_output_type != SQL_PARAM_INPUT {
        error!(
            input_output_type,
            "SQLBindParameter: only input parameters are supported"
        );
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HYC00,
            0,
            "Output parameters not yet implemented",
        );
        return SQL_ERROR;
    }

    let idx = (parameter_number - 1) as usize;
    if stmt_state.bound_params.len() <= idx {
        stmt_state.bound_params.resize(idx + 1, None);
    }
    stmt_state.bound_params[idx] = Some(BoundParam {
        input_output_type,
        c_type,
        sql_type: parameter_type,
        column_size,
        decimal_digits,
        parameter_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    });

    // A rebind invalidates any cached server-side prepared plan: the next
    // SQLExecute must re-prepare so the plan matches the new bindings. This
    // mirrors msodbcsql clearing DESC_CONSISTENT → FIsReprepareRequired. The
    // prepared SQL text is kept; the server handle is orphaned for release
    // (via sp_unprepare) at the next execute, forcing the sp_prepexec path.
    stmt_state.orphan_prepared_handle();

    debug!(parameter_number, "SQLBindParameter: parameter bound");
    SQL_SUCCESS
}

/// Implements the `SQL_RESET_PARAMS` option of `SQLFreeStmt` — releases all
/// parameter bindings on the statement. The prepared handle and cursor state
/// are left untouched.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_reset_params(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLFreeStmt(SQL_RESET_PARAMS) called");
    crate::ffi_entry!("SQLFreeStmt(SQL_RESET_PARAMS)", unsafe {
        sql_free_stmt_reset_params_impl(statement_handle)
    })
}

unsafe fn sql_free_stmt_reset_params_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_free_stmt_reset_params_safe(stmt)
}

fn sql_free_stmt_reset_params_safe(stmt: &StmtHandle) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut stmt_state);
    stmt_state.bound_params.clear();
    debug!("SQLFreeStmt(SQL_RESET_PARAMS): parameter bindings released");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_SLONG, SQL_GUID, SQL_INTEGER, SQL_NULL_HANDLE, SQL_PARAM_OUTPUT,
        SQL_VARCHAR,
    };
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                SQL_NULL_HANDLE,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn output_parameter_is_rejected_hyc00() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_OUTPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn bind_stores_param_and_grows_vec() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        // Bind parameter 3 first — slots 1 and 2 should be created empty.
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                3,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.bound_params.len(), 3);
        assert!(state.bound_params[0].is_none());
        assert!(state.bound_params[1].is_none());
        assert!(state.bound_params[2].is_some());
    }

    #[test]
    fn reset_params_clears_bindings() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        let _ = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        let ret = unsafe { sql_free_stmt_reset_params(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert!(state.bound_params.is_empty());
    }

    #[test]
    fn invalid_sql_type_returns_hy004() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                9999,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY004);
    }

    #[test]
    fn invalid_c_type_returns_hy003() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                9999,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY003);
    }

    #[test]
    fn unsupported_conversion_returns_07006() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut val: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_INTEGER,
                SQL_INTEGER,
                0,
                0,
                &mut val as *mut i32 as SqlPointer,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07006);
    }

    #[test]
    fn real_but_unconvertible_c_type_returns_07006() {
        // SQL_C_SLONG is a legal ODBC C type the driver cannot convert yet, so it
        // must fail the conversion check rather than the HY003 type check.
        let h = TestHandles::with_env_dbc_stmt();
        let mut val: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                0,
                &mut val as *mut i32 as SqlPointer,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07006);
    }

    #[test]
    fn default_c_type_is_resolved_before_storage() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_DEFAULT,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let bound = state.bound_params[0].expect("parameter 1 should be bound");
        assert_eq!(bound.c_type, SQL_C_CHAR);
    }

    #[test]
    fn default_c_type_resolved_but_unconvertible_returns_07006() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_DEFAULT,
                SQL_GUID,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_07006);
    }

    #[test]
    fn interval_sql_type_returns_hyc00() {
        // SQL Server has no interval type: a real ODBC identifier the driver
        // cannot implement is HYC00, not a conversion failure.
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_DEFAULT,
                crate::api::odbc_types::SQL_INTERVAL_YEAR,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn rebind_invalidates_cached_prepared_handle() {
        use mssql_tds::connection::tds_client::PreparedStatement;

        use crate::handles::stmt::PreparedPlan;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test(
                    "SELECT @P1",
                    mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42),
                ),
                marker_count: 0,
            });
        }
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = crate::api::odbc_types::SQL_NTS as SqlLen;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_VARCHAR,
                0,
                0,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        // The prepared text survives, but the server handle is orphaned for
        // release at the next execute, so that execute re-prepares.
        let state = stmt.inner.lock().unwrap();
        assert!(state.prepared.is_some());
        assert!(state.prepared.as_ref().and_then(|p| p.stmt.id()).is_none());
        let orphaned = state
            .pending_unprepare
            .expect("prior handle queued for release");
        assert_eq!(
            orphaned,
            mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42)
        );
    }
}
