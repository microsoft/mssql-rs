// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation with incremental row materialization.

use tracing::{debug, error};

use std::sync::MutexGuard;

use super::odbc_types::{
    SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_SBIGINT,
    SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT, SQL_C_TYPE_DATE,
    SQL_C_TYPE_TIMESTAMP, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA,
    SQL_NO_TOTAL, SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlDateStruct, SqlGuid,
    SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlSsTime2Struct,
    SqlSsTimestampoffsetStruct, SqlTimestampStruct, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::exec_common::release_busy_if_row_exhausted;
use crate::api::odbc_types::SqlWChar;
use crate::api::type_rules::{canonical_c_type, is_valid_c_type};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ActivePlpStream, STMT_STATE_CURSOR_OPEN, StmtState};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::{CursorColumn, CursorPoll, PlpChunk};
use mssql_tds::core::TdsResult;
use mssql_tds::encoding_rs::{self, Decoder};

use crate::conversion::error::{ConvError, ConvOk};
use crate::conversion::fetch_convert::{
    convert_datetime_c, convert_float_c, convert_guid_c, convert_integer_c, date_parts,
    datetime2_parts, datetimeoffset_parts, extract_datetime_parts, format_datetime_parts,
    is_datetime_c_target, is_float_c_target, is_integer_c_target, money_scaled, sql_string_to_text,
    time_parts,
};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::decoder::DECIMAL_STR_LEN;
use mssql_tds::datatypes::sql_string::EncodingType;
use mssql_tds::query::metadata::PlpEncoding;

/// Maximum speculative PLP read-ahead retained by one `SQLGetData` stream.
///
/// Larger tails remain on the wire so one call cannot allocate in proportion
/// to an arbitrarily large MAX value.
const MAX_PLP_PREFETCH_BYTES: usize = 64 * 1024;

/// Implements SQLGetData for current-row retrieval.
///
/// Current scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports `SQL_C_CHAR` and `SQL_C_WCHAR` for text retrieval.
/// - Supports incremental row resume and chunked PLP retrieval via
///   `read_active_plp_chunk`.
///
/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `target_value_ptr`, when non-null, must be writable for `buffer_length`
/// bytes for variable-width targets, including `SQL_C_WCHAR`, where the length
/// is still measured in bytes. For a fixed-width target it must be writable for
/// the full size of `target_type`, even when `buffer_length` is zero or smaller.
/// `strlen_or_ind_ptr`, when non-null, must be writable for one `SqlLen`.
pub(crate) unsafe fn sql_get_data(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number,
        target_type,
        ?target_value_ptr,
        buffer_length,
        ?strlen_or_ind_ptr,
        "SQLGetData called",
    );

    crate::ffi_entry!("SQLGetData", unsafe {
        sql_get_data_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
/// `target_value_ptr`, when non-null, must be writable for `buffer_length`
/// bytes for variable-width targets, including `SQL_C_WCHAR`, where the length
/// is still measured in bytes. For a fixed-width target it must be writable for
/// the full size of `target_type`, even when `buffer_length` is zero or smaller.
/// `strlen_or_ind_ptr`, when non-null, must be writable for one `SqlLen`.
unsafe fn sql_get_data_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetData: handle is not a STMT"
    );

    sql_get_data_safe(
        statement_handle,
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_get_data_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug_assert!(
        buffer_length >= 0,
        "SQLGetData: DM should reject negative buffer_length (HY090)"
    );

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    let col_index = usize::from(column_number);
    let metadata_len = stmt_state.column_metadata.len();
    if col_index == 0 || col_index > metadata_len {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // Whether the target is a C type at all is a property of the request, not
    // of the column, so it is settled before the captured/PLP dispatch below:
    // the same TargetType must give the same SQLSTATE whichever way the column
    // is delivered. Each path's own HYC00 check then covers valid target types
    // it cannot deliver yet. msodbcsql draws the same line in `IsValidCType`.
    if !is_valid_c_type(canonical_c_type(target_type)) {
        post_diag(&mut stmt_state, ERR_INVALID_C_DATA_TYPE);
        return SQL_ERROR;
    }

    // Continuation: app is calling SQLGetData again on the same PLP column to
    // get the next chunk from the active wire stream.
    if let Some(active_plp) = stmt_state
        .active_plp
        .as_ref()
        .filter(|stream| stream.column == col_index)
    {
        let prepared_stream = (
            active_plp.encoding,
            active_plp.pending_units.len(),
            active_plp.narrow_to_wide.is_some(),
        );
        return stream_active_plp_chunk(
            stmt,
            statement_handle,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
            false,
            Some(prepared_stream),
            Some(stmt_state),
        );
    }

    // If the app jumps to a different column while a PLP stream was open —
    // incorrect usage per the ODBC spec — clear the stale stream state.
    if stmt_state.active_plp.is_some() {
        stmt_state.active_plp = None;
    }

    // Enforce forward-only column access within a row.
    let last_col = stmt_state.current_row_last_col;
    if last_col > 0 {
        if col_index < last_col {
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }
        if col_index == last_col {
            return SQL_NO_DATA;
        }
    }

    if !stmt_state.row_positioned {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    }

    // If we already captured this column (e.g., prior HYC00 on same column), skip the resume.
    let already_captured = stmt_state
        .last_captured
        .as_ref()
        .is_some_and(|(c, _)| *c == col_index);

    if already_captured {
        let rc = write_captured_column(
            &mut stmt_state,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
        return finish_get_data(stmt, statement_handle, stmt_state, col_index, rc);
    }

    let mut try_complete_buffered_plp = false;
    if let Some(row) = stmt_state.buffered_get_data_row.as_mut() {
        row.discard_before(col_index - 1);
        let binary_probe = target_type == SQL_C_BINARY && buffer_length == 0;
        if binary_probe
            && let Some(value) = row.values.get(col_index - 1).and_then(Option::as_ref)
            && !matches!(value, ColumnValues::Null)
        {
            let length = binary_length(value);
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            // SAFETY: per the SQLGetData contract `strlen_or_ind_ptr` is null or
            // writable for one `SqlLen`; `write_if_some` null-checks.
            unsafe { write_if_some(strlen_or_ind_ptr, length) };
            return SQL_SUCCESS;
        }

        // The eight-column cutoff is only a performance gate: below it, the
        // extra complete-value probe costs more than the allocation it avoids.
        let direct_buffered = row.values.len() >= 8
            || row
                .variant_bases
                .get(col_index - 1)
                .is_some_and(Option::is_some);
        let direct_string = direct_buffered
            && matches!(target_type, SQL_C_CHAR | SQL_C_WCHAR)
            && row
                .values
                .get(col_index - 1)
                .and_then(Option::as_ref)
                .is_some_and(|value| unsafe {
                    // SAFETY: forwards the SQLGetData buffer contract —
                    // `target_value_ptr` is null or writable for `buffer_length`
                    // bytes and `strlen_or_ind_ptr` null or writable for one
                    // `SqlLen`. `value` is borrowed from the buffered row, which
                    // the application buffer cannot alias.
                    try_write_complete_buffered_string(
                        value,
                        target_type,
                        target_value_ptr,
                        buffer_length,
                        strlen_or_ind_ptr,
                    )
                });
        if direct_string {
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            row.consumed = row.consumed.max(col_index);
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            stmt_state.current_row_last_col = col_index;
            stmt_state.partial_text_offset = None;
            return finish_get_data(stmt, statement_handle, stmt_state, col_index, SQL_SUCCESS);
        }
        let direct_decimal = direct_buffered
            && target_type == SQL_C_CHAR
            && row
                .values
                .get(col_index - 1)
                .and_then(Option::as_ref)
                .is_some_and(|value| unsafe {
                    // SAFETY: same SQLGetData buffer contract as the string path
                    // above, with `value` borrowed from the non-aliasing row.
                    try_write_complete_buffered_decimal(
                        value,
                        target_value_ptr,
                        buffer_length,
                        strlen_or_ind_ptr,
                    )
                });
        if direct_decimal {
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            row.consumed = row.consumed.max(col_index);
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            stmt_state.current_row_last_col = col_index;
            stmt_state.partial_text_offset = None;
            return finish_get_data(stmt, statement_handle, stmt_state, col_index, SQL_SUCCESS);
        }
        let direct_scalar = row
            .values
            .get(col_index - 1)
            .and_then(Option::as_ref)
            .is_some_and(|value| unsafe {
                // SAFETY: the fixed-size arms only write when `target_type`
                // names a C type the DM sized `target_value_ptr` for, and
                // `strlen_or_ind_ptr` is null or writable for one `SqlLen`.
                // `value` is borrowed from the non-aliasing buffered row.
                try_write_exact_buffered_scalar(
                    value,
                    target_type,
                    target_value_ptr,
                    strlen_or_ind_ptr,
                )
            });
        if direct_scalar {
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            row.consumed = row.consumed.max(col_index);
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            stmt_state.current_row_last_col = col_index;
            stmt_state.partial_text_offset = None;
            return finish_get_data(stmt, statement_handle, stmt_state, col_index, SQL_SUCCESS);
        }
        let typed_buffered = is_typed_c_target(target_type)
            && row
                .values
                .get(col_index - 1)
                .and_then(Option::as_ref)
                .is_some_and(|value| !matches!(value, ColumnValues::Null));
        if typed_buffered {
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            let converted =
                row.values
                    .get(col_index - 1)
                    .and_then(Option::as_ref)
                    .map(|value| unsafe {
                        // SAFETY: `is_typed_c_target` restricts `target_type` to
                        // the fixed-size C types the DM sized `target_value_ptr`
                        // for; `strlen_or_ind_ptr` is null or writable for one
                        // `SqlLen`.
                        convert_typed_c(value, target_type, target_value_ptr, strlen_or_ind_ptr)
                    });
            let Some(converted) = converted else {
                post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
                return SQL_ERROR;
            };
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            let rc = finish_typed_conv(&mut stmt_state, converted);
            if rc != SQL_ERROR {
                if let Some(row) = stmt_state.buffered_get_data_row.as_mut()
                    && let Some(value) = row.values.get_mut(col_index - 1)
                {
                    *value = None;
                    row.consumed = row.consumed.max(col_index);
                }
                stmt_state.current_row_last_col = col_index;
                stmt_state.partial_text_offset = None;
            }
            return finish_get_data(stmt, statement_handle, stmt_state, col_index, rc);
        }

        let captured = row.values.get_mut(col_index - 1).and_then(Option::take);
        if let Some(value) = captured {
            row.consumed = row.consumed.max(col_index);
            let variant_base = row.variant_bases.get(col_index - 1).copied().flatten();
            stmt_state.last_captured = Some((col_index, value));
            stmt_state.last_variant_base = variant_base.map(|base| (col_index, base));
            let rc = write_captured_column(
                &mut stmt_state,
                col_index,
                target_type,
                target_value_ptr,
                buffer_length,
                strlen_or_ind_ptr,
            );
            return finish_get_data(stmt, statement_handle, stmt_state, col_index, rc);
        }

        // A deferred slot marks the first PLP or a later column. The TDS cursor
        // is already paused at that boundary, so discard the now-inaccessible
        // prefix and continue through the ordinary streaming/resume path.
        if let Some(row) = stmt_state.buffered_get_data_row.as_mut() {
            for value in row.values.iter_mut().take(row.consumed) {
                *value = None;
            }
            row.wire_deferred = true;
        }
        try_complete_buffered_plp = target_type == SQL_C_WCHAR
            && !strlen_or_ind_ptr.is_null()
            && buffer_length >= SqlLen::try_from(std::mem::size_of::<SqlWChar>()).unwrap_or(2)
            && stmt_state
                .column_metadata
                .get(col_index - 1)
                .and_then(|metadata| metadata.plp_encoding())
                == Some(PlpEncoding::Utf16Text);
    }

    // Resume the decoder to the requested column then write output.
    drop(stmt_state);
    if try_complete_buffered_plp
        && let Some(rc) = try_deliver_complete_buffered_unicode_plp(
            stmt,
            statement_handle,
            col_index,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    {
        return rc;
    }
    let rc = resume_row_to_column(stmt, statement_handle, col_index);
    if rc != SQL_SUCCESS {
        return rc;
    }
    let Ok(mut reopened_stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned after row resume");
        return SQL_ERROR;
    };
    // last_captured is None only when the decoder paused at a PLP column.
    if reopened_stmt_state.last_captured.is_none() && !reopened_stmt_state.row_exhausted {
        return stream_active_plp_chunk(
            stmt,
            statement_handle,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
            true,
            None,
            Some(reopened_stmt_state),
        );
    }
    let rc = write_captured_column(
        &mut reopened_stmt_state,
        col_index,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    );
    finish_get_data(stmt, statement_handle, reopened_stmt_state, col_index, rc)
}

/// Delivers a buffered string straight into the application buffer when the
/// stored encoding already matches `target_type`, skipping the decode and
/// intermediate allocation that the general conversion path performs.
///
/// Returns `false` when the value cannot be delivered verbatim, leaving the
/// application buffer untouched so the caller can fall back.
///
/// # Safety
/// - `target_value_ptr`, if non-null, must be writable for `buffer_length`
///   bytes. `buffer_length` is a byte count for both `SQL_C_CHAR` and
///   `SQL_C_WCHAR`, per the ODBC contract.
/// - `strlen_or_ind_ptr`, if non-null, must be writable for one `SqlLen`.
/// - Neither pointer may alias `value`.
#[inline(never)]
unsafe fn try_write_complete_buffered_string(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> bool {
    let ColumnValues::String(value) = value else {
        return false;
    };
    let bytes = &value.bytes;
    let direct_char = target_type == SQL_C_CHAR
        && (matches!(value.encoding_type(), EncodingType::Utf8)
            && std::str::from_utf8(bytes).is_ok()
            || matches!(value.encoding_type(), EncodingType::LcidBased(_)) && bytes.is_ascii());
    if direct_char && (buffer_length as usize) > bytes.len() {
        // SAFETY: the caller guarantees `target_value_ptr` is null or writable
        // for `buffer_length` bytes and `strlen_or_ind_ptr` null or writable for
        // one `SqlLen`. `buffer_length > bytes.len()` leaves room for the
        // terminator `copy_with_nul` reserves, and both helpers null-check.
        unsafe {
            write_if_some(
                strlen_or_ind_ptr,
                SqlLen::try_from(bytes.len()).unwrap_or(SqlLen::MAX),
            );
            copy_with_nul(target_value_ptr.cast::<u8>(), buffer_length as usize, bytes);
        }
        return true;
    }

    let utf16_ascii = target_type == SQL_C_CHAR
        && matches!(value.encoding_type(), EncodingType::Utf16)
        && bytes.len().is_multiple_of(2)
        && bytes
            .chunks_exact(2)
            .all(|unit| unit[1] == 0 && unit[0].is_ascii());
    let utf16_ascii_len = bytes.len() / 2;
    if utf16_ascii && (buffer_length as usize) > utf16_ascii_len {
        // SAFETY: same caller contract; `buffer_length > utf16_ascii_len` leaves
        // room for the narrowed bytes plus the terminator written below.
        unsafe {
            write_if_some(
                strlen_or_ind_ptr,
                SqlLen::try_from(utf16_ascii_len).unwrap_or(SqlLen::MAX),
            );
            if !target_value_ptr.is_null() {
                let target = target_value_ptr.cast::<u8>();
                for (index, unit) in bytes.chunks_exact(2).enumerate() {
                    target.add(index).write_unaligned(unit[0]);
                }
                target.add(utf16_ascii_len).write_unaligned(0);
            }
        }
        return true;
    }

    let direct_wchar = target_type == SQL_C_WCHAR
        && matches!(value.encoding_type(), EncodingType::Utf16)
        && bytes.len().is_multiple_of(2)
        && std::char::decode_utf16(
            bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])),
        )
        .all(|unit| unit.is_ok());
    let required = bytes.len().saturating_add(std::mem::size_of::<SqlWChar>());
    if !direct_wchar || usize::try_from(buffer_length).map_or(true, |len| len < required) {
        return false;
    }

    // SAFETY: same caller contract; the guard above proved `buffer_length >=
    // bytes.len() + size_of::<SqlWChar>()`, so the copy and its terminator both
    // fit. `bytes` belongs to `value`, which the caller guarantees does not
    // alias the application buffer.
    unsafe {
        write_if_some(
            strlen_or_ind_ptr,
            SqlLen::try_from(bytes.len()).unwrap_or(SqlLen::MAX),
        );
        if !target_value_ptr.is_null() {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                target_value_ptr.cast::<u8>(),
                bytes.len(),
            );
            target_value_ptr
                .cast::<u8>()
                .add(bytes.len())
                .cast::<SqlWChar>()
                .write_unaligned(0);
        }
    }
    true
}

/// Renders a buffered decimal or numeric into the application buffer through a
/// stack scratch array, avoiding the `String` the general path would allocate.
///
/// Returns `false` when the value is not decimal or does not fit, leaving the
/// application buffer untouched so the caller can fall back.
///
/// # Safety
/// - `target_value_ptr`, if non-null, must be writable for `buffer_length` bytes.
/// - `strlen_or_ind_ptr`, if non-null, must be writable for one `SqlLen`.
/// - Neither pointer may alias `value`.
#[inline(never)]
unsafe fn try_write_complete_buffered_decimal(
    value: &ColumnValues,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> bool {
    let parts = match value {
        ColumnValues::Decimal(parts) | ColumnValues::Numeric(parts) => parts,
        _ => return false,
    };
    let mut formatted = [0_u8; DECIMAL_STR_LEN];
    let rendered = parts.format_into(&mut formatted).as_bytes();
    let (prefix, rest): (&[u8], &[u8]) = if let Some(rest) = rendered.strip_prefix(b"0.") {
        (b".", rest)
    } else if let Some(rest) = rendered.strip_prefix(b"-0.") {
        (b"-.", rest)
    } else {
        (b"", rendered)
    };
    let Some(rendered_len) = prefix.len().checked_add(rest.len()) else {
        return false;
    };
    if (buffer_length as usize) <= rendered_len {
        return false;
    }
    // SAFETY: the caller guarantees `target_value_ptr` is null or writable for
    // `buffer_length` bytes and `strlen_or_ind_ptr` null or writable for one
    // `SqlLen`. `buffer_length > rendered_len` leaves room for the rendered
    // digits plus the terminator.
    unsafe {
        write_if_some(
            strlen_or_ind_ptr,
            SqlLen::try_from(rendered_len).unwrap_or(SqlLen::MAX),
        );
        if !target_value_ptr.is_null() {
            let target = target_value_ptr.cast::<u8>();
            std::ptr::copy_nonoverlapping(prefix.as_ptr(), target, prefix.len());
            std::ptr::copy_nonoverlapping(rest.as_ptr(), target.add(prefix.len()), rest.len());
            target.add(rendered_len).write_unaligned(0);
        }
    }
    true
}

/// Writes a buffered scalar whose stored representation is bit-identical to the
/// requested C type, so no conversion is needed.
///
/// Only exact type pairs match; anything else returns `false` with the
/// application buffer untouched. There is no length parameter because each arm
/// writes a fixed-size value that ODBC defines as needing no buffer length.
///
/// # Safety
/// - `target_value_ptr`, if non-null, must be writable for the size of the C
///   type named by `target_type`.
/// - `strlen_or_ind_ptr`, if non-null, must be writable for one `SqlLen`.
/// - Neither pointer may alias `value`.
#[inline(never)]
unsafe fn try_write_exact_buffered_scalar(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> bool {
    macro_rules! write_exact {
        ($value:expr) => {{
            let value = $value;
            // SAFETY: this arm only runs once `target_type` matched the C type of
            // `value`, which the caller guarantees `target_value_ptr` is sized
            // for; `strlen_or_ind_ptr` is null or writable for one `SqlLen`.
            unsafe {
                write_if_some(target_value_ptr.cast(), value);
                write_if_some(
                    strlen_or_ind_ptr,
                    SqlLen::try_from(std::mem::size_of_val(&value)).unwrap_or(SqlLen::MAX),
                );
            }
            return true;
        }};
    }

    match (value, target_type) {
        (ColumnValues::Bit(value), SQL_C_BIT) => write_exact!(u8::from(*value)),
        (ColumnValues::TinyInt(value), SQL_C_UTINYINT) => write_exact!(*value),
        (ColumnValues::SmallInt(value), SQL_C_SSHORT) => write_exact!(*value),
        (ColumnValues::Int(value), SQL_C_SLONG) => write_exact!(*value),
        (ColumnValues::BigInt(value), SQL_C_SBIGINT) => write_exact!(*value),
        (ColumnValues::Real(value), SQL_C_FLOAT) => write_exact!(*value),
        (ColumnValues::Float(value), SQL_C_DOUBLE) => write_exact!(*value),
        (ColumnValues::Date(value), SQL_C_TYPE_DATE) => {
            let parts = date_parts(value);
            write_exact!(SqlDateStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
            })
        }
        (ColumnValues::Time(value), SQL_C_SS_TIME2) => {
            let parts = time_parts(value);
            write_exact!(SqlSsTime2Struct {
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
            })
        }
        (ColumnValues::DateTime2(value), SQL_C_TYPE_TIMESTAMP) => {
            let parts = datetime2_parts(value);
            write_exact!(SqlTimestampStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
            })
        }
        (ColumnValues::DateTimeOffset(value), SQL_C_SS_TIMESTAMPOFFSET) => {
            let Some(parts) = datetimeoffset_parts(value) else {
                return false;
            };
            write_exact!(SqlSsTimestampoffsetStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
                timezone_hour: parts.tz_hour,
                timezone_minute: parts.tz_minute,
            })
        }
        (ColumnValues::Uuid(value), SQL_C_GUID) => {
            let (data1, data2, data3, data4) = value.as_fields();
            write_exact!(SqlGuid {
                data1,
                data2,
                data3,
                data4: *data4,
            })
        }
        _ => false,
    }
}

/// Delivers a complete buffered UTF-16 PLP value directly as `SQL_C_WCHAR`.
///
/// Returns `None` when the value is not fully buffered or the connection cannot
/// be used synchronously, leaving the caller to resume normal PLP streaming.
/// `Some` means the column was consumed, including SQL NULL (reported through
/// `strlen_or_ind_ptr` and terminated as an empty wide string).
///
/// The caller guarantees room for at least one `SqlWChar` terminator and a
/// non-null indicator pointer before selecting this path.
#[allow(clippy::too_many_arguments)]
fn try_deliver_complete_buffered_unicode_plp(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    col_index: usize,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Option<SqlReturn> {
    let payload_capacity = (buffer_length as usize).saturating_sub(std::mem::size_of::<SqlWChar>());
    let mut payload = [0_u8; 256];
    let out_len = payload_capacity.min(payload.len()) & !1;
    let dbc = stmt.parent_dbc();
    let mut dbc_state = dbc.inner.lock().ok()?;
    if dbc_state
        .active_stmt
        .is_some_and(|busy_stmt| busy_stmt != statement_handle)
    {
        return None;
    }
    let poll = dbc_state
        .client
        .as_mut()?
        .try_read_row_plp_complete(col_index - 1, &mut payload[..out_len])
        .ok()?;
    let value = match poll {
        CursorPoll::Pending => return None,
        CursorPoll::Ready(value) => value,
    };
    dbc_state.active_stmt = Some(statement_handle);
    drop(dbc_state);

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        return Some(SQL_ERROR);
    };
    let rc = if let Some(chunk) = value {
        if !target_value_ptr.is_null() {
            // SAFETY: `chunk.read <= out_len <= buffer_length -
            // size_of::<SqlWChar>()`, so the payload and the terminator that
            // follows it both fit in the application buffer. `payload` is a
            // local array and cannot alias it.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    target_value_ptr.cast::<u8>(),
                    chunk.read,
                );
                target_value_ptr
                    .cast::<u8>()
                    .add(chunk.read)
                    .cast::<SqlWChar>()
                    .write_unaligned(0);
            }
        }
        // SAFETY: the caller selects this path only with a non-null indicator
        // pointer, which is writable for one `SqlLen`.
        unsafe { write_if_some(strlen_or_ind_ptr, chunk.read as SqlLen) };
        SQL_SUCCESS
    } else {
        // SAFETY: as above for the indicator; the caller also guarantees room
        // for at least one `SqlWChar`, so the empty-string terminator fits.
        unsafe {
            write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA);
            if !target_value_ptr.is_null() {
                target_value_ptr.cast::<SqlWChar>().write_unaligned(0);
            }
        }
        SQL_SUCCESS
    };
    stmt_state.current_row_last_col = col_index;
    stmt_state.last_captured = None;
    stmt_state.active_plp = None;
    Some(finish_get_data(
        stmt,
        statement_handle,
        stmt_state,
        col_index,
        rc,
    ))
}

/// After `SQLGetData` finishes delivering `col_index`, peeks one token past
/// the current row if that was the result set's last column — mirroring
/// `fetch_scroll.rs`'s bound-column fetch path. Safe here because every
/// column has now been read (whether via `SQLBindCol` earlier or
/// `SQLGetData` just now), so nothing remains on the wire for this row that
/// a later `SQLGetData` call could still legitimately retrieve. Releases the
/// connection's busy claim immediately if the peek confirms no more rows
/// follow, instead of waiting for this cursor to be explicitly closed
/// (matches msodbcsql's wire-state busy gate; see AB#47508).
///
/// Out of scope for now: a column still mid-PLP-stream (`active_plp` set) is
/// excluded even when `col_index` is the last column, since the stream may
/// not have reached the wire's end yet; the peek only runs once it completes
/// naturally on a later `SQLGetData` call.
///
/// `ready` alone decides whether to peek — there is no separate `rc ==
/// SQL_ERROR` bail. Every `write_captured_column` path that returns
/// `SQL_ERROR` deliberately leaves `current_row_last_col` unadvanced (a
/// truncated read, an unconvertible type, a malformed payload — all keep the
/// column resident and re-readable), so `ready` already reads false for
/// every current error outcome. That also makes this correct for a future
/// error outcome that legitimately does finish delivering the column (e.g. a
/// NULL surfaced through an error indicator): `ready` still reflects the true
/// delivery state instead of a blanket rc check overriding it.
fn finish_get_data(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut stmt_state: MutexGuard<'_, StmtState>,
    col_index: usize,
    rc: SqlReturn,
) -> SqlReturn {
    let ready = stmt_state.current_row_last_col == col_index
        && col_index == stmt_state.column_metadata.len()
        && stmt_state.active_plp.is_none();
    if ready {
        stmt_state.spare_get_data_row = stmt_state.buffered_get_data_row.take();
    }
    drop(stmt_state);
    if !ready {
        return rc;
    }

    let dbc = stmt.parent_dbc();
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        return rc;
    };
    if dbc_state.active_stmt != Some(statement_handle) {
        return rc;
    }
    let Some(client) = dbc_state.client.take() else {
        return rc;
    };
    drop(dbc_state);
    // A row's column was genuinely captured to reach this point (`ready`
    // requires it), so a row was always delivered here — unlike
    // `fetch_scroll.rs`'s zero-row fetch case.
    release_busy_if_row_exhausted(dbc, stmt, statement_handle, client, true);
    rc
}

fn write_captured_column(
    stmt_state: &mut crate::handles::stmt::StmtState,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    // Codepage note: SQL_C_CHAR output is UTF-8, unconditionally. msodbcsql
    // instead converts to the client codepage, which it derives per platform:
    // `GetACP()` on Windows, `nl_langinfo(CODESET)` mapped to a codepage on
    // Linux/macOS, defaulting to UTF-8 (`Sql/Common/include/Localization.hpp`,
    // `LocalizationImpl.hpp`). So the two agree under a UTF-8 locale and
    // diverge under any other -- notably on Windows, where the ANSI codepage is
    // single-byte and unrepresentable characters are best-fit away. This
    // driver is codepage-agnostic by design; callers wanting the client
    // codepage must transcode. SQL_C_WCHAR is UTF-16LE on both drivers.

    // C-type legality (HY003) is settled by the caller before dispatch; what is
    // left here is whether this driver can deliver a value into a valid target.

    if stmt_state.last_captured.is_none() {
        post_sql_error(
            stmt_state,
            SQLSTATE_24000,
            0,
            "Requested column is not available in the current row",
        );
        return SQL_ERROR;
    }

    // Output buffer capacity in element units (u8 for SQL_C_CHAR, SqlWChar for
    // SQL_C_WCHAR). buffer_length is always in bytes per the ODBC spec.
    let buf_elements = if target_type == SQL_C_WCHAR {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };

    // NULL is answered before the conversion question is asked, because there is
    // no value to convert: the indicator carries the whole result.
    if matches!(
        stmt_state.last_captured.as_ref(),
        Some((_, ColumnValues::Null))
    ) {
        // ODBC spec: "If [StrLen_or_IndPtr] is a null pointer, no length or
        // indicator value is returned. This returns an error when the data
        // being fetched is NULL" (SQLSTATE 22002). There is nowhere to report
        // the NULL, and leaving the target buffer untouched would hand the
        // caller whatever was already there. Mirrors the identical check in
        // fetch_scroll.rs::deliver_bound for bound columns. Leave the value
        // resident so a retry with a real indicator can still succeed.
        if strlen_or_ind_ptr.is_null() {
            post_diag(stmt_state, ERR_INDICATOR_REQUIRED);
            return SQL_ERROR;
        }
        unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
        // Only character targets get a terminator; a fixed-width target's
        // buffer is left untouched on NULL.
        if target_type == SQL_C_WCHAR {
            unsafe {
                copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
            }
        } else if target_type == SQL_C_CHAR {
            unsafe {
                copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]);
            }
        }
        stmt_state.last_captured = None;
        stmt_state.partial_text_offset = None;
        stmt_state.current_row_last_col = col_index;
        return SQL_SUCCESS;
    }

    // A zero-length SQL_C_BINARY read is a length probe rather than a data read;
    // mssql-python issues one per sql_variant column to expose the underlying
    // type to SQLColAttribute. Binary data delivery is still unimplemented
    // (AB#47239).
    let binary_probe = target_type == SQL_C_BINARY && buffer_length == 0;
    let typed_target = is_typed_c_target(target_type);
    let deliverable_target =
        typed_target || target_type == SQL_C_CHAR || target_type == SQL_C_WCHAR || binary_probe;

    // Rejected before the value is borrowed, so it stays resident and the caller
    // can retry with a target this driver does deliver.
    if !deliverable_target {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
        );
        return SQL_ERROR;
    }

    // Borrow — not take — so a partial (truncated) read or an unconvertible
    // column type leaves the value resident and re-readable on the next call.
    // The presence check above already returned 24000; repeating it here keeps
    // the failure a diagnostic rather than a panic across the FFI boundary.
    let Some((_, value)) = stmt_state.last_captured.as_ref() else {
        post_sql_error(
            stmt_state,
            SQLSTATE_24000,
            0,
            "Requested column is not available in the current row",
        );
        return SQL_ERROR;
    };

    // Fixed / typed C targets deliver the whole value in one call through the
    // shared conversion core; only the character targets chunk.
    if binary_probe {
        // Report what is available and leave the value resident — the caller
        // reads it for real on a following call.
        unsafe { write_if_some(strlen_or_ind_ptr, binary_length(value)) };
        return SQL_SUCCESS;
    }

    if typed_target {
        let converted =
            unsafe { convert_typed_c(value, target_type, target_value_ptr, strlen_or_ind_ptr) };
        let rc = finish_typed_conv(stmt_state, converted);
        if rc != SQL_ERROR {
            stmt_state.current_row_last_col = col_index;
            stmt_state.last_captured = None;
            stmt_state.partial_text_offset = None;
        }
        return rc;
    }

    let offset = stmt_state
        .partial_text_offset
        .filter(|(c, _)| *c == col_index)
        .map(|(_, o)| o)
        .unwrap_or(0);
    let direct_validated = stmt_state.direct_text_target == Some((col_index, target_type));
    // SAFETY: `buf_elements` is `buffer_length` converted to the element unit of
    // `target_type`, so `target_value_ptr` is null or writable for that many
    // elements; `strlen_or_ind_ptr` is null or writable for one `SqlLen`.
    // `value` is a captured column that neither pointer aliases.
    if let Some((truncated, consumed, remaining)) = unsafe {
        try_write_direct_captured_string_chunk(
            value,
            target_type,
            target_value_ptr,
            buf_elements,
            strlen_or_ind_ptr,
            offset,
            direct_validated,
        )
    } {
        let rc = if truncated {
            post_diag(stmt_state, WARN_STRING_TRUNCATION);
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
        if truncated && consumed < remaining {
            stmt_state.partial_text_offset = Some((col_index, offset + consumed));
            stmt_state.direct_text_target = Some((col_index, target_type));
        } else {
            stmt_state.current_row_last_col = col_index;
            retain_completed_buffered_value(stmt_state, col_index);
            stmt_state.partial_text_offset = None;
            stmt_state.direct_text_target = None;
        }
        return rc;
    }
    stmt_state.direct_text_target = None;

    let as_text = match column_value_to_text(value) {
        Ok(t) => t,
        Err(TextError::Malformed) => {
            // Leave the value resident so the column stays re-readable. There is no
            // raw-bytes fallback today: SQL_C_BINARY only answers the zero-length
            // probe, it does not deliver data (AB#47239).
            error!("SQLGetData: column payload could not be decoded as text");
            post_diag(stmt_state, ERR_INVALID_CHARACTER_VALUE);
            return SQL_ERROR;
        }
        Err(TextError::Unsupported) => {
            // Unconvertible *column* type: HYC00 is a soft failure. Leave the value
            // in place (do not consume) so a retry with another C type can work.
            post_sql_error(
                stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            );
            return SQL_ERROR;
        }
    };
    // `value` borrow ends here — `as_text` is owned.

    // Resume from where a prior truncated read of this column left off. The
    // offset unit matches the target C type (bytes for CHAR, UTF-16 code units
    // for WCHAR); a single column's chunk loop uses one target type throughout.
    let (rc, consumed, remaining) = if target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = as_text.encode_utf16().skip(offset).collect();
        let consumed = buf_elements.saturating_sub(1).min(utf16.len());
        let rc = write_string_result(
            stmt_state,
            &utf16,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
        );
        (rc, consumed, utf16.len())
    } else {
        let all = as_text.as_bytes();
        let bytes = &all[offset.min(all.len())..];
        let consumed = buf_elements.saturating_sub(1).min(bytes.len());
        let rc = write_string_result(
            stmt_state,
            bytes,
            target_value_ptr as *mut u8,
            buf_elements,
            strlen_or_ind_ptr,
        );
        (rc, consumed, bytes.len())
    };

    if rc == SQL_SUCCESS_WITH_INFO && consumed < remaining {
        // Truncated: remember where to resume and keep the column addressable —
        // do NOT mark it consumed, so the next SQLGetData continues it.
        stmt_state.partial_text_offset = Some((col_index, offset + consumed));
    } else if rc != SQL_ERROR {
        // Fully delivered: the column is done.
        stmt_state.current_row_last_col = col_index;
        stmt_state.last_captured = None;
        stmt_state.partial_text_offset = None;
    }
    rc
}

/// Delivers one chunk of an already-buffered string from `offset` onward without
/// re-decoding it. The general path rebuilds and re-transcodes the whole value on
/// every call, which is quadratic across a chunked `SQLGetData` loop.
///
/// `validated` reports that eligibility was already established for this
/// column and target type, so the encoding scan is skipped on later chunks;
/// see `StmtState::direct_text_target`.
///
/// Returns `None` when the value cannot be delivered verbatim, leaving the
/// application buffer untouched. On success returns whether the chunk was
/// truncated, how many elements were consumed, and how many remained.
///
/// # Safety
/// - `target_value_ptr`, if non-null, must be writable for `buf_elements`
///   elements: bytes for `SQL_C_CHAR`, `SqlWChar`s for `SQL_C_WCHAR`. Note this
///   is an element count, unlike the byte-count `buffer_length` elsewhere.
/// - `strlen_or_ind_ptr`, if non-null, must be writable for one `SqlLen`.
/// - Neither pointer may alias `value`.
unsafe fn try_write_direct_captured_string_chunk(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
    offset: usize,
    validated: bool,
) -> Option<(bool, usize, usize)> {
    let ColumnValues::String(value) = value else {
        return None;
    };
    let bytes = &value.bytes;
    if target_type == SQL_C_CHAR {
        let direct = match value.encoding_type() {
            EncodingType::Utf8 => validated || std::str::from_utf8(bytes).is_ok(),
            EncodingType::LcidBased(_) => validated || bytes.is_ascii(),
            _ => false,
        };
        if !direct {
            return None;
        }
        let remaining = bytes.get(offset.min(bytes.len())..)?;
        // SAFETY: the caller guarantees `strlen_or_ind_ptr` is null or writable
        // for one `SqlLen`.
        unsafe {
            write_if_some(
                strlen_or_ind_ptr,
                SqlLen::try_from(remaining.len()).unwrap_or(SqlLen::MAX),
            );
        }
        let consumed = buf_elements.saturating_sub(1).min(remaining.len());
        // SAFETY: for `SQL_C_CHAR` the element unit is the byte, so the caller's
        // contract makes `target_value_ptr` null or writable for `buf_elements`
        // bytes — exactly the bound `copy_with_nul` respects, terminator
        // included. `remaining` borrows `value`, which cannot alias the buffer.
        let truncated =
            unsafe { copy_with_nul(target_value_ptr.cast::<u8>(), buf_elements, remaining) };
        return Some((truncated, consumed, remaining.len()));
    }

    if target_type != SQL_C_WCHAR
        || !matches!(value.encoding_type(), EncodingType::Utf16)
        || !bytes.len().is_multiple_of(2)
        || !validated
            && !std::char::decode_utf16(
                bytes
                    .chunks_exact(2)
                    .map(|unit| u16::from_le_bytes([unit[0], unit[1]])),
            )
            .all(|unit| unit.is_ok())
    {
        return None;
    }

    let total_units = bytes.len() / 2;
    let offset = offset.min(total_units);
    let remaining_units = total_units - offset;
    let remaining_bytes = remaining_units.saturating_mul(std::mem::size_of::<SqlWChar>());
    // SAFETY: the caller guarantees `strlen_or_ind_ptr` is null or writable for
    // one `SqlLen`.
    unsafe {
        write_if_some(
            strlen_or_ind_ptr,
            SqlLen::try_from(remaining_bytes).unwrap_or(SqlLen::MAX),
        );
    }
    let consumed = buf_elements.saturating_sub(1).min(remaining_units);
    let target = target_value_ptr.cast::<SqlWChar>();
    let truncated = if target.is_null() {
        false
    } else if buf_elements == 0 {
        remaining_units != 0
    } else {
        let start = offset.saturating_mul(2);
        for (index, unit) in bytes[start..].chunks_exact(2).take(consumed).enumerate() {
            // SAFETY: `target` is non-null and, for `SQL_C_WCHAR`, the caller's
            // contract makes it writable for `buf_elements` `SqlWChar`s;
            // `index < consumed <= buf_elements - 1`.
            unsafe {
                target
                    .add(index)
                    .write_unaligned(u16::from_le_bytes([unit[0], unit[1]]));
            }
        }
        // SAFETY: same contract; `consumed <= buf_elements - 1`, so the
        // terminator stays within the buffer even when truncating.
        unsafe { target.add(consumed).write_unaligned(0) };
        consumed < remaining_units
    };
    Some((truncated, consumed, remaining_units))
}

/// Returns a fully delivered captured value to its buffered-row slot.
///
/// Keeping the value allocation in the row allows the next fetch to recycle it;
/// values not captured from `col_index` are discarded instead.
fn retain_completed_buffered_value(stmt_state: &mut StmtState, col_index: usize) {
    let Some((captured_column, value)) = stmt_state.last_captured.take() else {
        return;
    };
    if captured_column != col_index {
        return;
    }
    let Some(slot_index) = col_index.checked_sub(1) else {
        return;
    };
    let Some(slot) = stmt_state
        .buffered_get_data_row
        .as_mut()
        .and_then(|row| row.values.get_mut(slot_index))
    else {
        return;
    };
    *slot = Some(value);
}

fn resume_row_to_column(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    column_number: usize,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    {
        // validate row is positioned before resuming
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while preparing row resume");
            return SQL_ERROR;
        };
        if !stmt_state.row_positioned {
            // Unreachable today — the `!row_positioned` check in the caller
            // fires first — but return a diagnostic rather than a bare
            // SQL_ERROR so a future guard reorder can't yield an empty
            // SQLGetDiagRec.
            let mut stmt_state = stmt_state;
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_24000,
                0,
                "Statement is not positioned on a row",
            );
            return SQL_ERROR;
        }
    };

    let target = column_number - 1; // 0-based
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLGetData: dbc mutex poisoned while resuming row");
        return SQL_ERROR;
    };

    if let Some(busy_stmt) = dbc_state.active_stmt
        && busy_stmt != statement_handle
    {
        drop(dbc_state);
        if let Ok(mut stmt_state) = stmt.inner.lock() {
            post_diag(&mut stmt_state, ERR_CONNECTION_BUSY);
        }
        return SQL_ERROR;
    }

    let cursor_poll = {
        let Some(client) = dbc_state.client.as_mut() else {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };
        client.try_read_row_column(target)
    };

    dbc_state.active_stmt = Some(statement_handle);
    let cursor_result = match cursor_poll {
        Ok(CursorPoll::Ready(column)) => {
            drop(dbc_state);
            Ok(column)
        }
        Err(error) => {
            drop(dbc_state);
            Err(error)
        }
        Ok(CursorPoll::Pending) => {
            let Some(mut client) = dbc_state.client.take() else {
                drop(dbc_state);
                if let Ok(mut stmt_state) = stmt.inner.lock() {
                    post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
                }
                return SQL_ERROR;
            };
            drop(dbc_state);

            let result = dbc.runtime.block_on(client.read_row_column(target));

            let Ok(mut dbc_state) = dbc.inner.lock() else {
                error!("SQLGetData: dbc mutex poisoned after row resume");
                return SQL_ERROR;
            };
            dbc_state.client = Some(client);
            dbc_state.active_stmt = Some(statement_handle);
            drop(dbc_state);
            result
        }
    };

    apply_cursor_result(stmt, column_number, cursor_result)
}

/// Applies one TDS cursor result to the statement's `SQLGetData` state.
///
/// Materialized values are captured for conversion, PLP values leave the
/// transport stream active, and terminal/error states update diagnostics and
/// row-exhaustion bookkeeping.
fn apply_cursor_result(
    stmt: &StmtHandle,
    column_number: usize,
    cursor_result: TdsResult<CursorColumn>,
) -> SqlReturn {
    match cursor_result {
        Ok(CursorColumn::Value {
            value,
            variant_base,
        }) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.last_captured = Some((column_number, value));
                stmt_state.last_variant_base = variant_base.map(|base| (column_number, base));
                stmt_state.row_exhausted = false;
                stmt_state.partial_text_offset = None;
                return SQL_SUCCESS;
            }
            SQL_ERROR
        }
        Ok(CursorColumn::PlpStreaming { .. }) => {
            // Target is a PLP column: leave last_captured empty so the caller
            // switches to chunked streaming via stream_active_plp_chunk.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.last_captured = None;
                stmt_state.row_exhausted = false;
                return SQL_SUCCESS;
            }
            SQL_ERROR
        }
        Ok(CursorColumn::AlreadyConsumed) => {
            // Forward-only violation. The caller's own last-column guard should
            // catch this first; treat any residual case as no-data.
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.last_captured = None;
                post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            }
            SQL_ERROR
        }
        Ok(CursorColumn::RowEnded) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.last_captured = None;
                stmt_state.row_exhausted = true;
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_24000,
                    0,
                    "Result set exhausted while resuming current row",
                );
            }
            SQL_ERROR
        }
        Err(e) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.reset_row_stream();
                stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
            }
            SQL_ERROR
        }
    }
}

/// Reads and returns one SQLGetData chunk directly from the active PLP stream.
///
/// Small known-length values may carry their remaining wire bytes in the
/// statement after the first async read. Larger values continue to stream
/// directly from the TDS client.
#[allow(clippy::too_many_arguments)]
fn stream_active_plp_chunk<'a>(
    stmt: &'a StmtHandle,
    statement_handle: SqlHandle,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
    starting_new_stream: bool,
    prepared_stream: Option<(PlpEncoding, usize, bool)>,
    mut retained_stmt_state: Option<MutexGuard<'a, StmtState>>,
) -> SqlReturn {
    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        if let Some(mut state) = retained_stmt_state.take() {
            post_sql_error(
                &mut state,
                SQLSTATE_HYC00,
                0,
                "Target type not yet implemented",
            );
        } else if let Ok(mut state) = stmt.inner.lock() {
            post_sql_error(
                &mut state,
                SQLSTATE_HYC00,
                0,
                "Target type not yet implemented",
            );
        }
        return SQL_ERROR;
    }

    let (plp_encoding, widen_carry_len) = if let Some((encoding, widen_carry_len, widening_ready)) =
        prepared_stream
    {
        let compatible = match (target_type, encoding) {
            (SQL_C_WCHAR, PlpEncoding::Utf16Text) => true,
            (SQL_C_WCHAR, PlpEncoding::SingleByteText | PlpEncoding::Utf8Text) => widening_ready,
            (
                SQL_C_CHAR,
                PlpEncoding::SingleByteText | PlpEncoding::Utf8Text | PlpEncoding::Utf16Text,
            ) => true,
            _ => false,
        };
        if !compatible {
            if let Some(mut stmt_state) = retained_stmt_state.take() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HYC00,
                    0,
                    "Target type not yet implemented for this column",
                );
            } else if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HYC00,
                    0,
                    "Target type not yet implemented for this column",
                );
            }
            return SQL_ERROR;
        }
        (Some(encoding), widen_carry_len)
    } else {
        let mut stmt_state = match retained_stmt_state.take() {
            Some(state) => state,
            None => {
                let Ok(state) = stmt.inner.lock() else {
                    error!("SQLGetData: stmt mutex poisoned while preparing PLP stream read");
                    return SQL_ERROR;
                };
                state
            }
        };

        if starting_new_stream {
            let column_meta = stmt_state.column_metadata.get(col_index - 1);
            let encoding = column_meta
                .and_then(|m| m.plp_encoding())
                .unwrap_or(PlpEncoding::SingleByteText);
            // Narrow text delivered as SQL_C_WCHAR is decoded through the
            // column's own collation, matching what the non-PLP path already
            // does via `SqlString::to_utf8_string`. Built once per stream so the
            // decoder can carry a character split across a chunk boundary.
            //
            // The encoding is derived here rather than through
            // `get_encoding_type`, which unwraps the collation and would panic
            // on a `json` column (UTF-8 on the wire, no collation) — a panic
            // across the FFI boundary is UB.
            let narrow_to_wide = if target_type == SQL_C_WCHAR {
                let encoder = match encoding {
                    // json is UTF-8 on the wire and carries no collation.
                    PlpEncoding::Utf8Text => Some(encoding_rs::UTF_8),
                    PlpEncoding::SingleByteText => column_meta
                        .and_then(|m| m.get_collation())
                        .and_then(|collation| {
                            if collation.utf8() {
                                Some(encoding_rs::UTF_8)
                            } else {
                                EncodingType::LcidBased(collation).encoding()
                            }
                        }),
                    _ => None,
                };
                encoder.map(|e| e.new_decoder_without_bom_handling())
            } else {
                None
            };
            stmt_state.active_plp = Some(ActivePlpStream::new(col_index, encoding, narrow_to_wide));
            stmt_state.current_row_last_col = col_index;
        }

        if stmt_state
            .active_plp
            .as_ref()
            .is_none_or(|s| s.column != col_index)
        {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_24000,
                0,
                "No active PLP stream for this column",
            );
            return SQL_ERROR;
        }

        // Supported text deliveries:
        //   SQL_C_WCHAR  <- nvarchar(max)/xml, already UTF-16LE on the wire
        //   SQL_C_WCHAR  <- varchar(max)/json, widened through the column's
        //                   collation (or UTF-8 for json, which has none)
        //   SQL_C_CHAR   <- any of the three, as UTF-8
        // Binary columns have no delivery path yet and return HYC00
        // (AB#47239).
        //
        // Codepage note: as in the non-PLP path, SQL_C_CHAR output is UTF-8
        // unconditionally, where msodbcsql converts to the client codepage it
        // derives from the platform -- so the two agree under a UTF-8 locale and
        // diverge under any other. SQL_C_WCHAR is UTF-16LE on both drivers.
        let stream = stmt_state.active_plp.as_ref();
        let encoding = stream.map(|s| s.encoding);
        // A narrow column can only be widened when its collation resolved to a
        // concrete encoding; without one there is nothing to decode through.
        let widening_ready = stream.is_some_and(|s| s.narrow_to_wide.is_some());
        let compatible = match (target_type, encoding) {
            (SQL_C_WCHAR, Some(PlpEncoding::Utf16Text)) => true,
            (SQL_C_WCHAR, Some(PlpEncoding::SingleByteText | PlpEncoding::Utf8Text)) => {
                widening_ready
            }
            (
                SQL_C_CHAR,
                Some(PlpEncoding::SingleByteText | PlpEncoding::Utf8Text | PlpEncoding::Utf16Text),
            ) => true,
            _ => false,
        };
        if !compatible {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Target type not yet implemented for this column",
            );
            return SQL_ERROR;
        }
        let stream_state = (
            stream.map(|s| s.encoding),
            stream.map_or(0, |s| s.pending_units.len()),
        );
        retained_stmt_state = Some(stmt_state);
        stream_state
    };
    let is_unicode_plp = matches!(plp_encoding, Some(PlpEncoding::Utf16Text));
    // SQL_C_CHAR delivery of a UTF-16 PLP column must transcode on the fly.
    let transcode_utf16_to_utf8 = target_type == SQL_C_CHAR && is_unicode_plp;
    // SQL_C_WCHAR delivery of a narrow (codepage or UTF-8) PLP column must
    // widen on the fly. Mirrors the compatibility gate above so the two cannot
    // drift: a Binary column never reaches here.
    let widen_narrow_to_utf16 = target_type == SQL_C_WCHAR
        && matches!(
            plp_encoding,
            Some(PlpEncoding::SingleByteText | PlpEncoding::Utf8Text)
        );

    // Room the caller left for payload, once the terminator every character
    // target needs is set aside.
    let terminator_bytes = if target_type == SQL_C_WCHAR {
        std::mem::size_of::<SqlWChar>()
    } else {
        1
    };
    let payload_capacity = (buffer_length as usize).saturating_sub(terminator_bytes);
    let widen_out_units = if widen_narrow_to_utf16 {
        payload_capacity / std::mem::size_of::<SqlWChar>()
    } else {
        usize::MAX
    };
    let max_read = if widen_narrow_to_utf16 {
        // Wire bytes in, UTF-16 code units out, so the caller's capacity does
        // not bound the read directly.
        //
        // With nothing carried over, read at least the longest single-character
        // sequence any reachable encoding produces (4 bytes) so a chunk always
        // completes at least one character while data remains — otherwise a call
        // could consume input, emit nothing, and still report truncation, which
        // an application sees as a stream that never advances.
        //
        // Once the carry alone can already fill the caller's buffer, drop to one
        // byte: the call is satisfied either way, and reading a full chunk every
        // time would grow the carry without bound when the buffer only has room
        // for a character or two.
        if widen_out_units == 0 {
            0
        } else if widen_carry_len >= widen_out_units {
            1
        } else {
            widen_out_units.max(4)
        }
    } else if target_type == SQL_C_WCHAR {
        // Whole UTF-16 code units only.
        payload_capacity & !1
    } else if transcode_utf16_to_utf8 {
        // One BMP UTF-16 code unit expands to at most 3 UTF-8 bytes, so read at
        // most (cap / 3) code units per chunk. Keeping the byte count even means
        // a code unit is never split mid-read; surrogate pairs that straddle a
        // chunk boundary are carried explicitly. This conservative sizing
        // guarantees the transcoded output always fits the caller's buffer.
        ((payload_capacity / 3) * 2) & !1
    } else {
        payload_capacity
    };

    // A buffer with no payload room at all is a length probe: the application
    // asking how much is there before sizing a real one. `SQLDescribeCol`
    // reports ColumnSize 0 for a MAX column, so a caller sizing as
    // `(ColumnSize + 1) * sizeof(SQLWCHAR)` arrives with 2 bytes. The read
    // consumes nothing (`PlpColumnStream::read_into` returns early on an empty
    // buffer), so the value stays resident while the terminator, the bytes
    // available and 01004 are reported as usual. msodbcsql answers this shape
    // the same way.
    //
    // Payload room too small to carry one whole character is a different case:
    // the buffer is not a probe, and the conservative sizing above cannot use
    // the room it has, so every retry would report truncation without
    // consuming anything and an application looping on an unchanged buffer
    // would never terminate. That stays HY090.
    //
    // Reached by a SQL_C_CHAR buffer of 2-3 bytes transcoding from UTF-16,
    // where one character needs up to 3, and by a SQL_C_WCHAR buffer of 1 or 3
    // bytes: one cannot hold even the terminator, the other has a spare byte
    // that no whole code unit fits in. msodbcsql instead delivers one byte per
    // call here; matching that needs an unflushed-tail buffer in
    // `ActivePlpStream` and is tracked separately.
    //
    // A probe is exactly two shapes: a zero-length buffer, and one sized for
    // the terminator alone. Everything else that cannot make progress is an
    // error, on the widening path as much as anywhere else -- widening sizes
    // its read from output units rather than byte capacity, so its own
    // zero-progress shapes have to be spelled out rather than inferred from
    // `max_read`.
    let is_length_probe = buffer_length == 0 || buffer_length as usize == terminator_bytes;
    let makes_no_progress = if widen_narrow_to_utf16 {
        widen_out_units == 0
    } else {
        max_read == 0
    };
    if makes_no_progress && !is_length_probe {
        if let Some(mut state) = retained_stmt_state.take() {
            post_sql_error(
                &mut state,
                SQLSTATE_HY090,
                0,
                "Buffer length too small to hold a single character and null terminator",
            );
        } else if let Ok(mut s) = stmt.inner.lock() {
            post_sql_error(
                &mut s,
                SQLSTATE_HY090,
                0,
                "Buffer length too small to hold a single character and null terminator",
            );
        }
        return SQL_ERROR;
    }

    let direct_wire_output = max_read > 0
        && !target_value_ptr.is_null()
        && matches!(
            (target_type, plp_encoding),
            (SQL_C_WCHAR, Some(PlpEncoding::Utf16Text))
                | (
                    SQL_C_CHAR,
                    Some(PlpEncoding::SingleByteText | PlpEncoding::Utf8Text)
                )
        );
    let mut inline_payload = [0_u8; 256];
    let mut heap_payload = Vec::new();
    let payload = if direct_wire_output {
        // The application owns this writable buffer for the duration of the ODBC
        // call. Initialize it before forming a byte slice because ODBC output
        // buffers may contain uninitialized storage; `max_read` reserves the
        // required terminator bytes.
        //
        // SAFETY: `direct_wire_output` required a non-null `target_value_ptr`,
        // which the DM guarantees is writable for `buffer_length` bytes, and
        // `max_read <= buffer_length - terminator_bytes`. The zeroing above
        // initializes every byte the slice exposes, and the slice is the only
        // live reference to that range for its lifetime.
        unsafe {
            std::ptr::write_bytes(target_value_ptr.cast::<u8>(), 0, max_read);
            std::slice::from_raw_parts_mut(target_value_ptr.cast::<u8>(), max_read)
        }
    } else if max_read <= inline_payload.len() {
        &mut inline_payload[..max_read]
    } else {
        heap_payload.resize(max_read, 0);
        heap_payload.as_mut_slice()
    };
    let (prefetch_error, prefetched_read) = if let Some(stmt_state) = retained_stmt_state.as_mut() {
        if let Some(stream) = stmt_state.active_plp.as_mut() {
            (
                stream.take_prefetch_error(),
                stream.read_prefetched_wire(payload),
            )
        } else {
            (None, None)
        }
    } else {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while reading prefetched PLP bytes");
            return SQL_ERROR;
        };
        if let Some(stream) = stmt_state.active_plp.as_mut() {
            (
                stream.take_prefetch_error(),
                stream.read_prefetched_wire(payload),
            )
        } else {
            (None, None)
        }
    };

    let read_result = if let Some(error) = prefetch_error {
        Err(error)
    } else if let Some((read, reached_end, known_total, total_read)) = prefetched_read {
        Ok(PlpChunk {
            read,
            reached_end,
            known_total,
            total_read,
        })
    } else {
        drop(retained_stmt_state.take());
        let dbc = stmt.parent_dbc();
        let mut dbc_state = match dbc.inner.lock() {
            Ok(state) => state,
            Err(_) => {
                error!("SQLGetData: dbc mutex poisoned while reading PLP stream");
                return SQL_ERROR;
            }
        };
        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut s) = stmt.inner.lock() {
                post_diag(&mut s, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }
        let buffered_read = {
            let Some(client) = dbc_state.client.as_mut() else {
                drop(dbc_state);
                if let Ok(mut s) = stmt.inner.lock() {
                    post_diag(&mut s, ERR_NO_ACTIVE_TDS_CLIENT);
                }
                return SQL_ERROR;
            };
            client.try_read_active_plp_chunk(payload)
        };
        dbc_state.active_stmt = Some(statement_handle);
        match buffered_read {
            Ok(CursorPoll::Ready(chunk)) => {
                drop(dbc_state);
                Ok(chunk)
            }
            Err(error) => {
                drop(dbc_state);
                Err(error)
            }
            Ok(CursorPoll::Pending) => {
                let Some(mut client) = dbc_state.client.take() else {
                    drop(dbc_state);
                    if let Ok(mut s) = stmt.inner.lock() {
                        post_diag(&mut s, ERR_NO_ACTIVE_TDS_CLIENT);
                    }
                    return SQL_ERROR;
                };
                drop(dbc_state);
                let mut prefetch_scratch = {
                    let Ok(mut stmt_state) = stmt.inner.lock() else {
                        error!("SQLGetData: stmt mutex poisoned while taking PLP prefetch buffer");
                        return SQL_ERROR;
                    };
                    std::mem::take(&mut stmt_state.plp_prefetch_scratch)
                };
                let result = dbc.runtime.block_on(async {
                    let chunk = client.read_active_plp_chunk(payload).await?;
                    let remaining = chunk
                        .known_total
                        .and_then(|total| total.checked_sub(chunk.total_read as u64))
                        .and_then(|bytes| usize::try_from(bytes).ok());
                    let prefetch_len = remaining.filter(|remaining| {
                        direct_wire_output
                            && *remaining > 0
                            && *remaining <= MAX_PLP_PREFETCH_BYTES
                            && !chunk.reached_end
                    });
                    let Some(prefetch_len) = prefetch_len else {
                        return Ok((chunk, None, Some(prefetch_scratch), None));
                    };

                    prefetch_scratch.resize(prefetch_len, 0);
                    match client.read_active_plp_chunk(&mut prefetch_scratch).await {
                        Ok(tail) => {
                            let carry = (prefetch_scratch, tail, chunk.total_read);
                            Ok((chunk, Some(carry), None, None))
                        }
                        Err(error) => Ok((chunk, None, Some(prefetch_scratch), Some(error))),
                    }
                });
                let Ok(mut dbc_state) = dbc.inner.lock() else {
                    error!("SQLGetData: dbc mutex poisoned after PLP read");
                    return SQL_ERROR;
                };
                dbc_state.client = Some(client);
                dbc_state.active_stmt = Some(statement_handle);
                drop(dbc_state);
                match result {
                    Ok((chunk, carry, unused_scratch, prefetch_error)) => {
                        let Ok(mut stmt_state) = stmt.inner.lock() else {
                            error!(
                                "SQLGetData: stmt mutex poisoned while saving PLP prefetch buffer"
                            );
                            return SQL_ERROR;
                        };
                        if let Some((bytes, tail, total_read_before)) = carry {
                            let Some(stream) = stmt_state.active_plp.as_mut() else {
                                error!(
                                    "SQLGetData: PLP stream vanished while saving prefetched bytes"
                                );
                                return SQL_ERROR;
                            };
                            stream.set_prefetched_wire(
                                bytes,
                                tail.read,
                                total_read_before,
                                tail.known_total,
                                tail.reached_end,
                            );
                        } else if let Some(buffer) = unused_scratch {
                            stmt_state.plp_prefetch_scratch = buffer;
                        }
                        if let Some(error) = prefetch_error {
                            let Some(stream) = stmt_state.active_plp.as_mut() else {
                                error!(
                                    "SQLGetData: PLP stream vanished while saving prefetch error"
                                );
                                return SQL_ERROR;
                            };
                            stream.set_prefetch_error(error);
                        }
                        Ok(chunk)
                    }
                    Err(error) => Err(error),
                }
            }
        }
    };

    let PlpChunk {
        read,
        reached_end,
        known_total,
        total_read,
    } = match read_result {
        Ok(chunk) => chunk,
        Err(e) => {
            if let Some(mut s) = retained_stmt_state.take() {
                s.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut s, &e, SQLSTATE_HY000);
            } else if let Ok(mut s) = stmt.inner.lock() {
                s.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut s, &e, SQLSTATE_HY000);
            }
            return SQL_ERROR;
        }
    };

    if widen_narrow_to_utf16 || transcode_utf16_to_utf8 {
        drop(retained_stmt_state.take());
    }

    if widen_narrow_to_utf16 {
        // varchar(max)/json wire bytes are narrow; widen them to UTF-16LE for
        // SQL_C_WCHAR. The decoder lives in the stream state so a character
        // split across a chunk boundary is carried rather than corrupted.
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        let emitted = {
            let Ok(mut ss) = stmt.inner.lock() else {
                return SQL_ERROR;
            };
            let Some(stream) = ss.active_plp.as_mut() else {
                error!("SQLGetData: narrow PLP stream vanished mid-call");
                return SQL_ERROR;
            };
            let ActivePlpStream {
                narrow_to_wide,
                pending_units,
                ..
            } = stream;
            let Some(decoder) = narrow_to_wide.as_mut() else {
                error!("SQLGetData: narrow PLP stream lost its decoder");
                return SQL_ERROR;
            };
            let emit = widen_into_pending(
                decoder,
                pending_units,
                &payload[..read],
                reached_end,
                widen_out_units,
            );
            unsafe {
                copy_with_nul(
                    target_value_ptr as *mut SqlWChar,
                    buf_elements,
                    &pending_units[..emit],
                );
                write_if_some(
                    strlen_or_ind_ptr,
                    (emit * std::mem::size_of::<SqlWChar>()) as SqlLen,
                );
            }
            pending_units.drain(..emit);
            emit
        };
        // Reading at least one whole character's worth of bytes, plus the carry,
        // is what keeps every truncated call carrying a payload. A zero-length
        // one would look to an application like a stream that stopped advancing.
        // The exception is a caller whose buffer has no payload room at all —
        // that is a length probe, and answering it with an empty chunk is the
        // point.
        debug_assert!(
            emitted > 0 || reached_end || widen_out_units == 0,
            "narrow PLP widening made no forward progress"
        );
    } else if target_type == SQL_C_WCHAR {
        let usable = read & !1;
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        if buf_elements > 0 && !target_value_ptr.is_null() {
            let copy_bytes = usable.min((buf_elements - 1) * std::mem::size_of::<SqlWChar>());
            // SAFETY: `target_value_ptr` is non-null and writable for
            // `buffer_length` bytes, and `copy_bytes <= (buf_elements - 1) *
            // size_of::<SqlWChar>()`, so the payload and the terminator that
            // follows it stay in bounds. The copy is skipped for
            // `direct_wire_output`, where `payload` already aliases this buffer.
            unsafe {
                if !direct_wire_output {
                    std::ptr::copy_nonoverlapping(
                        payload.as_ptr(),
                        target_value_ptr.cast::<u8>(),
                        copy_bytes,
                    );
                }
                target_value_ptr
                    .cast::<u8>()
                    .add(copy_bytes)
                    .cast::<SqlWChar>()
                    .write_unaligned(0);
            }
        }
        // SAFETY: per the SQLGetData contract `strlen_or_ind_ptr` is null or
        // writable for one `SqlLen`.
        unsafe { write_if_some(strlen_or_ind_ptr, usable as SqlLen) };
    } else if transcode_utf16_to_utf8 {
        // NVARCHAR PLP wire bytes are UTF-16LE; transcode to UTF-8 for
        // SQL_C_CHAR, carrying a split code unit or surrogate pair across the
        // chunk boundary so the value is never corrupted.
        let utf8 = {
            let Ok(mut ss) = stmt.inner.lock() else {
                return SQL_ERROR;
            };
            let Some(stream) = ss.active_plp.as_mut() else {
                return SQL_ERROR;
            };
            utf16le_chunk_to_utf8(
                &payload[..read],
                reached_end,
                &mut stream.pending_byte,
                &mut stream.pending_high_surrogate,
            )
        };
        let utf8_bytes = utf8.as_bytes();
        let truncated = unsafe {
            copy_with_nul(
                target_value_ptr as *mut u8,
                buffer_length as usize,
                utf8_bytes,
            )
        };
        // Conservative max_read sizing guarantees the transcoded chunk fits.
        debug_assert!(!truncated, "transcoded PLP chunk overflowed caller buffer");
        unsafe {
            write_if_some(strlen_or_ind_ptr, utf8_bytes.len() as SqlLen);
        }
    } else {
        // SQL_C_CHAR delivery of a non-UTF-16 text PLP column: the wire bytes are
        // copied verbatim. `SingleByteText` and `Utf8Text` have identical bodies
        // today because there is no codepage conversion on this path yet, but they
        // are kept as separate arms so the divergence is recorded: `json`
        // (`Utf8Text`) is UTF-8 and must NOT be folded into whatever codepage
        // conversion later lands for `varchar(max)` (`SingleByteText`), or
        // non-ASCII json silently corrupts.
        let copy_verbatim = || unsafe {
            if direct_wire_output {
                target_value_ptr.cast::<u8>().add(read).write_unaligned(0);
            } else {
                copy_with_nul(
                    target_value_ptr as *mut u8,
                    buffer_length as usize,
                    &payload[..read],
                );
            }
            write_if_some(strlen_or_ind_ptr, read as SqlLen);
        };
        match plp_encoding {
            // varchar(max)/char/text — single-byte / codepage text. Delivered
            // verbatim today, so a non-UTF-8 server collation yields raw
            // codepage bytes labelled UTF-8. Conversion attaches here: AB#47566.
            Some(PlpEncoding::SingleByteText) => copy_verbatim(),
            // json — UTF-8 on the wire; delivered verbatim to SQL_C_CHAR. Must
            // stay distinct from SingleByteText (see above).
            Some(PlpEncoding::Utf8Text) => copy_verbatim(),
            // Utf16Text/Binary/None never reach this branch: the compatibility
            // gate rejects them or an earlier arm handles them. Assert the
            // invariant in debug/tests; fall back to a verbatim copy in release
            // rather than panicking across the FFI boundary (which would be UB).
            other => {
                debug_assert!(
                    false,
                    "SQL_C_CHAR PLP delivery reached with unexpected encoding {other:?}"
                );
                copy_verbatim();
            }
        }
    }

    let mut stmt_state = match retained_stmt_state {
        Some(state) => state,
        None => {
            let Ok(state) = stmt.inner.lock() else {
                error!("SQLGetData: stmt mutex poisoned while finalizing PLP stream read");
                return SQL_ERROR;
            };
            state
        }
    };

    // The wire being exhausted is not the same as the value being delivered: the
    // widening path can still hold decoded units the caller's buffer had no room
    // for. Ending the stream here would drop them and report success.
    let widen_units_still_held = stmt_state
        .active_plp
        .as_ref()
        .is_some_and(|s| !s.pending_units.is_empty());

    if reached_end && !widen_units_still_held {
        if let Some(mut stream) = stmt_state.active_plp.take() {
            let buffer = stream.take_prefetch_buffer();
            if buffer.capacity() > stmt_state.plp_prefetch_scratch.capacity() {
                stmt_state.plp_prefetch_scratch = buffer;
            }
        }
        return finish_get_data(stmt, statement_handle, stmt_state, col_index, SQL_SUCCESS);
    }

    // active_plp already holds this column's stream state; leave it in place so
    // the next SQLGetData call continues from where this one stopped.
    //
    // StrLen_or_Ind reports the bytes still available *before* this call's copy,
    // matching the reference msodbcsql driver: for a known-length PLP value the
    // server sends the total up front, so each truncated chunk reports a concrete
    // decreasing remaining count rather than SQL_NO_TOTAL. `total_read` already
    // includes this read, so the remaining-before-this-call count is
    // `known_total - (total_read - read)`.
    //
    // Two cases still report SQL_NO_TOTAL, and both match msodbcsql:
    //   * unknown-length (streamed) PLP, where `known_total` is None; and
    //   * the nvarchar->SQL_C_CHAR transcode path, where delivered UTF-8 bytes do
    //     not equal wire UTF-16 bytes, so the wire-byte remaining count would be
    //     the wrong unit. msodbcsql behaves identically here: its GetColData
    //     length logic (sqlcdata.h) deliberately reports SQL_NO_TOTAL whenever the
    //     source and destination C types differ in encoding (SQL_C_WCHAR<->
    //     SQL_C_CHAR), because "we can't know the full size of the converted data
    //     value until we have converted all of it ... as per spec." Its own tests
    //     assert this (RegressionsODBC nvarchar->SQL_C_TCHAR under an ANSI client,
    //     and SQLVariantODBC's "Mplat driver conversion to UTF8 results in
    //     SQL_NO_TOTAL"). Only the same-encoding varchar->SQL_C_CHAR path, where
    //     msodbcsql assumes a 1:1 ratio, gets a concrete count -- which is exactly
    //     the `known_total` branch below. This path is therefore already converged.
    //
    // The varchar->SQL_C_WCHAR widening falls under the same rule and for the
    // same reason: delivered UTF-16 code units are not wire bytes, so it reports
    // SQL_NO_TOTAL too.
    let remaining_indicator = if transcode_utf16_to_utf8 || widen_narrow_to_utf16 {
        SQL_NO_TOTAL
    } else if let Some(total) = known_total {
        let consumed_before = total_read.saturating_sub(read) as u64;
        total.saturating_sub(consumed_before) as SqlLen
    } else {
        SQL_NO_TOTAL
    };
    unsafe { write_if_some(strlen_or_ind_ptr, remaining_indicator) };
    post_diag(&mut stmt_state, WARN_STRING_TRUNCATION);

    SQL_SUCCESS_WITH_INFO
}

/// Decodes one chunk of narrow PLP wire bytes to UTF-16 for `SQL_C_WCHAR`
/// delivery, accumulating into `pending`, and returns how many code units the
/// caller's buffer can take.
///
/// The caller emits `pending[..n]` and drains it. Keeping the emit out of here
/// leaves this a pure decode-and-accumulate step over the two pieces of stream
/// state, so the chunk-boundary rules can be tested without a live wire.
///
/// `decoder` carries a character split across a chunk boundary; `pending`
/// carries code units the caller's buffer had no room for. Both are needed:
/// the wire and the caller advance at rates that are unrelated when the
/// encoding is variable-width.
pub(crate) fn widen_into_pending(
    decoder: &mut Decoder,
    pending: &mut Vec<u16>,
    payload: &[u8],
    reached_end: bool,
    out_units: usize,
) -> usize {
    // Decode onto the tail of `pending` rather than into the caller's buffer.
    // The decoder is asked for its own worst-case bound, so the output slice
    // cannot be short and `OutputFull` cannot arise -- which matters because
    // the consumed-byte count is discarded below, so a short slice would drop
    // wire bytes silently. `max_utf16_buffer_length` also accounts for a
    // partial sequence already held by the decoder, which a fixed constant
    // could not. `None` means the length would overflow `usize`, unreachable
    // here since the read is bounded by the caller's buffer.
    //
    // Decoding into `pending` rather than the caller's buffer is deliberate
    // beyond avoiding a per-call allocation: a decoder that hits `OutputFull`
    // can return having consumed nothing (`encoding_rs::GBK` does exactly that
    // with one unit of room), and the caller's buffer may legitimately be that
    // small.
    //
    // `reached_end` flushes any half-formed sequence to U+FFFD, and the decoder
    // must not be used afterwards. Once the wire is exhausted there is nothing
    // left to feed it, so later calls only drain what is already decoded.
    if !(payload.is_empty() && reached_end) {
        let base = pending.len();
        let headroom = decoder
            .max_utf16_buffer_length(payload.len())
            .unwrap_or_else(|| payload.len().saturating_add(2));
        pending.resize(base + headroom, 0);
        let (result, _, written, _) =
            decoder.decode_to_utf16(payload, &mut pending[base..], reached_end);
        debug_assert_eq!(
            result,
            encoding_rs::CoderResult::InputEmpty,
            "widening output slice was too short, so input bytes were dropped"
        );
        pending.truncate(base + written);
    }
    out_units.min(pending.len())
}

/// Transcodes a chunk of UTF-16LE PLP wire bytes to UTF-8 for SQL_C_CHAR
/// delivery. A trailing odd byte (half a code unit) and an unpaired high
/// surrogate are carried in `pending_byte` / `pending_high_surrogate` so that
/// neither a split code unit nor a split surrogate pair corrupts the output.
/// At end-of-stream any carried half is genuinely malformed and becomes U+FFFD.
pub(crate) fn utf16le_chunk_to_utf8(
    new_bytes: &[u8],
    reached_end: bool,
    pending_byte: &mut Option<u8>,
    pending_high_surrogate: &mut Option<u16>,
) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(new_bytes.len() + 1);
    if let Some(b) = pending_byte.take() {
        bytes.push(b);
    }
    bytes.extend_from_slice(new_bytes);

    // Hold back a trailing odd byte; it is the low half of a code unit whose
    // high half arrives in the next chunk.
    let even = bytes.len() & !1;
    if even != bytes.len() {
        *pending_byte = Some(bytes[even]);
    }

    let mut units: Vec<u16> = Vec::with_capacity(even / 2 + 1);
    if let Some(high) = pending_high_surrogate.take() {
        units.push(high);
    }
    for pair in bytes[..even].chunks_exact(2) {
        units.push(u16::from_le_bytes([pair[0], pair[1]]));
    }

    // Hold back a trailing lone high surrogate so it can pair with the low
    // surrogate arriving next chunk rather than decode to U+FFFD now.
    if !reached_end
        && let Some(&last) = units.last()
        && (0xD800..=0xDBFF).contains(&last)
    {
        *pending_high_surrogate = Some(last);
        units.pop();
    }

    let mut out = String::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        out.push(r.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    if reached_end {
        let leftover = pending_byte.take().is_some() | pending_high_surrogate.take().is_some();
        if leftover {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out
}

fn write_string_result<T: Copy + Default>(
    stmt_state: &mut crate::handles::stmt::StmtState,
    src: &[T],
    target_value_ptr: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let byte_len = std::mem::size_of_val(src) as SqlLen;
    unsafe { write_if_some(strlen_or_ind_ptr, byte_len) };
    let truncated = unsafe { copy_with_nul(target_value_ptr, buf_elements, src) };
    if truncated {
        post_diag(stmt_state, WARN_STRING_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// Why a column value could not be rendered as text.
// Shared with the bound fetch path in `fetch_scroll`.
pub(crate) enum TextError {
    /// No text rendering is defined for this column type.
    Unsupported,
    /// The server payload could not be decoded (bad UTF-8/UTF-16 or a truncated
    /// UTF-16 code unit).
    Malformed,
}

/// `true` for the C targets served by the shared conversion core in one call.
/// Byte count a value would occupy in its `SQL_C_BINARY` form, for the length
/// probe. `SQL_NO_TOTAL` where the binary encoding is not fixed by the value
/// alone — this driver does not deliver binary data yet (AB#47239), so there is
/// no length to promise for those.
fn binary_length(value: &ColumnValues) -> SqlLen {
    let len = match value {
        ColumnValues::Bytes(b) => b.len(),
        ColumnValues::String(s) => s.bytes.len(),
        ColumnValues::Xml(x) => x.bytes.len(),
        ColumnValues::Json(j) => j.bytes.len(),
        ColumnValues::Bit(_) | ColumnValues::TinyInt(_) => 1,
        ColumnValues::SmallInt(_) => 2,
        ColumnValues::Int(_) | ColumnValues::Real(_) | ColumnValues::SmallMoney(_) => 4,
        ColumnValues::BigInt(_) | ColumnValues::Float(_) | ColumnValues::Money(_) => 8,
        ColumnValues::Uuid(_) => 16,
        _ => return SQL_NO_TOTAL,
    };
    SqlLen::try_from(len).unwrap_or(SqlLen::MAX)
}

pub(crate) fn is_typed_c_target(target_type: SqlSmallInt) -> bool {
    is_integer_c_target(target_type)
        || is_float_c_target(target_type)
        || target_type == SQL_C_GUID
        || is_datetime_c_target(target_type)
}

/// Routes a captured value to the matching converter.
///
/// # Safety
/// `target_value_ptr` must be valid for the target C type's size when non-null,
/// and `strlen_or_ind_ptr` null or valid for a `SqlLen` write.
pub(crate) unsafe fn convert_typed_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<ConvOk, ConvError> {
    unsafe {
        if is_integer_c_target(target_type) {
            convert_integer_c(value, target_type, target_value_ptr, strlen_or_ind_ptr)
        } else if is_float_c_target(target_type) {
            convert_float_c(value, target_type, target_value_ptr, strlen_or_ind_ptr)
        } else if target_type == SQL_C_GUID {
            convert_guid_c(value, target_type, target_value_ptr, strlen_or_ind_ptr)
        } else {
            convert_datetime_c(value, target_type, target_value_ptr, strlen_or_ind_ptr)
        }
    }
}

/// Maps a conversion outcome to an ODBC return code, posting the matching
/// diagnostic on the statement.
fn finish_typed_conv(
    stmt_state: &mut crate::handles::stmt::StmtState,
    r: Result<ConvOk, ConvError>,
) -> SqlReturn {
    match r {
        Ok(ConvOk::Exact) => SQL_SUCCESS,
        Ok(ConvOk::Truncated) => {
            post_diag(stmt_state, WARN_FRACTIONAL_TRUNCATION);
            SQL_SUCCESS_WITH_INFO
        }
        Err(ConvError::OutOfRange) => {
            post_diag(stmt_state, ERR_NUMERIC_OUT_OF_RANGE);
            SQL_ERROR
        }
        Err(ConvError::Restricted) => {
            post_diag(stmt_state, ERR_RESTRICTED_DATA_TYPE);
            SQL_ERROR
        }
        Err(ConvError::InvalidCharacterValue) => {
            post_diag(stmt_state, ERR_INVALID_CHARACTER_VALUE);
            SQL_ERROR
        }
        Err(ConvError::NotHandledHere) => {
            post_sql_error(
                stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            );
            SQL_ERROR
        }
    }
}

/// Formats a SQL Server `money` / `smallmoney` value (an integer scaled by
/// 10^4) as a fixed 4-decimal string, without the precision loss of an
/// intermediate `f64`.
fn money_scaled_to_string(scaled: i64) -> String {
    let neg = scaled < 0;
    let abs = scaled.unsigned_abs();
    format!(
        "{}{}.{:04}",
        if neg { "-" } else { "" },
        abs / 10_000,
        abs % 10_000
    )
}

/// Formats a SQL Server `vector` as a JSON-style array of its float elements.
fn format_vector(v: &mssql_tds::datatypes::sql_vector::SqlVector) -> String {
    use mssql_tds::datatypes::sql_vector::VectorData;
    let floats = match &v.data {
        VectorData::Float32(xs) | VectorData::Float16(xs) => xs,
    };
    let mut s = String::from("[");
    for (i, f) in floats.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&f.to_string());
    }
    s.push(']');
    s
}

/// Decodes UTF-16LE `xml` bytes without the panicking indexing/unwrap in
/// `SqlXml::as_string`.
fn xml_to_text(bytes: &[u8]) -> Result<String, TextError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(TextError::Malformed);
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| TextError::Malformed)
}

/// Drops the leading zero from a value whose magnitude is below one.
///
/// msodbcsql renders these as `.5000` / `-.0001`, not `0.5000` / `-0.0001`, and
/// applications that compare the rendered text see the difference. It strips
/// unconditionally, so an exact zero renders `.0000` too.
///
/// Decimal and money reach that result through separate code, which is why both
/// were verified rather than one assumed to follow the other:
/// - `sqlccnvt.cpp` `numerictostring` — the digit loop breaks on
///   `number.data[0] == 0 && i <= 0`, so it stops once the scale is satisfied
///   and never emits an integer-part digit for a sub-one value.
/// - `sqlccnvt.cpp` `BigintToChar`, called with `scale = 4` for money — loops
///   while `value != 0 || cch <= scale` and emits the separator at
///   `cch == scale`, arriving at the same shape.
///
/// `Real` and `Float` are deliberately **not** stripped. Float rendering goes
/// through `DoubleToChar`, which has an explicit "put in leading zero before
/// decimal point" branch, so msodbcsql really does render `0.5` for `FLOAT` and
/// `.5000` for `DECIMAL`. The asymmetry is the parity-correct answer, not an
/// oversight — removing it to make the two consistent would create a divergence.
///
/// Applied here rather than in `mssql-tds`'s formatters because this is the ODBC
/// parity contract, not a general property of number formatting.
fn strip_sub_one_leading_zero(s: String) -> String {
    if let Some(rest) = s.strip_prefix("0.") {
        format!(".{rest}")
    } else if let Some(rest) = s.strip_prefix("-0.") {
        format!("-.{rest}")
    } else {
        s
    }
}

pub(crate) fn column_value_to_text(v: &ColumnValues) -> Result<String, TextError> {
    match v {
        ColumnValues::TinyInt(x) => Ok(x.to_string()),
        ColumnValues::SmallInt(x) => Ok(x.to_string()),
        ColumnValues::Int(x) => Ok(x.to_string()),
        ColumnValues::BigInt(x) => Ok(x.to_string()),
        ColumnValues::Real(x) => Ok(x.to_string()),
        ColumnValues::Float(x) => Ok(x.to_string()),
        ColumnValues::Bit(x) => Ok(if *x { "1".into() } else { "0".into() }),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => {
            Ok(strip_sub_one_leading_zero(d.to_decimal_string()))
        }
        ColumnValues::Money(m) => Ok(strip_sub_one_leading_zero(money_scaled_to_string(
            money_scaled(m.lsb_part, m.msb_part),
        ))),
        ColumnValues::SmallMoney(m) => Ok(strip_sub_one_leading_zero(money_scaled_to_string(
            i64::from(m.int_val),
        ))),
        // `SqlString::to_utf8_string` unwraps on its UTF-8 branch; decode fallibly.
        ColumnValues::String(s) => sql_string_to_text(s).ok_or(TextError::Malformed),
        ColumnValues::Xml(x) => xml_to_text(&x.bytes),
        // `SqlJson::as_string` unwraps; decode fallibly.
        ColumnValues::Json(j) => {
            String::from_utf8(j.bytes.clone()).map_err(|_| TextError::Malformed)
        }
        // msodbcsql renders a uniqueidentifier in upper case; uuid's Display is
        // lower case.
        ColumnValues::Uuid(u) => {
            let mut buffer = uuid::Uuid::encode_buffer();
            Ok(u.hyphenated().encode_upper(&mut buffer).to_string())
        }
        ColumnValues::Vector(vec) => Ok(format_vector(vec)),
        ColumnValues::Date(_)
        | ColumnValues::Time(_)
        | ColumnValues::DateTime(_)
        | ColumnValues::DateTime2(_)
        | ColumnValues::DateTimeOffset(_)
        | ColumnValues::SmallDateTime(_) => extract_datetime_parts(v)
            .map(|p| format_datetime_parts(&p))
            .ok_or(TextError::Unsupported),
        ColumnValues::Null => Ok(String::new()),
        _ => Err(TextError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_SLONG, SQL_C_TYPE_TIMESTAMP};
    use crate::api::odbc_types::{SQL_NO_DATA, SQL_NULL_HANDLE};
    use crate::error::diag::DiagRecord;
    use crate::handles::DbcHandle;
    use crate::handles::stmt::BufferedGetDataRow;
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::SqlString;
    use mssql_tds::datatypes::sqldatatypes::TdsDataType;
    use mssql_tds::test_client_support::{int_columns, tds_client_from_int_rows};

    /// Assert the most recent diagnostic matches the expected canonical
    /// SQLSTATE and message text (the message is prefixed by the driver, so we
    /// match on a substring).
    fn assert_last_diag(records: &[DiagRecord], expected: DiagMsg) {
        let d = records.last().expect("expected a diagnostic record");
        assert_eq!(d.sql_state, expected.state, "SQLSTATE mismatch");
        assert!(
            d.message.contains(expected.text),
            "message {:?} did not contain {:?}",
            d.message,
            expected.text
        );
    }

    #[test]
    fn direct_captured_utf8_chunks_without_transcoding() {
        let value = ColumnValues::String(SqlString::new(b"abcdef".to_vec(), EncodingType::Utf8));
        let mut first = [0_u8; 4];
        let mut indicator = 0;

        let result = unsafe {
            try_write_direct_captured_string_chunk(
                &value,
                SQL_C_CHAR,
                first.as_mut_ptr().cast(),
                first.len(),
                &mut indicator,
                0,
                false,
            )
        };
        assert_eq!(result, Some((true, 3, 6)));
        assert_eq!(indicator, 6);
        assert_eq!(first, [b'a', b'b', b'c', 0]);

        let mut second = [0_u8; 4];
        let result = unsafe {
            try_write_direct_captured_string_chunk(
                &value,
                SQL_C_CHAR,
                second.as_mut_ptr().cast(),
                second.len(),
                &mut indicator,
                3,
                true,
            )
        };
        assert_eq!(result, Some((false, 3, 3)));
        assert_eq!(indicator, 3);
        assert_eq!(second, [b'd', b'e', b'f', 0]);
    }

    #[test]
    fn direct_captured_utf16_chunks_in_code_units() {
        let units: Vec<u16> = "a😀b".encode_utf16().collect();
        let value = ColumnValues::String(SqlString::new(
            units.iter().flat_map(|unit| unit.to_le_bytes()).collect(),
            EncodingType::Utf16,
        ));
        let mut first = [0_u16; 3];
        let mut indicator = 0;

        let result = unsafe {
            try_write_direct_captured_string_chunk(
                &value,
                SQL_C_WCHAR,
                first.as_mut_ptr().cast(),
                first.len(),
                &mut indicator,
                0,
                false,
            )
        };
        assert_eq!(result, Some((true, 2, 4)));
        assert_eq!(indicator, 8);
        assert_eq!(first, [units[0], units[1], 0]);

        let mut second = [0_u16; 3];
        let result = unsafe {
            try_write_direct_captured_string_chunk(
                &value,
                SQL_C_WCHAR,
                second.as_mut_ptr().cast(),
                second.len(),
                &mut indicator,
                2,
                true,
            )
        };
        assert_eq!(result, Some((false, 2, 2)));
        assert_eq!(indicator, 4);
        assert_eq!(second, [units[2], units[3], 0]);
    }

    #[test]
    fn direct_captured_string_requires_a_valid_matching_encoding() {
        let mut indicator = 0;
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &ColumnValues::Int(1),
                    SQL_C_CHAR,
                    std::ptr::null_mut(),
                    0,
                    &mut indicator,
                    0,
                    false,
                )
            },
            None
        );

        let invalid_utf8 = ColumnValues::String(SqlString::new(vec![0xFF], EncodingType::Utf8));
        let mut narrow = [0_u8; 2];
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &invalid_utf8,
                    SQL_C_CHAR,
                    narrow.as_mut_ptr().cast(),
                    narrow.len(),
                    &mut indicator,
                    0,
                    false,
                )
            },
            None
        );
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &invalid_utf8,
                    SQL_C_CHAR,
                    narrow.as_mut_ptr().cast(),
                    narrow.len(),
                    &mut indicator,
                    0,
                    true,
                )
            },
            Some((false, 1, 1))
        );
        assert_eq!(narrow, [0xFF, 0]);

        let odd_utf16 = ColumnValues::String(SqlString::new(vec![b'a'], EncodingType::Utf16));
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &odd_utf16,
                    SQL_C_WCHAR,
                    std::ptr::null_mut(),
                    2,
                    &mut indicator,
                    0,
                    true,
                )
            },
            None
        );

        let unpaired_surrogate = ColumnValues::String(SqlString::new(
            0xD800_u16.to_le_bytes().to_vec(),
            EncodingType::Utf16,
        ));
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &unpaired_surrogate,
                    SQL_C_WCHAR,
                    std::ptr::null_mut(),
                    2,
                    &mut indicator,
                    0,
                    false,
                )
            },
            None
        );
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &unpaired_surrogate,
                    SQL_C_WCHAR,
                    std::ptr::null_mut(),
                    2,
                    &mut indicator,
                    0,
                    true,
                )
            },
            Some((false, 1, 1))
        );

        let mut wide = [0xAAAA_u16; 1];
        assert_eq!(
            unsafe {
                try_write_direct_captured_string_chunk(
                    &unpaired_surrogate,
                    SQL_C_WCHAR,
                    wide.as_mut_ptr().cast(),
                    0,
                    &mut indicator,
                    0,
                    true,
                )
            },
            Some((true, 0, 1))
        );
        assert_eq!(wide, [0xAAAA]);
        assert_eq!(indicator, 2);
    }

    #[test]
    fn get_data_null_handle() {
        let ret = unsafe {
            sql_get_data(
                SQL_NULL_HANDLE,
                1,
                SQL_C_CHAR,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_data_without_cursor_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = stmt_handle.inner.lock().unwrap();
        assert_last_diag(&s.diag_records, ERR_INVALID_CURSOR_STATE);
    }

    /// CURSOR_OPEN with column 0 requested: an invalid descriptor index
    /// (07009) regardless of row state, since ordinal 0 is the bookmark column
    /// which this driver does not support.
    #[test]
    fn get_data_column_zero_is_invalid() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(2);
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                0,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = stmt_handle.inner.lock().unwrap();
        assert_last_diag(&s.diag_records, ERR_INVALID_DESCRIPTOR_INDEX);
    }

    /// Cursor is open but no row is positioned (SQLGetData before a successful
    /// SQLFetch): expect SQL_ERROR with SQLSTATE 24000.
    #[test]
    fn get_data_cursor_open_but_no_active_row_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(2);
            // row_positioned stays false: no SQLFetch has landed on a row yet.
        }

        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = stmt_handle.inner.lock().unwrap();
        let d = s.diag_records.last().unwrap();
        assert_eq!(d.sql_state, SQLSTATE_24000);
        assert!(
            d.message.contains("No current row"),
            "message was: {}",
            d.message
        );
    }

    /// Columns 1..=3 were consumed (cursor at 3). Requesting an earlier column
    /// (2) is backward retrieval, which this driver rejects with 07009 — the
    /// guard fires on statement state alone, before any wire access.
    #[test]
    fn get_data_backward_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(4);
            s.row_positioned = true;
            s.current_row_last_col = 3; // columns 1..=3 already consumed
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                2,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = stmt_handle.inner.lock().unwrap();
        assert_last_diag(&s.diag_records, ERR_INVALID_DESCRIPTOR_INDEX);
    }

    /// Re-requesting the most recently retrieved column (cursor == its ordinal)
    /// reports end-of-data, matching the SQLGetData streaming contract. This is
    /// a clean SQL_NO_DATA — no diagnostic is posted.
    #[test]
    fn get_data_reread_just_consumed_column_returns_no_data() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(4);
            s.row_positioned = true;
            s.current_row_last_col = 3; // column 3 was the last consumed
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                3,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_NO_DATA);
        let s = stmt_handle.inner.lock().unwrap();
        assert!(
            s.diag_records.is_empty(),
            "SQL_NO_DATA must not post a diagnostic, got: {:?}",
            s.diag_records
        );
    }

    /// Helper: transcode a full UTF-16LE buffer delivered in one chunk with no
    /// carried state, asserting both carries end empty.
    fn transcode_whole(bytes: &[u8]) -> String {
        let mut pending_byte = None;
        let mut pending_high = None;
        let out = utf16le_chunk_to_utf8(bytes, true, &mut pending_byte, &mut pending_high);
        assert!(pending_byte.is_none() && pending_high.is_none());
        out
    }

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A single-chunk ASCII buffer transcodes verbatim with no leftover state.
    #[test]
    fn utf16_chunk_ascii_roundtrips() {
        assert_eq!(transcode_whole(&utf16le("Hello")), "Hello");
    }

    /// A BMP code unit split across a chunk boundary is held in `pending_byte`
    /// and completed by the next chunk — the character appears once, intact.
    #[test]
    fn utf16_chunk_splits_code_unit_across_boundary() {
        // 'Z' = U+005A -> LE bytes [0x5A, 0x00]. Feed the low half, then the high.
        let mut pb = None;
        let mut ph = None;
        let first = utf16le_chunk_to_utf8(&[0x5A], false, &mut pb, &mut ph);
        assert_eq!(first, "");
        assert_eq!(pb, Some(0x5A));
        let second = utf16le_chunk_to_utf8(&[0x00], true, &mut pb, &mut ph);
        assert_eq!(second, "Z");
        assert!(pb.is_none() && ph.is_none());
    }

    /// A surrogate pair split across a chunk boundary is held in
    /// `pending_high_surrogate` so it pairs with the low surrogate next chunk
    /// instead of decoding to U+FFFD prematurely.
    #[test]
    fn utf16_chunk_splits_surrogate_pair_across_boundary() {
        // U+1F600 (😀) = surrogate pair D83D DE00.
        let full = utf16le("😀");
        let (high, low) = full.split_at(2);
        let mut pb = None;
        let mut ph = None;
        let first = utf16le_chunk_to_utf8(high, false, &mut pb, &mut ph);
        assert_eq!(first, "", "lone high surrogate must not emit yet");
        assert_eq!(ph, Some(0xD83D));
        let second = utf16le_chunk_to_utf8(low, true, &mut pb, &mut ph);
        assert_eq!(second, "😀");
        assert!(pb.is_none() && ph.is_none());
    }

    /// A dangling half code unit at true end-of-stream is genuinely malformed
    /// and becomes a single U+FFFD.
    #[test]
    fn utf16_chunk_trailing_odd_byte_at_end_is_replacement() {
        let mut pb = None;
        let mut ph = None;
        let out = utf16le_chunk_to_utf8(&[0x41], true, &mut pb, &mut ph);
        assert_eq!(out, "\u{FFFD}");
        assert!(pb.is_none() && ph.is_none());
    }

    /// An unpaired high surrogate at true end-of-stream decodes to U+FFFD (the
    /// end-of-stream guard skips the hold-back).
    #[test]
    fn utf16_chunk_lone_high_surrogate_at_end_is_replacement() {
        let out = transcode_whole(&[0x3D, 0xD8]); // D83D, no low surrogate
        assert_eq!(out, "\u{FFFD}");
    }

    /// Drives `widen_into_pending` the way `stream_active_plp_chunk` does:
    /// feed a chunk, take what the caller's buffer holds, drain it, repeat.
    /// Returns the assembled string and the units delivered per call.
    fn drain_widening(
        encoding: &'static encoding_rs::Encoding,
        wire: &[u8],
        chunk_bytes: usize,
        out_units: usize,
    ) -> (String, Vec<usize>) {
        let mut decoder = encoding.new_decoder_without_bom_handling();
        let mut pending: Vec<u16> = Vec::new();
        let mut delivered: Vec<u16> = Vec::new();
        let mut per_call = Vec::new();
        let mut offset = 0;

        loop {
            let end = (offset + chunk_bytes).min(wire.len());
            let payload = &wire[offset..end];
            let reached_end = end == wire.len();
            offset = end;

            let emit =
                widen_into_pending(&mut decoder, &mut pending, payload, reached_end, out_units);
            delivered.extend_from_slice(&pending[..emit]);
            pending.drain(..emit);
            per_call.push(emit);

            if reached_end && pending.is_empty() {
                break;
            }
            assert!(per_call.len() < 10_000, "widening made no forward progress");
        }
        (String::from_utf16(&delivered).unwrap(), per_call)
    }

    /// A multi-byte character split across a chunk boundary is rejoined by the
    /// carried decoder rather than each half becoming U+FFFD. GBK puts two
    /// bytes on the wire per CJK character, so an odd chunk size splits one on
    /// almost every call.
    #[test]
    fn widening_carries_a_character_across_a_chunk_boundary() {
        let wire = encoding_rs::GBK.encode("你好世界abc").0.into_owned();
        let (got, _) = drain_widening(encoding_rs::GBK, &wire, 3, 64);
        assert_eq!(got, "你好世界abc");
    }

    /// The caller's buffer does not bound the decode. `encoding_rs::GBK`
    /// returns `OutputFull` having consumed nothing when given a single unit of
    /// room, so decoding straight into a one-unit caller buffer would stall;
    /// accumulating into `pending` keeps every call delivering exactly one unit.
    #[test]
    fn widening_serves_a_one_unit_buffer_without_stalling() {
        let wire = encoding_rs::GBK.encode("你好世界abc").0.into_owned();
        let (got, per_call) = drain_widening(encoding_rs::GBK, &wire, 8, 1);
        assert_eq!(got, "你好世界abc");
        assert!(
            per_call.iter().take(per_call.len() - 1).all(|&n| n == 1),
            "every call but the last must deliver one unit: {per_call:?}"
        );
    }

    /// Surplus code units the caller had no room for outlive the wire: once
    /// `reached_end` is reported the decoder is spent, and later calls drain
    /// what is already decoded rather than feeding it again.
    #[test]
    fn widening_drains_pending_units_after_the_wire_is_exhausted() {
        let wire = b"abcdef";
        let mut decoder = encoding_rs::UTF_8.new_decoder_without_bom_handling();
        let mut pending = Vec::new();

        // Whole value arrives at once, but the caller can only take two units.
        let emit = widen_into_pending(&mut decoder, &mut pending, wire, true, 2);
        assert_eq!(emit, 2);
        assert_eq!(pending.len(), 6, "the surplus must be held, not dropped");
        pending.drain(..emit);

        // No wire left: the decoder must not be flushed twice, and the rest is
        // still delivered.
        let emit = widen_into_pending(&mut decoder, &mut pending, &[], true, 2);
        assert_eq!(emit, 2);
        pending.drain(..emit);
        let emit = widen_into_pending(&mut decoder, &mut pending, &[], true, 2);
        assert_eq!(emit, 2);
        pending.drain(..emit);
        assert!(pending.is_empty());
    }

    /// An astral character is two UTF-16 code units, so a buffer with room for
    /// one splits the pair across calls. That is legal — SQL_C_WCHAR chunking
    /// is in code units — and the assembled value must still round-trip.
    #[test]
    fn widening_splits_a_surrogate_pair_across_calls_without_corruption() {
        let wire = "😀ab😀".as_bytes();
        let (got, per_call) = drain_widening(encoding_rs::UTF_8, wire, 16, 1);
        assert_eq!(got, "😀ab😀");
        assert!(per_call.iter().take(per_call.len() - 1).all(|&n| n == 1));
    }

    /// A caller with no payload room is a length probe: nothing is emitted, and
    /// nothing decoded is lost.
    #[test]
    fn widening_emits_nothing_for_a_zero_capacity_buffer() {
        let mut decoder = encoding_rs::UTF_8.new_decoder_without_bom_handling();
        let mut pending = Vec::new();
        let emit = widen_into_pending(&mut decoder, &mut pending, b"abc", false, 0);
        assert_eq!(emit, 0);
        assert_eq!(pending, vec![b'a' as u16, b'b' as u16, b'c' as u16]);
    }

    /// Option-returning shim so these tests read the same as before the
    /// conversion core started distinguishing malformed payloads.
    fn column_value_to_text_opt(v: &ColumnValues) -> Option<String> {
        column_value_to_text(v).ok()
    }

    /// `column_value_to_text` renders scalar column values as text and returns
    /// `None` for types with no textual SQLGetData rendering.
    #[test]
    fn column_value_to_text_renders_scalars() {
        use mssql_tds::datatypes::sql_string::SqlString;
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::TinyInt(7)).as_deref(),
            Some("7")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::SmallInt(-3)).as_deref(),
            Some("-3")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::Int(42)).as_deref(),
            Some("42")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::BigInt(-9)).as_deref(),
            Some("-9")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::Bit(true)).as_deref(),
            Some("1")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::Bit(false)).as_deref(),
            Some("0")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::Null).as_deref(),
            Some("")
        );
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::String(SqlString::from_utf8_string(
                "hi".into()
            )))
            .as_deref(),
            Some("hi")
        );
        // A type with no textual rendering in this helper yields None.
        assert_eq!(
            column_value_to_text_opt(&ColumnValues::Bytes(vec![1, 2, 3])),
            None
        );
    }

    /// Seeds a statement as positioned on a row with `value` already captured
    /// for column 1, which is the state `SQLGetData` sees after the row decoder
    /// has resumed to that column.
    fn stmt_with_captured(h: &TestHandles, value: ColumnValues) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        s.set_state(STMT_STATE_CURSOR_OPEN);
        s.column_metadata = int_columns(2);
        s.row_positioned = true;
        s.last_captured = Some((1, value));
    }

    fn stmt_with_buffered_ints(h: &TestHandles, values: Vec<i32>) {
        h.mark_dbc_connected();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.column_metadata = int_columns(values.len());
            state.row_positioned = true;
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_int_rows(vec![values]);
        dbc.runtime
            .block_on(client.execute("SELECT buffered row".to_string(), ()))
            .unwrap();
        assert!(dbc.runtime.block_on(client.next_row_cursor()).unwrap());
        let mut state = dbc.inner.lock().unwrap();
        state.client = Some(client);
        state.active_stmt = Some(h.stmt);
    }

    fn stmt_with_buffered_values(h: &TestHandles, values: Vec<ColumnValues>) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        state.set_state(STMT_STATE_CURSOR_OPEN);
        state.column_metadata = int_columns(values.len());
        state.row_positioned = true;
        state.buffered_get_data_row = Some(BufferedGetDataRow {
            variant_bases: vec![None; values.len()],
            values: values.into_iter().map(Some).collect(),
            consumed: 0,
            wire_deferred: false,
        });
    }

    fn stmt_with_buffered_get_data_row(h: &TestHandles, values: Vec<i32>) {
        stmt_with_buffered_values(h, values.into_iter().map(ColumnValues::Int).collect());
    }

    fn stmt_with_buffered_string(h: &TestHandles, value: SqlString) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut state = stmt.inner.lock().unwrap();
        state.set_state(STMT_STATE_CURSOR_OPEN);
        state.column_metadata = int_columns(1);
        state.row_positioned = true;
        state.buffered_get_data_row = Some(BufferedGetDataRow {
            variant_bases: vec![None],
            values: vec![Some(ColumnValues::String(value))],
            consumed: 0,
            wire_deferred: false,
        });
    }

    fn stmt_with_buffered_prefix_and_deferred_client(h: &TestHandles) {
        h.mark_dbc_connected();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.column_metadata = int_columns(2);
            state.row_positioned = true;
            state.buffered_get_data_row = Some(BufferedGetDataRow {
                values: vec![Some(ColumnValues::Int(10)), None],
                variant_bases: vec![None, None],
                consumed: 0,
                wire_deferred: false,
            });
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_int_rows(vec![vec![10, 20]]);
        dbc.runtime
            .block_on(client.execute("SELECT deferred column".to_string(), ()))
            .unwrap();
        assert!(dbc.runtime.block_on(client.next_row_cursor()).unwrap());
        assert!(matches!(
            client.try_read_row_column(0).unwrap(),
            CursorPoll::Ready(CursorColumn::Value {
                value: ColumnValues::Int(10),
                ..
            })
        ));
        let mut state = dbc.inner.lock().unwrap();
        state.client = Some(client);
        state.active_stmt = Some(h.stmt);
    }

    #[test]
    fn get_data_reads_complete_buffered_row_without_a_client() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_get_data_row(&h, vec![10, 20, 30]);
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    2,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 20);
        assert_eq!(indicator, 4);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.current_row_last_col, 2);
        assert!(state.last_captured.is_none());
        assert!(state.diag_records.is_empty());
    }

    #[test]
    fn buffered_utf16_ascii_delivers_in_one_call() {
        let h = TestHandles::with_env_dbc_stmt();
        let utf16 = "hi"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut values = vec![ColumnValues::Null; 8];
        values[0] = ColumnValues::String(SqlString::new(utf16, EncodingType::Utf16));
        stmt_with_buffered_values(&h, values);
        let mut output = [0_u8; 3];
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_CHAR,
                    output.as_mut_ptr().cast(),
                    SqlLen::try_from(output.len()).unwrap(),
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(&output, b"hi\0");
        assert_eq!(indicator, 2);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let row = state.buffered_get_data_row.as_ref().unwrap();
        assert_eq!(row.consumed, 1);
        assert!(state.last_captured.is_none());
    }

    #[test]
    fn buffered_decimal_delivers_odbc_text_in_one_call() {
        use mssql_tds::datatypes::decoder::DecimalParts;

        let h = TestHandles::with_env_dbc_stmt();
        let mut values = vec![ColumnValues::Null; 8];
        values[0] = ColumnValues::Decimal(DecimalParts::from_string("0.4500", 18, 4).unwrap());
        stmt_with_buffered_values(&h, values);
        let mut output = [0_u8; 6];
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_CHAR,
                    output.as_mut_ptr().cast(),
                    SqlLen::try_from(output.len()).unwrap(),
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(&output, b".4500\0");
        assert_eq!(indicator, 5);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .buffered_get_data_row
                .as_ref()
                .unwrap()
                .consumed,
            1
        );
    }

    #[test]
    fn buffered_typed_conversion_handles_a_non_exact_target() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_values(&h, vec![ColumnValues::Int(42)]);
        let mut output = 0_i64;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SBIGINT,
                    (&mut output as *mut i64).cast(),
                    SqlLen::try_from(std::mem::size_of_val(&output)).unwrap(),
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(output, 42);
        assert_eq!(
            indicator,
            SqlLen::try_from(std::mem::size_of::<i64>()).unwrap()
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let row = state.spare_get_data_row.as_ref().unwrap();
        assert_eq!(row.values[0], None);
        assert_eq!(row.consumed, 1);
    }

    #[test]
    fn completed_direct_string_returns_its_allocation_to_the_spare_row() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_string(&h, SqlString::new(b"abcdef".to_vec(), EncodingType::Utf8));
        let mut first = [0_u8; 4];
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_CHAR,
                    first.as_mut_ptr().cast(),
                    first.len() as SqlLen,
                    &mut indicator,
                )
            },
            SQL_SUCCESS_WITH_INFO
        );
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.partial_text_offset, Some((1, 3)));
            assert_eq!(state.direct_text_target, Some((1, SQL_C_CHAR)));
        }
        let mut second = [0_u8; 4];
        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_CHAR,
                    second.as_mut_ptr().cast(),
                    second.len() as SqlLen,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.direct_text_target, None);
        let value = state
            .spare_get_data_row
            .as_ref()
            .and_then(|row| row.values.first())
            .and_then(Option::as_ref);
        assert!(matches!(value, Some(ColumnValues::String(value)) if value.bytes == b"abcdef"));
    }

    #[test]
    fn buffered_get_data_soft_failure_keeps_value_for_retry() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_get_data_row(&h, vec![42]);
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_BINARY,
                    (&mut value as *mut i32).cast(),
                    4,
                    &mut indicator,
                )
            },
            SQL_ERROR
        );
        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 42);
    }

    #[test]
    fn buffered_get_data_preserves_variant_base_for_probe() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_get_data_row(&h, vec![42]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .buffered_get_data_row
            .as_mut()
            .unwrap()
            .variant_bases[0] = Some(TdsDataType::Int4);
        let mut probe = 0_u8;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_BINARY,
                    (&mut probe as *mut u8).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(
            stmt.inner.lock().unwrap().last_variant_base,
            Some((1, TdsDataType::Int4))
        );
        {
            let state = stmt.inner.lock().unwrap();
            assert!(state.last_captured.is_none());
            assert!(state.buffered_get_data_row.as_ref().unwrap().values[0].is_some());
        }
        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_BINARY,
                    (&mut probe as *mut u8).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );

        let mut value = 0_i32;
        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 42);
    }

    #[test]
    fn buffered_typed_get_data_preserves_variant_base() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_get_data_row(&h, vec![42]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .buffered_get_data_row
            .as_mut()
            .unwrap()
            .variant_bases[0] = Some(TdsDataType::Int4);
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 42);
        assert_eq!(
            stmt.inner.lock().unwrap().last_variant_base,
            Some((1, TdsDataType::Int4))
        );
    }

    #[test]
    fn get_data_serves_prefix_then_resumes_deferred_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_prefix_and_deferred_client(&h);
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 10);

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    2,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 20);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert!(state.buffered_get_data_row.is_none());
        let row = state.spare_get_data_row.as_ref().unwrap();
        assert!(row.wire_deferred);
        assert!(row.values.iter().all(Option::is_none));
        assert_eq!(row.consumed, 1);
    }

    #[test]
    fn direct_deferred_access_discards_buffered_prefix() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_prefix_and_deferred_client(&h);
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    2,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 20);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert!(state.buffered_get_data_row.is_none());
        let row = state.spare_get_data_row.as_ref().unwrap();
        assert!(row.wire_deferred);
        assert!(row.values.iter().all(Option::is_none));
        assert_eq!(state.current_row_last_col, 2);
        drop(state);
        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_ERROR
        );
    }

    #[test]
    fn resume_row_to_column_keeps_ready_client_installed_and_captures_value() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_ints(&h, vec![42]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        assert_eq!(resume_row_to_column(stmt, h.stmt, 1), SQL_SUCCESS);

        {
            let state = dbc.inner.lock().unwrap();
            assert!(state.client.is_some());
            assert_eq!(state.active_stmt, Some(h.stmt));
        }
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.last_captured, Some((1, ColumnValues::Int(42))));
        assert_eq!(state.last_variant_base, None);
        assert!(!state.row_exhausted);
        assert_eq!(state.partial_text_offset, None);
    }

    #[test]
    fn get_data_resolves_a_buffered_cursor_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_ints(&h, vec![42]);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut value = 0_i32;
        let mut indicator = 0;

        assert_eq!(
            unsafe {
                sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 42);
        assert_eq!(indicator, 4);
        assert!(dbc.inner.lock().unwrap().client.is_some());
    }

    #[test]
    fn resume_row_to_column_restores_client_after_pending_fallback() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_ints(&h, vec![10, 17]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        assert_eq!(resume_row_to_column(stmt, h.stmt, 1), SQL_SUCCESS);

        let dbc_state = dbc.inner.lock().unwrap();
        assert!(dbc_state.client.is_some());
        assert_eq!(dbc_state.active_stmt, Some(h.stmt));
        drop(dbc_state);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.last_captured, Some((1, ColumnValues::Int(10))));
        assert!(state.has_state(STMT_STATE_CURSOR_OPEN));
        assert!(state.diag_records.is_empty());
    }

    /// The byte count a probe reports for each value kind. Variable-length
    /// values report their real size; fixed-width values report their wire
    /// width; anything without a defined binary form reports SQL_NO_TOTAL so
    /// the caller falls back to reading without a size hint.
    #[test]
    fn binary_length_covers_the_value_kinds() {
        use mssql_tds::datatypes::column_values::SqlXml;
        use mssql_tds::datatypes::sql_string::SqlString;

        let cases: &[(ColumnValues, SqlLen)] = &[
            (ColumnValues::Bytes(vec![1, 2, 3]), 3),
            // A SqlString holds the bytes as they came off the wire, so a
            // four-character UTF-16 string is eight bytes.
            (
                ColumnValues::String(SqlString::from_utf8_string("abcd".to_string())),
                8,
            ),
            (
                ColumnValues::Xml(SqlXml {
                    bytes: vec![0x41, 0x42],
                }),
                2,
            ),
            (ColumnValues::Bit(true), 1),
            (ColumnValues::TinyInt(1), 1),
            (ColumnValues::SmallInt(1), 2),
            (ColumnValues::Int(1), 4),
            (ColumnValues::Real(1.0), 4),
            (ColumnValues::BigInt(1), 8),
            (ColumnValues::Float(1.0), 8),
            (ColumnValues::Uuid(uuid::Uuid::nil()), 16),
            (ColumnValues::Null, SQL_NO_TOTAL),
        ];
        for (value, expected) in cases {
            assert_eq!(binary_length(value), *expected, "{value:?}");
        }
    }

    #[test]
    fn complete_buffered_strings_copy_only_for_matching_full_buffers() {
        use mssql_tds::datatypes::sql_string::SqlString;

        let narrow = ColumnValues::String(SqlString::new(b"hello".to_vec(), EncodingType::Utf8));
        let mut narrow_out = [0_u8; 6];
        let mut indicator = 0;
        assert!(unsafe {
            try_write_complete_buffered_string(
                &narrow,
                SQL_C_CHAR,
                narrow_out.as_mut_ptr().cast(),
                narrow_out.len() as SqlLen,
                &mut indicator,
            )
        });
        assert_eq!(&narrow_out, b"hello\0");
        assert_eq!(indicator, 5);
        assert!(!unsafe {
            try_write_complete_buffered_string(
                &narrow,
                SQL_C_CHAR,
                narrow_out.as_mut_ptr().cast(),
                5,
                &mut indicator,
            )
        });

        let wide_bytes = "hi"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let wide = ColumnValues::String(SqlString::new(wide_bytes, EncodingType::Utf16));
        let mut wide_out = [0_u16; 3];
        assert!(unsafe {
            try_write_complete_buffered_string(
                &wide,
                SQL_C_WCHAR,
                wide_out.as_mut_ptr().cast(),
                std::mem::size_of_val(&wide_out) as SqlLen,
                &mut indicator,
            )
        });
        assert_eq!(wide_out, [b'h' as u16, b'i' as u16, 0]);
        assert_eq!(indicator, 4);

        let mut utf8_out = [0_u8; 3];
        assert!(unsafe {
            try_write_complete_buffered_string(
                &wide,
                SQL_C_CHAR,
                utf8_out.as_mut_ptr().cast(),
                utf8_out.len() as SqlLen,
                &mut indicator,
            )
        });
        assert_eq!(&utf8_out, b"hi\0");
        assert_eq!(indicator, 2);

        let non_ascii_bytes = "\u{e9}"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let non_ascii = ColumnValues::String(SqlString::new(non_ascii_bytes, EncodingType::Utf16));
        assert!(!unsafe {
            try_write_complete_buffered_string(
                &non_ascii,
                SQL_C_CHAR,
                utf8_out.as_mut_ptr().cast(),
                utf8_out.len() as SqlLen,
                &mut indicator,
            )
        });

        let empty = ColumnValues::String(SqlString::new(Vec::new(), EncodingType::Utf16));
        let mut empty_out = [0xAA_u8; 2];
        for buffer_length in [0, 1] {
            assert!(!unsafe {
                try_write_complete_buffered_string(
                    &empty,
                    SQL_C_WCHAR,
                    empty_out.as_mut_ptr().cast(),
                    buffer_length,
                    &mut indicator,
                )
            });
            assert_eq!(empty_out, [0xAA, 0xAA]);
        }
        assert!(unsafe {
            try_write_complete_buffered_string(
                &empty,
                SQL_C_WCHAR,
                empty_out.as_mut_ptr().cast(),
                std::mem::size_of_val(&empty_out) as SqlLen,
                &mut indicator,
            )
        });
        assert_eq!(empty_out, [0, 0]);
        assert_eq!(indicator, 0);
    }

    #[test]
    fn complete_buffered_decimal_uses_odbc_text_without_allocating() {
        use mssql_tds::datatypes::decoder::DecimalParts;

        for (input, expected) in [
            ("0.4500", ".4500"),
            ("-0.1250", "-.1250"),
            ("42.0000", "42.0000"),
        ] {
            let decimal = ColumnValues::Decimal(DecimalParts::from_string(input, 18, 4).unwrap());
            let mut output = [0_u8; 16];
            let mut indicator = 0;

            assert!(unsafe {
                try_write_complete_buffered_decimal(
                    &decimal,
                    output.as_mut_ptr().cast(),
                    output.len() as SqlLen,
                    &mut indicator,
                )
            });
            assert_eq!(
                std::str::from_utf8(&output[..expected.len()]).unwrap(),
                expected
            );
            assert_eq!(output[expected.len()], 0);
            assert_eq!(indicator, expected.len() as SqlLen);
        }
    }

    #[test]
    fn exact_buffered_scalar_writes_only_matching_c_type() {
        let mut bytes = [0_u8; 9];
        let target = unsafe { bytes.as_mut_ptr().add(1).cast() };
        let mut indicator = 0;

        assert!(unsafe {
            try_write_exact_buffered_scalar(
                &ColumnValues::Int(42),
                SQL_C_SLONG,
                target,
                &mut indicator,
            )
        });
        assert_eq!(unsafe { target.cast::<i32>().read_unaligned() }, 42);
        assert_eq!(indicator, std::mem::size_of::<i32>() as SqlLen);
        assert!(!unsafe {
            try_write_exact_buffered_scalar(
                &ColumnValues::Int(42),
                SQL_C_SBIGINT,
                target,
                &mut indicator,
            )
        });
    }

    #[test]
    fn exact_buffered_scalars_write_each_supported_layout() {
        use std::mem::MaybeUninit;

        use mssql_tds::datatypes::column_values::{
            SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime,
        };

        macro_rules! check {
            ($value:expr, $target_type:expr, $target_rust_type:ty, $expected:expr) => {{
                let value = $value;
                let mut output = MaybeUninit::<$target_rust_type>::uninit();
                let mut indicator = 0;
                assert!(unsafe {
                    try_write_exact_buffered_scalar(
                        &value,
                        $target_type,
                        output.as_mut_ptr().cast(),
                        &mut indicator,
                    )
                });
                assert_eq!(
                    indicator,
                    SqlLen::try_from(std::mem::size_of::<$target_rust_type>()).unwrap()
                );
                assert_eq!(unsafe { output.assume_init() }, $expected);
            }};
        }

        check!(ColumnValues::Bit(true), SQL_C_BIT, u8, 1);
        check!(ColumnValues::TinyInt(2), SQL_C_UTINYINT, u8, 2);
        check!(ColumnValues::SmallInt(-3), SQL_C_SSHORT, i16, -3);
        check!(ColumnValues::BigInt(-4), SQL_C_SBIGINT, i64, -4);
        check!(ColumnValues::Real(5.5), SQL_C_FLOAT, f32, 5.5);
        check!(ColumnValues::Float(-6.5), SQL_C_DOUBLE, f64, -6.5);

        let date = SqlDate::create(0).unwrap();
        check!(
            ColumnValues::Date(date),
            SQL_C_TYPE_DATE,
            SqlDateStruct,
            SqlDateStruct {
                year: 1,
                month: 1,
                day: 1,
            }
        );
        let time = SqlTime {
            time_nanoseconds: 0,
            scale: 7,
        };
        check!(
            ColumnValues::Time(time.clone()),
            SQL_C_SS_TIME2,
            SqlSsTime2Struct,
            SqlSsTime2Struct::default()
        );
        let datetime2 = SqlDateTime2 {
            days: 0,
            time: time.clone(),
        };
        check!(
            ColumnValues::DateTime2(datetime2.clone()),
            SQL_C_TYPE_TIMESTAMP,
            SqlTimestampStruct,
            SqlTimestampStruct {
                year: 1,
                month: 1,
                day: 1,
                ..SqlTimestampStruct::default()
            }
        );
        let datetimeoffset = SqlDateTimeOffset {
            datetime2,
            offset: 90,
        };
        check!(
            ColumnValues::DateTimeOffset(datetimeoffset),
            SQL_C_SS_TIMESTAMPOFFSET,
            SqlSsTimestampoffsetStruct,
            SqlSsTimestampoffsetStruct {
                year: 1,
                month: 1,
                day: 1,
                hour: 1,
                minute: 30,
                timezone_hour: 1,
                timezone_minute: 30,
                ..SqlSsTimestampoffsetStruct::default()
            }
        );

        let uuid = uuid::Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        let (data1, data2, data3, data4) = uuid.as_fields();
        check!(
            ColumnValues::Uuid(uuid),
            SQL_C_GUID,
            SqlGuid,
            SqlGuid {
                data1,
                data2,
                data3,
                data4: *data4,
            }
        );
    }

    /// A zero-length SQL_C_BINARY read reports the available length and leaves
    /// the value resident, so the caller can still read it for real afterwards.
    /// This is the probe mssql-python issues on every sql_variant column.
    #[test]
    fn get_data_binary_probe_reports_length_without_consuming() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Int(7));

        let mut ind: SqlLen = 0;
        let ret =
            unsafe { sql_get_data(h.stmt, 1, SQL_C_BINARY, std::ptr::null_mut(), 0, &mut ind) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 4);

        // The value survived the probe.
        let mut out: i32 = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                crate::api::odbc_types::SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 7);
    }

    #[test]
    fn get_data_binary_probe_reports_null() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);

        let mut ind: SqlLen = 0;
        let ret =
            unsafe { sql_get_data(h.stmt, 1, SQL_C_BINARY, std::ptr::null_mut(), 0, &mut ind) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
    }

    /// SQLGetData on a NULL reports SQL_NULL_DATA for any valid C target and
    /// leaves a fixed-width buffer untouched. The buffer is nonzero-length, so
    /// this is a data fetch rather than the SQL_C_BINARY length probe above.
    #[test]
    fn get_data_null_reports_null_data_for_any_valid_target() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);

        let mut buf = [0xAAu8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);

        // A fixed-width target's buffer is left untouched on NULL.
        assert_eq!(buf, [0xAAu8; 8]);

        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert!(
            s.diag_records.is_empty(),
            "NULL must not raise a diagnostic: {:?}",
            s.diag_records
        );
    }

    /// C-type legality is settled before the value is looked at, so an invalid
    /// TargetType is HY003 even over a NULL. msodbcsql behaves the same way.
    #[test]
    fn get_data_null_still_rejects_invalid_target_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                9999,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(s.diag_records.last().unwrap().sql_state, SQLSTATE_HY003);
    }

    /// Only the zero-length probe is supported; asking for binary data is still
    /// unimplemented.
    #[test]
    fn get_data_binary_with_buffer_is_not_implemented() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Int(7));

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_BINARY,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(s.diag_records.last().unwrap().sql_state, SQLSTATE_HYC00);
    }

    #[test]
    fn get_data_typed_integer_target() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Int(-2_000_000));

        let mut out: i32 = 0;
        let mut ind: SqlLen = -99;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, -2_000_000);
        assert_eq!(ind, std::mem::size_of::<i32>() as SqlLen);
    }

    /// AB#47507 regression: a NULL value with no indicator must be SQLSTATE
    /// 22002, not a silent SQL_SUCCESS that leaves the caller's buffer
    /// untouched.
    #[test]
    fn get_data_typed_null_without_indicator_reports_22002() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);

        let mut out: i32 = -1_066_579_696; // poisoned sentinel, must survive untouched
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(out, -1_066_579_696, "NULL must not disturb the data slot");
        {
            let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = sh.inner.lock().unwrap();
            assert_last_diag(&s.diag_records, ERR_INDICATOR_REQUIRED);
        }

        // The value was not consumed: a retry with a real indicator still
        // reports the NULL correctly.
        let mut ind: SqlLen = -99;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
    }

    /// The same rule applies to character targets: a NULL with no indicator is
    /// still 22002, even though a terminator could otherwise be written.
    #[test]
    fn get_data_char_null_without_indicator_reports_22002() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);

        let mut buf = [b'X'; 8];
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(buf, [b'X'; 8], "NULL must not disturb the data slot");
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_last_diag(&s.diag_records, ERR_INDICATOR_REQUIRED);
    }

    /// PR review follow-up: the 22002 early return must not disturb the
    /// forward-only row cursor, or a later column in the same row becomes
    /// unreachable. Drives `SELECT 1, NULL, 2` purely through `StmtState`
    /// bookkeeping (manually re-seeding `last_captured` between calls the way
    /// `resume_row_to_column` would), so it pins the accounting invariant
    /// without needing a live decoder: column 2's 22002 must leave
    /// `current_row_last_col` at column 1, so the forward-only gate still
    /// admits column 3 afterward, and column 3 is not mistaken for an
    /// already-consumed re-request of column 2.
    #[test]
    fn get_data_null_without_indicator_does_not_block_later_columns_in_the_row() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(3);
            s.row_positioned = true;
            s.last_captured = Some((1, ColumnValues::Int(1)));
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 1);

        // Column 2 is NULL with no indicator: 22002, and must not advance the
        // forward-only cursor past column 1.
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_captured = Some((2, ColumnValues::Null));
        }
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                2,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        {
            let s = stmt_handle.inner.lock().unwrap();
            assert_eq!(
                s.current_row_last_col, 1,
                "a failed NULL column must not advance the forward-only cursor"
            );
        }

        // Column 3 is still reachable: the forward-only gate admits it
        // (current_row_last_col is still 1), and it is not treated as an
        // already-captured re-request of column 2.
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_captured = Some((3, ColumnValues::Int(2)));
        }
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                3,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 2);
    }

    #[test]
    fn get_data_typed_timestamp_target() {
        use crate::api::odbc_types::SqlTimestampStruct;
        use mssql_tds::datatypes::column_values::{SqlDateTime2, SqlTime};
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(
            &h,
            ColumnValues::DateTime2(SqlDateTime2 {
                days: 738_685, // 2023-06-15
                time: SqlTime {
                    time_nanoseconds: ((12 * 3600 + 34 * 60 + 56) as u64) * 10_000_000,
                    scale: 7,
                },
            }),
        );

        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                std::mem::size_of::<SqlTimestampStruct>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!((out.year, out.month, out.day), (2023, 6, 15));
        assert_eq!((out.hour, out.minute, out.second), (12, 34, 56));
    }

    #[test]
    fn get_data_typed_out_of_range_reports_22003() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::BigInt(i64::from(i32::MAX) + 1));

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut out as *mut i32).cast(),
                4,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = unsafe { handle_from_raw::<StmtHandle>(h.stmt) }
            .inner
            .lock()
            .unwrap();
        assert_last_diag(&s.diag_records, ERR_NUMERIC_OUT_OF_RANGE);
    }

    #[test]
    fn get_data_non_temporal_into_timestamp_is_restricted() {
        use crate::api::odbc_types::SqlTimestampStruct;
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Int(42));

        let mut out = SqlTimestampStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_TYPE_TIMESTAMP,
                (&mut out as *mut SqlTimestampStruct).cast(),
                std::mem::size_of::<SqlTimestampStruct>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = unsafe { handle_from_raw::<StmtHandle>(h.stmt) }
            .inner
            .lock()
            .unwrap();
        assert_last_diag(&s.diag_records, ERR_RESTRICTED_DATA_TYPE);
    }

    /// The strip rule is only otherwise verified by the e2e parity suite, which
    /// needs a live SQL Server and msodbcsql installed. Pinned here so a refactor
    /// of `column_value_to_text` cannot drop it and still look green.
    #[test]
    fn sub_one_values_render_without_the_leading_zero() {
        assert_eq!(strip_sub_one_leading_zero("0.5000".into()), ".5000");
        assert_eq!(strip_sub_one_leading_zero("-0.0001".into()), "-.0001");
        // msodbcsql strips unconditionally, so an exact zero loses its digit too.
        assert_eq!(strip_sub_one_leading_zero("0.0000".into()), ".0000");

        // At or above one, scale 0, and a bare integer are untouched.
        assert_eq!(strip_sub_one_leading_zero("123.4500".into()), "123.4500");
        assert_eq!(strip_sub_one_leading_zero("-1.5000".into()), "-1.5000");
        assert_eq!(strip_sub_one_leading_zero("0".into()), "0");
        assert_eq!(strip_sub_one_leading_zero("-0".into()), "-0");
    }

    /// Same reasoning as the strip rule: msodbcsql renders a uniqueidentifier
    /// in upper case and `uuid`'s Display is lower case, so this is a one-word
    /// change away from silently diverging again.
    #[test]
    fn guid_renders_in_upper_case() {
        let g = uuid::Uuid::parse_str("0123abcd-4567-89ef-0123-456789abcdef").unwrap();
        assert_eq!(
            column_value_to_text(&ColumnValues::Uuid(g)).ok().unwrap(),
            "0123ABCD-4567-89EF-0123-456789ABCDEF"
        );
    }

    /// Covers the wiring as well as the helper: a sub-one value has to arrive
    /// stripped through the real decimal and money arms, not just through a
    /// hand-built string.
    #[test]
    fn sub_one_decimal_and_money_render_stripped() {
        use mssql_tds::datatypes::decoder::DecimalParts;

        let d = ColumnValues::Numeric(DecimalParts::from_string("0.4500", 5, 4).unwrap());
        assert_eq!(column_value_to_text(&d).ok().unwrap(), ".4500");

        let neg = ColumnValues::Decimal(DecimalParts::from_string("-0.0001", 5, 4).unwrap());
        assert_eq!(column_value_to_text(&neg).ok().unwrap(), "-.0001");

        // 0.5 in money's fixed 4-digit scale is 5000.
        let m = ColumnValues::Money(mssql_tds::datatypes::column_values::SqlMoney::from(5000i32));
        assert_eq!(column_value_to_text(&m).ok().unwrap(), ".5000");
    }

    /// Float is the deliberate exception: msodbcsql's `DoubleToChar` writes the
    /// leading zero, so stripping here would create a divergence.
    #[test]
    fn sub_one_float_keeps_its_leading_zero() {
        assert_eq!(
            column_value_to_text(&ColumnValues::Float(0.5))
                .ok()
                .unwrap(),
            "0.5"
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::Real(0.5)).ok().unwrap(),
            "0.5"
        );
    }

    #[test]
    fn get_data_decimal_renders_as_text() {
        use mssql_tds::datatypes::decoder::DecimalParts;
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(
            &h,
            ColumnValues::Numeric(DecimalParts::from_string("123.45", 5, 2).unwrap()),
        );

        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(&buf[..ind as usize], b"123.45");
    }

    #[test]
    fn get_data_malformed_payload_reports_22018() {
        use mssql_tds::datatypes::column_values::SqlXml;
        let h = TestHandles::with_env_dbc_stmt();
        // Odd byte count: not a whole number of UTF-16 code units.
        stmt_with_captured(
            &h,
            ColumnValues::Xml(SqlXml {
                bytes: vec![0x41, 0x00, 0x42],
            }),
        );

        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = unsafe { handle_from_raw::<StmtHandle>(h.stmt) }
            .inner
            .lock()
            .unwrap();
        assert_last_diag(&s.diag_records, ERR_INVALID_CHARACTER_VALUE);
    }

    /// Character into a date/time target is legal per Appendix D and is
    /// implemented as of P1a.
    #[test]
    fn get_data_character_into_date_target_converts() {
        use crate::api::odbc_types::SqlDateStruct;
        use mssql_tds::datatypes::sql_string::SqlString;
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(
            &h,
            ColumnValues::String(SqlString::from_utf8_string("2023-06-15".to_string())),
        );

        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                crate::api::odbc_types::SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                std::mem::size_of::<SqlDateStruct>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!((out.year, out.month, out.day), (2023, 6, 15));
        assert_eq!(ind, std::mem::size_of::<SqlDateStruct>() as SqlLen);
    }

    /// Character that is not a valid literal for the target is 22018, not a
    /// silent zero value.
    #[test]
    fn get_data_invalid_character_into_date_target_is_22018() {
        use crate::api::odbc_types::SqlDateStruct;
        use mssql_tds::datatypes::sql_string::SqlString;
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(
            &h,
            ColumnValues::String(SqlString::from_utf8_string("not a date".to_string())),
        );

        let mut out = SqlDateStruct::default();
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                h.stmt,
                1,
                crate::api::odbc_types::SQL_C_TYPE_DATE,
                (&mut out as *mut SqlDateStruct).cast(),
                std::mem::size_of::<SqlDateStruct>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let s = unsafe { handle_from_raw::<StmtHandle>(h.stmt) }
            .inner
            .lock()
            .unwrap();
        let last = s.diag_records.last().unwrap();
        assert_eq!(&last.sql_state, b"22018");
    }

    /// Delivering the sole column of a single-column result must release the
    /// connection's busy claim right away — the statement's cursor stays
    /// open, but nothing on the wire remains for a later `SQLGetData` on this
    /// row to protect, so the peek is safe. This is the point of AB#47508's
    /// fix: msodbcsql's busy gate tracks the wire, not the statement's cursor
    /// lifetime.
    #[test]
    fn get_data_releases_busy_after_delivering_the_lone_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt_handle.inner.lock().unwrap().column_metadata = int_columns(1);
        }
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = mssql_tds::test_client_support::tds_client_from_tokens(vec![
            mssql_tds::test_client_support::col_metadata_empty(),
            mssql_tds::test_client_support::done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let rc = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };

        assert_eq!(rc, SQL_SUCCESS);
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt_handle.inner.lock().unwrap().result_set_exhausted);
    }

    #[test]
    fn buffered_get_data_releases_busy_after_delivering_the_lone_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_buffered_get_data_row(&h, vec![42]);
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = mssql_tds::test_client_support::tds_client_from_tokens(vec![
            mssql_tds::test_client_support::col_metadata_empty(),
            mssql_tds::test_client_support::done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT 42;".to_string(), ()))
            .unwrap();
        {
            let mut dbc_state = dbc.inner.lock().unwrap();
            dbc_state.client = Some(client);
            dbc_state.active_stmt = Some(h.stmt);
        }

        let mut value = 0_i32;
        let mut ind: SqlLen = 0;
        let rc = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_SLONG,
                (&mut value as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };

        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, 42);
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt_handle.inner.lock().unwrap().result_set_exhausted);
    }

    /// The mirror image: column 1 of a *two*-column result is not the last
    /// column, so a trailing `SQLGetData` call could still legitimately
    /// retrieve column 2. The busy claim must stay exactly as it was — and
    /// since no DBC client is configured at all here, a peek attempt would
    /// fail loudly rather than silently succeed, so `SQL_SUCCESS` below is
    /// itself proof the peek was correctly skipped.
    #[test]
    fn get_data_keeps_busy_when_a_trailing_column_remains() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_captured(&h, ColumnValues::Null); // int_columns(2) by default
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().active_stmt = Some(h.stmt);

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let rc = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };

        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(h.stmt));
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(!stmt_handle.inner.lock().unwrap().result_set_exhausted);
    }

    /// `finish_get_data` must decide whether to peek from delivery state
    /// (`current_row_last_col`/`column_metadata`/`active_plp`) alone, never
    /// from `rc`. Every *current* `write_captured_column` error path leaves
    /// `current_row_last_col` unadvanced, so this combination cannot happen
    /// today — but a future caller can legitimately finish delivering the
    /// column while still reporting `SQL_ERROR` (e.g. a NULL surfaced through
    /// an error-style indicator), and the busy-release optimization must not
    /// silently skip that case.
    #[test]
    fn finish_get_data_releases_busy_purely_on_delivery_state_even_when_rc_is_sql_error() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata = int_columns(1);
            s.current_row_last_col = 1; // as if column 1 (the only column) was just fully delivered.
        }
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = mssql_tds::test_client_support::tds_client_from_tokens(vec![
            mssql_tds::test_client_support::col_metadata_empty(),
            mssql_tds::test_client_support::done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let stmt_state = stmt_handle.inner.lock().unwrap();
        let rc = finish_get_data(stmt_handle, h.stmt, stmt_state, 1, SQL_ERROR);

        assert_eq!(rc, SQL_ERROR);
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
        assert!(stmt_handle.inner.lock().unwrap().result_set_exhausted);
    }

    /// The PLP scope limit (see the doc comment above `finish_get_data`):
    /// even when the last column of the last row was just delivered, a
    /// column still mid-PLP-stream (`active_plp` set) must NOT be peeked
    /// past — the stream may not have reached the wire's end yet, so the
    /// busy claim must be retained exactly as-is until the PLP stream
    /// completes naturally on a later `SQLGetData` call.
    #[test]
    fn finish_get_data_keeps_busy_while_a_plp_stream_is_still_active() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata = int_columns(1);
            s.current_row_last_col = 1; // the only column, otherwise fully delivered.
            s.active_plp = Some(ActivePlpStream::new(1, PlpEncoding::SingleByteText, None));
        }
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        // A real, positioned client: if the PLP guard were missing, `ready`
        // would wrongly read true and the peek below would actually run
        // (and succeed, since the scripted stream is well-formed) — the
        // claim retention this test checks would then fail for real,
        // not just because there was nothing to peek with.
        let mut client = mssql_tds::test_client_support::tds_client_from_tokens(vec![
            mssql_tds::test_client_support::col_metadata_empty(),
            mssql_tds::test_client_support::done_no_more(),
        ]);
        dbc.runtime
            .block_on(client.execute("SELECT 1;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let stmt_state = stmt_handle.inner.lock().unwrap();
        let rc = finish_get_data(stmt_handle, h.stmt, stmt_state, 1, SQL_SUCCESS);

        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            dbc.inner.lock().unwrap().active_stmt,
            Some(h.stmt),
            "an active PLP stream must retain the busy claim, not release it"
        );
        assert!(dbc.inner.lock().unwrap().client.is_some());
        assert!(!stmt_handle.inner.lock().unwrap().result_set_exhausted);
    }

    #[test]
    fn deferred_plp_prefetch_error_surfaces_on_next_get_data_call() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // Recreate the state left after one call returns its current chunk while
        // the read-ahead for the following chunk fails.
        let mut stream = ActivePlpStream::new(1, PlpEncoding::SingleByteText, None);
        stream.set_prefetch_error(mssql_tds::error::Error::Io(std::io::Error::other(
            "deferred PLP prefetch failure",
        )));
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.column_metadata = int_columns(1);
            state.row_positioned = true;
            state.active_plp = Some(stream);
        }
        let mut output = [0_u8; 8];
        let mut indicator = 0;

        let rc = unsafe {
            sql_get_data(
                h.stmt,
                1,
                SQL_C_CHAR,
                output.as_mut_ptr().cast(),
                SqlLen::try_from(output.len()).unwrap(),
                &mut indicator,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert!(!state.has_state(STMT_STATE_CURSOR_OPEN));
        let diagnostic = state.diag_records.last().unwrap();
        assert!(
            diagnostic.message.contains("deferred PLP prefetch failure"),
            "{diagnostic:?}"
        );
    }
}
