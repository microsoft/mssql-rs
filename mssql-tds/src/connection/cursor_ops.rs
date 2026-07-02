// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Server cursor RPCs (`sp_cursor*`), exposed through the
//! [`CursorClient`](crate::connection::cursor_ops::CursorClient) trait
//! implemented for [`TdsClient`](crate::connection::tds_client::TdsClient).
//!
//! Defining the cursor surface as a trait (mirroring
//! [`ResultSetClient`](crate::connection::tds_client::ResultSetClient)) keeps it
//! a distinct, swappable abstraction rather than inherent methods. Bring
//! [`CursorClient`](crate::connection::cursor_ops::CursorClient) into scope to
//! call these methods on a [`TdsClient`](crate::connection::tds_client::TdsClient).

// The cursor RPC methods mirror the positional parameter lists of the
// `sp_cursor*` system stored procedures, so several legitimately take more than
// the default argument threshold.
#![allow(clippy::too_many_arguments)]

use crate::connection::execution_context::ALREADY_EXECUTING_ERROR;
use crate::connection::tds_client::{ResultSet, ReturnStatus, TdsClient};
use crate::core::{CancelHandle, TdsResult};
use crate::cursor::{
    CursorConcurrency, CursorOpenResponse, CursorOperation, CursorOptionCode, CursorOptionValue,
    CursorPrepExecResponse, CursorPrepareResponse, CursorScrollOption, CursorStatus,
    FetchDirection, FetchStatus,
};
use crate::datatypes::column_values::ColumnValues;
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sqltypes::SqlType;
use crate::error::Error::{ProtocolError, UsageError};
use crate::message::messages::Request;
use crate::message::parameters::rpc_parameters::{
    RpcParameter, StatusFlags, build_parameter_list_string,
};
use crate::message::rpc::{RpcProcs, RpcType, SqlRpc};
use async_trait::async_trait;
use tracing::instrument;

/// The server-cursor RPC surface (`sp_cursor*`) for a TDS client.
///
/// Implemented for [`TdsClient`]; bring this trait into scope to open, fetch
/// from, mutate, prepare, and close server cursors. Modeled on
/// [`ResultSetClient`](crate::connection::tds_client::ResultSetClient) so the
/// cursor behavior is a distinct, swappable abstraction rather than inherent
/// methods on the client.
#[async_trait]
pub trait CursorClient {
    /// Opens a server cursor with a SQL statement (`sp_cursoropen`, RPC ID 2).
    ///
    /// Returns the server-assigned cursor handle and negotiated scroll/concurrency
    /// options. The response stream (including any metadata tokens) is fully
    /// consumed before returning.
    ///
    /// TODO: `AUTO_FETCH` is not yet supported. Passing `AUTO_FETCH` in
    /// `scroll_opt` returns an error. This will be implemented in a future PR.
    async fn cursor_open(
        &mut self,
        stmt: &str,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse>;

    /// Opens a parameterized server cursor (`sp_cursoropen`, RPC ID 2).
    ///
    /// Same as [`cursor_open`](Self::cursor_open) but includes a parameter
    /// declaration list and bound parameter values. The `PARAMETERIZED_STMT`
    /// flag (`0x1000`) is added to `scroll_opt` only when `params` is non-empty.
    async fn cursor_open_with_params(
        &mut self,
        stmt: &str,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse>;

    /// Fetches rows from an open cursor (`sp_cursorfetch`, RPC ID 7).
    ///
    /// After calling this, use `get_next_row_into()` to read rows and then
    /// `close_query()` before the next command. If no rows are available
    /// (end of cursor), `has_open_batch` will be false and no row reading
    /// is needed.
    async fn cursor_fetch(
        &mut self,
        cursor_id: i32,
        direction: FetchDirection,
        row_num: i32,
        num_rows: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()>;

    /// Reads the next row from an open cursor's fetch buffer, splitting off the
    /// hidden trailing `rowstat` column and decoding it as a [`FetchStatus`].
    ///
    /// `sp_cursorfetch` appends a hidden `int` `rowstat` column to every row
    /// (see [`cursor_fetch`](Self::cursor_fetch)). This method strips that
    /// column from the returned data and decodes it, so a driver can report
    /// per-row state (`SQL_ROW_DELETED` / `UPDATED` / `ADDED`) for keyset and
    /// dynamic cursors. Use it instead of [`next_row`](ResultSet::next_row) when
    /// consuming a cursor fetch.
    ///
    /// Returns `Ok(None)` at the end of the fetch buffer. The data columns are
    /// returned without the `rowstat`, though
    /// [`get_metadata`](ResultSet::get_metadata) still includes its descriptor.
    /// Returns a usage error if the current result set is not a cursor fetch
    /// (no trailing `rowstat` column).
    async fn next_cursor_row(&mut self) -> TdsResult<Option<(Vec<ColumnValues>, FetchStatus)>>;

    /// Closes a server cursor and releases server resources (`sp_cursorclose`, RPC ID 9).
    ///
    /// After this call the `cursor_id` is invalid and must not be reused.
    /// Passing `-1` closes all cursors on the current connection.
    async fn cursor_close(
        &mut self,
        cursor_id: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()>;

    /// Performs a positioned operation on the current fetch buffer of an open
    /// cursor (`sp_cursor`, RPC ID 1).
    ///
    /// Supports positioned `UPDATE` / `DELETE` / `INSERT` (and `LOCK`,
    /// `REFRESH`, `SETPOSITION`) via [`CursorOperation`]. The `values` are the
    /// column values for `UPDATE` / `INSERT`, supplied as **named** parameters
    /// whose names are the target columns prefixed with `@` (e.g. `@Name`);
    /// pass an empty vector for `DELETE` / `LOCK`. `rownum` selects the 1-based
    /// row within the fetch buffer (`0` targets all rows). `table` names the
    /// target table when the cursor's SELECT joins multiple tables; pass `""`
    /// to default to the first table in the FROM clause.
    ///
    /// Requires an updatable cursor (non-`READONLY` concurrency). A concurrency
    /// conflict or constraint violation is surfaced as an
    /// [`Error`](crate::error::Error) carrying the server message.
    async fn perform_cursor_operation(
        &mut self,
        cursor_id: i32,
        optype: CursorOperation,
        rownum: i32,
        table: &str,
        values: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()>;

    /// Sets an option on an open cursor (`sp_cursoroption`, RPC ID 8).
    ///
    /// Most commonly used to assign a **name** to the cursor
    /// ([`CursorOptionCode::CursorName`]) so Transact-SQL `WHERE CURRENT OF`
    /// positioned statements can target it, or to toggle text-pointer handling.
    /// The `value` variant must match what `code` expects: only
    /// [`CursorOptionCode::CursorName`] takes a [`CursorOptionValue::String`];
    /// every other code takes a [`CursorOptionValue::Int`]. A mismatch returns
    /// a usage error without contacting the server.
    async fn set_cursor_option(
        &mut self,
        cursor_id: i32,
        code: CursorOptionCode,
        value: CursorOptionValue,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()>;

    /// Prepares and opens a server cursor in a single round-trip
    /// (`sp_cursorprepexec`, RPC ID 5).
    ///
    /// Returns a reusable prepared handle (for later [`cursor_execute`] /
    /// [`cursor_unprepare`]) together with the opened cursor's handle and
    /// negotiated scroll/concurrency options. The parameter declaration list is
    /// built internally from `params`; the `PARAMETERIZED_STMT` flag is added
    /// automatically when `params` is non-empty.
    ///
    /// Metadata is requested via the default RPC header — no `options` proc
    /// parameter is sent, matching the ODBC wire contract.
    ///
    /// TODO: `AUTO_FETCH` is not yet supported. Passing `AUTO_FETCH` in
    /// `scroll_opt` returns an error.
    ///
    /// TODO: Piggyback unprepare is not exposed. The `sp_cursorprepexec`
    /// first procedure parameter (prepared-handle input/output) can carry an existing handle to
    /// release it in the same round-trip; this method always sends NULL, so a
    /// previously prepared handle must be released separately via
    /// [`cursor_unprepare`](Self::cursor_unprepare).
    ///
    /// [`cursor_execute`]: Self::cursor_execute
    /// [`cursor_unprepare`]: Self::cursor_unprepare
    async fn cursor_prepexec(
        &mut self,
        stmt: &str,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorPrepExecResponse>;

    /// Executes a previously prepared cursor (`sp_cursorexecute`, RPC ID 4).
    ///
    /// Opens a fresh cursor from a prepare handle returned by [`cursor_prepare`]
    /// or [`cursor_prepexec`], returning a **new** cursor handle each time along
    /// with the negotiated scroll/concurrency options. Bound parameter values are
    /// supplied via `params`; their types were fixed at prepare time, so no
    /// declaration list is sent.
    ///
    /// TODO: `AUTO_FETCH` is not yet supported. Passing `AUTO_FETCH` in
    /// `scroll_opt` returns an error.
    ///
    /// [`cursor_prepare`]: Self::cursor_prepare
    /// [`cursor_prepexec`]: Self::cursor_prepexec
    async fn cursor_execute(
        &mut self,
        prepared_handle: i32,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse>;

    /// Prepares a server cursor without opening it (`sp_cursorprepare`, RPC ID 3).
    ///
    /// Compiles the statement and returns a reusable prepared handle plus the
    /// negotiated scroll/concurrency options. No cursor is opened and no rows are
    /// returned — call [`cursor_execute`] to open a cursor from the handle.
    ///
    /// `param_def` is the explicit parameter declaration list (e.g.
    /// `"@p1 INT, @p2 NVARCHAR(50)"`); pass `""` for a non-parameterized
    /// statement. The `options` parameter (`PREPARE_METADATA`) is sent so the
    /// server returns the result-set column metadata.
    ///
    /// [`cursor_execute`]: Self::cursor_execute
    async fn cursor_prepare(
        &mut self,
        stmt: &str,
        param_def: &str,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorPrepareResponse>;

    /// Releases a prepared cursor handle (`sp_cursorunprepare`, RPC ID 6).
    ///
    /// After this call the `prepared_handle` is invalid and must not be reused
    /// with [`cursor_execute`](Self::cursor_execute).
    async fn cursor_unprepare(
        &mut self,
        prepared_handle: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()>;
}

#[async_trait]
impl CursorClient for TdsClient {
    #[instrument(skip(self), level = "info")]
    async fn cursor_open(
        &mut self,
        stmt: &str,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse> {
        if scroll_opt.contains(CursorScrollOption::AUTO_FETCH) {
            return Err(crate::error::Error::UsageError(
                "AUTO_FETCH is not yet supported in cursor_open".to_string(),
            ));
        }

        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // Parameter order matches sp_cursoropen positional spec.
        let params = vec![
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(stmt.to_string()))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(scroll_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(cc_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(row_count)),
            ),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorOpen),
            Some(params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        self.consume_cursor_open_response().await
    }

    #[instrument(skip(self, params), level = "info")]
    async fn cursor_open_with_params(
        &mut self,
        stmt: &str,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse> {
        if scroll_opt.contains(CursorScrollOption::AUTO_FETCH) {
            return Err(crate::error::Error::UsageError(
                "AUTO_FETCH is not yet supported in cursor_open_with_params".to_string(),
            ));
        }

        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // Add PARAMETERIZED_STMT only when the statement actually has parameters.
        let scroll_opt = if params.is_empty() {
            scroll_opt
        } else {
            scroll_opt | CursorScrollOption::PARAMETERIZED_STMT
        };

        // Build the parameter declaration string from the named params
        let mut param_def_string = String::new();
        build_parameter_list_string(&params, &mut param_def_string)?;

        // Parameter order matches sp_cursoropen positional spec.
        let positional_params = vec![
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(stmt.to_string()))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(scroll_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(cc_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(row_count)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(param_def_string))),
            ),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorOpen),
            Some(positional_params),
            Some(params),
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        self.consume_cursor_open_response().await
    }

    #[instrument(skip(self), level = "info")]
    async fn cursor_fetch(
        &mut self,
        cursor_id: i32,
        direction: FetchDirection,
        row_num: i32,
        num_rows: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        let params = vec![
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(cursor_id))),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::Int(Some(direction.bits() as i32)),
            ),
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(row_num))),
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(num_rows))),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorFetch),
            Some(params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        let metadata = self.move_to_column_metadata().await?;
        if metadata.is_none() {
            self.execution_context.set_has_open_batch(false);
            self.current_result_set_has_been_read_till_end = true;
        } else {
            self.current_metadata = metadata;
            self.current_result_set_has_been_read_till_end = false;
            self.execution_context.set_has_open_batch(true);
        }
        Ok(())
    }

    #[instrument(skip(self), level = "info")]
    async fn next_cursor_row(&mut self) -> TdsResult<Option<(Vec<ColumnValues>, FetchStatus)>> {
        // Guard: only valid on an sp_cursorfetch result, whose last column is the
        // trailing `rowstat`. The server names it `rowstat` but does NOT set the
        // hidden flag on it, so it is identified by name. Refuse to strip a real
        // column from a normal result.
        let has_rowstat = self
            .get_metadata()
            .last()
            .map(|c| c.column_name.eq_ignore_ascii_case("rowstat"))
            .unwrap_or(false);
        if !has_rowstat {
            return Err(UsageError(
                "next_cursor_row requires a cursor fetch result with a trailing rowstat column"
                    .to_string(),
            ));
        }

        let Some(mut row) = self.next_row().await? else {
            return Ok(None);
        };
        let rowstat = row.pop().ok_or_else(|| {
            crate::error::Error::ProtocolError(
                "cursor fetch row is missing its trailing rowstat column".to_string(),
            )
        })?;
        let bits = match rowstat {
            ColumnValues::Int(v) => v as u32,
            other => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "expected an INT rowstat column at the end of a cursor fetch row, got {other:?}"
                )));
            }
        };
        Ok(Some((row, FetchStatus::from_bits_truncate(bits))))
    }

    #[instrument(skip(self), level = "info")]
    async fn cursor_close(
        &mut self,
        cursor_id: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        let params = vec![RpcParameter::new(
            None,
            StatusFlags::NONE,
            SqlType::Int(Some(cursor_id)),
        )];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorClose),
            Some(params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        let server_errors = self.drain_stream().await?;
        self.execution_context.set_has_open_batch(false);
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        Ok(())
    }

    #[instrument(skip(self, values), level = "info")]
    async fn perform_cursor_operation(
        &mut self,
        cursor_id: i32,
        optype: CursorOperation,
        rownum: i32,
        table: &str,
        values: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        // The column values are sent as named (@column) RPC parameters, so each
        // must carry a name; an unnamed parameter would panic during
        // serialization. Reject it as a usage error before any I/O.
        if values.iter().any(|p| p.name.is_none()) {
            return Err(UsageError(
                "perform_cursor_operation values must be named parameters (column names prefixed with `@`)"
                    .to_string(),
            ));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // sp_cursor positional params: cursor, optype, rownum, table. The column
        // values follow as named (@column) RPC parameters, so no parameter
        // declaration string is sent (unlike sp_cursoropen).
        let positional_params = vec![
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(cursor_id))),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::Int(Some(optype.bits() as i32)),
            ),
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(rownum))),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(table.to_string()))),
            ),
        ];

        let user_params = if values.is_empty() {
            None
        } else {
            Some(values)
        };

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::Cursor),
            Some(positional_params),
            user_params,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        let server_errors = self.drain_stream().await?;
        self.execution_context.set_has_open_batch(false);
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        Ok(())
    }

    #[instrument(skip(self), level = "info")]
    async fn set_cursor_option(
        &mut self,
        cursor_id: i32,
        code: CursorOptionCode,
        value: CursorOptionValue,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        // Validate the value type matches the option code before any server
        // round-trip, so bad input fails fast.
        let value_param = match (&value, code.expects_string()) {
            (CursorOptionValue::String(s), true) => RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(s.clone()))),
            ),
            (CursorOptionValue::Int(i), false) => {
                RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(*i)))
            }
            _ => {
                return Err(UsageError(format!(
                    "sp_cursoroption code {code:?} expects a {} value",
                    if code.expects_string() {
                        "string"
                    } else {
                        "integer"
                    }
                )));
            }
        };

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // sp_cursoroption positional params: cursor, code, value.
        let params = vec![
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(cursor_id))),
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(code as i32))),
            value_param,
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorOption),
            Some(params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        let server_errors = self.drain_stream().await?;
        self.execution_context.set_has_open_batch(false);
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        Ok(())
    }

    #[instrument(skip(self, params), level = "info")]
    async fn cursor_prepexec(
        &mut self,
        stmt: &str,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorPrepExecResponse> {
        if scroll_opt.contains(CursorScrollOption::AUTO_FETCH) {
            return Err(UsageError(
                "AUTO_FETCH is not yet supported in cursor_prepexec".to_string(),
            ));
        }

        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // Add PARAMETERIZED_STMT only when the statement actually has parameters.
        let scroll_opt = if params.is_empty() {
            scroll_opt
        } else {
            scroll_opt | CursorScrollOption::PARAMETERIZED_STMT
        };

        // Build the parameter declaration list from the named params.
        let mut param_def_string = String::new();
        build_parameter_list_string(&params, &mut param_def_string)?;

        // Parameter order matches the sp_cursorprepexec ODBC wire contract:
        // prepared_handle(OUT), cursor(OUT), params(decl), stmt, scrollopt, ccopt, rowcount.
        let positional_params = vec![
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(param_def_string))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(stmt.to_string()))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(scroll_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(cc_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(row_count)),
            ),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorPrepExec),
            Some(positional_params),
            Some(params),
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        self.drain_cursor_response().await?;
        let status = self.captured_cursor_status()?;

        // OUTPUT ordinals: 0=prepared_handle, 1=cursor, 2=scrollopt, 3=ccopt, 4=rowcount
        Ok(CursorPrepExecResponse {
            prepared_handle: self.extract_return_value_int(0)?,
            cursor: CursorOpenResponse {
                cursor_id: self.extract_return_value_int(1)?,
                negotiated_scroll: CursorScrollOption::from_bits_truncate(
                    self.extract_return_value_int(2)? as u32,
                ),
                negotiated_concurrency: CursorConcurrency::from_bits_truncate(
                    self.extract_return_value_int(3)? as u32,
                ),
                row_count: self.extract_return_value_int(4)?,
                status,
            },
        })
    }

    #[instrument(skip(self, params), level = "info")]
    async fn cursor_execute(
        &mut self,
        prepared_handle: i32,
        params: Vec<RpcParameter>,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        row_count: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorOpenResponse> {
        if scroll_opt.contains(CursorScrollOption::AUTO_FETCH) {
            return Err(UsageError(
                "AUTO_FETCH is not yet supported in cursor_execute".to_string(),
            ));
        }

        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // Parameter order matches the sp_cursorexecute wire contract:
        // prepared_handle(IN), cursor(OUT), scrollopt, ccopt, rowcount.
        let positional_params = vec![
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(prepared_handle))),
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(scroll_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(cc_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(row_count)),
            ),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorExecute),
            Some(positional_params),
            Some(params),
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        // prepared_handle is plain INPUT (no ReturnValue), so the OUTPUT ordinals
        // match sp_cursoropen: 0=cursor, 1=scrollopt, 2=ccopt, 3=rowcount.
        self.consume_cursor_open_response().await
    }

    #[instrument(skip(self), level = "info")]
    async fn cursor_prepare(
        &mut self,
        stmt: &str,
        param_def: &str,
        scroll_opt: CursorScrollOption,
        cc_opt: CursorConcurrency,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<CursorPrepareResponse> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        // Add PARAMETERIZED_STMT only when a declaration list is supplied.
        let scroll_opt = if param_def.is_empty() {
            scroll_opt
        } else {
            scroll_opt | CursorScrollOption::PARAMETERIZED_STMT
        };

        // options = PREPARE_METADATA: ask the server to return the column metadata.
        const PREPARE_METADATA: i32 = 0x0001;

        // Parameter order matches the sp_cursorprepare wire contract:
        // prepared_handle(OUT), params(decl), stmt, options, scrollopt, ccopt.
        let positional_params = vec![
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None)),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(param_def.to_string()))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarcharMax(Some(SqlString::from_utf8_string(stmt.to_string()))),
            ),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::Int(Some(PREPARE_METADATA)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(scroll_opt.bits() as i32)),
            ),
            RpcParameter::new(
                None,
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(Some(cc_opt.bits() as i32)),
            ),
        ];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorPrepare),
            Some(positional_params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        self.drain_cursor_response().await?;
        let status = self.captured_cursor_status()?;

        // OUTPUT ordinals: 0=prepared_handle, 1=scrollopt, 2=ccopt (no rowcount).
        Ok(CursorPrepareResponse {
            prepared_handle: self.extract_return_value_int(0)?,
            negotiated_scroll: CursorScrollOption::from_bits_truncate(
                self.extract_return_value_int(1)? as u32,
            ),
            negotiated_concurrency: CursorConcurrency::from_bits_truncate(
                self.extract_return_value_int(2)? as u32,
            ),
            status,
        })
    }

    #[instrument(skip(self), level = "info")]
    async fn cursor_unprepare(
        &mut self,
        prepared_handle: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.return_values.clear();
        self.transport.reset_reader();

        let db_collation = self.negotiated_settings.database_collation;

        let params = vec![RpcParameter::new(
            None,
            StatusFlags::NONE,
            SqlType::Int(Some(prepared_handle)),
        )];

        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::CursorUnprepare),
            Some(params),
            None,
            &db_collation,
            &self.execution_context,
        );

        let mut pw =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut pw).await?;

        let server_errors = self.drain_stream().await?;
        self.execution_context.set_has_open_batch(false);
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        Ok(())
    }
}

/// Private cursor-response helpers used by the [`CursorClient`] implementation.
impl TdsClient {
    /// Extracts an `i32` from the return values at the given index.
    fn extract_return_value_int(&self, index: usize) -> TdsResult<i32> {
        let rv = self.return_values.get(index).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Expected return value at index {index}, but only {} values received",
                self.return_values.len()
            ))
        })?;
        match &rv.value {
            ColumnValues::Int(v) => Ok(*v),
            other => Err(crate::error::Error::ProtocolError(format!(
                "Expected Int return value at index {index}, got {other:?}"
            ))),
        }
    }

    /// Shared response handling for `cursor_open` and `cursor_open_with_params`.
    ///
    /// The sp_cursoropen response sends ColMetadata (describing cursor columns)
    /// followed by TabName, ColInfo, Order tokens, then DoneInProc,
    /// ReturnStatus, ReturnValue×4, DoneProc.
    /// `move_to_column_metadata` stops at ColMetadata; `drain_stream` reads the
    /// rest and collects the OUTPUT parameters.
    async fn consume_cursor_open_response(&mut self) -> TdsResult<CursorOpenResponse> {
        self.drain_cursor_response().await?;
        let status = self.captured_cursor_status()?;

        Ok(CursorOpenResponse {
            cursor_id: self.extract_return_value_int(0)?,
            negotiated_scroll: CursorScrollOption::from_bits_truncate(
                self.extract_return_value_int(1)? as u32,
            ),
            negotiated_concurrency: CursorConcurrency::from_bits_truncate(
                self.extract_return_value_int(2)? as u32,
            ),
            row_count: self.extract_return_value_int(3)?,
            status,
        })
    }

    /// Interprets the most recently captured `ReturnStatus` token as a
    /// [`CursorStatus`] for the open-family RPCs.
    ///
    /// A missing token (`NotReceived`) maps to [`CursorStatus::Succeeded`]; an
    /// unrecognized raw value is surfaced as a protocol error rather than being
    /// silently treated as success.
    fn captured_cursor_status(&self) -> TdsResult<CursorStatus> {
        match self.last_return_status {
            ReturnStatus::NotReceived => Ok(CursorStatus::Succeeded),
            ReturnStatus::Received(raw) => CursorStatus::from_raw(raw).ok_or_else(|| {
                ProtocolError(format!(
                    "server returned an unrecognized cursor status: {raw}"
                ))
            }),
        }
    }

    /// Shared response tail for the cursor open-family RPCs (`sp_cursoropen`,
    /// `sp_cursorexecute`, `sp_cursorprepexec`, `sp_cursorprepare`).
    ///
    /// Captures any schema `ColMetadata`, then eagerly drains the remaining
    /// tokens (`ReturnValue`/`ReturnStatus`/`Done`), resets the batch state, and
    /// surfaces any server errors. The OUTPUT parameters are left in
    /// `self.return_values` for the caller to extract by ordinal.
    async fn drain_cursor_response(&mut self) -> TdsResult<()> {
        // Clear any stale metadata up-front so an error here cannot leak columns
        // from a previous result set through get_metadata().
        self.current_metadata = None;
        // Clear any stale return status so a missing ReturnStatus token surfaces
        // as Succeeded rather than the previous RPC's status.
        self.last_return_status = ReturnStatus::NotReceived;
        let metadata = self.move_to_column_metadata().await?;
        self.current_metadata = metadata;
        let server_errors = self.drain_stream().await?;
        self.execution_context.set_has_open_batch(false);
        self.current_result_set_has_been_read_till_end = true;

        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        Ok(())
    }
}
