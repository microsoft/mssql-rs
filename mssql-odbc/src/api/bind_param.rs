// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLBindParameter — bind an application buffer to a
//! statement parameter marker.

use tracing::{debug, error};

use std::mem::size_of;

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SQL_PARAM_INPUT, SQL_PARAM_INPUT_OUTPUT,
    SQL_PARAM_INPUT_OUTPUT_STREAM, SQL_PARAM_OUTPUT, SQL_PARAM_OUTPUT_STREAM, SQL_RETURN_VALUE,
    SQL_SS_XML, SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlULen,
    SqlUSmallInt,
};
use crate::api::type_rules::{
    SqlTypeSupport, canonical_c_type, classify_parameter_sql_type, is_valid_c_type,
    parameter_column_size_is_valid, resolve_default_c_type,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{DescHandle, HandleType, StmtHandle, handle_from_raw};
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
    crate::ffi_entry!(
        "SQLBindParameter",
        statement_handle,
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
        ),
        unsafe {
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
        }
    )
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

    // Validated under the STMT lock, exactly as before, but the write lands
    // on the effective APD's and the IPD's own DescState records (AB#47437:
    // the descriptors are the storage `SQLBindParameter` and
    // `SQLSetDescFieldW` share, not a separate table), which need their own
    // locks. The STMT lock is dropped before those are taken — this crate
    // never holds a STMT lock while acquiring a DESC lock (see
    // bind_col.rs's identical rationale for SQLBindCol).
    let (apd, c_type, param_focus) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLBindParameter: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        // The current table-valued-parameter focus. When non-zero, this bind
        // targets a constituent column of that TVP rather than a top-level
        // parameter (see the routing after the validation block).
        let param_focus = stmt_state
            .vendor_attrs
            .get(crate::api::odbc_types::SQL_SOPT_SS_PARAM_FOCUS)
            .unwrap_or(0);

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

        // A table-valued parameter is not a scalar conversion: its top-level
        // ValueType must be SQL_C_BINARY (SQL_C_DEFAULT already resolved to it
        // above), and its ColumnSize is the row capacity rather than a
        // precision. Neither the scalar conversion matrix nor the scalar
        // ColumnSize check applies - the table's rows are assembled from the
        // columns bound under SQL_SOPT_SS_PARAM_FOCUS at execute, not from this
        // buffer. Every other type goes through both checks.
        if parameter_type == crate::api::odbc_types::SQL_SS_TABLE {
            if c_type != crate::api::odbc_types::SQL_C_BINARY {
                error!(
                    c_type,
                    "SQLBindParameter: a table-valued parameter requires SQL_C_BINARY or SQL_C_DEFAULT"
                );
                post_diag(&mut stmt_state, ERR_PARAM_CONVERSION_NOT_IMPLEMENTED);
                return SQL_ERROR;
            }
        } else {
            // The C type -> SQL type conversion must be one this driver implements.
            if !is_supported_conversion(c_type, parameter_type) {
                error!(
                    c_type,
                    parameter_type, "SQLBindParameter: unsupported C/SQL type conversion"
                );
                post_diag(&mut stmt_state, ERR_PARAM_CONVERSION_NOT_IMPLEMENTED);
                return SQL_ERROR;
            }

            let column_size_is_valid = parameter_column_size_is_valid(parameter_type, column_size);
            let potentially_streamed_large_xml = !column_size_is_valid
                && parameter_type == SQL_SS_XML
                && matches!(input_output_type, SQL_PARAM_INPUT | SQL_PARAM_INPUT_OUTPUT);

            // ColumnSize is validated last, after the type and conversion checks, the
            // order msodbcsql's SQLBindParameter uses before CheckSqlPrecScale.
            //
            // StrLen_or_IndPtr is deferred input and need not be initialized until
            // execution. Oversized XML input is therefore admitted here and
            // revalidated against the positioned execution-time indicator.
            if !column_size_is_valid && !potentially_streamed_large_xml {
                error!(
                    parameter_type,
                    column_size, "SQLBindParameter: invalid ColumnSize for the SQL type"
                );
                post_diag(&mut stmt_state, ERR_INVALID_PARAM_PRECISION_OR_SCALE);
                return SQL_ERROR;
            }
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

        // A table-valued parameter is input-only, and cannot nest inside
        // another TVP. msodbcsql's SQLBindParameter rejects a non-input
        // direction for SQL_SS_TABLE with HY105 (IDS_S1_105), and a TVP bound
        // while focus is set (which would make it a column of another TVP) with
        // HY004 (IDS_S1_004) - both up front, rather than deferring to execute.
        if parameter_type == crate::api::odbc_types::SQL_SS_TABLE {
            if param_focus != 0 {
                error!("SQLBindParameter: table-valued parameters cannot be nested");
                post_diag(&mut stmt_state, ERR_INVALID_SQL_DATA_TYPE);
                return SQL_ERROR;
            }
            if input_output_type != SQL_PARAM_INPUT {
                error!(
                    input_output_type,
                    "SQLBindParameter: a table-valued parameter must be input-only"
                );
                post_diag(&mut stmt_state, ERR_INVALID_PARAMETER_TYPE);
                return SQL_ERROR;
            }
            // Re-declaring a TVP at this ordinal supersedes any columns captured
            // for a previous one, so drop them before new columns are focused in.
            stmt_state
                .tvp_column_bindings
                .remove(&usize::from(parameter_number));
        }

        // SQLBindParameter/SetIPDRec in sqlcdesc.cpp retain indicator-only
        // output bindings; GetReturnValue treats a null destination as size 0.

        (stmt_state.effective_apd(stmt), c_type, param_focus)
    };

    let mut bound = BoundParam {
        input_output_type,
        c_type,
        sql_type: parameter_type,
        column_size,
        decimal_digits,
        app_precision: 0,
        app_scale: 0,
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

    // A bind issued while SQL_SOPT_SS_PARAM_FOCUS is set is a constituent
    // column of that TVP, not a top-level parameter: capture it under the TVP's
    // ordinal (indexed by column ordinal) instead of writing the top-level
    // APD/IPD, mirroring msodbcsql routing the bind to the TVP's child
    // descriptor. The column's own type was validated above like any scalar.
    if param_focus != 0 {
        if input_output_type != SQL_PARAM_INPUT {
            error!("SQLBindParameter: a table-valued parameter column must be input-only");
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLBindParameter: stmt mutex poisoned rejecting TVP column direction");
                return SQL_ERROR;
            };
            post_diag(&mut stmt_state, ERR_INVALID_PARAMETER_TYPE);
            return SQL_ERROR;
        }
        let mut defaults =
            crate::handles::desc::DescRecord::default_for(crate::handles::desc::DescKind::AppParam);
        defaults.reset_app_type_defaults(c_type);
        bound.app_precision = defaults.precision;
        bound.app_scale = defaults.scale;
        let tvp_ordinal = param_focus;
        let column_index = usize::from(parameter_number) - 1;
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLBindParameter: stmt mutex poisoned storing TVP column");
            return SQL_ERROR;
        };
        let columns = stmt_state
            .tvp_column_bindings
            .entry(tvp_ordinal)
            .or_default();
        if column_index >= columns.len() {
            columns.resize(column_index + 1, None);
        }
        columns[column_index] = Some(bound);
        debug!(
            tvp_ordinal,
            column = parameter_number,
            "SQLBindParameter: TVP column bound under focus"
        );
        return SQL_SUCCESS;
    }

    let Ok(definition_changed) = bind_param_records(apd, stmt.ipd, parameter_number, bound) else {
        error!("SQLBindParameter: failed writing to apd/ipd (poisoned mutex or missing record)");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error binding parameter",
            );
        }
        return SQL_ERROR;
    };

    // Classic SetIPDRec uses ParamInfoSnapshot::FHasChanged to mark RE_PREPARE
    // and DropPrepHandle(FALSE); DESC_CONSISTENT only controls validation.
    if definition_changed
        && stmt
            .invalidate_parameter_definition(usize::from(parameter_number))
            .is_err()
    {
        return SQL_ERROR;
    }

    // A top-level TVP's type name may be supplied in the value buffer, which
    // maps to SQL_CA_SS_TYPE_NAME (the ODBC SQLBindParameter contract; msodbcsql
    // reads it there). Record it on the IPD so execute can name the table type
    // even without a prior SQLDescribeParam. Done after the bind, when the IPD
    // record exists; a `None` here leaves any describe-supplied name in place.
    if parameter_type == crate::api::odbc_types::SQL_SS_TABLE {
        match unsafe { read_tvp_type_name(parameter_value_ptr, buffer_length) } {
            Ok(Some(name)) => match store_tvp_type_name(stmt.ipd, parameter_number, name) {
                Ok(changed) => {
                    if changed
                        && stmt
                            .invalidate_parameter_definition(usize::from(parameter_number))
                            .is_err()
                    {
                        return SQL_ERROR;
                    }
                }
                Err(()) => {
                    error!("SQLBindParameter: failed storing TVP type name");
                    if let Ok(mut stmt_state) = stmt.inner.lock() {
                        post_sql_error(
                            &mut stmt_state,
                            SQLSTATE_HY000,
                            0,
                            "Internal error storing table-valued parameter type name",
                        );
                    }
                    return SQL_ERROR;
                }
            },
            Ok(None) => {}
            Err(TvpTypeNameReadError::InvalidLength) => {
                error!("SQLBindParameter: invalid TVP type name length");
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
                }
                return SQL_ERROR;
            }
            Err(TvpTypeNameReadError::MemoryAllocation) => {
                error!("SQLBindParameter: failed reading TVP type name");
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_diag(&mut stmt_state, ERR_MEMORY_ALLOCATION);
                }
                return SQL_ERROR;
            }
        }
    }

    debug!(parameter_number, "SQLBindParameter: parameter bound");
    SQL_SUCCESS
}

#[derive(Debug, PartialEq, Eq)]
enum TvpTypeNameReadError {
    InvalidLength,
    MemoryAllocation,
}

/// Reads a table-valued-parameter type name from a `SQLBindParameter` value
/// buffer: `buffer_length` bytes of UTF-16 when positive, or a NUL-terminated
/// UTF-16 string when `SQL_NTS`. Returns `None` for a null pointer, a
/// non-length, or an empty name.
///
/// # Safety
/// `ptr` must be null or address a readable UTF-16 buffer of the indicated
/// length (or NUL-terminated for `SQL_NTS`).
unsafe fn read_tvp_type_name(
    ptr: SqlPointer,
    buffer_length: SqlLen,
) -> Result<Option<String>, TvpTypeNameReadError> {
    if ptr.is_null() {
        return Ok(None);
    }
    let ptr = ptr.cast::<u8>();
    let units: Vec<u16> = if buffer_length > 0 {
        if buffer_length % size_of::<u16>() as SqlLen != 0 {
            return Err(TvpTypeNameReadError::InvalidLength);
        }
        let count = usize::try_from(buffer_length)
            .map_err(|_| TvpTypeNameReadError::InvalidLength)?
            / size_of::<u16>();
        let mut units = Vec::new();
        units
            .try_reserve(count)
            .map_err(|_| TvpTypeNameReadError::MemoryAllocation)?;
        for i in 0..count {
            // ODBC does not require the application buffer to be aligned for
            // SQLWCHAR, so read each code unit without creating a u16 slice.
            let offset = i
                .checked_mul(size_of::<u16>())
                .ok_or(TvpTypeNameReadError::InvalidLength)?;
            units.push(unsafe { ptr.add(offset).cast::<u16>().read_unaligned() });
        }
        units
    } else if SqlLen::from(SQL_NTS) == buffer_length {
        let mut units = Vec::new();
        let mut i = 0usize;
        loop {
            let offset = i
                .checked_mul(size_of::<u16>())
                .ok_or(TvpTypeNameReadError::InvalidLength)?;
            let unit = unsafe { ptr.wrapping_add(offset).cast::<u16>().read_unaligned() };
            if unit == 0 {
                break;
            }
            if units.len() == units.capacity() {
                units
                    .try_reserve(1)
                    .map_err(|_| TvpTypeNameReadError::MemoryAllocation)?;
            }
            units.push(unit);
            i = i
                .checked_add(1)
                .ok_or(TvpTypeNameReadError::InvalidLength)?;
        }
        units
    } else {
        return Ok(None);
    };
    let name = String::from_utf16_lossy(&units);
    let name = name.trim_end_matches('\0');
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// Records `name` as the table type name on `ipd`'s record, claiming it so a
/// later `SQLDescribeParam` does not overwrite the application's choice (the
/// same provenance the `SQL_CA_SS_*` descriptor-field writes use).
fn store_tvp_type_name(
    ipd: SqlHandle,
    record_number: SqlUSmallInt,
    name: String,
) -> Result<bool, ()> {
    let record_number = SqlSmallInt::try_from(record_number).map_err(|_| ())?;
    let desc = unsafe { handle_from_raw::<DescHandle>(ipd) };
    let mut state = desc.inner.lock().map_err(|_| ())?;
    let Some(record) = state.record_mut(record_number) else {
        return Err(());
    };
    let previous = record.parameter_definition();
    {
        let names = std::sync::Arc::make_mut(record.udt_names.get_or_insert_with(Default::default));
        names.type_name = name;
        record.udt_name_claimed.type_name = true;
    }
    Ok(previous != record.parameter_definition())
}

/// Writes `bound` into `apd`'s and `ipd`'s records at `parameter_number`,
/// growing either record list first if that ordinal doesn't exist yet on it.
/// Returns whether the SQL definition changed. `Err(())` on a poisoned mutex
/// (either descriptor) or a missing record
/// after growth — the caller decides how to report that against the
/// statement, since bind errors are always posted to the STMT handle, never
/// a descriptor. `parameter_number` must already fit `SqlSmallInt`
/// (`sql_bind_parameter_safe` rejects an out-of-range ordinal before this is
/// ever called), so the conversion here is not expected to fail in practice —
/// but this still reports it as an error rather than panicking or silently
/// truncating to the wrong record.
///
/// Always locks `apd` before `ipd` (see
/// ".github/instructions/mssql-odbc.instructions.md", "Locking rules" —
/// "APD before IPD"): the only place in this crate that holds two DESC locks
/// at once, so this order must stay the only order, matching how
/// [`BoundParam::all_from_descriptor_states`] reads them back.
fn bind_param_records(
    apd: SqlHandle,
    ipd: SqlHandle,
    parameter_number: SqlUSmallInt,
    bound: BoundParam,
) -> Result<bool, ()> {
    // `apd` can be an explicit descriptor `effective_apd` resolved under the
    // STMT lock, already dropped by the time this runs — re-check liveness
    // right before dereferencing to narrow (not fully close) the race against
    // a concurrent `SQLFreeHandle(SQL_HANDLE_DESC)` on that same descriptor.
    // `ipd` is always `stmt.ipd`, freed only with the statement itself, so it
    // needs no equivalent check.
    if crate::handles::live_type(apd) != Some(HandleType::Desc) {
        return Err(());
    }
    let apd_desc = unsafe { handle_from_raw::<DescHandle>(apd) };
    let Ok(mut apd_state) = apd_desc.inner.lock() else {
        return Err(());
    };
    let ipd_desc = unsafe { handle_from_raw::<DescHandle>(ipd) };
    let Ok(mut ipd_state) = ipd_desc.inner.lock() else {
        return Err(());
    };

    let record_number = SqlSmallInt::try_from(parameter_number).map_err(|_| ())?;
    let previous = ipd_state
        .record(record_number)
        .map(crate::handles::desc::DescRecord::parameter_definition);

    let target_count = apd_state.records.len().max(usize::from(parameter_number));
    apd_state.set_record_count(target_count, apd_desc.kind);
    let target_count = ipd_state.records.len().max(usize::from(parameter_number));
    ipd_state.set_record_count(target_count, ipd_desc.kind);

    let apd_record = apd_state.record_mut(record_number).ok_or(())?;
    let ipd_record = ipd_state.record_mut(record_number).ok_or(())?;
    bound.write_to_records(apd_record, ipd_record);
    Ok(previous != Some(ipd_record.parameter_definition()))
}

/// Implements the `SQL_RESET_PARAMS` option of `SQLFreeStmt` — releases all
/// parameter bindings on the statement and invalidates its parameter declaration.
/// The cursor state is left untouched.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_free_stmt_reset_params(statement_handle: SqlHandle) -> SqlReturn {
    crate::ffi_entry!(
        "SQLFreeStmt(SQL_RESET_PARAMS)",
        statement_handle,
        debug!(?statement_handle, "SQLFreeStmt(SQL_RESET_PARAMS) called"),
        unsafe { sql_free_stmt_reset_params_impl(statement_handle) }
    )
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
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
    // Per spec (`SQLFreeStmt`'s `SQL_RESET_PARAMS` option) this sets
    // `SQL_DESC_COUNT` on the APD to 0 — a real truncation, matching
    // `SQL_UNBIND`'s identical rule for the ARD (bind_col.rs).
    //
    // The IPD is truncated too, beyond what the spec text names: leaving it
    // alone once relied on `all_from_descriptor_states` only ever iterating
    // as far as the (now-truncated) APD, so a stale IPD record past that
    // range was simply never visited — but that invariant broke the moment
    // `describe_param.rs`'s `refine_ipd` stopped unconditionally overwriting
    // a record (`DescRecord::explicitly_bound`), since a stale record now
    // *looks* explicitly bound and no longer self-heals on the next
    // `SQLDescribeParam`. An application that rebinds only the APD
    // afterwards (`SQLSetDescField`/`SQLSetDescRec`, no matching IPD write)
    // would otherwise pick the old type, direction and size back up at the
    // next execute. `stmt.ipd` is never reassociated or independently freed
    // (`SQL_ATTR_IMP_PARAM_DESC` is read-only), so this needs no liveness
    // recheck the way the APD's resolution does below.
    let apd = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFreeStmt(SQL_RESET_PARAMS): stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        // The TVP column captures are part of the parameter bindings this
        // option resets.
        stmt_state.tvp_column_bindings.clear();
        stmt_state.tvp_column_layouts.clear();
        stmt_state.effective_apd(stmt)
    };

    // `apd` can be an explicit descriptor resolved under the STMT lock,
    // already dropped by now — re-check liveness right before dereferencing
    // to narrow the race against a concurrent
    // `SQLFreeHandle(SQL_HANDLE_DESC)` on that same descriptor.
    if crate::handles::live_type(apd) != Some(HandleType::Desc) {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): apd freed concurrently; reset failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error resetting parameter bindings",
            );
        }
        return SQL_ERROR;
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(apd) };
    let Ok(mut desc_state) = desc.inner.lock() else {
        error!("SQLFreeStmt(SQL_RESET_PARAMS): apd mutex poisoned; reset failed");
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HY000,
                0,
                "Internal error resetting parameter bindings",
            );
        }
        return SQL_ERROR;
    };
    desc_state.set_record_count(0, desc.kind);
    drop(desc_state);

    // Best-effort beyond the spec-mandated APD reset above: log rather than
    // fail the whole call if the IPD mutex is poisoned, since the required
    // behavior already succeeded.
    let ipd = unsafe { handle_from_raw::<DescHandle>(stmt.ipd) };
    match ipd.inner.lock() {
        Ok(mut ipd_state) => ipd_state.set_record_count(0, ipd.kind),
        Err(_) => error!("SQLFreeStmt(SQL_RESET_PARAMS): ipd mutex poisoned; IPD left stale"),
    }
    if stmt.invalidate_parameter_definition(1).is_err() {
        return SQL_ERROR;
    }

    debug!("SQLFreeStmt(SQL_RESET_PARAMS): parameter bindings released");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_BINARY, SQL_C_CHAR, SQL_C_SLONG, SQL_GUID, SQL_INTEGER, SQL_NTS, SQL_NULL_DATA,
        SQL_NULL_HANDLE, SQL_PARAM_OUTPUT, SQL_SS_UDT, SQL_VARBINARY, SQL_VARCHAR,
    };
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    /// Every parameter position on `h`'s implicit APD/IPD, in ordinal order —
    /// the same view `snapshot_bound_params` derives fresh before an execute.
    fn bound_params(h: &TestHandles) -> Vec<Option<crate::params::ParamSnapshot>> {
        let apd = unsafe { handle_from_raw::<DescHandle>(h.apd()) };
        let ipd = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
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
        let apd = unsafe { handle_from_raw::<DescHandle>(h.apd()) };
        apd.inner.lock().unwrap().records.len()
    }

    /// `SQL_DESC_COUNT` on `h`'s implicit IPD.
    fn ipd_record_count(h: &TestHandles) -> usize {
        let ipd = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
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
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
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
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
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
        let binding = bound_params(&h);
        let bound = binding[0].as_ref().expect("parameter 1 should be bound");
        assert_eq!(bound.param.c_type, SQL_C_CHAR);
    }

    #[test]
    fn tvp_binds_with_default_c_type_resolving_to_binary() {
        // `SQL_SS_TABLE` resolves `SQL_C_DEFAULT` to `SQL_C_BINARY`, the only
        // value type a TVP accepts. Binding it as an input parameter now
        // succeeds (the top-level identity and rows are supplied through the
        // IPD name fields and the focused columns, not this buffer).
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_DEFAULT,
                crate::api::odbc_types::SQL_SS_TABLE,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let binding = bound_params(&h);
        let bound = binding[0].as_ref().expect("parameter 1 should be bound");
        assert_eq!(bound.param.c_type, SQL_C_BINARY);
    }

    /// A table-valued parameter is input-only; any other direction is rejected
    /// at bind with HY105, matching msodbcsql's `SQLBindParameter`
    /// (`IDS_S1_105`) rather than being deferred to execute.
    #[test]
    fn tvp_rejects_a_non_input_direction() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_OUTPUT,
                SQL_C_BINARY,
                crate::api::odbc_types::SQL_SS_TABLE,
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
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY105);
    }

    #[test]
    fn tvp_columns_reject_non_input_directions() {
        for direction in [SQL_PARAM_OUTPUT, SQL_PARAM_INPUT_OUTPUT] {
            let h = TestHandles::with_env_dbc_stmt();
            bind_and_focus_tvp(&h, 1);
            let mut value = 17i32;
            let mut indicator: SqlLen = 0;
            let ret = unsafe {
                crate::api::exports::SQLBindParameter(
                    h.stmt,
                    1,
                    direction,
                    crate::api::odbc_types::SQL_C_SLONG,
                    crate::api::odbc_types::SQL_INTEGER,
                    10,
                    0,
                    (&raw mut value).cast(),
                    std::mem::size_of::<i32>() as SqlLen,
                    &mut indicator,
                )
            };
            assert_eq!(ret, SQL_ERROR);
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY105);
            assert!(!state.tvp_column_bindings.contains_key(&1));
        }
    }

    /// The table type name supplied in the value buffer is recorded on the IPD
    /// (claimed), so execute can name the type without a prior SQLDescribeParam;
    /// the ODBC SQLBindParameter contract maps `ParameterValuePtr` to
    /// `SQL_CA_SS_TYPE_NAME`.
    #[test]
    fn tvp_bind_records_the_type_name_from_the_value_buffer() {
        use crate::handles::desc::DescHandle;
        let h = TestHandles::with_env_dbc_stmt();
        let mut name: Vec<u16> = "OrderList".encode_utf16().collect();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_BINARY,
                crate::api::odbc_types::SQL_SS_TABLE,
                0,
                0,
                name.as_mut_ptr() as SqlPointer,
                (name.len() * 2) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let ipd = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
        let state = ipd.inner.lock().unwrap();
        let record = state.record(1).expect("ipd record 1");
        let names = record.udt_names.as_ref().expect("type name recorded");
        assert_eq!(names.type_name, "OrderList");
        assert!(record.udt_name_claimed.type_name, "name is claimed");
    }

    #[test]
    fn tvp_type_name_buffer_may_be_unaligned() {
        let units: Vec<u16> = "OrderList".encode_utf16().collect();
        let mut bytes = vec![0u8; units.len() * size_of::<u16>() + 1];
        for (index, unit) in units.iter().enumerate() {
            let offset = 1 + index * size_of::<u16>();
            bytes[offset..offset + 2].copy_from_slice(&unit.to_ne_bytes());
        }
        let name = unsafe {
            read_tvp_type_name(
                bytes.as_mut_ptr().add(1).cast(),
                (units.len() * size_of::<u16>()) as SqlLen,
            )
        };
        assert_eq!(name, Ok(Some("OrderList".to_string())));
    }

    #[test]
    fn tvp_type_name_rejects_odd_byte_lengths() {
        let mut bytes = [b'O', 0, b'r'];
        let name = unsafe { read_tvp_type_name(bytes.as_mut_ptr().cast(), bytes.len() as SqlLen) };
        assert_eq!(name, Err(TvpTypeNameReadError::InvalidLength));
    }

    /// Helper: bind a scalar `SQL_C_SLONG` -> `SQL_INTEGER` column at
    /// `ordinal`, returning the `SQLBindParameter` result.
    fn bind_int_column(
        h: &TestHandles,
        ordinal: SqlUSmallInt,
        value: &mut i32,
        ind: &mut SqlLen,
    ) -> SqlReturn {
        unsafe {
            sql_bind_parameter(
                h.stmt,
                ordinal,
                SQL_PARAM_INPUT,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                0,
                value as *mut i32 as SqlPointer,
                0,
                ind,
            )
        }
    }

    /// Helper: bind a TVP at `ordinal` and focus on it, directly setting the
    /// focus to avoid the cross-module attribute call.
    fn bind_and_focus_tvp(h: &TestHandles, ordinal: SqlUSmallInt) {
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                ordinal,
                SQL_PARAM_INPUT,
                SQL_C_BINARY,
                crate::api::odbc_types::SQL_SS_TABLE,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "binding the TVP at {ordinal}");
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .vendor_attrs
            .store_param_focus(SqlULen::from(ordinal));
    }

    /// While SQL_SOPT_SS_PARAM_FOCUS names a TVP, SQLBindParameter captures the
    /// bind as one of that TVP's columns instead of writing the top-level
    /// APD/IPD. The top-level records keep only the TVP itself.
    #[test]
    fn tvp_columns_are_captured_under_focus() {
        let h = TestHandles::with_env_dbc_stmt();
        bind_and_focus_tvp(&h, 1);

        let (mut c0, mut c1) = (10i32, 20i32);
        let (mut i0, mut i1) = (0 as SqlLen, 0 as SqlLen);
        assert_eq!(bind_int_column(&h, 1, &mut c0, &mut i0), SQL_SUCCESS);
        assert_eq!(bind_int_column(&h, 2, &mut c1, &mut i1), SQL_SUCCESS);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let columns = state
            .tvp_column_bindings
            .get(&1)
            .expect("the TVP should have captured columns");
        assert_eq!(columns.len(), 2);
        assert!(columns[0].is_some() && columns[1].is_some());
        // The focused column binds did not grow the top-level IPD past the TVP.
        drop(state);
        let ipd = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
        assert_eq!(ipd.inner.lock().unwrap().records.len(), 1);
    }

    #[test]
    fn focused_apd_data_pointer_updates_the_tvp_column_binding() {
        use crate::api::odbc_types::{SQL_DESC_DATA_PTR, SqlPointer, SqlSmallInt};

        let h = TestHandles::with_env_dbc_stmt();
        bind_and_focus_tvp(&h, 1);
        let mut original = 11i32;
        let mut indicator: SqlLen = 0;
        assert_eq!(
            bind_int_column(&h, 1, &mut original, &mut indicator),
            SQL_SUCCESS
        );

        let mut replacement = 99i32;
        let replacement_ptr = (&raw mut replacement).cast();
        let apd = h.apd();
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_DATA_PTR as SqlSmallInt,
                    replacement_ptr,
                    0,
                )
            },
            SQL_SUCCESS
        );

        let mut returned: SqlPointer = std::ptr::null_mut();
        assert_eq!(
            unsafe {
                crate::api::exports::SQLGetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_DATA_PTR as SqlSmallInt,
                    (&raw mut returned).cast(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(returned, replacement_ptr);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let column = state.tvp_column_bindings[&1][0].unwrap();
        assert_eq!(column.parameter_value_ptr, replacement_ptr);
        let value =
            unsafe { crate::conversion::param_convert::bound_tvp_cell_to_value(&column) }.unwrap();
        assert_eq!(
            value,
            mssql_tds::datatypes::sqltypes::SqlType::Int(Some(99))
        );
    }

    #[test]
    fn focused_numeric_bind_uses_apd_defaults_and_preserves_decimal_value() {
        use crate::api::odbc_types::{SQL_C_NUMERIC, SQL_DECIMAL, SqlNumericStruct};
        use mssql_tds::datatypes::sqltypes::SqlType;

        for (magnitude, positive, expected) in [(120u128, 1, 12u128), (125, 1, 13), (125, 0, 13)] {
            let h = TestHandles::with_env_dbc_stmt();
            bind_and_focus_tvp(&h, 1);
            let mut value = SqlNumericStruct {
                precision: 38,
                scale: 1,
                sign: positive,
                val: magnitude.to_le_bytes(),
            };
            let mut indicator: SqlLen = 0;
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLBindParameter(
                        h.stmt,
                        1,
                        SQL_PARAM_INPUT,
                        SQL_C_NUMERIC,
                        SQL_DECIMAL,
                        38,
                        0,
                        (&raw mut value).cast(),
                        0,
                        &mut indicator,
                    )
                },
                SQL_SUCCESS
            );
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let column = stmt.inner.lock().unwrap().tvp_column_bindings[&1][0].unwrap();
            assert_eq!((column.app_precision, column.app_scale), (38, 0));
            let result =
                unsafe { crate::conversion::param_convert::bound_tvp_cell_to_value(&column) }
                    .unwrap();
            let SqlType::Decimal(Some(parts)) = result else {
                panic!("expected decimal cell");
            };
            assert_eq!(parts.magnitude(), expected);
            assert_eq!(parts.scale, 0);
            assert_eq!(parts.is_positive, positive != 0);
        }
    }

    #[test]
    fn focused_numeric_descriptor_precision_and_scale_update_tvp_conversion() {
        use crate::api::odbc_types::{
            SQL_C_NUMERIC, SQL_DECIMAL, SQL_DESC_PRECISION, SQL_DESC_SCALE, SqlNumericStruct,
            SqlSmallInt,
        };
        use mssql_tds::datatypes::sqltypes::SqlType;

        let h = TestHandles::with_env_dbc_stmt();
        bind_and_focus_tvp(&h, 1);
        let mut value = SqlNumericStruct {
            precision: 38,
            scale: 2,
            sign: 1,
            val: 123u128.to_le_bytes(),
        };
        let mut indicator: SqlLen = 0;
        assert_eq!(
            unsafe {
                crate::api::exports::SQLBindParameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    SQL_C_NUMERIC,
                    SQL_DECIMAL,
                    10,
                    2,
                    (&raw mut value).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );

        let apd = h.apd();
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_PRECISION as SqlSmallInt,
                    10isize as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_SCALE as SqlSmallInt,
                    2isize as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );

        for (field, expected) in [(SQL_DESC_PRECISION, 10), (SQL_DESC_SCALE, 2)] {
            let mut actual: SqlSmallInt = -1;
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLGetDescFieldW(
                        apd,
                        1,
                        field as SqlSmallInt,
                        (&raw mut actual).cast(),
                        0,
                        std::ptr::null_mut(),
                    )
                },
                SQL_SUCCESS
            );
            assert_eq!(actual, expected);
        }

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let column = stmt.inner.lock().unwrap().tvp_column_bindings[&1][0].unwrap();
        assert_eq!((column.app_precision, column.app_scale), (10, 2));
        let SqlType::Decimal(Some(parts)) =
            unsafe { crate::conversion::param_convert::bound_tvp_cell_to_value(&column) }.unwrap()
        else {
            panic!("expected decimal cell");
        };
        assert_eq!(parts.magnitude(), 123);
        assert_eq!(parts.scale, 2);

        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_SCALE as SqlSmallInt,
                    10isize as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetDescFieldW(
                    apd,
                    1,
                    SQL_DESC_PRECISION as SqlSmallInt,
                    5isize as SqlPointer,
                    0,
                )
            },
            SQL_ERROR
        );
        let desc = unsafe { handle_from_raw::<crate::handles::DescHandle>(apd) };
        assert_eq!(
            desc.inner.lock().unwrap().diag_records[0].sql_state,
            crate::api::sqlstate::ERR_INVALID_PRECISION_OR_SCALE.state
        );
        let column = stmt.inner.lock().unwrap().tvp_column_bindings[&1][0].unwrap();
        assert_eq!((column.app_precision, column.app_scale), (10, 10));
    }

    #[test]
    fn tvp_type_name_rebind_and_descriptor_changes_invalidate_prepared_definition() {
        use crate::api::odbc_types::{SQL_CA_SS_SCHEMA_NAME, SQL_CA_SS_TYPE_NAME, SQL_SS_TABLE};
        use crate::handles::stmt::PreparedPlan;
        use mssql_tds::connection::tds_client::{PreparedStatement, StatementId};

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut indicator: SqlLen = 0;
        let mut bind_name = |name: &str| {
            let mut name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLBindParameter(
                        h.stmt,
                        1,
                        SQL_PARAM_INPUT,
                        SQL_C_BINARY,
                        SQL_SS_TABLE,
                        0,
                        0,
                        name.as_mut_ptr().cast(),
                        SqlLen::from(SQL_NTS),
                        &mut indicator,
                    )
                },
                SQL_SUCCESS
            );
        };
        let install_plan = || {
            let mut state = stmt.inner.lock().unwrap();
            // A new prepare would first drain the previous unprepare.
            state.pending_unprepare = None;
            state.prepared = Some(PreparedPlan {
                stmt: PreparedStatement::materialized_for_test(
                    "SELECT @P1",
                    StatementId::from_raw_for_test(42),
                ),
                marker_count: 1,
                original_sql: String::new(),
            });
        };
        bind_name("TypeA");
        install_plan();
        bind_name("TypeA");
        assert!(
            stmt.inner
                .lock()
                .unwrap()
                .prepared
                .as_ref()
                .unwrap()
                .stmt
                .id()
                .is_some()
        );
        bind_name("TypeB");
        assert!(
            stmt.inner
                .lock()
                .unwrap()
                .prepared
                .as_ref()
                .unwrap()
                .stmt
                .id()
                .is_none()
        );
        for (field, value) in [
            (SQL_CA_SS_SCHEMA_NAME, "other"),
            (SQL_CA_SS_TYPE_NAME, "TypeC"),
        ] {
            install_plan();
            let mut value: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLSetDescFieldW(
                        h.ipd(),
                        1,
                        SqlSmallInt::try_from(field).unwrap(),
                        value.as_mut_ptr().cast(),
                        crate::api::odbc_types::SqlInteger::from(SQL_NTS),
                    )
                },
                SQL_SUCCESS
            );
            assert!(
                stmt.inner
                    .lock()
                    .unwrap()
                    .prepared
                    .as_ref()
                    .unwrap()
                    .stmt
                    .id()
                    .is_none()
            );
        }
    }

    /// A TVP cannot be a column of another TVP: binding SQL_SS_TABLE while focus
    /// is set is HY004 (msodbcsql IDS_S1_004), not a captured column.
    #[test]
    fn tvp_cannot_nest_under_focus() {
        let h = TestHandles::with_env_dbc_stmt();
        bind_and_focus_tvp(&h, 1);

        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                2,
                SQL_PARAM_INPUT,
                SQL_C_BINARY,
                crate::api::odbc_types::SQL_SS_TABLE,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            crate::api::sqlstate::SQLSTATE_HY004
        );
    }

    /// SQLFreeStmt(SQL_RESET_PARAMS) drops captured TVP columns along with the
    /// other parameter bindings.
    #[test]
    fn reset_params_clears_captured_tvp_columns() {
        let h = TestHandles::with_env_dbc_stmt();
        bind_and_focus_tvp(&h, 1);
        let (mut c0, mut i0) = (1i32, 0 as SqlLen);
        assert_eq!(bind_int_column(&h, 1, &mut c0, &mut i0), SQL_SUCCESS);

        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().tvp_column_bindings.is_empty());
    }

    /// Binding a UDT with a scalar C type is illegal, not merely unbuilt:
    /// msodbcsql's `fValidConversion` gives `SQL_UDT_MAPPED` only the three
    /// buffer C types, and its own regression (`TCLargeUDT.cpp`,
    /// `negative_variation_1`) asserts `07006`.
    ///
    /// This driver answers `HYC00` because the conversion matrix is still an
    /// implementation-progress list rather than a legality table; flipping the
    /// two apart is P9f (AB#48249). Pinned here so the divergence is visible
    /// and disappears deliberately rather than by accident.
    #[test]
    fn a_scalar_c_type_against_udt_is_rejected_at_bind() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut value: i32 = 7;
        let mut ind: SqlLen = 4;
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                crate::api::odbc_types::SQL_C_SLONG,
                SQL_SS_UDT,
                0,
                0,
                (&raw mut value).cast(),
                0,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
    }

    /// A defaulted UDT bind resolves to `SQL_C_BINARY` and is accepted: the
    /// type name it still needs is supplied through the IPD, not here.
    #[test]
    fn default_c_type_udt_is_accepted_at_bind() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut ind: SqlLen = SQL_NULL_DATA;
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
        assert_eq!(ret, SQL_SUCCESS);
        let binding = bound_params(&h);
        let bound = binding[0].as_ref().expect("parameter 1 should be bound");
        assert_eq!(bound.param.c_type, crate::api::odbc_types::SQL_C_BINARY);
    }

    /// The `ColumnSize` bound reaches the application through
    /// `SQLBindParameter`, not just through the predicate: 8000 binds, 8001 is
    /// `HY104`, and 0 binds because it is `SQL_SS_LENGTH_UNLIMITED` - the only
    /// way to send a UDT larger than the ceiling.
    ///
    /// Written with literals, not `SQL_PREC_UDT`. The 8000 edge is
    /// load-bearing rather than incidental: mssql-python's documented
    /// `setinputsizes([(SQL_SS_UDT, 8000, 0)])` sits exactly on it. Expressing
    /// the bound in terms of the constant would move the assertion with the
    /// constant, so shrinking it would silently break that path with the test
    /// still green - verified by mutation, which is how this test was first
    /// written and why it is not.
    #[test]
    fn udt_column_size_is_bounded_at_bind() {
        for (column_size, expected) in [
            (0usize, SQL_SUCCESS),
            (8000, SQL_SUCCESS),
            (8001, SQL_ERROR),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let mut ind: SqlLen = SQL_NULL_DATA;
            let ret = unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_INPUT,
                    crate::api::odbc_types::SQL_C_BINARY,
                    SQL_SS_UDT,
                    column_size as crate::api::odbc_types::SqlULen,
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut ind,
                )
            };
            assert_eq!(ret, expected, "ColumnSize {column_size}");
            if expected == SQL_ERROR {
                let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
                let state = stmt.inner.lock().unwrap();
                assert_eq!(
                    state.diag_records[0].sql_state,
                    crate::api::sqlstate::SQLSTATE_HY104,
                    "ColumnSize {column_size}"
                );
            }
        }
    }

    #[test]
    fn large_xml_binding_defers_indicator_read_and_preserves_column_size() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut token = 0u8;
        let mut ind = std::mem::MaybeUninit::<SqlLen>::uninit();
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_SS_XML,
                4001,
                0,
                (&raw mut token).cast(),
                0,
                ind.as_mut_ptr(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let binding = bound_params(&h);
        let bound = binding[0].as_ref().expect("parameter 1 should be bound");
        assert_eq!(bound.param.column_size, 4001);
    }

    #[test]
    fn large_materialized_xml_column_size_is_deferred_to_execute() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut value = b"<x/>\0".to_vec();
        let mut ind: SqlLen = SQL_NTS.into();
        let ret = unsafe {
            sql_bind_parameter(
                h.stmt,
                1,
                SQL_PARAM_INPUT,
                SQL_C_CHAR,
                SQL_SS_XML,
                4001,
                0,
                value.as_mut_ptr().cast(),
                SqlLen::try_from(value.len()).unwrap(),
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let binding = bound_params(&h);
        let bound = binding[0].as_ref().expect("parameter 1 should be bound");
        assert_eq!(bound.param.column_size, 4001);
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
        let bound = bound_params(&h)[0]
            .clone()
            .expect("parameter 1 should be bound");
        assert_eq!(bound.param.c_type, crate::api::odbc_types::SQL_C_GUID);
        assert_eq!(bound.param.sql_type, SQL_GUID);
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
                let bound = bound_params(&h)[0]
                    .clone()
                    .expect("parameter 1 should be bound");
                assert_eq!(bound.param.c_type, default_c, "sql_type {sql_type}");
            } else {
                assert_eq!(ret, SQL_ERROR, "sql_type {sql_type}");
                let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
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
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
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
        let bound = bound_params(&h)[0]
            .clone()
            .expect("parameter 1 should be bound");
        assert_eq!(
            bound.param.c_type,
            crate::api::odbc_types::SQL_C_TYPE_TIMESTAMP,
            "the deprecated spelling must be stored canonically"
        );
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
                marker_count: 1,
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
        let handle = unsafe { handle_from_raw::<DescHandle>(apd) };
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
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
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

        let explicit = unsafe { handle_from_raw::<DescHandle>(explicit_apd) };
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

    /// The narrow race `bind_param_records`'s liveness check guards against:
    /// `effective_apd` resolves an explicit descriptor under the STMT lock,
    /// which is dropped before the descriptor is actually locked and
    /// written. If a concurrent `SQLFreeHandle(SQL_HANDLE_DESC)` completes in
    /// that window, the stale pointer must fail cleanly (`Err`), not
    /// dereference freed memory. Calling the helper directly with an
    /// already-freed handle reproduces the state that window leaves behind.
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
            parameter_value_ptr: &mut buf as *mut i32 as SqlPointer,
            buffer_length: 4,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        };
        assert!(bind_param_records(explicit_apd, h.ipd(), 1, bound).is_err());
    }
}
