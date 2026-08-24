// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLGetDescFieldW`.
//!
//! Field validity per descriptor kind is decided once, in
//! [`crate::handles::desc::classify_field`], shared with `SQLSetDescFieldW`.
//! This module owns only the get-direction concerns: mapping a valid field
//! to its stored value ([`header_field_value`] / [`record_field_value`]) and
//! writing that value out at the field's ODBC-mandated width
//! ([`FieldValue::write`]).

use std::mem::size_of;

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DESC_ALLOC_TYPE, SQL_DESC_ARRAY_SIZE, SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_BIND_OFFSET_PTR,
    SQL_DESC_BIND_TYPE, SQL_DESC_CONCISE_TYPE, SQL_DESC_COUNT, SQL_DESC_DATA_PTR,
    SQL_DESC_DATETIME_INTERVAL_CODE, SQL_DESC_INDICATOR_PTR, SQL_DESC_LENGTH, SQL_DESC_NAME,
    SQL_DESC_NULLABLE, SQL_DESC_OCTET_LENGTH, SQL_DESC_OCTET_LENGTH_PTR, SQL_DESC_PARAMETER_TYPE,
    SQL_DESC_PRECISION, SQL_DESC_ROWS_PROCESSED_PTR, SQL_DESC_SCALE, SQL_DESC_TYPE,
    SQL_DESC_UNNAMED, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlInteger, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlULen, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_INVALID_DESCRIPTOR_FIELD, ERR_INVALID_DESCRIPTOR_INDEX, ERR_STRING_RIGHT_TRUNCATION,
    post_diag,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{HasDiagnostics, free_errors};
use crate::handles::desc::{DescHeader, DescRecord, FieldScope, classify_field};
use crate::handles::{DescHandle, HandleType, handle_from_raw};

/// Implementation of [`SQLGetDescFieldW`](super::exports::SQLGetDescFieldW).
///
/// # Safety
/// See the exported function's doc for caller requirements.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_get_desc_field_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?descriptor_handle,
        record_number,
        field_identifier,
        ?value_ptr,
        buffer_length,
        ?string_length_ptr,
        "SQLGetDescFieldW called",
    );
    crate::ffi_entry!("SQLGetDescFieldW", unsafe {
        sql_get_desc_field_w_impl(
            descriptor_handle,
            record_number,
            field_identifier,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_get_desc_field_w_impl(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    if descriptor_handle.is_null() {
        error!("SQLGetDescFieldW: descriptor_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let desc = unsafe { handle_from_raw::<DescHandle>(descriptor_handle) };
    debug_assert_eq!(
        desc.object_type,
        HandleType::Desc,
        "SQLGetDescFieldW: handle is not a DESC"
    );

    sql_get_desc_field_w_safe(
        desc,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
        string_length_ptr,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_get_desc_field_w_safe(
    desc: &DescHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLGetDescFieldW: desc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // FieldIdentifier is SQLSMALLINT (signed) per the ODBC prototype, while
    // SQL_DESC_* constants are SQLUSMALLINT-typed (shared with
    // SQLColAttributeW). Every real field id is small and non-negative, so
    // this narrowing is lossless; a negative id just fails to match any
    // known field below.
    let Ok(field) = SqlUSmallInt::try_from(field_identifier) else {
        error!(
            field_identifier,
            "SQLGetDescFieldW: negative field identifier"
        );
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    let Some(access) = classify_field(desc.kind, field) else {
        error!(
            field,
            kind = ?desc.kind,
            "SQLGetDescFieldW: field not valid for this descriptor kind"
        );
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    let value = match access.scope {
        FieldScope::Header if field == SQL_DESC_COUNT => {
            // SQL_DESC_COUNT is SQLSMALLINT-width and answered from the
            // record list itself, not from a stored header field (verified
            // against msodbcsql's GetADHeaderField/GetIPDHeaderField/
            // GetIRDHeaderField, sqlcdesc.cpp:4065-4072, 4621-4639,
            // 5207-5213 — all three report `CItemsPl(...)`, the live plex
            // size, at `sizeof(SQLSMALLINT)`).
            FieldValue::SmallInt(
                SqlSmallInt::try_from(state.records.len()).unwrap_or(SqlSmallInt::MAX),
            )
        }
        FieldScope::Header => match header_field_value(&state.header, field) {
            Some(v) => v,
            None => {
                debug_assert!(false, "classify_field/header_field_value out of sync");
                post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
                return SQL_ERROR;
            }
        },
        FieldScope::Record => {
            if record_number < 1 {
                error!(record_number, "SQLGetDescFieldW: invalid record number");
                post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
                return SQL_ERROR;
            }
            let Some(record) = state.record(record_number) else {
                // Past SQL_DESC_COUNT is "no such record yet", not an error
                // (matches msodbcsql: AD/IPD existence checked against plex
                // size, missing record returns SQL_NO_DATA_FOUND).
                return SQL_NO_DATA;
            };
            match record_field_value(record, field) {
                Some(v) => v,
                None => {
                    debug_assert!(false, "classify_field/record_field_value out of sync");
                    post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
                    return SQL_ERROR;
                }
            }
        }
    };

    value.write(value_ptr, buffer_length, string_length_ptr, &mut state)
}

/// Maps a header field to its stored value. `SQL_DESC_COUNT` is handled by
/// the caller (it is derived from the record list, not stored on the
/// header). Returns `None` only if out of sync with [`classify_field`].
fn header_field_value(header: &DescHeader, field: SqlUSmallInt) -> Option<FieldValue> {
    let value = match field {
        SQL_DESC_ALLOC_TYPE => FieldValue::SmallInt(header.alloc_type),
        SQL_DESC_ARRAY_SIZE => FieldValue::ULen(header.array_size),
        SQL_DESC_ARRAY_STATUS_PTR => FieldValue::Pointer(header.array_status_ptr),
        SQL_DESC_BIND_OFFSET_PTR => FieldValue::Pointer(header.bind_offset_ptr),
        SQL_DESC_BIND_TYPE => FieldValue::Integer(header.bind_type),
        SQL_DESC_ROWS_PROCESSED_PTR => FieldValue::Pointer(header.rows_processed_ptr),
        _ => return None,
    };
    Some(value)
}

/// Maps a record field to its stored value. Returns `None` only if out of
/// sync with [`classify_field`].
fn record_field_value(record: &DescRecord, field: SqlUSmallInt) -> Option<FieldValue> {
    let value = match field {
        SQL_DESC_CONCISE_TYPE => FieldValue::SmallInt(record.concise_type),
        SQL_DESC_TYPE => FieldValue::SmallInt(record.verbose_type()),
        SQL_DESC_DATETIME_INTERVAL_CODE => FieldValue::SmallInt(record.datetime_interval_code),
        SQL_DESC_LENGTH => FieldValue::ULen(record.length),
        SQL_DESC_OCTET_LENGTH => FieldValue::Len(record.octet_length),
        SQL_DESC_PRECISION => FieldValue::SmallInt(record.precision),
        SQL_DESC_SCALE => FieldValue::SmallInt(record.scale),
        SQL_DESC_NULLABLE => FieldValue::SmallInt(record.nullable),
        SQL_DESC_NAME => FieldValue::Text(record.name.clone()),
        // SQL_UNNAMED (1) / SQL_NAMED (0): derived from `name`, not stored.
        SQL_DESC_UNNAMED => FieldValue::SmallInt(if record.name.is_empty() { 1 } else { 0 }),
        SQL_DESC_PARAMETER_TYPE => FieldValue::SmallInt(record.parameter_type),
        SQL_DESC_DATA_PTR => FieldValue::Pointer(record.data_ptr),
        SQL_DESC_INDICATOR_PTR => FieldValue::Pointer(record.indicator_ptr),
        SQL_DESC_OCTET_LENGTH_PTR => FieldValue::Pointer(record.octet_length_ptr),
        _ => return None,
    };
    Some(value)
}

/// A descriptor field's value, tagged with its ODBC-mandated output width so
/// [`FieldValue::write`] copies exactly that many bytes into the caller's
/// buffer — `SQLGetDescFieldW` does not use one generic width for every
/// field (unlike `SQLColAttributeW`'s numeric fields, which are always
/// `SQLLEN`). Verified per-field against msodbcsql's `GetADHeaderField`
/// (`sqlcdesc.cpp:4022-4101`), `GetIPDHeaderField` (`4592-4675`),
/// `GetIRDHeaderField` (`5178-5270`), and the generic record dispatch
/// (`2220-2434`).
enum FieldValue {
    SmallInt(SqlSmallInt),
    Integer(SqlInteger),
    ULen(SqlULen),
    Len(SqlLen),
    Pointer(SqlPointer),
    Text(String),
}

impl FieldValue {
    fn write(
        self,
        value_ptr: SqlPointer,
        buffer_length: SqlInteger,
        string_length_ptr: *mut SqlInteger,
        state: &mut impl HasDiagnostics,
    ) -> SqlReturn {
        match self {
            FieldValue::SmallInt(v) => {
                unsafe { write_if_some(value_ptr as *mut SqlSmallInt, v) };
                write_scalar_length(string_length_ptr, size_of::<SqlSmallInt>());
                SQL_SUCCESS
            }
            FieldValue::Integer(v) => {
                unsafe { write_if_some(value_ptr as *mut SqlInteger, v) };
                write_scalar_length(string_length_ptr, size_of::<SqlInteger>());
                SQL_SUCCESS
            }
            FieldValue::ULen(v) => {
                unsafe { write_if_some(value_ptr as *mut SqlULen, v) };
                write_scalar_length(string_length_ptr, size_of::<SqlULen>());
                SQL_SUCCESS
            }
            FieldValue::Len(v) => {
                unsafe { write_if_some(value_ptr as *mut SqlLen, v) };
                write_scalar_length(string_length_ptr, size_of::<SqlLen>());
                SQL_SUCCESS
            }
            FieldValue::Pointer(v) => {
                unsafe { write_if_some(value_ptr as *mut SqlPointer, v) };
                write_scalar_length(string_length_ptr, size_of::<SqlPointer>());
                SQL_SUCCESS
            }
            FieldValue::Text(s) => {
                let utf16: Vec<u16> = s.encode_utf16().collect();
                // StringLengthPtr is in bytes for the wide entry point and
                // excludes the terminator (matches SQLColAttributeW's text
                // handling, api::col_attribute).
                let byte_len = SqlInteger::try_from(utf16.len() * size_of::<SqlWChar>())
                    .unwrap_or(SqlInteger::MAX);
                unsafe { write_if_some(string_length_ptr, byte_len) };

                let buf_elements = if buffer_length > 0 {
                    (buffer_length as usize) / size_of::<SqlWChar>()
                } else {
                    0
                };
                let truncated =
                    unsafe { copy_with_nul(value_ptr as *mut SqlWChar, buf_elements, &utf16) };
                if truncated {
                    post_diag(state, ERR_STRING_RIGHT_TRUNCATION);
                    SQL_SUCCESS_WITH_INFO
                } else {
                    SQL_SUCCESS
                }
            }
        }
    }
}

/// Writes a scalar field's byte width to `StringLengthPtr`, if non-null.
/// `size_of` a fixed-width type never exceeds `SqlInteger::MAX`, so the
/// fallback is unreachable in practice but kept for defensiveness rather
/// than an `as` truncation.
fn write_scalar_length(string_length_ptr: *mut SqlInteger, width: usize) {
    unsafe { write_if_some(string_length_ptr, SqlInteger::try_from(width).unwrap_or(0)) };
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_C_NUMERIC, SQL_DESC_ALLOC_AUTO, SQL_INVALID_HANDLE, SQL_NULL_HANDLE, SQL_PARAM_INPUT,
        SQL_UNNAMED,
    };
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::error::diag::DiagRecord;
    use crate::handles::{DescHandle, handle_from_raw};
    use crate::test_support::TestHandles;

    // `SQL_DESC_*` field-identifier constants are `SqlUSmallInt`-typed (shared
    // with `SQLColAttributeW`'s unsigned `FieldIdentifier`), but
    // `SQLGetDescFieldW`/`SQLSetDescFieldW`'s `FieldIdentifier` is signed per
    // the ODBC prototype. Every real field id is small and non-negative, so
    // this is a lossless, compile-time-constant retyping done once per name
    // here rather than a cast at every call site below.
    const SQL_DESC_TYPE: SqlSmallInt = crate::api::odbc_types::SQL_DESC_TYPE as SqlSmallInt;
    const SQL_DESC_CONCISE_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_CONCISE_TYPE as SqlSmallInt;
    const SQL_DESC_COUNT: SqlSmallInt = crate::api::odbc_types::SQL_DESC_COUNT as SqlSmallInt;
    const SQL_DESC_ALLOC_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_ALLOC_TYPE as SqlSmallInt;
    const SQL_DESC_PARAMETER_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_PARAMETER_TYPE as SqlSmallInt;
    const SQL_DESC_UNNAMED: SqlSmallInt = crate::api::odbc_types::SQL_DESC_UNNAMED as SqlSmallInt;
    const SQL_DESC_NAME: SqlSmallInt = crate::api::odbc_types::SQL_DESC_NAME as SqlSmallInt;

    fn assert_last_diag(records: &[DiagRecord], expected: crate::api::sqlstate::DiagMsg) {
        let d = records.last().expect("expected a diagnostic record");
        assert_eq!(d.sql_state, expected.state, "SQLSTATE mismatch");
        assert!(
            d.message.contains(expected.text),
            "message {:?} did not contain {:?}",
            d.message,
            expected.text
        );
    }

    fn desc_diags(handle: SqlHandle) -> Vec<DiagRecord> {
        let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
        desc.inner.lock().unwrap().diag_records.clone()
    }

    #[test]
    fn get_desc_field_null_handle_returns_invalid_handle() {
        let ret = unsafe {
            sql_get_desc_field_w(
                SQL_NULL_HANDLE,
                1,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_desc_field_fresh_descriptor_has_zero_count() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut count: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                0,
                SQL_DESC_COUNT,
                &mut count as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(count, 0);
    }

    #[test]
    fn get_desc_field_alloc_type_is_auto_for_implicit_descriptor() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut alloc_type: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ard(),
                0,
                SQL_DESC_ALLOC_TYPE,
                &mut alloc_type as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(alloc_type, SQL_DESC_ALLOC_AUTO);
    }

    #[test]
    fn get_desc_field_unknown_field_returns_hy091() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_desc_field_w(h.ard(), 0, 0x7FFF, ptr::null_mut(), 0, ptr::null_mut())
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ard()), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn get_desc_field_wrong_kind_field_returns_hy091() {
        let h = TestHandles::with_env_dbc_stmt();
        // SQL_DESC_PARAMETER_TYPE is IPD-only.
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_PARAMETER_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ard()), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn get_desc_field_invalid_record_number_returns_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                0,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_DESCRIPTOR_INDEX);

        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                -1,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_DESCRIPTOR_INDEX);
    }

    #[test]
    fn get_desc_field_record_past_count_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }

    #[test]
    fn get_desc_field_reads_back_a_value_set_through_set_desc_field() {
        let h = TestHandles::with_env_dbc_stmt();
        unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_NUMERIC as isize as SqlPointer,
                0,
            )
        };

        let mut concise: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_CONCISE_TYPE,
                &mut concise as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(concise, SQL_C_NUMERIC);
    }

    #[test]
    fn get_desc_field_unnamed_derives_from_name() {
        let h = TestHandles::with_env_dbc_stmt();
        // Force a record into existence on the IPD (name is IPD-writable).
        unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_PARAMETER_TYPE,
                SQL_PARAM_INPUT as isize as SqlPointer,
                0,
            )
        };

        let mut unnamed: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_UNNAMED,
                &mut unnamed as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(unnamed, SQL_UNNAMED as SqlSmallInt);
    }

    #[test]
    fn get_desc_field_name_truncation_reports_success_with_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let name: Vec<u16> = "param_name".encode_utf16().collect();
        unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_NAME,
                name.as_ptr() as SqlPointer,
                SqlInteger::try_from(name.len() * size_of::<SqlWChar>()).unwrap(),
            )
        };

        let mut buf: [SqlWChar; 4] = [0xDEAD; 4];
        let mut string_length: SqlInteger = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_NAME,
                buf.as_mut_ptr() as SqlPointer,
                SqlInteger::try_from(buf.len() * size_of::<SqlWChar>()).unwrap(),
                &mut string_length,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        // Untruncated length is reported in bytes, independent of the small buffer.
        assert_eq!(
            string_length,
            SqlInteger::try_from(name.len() * size_of::<SqlWChar>()).unwrap()
        );
    }

    #[test]
    fn get_desc_field_ird_reports_type_read_only_via_get() {
        // IRD supports GET on the common record fields even though SET is
        // blocked; verify a freshly-empty IRD reports SQL_NO_DATA rather than
        // an error for a record that has not been populated yet (AB#47437).
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ird(),
                1,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
    }
}
