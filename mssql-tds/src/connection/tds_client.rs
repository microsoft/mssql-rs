// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::bulk_copy::{BulkCopyOptions, BulkLoadRow, ResolvedColumnMapping};
use crate::connection::bulk_copy_state::ATTENTION_TIMEOUT_SECONDS;
use crate::connection::client_context::{ClientContext, ExecutionColumnEncryptionSetting};
use crate::connection::session_recovery::RecoveryContext;
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use crate::datatypes::row_writer::{DefaultRowWriter, RowWriter};
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sqltypes::SqlType;
use crate::error::Error::UsageError;
use crate::error::{SqlErrorInfo, SqlInfoMessage};
use crate::io::packet_writer::PacketWriter;
use crate::message::bulk_load::{StreamingBulkLoadWriter, build_insert_bulk_command};
use crate::message::messages::{PacketType, ResetConnectionMode};
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
    token::tokens::{ColMetadataToken, CurrentCommand, DoneStatus, EnvChangeTokenSubType, Tokens},
};
use async_trait::async_trait;
use std::collections::HashMap;
use tracing::{debug, error, info, instrument};

use crate::{
    core::{CancelHandle, TdsResult},
    query::metadata::ColumnMetadata,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Prefix for the synthetic parameter names given to positional stored-procedure
/// parameters when building the `sp_describe_parameter_encryption` request.
/// Positional arguments have no caller-supplied name, so they are declared as
/// `@ce_pos_0`, `@ce_pos_1`, ... in the describe `EXEC`. These names exist only
/// in the describe request; the real RPC still sends the parameters unnamed.
const SYNTHETIC_POSITIONAL_PARAM_PREFIX: &str = "ce_pos_";

/// State of the `ReturnStatus` token observed while draining the most recent
/// cursor RPC response. Distinguishes "no token was sent" from an actual raw
/// status value, so neither case is silently collapsed at interpretation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connection) enum ReturnStatus {
    /// No `ReturnStatus` token was sent for the most recent RPC.
    NotReceived,
    /// The server sent a `ReturnStatus` token carrying this raw value.
    Received(i32),
}

/// Memoized cell decryptor paired with the column metadata it was built for, so
/// it can be rebuilt when the result set changes.
type MemoizedCellDecryptor = (
    Arc<ColMetadataToken>,
    Option<Arc<dyn crate::security::cell_decryptor::CellDecryptor>>,
);

/// Active TDS connection to a SQL Server instance.
///
/// Created by [`TdsConnectionProvider::create_client()`](crate::connection_provider::tds_connection_provider::TdsConnectionProvider::create_client).
/// Provides methods for executing queries, managing transactions, and bulk copy.
#[derive(Debug)]
pub struct TdsClient {
    pub(crate) transport: Box<dyn TdsTransport>,
    pub(crate) negotiated_settings: NegotiatedSettings,
    pub(crate) execution_context: ExecutionContext,
    pub(crate) recovery_context: Box<RecoveryContext>,

    // pub(crate) batch_result: Option<BatchResult<'static>>,
    pub(crate) current_metadata: Option<Arc<ColMetadataToken>>,
    /// Memoized cell decryptor for `current_metadata`'s CEK table, paired with
    /// the metadata it was built for so it is rebuilt when the result set
    /// changes. `None` until the first encrypted result set is seen.
    current_decryptor: Option<MemoizedCellDecryptor>,
    count_map: HashMap<CurrentCommand, u64>,

    pub(in crate::connection) return_values: Vec<ReturnValue>,
    info_messages: Vec<SqlInfoMessage>,
    /// Per-prepared-handle Always Encrypted parameter metadata, captured by
    /// `execute_sp_prepare` from `sp_describe_parameter_encryption` and reused by
    /// `execute_sp_execute` to encrypt parameter values without describing again.
    /// Holds an `Arc` to the same describe result stored in
    /// `query_metadata_cache`, pinning it for the prepared statement's lifetime
    /// even if the shared cache evicts it. Evicted by `execute_sp_unprepare`.
    prepared_param_encryption: HashMap<
        i32,
        std::sync::Arc<
            crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult,
        >,
    >,
    /// Connection-scoped cache of `sp_describe_parameter_encryption` results,
    /// keyed by (database, query text), so every parameterized execution of the
    /// same statement reuses the describe result instead of re-querying the
    /// server. Mirrors the SqlClient/JDBC query-metadata caches.
    query_metadata_cache: crate::security::query_metadata_cache::QueryMetadataCache,
    /// Number of `sp_describe_parameter_encryption` round-trips actually sent to
    /// the server (query-metadata cache misses). Exposed for observability and to
    /// let tests confirm the cache elides repeat describes.
    describe_round_trips: u64,
    /// Plaintext column encryption keys retained from the current command's
    /// `sp_describe_parameter_encryption` call, keyed by normalized parameter
    /// name (leading `@` stripped, ASCII-uppercased). An encrypted RETURNVALUE
    /// output parameter carries no CEK table and reuses the CEK that encrypted
    /// the matching input parameter, so these are consulted when decrypting
    /// output parameters. Cleared and repopulated by `encrypt_parameters` on
    /// every command.
    output_param_ceks: HashMap<String, Arc<zeroize::Zeroizing<Vec<u8>>>>,
    /// State of the most recent `ReturnStatus` token, captured while draining a
    /// cursor RPC response and interpreted as a [`CursorStatus`](crate::cursor::CursorStatus).
    pub(in crate::connection) last_return_status: ReturnStatus,
    pub(in crate::connection) current_result_set_has_been_read_till_end: bool,

    /// Column Encryption setting for the command currently executing. Set by
    /// each execute entry point; consulted by the parameter-encryption and
    /// result-decryption paths to honor per-command overrides.
    current_command_ce_setting: crate::connection::client_context::ExecutionColumnEncryptionSetting,

    /// Set while an `sp_prepexec` is in flight, cleared once its `@handle`
    /// output parameter (RETURNVALUE ordinal 0) has been captured.
    expecting_prepare_handle: bool,
    /// Prepared-statement handle from the most recent `sp_prepexec`, surfaced
    /// via [`take_prepared_statement_handle`](Self::take_prepared_statement_handle).
    /// Kept separately because [`close_query`](Self::close_query) clears
    /// `return_values` once the batch has been drained.
    prepared_statement_handle: Option<i32>,

    /// The remaining request timeout for operations. This is updated after each token read.
    pub(in crate::connection) remaining_request_timeout: Option<Duration>,

    /// The cancel handle for this client. Used to cancel operations.
    pub(in crate::connection) cancel_handle: Option<CancelHandle>,

    /// Empty metadata vector for returning when no metadata is available
    empty_metadata: Vec<ColumnMetadata>,
}

impl TdsClient {
    pub(crate) fn new(
        transport: Box<dyn TdsTransport>,
        negotiated_settings: NegotiatedSettings,
        execution_context: ExecutionContext,
        client_context: ClientContext,
    ) -> Self {
        let mut recovery_context = RecoveryContext::new();
        recovery_context.initialize(
            client_context,
            negotiated_settings.login_ack_tds_version,
            negotiated_settings.login_ack_server_version,
            negotiated_settings
                .session_settings
                .negotiated_encryption_settings,
            negotiated_settings.session_settings.mars_enabled,
        );

        Self {
            transport,
            negotiated_settings,
            execution_context,
            recovery_context: Box::new(recovery_context),
            current_metadata: None,
            current_decryptor: None,
            count_map: HashMap::new(),
            return_values: Vec::new(),
            info_messages: Vec::new(),
            prepared_param_encryption: HashMap::new(),
            query_metadata_cache: crate::security::query_metadata_cache::QueryMetadataCache::new(),
            describe_round_trips: 0,
            output_param_ceks: HashMap::new(),
            last_return_status: ReturnStatus::NotReceived,
            current_result_set_has_been_read_till_end: false,
            current_command_ce_setting:
                crate::connection::client_context::ExecutionColumnEncryptionSetting::default(),
            expecting_prepare_handle: false,
            prepared_statement_handle: None,
            remaining_request_timeout: None,
            cancel_handle: None,
            empty_metadata: Vec::new(),
        }
    }

    /// Attempt to reconnect a dead connection by replaying session state.
    ///
    /// The overall reconnection is bounded by `timeout`. Each individual
    /// TCP/TDS handshake attempt uses the original `connect_timeout`. Before
    /// each retry sleep, we verify enough time remains for the interval.
    #[instrument(skip(self), level = "info")]
    pub(crate) async fn reconnect(
        &mut self,
        timeout: Duration,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        use crate::connection_provider::tds_connection_provider::TdsConnectionProvider;
        use crate::error::Error;

        // Gate: session must be recoverable
        if !self
            .recovery_context
            .is_recovery_possible(&self.execution_context)
        {
            return Err(Error::SessionNotRecoverable(
                "Session state does not allow recovery".to_string(),
            ));
        }

        let client_context = match self.recovery_context.client_context.as_ref() {
            Some(ctx) => ctx.clone(),
            None => {
                return Err(Error::SessionNotRecoverable(
                    "No client context available for reconnection".to_string(),
                ));
            }
        };

        // Snapshot session state for the reconnection LOGIN7
        let snapshot = self.recovery_context.session_state_table.snapshot(
            Some(&self.negotiated_settings.database),
            Some(&self.negotiated_settings.language),
            Some(self.negotiated_settings.database_collation),
        );

        // Close the dead transport (best-effort)
        let _ = self.transport.close_transport().await;

        let deadline = Instant::now() + timeout;
        let connect_retry_count = client_context.connect_retry_count;
        let connect_retry_interval =
            Duration::from_secs(client_context.connect_retry_interval as u64);
        let transport_context = client_context.transport_context.clone();

        let mut last_error: Option<Error> = None;

        for attempt in 0..=connect_retry_count {
            // Wait before retry (not before first attempt)
            if attempt > 0 {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining < connect_retry_interval {
                    info!(
                        attempt,
                        "Not enough time for retry interval, aborting reconnection"
                    );
                    break;
                }
                // Cancellable sleep — if the caller cancels, abort immediately
                // rather than blocking until the interval expires (matches ODBC's
                // recoveryCancelledEvent.Wait() interruptible sleep).
                CancelHandle::run_until_cancelled(cancel_handle, async {
                    tokio::time::sleep(connect_retry_interval).await;
                    Ok(())
                })
                .await?;
            }

            // Check deadline
            if Instant::now() >= deadline {
                info!("Reconnection deadline exceeded");
                break;
            }

            // Inject recovery data into the client context clone
            let mut reconnect_ctx = client_context.clone();

            // Cap the per-attempt connect timeout to the remaining reconnect budget
            let remaining_secs =
                deadline.saturating_duration_since(Instant::now()).as_secs() as u32;
            reconnect_ctx.connect_timeout = reconnect_ctx.connect_timeout.min(remaining_secs);

            info!(attempt, "Attempting reconnection");
            let connect_result = CancelHandle::run_until_cancelled(
                cancel_handle,
                TdsConnectionProvider::connect_with_transport_context(
                    &reconnect_ctx,
                    &transport_context,
                    Some(Box::new((*snapshot).clone())),
                ),
            )
            .await;
            match connect_result {
                Ok((new_transport, new_settings, new_exec_ctx, info_messages)) => {
                    // Validate reconnection properties match original
                    if let Err(validation_err) =
                        self.recovery_context.validate_reconnection(&new_settings)
                    {
                        // Close the new transport — it's unusable
                        let mut transport = new_transport;
                        let _ = transport.close_transport().await;
                        info!(error = %validation_err, "Reconnection validation failed");
                        return Err(validation_err);
                    }

                    // Replace connection state
                    self.transport = new_transport;
                    self.negotiated_settings = new_settings;
                    self.execution_context = new_exec_ctx;

                    // Reset per-request state
                    self.current_metadata = None;
                    self.count_map.clear();
                    self.return_values.clear();
                    self.info_messages.clear();
                    self.info_messages.extend(info_messages);
                    // Prepared-statement handles do not survive a reconnect, so
                    // drop their cached Always Encrypted metadata to avoid
                    // encrypting a later sp_execute with a stale describe result.
                    self.prepared_param_encryption.clear();
                    self.expecting_prepare_handle = false;
                    self.prepared_statement_handle = None;
                    self.current_result_set_has_been_read_till_end = false;
                    self.remaining_request_timeout = None;
                    self.cancel_handle = None;

                    // Reset session state table for the new session
                    self.recovery_context.session_state_table.reset();

                    self.recovery_context.recovery_count += 1;
                    info!(
                        recovery_count = self.recovery_context.recovery_count,
                        "Reconnection successful"
                    );
                    return Ok(());
                }
                Err(e) => {
                    info!(attempt, error = %e, "Reconnection attempt failed");
                    last_error = Some(e);
                }
            }
        }

        // All attempts exhausted
        let message = last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "Deadline exceeded".to_string());
        Err(Error::SessionRecoveryFailed {
            attempts: connect_retry_count + 1,
            message,
        })
    }

    /// Returns the current database collation.
    ///
    /// If the collation changed after login (via an ENVCHANGE token), the
    /// updated value is returned; otherwise the collation negotiated at login.
    pub fn get_collation(&self) -> SqlCollation {
        self.negotiated_settings.database_collation
    }

    /// Returns the name of the database the connection is currently using.
    ///
    /// Reflects any change made after login (e.g. a `USE` statement, surfaced
    /// via an ENVCHANGE token); otherwise the database negotiated at login.
    ///
    /// Intended for connection-pool consumers that need to match a pooled
    /// connection to a request or decide whether a reset is required.
    pub fn database(&self) -> &str {
        &self.negotiated_settings.database
    }

    /// Returns the language the connection is currently using.
    ///
    /// Reflects any change made after login (e.g. `SET LANGUAGE`, surfaced via
    /// an ENVCHANGE token); otherwise the language negotiated at login.
    ///
    /// Intended for connection-pool consumers that need to match a pooled
    /// connection to a request or decide whether a reset is required.
    pub fn language(&self) -> &str {
        &self.negotiated_settings.language
    }

    /// Returns the negotiated TDS packet size, in bytes.
    ///
    /// The packet size is fixed for the lifetime of the connection (the server
    /// rejects mid-session packet-size changes), so this always reflects the
    /// value negotiated at login.
    pub fn packet_size(&self) -> u32 {
        self.negotiated_settings.session_settings.packet_size
    }

    /// Returns `true` if the connection is known to be dead.
    ///
    /// This surfaces the connection's last-known liveness status, updated
    /// whenever the connection is explicitly closed or an I/O operation observes
    /// it broken. It is a cached read: it never touches the socket, so it is
    /// always safe to call regardless of connection state and never consumes
    /// in-flight protocol data.
    ///
    /// A `true` result means the connection is definitively dead. A `false`
    /// result means it has not been observed dead — it may still have failed
    /// silently while idle. That case is handled transparently by idle
    /// connection resiliency, which detects and recovers a dead connection on
    /// the next operation. This makes the method suitable for connection pools
    /// that want a cheap, always-safe liveness check before handing out a
    /// connection.
    pub fn is_connection_dead(&self) -> bool {
        self.transport.connection_known_dead()
    }

    pub(crate) fn get_current_metadata(&self) -> Option<&ColMetadataToken> {
        self.current_metadata.as_deref()
    }

    /// Converts an `Option<u32>` timeout (where `Some(0)` means infinite) to `Option<Duration>`.
    ///
    /// The bulk copy API uses `0` to mean "no timeout" (infinite). This helper
    /// normalises that convention so `Some(0)` becomes `None` (no deadline).
    pub(in crate::connection) fn timeout_to_duration(timeout_sec: Option<u32>) -> Option<Duration> {
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

    /// Pre-execution check: detect a dead connection and attempt session recovery.
    ///
    /// Called at the top of every method that sends a TDS request (SQL batch,
    /// RPC, bulk load, `BEGIN TRANSACTION`). If session recovery was negotiated
    /// and the underlying TCP socket is dead, this will attempt `reconnect()`.
    ///
    /// Returns the time spent reconnecting so callers can deduct it from the
    /// command timeout. When no reconnection is needed, returns `Duration::ZERO`.
    ///
    /// The command timeout (`timeout_sec`) is used as the overall budget for
    /// recovery + execution, matching ODBC's `CheckOrRecoverConnection` which
    /// deducts recovery time from the remaining command timeout via
    /// `timer.GetTimeoutLeft()`. This ensures applications can set reliable
    /// SLAs — a 30-second command timeout means at most 30 seconds total,
    /// regardless of whether a reconnect occurred.
    ///
    /// Methods that operate within an active transaction (`COMMIT`, `ROLLBACK`,
    /// `SAVE`) intentionally skip this — `is_recovery_possible()` returns
    /// `false` when a transaction is active, matching SqlClient's
    /// `RestoreBrokenConnection` flag behavior.
    pub(in crate::connection) async fn check_and_reconnect(
        &mut self,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Duration> {
        // Only attempt recovery when session recovery was negotiated and
        // the server supports retry (connect_retry_count > 0).
        if !self.recovery_context.session_recovery_negotiated {
            return Ok(Duration::ZERO);
        }
        let connect_retry_count = self
            .recovery_context
            .client_context
            .as_ref()
            .map_or(0, |ctx| ctx.connect_retry_count);
        if connect_retry_count == 0 {
            return Ok(Duration::ZERO);
        }

        // Non-blocking poll — returns immediately.
        if !self.transport.is_connection_dead() {
            return Ok(Duration::ZERO);
        }

        // Connection is dead. Check if recovery is possible.
        if !self
            .recovery_context
            .is_recovery_possible(&self.execution_context)
        {
            return Err(crate::error::Error::ConnectionClosed(
                "Connection is dead and session state does not allow recovery".to_string(),
            ));
        }

        // Use the command timeout as the reconnection budget. If no command
        // timeout is set, fall back to connect_timeout so reconnection is
        // still bounded.
        let reconnect_timeout = match timeout_sec {
            Some(t) if t > 0 => Duration::from_secs(t as u64),
            _ => {
                let connect_timeout = self
                    .recovery_context
                    .client_context
                    .as_ref()
                    .map_or(15, |ctx| ctx.connect_timeout);
                Duration::from_secs(connect_timeout as u64)
            }
        };

        let start = Instant::now();
        self.reconnect(reconnect_timeout, cancel_handle).await?;
        Ok(start.elapsed())
    }

    /// Subtracts `elapsed` from `timeout_sec`, returning the remaining seconds.
    /// Returns `Some(0)` (immediate timeout) if recovery consumed the entire budget.
    /// Passes through `None` (no timeout) unchanged.
    /// Rounds up to avoid exceeding the caller's timeout budget on sub-second elapsed times.
    pub(in crate::connection) fn deduct_timeout(
        timeout_sec: Option<u32>,
        elapsed: Duration,
    ) -> Option<u32> {
        timeout_sec.map(|t| {
            let elapsed_secs = u32::try_from(
                elapsed
                    .as_secs()
                    .saturating_add(if elapsed.subsec_nanos() > 0 { 1 } else { 0 }),
            )
            .unwrap_or(u32::MAX);
            t.saturating_sub(elapsed_secs)
        })
    }

    /// Requests that the connection be reset before the next request is
    /// processed by the server, to support connection pooling.
    ///
    /// This sets the RESETCONNECTION (or RESETCONNECTIONSKIPTRAN) status bit on
    /// the first packet of the next SQL Batch, RPC, or Transaction Manager
    /// request sent on this connection (MS-TDS section 2.2.3.1.2). The server
    /// resets the session state back to its login defaults — equivalent to
    /// `sp_reset_connection` — before processing that request. The request is
    /// one-shot: it is cleared once the next such request has been sent.
    ///
    /// # Parameters
    /// - `preserve_transaction` — when `true`, the reset preserves the current
    ///   transaction state (a local or enlisted/distributed transaction survives
    ///   the reset) by using RESETCONNECTIONSKIPTRAN instead of RESETCONNECTION.
    ///   Callers (typically a connection pool) should pass `true` only when the
    ///   pooled connection is enlisted in a transaction that must outlive the
    ///   reset.
    pub fn prepare_reset_connection(&mut self, preserve_transaction: bool) {
        let mode = match preserve_transaction {
            true => ResetConnectionMode::ResetSkipTran,
            false => ResetConnectionMode::Reset,
        };
        self.transport.as_writer().set_reset_mode(mode);
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
        self.send_query_batch(sql_command, timeout_sec, cancel_handle)
            .await?;

        let metadata = self.move_to_column_metadata().await?;
        // No metadata means no rows were returned, so we set has_open_batch to false.
        if metadata.is_none() {
            self.execution_context.set_has_open_batch(false);
            self.current_metadata = None;
        } else {
            self.current_metadata = metadata;

            self.execution_context.set_has_open_batch(true);
        }
        Ok(())
    }

    /// Runs the batch-execution prologue and sends a SQL batch to the wire:
    /// sets the batch-level Always Encrypted setting, rejects a re-entrant call,
    /// resets the per-command info buffer, reconnects if needed, stores the
    /// timeout / cancel handle, and serializes the batch. Shared by
    /// [`execute()`](Self::execute) and
    /// [`execute_multi_statement()`](Self::execute_multi_statement); the caller
    /// then consumes the response with the navigation model it wants.
    async fn send_query_batch(
        &mut self,
        sql_command: String,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        // Batch execution always uses the connection's Always Encrypted setting.
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;

        if self.execution_context.has_open_batch() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();
        let batch = SqlBatch::new(sql_command, &self.execution_context);
        let mut packet_writer =
            batch.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        batch.serialize(&mut packet_writer).await?;
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
        self.execute_sp_executesql_core(
            sql,
            named_params,
            ExecutionColumnEncryptionSetting::UseConnectionSetting,
            timeout_sec,
            cancel_handle,
        )
        .await
    }

    /// Executes a parameterized statement via `sp_executesql` with a per-command
    /// [`ExecutionColumnEncryptionSetting`] that overrides the connection's
    /// Always Encrypted behavior for this execution only.
    ///
    /// See [`execute_sp_executesql`](Self::execute_sp_executesql) for the common
    /// path that inherits the connection setting.
    #[instrument(skip(self, named_params), level = "info")]
    pub async fn execute_sp_executesql_with_encryption_setting(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        encryption_setting: ExecutionColumnEncryptionSetting,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        self.execute_sp_executesql_core(
            sql,
            named_params,
            encryption_setting,
            timeout_sec,
            cancel_handle,
        )
        .await
    }

    async fn execute_sp_executesql_core(
        &mut self,
        sql: String,
        mut named_params: Vec<RpcParameter>,
        encryption_setting: ExecutionColumnEncryptionSetting,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        self.current_command_ce_setting = encryption_setting;

        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

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

        build_parameter_list_string(&named_params, &mut params_list_as_string)?;

        // Always Encrypted: when the connection enabled column encryption and the
        // server acknowledged the feature, ask the server which parameters need
        // encryption and encrypt them in place before sending the real RPC.
        self.ensure_force_column_encryption_supported(named_params.iter())?;
        if self.should_encrypt_parameters() && !named_params.is_empty() {
            self.encrypt_parameters(
                &sql,
                &params_list_as_string,
                &mut named_params,
                timeout_sec,
                cancel_handle,
            )
            .await?;
            // The describe round-trip closes its own batch, which clears the
            // per-operation timeout/cancel state; restore it for the real RPC.
            self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        }

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
            self.current_metadata = None;
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

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

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

        // Always Encrypted: when column encryption is negotiated and enabled,
        // resolve the plaintext CEK for each encrypted destination column so the
        // writer can encrypt row values and emit the encrypted COLMETADATA.
        //
        // With `allow_encrypted_value_modifications`, the caller supplies
        // ciphertext directly, so we skip CEK resolution entirely and let the
        // writer pass those values through verbatim.
        let has_encrypted_columns = mapped_column_metadata.iter().any(|c| c.is_encrypted);
        let encrypt_bulk_copy = self.should_encrypt_bulk_copy() && has_encrypted_columns;
        let passthrough_ciphertext =
            encrypt_bulk_copy && options.allow_encrypted_value_modifications;

        let plaintext_ceks: Vec<Option<std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>>> =
            if encrypt_bulk_copy && !passthrough_ciphertext {
                use crate::security::keystore::decrypt_cek;

                let (providers, cek_cache, trusted_key_paths) = {
                    let client_context =
                        self.recovery_context
                            .client_context
                            .as_ref()
                            .ok_or_else(|| {
                                crate::error::Error::ColumnEncryptionError(
                                    "Cannot encrypt bulk copy values without a client context"
                                        .to_string(),
                                )
                            })?;
                    (
                        client_context.column_encryption_key_store_providers.clone(),
                        client_context.cek_cache.clone(),
                        client_context
                            .trusted_key_paths_for_current_server()
                            .to_vec(),
                    )
                };

                let mut ceks = Vec::with_capacity(mapped_column_metadata.len());
                for col in &mapped_column_metadata {
                    match &col.encryption {
                        Some(enc) => {
                            let cek = decrypt_cek(
                                &providers,
                                &cek_cache,
                                &enc.cek_entry,
                                &trusted_key_paths,
                            )
                            .await?;
                            ceks.push(Some(cek));
                        }
                        None => ceks.push(None),
                    }
                }
                ceks
            } else {
                Vec::new()
            };

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

        // Enable Always Encrypted serialization before writing metadata so the
        // COLMETADATA carries the CEK table and per-column crypto metadata. Under
        // ciphertext passthrough the metadata is still emitted, but values are
        // sent verbatim rather than encrypted, so no plaintext CEKs are attached.
        if passthrough_ciphertext {
            writer.set_column_encryption_enabled(true);
            writer.set_allow_encrypted_value_modifications(true);
        } else if !plaintext_ceks.is_empty() {
            writer.set_column_encryption_enabled(true);
            writer.set_plaintext_ceks(plaintext_ceks);
        }

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
            // Send attention packet to cancel the bulk load operation gracefully.
            // This tells SQL Server to abort the current operation and resets the
            // TDS protocol state so the connection can be reused.
            // The stream is always in a clean state here because writes are never
            // dropped mid-flight (issue #513).
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
                    self.capture_info_message(&info_token);
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                        self.recovery_context.session_state_table.reset();
                    }
                    self.execution_context
                        .capture_change_property(&env_change, &mut self.negotiated_settings)?;
                    continue;
                }
                Tokens::SessionState(session_state) => {
                    self.recovery_context
                        .process_session_state(&session_state)?;
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
        // Stored-procedure execution uses the connection's Always Encrypted
        // setting; there is no per-command override on this path.
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;

        let mut positional_parameters = positional_parameters;
        let mut named_parameters = named_parameters;

        if self.execution_context.has_open_batch() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        // Always Encrypted: when the connection enabled column encryption and the
        // server acknowledged the feature, ask the server which parameters need
        // encryption (via `sp_describe_parameter_encryption` against an `EXEC`
        // form of the call) and encrypt them in place before sending the real
        // stored-procedure RPC. Positional parameters are described under
        // synthetic names bound by position; named parameters bind by name.
        self.ensure_force_column_encryption_supported(
            positional_parameters
                .iter()
                .flatten()
                .chain(named_parameters.iter().flatten()),
        )?;
        let has_positional = positional_parameters
            .as_ref()
            .is_some_and(|p| !p.is_empty());
        let has_named = named_parameters
            .as_ref()
            .is_some_and(|p| p.iter().any(|param| param.name.is_some()));
        if self.should_encrypt_parameters() && (has_positional || has_named) {
            let (tsql, params_decl) = Self::build_stored_procedure_describe_request(
                &stored_procedure_name,
                positional_parameters.as_deref().unwrap_or(&[]),
                named_parameters.as_deref().unwrap_or(&[]),
            )?;

            // Assemble one slice of mutable references in declaration order
            // (positional first, then named) so the describe result maps back by
            // ordinal (positional) or name (named).
            let mut combined: Vec<&mut RpcParameter> = Vec::new();
            if let Some(positional) = positional_parameters.as_mut() {
                combined.extend(positional.iter_mut());
            }
            if let Some(named) = named_parameters.as_mut() {
                combined.extend(named.iter_mut());
            }

            self.encrypt_combined_parameters(
                &tsql,
                &params_decl,
                &mut combined,
                timeout_sec,
                cancel_handle,
            )
            .await?;

            // The describe round-trip closes its own batch, which clears
            // the per-operation timeout/cancel state; restore it for the
            // real RPC.
            self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
            self.transport.reset_reader();
        }

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
            self.current_metadata = None;
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
    ///
    /// # Arguments
    ///
    /// * `sql` — the parameterized T-SQL statement to prepare. Parameter
    ///   placeholders (e.g. `@p1`, `@db_name`) referenced here must be
    ///   declared in `named_params`.
    /// * `named_params` — declarations of the statement's parameters. Only
    ///   the `name` and SQL `type` of each entry are used to build the
    ///   `@params` declaration string passed to `sp_prepare`; any values
    ///   carried by the entries are ignored. Supply the actual parameter
    ///   values later on the matching
    ///   [`execute_sp_execute()`](Self::execute_sp_execute) call.
    /// * `timeout_sec` — optional per-request timeout in seconds. `None`
    ///   means no timeout beyond the connection default.
    /// * `cancel_handle` — optional handle to cooperatively cancel the
    ///   request.
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

        // Prepared-statement execution uses the connection's Always Encrypted
        // setting; there is no per-command override on this path.
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql.clone())));

        // Create the parameter list for sp_prepare
        let execute_sql_statement_parameter =
            RpcParameter::new(None, StatusFlags::NONE, sql_statement_value);

        // Build the comma separated list of parameters
        let mut params_list_as_string = String::new();

        build_parameter_list_string(&named_params, &mut params_list_as_string)?;

        // Always Encrypted: describe the statement's parameters now (serving from
        // or populating the query-metadata cache) and pin the result under the
        // prepared handle so a later sp_execute can encrypt values without
        // describing again. sp_prepare itself sends no user parameter values, so
        // nothing is encrypted here.
        let describe_for_cache = if self.should_encrypt_parameters() && !named_params.is_empty() {
            let has_output = named_params.iter().any(|p| p.is_output());
            let describe = self
                .describe_parameters_cached(
                    &sql,
                    &params_list_as_string,
                    has_output,
                    timeout_sec,
                    cancel_handle,
                )
                .await?;
            // A describe round-trip (on a cache miss) closes its own batch and
            // clears the per-operation timeout/cancel state; restore it for the
            // prepare RPC.
            self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
            self.transport.reset_reader();
            Some(describe)
        } else {
            None
        };

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
        // sp_prepare's RPC contract is fixed: @handle (output int), @params (ntext),
        // @stmt (ntext), @options (int, optional). It does not accept any user
        // parameter values; those are sent later on sp_execute (or together on
        // sp_prepexec / sp_executesql). Forwarding `named_params` here causes the
        // server to surface "Procedure expects parameter '@options' of type 'int'."
        // for any non-int user parameter type.
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::Prepare),
            positional_parameters,
            None,
            &database_collation,
            &self.execution_context,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        // Drain to completion to get output parameters and any server errors.
        let server_errors = self.drain_stream().await?;

        // We need to get the return value, and then extract the handle from it.
        // If the server reported errors during prepare, surface them instead of a
        // generic ProtocolError so callers can see the underlying SQL Server
        // diagnostic.
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        if self.return_values.len() == 1 {
            let returned_parameter = self.return_values.first().unwrap();
            if let ColumnValues::Int(handle) = &returned_parameter.value {
                let handle = *handle;
                // Cache the parameter-encryption metadata (if any) under the
                // handle for reuse by execute_sp_execute.
                if let Some(describe) = describe_for_cache {
                    self.prepared_param_encryption.insert(handle, describe);
                }
                Ok(handle)
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

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

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

        // Drain the result set. A successful unprepare returns no results,
        // but surface any server errors collected during the drain instead of
        // silently discarding them.
        let server_errors = self.drain_stream().await?;
        if !server_errors.is_empty() {
            return Err(crate::error::Error::from_sql_errors(server_errors));
        }
        // The handle is now released on the server; drop its cached Always
        // Encrypted metadata. Done only after a successful unprepare so a failed
        // RPC (handle possibly still valid) does not strip metadata a later
        // sp_execute would need.
        self.prepared_param_encryption.remove(&handle);
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
    /// `drop_handle` piggybacks a prepared-handle release onto this call: when
    /// `Some(h)`, `h` is sent as the input value of the by-reference `@handle`
    /// parameter, so the server drops that prior prepared statement before
    /// preparing the new one - replacing a separate `sp_unprepare` round trip.
    /// `None` prepares fresh (the `@handle` input is NULL). Either way the new
    /// handle is returned in the `@handle` RETURNVALUE (ordinal 0). This mirrors
    /// the reference ODBC/`SqlClient` drivers, which send the retained handle as
    /// the `sp_prepexec` in/out `@handle` argument.
    ///
    /// Result rows are available through [`read_row()`](Self::read_row) after
    /// this call returns.
    #[instrument(skip(self, named_params), level = "info")]
    pub async fn execute_sp_prepexec(
        &mut self,
        sql: String,
        mut named_params: Vec<RpcParameter>,
        drop_handle: Option<i32>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Prepared-statement execution uses the connection's Always Encrypted
        // setting; there is no per-command override on this path.
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql.clone())));

        // Reset any prepared handle from a prior operation. The capture flag is
        // armed just before the send below so a failure while building the RPC
        // cannot leave it set.
        self.prepared_statement_handle = None;

        // Create the parameter list for sp_prepexec
        let statement_parameter = RpcParameter::new(None, StatusFlags::NONE, sql_statement_value);

        // Build the comma separated list of parameters
        let mut params_list_as_string = String::new();

        build_parameter_list_string(&named_params, &mut params_list_as_string)?;

        // Always Encrypted: sp_prepexec prepares and executes in one round-trip,
        // so — like sp_executesql — it runs `sp_describe_parameter_encryption`
        // against the statement and encrypts flagged parameters in place before
        // sending the real RPC. The `@params` declaration keeps each parameter's
        // original type; only the value is replaced with ciphertext plus cipher
        // metadata.
        self.ensure_force_column_encryption_supported(named_params.iter())?;
        if self.should_encrypt_parameters() && !named_params.is_empty() {
            self.encrypt_parameters(
                &sql,
                &params_list_as_string,
                &mut named_params,
                timeout_sec,
                cancel_handle,
            )
            .await?;
            // The describe round-trip closes its own batch and clears the
            // per-operation timeout/cancel state; restore it for the real RPC.
            self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
            self.transport.reset_reader();
        }

        let params_as_sql_string =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_list_as_string)));

        let params_parameter = RpcParameter::new(None, StatusFlags::NONE, params_as_sql_string);

        // The by-reference `@handle`: NULL input prepares fresh; a `Some(h)`
        // input tells the server to drop prepared statement `h` before
        // preparing. The new handle comes back as the `@handle` RETURNVALUE captured during drain.
        let handle_value = SqlType::Int(drop_handle);

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

        // Clear the flag on any send/read failure so a leaked
        // `true` cannot miscapture the first Int RETURNVALUE of a later
        // operation as a prepared handle.
        self.expecting_prepare_handle = true;

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        let serialize_result = rpc.serialize(&mut packet_writer).await;
        drop(packet_writer);
        if let Err(e) = serialize_result {
            self.expecting_prepare_handle = false;
            return Err(e);
        }

        let metadata = match self.move_to_column_metadata().await {
            Ok(metadata) => metadata,
            Err(e) => {
                self.expecting_prepare_handle = false;
                return Err(e);
            }
        };
        // No metadata means no rows were returned, so we set has_open_batch to false.
        if metadata.is_none() {
            self.execution_context.set_has_open_batch(false);
            self.current_result_set_has_been_read_till_end = true;
            self.current_metadata = None;
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
        mut positional_parameters: Option<Vec<RpcParameter>>,
        mut named_parameters: Option<Vec<RpcParameter>>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        if self.execution_context.has_open_batch() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        // Prepared-statement execution uses the connection's Always Encrypted
        // setting; there is no per-command override on this path.
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let timeout_sec = Self::deduct_timeout(timeout_sec, reconnect_elapsed);

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = Self::timeout_to_duration(timeout_sec);
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.return_values.clear();
        self.transport.reset_reader();

        // Always Encrypted: encrypt the supplied parameter values in place using
        // the metadata captured when the statement was prepared, then send the
        // real RPC. sp_execute never describes — the metadata must already have
        // been cached by execute_sp_prepare on this connection.
        self.ensure_force_column_encryption_supported(
            positional_parameters
                .iter()
                .flatten()
                .chain(named_parameters.iter().flatten()),
        )?;
        if self.should_encrypt_parameters()
            && (positional_parameters
                .as_ref()
                .is_some_and(|p| !p.is_empty())
                || named_parameters.as_ref().is_some_and(|p| !p.is_empty()))
        {
            let (providers, cek_cache, trusted_key_paths) = self.cloned_ce_key_material()?;
            let describe = self
                .prepared_param_encryption
                .get(&handle)
                .cloned()
                .ok_or_else(|| {
                    crate::error::Error::ColumnEncryptionError(format!(
                        "Prepared statement handle {handle} has no Always Encrypted parameter \
                         metadata; prepare it with execute_sp_prepare on this connection before \
                         executing with parameters"
                    ))
                })?;
            // Encrypt positional and named parameters together in one pass so a
            // describe entry that lives in the other list is not misreported as
            // "not supplied".
            let mut param_refs: Vec<&mut RpcParameter> = Vec::new();
            if let Some(params) = positional_parameters.as_mut() {
                param_refs.extend(params.iter_mut());
            }
            if let Some(params) = named_parameters.as_mut() {
                param_refs.extend(params.iter_mut());
            }
            Self::apply_parameter_encryption(
                &describe,
                &providers,
                &cek_cache,
                &mut param_refs,
                &mut self.output_param_ceks,
                &trusted_key_paths,
            )
            .await?;
        }

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
            self.current_metadata = None;
        } else {
            self.current_metadata = metadata;
            self.current_result_set_has_been_read_till_end = false;
            self.execution_context.set_has_open_batch(true);
        }
        Ok(())
    }

    /// Collects a return value, capturing the `sp_prepexec` `@handle`
    /// (RETURNVALUE ordinal 0) the first time one arrives while a prepare is in
    /// flight. Every `Tokens::ReturnValue` is funnelled through here so capture
    /// works regardless of which drain path reads the stream.
    fn push_return_value(&mut self, return_value: ReturnValue) {
        if self.expecting_prepare_handle
            && return_value.param_ordinal == 0
            && let ColumnValues::Int(handle) = &return_value.value
        {
            self.prepared_statement_handle = Some(*handle);
            self.expecting_prepare_handle = false;
            return;
        }
        self.return_values.push(return_value);
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
    pub(in crate::connection) async fn drain_stream(&mut self) -> TdsResult<Vec<SqlErrorInfo>> {
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
                Tokens::Info(info_token) => {
                    info!(?info_token, "Draining INFO token from stream");
                    self.capture_info_message(&info_token);
                }
                Tokens::EnvChange(t1) => {
                    if t1.sub_type == EnvChangeTokenSubType::ResetConnection {
                        self.recovery_context.session_state_table.reset();
                    }
                    self.execution_context
                        .capture_change_property(&t1, &mut self.negotiated_settings)?;
                }
                Tokens::SessionState(session_state) => {
                    self.recovery_context
                        .process_session_state(&session_state)?;
                }
                Tokens::ReturnValue(return_value_token) => {
                    let return_value = self.finalize_return_value(return_value_token)?;
                    self.push_return_value(return_value);
                }
                Tokens::ReturnStatus(return_status) => {
                    self.last_return_status = ReturnStatus::Received(return_status.value);
                    info!(?return_status);
                }
                _ => {
                    info!(?token);
                }
            }
        }
        Ok(collected_errors)
    }

    /// Reads tokens up to the next result boundary in the response stream.
    ///
    /// With `expose_norow_statements = false` (result-set navigation used by
    /// batch execution and the JS/Python consumers), a no-row statement's DONE
    /// token carrying the MORE flag is skipped so the method advances to the
    /// next COLMETADATA — consecutive no-row statements collapse into the
    /// following row-returning result set.
    ///
    /// With `true` (ODBC statement-wise navigation, matching msodbcsql), each
    /// statement's DONE token is its own result boundary: a no-row statement is
    /// returned as [`ResultBoundaryKind::NoRows`] instead of being skipped, so
    /// every statement in a batch is individually navigable. A DONE reached in
    /// this method (without a COLMETADATA earlier in the same call) always
    /// belongs to a no-row statement, because a row-returning statement's DONE
    /// is consumed while its rows are read/drained.
    async fn advance_to_result_boundary(
        &mut self,
        expose_norow_statements: bool,
    ) -> TdsResult<ResultBoundaryKind> {
        // Tell the COLMETADATA parser whether Always Encrypted was negotiated so
        // it can parse the CEK table and per-column crypto metadata.
        let parser_context = ParserContext::ColumnEncryption(
            self.negotiated_settings.is_column_encryption_supported(),
        );
        let mut loop_count = 0u32;
        // Whether the statement whose DONE we are about to reach produced any
        // informational message. In statement-wise navigation, msodbcsql exposes
        // a statement as its own result when it returns rows, carries a row count
        // (DONE COUNT flag), or produced messages; pure DDL / no-op statements
        // with none of these are collapsed. Tracks messages since the last
        // boundary so a PRINT / low-severity RAISERROR is surfaced individually.
        let mut saw_message = false;

        loop {
            loop_count += 1;

            // Warn when approaching iteration limit to help diagnose issues
            if loop_count.is_multiple_of(1000) {
                debug!(
                    loop_count,
                    "High iteration count in advance_to_result_boundary"
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
                    self.current_result_set_has_been_read_till_end = false;
                    return Ok(ResultBoundaryKind::RowSet(Arc::new(md)));
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

                    let is_last = !done.has_more();

                    if expose_norow_statements {
                        // Statement-wise navigation (msodbcsql parity): this DONE
                        // is a navigable result only if the statement returned a
                        // row count (COUNT flag) or produced messages. Pure DDL /
                        // no-op statements (no count, no messages) are collapsed,
                        // exactly like result-set navigation, so a batch such as
                        // `CREATE; INSERT; SELECT` exposes the INSERT's row count
                        // and the SELECT, not the bare CREATE.
                        let has_count = done.status.contains(DoneStatus::COUNT);
                        if has_count || saw_message {
                            self.execution_context.set_has_open_batch(!is_last);
                            return Ok(ResultBoundaryKind::NoRows {
                                rows_affected: if has_count { done.row_count } else { 0 },
                            });
                        }
                        // Collapsed no-op statement: fall through to the shared
                        // skip / end-of-batch handling below.
                    }

                    if is_last {
                        // No more result sets - end of batch.
                        info!("No more result sets (has_more=false), ending batch");
                        self.execution_context.set_has_open_batch(false);
                        return Ok(ResultBoundaryKind::End);
                    }

                    // has_more() is true - there are more result sets coming.
                    // For no-row statements (PRINT / RAISERROR / DDL / DML) there
                    // is no ColMetadata; in result-set navigation (and for
                    // collapsed no-op statements above) we skip over their DONE
                    // token to find the next result set with ColMetadata (SELECT).
                    info!(
                        "More result sets available (has_more=true), continuing to look for ColMetadata"
                    );

                    // Prevent infinite loops from malicious inputs sending endless Done tokens with has_more=true
                    if loop_count > 10000 {
                        error!(
                            loop_count,
                            "Excessive iterations in advance_to_result_boundary - possible malicious input or protocol violation"
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
                    if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                        self.recovery_context.session_state_table.reset();
                    }
                    self.execution_context
                        .capture_change_property(&env_change, &mut self.negotiated_settings)?;
                }
                Tokens::SessionState(session_state) => {
                    self.recovery_context
                        .process_session_state(&session_state)?;
                }
                Tokens::ReturnValue(return_value_token) => {
                    let return_value = self.finalize_return_value(return_value_token)?;
                    self.push_return_value(return_value);
                }
                Tokens::ReturnStatus(return_status) => {
                    self.last_return_status = ReturnStatus::Received(return_status.value);
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
                    self.capture_info_message(&info_token);
                    // Marks the current statement as message-bearing so
                    // statement-wise navigation surfaces it as its own result.
                    saw_message = true;
                    continue;
                }
                Tokens::TabName | Tokens::ColInfo => {
                    continue;
                }
                _ => {
                    info!("advance_to_result_boundary: {:?}", token);
                    return Err(UsageError(format!(
                        "Unexpected token while moving to next result boundary: {token:?}"
                    )));
                }
            }
        }
    }

    #[instrument(skip(self), level = "debug", name = "move_to_column_metadata")]
    pub(crate) async fn move_to_column_metadata(
        &mut self,
    ) -> TdsResult<Option<Arc<ColMetadataToken>>> {
        match self.advance_to_result_boundary(false).await? {
            ResultBoundaryKind::RowSet(md) => Ok(Some(md)),
            ResultBoundaryKind::End => Ok(None),
            // `expose_norow_statements = false` never yields a NoRows boundary.
            ResultBoundaryKind::NoRows { .. } => Ok(None),
        }
    }

    /// Applies a [`ResultBoundaryKind`] to the client's current-result state and
    /// maps it to the public [`StatementResult`] returned by the statement-wise
    /// navigation entry points ([`execute_multi_statement`](Self::execute_multi_statement),
    /// [`move_to_next_statement`](Self::move_to_next_statement)).
    fn apply_result_boundary(&mut self, boundary: ResultBoundaryKind) -> StatementResult {
        match boundary {
            ResultBoundaryKind::RowSet(md) => {
                self.current_metadata = Some(md);
                self.execution_context.set_has_open_batch(true);
                self.current_result_set_has_been_read_till_end = false;
                StatementResult::RowSet
            }
            ResultBoundaryKind::NoRows { rows_affected } => {
                // A no-row statement has zero columns; `has_open_batch` was set
                // by `advance_to_result_boundary` based on the DONE MORE flag.
                self.current_metadata = None;
                StatementResult::NoRows { rows_affected }
            }
            ResultBoundaryKind::End => {
                self.current_metadata = None;
                self.execution_context.set_has_open_batch(false);
                self.current_result_set_has_been_read_till_end = true;
                StatementResult::End
            }
        }
    }

    /// Executes a SQL batch and positions on its **first statement** using
    /// statement-wise navigation, returning that statement's [`StatementResult`].
    ///
    /// Unlike [`execute()`](Self::execute) — which skips leading no-row
    /// statements to position on the first row-returning result set — this
    /// exposes every statement (including PRINT / RAISERROR / DDL / DML) as its
    /// own navigable result, matching msodbcsql's `SQLExecDirect` +
    /// `SQLMoreResults` semantics. Advance through the remaining statements with
    /// [`move_to_next_statement()`](Self::move_to_next_statement).
    #[instrument(skip(self), level = "info")]
    pub async fn execute_multi_statement(
        &mut self,
        sql_command: String,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<StatementResult> {
        self.send_query_batch(sql_command, timeout_sec, cancel_handle)
            .await?;
        let boundary = self.advance_to_result_boundary(true).await?;
        Ok(self.apply_result_boundary(boundary))
    }

    /// Advances to the next statement's result using statement-wise navigation.
    ///
    /// Companion to [`execute_multi_statement()`](Self::execute_multi_statement):
    /// returns the next statement as a [`StatementResult`] (a row set, a no-row
    /// statement, or end of batch), draining any unread rows of the current
    /// result set first. This is the msodbcsql-aligned counterpart to
    /// [`move_to_next()`](Self::move_to_next), which instead collapses no-row
    /// statements.
    #[instrument(skip(self), level = "info")]
    pub async fn move_to_next_statement(&mut self) -> TdsResult<StatementResult> {
        if !self.execution_context.has_open_batch() {
            return Ok(StatementResult::End);
        }
        if self.maybe_has_unread_rows() {
            self.drain_rows().await?;
        }
        let boundary = self.advance_to_result_boundary(true).await?;
        Ok(self.apply_result_boundary(boundary))
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

    /// Returns `true` when transparent parameter encryption should be attempted:
    /// the connection requested Always Encrypted and the server acknowledged the
    /// feature during login.
    fn should_encrypt_parameters(&self) -> bool {
        self.negotiated_settings.is_column_encryption_supported()
            && self.effective_command_ce_setting() == ExecutionColumnEncryptionSetting::Enabled
    }

    /// Enforces the ForceColumnEncryption precondition: if any supplied parameter
    /// requires encryption but Always Encrypted is not enabled for this command,
    /// fail rather than sending it as plaintext. The per-parameter downgrade
    /// check (server reports the column plaintext) is enforced separately in
    /// [`apply_parameter_encryption`](Self::apply_parameter_encryption).
    fn ensure_force_column_encryption_supported<'p>(
        &self,
        params: impl IntoIterator<Item = &'p RpcParameter>,
    ) -> TdsResult<()> {
        if !self.should_encrypt_parameters()
            && params.into_iter().any(|p| p.force_column_encryption())
        {
            return Err(crate::error::Error::UsageError(
                "A parameter has ForceColumnEncryption set, but Always Encrypted is not enabled \
                 for this command; enable column encryption on the connection, or clear the flag."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Returns `true` when the connection negotiated Always Encrypted and
    /// enabled column encryption, ignoring any per-command override. Used by
    /// paths that have no per-command setting (bulk copy, prepared statements).
    fn column_encryption_enabled_on_connection(&self) -> bool {
        use crate::connection::client_context::ColumnEncryptionSetting;

        self.negotiated_settings.is_column_encryption_supported()
            && self
                .recovery_context
                .client_context
                .as_ref()
                .map(|c| c.column_encryption_setting == ColumnEncryptionSetting::Enabled)
                .unwrap_or(false)
    }

    /// Returns `true` when bulk copy row values should be encrypted: the server
    /// acknowledged Always Encrypted during login and the connection enabled
    /// column encryption. Bulk copy has no per-command override, so this folds
    /// directly against the connection setting.
    fn should_encrypt_bulk_copy(&self) -> bool {
        self.column_encryption_enabled_on_connection()
    }

    /// Normalizes a parameter name for matching across
    /// `sp_describe_parameter_encryption` output and RETURNVALUE tokens: strips a
    /// single leading `@` and ASCII-uppercases it, mirroring T-SQL's
    /// case-insensitive identifier matching.
    fn normalize_param_name(name: &str) -> String {
        name.strip_prefix('@').unwrap_or(name).to_ascii_uppercase()
    }

    /// Converts a RETURNVALUE token into a [`ReturnValue`], decrypting the value
    /// when it is an encrypted Always Encrypted output parameter and result
    /// decryption is active for the current command.
    ///
    /// An encrypted output parameter arrives as ciphertext with `CryptoMetaData`
    /// but no CEK table; its CEK is the one resolved when the matching input
    /// parameter was encrypted (retained in `output_param_ceks`). Decryption only
    /// happens when the command's effective Column Encryption setting is
    /// `Enabled` — under a `Disabled` or `ResultSetOnly` setting the ciphertext is
    /// passed through unchanged, mirroring the result-column path and ensuring a
    /// stale CEK from an earlier command is never consulted. A plaintext parameter
    /// is returned unchanged. Returns an error if an encrypted output parameter
    /// has no retained CEK or did not arrive as varbinary ciphertext, rather than
    /// surfacing ciphertext to the caller.
    fn finalize_return_value(
        &self,
        token: crate::token::tokens::ReturnValueToken,
    ) -> TdsResult<ReturnValue> {
        // Only decrypt when the encrypted value carries crypto metadata and
        // result decryption is enabled for this command. Otherwise pass the value
        // through unchanged (plaintext, or ciphertext varbinary when encryption is
        // disabled), consistent with how encrypted result columns are decoded.
        let crypto = match token.column_metadata.crypto_metadata.as_ref() {
            Some(crypto)
                if self.effective_command_ce_setting()
                    == ExecutionColumnEncryptionSetting::Enabled =>
            {
                crypto
            }
            _ => return Ok(token.into()),
        };

        let cek = self
            .output_param_ceks
            .get(&Self::normalize_param_name(&token.param_name))
            .ok_or_else(|| {
                crate::error::Error::ColumnEncryptionError(format!(
                    "No column encryption key available to decrypt encrypted output parameter {}",
                    token.param_name
                ))
            })?;

        let decrypted = match &token.value {
            ColumnValues::Null => ColumnValues::Null,
            ColumnValues::Bytes(cipher) => {
                crate::security::encryption::decrypt_cell(crypto, cek.as_slice(), cipher)?
            }
            other => {
                return Err(crate::error::Error::ColumnEncryptionError(format!(
                    "Encrypted output parameter {} was expected to arrive as varbinary cipher \
                     bytes, but decoded as {other:?}",
                    token.param_name
                )));
            }
        };

        // Reuse the `From<ReturnValueToken>` conversion for the field mapping and
        // only override the decrypted value, so this stays in sync if
        // `ReturnValue` gains fields later.
        let mut return_value: ReturnValue = token.into();
        return_value.value = decrypted;
        Ok(return_value)
    }

    /// Resolves the effective Column Encryption setting for the current command,
    /// folding the per-command override against the connection setting.
    ///
    /// A command's [`ExecutionColumnEncryptionSetting::UseConnectionSetting`]
    /// maps to `Enabled` when the connection enabled Always Encrypted, otherwise
    /// `Disabled`. Explicit per-command values are returned as-is.
    fn effective_command_ce_setting(&self) -> ExecutionColumnEncryptionSetting {
        use crate::connection::client_context::ColumnEncryptionSetting;

        match self.current_command_ce_setting {
            ExecutionColumnEncryptionSetting::UseConnectionSetting => {
                let connection_enabled = self
                    .recovery_context
                    .client_context
                    .as_ref()
                    .map(|c| c.column_encryption_setting == ColumnEncryptionSetting::Enabled)
                    .unwrap_or(false);
                if connection_enabled {
                    ExecutionColumnEncryptionSetting::Enabled
                } else {
                    ExecutionColumnEncryptionSetting::Disabled
                }
            }
            other => other,
        }
    }

    /// Calls `sp_describe_parameter_encryption` for the given statement and
    /// parameter declaration, parsing the two result sets into a
    /// [`DescribeParameterEncryptionResult`](crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult).
    ///
    /// The first result set carries the CEK table (keyed by ordinal); the second
    /// describes, per parameter, whether and how it must be encrypted.
    async fn describe_parameter_encryption(
        &mut self,
        tsql: &str,
        params_decl: &str,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult>
    {
        use crate::security::describe_parameter_encryption::{
            DescribeParameterEncryptionResult, accumulate_cek_entry, parse_parameter_info,
        };

        // Count every actual describe round-trip so callers/tests can confirm the
        // query-metadata cache is eliding repeats.
        self.describe_round_trips = self.describe_round_trips.saturating_add(1);

        self.transport.reset_reader();
        let database_collation = self.negotiated_settings.database_collation;

        let tsql_param = RpcParameter::new(
            Some("@tsql".to_string()),
            StatusFlags::NONE,
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(tsql.to_string()))),
        );
        let params_param = RpcParameter::new(
            Some("@params".to_string()),
            StatusFlags::NONE,
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_decl.to_string()))),
        );

        let rpc = SqlRpc::new(
            RpcType::Named("sp_describe_parameter_encryption".to_string()),
            None,
            Some(vec![tsql_param, params_param]),
            &database_collation,
            &self.execution_context,
        );
        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        let mut result = DescribeParameterEncryptionResult::new();

        // Result set 1: CEK table metadata.
        match self.move_to_column_metadata().await? {
            Some(metadata) => {
                self.current_metadata = Some(metadata);
                self.execution_context.set_has_open_batch(true);
                self.current_result_set_has_been_read_till_end = false;
            }
            None => {
                self.execution_context.set_has_open_batch(false);
                self.current_metadata = None;
                self.current_result_set_has_been_read_till_end = true;
                return Err(crate::error::Error::ColumnEncryptionError(
                    "sp_describe_parameter_encryption returned no result sets".to_string(),
                ));
            }
        }
        while let Some(row) = self.get_next_row().await? {
            accumulate_cek_entry(&mut result.cek_entries, &row)?;
        }

        // Result set 2: per-parameter encryption info.
        if self.move_to_next().await? {
            while let Some(row) = self.get_next_row().await? {
                result.parameters.push(parse_parameter_info(&row)?);
            }
        }

        self.close_query().await?;
        Ok(result)
    }

    /// Encrypts, in place, the parameters that `sp_describe_parameter_encryption`
    /// reports as requiring encryption: each flagged parameter's CEK is unwrapped
    /// through the registered key store providers, its plaintext value is
    /// normalized and encrypted, and the resulting ciphertext plus cipher
    /// metadata are stored on the [`RpcParameter`] for serialization.
    ///
    /// The describe result is served from (and populated into) the connection's
    /// query-metadata cache, so repeat executions of the same statement avoid the
    /// extra round-trip.
    async fn encrypt_parameters(
        &mut self,
        sql: &str,
        params_decl: &str,
        named_params: &mut [RpcParameter],
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        let mut param_refs: Vec<&mut RpcParameter> = named_params.iter_mut().collect();
        self.encrypt_combined_parameters(
            sql,
            params_decl,
            &mut param_refs,
            timeout_sec,
            cancel_handle,
        )
        .await
    }

    /// Describes and encrypts, in place, a combined set of parameter references.
    ///
    /// This is the shared core behind [`encrypt_parameters`](Self::encrypt_parameters)
    /// (all-named) and the stored-procedure path (positional and/or named): the
    /// caller assembles one slice of mutable parameter references in the same
    /// order they were declared in `params_decl`, and
    /// [`apply_parameter_encryption`](Self::apply_parameter_encryption) matches
    /// each describe entry back by name (named) or ordinal (positional).
    async fn encrypt_combined_parameters(
        &mut self,
        sql: &str,
        params_decl: &str,
        params: &mut [&mut RpcParameter],
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<()> {
        // Mirror SqlClient: don't cache metadata for statements with output
        // parameters — the client can't validate cached describe results against
        // a RETURNVALUE — but still use it for this call.
        let has_output = params.iter().any(|p| p.is_output());
        let describe = self
            .describe_parameters_cached(sql, params_decl, has_output, timeout_sec, cancel_handle)
            .await?;

        let (providers, cek_cache, trusted_key_paths) = self.cloned_ce_key_material()?;
        Self::apply_parameter_encryption(
            &describe,
            &providers,
            &cek_cache,
            params,
            &mut self.output_param_ceks,
            &trusted_key_paths,
        )
        .await
    }

    /// Returns the describe result for a statement, serving it from the
    /// connection's query-metadata cache when present and otherwise calling
    /// `sp_describe_parameter_encryption` and caching the result.
    ///
    /// When `skip_cache` is set (a statement with output parameters), the fresh
    /// describe result is returned but not stored, matching SqlClient.
    async fn describe_parameters_cached(
        &mut self,
        sql: &str,
        params_decl: &str,
        skip_cache: bool,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<
        std::sync::Arc<
            crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult,
        >,
    > {
        use crate::security::query_metadata_cache::QueryMetadataCache;

        let key = QueryMetadataCache::key(&self.negotiated_settings.database, sql);
        if let Some(describe) = self.query_metadata_cache.get(&key) {
            return Ok(describe);
        }

        let describe = std::sync::Arc::new(
            self.describe_parameter_encryption(sql, params_decl, timeout_sec, cancel_handle)
                .await?,
        );
        if !skip_cache {
            self.query_metadata_cache
                .insert(key, std::sync::Arc::clone(&describe));
        }
        Ok(describe)
    }

    /// Number of `sp_describe_parameter_encryption` round-trips this connection
    /// has sent to the server (query-metadata cache misses). Useful for
    /// observability and for verifying the metadata cache is effective.
    pub fn describe_round_trips(&self) -> u64 {
        self.describe_round_trips
    }

    /// Clones the `Arc` handles to the column-master-key provider registry and
    /// the CEK cache from the client context, plus the trusted master key path
    /// allow-list for the connected server, so the parameter-encryption paths
    /// can pass them around without holding a borrow on `self`.
    fn cloned_ce_key_material(
        &self,
    ) -> TdsResult<(
        std::sync::Arc<crate::security::keystore::ColumnEncryptionKeyStoreProviderRegistry>,
        std::sync::Arc<crate::security::keystore::CekCache>,
        Vec<String>,
    )> {
        let client_context = self
            .recovery_context
            .client_context
            .as_ref()
            .ok_or_else(|| {
                crate::error::Error::ColumnEncryptionError(
                    "Cannot encrypt parameters without a client context".to_string(),
                )
            })?;
        Ok((
            client_context.column_encryption_key_store_providers.clone(),
            client_context.cek_cache.clone(),
            client_context
                .trusted_key_paths_for_current_server()
                .to_vec(),
        ))
    }

    /// Matches a `sp_describe_parameter_encryption` result entry to a supplied
    /// parameter: by name first (case-insensitively, like a T-SQL identifier),
    /// otherwise falling back to the *unnamed* parameter at the describe's
    /// 1-based ordinal (the positional case). Requiring the ordinal slot to be
    /// unnamed keeps a named parameter from being matched by position.
    fn match_describe_param_index(
        params: &[&mut RpcParameter],
        info: &crate::security::describe_parameter_encryption::ParameterEncryptionInfo,
    ) -> Option<usize> {
        params
            .iter()
            .position(|p| {
                p.name
                    .as_deref()
                    .map(|n| n.eq_ignore_ascii_case(&info.parameter_name))
                    .unwrap_or(false)
            })
            .or_else(|| {
                (info.parameter_ordinal as usize)
                    .checked_sub(1)
                    .filter(|&i| i < params.len() && params[i].name.is_none())
            })
    }

    /// Encrypts, in place, the parameters that a prior
    /// `sp_describe_parameter_encryption` call reported as requiring encryption.
    ///
    /// Each describe parameter is matched to a supplied parameter by name
    /// (case-insensitively, like a T-SQL identifier); if no name matches it falls
    /// back to the *unnamed* parameter at the describe's 1-based ordinal (the
    /// positional case used by `sp_execute`). Accepting one combined slice of
    /// mutable references lets a single call cover all-named, all-positional, and
    /// mixed positional/named parameter lists. Each flagged parameter's CEK is
    /// unwrapped through the key store providers, its value is normalized and
    /// encrypted, and the ciphertext plus cipher metadata are stored on the
    /// [`RpcParameter`] for serialization.
    async fn apply_parameter_encryption(
        describe: &crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult,
        providers: &crate::security::keystore::ColumnEncryptionKeyStoreProviderRegistry,
        cek_cache: &crate::security::keystore::CekCache,
        params: &mut [&mut RpcParameter],
        output_param_ceks: &mut HashMap<String, Arc<zeroize::Zeroizing<Vec<u8>>>>,
        trusted_key_paths: &[String],
    ) -> TdsResult<()> {
        use crate::message::parameters::rpc_parameters::RpcEncryptionMetadata;
        use crate::security::encryption::encrypt_parameter;
        use crate::security::keystore::decrypt_cek;

        // Reset the per-command retained CEKs before (re)populating them for this
        // command's parameters, so a previous command's keys cannot leak into
        // this one's output-parameter decryption.
        output_param_ceks.clear();

        // ForceColumnEncryption: every supplied parameter that demands
        // encryption must be reported as encrypted by the server. Validate before
        // any work — and before the "nothing encrypted" early return below — so a
        // server that downgrades a forced parameter is caught rather than
        // silently sending its value as plaintext. A downgrade takes two forms,
        // both rejected here: the server reports the parameter's target column as
        // plaintext, or it omits a row for the parameter entirely (which would
        // otherwise slip past a describe-driven check and be serialized in the
        // clear).
        //
        // This is a server-trust failure — the "compromised server downgrades to
        // harvest plaintext" threat this flag defends against — so it is a
        // `ColumnEncryptionError`, matching the "parameter not supplied" mismatch
        // below and the trusted-master-key-path rejection in `decrypt_cek`. It is
        // deliberately distinct from the caller-misconfiguration case (the flag
        // set while Always Encrypted is off) that
        // `ensure_force_column_encryption_supported` reports as a `UsageError`.
        for index in 0..params.len() {
            if !params[index].force_column_encryption() {
                continue;
            }
            let reported_encrypted = describe.parameters.iter().any(|info| {
                info.is_encrypted() && Self::match_describe_param_index(params, info) == Some(index)
            });
            if !reported_encrypted {
                let name = params[index]
                    .name
                    .as_deref()
                    .unwrap_or("<positional>")
                    .to_string();
                return Err(crate::error::Error::ColumnEncryptionError(format!(
                    "Parameter {name} has ForceColumnEncryption set, but the server did not \
                     report it as encrypted; refusing to send it as plaintext.",
                )));
            }
        }

        // Nothing to do when the server reports no encrypted parameters.
        if !describe.parameters.iter().any(|p| p.is_encrypted()) {
            return Ok(());
        }

        for info in &describe.parameters {
            if !info.is_encrypted() {
                continue;
            }

            let cek_entry = describe.cek_entry_for(info.cek_ordinal).ok_or_else(|| {
                crate::error::Error::ColumnEncryptionError(format!(
                    "sp_describe_parameter_encryption referenced unknown CEK ordinal {} for parameter {}",
                    info.cek_ordinal, info.parameter_name
                ))
            })?;

            let Some(index) = Self::match_describe_param_index(params, info) else {
                return Err(crate::error::Error::ColumnEncryptionError(format!(
                    "sp_describe_parameter_encryption returned encryption info for parameter {} \
                     that was not supplied to the call",
                    info.parameter_name
                )));
            };

            // An encrypted positional (unnamed) OUTPUT parameter cannot have its
            // returned value decrypted: the RETURNVALUE token arrives unnamed, so
            // its ciphertext can't be matched back to the CEK, which is retained
            // under the parameter's synthetic describe name (`@ce_pos_N`). Reject
            // it with an actionable error instead of returning ciphertext the
            // caller can't read. Named OUTPUT parameters are unaffected (the
            // RETURNVALUE name matches), as are non-encrypted positional OUTPUT
            // parameters (they never reach this encrypted-parameter branch).
            if params[index].is_output() && params[index].name.is_none() {
                return Err(crate::error::Error::UsageError(
                    "Encrypted positional OUTPUT stored-procedure parameters are not supported \
                     because their returned value cannot be matched back to a column encryption \
                     key; pass the output parameter by name so it can be decrypted on return."
                        .to_string(),
                ));
            }

            let plaintext_cek =
                decrypt_cek(providers, cek_cache, cek_entry, trusted_key_paths).await?;

            // An encrypted RETURNVALUE output parameter carries no CEK table and
            // reuses the CEK that encrypted the matching input parameter. Retain
            // it here, keyed by normalized name, so the RETURNVALUE decode path
            // can decrypt the returned value.
            output_param_ceks.insert(
                Self::normalize_param_name(&info.parameter_name),
                plaintext_cek.clone(),
            );

            let param = &mut *params[index];

            let ciphertext = encrypt_parameter(
                param.value(),
                plaintext_cek.as_slice(),
                info.cipher_algorithm_id,
                info.encryption_type,
                info.normalization_rule_version,
            )?;

            param.set_encrypted(
                ciphertext,
                RpcEncryptionMetadata {
                    cipher_algorithm_id: info.cipher_algorithm_id,
                    encryption_type: info.encryption_type,
                    database_id: cek_entry.database_id,
                    cek_id: cek_entry.cek_id,
                    cek_version: cek_entry.cek_version,
                    cek_md_version: cek_entry.cek_md_version,
                    normalization_rule_version: info.normalization_rule_version,
                },
            );
        }
        Ok(())
    }

    /// Builds the `@tsql` and `@params` arguments for
    /// `sp_describe_parameter_encryption` when the original call is a stored
    /// procedure (rather than an `sp_executesql` statement).
    ///
    /// `@tsql` is an `EXEC` form of the call. Positional parameters bind by
    /// position and are given synthetic names (`@ce_pos_0`, `@ce_pos_1`, ...) so
    /// they can be declared in `@params` and referenced in the `EXEC`; named
    /// parameters bind by name. Positional arguments precede named ones, as
    /// T-SQL requires. `@params` is the matching declaration list. The synthetic
    /// names exist only in the describe request — the real RPC still sends the
    /// positional parameters unnamed, and the describe result is mapped back by
    /// ordinal (positional) or name (named) in
    /// [`apply_parameter_encryption`](Self::apply_parameter_encryption).
    ///
    /// Example: `EXEC dbo.p @ce_pos_0, @name1=@name1 OUTPUT` with
    /// `@ce_pos_0 int, @name1 nvarchar(10) OUTPUT`. Mirrors dotnet
    /// `BuildStoredProcedureStatementForColumnEncryption`, extended to cover the
    /// positional-parameter case the Rust API exposes.
    fn build_stored_procedure_describe_request(
        stored_procedure_name: &str,
        positional_params: &[RpcParameter],
        named_params: &[RpcParameter],
    ) -> TdsResult<(String, String)> {
        use std::fmt::Write as _;

        for named in named_params
            .iter()
            .filter_map(|param| param.name.as_deref())
        {
            for ordinal in 0..positional_params.len() {
                let synthetic = format!("@{SYNTHETIC_POSITIONAL_PARAM_PREFIX}{ordinal}");
                if named.eq_ignore_ascii_case(&synthetic) {
                    return Err(UsageError(format!(
                        "Named parameter '{named}' conflicts with internally generated positional parameter name '{synthetic}'"
                    )));
                }
            }
        }

        let mut tsql = format!("EXEC {stored_procedure_name}");
        let mut params_decl = String::new();
        let mut first = true;

        // Positional parameters: synthetic name, bound by position in the EXEC.
        for (ordinal, param) in positional_params.iter().enumerate() {
            let synthetic = format!("@{SYNTHETIC_POSITIONAL_PARAM_PREFIX}{ordinal}");
            let type_name = RpcParameter::get_sql_name(param.value())?;
            let output = if param.is_output() { " OUTPUT" } else { "" };

            if first {
                tsql.push(' ');
                first = false;
            } else {
                tsql.push_str(", ");
                params_decl.push_str(", ");
            }

            // `write!` into a String is infallible.
            let _ = write!(tsql, "{synthetic}{output}");
            let _ = write!(params_decl, "{synthetic} {type_name}{output}");
        }

        // Named parameters: bound by name.
        for param in named_params {
            let Some(name) = param.name.as_deref() else {
                continue;
            };
            let type_name = RpcParameter::get_sql_name(param.value())?;
            let output = if param.is_output() { " OUTPUT" } else { "" };

            if first {
                tsql.push(' ');
                first = false;
            } else {
                tsql.push_str(", ");
                params_decl.push_str(", ");
            }

            // `write!` into a String is infallible.
            let _ = write!(tsql, "{name}={name}{output}");
            let _ = write!(params_decl, "{name} {type_name}{output}");
        }

        Ok((tsql, params_decl))
    }

    /// Resolves (and memoizes) the cell decryptor for the current result set's
    /// CEK table, used to decrypt Always Encrypted columns while decoding rows.
    ///
    /// Returns `None` when the result set has no encrypted columns. The
    /// decryptor is rebuilt only when the result set (column metadata) changes,
    /// so the CEK table is resolved at most once per result set.
    async fn resolve_cell_decryptor(
        &mut self,
        metadata: &Arc<ColMetadataToken>,
    ) -> TdsResult<Option<Arc<dyn crate::security::cell_decryptor::CellDecryptor>>> {
        use crate::security::cell_decryptor::CellDecryptor;
        use crate::security::keystore::ResolvedCekDecryptor;

        // No CEK table normally means no encrypted columns in this result set.
        // A per-command `Disabled` override suppresses result decryption: any
        // encrypted column is then decoded as varbinary and its ciphertext is
        // returned through the normal decode path.
        if self.effective_command_ce_setting() == ExecutionColumnEncryptionSetting::Disabled {
            return Ok(None);
        }

        // An empty CEK table normally means the result set has no encrypted
        // columns. But the table can be empty even when a column carries
        // `CryptoMetadata` (a protocol/server anomaly); decryption is then
        // impossible, so fail fast rather than silently surface ciphertext for a
        // column we were asked to decrypt.
        if metadata.cek_table.is_empty() {
            if metadata.columns.iter().any(|c| c.crypto_metadata.is_some()) {
                return Err(crate::error::Error::ColumnEncryptionError(
                    "Result set has encrypted column metadata but an empty CEK table; \
                     cannot resolve column encryption keys"
                        .to_string(),
                ));
            }
            return Ok(None);
        }

        // Reuse the decryptor if it was built for this exact result set.
        if let Some((built_for, decryptor)) = &self.current_decryptor
            && Arc::ptr_eq(built_for, metadata)
        {
            return Ok(decryptor.clone());
        }

        let client_context = self
            .recovery_context
            .client_context
            .as_ref()
            .ok_or_else(|| {
                crate::error::Error::ColumnEncryptionError(
                    "Cannot decrypt encrypted columns without a client context".to_string(),
                )
            })?;

        let resolved = ResolvedCekDecryptor::resolve(
            &client_context.column_encryption_key_store_providers,
            &client_context.cek_cache,
            &metadata.cek_table,
            client_context.trusted_key_paths_for_current_server(),
        )
        .await;
        let decryptor: Arc<dyn CellDecryptor> = Arc::new(resolved);
        self.current_decryptor = Some((Arc::clone(metadata), Some(decryptor.clone())));
        Ok(Some(decryptor))
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
        let metadata = Arc::clone(self.current_metadata.as_ref().unwrap());
        let decryptor = self.resolve_cell_decryptor(&metadata).await?;
        let parser_context = ParserContext::ColumnMetadata(metadata, decryptor);
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
                        if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                            self.recovery_context.session_state_table.reset();
                        }
                        self.execution_context
                            .capture_change_property(&env_change, &mut self.negotiated_settings)?;
                        continue;
                    }
                    Tokens::SessionState(session_state) => {
                        self.recovery_context
                            .process_session_state(&session_state)?;
                        continue;
                    }
                    Tokens::ReturnValue(return_value_token) => {
                        let return_value = self.finalize_return_value(return_value_token)?;
                        self.push_return_value(return_value);
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
                    Tokens::Info(info_token) => {
                        info!(?info_token);
                        self.capture_info_message(&info_token);
                        continue;
                    }
                    Tokens::TabName | Tokens::ColInfo => {
                        continue;
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

    /// Returns the informational (INFO-token) messages captured from the
    /// current or most recent command's token stream — server `PRINT` output
    /// and low-severity `RAISERROR`/context notices.
    ///
    /// The buffer is reset at the start of each command, so this reflects only
    /// the most recent one. Messages are **retained even when that command
    /// returned an error**: a failed statement/RPC/batch surfaces its errors in
    /// [`Error::SqlServerError`](crate::error::Error::SqlServerError) whose
    /// `diagnostics.info_messages` is empty on the statement path, so any INFO it
    /// emitted must still be read from here. ([`close_query()`](Self::close_query)
    /// deliberately preserves the buffer for the same reason.)
    pub fn info_messages(&self) -> &[SqlInfoMessage] {
        &self.info_messages
    }

    /// Drains and returns the captured informational messages, leaving the
    /// buffer empty.
    ///
    /// Same lifecycle as [`info_messages()`](Self::info_messages): the buffer
    /// reflects the current/most-recent command, is populated even when that
    /// command errored (statement-path errors carry no INFO in
    /// [`Error::SqlServerError`](crate::error::Error::SqlServerError)), and is
    /// reset at the next command's start — so drain it before issuing the next
    /// command if you need the messages.
    pub fn take_info_messages(&mut self) -> Vec<SqlInfoMessage> {
        std::mem::take(&mut self.info_messages)
    }

    pub(crate) fn extend_info_messages(&mut self, messages: Vec<SqlInfoMessage>) {
        self.info_messages.extend(messages);
    }

    fn capture_info_message(&mut self, token: &crate::token::tokens::InfoToken) {
        self.info_messages.push(SqlInfoMessage::from(token));
    }

    /// Resets the informational-message buffer at the start of a new command so
    /// [`info_messages()`](Self::info_messages) reflects only that command.
    ///
    /// Call this at the top of every public token-consuming command, before
    /// consuming the response (and before `check_and_reconnect` where
    /// applicable): a transparent reconnect repopulates login messages for the
    /// new session *after* this point, so those remain visible as part of the
    /// command that triggered the reconnect.
    fn begin_command(&mut self) {
        self.info_messages.clear();
    }

    /// Returns and clears the prepared-statement handle captured from the most
    /// recent `sp_prepexec` (its `@handle` output parameter, RETURNVALUE
    /// ordinal 0).
    pub fn take_prepared_statement_handle(&mut self) -> Option<i32> {
        self.prepared_statement_handle.take()
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
        // Note: `info_messages` is intentionally NOT cleared here. Draining the
        // trailing token stream above can surface INFO/warning messages (e.g. a
        // PRINT after the last result set), and the caller drains them via
        // `take_info_messages()` after this returns (see the ODBC
        // `drain_and_release` path). Clearing them here would discard them.
        // The sp_prepexec @handle, if any, was captured during the drain above
        // (see push_return_value) and survives this clear.
        self.current_metadata = None;
        self.return_values.clear();
        self.expecting_prepare_handle = false;
        self.remaining_request_timeout = None;
        self.cancel_handle = None;
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;
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

    /// Returns whether session recovery (idle connection resiliency) was
    /// negotiated with the server during login.
    ///
    /// When `true`, the driver will transparently attempt to reconnect and
    /// restore session state if a dead connection is detected before executing
    /// a command — provided the session is in a recoverable state (no open
    /// transactions, etc.).
    pub fn is_session_recovery_enabled(&self) -> bool {
        self.recovery_context.session_recovery_negotiated
    }

    /// Returns the number of times this connection has been successfully
    /// recovered after detecting a dead connection.
    ///
    /// The count is incremented each time [`reconnect()`] completes
    /// successfully, including session-state restoration and server-property
    /// validation.
    pub fn connection_recovery_count(&self) -> u32 {
        self.recovery_context.recovery_count
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

        self.begin_command();
        // begin_transaction has no command timeout — use connect_timeout as fallback.
        let _reconnect_elapsed = self.check_and_reconnect(None, None).await?;

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
        self.begin_command();
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
        self.begin_command();
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
        self.begin_command();
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
        self.begin_command();
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
            self.current_metadata = None;
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
                Tokens::Info(info_token) => {
                    info!(?info_token);
                    self.capture_info_message(&info_token);
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                        self.recovery_context.session_state_table.reset();
                    }
                    self.execution_context
                        .capture_change_property(&env_change, &mut self.negotiated_settings)?;
                    continue;
                }
                Tokens::SessionState(session_state) => {
                    self.recovery_context
                        .process_session_state(&session_state)?;
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

/// The outcome of advancing to the next statement boundary during
/// statement-wise result navigation
/// ([`execute_multi_statement`](TdsClient::execute_multi_statement) /
/// [`move_to_next_statement`](TdsClient::move_to_next_statement)).
///
/// Unlike the result-set navigation used by [`execute`](TdsClient::execute) /
/// [`move_to_next`](TdsClient::move_to_next) — which collapses statements that
/// return no rows — this exposes every statement in a batch individually,
/// matching msodbcsql's `SQLExecDirect` + `SQLMoreResults` behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementResult {
    /// A row-returning result set (e.g. `SELECT`). Column metadata is available
    /// via [`TdsClient::get_metadata`] and rows via the [`ResultSet`] API.
    RowSet,
    /// A statement that produced no result set (`PRINT`, low-severity
    /// `RAISERROR`, DDL, or DML). It has zero columns; `rows_affected` is the
    /// row count reported by the statement's DONE token (0 for PRINT/RAISERROR).
    NoRows {
        /// Rows affected reported by the statement's DONE token.
        rows_affected: u64,
    },
    /// No more statements remain in the batch.
    End,
}

/// Internal boundary kind produced by
/// [`advance_to_result_boundary`](TdsClient::advance_to_result_boundary),
/// before it is mapped to the public [`StatementResult`].
enum ResultBoundaryKind {
    /// A row-returning result set; carries its column metadata.
    RowSet(Arc<ColMetadataToken>),
    /// A no-row statement (only produced with `expose_norow_statements = true`).
    NoRows { rows_affected: u64 },
    /// End of batch.
    End,
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
    use crate::connection::client_context::ClientContext;
    use crate::connection::transport::network_transport::TransportSslHandler;
    use crate::connection::transport::tds_transport::TdsTransport;
    use crate::core::{CancelHandle, TdsResult};
    use crate::datatypes::row_writer::RowWriter;
    use crate::io::reader_writer::{NetworkReader, NetworkWriter};
    use crate::io::token_stream::{ParserContext, RowReadResult, TdsTokenStreamReader};
    use crate::token::tokens::{
        ColMetadataToken, CurrentCommand, DoneStatus, DoneToken, InfoToken, Tokens,
    };
    use async_trait::async_trait;
    use std::collections::VecDeque;

    // ── Minimal mock transport for reconnect() unit tests ──

    #[derive(Debug)]
    struct TestTransport {
        closed: bool,
        pending_tokens: VecDeque<Tokens>,
        reset_mode: ResetConnectionMode,
        /// Every byte handed to `send` (request framing + payload), so tests can
        /// assert what was actually written to the wire.
        sent: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl TestTransport {
        fn new() -> Self {
            Self {
                closed: false,
                pending_tokens: VecDeque::new(),
                reset_mode: ResetConnectionMode::None,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn with_tokens(tokens: Vec<Tokens>) -> Self {
            Self {
                closed: false,
                pending_tokens: VecDeque::from(tokens),
                reset_mode: ResetConnectionMode::None,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl TdsTokenStreamReader for TestTransport {
        async fn receive_token(
            &mut self,
            _context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
        ) -> TdsResult<Tokens> {
            if let Some(tok) = self.pending_tokens.pop_front() {
                return Ok(tok);
            }
            Err(crate::error::Error::ConnectionClosed("test".to_string()))
        }

        async fn receive_row_into(
            &mut self,
            _context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
            _writer: &mut (dyn RowWriter + Send),
        ) -> TdsResult<RowReadResult> {
            Err(crate::error::Error::ConnectionClosed("test".to_string()))
        }
    }

    #[async_trait]
    impl TransportSslHandler for TestTransport {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
        async fn disable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl NetworkWriter for TestTransport {
        async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
            self.sent.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
        fn packet_size(&self) -> u32 {
            4096
        }
        fn get_encryption_setting(&self) -> crate::core::NegotiatedEncryptionSetting {
            crate::core::NegotiatedEncryptionSetting::NoEncryption
        }
        fn set_reset_mode(&mut self, mode: ResetConnectionMode) {
            self.reset_mode = mode;
        }
        fn take_reset_mode(&mut self) -> ResetConnectionMode {
            std::mem::replace(&mut self.reset_mode, ResetConnectionMode::None)
        }
    }

    #[async_trait]
    impl NetworkReader for TestTransport {
        async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
            buffer.fill(0);
            Ok(0)
        }
        fn packet_size(&self) -> u32 {
            4096
        }
    }

    #[async_trait]
    impl TdsTransport for TestTransport {
        fn as_writer(&mut self) -> &mut dyn NetworkWriter {
            self
        }
        fn reset_reader(&mut self) {}
        fn packet_size(&self) -> u32 {
            4096
        }
        async fn close_transport(&mut self) -> TdsResult<()> {
            self.closed = true;
            Ok(())
        }
        async fn send_attention_with_timeout(&mut self, _timeout: Duration) -> TdsResult<bool> {
            Ok(false)
        }
        fn is_connection_dead(&self) -> bool {
            true
        }
    }

    fn create_test_client() -> TdsClient {
        let transport = Box::new(TestTransport::new());
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
        )
    }

    #[test]
    fn prepare_reset_connection_routes_mode_to_transport() {
        let mut client = create_test_client();

        // Default: no reset pending.
        assert_eq!(
            client.transport.as_writer().take_reset_mode(),
            ResetConnectionMode::None
        );

        // Plain reset.
        client.prepare_reset_connection(false);
        assert_eq!(
            client.transport.as_writer().take_reset_mode(),
            ResetConnectionMode::Reset
        );
        // take_reset_mode is one-shot.
        assert_eq!(
            client.transport.as_writer().take_reset_mode(),
            ResetConnectionMode::None
        );

        // Preserve transaction => SKIPTRAN.
        client.prepare_reset_connection(true);
        assert_eq!(
            client.transport.as_writer().take_reset_mode(),
            ResetConnectionMode::ResetSkipTran
        );
    }

    fn create_test_client_with_tokens(tokens: Vec<Tokens>) -> TdsClient {
        let transport = Box::new(TestTransport::with_tokens(tokens));
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
        )
    }

    /// Builds a client whose transport replays `tokens` and captures every byte
    /// written to the wire, returning the shared capture buffer alongside it.
    fn create_capturing_client(tokens: Vec<Tokens>) -> (TdsClient, Arc<std::sync::Mutex<Vec<u8>>>) {
        let transport = Box::new(TestTransport::with_tokens(tokens));
        let sent = Arc::clone(&transport.sent);
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        let client = TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
        );
        (client, sent)
    }

    fn done_no_more() -> Tokens {
        Tokens::Done(DoneToken {
            status: DoneStatus::FINAL,
            cur_cmd: CurrentCommand::Insert,
            row_count: 0,
        })
    }

    fn info_token(number: u32, severity: u8, message: &str) -> Tokens {
        Tokens::Info(InfoToken {
            number,
            state: 1,
            severity,
            message: message.to_string(),
            server_name: "test-server".to_string(),
            proc_name: String::new(),
            line_number: 7,
        })
    }

    fn empty_col_metadata() -> Tokens {
        Tokens::ColMetadata(ColMetadataToken::default())
    }

    fn stale_metadata() -> Arc<ColMetadataToken> {
        Arc::new(ColMetadataToken::default())
    }

    fn done_more() -> Tokens {
        Tokens::Done(DoneToken {
            status: DoneStatus::MORE,
            cur_cmd: CurrentCommand::Insert,
            row_count: 0,
        })
    }

    fn done_more_with_count(row_count: u64) -> Tokens {
        Tokens::Done(DoneToken {
            status: DoneStatus::MORE | DoneStatus::COUNT,
            cur_cmd: CurrentCommand::Insert,
            row_count,
        })
    }

    /// Statement-wise navigation exposes each no-row statement (PRINT /
    /// RAISERROR) as its own result, matching msodbcsql, instead of collapsing
    /// them the way `execute()` / `move_to_next()` do.
    #[tokio::test]
    async fn execute_multi_statement_exposes_each_norow_statement() {
        // Batch: PRINT N'one'; RAISERROR(N'two', 10, 1);
        let mut client = create_test_client_with_tokens(vec![
            info_token(0, 0, "print one"),
            done_more(),
            info_token(50000, 10, "raiserror two"),
            done_no_more(),
        ]);

        // First statement surfaces as its own no-row result.
        let r1 = client
            .execute_multi_statement(
                "PRINT N'one'; RAISERROR(N'two', 10, 1) WITH NOWAIT;".to_string(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r1, StatementResult::NoRows { rows_affected: 0 });
        // Only the first statement's INFO is present when it is drained.
        let info1 = client.take_info_messages();
        assert!(
            info1.iter().any(|m| m.message == "print one"),
            "first statement's PRINT should be captured: {info1:?}"
        );
        assert!(
            !info1.iter().any(|m| m.message == "raiserror two"),
            "second statement's INFO must not leak into the first: {info1:?}"
        );

        // Second statement is a separate no-row result.
        let r2 = client.move_to_next_statement().await.unwrap();
        assert_eq!(r2, StatementResult::NoRows { rows_affected: 0 });
        let info2 = client.take_info_messages();
        assert!(
            info2.iter().any(|m| m.message == "raiserror two"),
            "second statement's RAISERROR should surface on its own step: {info2:?}"
        );

        // No more statements.
        let r3 = client.move_to_next_statement().await.unwrap();
        assert_eq!(r3, StatementResult::End);
    }

    /// A single no-row statement is exposed once, then the batch ends.
    #[tokio::test]
    async fn execute_multi_statement_single_norow_then_end() {
        let mut client =
            create_test_client_with_tokens(vec![info_token(0, 0, "just a print"), done_no_more()]);

        let r1 = client
            .execute_multi_statement("PRINT N'just a print';".to_string(), None, None)
            .await
            .unwrap();
        assert_eq!(r1, StatementResult::NoRows { rows_affected: 0 });

        let r2 = client.move_to_next_statement().await.unwrap();
        assert_eq!(r2, StatementResult::End);
    }

    /// Statement-wise navigation collapses pure no-op statements (no row count,
    /// no messages — e.g. `CREATE TABLE`) but surfaces a DML statement's row
    /// count, matching msodbcsql (`CREATE; INSERT; SELECT` exposes the INSERT
    /// count and the SELECT, not the bare CREATE).
    #[tokio::test]
    async fn execute_multi_statement_collapses_noop_surfaces_rowcount() {
        let mut client = create_test_client_with_tokens(vec![
            done_more(),             // pure no-op (CREATE) - collapsed
            done_more_with_count(5), // DML with a row count - surfaced
            done_no_more(),          // trailing no-op - collapsed -> End
        ]);

        let r1 = client
            .execute_multi_statement(
                "CREATE TABLE #t(i int); INSERT INTO #t VALUES(1);".to_string(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r1, StatementResult::NoRows { rows_affected: 5 });

        let r2 = client.move_to_next_statement().await.unwrap();
        assert_eq!(r2, StatementResult::End);
    }

    #[tokio::test]
    async fn move_to_next_statement_end_when_no_open_batch() {
        let mut client = create_test_client();
        assert_eq!(
            client.move_to_next_statement().await.unwrap(),
            StatementResult::End
        );
    }

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

    #[test]
    fn normalize_param_name_strips_at_and_uppercases() {
        // A single leading '@' is stripped and the name is ASCII-uppercased, so a
        // describe parameter name, an RPC parameter name, and a RETURNVALUE name
        // for the same parameter all normalize to the same key.
        assert_eq!(TdsClient::normalize_param_name("@Out"), "OUT");
        assert_eq!(TdsClient::normalize_param_name("out"), "OUT");
        assert_eq!(TdsClient::normalize_param_name("@p1"), "P1");
        // Only one leading '@' is stripped.
        assert_eq!(TdsClient::normalize_param_name("@@version"), "@VERSION");
        assert_eq!(TdsClient::normalize_param_name(""), "");
    }

    #[tokio::test]
    async fn consume_done_token_captures_all_info_tokens() {
        let mut client = create_test_client_with_tokens(vec![
            info_token(5701, 10, "Changed database context to 'master'."),
            info_token(0, 0, "hello from PRINT"),
            done_no_more(),
        ]);

        let rows_affected = client.consume_done_token().await.unwrap();

        assert_eq!(rows_affected, 0);
        assert_eq!(client.info_messages().len(), 2);
        assert_eq!(client.info_messages()[0].number, 5701);
        assert_eq!(client.info_messages()[0].class, 10);
        assert_eq!(client.info_messages()[1].message, "hello from PRINT");

        let messages = client.take_info_messages();
        assert_eq!(messages.len(), 2);
        assert!(client.info_messages().is_empty());
    }

    #[test]
    fn begin_command_clears_stale_info_messages() {
        // Simulates login/connect messages left on the client before a new
        // command starts. `begin_command` (called at the top of every execute*
        // entry point) must clear them so `info_messages()` reflects only the
        // current command.
        let mut client = create_test_client();
        client.extend_info_messages(vec![SqlInfoMessage::from(
            &crate::token::tokens::InfoToken {
                number: 5701,
                state: 1,
                severity: 10,
                message: "Changed database context to 'master'.".to_string(),
                server_name: "srv".to_string(),
                proc_name: String::new(),
                line_number: 1,
            },
        )]);
        assert_eq!(client.info_messages().len(), 1);

        client.begin_command();
        assert!(
            client.info_messages().is_empty(),
            "begin_command must clear stale info messages"
        );
    }

    #[tokio::test]
    async fn execute_sp_unprepare_clears_stale_info_messages() {
        // Regression: token-consuming commands beyond the execute*/query family
        // (here sp_unprepare, which drains via drain_stream) must also reset the
        // info buffer so a prior command's messages don't leak into this one.
        let mut client = create_test_client_with_tokens(vec![
            info_token(0, 0, "unprepare info"),
            done_no_more(),
        ]);

        // Stale message left over from an earlier command / login.
        client.extend_info_messages(vec![SqlInfoMessage {
            message: "stale from previous command".to_string(),
            state: 1,
            class: 10,
            number: 5701,
            server_name: None,
            proc_name: None,
            line_number: None,
        }]);

        client.execute_sp_unprepare(1, None, None).await.unwrap();

        let msgs = client.info_messages();
        assert!(
            msgs.iter()
                .all(|m| m.message != "stale from previous command"),
            "stale info from a prior command must be cleared: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.message == "unprepare info"),
            "the current command's info should be captured: {msgs:?}"
        );
    }

    // ── finalize_return_value (encrypted RETURNVALUE decryption) tests ──

    /// Builds a minimal `CryptoMetadata` describing an encrypted `int` output
    /// parameter (AEAD cipher, base type `IntN`).
    fn ae_crypto_metadata() -> crate::query::metadata::CryptoMetadata {
        use crate::datatypes::sqldatatypes::{
            FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant,
        };
        crate::query::metadata::CryptoMetadata {
            cek_table_ordinal: 0,
            base_data_type: TdsDataType::IntN,
            base_type_info: TypeInfo {
                tds_type: TdsDataType::IntN,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            cipher_algorithm_id: 2,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        }
    }

    /// Builds a RETURNVALUE token named `name` carrying `value`, with the given
    /// optional crypto metadata (present = encrypted output parameter).
    fn ae_return_value_token(
        name: &str,
        value: ColumnValues,
        crypto: Option<crate::query::metadata::CryptoMetadata>,
    ) -> crate::token::tokens::ReturnValueToken {
        use crate::datatypes::sqldatatypes::{
            FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant,
        };
        let column_metadata = crate::query::metadata::ColumnMetadata {
            user_type: 0,
            flags: if crypto.is_some() { 0x0800 } else { 0 },
            data_type: TdsDataType::BigVarBinary,
            type_info: TypeInfo {
                tds_type: TdsDataType::BigVarBinary,
                length: 8000,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            column_name: name.to_string(),
            multi_part_name: None,
            crypto_metadata: crypto,
        };
        crate::token::tokens::ReturnValueToken {
            param_ordinal: 0,
            param_name: name.to_string(),
            value,
            column_metadata: Box::new(column_metadata),
            status: crate::token::tokenitems::ReturnValueStatus::from(0u8),
        }
    }

    fn insert_test_cek(client: &mut TdsClient, name: &str, cek: Vec<u8>) {
        client.output_param_ceks.insert(
            TdsClient::normalize_param_name(name),
            std::sync::Arc::new(zeroize::Zeroizing::new(cek)),
        );
    }

    #[test]
    fn finalize_return_value_passes_through_plaintext() {
        // No crypto metadata => a plaintext RETURNVALUE is returned unchanged.
        let client = create_test_client();
        let token = ae_return_value_token("@out", ColumnValues::Int(7), None);
        let rv = client.finalize_return_value(token).unwrap();
        assert_eq!(rv.value, ColumnValues::Int(7));
    }

    #[test]
    fn finalize_return_value_passes_through_ciphertext_when_disabled() {
        // Encrypted value but the command disabled AE => ciphertext is passed
        // through unchanged and no CEK is consulted.
        let mut client = create_test_client();
        client.current_command_ce_setting = ExecutionColumnEncryptionSetting::Disabled;
        let token = ae_return_value_token(
            "@out",
            ColumnValues::Bytes(vec![1, 2, 3]),
            Some(ae_crypto_metadata()),
        );
        let rv = client.finalize_return_value(token).unwrap();
        assert_eq!(rv.value, ColumnValues::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn finalize_return_value_errors_without_cek() {
        // Encrypted value under an enabled command but no retained CEK => error
        // rather than surfacing ciphertext.
        let mut client = create_test_client();
        client.current_command_ce_setting = ExecutionColumnEncryptionSetting::Enabled;
        let token = ae_return_value_token(
            "@out",
            ColumnValues::Bytes(vec![1, 2, 3]),
            Some(ae_crypto_metadata()),
        );
        let err = client.finalize_return_value(token).unwrap_err();
        assert!(matches!(err, crate::error::Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn finalize_return_value_decrypts_null_output() {
        // A NULL encrypted output parameter decrypts to NULL without invoking the
        // cipher.
        let mut client = create_test_client();
        client.current_command_ce_setting = ExecutionColumnEncryptionSetting::Enabled;
        insert_test_cek(&mut client, "@out", vec![0u8; 32]);
        let token = ae_return_value_token("@out", ColumnValues::Null, Some(ae_crypto_metadata()));
        let rv = client.finalize_return_value(token).unwrap();
        assert_eq!(rv.value, ColumnValues::Null);
    }

    #[test]
    fn finalize_return_value_errors_on_non_varbinary_ciphertext() {
        // An encrypted output parameter that did not arrive as varbinary cipher
        // bytes is a protocol violation.
        let mut client = create_test_client();
        client.current_command_ce_setting = ExecutionColumnEncryptionSetting::Enabled;
        insert_test_cek(&mut client, "@out", vec![0u8; 32]);
        let token = ae_return_value_token("@out", ColumnValues::Int(5), Some(ae_crypto_metadata()));
        let err = client.finalize_return_value(token).unwrap_err();
        assert!(matches!(err, crate::error::Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn finalize_return_value_decrypts_ciphertext() {
        // A ciphertext output parameter decrypts to the original value using the
        // retained CEK.
        use crate::security::encryption::{AeadAes256CbcHmacSha256, ColumnEncryptionType};
        let mut client = create_test_client();
        client.current_command_ce_setting = ExecutionColumnEncryptionSetting::Enabled;
        let cek = [0x2a_u8; 32];
        insert_test_cek(&mut client, "@out", cek.to_vec());

        // Normalized 8-byte little-endian form of an `int`, encrypted with the CEK.
        let normalized = 987_654_i64.to_le_bytes();
        let cipher = AeadAes256CbcHmacSha256::new(&cek)
            .unwrap()
            .encrypt(&normalized, ColumnEncryptionType::Randomized)
            .unwrap();
        let token = ae_return_value_token(
            "@out",
            ColumnValues::Bytes(cipher),
            Some(ae_crypto_metadata()),
        );
        let rv = client.finalize_return_value(token).unwrap();
        assert_eq!(rv.value, ColumnValues::Int(987_654));
    }

    // ── Reconnection orchestration tests ──

    #[tokio::test]
    async fn reconnect_fails_when_not_negotiated() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = false;

        let result = client.reconnect(Duration::from_secs(10), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Session not recoverable"),
            "Expected SessionNotRecoverable, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_fails_when_no_client_context() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        client.recovery_context.client_context = None;

        let result = client.reconnect(Duration::from_secs(10), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("No client context"),
            "Expected no client context error, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_fails_when_transaction_active() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        client.execution_context.set_transaction_descriptor(999);

        let result = client.reconnect(Duration::from_secs(10), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Session not recoverable"),
            "Expected SessionNotRecoverable, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_fails_when_batch_open() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        client.execution_context.set_has_open_batch(true);

        let result = client.reconnect(Duration::from_secs(10), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Session not recoverable"),
            "Expected SessionNotRecoverable, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_fails_with_zero_timeout() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;

        // Zero-duration timeout → deadline immediately exceeded
        let result = client.reconnect(Duration::ZERO, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Session recovery failed"),
            "Expected SessionRecoveryFailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_returns_session_recovery_failed_on_connection_failure() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        // Use a very short timeout — connect will fail (no server) and exhaust attempts
        // connect_retry_count defaults to 1, connect_retry_interval defaults to 10
        // With a 1-second timeout the first attempt fails and no time for retry
        let result = client.reconnect(Duration::from_secs(1), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should be SessionRecoveryFailed (not SessionNotRecoverable)
        assert!(
            err.to_string().contains("Session recovery failed")
                || err.to_string().contains("attempt"),
            "Expected SessionRecoveryFailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn reconnect_increments_recovery_count_tracking() {
        // Verify initial state
        let client = create_test_client();
        assert_eq!(client.recovery_context.recovery_count, 0);
    }

    // ── Pre-execution dead connection check tests ──

    #[tokio::test]
    async fn check_and_reconnect_skips_when_not_negotiated() {
        let mut client = create_test_client();
        // session_recovery_negotiated is false by default
        assert!(!client.recovery_context.session_recovery_negotiated);

        // Should return Ok(Duration::ZERO) even though transport is "dead"
        let elapsed = client.check_and_reconnect(Some(5), None).await.unwrap();
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[tokio::test]
    async fn check_and_reconnect_skips_when_retry_count_zero() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        if let Some(ref mut ctx) = client.recovery_context.client_context {
            ctx.connect_retry_count = 0;
        }

        // Should skip even with dead transport because retry count is 0
        let elapsed = client.check_and_reconnect(Some(5), None).await.unwrap();
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[tokio::test]
    async fn check_and_reconnect_returns_error_when_dead_and_not_recoverable() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        // Make it not recoverable by starting a transaction
        client.execution_context.set_transaction_descriptor(42);

        let result = client.check_and_reconnect(Some(5), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Connection is dead"),
            "Expected ConnectionClosed, got: {err}"
        );
    }

    #[tokio::test]
    async fn check_and_reconnect_attempts_reconnect_when_dead_and_recoverable() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        // Transport (TestTransport) returns is_connection_dead() = true,
        // recovery is possible (no txn, no open batch, negotiated=true).
        // reconnect() will fail because TestTransport can't actually connect,
        // but it should be *attempted* — we'll get SessionRecoveryFailed.
        let result = client.check_and_reconnect(Some(1), None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Session recovery failed"),
            "Expected reconnect attempt resulting in SessionRecoveryFailed, got: {err}"
        );
    }

    #[tokio::test]
    async fn check_and_reconnect_skips_when_no_client_context() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        client.recovery_context.client_context = None;

        // connect_retry_count defaults to 0 when no client context → skip
        let elapsed = client.check_and_reconnect(Some(5), None).await.unwrap();
        assert_eq!(elapsed, Duration::ZERO);
    }

    // ── deduct_timeout tests ──

    #[test]
    fn deduct_timeout_subtracts_elapsed() {
        let result = TdsClient::deduct_timeout(Some(30), Duration::from_secs(12));
        assert_eq!(result, Some(18));
    }

    #[test]
    fn deduct_timeout_saturates_at_zero() {
        let result = TdsClient::deduct_timeout(Some(5), Duration::from_secs(10));
        assert_eq!(result, Some(0));
    }

    #[test]
    fn deduct_timeout_passes_through_none() {
        let result = TdsClient::deduct_timeout(None, Duration::from_secs(10));
        assert_eq!(result, None);
    }

    #[test]
    fn deduct_timeout_zero_elapsed() {
        let result = TdsClient::deduct_timeout(Some(30), Duration::ZERO);
        assert_eq!(result, Some(30));
    }

    #[test]
    fn deduct_timeout_rounds_up_sub_second() {
        // 1.9 seconds elapsed should round up to 2 seconds deducted
        let result = TdsClient::deduct_timeout(Some(30), Duration::from_millis(1900));
        assert_eq!(result, Some(28));
    }

    // Public Recovery API ──────────────────────────────────────

    #[test]
    fn is_session_recovery_enabled_returns_false_by_default() {
        let client = create_test_client();
        assert!(!client.is_session_recovery_enabled());
    }

    #[test]
    fn is_session_recovery_enabled_returns_true_when_negotiated() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;
        assert!(client.is_session_recovery_enabled());
    }

    #[test]
    fn connection_recovery_count_starts_at_zero() {
        let client = create_test_client();
        assert_eq!(client.connection_recovery_count(), 0);
    }

    #[test]
    fn connection_recovery_count_reflects_recovery_count() {
        let mut client = create_test_client();
        client.recovery_context.recovery_count = 3;
        assert_eq!(client.connection_recovery_count(), 3);
    }

    // ── execute() / current_metadata invariants ──
    //
    // After an execute path returns, `current_metadata` must reflect the
    // current batch:
    //   - DDL/DML (no COLMETADATA from the server) → `current_metadata` is None
    //     and `has_open_batch` is false.
    //   - Result-bearing query (COLMETADATA received) → `current_metadata`
    //     points at the freshly received metadata and `has_open_batch` is true.
    // No state from a prior batch is observable via `get_metadata()`.

    /// Seeds a client with stale metadata + a single DONE token, runs `invoke`,
    /// and asserts the no-result-set post-conditions. Failure attribution comes
    /// from the calling test's name.
    async fn assert_no_result_set_clears_metadata<F>(invoke: F)
    where
        F: AsyncFnOnce(&mut TdsClient) -> TdsResult<()>,
    {
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client.current_metadata = Some(stale_metadata());

        invoke(&mut client)
            .await
            .expect("execute path should succeed against the queued DONE token");

        assert!(
            client.current_metadata.is_none(),
            "DDL/DML must clear cached metadata so get_metadata() doesn't return stale columns"
        );
        assert!(
            !client.execution_context.has_open_batch(),
            "no result set => has_open_batch must be false"
        );
        assert!(client.get_metadata().is_empty());
    }

    #[tokio::test]
    async fn execute_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute("INSERT INTO t VALUES (1)".to_string(), None, None)
                .await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_replaces_stale_metadata_when_result_set_returned() {
        let mut client = create_test_client_with_tokens(vec![empty_col_metadata(), done_no_more()]);
        let stale = stale_metadata();
        client.current_metadata = Some(Arc::clone(&stale));

        client
            .execute("SELECT 1".to_string(), None, None)
            .await
            .expect("execute should consume COLMETADATA and return Ok");

        let new_metadata = client
            .current_metadata
            .as_ref()
            .expect("COLMETADATA branch must populate current_metadata");
        assert!(
            !Arc::ptr_eq(new_metadata, &stale),
            "current_metadata must point to the freshly received COLMETADATA, not the stale Arc"
        );
        assert!(
            client.execution_context.has_open_batch(),
            "result-set => has_open_batch must be true"
        );
    }

    #[tokio::test]
    async fn execute_stored_procedure_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_stored_procedure("dbo.do_work".to_string(), None, None, None, None)
                .await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_sp_executesql_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_sp_executesql("UPDATE t SET v = 1".to_string(), Vec::new(), None, None)
                .await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_sp_prepexec_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_sp_prepexec(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                None,
                None,
                None,
            )
            .await
        })
        .await;
    }

    // The `@handle` positional parameter of sp_prepexec serializes as:
    //   0x00  positional name length
    //   0x01  status flags = BY_REF_VALUE
    //   0x26  TYPE_INFO type byte = INTN
    //   0x04  TYPE_INFO max size = 4
    //   value: length byte then little-endian bytes (length 0x00 for NULL).
    // These tests pin the byte the current selection controls: `drop_handle`
    // becomes the input value of that by-reference `@handle`.

    #[tokio::test]
    async fn execute_sp_prepexec_sends_drop_handle_as_byref_handle_input() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        client
            .execute_sp_prepexec(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                Some(0x5152_5354),
                None,
                None,
            )
            .await
            .expect("sp_prepexec should succeed against the queued DONE token");

        let bytes = sent.lock().unwrap().clone();
        let expected = [0x00, 0x01, 0x26, 0x04, 0x04, 0x54, 0x53, 0x52, 0x51];
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "Some(handle) must be sent as the by-reference @handle input so the server \
             drops the prior prepared statement"
        );
    }

    #[tokio::test]
    async fn execute_sp_prepexec_sends_null_handle_when_no_drop_handle() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        client
            .execute_sp_prepexec(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                None,
                None,
                None,
            )
            .await
            .expect("sp_prepexec should succeed against the queued DONE token");

        let bytes = sent.lock().unwrap().clone();
        let expected_null = [0x00, 0x01, 0x26, 0x04, 0x00];
        assert!(
            bytes
                .windows(expected_null.len())
                .any(|w| w == expected_null),
            "None must send a NULL @handle input so the server prepares fresh"
        );
    }

    /// Builds an `Int` RETURNVALUE for exercising `push_return_value` directly.
    /// The column metadata is irrelevant to the capture logic (which only
    /// inspects the value), so a minimal INT4 descriptor is used.
    fn int_return_value(ordinal: u16, value: i32) -> ReturnValue {
        use crate::datatypes::sqldatatypes::{
            FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant,
        };
        use crate::token::tokenitems::ReturnValueStatus;

        ReturnValue {
            param_ordinal: ordinal,
            param_name: String::new(),
            value: ColumnValues::Int(value),
            column_metadata: Box::new(ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::IntN,
                type_info: TypeInfo {
                    tds_type: TdsDataType::IntN,
                    length: 4,
                    type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
                },
                column_name: String::new(),
                multi_part_name: None,
                crypto_metadata: None,
            }),
            status: ReturnValueStatus::OutputParam,
        }
    }

    #[test]
    fn push_return_value_captures_handle_then_surfaces_following_output_params() {
        let mut client = create_test_client();
        client.expecting_prepare_handle = true;

        // First value = the sp_prepexec @handle: captured into the dedicated
        // field and NOT surfaced as a user output parameter — mirroring
        // msodbcsql, which routes it to hPrepCurrent, not the output-param path.
        client.push_return_value(int_return_value(0, 0x0102_0304));

        assert_eq!(client.prepared_statement_handle, Some(0x0102_0304));
        assert!(!client.expecting_prepare_handle, "flag must be one-shot");
        assert!(
            client.return_values.is_empty(),
            "the internal handle must not appear in return_values"
        );
        assert!(client.get_return_values().is_empty());

        // Subsequent values are genuine output params and must be surfaced.
        client.push_return_value(int_return_value(1, 7));
        assert_eq!(client.return_values.len(), 1);
        assert!(matches!(
            client.return_values[0].value,
            ColumnValues::Int(7)
        ));

        // The captured handle stays retrievable independently of return_values.
        assert_eq!(client.take_prepared_statement_handle(), Some(0x0102_0304));
    }

    #[tokio::test]
    async fn execute_sp_execute_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_sp_execute(42, None, None, None, None).await
        })
        .await;
    }

    #[tokio::test]
    async fn get_dtc_address_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| c.get_dtc_address().await)
            .await;
    }

    #[test]
    fn effective_ce_setting_resolves_against_connection() {
        use crate::connection::client_context::{
            ColumnEncryptionSetting, ExecutionColumnEncryptionSetting as S,
        };

        let mut client = create_test_client();

        // Connection is Disabled by default: UseConnectionSetting -> Disabled.
        client.current_command_ce_setting = S::UseConnectionSetting;
        assert_eq!(client.effective_command_ce_setting(), S::Disabled);

        // Explicit per-command settings pass through unchanged.
        for setting in [S::Enabled, S::ResultSetOnly, S::Disabled] {
            client.current_command_ce_setting = setting;
            assert_eq!(client.effective_command_ce_setting(), setting);
        }

        // With the connection enabled, UseConnectionSetting -> Enabled.
        if let Some(ctx) = client.recovery_context.client_context.as_mut() {
            ctx.column_encryption_setting = ColumnEncryptionSetting::Enabled;
        }
        client.current_command_ce_setting = S::UseConnectionSetting;
        assert_eq!(client.effective_command_ce_setting(), S::Enabled);
    }

    #[test]
    fn should_not_encrypt_when_feature_not_acknowledged() {
        use crate::connection::client_context::{
            ColumnEncryptionSetting, ExecutionColumnEncryptionSetting as S,
        };

        let mut client = create_test_client();
        if let Some(ctx) = client.recovery_context.client_context.as_mut() {
            ctx.column_encryption_setting = ColumnEncryptionSetting::Enabled;
        }
        // The test negotiated settings carry no acknowledged TCE feature, so
        // parameter encryption must not be attempted even when enabled.
        client.current_command_ce_setting = S::Enabled;
        assert!(!client.should_encrypt_parameters());
    }

    #[test]
    fn build_sp_describe_request_names_params_and_marks_output() {
        use crate::datatypes::sqltypes::SqlType;
        use crate::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let params = vec![
            RpcParameter::new(
                Some("@id".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(1)),
            ),
            RpcParameter::new(
                Some("@count".to_string()),
                StatusFlags::BY_REF_VALUE,
                SqlType::BigInt(None),
            ),
        ];

        let (tsql, params_decl) =
            TdsClient::build_stored_procedure_describe_request("dbo.my_proc", &[], &params)
                .expect("building the describe request should succeed");

        assert_eq!(tsql, "EXEC dbo.my_proc @id=@id, @count=@count OUTPUT");
        assert_eq!(params_decl, "@id int, @count bigint OUTPUT");
    }

    #[test]
    fn build_sp_describe_request_positional_and_named() {
        use crate::datatypes::sqltypes::SqlType;
        use crate::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        // Positional (unnamed) parameters get synthetic names bound by position
        // and precede the named parameters, which bind by name.
        let positional = vec![
            RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(1))),
            RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::BigInt(None)),
        ];
        let named = vec![RpcParameter::new(
            Some("@b".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(3)),
        )];

        let (tsql, params_decl) =
            TdsClient::build_stored_procedure_describe_request("proc", &positional, &named)
                .expect("building the describe request should succeed");

        assert_eq!(tsql, "EXEC proc @ce_pos_0, @ce_pos_1 OUTPUT, @b=@b");
        assert_eq!(
            params_decl,
            "@ce_pos_0 int, @ce_pos_1 bigint OUTPUT, @b int"
        );
    }

    #[test]
    fn build_sp_describe_request_rejects_synthetic_named_collision() {
        use crate::datatypes::sqltypes::SqlType;
        use crate::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

        let positional = vec![RpcParameter::new(
            None,
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        )];
        let named = vec![RpcParameter::new(
            Some("@CE_POS_0".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(2)),
        )];

        let err = TdsClient::build_stored_procedure_describe_request("proc", &positional, &named)
            .expect_err("synthetic positional name collision should be rejected");

        assert!(matches!(err, UsageError(message) if message.contains("@CE_POS_0")));
    }

    /// A forced parameter whose row the server omits entirely from the describe
    /// result must be rejected — not silently sent as plaintext. This is the
    /// downgrade a describe-driven check (iterating only the server's rows) would
    /// miss, so the validation iterates the supplied forced parameters instead.
    #[tokio::test]
    async fn apply_parameter_encryption_rejects_forced_param_omitted_from_describe() {
        use crate::datatypes::sqltypes::SqlType;
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;
        use crate::security::keystore::{CekCache, ColumnEncryptionKeyStoreProviderRegistry};

        // Server returns an empty describe result: no row for the forced param.
        let describe = DescribeParameterEncryptionResult::new();
        let providers = ColumnEncryptionKeyStoreProviderRegistry::new();
        let cek_cache = CekCache::new();
        let mut output_param_ceks = HashMap::new();

        let mut forced = RpcParameter::new(
            Some("@p1".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(42)),
        )
        .with_force_column_encryption(true);
        let mut params: Vec<&mut RpcParameter> = vec![&mut forced];

        let err = TdsClient::apply_parameter_encryption(
            &describe,
            &providers,
            &cek_cache,
            &mut params,
            &mut output_param_ceks,
            &[],
        )
        .await
        .expect_err("a forced parameter omitted from the describe result must be rejected");

        assert!(
            matches!(&err, crate::error::Error::ColumnEncryptionError(message) if message.contains("ForceColumnEncryption")),
            "expected a ForceColumnEncryption column-encryption error, got: {err}"
        );
    }
}
