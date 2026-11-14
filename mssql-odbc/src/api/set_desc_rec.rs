// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLSetDescRec`.
//!
//! Sets the same eight fields `SQLSetDescFieldW` can set individually —
//! `SQL_DESC_TYPE`, `SQL_DESC_DATETIME_INTERVAL_CODE`, `SQL_DESC_OCTET_LENGTH`,
//! `SQL_DESC_PRECISION`, `SQL_DESC_SCALE`, `SQL_DESC_DATA_PTR`,
//! `SQL_DESC_OCTET_LENGTH_PTR`, and `SQL_DESC_INDICATOR_PTR` — in one call, by
//! calling directly into `set_desc_field.rs`'s own field setters
//! (`set_type`/`set_precision`/`set_scale`/`set_data_ptr`/`write_record_field`),
//! so the two APIs cannot diverge in what they accept or how they resolve a
//! type. With TVP parameter focus, the same setters validate a scratch record
//! before the child binding is updated atomically (AB#47437).
//!
//! No `W`/`A` split: none of `SQLSetDescRec`'s arguments are character data
//! (unlike `SQLGetDescRecW`'s `Name`), so the ODBC spec defines only the one
//! entry point — `sql.h` declares `SQLSetDescRec` directly, with no
//! `SQLSetDescRecW`/`SQLSetDescRecA` pair.
//!
//! Shares `SQLSetDescFieldW`'s write path, including its HY010 refusal while
//! a fetch is reading through the target ARD.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_DESC_TYPE, SQL_ERROR, SQL_INVALID_HANDLE, SQL_PARAM_INPUT, SQL_SUCCESS,
    SQL_UNKNOWN_TYPE, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlULen,
};
use crate::api::set_desc_field::{
    datetime_interval_code_for, set_data_ptr, set_precision, set_scale, set_type,
    write_record_field,
};
use crate::api::sqlstate::{
    DiagMsg, ERR_CANNOT_MODIFY_IRD, ERR_INVALID_C_DATA_TYPE, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_INVALID_PRECISION_OR_SCALE, SQLSTATE_HY000, post_diag,
};
use crate::api::type_rules::{canonical_c_type, is_valid_c_type, resolve_default_c_type};
use crate::error::{free_errors, post_sql_error};
use crate::handles::desc::{DescHeader, DescKind, DescRecord, DescState};
use crate::handles::{DbcHandle, DescHandle, HandleType, OdbcVersion, handle_from_raw};

/// Implementation of [`SQLSetDescRec`](super::exports::SQLSetDescRec).
///
/// # Safety
/// `descriptor_handle` must be null or point to a live, non-IRD `DescHandle`.
/// `data_ptr`, `string_length_ptr`, and `indicator_ptr`, when non-null, are
/// stored and must remain valid while the descriptor record is bound.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_set_desc_rec(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> SqlReturn {
    crate::ffi_entry!(
        "SQLSetDescRec",
        descriptor_handle,
        debug!(
            ?descriptor_handle,
            record_number,
            field_type,
            sub_type,
            length,
            precision,
            scale,
            ?data_ptr,
            ?string_length_ptr,
            ?indicator_ptr,
            "SQLSetDescRec called",
        ),
        unsafe {
            sql_set_desc_rec_impl(
                descriptor_handle,
                record_number,
                field_type,
                sub_type,
                length,
                precision,
                scale,
                data_ptr,
                string_length_ptr,
                indicator_ptr,
            )
        }
    )
}

/// # Safety
/// `descriptor_handle` must be null or point to a live, non-IRD `DescHandle`.
/// `data_ptr`, `string_length_ptr`, and `indicator_ptr`, when non-null, are
/// stored and must remain valid while the descriptor record is bound.
#[allow(clippy::too_many_arguments)]
unsafe fn sql_set_desc_rec_impl(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> SqlReturn {
    if descriptor_handle.is_null() {
        error!("SQLSetDescRec: descriptor_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let desc = unsafe { handle_from_raw::<DescHandle>(descriptor_handle) };
    debug_assert_eq!(
        desc.object_type,
        HandleType::Desc,
        "SQLSetDescRec: handle is not a DESC"
    );

    sql_set_desc_rec_safe(
        desc,
        record_number,
        field_type,
        sub_type,
        length,
        precision,
        scale,
        data_ptr,
        string_length_ptr,
        indicator_ptr,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_set_desc_rec_safe(
    desc: &DescHandle,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> SqlReturn {
    if desc.kind == DescKind::ImpParam && record_number >= 1 {
        let may_route = match desc.readers_through(false, true) {
            Ok((_, executing)) => !executing,
            Err(()) => {
                return post_focused_tvp_internal_error(
                    desc,
                    "Internal error checking focused descriptor statement state",
                );
            }
        };
        if may_route {
            let validation_diagnostic = std::cell::RefCell::new(None);
            let updated = desc.update_focused_tvp_bindings(
                record_number,
                Some(focused_tvp_initial_binding()),
                None,
                |binding| match focused_tvp_ipd_desc_rec(
                    binding,
                    record_number,
                    field_type,
                    sub_type,
                    length,
                    precision,
                    scale,
                    data_ptr,
                    string_length_ptr,
                    indicator_ptr,
                ) {
                    Ok(_) => true,
                    Err(diagnostic) => {
                        *validation_diagnostic.borrow_mut() = diagnostic;
                        false
                    }
                },
                |binding| {
                    if let Ok((sql_type, length, precision, scale, interval_code)) =
                        focused_tvp_ipd_desc_rec(
                            binding,
                            record_number,
                            field_type,
                            sub_type,
                            length,
                            precision,
                            scale,
                            data_ptr,
                            string_length_ptr,
                            indicator_ptr,
                        )
                    {
                        binding.sql_type = sql_type;
                        binding.column_size =
                            if crate::api::type_rules::parameter_size_is_precision(sql_type) {
                                SqlULen::try_from(precision.max(0)).unwrap_or_default()
                            } else {
                                SqlULen::try_from(length.max(0)).unwrap_or_default()
                            };
                        binding.decimal_digits = scale;
                        binding.sql_datetime_interval_code = Some(interval_code);
                    }
                },
            );
            match updated {
                Ok(Some(true)) => {
                    let Ok(mut state) = desc.inner.lock() else {
                        error!("SQLSetDescRec: desc mutex poisoned clearing diagnostics");
                        return SQL_ERROR;
                    };
                    free_errors(&mut state);
                    return SQL_SUCCESS;
                }
                Ok(Some(false)) => {
                    return match validation_diagnostic.borrow_mut().take() {
                        Some(diagnostic) => post_focused_tvp_record_diag(desc, diagnostic),
                        None => post_focused_tvp_diag(desc, ERR_INVALID_PRECISION_OR_SCALE),
                    };
                }
                Ok(None) => {}
                Err(()) => {
                    return post_focused_tvp_internal_error(
                        desc,
                        "Internal error updating focused table-valued parameter descriptor",
                    );
                }
            }
        }
    }

    if matches!(desc.kind, DescKind::AppParam | DescKind::Ad) && record_number >= 1 {
        let may_route = match desc.readers_through(false, true) {
            Ok((_, executing)) => !executing,
            Err(()) => {
                return post_focused_tvp_internal_error(
                    desc,
                    "Internal error checking focused descriptor statement state",
                );
            }
        };
        if may_route {
            let requested_c_type = canonical_c_type(field_type);
            let odbc_version = if requested_c_type == SQL_C_DEFAULT {
                current_odbc_version(desc).ok()
            } else {
                Some(OdbcVersion::Unset)
            };
            let validation_diagnostic = std::cell::Cell::new(None);
            let internal_error = std::cell::Cell::new(false);
            let focused_type = std::cell::Cell::new(None);
            let initial_binding = focused_tvp_initial_binding();
            let updated = desc.update_focused_tvp_bindings(
                record_number,
                Some(initial_binding),
                None,
                |binding| {
                    if !is_valid_c_type(requested_c_type) {
                        validation_diagnostic.set(Some(ERR_INVALID_C_DATA_TYPE));
                        return false;
                    }
                    if requested_c_type == SQL_C_DEFAULT && odbc_version.is_none() {
                        internal_error.set(true);
                        return false;
                    }
                    if let Some(resolved_type) = focused_tvp_desc_rec_type(
                        binding,
                        desc.kind,
                        record_number,
                        field_type,
                        sub_type,
                        length,
                        precision,
                        scale,
                        data_ptr,
                        string_length_ptr,
                        indicator_ptr,
                    ) {
                        focused_type.set(Some(resolved_type));
                        true
                    } else {
                        validation_diagnostic.set(Some(ERR_INVALID_PRECISION_OR_SCALE));
                        false
                    }
                },
                |binding| {
                    let (requested_type, datetime_interval_code) =
                        focused_type.get().unwrap_or((requested_c_type, 0));
                    let resolved_type = if requested_c_type == SQL_C_DEFAULT {
                        resolve_default_c_type(
                            binding.sql_type,
                            odbc_version.unwrap_or(OdbcVersion::Unset),
                        )
                        .unwrap_or(SQL_C_DEFAULT)
                    } else {
                        requested_type
                    };
                    binding.c_type = resolved_type;
                    binding.app_datetime_interval_code =
                        Some(if requested_c_type == SQL_C_DEFAULT {
                            datetime_interval_code_for(resolved_type)
                        } else {
                            datetime_interval_code
                        });
                    binding.buffer_length = length;
                    binding.app_precision = precision;
                    binding.app_scale = scale;
                    binding.parameter_value_ptr = data_ptr;
                    binding.octet_length_ptr = string_length_ptr;
                    binding.strlen_or_ind_ptr = indicator_ptr;
                },
            );
            match updated {
                Ok(Some(true)) => {
                    let Ok(mut state) = desc.inner.lock() else {
                        error!("SQLSetDescRec: desc mutex poisoned clearing diagnostics");
                        return SQL_ERROR;
                    };
                    free_errors(&mut state);
                    return SQL_SUCCESS;
                }
                Ok(Some(false)) if internal_error.get() => {
                    return post_focused_tvp_internal_error(
                        desc,
                        "Internal error resolving focused TVP C type",
                    );
                }
                Ok(Some(false)) => {
                    return post_focused_tvp_diag(
                        desc,
                        validation_diagnostic
                            .get()
                            .unwrap_or(ERR_INVALID_PRECISION_OR_SCALE),
                    );
                }
                Ok(None) => {}
                Err(()) => {
                    return post_focused_tvp_internal_error(
                        desc,
                        "Internal error updating focused table-valued parameter descriptor",
                    );
                }
            }
        }
    }

    desc.update_definition(record_number, "SQLSetDescRec", |state| {
        set_desc_rec(
            state,
            desc.kind,
            record_number,
            field_type,
            sub_type,
            length,
            precision,
            scale,
            data_ptr,
            string_length_ptr,
            indicator_ptr,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn focused_tvp_ipd_desc_rec(
    binding: &crate::params::BoundParam,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> Result<
    (SqlSmallInt, SqlLen, SqlSmallInt, SqlSmallInt, SqlSmallInt),
    Option<crate::error::DiagRecord>,
> {
    let record_index = usize::try_from(record_number)
        .ok()
        .and_then(|number| number.checked_sub(1))
        .ok_or(None)?;
    let mut apd_record = DescRecord::default_for(DescKind::AppParam);
    let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
    binding.write_to_records(&mut apd_record, &mut ipd_record);
    let mut records = vec![DescRecord::default_for(DescKind::ImpParam); record_index + 1];
    records[record_index] = ipd_record;
    let mut state = DescState {
        diag_records: Default::default(),
        header: DescHeader::default(),
        records,
        data_classification: None,
    };
    if set_desc_rec(
        &mut state,
        DescKind::ImpParam,
        record_number,
        field_type,
        sub_type,
        length,
        precision,
        scale,
        data_ptr,
        string_length_ptr,
        indicator_ptr,
    ) != SQL_SUCCESS
    {
        return Err(state.diag_records.pop());
    }
    let record = state.record(record_number).ok_or(None)?;
    let length = SqlLen::try_from(record.length).map_err(|_| None)?;
    Ok((
        record.concise_type,
        length,
        record.precision,
        record.scale,
        record.datetime_interval_code,
    ))
}

fn post_focused_tvp_record_diag(
    desc: &DescHandle,
    diagnostic: crate::error::DiagRecord,
) -> SqlReturn {
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLSetDescRec: desc mutex poisoned posting focused validation error");
        return SQL_ERROR;
    };
    free_errors(&mut state);
    state.diag_records.push(diagnostic);
    SQL_ERROR
}

pub(super) fn current_odbc_version(desc: &DescHandle) -> Result<OdbcVersion, ()> {
    let dbc = unsafe { handle_from_raw::<DbcHandle>(desc.parent_dbc) };
    let Ok(state) = dbc.parent_env().inner.lock() else {
        error!("descriptor update: env mutex poisoned reading ODBC version");
        return Err(());
    };
    Ok(state.odbc_version)
}

pub(super) fn focused_tvp_initial_binding() -> crate::params::BoundParam {
    crate::params::BoundParam {
        input_output_type: SQL_PARAM_INPUT,
        c_type: SQL_C_DEFAULT,
        sql_type: SQL_UNKNOWN_TYPE,
        column_size: 0,
        decimal_digits: 0,
        app_datetime_interval_code: None,
        sql_datetime_interval_code: None,
        app_precision: 0,
        app_scale: 0,
        parameter_value_ptr: std::ptr::null_mut(),
        buffer_length: 0,
        strlen_or_ind_ptr: std::ptr::null_mut(),
        octet_length_ptr: std::ptr::null_mut(),
    }
}

#[allow(clippy::too_many_arguments)]
fn focused_tvp_desc_rec_type(
    binding: &crate::params::BoundParam,
    kind: DescKind,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> Option<(SqlSmallInt, SqlSmallInt)> {
    let record_index = usize::try_from(record_number)
        .ok()
        .and_then(|number| number.checked_sub(1))?;
    let mut apd_record = DescRecord::default_for(kind);
    let mut ipd_record = DescRecord::default_for(DescKind::ImpParam);
    binding.write_to_records(&mut apd_record, &mut ipd_record);
    apd_record.precision = binding.app_precision;
    apd_record.scale = binding.app_scale;

    let mut records = vec![DescRecord::default_for(kind); record_index + 1];
    records[record_index] = apd_record;
    let mut state = DescState {
        diag_records: Default::default(),
        header: DescHeader::default(),
        records,
        data_classification: None,
    };
    if set_desc_rec(
        &mut state,
        kind,
        record_number,
        field_type,
        sub_type,
        length,
        precision,
        scale,
        data_ptr,
        string_length_ptr,
        indicator_ptr,
    ) != SQL_SUCCESS
    {
        return None;
    }
    state
        .records
        .get(record_index)
        .map(|record| (record.concise_type, record.datetime_interval_code))
}

fn post_focused_tvp_diag(desc: &DescHandle, diagnostic: DiagMsg) -> SqlReturn {
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLSetDescRec: desc mutex poisoned posting focused TVP diagnostic");
        return SQL_ERROR;
    };
    free_errors(&mut state);
    post_diag(&mut state, diagnostic);
    SQL_ERROR
}

fn post_focused_tvp_internal_error(desc: &DescHandle, message: &'static str) -> SqlReturn {
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLSetDescRec: desc mutex poisoned posting internal error");
        return SQL_ERROR;
    };
    free_errors(&mut state);
    post_sql_error(&mut state, SQLSTATE_HY000, 0, message);
    SQL_ERROR
}

#[allow(clippy::too_many_arguments)]
fn set_desc_rec(
    state: &mut DescState,
    kind: DescKind,
    record_number: SqlSmallInt,
    field_type: SqlSmallInt,
    sub_type: SqlSmallInt,
    length: SqlLen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    data_ptr: SqlPointer,
    string_length_ptr: *mut SqlLen,
    indicator_ptr: *mut SqlLen,
) -> SqlReturn {
    // "must not be an IRD handle" (spec) — mirrors SQLSetDescFieldW's own
    // blanket IRD gate (set_desc_field.rs).
    if kind == DescKind::ImpRow {
        error!("SQLSetDescRec: cannot modify an implementation row descriptor");
        post_diag(state, ERR_CANNOT_MODIFY_IRD);
        return SQL_ERROR;
    }

    // Bookmarks (record 0) are unsupported anywhere in this driver — see
    // bind_col.rs's identical rejection of the bookmark column, and
    // SQLGetDescRecW's matching gate — so record 0 is out of range here too.
    if record_number < 1 {
        error!(record_number, "SQLSetDescRec: invalid record number");
        post_diag(state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }
    let Ok(count) = usize::try_from(record_number) else {
        post_diag(state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    };
    // Growing on demand, same as SQLSetDescFieldW's per-record-field growth:
    // msodbcsql calls AllocPlex before the field-specific setter for any
    // record write, which can grow SQL_DESC_COUNT even if a setter further
    // down this sequence later fails.
    if count > state.records.len() {
        state.set_record_count(count, kind);
    }

    // Order matters, and matches the field list order the ODBC spec itself
    // gives for SQLSetDescRec:
    // - SubType first: a raw field with no validation of its own, and
    //   `set_type` resolves a verbose `Type = SQL_DATETIME` from whatever
    //   `SQL_DESC_DATETIME_INTERVAL_CODE` is *already stored* on the record
    //   (see `set_type`'s own doc comment) — so it must land before Type.
    // - DataPtr last: `set_data_ptr` reruns the SQL_C_NUMERIC precision/scale
    //   consistency check, which needs Precision/Scale already written.
    let sub_type_write = write_record_field(state, record_number, |r| {
        r.datetime_interval_code = sub_type;
        r.explicitly_bound = true;
    });
    if sub_type_write != SQL_SUCCESS {
        return sub_type_write;
    }
    let type_write = set_type(
        state,
        kind,
        record_number,
        SQL_DESC_TYPE,
        field_type as SqlPointer,
    );
    if type_write != SQL_SUCCESS {
        return type_write;
    }
    let length_write = write_record_field(state, record_number, |r| {
        r.octet_length = length;
        r.explicitly_bound = true;
    });
    if length_write != SQL_SUCCESS {
        return length_write;
    }
    let precision_write = set_precision(state, record_number, precision as SqlPointer);
    if precision_write != SQL_SUCCESS {
        return precision_write;
    }
    let scale_write = set_scale(state, kind, record_number, scale as SqlPointer);
    if scale_write != SQL_SUCCESS {
        return scale_write;
    }
    let string_length_write = write_record_field(state, record_number, |r| {
        r.octet_length_ptr = string_length_ptr as SqlPointer
    });
    if string_length_write != SQL_SUCCESS {
        return string_length_write;
    }
    let indicator_write = write_record_field(state, record_number, |r| {
        r.indicator_ptr = indicator_ptr as SqlPointer
    });
    if indicator_write != SQL_SUCCESS {
        return indicator_write;
    }
    let data_ptr_write = set_data_ptr(state, record_number, data_ptr);
    if data_ptr_write != SQL_SUCCESS {
        return data_ptr_write;
    }

    debug!(record_number, "SQLSetDescRec: record updated");
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::get_desc_field::sql_get_desc_field_w;
    use crate::api::get_desc_rec::sql_get_desc_rec_w;
    use crate::api::odbc_types::{
        SQL_C_NUMERIC, SQL_C_TYPE_TIMESTAMP, SQL_CODE_DATE, SQL_CODE_TIMESTAMP, SQL_DATETIME,
        SQL_DECIMAL, SQL_DESC_CONCISE_TYPE, SQL_DESC_DATA_PTR, SQL_DESC_DATETIME_INTERVAL_CODE,
        SQL_DESC_INDICATOR_PTR, SQL_DESC_OCTET_LENGTH, SQL_DESC_OCTET_LENGTH_PTR,
        SQL_DESC_PRECISION, SQL_DESC_SCALE, SQL_DESC_TYPE, SQL_INTEGER, SQL_NO_NULLS,
        SQL_NULL_HANDLE, SQL_NUMERIC, SQL_PARAM_INPUT, SQL_TYPE_TIMESTAMP, SqlPointer,
        SqlTimestampStruct,
    };
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::api::sqlstate::{
        ERR_CANNOT_MODIFY_IRD, ERR_INVALID_PRECISION_OR_SCALE, ERR_INVALID_SQL_DATA_TYPE,
    };
    use crate::api::type_rules::canonical_c_type;
    use crate::handles::desc::DescRecord;
    use crate::test_support::TestHandles;

    fn desc_diag_states(handle: SqlHandle) -> Vec<[u8; 5]> {
        let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
        desc.inner
            .lock()
            .unwrap()
            .diag_records
            .iter()
            .map(|d| d.sql_state)
            .collect()
    }

    fn record_count(handle: SqlHandle) -> usize {
        let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
        desc.inner.lock().unwrap().records.len()
    }

    fn cloned_record(handle: SqlHandle, index: usize) -> DescRecord {
        let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
        desc.inner.lock().unwrap().records[index].clone()
    }

    fn get_small_int(handle: SqlHandle, record: SqlSmallInt, field: SqlSmallInt) -> SqlSmallInt {
        let mut value: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                handle,
                record,
                field,
                &mut value as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "GET failed reading back field {field}");
        value
    }

    fn get_len(handle: SqlHandle, record: SqlSmallInt, field: SqlSmallInt) -> SqlLen {
        let mut value: SqlLen = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                handle,
                record,
                field,
                &mut value as *mut SqlLen as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "GET failed reading back field {field}");
        value
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe {
            sql_set_desc_rec(
                SQL_NULL_HANDLE,
                1,
                SQL_INTEGER,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn cannot_modify_ird_returns_hy016() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_rec(
                h.ird(),
                1,
                SQL_INTEGER,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(desc_diag_states(h.ird())[0], ERR_CANNOT_MODIFY_IRD.state);
    }

    #[test]
    fn record_number_zero_returns_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_rec(
                h.ard(),
                0,
                SQL_INTEGER,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(
            desc_diag_states(h.ard())[0],
            ERR_INVALID_DESCRIPTOR_INDEX.state
        );
    }

    #[test]
    fn grows_record_count_when_needed() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(record_count(h.ard()), 0);
        let ret = unsafe {
            sql_set_desc_rec(
                h.ard(),
                3,
                canonical_c_type_i32(),
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(record_count(h.ard()), 3);
    }

    fn canonical_c_type_i32() -> SqlSmallInt {
        canonical_c_type(crate::api::odbc_types::SQL_C_SLONG)
    }

    /// Every one of the eight documented fields lands, and reading each back
    /// through `SQLGetDescFieldW` — a completely independent code path —
    /// confirms this is genuinely stored on the record, not just accepted.
    #[test]
    fn writes_all_eight_fields() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = 0i32;
        let mut ind: SqlLen = 0;
        let mut str_len: SqlLen = 0;
        let ret = unsafe {
            sql_set_desc_rec(
                h.ard(),
                1,
                canonical_c_type_i32(),
                0,
                4,
                0,
                0,
                &mut buf as *mut i32 as SqlPointer,
                &mut str_len,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        assert_eq!(
            get_small_int(h.ard(), 1, SQL_DESC_CONCISE_TYPE as SqlSmallInt),
            canonical_c_type_i32()
        );
        assert_eq!(get_len(h.ard(), 1, SQL_DESC_OCTET_LENGTH as SqlSmallInt), 4);
        let mut data_ptr: SqlPointer = ptr::null_mut();
        unsafe {
            sql_get_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_DATA_PTR as SqlSmallInt,
                &mut data_ptr as *mut SqlPointer as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(data_ptr, &mut buf as *mut i32 as SqlPointer);
        let mut octet_length_ptr: SqlPointer = ptr::null_mut();
        unsafe {
            sql_get_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_OCTET_LENGTH_PTR as SqlSmallInt,
                &mut octet_length_ptr as *mut SqlPointer as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(octet_length_ptr, &mut str_len as *mut SqlLen as SqlPointer);
        let mut indicator_ptr: SqlPointer = ptr::null_mut();
        unsafe {
            sql_get_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_INDICATOR_PTR as SqlSmallInt,
                &mut indicator_ptr as *mut SqlPointer as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(indicator_ptr, &mut ind as *mut SqlLen as SqlPointer);
    }

    /// `SQL_DESC_TYPE = SQL_DATETIME` resolves its concise form from the
    /// `SubType` this same call carries — SubType must therefore land before
    /// Type is resolved, exactly as the ODBC spec's field list orders them.
    #[test]
    fn datetime_subtype_resolves_the_concise_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_rec(
                h.ipd(),
                1,
                SQL_DATETIME,
                SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap(),
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            get_small_int(h.ipd(), 1, SQL_DESC_CONCISE_TYPE as SqlSmallInt),
            SQL_TYPE_TIMESTAMP
        );
    }

    #[test]
    fn focused_datetime_subtype_is_committed_to_the_child_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        let mut original = 7i32;
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(
                1,
                vec![Some(crate::params::BoundParam {
                    input_output_type: SQL_PARAM_INPUT,
                    c_type: SQL_C_NUMERIC,
                    sql_type: SQL_NUMERIC,
                    column_size: 10,
                    decimal_digits: 2,
                    app_datetime_interval_code: None,
                    sql_datetime_interval_code: None,
                    app_precision: 10,
                    app_scale: 2,
                    parameter_value_ptr: (&mut original as *mut i32).cast(),
                    buffer_length: SqlLen::try_from(std::mem::size_of::<i32>()).unwrap(),
                    strlen_or_ind_ptr: ptr::null_mut(),
                    octet_length_ptr: ptr::null_mut(),
                })],
            );
        }

        let mut value = SqlTimestampStruct {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            fraction: 6,
        };
        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.apd(),
                    1,
                    SQL_DATETIME,
                    SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap(),
                    SqlLen::try_from(std::mem::size_of::<SqlTimestampStruct>()).unwrap(),
                    0,
                    9,
                    (&raw mut value).cast(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(
            get_small_int(
                h.apd(),
                1,
                SqlSmallInt::try_from(SQL_DESC_CONCISE_TYPE).unwrap()
            ),
            SQL_C_TYPE_TIMESTAMP
        );
        assert_eq!(
            get_small_int(h.apd(), 1, SqlSmallInt::try_from(SQL_DESC_TYPE).unwrap()),
            SQL_DATETIME
        );
        assert_eq!(
            get_small_int(
                h.apd(),
                1,
                SqlSmallInt::try_from(SQL_DESC_DATETIME_INTERVAL_CODE).unwrap()
            ),
            SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap()
        );
        let mut record_type = -1;
        let mut record_sub_type = -1;
        assert_eq!(
            unsafe {
                sql_get_desc_rec_w(
                    h.apd(),
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut record_type,
                    &mut record_sub_type,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_type, SQL_DATETIME);
        assert_eq!(
            record_sub_type,
            SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap()
        );
        let state = stmt.inner.lock().unwrap();
        let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
        assert_eq!(binding.c_type, SQL_C_TYPE_TIMESTAMP);
        assert_eq!(
            binding.parameter_value_ptr,
            (&raw mut value).cast::<std::ffi::c_void>()
        );
    }

    #[test]
    fn focused_default_timestamp_derives_its_datetime_subtype() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(
                1,
                vec![Some(crate::params::BoundParam {
                    input_output_type: SQL_PARAM_INPUT,
                    c_type: SQL_C_DEFAULT,
                    sql_type: SQL_TYPE_TIMESTAMP,
                    column_size: 23,
                    decimal_digits: 7,
                    app_datetime_interval_code: None,
                    sql_datetime_interval_code: None,
                    app_precision: 0,
                    app_scale: 0,
                    parameter_value_ptr: ptr::null_mut(),
                    buffer_length: 0,
                    strlen_or_ind_ptr: ptr::null_mut(),
                    octet_length_ptr: ptr::null_mut(),
                })],
            );
        }

        let mut value = SqlTimestampStruct {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            fraction: 6,
        };
        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.apd(),
                    1,
                    SQL_C_DEFAULT,
                    0,
                    SqlLen::try_from(std::mem::size_of::<SqlTimestampStruct>()).unwrap(),
                    0,
                    7,
                    (&raw mut value).cast(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );

        let mut record_type = -1;
        let mut record_sub_type = -1;
        assert_eq!(
            unsafe {
                sql_get_desc_rec_w(
                    h.apd(),
                    1,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut record_type,
                    &mut record_sub_type,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_type, SQL_DATETIME);
        assert_eq!(
            record_sub_type,
            SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap()
        );
        let state = stmt.inner.lock().unwrap();
        let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
        assert_eq!(binding.c_type, SQL_C_TYPE_TIMESTAMP);
        assert_eq!(
            binding.app_datetime_interval_code,
            Some(SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap())
        );
    }

    #[test]
    fn focused_set_desc_rec_initializes_an_unbound_child_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        let mut value = 42i32;
        let value_ptr = (&mut value as *mut i32).cast();

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.apd(),
                    1,
                    crate::api::odbc_types::SQL_C_LONG,
                    0,
                    SqlLen::try_from(std::mem::size_of::<i32>()).unwrap(),
                    0,
                    0,
                    value_ptr,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_count(h.apd()), 0);

        let state = stmt.inner.lock().unwrap();
        let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
        assert_eq!(binding.c_type, crate::api::odbc_types::SQL_C_LONG);
        assert_eq!(binding.parameter_value_ptr, value_ptr);
        assert_eq!(
            binding.buffer_length,
            SqlLen::try_from(std::mem::size_of::<i32>()).unwrap()
        );
    }

    /// `SQLBindParameter`'s own numeric-consistency gate (AB#47297) applies
    /// here too: `set_data_ptr` reruns it, so an out-of-range `SQL_C_NUMERIC`
    /// precision must fail even though Precision/Scale were "set" earlier in
    /// this same call.
    #[test]
    fn numeric_precision_scale_consistency_check_reruns_at_data_ptr() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = 0u8;
        let ret = unsafe {
            sql_set_desc_rec(
                h.apd(),
                1,
                SQL_C_NUMERIC,
                0,
                0,
                0, // invalid: SQL_C_NUMERIC precision must be 1..=38
                0,
                &mut buf as *mut u8 as SqlPointer,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    /// `SQLSetDescRec` and an equivalent sequence of `SQLSetDescFieldW`
    /// calls must produce byte-identical records: they are two entry points
    /// into the exact same field setters (AB#47437's "cannot produce
    /// contradictory binding state" requirement, extended from bind APIs to
    /// the bulk-vs-single-field descriptor APIs).
    #[test]
    fn equivalent_to_the_same_sequence_of_set_desc_field_calls() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut buf = 0i32;
        let mut ind: SqlLen = 0;

        let via_rec = TestHandles::with_env_dbc_stmt();
        unsafe {
            sql_set_desc_rec(
                via_rec.ard(),
                1,
                canonical_c_type_i32(),
                0,
                4,
                0,
                0,
                &mut buf as *mut i32 as SqlPointer,
                ptr::null_mut(),
                &mut ind,
            )
        };

        unsafe {
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_DATETIME_INTERVAL_CODE as SqlSmallInt,
                0 as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_CONCISE_TYPE as SqlSmallInt,
                canonical_c_type_i32() as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_OCTET_LENGTH as SqlSmallInt,
                4 as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_PRECISION as SqlSmallInt,
                0 as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_SCALE as SqlSmallInt,
                0 as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_OCTET_LENGTH_PTR as SqlSmallInt,
                ptr::null_mut(),
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_INDICATOR_PTR as SqlSmallInt,
                &mut ind as *mut SqlLen as SqlPointer,
                0,
            );
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_DATA_PTR as SqlSmallInt,
                &mut buf as *mut i32 as SqlPointer,
                0,
            );
        }

        let rec_record = cloned_record(via_rec.ard(), 0);
        let field_record = cloned_record(h.ard(), 0);
        assert_eq!(rec_record.concise_type, field_record.concise_type);
        assert_eq!(rec_record.octet_length, field_record.octet_length);
        assert_eq!(rec_record.precision, field_record.precision);
        assert_eq!(rec_record.scale, field_record.scale);
        assert_eq!(rec_record.data_ptr, field_record.data_ptr);
        assert_eq!(rec_record.indicator_ptr, field_record.indicator_ptr);
        assert_eq!(rec_record.octet_length_ptr, field_record.octet_length_ptr);
    }

    #[test]
    fn focused_tvp_record_apis_read_and_write_the_child_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        let mut old_value = 0u8;
        let mut old_length: SqlLen = 0;
        let mut old_indicator: SqlLen = 0;
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(
                1,
                vec![Some(crate::params::BoundParam {
                    input_output_type: SQL_PARAM_INPUT,
                    c_type: SQL_C_NUMERIC,
                    sql_type: SQL_NUMERIC,
                    column_size: 10,
                    decimal_digits: 2,
                    app_datetime_interval_code: None,
                    sql_datetime_interval_code: None,
                    app_precision: 10,
                    app_scale: 2,
                    parameter_value_ptr: (&mut old_value as *mut u8).cast(),
                    buffer_length: 16,
                    strlen_or_ind_ptr: &mut old_indicator,
                    octet_length_ptr: &mut old_length,
                })],
            );
        }

        let mut value = 0u8;
        let mut length: SqlLen = 0;
        let mut indicator: SqlLen = 0;
        let new_value_ptr: SqlPointer = (&mut value as *mut u8).cast();
        let ret = unsafe {
            sql_set_desc_rec(
                h.apd(),
                1,
                SQL_C_NUMERIC,
                0,
                24,
                12,
                3,
                new_value_ptr,
                &mut length,
                &mut indicator,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(record_count(h.apd()), 0, "the outer APD stays untouched");
        assert_eq!(
            get_len(h.apd(), 1, SQL_DESC_OCTET_LENGTH as SqlSmallInt),
            24
        );

        let ret = unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_OCTET_LENGTH as SqlSmallInt,
                30 as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        {
            let state = stmt.inner.lock().unwrap();
            let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
            assert_eq!(binding.c_type, SQL_C_NUMERIC);
            assert_eq!(binding.buffer_length, 30);
            assert_eq!(binding.app_precision, 12);
            assert_eq!(binding.app_scale, 3);
            assert_eq!(binding.parameter_value_ptr, new_value_ptr);
            assert_eq!(binding.octet_length_ptr, &mut length as *mut SqlLen);
            assert_eq!(binding.strlen_or_ind_ptr, &mut indicator as *mut SqlLen);
        }
        assert_eq!(
            get_len(h.apd(), 1, SQL_DESC_OCTET_LENGTH as SqlSmallInt),
            30
        );

        let ret = unsafe {
            sql_set_desc_rec(
                h.apd(),
                1,
                SQL_C_NUMERIC,
                0,
                99,
                12,
                13,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(
            desc_diag_states(h.apd())[0],
            ERR_INVALID_PRECISION_OR_SCALE.state
        );
        {
            let state = stmt.inner.lock().unwrap();
            let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
            assert_eq!(binding.buffer_length, 30);
            assert_eq!(binding.app_precision, 12);
            assert_eq!(binding.app_scale, 3);
            assert_eq!(binding.parameter_value_ptr, new_value_ptr);
        }

        let mut name_length: SqlSmallInt = -1;
        let mut c_type: SqlSmallInt = -1;
        let mut sub_type: SqlSmallInt = -1;
        let mut buffer_length: SqlLen = -1;
        let mut precision: SqlSmallInt = -1;
        let mut scale: SqlSmallInt = -1;
        let mut nullable: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_rec_w(
                h.apd(),
                1,
                ptr::null_mut(),
                0,
                &mut name_length,
                &mut c_type,
                &mut sub_type,
                &mut buffer_length,
                &mut precision,
                &mut scale,
                &mut nullable,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(name_length, 0);
        assert_eq!(c_type, SQL_C_NUMERIC);
        assert_eq!(sub_type, 0);
        assert_eq!(buffer_length, 30);
        assert_eq!(precision, 12);
        assert_eq!(scale, 3);
        assert_eq!(nullable, SQL_NO_NULLS);
    }

    #[test]
    fn focused_ipd_set_desc_rec_updates_the_child_sql_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(
                1,
                vec![Some(crate::params::BoundParam {
                    input_output_type: SQL_PARAM_INPUT,
                    c_type: SQL_C_NUMERIC,
                    sql_type: SQL_DECIMAL,
                    column_size: 10,
                    decimal_digits: 2,
                    app_datetime_interval_code: None,
                    sql_datetime_interval_code: None,
                    app_precision: 10,
                    app_scale: 2,
                    parameter_value_ptr: ptr::null_mut(),
                    buffer_length: 0,
                    strlen_or_ind_ptr: ptr::null_mut(),
                    octet_length_ptr: ptr::null_mut(),
                })],
            );
        }

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.ipd(),
                    1,
                    SQL_NUMERIC,
                    0,
                    12,
                    12,
                    3,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_count(h.ipd()), 0, "the outer IPD stays untouched");
        let state = stmt.inner.lock().unwrap();
        let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
        assert_eq!(binding.sql_type, SQL_NUMERIC);
        assert_eq!(binding.column_size, 12);
        assert_eq!(binding.decimal_digits, 3);
    }

    #[test]
    fn focused_ipd_set_desc_rec_preserves_invalid_sql_type_diagnostic() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        stmt.inner.lock().unwrap().tvp_column_bindings.insert(
            1,
            vec![Some(crate::params::BoundParam {
                input_output_type: SQL_PARAM_INPUT,
                c_type: SQL_C_NUMERIC,
                sql_type: SQL_INTEGER,
                column_size: 4,
                decimal_digits: 0,
                app_datetime_interval_code: None,
                sql_datetime_interval_code: None,
                app_precision: 10,
                app_scale: 2,
                parameter_value_ptr: ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
            })],
        );

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.ipd(),
                    1,
                    9999,
                    0,
                    8,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_ERROR
        );
        assert_eq!(
            desc_diag_states(h.ipd())[0],
            ERR_INVALID_SQL_DATA_TYPE.state
        );
        assert_eq!(record_count(h.ipd()), 0, "the outer IPD stays untouched");
        let state = stmt.inner.lock().unwrap();
        let binding = state.tvp_column_bindings[&1][0].as_ref().unwrap();
        assert_eq!(binding.sql_type, SQL_INTEGER);
        assert_eq!(binding.column_size, 4);
        assert_eq!(binding.decimal_digits, 0);
    }

    #[test]
    fn focused_ipd_set_desc_rec_initializes_an_unbound_child_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.ipd(),
                    1,
                    crate::api::odbc_types::SQL_INTEGER,
                    0,
                    4,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_count(h.ipd()), 0, "the outer IPD stays untouched");
        let binding = stmt.inner.lock().unwrap().tvp_column_bindings[&1][0].unwrap();
        assert_eq!(binding.sql_type, crate::api::odbc_types::SQL_INTEGER);
    }

    #[test]
    fn focused_ipd_set_desc_rec_preserves_the_requested_child_metadata() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        let varchar = crate::params::BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: crate::api::odbc_types::SQL_C_CHAR,
            sql_type: crate::api::odbc_types::SQL_VARCHAR,
            column_size: 50,
            decimal_digits: 0,
            app_datetime_interval_code: None,
            sql_datetime_interval_code: None,
            app_precision: 0,
            app_scale: 0,
            parameter_value_ptr: ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: ptr::null_mut(),
            octet_length_ptr: ptr::null_mut(),
        };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state
                .tvp_column_bindings
                .insert(1, vec![None, Some(varchar)]);
        }

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.ipd(),
                    2,
                    crate::api::odbc_types::SQL_VARCHAR,
                    0,
                    200,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(record_count(h.ipd()), 0, "the outer IPD stays untouched");
        let binding = stmt.inner.lock().unwrap().tvp_column_bindings[&1][1].unwrap();
        assert_eq!(binding.sql_type, crate::api::odbc_types::SQL_VARCHAR);
        assert_eq!(binding.column_size, 50);
    }

    #[test]
    fn focused_ipd_set_desc_rec_preserves_the_child_datetime_subtype() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        stmt.inner.lock().unwrap().tvp_column_bindings.insert(
            1,
            vec![Some(crate::params::BoundParam {
                input_output_type: SQL_PARAM_INPUT,
                c_type: SQL_C_TYPE_TIMESTAMP,
                sql_type: SQL_TYPE_TIMESTAMP,
                column_size: 27,
                decimal_digits: 7,
                app_datetime_interval_code: None,
                sql_datetime_interval_code: None,
                app_precision: 0,
                app_scale: 0,
                parameter_value_ptr: ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
            })],
        );

        assert_eq!(
            unsafe {
                sql_set_desc_rec(
                    h.ipd(),
                    1,
                    SQL_DATETIME,
                    SqlSmallInt::try_from(SQL_CODE_DATE).unwrap(),
                    0,
                    0,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        let binding = stmt.inner.lock().unwrap().tvp_column_bindings[&1][0].unwrap();
        assert_eq!(binding.sql_type, crate::api::odbc_types::SQL_TYPE_DATE);
        assert_eq!(
            binding.sql_datetime_interval_code,
            Some(SqlSmallInt::try_from(SQL_CODE_DATE).unwrap())
        );
    }
}
