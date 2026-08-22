// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFetchScroll: block fetch of a rowset into the columns
//! bound by `SQLBindCol`.
//!
//! This is the columnar path `mssql-python` uses for `fetchmany` / `fetchall`:
//! `SQL_ATTR_ROW_ARRAY_SIZE` rows are pulled in one call and written into the
//! application's per-column arrays, with `*rows_fetched_ptr` reporting how many
//! arrived and the row status array reporting each row's outcome.
//!
//! Only `SQL_FETCH_NEXT` is served — the cursor is forward-only — and only
//! column-wise binding, which is the ODBC default and what `mssql-python` uses.
//!
//! Values are converted by the same core `SQLGetData` uses, so a column reads
//! the same either way. The difference is cadence: `SQLGetData` may return a
//! long value in chunks across repeated calls, whereas a bound column gets one
//! shot at a fixed-size buffer and reports `01004` if the value does not fit.

use tracing::{debug, error};

use mssql_tds::connection::tds_client::CursorColumn;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::Error as TdsError;

use super::sqlstate::*;
use crate::api::get_data::{TextError, column_value_to_text, convert_typed_c, is_typed_c_target};
use crate::api::odbc_types::{
    SQL_BIND_BY_COLUMN, SQL_C_BIT, SQL_C_CHAR, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID,
    SQL_C_SBIGINT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_ERROR,
    SQL_FETCH_NEXT, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NULL_DATA, SQL_ROW_ERROR, SQL_ROW_NOROW,
    SQL_ROW_SUCCESS, SQL_ROW_SUCCESS_WITH_INFO, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlHandle,
    SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlULen, SqlUSmallInt, SqlWChar,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::conversion::error::{ConvError, ConvOk};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{
    ColumnBinding, STMT_STATE_CURSOR_OPEN, STMT_STATE_FETCH_IN_PROGRESS, StmtState,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements SQLFetchScroll for the current forward-only result set.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_fetch_scroll(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        fetch_orientation, fetch_offset, "SQLFetchScroll called"
    );
    crate::ffi_entry!("SQLFetchScroll", unsafe {
        sql_fetch_scroll_impl(statement_handle, fetch_orientation, fetch_offset)
    })
}

pub(crate) unsafe fn sql_fetch_scroll_impl(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFetchScroll: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    fetch_scroll_safe(statement_handle, stmt, fetch_orientation, fetch_offset)
}

/// Why a bound column write did not land exactly, so the row can report the
/// same SQLSTATE `SQLGetData` would have for the identical value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowIssue {
    /// 01004 — the value did not fit the bound buffer.
    StringTruncated,
    /// 01S07 — fractional digits were dropped to fit the target.
    FractionalTruncated,
    /// 22003 — numeric value out of the target's range.
    OutOfRange,
    /// 07006 — the source type cannot convert to the requested target.
    Restricted,
    /// 22018 — the payload is not a valid literal for the target.
    InvalidCharacter,
    /// 22002 — NULL arrived with no indicator to report it through.
    IndicatorRequired,
    /// HYC00 — a target or source this driver does not deliver yet.
    Unsupported,
}

impl RowIssue {
    fn post(self, stmt_state: &mut StmtState) {
        match self {
            RowIssue::StringTruncated => post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION),
            RowIssue::FractionalTruncated => post_diag(stmt_state, WARN_FRACTIONAL_TRUNCATION),
            RowIssue::OutOfRange => post_diag(stmt_state, ERR_NUMERIC_OUT_OF_RANGE),
            RowIssue::Restricted => post_diag(stmt_state, ERR_RESTRICTED_DATA_TYPE),
            RowIssue::InvalidCharacter => post_diag(stmt_state, ERR_INVALID_CHARACTER_VALUE),
            RowIssue::IndicatorRequired => post_diag(stmt_state, ERR_INDICATOR_REQUIRED),
            RowIssue::Unsupported => post_sql_error(
                stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            ),
        }
    }
}

/// The per-row outcome recorded in the row status array.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowOutcome {
    Success,
    Info(RowIssue),
    Error(RowIssue),
}

impl RowOutcome {
    fn status(self) -> SqlUSmallInt {
        match self {
            RowOutcome::Success => SQL_ROW_SUCCESS,
            RowOutcome::Info(_) => SQL_ROW_SUCCESS_WITH_INFO,
            RowOutcome::Error(_) => SQL_ROW_ERROR,
        }
    }

    fn issue(self) -> Option<RowIssue> {
        match self {
            RowOutcome::Success => None,
            RowOutcome::Info(i) | RowOutcome::Error(i) => Some(i),
        }
    }

    /// Keeps the worst outcome seen while filling one row, so a row that both
    /// truncated one column and failed another reports the failure.
    fn merge(self, other: RowOutcome) -> RowOutcome {
        match (self, other) {
            (e @ RowOutcome::Error(_), _) => e,
            (_, e @ RowOutcome::Error(_)) => e,
            (i @ RowOutcome::Info(_), _) => i,
            (_, i @ RowOutcome::Info(_)) => i,
            _ => RowOutcome::Success,
        }
    }
}

fn fetch_scroll_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    fetch_orientation: SqlSmallInt,
    _fetch_offset: SqlLen,
) -> SqlReturn {
    // Snapshot the rowset controls and the binding table, then release the
    // statement lock: the fill loop below blocks on the network and must not
    // hold it. The application is not allowed to rebind concurrently with a
    // fetch on the same statement, so the snapshot cannot go stale under us.
    let (
        row_array_size,
        bindings,
        rows_fetched_ptr,
        row_status_ptr,
        column_count,
        row_bind_offset_ptr,
    ) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFetchScroll: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if fetch_orientation != SQL_FETCH_NEXT {
            error!(
                fetch_orientation,
                "SQLFetchScroll: only SQL_FETCH_NEXT is supported on a forward-only cursor"
            );
            post_diag(&mut stmt_state, ERR_FETCH_TYPE_OUT_OF_RANGE);
            return SQL_ERROR;
        }
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            error!("SQLFetchScroll: no open cursor on this statement");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
            error!("SQLFetchScroll: a fetch is already in progress on this statement");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
        if stmt_state.row_bind_type != SQL_BIND_BY_COLUMN {
            error!(
                row_bind_type = stmt_state.row_bind_type,
                "SQLFetchScroll: row-wise binding is not implemented"
            );
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Row-wise binding is not yet implemented",
            );
            return SQL_ERROR;
        }

        let bindings: Vec<ColumnBinding> = stmt_state.bindings.clone();
        // The buffers in that snapshot belong to the application, and the fill
        // loop writes through them after this lock is released. Claiming the
        // statement here is what stops a concurrent SQLBindCol from freeing one
        // mid-write; the mutating entry points refuse while this is set.
        stmt_state.set_state(STMT_STATE_FETCH_IN_PROGRESS);
        (
            stmt_state.row_array_size,
            bindings,
            stmt_state.rows_fetched_ptr,
            stmt_state.row_status_ptr,
            stmt_state.column_metadata.len(),
            stmt_state.row_bind_offset_ptr,
        )
    };

    let rc = fill_rowset(
        statement_handle,
        stmt,
        &bindings,
        row_array_size,
        column_count,
        rows_fetched_ptr,
        row_status_ptr,
        row_bind_offset_ptr,
    );

    // Single clearing point for the guard, so every early return inside the
    // fill loop still releases it.
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.clear_state(STMT_STATE_FETCH_IN_PROGRESS);
    }
    debug!(?rc, "SQLFetchScroll returning");
    rc
}

#[allow(clippy::too_many_arguments)]
fn fill_rowset(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    bindings: &[ColumnBinding],
    row_array_size: SqlULen,
    column_count: usize,
    rows_fetched_ptr: *mut SqlULen,
    row_status_ptr: *mut SqlUSmallInt,
    row_bind_offset_ptr: *mut SqlULen,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned");
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }

        if dbc_state.active_stmt.is_none() {
            // Already drained by an earlier fetch; the cursor stays open until
            // it is explicitly closed, so this is SQL_NO_DATA rather than an
            // error. Report a zero-row rowset so the caller sees the count.
            drop(dbc_state);
            unsafe { write_if_some(rows_fetched_ptr, 0) };
            mark_no_rows(row_status_ptr, 0, row_array_size);
            debug!("SQLFetchScroll: cursor already drained; returning SQL_NO_DATA");
            return SQL_NO_DATA;
        }

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };
        client
    };

    // A no-row statement result (DDL / DML / PRINT) is positioned with zero
    // columns; there is nothing to fetch, so 24000 matches SQLFetch.
    if column_count == 0 {
        error!("SQLFetchScroll: current result has no columns (no-row statement)");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
        }
        if let Ok(mut ss) = stmt.inner.lock() {
            post_diag(&mut ss, ERR_INVALID_CURSOR_STATE);
        }
        return SQL_ERROR;
    }

    let mut rows_filled: SqlULen = 0;
    let mut worst = RowOutcome::Success;
    let mut fetch_error: Option<TdsError> = None;
    let mut last_column_read = 0usize;

    // Read once per fetch, not once per bind, so an application can move the
    // whole rowset between calls by updating the pointed-to value.
    let bind_offset = unsafe { read_bind_offset(row_bind_offset_ptr) };

    while rows_filled < row_array_size {
        match dbc.runtime.block_on(client.next_row_cursor()) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                fetch_error = Some(e);
                break;
            }
        }

        let mut outcome = RowOutcome::Success;
        let mut columns_read = 0usize;
        for binding in bindings {
            let column = binding.column_number as usize;
            if column == 0 || column > column_count {
                // msodbcsql skips a binding whose ordinal is past the end of
                // this result set and reports nothing -- a binding left over
                // from a wider one is not an error there, so it is not one
                // here either.
                continue;
            }
            let pulled = dbc.runtime.block_on(client.read_row_column(column - 1));
            columns_read = column;
            let result = match pulled {
                Ok(CursorColumn::Value { value, .. }) => unsafe {
                    deliver_bound(binding, rows_filled as usize, bind_offset, &value)
                },
                // A bound long/LOB column would have to be drained into the
                // fixed buffer here; that path is owned by SQLGetData today, so
                // report the row rather than deliver a wrong value (AB#47361).
                // Abandoning the stream is safe: the next `read_row_column`
                // finishes off a paused PLP value before it decodes anything.
                Ok(CursorColumn::PlpStreaming { .. }) => RowOutcome::Error(RowIssue::Unsupported),
                // Reading ascending and once per column, neither of these is
                // reachable; treat them as a row error rather than assuming.
                Ok(CursorColumn::RowEnded) | Ok(CursorColumn::AlreadyConsumed) => {
                    RowOutcome::Error(RowIssue::Restricted)
                }
                Err(e) => {
                    fetch_error = Some(e);
                    RowOutcome::Error(RowIssue::Restricted)
                }
            };
            outcome = outcome.merge(result);
            if fetch_error.is_some() {
                break;
            }
        }
        last_column_read = columns_read;

        unsafe { write_row_status(row_status_ptr, rows_filled, outcome.status()) };
        worst = worst.merge(outcome);
        rows_filled += 1;

        if fetch_error.is_some() {
            break;
        }
    }

    // A zero-row end of set returns SQL_NO_DATA, which cannot carry
    // SQL_SUCCESS_WITH_INFO, so anything drained here would be posted under a
    // code most applications never inspect and cleared by the next call. Leave
    // those messages on the client for SQLMoreResults or the cursor close to
    // surface, exactly as SQLFetch does.
    let info_messages = if rows_filled > 0 || fetch_error.is_some() {
        client.take_info_messages()
    } else {
        Vec::new()
    };

    // Hand the connection back before touching the statement, mirroring
    // SQLFetch's lock order.
    {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned returning client");
            return SQL_ERROR;
        };
        dbc_state.client = Some(client);
        // A failed protocol read leaves the connection with no usable cursor,
        // so it stops being busy with this statement; otherwise it stays busy
        // until SQLMoreResults or a cursor close, matching SQLFetch.
        if fetch_error.is_some() {
            if dbc_state.active_stmt == Some(statement_handle) {
                dbc_state.active_stmt = None;
            }
        } else {
            dbc_state.active_stmt = Some(statement_handle);
        }
    }

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFetchScroll: stmt mutex poisoned recording rowset");
        return SQL_ERROR;
    };

    unsafe { write_if_some(rows_fetched_ptr, rows_filled) };
    mark_no_rows(row_status_ptr, rows_filled, row_array_size);

    if let Some(e) = fetch_error {
        error!(%e, "SQLFetchScroll: row fetch failed");
        // The cursor cannot be resumed after a protocol failure, so tear the
        // row stream down rather than leaving it addressable.
        stmt_state.reset_row_stream();
        stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
        // Fans a server error out into one diagnostic per record, keeping each
        // SQLSTATE and native error rather than flattening them into HY000.
        post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        post_tds_info_messages(&mut stmt_state, &info_messages);
        return SQL_ERROR;
    }

    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);

    if rows_filled == 0 {
        stmt_state.reset_row_stream();
        debug!("SQLFetchScroll: end of rowset");
        return SQL_NO_DATA;
    }

    // Mixed SQLGetData access is only well defined when the rowset holds a
    // single row. With a wider rowset ODBC expects SQLSetPos to nominate the
    // current row first, and that is not implemented, so leave the cursor
    // unpositioned rather than silently handing back the last row of the block.
    if row_array_size == 1 {
        // The cursor is left on the row just read, with the bound columns
        // already consumed, so a following SQLGetData continues from there
        // rather than re-reading a column the fill loop took.
        stmt_state.begin_row();
        stmt_state.current_row_last_col = last_column_read;
    } else {
        stmt_state.reset_row_stream();
    }

    // Report why the rowset was imperfect with the SQLSTATE that value would
    // have produced through SQLGetData, rather than a blanket truncation
    // warning. Per-row detail lives in the row status array.
    // msodbcsql keys the return code on the rowset size, not on how many rows
    // failed: a block fetch demotes a row error to SQL_SUCCESS_WITH_INFO and
    // leaves the detail in the row status array, while a single-row fetch lets
    // the error stand (`sqlccurs.cpp`, gated on dwRowSize > 1).
    if let Some(issue) = worst.issue() {
        issue.post(&mut stmt_state);
        if row_array_size == 1 && matches!(worst, RowOutcome::Error(_)) {
            return SQL_ERROR;
        }
        return SQL_SUCCESS_WITH_INFO;
    }
    if has_server_info {
        return SQL_SUCCESS_WITH_INFO;
    }
    SQL_SUCCESS
}

/// Writes `SQL_ROW_NOROW` into the unused tail of the row status array.
fn mark_no_rows(row_status_ptr: *mut SqlUSmallInt, from: SqlULen, row_array_size: SqlULen) {
    if row_status_ptr.is_null() {
        return;
    }
    for i in from..row_array_size {
        unsafe { write_row_status(row_status_ptr, i, SQL_ROW_NOROW) };
    }
}

/// # Safety
/// `row_status_ptr` must be null or valid for `row_array_size` elements.
unsafe fn write_row_status(row_status_ptr: *mut SqlUSmallInt, row: SqlULen, status: SqlUSmallInt) {
    if row_status_ptr.is_null() {
        return;
    }
    unsafe { row_status_ptr.add(row).write_unaligned(status) };
}

/// Byte stride between consecutive elements of a column-wise bound array.
///
/// ODBC ignores `BufferLength` for a fixed-width target — an application may
/// legitimately pass anything, including 0 — so the stride there comes from the
/// C type. Only the character and binary targets are sized by the application.
fn element_stride(target_type: SqlSmallInt, buffer_length: SqlLen) -> usize {
    match target_type {
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_STINYINT | SQL_C_UTINYINT => 1,
        SQL_C_SSHORT | SQL_C_USHORT => 2,
        SQL_C_SLONG | SQL_C_ULONG | SQL_C_FLOAT => 4,
        SQL_C_SBIGINT | SQL_C_UBIGINT | SQL_C_DOUBLE => 8,
        SQL_C_GUID => 16,
        SQL_C_TYPE_DATE | SQL_C_TYPE_TIME => 6,
        SQL_C_TYPE_TIMESTAMP => 16,
        SQL_C_SS_TIME2 => 12,
        SQL_C_SS_TIMESTAMPOFFSET => 20,
        // Character and binary: the application sizes the slot.
        _ => buffer_length.max(0) as usize,
    }
}

/// Current value of `SQL_ATTR_ROW_BIND_OFFSET_PTR`, in bytes.
///
/// Read unaligned: the offset displaces application pointers by an arbitrary
/// byte count, so nothing guarantees the result is aligned for `SqlULen`, and a
/// misaligned `read` is UB in Rust on every target rather than merely slow.
///
/// # Safety
/// `ptr` must be null or point to a readable `SqlULen`.
unsafe fn read_bind_offset(ptr: *mut SqlULen) -> usize {
    if ptr.is_null() {
        return 0;
    }
    unsafe { ptr.read_unaligned() }
}

/// Writes one column value into its bound buffer slot for row `row_index`.
///
/// # Safety
/// The binding's pointers, displaced by `bind_offset`, must address at least
/// `row_index + 1` elements — the contract `SQLBindCol` places on the
/// application together with `SQL_ATTR_ROW_ARRAY_SIZE`.
unsafe fn deliver_bound(
    binding: &ColumnBinding,
    row_index: usize,
    bind_offset: usize,
    value: &ColumnValues,
) -> RowOutcome {
    // A zero stride only arises from a character or binary binding with
    // BufferLength 0, which msodbcsql treats as a length probe: the indicator
    // gets the available length and the buffer is left alone. The copy below
    // already does exactly that, so this is not rejected up front.
    let stride = element_stride(binding.target_type, binding.buffer_length);

    // SQL_ATTR_ROW_BIND_OFFSET_PTR displaces both bases by the same byte count.
    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            (binding.strlen_or_ind_ptr as *mut u8)
                .add(bind_offset)
                .cast::<SqlLen>()
                .add(row_index)
        }
    };

    let is_null = matches!(value, ColumnValues::Null);
    if is_null && indicator.is_null() {
        // There is nowhere to report the NULL, and leaving the slot untouched
        // would read back as the previous row's value.
        return RowOutcome::Error(RowIssue::IndicatorRequired);
    }

    let slot =
        unsafe { (binding.target_value_ptr as *mut u8).add(bind_offset + row_index * stride) };

    if is_null {
        unsafe { write_if_some(indicator, SQL_NULL_DATA) };
        // A character target still gets a terminator so the slot does not read
        // back as whatever the previous row left there.
        let buf_elements = char_buf_elements(binding.target_type, stride);
        if binding.target_type == SQL_C_WCHAR {
            unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &[]) };
        } else if binding.target_type == SQL_C_CHAR {
            unsafe { copy_with_nul(slot, buf_elements, &[]) };
        }
        return RowOutcome::Success;
    }

    if is_typed_c_target(binding.target_type) {
        let converted =
            unsafe { convert_typed_c(value, binding.target_type, slot as SqlPointer, indicator) };
        return match converted {
            Ok(ConvOk::Exact) => RowOutcome::Success,
            Ok(ConvOk::Truncated) => RowOutcome::Info(RowIssue::FractionalTruncated),
            Err(ConvError::OutOfRange) => RowOutcome::Error(RowIssue::OutOfRange),
            Err(ConvError::Restricted) => RowOutcome::Error(RowIssue::Restricted),
            Err(ConvError::InvalidCharacterValue) => RowOutcome::Error(RowIssue::InvalidCharacter),
            Err(ConvError::NotHandledHere) => RowOutcome::Error(RowIssue::Unsupported),
        };
    }

    if binding.target_type != SQL_C_CHAR && binding.target_type != SQL_C_WCHAR {
        // SQL_C_BINARY delivery is still unimplemented (AB#47239); anything else
        // is an unsupported target.
        return RowOutcome::Error(RowIssue::Unsupported);
    }

    let text = match column_value_to_text(value) {
        Ok(t) => t,
        Err(TextError::Malformed) => return RowOutcome::Error(RowIssue::InvalidCharacter),
        Err(TextError::Unsupported) => return RowOutcome::Error(RowIssue::Unsupported),
    };

    let buf_elements = char_buf_elements(binding.target_type, stride);
    if binding.target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe { write_if_some(indicator, (utf16.len() * 2) as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &utf16) };
        if truncated {
            return RowOutcome::Info(RowIssue::StringTruncated);
        }
    } else {
        let bytes = text.as_bytes();
        unsafe { write_if_some(indicator, bytes.len() as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot, buf_elements, bytes) };
        if truncated {
            return RowOutcome::Info(RowIssue::StringTruncated);
        }
    }
    RowOutcome::Success
}

/// Capacity of one bound slot in target elements, so a `SQL_C_WCHAR` buffer is
/// measured in UTF-16 code units rather than bytes.
fn char_buf_elements(target_type: SqlSmallInt, stride: usize) -> usize {
    if target_type == SQL_C_WCHAR {
        stride / std::mem::size_of::<SqlWChar>()
    } else {
        stride
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_C_BINARY, SQL_C_SLONG, SQL_FETCH_ABSOLUTE, SQL_FETCH_FIRST, SQL_FETCH_LAST,
        SQL_FETCH_PRIOR, SQL_FETCH_RELATIVE,
    };
    use crate::api::sqlstate::SQLSTATE_HY106;
    use crate::api::sqlstate::{ERR_CONNECTION_BUSY, SQLSTATE_24000, SQLSTATE_HY000};
    use crate::handles::dbc::DbcHandle;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
    use mssql_tds::test_client_support::int_columns;

    fn binding(
        column_number: SqlUSmallInt,
        target_type: SqlSmallInt,
        target_value_ptr: SqlPointer,
        buffer_length: SqlLen,
        strlen_or_ind_ptr: *mut SqlLen,
    ) -> ColumnBinding {
        ColumnBinding {
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        }
    }

    fn open_cursor(h: &TestHandles) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt.inner.lock().unwrap();
        s.set_state(STMT_STATE_CURSOR_OPEN);
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe { sql_fetch_scroll(ptr::null_mut(), SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    /// The cursor is forward-only, so every other orientation is rejected
    /// rather than silently treated as SQL_FETCH_NEXT.
    #[test]
    fn only_fetch_next_is_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        for orientation in [
            SQL_FETCH_FIRST,
            SQL_FETCH_LAST,
            SQL_FETCH_PRIOR,
            SQL_FETCH_ABSOLUTE,
            SQL_FETCH_RELATIVE,
        ] {
            let rc = unsafe { sql_fetch_scroll(h.stmt, orientation, 0) };
            assert_eq!(rc, SQL_ERROR, "orientation {orientation}");
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = stmt.inner.lock().unwrap();
            assert_eq!(s.diag_records.last().unwrap().sql_state, SQLSTATE_HY106);
        }
    }

    #[test]
    fn fetch_without_an_open_cursor_is_a_cursor_state_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_INVALID_CURSOR_STATE.state
        );
    }

    /// Row-wise binding is not implemented, and reporting HYC00 is better than
    /// filling the application's struct array as if it were column-wise.
    #[test]
    fn row_wise_binding_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.row_bind_type = 64; // a row-struct size
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(s.diag_records.last().unwrap().sql_state, *b"HYC00");
    }

    /// An application may pass BufferLength 0 for a fixed-width target, so the
    /// stride has to come from the C type in that case.
    #[test]
    fn element_stride_falls_back_to_the_c_type_size() {
        assert_eq!(element_stride(SQL_C_SLONG, 0), 4);
        assert_eq!(element_stride(SQL_C_SBIGINT, 0), 8);
        assert_eq!(element_stride(SQL_C_GUID, 0), 16);
        assert_eq!(element_stride(SQL_C_TYPE_TIMESTAMP, 0), 16);
        assert_eq!(element_stride(SQL_C_SS_TIMESTAMPOFFSET, 0), 20);
        // An explicit buffer length always wins.
        assert_eq!(element_stride(SQL_C_SLONG, 4), 4);
        assert_eq!(element_stride(SQL_C_CHAR, 32), 32);
        // A character target with no buffer length has nowhere to write.
        assert_eq!(element_stride(SQL_C_CHAR, 0), 0);
    }

    /// Each row lands at its own offset in the bound array, which is the whole
    /// point of a block fetch.
    #[test]
    fn bound_values_land_at_their_row_offset() {
        let mut buf = [0i32; 4];
        let mut ind = [0isize as SqlLen; 4];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        for (row, value) in [10i32, 20, 30].iter().enumerate() {
            let outcome = unsafe { deliver_bound(&b, row, 0, &ColumnValues::Int(*value)) };
            assert!(matches!(outcome, RowOutcome::Success));
        }
        assert_eq!(buf, [10, 20, 30, 0]);
        assert_eq!(ind[0], 4);
    }

    /// NULL is reported through the indicator; the data slot is left alone for
    /// a fixed-width target.
    #[test]
    fn null_is_reported_through_the_indicator() {
        let mut buf = [7i32; 2];
        let mut ind = [0 as SqlLen; 2];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&b, 1, 0, &ColumnValues::Null) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(ind[1], SQL_NULL_DATA);
        assert_eq!(buf[1], 7, "a NULL must not disturb the data slot");
    }

    /// A bound column gets one shot at a fixed buffer, so an over-long value is
    /// truncated and reported rather than chunked the way SQLGetData does it.
    #[test]
    fn character_data_is_truncated_to_the_bound_buffer() {
        let mut buf = [0u8; 8];
        let mut ind = [0 as SqlLen; 1];
        let b = binding(
            1,
            SQL_C_CHAR,
            buf.as_mut_ptr() as SqlPointer,
            8,
            ind.as_mut_ptr(),
        );
        let value = ColumnValues::Int(1234567890);
        let outcome = unsafe { deliver_bound(&b, 0, 0, &value) };
        assert!(matches!(
            outcome,
            RowOutcome::Info(RowIssue::StringTruncated)
        ));
        // The indicator reports the untruncated length.
        assert_eq!(ind[0], 10);
        assert_eq!(&buf[..7], b"1234567");
        assert_eq!(buf[7], 0, "the buffer stays NUL-terminated");
    }

    /// A zero-length character binding is a length probe, as it is for
    /// SQLGetData: report what is available, write nothing, flag truncation.
    #[test]
    fn a_zero_length_character_binding_probes_the_length() {
        let mut buf = [b'#'; 4];
        let mut ind = [-999 as SqlLen; 1];
        let b = binding(
            1,
            SQL_C_CHAR,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );

        let value = ColumnValues::String(SqlString::new(b"hello".to_vec(), EncodingType::Utf8));
        let outcome = unsafe { deliver_bound(&b, 0, 0, &value) };

        assert!(matches!(
            outcome,
            RowOutcome::Info(RowIssue::StringTruncated)
        ));
        assert_eq!(ind[0], 5, "the available length is reported");
        assert_eq!(buf, [b'#'; 4], "a zero-length buffer is never written");
    }

    /// A drained cursor is not an error: the result set ended on an earlier
    /// fetch and the cursor stays open until it is explicitly closed. The rowset
    /// counters still have to be written so the caller sees zero rows.
    #[test]
    fn fetch_after_the_cursor_drained_reports_an_empty_rowset() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        let mut rows_fetched: SqlULen = 999;
        let mut status = [SQL_ROW_SUCCESS; 3];
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.row_array_size = 3;
            s.rows_fetched_ptr = &mut rows_fetched;
            s.row_status_ptr = status.as_mut_ptr();
            s.column_metadata = int_columns(1);
        }
        // active_stmt stays None: an earlier fetch drained the connection.
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_NO_DATA);
        assert_eq!(rows_fetched, 0);
        assert_eq!(status, [SQL_ROW_NOROW; 3]);
    }

    /// The connection can only serve one statement's results at a time.
    #[test]
    fn fetch_while_another_statement_owns_the_connection_is_rejected() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        open_cursor(&h);
        {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut d = dbc.inner.lock().unwrap();
            d.active_stmt = Some(other_stmt);
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_CONNECTION_BUSY.state
        );
    }

    /// A statement positioned on a no-row result (DDL / DML / PRINT) has no
    /// columns to fetch, which is 24000 rather than an empty rowset.
    #[test]
    fn fetch_on_a_no_column_result_is_a_cursor_state_error() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut d = dbc.inner.lock().unwrap();
            d.active_stmt = Some(h.stmt);
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        // No TDS client is attached either, so the no-client guard fires first;
        // both are cursor-state failures rather than a silent empty rowset.
        assert!(matches!(
            s.diag_records.last().unwrap().sql_state,
            SQLSTATE_HY000 | SQLSTATE_24000
        ));
    }

    /// The exported entry point is what the Driver Manager calls, so it needs
    /// its own guard against a null handle.
    #[test]
    fn the_exported_entry_point_rejects_a_null_handle() {
        let rc =
            unsafe { crate::api::exports::SQLFetchScroll(std::ptr::null_mut(), SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    /// ODBC ignores BufferLength for a fixed-width target, so the stride has to
    /// come from the C type. Honouring a bogus length would place later rows
    /// outside the application's array.
    #[test]
    fn fixed_width_stride_ignores_the_buffer_length() {
        // A caller that passes the whole array size rather than one element.
        assert_eq!(element_stride(SQL_C_SLONG, 400), 4);
        assert_eq!(element_stride(SQL_C_SBIGINT, 1), 8);
        assert_eq!(element_stride(SQL_C_TYPE_TIMESTAMP, 999), 16);
        // Character and binary targets are sized by the application.
        assert_eq!(element_stride(SQL_C_CHAR, 32), 32);
        assert_eq!(element_stride(SQL_C_WCHAR, 64), 64);
        // A negative length cannot become a huge stride.
        assert_eq!(element_stride(SQL_C_CHAR, -8), 0);
    }

    /// NULL with nowhere to report it is 22002: leaving the slot untouched
    /// would read back as the previous row's value with no way to tell.
    #[test]
    fn null_without_an_indicator_is_an_error() {
        let mut buf = [7i32; 1];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ptr::null_mut(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::Null) };
        assert_eq!(outcome.issue(), Some(RowIssue::IndicatorRequired));
        assert_eq!(outcome.status(), SQL_ROW_ERROR);
        assert_eq!(
            buf[0], 7,
            "the stale value is left visible, not overwritten"
        );
    }

    /// A non-NULL value still delivers without an indicator; only NULL needs one.
    #[test]
    fn a_value_without_an_indicator_still_delivers() {
        let mut buf = [0i32; 1];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ptr::null_mut(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::Int(42)) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(buf[0], 42);
    }

    /// SQL_ATTR_ROW_BIND_OFFSET_PTR displaces the data and indicator bases by
    /// the same byte count, so the application can move a whole rowset.
    #[test]
    fn the_bind_offset_displaces_both_bases() {
        let mut buf = [0i32; 4];
        let mut ind = [0 as SqlLen; 4];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        // A whole-rowset displacement, which is what the attribute is for: a
        // byte count that leaves both arrays naturally aligned.
        let offset = std::mem::size_of::<SqlLen>();
        let outcome = unsafe { deliver_bound(&b, 0, offset, &ColumnValues::Int(99)) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(buf[0], 0, "the offset must skip past the first slots");
        assert_eq!(buf[offset / std::mem::size_of::<i32>()], 99);
        assert_eq!(ind[0], 0, "the indicator base moves too");
        assert_eq!(ind[offset / std::mem::size_of::<SqlLen>()], 4);
    }

    #[test]
    fn a_zero_offset_reads_as_zero_from_a_null_pointer() {
        assert_eq!(unsafe { read_bind_offset(ptr::null_mut()) }, 0);
        let mut value: SqlULen = 24;
        assert_eq!(unsafe { read_bind_offset(&mut value) }, 24);
    }

    /// Each conversion failure keeps its own SQLSTATE rather than being
    /// flattened into one truncation warning.
    #[test]
    fn conversion_failures_map_to_their_own_sqlstate() {
        let mut buf = [0u8; 1];
        let mut ind = [0 as SqlLen; 1];
        // A bigint that cannot fit a tinyint target is 22003.
        let b = binding(
            1,
            SQL_C_TINYINT,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::BigInt(i64::MAX)) };
        assert_eq!(outcome.issue(), Some(RowIssue::OutOfRange));

        // A target this driver does not deliver is HYC00, not a truncation.
        let mut bin = [0u8; 8];
        let unsupported = binding(
            1,
            SQL_C_BINARY,
            bin.as_mut_ptr() as SqlPointer,
            8,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&unsupported, 0, 0, &ColumnValues::Int(1)) };
        assert_eq!(outcome.issue(), Some(RowIssue::Unsupported));
    }

    /// Each issue posts the SQLSTATE the same value would have produced through
    /// SQLGetData.
    #[test]
    fn each_issue_posts_its_own_sqlstate() {
        let cases: &[(RowIssue, [u8; 5])] = &[
            (RowIssue::StringTruncated, *b"01004"),
            (RowIssue::FractionalTruncated, *b"01S07"),
            (RowIssue::OutOfRange, *b"22003"),
            (RowIssue::Restricted, *b"07006"),
            (RowIssue::InvalidCharacter, *b"22018"),
            (RowIssue::IndicatorRequired, *b"22002"),
            (RowIssue::Unsupported, *b"HYC00"),
        ];
        for (issue, state) in cases {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            issue.post(&mut s);
            assert_eq!(
                s.diag_records.last().unwrap().sql_state,
                *state,
                "{issue:?}"
            );
        }
    }

    /// The rows the fetch did not fill must be marked, or the application reads
    /// stale statuses from a previous, longer rowset.
    #[test]
    fn unfilled_rows_are_marked_norow() {
        let mut status = [SQL_ROW_SUCCESS; 5];
        mark_no_rows(status.as_mut_ptr(), 2, 5);
        assert_eq!(
            status,
            [
                SQL_ROW_SUCCESS,
                SQL_ROW_SUCCESS,
                SQL_ROW_NOROW,
                SQL_ROW_NOROW,
                SQL_ROW_NOROW
            ]
        );
    }

    #[test]
    fn row_status_and_outcome_merge_keeps_the_worst() {
        let info = RowOutcome::Info(RowIssue::StringTruncated);
        let err = RowOutcome::Error(RowIssue::OutOfRange);
        assert_eq!(RowOutcome::Success.status(), SQL_ROW_SUCCESS);
        assert_eq!(info.status(), SQL_ROW_SUCCESS_WITH_INFO);
        assert_eq!(err.status(), SQL_ROW_ERROR);
        assert!(matches!(
            RowOutcome::Success.merge(info),
            RowOutcome::Info(_)
        ));
        assert!(matches!(info.merge(err), RowOutcome::Error(_)));
        assert!(matches!(
            err.merge(RowOutcome::Success),
            RowOutcome::Error(_)
        ));
        // An issue survives being merged with a clean row from either side.
        assert!(matches!(
            info.merge(RowOutcome::Success),
            RowOutcome::Info(_)
        ));
        assert!(matches!(
            RowOutcome::Success.merge(RowOutcome::Success),
            RowOutcome::Success
        ));
        // The reason survives the merge so the statement can report it.
        assert_eq!(info.merge(err).issue(), Some(RowIssue::OutOfRange));
        assert_eq!(RowOutcome::Success.issue(), None);
    }
}
