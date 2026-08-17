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

use super::sqlstate::*;
use crate::conversion::fetch_convert::{ConvError, ConvOk};
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
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ColumnBinding, STMT_STATE_CURSOR_OPEN};
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
        fetch_orientation, "SQLFetchScroll called"
    );
    crate::ffi_entry!("SQLFetchScroll", unsafe {
        sql_fetch_scroll_impl(statement_handle, fetch_orientation, fetch_offset)
    })
}

unsafe fn sql_fetch_scroll_impl(
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

/// The per-row outcome recorded in the row status array.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowOutcome {
    Success,
    Info,
    Error,
}

impl RowOutcome {
    fn status(self) -> SqlUSmallInt {
        match self {
            RowOutcome::Success => SQL_ROW_SUCCESS,
            RowOutcome::Info => SQL_ROW_SUCCESS_WITH_INFO,
            RowOutcome::Error => SQL_ROW_ERROR,
        }
    }

    /// Keeps the worst outcome seen while filling one row.
    fn merge(self, other: RowOutcome) -> RowOutcome {
        match (self, other) {
            (RowOutcome::Error, _) | (_, RowOutcome::Error) => RowOutcome::Error,
            (RowOutcome::Info, _) | (_, RowOutcome::Info) => RowOutcome::Info,
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
    let (row_array_size, mut bindings, rows_fetched_ptr, row_status_ptr, column_count) = {
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
        (
            stmt_state.row_array_size,
            bindings,
            stmt_state.rows_fetched_ptr,
            stmt_state.row_status_ptr,
            stmt_state.column_metadata.len(),
        )
    };

    // The row cursor only moves forward within a row, so the bound columns have
    // to be visited in ascending order regardless of the order they were bound.
    bindings.sort_by_key(|b| b.column_number);

    let rc = fill_rowset(
        statement_handle,
        stmt,
        &bindings,
        row_array_size,
        column_count,
        rows_fetched_ptr,
        row_status_ptr,
    );
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
    let mut fetch_error: Option<String> = None;
    let mut last_column_read = 0usize;

    while rows_filled < row_array_size {
        match dbc.runtime.block_on(client.next_row_cursor()) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                fetch_error = Some(e.to_string());
                break;
            }
        }

        let mut outcome = RowOutcome::Success;
        let mut columns_read = 0usize;
        for binding in bindings {
            let column = binding.column_number as usize;
            if column == 0 || column > column_count {
                // A binding left over from a wider result set must not read past
                // the end of this one.
                outcome = outcome.merge(RowOutcome::Error);
                continue;
            }
            let pulled = dbc.runtime.block_on(client.read_row_column(column - 1));
            columns_read = column;
            let result = match pulled {
                Ok(CursorColumn::Value { value, .. }) => unsafe {
                    deliver_bound(binding, rows_filled as usize, &value)
                },
                // A bound long/LOB column would have to be drained into the
                // fixed buffer here; that path is owned by SQLGetData today, so
                // report the row rather than deliver a wrong value (AB#47361).
                Ok(CursorColumn::PlpStreaming { .. }) => RowOutcome::Error,
                // Reading ascending and once per column, neither of these is
                // reachable; treat them as a row error rather than assuming.
                Ok(CursorColumn::RowEnded) | Ok(CursorColumn::AlreadyConsumed) => RowOutcome::Error,
                Err(e) => {
                    fetch_error = Some(e.to_string());
                    RowOutcome::Error
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

    let info_messages = client.take_info_messages();

    // Hand the connection back before touching the statement, mirroring
    // SQLFetch's lock order.
    {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned returning client");
            return SQL_ERROR;
        };
        dbc_state.client = Some(client);
        if rows_filled == 0 && fetch_error.is_none() {
            // End of result set: leave active_stmt set so the connection stays
            // busy with this statement until SQLMoreResults or a cursor close,
            // matching SQLFetch.
        }
        dbc_state.active_stmt = Some(statement_handle);
    }

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFetchScroll: stmt mutex poisoned recording rowset");
        return SQL_ERROR;
    };

    unsafe { write_if_some(rows_fetched_ptr, rows_filled) };
    mark_no_rows(row_status_ptr, rows_filled, row_array_size);

    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);

    if let Some(message) = fetch_error {
        error!(%message, "SQLFetchScroll: row fetch failed");
        post_sql_error(&mut stmt_state, SQLSTATE_HY000, 0, &message);
        return SQL_ERROR;
    }

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

    if worst == RowOutcome::Error {
        post_diag(&mut stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        return SQL_SUCCESS_WITH_INFO;
    }
    if worst == RowOutcome::Info || has_server_info {
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
    unsafe { row_status_ptr.add(row).write(status) };
}

/// Byte stride between consecutive elements of a column-wise bound array.
///
/// ODBC lets an application pass `BufferLength` 0 for a fixed-width target,
/// where the C type's own size is the stride; the character and binary targets
/// always carry a real buffer length.
fn element_stride(target_type: SqlSmallInt, buffer_length: SqlLen) -> usize {
    if buffer_length > 0 {
        return buffer_length as usize;
    }
    match target_type {
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_STINYINT | SQL_C_UTINYINT => 1,
        SQL_C_SSHORT | SQL_C_USHORT => 2,
        SQL_C_SLONG | SQL_C_ULONG | SQL_C_FLOAT => 4,
        SQL_C_SBIGINT | SQL_C_UBIGINT | SQL_C_DOUBLE => 8,
        SQL_C_GUID => 16,
        SQL_C_TYPE_DATE => 6,
        SQL_C_TYPE_TIME => 6,
        SQL_C_TYPE_TIMESTAMP => 16,
        SQL_C_SS_TIME2 => 12,
        SQL_C_SS_TIMESTAMPOFFSET => 20,
        _ => 0,
    }
}

/// Writes one column value into its bound buffer slot for row `row_index`.
///
/// # Safety
/// The binding's pointers must address at least `row_index + 1` elements, which
/// is the contract `SQLBindCol` places on the application together with
/// `SQL_ATTR_ROW_ARRAY_SIZE`.
unsafe fn deliver_bound(
    binding: &ColumnBinding,
    row_index: usize,
    value: &ColumnValues,
) -> RowOutcome {
    let stride = element_stride(binding.target_type, binding.buffer_length);
    if stride == 0 {
        return RowOutcome::Error;
    }

    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { binding.strlen_or_ind_ptr.add(row_index) }
    };

    if binding.target_value_ptr.is_null() {
        // Bound for its indicator only; report the length without a data write.
        if matches!(value, ColumnValues::Null) {
            unsafe { write_if_some(indicator, SQL_NULL_DATA) };
        }
        return RowOutcome::Success;
    }
    let slot = unsafe { (binding.target_value_ptr as *mut u8).add(row_index * stride) };

    if matches!(value, ColumnValues::Null) {
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
            Ok(ConvOk::Truncated) => RowOutcome::Info,
            Err(ConvError::NotHandledHere) | Err(_) => RowOutcome::Error,
        };
    }

    if binding.target_type != SQL_C_CHAR && binding.target_type != SQL_C_WCHAR {
        // SQL_C_BINARY delivery is still unimplemented (AB#47239); anything else
        // is an unsupported target.
        return RowOutcome::Error;
    }

    let text = match column_value_to_text(value) {
        Ok(t) => t,
        Err(TextError::Malformed) | Err(TextError::Unsupported) => return RowOutcome::Error,
    };

    let buf_elements = char_buf_elements(binding.target_type, stride);
    if binding.target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe { write_if_some(indicator, (utf16.len() * 2) as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &utf16) };
        if truncated {
            return RowOutcome::Info;
        }
    } else {
        let bytes = text.as_bytes();
        unsafe { write_if_some(indicator, bytes.len() as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot, buf_elements, bytes) };
        if truncated {
            return RowOutcome::Info;
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
        SQL_C_SLONG, SQL_FETCH_ABSOLUTE, SQL_FETCH_FIRST, SQL_FETCH_LAST, SQL_FETCH_PRIOR,
        SQL_FETCH_RELATIVE,
    };
    use crate::api::sqlstate::SQLSTATE_HY106;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::test_support::TestHandles;

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
            let outcome = unsafe { deliver_bound(&b, row, &ColumnValues::Int(*value)) };
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
        let outcome = unsafe { deliver_bound(&b, 1, &ColumnValues::Null) };
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
        let outcome = unsafe { deliver_bound(&b, 0, &value) };
        assert!(matches!(outcome, RowOutcome::Info));
        // The indicator reports the untruncated length.
        assert_eq!(ind[0], 10);
        assert_eq!(&buf[..7], b"1234567");
        assert_eq!(buf[7], 0, "the buffer stays NUL-terminated");
    }

    /// A binding with no data pointer is still allowed to carry an indicator.
    #[test]
    fn indicator_only_binding_reports_null_without_writing_data() {
        let mut ind = [0 as SqlLen; 1];
        let b = binding(1, SQL_C_SLONG, ptr::null_mut(), 0, ind.as_mut_ptr());
        let outcome = unsafe { deliver_bound(&b, 0, &ColumnValues::Null) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(ind[0], SQL_NULL_DATA);
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
        assert_eq!(RowOutcome::Success.status(), SQL_ROW_SUCCESS);
        assert_eq!(RowOutcome::Info.status(), SQL_ROW_SUCCESS_WITH_INFO);
        assert_eq!(RowOutcome::Error.status(), SQL_ROW_ERROR);
        assert!(matches!(
            RowOutcome::Success.merge(RowOutcome::Info),
            RowOutcome::Info
        ));
        assert!(matches!(
            RowOutcome::Info.merge(RowOutcome::Error),
            RowOutcome::Error
        ));
        assert!(matches!(
            RowOutcome::Error.merge(RowOutcome::Success),
            RowOutcome::Error
        ));
    }
}
