// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;

use crate::connection::bulk_copy::{BulkCopyOptions, BulkLoadRow, ResolvedColumnMapping};
use crate::connection::bulk_copy_state::ATTENTION_TIMEOUT_SECONDS;
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use crate::datatypes::row_writer::{DefaultRowWriter, RowWriter};
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sqltypes::SqlType;
use crate::error::Error::UsageError;
use crate::error::SqlErrorInfo;
use crate::io::packet_writer::PacketWriter;
use crate::message::bulk_load::{StreamingBulkLoadWriter, build_insert_bulk_command};
use crate::message::messages::PacketType;
use crate::message::parameters::rpc_parameters::{
    RpcParameter, StatusFlags, build_parameter_list_string,
};
use crate::message::rpc::{RpcProcs, RpcType, SqlRpc};
use crate::message::transaction_management::{
    CreateTxnParams, TransactionIsolationLevel, TransactionManagementRequest,
    TransactionManagementType,
};
use crate::query::result::ReturnValue;
use crate::token::tokens::SqlCollation;
use crate::{
    connection::{
        execution_context::{ALREADY_EXECUTING_ERROR, ExecutionContext},
        transport::tds_transport::TdsTransport,
    },
    datatypes::column_values::ColumnValues,
    handler::handler_factory::NegotiatedSettings,
    io::token_stream::{ParserContext, RowReadResult},
    message::{batch::SqlBatch, messages::Request},
    token::tokens::{ColMetadataToken, CurrentCommand, Tokens},
};
use async_trait::async_trait;
use tracing::{debug, error, info, instrument};

use crate::{
    core::{CancelHandle, TdsResult},
    query::metadata::ColumnMetadata,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Active TDS connection to a SQL Server instance.
///
/// Created by [`TdsConnectionProvider::create_client()`](crate::connection_provider::tds_connection_provider::TdsConnectionProvider::create_client).
/// Provides methods for executing queries, managing transactions, and bulk copy.
#[derive(Debug)]
pub struct TdsClient {
    pub(crate) transport: Box<dyn TdsTransport>,
    pub(crate) negotiated_settings: NegotiatedSettings,
    pub(crate) execution_context: ExecutionContext,

    // pub(crate) batch_result: Option<BatchResult<'static>>,
    pub(crate) current_metadata: Option<Arc<ColMetadataToken>>,
    count_map: HashMap<CurrentCommand, u64>,

    return_values: Vec<ReturnValue>,
    current_result_set_has_been_read_till_end: bool,

    /// The remaining request timeout for operations. This is updated after each token read.
    remaining_request_timeout: Option<Duration>,

    /// The cancel handle for this client. Used to cancel operations.
    cancel_handle: Option<CancelHandle>,

    /// Empty metadata vector for returning when no metadata is available
    empty_metadata: Vec<ColumnMetadata>,
}

impl TdsClient {
    pub(crate) fn new(
        transport: Box<dyn TdsTransport>,
        negotiated_settings: NegotiatedSettings,
        execution_context: ExecutionContext,
    ) -> Self {
        Self {
            transport,
            negotiated_settings,
            execution_context,
            current_metadata: None,
            count_map: HashMap::new(),
            return_values: Vec::new(),
            current_result_set_has_been_read_till_end: false,
            remaining_request_timeout: None,
            cancel_handle: None,
            empty_metadata: Vec::new(),
        }
    }

    /// Returns the database collation negotiated during login.
    pub fn get_collation(&self) -> SqlCollation {
        self.negotiated_settings.database_collation
    }

    pub(crate) fn get_current_metadata(&self) -> Option<&ColMetadataToken> {
        self.current_metadata.as_deref()
    }

    /// Converts an `Option<u32>` timeout (where `Some(0)` means infinite) to `Option<Duration>`.
    ///
    /// The bulk copy API uses `0` to mean "no timeout" (infinite). This helper
    /// normalises that convention so `Some(0)` becomes `None` (no deadline).
    fn timeout_to_duration(timeout_sec: Option<u32>) -> Option<Duration> {
        timeout_sec.and_then(|secs| {
            if secs == 0 {
                None
            } else {
                Some(Duration::from_secs(secs as u64))
            }
        })
    }

    /// Updates the remaining timeout by subtracting the elapsed time.
    fn update_remaining_timeout(&mut self, start: Instant) {
        self.remaining_request_timeout = self.remaining_request_timeout.map(|t| {
            let elapsed = start.elapsed();
            if elapsed > t {
                Duration::ZERO
            } else {
                t.saturating_sub(elapsed)
            }
        });
    }

    /// Sends a SQL batch to the server for execution.
    ///
    /// Wraps the SQL text in a TDS `SQL_BATCH` message. After this call returns,
    /// use [`read_row()`](Self::read_row) to consume result rows, then
    /// [`close_query()`](Self::close_query) to finalize.
    ///
    /// # Parameters
    /// - `sql_command` — raw T-SQL text to execute.
    /// - `timeout_sec` — per-request timeout in seconds. `None` means no timeout.
    /// - `cancel_handle` — optional [`CancelHandle`] for cooperative cancellation.
    ///   A child token is derived so cancelling the handle aborts this request
    ///   without tearing down the connection.
    ///
    /// # Errors
    /// Returns [`UsageError`](crate::error::Error::UsageError) if a previous
    /// batch is still open.
    #[instrument(skip(self), level = "info")]
    pub async fn execute(
        &mut self,
        sql_command: String,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();
        let batch = SqlBatch::new(sql_command, &self.execution_context);
        let mut packet_writer =
            batch.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        batch.serialize(&mut packet_writer).await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
        if metadata.is_none() {
            self.execution_context.set_has_open_batch(false);
        } else {
            self.current_metadata = metadata;

            self.execution_context.set_has_open_batch(true);
        }
        Ok(())
    }

    /// Executes a parameterized query via `sp_executesql`.
    ///
    /// The SQL text and parameter declarations are sent as positional RPC
    /// arguments. Caller-supplied `named_params` are appended as named
    /// parameters — each [`RpcParameter`] must have a `name` matching the
    /// declaration in the query (e.g. `@id`).
    ///
    /// This is the primary path for parameterized queries; prefer it over
    /// string interpolation to avoid SQL injection and benefit from plan
    /// caching on the server.
    ///
    /// # Parameters
    /// - `sql` — parameterized T-SQL statement.
    /// - `named_params` — parameter values. Build with [`RpcParameter::new`].
    /// - `timeout_sec` / `cancel_handle` — see [`execute()`](Self::execute).
    #[instrument(skip(self, named_params), level = "info")]
    pub async fn execute_sp_executesql(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();
        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql.clone())));

        // Create the parameter list for sp_execute_sql
        let statement_parameter = RpcParameter::new(None, StatusFlags::NONE, sql_statement_value);

        // Build the comma separated list of parameters
        let mut params_list_as_string = String::new();

        build_parameter_list_string(&named_params, &mut params_list_as_string);

        let params_as_sql_string = SqlType::NVarcharMax(Some(SqlString::from_utf8_string(
            params_list_as_string.clone(),
        )));

        let params_parameter = RpcParameter::new(None, StatusFlags::NONE, params_as_sql_string);

        // Create the parameter list for positional parameters of sp_execute_sql.
        // These could be named parameters as well, but we want to avoid sending the name
        // to send less data over the wire.
        let positional_parameters_vec = vec![statement_parameter, params_parameter];
        let positional_parameters = Some(positional_parameters_vec);

        // Build the RPC request.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::ExecuteSql),
            positional_parameters,
            Some(named_params),
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
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

    /// Executes a bulk load operation using zero-copy streaming.
    ///
    /// This method provides superior performance by eliminating per-row Vec allocations.
    /// Rows are serialized directly to the packet writer via the `BulkLoadRow` trait.
    ///
    /// # Performance Benefits
    ///
    /// - **Zero allocations per row**: No `dest_buffer.clone()` needed
    /// - **Direct serialization**: Columns written directly to TDS packet
    /// - **Column context reuse**: Created once, reused for all rows
    ///
    /// # Type Parameters
    ///
    /// * `R` - Row type implementing `BulkLoadRow` trait
    ///
    /// # Arguments
    ///
    /// * `table_name` - Target table name
    /// * `column_metadata` - Column metadata for destination columns
    /// * `options` - Bulk copy options
    /// * `timeout_sec` - Optional timeout in seconds
    /// * `cancel_handle` - Optional cancellation handle
    /// * `rows` - Vector of rows to insert
    /// * `resolved_mappings` - Column mapping information
    ///
    /// # Returns
    ///
    /// Returns the number of rows actually inserted by SQL Server.
    #[instrument(skip(self, rows), level = "info")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_bulk_load_streaming_zerocopy<R>(
        &mut self,
        table_name: String,
        column_metadata: Vec<BulkCopyColumnMetadata>,
        options: BulkCopyOptions,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
        rows: impl Iterator<Item = R>,
        resolved_mappings: &[ResolvedColumnMapping],
    ) -> TdsResult<u64>
    where
        R: BulkLoadRow,
    {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();

        // STEP 1: Filter column metadata to only include mapped columns
        // If we have column mappings, only include the destination columns that are mapped.
        // This allows SQL Server to handle NULL/defaults for unmapped columns.
        let mapped_column_metadata = if resolved_mappings.is_empty() {
            // No mappings specified - use all columns (ordinal mapping)
            column_metadata.clone()
        } else {
            // Filter to only mapped destination columns, preserving their order
            resolved_mappings
                .iter()
                .map(|mapping| column_metadata[mapping.destination_index].clone())
                .collect()
        };

        // STEP 2: Send INSERT BULK command and consume response
        // Use the filtered metadata so the command only references mapped columns
        let insert_bulk_command =
            build_insert_bulk_command(&table_name, &mapped_column_metadata, &options)?;
        self.send_batch_and_consume_response(insert_bulk_command, timeout_sec, cancel_handle)
            .await?;

        // STEP 3: Create streaming writer and begin
        let default_collation = self.get_collation();

        let mut packet_writer = PacketWriter::new(
            PacketType::BulkLoad,
            self.transport.as_writer(),
            timeout_sec,
            cancel_handle,
        );

        let mut writer = StreamingBulkLoadWriter::new(
            &mut packet_writer,
            table_name,
            mapped_column_metadata,
            default_collation,
        );

        // Begin streaming (write metadata)
        writer.begin().await?;

        // STEP 3: Stream rows using zero-copy path
        // If an error occurs during row writing, we need to send an attention packet
        // to gracefully cancel the bulk load operation and leave the connection usable.
        let mut row_write_error: Option<crate::error::Error> = None;
        for row in rows {
            // Write the row directly using the streaming writer
            if let Err(e) = writer.write_row_zerocopy(&row).await {
                row_write_error = Some(e);
                break;
            }
        }

        // Handle error during row streaming
        if let Some(original_error) = row_write_error {
            // Send attention packet to cancel the bulk load operation gracefully
            // This tells SQL Server to abort the current operation and resets the
            // TDS protocol state so the connection can be reused.
            let attention_timeout = Duration::from_secs(ATTENTION_TIMEOUT_SECONDS);
            let _ = self.send_attention_with_timeout(attention_timeout).await;
            // Clear the open batch flag since we've cancelled the operation
            // This allows subsequent operations to use this connection
            self.execution_context.set_has_open_batch(false);
            return Err(original_error);
        }

        // STEP 4: End streaming (write DONE token and finalize)
        let _rows_written = writer.end().await?;

        // STEP 5: Read the final response with row count
        let rows_affected = self.consume_done_token().await?;

        Ok(rows_affected)
    }

    /// Consumes response tokens until a DONE token is received.
    /// Returns the row count from the DONE token.
    ///
    /// This helper method implements the standard TDS response consumption pattern,
    /// handling INFO, ERROR, and DONE tokens appropriately.
    async fn consume_done_token(&mut self) -> TdsResult<u64> {
        let parser_context = ParserContext::None(());
        let mut rows_affected = 0_u64;
        let mut collected_errors: Vec<SqlErrorInfo> = Vec::new();

        loop {
            let start = Instant::now();
            let token = self
                .transport
                .receive_token(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                )
                .await?;
            self.update_remaining_timeout(start);

            match token {
                Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done) => {
                    info!("Done token: {:?}", done);

                    if done.has_error() && collected_errors.is_empty() {
                        return Err(crate::error::Error::ProtocolError(
                            "Server reported error in DONE token without preceding ERROR token"
                                .to_string(),
                        ));
                    }

                    // Accumulate row count from multiple DONE tokens
                    rows_affected += done.row_count;

                    // Stop when we receive a DONE token without the MORE flag
                    if !done.has_more() {
                        break;
                    }
                }
                Tokens::Error(error_token) => {
                    info!(?error_token);
                    collected_errors.push(SqlErrorInfo::from(&error_token));
                }
                Tokens::Info(info_token) => {
                    info!(?info_token);
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    self.execution_context
                        .capture_change_property(&env_change)?;
                    continue;
                }
                _ => {
                    info!("Unexpected token during bulk load: {:?}", token);
                    return Err(UsageError(format!(
                        "Unexpected token while executing bulk load: {token:?}"
                    )));
                }
            }
        }

        if !collected_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(collected_errors));
        }

        Ok(rows_affected)
    }

    /// Sends a SQL batch and consumes the response without expecting column metadata.
    /// This is used for commands that don't return result sets (DML statements, etc.).
    ///
    /// Returns the row count from the DONE token.
    async fn send_batch_and_consume_response(
        &mut self,
        sql_command: String,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<u64> {
        let batch = SqlBatch::new(sql_command, &self.execution_context);
        let mut packet_writer =
            batch.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        batch.serialize(&mut packet_writer).await?;

        // Consume the response
        self.consume_done_token().await
    }

    /// Executes a stored procedure via the TDS RPC protocol.
    ///
    /// Sends an `sp_executesql`-style RPC request for the named procedure.
    /// Parameters can be supplied positionally, by name, or both. If the
    /// procedure returns result sets, iterate rows with
    /// [`move_to_next()`](Self::move_to_next) and
    /// [`column_value()`](Self::column_value). After all result sets are
    /// consumed, retrieve output parameters with
    /// [`get_return_values()`](Self::get_return_values).
    ///
    /// Only one batch may be active at a time — calling this while a previous
    /// result set is unread returns [`Error::UsageError`](crate::error::Error::UsageError).
    ///
    /// # Cancel / Timeout
    ///
    /// Pass `timeout_sec` to cap server-side execution time, or supply a
    /// [`CancelHandle`] to cancel the operation cooperatively from another
    /// task.
    #[instrument(skip(self, positional_parameters, named_parameters), level = "info")]
    pub async fn execute_stored_procedure(
        &mut self,
        stored_procedure_name: String,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();
        let database_collation = self.negotiated_settings.database_collation;

        let rpc = SqlRpc::new(
            RpcType::Named(stored_procedure_name),
            positional_parameters,
            named_parameters,
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
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

    /// Prepares a parameterized statement via `sp_prepare` and returns the
    /// server-side handle.
    ///
    /// The returned `i32` handle can be passed to
    /// [`execute_sp_execute()`](Self::execute_sp_execute) for repeated
    /// execution without re-parsing. Call
    /// [`execute_sp_unprepare()`](Self::execute_sp_unprepare) when the handle
    /// is no longer needed.
    ///
    /// Drains the token stream internally — no rows are returned.
    #[instrument(skip(self, named_params), level = "info")]
    pub async fn execute_sp_prepare(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<i32> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value = SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql)));

        // Create the parameter list for sp_prepare
        let execute_sql_statement_parameter =
            RpcParameter::new(None, StatusFlags::NONE, sql_statement_value);

        // Build the comma separated list of parameters
        let mut params_list_as_string = String::new();

        build_parameter_list_string(&named_params, &mut params_list_as_string);

        let params_as_sql_string =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_list_as_string)));

        let params_parameter = RpcParameter::new(None, StatusFlags::NONE, params_as_sql_string);

        let output_handler_value = SqlType::Int(None);

        let output_handler_parameter = RpcParameter::new(
            None,
            StatusFlags::BY_REF_VALUE, // Output parameter
            output_handler_value,
        );

        // Create the parameter list for positional parameters of sp_prepare.
        let positional_parameters_vec = vec![
            output_handler_parameter,
            params_parameter,
            execute_sql_statement_parameter,
        ];
        let positional_parameters = Some(positional_parameters_vec);

        // Build the RPC request.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::Prepare),
            positional_parameters,
            Some(named_params),
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        // Drain to completion to get output parameters
        self.drain_stream().await?;

        // We need to get the return value, and then extract the handle from it.
        if self.return_values.len() == 1 {
            let returned_parameter = self.return_values.first().unwrap();
            if let ColumnValues::Int(handle) = &returned_parameter.value {
                Ok(*handle)
            } else {
                Err(crate::error::Error::ProtocolError(
                    "Expected an integer value".to_string(),
                ))
            }
        } else {
            Err(crate::error::Error::ProtocolError(
                "Expected exactly one output parameter".to_string(),
            ))
        }
    }

    /// Releases a prepared statement handle via `sp_unprepare`.
    ///
    /// Frees server-side resources associated with the handle returned by
    /// [`execute_sp_prepare()`](Self::execute_sp_prepare) or
    /// [`execute_sp_prepexec()`](Self::execute_sp_prepexec).
    #[instrument(skip(self), level = "info")]
    pub async fn execute_sp_unprepare(
        &mut self,
        handle: i32,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let handle_value = SqlType::Int(Some(handle));
        let handle_parameter = RpcParameter::new(None, StatusFlags::NONE, handle_value);

        let positional_parameters_vec = vec![handle_parameter];
        let positional_parameters = Some(positional_parameters_vec);

        // Build the RPC request.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::Unprepare),
            positional_parameters,
            None,
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        // Drain the result set. A successful unprepare will not return any results.
        self.drain_stream().await?;
        Ok(())
    }

    /// Prepares and executes a parameterized statement in a single round-trip
    /// via `sp_prepexec`.
    ///
    /// Combines [`execute_sp_prepare()`](Self::execute_sp_prepare) and
    /// [`execute_sp_execute()`](Self::execute_sp_execute). The prepared handle
    /// is stored internally and can be retrieved with
    /// [`get_return_values()`](Self::get_return_values).
    ///
    /// Result rows are available through [`read_row()`](Self::read_row) after
    /// this call returns.
    #[instrument(skip(self, named_params), level = "info")]
    pub async fn execute_sp_prepexec(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value = SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql)));

        // Create the parameter list for sp_prepexec
        let statement_parameter = RpcParameter::new(None, StatusFlags::NONE, sql_statement_value);

        // Build the comma separated list of parameters
        let mut params_list_as_string = String::new();

        build_parameter_list_string(&named_params, &mut params_list_as_string);

        let params_as_sql_string =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_list_as_string)));

        let params_parameter = RpcParameter::new(None, StatusFlags::NONE, params_as_sql_string);

        let handle_value = SqlType::Int(None);

        let handle_parameter = RpcParameter::new(None, StatusFlags::BY_REF_VALUE, handle_value);

        // Create the parameter list for positional parameters of sp_prepexec.
        let positional_parameters_list =
            vec![handle_parameter, params_parameter, statement_parameter];
        let positional_parameters = Some(positional_parameters_list);

        // Build the RPC request.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::PrepExec),
            positional_parameters,
            Some(named_params),
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
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

    /// Executes a previously prepared statement by handle via `sp_execute`.
    ///
    /// Re-uses the execution plan from an earlier
    /// [`execute_sp_prepare()`](Self::execute_sp_prepare) or
    /// [`execute_sp_prepexec()`](Self::execute_sp_prepexec) call.
    /// Supply fresh parameter values through `positional_parameters` and/or
    /// `named_parameters`.
    #[instrument(skip(self, positional_parameters, named_parameters), level = "info")]
    pub async fn execute_sp_execute(
        &mut self,
        handle: i32,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let handle_value = SqlType::Int(Some(handle));
        let handle_parameter = RpcParameter::new(None, StatusFlags::NONE, handle_value);

        // Create the parameter list for positional parameters of sp_execute.
        let mut all_positional_parameters = vec![handle_parameter];

        if let Some(mut params) = positional_parameters {
            all_positional_parameters.append(&mut params);
        }
        let all_positional_parameters = Some(all_positional_parameters);

        // Build the RPC request.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::Execute),
            all_positional_parameters,
            named_parameters,
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
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
    async fn drain_rows(&mut self) -> TdsResult<()> {
        if self.maybe_has_unread_rows() {
            // Drain the current result set.
            while let Some(row) = self.get_next_row().await? {
                info!("Consuming row while draining result set {:?}", row.len());
            }
        }
        Ok(())
    }

    /// Drains all remaining tokens from the stream until a terminal DONE token.
    /// Collects any ERROR tokens encountered and returns them.
    async fn drain_stream(&mut self) -> TdsResult<Vec<SqlErrorInfo>> {
        let mut collected_errors: Vec<SqlErrorInfo> = Vec::new();
        loop {
            let start = Instant::now();
            let token = self
                .transport
                .receive_token(
                    &ParserContext::None(()),
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                )
                .await?;
            self.update_remaining_timeout(start);

            match token {
                Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done) => {
                    info!(?done);
                    info!(?done.status);
                    if !done.has_more() {
                        break;
                    }
                }
                Tokens::Error(error_token) => {
                    info!(?error_token, "Draining ERROR token from stream");
                    collected_errors.push(SqlErrorInfo::from(&error_token));
                }
                Tokens::EnvChange(t1) => {
                    self.execution_context.capture_change_property(&t1)?;
                }
                Tokens::ReturnValue(return_value_token) => {
                    let return_value = return_value_token.into();
                    self.return_values.push(return_value);
                }
                Tokens::ReturnStatus(_return_status) => {
                    info!(?_return_status);
                }
                _ => {
                    info!(?token);
                }
            }
        }
        Ok(collected_errors)
    }

    #[instrument(skip(self), level = "debug", name = "move_to_column_metadata")]
    pub(crate) async fn move_to_column_metadata(
        &mut self,
    ) -> TdsResult<Option<Arc<ColMetadataToken>>> {
        let parser_context = ParserContext::None(());
        let mut col_metadata: Option<Arc<ColMetadataToken>> = None;
        let mut loop_count = 0u32;

        loop {
            loop_count += 1;

            // Warn when approaching iteration limit to help diagnose issues
            if loop_count.is_multiple_of(1000) {
                debug!(
                    loop_count,
                    "High iteration count in move_to_column_metadata"
                );
            }

            let start = Instant::now();
            let token = self
                .transport
                .receive_token(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                )
                .await?;
            self.update_remaining_timeout(start);
            match token {
                Tokens::ColMetadata(md) => {
                    info!(?md);
                    col_metadata = Some(Arc::new(md));
                    self.current_result_set_has_been_read_till_end = false;
                    break;
                }
                Tokens::DoneInProc(done) | Tokens::DoneProc(done) | Tokens::Done(done) => {
                    info!(
                        ?done,
                        "Received Done token with has_more={}",
                        done.has_more()
                    );

                    if done.has_error() {
                        return Err(crate::error::Error::ProtocolError(
                            "Server reported error in DONE token without preceding ERROR token"
                                .to_string(),
                        ));
                    }

                    let count = self.count_map.entry(done.cur_cmd).or_insert(0);
                    // Use saturating_add to prevent integer overflow from malicious/corrupted TDS responses
                    *count = count.saturating_add(done.row_count);
                    self.current_result_set_has_been_read_till_end = true;

                    if !done.has_more() {
                        // No more result sets - end of batch
                        info!("No more result sets (has_more=false), ending batch");
                        self.execution_context.set_has_open_batch(false);
                        break;
                    }

                    // has_more() is true - there are more result sets coming
                    // For DML operations (CREATE TABLE, INSERT, UPDATE, DELETE), there's no ColMetadata.
                    // The Done token represents the result, but we skip over it to find the next
                    // result set with ColMetadata (SELECT). This matches SQL Server behavior.
                    info!(
                        "More result sets available (has_more=true), continuing to look for ColMetadata"
                    );

                    // Prevent infinite loops from malicious inputs sending endless Done tokens with has_more=true
                    if loop_count > 10000 {
                        error!(
                            loop_count,
                            "Excessive iterations in move_to_column_metadata - possible malicious input or protocol violation"
                        );
                        return Err(crate::error::Error::UsageError(
                            "Too many Done tokens with has_more=true without ColMetadata"
                                .to_string(),
                        ));
                    }
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    self.execution_context
                        .capture_change_property(&env_change)?;
                }
                Tokens::ReturnValue(return_value_token) => {
                    let return_value = return_value_token.into();
                    self.return_values.push(return_value);
                }
                Tokens::ReturnStatus(return_status) => {
                    info!("Received return_status token: {:?}", return_status);
                    continue;
                }
                Tokens::Error(error_token) => {
                    info!(?error_token);
                    let mut all_errors = vec![SqlErrorInfo::from(&error_token)];
                    let mut drain_errors = self.drain_stream().await?;
                    all_errors.append(&mut drain_errors);
                    self.execution_context.set_has_open_batch(false);
                    return Err(crate::error::Error::from_sql_errors(all_errors));
                }
                Tokens::Info(info_token) => {
                    info!(?info_token);
                    continue;
                }
                _ => {
                    info!("move_to_column_metadata: {:?}", token);
                    return Err(UsageError(format!(
                        "Unexpected token while moving to column metadata: {token:?}"
                    )));
                }
            }
        }
        Ok(col_metadata)
    }

    /// This functions returns to the next row in the result set.
    /// If there are no more rows, it returns None.
    #[instrument(skip(self), level = "info")]
    pub(crate) async fn get_next_row(&mut self) -> TdsResult<Option<Vec<ColumnValues>>> {
        let col_count = self
            .current_metadata
            .as_ref()
            .map(|m| m.columns.len())
            .unwrap_or(0);
        let mut writer = DefaultRowWriter::new(col_count);
        if self.get_next_row_into(&mut writer).await? {
            Ok(Some(writer.take_row()))
        } else {
            Ok(None)
        }
    }

    /// Decodes the next row directly into a [`RowWriter`], returning `true` if
    /// a row was written or `false` when the result set is exhausted.
    ///
    /// Uses `receive_row_into` to decode ROW/NBCROW tokens directly through
    /// `decode_into`, bypassing the intermediate `RowToken { all_values }`.
    #[instrument(skip(self, writer), level = "info")]
    pub(crate) async fn get_next_row_into(
        &mut self,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<bool> {
        if self.current_metadata.is_none() {
            return Err(UsageError(
                "No metadata found while fetching the next row. Have you called the execute method or was the query supposed to return resultset?".to_string(),
            ));
        }
        let parser_context =
            ParserContext::ColumnMetadata(Arc::clone(self.current_metadata.as_ref().unwrap()));
        loop {
            let start = Instant::now();
            let result = self
                .transport
                .receive_row_into(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                    writer,
                )
                .await?;
            self.update_remaining_timeout(start);

            match result {
                RowReadResult::RowWritten => {
                    writer.end_row();
                    info!("Row Received");
                    return Ok(true);
                }
                RowReadResult::Token(token) => match token {
                    Tokens::DoneInProc(done) | Tokens::DoneProc(done) | Tokens::Done(done) => {
                        info!("done while get_next_row: {:?}", done);

                        if done.has_error() {
                            return Err(crate::error::Error::ProtocolError(
                                "Server reported error in DONE token without preceding ERROR token"
                                    .to_string(),
                            ));
                        }

                        let count = self.count_map.entry(done.cur_cmd).or_insert(0);
                        *count = count.saturating_add(done.row_count);

                        self.current_result_set_has_been_read_till_end = true;
                        if !done.has_more() {
                            info!("No more rows for current command: {:?}", done.cur_cmd);
                            self.execution_context.set_has_open_batch(false);
                        }
                        return Ok(false);
                    }
                    Tokens::Order(order_token) => {
                        info!(?order_token);
                        continue;
                    }
                    Tokens::EnvChange(env_change) => {
                        info!(?env_change);
                        self.execution_context
                            .capture_change_property(&env_change)?;
                        continue;
                    }
                    Tokens::ReturnValue(return_value_token) => {
                        let return_value = return_value_token.into();
                        self.return_values.push(return_value);
                        continue;
                    }
                    Tokens::Error(error_token) => {
                        info!(?error_token);
                        let mut all_errors = vec![SqlErrorInfo::from(&error_token)];
                        let drain_errors = self.drain_stream().await?;
                        all_errors.extend(drain_errors);
                        return Err(crate::error::Error::from_sql_errors(all_errors));
                    }
                    Tokens::ColMetadata(_) => {
                        return Err(crate::error::Error::UsageError(
                            "Unexpected ColMetadata token encountered while reading rows. \
                             This typically indicates the API was not used correctly - \
                             you may need to call move_to_next() to advance to the next result set."
                                .to_string(),
                        ));
                    }
                    _ => {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Unexpected token while finding the next row: {token:?}"
                        )));
                    }
                },
            }
        }
    }

    /// Returns a clone of all [`ReturnValue`]s collected during the current
    /// batch — output parameters and UDF return values.
    ///
    /// Values accumulate as the token stream is read; call this after the
    /// result set is fully consumed (e.g. after [`close_query()`](Self::close_query)
    /// or after [`move_to_next()`](Self::move_to_next) returns `false`).
    pub fn get_return_values(&self) -> Vec<ReturnValue> {
        self.return_values.clone()
    }

    /// Retrieves a snapshot of the output parameters (including return values)
    /// that have been retrieved from the result stream.
    ///
    /// Returns `None` if there are no output parameters, otherwise returns
    /// a reference to the collected return values.
    pub fn retrieve_output_params(&self) -> TdsResult<Option<&Vec<ReturnValue>>> {
        if self.return_values.is_empty() {
            Ok(None)
        } else {
            Ok(Some(&self.return_values))
        }
    }

    /// Drains all remaining result sets and resets the client for the next request.
    ///
    /// Any unread rows and result sets are consumed so the TDS stream is left in
    /// a clean state. Must be called (or the result sets fully iterated) before
    /// executing another query on the same connection.
    #[instrument(skip(self), level = "info")]
    pub async fn close_query(&mut self) -> TdsResult<()> {
        if !self.execution_context.has_open_batch() {
            return Ok(());
        }
        // call next row to consume any remaining tokens
        while self.move_to_next().await? {}
        info!("No more rows to consume.");

        // Reset the current metadata, return values, and timeout/cancel state.
        self.current_metadata = None;
        self.return_values.clear();
        self.remaining_request_timeout = None;
        self.cancel_handle = None;
        self.execution_context.set_has_open_batch(false);
        Ok(())
    }

    /// Close the underlying transport, ending the TDS session.
    #[instrument(skip(self), level = "info")]
    pub async fn close_connection(&mut self) -> TdsResult<()> {
        self.transport.close_transport().await?;
        Ok(())
    }

    /// Send an attention packet and wait for acknowledgment with a timeout.
    ///
    /// This method is used by bulk copy operations to implement timeout handling
    /// per the SqlClient behavior:
    /// 1. Send MT_ATTN (0x06) packet to cancel the current operation
    /// 2. Wait for DONE token with ATTN (0x0020) status flag
    /// 3. If no acknowledgment within timeout, return false
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for attention acknowledgment
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Attention acknowledged by server
    /// * `Ok(false)` - Attention sent but timeout expired waiting for ACK
    /// * `Err(_)` - Error sending attention or reading response
    #[instrument(skip(self), level = "info")]
    pub async fn send_attention_with_timeout(&mut self, timeout: Duration) -> TdsResult<bool> {
        self.transport.send_attention_with_timeout(timeout).await
    }

    /// Check if the connection has an active transaction.
    ///
    /// A transaction is considered active when a BEGIN TRANSACTION has been
    /// executed and no corresponding COMMIT or ROLLBACK has occurred.
    ///
    /// # Returns
    ///
    /// * `true` - if a transaction is active on this connection
    /// * `false` - if no transaction is active (autocommit mode)
    pub fn has_active_transaction(&self) -> bool {
        self.execution_context.has_active_transaction()
    }

    /// Begin a new transaction with the given isolation level and optional name.
    ///
    /// Fails if a batch is currently executing. Use [`has_active_transaction`](Self::has_active_transaction)
    /// to check whether a transaction is already open.
    #[instrument(skip(self), level = "info")]
    pub async fn begin_transaction(
        &mut self,
        isolation_level: TransactionIsolationLevel,
        name: Option<String>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(
                "Cannot begin transaction while another batch is executing.".to_string(),
            ));
        }
        let transaction_params = TransactionManagementType::Begin(CreateTxnParams {
            level: isolation_level,
            name,
        });
        let transaction =
            TransactionManagementRequest::new(transaction_params, &self.execution_context);
        let mut packet_writer =
            transaction.create_packet_writer(self.transport.as_writer(), None, None);
        transaction.serialize(&mut packet_writer).await?;

        self.consume_transaction_response().await?;

        Ok(())
    }

    /// Create a savepoint within the current transaction.
    ///
    /// The savepoint `name` can later be passed to
    /// [`rollback_transaction`](Self::rollback_transaction) to partially undo work.
    #[instrument(skip(self), level = "info")]
    pub async fn save_transaction(&mut self, name: String) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(
                "Cannot save transaction while another batch is executing.".to_string(),
            ));
        }
        let transaction = TransactionManagementRequest::new(
            TransactionManagementType::Save(name),
            &self.execution_context,
        );
        let mut packet_writer =
            transaction.create_packet_writer(self.transport.as_writer(), None, None);
        transaction.serialize(&mut packet_writer).await?;

        self.consume_transaction_response().await?;

        Ok(())
    }

    /// Commit the current transaction.
    ///
    /// If `create_txn_params` is provided, a new transaction begins immediately
    /// after the commit (atomic commit-and-begin).
    #[instrument(skip(self), level = "info")]
    pub async fn commit_transaction(
        &mut self,
        name: Option<String>,
        create_txn_params: Option<CreateTxnParams>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(
                "Cannot commit transaction while another batch is executing.".to_string(),
            ));
        }
        let transaction = TransactionManagementRequest::new(
            TransactionManagementType::Commit {
                name,
                create_txn_params,
            },
            &self.execution_context,
        );
        let mut packet_writer =
            transaction.create_packet_writer(self.transport.as_writer(), None, None);
        transaction.serialize(&mut packet_writer).await?;

        self.consume_transaction_response().await?;

        Ok(())
    }

    /// Roll back the current transaction, or roll back to a named savepoint.
    ///
    /// If `create_txn_params` is provided, a new transaction begins immediately
    /// after the rollback.
    #[instrument(skip(self), level = "info")]
    pub async fn rollback_transaction(
        &mut self,
        name: Option<String>,
        create_txn_params: Option<CreateTxnParams>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(
                "Cannot rollback transaction while another batch is executing.".to_string(),
            ));
        }
        let transaction = TransactionManagementRequest::new(
            TransactionManagementType::Rollback {
                name,
                create_txn_params,
            },
            &self.execution_context,
        );
        let mut packet_writer =
            transaction.create_packet_writer(self.transport.as_writer(), None, None);
        transaction.serialize(&mut packet_writer).await?;

        self.consume_transaction_response().await?;

        Ok(())
    }

    /// Retrieve the DTC (Distributed Transaction Coordinator) network address from the server.
    ///
    /// Returns a result set that can be iterated with the normal row-reading API.
    #[instrument(skip(self), level = "info")]
    pub async fn get_dtc_address(&mut self) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(
                "Cannot get DTC address while another batch is executing.".to_string(),
            ));
        }
        let transaction = TransactionManagementRequest::new(
            TransactionManagementType::GetDtcAddress,
            &self.execution_context,
        );
        let mut packet_writer =
            transaction.create_packet_writer(self.transport.as_writer(), None, None);
        transaction.serialize(&mut packet_writer).await?;

        // GetDtcAddress returns a result set, unlike other transaction commands
        // Set up execution state for result iteration (similar to execute())
        let metadata = self.move_to_column_metadata().await?;
        if metadata.is_none() {
            self.execution_context.set_has_open_batch(false);
        } else {
            self.current_metadata = metadata;
            self.execution_context.set_has_open_batch(true);
        }

        Ok(())
    }

    #[instrument(skip(self), level = "info")]
    pub(crate) async fn consume_transaction_response(&mut self) -> TdsResult<()> {
        let mut collected_errors: Vec<SqlErrorInfo> = Vec::new();
        loop {
            let start = Instant::now();
            let token = self
                .transport
                .receive_token(
                    &ParserContext::None(()),
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                )
                .await?;
            self.update_remaining_timeout(start);

            match token {
                Tokens::DoneInProc(done) | Tokens::DoneProc(done) | Tokens::Done(done) => {
                    info!("done while consume_transaction_response: {:?}", done);

                    if done.has_error() && collected_errors.is_empty() {
                        return Err(crate::error::Error::ProtocolError(
                            "Server reported error in DONE token without preceding ERROR token"
                                .to_string(),
                        ));
                    }

                    let count = self.count_map.entry(done.cur_cmd).or_insert(0);
                    // Use saturating_add to prevent integer overflow from malicious/corrupted TDS responses
                    *count = count.saturating_add(done.row_count);

                    if !done.has_more() {
                        info!("No more rows for current command: {:?}", done.cur_cmd);
                        if !collected_errors.is_empty() {
                            return Err(crate::error::Error::from_sql_errors(collected_errors));
                        }
                    }
                    break;
                }
                Tokens::Error(error_token) => {
                    info!(?error_token);
                    collected_errors.push(SqlErrorInfo::from(&error_token));
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    self.execution_context
                        .capture_change_property(&env_change)?;
                    continue;
                }
                _ => {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "Unexpected token while reading transaction request response: {token:?}"
                    )));
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ResultSet for TdsClient {
    fn get_metadata(&self) -> &Vec<ColumnMetadata> {
        // If no metadata is available, return an empty vector
        // This can happen if get_metadata is called before executing a query
        // or if the query didn't return any result sets
        self.current_metadata
            .as_ref()
            .map(|m| &m.columns)
            .unwrap_or(&self.empty_metadata)
    }

    #[instrument(skip(self), level = "info")]
    async fn next_row(&mut self) -> TdsResult<Option<Vec<ColumnValues>>> {
        if self.maybe_has_unread_rows() {
            self.get_next_row().await
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self, writer), level = "info")]
    async fn next_row_into(&mut self, writer: &mut (dyn RowWriter + Send)) -> TdsResult<bool> {
        if self.maybe_has_unread_rows() {
            self.get_next_row_into(writer).await
        } else {
            Ok(false)
        }
    }

    fn maybe_has_unread_rows(&self) -> bool {
        !self.current_result_set_has_been_read_till_end
    }

    #[instrument(skip(self), level = "info")]
    async fn close(&mut self) -> TdsResult<()> {
        self.close_query().await
    }
}

#[async_trait]
impl ResultSetClient for TdsClient {
    fn get_current_resultset(&mut self) -> Option<&mut TdsClient> {
        if self.execution_context.has_open_batch() {
            Some(self)
        } else {
            None
        }
    }

    #[instrument(skip(self), level = "info")]
    async fn move_to_next(&mut self) -> TdsResult<bool> {
        if !self.execution_context.has_open_batch() {
            return Ok(false);
        }
        // Drain the current result set.
        if self.maybe_has_unread_rows() {
            self.drain_rows().await?;
        }

        info!("Moving to next result set...");

        let has_open_batch = self.execution_context.has_open_batch();
        info!("Has open batch: {}", has_open_batch);
        if !has_open_batch {
            return Ok(false);
        }
        let metadata_token = self.move_to_column_metadata().await?;

        match metadata_token {
            Some(metadata) => {
                self.current_metadata = Some(metadata);
                self.execution_context.set_has_open_batch(true);
                self.current_result_set_has_been_read_till_end = false;
                Ok(true)
            }
            None => {
                // No metadata means no more result sets.
                self.execution_context.set_has_open_batch(false);
                self.current_metadata = None;
                self.current_result_set_has_been_read_till_end = true;
                Ok(false)
            }
        }
    }
}

/// Async result set iteration.
#[async_trait]
pub trait ResultSet {
    /// Returns the metadata of the result set.
    /// This metadata includes information about the columns in the result set.
    fn get_metadata(&self) -> &Vec<ColumnMetadata>;

    /// Returns the next row of data as a vector of column values.
    /// If there is no more data, it returns None.
    async fn next_row(&mut self) -> TdsResult<Option<Vec<ColumnValues>>>;

    /// Decodes the next row directly into a [`RowWriter`], returning `true` if
    /// a row was written or `false` when the result set is exhausted.
    async fn next_row_into(&mut self, writer: &mut (dyn RowWriter + Send)) -> TdsResult<bool>;

    /// Returns `true` if the result set may still contain unread rows.
    fn maybe_has_unread_rows(&self) -> bool;

    /// Iterates over the result set, and marks it as closed. After calling close, the next_row method,
    /// will always return None.
    async fn close(&mut self) -> TdsResult<()>;
}

/// Navigation across multiple result sets.
#[async_trait]
pub trait ResultSetClient<T = TdsClient> {
    /// Returns the current result set on the client.
    /// Execution of query positions the client at the first result set.
    /// If we have read all the results from the current result set,
    /// this method will return None.
    fn get_current_resultset(&mut self) -> Option<&mut T>;

    /// Moves to the next result set, if available.
    /// Returns true if there is a next result set, false otherwise.
    /// The current_resultset will be closed and if the next result set is available,
    /// it will be set as the current result set.
    /// If there is no next result set, the current result set will be closed and
    /// the method will return false.
    async fn move_to_next(&mut self) -> TdsResult<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_to_duration_none_yields_none() {
        assert_eq!(TdsClient::timeout_to_duration(None), None);
    }

    #[test]
    fn timeout_to_duration_zero_yields_none() {
        assert_eq!(TdsClient::timeout_to_duration(Some(0)), None);
    }

    #[test]
    fn timeout_to_duration_positive_yields_duration() {
        assert_eq!(
            TdsClient::timeout_to_duration(Some(30)),
            Some(Duration::from_secs(30))
        );
    }
}
