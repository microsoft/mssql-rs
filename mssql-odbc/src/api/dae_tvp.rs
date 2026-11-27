// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writes data-at-execution table-valued parameters into an open RPC.
//!
//! Once the RPC is open, a TVP's tokens are visited in order and nothing is
//! kept for the table as a whole:
//!
//! - a bound cell is converted and written in a short run with its neighbours;
//! - a cell supplied at execution into an unbounded MAX column is opened on the
//!   wire, and `SQLPutData` streams its chunks straight through;
//! - any other cell supplied at execution is collected, converted when it
//!   closes, and written;
//! - the table closes when its row-control token receives zero rows or, for a
//!   table with a fixed row count, when the cursor moves past its last cell.
//!
//! Tokens behind the cursor are dropped as each refilled batch begins, so
//! driver memory follows the batch the application supplies rather than the
//! rows already sent, as msodbcsql's `ProcessTvpRows` / `PushTVPRowData`
//! (`odbc/sqlcmisc.cpp`) do.

use tracing::error;

use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::sqltypes::SqlType;

use super::exec_common::{
    ParamRowBuildError, abort_dae_with_diag, dae_expected_length, fail_with_tds,
    post_param_row_build_error, return_client_idle, unwind_dae,
};
use super::param_data::{CloseOutcome, end_open_param};
use super::sqlstate::*;
use super::util::write_if_some;
use crate::api::odbc_types::{SQL_ERROR, SQL_NEED_DATA, SqlHandle, SqlPointer, SqlReturn};
use crate::conversion::param_convert::{
    ParamBuildError, bound_tvp_cell_to_value, buffered_tvp_cell_to_value,
    tvp_cell_data_at_exec_indicator,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{DaeState, DaeTarget, STMT_STATE_EXEC_STARTED};
use crate::handles::wire::WireLease;
use crate::handles::{DbcHandle, StmtHandle};

/// Bound cells converted before they are written together, so a batch of
/// small cells does not cost a client round trip each. A run also ends after
/// any MAX value, so at most one large copy of application data is held.
const BOUND_CELL_RUN: usize = 64;

/// Where advancing over the tokens the application does not supply stopped.
#[derive(Debug)]
pub(super) enum Stop {
    /// The application supplies the token the cursor is on.
    NeedData,
    /// The cursor is on a MAX cell the application streams; it has to be
    /// opened on the wire first.
    OpenCell,
    /// The run of bound cells is full.
    FlushRun,
    /// The cursor has left the table that is open on the wire.
    CloseTable,
    /// A bound cell could not be converted.
    Failed(ParamRowBuildError),
    /// No token is left although the request is still open.
    Exhausted,
}

/// Advances over bound TVP cells, converting them into `run`, until the
/// application has to supply a token or the open table has to be written to
/// or closed. Reads application cell buffers, which stay valid for the whole
/// Need Data sequence.
pub(super) fn next_stop(dae: &mut DaeState, run: &mut Vec<SqlType>) -> Stop {
    loop {
        let Some(target) = dae.current_param().map(|param| param.target) else {
            return if dae.open_tvp.is_some() {
                Stop::CloseTable
            } else {
                Stop::Exhausted
            };
        };
        let token_tvp = match target {
            DaeTarget::TvpRowControl { tvp_index, .. } | DaeTarget::TvpCell { tvp_index, .. } => {
                Some(tvp_index)
            }
            DaeTarget::Parameter => None,
        };
        if dae.open_tvp.is_some() && dae.open_tvp != token_tvp {
            return Stop::CloseTable;
        }
        match target {
            DaeTarget::Parameter => return Stop::NeedData,
            DaeTarget::TvpRowControl {
                tvp_index,
                row_index,
            } => {
                dae.open_tvp = Some(tvp_index);
                // A row control inside a batch the application already sized
                // is not its to supply.
                if dae
                    .tvp_batches
                    .get(&tvp_index)
                    .is_some_and(|&(_, batch_end)| row_index < batch_end)
                {
                    dae.advance();
                    continue;
                }
                return Stop::NeedData;
            }
            DaeTarget::TvpCell {
                tvp_index,
                row_index,
                column_index,
            } => {
                dae.open_tvp = Some(tvp_index);
                let failed = |source| {
                    Stop::Failed(ParamRowBuildError::Conversion {
                        parameter: tvp_index + 1,
                        source,
                    })
                };
                if dae.position_current_tvp_cell().is_err() {
                    return failed(ParamBuildError::InvalidBufferLength);
                }
                let Some(param) = dae.current_param() else {
                    return Stop::Exhausted;
                };
                let binding = param.binding;
                let collects = param.collects();
                if let Some(indicator) = unsafe { tvp_cell_data_at_exec_indicator(&binding) } {
                    if let Some(param) = dae.current_param_mut() {
                        param.expected_len = dae_expected_length(indicator);
                    }
                    return if collects {
                        Stop::NeedData
                    } else {
                        Stop::OpenCell
                    };
                }
                let value = match unsafe { bound_tvp_cell_to_value(&binding) } {
                    Ok(value) => value,
                    Err(error) => {
                        return failed(ParamBuildError::TvpCellConversion {
                            row: row_index,
                            column: column_index,
                            diagnostic: error.diag(),
                        });
                    }
                };
                let large = is_large(&value);
                run.push(value);
                dae.advance();
                if large || run.len() >= BOUND_CELL_RUN {
                    return Stop::FlushRun;
                }
            }
        }
    }
}

fn is_large(value: &SqlType) -> bool {
    matches!(
        value,
        SqlType::VarcharMax(Some(_))
            | SqlType::NVarcharMax(Some(_))
            | SqlType::VarBinaryMax(Some(_))
            | SqlType::Xml(Some(_))
    )
}

/// Advances an open RPC until the application must supply data, writing what
/// it does not supply: `run` (cells already converted) first, then any bound
/// cells after the cursor. Returns `SQL_NEED_DATA` with the next token written
/// to `value_ptr_ptr`, or the execution's result once the last table closes
/// the request.
pub(super) fn drive_streaming(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
    mut run: Vec<SqlType>,
) -> SqlReturn {
    loop {
        let stop = {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLParamData: stmt mutex poisoned advancing a streamed TVP");
                return SQL_ERROR;
            };
            let Some(dae) = stmt_state.dae.as_mut() else {
                error!("SQLParamData: DAE sequence vanished advancing a streamed TVP");
                return SQL_ERROR;
            };
            next_stop(dae, &mut run)
        };
        match stop {
            Stop::Failed(error) => {
                return abort_with_row_error(dbc, stmt, statement_handle, &error);
            }
            Stop::Exhausted => {
                return abort_internal(
                    dbc,
                    stmt,
                    statement_handle,
                    "no data-at-execution token is left for the open request",
                );
            }
            Stop::NeedData | Stop::OpenCell | Stop::FlushRun | Stop::CloseTable => {}
        }

        if !run.is_empty() {
            if let Err(rc) = write_on_open_rpc(dbc, stmt, statement_handle, |client| {
                dbc.runtime.block_on(client.write_streamed_tvp_cells(&run))
            }) {
                return rc;
            }
            run.clear();
        }

        match stop {
            Stop::FlushRun => {}
            Stop::NeedData => return need_data(stmt, value_ptr_ptr),
            Stop::OpenCell => {
                if let Err(rc) = write_on_open_rpc(dbc, stmt, statement_handle, |client| {
                    dbc.runtime.block_on(client.begin_streamed_tvp_cell())
                }) {
                    return rc;
                }
                return need_data(stmt, value_ptr_ptr);
            }
            Stop::CloseTable => {
                let client = match checkout(stmt, |dae| dae.open_tvp = None) {
                    Ok(client) => client,
                    Err(rc) => return rc,
                };
                match end_open_param(dbc, stmt, statement_handle, client, false) {
                    CloseOutcome::NextOpen => {}
                    CloseOutcome::Finished(rc) => return rc,
                }
            }
            Stop::Failed(_) | Stop::Exhausted => unreachable!("handled above"),
        }
    }
}

/// Closes a TVP row-control token while the RPC is open. A positive count
/// opens the batch; zero ends the table, which the driver then closes on the
/// wire.
pub(super) fn close_row_control(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    let closed = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLParamData: stmt mutex poisoned closing a TVP row count");
            return SQL_ERROR;
        };
        let Some(dae) = stmt_state.dae.as_mut() else {
            error!("SQLParamData: DAE sequence vanished closing a TVP row count");
            return SQL_ERROR;
        };
        let closed = dae.close_tvp_row_control();
        if closed.is_ok() {
            dae.discard_consumed_tokens();
        }
        closed
    };
    if let Err(problem) = closed {
        return abort_internal(dbc, stmt, statement_handle, problem);
    }
    drive_streaming(dbc, stmt, statement_handle, value_ptr_ptr, Vec::new())
}

/// Closes a collected TVP cell while the RPC is open: converts it as the
/// materialized table would and writes it with the bound cells that follow.
pub(super) fn close_collected_cell(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
) -> SqlReturn {
    let converted =
        {
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLParamData: stmt mutex poisoned closing a TVP cell");
                return SQL_ERROR;
            };
            let Some(dae) = stmt_state.dae.as_mut() else {
                error!("SQLParamData: DAE sequence vanished closing a TVP cell");
                return SQL_ERROR;
            };
            let Some((
                DaeTarget::TvpCell {
                    tvp_index,
                    row_index,
                    column_index,
                },
                binding,
            )) = dae.current_param().map(|param| {
                (
                    param.target,
                    param
                        .tvp_cell_binding
                        .map_or(param.binding, |layout| layout.binding),
                )
            })
            else {
                drop(stmt_state);
                return abort_internal(
                    dbc,
                    stmt,
                    statement_handle,
                    "the open data-at-execution token is not a TVP cell",
                );
            };
            // A trailing partial SQL_C_WCHAR unit is floored here exactly as the
            // collected-table path does.
            let bytes = std::mem::take(&mut dae.progress.buffer);
            let converted = buffered_tvp_cell_to_value(&binding, &bytes, dae.progress.is_null)
                .map_err(|error| ParamRowBuildError::Conversion {
                    parameter: tvp_index + 1,
                    source: ParamBuildError::TvpCellConversion {
                        row: row_index,
                        column: column_index,
                        diagnostic: error.diag(),
                    },
                });
            if converted.is_ok() {
                dae.advance();
            }
            converted
        };
    match converted {
        Ok(value) => drive_streaming(dbc, stmt, statement_handle, value_ptr_ptr, vec![value]),
        Err(error) => abort_with_row_error(dbc, stmt, statement_handle, &error),
    }
}

/// Closes a streamed TVP cell on the wire with the `client` that flushed its
/// transcoder tail; the table stays open for the rest of the row.
pub(super) fn close_streamed_cell(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    value_ptr_ptr: *mut SqlPointer,
    mut client: WireLease,
) -> SqlReturn {
    if let Err(error) = dbc.runtime.block_on(client.end_streamed_tvp_cell()) {
        return fail_streamed_write(dbc, stmt, statement_handle, client, &error);
    }
    if let Err(rc) = park(dbc, stmt, statement_handle, client, DaeState::advance) {
        return rc;
    }
    drive_streaming(dbc, stmt, statement_handle, value_ptr_ptr, Vec::new())
}

fn need_data(stmt: &StmtHandle, value_ptr_ptr: *mut SqlPointer) -> SqlReturn {
    let Ok(stmt_state) = stmt.inner.lock() else {
        error!("SQLParamData: stmt mutex poisoned reading the next token");
        return SQL_ERROR;
    };
    unsafe { write_if_some(value_ptr_ptr, stmt_state.dae_current_value_ptr()) };
    SQL_NEED_DATA
}

/// Runs one write on the open RPC with the client checked out of the sequence.
fn write_on_open_rpc(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    write: impl FnOnce(&mut WireLease) -> TdsResult<()>,
) -> Result<(), SqlReturn> {
    let mut client = checkout(stmt, |_| {})?;
    match write(&mut client) {
        Ok(()) => park(dbc, stmt, statement_handle, client, |_| {}),
        Err(error) => Err(fail_streamed_write(
            dbc,
            stmt,
            statement_handle,
            client,
            &error,
        )),
    }
}

/// Checks the client out for network I/O, applying `update` to the sequence in
/// the same critical section.
fn checkout(stmt: &StmtHandle, update: impl FnOnce(&mut DaeState)) -> Result<WireLease, SqlReturn> {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLParamData: stmt mutex poisoned taking the DAE client");
        return Err(SQL_ERROR);
    };
    let checked_out = stmt_state.dae.as_mut().and_then(|dae| {
        let client = dae.checkout_client()?;
        update(dae);
        Some(client)
    });
    checked_out.ok_or_else(|| {
        error!("SQLParamData: DAE client is unavailable — internal state corruption");
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        SQL_ERROR
    })
}

/// Parks the client back on the sequence after a clean write, applying
/// `update` in the same critical section.
fn park(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: WireLease,
    update: impl FnOnce(&mut DaeState),
) -> Result<(), SqlReturn> {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLParamData: stmt mutex poisoned returning the DAE client");
        // The request is still open, so it is discarded before the client
        // goes back to the idle pool.
        dbc.runtime.block_on(client.cancel_streamed_write());
        return_client_idle(dbc, statement_handle, client);
        return Err(SQL_ERROR);
    };
    match stmt_state.dae.as_mut() {
        Some(dae) => {
            dae.return_client(client);
            update(dae);
            Ok(())
        }
        None => {
            // STMT is released before the DBC lock `return_client_idle` takes.
            drop(stmt_state);
            error!("SQLParamData: DAE sequence ended while the client was checked out");
            dbc.runtime.block_on(client.cancel_streamed_write());
            return_client_idle(dbc, statement_handle, client);
            Err(SQL_ERROR)
        }
    }
}

/// Ends the sequence after a failed write. Rejections that leave the stream
/// active are withdrawn too, so the client never re-enters the idle pool with
/// a half-written request.
fn fail_streamed_write(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: WireLease,
    error: &mssql_tds::error::Error,
) -> SqlReturn {
    error!(%error, "SQLParamData: writing a streamed TVP failed");
    dbc.runtime.block_on(client.cancel_streamed_write());
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        let parked = stmt_state.take_dae();
        debug_assert!(parked.is_none(), "the client is checked out by this call");
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    }
    fail_with_tds(dbc, stmt, statement_handle, client, error)
}

fn abort_with_row_error(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    error: &ParamRowBuildError,
) -> SqlReturn {
    let rc = abort_dae_with_diag(dbc, stmt, statement_handle, error.diag());
    if let Ok(mut state) = stmt.inner.lock() {
        free_errors(&mut state);
        post_param_row_build_error(&mut state, error, 0);
    }
    rc
}

fn abort_internal(
    dbc: &DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    problem: &str,
) -> SqlReturn {
    error!("SQLParamData: {problem}");
    unwind_dae(dbc, stmt, statement_handle, None);
    if let Ok(mut state) = stmt.inner.lock() {
        post_sql_error(
            &mut state,
            SQLSTATE_HY000,
            0,
            format!("Internal error streaming a table-valued parameter: {problem}"),
        );
    }
    SQL_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::exports::{
        SQLBindParameter, SQLCancel, SQLExecDirectW, SQLExecute, SQLParamData, SQLPrepareW,
        SQLPutData, SQLSetStmtAttrW,
    };
    use crate::api::odbc_types::*;
    use crate::handles::stmt::DaeParam;
    use crate::handles::{DbcHandle, handle_from_raw};
    use crate::params::BoundParam;
    use crate::test_support::TestHandles;
    use mssql_tds::test_client_support::{
        ScriptedToken, done_no_more, int_columns, return_value, tds_client_from_tokens_capturing,
    };
    use std::sync::{Arc, Mutex};

    const ROW: u8 = 0x01;
    const END: u8 = 0x00;
    const PLP_UNKNOWN: [u8; 8] = [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    const PLP_NULL: [u8; 8] = [0xFF; 8];
    const PLP_TERMINATOR: [u8; 4] = [0; 4];

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// The request payload with each 8-byte TDS packet header removed.
    fn payload(sent: &Mutex<Vec<u8>>) -> Vec<u8> {
        let sent = sent.lock().unwrap();
        let mut out = Vec::new();
        let mut offset = 0;
        while offset + 8 <= sent.len() {
            let length = usize::from(u16::from_be_bytes([sent[offset + 2], sent[offset + 3]]));
            out.extend_from_slice(&sent[offset + 8..offset + length]);
            offset += length;
        }
        out
    }

    fn connect(h: &TestHandles, tokens: Vec<ScriptedToken>) -> Arc<Mutex<Vec<u8>>> {
        h.mark_dbc_connected();
        let (client, sent) = tds_client_from_tokens_capturing(tokens);
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().wire.put_client_for_test(client);
        sent
    }

    fn stmt(h: &TestHandles) -> &StmtHandle {
        unsafe { handle_from_raw::<StmtHandle>(h.stmt) }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind(
        h: &TestHandles,
        ordinal: u16,
        c_type: SqlSmallInt,
        sql_type: SqlSmallInt,
        column_size: SqlULen,
        value: SqlPointer,
        buffer_length: SqlLen,
        indicator: *mut SqlLen,
    ) {
        let rc = unsafe {
            SQLBindParameter(
                h.stmt,
                ordinal,
                SQL_PARAM_INPUT,
                c_type,
                sql_type,
                column_size,
                0,
                value,
                buffer_length,
                indicator,
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "bind {ordinal}");
    }

    fn focus(h: &TestHandles, ordinal: usize) {
        stmt(h)
            .inner
            .lock()
            .unwrap()
            .vendor_attrs
            .store_param_focus(ordinal);
    }

    fn exec_direct(h: &TestHandles, sql: &str) -> SqlReturn {
        let sql = wide(sql);
        unsafe { SQLExecDirectW(h.stmt, sql.as_ptr(), SQL_NTS.into()) }
    }

    fn param_data(h: &TestHandles) -> (SqlReturn, SqlPointer) {
        let mut token = std::ptr::null_mut();
        let rc = unsafe { SQLParamData(h.stmt, &mut token) };
        (rc, token)
    }

    fn put(h: &TestHandles, data: SqlPointer, length: SqlLen) -> SqlReturn {
        unsafe { SQLPutData(h.stmt, data, length) }
    }

    fn put_rows(h: &TestHandles, rows: usize) -> SqlReturn {
        if rows == 0 {
            put(h, std::ptr::null_mut(), 0)
        } else {
            put(h, 1usize as SqlPointer, SqlLen::try_from(rows).unwrap())
        }
    }

    fn token_count(h: &TestHandles) -> usize {
        stmt(h)
            .inner
            .lock()
            .unwrap()
            .dae
            .as_ref()
            .map_or(0, DaeState::token_count)
    }

    fn assert_idle(h: &TestHandles) {
        assert!(!stmt(h).inner.lock().unwrap().needs_data());
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc.inner.lock().unwrap();
        assert!(state.wire.owner().is_none(), "the connection is released");
        assert!(
            state.wire.client().is_some(),
            "the client is back on the connection"
        );
    }

    /// A table of `(id int, label nvarchar(max))` whose rows and cells are all
    /// supplied at execution, the way mssql-python's `TableValuedParameter`
    /// drives it: one row per batch, every cell its own token.
    struct PythonStyleTable {
        type_name: Vec<u16>,
        tvp_indicator: Box<SqlLen>,
        id: Box<i32>,
        id_indicator: Box<SqlLen>,
        label: Box<[u16; 1]>,
        label_indicator: Box<SqlLen>,
    }

    impl PythonStyleTable {
        fn bind(h: &TestHandles, ordinal: u16) -> Self {
            let mut table = Self {
                type_name: wide("Items"),
                tvp_indicator: Box::new(SQL_DATA_AT_EXEC),
                id: Box::new(0),
                id_indicator: Box::new(SQL_DATA_AT_EXEC),
                label: Box::new([0]),
                label_indicator: Box::new(SQL_DATA_AT_EXEC),
            };
            bind(
                h,
                ordinal,
                SQL_C_DEFAULT,
                SQL_SS_TABLE,
                1_000_000,
                table.type_name.as_mut_ptr().cast(),
                SQL_NTS.into(),
                &mut *table.tvp_indicator,
            );
            focus(h, usize::from(ordinal));
            bind(
                h,
                1,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                (&mut *table.id as *mut i32).cast(),
                4,
                &mut *table.id_indicator,
            );
            bind(
                h,
                2,
                SQL_C_WCHAR,
                SQL_WVARCHAR,
                0,
                table.label.as_mut_ptr().cast(),
                0,
                &mut *table.label_indicator,
            );
            focus(h, 0);
            table
        }

        fn table_token(&mut self) -> SqlPointer {
            self.type_name.as_mut_ptr().cast()
        }

        fn id_token(&mut self) -> SqlPointer {
            (&mut *self.id as *mut i32).cast()
        }

        fn label_token(&mut self) -> SqlPointer {
            self.label.as_mut_ptr().cast()
        }

        /// Supplies one row from the row-control token on; returns the code of
        /// the `SQLParamData` that closes its last cell.
        fn supply_row(&mut self, h: &TestHandles, id: i32, label: Option<&str>) -> SqlReturn {
            assert_eq!(put_rows(h, 1), SQL_SUCCESS);
            let (rc, token) = param_data(h);
            assert_eq!((rc, token), (SQL_NEED_DATA, self.id_token()));
            *self.id = id;
            assert_eq!(put(h, self.id_token(), 4), SQL_SUCCESS);
            let (rc, token) = param_data(h);
            assert_eq!((rc, token), (SQL_NEED_DATA, self.label_token()));
            match label {
                None => assert_eq!(put(h, std::ptr::null_mut(), SQL_NULL_DATA), SQL_SUCCESS),
                Some(label) => {
                    let bytes = utf16le(label);
                    // Two chunks prove a cell spans SQLPutData calls.
                    let (first, second) = bytes.split_at((bytes.len() / 2) & !1);
                    for chunk in [first, second] {
                        assert_eq!(
                            put(
                                h,
                                chunk.as_ptr().cast_mut().cast(),
                                SqlLen::try_from(chunk.len()).unwrap(),
                            ),
                            SQL_SUCCESS
                        );
                    }
                }
            }
            let (rc, token) = param_data(h);
            if rc == SQL_NEED_DATA {
                assert_eq!(token, self.table_token(), "the next row count is requested");
            }
            rc
        }
    }

    fn streamed_row(id: i32, label: Option<&str>) -> Vec<u8> {
        let mut row = vec![ROW, 4];
        row.extend(id.to_le_bytes());
        match label {
            None => row.extend(PLP_NULL),
            Some(label) => {
                row.extend(PLP_UNKNOWN);
                let bytes = utf16le(label);
                let (first, second) = bytes.split_at((bytes.len() / 2) & !1);
                for chunk in [first, second]
                    .into_iter()
                    .filter(|chunk| !chunk.is_empty())
                {
                    row.extend(u32::try_from(chunk.len()).unwrap().to_le_bytes());
                    row.extend_from_slice(chunk);
                }
                row.extend(PLP_TERMINATOR);
            }
        }
        row
    }

    fn label(id: usize) -> String {
        format!("{id:0>40}")
    }

    /// The mssql-python memory workload in miniature: rows leave as they are
    /// supplied and the sequence keeps a bounded number of tokens, instead of a
    /// token and a buffered cell for every row.
    #[test]
    fn refilled_rows_stream_with_bounded_tokens() {
        const ROWS: usize = 300;
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut table = PythonStyleTable::bind(&h, 1);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        let (rc, token) = param_data(&h);
        assert_eq!((rc, token), (SQL_NEED_DATA, table.table_token()));

        let mut most_tokens = 0;
        let mut sent_before_last_row = 0;
        for id in 0..ROWS {
            if id == ROWS - 1 {
                sent_before_last_row = sent.lock().unwrap().len();
            }
            let label = label(id);
            let rc = table.supply_row(&h, i32::try_from(id).unwrap(), Some(&label));
            assert_eq!(rc, SQL_NEED_DATA);
            most_tokens = most_tokens.max(token_count(&h));
            let state = stmt(&h).inner.lock().unwrap();
            let dae = state.dae.as_ref().unwrap();
            assert!(!dae.deferred);
            assert!(dae.tvp_cells.is_empty(), "no cell is kept");
        }
        assert!(
            most_tokens <= 6,
            "tokens must not grow with rows: {most_tokens}"
        );
        assert!(
            sent_before_last_row > 16 * 1024,
            "earlier rows are on the wire before the last is supplied, sent {sent_before_last_row}"
        );

        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);
        assert_idle(&h);

        let payload = payload(&sent);
        let mut rows = Vec::new();
        for id in 0..ROWS {
            rows.extend(streamed_row(i32::try_from(id).unwrap(), Some(&label(id))));
        }
        rows.push(END);
        assert!(
            payload.ends_with(&rows),
            "every row, in order, then the end token"
        );
    }

    #[test]
    fn null_and_empty_streamed_cells_keep_their_meaning() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut table = PythonStyleTable::bind(&h, 1);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        param_data(&h);
        assert_eq!(table.supply_row(&h, 1, None), SQL_NEED_DATA);
        assert_eq!(table.supply_row(&h, 2, Some("")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let mut expected = streamed_row(1, None);
        expected.extend(streamed_row(2, Some("")));
        expected.push(END);
        assert!(payload(&sent).ends_with(&expected));
    }

    #[test]
    fn an_empty_streamed_table_sends_metadata_and_no_rows() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let _table = PythonStyleTable::bind(&h, 1);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        param_data(&h);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let payload = payload(&sent);
        let type_name = [&[0x05][..], &utf16le("Items")].concat();
        let header = payload
            .windows(type_name.len())
            .rposition(|window| window == type_name.as_slice())
            .expect("the table type is written");
        assert!(
            payload.ends_with(&[END, END]),
            "end of metadata, then of rows"
        );
        assert!(
            !contains(&payload[header..], &PLP_UNKNOWN),
            "no cell was written"
        );
    }

    #[test]
    fn a_prepared_statement_streams_its_table_through_sp_prepexec() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(
            &h,
            vec![
                return_value(mssql_tds::query::result::ReturnValue {
                    param_ordinal: 0,
                    param_name: String::new(),
                    value: mssql_tds::datatypes::column_values::ColumnValues::Int(55),
                    column_metadata: Box::new(int_columns(1).remove(0)),
                    status: mssql_tds::token::tokenitems::ReturnValueStatus::OutputParam,
                }),
                done_no_more(),
            ],
        );
        let mut table = PythonStyleTable::bind(&h, 1);
        let sql = wide("EXEC dbo.CountRows ?");
        assert_eq!(
            unsafe { SQLPrepareW(h.stmt, sql.as_ptr(), SQL_NTS.into()) },
            SQL_SUCCESS
        );

        assert_eq!(unsafe { SQLExecute(h.stmt) }, SQL_NEED_DATA);
        param_data(&h);
        assert_eq!(table.supply_row(&h, 7, Some("seven")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let payload = payload(&sent);
        assert!(contains(&payload, &[0xFF, 0xFF, 13, 0]), "sp_prepexec");
        let mut expected = streamed_row(7, Some("seven"));
        expected.push(END);
        assert!(payload.ends_with(&expected));
        assert!(stmt(&h).inner.lock().unwrap().prepared.is_some());
    }

    /// Bound cells of a refilled batch are written as converted, and the
    /// request matches the one a fully bound table produces.
    #[test]
    fn bound_cells_stream_exactly_like_a_materialized_table() {
        struct Columns {
            ids: Box<[i32; 3]>,
            id_lengths: Box<[SqlLen; 3]>,
            names: Box<[[u16; 21]; 3]>,
            name_lengths: Box<[SqlLen; 3]>,
        }
        fn bind_columns(h: &TestHandles) -> Columns {
            let mut columns = Columns {
                ids: Box::new([10, 20, 30]),
                id_lengths: Box::new([4, SQL_NULL_DATA, 4]),
                names: Box::new([[0; 21]; 3]),
                name_lengths: Box::new([SQL_NTS as SqlLen; 3]),
            };
            for (row, name) in ["ten", "", "thirty"].into_iter().enumerate() {
                for (unit, value) in name.encode_utf16().enumerate() {
                    columns.names[row][unit] = value;
                }
            }
            focus(h, 1);
            assert_eq!(
                unsafe { SQLSetStmtAttrW(h.stmt, SQL_ATTR_PARAMSET_SIZE, 3usize as SqlPointer, 0) },
                SQL_SUCCESS
            );
            bind(
                h,
                1,
                SQL_C_SLONG,
                SQL_INTEGER,
                0,
                columns.ids.as_mut_ptr().cast(),
                4,
                columns.id_lengths.as_mut_ptr(),
            );
            bind(
                h,
                2,
                SQL_C_WCHAR,
                SQL_WVARCHAR,
                20,
                columns.names.as_mut_ptr().cast(),
                42,
                columns.name_lengths.as_mut_ptr(),
            );
            focus(h, 0);
            columns
        }

        let materialized = {
            let h = TestHandles::with_env_dbc_stmt();
            let sent = connect(&h, vec![done_no_more()]);
            let mut type_name = wide("Items");
            let mut rows: SqlLen = 3;
            bind(
                &h,
                1,
                SQL_C_DEFAULT,
                SQL_SS_TABLE,
                3,
                type_name.as_mut_ptr().cast(),
                SQL_NTS.into(),
                &mut rows,
            );
            let _columns = bind_columns(&h);
            assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_SUCCESS);
            payload(&sent)
        };

        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut type_name = wide("Items");
        let mut indicator = SQL_DATA_AT_EXEC;
        bind(
            &h,
            1,
            SQL_C_DEFAULT,
            SQL_SS_TABLE,
            3,
            type_name.as_mut_ptr().cast(),
            SQL_NTS.into(),
            &mut indicator,
        );
        let _columns = bind_columns(&h);
        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        assert_eq!(
            param_data(&h),
            (SQL_NEED_DATA, type_name.as_mut_ptr().cast())
        );
        assert_eq!(put_rows(&h, 3), SQL_SUCCESS);
        assert_eq!(
            param_data(&h),
            (SQL_NEED_DATA, type_name.as_mut_ptr().cast())
        );
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        assert_eq!(payload(&sent), materialized);
    }

    /// A collected cell that fails conversion abandons the request: rows
    /// already sent must not run as a truncated table.
    #[test]
    fn a_collected_cell_conversion_error_withdraws_the_request() {
        let h = TestHandles::with_env_dbc_stmt();
        let _sent = connect(&h, vec![done_no_more()]);
        let mut type_name = wide("Items");
        let mut tvp_indicator = SQL_DATA_AT_EXEC;
        let mut id = *b"12345678";
        let mut id_indicator = SQL_DATA_AT_EXEC;
        bind(
            &h,
            1,
            SQL_C_DEFAULT,
            SQL_SS_TABLE,
            10,
            type_name.as_mut_ptr().cast(),
            SQL_NTS.into(),
            &mut tvp_indicator,
        );
        focus(&h, 1);
        bind(
            &h,
            1,
            SQL_C_CHAR,
            SQL_INTEGER,
            0,
            id.as_mut_ptr().cast(),
            8,
            &mut id_indicator,
        );
        focus(&h, 0);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        param_data(&h);
        for (row, text) in [b"1".as_slice(), b"abc"].into_iter().enumerate() {
            assert_eq!(put_rows(&h, 1), SQL_SUCCESS);
            assert_eq!(param_data(&h), (SQL_NEED_DATA, id.as_mut_ptr().cast()));
            assert_eq!(
                put(
                    &h,
                    text.as_ptr().cast_mut().cast(),
                    SqlLen::try_from(text.len()).unwrap()
                ),
                SQL_SUCCESS
            );
            let rc = param_data(&h).0;
            if row == 0 {
                assert_eq!(rc, SQL_NEED_DATA);
            } else {
                assert_eq!(rc, SQL_ERROR);
            }
        }

        let state = stmt(&h).inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_22018);
        assert!(
            state.diag_records[0]
                .message
                .contains("TVP parameter ODBC ordinal 1, row 2, column 1"),
            "{}",
            state.diag_records[0].message
        );
        drop(state);
        assert_idle(&h);
    }

    #[test]
    fn cancel_inside_a_streamed_table_releases_the_connection() {
        let h = TestHandles::with_env_dbc_stmt();
        let _sent = connect(&h, vec![done_no_more()]);
        let mut table = PythonStyleTable::bind(&h, 1);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        param_data(&h);
        assert_eq!(table.supply_row(&h, 1, Some("one")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 1), SQL_SUCCESS);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, table.id_token()));

        assert_eq!(unsafe { SQLCancel(h.stmt) }, SQL_SUCCESS);
        assert_idle(&h);
    }

    /// A buffered scalar after the table holds the RPC back, so the table is
    /// collected and materialized as before; nothing streams early.
    #[test]
    fn a_later_buffered_parameter_keeps_the_table_collected() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut table = PythonStyleTable::bind(&h, 1);
        let mut number = *b"42";
        let mut number_indicator = SQL_DATA_AT_EXEC;
        bind(
            &h,
            2,
            SQL_C_CHAR,
            SQL_INTEGER,
            0,
            number.as_mut_ptr().cast(),
            2,
            &mut number_indicator,
        );

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?, ?"), SQL_NEED_DATA);
        assert!(
            stmt(&h)
                .inner
                .lock()
                .unwrap()
                .dae
                .as_ref()
                .unwrap()
                .deferred
        );
        param_data(&h);
        assert_eq!(table.supply_row(&h, 5, Some("five")), SQL_NEED_DATA);
        assert!(
            sent.lock().unwrap().is_empty(),
            "nothing is sent while a later value must be collected first"
        );
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, number.as_mut_ptr().cast()));
        assert_eq!(put(&h, number.as_mut_ptr().cast(), 2), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let payload = payload(&sent);
        assert!(contains(&payload, &[0x26, 4, 4, 42, 0, 0, 0]));
        // The collected cell is converted whole and sent as one chunk; streamed,
        // it would keep the application's two chunks.
        let materialized_cell = [
            &PLP_UNKNOWN[..],
            &8u32.to_le_bytes(),
            &utf16le("five"),
            &PLP_TERMINATOR,
        ]
        .concat();
        assert!(
            contains(&payload, &materialized_cell),
            "the table was materialized"
        );
    }

    /// An earlier buffered scalar is collected first; once only streamable
    /// tokens remain the RPC opens and the table streams.
    #[test]
    fn an_earlier_buffered_parameter_still_lets_the_table_stream() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut number = *b"42";
        let mut number_indicator = SQL_DATA_AT_EXEC;
        bind(
            &h,
            1,
            SQL_C_CHAR,
            SQL_INTEGER,
            0,
            number.as_mut_ptr().cast(),
            2,
            &mut number_indicator,
        );
        let mut table = PythonStyleTable::bind(&h, 2);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?, ?"), SQL_NEED_DATA);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, number.as_mut_ptr().cast()));
        assert_eq!(put(&h, number.as_mut_ptr().cast(), 2), SQL_SUCCESS);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, table.table_token()));
        {
            let state = stmt(&h).inner.lock().unwrap();
            assert!(!state.dae.as_ref().unwrap().deferred, "the RPC is open");
        }
        assert_eq!(table.supply_row(&h, 9, Some("nine")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let payload = payload(&sent);
        assert!(contains(&payload, &[0x26, 4, 4, 42, 0, 0, 0]));
        let mut expected = streamed_row(9, Some("nine"));
        expected.push(END);
        assert!(payload.ends_with(&expected));
    }

    /// A cell that fails after earlier packets have left withdraws the request
    /// with an IGNORE packet, so the server never runs a truncated table, and
    /// the connection stays usable.
    #[test]
    fn a_failure_after_rows_left_withdraws_the_sent_request() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut table = PythonStyleTable::bind(&h, 1);

        assert_eq!(exec_direct(&h, "EXEC dbo.CountRows ?"), SQL_NEED_DATA);
        param_data(&h);
        for id in 0..100 {
            let label = label(id);
            assert_eq!(table.supply_row(&h, id as i32, Some(&label)), SQL_NEED_DATA);
        }
        assert!(
            !sent.lock().unwrap().is_empty(),
            "rows are already on the wire"
        );

        assert_eq!(put_rows(&h, 1), SQL_SUCCESS);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, table.id_token()));
        // Two bytes cannot be a SQL_C_SLONG.
        assert_eq!(put(&h, table.id_token(), 2), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_ERROR);

        let state = stmt(&h).inner.lock().unwrap();
        assert!(
            state.diag_records[0]
                .message
                .contains("TVP parameter ODBC ordinal 1, row 101, column 1"),
            "{}",
            state.diag_records[0].message
        );
        drop(state);
        assert_idle(&h);
        let sent = sent.lock().unwrap();
        let mut offset = 0;
        let mut last_status = 0;
        while offset + 8 <= sent.len() {
            last_status = sent[offset + 1];
            offset += usize::from(u16::from_be_bytes([sent[offset + 2], sent[offset + 3]]));
        }
        assert_eq!(last_status & 0x03, 0x03, "the request ends EOM | IGNORE");
    }

    /// Each streamed table closes when the cursor reaches the next streamed
    /// value, and the values keep their parameter order on the wire.
    #[test]
    fn two_streamed_tables_around_a_streamed_scalar_keep_order() {
        let h = TestHandles::with_env_dbc_stmt();
        let sent = connect(&h, vec![done_no_more()]);
        let mut first = PythonStyleTable::bind(&h, 1);
        let mut marker = [0u16; 1];
        let mut marker_indicator = SQL_DATA_AT_EXEC;
        bind(
            &h,
            2,
            SQL_C_WCHAR,
            SQL_WVARCHAR,
            0,
            marker.as_mut_ptr().cast(),
            0,
            &mut marker_indicator,
        );
        let mut second = PythonStyleTable::bind(&h, 3);

        assert_eq!(exec_direct(&h, "EXEC dbo.Pair ?, ?, ?"), SQL_NEED_DATA);
        assert!(
            !stmt(&h)
                .inner
                .lock()
                .unwrap()
                .dae
                .as_ref()
                .unwrap()
                .deferred
        );
        assert_eq!(param_data(&h), (SQL_NEED_DATA, first.table_token()));
        assert_eq!(first.supply_row(&h, 1, Some("first")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h), (SQL_NEED_DATA, marker.as_mut_ptr().cast()));
        let text = utf16le("between");
        assert_eq!(
            put(
                &h,
                text.as_ptr().cast_mut().cast(),
                SqlLen::try_from(text.len()).unwrap()
            ),
            SQL_SUCCESS
        );
        assert_eq!(param_data(&h), (SQL_NEED_DATA, second.table_token()));
        assert_eq!(second.supply_row(&h, 2, Some("second")), SQL_NEED_DATA);
        assert_eq!(put_rows(&h, 0), SQL_SUCCESS);
        assert_eq!(param_data(&h).0, SQL_SUCCESS);

        let payload = payload(&sent);
        let mut first_table = streamed_row(1, Some("first"));
        first_table.push(END);
        let scalar = [
            &PLP_UNKNOWN[..],
            &u32::try_from(text.len()).unwrap().to_le_bytes(),
            &text,
            &PLP_TERMINATOR,
        ]
        .concat();
        let mut second_table = streamed_row(2, Some("second"));
        second_table.push(END);
        let position = |needle: &[u8]| {
            payload
                .windows(needle.len())
                .position(|window| window == needle)
                .expect("value is sent")
        };
        assert!(position(&first_table) < position(&scalar));
        assert!(position(&scalar) < position(&second_table));
        assert!(payload.ends_with(&second_table));
    }

    #[test]
    fn only_unbounded_same_family_max_cells_stream() {
        let binding = |c_type, sql_type, column_size| BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type,
            sql_type,
            column_size,
            decimal_digits: 0,
            app_datetime_interval_code: None,
            sql_datetime_interval_code: None,
            app_precision: 0,
            app_scale: 0,
            parameter_value_ptr: std::ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        };
        for (c_type, sql_type, size, streams) in [
            (SQL_C_WCHAR, SQL_WVARCHAR, 0, true),
            (SQL_C_CHAR, SQL_WVARCHAR, 0, true),
            (SQL_C_CHAR, SQL_VARCHAR, 0, true),
            (SQL_C_WCHAR, SQL_VARCHAR, 9000, true),
            (SQL_C_BINARY, SQL_VARBINARY, 0, true),
            (SQL_C_WCHAR, SQL_WVARCHAR, 50, false),
            (SQL_C_BINARY, SQL_VARBINARY, 8000, false),
            (SQL_C_CHAR, SQL_LONGVARCHAR, 0, false),
            (SQL_C_WCHAR, SQL_WLONGVARCHAR, 0, false),
            (SQL_C_BINARY, SQL_LONGVARBINARY, 0, false),
            (SQL_C_BINARY, SQL_WVARCHAR, 0, false),
            (SQL_C_WCHAR, SQL_VARBINARY, 0, false),
            (SQL_C_SLONG, SQL_INTEGER, 0, false),
            (SQL_C_DEFAULT, SQL_WVARCHAR, 0, false),
        ] {
            assert_eq!(
                crate::conversion::param_convert::tvp_cell_streams(&binding(
                    c_type, sql_type, size
                )),
                streams,
                "{c_type} -> {sql_type}({size})"
            );
        }
    }

    #[test]
    fn next_stop_closes_a_fixed_row_table_after_its_last_cell() {
        let cell = |row_index| {
            DaeParam::unbounded(0, std::ptr::null_mut(), None).with_target(DaeTarget::TvpCell {
                tvp_index: 0,
                row_index,
                column_index: 0,
            })
        };
        let mut indicator = SQL_DATA_AT_EXEC;
        let mut params = vec![
            cell(0),
            cell(1),
            DaeParam::unbounded(1, std::ptr::null_mut(), None),
        ];
        for param in &mut params[..2] {
            param.binding.octet_length_ptr = &mut indicator;
            param.binding.strlen_or_ind_ptr = &mut indicator;
        }
        let mut dae = DaeState::for_test(params, Some(0))
            .with_tvp_row_counts(std::collections::BTreeMap::from([(0, 2)]));
        let mut run = Vec::new();

        assert!(matches!(next_stop(&mut dae, &mut run), Stop::OpenCell));
        assert_eq!(dae.open_tvp, Some(0));
        dae.advance();
        assert!(matches!(next_stop(&mut dae, &mut run), Stop::OpenCell));
        dae.advance();
        assert!(matches!(next_stop(&mut dae, &mut run), Stop::CloseTable));
        dae.open_tvp = None;
        assert!(matches!(next_stop(&mut dae, &mut run), Stop::NeedData));
        assert!(run.is_empty());
    }

    #[test]
    fn next_stop_reports_an_exhausted_sequence() {
        let mut dae = DaeState::for_test(Vec::new(), Some(0));
        assert!(matches!(
            next_stop(&mut dae, &mut Vec::new()),
            Stop::Exhausted
        ));
    }
}
