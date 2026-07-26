// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLGetData implementation with incremental row materialization.

use tracing::{debug, error};

use super::odbc_row_writer::OdbcRowWriter;
use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_TOTAL, SQL_NULL_DATA,
    SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ActivePlpText, STMT_STATE_CURSOR_OPEN};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::token::tokens::SqlCollation;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;

/// Implements SQLGetData for current-row retrieval.
///
/// Current scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports `SQL_C_CHAR` and `SQL_C_WCHAR` for text retrieval.
/// - Supports incremental row resume and chunked PLP retrieval via
///   `read_active_plp_bytes` + `active_plp_reached_end`.
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
    let metadata_len = if stmt_state.column_metadata.is_empty() {
        stmt_state
            .current_row
            .as_ref()
            .map(|row| row.len())
            .unwrap_or(0)
    } else {
        stmt_state.column_metadata.len()
    };
    if col_index == 0 || col_index > metadata_len {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // Continuation: app is calling SQLGetData again on the same PLP column to get
    // the next chunk. Deliver from the already-decoded text in active_plp_text.
    // Do this before borrowing current_row so we can still mutate stmt_state below.
    if stmt_state.active_plp_column == Some(col_index) {
        return deliver_plp_chunk(
            &mut stmt_state,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
    }

    // If the app jumps to a different column while a PLP stream was open —
    // incorrect usage per the ODBC spec — clear the stale stream state.
    if stmt_state.active_plp_column.is_some() {
        stmt_state.active_plp_column = None;
        stmt_state.active_plp_text = None;
    }

    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    let need_resume = row.len() < col_index && !stmt_state.current_row_complete;

    if need_resume {
        drop(stmt_state);
        let rc = resume_row_to_column(stmt, statement_handle, col_index);
        if rc != SQL_SUCCESS {
            return rc;
        }
        let Ok(mut reopened_stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned after row resume");
            return SQL_ERROR;
        };
        // Column still missing from row → the decoder paused on a PLP column.
        // Load the PLP bytes from the ResultSet stream and cache the decoded text.
        let col_in_row = reopened_stmt_state
            .current_row
            .as_ref()
            .map(|r| r.len() >= col_index)
            .unwrap_or(false);
        if !col_in_row && !reopened_stmt_state.current_row_complete {
            drop(reopened_stmt_state);
            let rc = load_plp_stream(stmt, statement_handle, col_index);
            if rc != SQL_SUCCESS {
                return rc;
            }
            let Ok(mut s) = stmt.inner.lock() else {
                error!("SQLGetData: stmt mutex poisoned after PLP load");
                return SQL_ERROR;
            };
            return deliver_plp_chunk(
                &mut s,
                col_index,
                target_type,
                target_value_ptr,
                buffer_length,
                strlen_or_ind_ptr,
            );
        }
        return write_column_as_text(
            &mut reopened_stmt_state,
            col_index,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        );
    }

    write_column_as_text(
        &mut stmt_state,
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
    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    if row.len() < col_index {
        post_sql_error(
            stmt_state,
            SQLSTATE_24000,
            0,
            "Requested column is not available in the current row",
        );
        return SQL_ERROR;
    }

    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
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

    let value = &row[col_index - 1];
    if matches!(value, ColumnValues::Null) {
        unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
        // Write a NUL terminator into the caller buffer when there's room. The
        // helper handles null `dst` and zero-length uniformly.
        if target_type == SQL_C_WCHAR {
            unsafe {
                copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]);
            }
        } else {
            unsafe {
                copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]);
            }
        }
        return SQL_SUCCESS;
    }

    let Some(as_text) = column_value_to_text(value) else {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Column type conversion not yet implemented",
        );
        return SQL_ERROR;
    };

    if target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = as_text.encode_utf16().collect();
        write_string_result(
            stmt_state,
            &utf16,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
        )
    } else {
        write_string_result(
            stmt_state,
            as_text.as_bytes(),
            target_value_ptr as *mut u8,
            buf_elements,
            strlen_or_ind_ptr,
        )
    }
}

fn resume_row_to_column(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    column_number: usize,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    let (col_count, current_row) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while preparing row resume");
            return SQL_ERROR;
        };

        let Some(row) = stmt_state.current_row.take() else {
            post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
            return SQL_ERROR;
        };

        (stmt_state.column_metadata.len(), row)
    };

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLGetData: dbc mutex poisoned while resuming row");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
            }
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
                post_diag(&mut stmt_state, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(current_row);
                post_diag(&mut stmt_state, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };

        client
    };

    let mut writer = OdbcRowWriter::from_row(current_row, col_count);
    writer.request_pause_after_column(column_number);

    let row_read = dbc.runtime.block_on(client.next_row_into(&mut writer));
    let row_complete = writer.row_complete();
    let updated_row = writer.into_row();

    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        dbc_state.active_stmt = Some(statement_handle);
    }

    match row_read {
        Ok(true) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(updated_row);
                stmt_state.current_row_complete = row_complete;
                return SQL_SUCCESS;
            }
            SQL_ERROR
        }
        Ok(false) => {
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                stmt_state.current_row = Some(updated_row);
                stmt_state.current_row_complete = true;
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
                stmt_state.current_row = None;
                stmt_state.current_row_complete = false;
                stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
            }
            SQL_ERROR
        }
    }
}

/// Drains the active PLP stream from the `ResultSet` (i.e., `TdsClient`), decodes
/// the raw bytes into a Unicode `String`, and caches the result in
/// `stmt_state.active_plp_text` so that successive `SQLGetData` calls can deliver
/// it in application-buffer-sized chunks without re-reading the wire.
///
/// This follows the same read path as `SparseCaptureWriter` in the TDS integration
/// tests: `pause_after_column` stops the decoder at the PLP column, and the caller
/// then calls `ResultSet::read_active_plp_bytes` directly — the PLP data never
/// flows through any `RowWriter::write_*` method.
fn load_plp_stream(
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    column_number: usize,
) -> SqlReturn {
    let metadata = {
        let Ok(stmt_state) = stmt.inner.lock() else {
            error!("SQLGetData: stmt mutex poisoned while reading column metadata for PLP");
            return SQL_ERROR;
        };
        stmt_state.column_metadata.get(column_number - 1).cloned()
    };

    let dbc = stmt.parent_dbc();
    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLGetData: dbc mutex poisoned while loading PLP stream");
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

    let stream_collation = client.active_plp_collation();

    // Read all remaining PLP chunks from the wire via ResultSet::read_active_plp_bytes.
    let mut plp_bytes: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match dbc
            .runtime
            .block_on(client.read_active_plp_bytes(&mut chunk))
        {
            Ok(n) => {
                if n > 0 {
                    plp_bytes.extend_from_slice(&chunk[..n]);
                }
                if client.active_plp_reached_end() {
                    break;
                }
            }
            Err(e) => {
                if let Ok(mut dbc_state) = dbc.inner.lock() {
                    dbc_state.client = Some(client);
                    dbc_state.active_stmt = Some(statement_handle);
                }
                if let Ok(mut s) = stmt.inner.lock() {
                    s.clear_state(STMT_STATE_CURSOR_OPEN);
                    post_tds_error(&mut s, &e, SQLSTATE_HY000);
                }
                return SQL_ERROR;
            }
        }
    }

    // Put client back. Its internal state is now PlpPaused(completed) — subsequent
    // calls to next_row_into will advance past the PLP column to decode further columns.
    if let Ok(mut dbc_state) = dbc.inner.lock() {
        dbc_state.client = Some(client);
        dbc_state.active_stmt = Some(statement_handle);
    }

    // Decode raw bytes to Unicode text according to the TDS column type.
    let decoded = plp_bytes_to_string(metadata.as_ref(), stream_collation, &plp_bytes);

    let Ok(mut s) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned while storing decoded PLP text");
        return SQL_ERROR;
    };
    s.active_plp_column = Some(column_number);
    s.active_plp_text = Some(ActivePlpText {
        decoded,
        target_type: None,
        offset: 0,
        collation: stream_collation,
    });
    SQL_SUCCESS
}

/// Deliver the next chunk from the cached PLP text in `active_plp_text` to the
/// caller's ODBC output buffer.
///
/// Repeated `SQLGetData` calls on a PLP column each get one buffer-sized slice.
/// Truncation is reported via `SQL_SUCCESS_WITH_INFO` + indicator `SQL_NO_TOTAL`;
/// the final chunk returns `SQL_SUCCESS` with the byte count of the data written.
fn deliver_plp_chunk(
    stmt_state: &mut crate::handles::stmt::StmtState,
    col_index: usize,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let (decoded, stream_target_type, stream_offset, stream_collation) = {
        let Some(plp) = stmt_state.active_plp_text.as_ref() else {
            post_sql_error(
                stmt_state,
                SQLSTATE_24000,
                0,
                "No active PLP stream for this column",
            );
            return SQL_ERROR;
        };
        (
            plp.decoded.clone(),
            plp.target_type,
            plp.offset,
            plp.collation,
        )
    };

    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
        );
        return SQL_ERROR;
    }

    if let Some(existing_target) = stream_target_type
        && existing_target != target_type
    {
        post_sql_error(
            stmt_state,
            SQLSTATE_HYC00,
            0,
            "Switching SQLGetData target type during PLP streaming is not supported",
        );
        return SQL_ERROR;
    }

    let mut writer = OdbcRowWriter::new(0);
    writer.set_active_plp_text(decoded, stream_collation);
    writer.set_active_plp_target_type(target_type);
    writer.set_active_plp_offset(stream_offset);
    let remaining_len = writer.active_plp_remaining_len();

    let payload_capacity = if target_type == SQL_C_WCHAR {
        (buffer_length as usize).saturating_sub(std::mem::size_of::<SqlWChar>())
    } else {
        (buffer_length as usize).saturating_sub(1)
    };

    let mut payload = vec![0u8; payload_capacity];
    let copied = match RowWriter::read_active_plp_bytes(&mut writer, &mut payload) {
        Ok(n) => n,
        Err(_) => {
            post_sql_error(stmt_state, SQLSTATE_HY000, 0, "Failed to read PLP chunk");
            return SQL_ERROR;
        }
    };

    if target_type == SQL_C_WCHAR {
        let units: Vec<u16> = payload[..copied]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let buf_elements = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
        unsafe {
            copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &units);
        }
    } else {
        unsafe {
            copy_with_nul(
                target_value_ptr as *mut u8,
                buffer_length as usize,
                &payload[..copied],
            );
        }
    }

    if RowWriter::active_plp_reached_end(&writer) {
        unsafe { write_if_some(strlen_or_ind_ptr, remaining_len as SqlLen) };
        stmt_state.active_plp_column = None;
        stmt_state.active_plp_text = None;
        return SQL_SUCCESS;
    }

    if let Some(plp) = stmt_state.active_plp_text.as_mut() {
        plp.target_type = Some(target_type);
        plp.offset = writer.active_plp_offset();
    }
    stmt_state.active_plp_column = Some(col_index);
    unsafe { write_if_some(strlen_or_ind_ptr, SQL_NO_TOTAL) };
    post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);

    SQL_SUCCESS_WITH_INFO
}
fn plp_bytes_to_string(
    metadata: Option<&ColumnMetadata>,
    stream_collation: Option<SqlCollation>,
    bytes: &[u8],
) -> String {
    let Some(meta) = metadata else {
        // No metadata: treat as UTF-16LE (most common PLP text type).
        return decode_utf16le(bytes);
    };
    match meta.data_type {
        TdsDataType::NChar | TdsDataType::NVarChar | TdsDataType::NText => decode_utf16le(bytes),
        TdsDataType::Char
        | TdsDataType::VarChar
        | TdsDataType::Text
        | TdsDataType::BigChar
        | TdsDataType::BigVarChar => {
            let encoding = stream_collation
                .or_else(|| meta.get_collation())
                .map(EncodingType::LcidBased)
                .unwrap_or(EncodingType::Utf8);
            let sql_str = SqlString::new(bytes.to_vec(), encoding);
            sql_str.to_utf8_string()
        }
        // All other PLP types (varbinary(max), xml, json …) are binary;
        // represent as a lossless Latin-1 string so column_value_to_text
        // can reformat if needed.
        _ => bytes.iter().map(|b| *b as char).collect(),
    }
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let (decoded, _, _) = encoding_rs::UTF_16LE.decode(bytes);
    decoded.into_owned()
}

/// Writes `src` to the caller's output buffer with ODBC string semantics:
/// the indicator (when present) reports the untruncated byte length, the
/// payload is NUL-terminated within the buffer, and truncation is reported via
/// SQLSTATE 01004 + `SQL_SUCCESS_WITH_INFO`.
///
/// `buf_elements` is the buffer capacity in units of `T` (not bytes).
///
/// The caller-provided pointers are written through small `unsafe` blocks
/// inside this function; both pointer arguments are obligations of the FFI
/// caller (validated against the buffer length passed by the DM).
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

fn column_value_to_text(v: &ColumnValues) -> Option<String> {
    match v {
        ColumnValues::TinyInt(x) => Some(x.to_string()),
        ColumnValues::SmallInt(x) => Some(x.to_string()),
        ColumnValues::Int(x) => Some(x.to_string()),
        ColumnValues::BigInt(x) => Some(x.to_string()),
        ColumnValues::Real(x) => Some(x.to_string()),
        ColumnValues::Float(x) => Some(x.to_string()),
        ColumnValues::Bit(x) => Some(if *x { "1".into() } else { "0".into() }),
        ColumnValues::String(s) => Some(s.to_utf8_string()),
        ColumnValues::Uuid(u) => Some(u.to_string()),
        ColumnValues::Null => Some(String::new()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::SqlString;

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
    }

    #[test]
    fn get_data_string_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "hello".to_string(),
            ))]);
        }

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
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 5);
        assert_eq!(std::str::from_utf8(&buf[..5]).unwrap(), "hello");
    }

    #[test]
    fn get_data_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        let mut buf = [0u8; 3];
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
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
    }

    #[test]
    fn get_data_empty_string_zero_buffer_no_truncation() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                String::new(),
            ))]);
        }

        let mut ind: SqlLen = -1;
        let ret = unsafe { sql_get_data(stmt, 1, SQL_C_CHAR, std::ptr::null_mut(), 0, &mut ind) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 0);
    }

    #[test]
    fn get_data_null_column_writes_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0u8; 4];
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
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
    }

    #[test]
    fn get_data_unsupported_target_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_LONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_invalid_column_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                2,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    /// Helper: read a NUL-terminated UTF-16 buffer back to a Rust String.
    fn read_until_nul(buf: &[u16]) -> String {
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..len]).unwrap()
    }

    #[test]
    fn get_data_wchar_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "héllo".to_string(),
            ))]);
        }

        let mut buf = [0u16; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        // Indicator is byte length of untruncated value, excluding NUL.
        // "héllo" → 5 u16 units → 10 bytes.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "héllo");
    }

    #[test]
    fn get_data_wchar_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        // 3 u16 slots = 6 bytes. "12345" needs 6 units (5 chars + NUL) → truncated.
        let mut buf = [0u16; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        // Untruncated byte length: 5 chars × 2 bytes = 10.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "12");
    }

    #[test]
    fn get_data_wchar_null_column_writes_nul_and_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0xDEADu16; 4];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
        // First slot must be NUL; nothing else touched.
        assert_eq!(buf[0], 0);
        assert_eq!(&buf[1..], &[0xDEAD; 3]);
    }
}
