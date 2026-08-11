// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation with incremental row materialization.

use tracing::{debug, error};

use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NO_TOTAL,
    SQL_NULL_DATA, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, drive_read, write_if_some};
use crate::api::value_text::column_value_to_text;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ActivePlpStream, STMT_STATE_CURSOR_OPEN};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::{CursorColumn, PlpChunk};
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
        return write_column_as_text(
            &mut stmt_state,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
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
    write_column_as_text(
        &mut reopened_stmt_state,
        col_index,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn write_column_as_text(
    stmt_state: &mut crate::handles::stmt::StmtState,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    // Codepage note: SQL_C_CHAR output is UTF-8. This diverges from msodbcsql,
    // which converts character data to the client's Windows ANSI codepage. The
    // divergence is intentional (this driver is codepage-agnostic and UTF-8
    // native); callers that need ANSI must transcode. SQL_C_WCHAR is UTF-16LE on
    // both drivers.
    // Check target type first — an unsupported type must not consume last_captured so the app can retry.
    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
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
        unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
        if target_type == SQL_C_WCHAR {
            unsafe {
                copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
            }
        } else {
            unsafe {
                copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]);
            }
        }
        stmt_state.last_captured = None;
        stmt_state.partial_text_offset = None;
        stmt_state.current_row_last_col = col_index;
        return SQL_SUCCESS;
    }

    let Some(as_text) = column_value_to_text(value) else {
        // Unconvertible *column* type: HYC00 is a soft failure. Leave the value
        // in place (do not consume) so a retry with another C type can work.
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Column type conversion not yet implemented",
        );
        return SQL_ERROR;
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
    let cursor_result = drive_read(&dbc.runtime, client.read_row_column(target));

    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!("SQLGetData: dbc mutex poisoned after row resume");
        return SQL_ERROR;
    };
    dbc_state.client = Some(client);
    dbc_state.active_stmt = Some(statement_handle);
    drop(dbc_state);

    match cursor_result {
        Ok(CursorColumn::Value(value)) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.last_captured = Some((column_number, value));
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
        // Codepage note: as in the non-PLP path, SQL_C_CHAR output is UTF-8,
        // which diverges from msodbcsql's Windows ANSI codepage conversion. This
        // is intentional; SQL_C_WCHAR is UTF-16LE on both drivers.
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

    let read_result = drive_read(&dbc.runtime, client.read_active_plp_chunk(&mut payload));

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
            // varchar(max)/char/text — single-byte / codepage text. Codepage
            // conversion will attach here in a follow-up.
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
    post_diag(&mut stmt_state, ERR_STRING_RIGHT_TRUNCATION);

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
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_NO_DATA, SQL_NULL_HANDLE};
    use crate::error::diag::DiagRecord;
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

    /// `column_value_to_text` renders scalar column values as text and returns
    /// `None` for types with no textual SQLGetData rendering.
    #[test]
    fn column_value_to_text_renders_scalars() {
        use mssql_tds::datatypes::sql_string::SqlString;
        assert_eq!(
            column_value_to_text(&ColumnValues::TinyInt(7)).as_deref(),
            Some("7")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::SmallInt(-3)).as_deref(),
            Some("-3")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::Int(42)).as_deref(),
            Some("42")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::BigInt(-9)).as_deref(),
            Some("-9")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::Bit(true)).as_deref(),
            Some("1")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::Bit(false)).as_deref(),
            Some("0")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::Null).as_deref(),
            Some("")
        );
        assert_eq!(
            column_value_to_text(&ColumnValues::String(SqlString::from_utf8_string(
                "hi".into()
            )))
            .as_deref(),
            Some("hi")
        );
        // Binary data converts to its uppercase hex literal (ODBC SQL_C_CHAR form).
        assert_eq!(
            column_value_to_text(&ColumnValues::Bytes(vec![1, 2, 3])).as_deref(),
            Some("010203")
        );
    }
}
