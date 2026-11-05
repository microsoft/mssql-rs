// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLGetDescRecW`.
//!
//! `SQLGetDescRec` answers a fixed set of record fields in one call —
//! `SQL_DESC_NAME`, `SQL_DESC_TYPE`, `SQL_DESC_DATETIME_INTERVAL_CODE`,
//! `SQL_DESC_OCTET_LENGTH`, `SQL_DESC_PRECISION`, `SQL_DESC_SCALE`, and
//! `SQL_DESC_NULLABLE` — for any descriptor kind, unlike `SQLGetDescFieldW`'s
//! arbitrary single-field lookup. Reading `DescRecord`'s fields directly
//! (rather than routing through `get_desc_field.rs`'s single-field
//! `record_field_value`) answers with the same stored values. Under TVP
//! parameter focus, the child binding is materialized as the matching APD or
//! IPD record.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlLen, SqlReturn, SqlSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_INVALID_DESCRIPTOR_INDEX, SQLSTATE_HY000, WARN_STRING_TRUNCATION, post_diag,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::desc::{DescKind, DescRecord};
use crate::handles::{DescHandle, HandleType, handle_from_raw};

/// Implementation of [`SQLGetDescRecW`](super::exports::SQLGetDescRecW).
///
/// # Safety
/// `descriptor_handle` must be null or point to a live `DescHandle`. `name`,
/// when non-null, must be writable for `buffer_length` UTF-16 code units. Every
/// other output pointer, when non-null, must be writable for one value of its
/// pointed-to type.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_get_desc_rec_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    type_ptr: *mut SqlSmallInt,
    sub_type_ptr: *mut SqlSmallInt,
    length_ptr: *mut SqlLen,
    precision_ptr: *mut SqlSmallInt,
    scale_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    crate::ffi_entry!(
        "SQLGetDescRecW",
        descriptor_handle,
        debug!(
            ?descriptor_handle,
            record_number,
            ?name,
            buffer_length,
            ?string_length_ptr,
            ?type_ptr,
            ?sub_type_ptr,
            ?length_ptr,
            ?precision_ptr,
            ?scale_ptr,
            ?nullable_ptr,
            "SQLGetDescRecW called",
        ),
        unsafe {
            sql_get_desc_rec_w_impl(
                descriptor_handle,
                record_number,
                name,
                buffer_length,
                string_length_ptr,
                type_ptr,
                sub_type_ptr,
                length_ptr,
                precision_ptr,
                scale_ptr,
                nullable_ptr,
            )
        }
    )
}

/// # Safety
/// `descriptor_handle` must be null or point to a live `DescHandle`. `name`,
/// when non-null, must be writable for `buffer_length` UTF-16 code units. Every
/// other output pointer, when non-null, must be writable for one value of its
/// pointed-to type.
#[allow(clippy::too_many_arguments)]
unsafe fn sql_get_desc_rec_w_impl(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    type_ptr: *mut SqlSmallInt,
    sub_type_ptr: *mut SqlSmallInt,
    length_ptr: *mut SqlLen,
    precision_ptr: *mut SqlSmallInt,
    scale_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if descriptor_handle.is_null() {
        error!("SQLGetDescRecW: descriptor_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let desc = unsafe { handle_from_raw::<DescHandle>(descriptor_handle) };
    debug_assert_eq!(
        desc.object_type,
        HandleType::Desc,
        "SQLGetDescRecW: handle is not a DESC"
    );

    sql_get_desc_rec_w_safe(
        desc,
        record_number,
        name,
        buffer_length,
        string_length_ptr,
        type_ptr,
        sub_type_ptr,
        length_ptr,
        precision_ptr,
        scale_ptr,
        nullable_ptr,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_get_desc_rec_w_safe(
    desc: &DescHandle,
    record_number: SqlSmallInt,
    name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    type_ptr: *mut SqlSmallInt,
    sub_type_ptr: *mut SqlSmallInt,
    length_ptr: *mut SqlLen,
    precision_ptr: *mut SqlSmallInt,
    scale_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    // BufferLength is validated by the DM (SQLSTATE HY090), same as
    // SQLDescribeColW's identical argument (describe_col.rs).
    debug_assert!(
        buffer_length >= 0,
        "SQLGetDescRecW: DM should reject negative buffer_length (HY090)"
    );

    let (focused_record, focused_record_past_count) = if record_number >= 1
        && matches!(
            desc.kind,
            DescKind::AppParam | DescKind::Ad | DescKind::ImpParam
        ) {
        match focused_tvp_record_for_desc(desc, record_number) {
            Ok(Some(FocusedTvpRecord::Record(record))) => (Some(record), false),
            Ok(Some(FocusedTvpRecord::PastCount)) => (None, true),
            Ok(None) => (None, false),
            Err(()) => {
                let Ok(mut state) = desc.inner.lock() else {
                    error!("SQLGetDescRecW: desc mutex poisoned posting focused lookup error");
                    return SQL_ERROR;
                };
                free_errors(&mut state);
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error reading focused table-valued parameter descriptor",
                );
                return SQL_ERROR;
            }
        }
    } else {
        (None, false)
    };

    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLGetDescRecW: desc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // Bookmarks (record 0) are unsupported anywhere in this driver — see
    // bind_col.rs's identical rejection of the bookmark column — so record 0
    // is out of range here too, matching SQLGetDescFieldW's own unconditional
    // `record_number < 1` gate rather than special-casing an ARD's record 0.
    if record_number < 1 {
        error!(record_number, "SQLGetDescRecW: invalid record number");
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // Past SQL_DESC_COUNT: SQL_NO_DATA per spec, not an error. A record
    // within SQL_DESC_COUNT but never populated answers its stored defaults
    // (`DescRecord::default_for`) — already the same values a fresh record
    // holds, so no separate "no data for this column" branch is needed.
    if focused_record_past_count {
        return SQL_NO_DATA;
    }

    let Some(record) = focused_record
        .as_ref()
        .or_else(|| state.record(record_number))
    else {
        debug!(record_number, "SQLGetDescRecW: record past SQL_DESC_COUNT");
        return SQL_NO_DATA;
    };

    let name_utf16: Vec<u16> = record.name.encode_utf16().collect();
    let name_len = SqlSmallInt::try_from(name_utf16.len()).unwrap_or(SqlSmallInt::MAX);
    unsafe { write_if_some(string_length_ptr, name_len) };
    let truncated = unsafe { copy_with_nul(name, buffer_length as usize, &name_utf16) };

    unsafe { write_if_some(type_ptr, record.verbose_type()) };
    unsafe { write_if_some(sub_type_ptr, record.datetime_interval_code) };
    unsafe { write_if_some(length_ptr, record.octet_length) };
    unsafe { write_if_some(precision_ptr, record.precision) };
    unsafe { write_if_some(scale_ptr, record.scale) };
    unsafe { write_if_some(nullable_ptr, record.nullable) };

    if truncated {
        post_diag(&mut state, WARN_STRING_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

pub(super) enum FocusedTvpRecord {
    PastCount,
    Record(DescRecord),
}

pub(super) fn focused_tvp_record_for_desc(
    desc: &DescHandle,
    record_number: SqlSmallInt,
) -> Result<Option<FocusedTvpRecord>, ()> {
    let Some((count, record)) = desc.focused_tvp_record_snapshot(record_number)? else {
        return Ok(None);
    };
    if usize::try_from(record_number).map_err(|_| ())? > count {
        return Ok(Some(FocusedTvpRecord::PastCount));
    }
    Ok(Some(FocusedTvpRecord::Record(record)))
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::get_desc_field::sql_get_desc_field_w;
    use crate::api::odbc_types::{
        SQL_C_DEFAULT, SQL_C_SLONG, SQL_DATETIME, SQL_DECIMAL, SQL_DESC_COUNT, SQL_INTEGER,
        SQL_NULL_HANDLE, SQL_NULLABLE, SQL_PARAM_INPUT, SQL_TYPE_TIMESTAMP,
    };
    use crate::handles::StmtHandle;
    use crate::handles::desc::{DescKind, DescRecord};
    use crate::test_support::TestHandles;

    struct GetRecResult {
        rc: SqlReturn,
        name: String,
        name_len: SqlSmallInt,
        sql_type: SqlSmallInt,
        sub_type: SqlSmallInt,
        length: SqlLen,
        precision: SqlSmallInt,
        scale: SqlSmallInt,
        nullable: SqlSmallInt,
    }

    fn get_rec(handle: SqlHandle, record_number: SqlSmallInt, buf_len: usize) -> GetRecResult {
        let mut name_buf = vec![0u16; buf_len];
        let mut name_len: SqlSmallInt = -1;
        let mut sql_type: SqlSmallInt = -1;
        let mut sub_type: SqlSmallInt = -1;
        let mut length: SqlLen = -1;
        let mut precision: SqlSmallInt = -1;
        let mut scale: SqlSmallInt = -1;
        let mut nullable: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_desc_rec_w(
                handle,
                record_number,
                if buf_len == 0 {
                    ptr::null_mut()
                } else {
                    name_buf.as_mut_ptr()
                },
                SqlSmallInt::try_from(buf_len).unwrap(),
                &mut name_len,
                &mut sql_type,
                &mut sub_type,
                &mut length,
                &mut precision,
                &mut scale,
                &mut nullable,
            )
        };
        let name = String::from_utf16_lossy(&name_buf)
            .trim_end_matches('\0')
            .to_string();
        GetRecResult {
            rc,
            name,
            name_len,
            sql_type,
            sub_type,
            length,
            precision,
            scale,
            nullable,
        }
    }

    fn set_ird_record(h: &TestHandles, record: DescRecord) {
        let desc = unsafe { handle_from_raw::<DescHandle>(h.ird()) };
        let mut state = desc.inner.lock().unwrap();
        state.set_record_count(1, DescKind::ImpRow);
        *state.record_mut(1).unwrap() = record;
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let result = get_rec(SQL_NULL_HANDLE, 1, 0);
        assert_eq!(result.rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn record_number_zero_returns_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let result = get_rec(h.ird(), 0, 0);
        assert_eq!(result.rc, SQL_ERROR);
        let desc = unsafe { handle_from_raw::<DescHandle>(h.ird()) };
        assert_eq!(
            desc.inner.lock().unwrap().diag_records[0].sql_state,
            ERR_INVALID_DESCRIPTOR_INDEX.state
        );
    }

    #[test]
    fn record_past_count_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        // A fresh IRD has SQL_DESC_COUNT == 0.
        let result = get_rec(h.ird(), 1, 0);
        assert_eq!(result.rc, SQL_NO_DATA);
    }

    #[test]
    fn focused_ipd_reads_the_sql_side_record() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        stmt.inner.lock().unwrap().tvp_column_bindings.insert(
            1,
            vec![Some(crate::params::BoundParam {
                input_output_type: SQL_PARAM_INPUT,
                c_type: SQL_C_SLONG,
                sql_type: SQL_DECIMAL,
                column_size: 10,
                decimal_digits: 2,
                app_datetime_interval_code: None,
                sql_datetime_interval_code: None,
                app_precision: 10,
                app_scale: 0,
                parameter_value_ptr: ptr::null_mut(),
                buffer_length: 0,
                strlen_or_ind_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
            })],
        );

        let result = get_rec(h.ipd(), 1, 0);
        assert_eq!(result.rc, SQL_SUCCESS);
        assert_eq!(result.sql_type, SQL_DECIMAL);
        assert_eq!(result.precision, 10);
        assert_eq!(result.scale, 2);
    }

    #[test]
    fn focused_record_snapshot_keeps_child_count_and_binding_from_one_focus() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let binding = crate::params::BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_SLONG,
            sql_type: SQL_INTEGER,
            column_size: 4,
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
            state.tvp_column_bindings.insert(1, vec![Some(binding)]);
            state.tvp_column_bindings.insert(2, Vec::new());
            state.vendor_attrs.store_param_focus(1);
        }

        let barrier = Arc::new(Barrier::new(2));
        let toggle_barrier = Arc::clone(&barrier);
        let stmt_handle = h.stmt as usize;
        let toggler = thread::spawn(move || {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_handle as SqlHandle) };
            toggle_barrier.wait();
            for index in 0..10_000 {
                stmt.inner
                    .lock()
                    .unwrap()
                    .vendor_attrs
                    .store_param_focus(if index % 2 == 0 { 2 } else { 1 });
            }
        });

        let desc = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
        let empty_record = DescRecord::default_for(DescKind::ImpParam);
        let mut bound_record = empty_record.clone();
        binding.write_to_records(
            &mut DescRecord::default_for(DescKind::AppParam),
            &mut bound_record,
        );
        barrier.wait();
        for _ in 0..10_000 {
            let (count, record) = desc
                .focused_tvp_record_snapshot(1)
                .unwrap()
                .expect("the statement owns the focused IPD");
            assert!(
                (count == 1 && record == bound_record) || (count == 0 && record == empty_record),
                "count {count} and record {record:?} must come from the same focus"
            );
        }
        toggler.join().unwrap();
    }

    #[test]
    fn shared_apd_with_conflicting_focused_shapes_returns_error() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let second_stmt = h.alloc_extra_stmt();
        let shared_apd = h.alloc_explicit_desc();
        for (stmt_handle, column_count) in [(h.stmt, 1), (second_stmt, 2)] {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_handle) };
            let mut state = stmt.inner.lock().unwrap();
            state.active_apd = Some(shared_apd);
            state.vendor_attrs.store_param_focus(1);
            state
                .tvp_column_bindings
                .insert(1, vec![None; column_count]);
        }

        let mut count: SqlSmallInt = -1;
        assert_eq!(
            unsafe {
                sql_get_desc_field_w(
                    shared_apd,
                    0,
                    SQL_DESC_COUNT as SqlSmallInt,
                    (&mut count as *mut SqlSmallInt).cast(),
                    0,
                    ptr::null_mut(),
                )
            },
            SQL_ERROR
        );
        let desc = unsafe { handle_from_raw::<DescHandle>(shared_apd) };
        assert_eq!(
            desc.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY000
        );

        assert_eq!(get_rec(shared_apd, 2, 0).rc, SQL_ERROR);
        assert_eq!(
            desc.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY000
        );
    }

    #[test]
    fn focused_get_rec_uses_child_defaults_and_count() {
        let h = TestHandles::with_env_dbc_stmt();
        let apd = unsafe { handle_from_raw::<DescHandle>(h.apd()) };
        {
            let mut state = apd.inner.lock().unwrap();
            state.set_record_count(1, DescKind::AppParam);
            state.record_mut(1).unwrap().concise_type = SQL_C_SLONG;
        }

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(1, vec![None]);
        }

        let result = get_rec(h.apd(), 1, 0);
        assert_eq!(result.rc, SQL_SUCCESS);
        assert_eq!(result.sql_type, SQL_C_DEFAULT);

        stmt.inner
            .lock()
            .unwrap()
            .tvp_column_bindings
            .insert(1, Vec::new());
        let result = get_rec(h.apd(), 1, 0);
        assert_eq!(result.rc, SQL_NO_DATA);
    }

    /// Every field lands from the same record `SQLGetDescFieldW` would read,
    /// including the verbose type: a timestamp-family column reports
    /// `SQL_DATETIME`, not the stored concise `SQL_TYPE_TIMESTAMP`.
    #[test]
    fn reads_type_length_precision_scale_nullable_and_name() {
        let h = TestHandles::with_env_dbc_stmt();
        set_ird_record(
            &h,
            DescRecord {
                concise_type: SQL_TYPE_TIMESTAMP,
                datetime_interval_code: 3,
                length: 23,
                octet_length: 16,
                precision: 3,
                scale: 3,
                nullable: SQL_NULLABLE,
                name: "created_at".to_string(),
                parameter_type: 0,
                data_ptr: ptr::null_mut(),
                indicator_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
                data_bound: false,
                explicitly_bound: false,
                udt_names: None,
                udt_name_claimed: Default::default(),
            },
        );

        let result = get_rec(h.ird(), 1, 64);
        assert_eq!(result.rc, SQL_SUCCESS);
        assert_eq!(result.sql_type, SQL_DATETIME, "verbose type must fold");
        assert_eq!(result.sub_type, 3);
        assert_eq!(result.length, 16, "LengthPtr reports SQL_DESC_OCTET_LENGTH");
        assert_eq!(result.precision, 3);
        assert_eq!(result.scale, 3);
        assert_eq!(result.nullable, SQL_NULLABLE);
        assert_eq!(result.name, "created_at");
        assert_eq!(result.name_len, 10);
    }

    /// A too-small name buffer truncates and reports 01004, matching
    /// `SQLDescribeColW`'s own truncation behavior for the same field.
    #[test]
    fn name_truncation_reports_success_with_info() {
        let h = TestHandles::with_env_dbc_stmt();
        set_ird_record(
            &h,
            DescRecord {
                concise_type: SQL_INTEGER,
                datetime_interval_code: 0,
                length: 10,
                octet_length: 4,
                precision: 10,
                scale: 0,
                nullable: SQL_NULLABLE,
                name: "a_long_column_name".to_string(),
                parameter_type: 0,
                data_ptr: ptr::null_mut(),
                indicator_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
                data_bound: false,
                explicitly_bound: false,
                udt_names: None,
                udt_name_claimed: Default::default(),
            },
        );

        let result = get_rec(h.ird(), 1, 5);
        assert_eq!(result.rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(result.name_len, 18, "reports the untruncated length");
        assert_eq!(result.name.len(), 4, "buffer holds buffer_length - 1 chars");
        let desc = unsafe { handle_from_raw::<DescHandle>(h.ird()) };
        assert_eq!(
            desc.inner.lock().unwrap().diag_records[0].sql_state,
            WARN_STRING_TRUNCATION.state
        );
    }

    /// A null `Name` buffer still reports the required length, matching
    /// `SQLDescribeColW`'s own "query the length" convention.
    #[test]
    fn null_name_buffer_still_reports_length() {
        let h = TestHandles::with_env_dbc_stmt();
        set_ird_record(
            &h,
            DescRecord {
                concise_type: SQL_C_SLONG,
                datetime_interval_code: 0,
                length: 10,
                octet_length: 4,
                precision: 0,
                scale: 0,
                nullable: SQL_NULLABLE,
                name: "col1".to_string(),
                parameter_type: 0,
                data_ptr: ptr::null_mut(),
                indicator_ptr: ptr::null_mut(),
                octet_length_ptr: ptr::null_mut(),
                data_bound: false,
                explicitly_bound: false,
                udt_names: None,
                udt_name_claimed: Default::default(),
            },
        );

        let result = get_rec(h.ird(), 1, 0);
        assert_eq!(result.rc, SQL_SUCCESS);
        assert_eq!(result.name_len, 4);
    }
}
