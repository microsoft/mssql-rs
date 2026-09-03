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
//! type: this is the same descriptor-record storage either way (AB#47437).
//!
//! No `W`/`A` split: none of `SQLSetDescRec`'s arguments are character data
//! (unlike `SQLGetDescRecW`'s `Name`), so the ODBC spec defines only the one
//! entry point — `sql.h` declares `SQLSetDescRec` directly, with no
//! `SQLSetDescRecW`/`SQLSetDescRecA` pair.
//!
//! Shares `SQLSetDescFieldW`'s known, deferred gap: no
//! `STMT_STATE_FETCH_IN_PROGRESS` check before writing an ARD/APD record a
//! fetch may still be reading through — see
//! [#472](https://github.com/microsoft/mssql-rs/issues/472).

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DESC_TYPE, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlLen, SqlPointer,
    SqlReturn, SqlSmallInt,
};
use crate::api::set_desc_field::{
    set_data_ptr, set_precision, set_scale, set_type, write_record_field,
};
use crate::api::sqlstate::{ERR_CANNOT_MODIFY_IRD, ERR_INVALID_DESCRIPTOR_INDEX, post_diag};
use crate::error::free_errors;
use crate::handles::desc::DescKind;
use crate::handles::{DescHandle, HandleType, handle_from_raw};

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
    );
    crate::ffi_entry!("SQLSetDescRec", unsafe {
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
    })
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
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLSetDescRec: desc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // "must not be an IRD handle" (spec) — mirrors SQLSetDescFieldW's own
    // blanket IRD gate (set_desc_field.rs).
    if desc.kind == DescKind::ImpRow {
        error!("SQLSetDescRec: cannot modify an implementation row descriptor");
        post_diag(&mut state, ERR_CANNOT_MODIFY_IRD);
        return SQL_ERROR;
    }

    // Bookmarks (record 0) are unsupported anywhere in this driver — see
    // bind_col.rs's identical rejection of the bookmark column, and
    // SQLGetDescRecW's matching gate — so record 0 is out of range here too.
    if record_number < 1 {
        error!(record_number, "SQLSetDescRec: invalid record number");
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }
    let Ok(count) = usize::try_from(record_number) else {
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    };
    // Growing on demand, same as SQLSetDescFieldW's per-record-field growth:
    // msodbcsql calls AllocPlex before the field-specific setter for any
    // record write, which can grow SQL_DESC_COUNT even if a setter further
    // down this sequence later fails.
    if count > state.records.len() {
        state.set_record_count(count, desc.kind);
    }

    // Order matters, and matches the field list order the ODBC spec itself
    // gives for SQLSetDescRec:
    // - SubType first: a raw field with no validation of its own, and
    //   `set_type` resolves a verbose `Type = SQL_DATETIME` from whatever
    //   `SQL_DESC_DATETIME_INTERVAL_CODE` is *already stored* on the record
    //   (see `set_type`'s own doc comment) — so it must land before Type.
    // - DataPtr last: `set_data_ptr` reruns the SQL_C_NUMERIC precision/scale
    //   consistency check, which needs Precision/Scale already written.
    let sub_type_write = write_record_field(&mut state, record_number, |r| {
        r.datetime_interval_code = sub_type;
        r.explicitly_bound = true;
    });
    if sub_type_write != SQL_SUCCESS {
        return sub_type_write;
    }
    let type_write = set_type(
        &mut state,
        desc.kind,
        record_number,
        SQL_DESC_TYPE,
        field_type as SqlPointer,
    );
    if type_write != SQL_SUCCESS {
        return type_write;
    }
    let length_write = write_record_field(&mut state, record_number, |r| {
        r.octet_length = length;
        r.explicitly_bound = true;
    });
    if length_write != SQL_SUCCESS {
        return length_write;
    }
    let precision_write = set_precision(&mut state, record_number, precision as SqlPointer);
    if precision_write != SQL_SUCCESS {
        return precision_write;
    }
    let scale_write = set_scale(&mut state, record_number, scale as SqlPointer);
    if scale_write != SQL_SUCCESS {
        return scale_write;
    }
    let string_length_write = write_record_field(&mut state, record_number, |r| {
        r.octet_length_ptr = string_length_ptr as SqlPointer
    });
    if string_length_write != SQL_SUCCESS {
        return string_length_write;
    }
    let indicator_write = write_record_field(&mut state, record_number, |r| {
        r.indicator_ptr = indicator_ptr as SqlPointer
    });
    if indicator_write != SQL_SUCCESS {
        return indicator_write;
    }
    let data_ptr_write = set_data_ptr(&mut state, record_number, data_ptr);
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
    use crate::api::odbc_types::{
        SQL_C_NUMERIC, SQL_CODE_TIMESTAMP, SQL_DATETIME, SQL_DESC_CONCISE_TYPE, SQL_DESC_DATA_PTR,
        SQL_DESC_DATETIME_INTERVAL_CODE, SQL_DESC_INDICATOR_PTR, SQL_DESC_OCTET_LENGTH,
        SQL_DESC_OCTET_LENGTH_PTR, SQL_DESC_PRECISION, SQL_DESC_SCALE, SQL_INTEGER,
        SQL_NULL_HANDLE, SQL_TYPE_TIMESTAMP, SqlPointer,
    };
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::api::sqlstate::ERR_CANNOT_MODIFY_IRD;
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
        assert_eq!(
            get_small_int(h.ard(), 1, SQL_DESC_OCTET_LENGTH as SqlSmallInt),
            4
        );
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
        assert_eq!(
            get_small_int(h.ipd(), 1, SQL_DESC_DATETIME_INTERVAL_CODE as SqlSmallInt),
            SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).unwrap()
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
}
