// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation with incremental row materialization.

use tracing::{debug, error};

use std::sync::MutexGuard;

use super::odbc_types::{
    SQL_C_BINARY, SQL_C_CHAR, SQL_C_GUID, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA,
    SQL_NO_TOTAL, SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer,
    SqlReturn, SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::exec_common::release_busy_if_row_exhausted;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ActivePlpStream, STMT_STATE_CURSOR_OPEN, StmtState};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::{CursorColumn, PlpChunk};

use crate::conversion::error::{ConvError, ConvOk};
use crate::conversion::fetch_convert::{
    convert_datetime_c, convert_float_c, convert_guid_c, convert_integer_c, extract_datetime_parts,
    format_datetime_parts, is_datetime_c_target, is_float_c_target, is_integer_c_target,
    money_scaled, sql_string_to_text,
};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::metadata::PlpEncoding;

/// Implements SQLGetData for current-row retrieval.
///
/// Current scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports `SQL_C_CHAR` and `SQL_C_WCHAR` for text retrieval.
/// - Supports incremental row resume and chunked PLP retrieval via
///   `read_active_plp_chunk`.
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

    // Continuation: app is calling SQLGetData again on the same PLP column to
    // get the next chunk from the active wire stream.
    if stmt_state
        .active_plp
        .as_ref()
        .is_some_and(|s| s.column == col_index)
    {
        drop(stmt_state);
        return stream_active_plp_chunk(
            stmt,
            statement_handle,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
            false,
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

    // Resume the decoder to the requested column then write output.
    drop(stmt_state);
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
        drop(reopened_stmt_state);
        return stream_active_plp_chunk(
            stmt,
            statement_handle,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
            true,
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
    stmt_state: MutexGuard<'_, StmtState>,
    col_index: usize,
    rc: SqlReturn,
) -> SqlReturn {
    let ready = stmt_state.current_row_last_col == col_index
        && col_index == stmt_state.column_metadata.len()
        && stmt_state.active_plp.is_none();
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
    release_busy_if_row_exhausted(dbc, stmt, statement_handle, client);
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
    // instead converts to the client code page (`GetACP()` on Windows,
    // `nl_langinfo(CODESET)` elsewhere), so the two agree on a UTF-8 locale and
    // differ on a default Windows one. UTF-8 is chosen because the only
    // supported consumer is mssql-python, which is UTF-8 native; the ODBC spec
    // fixes no encoding for SQL_C_CHAR, so neither is more conformant. Client
    // code page support is AB#47564. SQL_C_WCHAR is UTF-16LE on both drivers.
    // Check target type first — an unsupported type must not consume last_captured so the app can retry.
    // A zero-length SQL_C_BINARY read is a length/NULL probe rather than a data
    // read. mssql-python issues one on every sql_variant column to detect NULL
    // and to make the underlying type available to SQLColAttribute, so the probe
    // is admitted here; delivering binary data is still AB#47239.
    let binary_probe = target_type == SQL_C_BINARY && buffer_length == 0;
    let typed_target = is_typed_c_target(target_type);
    if !typed_target && target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR && !binary_probe {
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
    let Some((_, value)) = stmt_state.last_captured.as_ref() else {
        post_sql_error(
            stmt_state,
            SQLSTATE_24000,
            0,
            "Requested column is not available in the current row",
        );
        return SQL_ERROR;
    };

    // Output buffer capacity in element units (u8 for SQL_C_CHAR, SqlWChar for
    // SQL_C_WCHAR). buffer_length is always in bytes per the ODBC spec.
    let buf_elements = if target_type == SQL_C_WCHAR {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };

    if matches!(value, ColumnValues::Null) {
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
    let offset = stmt_state
        .partial_text_offset
        .filter(|(c, _)| *c == col_index)
        .map(|(_, o)| o)
        .unwrap_or(0);

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

    let mut client = {
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

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    let target = column_number - 1; // 0-based
    let cursor_result = dbc.runtime.block_on(client.read_row_column(target));

    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLGetData: dbc mutex poisoned after row resume");
        return SQL_ERROR;
    };
    dbc_state.client = Some(client);
    dbc_state.active_stmt = Some(statement_handle);
    drop(dbc_state);

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
/// This never buffers the full PLP payload in ODBC-layer memory. The TDS
/// client remains the owner of stream state between repeated calls.
#[allow(clippy::too_many_arguments)]
fn stream_active_plp_chunk(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
    starting_new_stream: bool,
) -> SqlReturn {
    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        if let Ok(mut s) = stmt.inner.lock() {
            post_sql_error(&mut s, SQLSTATE_HYC00, 0, "Target type not yet implemented");
        }
        return SQL_ERROR;
    }

    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while preparing PLP stream read");
            return SQL_ERROR;
        };

        if starting_new_stream {
            let encoding = stmt_state
                .column_metadata
                .get(col_index - 1)
                .and_then(|m| m.plp_encoding())
                .unwrap_or(PlpEncoding::SingleByteText);
            stmt_state.active_plp = Some(ActivePlpStream {
                column: col_index,
                encoding,
                pending_byte: None,
                pending_high_surrogate: None,
            });
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

        // Supported text deliveries: SQL_C_WCHAR for nvarchar(max)/xml
        // (UTF-16LE) and SQL_C_CHAR for either varchar(max) (single byte) or
        // nvarchar(max) (UTF-16LE transcoded to UTF-8). Binary columns and the
        // varchar->SQL_C_WCHAR widening are not yet implemented; they return
        // HYC00 and are deferred to a follow-up change.
        //
        // Codepage note: as in the non-PLP path, SQL_C_CHAR output is UTF-8
        // rather than the client code page msodbcsql uses (AB#47564).
        let encoding = stmt_state.active_plp.as_ref().map(|s| s.encoding);
        let compatible = matches!(
            (target_type, encoding),
            (SQL_C_WCHAR, Some(PlpEncoding::Utf16Text))
                | (SQL_C_CHAR, Some(PlpEncoding::SingleByteText))
                | (SQL_C_CHAR, Some(PlpEncoding::Utf8Text))
                | (SQL_C_CHAR, Some(PlpEncoding::Utf16Text))
        );
        if !compatible {
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Target type not yet implemented for this column",
            );
            return SQL_ERROR;
        }
    }

    let plp_encoding = {
        let Ok(ss) = stmt.inner.lock() else {
            return SQL_ERROR;
        };
        ss.active_plp.as_ref().map(|s| s.encoding)
    };
    let is_unicode_plp = matches!(plp_encoding, Some(PlpEncoding::Utf16Text));
    // SQL_C_CHAR delivery of a UTF-16 PLP column must transcode on the fly.
    let transcode_utf16_to_utf8 = target_type == SQL_C_CHAR && is_unicode_plp;

    let payload_capacity = if target_type == SQL_C_WCHAR {
        (buffer_length as usize).saturating_sub(std::mem::size_of::<SqlWChar>())
    } else {
        (buffer_length as usize).saturating_sub(1)
    };
    let max_read = if target_type == SQL_C_WCHAR {
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

    // A non-empty buffer too small to hold even one character plus the NUL is a
    // caller error (HY090). buffer_length == 0 is a legal length probe and must
    // fall through (PLP length is unknown, reported later as SQL_NO_TOTAL).
    //
    // TODO(convergence): msodbcsql does NOT reject the sub-minimal buffer here.
    // Probed against ODBC Driver 18 with a 2-byte SQL_C_CHAR buffer over an
    // nvarchar(max) column, it returns SQL_SUCCESS_WITH_INFO/01004 with
    // indicator SQL_NO_TOTAL and delivers one payload byte per call (splitting a
    // multibyte UTF-8 sequence across calls when necessary), so the stream still
    // drains and reassembles. To converge, deliver whole *bytes* that fit (even
    // 1) and carry the unflushed UTF-8 tail in ActivePlpStream (a pending_utf8
    // buffer beside pending_byte/pending_high_surrogate), draining it first on
    // the next call. That guarantees >=1 byte of forward progress and lets this
    // HY090 guard be removed. Tracked by the skipped e2e test
    // PlpZeroCapacityBufferDoesNotSpin.
    if max_read == 0 && buffer_length > 0 {
        if let Ok(mut s) = stmt.inner.lock() {
            post_sql_error(
                &mut s,
                SQLSTATE_HY090,
                0,
                "Buffer length too small to hold a single character and null terminator",
            );
        }
        return SQL_ERROR;
    }

    let mut payload = vec![0u8; max_read];
    let dbc = stmt.parent_dbc();
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLGetData: dbc mutex poisoned while reading PLP stream");
            return SQL_ERROR;
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

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut s) = stmt.inner.lock() {
                post_diag(&mut s, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    let read_result = dbc
        .runtime
        .block_on(client.read_active_plp_chunk(&mut payload));

    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLGetData: dbc mutex poisoned after PLP read");
        return SQL_ERROR;
    };
    dbc_state.client = Some(client);
    dbc_state.active_stmt = Some(statement_handle);
    drop(dbc_state);

    let PlpChunk {
        read,
        reached_end,
        known_total,
        total_read,
    } = match read_result {
        Ok(chunk) => chunk,
        Err(e) => {
            if let Ok(mut s) = stmt.inner.lock() {
                s.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut s, &e, SQLSTATE_HY000);
            }
            return SQL_ERROR;
        }
    };

    if target_type == SQL_C_WCHAR {
        let usable = read & !1;
        let units: Vec<u16> = payload[..usable]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        unsafe {
            copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &units);
            write_if_some(strlen_or_ind_ptr, usable as SqlLen);
        }
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
            copy_with_nul(
                target_value_ptr as *mut u8,
                buffer_length as usize,
                &payload[..read],
            );
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

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned while finalizing PLP stream read");
        return SQL_ERROR;
    };

    if reached_end {
        stmt_state.active_plp = None;
        return SQL_SUCCESS;
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
    let remaining_indicator = if transcode_utf16_to_utf8 {
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

/// Transcodes a chunk of UTF-16LE PLP wire bytes to UTF-8 for SQL_C_CHAR
/// delivery. A trailing odd byte (half a code unit) and an unpaired high
/// surrogate are carried in `pending_byte` / `pending_high_surrogate` so that
/// neither a split code unit nor a split surrogate pair corrupts the output.
/// At end-of-stream any carried half is genuinely malformed and becomes U+FFFD.
fn utf16le_chunk_to_utf8(
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

pub(crate) fn column_value_to_text(v: &ColumnValues) -> Result<String, TextError> {
    match v {
        ColumnValues::TinyInt(x) => Ok(x.to_string()),
        ColumnValues::SmallInt(x) => Ok(x.to_string()),
        ColumnValues::Int(x) => Ok(x.to_string()),
        ColumnValues::BigInt(x) => Ok(x.to_string()),
        ColumnValues::Real(x) => Ok(x.to_string()),
        ColumnValues::Float(x) => Ok(x.to_string()),
        ColumnValues::Bit(x) => Ok(if *x { "1".into() } else { "0".into() }),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Ok(d.to_decimal_string()),
        ColumnValues::Money(m) => Ok(money_scaled_to_string(money_scaled(m.lsb_part, m.msb_part))),
        ColumnValues::SmallMoney(m) => Ok(money_scaled_to_string(i64::from(m.int_val))),
        // `SqlString::to_utf8_string` unwraps on its UTF-8 branch; decode fallibly.
        ColumnValues::String(s) => sql_string_to_text(s).ok_or(TextError::Malformed),
        ColumnValues::Xml(x) => xml_to_text(&x.bytes),
        // `SqlJson::as_string` unwraps; decode fallibly.
        ColumnValues::Json(j) => {
            String::from_utf8(j.bytes.clone()).map_err(|_| TextError::Malformed)
        }
        ColumnValues::Uuid(u) => Ok(u.to_string()),
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
    use crate::test_support::TestHandles;
    use mssql_tds::test_client_support::int_columns;

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
}
