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
    SQL_CA_SS_CATALOG_NAME, SQL_CA_SS_DATA_CLASSIFICATION, SQL_CA_SS_DATA_CLASSIFICATION_VERSION,
    SQL_CA_SS_SCHEMA_NAME, SQL_CA_SS_TYPE_NAME, SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME,
    SQL_CA_SS_UDT_CATALOG_NAME, SQL_CA_SS_UDT_SCHEMA_NAME, SQL_CA_SS_UDT_TYPE_NAME,
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
    ERR_INVALID_DESCRIPTOR_FIELD, ERR_INVALID_DESCRIPTOR_INDEX, WARN_STRING_TRUNCATION, post_diag,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::post_sql_error;
use crate::error::{HasDiagnostics, free_errors};
use crate::handles::desc::{DescHeader, DescRecord, DescState, FieldScope, classify_field};
use crate::handles::{DbcHandle, DescHandle, HandleType, handle_from_raw};

/// Implementation of [`SQLGetDescFieldW`](super::exports::SQLGetDescFieldW).
///
/// # Safety
/// `descriptor_handle` must be null or point to a live `DescHandle`. For a
/// character field, `value_ptr`, when non-null, must be writable for
/// `buffer_length` bytes; for any other field it must be writable for one value
/// of that field's declared type. `string_length_ptr`, when non-null, must be
/// writable for one `SqlInteger`.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_get_desc_field_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    crate::ffi_entry!(
        "SQLGetDescFieldW",
        descriptor_handle,
        debug!(
            ?descriptor_handle,
            record_number,
            field_identifier,
            ?value_ptr,
            buffer_length,
            ?string_length_ptr,
            "SQLGetDescFieldW called",
        ),
        unsafe {
            sql_get_desc_field_w_impl(
                descriptor_handle,
                record_number,
                field_identifier,
                value_ptr,
                buffer_length,
                string_length_ptr,
            )
        }
    )
}

/// # Safety
/// `descriptor_handle` must be null or point to a live `DescHandle`. For a
/// character field, `value_ptr`, when non-null, must be writable for
/// `buffer_length` bytes; for any other field it must be writable for one value
/// of that field's declared type. `string_length_ptr`, when non-null, must be
/// writable for one `SqlInteger`.
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
    if let Some(result) = get_focused_tvp_header_field(
        desc,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
        string_length_ptr,
    ) {
        return result;
    }

    if let Ok(field) = SqlUSmallInt::try_from(field_identifier)
        && record_number >= 1
        && matches!(
            desc.kind,
            crate::handles::desc::DescKind::AppParam
                | crate::handles::desc::DescKind::Ad
                | crate::handles::desc::DescKind::ImpParam
        )
        && matches!(
            field,
            SQL_DESC_DATA_PTR
                | SQL_DESC_INDICATOR_PTR
                | SQL_DESC_OCTET_LENGTH_PTR
                | SQL_DESC_OCTET_LENGTH
                | SQL_DESC_LENGTH
                | SQL_DESC_NULLABLE
                | SQL_DESC_CONCISE_TYPE
                | SQL_DESC_DATETIME_INTERVAL_CODE
                | SQL_DESC_PRECISION
                | SQL_DESC_SCALE
                | SQL_DESC_TYPE
                | SQL_DESC_NAME
                | SQL_DESC_UNNAMED
                | SQL_DESC_PARAMETER_TYPE
        )
        && classify_field(desc.kind, field).is_some_and(|access| access.scope == FieldScope::Record)
    {
        match super::get_desc_rec::focused_tvp_record_for_desc(desc, record_number) {
            Ok(Some(super::get_desc_rec::FocusedTvpRecord::PastCount)) => {
                let Ok(mut state) = desc.inner.lock() else {
                    error!("SQLGetDescFieldW: desc mutex poisoned");
                    return SQL_ERROR;
                };
                free_errors(&mut state);
                return SQL_NO_DATA;
            }
            Ok(Some(super::get_desc_rec::FocusedTvpRecord::Record(record))) => {
                let Ok(mut state) = desc.inner.lock() else {
                    error!("SQLGetDescFieldW: desc mutex poisoned");
                    return SQL_ERROR;
                };
                free_errors(&mut state);
                let Some(value) = record_field_value(&record, field) else {
                    post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
                    return SQL_ERROR;
                };
                return value.write(value_ptr, buffer_length, string_length_ptr, &mut *state);
            }
            Ok(None) => {}
            Err(()) => {
                error!("SQLGetDescFieldW: failed reading focused TVP descriptor records");
                if let Ok(mut state) = desc.inner.lock() {
                    free_errors(&mut state);
                    post_sql_error(
                        &mut state,
                        crate::api::sqlstate::SQLSTATE_HY000,
                        0,
                        "Internal error reading focused table-valued parameter descriptor",
                    );
                }
                return SQL_ERROR;
            }
        }
    }

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
        FieldScope::Header if field == SQL_CA_SS_DATA_CLASSIFICATION_VERSION => {
            // The session's acknowledged version, as `GetIRDHeaderField` reads
            // `hConn->GetDataClassificationVersion()`, so it answers before
            // any result set.
            // SAFETY: the DM frees every descriptor before its connection.
            let dbc = unsafe { handle_from_raw::<DbcHandle>(desc.parent_dbc) };
            FieldValue::Integer(SqlInteger::from(
                dbc.negotiated_data_classification_version(),
            ))
        }
        FieldScope::Header if field == SQL_CA_SS_DATA_CLASSIFICATION => {
            return unsafe {
                write_data_classification(&mut state, value_ptr, buffer_length, string_length_ptr)
            };
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

fn get_focused_tvp_header_field(
    desc: &DescHandle,
    _record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> Option<SqlReturn> {
    let field = SqlUSmallInt::try_from(field_identifier).ok()?;
    let is_count = field == SQL_DESC_COUNT;
    if if is_count {
        !matches!(
            desc.kind,
            crate::handles::desc::DescKind::AppParam
                | crate::handles::desc::DescKind::Ad
                | crate::handles::desc::DescKind::ImpParam
        )
    } else {
        !matches!(
            desc.kind,
            crate::handles::desc::DescKind::AppParam | crate::handles::desc::DescKind::Ad
        )
    } {
        return None;
    }
    if !matches!(
        field,
        SQL_DESC_COUNT | SQL_DESC_ARRAY_SIZE | SQL_DESC_BIND_OFFSET_PTR | SQL_DESC_BIND_TYPE
    ) || !classify_field(desc.kind, field)
        .is_some_and(|access| access.scope == FieldScope::Header)
    {
        return None;
    }
    if is_count {
        let count = match desc.focused_tvp_record_count() {
            Ok(Some(count)) => count,
            Ok(None) => return None,
            Err(()) => {
                error!("SQLGetDescFieldW: failed reading focused TVP descriptor count");
                let Ok(mut state) = desc.inner.lock() else {
                    error!("SQLGetDescFieldW: desc mutex poisoned posting child-count error");
                    return Some(SQL_ERROR);
                };
                free_errors(&mut state);
                post_sql_error(
                    &mut state,
                    crate::api::sqlstate::SQLSTATE_HY000,
                    0,
                    "Internal error reading focused table-valued parameter descriptor count",
                );
                return Some(SQL_ERROR);
            }
        };
        let Ok(mut state) = desc.inner.lock() else {
            error!("SQLGetDescFieldW: desc mutex poisoned reading child count");
            return Some(SQL_ERROR);
        };
        free_errors(&mut state);
        let count = SqlSmallInt::try_from(count).unwrap_or(SqlSmallInt::MAX);
        return Some(FieldValue::SmallInt(count).write(
            value_ptr,
            buffer_length,
            string_length_ptr,
            &mut state,
        ));
    }
    let header = match desc.focused_tvp_layout_header() {
        Ok(Some(header)) => header,
        Ok(None) => return None,
        Err(()) => {
            error!("SQLGetDescFieldW: failed reading focused TVP descriptor headers");
            let Ok(mut state) = desc.inner.lock() else {
                error!("SQLGetDescFieldW: desc mutex poisoned posting child-header error");
                return Some(SQL_ERROR);
            };
            free_errors(&mut state);
            post_sql_error(
                &mut state,
                crate::api::sqlstate::SQLSTATE_HY000,
                0,
                "Internal error reading focused table-valued parameter descriptor header",
            );
            return Some(SQL_ERROR);
        }
    };
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLGetDescFieldW: desc mutex poisoned reading child header");
        return Some(SQL_ERROR);
    };
    free_errors(&mut state);
    let Some(value) = header_field_value(&header, field) else {
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return Some(SQL_ERROR);
    };
    Some(value.write(value_ptr, buffer_length, string_length_ptr, &mut *state))
}

/// Copies the current result set's DATACLASSIFICATION payload, as msodbcsql's
/// `SQLGetDescFieldW` does (`sqlcdesc.cpp:5220-5260`): `StringLengthPtr`
/// receives the full length, a short buffer gets the prefix and `01004`, and a
/// result set without classification is `HY091`.
///
/// # Safety
/// `value_ptr`, when non-null, must be writable for `buffer_length` bytes;
/// `string_length_ptr`, when non-null, must be writable for one `SqlInteger`.
unsafe fn write_data_classification(
    state: &mut DescState,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Some(payload) = state.data_classification.as_deref() else {
        post_diag(state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };
    let length = payload.len();
    let truncated = if value_ptr.is_null() {
        false
    } else {
        let capacity = usize::try_from(buffer_length).unwrap_or(0);
        let copied = capacity.min(length);
        // SAFETY: the caller guarantees `value_ptr` is writable for
        // `buffer_length` bytes, and `copied` does not exceed that or the
        // payload. A byte copy has no alignment requirement.
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), value_ptr.cast::<u8>(), copied) };
        capacity < length
    };
    unsafe {
        write_if_some(
            string_length_ptr,
            SqlInteger::try_from(length).unwrap_or(SqlInteger::MAX),
        );
    }
    if truncated {
        post_diag(state, WARN_STRING_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
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
        SQL_DESC_BIND_TYPE => FieldValue::Integer(header.bind_type_as_desc_field()),
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
        // A type-name part that was never set reads back empty, the same answer
        // a record with no server type identity at all gives. Catalog/schema
        // aliases share storage, while UDT and TVP type names are distinct.
        SQL_CA_SS_UDT_CATALOG_NAME
        | SQL_CA_SS_UDT_SCHEMA_NAME
        | SQL_CA_SS_UDT_TYPE_NAME
        | SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME
        | SQL_CA_SS_CATALOG_NAME
        | SQL_CA_SS_SCHEMA_NAME
        | SQL_CA_SS_TYPE_NAME => {
            let part = record.udt_names.as_deref().map(|n| match field {
                SQL_CA_SS_UDT_CATALOG_NAME | SQL_CA_SS_CATALOG_NAME => &n.catalog,
                SQL_CA_SS_UDT_SCHEMA_NAME | SQL_CA_SS_SCHEMA_NAME => &n.schema,
                SQL_CA_SS_UDT_TYPE_NAME => &n.type_name,
                SQL_CA_SS_TYPE_NAME => &n.tvp_type_name,
                _ => &n.assembly_type_name,
            });
            FieldValue::Text(part.cloned().unwrap_or_default())
        }
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
                    post_diag(state, WARN_STRING_TRUNCATION);
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
        SQL_C_NUMERIC, SQL_DECIMAL, SQL_DESC_ALLOC_AUTO, SQL_INVALID_HANDLE, SQL_NULL_HANDLE,
        SQL_NULLABLE, SQL_PARAM_INPUT, SQL_UNNAMED,
    };
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::error::diag::DiagRecord;
    use crate::handles::{DescHandle, StmtHandle, handle_from_raw};
    use crate::test_support::TestHandles;
    use crate::test_support::assert_last_diag;

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
    const SQL_DESC_PRECISION: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_PRECISION as SqlSmallInt;
    const SQL_DESC_SCALE: SqlSmallInt = crate::api::odbc_types::SQL_DESC_SCALE as SqlSmallInt;
    const SQL_DESC_NULLABLE: SqlSmallInt = crate::api::odbc_types::SQL_DESC_NULLABLE as SqlSmallInt;

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
    fn focused_record_uses_child_defaults_and_returns_no_data_past_count() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        assert_eq!(
            unsafe {
                sql_set_desc_field_w(
                    h.apd(),
                    1,
                    SQL_DESC_CONCISE_TYPE,
                    SQL_C_NUMERIC as isize as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );
        assert!(
            unsafe { handle_from_raw::<DescHandle>(h.apd()) }
                .inner
                .lock()
                .unwrap()
                .records
                .is_empty(),
            "focused write must not create an outer APD record"
        );

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

        let mut length: SqlULen = 0;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                1,
                crate::api::odbc_types::SQL_DESC_LENGTH as SqlSmallInt,
                &mut length as *mut SqlULen as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(length, 38);

        stmt.inner
            .lock()
            .unwrap()
            .tvp_column_bindings
            .insert(1, Vec::new());
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
        assert_eq!(ret, SQL_NO_DATA);
    }

    #[test]
    fn focused_ipd_fields_read_sql_metadata_from_the_child_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let ipd = unsafe { handle_from_raw::<DescHandle>(h.ipd()) };
        {
            let mut state = ipd.inner.lock().unwrap();
            state.set_record_count(1, ipd.kind);
            let outer = state.record_mut(1).unwrap();
            outer.name = "outer parameter".to_string();
            outer.parameter_type = crate::api::odbc_types::SQL_PARAM_OUTPUT;
        }
        stmt.inner.lock().unwrap().vendor_attrs.store_param_focus(1);
        stmt.inner.lock().unwrap().tvp_column_bindings.insert(
            1,
            vec![
                Some(crate::params::BoundParam {
                    input_output_type: SQL_PARAM_INPUT,
                    c_type: crate::api::odbc_types::SQL_C_SLONG,
                    sql_type: SQL_DECIMAL,
                    column_size: 10,
                    decimal_digits: 2,
                    app_datetime_interval_code: None,
                    sql_datetime_interval_code: None,
                    app_precision: 0,
                    app_scale: 0,
                    parameter_value_ptr: ptr::null_mut(),
                    buffer_length: 0,
                    strlen_or_ind_ptr: ptr::null_mut(),
                    octet_length_ptr: ptr::null_mut(),
                }),
                None,
            ],
        );

        let mut concise: SqlSmallInt = -1;
        let mut precision: SqlSmallInt = -1;
        let mut scale: SqlSmallInt = -1;
        for (field, value) in [
            (SQL_DESC_CONCISE_TYPE, &mut concise),
            (SQL_DESC_PRECISION, &mut precision),
            (SQL_DESC_SCALE, &mut scale),
        ] {
            assert_eq!(
                unsafe {
                    sql_get_desc_field_w(
                        h.ipd(),
                        1,
                        field,
                        value as *mut SqlSmallInt as SqlPointer,
                        0,
                        ptr::null_mut(),
                    )
                },
                SQL_SUCCESS
            );
        }

        assert_eq!(concise, SQL_DECIMAL);
        assert_eq!(precision, 10);
        assert_eq!(scale, 2);

        let mut name = [0u16; 32];
        let mut name_length = -1;
        assert_eq!(
            unsafe {
                sql_get_desc_field_w(
                    h.ipd(),
                    1,
                    SQL_DESC_NAME,
                    name.as_mut_ptr().cast(),
                    (name.len() * size_of::<u16>()) as SqlInteger,
                    &mut name_length,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(name_length, 0);
        assert_eq!(name[0], 0, "the focused child name is empty");
        let get_small_int = |record_number, field_identifier| {
            let mut value: SqlSmallInt = 0;
            assert_eq!(
                unsafe {
                    sql_get_desc_field_w(
                        h.ipd(),
                        record_number,
                        field_identifier,
                        (&mut value as *mut SqlSmallInt).cast(),
                        0,
                        ptr::null_mut(),
                    )
                },
                SQL_SUCCESS
            );
            value
        };
        assert_eq!(
            get_small_int(1, SQL_DESC_UNNAMED),
            SQL_UNNAMED as SqlSmallInt,
            "an overlapping outer IPD name must not leak into the focused child"
        );
        assert_eq!(
            get_small_int(1, SQL_DESC_PARAMETER_TYPE),
            SQL_PARAM_INPUT,
            "an overlapping outer parameter type must not leak into the child"
        );
        assert_eq!(
            get_small_int(2, SQL_DESC_PARAMETER_TYPE),
            SQL_PARAM_INPUT,
            "a focused child past the outer IPD count still has a default record"
        );
    }

    #[test]
    fn focused_ipd_nullable_reads_from_the_child_record() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.vendor_attrs.store_param_focus(1);
            state.tvp_column_bindings.insert(1, vec![None, None]);
        }

        let mut nullable: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ipd(),
                2,
                SQL_DESC_NULLABLE,
                (&mut nullable as *mut SqlSmallInt).cast(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(nullable, SQL_NULLABLE);
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
