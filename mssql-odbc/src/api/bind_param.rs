// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindParameter — bind an application buffer to a
//! statement parameter marker.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_PARAM_INPUT, SQL_PARAM_INPUT_OUTPUT,
    SQL_PARAM_INPUT_OUTPUT_STREAM, SQL_PARAM_OUTPUT, SQL_PARAM_OUTPUT_STREAM, SQL_RETURN_VALUE,
    SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlULen, SqlUSmallInt,
};
use crate::api::type_rules::{
    SqlTypeSupport, canonical_c_type, classify_parameter_sql_type, is_valid_c_type,
    parameter_column_size_is_valid, resolve_default_c_type,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::bindings::{BindingError, owned_descriptor};
use crate::handles::dbc::DbcState;
use crate::handles::{HandleType, StmtHandle};
use crate::params::BoundParam;
use crate::params::conversion_matrix::is_supported_conversion;

/// Binds a buffer to a parameter marker in an SQL statement.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
/// - `parameter_value_ptr` / `strlen_or_ind_ptr`, if non-null, must remain valid
///   until the statement is executed (ODBC binds by reference).
/// - When `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is non-null, the readable extents
///   begin at each bound base plus the pointed-to signed byte offset, which may
///   be negative, so every allocation must cover that displaced range.
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

/// # Safety
/// `statement_handle` must point to a live `StmtHandle`.
/// `parameter_value_ptr` and `strlen_or_ind_ptr`, when non-null, must remain
/// valid until the parameter is unbound or the statement is freed. When
/// `SQL_ATTR_PARAM_BIND_OFFSET_PTR` is non-null, the readable extents begin at
/// each bound base plus the pointed-to signed byte offset, which may be
/// negative, so every allocation must cover that displaced range.
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

    let stmt_owner = crate::handles::get_handle!(StmtHandle, statement_handle);
    let stmt = &*stmt_owner;
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
    // The declared ODBC version selects the SQL_C_DEFAULT table. Read it before
    // the stmt lock to preserve parent-before-child lock ordering.
    let odbc_version = {
        let env = stmt.parent_dbc().parent_env();
        let Ok(env_state) = env.inner.lock() else {
            error!("SQLBindParameter: env mutex poisoned");
            return SQL_ERROR;
        };
        env_state.odbc_version
    };

    let Ok(gate) = stmt.parent_dbc().inner.lock() else {
        error!("SQLBindParameter: dbc mutex poisoned");
        return SQL_ERROR;
    };
    let (apd, c_type) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLBindParameter: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if let Err(error) = stmt.param_binding_use.ensure_idle(&gate) {
            return error.post(&mut *stmt_state);
        }

        // Fold the deprecated 2.x date/time C spellings onto the SQL_C_TYPE_*
        // forms so only one form per type reaches validation, conversion, and
        // storage.
        let value_type = canonical_c_type(value_type);

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

        // A descriptor record number is an SQLSMALLINT (07009's own
        // representation): reject an ordinal that can't be represented as
        // one before it ever reaches `bind_param_records`, which would
        // otherwise grow the APD/IPD to `parameter_number`'s full width and
        // then silently truncate the write to record 32767 via `as`/
        // `unwrap_or(SqlSmallInt::MAX)` — binding the wrong parameter while
        // still reporting success.
        if parameter_number > SqlUSmallInt::try_from(SqlSmallInt::MAX).unwrap_or(SqlUSmallInt::MAX)
        {
            error!(
                parameter_number,
                "SQLBindParameter: parameter number exceeds the maximum descriptor record number"
            );
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }
        match classify_parameter_sql_type(parameter_type) {
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
            let Some(resolved) = resolve_default_c_type(parameter_type, odbc_version) else {
                // Unreachable: every Supported type has a default C type, pinned by
                // every_supported_sql_type_has_a_default_c_type.
                debug_assert!(
                    false,
                    "no default C type for supported SQL type {parameter_type}"
                );
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

        // The C type -> SQL type conversion must be one this driver implements.
        if !is_supported_conversion(c_type, parameter_type) {
            error!(
                c_type,
                parameter_type, "SQLBindParameter: unsupported C/SQL type conversion"
            );
            post_diag(&mut stmt_state, ERR_PARAM_CONVERSION_NOT_IMPLEMENTED);
            return SQL_ERROR;
        }

        // ColumnSize is validated last, after the type and conversion checks, the
        // order msodbcsql's SQLBindParameter uses before CheckSqlPrecScale.
        if !parameter_column_size_is_valid(parameter_type, column_size) {
            error!(
                parameter_type,
                column_size, "SQLBindParameter: invalid ColumnSize for the SQL type"
            );
            post_diag(&mut stmt_state, ERR_INVALID_PARAM_PRECISION_OR_SCALE);
            return SQL_ERROR;
        }

        // Output values are only ever produced by a procedure call, so a
        // direction other than input is accepted here and enforced at execute
        // time. Streamed output (SQL_PARAM_OUTPUT_STREAM / _INPUT_OUTPUT_STREAM)
        // is data-at-execution output, which this driver does not implement.
        match input_output_type {
            SQL_PARAM_INPUT | SQL_PARAM_INPUT_OUTPUT | SQL_PARAM_OUTPUT | SQL_RETURN_VALUE => {}
            SQL_PARAM_OUTPUT_STREAM | SQL_PARAM_INPUT_OUTPUT_STREAM => {
                error!(
                    input_output_type,
                    "SQLBindParameter: streamed output parameters are not supported"
                );
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HYC00,
                    0,
                    "Streamed output parameters are not implemented",
                );
                return SQL_ERROR;
            }
            _ => {
                error!(
                    input_output_type,
                    "SQLBindParameter: invalid InputOutputType"
                );
                post_diag(&mut stmt_state, ERR_INVALID_PARAMETER_TYPE);
                return SQL_ERROR;
            }
        }

        // SQLBindParameter/SetIPDRec in sqlcdesc.cpp retain indicator-only
        // output bindings; GetReturnValue treats a null destination as size 0.

        (stmt_state.effective_apd(stmt), c_type)
    };

    let bound = BoundParam {
        input_output_type,
        c_type,
        sql_type: parameter_type,
        column_size,
        decimal_digits,
        app_precision: 0,
        app_scale: 0,
        precision_scale_explicit: false,
        parameter_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
        // SQLBindParameter's one StrLen_or_IndPtr argument feeds both
        // descriptor fields at once (matches msodbcsql's `pIndValue =
        // pcbValue = pcbValue`, sqlcdesc.cpp) — SQL_DESC_INDICATOR_PTR and
        // SQL_DESC_OCTET_LENGTH_PTR only diverge when set independently via
        // SQLSetDescFieldW/SQLSetDescRec.
        octet_length_ptr: strlen_or_ind_ptr,
    };
    if let Err(error) = bind_param_records(apd, stmt.ipd, parameter_number, bound, &gate) {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            error.post(&mut *stmt_state);
        }
        return SQL_ERROR;
    }

    // A rebind invalidates any cached server-side prepared plan: the next
    // SQLExecute must re-prepare so the plan matches the new bindings. This
    // mirrors msodbcsql clearing DESC_CONSISTENT → FIsReprepareRequired. The
    // prepared SQL text is kept; the server handle is orphaned for release
    // (via sp_unprepare) at the next execute, forcing the sp_prepexec path.
    // Runs only now that the write above actually succeeded — orphaning
    // during the earlier STMT-locked validation would discard a still-valid
    // plan for a binding that turned out to fail (e.g. a poisoned/concurrently
    // freed APD) and never actually changed.
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.orphan_prepared_handle();
    } else {
        error!("SQLBindParameter: stmt mutex poisoned; prepared plan not invalidated");
        return SQL_ERROR;
    }

    debug!(parameter_number, "SQLBindParameter: parameter bound");
    SQL_SUCCESS
}

/// Preflights both descriptors before changing either. Lock APD before IPD.
fn bind_param_records(
    apd: SqlHandle,
    ipd: SqlHandle,
    parameter_number: SqlUSmallInt,
    bound: BoundParam,
    gate: &DbcState,
) -> Result<(), BindingError> {
    let apd_desc = owned_descriptor(apd)?;
    let ipd_desc = owned_descriptor(ipd)?;
    let mut apd_state = apd_desc.inner.lock().map_err(|_| BindingError::Poisoned)?;
    let mut ipd_state = ipd_desc.inner.lock().map_err(|_| BindingError::Poisoned)?;
    apd_desc.binding_use.ensure_idle(gate)?;
    ipd_desc.binding_use.ensure_idle(gate)?;

    let record_number =
        SqlSmallInt::try_from(parameter_number).map_err(|_| BindingError::InvalidRecord)?;

    let target_count = apd_state.records.len().max(usize::from(parameter_number));
    apd_state.set_record_count(target_count, apd_desc.kind);
    let target_count = ipd_state.records.len().max(usize::from(parameter_number));
    ipd_state.set_record_count(target_count, ipd_desc.kind);

    let apd_record = apd_state
        .record_mut(record_number)
        .ok_or(BindingError::InvalidRecord)?;
    let ipd_record = ipd_state
        .record_mut(record_number)
        .ok_or(BindingError::InvalidRecord)?;
    bound.write_to_records(apd_record, ipd_record);
    Ok(())
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

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
unsafe fn sql_free_stmt_reset_params_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt_owner = crate::handles::get_handle!(StmtHandle, statement_handle);
    let stmt = &*stmt_owner;
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    sql_free_stmt_reset_params_safe(stmt)
}

fn sql_free_stmt_reset_params_safe(stmt: &StmtHandle) -> SqlReturn {
    let Ok(gate) = stmt.parent_dbc().inner.lock() else {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): dbc mutex poisoned");
        return SQL_ERROR;
    };
    let apd = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFreeStmt(SQL_RESET_PARAMS): stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if let Err(error) = stmt.param_binding_use.ensure_idle(&gate) {
            return error.post(&mut *stmt_state);
        }
        stmt_state.effective_apd(stmt)
    };

    let result = (|| {
        let apd = owned_descriptor(apd)?;
        let ipd = owned_descriptor(stmt.ipd)?;
        let mut apd_state = apd.inner.lock().map_err(|_| BindingError::Poisoned)?;
        let mut ipd_state = ipd.inner.lock().map_err(|_| BindingError::Poisoned)?;
        apd.binding_use.ensure_idle(&gate)?;
        ipd.binding_use.ensure_idle(&gate)?;
        apd_state.set_record_count(0, apd.kind);
        ipd_state.set_record_count(0, ipd.kind);
        Ok::<(), BindingError>(())
    })();
    if let Err(error) = result {
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            error.post(&mut *stmt_state);
        }
        return SQL_ERROR;
    }

    debug!("SQLFreeStmt(SQL_RESET_PARAMS): parameter bindings released");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_SLONG, SQL_GUID, SQL_INTEGER, SQL_NULL_DATA, SQL_NULL_HANDLE,
        SQL_PARAM_OUTPUT, SQL_SS_UDT, SQL_VARBINARY, SQL_VARCHAR,
    };
    use crate::handles::{DescHandle, handle_from_raw};
    use crate::test_support::TestHandles;

    /// Every parameter position on `h`'s implicit APD/IPD, in ordinal order —
    /// the same view `snapshot_bound_params` derives fresh before an execute.
    fn bound_params(h: &TestHandles) -> Vec<Option<BoundParam>> {
        let apd_owner = handle_from_raw::<DescHandle>(h.apd()).unwrap().into_arc();
        let apd = &*apd_owner;
        let ipd_owner = handle_from_raw::<DescHandle>(h.ipd()).unwrap().into_arc();
        let ipd = &*ipd_owner;
        let apd_state = apd.inner.lock().unwrap();
        let ipd_state = ipd.inner.lock().unwrap();
        BoundParam::all_from_descriptor_states(
            &apd_state,
            &ipd_state,
            crate::handles::OdbcVersion::Odbc3_80,
        )
    }

    /// `SQL_DESC_COUNT` on `h`'s implicit APD.
    fn apd_record_count(h: &TestHandles) -> usize {
        let apd_owner = handle_from_raw::<DescHandle>(h.apd()).unwrap().into_arc();
        let apd = &*apd_owner;
        apd.inner.lock().unwrap().records.len()
    }

    /// `SQL_DESC_COUNT` on `h`'s implicit IPD.
    fn ipd_record_count(h: &TestHandles) -> usize {
        let ipd_owner = handle_from_raw::<DescHandle>(h.ipd()).unwrap().into_arc();
        let ipd = &*ipd_owner;
        ipd.inner.lock().unwrap().records.len()
    }

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
    fn output_directions_are_accepted() {
        for direction in [
            SQL_PARAM_INPUT,
            SQL_PARAM_INPUT_OUTPUT,
            SQL_PARAM_OUTPUT,
            SQL_RETURN_VALUE,
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf: Vec<u8> = b"abc\0".to_vec();
            let mut ind: SqlLen = 0;
            let ret = unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    direction,
                    SQL_C_CHAR,
                    SQL_VARCHAR,
                    0,
                    0,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    &mut ind,
                )
            };
            assert_eq!(ret, SQL_SUCCESS, "direction {direction}");
        }
    }

    /// Data-at-execution *output* is a separate feature; ODBC's streamed
    /// directions stay refused rather than silently behaving like plain output.
    #[test]
    fn streamed_output_directions_are_refused() {
        for direction in [SQL_PARAM_OUTPUT_STREAM, SQL_PARAM_INPUT_OUTPUT_STREAM] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut buf: Vec<u8> = b"abc\0".to_vec();
            let mut ind: SqlLen = 0;
            let ret = unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    direction,
                    SQL_C_CHAR,
                    SQL_VARCHAR,
                    0,
                    0,
                    buf.as_mut_ptr() as SqlPointer,
                    buf.len() as SqlLen,
                    &mut ind,
                )
            };
            assert_eq!(ret, SQL_ERROR, "direction {direction}");
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
        }
    }

    #[test]
    fn unknown_direction_is_refused() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                99,
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
    }

    #[test]
    fn output_parameter_accepts_an_indicator_without_a_value_buffer() {
        let h = TestHandles::with_env_dbc_stmt();
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
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
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
        let params = bound_params(&h);
        assert_eq!(params.len(), 3);
        assert!(params[0].is_none());
        assert!(params[1].is_none());
        assert!(params[2].is_some());
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
        assert_eq!(
            apd_record_count(&h),
            1,
            "parameter 1 should have grown the APD"
        );

        let ret = unsafe { sql_free_stmt_reset_params(h.stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            apd_record_count(&h),
            0,
            "SQL_RESET_PARAMS must reset SQL_DESC_COUNT to 0"
        );
        assert!(bound_params(&h).is_empty());
    }

    /// The IPD must be truncated alongside the APD, not just left with a
    /// stale record past the APD's now-zero range: `refine_ipd` no longer
    /// unconditionally overwrites a record (`DescRecord::explicitly_bound`),
    /// so a stale IPD record left behind would look explicitly bound and
    /// stop self-healing on a later `SQLDescribeParam` — surfacing at
    /// execute time if the application rebinds only the APD afterwards.
    #[test]
    fn reset_params_also_truncates_the_ipd() {
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
        assert_eq!(
            ipd_record_count(&h),
            1,
            "parameter 1 should have grown the IPD"
        );

        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        assert_eq!(
            ipd_record_count(&h),
            0,
            "SQL_RESET_PARAMS must reset the IPD too, not just the APD"
        );
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
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
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
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY003);
    }

    #[test]
    fn unsupported_conversion_returns_hyc00() {
        // Both types are supported on their own, but integer -> binary is not a
        // pairing the execute path can convert yet.
        let h = TestHandles::with_env_dbc_stmt();
        let mut val: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_SLONG,
                SQL_VARBINARY,
                0,
                0,
                &mut val as *mut i32 as SqlPointer,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn valid_c_type_with_no_matrix_row_returns_hyc00() {
        // SQL_C_SS_VECTOR is a legal ODBC C type with no row in the conversion
        // matrix, so it must fail the conversion check rather than the HY003
        // identifier check - the two are different answers to the application.
        let h = TestHandles::with_env_dbc_stmt();
        let mut val: [u8; 8] = [0; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                crate::api::odbc_types::SQL_C_SS_VECTOR,
                SQL_INTEGER,
                0,
                0,
                val.as_mut_ptr() as SqlPointer,
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
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
        let bound = bound_params(&h)[0].expect("parameter 1 should be bound");
        assert_eq!(bound.c_type, SQL_C_CHAR);
    }

    #[test]
    fn default_c_type_resolved_but_unconvertible_returns_hyc00() {
        // `SQL_SS_UDT` needs the fully qualified server type name, which
        // `SQLDescribeParam` does not report and the driver cannot otherwise
        // obtain, so a defaulted bind of it is still rejected up front.
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_DEFAULT,
                SQL_SS_UDT,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn default_c_type_guid_is_accepted_and_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = SQL_NULL_DATA;
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
        assert_eq!(ret, SQL_SUCCESS);
        let bound = bound_params(&h)[0].expect("parameter 1 should be bound");
        assert_eq!(bound.c_type, crate::api::odbc_types::SQL_C_GUID);
        assert_eq!(bound.sql_type, SQL_GUID);
    }

    /// ODBC gives some non-character SQL types a character default C type. The
    /// bind result must agree with the implemented conversion row after that
    /// default is resolved.
    ///
    /// The set is derived from `resolve_default_c_type` rather than listed, so a
    /// type that starts defaulting to a character C type is covered here without
    /// anyone remembering to add it.
    #[test]
    fn default_bind_matches_character_conversion_rows() {
        use crate::api::type_rules::classify_parameter_sql_type;
        use crate::handles::OdbcVersion;

        let character_sql_types = [
            crate::api::odbc_types::SQL_CHAR,
            SQL_VARCHAR,
            crate::api::odbc_types::SQL_LONGVARCHAR,
            crate::api::odbc_types::SQL_WCHAR,
            crate::api::odbc_types::SQL_WVARCHAR,
            crate::api::odbc_types::SQL_WLONGVARCHAR,
        ];

        let mut checked = 0;
        for sql_type in -160..=120 {
            if !matches!(
                classify_parameter_sql_type(sql_type),
                SqlTypeSupport::Supported
            ) || character_sql_types.contains(&sql_type)
            {
                continue;
            }
            let Some(default_c) = resolve_default_c_type(sql_type, OdbcVersion::Odbc3_80) else {
                continue;
            };
            if !matches!(default_c, SQL_C_CHAR | crate::api::odbc_types::SQL_C_WCHAR) {
                continue;
            }

            checked += 1;
            let h = TestHandles::with_env_dbc_stmt();
            let mut ind: SqlLen = SQL_NULL_DATA;
            let ret = unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_DEFAULT,
                    sql_type,
                    // A real precision and scale: `decimal` rejects 0 as a
                    // precision, and this path is about the C type, not the size.
                    18,
                    2,
                    std::ptr::null_mut(),
                    0,
                    &mut ind,
                )
            };
            if is_supported_conversion(default_c, sql_type) {
                assert_eq!(ret, SQL_SUCCESS, "sql_type {sql_type}");
                let bound = bound_params(&h)[0].expect("parameter 1 should be bound");
                assert_eq!(bound.c_type, default_c, "sql_type {sql_type}");
            } else {
                assert_eq!(ret, SQL_ERROR, "sql_type {sql_type}");
                let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
                let stmt = &*stmt_owner;
                let state = stmt.inner.lock().unwrap();
                assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
            }
        }
        assert!(checked > 0, "no SQL type defaults to a character C type");
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
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn deprecated_c_type_spelling_is_stored_canonically() {
        // SQL_C_TIMESTAMP is folded to SQL_C_TYPE_TIMESTAMP before validation,
        // so only one spelling per type reaches conversion and storage.
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                crate::api::odbc_types::SQL_C_TIMESTAMP,
                crate::api::odbc_types::SQL_TYPE_TIMESTAMP,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let bound = bound_params(&h)[0].expect("parameter 1 should be bound");
        assert_eq!(
            bound.c_type,
            crate::api::odbc_types::SQL_C_TYPE_TIMESTAMP,
            "the deprecated spelling must be stored canonically"
        );
    }

    #[test]
    fn rebind_invalidates_cached_prepared_handle() {
        use mssql_tds::connection::tds_client::PreparedStatement;

        use crate::handles::stmt::PreparedPlan;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test(
                    "SELECT @P1",
                    mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42),
                ),
                marker_count: 0,
                original_sql: String::new(),
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

    /// Panics while holding the APD lock, leaving the mutex poisoned.
    fn poison_apd(apd: SqlHandle) {
        let handle_owner = handle_from_raw::<DescHandle>(apd).unwrap().into_arc();
        let handle = &*handle_owner;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.inner.lock().unwrap();
            panic!("poison the apd lock");
        }));
    }

    /// A bind that fails after the STMT-locked validation (here, a poisoned
    /// APD makes `bind_param_records` return `Err`) must leave a cached
    /// prepared plan alone: orphaning it for a binding that was never
    /// actually applied would force a needless re-prepare on the next
    /// execute for no corresponding state change.
    #[test]
    fn a_failed_bind_does_not_orphan_the_prepared_handle() {
        use mssql_tds::connection::tds_client::PreparedStatement;

        use crate::handles::stmt::PreparedPlan;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test(
                    "SELECT @P1",
                    mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42),
                ),
                marker_count: 0,
                original_sql: String::new(),
            });
        }
        poison_apd(h.apd());

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
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.prepared.as_ref().and_then(|p| p.stmt.id()),
            Some(mssql_tds::connection::tds_client::StatementId::from_raw_for_test(42)),
            "a failed bind must not orphan a plan that was never actually invalidated"
        );
        assert!(
            state.pending_unprepare.is_none(),
            "nothing should be queued for release when the bind itself failed"
        );
    }

    /// After `SQLSetStmtAttrW` reassociates the APD, `SQLBindParameter` must
    /// write through to the *new* explicit descriptor, not the implicit one
    /// it replaced — the actual bind-through-reassociation path, matching
    /// `bind_col.rs`'s identical ARD test.
    #[test]
    fn bind_parameter_writes_through_a_reassociated_apd() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_apd = h.alloc_explicit_desc();
        assert_eq!(
            unsafe {
                crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                    h.stmt,
                    crate::api::odbc_types::SQL_ATTR_APP_PARAM_DESC,
                    explicit_apd as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );

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

        let explicit_owner = handle_from_raw::<DescHandle>(explicit_apd)
            .unwrap()
            .into_arc();
        let explicit = &*explicit_owner;
        assert_eq!(
            explicit.inner.lock().unwrap().records.len(),
            1,
            "the bind must land on the reassociated descriptor"
        );
        assert_eq!(
            apd_record_count(&h),
            0,
            "the implicit APD it replaced must be untouched"
        );
    }

    /// Freeing the explicit descriptor currently associated as the APD
    /// resets the statement back to its implicit APD (`free_desc`'s existing
    /// association-reset logic) — and a *subsequent* `SQLBindParameter` must
    /// write through to that implicit descriptor rather than erroring or
    /// touching the now-freed one.
    #[test]
    fn bind_parameter_falls_back_to_the_implicit_apd_after_the_explicit_one_is_freed() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_apd = h.alloc_explicit_desc();
        unsafe {
            crate::api::set_stmt_attr::sql_set_stmt_attr_w(
                h.stmt,
                crate::api::odbc_types::SQL_ATTR_APP_PARAM_DESC,
                explicit_apd as SqlPointer,
                0,
            )
        };
        assert_eq!(h.free_explicit_desc(explicit_apd), SQL_SUCCESS);

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
        assert_eq!(
            ret, SQL_SUCCESS,
            "must fall back to the implicit APD, not error or touch freed memory"
        );
        assert_eq!(apd_record_count(&h), 1);
    }

    #[test]
    fn bind_param_records_fails_cleanly_on_a_freed_apd() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit_apd = h.alloc_explicit_desc();
        assert_eq!(h.free_explicit_desc(explicit_apd), SQL_SUCCESS);

        let mut buf = 0i32;
        let bound = BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: crate::api::odbc_types::SQL_C_SLONG,
            sql_type: crate::api::odbc_types::SQL_INTEGER,
            column_size: 0,
            decimal_digits: 0,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: &mut buf as *mut i32 as SqlPointer,
            buffer_length: 4,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        };
        let dbc = handle_from_raw::<crate::handles::DbcHandle>(h.dbc)
            .unwrap()
            .into_arc();
        let gate = dbc.inner.lock().unwrap();
        assert!(bind_param_records(explicit_apd, h.ipd(), 1, bound, &gate).is_err());
    }
}
