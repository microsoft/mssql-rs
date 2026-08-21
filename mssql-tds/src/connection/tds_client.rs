// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::bulk_copy::{BulkCopyOptions, BulkLoadRow, ResolvedColumnMapping};
use crate::connection::bulk_copy_state::ATTENTION_TIMEOUT_SECONDS;
use crate::connection::client_context::{ClientContext, ExecutionColumnEncryptionSetting};
use crate::connection::session_recovery::RecoveryContext;
use crate::connection::transport::any_transport::AnyTransport;
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use crate::datatypes::encoder::GenericEncoder;
use crate::datatypes::row_writer::{DefaultRowWriter, DiscardRowWriter, RowWriter};
use crate::datatypes::sql_string::SqlString;
use crate::datatypes::sqldatatypes::TdsDataType;
use crate::datatypes::sqltypes::SqlType;
use crate::datatypes::tds_value_serializer::{PLP_NULL, PLP_TERMINATOR, PLP_UNKNOWN_LEN};
use crate::error::Error::UsageError;
use crate::error::{SqlErrorInfo, SqlInfoMessage};
use crate::io::packet_writer::{PacketWriter, SuspendedMessage, TdsPacketWriter};
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
use crate::token::tokens::{SessionStateToken, SqlCollation};
use crate::{
    connection::execution_context::{ALREADY_EXECUTING_ERROR, ExecutionContext},
    datatypes::column_values::ColumnValues,
    handler::handler_factory::NegotiatedSettings,
    io::token_stream::{
        ColumnPolicy, ParserContext, PlpPauseState, RowHeader, RowPauseState, RowReadResult,
    },
    message::{batch::SqlBatch, messages::Request},
    token::tokens::{ColMetadataToken, CurrentCommand, DoneStatus, EnvChangeTokenSubType, Tokens},
};
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU32;
use tracing::{debug, error, info, instrument, warn};

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

/// Server error severity/class at or above which the connection is fatally
/// broken and must not be reused. Mirrors msodbcsql's `MINFATALERR`: a token
/// with `severity >= 20` marks the transport dead so a pool checkout discards
/// it, even if the socket is still readable.
const FATAL_ERROR_SEVERITY: u8 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connection) enum CommandTimeoutBudget {
    None,
    Remaining(NonZeroU32),
    Exhausted,
}

/// A command timeout resolved after connection recovery, ready for request I/O.
///
/// Holds one `Option<NonZeroU32>` (niche-optimized to a bare `u32`) so the
/// seconds and the deadline it derives can never disagree; `None` is no
/// deadline (infinite).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connection) struct ResolvedCommandTimeout {
    seconds: Option<NonZeroU32>,
}

impl ResolvedCommandTimeout {
    /// The timeout in whole seconds, or `None` for no deadline.
    pub(in crate::connection) fn seconds(self) -> Option<u32> {
        self.seconds.map(NonZeroU32::get)
    }

    /// The timeout as a [`Duration`] deadline, or `None` for no deadline.
    pub(in crate::connection) fn duration(self) -> Option<Duration> {
        self.seconds
            .map(|seconds| Duration::from_secs(u64::from(seconds.get())))
    }
}

impl CommandTimeoutBudget {
    /// Resolves the post-recovery budget into the timeout used by request I/O.
    ///
    /// `None`/`Remaining` become no-deadline / the remaining positive seconds;
    /// `Exhausted` is rejected here so a spent budget can never enter the
    /// `0 == infinite` path.
    ///
    /// # Errors
    /// Returns [`TimeoutError`](crate::error::Error::TimeoutError) when recovery
    /// consumed the whole command-timeout budget.
    pub(in crate::connection) fn into_timeout(self) -> TdsResult<ResolvedCommandTimeout> {
        match self {
            Self::None => Ok(ResolvedCommandTimeout { seconds: None }),
            Self::Remaining(seconds) => Ok(ResolvedCommandTimeout {
                seconds: Some(seconds),
            }),
            Self::Exhausted => Err(crate::error::Error::TimeoutError(
                crate::error::TimeoutErrorType::String("command timeout exhausted".to_string()),
            )),
        }
    }
}

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

#[derive(Debug)]
enum ActiveRowReadState {
    /// No row is positioned: either nothing has been fetched yet or the result
    /// set is exhausted. `read_row_column` reports `RowEnded` here.
    Idle,
    /// A row is positioned and partially read; `next_column_index` columns have
    /// been consumed and more remain on the wire.
    RowPaused(Box<RowPauseState>),
    /// A row is positioned with an active PLP column stream mid-flight.
    PlpPaused(Box<PlpPauseState>),
}

/// One chunk of an active PLP (partially-length-prefixed) column stream,
/// returned by [`TdsClient::read_active_plp_chunk`].
///
/// Carries both facts a streaming consumer needs after each read: how many
/// bytes landed in the output buffer, and whether the stream is now finished.
/// Combining them avoids a second `&self` call and keeps the "reached end"
/// answer consistent with the read that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlpChunk {
    /// Number of bytes written into the caller's buffer by this read.
    pub read: usize,
    /// `true` once the PLP stream has been fully consumed; the caller should
    /// stop reading and resume the row cursor.
    pub reached_end: bool,
    /// Declared total length of the whole PLP value in wire bytes when the
    /// server sent a known-length PLP header; `None` for unknown-length
    /// (streamed) PLP whose total is not known in advance. Lets a consumer
    /// report the concrete bytes-remaining indicator instead of `SQL_NO_TOTAL`.
    pub known_total: Option<u64>,
    /// Cumulative wire bytes consumed from this PLP value across all chunks,
    /// including this read. With `known_total`, the bytes still available
    /// before this call are `known_total - (total_read - read)`.
    pub total_read: usize,
}

/// Outcome of [`TdsClient::read_row_column`], the ODBC pull-cursor column fetch.
#[derive(Debug, PartialEq)]
pub enum CursorColumn {
    /// A fully decoded, materialized column value (non-PLP).
    Value {
        /// The decoded value.
        value: ColumnValues,
        /// Base type declared by a `sql_variant` column, `None` otherwise. The
        /// decoded value cannot always recover it, since `varchar` and
        /// `nvarchar` both arrive as [`ColumnValues::String`].
        variant_base: Option<TdsDataType>,
    },
    /// `target` is a PLP column; its bytes are streamed via
    /// [`TdsClient::read_active_plp_chunk`] until
    /// [`PlpChunk::reached_end`] is `true`.
    PlpStreaming {
        /// Collation for the whole stream, fixed for its lifetime. `None` for
        /// binary PLP types, so callers do not need a separate lookup per chunk.
        collation: Option<SqlCollation>,
    },
    /// `target` was already read or skipped (forward-only violation). The
    /// cursor is left positioned on its next undecoded column.
    AlreadyConsumed,
    /// No row is positioned (cursor is idle / row already fully consumed).
    RowEnded,
}

/// Where a connection stands in the RESETCONNECTION handshake (MS-TDS
/// 2.2.3.1.2). The bit is armed on the connection and consumed by the packet
/// writer, so confirming that the server honored it needs all three states
/// rather than a single "pending" flag: only [`AwaitingAck`](Self::AwaitingAck)
/// means an acknowledgement is genuinely owed and its absence is a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetAckState {
    /// No reset armed or outstanding.
    Idle,
    /// Armed on the writer; no request has carried the bit to the server yet.
    Armed,
    /// A request carried the bit; its response must contain the
    /// `ResetConnection` ENVCHANGE.
    AwaitingAck,
}

/// Result of beginning or continuing a streamed PLP parameter write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamedParamStatus {
    /// The server still expects a streamed parameter value. Stream its chunks
    /// next via [`TdsClient::write_streamed_chunk`], then call
    /// [`TdsClient::end_streamed_param`].
    NeedData {
        /// Name of the streamed parameter now awaiting its value chunks.
        param_name: String,
    },
    /// All streamed parameters have been written, and the server response has
    /// been positioned exactly as after a normal execute call.
    Complete(StatementResult),
}

/// State machine for an in-progress incremental (streamed) PLP parameter write,
/// parked as owned state on the client between calls (mirrors the read side's
/// [`ActiveRowReadState`]). `Active` holds the suspended RPC message plus the
/// streamed parameters whose headers have not yet been emitted.
#[derive(Debug)]
enum StreamedWriteState {
    Idle,
    Active(Box<StreamedWriteContext>),
}

/// Owned context for a suspended, partially-written streamed RPC.
#[derive(Debug)]
struct StreamedWriteContext {
    /// The parked outgoing RPC message. The parameter currently open for data
    /// has had its header (status byte + `TYPE_INFO`) written; the PLP length
    /// field (the unknown-length opener, or `PLP_NULL`) and the value chunks are
    /// appended afterwards by the streaming driver.
    message: SuspendedMessage,
    /// Streamed parameters whose headers have not yet been written, in order.
    pending: std::collections::VecDeque<RpcParameter>,
    /// Collation used to write `TYPE_INFO` for subsequent streamed parameters.
    db_collation: SqlCollation,
    /// `true` once the unknown-length PLP opener (`PLP_UNKNOWN_LEN`) has been
    /// written for the currently-open parameter — i.e. at least one value chunk
    /// has been emitted. The opener is written lazily (on the first chunk) so a
    /// parameter can still resolve to NULL before any data is sent.
    value_opened: bool,
    /// `true` when the caller has signalled the currently-open parameter is NULL
    /// via [`TdsClient::write_streamed_null`]. Closing the parameter then writes
    /// `PLP_NULL` instead of an opener + terminator, and no chunks may follow.
    null_signaled: bool,
    /// Configured timeout reused for each resumed streaming operation.
    timeout_sec: Option<u32>,
}

/// Active TDS connection to a SQL Server instance.
///
/// Created by [`TdsConnectionProvider::create_client()`](crate::connection_provider::tds_connection_provider::TdsConnectionProvider::create_client).
/// Provides methods for executing queries, managing transactions, and bulk copy.
#[derive(Debug)]
pub struct TdsClient {
    pub(crate) transport: AnyTransport,
    pub(crate) negotiated_settings: NegotiatedSettings,
    pub(crate) execution_context: ExecutionContext,
    pub(crate) recovery_context: Box<RecoveryContext>,
    /// Where this connection stands in the RESETCONNECTION handshake. Driven by
    /// [`prepare_reset_connection`](TdsClient::prepare_reset_connection),
    /// [`observe_response_token`](TdsClient::observe_response_token), and
    /// [`on_reset_connection_ack`](TdsClient::on_reset_connection_ack).
    reset_state: ResetAckState,

    // pub(crate) batch_result: Option<BatchResult<'static>>,
    pub(crate) current_metadata: Option<Arc<ColMetadataToken>>,
    /// Memoized cell decryptor for `current_metadata`'s CEK table, paired with
    /// the metadata it was built for so it is rebuilt when the result set
    /// changes. `None` until the first encrypted result set is seen.
    current_decryptor: Option<MemoizedCellDecryptor>,
    count_map: HashMap<CurrentCommand, u64>,
    /// Rows affected by the most recent statement; see [`last_rows_affected`](Self::last_rows_affected).
    last_rows_affected: i64,
    /// Per-statement affected-row counts captured (in order) from every counted
    /// DONE token seen since the last COLMETADATA or command start. For a
    /// pure-DML batch (`UPDATE; DELETE; INSERT`) this holds one entry per
    /// statement so the ODBC layer can surface each as its own result set via
    /// [`take_dml_result_counts`](Self::take_dml_result_counts).
    dml_result_counts: Vec<i64>,

    pub(in crate::connection) return_values: Vec<ReturnValue>,
    info_messages: Vec<SqlInfoMessage>,
    /// Per-statement Always Encrypted parameter metadata, keyed by the same
    /// client-issued [`StatementId`] as `prepared_handles`. Captured by
    /// `execute_sp_prepare` / `sp_prepexec` from
    /// `sp_describe_parameter_encryption` and reused by `execute_sp_execute` to
    /// encrypt parameter values without describing again. Holds an `Arc` to the
    /// same describe result stored in `query_metadata_cache`, pinning it for the
    /// statement's lifetime even if the shared cache evicts it. Evicted when the
    /// statement is unprepared or superseded, and cleared on reconnect.
    prepared_param_encryption: HashMap<
        StatementId,
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

    /// The [`StatementId`] awaiting its `@handle` from an in-flight managed
    /// `sp_prepexec` (armed by [`execute_prepared`](Self::execute_prepared));
    /// `None` once captured, aborted, or on the raw protocol path (sp_* RPCs directly,
    /// where `@handle` is left in `return_values` like any other output parameter).
    pending_capture: Option<StatementId>,
    /// Always Encrypted metadata produced for the in-flight `sp_prepexec`,
    /// pinned under the statement's id when its handle is captured from the
    /// token stream.
    pending_prepared_param_encryption: Option<
        Arc<crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult>,
    >,

    /// Monotonic source of [`StatementId`]s for managed prepared statements.
    /// Never reset (including across reconnect) so an id is never reused within
    /// this client's lifetime.
    next_statement_id: u64,
    /// Live server handles of prepared statements, keyed by their client-issued
    /// [`StatementId`]. Owned by the sp_* RPCs: written by `execute_sp_prepare`
    /// and by `push_return_value` when the `sp_prepexec` `@handle` RETURNVALUE
    /// lands, removed by `execute_sp_unprepare`. Cleared on reconnect — the old
    /// session's handles are gone — so "stale" collapses to "absent from the
    /// map".
    prepared_handles: HashMap<StatementId, i32>,

    /// The remaining request timeout for operations. This is updated after each token read.
    pub(in crate::connection) remaining_request_timeout: Option<Duration>,

    /// The cancel handle for this client. Used to cancel operations.
    pub(in crate::connection) cancel_handle: Option<CancelHandle>,

    /// Empty metadata vector for returning when no metadata is available
    empty_metadata: Vec<ColumnMetadata>,

    // Active PLP stream state when row decoding paused at a PLP target column.
    active_row_read_state: ActiveRowReadState,

    /// Test-only: when `Some`, [`check_and_reconnect`](Self::check_and_reconnect)
    /// reports this as the elapsed recovery time and skips the transport, so
    /// budget-exhaustion paths are exercised without live reconnect timing.
    #[cfg(test)]
    reconnect_elapsed_for_test: Option<Duration>,

    // Active incremental (streamed) PLP parameter-write state, if a streamed
    // RPC is mid-flight. Idle for the normal atomic send path.
    streamed_write_state: StreamedWriteState,
}

impl TdsClient {
    pub(crate) fn new(
        transport: AnyTransport,
        negotiated_settings: NegotiatedSettings,
        execution_context: ExecutionContext,
        client_context: ClientContext,
        login_session_state_tokens: Vec<SessionStateToken>,
    ) -> Self {
        let mut recovery_context = RecoveryContext::new();
        recovery_context.initialize(
            client_context,
            &negotiated_settings,
            &login_session_state_tokens,
        );

        Self {
            transport,
            negotiated_settings,
            execution_context,
            recovery_context: Box::new(recovery_context),
            reset_state: ResetAckState::Idle,
            current_metadata: None,
            current_decryptor: None,
            count_map: HashMap::new(),
            last_rows_affected: -1,
            dml_result_counts: Vec::new(),
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
            pending_capture: None,
            pending_prepared_param_encryption: None,
            next_statement_id: 0,
            prepared_handles: HashMap::new(),
            remaining_request_timeout: None,
            cancel_handle: None,
            empty_metadata: Vec::new(),
            active_row_read_state: ActiveRowReadState::Idle,
            #[cfg(test)]
            reconnect_elapsed_for_test: None,
            streamed_write_state: StreamedWriteState::Idle,
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
                Ok((
                    new_transport,
                    new_settings,
                    new_exec_ctx,
                    info_messages,
                    session_state_tokens,
                )) => {
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

                    self.adopt_recovered_session(
                        new_transport,
                        new_settings,
                        new_exec_ctx,
                        info_messages,
                        &session_state_tokens,
                    )?;

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

    /// Adopts a freshly recovered session: swaps in the new transport, settings,
    /// and execution context, then drops everything bound to the dead one.
    ///
    /// Split out of [`reconnect`](Self::reconnect) so the invariants a recovered
    /// session must satisfy can be asserted without a live server.
    fn adopt_recovered_session(
        &mut self,
        transport: AnyTransport,
        settings: NegotiatedSettings,
        execution_context: ExecutionContext,
        info_messages: Vec<SqlInfoMessage>,
        session_state_tokens: &[crate::token::tokens::SessionStateToken],
    ) -> TdsResult<()> {
        self.transport = transport;
        self.negotiated_settings = settings;
        self.execution_context = execution_context;

        // Reset per-request state
        self.current_metadata = None;
        self.count_map.clear();
        self.return_values.clear();
        self.info_messages.clear();
        self.info_messages.extend(info_messages);
        // Prepared-statement handles belong to the dead session and do not
        // survive a reconnect, so drop them and their cached Always Encrypted
        // metadata.
        self.clear_session_bound_caches();
        self.current_result_set_has_been_read_till_end = false;
        self.remaining_request_timeout = None;
        self.cancel_handle = None;

        // Mirror msodbcsql's recovery-login handling (ReadUpdatedSessionState,
        // seq 0): the FEATUREEXTACK payload REPLACES the current (delta) table
        // (reset-then-reload), then any reconnect SESSIONSTATE tokens apply on
        // top. The initial baseline is untouched; both blocks serialize on the
        // next reconnect. Read from `negotiated_settings` *after* the swap above
        // so it is the recovered session's acknowledgement, not the dead one's.
        if let Some(ack) = self
            .negotiated_settings
            .session_recovery_initial_state()
            .map(|s| s.to_vec())
        {
            self.recovery_context
                .session_state_table
                .apply_feature_ack_to_delta(&ack);
        }
        for token in session_state_tokens {
            self.recovery_context.process_session_state(token)?;
        }

        // A pending reset is satisfied — more completely than RESETCONNECTION
        // would — by the fresh login: the new session is already at its login
        // defaults, and the armed header bit died with the old transport, so no
        // acknowledgement is coming. Leaving the state set would make the next
        // response report the reset as unacknowledged and discard a healthy,
        // brand-new connection.
        self.reset_state = ResetAckState::Idle;
        Ok(())
    }

    /// Clears the caches bound to a specific server session: managed
    /// prepared-statement handles, their cached Always Encrypted describe
    /// metadata, and any in-flight `sp_prepexec` capture state. Shared by the
    /// reconnect path (the old session's handles are gone) and the
    /// connection-reset path (`sp_reset_connection` drops server-side prepared
    /// handles), so a later `execute_prepared` re-prepares against the live
    /// session instead of aliasing a stale or unrelated handle.
    fn clear_session_bound_caches(&mut self) {
        self.prepared_handles.clear();
        self.prepared_param_encryption.clear();
        self.pending_capture = None;
        self.pending_prepared_param_encryption = None;
    }

    /// Confirms a reset and reconciles client state with the reset server session.
    ///
    /// `prepare_reset_connection` performs the same invalidation at arm time so
    /// the carrying request cannot use stale state. Repeating it here is
    /// intentional: this is the protocol-level transition shared by every token
    /// processing path, including resets not coordinated by the ODBC checkout
    /// flow.
    fn on_reset_connection_ack(&mut self) {
        self.reset_state = ResetAckState::Idle;
        self.recovery_context.session_state_table.reset();
        self.clear_session_bound_caches();
        self.negotiated_settings.restore_login_defaults();
    }

    /// Advances reset-acknowledgement tracking for one token just read from a
    /// response, and fails the connection when the acknowledgement is provably
    /// missing.
    ///
    /// The packet writer — not this client — consumes the armed mode, so the
    /// only way to learn that the bit truly reached the wire is to ask the
    /// writer. That is done solely while this client still believes a reset is
    /// armed, so a dispatch record left behind by a response that was never
    /// read cannot be charged to an unrelated later request.
    ///
    /// SQL Server resets the session *before* it parses the carrying request,
    /// so the `ResetConnection` ENVCHANGE precedes everything that request
    /// produces. The first token that is evidence the request actually ran is
    /// therefore the last moment the acknowledgement could still have arrived,
    /// and its absence means the session was never reset. ERROR is exempt so a
    /// server diagnostic reaches the caller instead of being masked by this
    /// one; a reset still unacknowledged after it is caught on the next token.
    fn observe_response_token(&mut self, token: &Tokens) -> TdsResult<()> {
        if self.reset_state == ResetAckState::Armed
            && self.transport.as_writer().take_reset_dispatched()
        {
            self.reset_state = ResetAckState::AwaitingAck;
        }

        if self.reset_state != ResetAckState::AwaitingAck || !Self::proves_request_ran(token) {
            return Ok(());
        }

        warn!(
            token = Self::token_variant_name(token),
            "Request carrying the RESETCONNECTION bit produced output without a ResetConnection \
             ENVCHANGE; marking connection dead"
        );
        // Clearing first keeps the failure to a single report: the caller's
        // error drain reads more tokens on the same dead connection.
        self.reset_state = ResetAckState::Idle;
        self.transport.mark_known_dead();
        Err(crate::error::Error::ConnectionResetNotAcknowledged)
    }

    /// The token's variant name, for diagnostics. Deliberately *not* `?token`:
    /// the tokens that reach the failure log are typically ROW or RETURNVALUE,
    /// whose Debug output is customer row and output-parameter data. The kind is
    /// the whole diagnostic value here; the payload is not ours to log.
    fn token_variant_name(token: &Tokens) -> &'static str {
        match token {
            Tokens::Done(_) => "Done",
            Tokens::DoneInProc(_) => "DoneInProc",
            Tokens::DoneProc(_) => "DoneProc",
            Tokens::EnvChange(_) => "EnvChange",
            Tokens::Error(_) => "Error",
            Tokens::Info(_) => "Info",
            Tokens::LoginAck(_) => "LoginAck",
            Tokens::FeatureExtAck(_) => "FeatureExtAck",
            Tokens::FedAuthInfo(_) => "FedAuthInfo",
            Tokens::SessionState(_) => "SessionState",
            Tokens::Sspi(_) => "Sspi",
            Tokens::Row(_) => "Row",
            Tokens::ColMetadata(_) => "ColMetadata",
            Tokens::Order(_) => "Order",
            Tokens::ReturnStatus(_) => "ReturnStatus",
            Tokens::ReturnValue(_) => "ReturnValue",
            Tokens::TabName => "TabName",
            Tokens::ColInfo => "ColInfo",
        }
    }

    /// Whether a token proves the server started executing the request, and so
    /// is past the point where a `ResetConnection` ENVCHANGE could still
    /// arrive. ENVCHANGE, INFO, and SESSIONSTATE can legitimately precede or
    /// accompany the acknowledgement; ERROR is treated as inconclusive so the
    /// server's own diagnostic wins.
    fn proves_request_ran(token: &Tokens) -> bool {
        !matches!(
            token,
            Tokens::EnvChange(_) | Tokens::Info(_) | Tokens::SessionState(_) | Tokens::Error(_)
        )
    }

    /// Settles reset tracking left over from an earlier request whose response
    /// was abandoned before any verdict could be reached.
    ///
    /// Cancellation and timeout are the reachable cases. `receive_token`
    /// answers both by draining to the attention acknowledgement, and that
    /// drain discards every other token — the `ResetConnection` ENVCHANGE
    /// included. The carrying request therefore ends with the bit on the wire
    /// and nothing observed about it.
    ///
    /// This runs at the request boundary, before the current request has sent
    /// anything, so a dispatch record or outstanding acknowledgement seen here
    /// necessarily belongs to an earlier request. Carrying that suspicion
    /// forward would mark a healthy connection dead on the first token of a
    /// request that never had anything to do with the reset — the failure mode
    /// this verification exists to avoid causing.
    ///
    /// Called from both [`begin_command`](Self::begin_command) and
    /// [`check_and_reconnect`](Self::check_and_reconnect) because neither alone
    /// is universal: the cursor RPCs in `cursor_ops` never call `begin_command`,
    /// and the transaction ops deliberately skip recovery. Together they cover
    /// every request path. It is idempotent — once settled, the state is `Idle`
    /// and the dispatch record consumed — so reaching it twice is harmless.
    ///
    /// Settling as reset rather than as a failure is what the protocol
    /// supports: the server resets the session *before* it processes the
    /// carrying request, the bit demonstrably reached the wire, and
    /// `prepare_reset_connection` already reconciled every client-side cache at
    /// arm time. No pool guarantee is weakened either — an abandoned carrier
    /// fails its own request, so the checkout that issued it reports failure and
    /// discards the connection.
    fn settle_abandoned_reset_verification(&mut self) {
        let carried = self.transport.as_writer().take_reset_dispatched();
        let abandoned = match self.reset_state {
            ResetAckState::Idle => false,
            // Still `Armed` with the bit already sent: an earlier request
            // carried it and its response was abandoned before the first token.
            ResetAckState::Armed => carried,
            // The response ended without ever reaching a verdict token.
            ResetAckState::AwaitingAck => true,
        };
        if !abandoned {
            return;
        }
        warn!(
            "A request carrying the RESETCONNECTION bit was abandoned before its acknowledgement \
             could be observed; treating the session as reset rather than charging the next request"
        );
        self.reset_state = ResetAckState::Idle;
    }

    /// True when a RESETCONNECTION bit has been armed but the server has not yet
    /// acknowledged it with a `ResetConnection` ENVCHANGE.
    ///
    /// The client verifies this itself: the response to the request that carried
    /// the bit must contain the ENVCHANGE, or the connection is marked dead and
    /// the request fails with
    /// [`Error::ConnectionResetNotAcknowledged`](crate::error::Error::ConnectionResetNotAcknowledged).
    /// This is exposed for observability and for a pool that wants to assert the
    /// reset has been settled; correctness does not depend on anyone calling it.
    pub fn reset_pending(&self) -> bool {
        self.reset_state != ResetAckState::Idle
    }

    /// Converts a server `ERROR` token into [`SqlErrorInfo`], marking the
    /// transport known-dead when the error is fatal (severity/class ≥
    /// [`FATAL_ERROR_SEVERITY`]). A fatal error breaks the session even when the
    /// socket is still readable, so flagging it here lets a later pool checkout
    /// discard the connection instead of handing back an unusable one.
    fn record_error_token(
        &mut self,
        error_token: &crate::token::tokens::ErrorToken,
    ) -> SqlErrorInfo {
        if error_token.severity >= FATAL_ERROR_SEVERITY {
            warn!(
                severity = error_token.severity,
                number = error_token.number,
                "Fatal server error token received; marking connection dead"
            );
            self.transport.mark_known_dead();
        }
        SqlErrorInfo::from(error_token)
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

    /// Returns the SQL Server version reported in the `LOGINACK` token, if the
    /// server sent one during login.
    pub fn server_version(&self) -> Option<crate::core::Version> {
        self.negotiated_settings.login_ack_server_version
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

    /// Marks the connection known-dead so [`is_connection_dead`] reports it
    /// without touching the socket. A caller that observes the session in an
    /// unrecoverable state — e.g. a connection-pool reset that failed partway —
    /// uses this so a later liveness check discards the connection instead of
    /// handing back a session left in an unknown state.
    ///
    /// [`is_connection_dead`]: Self::is_connection_dead
    pub fn mark_connection_dead(&mut self) {
        self.transport.mark_known_dead();
    }

    pub(crate) fn get_current_metadata(&self) -> Option<&ColMetadataToken> {
        self.current_metadata.as_deref()
    }

    /// Rows affected by the most recently executed statement.
    ///
    /// Returns the row count from the last DONE token that carried the
    /// `DONE_COUNT` flag, or `-1` when no count is available (DDL,
    /// `SET NOCOUNT ON`, a forward-only SELECT whose trailing DONE has not been
    /// read, or before any statement has executed). This maps directly to the
    /// value ODBC `SQLRowCount` reports.
    pub fn last_rows_affected(&self) -> i64 {
        self.last_rows_affected
    }

    /// Drains the per-statement affected-row counts captured since the last
    /// COLMETADATA (or command start), in statement order. Used by the ODBC
    /// layer to surface each DML statement in a pure-DML batch as its own
    /// result set. Returns an empty vec when the batch produced no counted DONE
    /// (e.g. DDL, `SET NOCOUNT ON`).
    pub fn take_dml_result_counts(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.dml_result_counts)
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
    /// Call this at the top of any operation that sends a TDS request (SQL
    /// batch, RPC, bulk load, `BEGIN TRANSACTION`). Recovery is attempted only
    /// when all of the following hold; otherwise this is a cheap no-op that
    /// returns `Duration::ZERO`:
    /// - session recovery was negotiated at login, and the server offered a
    ///   retry count (`connect_retry_count > 0`); and
    /// - the underlying socket is actually dead (a non-blocking poll).
    ///
    /// `timeout_sec` is the overall budget for recovery **and** the subsequent
    /// execution, matching ODBC's `CheckOrRecoverConnection` (which deducts
    /// recovery time from the remaining command timeout). Charge the returned
    /// duration back with [`deduct_timeout`](Self::deduct_timeout) so a
    /// 30-second command timeout still means at most 30 seconds total whether
    /// or not a reconnect occurred. When `timeout_sec` is `None` or `Some(0)`,
    /// recovery is instead bounded by the login `connect_timeout`, so it can
    /// never block indefinitely.
    ///
    /// Operations inside an active transaction (`COMMIT`, `ROLLBACK`, `SAVE`)
    /// intentionally skip recovery — `is_recovery_possible()` returns `false`
    /// while a transaction is open, matching SqlClient's
    /// `RestoreBrokenConnection` behavior.
    ///
    /// A caller managing prepared statements uses this as its single recovery
    /// point: a successful reconnect clears the client's prepared-handle map, so
    /// the next [`execute_prepared`](Self::execute_prepared) re-prepares against
    /// the new session instead of addressing a plan that died with the old one.
    ///
    /// # Parameters
    /// - `timeout_sec`: recovery budget in seconds; `None` (or `Some(0)`) falls
    ///   back to the connection's `connect_timeout`.
    /// - `cancel_handle`: optional token used to abort an in-progress reconnect.
    ///
    /// # Returns
    /// The wall-clock time spent reconnecting, or `Duration::ZERO` when no
    /// reconnect was needed.
    ///
    /// # Errors
    /// Returns [`Error::ConnectionClosed`](crate::error::Error::ConnectionClosed)
    /// when the socket is dead but the session state forbids recovery (e.g. an
    /// open transaction), and propagates any error raised by the underlying
    /// `reconnect()` attempt (including a timed-out or cancelled reconnect).
    pub(crate) async fn check_and_reconnect(
        &mut self,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Duration> {
        // Request boundary. `begin_command` misses the cursor RPCs (they never
        // call it) and this misses the transaction ops (they deliberately skip
        // recovery), so the settle lives in both; it is idempotent, so a path
        // that reaches it twice costs nothing.
        self.settle_abandoned_reset_verification();

        #[cfg(test)]
        if let Some(elapsed) = self.reconnect_elapsed_for_test.take() {
            return Ok(elapsed);
        }

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

    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub async fn check_and_reconnect_for_test(
        &mut self,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Duration> {
        self.check_and_reconnect(timeout_sec, cancel_handle).await
    }

    /// Charges elapsed recovery time against a command-timeout budget.
    ///
    /// # Parameters
    /// - `timeout_sec`: remaining command budget in seconds; `None` means "no
    ///   timeout".
    /// - `elapsed`: time already consumed.
    ///
    /// `None` and caller-supplied `Some(0)` both represent an infinite budget.
    /// A positive timeout becomes [`CommandTimeoutBudget::Exhausted`] when
    /// recovery consumes it, which must be rejected by `into_timeout` before a
    /// request is serialized.
    pub(in crate::connection) fn deduct_timeout(
        timeout_sec: Option<u32>,
        elapsed: Duration,
    ) -> CommandTimeoutBudget {
        let Some(timeout_sec) = timeout_sec.and_then(NonZeroU32::new) else {
            return CommandTimeoutBudget::None;
        };
        let elapsed_secs = u32::try_from(
            elapsed
                .as_secs()
                .saturating_add(u64::from(elapsed.subsec_nanos() > 0)),
        )
        .unwrap_or(u32::MAX);

        match NonZeroU32::new(timeout_sec.get().saturating_sub(elapsed_secs)) {
            Some(remaining) => CommandTimeoutBudget::Remaining(remaining),
            None => CommandTimeoutBudget::Exhausted,
        }
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
    /// - `preserve_transaction` — when `true`, use
    ///   RESETCONNECTIONSKIPTRAN so a local or enlisted/distributed transaction
    ///   survives. The ODBC pool always passes `false`; low-level TDS consumers
    ///   should pass `true` only when a transaction must outlive the reset.
    ///
    /// # Client-side state is dropped immediately
    /// The session-bound caches (prepared handles, their Always Encrypted
    /// describe metadata, in-flight capture state), the accumulated
    /// session-recovery state, and the negotiated DATABASE / LANGUAGE /
    /// COLLATION are reset here, at arm time, rather than when the
    /// acknowledgement arrives. The server *will* reset the session before it
    /// processes the next request, so continuing to serve a borrower from those
    /// caches in the meantime is never correct: a cached prepared handle would
    /// be sent on a request whose own header bit is about to drop it
    /// server-side, which is how msodbcsql produces "Could not find prepared
    /// statement with handle N" (native 8179) across a pool reset.
    ///
    /// Dropping them eagerly is conservative — the worst case is re-preparing a
    /// statement that a deferred reset had not yet invalidated — and it removes
    /// the window without costing a round trip. msodbcsql does the same for its
    /// cached database/language/collation (`sqlcmisc.cpp:2373-2461`); it simply
    /// does not extend it to prepared handles.
    ///
    /// The acknowledgement is verified by the client itself: the response to the
    /// request that carries the bit must contain the server's `ResetConnection`
    /// ENVCHANGE. If it does not, that request fails with
    /// [`Error::ConnectionResetNotAcknowledged`](crate::error::Error::ConnectionResetNotAcknowledged)
    /// and the connection is marked dead, so a pool cannot hand out a session
    /// that was never reset. Verification therefore does not depend on the
    /// consumer issuing any particular request after arming.
    pub fn prepare_reset_connection(&mut self, preserve_transaction: bool) {
        let mode = match preserve_transaction {
            true => ResetConnectionMode::ResetSkipTran,
            false => ResetConnectionMode::Reset,
        };
        self.transport.as_writer().set_reset_mode(mode);
        self.reset_state = ResetAckState::Armed;
        // A full reset discards the server-side transaction, so mirror that here.
        // Leaving a stale descriptor would make `has_active_transaction()` report
        // a transaction the carrying request is itself about to destroy, and the
        // next caller would send a TM rollback for it — which the server answers
        // with 3903 ("no corresponding BEGIN TRANSACTION"), failing the checkout.
        // RESETCONNECTIONSKIPTRAN deliberately preserves the transaction, so the
        // descriptor must survive in that mode.
        if !preserve_transaction {
            self.execution_context.set_transaction_descriptor(0);
        }
        // Mirror the reset the server will apply before the next request, so no
        // borrower is served from state the reset is about to invalidate.
        self.recovery_context.session_state_table.reset();
        self.clear_session_bound_caches();
        self.negotiated_settings.restore_login_defaults();
    }

    /// Executes a SQL batch and positions on its **first navigable result**,
    /// returning that result's [`StatementResult`].
    ///
    /// Navigation is statement-wise (lossless): a no-row statement that carries
    /// a row count or produced a message is surfaced as its own
    /// [`StatementResult::NoRows`]; a pure no-op statement (e.g. a bare
    /// `CREATE TABLE`) is collapsed. Advance through the rest of the batch with
    /// [`advance()`](Self::advance), or skip straight to row-returning result
    /// sets with [`advance_to_rows()`](Self::advance_to_rows).
    ///
    /// # Parameters
    /// - `sql_command` — raw T-SQL text to execute.
    /// - `options` — per-command [`ExecuteOptions`] (timeout, cancellation,
    ///   Always Encrypted override). Pass `()` for defaults.
    ///
    /// # Errors
    /// Returns [`UsageError`](crate::error::Error::UsageError) if a previous
    /// batch is still open.
    #[instrument(skip(self, options), level = "info")]
    pub async fn execute<'a>(
        &mut self,
        sql_command: String,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        self.send_query_batch(sql_command, options.into()).await?;
        let boundary = self.advance_to_result_boundary().await?;
        Ok(self.apply_result_boundary(boundary))
    }

    /// Runs the batch-execution prologue and sends a SQL batch to the wire:
    /// sets the per-command Always Encrypted setting, rejects a re-entrant call,
    /// reconnects if needed, stores the timeout / cancel handle, and serializes
    /// the batch. The caller then consumes the response via
    /// [`advance_to_result_boundary`](Self::advance_to_result_boundary).
    async fn send_query_batch(
        &mut self,
        sql_command: String,
        options: ExecuteOptions<'_>,
    ) -> TdsResult<()> {
        let ExecuteOptions {
            timeout,
            cancel,
            column_encryption,
        } = options;
        self.current_command_ce_setting = column_encryption;

        if self.command_is_busy() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout, cancel).await?;
        let budget = Self::deduct_timeout(timeout, reconnect_elapsed);
        let resolved = budget.into_timeout()?;
        let timeout = resolved.seconds();
        let request_timeout = resolved.duration();

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel.map(|handle| handle.child_handle());

        self.transport.reset_reader();
        let batch = SqlBatch::new(sql_command, &self.execution_context);
        let mut packet_writer =
            batch.create_packet_writer(self.transport.as_writer(), timeout, cancel);
        batch.serialize(&mut packet_writer).await?;
        Ok(())
    }

    /// Executes a parameterized query via `sp_executesql`, positioning on its
    /// first navigable result.
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
    /// - `options` — per-command [`ExecuteOptions`]; set
    ///   [`column_encryption`](ExecuteOptions::column_encryption) to override
    ///   Always Encrypted for this call. Pass `()` for defaults.
    #[instrument(skip(self, named_params, options), level = "info")]
    pub async fn execute_sp_executesql<'a>(
        &mut self,
        sql: String,
        mut named_params: Vec<RpcParameter>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();
        self.current_command_ce_setting = column_encryption;

        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };
        RpcParameter::reject_data_at_exec(named_params.iter())?;

        let declaration_params = named_params.clone();
        let mut params_list_as_string = String::new();
        build_parameter_list_string(&declaration_params, &mut params_list_as_string)?;

        // Always Encrypted: when the connection enabled column encryption and the
        // server acknowledged the feature, ask the server which parameters need
        // encryption and encrypt them in place before sending the real RPC.
        self.ensure_force_column_encryption_supported(named_params.iter())?;
        let (timeout_sec, request_timeout, database_collation) = self
            .prepare_sp_executesql_command(timeout_sec, cancel_handle)
            .await?;

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
            self.remaining_request_timeout = request_timeout;
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        }

        let rpc = self.build_sp_executesql_rpc(
            sql,
            named_params,
            params_list_as_string,
            &database_collation,
        );

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        rpc.serialize(&mut packet_writer).await?;

        self.position_on_first_result().await
    }

    /// Starts a parameterized `sp_executesql` whose MAX parameter values are
    /// supplied later, in chunks (data-at-execution).
    ///
    /// This is the additive streaming counterpart to
    /// [`TdsClient::execute_sp_executesql`]. Existing materialized-parameter
    /// callers should continue using that method; this method is for
    /// [`RpcParameter::data_at_exec`] parameters.
    ///
    /// Supported streamed types are listed by [`StreamedSqlType`]. Chunks are
    /// raw wire bytes and are written verbatim, so the caller owns the encoding:
    /// `nvarchar(max)` chunks must be UTF-16LE, and `varchar(max)`/
    /// `varbinary(max)` chunks their corresponding single-byte/binary
    /// representation. A zero-length stream is a present empty value; use
    /// [`TdsClient::write_streamed_null`] for SQL `NULL`.
    ///
    /// The method returns [`StreamedParamStatus::NeedData`] after the RPC
    /// prefix is parked. Supply zero or more chunks and call
    /// [`TdsClient::end_streamed_param`]. When all parameters are complete,
    /// the final server result is returned as
    /// [`StreamedParamStatus::Complete`].
    ///
    /// Streamed parameters must be named and must use one of the
    /// [`StreamedSqlType`] variants. Streaming is not supported when Always
    /// Encrypted is active.
    ///
    /// # Errors
    /// Returns a usage error for invalid streamed parameters or an active
    /// command, and returns transport/serialization errors from the request.
    pub async fn begin_sp_executesql<'a>(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StreamedParamStatus> {
        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();

        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        self.current_command_ce_setting = column_encryption;
        self.ensure_force_column_encryption_supported(named_params.iter())?;

        let (streamed_params, materialized_params) =
            Self::split_and_validate_streamed_params(named_params)?;

        if streamed_params.is_empty() {
            let result = self
                .execute_sp_executesql(
                    sql,
                    materialized_params,
                    ExecuteOptions {
                        timeout: timeout_sec,
                        cancel: cancel_handle,
                        column_encryption,
                    },
                )
                .await?;
            return Ok(StreamedParamStatus::Complete(result));
        }

        if self.should_encrypt_parameters() {
            return Err(UsageError(
                "Streamed PLP parameter writes are not supported with Always Encrypted."
                    .to_string(),
            ));
        }

        let mut declaration_params = materialized_params.clone();
        declaration_params.extend(streamed_params.iter().cloned());
        let mut params_list_as_string = String::new();
        build_parameter_list_string(&declaration_params, &mut params_list_as_string)?;

        let (timeout_sec, _request_timeout, database_collation) = self
            .prepare_sp_executesql_command(timeout_sec, cancel_handle)
            .await?;
        let rpc = self.build_sp_executesql_rpc(
            sql,
            materialized_params,
            params_list_as_string,
            &database_collation,
        );

        self.start_sp_executesql_streamed(
            rpc,
            streamed_params,
            timeout_sec,
            cancel_handle,
            database_collation,
        )
        .await
    }

    async fn prepare_sp_executesql_command(
        &mut self,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<(Option<u32>, Option<Duration>, SqlCollation)> {
        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let budget = Self::deduct_timeout(timeout_sec, reconnect_elapsed);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();

        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
        self.transport.reset_reader();

        Ok((
            timeout_sec,
            request_timeout,
            self.negotiated_settings.database_collation,
        ))
    }

    fn build_sp_executesql_rpc<'a>(
        &self,
        sql: String,
        named_params: Vec<RpcParameter>,
        params_list_as_string: String,
        database_collation: &'a SqlCollation,
    ) -> SqlRpc<'a> {
        let statement_parameter = RpcParameter::new(
            None,
            StatusFlags::NONE,
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql))),
        );

        let params_parameter = RpcParameter::new(
            None,
            StatusFlags::NONE,
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_list_as_string))),
        );

        let positional_parameters = Some(vec![statement_parameter, params_parameter]);

        SqlRpc::new(
            RpcType::ProcId(RpcProcs::ExecuteSql),
            positional_parameters,
            Some(named_params),
            database_collation,
            &self.execution_context,
        )
    }

    fn split_and_validate_streamed_params(
        named_params: Vec<RpcParameter>,
    ) -> TdsResult<(Vec<RpcParameter>, Vec<RpcParameter>)> {
        let (streamed_params, materialized_params): (Vec<_>, Vec<_>) = named_params
            .into_iter()
            .partition(RpcParameter::is_data_at_exec);

        for param in &streamed_params {
            if param.name.is_none() {
                return Err(UsageError("Streamed parameters must be named.".to_string()));
            }
        }

        Ok((streamed_params, materialized_params))
    }

    async fn start_sp_executesql_streamed(
        &mut self,
        rpc: SqlRpc<'_>,
        streamed_params: Vec<RpcParameter>,
        timeout_sec: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
        database_collation: SqlCollation,
    ) -> TdsResult<StreamedParamStatus> {
        let mut pending: std::collections::VecDeque<RpcParameter> =
            streamed_params.into_iter().collect();
        let first = pending
            .pop_front()
            .expect("streamed_params checked non-empty above");
        let first_name = first
            .name
            .clone()
            .expect("streamed parameter names validated above");

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        // Write the RPC prefix (headers, proc, positional + materialized named
        // params) then the first streamed parameter's header. The data-at-exec
        // branch of `serialize` writes name + status + TYPE_INFO and stops before
        // the value. The PLP opener is written lazily with the first chunk so the
        // parameter can still resolve to NULL.
        let serialization_result = async {
            rpc.serialize_prefix(&mut packet_writer).await?;
            first
                .serialize(
                    &mut packet_writer,
                    &database_collation,
                    false,
                    &GenericEncoder::new(),
                )
                .await
        }
        .await;
        if let Err(error) = serialization_result {
            drop(packet_writer);
            self.abort_streamed_write().await;
            return Err(error);
        }
        let message = packet_writer.suspend();

        self.streamed_write_state = StreamedWriteState::Active(Box::new(StreamedWriteContext {
            message,
            pending,
            db_collation: database_collation,
            value_opened: false,
            null_signaled: false,
            timeout_sec,
        }));

        Ok(StreamedParamStatus::NeedData {
            param_name: first_name,
        })
    }

    /// Appends one value chunk to the streamed parameter currently open for
    /// data. Call zero or more times between
    /// a [`StreamedParamStatus::NeedData`] from
    /// [`begin_sp_executesql`](Self::begin_sp_executesql) (or
    /// [`end_streamed_param`](Self::end_streamed_param)) and the matching
    /// [`end_streamed_param`](Self::end_streamed_param).
    ///
    /// Empty chunks are ignored: a zero-length PLP chunk header is the value
    /// terminator, so it must never be emitted mid-value.
    ///
    /// # Errors
    /// Returns a usage error if no streamed parameter is currently open, if the
    /// parameter was already marked NULL via
    /// [`write_streamed_null`](Self::write_streamed_null), or if `chunk` is longer
    /// than [`u32::MAX`] bytes (the PLP chunk-length field is 32-bit).
    pub async fn write_streamed_chunk(&mut self, chunk: &[u8]) -> TdsResult<()> {
        if matches!(self.streamed_write_state, StreamedWriteState::Idle) {
            return Err(UsageError(
                "write_streamed_chunk called with no active streamed parameter.".to_string(),
            ));
        }
        if chunk.len() > u32::MAX as usize {
            return Err(UsageError(format!(
                "Streamed PLP chunk length {} exceeds the maximum chunk size of {} bytes.",
                chunk.len(),
                u32::MAX
            )));
        }

        let ctx = match std::mem::replace(&mut self.streamed_write_state, StreamedWriteState::Idle)
        {
            StreamedWriteState::Active(ctx) => ctx,
            StreamedWriteState::Idle => {
                return Err(UsageError(
                    "write_streamed_chunk called with no active streamed parameter.".to_string(),
                ));
            }
        };
        let StreamedWriteContext {
            message,
            pending,
            db_collation,
            value_opened,
            null_signaled,
            timeout_sec,
        } = *ctx;
        self.remaining_request_timeout = timeout_sec.map(|s| Duration::from_secs(u64::from(s)));

        // A parameter already signalled NULL cannot carry data. Re-park the
        // (still-clean) message and reject; this is a caller sequencing error,
        // not a wire failure, so the stream stays usable. Checked before the
        // empty-chunk short-circuit below so `write_streamed_chunk(&[])` after
        // `write_streamed_null()` still errors instead of silently succeeding.
        if null_signaled {
            self.streamed_write_state =
                StreamedWriteState::Active(Box::new(StreamedWriteContext {
                    message,
                    pending,
                    db_collation,
                    value_opened,
                    null_signaled,
                    timeout_sec,
                }));
            return Err(UsageError(
                "write_streamed_chunk called after the parameter was marked NULL.".to_string(),
            ));
        }

        // Empty chunks are ignored: a zero-length PLP chunk header is the value
        // terminator, so it must never be emitted mid-value. Re-park the
        // (unchanged) message so a later chunk or the terminator continues
        // from the same point.
        if chunk.is_empty() {
            self.streamed_write_state =
                StreamedWriteState::Active(Box::new(StreamedWriteContext {
                    message,
                    pending,
                    db_collation,
                    value_opened,
                    null_signaled,
                    timeout_sec,
                }));
            return Ok(());
        }

        let mut packet_writer = PacketWriter::resume(message, self.transport.as_writer());
        let result = async {
            // Lazily write the unknown-length opener on the first chunk. Deferring
            // it (rather than emitting it at header time) is what lets a streamed
            // parameter still resolve to NULL before any data is sent.
            if !value_opened {
                packet_writer.write_u64_async(PLP_UNKNOWN_LEN).await?;
            }
            packet_writer.write_u32_async(chunk.len() as u32).await?;
            packet_writer.write_async(chunk).await
        }
        .await;
        let message = packet_writer.suspend();

        match result {
            Ok(()) => {
                // Chunk framed cleanly: re-park the message so the next chunk or
                // the terminator continues exactly where this one left off. The
                // opener is now on the wire, so the value is committed as present.
                self.streamed_write_state =
                    StreamedWriteState::Active(Box::new(StreamedWriteContext {
                        message,
                        pending,
                        db_collation,
                        value_opened: true,
                        null_signaled: false,
                        timeout_sec,
                    }));
                Ok(())
            }
            Err(e) => {
                // A partial value chunk is now on the wire, so this message can no
                // longer be continued safely. Drop it and abort the streamed
                // write rather than re-parking it as resumable.
                drop(message);
                self.abort_streamed_write().await;
                Err(e)
            }
        }
    }

    /// Aborts an in-progress streamed PLP write after a mid-stream failure.
    ///
    /// The half-written RPC message is dropped rather than re-parked, so a
    /// subsequent [`write_streamed_chunk`](Self::write_streamed_chunk) or
    /// [`end_streamed_param`](Self::end_streamed_param) fails cleanly (no active
    /// stream) instead of appending to a corrupt message. The transport is closed
    /// because RESETCONNECTION applies only to a subsequent request and cannot
    /// terminate this incomplete RPC.
    async fn abort_streamed_write(&mut self) {
        self.streamed_write_state = StreamedWriteState::Idle;
        self.execution_context.set_has_open_batch(false);
        self.transport.mark_known_dead();
        // TODO: Match msodbcsql's state-aware cancellation: discard an unsent
        // request locally, or send EOM | IGNORE and drain DONE after a partial send.
        if let Err(error) = self.transport.close_transport().await {
            warn!(%error, "Failed to close transport after streamed write abort");
        }
    }

    /// Cancels an in-progress streamed PLP write while the client is parked in
    /// [`StreamedParamStatus::NeedData`](StreamedParamStatus::NeedData),
    /// discarding the half-written RPC and closing the transport.
    ///
    /// This is the supported way for a caller to bail out of a streamed
    /// parameter sequence once [`begin_sp_executesql`](Self::begin_sp_executesql)
    /// has parked a message (for example, to service `SQLCancel` while an ODBC
    /// driver is between `SQLPutData` calls). After this returns, no streamed
    /// write is active, so a subsequent [`write_streamed_chunk`](Self::write_streamed_chunk),
    /// [`write_streamed_null`](Self::write_streamed_null), or
    /// [`end_streamed_param`](Self::end_streamed_param) fails with a usage
    /// error. Calling this with no streamed write active is a no-op.
    pub async fn cancel_streamed_write(&mut self) {
        if matches!(self.streamed_write_state, StreamedWriteState::Idle) {
            return;
        }
        self.abort_streamed_write().await;
    }

    /// Marks the streamed parameter currently open for data as SQL NULL.
    ///
    /// Call this instead of [`write_streamed_chunk`](Self::write_streamed_chunk)
    /// when a data-at-execution parameter resolves to NULL (the ODBC
    /// `SQLPutData(SQL_NULL_DATA)` case). No bytes are written now; when the
    /// parameter is closed with [`end_streamed_param`](Self::end_streamed_param)
    /// the driver emits `PLP_NULL` instead of an unknown-length opener +
    /// terminator. Mirrors msodbcsql, which writes `VARMAX_LENGTH_NULL` with no
    /// chunks for a DAE parameter that resolves to NULL.
    ///
    /// # Errors
    /// Returns a usage error if no streamed parameter is currently open, or if
    /// value chunks have already been written for this parameter (a value that
    /// has begun streaming cannot become NULL).
    pub fn write_streamed_null(&mut self) -> TdsResult<()> {
        let ctx = match std::mem::replace(&mut self.streamed_write_state, StreamedWriteState::Idle)
        {
            StreamedWriteState::Active(ctx) => ctx,
            StreamedWriteState::Idle => {
                return Err(UsageError(
                    "write_streamed_null called with no active streamed parameter.".to_string(),
                ));
            }
        };
        let StreamedWriteContext {
            message,
            pending,
            db_collation,
            value_opened,
            null_signaled: _,
            timeout_sec,
        } = *ctx;

        if value_opened {
            // Chunks are already on the wire; the value cannot become NULL. Re-park
            // the (still-clean) message — this is a caller sequencing error.
            self.streamed_write_state =
                StreamedWriteState::Active(Box::new(StreamedWriteContext {
                    message,
                    pending,
                    db_collation,
                    value_opened,
                    null_signaled: false,
                    timeout_sec,
                }));
            return Err(UsageError(
                "write_streamed_null called after value chunks were already written.".to_string(),
            ));
        }

        self.streamed_write_state = StreamedWriteState::Active(Box::new(StreamedWriteContext {
            message,
            pending,
            db_collation,
            value_opened: false,
            null_signaled: true,
            timeout_sec,
        }));
        Ok(())
    }

    /// Closes the streamed parameter currently open for data.
    ///
    /// The current parameter's value is closed on the wire:
    /// - if it was marked NULL via [`write_streamed_null`](Self::write_streamed_null),
    ///   `PLP_NULL` is written (no opener, no terminator);
    /// - if one or more chunks were written, the `PLP_TERMINATOR` is written; and
    /// - if neither (an untouched parameter), the unknown-length opener +
    ///   terminator are written, encoding a present, zero-length value.
    ///
    /// If more streamed parameters remain, the next one's header is written and
    /// [`StreamedParamStatus::NeedData`] is returned (stream its chunks next).
    /// When the last streamed parameter closes, the RPC message is finalized and
    /// sent, then the real server result is returned in
    /// [`StreamedParamStatus::Complete`].
    ///
    /// # Errors
    /// Returns a usage error if no streamed parameter is currently open, or an
    /// I/O error if sending fails.
    pub async fn end_streamed_param(&mut self) -> TdsResult<StreamedParamStatus> {
        let ctx = match std::mem::replace(&mut self.streamed_write_state, StreamedWriteState::Idle)
        {
            StreamedWriteState::Active(ctx) => ctx,
            StreamedWriteState::Idle => {
                return Err(UsageError(
                    "end_streamed_param called with no active streamed parameter.".to_string(),
                ));
            }
        };
        let StreamedWriteContext {
            message,
            mut pending,
            db_collation,
            value_opened,
            null_signaled,
            timeout_sec,
        } = *ctx;
        self.remaining_request_timeout = timeout_sec.map(|s| Duration::from_secs(u64::from(s)));

        let mut packet_writer = PacketWriter::resume(message, self.transport.as_writer());

        // Close the current value, then either open the next streamed parameter's
        // header or (for the last one) finalize the send. Anything that fails
        // mid-message aborts the whole streamed write.
        let write_outcome = async {
            if null_signaled {
                // NULL: the length field is PLP_NULL and no chunks/terminator
                // follow.
                packet_writer.write_u64_async(PLP_NULL).await?;
            } else {
                // An untouched parameter never wrote its opener; write it now so
                // the value is a present, zero-length value rather than absent.
                if !value_opened {
                    packet_writer.write_u64_async(PLP_UNKNOWN_LEN).await?;
                }
                packet_writer.write_u32_async(PLP_TERMINATOR).await?;
            }
            match pending.pop_front() {
                Some(next) => {
                    let next_name = next
                        .name
                        .clone()
                        .expect("streamed parameter names validated at begin");
                    next.serialize(
                        &mut packet_writer,
                        &db_collation,
                        false,
                        &GenericEncoder::new(),
                    )
                    .await?;
                    Ok(Some(next_name))
                }
                None => {
                    packet_writer.finalize().await?;
                    Ok(None)
                }
            }
        }
        .await;

        match write_outcome {
            // Another streamed parameter is now open for data.
            Ok(Some(next_name)) => {
                let message = packet_writer.suspend();
                self.streamed_write_state =
                    StreamedWriteState::Active(Box::new(StreamedWriteContext {
                        message,
                        pending,
                        db_collation,
                        // Fresh parameter: its opener has not been written and it
                        // has not been marked NULL.
                        value_opened: false,
                        null_signaled: false,
                        timeout_sec,
                    }));
                Ok(StreamedParamStatus::NeedData {
                    param_name: next_name,
                })
            }
            // Last streamed parameter closed and the message was sent: consume
            // the response exactly like execute_sp_executesql does. A failure
            // reading the response also aborts (the request is already on the
            // wire, so the connection must be reset before reuse).
            Ok(None) => {
                drop(packet_writer);
                match self.position_on_first_result().await {
                    Ok(result) => Ok(StreamedParamStatus::Complete(result)),
                    Err(e) => {
                        self.abort_streamed_write().await;
                        Err(e)
                    }
                }
            }
            // Terminator or next-parameter header write failed mid-message: drop
            // the half-written message and abort rather than leave it resumable.
            Err(e) => {
                drop(packet_writer);
                self.abort_streamed_write().await;
                Err(e)
            }
        }
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
    /// Returns the number of rows this client serialized to the wire, matching
    /// `Microsoft.Data.SqlClient`'s `SqlBulkCopy.RowsCopied` semantics. This is
    /// a client-side count, not the server's DONE token row count, so it is not
    /// affected by distributed engines that acknowledge one load with multiple
    /// DONE_COUNT tokens (issue #209). It also does not reflect server-side row
    /// count changes from triggers on the destination table.
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
        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let budget = Self::deduct_timeout(timeout_sec, reconnect_elapsed);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = request_timeout;
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
        let rows_written = writer.end().await?;

        // STEP 5: Drain the server response for error handling and INFO capture.
        // Its returned count is informational only; callers receive the client-side
        // `rows_written` (see the `# Returns` doc and `consume_done_token`).
        self.consume_done_token().await?;

        Ok(rows_written)
    }

    /// Consumes response tokens until a DONE token is received.
    ///
    /// Returns the last counted DONE row count. This value is currently
    /// informational: both call sites discard it, and the bulk-load path reports
    /// the client-side rows written instead (issue #209). It is retained for
    /// error/INFO draining and as defensive last-DONE_COUNT-wins hardening.
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
            self.observe_response_token(&token)?;

            match token {
                Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done) => {
                    info!("Done token: {:?}", done);

                    if done.has_error() && collected_errors.is_empty() {
                        return Err(crate::error::Error::ProtocolError(
                            "Server reported error in DONE token without preceding ERROR token"
                                .to_string(),
                        ));
                    }

                    // Distributed engines send multiple DONE_COUNT tokens each
                    // carrying the full count; summing them double-counts (#209).
                    // Last counted DONE wins.
                    if done.has_count() {
                        rows_affected = done.row_count;
                    }

                    // Stop when we receive a DONE token without the MORE flag
                    if !done.has_more() {
                        break;
                    }
                }
                Tokens::Error(error_token) => {
                    info!(?error_token);
                    collected_errors.push(self.record_error_token(&error_token));
                }
                Tokens::Info(info_token) => {
                    info!(?info_token);
                    self.capture_info_message(&info_token);
                    continue;
                }
                Tokens::EnvChange(env_change) => {
                    info!(?env_change);
                    if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                        self.on_reset_connection_ack();
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
    /// Returns the DONE token row count. Its only caller (the INSERT BULK preamble)
    /// discards it; see `consume_done_token` for why the count is informational.
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
    /// procedure returns result sets, read the rows of each and move between
    /// result sets with [`advance_to_rows()`](Self::advance_to_rows). After all
    /// result sets are consumed, retrieve output parameters with
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
    #[instrument(
        skip(self, positional_parameters, named_parameters, options),
        level = "info"
    )]
    pub async fn execute_stored_procedure<'a>(
        &mut self,
        stored_procedure_name: String,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();
        self.current_command_ce_setting = column_encryption;

        let mut positional_parameters = positional_parameters;
        let mut named_parameters = named_parameters;

        if self.command_is_busy() {
            return Err(crate::error::Error::UsageError(
                ALREADY_EXECUTING_ERROR.to_string(),
            ));
        };

        RpcParameter::reject_data_at_exec(
            positional_parameters
                .iter()
                .flatten()
                .chain(named_parameters.iter().flatten()),
        )?;

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let budget = Self::deduct_timeout(timeout_sec, reconnect_elapsed);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

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
            self.remaining_request_timeout = request_timeout;
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

        self.position_on_first_result().await
    }

    /// Prepares a parameterized statement via `sp_prepare`, returning the
    /// [`StatementId`] it issues for the statement.
    ///
    /// The server handle stays inside the client, keyed by that id, alongside
    /// the statement's cached Always Encrypted metadata. Pass the id to
    /// [`execute_sp_execute()`](Self::execute_sp_execute) for repeated execution
    /// without re-parsing, and to
    /// [`execute_sp_unprepare()`](Self::execute_sp_unprepare) when the statement
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
    /// # Recovery
    ///
    /// Unlike the other prepared RPCs, this method reconnects
    /// before sending. This is safe because `sp_prepare` accepts no existing
    /// handle that recovery could invalidate; the handle it records belongs to
    /// the resulting live session. Prefer the managed
    /// [`execute_prepared`](Self::execute_prepared) / [`unprepare`](Self::unprepare) API.
    #[allow(dead_code)]
    #[instrument(skip(self, named_params, options), level = "info")]
    async fn execute_sp_prepare<'a>(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementId> {
        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        };

        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();
        self.current_command_ce_setting = column_encryption;

        self.begin_command();
        let reconnect_elapsed = self.check_and_reconnect(timeout_sec, cancel_handle).await?;
        let budget = Self::deduct_timeout(timeout_sec, reconnect_elapsed);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();

        // Store timeout and cancel handle for this operation
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

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
        // statement's id so a later sp_execute can encrypt values without
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
            self.remaining_request_timeout = request_timeout;
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
                let statement_id = self.issue_statement_id();
                let previous = self.prepared_handles.insert(statement_id, handle);
                debug_assert!(
                    previous.is_none(),
                    "prepared handle map overwrote a live entry for a reused StatementId"
                );
                if let Some(describe) = describe_for_cache {
                    self.prepared_param_encryption
                        .insert(statement_id, describe);
                }
                Ok(statement_id)
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
    ///
    /// # Recovery
    ///
    /// This method never reconnects. `sp_unprepare` releases an *existing*
    /// server handle, so once the connection has reconnected that handle is
    /// already gone — reusing it hits SQL Server error 8179. Staleness needs no
    /// caller check: a reconnect clears `prepared_handles`, so an id with no
    /// entry is skipped below without an RPC; on a dead connection the send just
    /// fails and is ignored (best-effort). Forcing recovery, if ever wanted, is
    /// the caller's job (see
    /// [`check_and_reconnect`](Self::check_and_reconnect)).
    /// A low-level wire call with caller-owned recovery — prefer the
    /// managed [`execute_prepared`](Self::execute_prepared) / [`unprepare`](Self::unprepare) API.
    ///
    /// Drops `statement_id`'s handle and cached Always Encrypted metadata from
    /// the client's maps before sending, whether or not the RPC succeeds: a
    /// failure is ambiguous (the send may have landed and the response been
    /// lost), and a map entry naming a plan the server already dropped would
    /// make the next execute fail with error 8179 instead of re-preparing.
    /// Absent is the recoverable state; stale-but-present is not.
    ///
    /// `command_started`: `true` when the caller already opened the command
    /// boundary via `begin_command` (as `unprepare` does before recovery, so the
    /// reconnect's info messages survive); `false` opens it here. It does not
    /// affect recovery — this method never reconnects.
    #[instrument(skip(self, options), level = "info")]
    async fn execute_sp_unprepare<'a>(
        &mut self,
        statement_id: StatementId,
        command_started: bool,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<()> {
        if !command_started {
            if self.command_is_busy() {
                return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
            }
            self.begin_command();
        }
        // No reconnect here — recovery is caller-owned (see this method's
        // `# Recovery` docs).

        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            ..
        } = options.into();

        // Store timeout and cancel handle for this operation
        let budget = Self::deduct_timeout(timeout_sec, Duration::ZERO);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();

        // Evicted before the send: a failed RPC is ambiguous, and keeping an
        // entry the server may already have dropped is the worse of the two
        // wrong answers (see this method's docs).
        let Some(handle) = self.prepared_handles.remove(&statement_id) else {
            return Ok(());
        };
        self.prepared_param_encryption.remove(&statement_id);

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
        Ok(())
    }

    /// Executes `statement`, transparently recovering the connection and
    /// re-preparing the server handle when it no longer belongs to the live
    /// session.
    ///
    /// Owns the full recovery protocol so callers never juggle raw handle ids or
    /// session epochs:
    ///
    /// 1. Recover the connection if the socket died, and charge the elapsed
    ///    time against the command timeout.
    /// 2. If `statement` holds a handle from the live session, reuse it via
    ///    `sp_execute`.
    /// 3. Otherwise prepare and execute in one round trip via `sp_prepexec`; the
    ///    new handle is recorded inside the client, keyed by the statement's
    ///    [`StatementId`], when the `@handle` RETURNVALUE lands during the drain
    ///    — no caller step is required.
    ///
    /// `orphaned` names a statement superseded by a re-prepare/rebind whose
    /// server handle should be released: its drop is piggybacked onto the
    /// `sp_prepexec` `@handle` argument when the handle is still live in this
    /// session, and skipped when the map no longer holds it (a reconnect already
    /// discarded it server-side). The id is cleared from `orphaned` only once the
    /// drop crosses the serialization boundary, so callers retain it across
    /// reconnect, validation, parameter-building, and encryption failures that
    /// precede the send and can retry its release.
    ///
    /// Returns positioned on the first result; drain rows with
    /// [`next_row`](ResultSet::next_row) / [`advance`](Self::advance).
    pub async fn execute_prepared<'a>(
        &mut self,
        statement: &mut PreparedStatement,
        named_params: Vec<RpcParameter>,
        orphaned: &mut Option<StatementId>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        RpcParameter::reject_data_at_exec(named_params.iter())?;

        // Open the command boundary before recovery so a transparent reconnect's
        // login info messages land in this command's buffer; the inner RPC skips
        // its own `begin_command` via `command_started`.
        self.begin_command();

        let mut opts = options.into();
        let reconnect_elapsed = self.check_and_reconnect(opts.timeout, opts.cancel).await?;
        let budget = Self::deduct_timeout(opts.timeout, reconnect_elapsed);
        opts.timeout = budget.into_timeout()?.seconds();

        // Reuse the statement's handle when the client still holds one; a
        // reconnect clears the map, so an id from a dead session re-prepares.
        match statement
            .id
            .filter(|id| self.prepared_handles.contains_key(id))
        {
            Some(statement_id) => {
                self.execute_sp_execute(statement_id, None, Some(named_params), true, opts)
                    .await
            }
            None => {
                // Reported by `sp_prepexec` whenever the server's prepare `@handle`
                // landed - including on a failed batch, where the plan still
                // exists and must stay releasable (Ex: prepare passed but execute failed).
                let mut issued_id = None;
                let result = self
                    .execute_sp_prepexec(
                        statement.sql.clone(),
                        named_params,
                        &mut issued_id,
                        orphaned,
                        true,
                        opts,
                    )
                    .await;
                if issued_id.is_some() {
                    statement.id = issued_id;
                }
                result
            }
        }
    }

    /// Issues the next unique [`StatementId`] for a managed prepared statement.
    fn issue_statement_id(&mut self) -> StatementId {
        let id = StatementId(self.next_statement_id);
        // Exhausting u64 (~1.8e19 prepares on one connection) is unreachable, so
        // wrap rather than branch; the insert-site debug_assert catches the
        // impossible id-0 collision in tests.
        self.next_statement_id = self.next_statement_id.wrapping_add(1);
        id
    }

    /// Releases a managed prepared statement's server handle via `sp_unprepare`.
    ///
    /// Recovers a dead connection first (charging the elapsed time against the
    /// command budget), then releases the handle only if the client still holds
    /// one for `statement_id` in the live session. A statement whose handle is
    /// absent — never materialized, or dropped when a reconnect cleared the map —
    /// is already gone server-side, so it is skipped with no RPC. This mirrors
    /// msodbcsql's `DropPrepHandle`, which recovers via `GetBatchCtxOrRecover`
    /// and sends `sp_unprepare` only when the statement's connection id still
    /// matches the recovered connection. Take the id with
    /// [`PreparedStatement::take_id`] once the statement is done.
    pub async fn unprepare<'a>(
        &mut self,
        statement_id: StatementId,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<()> {
        if self.command_is_busy() {
            return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
        }

        // Open the command boundary before recovery so a transparent reconnect's
        // login info messages land in this command's buffer; the inner RPC skips
        // its own `begin_command` via `command_started`.
        self.begin_command();

        let mut opts = options.into();
        let reconnect_elapsed = self.check_and_reconnect(opts.timeout, opts.cancel).await?;
        let budget = Self::deduct_timeout(opts.timeout, reconnect_elapsed);
        // Absent handle (never materialized, or a reconnect cleared the map):
        // already gone server-side, skip with no RPC — including the timeout
        // check below, since releasing nothing cannot time out.
        if !self.prepared_handles.contains_key(&statement_id) {
            return Ok(());
        }
        opts.timeout = budget.into_timeout()?.seconds();
        self.execute_sp_unprepare(statement_id, true, opts).await
    }

    /// Prepares and executes a parameterized statement in a single round-trip
    /// via `sp_prepexec`, combining
    /// [`execute_sp_prepare()`](Self::execute_sp_prepare) and
    /// [`execute_sp_execute()`](Self::execute_sp_execute).
    ///
    /// The `@handle` RETURNVALUE trails the result set, so it is captured during
    /// the push_return_values and recorded under the issued [`StatementId`]; the statement
    /// becomes reusable via `sp_execute` once drained. Rows are read with
    /// [`read_row()`](Self::read_row).
    ///
    /// A low-level wire call - prefer the managed
    /// [`execute_prepared`](Self::execute_prepared) /
    /// [`unprepare`](Self::unprepare) API.
    ///
    /// # Parameters
    ///
    /// - `named_params` - the statement's parameters. Unlike `sp_prepare`,
    ///   their values are sent and executed, not just declared.
    ///
    /// - `statement_id` - **out**: the id this call issues. Set whenever the
    ///   server allocated a plan, which is not the same as "the batch
    ///   succeeded":
    ///   - success - always set (the `@handle` may still be in flight);
    ///   - error *with* a `@handle` (e.g. Prepare passed but execute failed)
    ///     - set, so the caller can reuse or [`unprepare`](Self::unprepare) it;
    ///   - error *without* one (compile failure, or the stream broke first) -
    ///     left untouched, leaving the statement unmaterialized.
    ///
    /// - `orphan` - **in/out**: a statement whose prepared handle is to be
    ///   released, piggybacked onto this prepexec.
    ///   `Some(id)` sends its handle as the by-reference `@handle`
    ///   input so the server drops that plan while preparing the new one,
    ///   saving a separate `sp_unprepare` round trip; `None`, or an id with
    ///   no live handle, sends NULL and prepares fresh. On return it reports
    ///   ownership: still `Some` if the call failed before serialization (the
    ///   caller may retry the release), `None` once the drop crossed that
    ///   boundary - past which the server may have consumed it even if the call
    ///   then fails after the dropping of orphan.
    ///
    /// - `command_started` - `true` when the caller already opened the command
    ///   boundary via `begin_command` (as
    ///   [`execute_prepared`](Self::execute_prepared) does before recovering, so
    ///   the reconnect's info messages survive); `false` opens it here.
    ///
    /// Both handle decisions match msodbcsql, which keys them on whether it
    /// holds a handle rather than on the batch's return code.
    ///
    /// # Recovery
    ///
    /// Never reconnects - the caller recovers once up front, as
    /// [`execute_prepared`](Self::execute_prepared) does. Staleness needs no
    /// caller check: a reconnect clears `prepared_handles`, so an `orphan` from
    /// the dead session resolves to no handle and its drop is skipped.
    #[instrument(skip(self, named_params, options), level = "info")]
    async fn execute_sp_prepexec<'a>(
        &mut self,
        sql: String,
        mut named_params: Vec<RpcParameter>,
        statement_id: &mut Option<StatementId>,
        orphan: &mut Option<StatementId>,
        command_started: bool,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        if !command_started {
            if self.command_is_busy() {
                return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
            }
            self.begin_command();
        }
        // No reconnect here — recovery is caller-owned (see this method's
        // `# Recovery` docs).

        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();
        self.current_command_ce_setting = column_encryption;

        // Store timeout and cancel handle for this operation
        let budget = Self::deduct_timeout(timeout_sec, Duration::ZERO);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

        self.transport.reset_reader();

        let database_collation = self.negotiated_settings.database_collation;

        let sql_statement_value =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql.clone())));

        // The capture target is armed just before the send below so a failure
        // while building the RPC cannot leave it set.
        self.pending_prepared_param_encryption = None;
        self.pending_capture = None;

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
            self.pending_prepared_param_encryption = Some(
                self.encrypt_parameters(
                    &sql,
                    &params_list_as_string,
                    &mut named_params,
                    timeout_sec,
                    cancel_handle,
                )
                .await?,
            );
            // The describe round-trip closes its own batch and clears the
            // per-operation timeout/cancel state; restore it for the real RPC.
            self.remaining_request_timeout = request_timeout;
            self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());
            self.transport.reset_reader();
        }

        let params_as_sql_string =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(params_list_as_string)));

        let params_parameter = RpcParameter::new(None, StatusFlags::NONE, params_as_sql_string);

        // The by-reference `@handle`: NULL input prepares fresh; a `Some(h)`
        // input tells the server to drop prepared statement `h` before
        // preparing. The new handle comes back as the `@handle` RETURNVALUE captured during drain.
        // From this point onward, serialization or response failures are
        // ambiguous: the server may have consumed the piggybacked drop, so the
        // orphan is released and its entries evicted either way.
        let drop_handle = orphan.take().and_then(|orphan_id| {
            self.prepared_param_encryption.remove(&orphan_id);
            self.prepared_handles.remove(&orphan_id)
        });
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

        // Armed here, not earlier: the AE describe round-trip above emits its own
        // RETURNVALUEs, and a build failure must not leave a stale target.
        let issued_id = self.issue_statement_id();
        self.pending_capture = Some(issued_id);

        let mut packet_writer =
            rpc.create_packet_writer(self.transport.as_writer(), timeout_sec, cancel_handle);
        let serialize_result = rpc.serialize(&mut packet_writer).await;
        drop(packet_writer);
        if let Err(e) = serialize_result {
            self.abort_pending_prepare_capture();
            self.report_issued_id(issued_id, statement_id);
            return Err(e);
        }

        let boundary = match self.advance_to_result_boundary().await {
            Ok(boundary) => boundary,
            Err(e) => {
                self.abort_pending_prepare_capture();
                self.report_issued_id(issued_id, statement_id);
                return Err(e);
            }
        };
        // The handle may still be in flight (it follows the result set), so the id
        // is always reported here.
        *statement_id = Some(issued_id);
        Ok(self.apply_result_boundary(boundary))
    }

    /// Reports `issued_id` to the caller only if the server's `@handle` actually
    /// arrived.
    ///
    /// Call this after aborting the capture, so no later token can land: the map
    /// lookup is then a final answer to "did we get a prepared plan?". Reporting an id
    /// with no handle behind it would leave the statement claiming to be
    /// prepared when nothing was.
    fn report_issued_id(&self, issued_id: StatementId, out: &mut Option<StatementId>) {
        if self.prepared_handles.contains_key(&issued_id) {
            *out = Some(issued_id);
        }
    }

    /// Executes a previously prepared statement by handle via `sp_execute`.
    ///
    /// Re-uses the execution plan from an earlier
    /// [`execute_sp_prepare()`](Self::execute_sp_prepare) or
    /// [`execute_sp_prepexec()`](Self::execute_sp_prepexec) call.
    /// Supply fresh parameter values through `positional_parameters` and/or
    /// `named_parameters`.
    ///
    /// # Recovery
    ///
    /// This method never reconnects; callers that need it recover once with
    /// [`check_and_reconnect`](Self::check_and_reconnect) first, as
    /// [`execute_prepared`](Self::execute_prepared) does. Staleness needs no
    /// caller check: a reconnect clears `prepared_handles`, so `statement_id`
    /// either resolves to a handle belonging to the live session or the call
    /// fails before reaching the wire.
    /// A low-level wire call with caller-owned recovery — prefer the
    /// managed [`execute_prepared`](Self::execute_prepared) / [`unprepare`](Self::unprepare) API.
    ///
    /// `command_started`: `true` when the caller already opened the command
    /// boundary via `begin_command` (as `execute_prepared` does before recovery,
    /// so the reconnect's info messages survive); `false` opens it here. It does
    /// not affect recovery — this method never reconnects.
    #[instrument(
        skip(self, positional_parameters, named_parameters, options),
        level = "info"
    )]
    async fn execute_sp_execute<'a>(
        &mut self,
        statement_id: StatementId,
        mut positional_parameters: Option<Vec<RpcParameter>>,
        mut named_parameters: Option<Vec<RpcParameter>>,
        command_started: bool,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        let Some(handle) = self.prepared_handles.get(&statement_id).copied() else {
            return Err(UsageError(
                "Cannot execute. Given prepared statement is not materialized on this connection"
                    .to_string(),
            ));
        };
        if !command_started {
            if self.command_is_busy() {
                return Err(UsageError(ALREADY_EXECUTING_ERROR.to_string()));
            }
            self.begin_command();
        }
        // No reconnect here — recovery is caller-owned (see this method's
        // `# Recovery` docs).

        let ExecuteOptions {
            timeout: timeout_sec,
            cancel: cancel_handle,
            column_encryption,
        } = options.into();
        self.current_command_ce_setting = column_encryption;

        // Store timeout and cancel handle for this operation
        let budget = Self::deduct_timeout(timeout_sec, Duration::ZERO);
        let resolved = budget.into_timeout()?;
        let timeout_sec = resolved.seconds();
        let request_timeout = resolved.duration();
        self.remaining_request_timeout = request_timeout;
        self.cancel_handle = cancel_handle.map(|handle| handle.child_handle());

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
                .get(&statement_id)
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

        self.position_on_first_result().await
    }

    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub async fn execute_sp_prepare_for_test<'a>(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementId> {
        self.execute_sp_prepare(sql, named_params, options).await
    }

    /// Records `handle` under a fresh id without a round-trip, so wire-protocol
    /// tests can drive the sp_* RPCs with a handle the server never issued (or
    /// issued to a session that has since died).
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn register_prepared_handle_for_test(&mut self, handle: i32) -> StatementId {
        let statement_id = self.issue_statement_id();
        self.prepared_handles.insert(statement_id, handle);
        statement_id
    }

    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub async fn execute_sp_unprepare_for_test<'a>(
        &mut self,
        statement_id: StatementId,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<()> {
        self.execute_sp_unprepare(statement_id, false, options)
            .await
    }

    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub async fn execute_sp_prepexec_for_test<'a>(
        &mut self,
        sql: String,
        named_params: Vec<RpcParameter>,
        orphan: &mut Option<StatementId>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<(StatementId, StatementResult)> {
        let mut issued_id = None;
        let result = self
            .execute_sp_prepexec(sql, named_params, &mut issued_id, orphan, false, options)
            .await?;
        Ok((
            issued_id.expect("a successful sp_prepexec always reports its id"),
            result,
        ))
    }

    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub async fn execute_sp_execute_for_test<'a>(
        &mut self,
        statement_id: StatementId,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        options: impl Into<ExecuteOptions<'a>>,
    ) -> TdsResult<StatementResult> {
        self.execute_sp_execute(
            statement_id,
            positional_parameters,
            named_parameters,
            false,
            options,
        )
        .await
    }

    /// Collects a return value. While an `sp_prepexec` is in flight (an armed
    /// `pending_capture`) the `@handle` (RETURNVALUE ordinal 0) is diverted into
    /// the client's prepared handle map instead of being surfaced; every
    /// `Tokens::ReturnValue` funnels through here so capture works regardless of
    /// which drain path reads the stream. For every other RPC `pending_capture`
    /// is `None` and ordinal 0 is a genuine output parameter, so it falls
    /// through to `return_values`.
    fn push_return_value(&mut self, return_value: ReturnValue) {
        if let Some(statement_id) = self.pending_capture
            && return_value.param_ordinal == 0
            && let ColumnValues::Int(handle) = &return_value.value
        {
            // Route the handle into the client's map under the
            // statement's id and divert it from `return_values`. `execute_prepared`
            // arms `pending_capture` only when the id was absent from the map, so
            // this never overwrites a live handle.
            self.pending_capture = None;
            let previous = self.prepared_handles.insert(statement_id, *handle);
            debug_assert!(
                previous.is_none(),
                "prepared handle map overwrote a live entry for a reused StatementId"
            );
            if let Some(describe) = self.pending_prepared_param_encryption.take() {
                self.prepared_param_encryption
                    .insert(statement_id, describe);
            }
            return;
        }
        self.return_values.push(return_value);
    }

    #[instrument(skip(self), level = "info")]
    async fn drain_rows(&mut self) -> TdsResult<()> {
        if self.maybe_has_unread_rows() {
            // A pull cursor (`next_row_cursor`) may have left the current row
            // partially read (`RowPaused`/`PlpPaused`). The push API used below
            // rejects that state to avoid silently skipping a row the caller
            // still expects to see — but here we are intentionally discarding
            // the rest of the result set, so realign the stream past that parked
            // row first. `drain_active_row` is a no-op when nothing is parked.
            self.drain_active_row_if_needed().await?;
            // Drain the current result set.
            let mut writer = DiscardRowWriter;
            while self.next_row_into(&mut writer).await? {
                info!(
                    column_count = self
                        .current_metadata
                        .as_ref()
                        .map_or(0, |m| m.columns.len()),
                    "Consuming row while draining result set"
                );
            }
        }
        Ok(())
    }

    /// Drains all remaining tokens from the stream until a terminal DONE token.
    /// Collects any ERROR tokens encountered and returns them.
    ///
    /// A statement-scoped error (for example lock timeout 1222) does not abort
    /// the batch, so the server keeps streaming any result sets that follow it.
    /// A trailing row-returning result set must therefore be consumed through
    /// the row-decoding path: ROW/NBCROW tokens carry no length prefix and can
    /// only be parsed with the preceding COLMETADATA in the parser context.
    /// Skipping that step would leave unparsed row bytes in the transport and
    /// corrupt the connection for reuse.
    pub(in crate::connection) async fn drain_stream(&mut self) -> TdsResult<Vec<SqlErrorInfo>> {
        let mut collected_errors: Vec<SqlErrorInfo> = Vec::new();
        // A COLMETADATA reached at the top level of the drain must be parsed with
        // the same Always Encrypted awareness as advance_to_result_boundary: when
        // column encryption is negotiated the token carries a CEK-table prefix
        // (empty or not) before the first column, and reading it with
        // ParserContext::None would misinterpret those bytes and desynchronize the
        // stream — the very corruption this drain exists to prevent.
        let parser_context = ParserContext::ColumnEncryption(
            self.negotiated_settings.is_column_encryption_supported(),
        );
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
            self.observe_response_token(&token)?;

            match token {
                Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done) => {
                    info!(?done);
                    info!(?done.status);
                    if !done.has_more() {
                        if !collected_errors.is_empty() || done.has_error() {
                            self.abort_pending_prepare_capture();
                        }
                        break;
                    }
                }
                Tokens::ColMetadata(colmetadata) => {
                    // A row-returning result set began. Consume its rows via the
                    // row-decoding path; the trailing DONE tells us whether the
                    // batch continues.
                    let batch_ended = self
                        .drain_result_set_rows(Arc::new(colmetadata), &mut collected_errors)
                        .await?;
                    if batch_ended {
                        break;
                    }
                }
                other => self.apply_drain_side_effect(other, &mut collected_errors)?,
            }
        }
        Ok(collected_errors)
    }

    /// Consumes every ROW/NBCROW token of one result set (given its
    /// `metadata`), discarding the decoded values, and returns `true` when the
    /// result set's DONE token terminates the batch (no MORE flag).
    ///
    /// Rows are read with [`ColumnPolicy::SkipAll`] into a [`DiscardRowWriter`],
    /// so `receive_row_into` fully consumes each row — including PLP payloads —
    /// without materializing any column and never yields a pause result here.
    async fn drain_result_set_rows(
        &mut self,
        metadata: Arc<ColMetadataToken>,
        collected_errors: &mut Vec<SqlErrorInfo>,
    ) -> TdsResult<bool> {
        // Rows are skipped, so no cell decryptor is resolved: encrypted columns
        // are consumed as raw ciphertext bytes without decoding. Resolving a
        // decryptor here would add key-store round trips and decryption
        // failures — any of which returns `Err` and would mask the SQL error
        // that this drain exists to surface — and would overwrite
        // `self.current_decryptor` with a discarded set's metadata.
        let parser_context = ParserContext::ColumnMetadata(metadata, None);
        let mut discarded_rows: u64 = 0;
        loop {
            let start = Instant::now();
            let mut writer = DiscardRowWriter;
            let result = self
                .transport
                .receive_row_into(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                    ColumnPolicy::SkipAll,
                    &mut writer,
                )
                .await?;
            self.update_remaining_timeout(start);

            match result {
                RowReadResult::RowWritten => {
                    discarded_rows += 1;
                }
                RowReadResult::RowPaused(_) | RowReadResult::PlpPaused(_) => {
                    return Err(crate::error::Error::ProtocolError(
                        "Row read paused while draining a result set; the drain writer never requests a pause".to_string(),
                    ));
                }
                RowReadResult::Token(token) => match token {
                    Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done) => {
                        info!(
                            ?done,
                            discarded_rows, "Draining DONE token ending result set"
                        );
                        return Ok(!done.has_more());
                    }
                    Tokens::ColMetadata(_) => {
                        return Err(crate::error::Error::ProtocolError(
                            "Unexpected COLMETADATA token before the previous result set's DONE while draining".to_string(),
                        ));
                    }
                    other => self.apply_drain_side_effect(other, collected_errors)?,
                },
            }
        }
    }

    /// Applies the side effects of a non-terminal control token seen while
    /// draining. DONE and COLMETADATA are handled by the callers because they
    /// steer the drain loop; everything else is either recorded or skipped.
    fn apply_drain_side_effect(
        &mut self,
        token: Tokens,
        collected_errors: &mut Vec<SqlErrorInfo>,
    ) -> TdsResult<()> {
        match token {
            Tokens::Error(error_token) => {
                info!(?error_token, "Draining ERROR token from stream");
                collected_errors.push(self.record_error_token(&error_token));
            }
            Tokens::Info(info_token) => {
                info!(?info_token, "Draining INFO token from stream");
                self.capture_info_message(&info_token);
            }
            Tokens::EnvChange(t1) => {
                if t1.sub_type == EnvChangeTokenSubType::ResetConnection {
                    self.on_reset_connection_ack();
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
            other => {
                info!(?other);
            }
        }
        Ok(())
    }

    /// Reads tokens up to the next result boundary in the response stream.
    ///
    /// With `expose_norow_statements = false` (result-set navigation used by
    /// batch execution and the JS/Python consumers), a no-row statement's DONE
    /// token carrying the MORE flag is skipped so the method advances to the
    /// next COLMETADATA — consecutive no-row statements collapse into the
    /// following row-returning result set.
    ///
    /// With `true` (ODBC statement-wise navigation, matching msodbcsql), a
    /// no-row statement's DONE token can be its own result boundary, returned
    /// as [`ResultBoundaryKind::NoRows`] instead of always being skipped. It is
    /// surfaced only when the statement carries a row count (DONE `COUNT` flag)
    /// or produced an informational message (PRINT / low-severity RAISERROR);
    /// a pure no-op statement with neither — e.g. a bare `CREATE TABLE` — is
    /// still collapsed into the following result, exactly as in result-set
    /// navigation. This mirrors msodbcsql, which exposes a statement as its own
    /// result iff it returns rows, carries a count, or produced a message. A
    /// DONE reached in this method (without a COLMETADATA earlier in the same
    /// call) always belongs to a no-row statement, because a row-returning
    /// statement's DONE is consumed while its rows are read/drained.
    async fn advance_to_result_boundary(&mut self) -> TdsResult<ResultBoundaryKind> {
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
            self.observe_response_token(&token)?;
            match token {
                Tokens::ColMetadata(md) => {
                    info!(?md);
                    self.current_result_set_has_been_read_till_end = false;
                    // Positioning on a row-returning result: its count is
                    // unavailable on a forward-only cursor. Clear any count
                    // captured from a preceding DONE-only (DML) result in the
                    // same batch so it is not misreported for this SELECT.
                    self.last_rows_affected = -1;
                    self.dml_result_counts.clear();
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

                    // Capture the affected-row count for `SQLRowCount`, but only
                    // when the DONE_COUNT flag is set — otherwise `row_count` is
                    // not meaningful (DDL, SET NOCOUNT ON) and must stay -1. Each
                    // counted DONE is also appended in order so a pure-DML batch
                    // surfaces one count per statement.
                    let has_count = done.status.contains(DoneStatus::COUNT);
                    if has_count {
                        let count = i64::try_from(done.row_count).unwrap_or(i64::MAX);
                        self.last_rows_affected = count;
                        self.dml_result_counts.push(count);
                    }

                    // Statement-wise navigation (msodbcsql parity): this DONE is
                    // a navigable result only if the statement returned a row
                    // count (COUNT flag) or produced messages. Pure DDL / no-op
                    // statements (no count, no messages) are collapsed, exactly
                    // like result-set navigation, so a batch such as
                    // `CREATE; INSERT; SELECT` exposes the INSERT's row count and
                    // the SELECT, not the bare CREATE. `rows_affected` is
                    // `Some(n)` only when the DONE carried a COUNT.
                    if has_count || saw_message {
                        self.execution_context.set_has_open_batch(!is_last);
                        return Ok(ResultBoundaryKind::NoRows {
                            rows_affected: if has_count {
                                Some(done.row_count)
                            } else {
                                None
                            },
                        });
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
                        self.on_reset_connection_ack();
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
                    let mut all_errors = vec![self.record_error_token(&error_token)];
                    let drain_result = self.drain_stream().await;
                    // Reset batch state before propagating: the error terminates
                    // the batch regardless of whether the drain fully consumed
                    // it, so a subsequent `next_row` / `advance` must not pass
                    // the `maybe_has_unread_rows` guard and read a stream we have
                    // given up on (mirrors `handle_row_read_token`).
                    self.execution_context.set_has_open_batch(false);
                    self.current_result_set_has_been_read_till_end = true;
                    self.current_metadata = None;
                    match drain_result {
                        Ok(mut drain_errors) => all_errors.append(&mut drain_errors),
                        Err(e) => {
                            warn!(error = ?e, "Drain after statement error failed; connection may not be reusable");
                        }
                    }
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

    /// Positions on the next **row-returning** result set, collapsing (skipping)
    /// any no-row statements, and returns its column metadata — or `None` at end
    /// of batch. Internal helper for paths that only consume row sets (e.g.
    /// reading the result sets of `sp_describe_parameter_encryption`).
    #[instrument(skip(self), level = "debug", name = "next_rowset")]
    pub(crate) async fn next_rowset(&mut self) -> TdsResult<Option<Arc<ColMetadataToken>>> {
        loop {
            match self.advance_to_result_boundary().await? {
                ResultBoundaryKind::RowSet(md) => return Ok(Some(md)),
                ResultBoundaryKind::NoRows { .. } => continue,
                ResultBoundaryKind::End => return Ok(None),
            }
        }
    }

    /// Applies a [`ResultBoundaryKind`] to the client's current-result state and
    /// maps it to the public [`StatementResult`] returned by
    /// [`execute`](Self::execute) and [`advance`](Self::advance).
    fn apply_result_boundary(&mut self, boundary: ResultBoundaryKind) -> StatementResult {
        match boundary {
            ResultBoundaryKind::RowSet(md) => {
                self.current_metadata = Some(md);
                self.execution_context.set_has_open_batch(true);
                self.current_result_set_has_been_read_till_end = false;
                StatementResult::Rows
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

    /// Returns `true` while the current batch still has unconsumed results on
    /// the wire (a positioned result set, or further statements to navigate to).
    /// Used by the ODBC layer to decide whether the connection stays busy after
    /// positioning on a no-row statement result.
    pub fn has_open_batch(&self) -> bool {
        self.execution_context.has_open_batch()
    }

    /// Returns `true` when an open result batch or streamed write blocks a new command.
    pub(in crate::connection) fn command_is_busy(&self) -> bool {
        self.execution_context.has_open_batch()
            || !matches!(self.streamed_write_state, StreamedWriteState::Idle)
    }

    /// Returns `true` when the client is currently positioned on a row-returning
    /// result set (the last [`execute`](Self::execute) / [`advance`](Self::advance)
    /// returned [`StatementResult::Rows`]). Row-reading via the [`ResultSet`] API
    /// is only meaningful in this state.
    pub fn on_rows(&self) -> bool {
        self.current_metadata.is_some()
    }

    /// Positions on the first navigable result after a request has been sent to
    /// the wire. Shared tail of the `execute*` entry points.
    async fn position_on_first_result(&mut self) -> TdsResult<StatementResult> {
        let boundary = self.advance_to_result_boundary().await?;
        Ok(self.apply_result_boundary(boundary))
    }

    /// Advances to the next navigable result in the current batch, draining any
    /// unread rows of the current result set first. Returns
    /// [`StatementResult::End`] when the batch is exhausted.
    ///
    /// This is the lossless, statement-wise "next": each DML count, message-only
    /// statement, and row set is surfaced individually (matching msodbcsql's
    /// `SQLMoreResults`). Use [`advance_to_rows()`](Self::advance_to_rows) to
    /// skip straight to the next row-returning result set.
    #[instrument(skip(self), level = "info")]
    pub async fn advance(&mut self) -> TdsResult<StatementResult> {
        if !self.execution_context.has_open_batch() {
            return Ok(StatementResult::End);
        }
        if self.maybe_has_unread_rows()
            && let Err(error) = self.drain_rows().await
        {
            self.abort_pending_prepare_capture();
            return Err(error);
        }
        // Draining the current result set may have consumed the batch's final
        // DONE token (has_more=false), which closes the batch. If so there is
        // nothing left on the wire to advance to; reading again would block
        // forever waiting for a token that never arrives.
        if !self.execution_context.has_open_batch() {
            return Ok(StatementResult::End);
        }
        match self.position_on_first_result().await {
            Ok(result) => Ok(result),
            Err(error) => {
                self.abort_pending_prepare_capture();
                Err(error)
            }
        }
    }

    /// Advances to the next **row-returning** result set, collapsing (skipping)
    /// no-row statements (DML counts / message-only statements). Returns `true`
    /// when positioned on rows, or `false` at end of batch. This is the
    /// "give me the next rowset" convenience for consumers that don't care about
    /// per-statement counts — the equivalent of ADO.NET's `NextResult`.
    #[instrument(skip(self), level = "info")]
    pub async fn advance_to_rows(&mut self) -> TdsResult<bool> {
        loop {
            match self.advance().await? {
                StatementResult::Rows => return Ok(true),
                StatementResult::NoRows { .. } => continue,
                StatementResult::End => return Ok(false),
            }
        }
    }

    /// This functions returns to the next row in the result set.
    /// If there are no more rows, it returns None.
    // Not instrumented: the span pushes ResultSet::next_row over the 4 KiB
    // hot-path future budget. Successful rows still emit `Row Received`.
    pub(crate) async fn get_next_row(&mut self) -> TdsResult<Option<Vec<ColumnValues>>> {
        let col_count = self
            .current_metadata
            .as_ref()
            .map(|m| m.columns.len())
            .unwrap_or(0);
        let mut writer = DefaultRowWriter::new(col_count);
        if self.next_row_into(&mut writer).await? {
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
        match self.next_rowset().await? {
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
        if self.advance_to_rows().await? {
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
    ) -> TdsResult<
        Arc<crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult>,
    > {
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
    ) -> TdsResult<
        Arc<crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult>,
    > {
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
        .await?;
        Ok(describe)
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
                param.value()?,
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
            let type_name = param.sql_declaration()?;
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
            let type_name = param.sql_declaration()?;
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

    pub(crate) fn active_plp_reached_end(&self) -> bool {
        match &self.active_row_read_state {
            ActiveRowReadState::PlpPaused(plp_state) => plp_state.reached_end(),
            _ => true,
        }
    }

    /// Declared total length (wire bytes) of the active PLP value when the
    /// server sent a known-length header; `None` when no PLP stream is active
    /// or the value is unknown-length (streamed).
    pub(crate) fn active_plp_known_len(&self) -> Option<u64> {
        match &self.active_row_read_state {
            ActiveRowReadState::PlpPaused(plp_state) => plp_state.known_len(),
            _ => None,
        }
    }

    /// Cumulative wire bytes consumed from the active PLP value across all
    /// chunks; `0` when no PLP stream is active.
    pub(crate) fn active_plp_total_read(&self) -> usize {
        match &self.active_row_read_state {
            ActiveRowReadState::PlpPaused(plp_state) => plp_state.total_read(),
            _ => 0,
        }
    }

    pub(crate) async fn read_active_plp_bytes(&mut self, out: &mut [u8]) -> TdsResult<usize> {
        let ActiveRowReadState::PlpPaused(plp_state) = &mut self.active_row_read_state else {
            // No active stream is a sequencing error, not EOF: returning `Ok(0)`
            // here is indistinguishable from a legitimately exhausted stream, so
            // a mis-ordered call would silently yield an empty value instead of
            // surfacing the bug.
            return Err(UsageError(
                "read_active_plp_bytes called with no active PLP stream; \
                 read_row_column must report CursorColumn::PlpStreaming first"
                    .to_string(),
            ));
        };

        let start = Instant::now();
        let result = self
            .transport
            .read_active_plp_bytes(
                plp_state,
                self.remaining_request_timeout,
                self.cancel_handle.as_ref(),
                out,
            )
            .await;
        self.update_remaining_timeout(start);
        match result {
            Ok(read) => Ok(read),
            Err(error) => {
                self.abort_pending_prepare_capture();
                Err(error)
            }
        }
    }

    /// Streams the next chunk of the active PLP column into `out`, returning the
    /// bytes written and whether the stream is now finished ([`PlpChunk`]).
    ///
    /// This is the public continuation for [`CursorColumn::PlpStreaming`]: after
    /// [`read_row_column`](Self::read_row_column) reports a PLP column, call this
    /// in a loop until [`PlpChunk::reached_end`] is `true`, then resume the
    /// cursor with the next [`read_row_column`](Self::read_row_column).
    ///
    /// # Errors
    ///
    /// Returns `UsageError` when no PLP stream is active — i.e. when called
    /// without a preceding [`CursorColumn::PlpStreaming`] from
    /// [`read_row_column`](Self::read_row_column). This is a sequencing error,
    /// deliberately distinct from a legitimately exhausted stream (which reports
    /// `read == 0, reached_end == true`).
    #[instrument(skip(self, out), level = "info")]
    pub async fn read_active_plp_chunk(&mut self, out: &mut [u8]) -> TdsResult<PlpChunk> {
        let read = self.read_active_plp_bytes(out).await?;
        Ok(PlpChunk {
            read,
            reached_end: self.active_plp_reached_end(),
            known_total: self.active_plp_known_len(),
            total_read: self.active_plp_total_read(),
        })
    }

    /// Decodes the next row directly into a [`RowWriter`], returning `true` if
    /// a row was written or `false` when the result set is exhausted.
    ///
    /// This is the **push** entry point used by bulk consumers (Arrow / N-API /
    /// `next_row`). It always decodes a full row ([`ColumnPolicy::DecodeAll`]) and
    /// never pauses. If the pull cursor
    /// ([`next_row_cursor`](Self::next_row_cursor)) left a row *partially* read,
    /// this returns a [`UsageError`] rather than silently draining and skipping
    /// that row; a fully-consumed or absent row is accepted.
    ///
    /// Uses `receive_row_into` to decode ROW/NBCROW tokens directly through
    /// `decode_into`, bypassing the intermediate `RowToken { all_values }`.
    /// Concrete writers stay concrete through the production transport and
    /// decode chain. [`ResultSet::next_row_into`] provides the same operation
    /// through statically dispatched trait calls.
    // `#[instrument]` adds enough state to exceed the 4096 B budget once the
    // lazy timeout future is inlined. Successful rows still emit `Row Received`.
    pub async fn next_row_into<W>(&mut self, writer: &mut W) -> TdsResult<bool>
    where
        W: RowWriter + Send + ?Sized,
    {
        // Every error return below must abort the pending prepare capture. A
        // wrapper that centralizes this cleanup exceeds the row-future size budget.
        // End-of-set reads are idempotent even after advancing clears metadata.
        if self.current_result_set_has_been_read_till_end {
            return Ok(false);
        }

        if self.current_metadata.is_none() {
            self.abort_pending_prepare_capture();
            return Err(UsageError(
                "No metadata found while fetching the next row. Have you called the execute method or was the query supposed to return resultset?".to_string(),
            ));
        }

        // The push path decodes whole rows and never pauses, so it must not run
        // while the pull cursor has a row *partially* read: silently draining it
        // would discard that row and return the *next* one, mapping the caller's
        // earlier `next_row_cursor() == true` to a row it never sees. An absent
        // (`Idle`) row is fine because there is nothing left to skip.
        match &self.active_row_read_state {
            ActiveRowReadState::Idle => {}
            ActiveRowReadState::RowPaused(_) | ActiveRowReadState::PlpPaused(_) => {
                self.abort_pending_prepare_capture();
                return Err(UsageError(
                    "next_row_into called while a pull-cursor row is still active; \
                     advance the cursor with next_row_cursor before using the push row API"
                        .to_string(),
                ));
            }
        }

        let metadata = Arc::clone(self.current_metadata.as_ref().unwrap());
        let decryptor = match self.resolve_cell_decryptor(&metadata).await {
            Ok(decryptor) => decryptor,
            Err(error) => {
                self.abort_pending_prepare_capture();
                return Err(error);
            }
        };
        let parser_context = ParserContext::ColumnMetadata(metadata, decryptor);
        loop {
            let start = Instant::now();
            let result = match self
                .transport
                .receive_row_into(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                    ColumnPolicy::DecodeAll,
                    writer,
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    self.abort_pending_prepare_capture();
                    return Err(error);
                }
            };
            self.update_remaining_timeout(start);

            match result {
                RowReadResult::RowWritten => {
                    writer.end_row();
                    info!("Row Received");
                    return Ok(true);
                }
                RowReadResult::RowPaused(_) | RowReadResult::PlpPaused(_) => {
                    // DecodeAll never pauses; a pause here is a protocol/logic error.
                    self.abort_pending_prepare_capture();
                    return Err(crate::error::Error::ProtocolError(
                        "Unexpected pause while decoding a full row (ColumnPolicy::DecodeAll)"
                            .to_string(),
                    ));
                }
                RowReadResult::Token(token) => {
                    let handled = match Box::pin(self.handle_row_read_token(token)).await {
                        Ok(handled) => handled,
                        Err(error) => {
                            self.abort_pending_prepare_capture();
                            return Err(error);
                        }
                    };
                    if let Some(has_row) = handled {
                        return Ok(has_row);
                    }
                }
            }
        }
    }

    /// Positions the cursor on the next row without decoding any column
    /// (ODBC `SQLFetch`). Returns `Ok(true)` when positioned on a row and
    /// `Ok(false)` when the result set is exhausted.
    ///
    /// After this returns `true`, individual columns are pulled with
    /// [`read_row_column`](Self::read_row_column). Any previously positioned row
    /// is drained first; its remaining column bytes are read and discarded rather
    /// than returned to the caller.
    #[instrument(skip(self), level = "info")]
    pub async fn next_row_cursor(&mut self) -> TdsResult<bool> {
        if self.current_metadata.is_none() {
            return Err(UsageError(
                "No metadata found while fetching the next row. Have you called the execute method or was the query supposed to return resultset?".to_string(),
            ));
        }

        // Idempotent at end-of-set: once the terminating DONE has been read the
        // wire holds nothing more for this result set until the caller advances
        // (SQLMoreResults). Re-reading here would block on a packet the server
        // will never send, so report exhaustion without touching the transport.
        if self.current_result_set_has_been_read_till_end {
            return Ok(false);
        }

        self.drain_active_row_if_needed().await?;

        let metadata = Arc::clone(self.current_metadata.as_ref().unwrap());
        let decryptor = self.resolve_cell_decryptor(&metadata).await?;
        let parser_context = ParserContext::ColumnMetadata(metadata, decryptor);
        loop {
            let start = Instant::now();
            let header = self
                .transport
                .receive_row_header(
                    &parser_context,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                )
                .await?;
            self.update_remaining_timeout(start);

            match header {
                RowHeader::Positioned(pause_state) => {
                    // Positioned before column 0; columns are pulled lazily via
                    // `read_row_column`. A zero-column row parks here too and
                    // drains as a no-op on the next advance.
                    self.active_row_read_state =
                        ActiveRowReadState::RowPaused(Box::new(pause_state));
                    return Ok(true);
                }
                RowHeader::Token(token) => {
                    if let Some(has_row) = Box::pin(self.handle_row_read_token(token)).await? {
                        return Ok(has_row);
                    }
                }
            }
        }
    }

    /// Pulls column `target` (0-based) of the currently positioned row,
    /// skipping any intervening columns (ODBC `SQLGetData`). Forward-only:
    /// `target` must be at or after the cursor's next undecoded column.
    ///
    /// Returns:
    /// - [`CursorColumn::Value`] with the decoded value for non-PLP columns,
    /// - [`CursorColumn::PlpStreaming`] when `target` is a PLP column whose
    ///   bytes must be pulled via [`read_active_plp_chunk`](Self::read_active_plp_chunk),
    /// - [`CursorColumn::AlreadyConsumed`] if `target` was already read/skipped
    ///   (including after the whole row has been consumed),
    /// - [`CursorColumn::RowEnded`] when no row is positioned.
    ///
    /// # Errors
    ///
    /// Returns `UsageError` when `target` is out of range **and** a row is
    /// still positioned with unread columns (partially read). When no row is
    /// positioned — including after the final column has been read — an
    /// out-of-range or backward `target` yields [`CursorColumn::RowEnded`]
    /// instead of an error.
    ///
    /// # Notes
    ///
    /// Reading the final column advances the cursor to idle, so a subsequent
    /// backward or out-of-range request returns [`CursorColumn::RowEnded`]. A
    /// caller that needs to distinguish "I rewound past the last column" from
    /// "no row is positioned" must track the column it last read itself (as the
    /// ODBC layer does).
    ///
    /// A `true` return from [`next_row_cursor`](Self::next_row_cursor) does not
    /// guarantee `read_row_column(0)` yields a value: a zero-column row is
    /// positioned with a column count of 0, so `read_row_column(0)` is
    /// out-of-range and returns `UsageError`.
    #[instrument(skip(self), level = "info")]
    pub async fn read_row_column(&mut self, target: usize) -> TdsResult<CursorColumn> {
        match std::mem::replace(&mut self.active_row_read_state, ActiveRowReadState::Idle) {
            ActiveRowReadState::Idle => Ok(CursorColumn::RowEnded),
            ActiveRowReadState::RowPaused(pause_state) => {
                self.resume_to_column(*pause_state, target).await
            }
            ActiveRowReadState::PlpPaused(mut plp_state) => {
                self.drain_active_plp(&mut plp_state).await?;
                self.resume_to_column(plp_state.row_pause_state, target)
                    .await
            }
        }
    }

    async fn resume_to_column(
        &mut self,
        pause_state: RowPauseState,
        target: usize,
    ) -> TdsResult<CursorColumn> {
        let column_count = pause_state.columns().len();
        if target >= column_count {
            // Out-of-range: decoding with ColumnPolicy::DecodeOne(target) would skip
            // every remaining column and report RowWritten, silently consuming
            // the row. Reject without touching the transport and keep the
            // cursor positioned so valid pulls still work. ODBC validates the
            // column index first, so this guards the public API against other
            // callers.
            self.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(pause_state));
            return Err(UsageError(format!(
                "read_row_column target column {target} is out of range (row has {column_count} columns)"
            )));
        }

        if target < pause_state.next_column_index {
            // Forward-only: the target column's bytes are already gone. Keep the
            // cursor where it is so later (valid) pulls still work.
            self.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(pause_state));
            return Ok(CursorColumn::AlreadyConsumed);
        }

        let mut capture = DefaultRowWriter::new(1);
        let start = Instant::now();
        let result = self
            .transport
            .resume_row_into(
                pause_state,
                self.remaining_request_timeout,
                self.cancel_handle.as_ref(),
                ColumnPolicy::DecodeOne(target),
                &mut capture,
            )
            .await?;
        self.update_remaining_timeout(start);

        match result {
            RowReadResult::RowPaused(next_pause) => {
                self.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(next_pause));
                let variant_base = capture.variant_base(0);
                let value = capture.take_row().into_iter().next().ok_or_else(|| {
                    crate::error::Error::ProtocolError(format!(
                        "Decoder produced no value for non-null column {target}"
                    ))
                })?;
                Ok(CursorColumn::Value {
                    value,
                    variant_base,
                })
            }
            RowReadResult::RowWritten => {
                // `target` was the last column; the row is now fully consumed.
                // Advance the cursor to idle — a later out-of-range or backward
                // pull reports `RowEnded`. Callers needing to distinguish a
                // rewind from "no row positioned" track the column themselves.
                self.active_row_read_state = ActiveRowReadState::Idle;
                let variant_base = capture.variant_base(0);
                let value = capture.take_row().into_iter().next().ok_or_else(|| {
                    crate::error::Error::ProtocolError(format!(
                        "Decoder produced no value for non-null column {target}"
                    ))
                })?;
                Ok(CursorColumn::Value {
                    value,
                    variant_base,
                })
            }
            RowReadResult::PlpPaused(plp_state) => {
                let collation = plp_state.collation();
                self.active_row_read_state = ActiveRowReadState::PlpPaused(Box::new(plp_state));
                Ok(CursorColumn::PlpStreaming { collation })
            }
            RowReadResult::Token(_) => Err(crate::error::Error::ProtocolError(
                "Unexpected token while resuming to a target column".to_string(),
            )),
        }
    }

    /// Drains any partially read current row so the stream is aligned on the
    /// next ROW/NBCROW (or terminating) token. Skips remaining column bytes
    /// without materializing them.
    async fn drain_active_row_if_needed(&mut self) -> TdsResult<()> {
        if matches!(self.active_row_read_state, ActiveRowReadState::Idle) {
            return Ok(());
        }

        // Mixing the pull and push APIs is rare. Keep that large continuation
        // out of every ordinary row-fetch future, paying only on this recovery path.
        Box::pin(self.drain_active_row()).await
    }

    async fn drain_active_row(&mut self) -> TdsResult<()> {
        match std::mem::replace(&mut self.active_row_read_state, ActiveRowReadState::Idle) {
            ActiveRowReadState::Idle => Ok(()),
            ActiveRowReadState::RowPaused(pause_state) => {
                let mut sink = DiscardRowWriter;
                self.resume_row_loop(*pause_state, ColumnPolicy::SkipAll, &mut sink)
                    .await?;
                Ok(())
            }
            ActiveRowReadState::PlpPaused(mut plp_state) => {
                self.drain_active_plp(&mut plp_state).await?;
                let mut sink = DiscardRowWriter;
                self.resume_row_loop(plp_state.row_pause_state, ColumnPolicy::SkipAll, &mut sink)
                    .await?;
                Ok(())
            }
        }
    }

    /// Reads and discards all remaining bytes of an active PLP stream.
    ///
    /// The scratch buffer is heap-allocated rather than a stack array: it is live
    /// across the await below, so a stack array would be stored inline in this
    /// future and propagate into every caller that awaits it — `read_row_column`
    /// directly, plus `drain_rows`, `next_row_into` and `next_row_cursor` via
    /// `drain_active_row`. Abandoning a partially read PLP column is rare and
    /// already network-bound, so one allocation there is negligible; an 8 KiB
    /// per-row state machine is not.
    async fn drain_active_plp(&mut self, plp_state: &mut PlpPauseState) -> TdsResult<()> {
        let mut buffer = vec![0u8; 8192];
        while !plp_state.reached_end() {
            let start = Instant::now();
            let read = self
                .transport
                .read_active_plp_bytes(
                    plp_state,
                    self.remaining_request_timeout,
                    self.cancel_handle.as_ref(),
                    &mut buffer,
                )
                .await?;
            self.update_remaining_timeout(start);

            if read == 0 && !plp_state.reached_end() {
                return Err(crate::error::Error::ProtocolError(
                    "Active PLP drain made no progress before end-of-stream".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn resume_row_loop<W>(
        &mut self,
        pause_state: RowPauseState,
        plan: ColumnPolicy,
        writer: &mut W,
    ) -> TdsResult<bool>
    where
        W: RowWriter + Send + ?Sized,
    {
        let current = pause_state;
        let start = Instant::now();
        let result = self
            .transport
            .resume_row_into(
                current,
                self.remaining_request_timeout,
                self.cancel_handle.as_ref(),
                plan,
                writer,
            )
            .await?;
        self.update_remaining_timeout(start);
        match result {
            RowReadResult::RowWritten => {
                writer.end_row();
                info!("Row Received");
                Ok(true)
            }
            RowReadResult::RowPaused(next_pause) => {
                self.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(next_pause));
                Ok(true)
            }
            RowReadResult::PlpPaused(plp_state) => {
                self.active_row_read_state = ActiveRowReadState::PlpPaused(Box::new(plp_state));
                Ok(true)
            }
            RowReadResult::Token(token) => {
                if let Some(has_row) = self.handle_row_read_token(token).await? {
                    Ok(has_row)
                } else {
                    // This should not happen in normal resume flow; keep as a defensive guard.
                    Err(crate::error::Error::ProtocolError(
                        "Unexpected token during row resume".to_string(),
                    ))
                }
            }
        }
    }

    async fn handle_row_read_token(&mut self, token: Tokens) -> TdsResult<Option<bool>> {
        match token {
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
                Ok(Some(false))
            }
            Tokens::Order(order_token) => {
                info!(?order_token);
                Ok(None)
            }
            Tokens::EnvChange(env_change) => {
                info!(?env_change);
                if env_change.sub_type == EnvChangeTokenSubType::ResetConnection {
                    self.on_reset_connection_ack();
                }
                self.execution_context
                    .capture_change_property(&env_change, &mut self.negotiated_settings)?;
                Ok(None)
            }
            Tokens::SessionState(session_state) => {
                self.recovery_context
                    .process_session_state(&session_state)?;
                Ok(None)
            }
            Tokens::ReturnValue(return_value_token) => {
                let return_value = self.finalize_return_value(return_value_token)?;
                self.push_return_value(return_value);
                Ok(None)
            }
            Tokens::Error(error_token) => {
                info!(?error_token);
                let mut all_errors = vec![self.record_error_token(&error_token)];
                let drain_result = self.drain_stream().await;
                // Reset batch state before propagating: the error terminates the
                // batch regardless of whether the drain fully consumed it, so a
                // subsequent `close_query` / `advance` does not block trying to
                // read a stream we have given up on.
                self.execution_context.set_has_open_batch(false);
                self.current_result_set_has_been_read_till_end = true;
                self.current_metadata = None;
                match drain_result {
                    Ok(drain_errors) => all_errors.extend(drain_errors),
                    Err(e) => {
                        warn!(error = ?e, "Drain after statement error failed; connection may not be reusable");
                    }
                }
                Err(crate::error::Error::from_sql_errors(all_errors))
            }
            Tokens::ColMetadata(_) => Err(crate::error::Error::UsageError(
                "Unexpected ColMetadata token encountered while reading rows. \
                     This typically indicates the API was not used correctly - \
                     you may need to call advance_to_rows() to advance to the next result set."
                    .to_string(),
            )),
            Tokens::Info(info_token) => {
                info!(?info_token);
                self.capture_info_message(&info_token);
                Ok(None)
            }
            Tokens::TabName | Tokens::ColInfo => Ok(None),
            _ => Err(crate::error::Error::ProtocolError(format!(
                "Unexpected token while finding the next row: {token:?}"
            ))),
        }
    }

    /// Returns a clone of all [`ReturnValue`]s collected during the current
    /// batch — output parameters and UDF return values.
    ///
    /// Values accumulate as the token stream is read; call this after the
    /// result set is fully consumed (e.g. after [`close_query()`](Self::close_query)
    /// or after [`advance_to_rows()`](Self::advance_to_rows) returns `false`).
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
        self.settle_abandoned_reset_verification();
        self.info_messages.clear();
        // Clear output parameters / return values from the previous command so a
        // fully-navigated prior RPC does not leave `get_return_values()` /
        // `retrieve_output_params()` reporting stale values for this new command.
        self.return_values.clear();
        // Every execution RPC path (plain batch, sp_executesql, sp_execute,
        // sp_prepexec, stored proc) funnels through here, so reset the
        // affected-row count for the new command. A prior DML count must not
        // leak into `SQLRowCount` when this command reports none (DDL /
        // `SET NOCOUNT ON` / SELECT).
        self.last_rows_affected = -1;
        self.dml_result_counts.clear();
    }

    /// The live server handle the client holds for `statement_id`, if any.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
    pub fn prepared_handle_for_test(&self, statement_id: StatementId) -> Option<i32> {
        self.prepared_handles.get(&statement_id).copied()
    }

    fn abort_pending_prepare_capture(&mut self) {
        self.pending_capture = None;
        self.pending_prepared_param_encryption = None;
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
        while self.advance_to_rows().await? {}
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
        self.abort_pending_prepare_capture();
        self.remaining_request_timeout = None;
        self.cancel_handle = None;
        self.active_row_read_state = ActiveRowReadState::Idle;
        self.current_command_ce_setting = ExecutionColumnEncryptionSetting::UseConnectionSetting;
        self.execution_context.set_has_open_batch(false);
        Ok(())
    }

    /// Close the underlying transport, ending the TDS session.
    #[instrument(skip(self), level = "info")]
    pub async fn close_connection(&mut self) -> TdsResult<()> {
        // An intentional close is not a recoverable disconnect: disable recovery
        // so a later call on a lingering handle cannot silently resurrect it.
        self.recovery_context.session_recovery_negotiated = false;
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

    /// Test only.
    /// Returns the number of times this connection has been successfully
    /// recovered after detecting a dead connection.
    ///
    /// The count is incremented each time [`reconnect()`] completes
    /// successfully, including session-state restoration and server-property
    /// validation.
    #[cfg(any(test, feature = "test-util"))]
    #[doc(hidden)]
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
        if self.command_is_busy() {
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
        if self.command_is_busy() {
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
        if self.command_is_busy() {
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
        if self.command_is_busy() {
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
        if self.command_is_busy() {
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
        let metadata = self.next_rowset().await?;
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
            self.observe_response_token(&token)?;

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
                    collected_errors.push(self.record_error_token(&error_token));
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
                        self.on_reset_connection_ack();
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

    fn next_row(&mut self) -> impl Future<Output = TdsResult<Option<Vec<ColumnValues>>>> + Send {
        self.get_next_row()
    }

    fn next_row_into(
        &mut self,
        writer: &mut (dyn RowWriter + Send),
    ) -> impl Future<Output = TdsResult<bool>> + Send {
        TdsClient::next_row_into(self, writer)
    }

    fn maybe_has_unread_rows(&self) -> bool {
        !self.current_result_set_has_been_read_till_end
    }

    fn close(&mut self) -> impl Future<Output = TdsResult<()>> + Send {
        self.close_query()
    }
}

/// Opaque identity for a managed prepared statement.
///
/// Issued by [`TdsClient::execute_prepared`] the first time a statement is
/// materialized, and used to key the issuing client's private server-handle
/// map. Ids are unique and never reused within the lifetime of the client that
/// issued them.
///
/// They are **not** globally unique across clients yet (AB#47098): the counter
/// restarts from the same value on each client, so a statement must only be
/// executed or unprepared with the [`TdsClient`] that materialized it. Using it
/// with a different client can alias that client's unrelated handle instead of
/// re-preparing. ODBC upholds this because an `HSTMT` is bound to one
/// connection; direct `mssql-tds` consumers must not share a statement across
/// clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatementId(u64);

#[cfg(any(test, feature = "test-util"))]
impl StatementId {
    /// Builds a `StatementId` from a raw value, for tests that need to seed a
    /// statement's identity without a live server.
    #[doc(hidden)]
    pub fn from_raw_for_test(raw: u64) -> Self {
        Self(raw)
    }
}

/// A logical prepared statement: the SQL to (re)prepare plus its client-issued
/// identity once materialized.
///
/// The server handle is an implementation detail the [`TdsClient`] owns and
/// resolves from [`id`](Self::id) — callers never touch raw handle ids or
/// epochs. If the connection reconnects, the next
/// [`execute_prepared`](TdsClient::execute_prepared) transparently re-prepares
/// against the new session.
///
// Not `Clone`: clones would share one id, so unpreparing either would silently
// unmaterialize the other.
#[derive(Debug)]
pub struct PreparedStatement {
    sql: String,
    id: Option<StatementId>,
}

impl PreparedStatement {
    /// Creates an unmaterialized prepared statement for `sql`. The server handle
    /// is created lazily on the first [`TdsClient::execute_prepared`].
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            id: None,
        }
    }

    /// The client-issued identity, once the statement has been materialized by
    /// [`TdsClient::execute_prepared`]. `None` before the first execute.
    pub fn id(&self) -> Option<StatementId> {
        self.id
    }

    /// Removes and returns the statement's identity, leaving it unmaterialized
    /// with its SQL intact. The caller owns releasing the associated server
    /// handle via [`TdsClient::unprepare`].
    pub fn take_id(&mut self) -> Option<StatementId> {
        self.id.take()
    }

    /// The SQL text this statement prepares.
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

#[cfg(any(test, feature = "test-util"))]
impl PreparedStatement {
    /// Builds a statement already carrying an identity, for tests that need to
    /// exercise handle-lifecycle transitions without a live server.
    #[doc(hidden)]
    pub fn materialized_for_test(sql: impl Into<String>, id: StatementId) -> Self {
        Self {
            sql: sql.into(),
            id: Some(id),
        }
    }
}

/// Per-execution options shared by every `execute*` entry point on
/// [`TdsClient`].
///
/// All fields default to "inherit the connection's behavior", so
/// [`ExecuteOptions::default()`] (or passing `()`, which converts via
/// [`From`]) reproduces the implicit defaults. New per-command capabilities are
/// added here as new defaulted fields — never as new methods or changed
/// signatures — keeping the `execute*` surface forward-compatible.
#[derive(Default, Clone)]
pub struct ExecuteOptions<'a> {
    /// Per-request timeout in seconds. Both `None` and `Some(0)` mean no
    /// client-side timeout (unlimited); a positive value bounds the command,
    /// with connection-recovery time charged against it.
    pub timeout: Option<u32>,
    /// Optional [`CancelHandle`] for cooperative cancellation. A child token is
    /// derived so cancelling aborts the request without tearing down the
    /// connection.
    pub cancel: Option<&'a CancelHandle>,
    /// Per-command Always Encrypted override. Defaults to
    /// [`ExecutionColumnEncryptionSetting::UseConnectionSetting`] (inherit the
    /// connection). Only has effect when the server acknowledged the Column
    /// Encryption feature during login.
    pub column_encryption: ExecutionColumnEncryptionSetting,
}

impl<'a> ExecuteOptions<'a> {
    /// Creates default options (inherit all connection behavior).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a per-request timeout, in seconds. `0` means unlimited (no
    /// deadline), the same as the `None` default — including on the managed
    /// [`execute_prepared`](TdsClient::execute_prepared) /
    /// [`unprepare`](TdsClient::unprepare) paths, where `timeout_secs(0)` now
    /// runs unbounded instead of returning a timeout error.
    pub fn timeout_secs(mut self, seconds: u32) -> Self {
        self.timeout = Some(seconds);
        self
    }

    /// Attaches a cancellation handle.
    pub fn cancel(mut self, handle: &'a CancelHandle) -> Self {
        self.cancel = Some(handle);
        self
    }

    /// Overrides the Always Encrypted behavior for this command only.
    pub fn column_encryption(mut self, setting: ExecutionColumnEncryptionSetting) -> Self {
        self.column_encryption = setting;
        self
    }
}

impl From<()> for ExecuteOptions<'_> {
    fn from(_: ()) -> Self {
        Self::default()
    }
}

/// The result the client is positioned on after an `execute*` call or an
/// [`advance()`](TdsClient::advance).
///
/// This is the lossless, statement-wise view of a batch: every statement that
/// returns rows, carries a row count, or produced a message is surfaced as its
/// own result (matching msodbcsql's `SQLMoreResults` and JDBC's
/// `getMoreResults`/`getUpdateCount`). Consumers that only care about
/// row-returning result sets can collapse no-row statements with
/// [`advance_to_rows()`](TdsClient::advance_to_rows). A pure no-op statement
/// with neither a count nor a message (e.g. a bare `CREATE TABLE`) is collapsed
/// and never surfaces as [`NoRows`](Self::NoRows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementResult {
    /// A row-returning result set (e.g. `SELECT`). Column metadata is available
    /// via [`TdsClient::get_metadata`] and rows via the [`ResultSet`] API.
    Rows,
    /// A statement that produced no result set but is still individually
    /// navigable. `rows_affected` is `Some(n)` when the statement's DONE token
    /// carried a row count (DML), or `None` for a message-only statement
    /// (`PRINT` / low-severity `RAISERROR`) or plain DDL. Messages are drained
    /// separately via [`take_info_messages`](TdsClient::take_info_messages).
    NoRows {
        /// Rows affected, when the DONE token carried a COUNT; otherwise `None`.
        rows_affected: Option<u64>,
    },
    /// No more statements remain in the batch; the connection is idle.
    End,
}

/// Internal boundary kind produced by
/// [`advance_to_result_boundary`](TdsClient::advance_to_result_boundary),
/// before it is mapped to the public [`StatementResult`].
#[derive(Debug)]
enum ResultBoundaryKind {
    /// A row-returning result set; carries its column metadata.
    RowSet(Arc<ColMetadataToken>),
    /// A no-row statement (DML count, message-only, or DDL).
    NoRows { rows_affected: Option<u64> },
    /// End of batch.
    End,
}

/// Async result set iteration through statically dispatched futures.
///
/// The returned futures are native, unboxed futures with an explicit [`Send`]
/// guarantee.
///
/// # Dyn compatibility
///
/// This trait is intentionally not dyn-compatible and cannot be used through
/// `dyn ResultSet`. This is a breaking change for trait-object consumers and for
/// implementations written with `#[async_trait]`; concrete call sites can keep
/// awaiting the methods unchanged, while implementations must return native
/// `Send` futures.
pub trait ResultSet {
    /// Returns the metadata of the result set.
    /// This metadata includes information about the columns in the result set.
    fn get_metadata(&self) -> &Vec<ColumnMetadata>;

    /// Returns the next row of data as a vector of column values.
    /// If there is no more data, it returns None.
    fn next_row(&mut self) -> impl Future<Output = TdsResult<Option<Vec<ColumnValues>>>> + Send;

    /// Decodes the next row directly into a [`RowWriter`], returning `true` if
    /// a row was written or `false` when the result set is exhausted.
    ///
    /// This is the bulk push path: it decodes a full row into `writer` and does
    /// not pause. The ODBC column-at-a-time pull path uses the client cursor
    /// (`next_row_cursor` / `read_row_column`) instead.
    /// Concrete [`TdsClient`] receivers use [`TdsClient::next_row_into`] to keep
    /// the writer statically dispatched. Calls through this trait remain
    /// statically dispatched because [`ResultSet`] is not dyn-compatible.
    ///
    /// # Errors
    ///
    /// Returns a usage error if called while a pull-cursor row is still
    /// partially read. Draining that row here would silently discard it and
    /// return the *next* one, so callers must first finish the row with
    /// `next_row_cursor`. A fully-consumed or absent row is fine.
    fn next_row_into(
        &mut self,
        writer: &mut (dyn RowWriter + Send),
    ) -> impl Future<Output = TdsResult<bool>> + Send;

    /// Returns `true` if the result set may still contain unread rows.
    fn maybe_has_unread_rows(&self) -> bool;

    /// Iterates over the result set, and marks it as closed. After calling close, the next_row method,
    /// will always return None.
    fn close(&mut self) -> impl Future<Output = TdsResult<()>> + Send;
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
    use crate::io::token_stream::{
        ColumnPolicy, ParserContext, RowHeader, RowPauseState, RowReadResult, TdsTokenStreamReader,
    };
    use crate::message::parameters::rpc_parameters::StreamedSqlType;
    use crate::test_client_support::byte_stream::tds_client_over_raw_bytes as client_over_bytes;
    use crate::test_client_support::byte_stream::tds_client_over_raw_bytes_with_column_encryption as client_over_bytes_with_ae;
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
        /// Mirrors the production writer's dispatch record so client tests can
        /// drive acknowledgement verification without a real socket.
        reset_dispatched: bool,

        /// Every byte handed to `send` (request framing + payload), so tests can
        /// assert what was actually written to the wire.
        sent: Arc<std::sync::Mutex<Vec<u8>>>,
        packet_data: Vec<u8>,
        packet_pos: usize,
        /// Results the mock replays from `resume_row_into`, so tests can drive
        /// `read_row_column` down a specific arm (e.g. a `PlpPaused` result that
        /// makes the cursor emit `CursorColumn::PlpStreaming`).
        resume_results: VecDeque<RowReadResult>,
        /// When set, the next (and every subsequent) `send` fails, simulating a
        /// mid-message wire failure. Shared so a test can flip it after setup.
        send_should_fail: Arc<std::sync::atomic::AtomicBool>,
        /// Cached liveness flag toggled by `mark_known_dead`, surfaced through
        /// `connection_known_dead` so tests can assert the fatal-error path.
        known_dead: bool,
    }

    impl TestTransport {
        fn new() -> Self {
            Self {
                closed: false,
                pending_tokens: VecDeque::new(),
                reset_mode: ResetConnectionMode::None,
                reset_dispatched: false,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                packet_data: Vec::new(),
                packet_pos: 0,
                resume_results: VecDeque::new(),
                send_should_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                known_dead: false,
            }
        }

        fn with_tokens(tokens: Vec<Tokens>) -> Self {
            Self {
                closed: false,
                pending_tokens: VecDeque::from(tokens),
                reset_mode: ResetConnectionMode::None,
                reset_dispatched: false,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                packet_data: Vec::new(),
                packet_pos: 0,
                resume_results: VecDeque::new(),
                send_should_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                known_dead: false,
            }
        }

        fn with_packet_data(packet_data: Vec<u8>) -> Self {
            Self {
                closed: false,
                pending_tokens: VecDeque::new(),
                reset_mode: ResetConnectionMode::None,
                reset_dispatched: false,
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                packet_data,
                packet_pos: 0,
                resume_results: VecDeque::new(),
                send_should_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                known_dead: false,
            }
        }

        fn take_packet_bytes(&mut self, count: usize) -> TdsResult<&[u8]> {
            if self.packet_pos + count > self.packet_data.len() {
                return Err(crate::error::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "End of data",
                )));
            }
            let slice = &self.packet_data[self.packet_pos..self.packet_pos + count];
            self.packet_pos += count;
            Ok(slice)
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
            _plan: ColumnPolicy,
            _writer: &mut (dyn RowWriter + Send),
        ) -> TdsResult<RowReadResult> {
            // The mock has no row bytes to materialize, so it replays the queued
            // tokens as control tokens (e.g. a terminal DONE). This lets drain
            // paths — which read rows until the result set's DONE — be exercised
            // without a live server. Running dry is a protocol error, mirroring a
            // closed connection.
            if let Some(tok) = self.pending_tokens.pop_front() {
                return Ok(RowReadResult::Token(tok));
            }
            Err(crate::error::Error::ConnectionClosed("test".to_string()))
        }

        async fn receive_row_header(
            &mut self,
            _context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
        ) -> TdsResult<RowHeader> {
            // Mirrors `receive_row_into`: the mock replays queued control tokens
            // and has no row bytes, so a row header is only ever a `Token`.
            if let Some(tok) = self.pending_tokens.pop_front() {
                return Ok(RowHeader::Token(tok));
            }
            Err(crate::error::Error::ConnectionClosed("test".to_string()))
        }

        async fn resume_row_into(
            &mut self,
            _pause_state: RowPauseState,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
            _plan: ColumnPolicy,
            _writer: &mut (dyn RowWriter + Send),
        ) -> TdsResult<RowReadResult> {
            if let Some(result) = self.resume_results.pop_front() {
                return Ok(result);
            }
            Err(crate::error::Error::ConnectionClosed("test".to_string()))
        }

        async fn read_active_plp_bytes(
            &mut self,
            _plp_state: &mut PlpPauseState,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
            _out: &mut [u8],
        ) -> TdsResult<usize> {
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
            if self.closed {
                return Err(crate::error::Error::ConnectionClosed(
                    "test transport is closed".to_string(),
                ));
            }
            if self
                .send_should_fail
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(crate::error::Error::ConnectionClosed(
                    "injected send failure".to_string(),
                ));
            }
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
            self.reset_dispatched = false;
        }
        fn take_reset_mode(&mut self) -> ResetConnectionMode {
            std::mem::replace(&mut self.reset_mode, ResetConnectionMode::None)
        }
        fn note_reset_dispatched(&mut self) {
            self.reset_dispatched = true;
        }
        fn take_reset_dispatched(&mut self) -> bool {
            std::mem::replace(&mut self.reset_dispatched, false)
        }
    }

    #[async_trait]
    impl NetworkReader for TestTransport {
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
            self.closed
        }
        fn connection_known_dead(&self) -> bool {
            self.known_dead
        }
        fn mark_known_dead(&mut self) {
            self.known_dead = true;
        }
    }

    impl crate::io::packet_reader::TdsPacketReader for TestTransport {
        async fn read_byte(&mut self) -> TdsResult<u8> {
            Ok(self.take_packet_bytes(1)?[0])
        }
        async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
            unimplemented!("TestTransport")
        }
        async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
            unimplemented!("TestTransport")
        }
        async fn read_uint40(&mut self) -> TdsResult<u64> {
            unimplemented!("TestTransport")
        }
        async fn read_float32(&mut self) -> TdsResult<f32> {
            unimplemented!("TestTransport")
        }
        async fn read_float64(&mut self) -> TdsResult<f64> {
            unimplemented!("TestTransport")
        }
        async fn read_int16(&mut self) -> TdsResult<i16> {
            unimplemented!("TestTransport")
        }
        async fn read_uint16(&mut self) -> TdsResult<u16> {
            unimplemented!("TestTransport")
        }
        async fn read_uint24(&mut self) -> TdsResult<u32> {
            unimplemented!("TestTransport")
        }
        async fn read_int32(&mut self) -> TdsResult<i32> {
            unimplemented!("TestTransport")
        }
        async fn read_uint32(&mut self) -> TdsResult<u32> {
            unimplemented!("TestTransport")
        }
        async fn read_int64(&mut self) -> TdsResult<i64> {
            Ok(i64::from_le_bytes(
                self.take_packet_bytes(8)?.try_into().unwrap(),
            ))
        }
        async fn read_uint64(&mut self) -> TdsResult<u64> {
            unimplemented!("TestTransport")
        }
        async fn read_bytes(&mut self, _buf: &mut [u8]) -> TdsResult<usize> {
            unimplemented!("TestTransport")
        }
        async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            unimplemented!("TestTransport")
        }
        async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            unimplemented!("TestTransport")
        }
        async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
            unimplemented!("TestTransport")
        }
        async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
            unimplemented!("TestTransport")
        }
        async fn read_unicode(&mut self, _len: usize) -> TdsResult<String> {
            unimplemented!("TestTransport")
        }
        async fn read_unicode_with_byte_length(&mut self, _len: usize) -> TdsResult<String> {
            unimplemented!("TestTransport")
        }
        async fn skip_bytes(&mut self, _count: usize) -> TdsResult<()> {
            unimplemented!("TestTransport")
        }
        async fn cancel_read_stream(&mut self) -> TdsResult<()> {
            unimplemented!("TestTransport")
        }
        fn reset_reader(&mut self) {
            self.packet_pos = 0;
        }
    }

    fn create_test_client() -> TdsClient {
        create_test_client_with_transport(TestTransport::new())
    }

    fn create_test_client_with_transport(transport: TestTransport) -> TdsClient {
        let transport = AnyTransport::dynamic(transport);
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        )
    }

    #[test]
    fn session_recovery_wiring_from_negotiated_settings() {
        use crate::message::features::session_recovery::SessionRecoveryFeature;
        use crate::message::login::Feature;

        let transport = AnyTransport::dynamic(TestTransport::new());
        let mut negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let mut feature = SessionRecoveryFeature::new(1);
        feature.set_acknowledged(true);
        feature.deserialize(&[0x07, 0x02, 0x01, 0x02]).unwrap();
        negotiated_settings
            .session_settings
            .supported_features
            .push(Box::new(feature));

        let client = TdsClient::new(
            transport,
            negotiated_settings,
            crate::connection::execution_context::ExecutionContext::new(),
            ClientContext::with_data_source("tcp:localhost,1433"),
            Vec::new(),
        );

        // The ack turns session_recovery_negotiated on (previously only ever set
        // in tests) and seeds the baseline state for replay on reconnect.
        assert!(client.recovery_context.session_recovery_negotiated);
        assert_eq!(
            client.recovery_context.session_state_table.initial_state[7]
                .as_ref()
                .unwrap()
                .data,
            vec![0x01, 0x02]
        );
    }

    /// Guards the fix for #225: a large local held across an `.await` in
    /// `drain_active_plp` is stored inline in that future and propagates into
    /// every caller in the await chain, costing a memcpy per row on the hot path.
    ///
    /// Under `cfg(test)`, `AnyTransport` includes its dynamic fallback, so these
    /// sizes conservatively include the larger `Either` representation. A
    /// regression can therefore originate in the test-only arm even when the
    /// production future remains smaller.
    #[test]
    fn row_fetch_futures_stay_small() {
        const MAX: usize = 4096;

        let mut client = create_test_client();
        let mut sink = DiscardRowWriter;
        let mut plp_out = [0u8; 64];

        // Constructing an async fn's future runs none of its body, so these are
        // free to build and drop unpolled. Each borrow ends with its statement.
        let next_row_cursor = std::mem::size_of_val(&client.next_row_cursor());
        let read_row_column = std::mem::size_of_val(&client.read_row_column(0));
        let drain_rows = std::mem::size_of_val(&client.drain_rows());
        let next_row_into = std::mem::size_of_val(&client.next_row_into(&mut sink));
        let next_row_into_dyn =
            std::mem::size_of_val(&client.next_row_into(&mut sink as &mut (dyn RowWriter + Send)));
        let read_active_plp_chunk =
            std::mem::size_of_val(&client.read_active_plp_chunk(&mut plp_out));

        for (name, size) in [
            ("next_row_cursor", next_row_cursor),
            ("read_row_column", read_row_column),
            ("drain_rows", drain_rows),
            ("next_row_into", next_row_into),
            ("next_row_into (dyn writer)", next_row_into_dyn),
            ("read_active_plp_chunk", read_active_plp_chunk),
        ] {
            assert!(
                size <= MAX,
                "{name} future is {size} B, expected <= {MAX} B"
            );
        }

        let native_next_row = std::mem::size_of_val(&client.get_next_row());
        let result_set_next_row = std::mem::size_of_val(&ResultSet::next_row(&mut client));
        let native_next_row_into =
            std::mem::size_of_val(&client.next_row_into(&mut sink as &mut (dyn RowWriter + Send)));
        let result_set_next_row_into =
            std::mem::size_of_val(&ResultSet::next_row_into(&mut client, &mut sink));

        assert_eq!(
            result_set_next_row, native_next_row,
            "ResultSet::next_row must forward the native future without boxing"
        );
        assert_eq!(
            result_set_next_row_into, native_next_row_into,
            "ResultSet::next_row_into must forward the native future without boxing"
        );

        // `close` is not checked against the per-row budget because it runs only
        // once per result set; its larger future does not affect row iteration.
        for (name, size) in [
            ("ResultSet::next_row", result_set_next_row),
            ("ResultSet::next_row_into", result_set_next_row_into),
        ] {
            assert!(
                size <= MAX,
                "{name} future is {size} B, expected <= {MAX} B"
            );
        }
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
        let transport = AnyTransport::dynamic(TestTransport::with_tokens(tokens));
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        )
    }

    /// Builds a client whose transport replays `tokens` and captures every byte
    /// written to the wire, returning the shared capture buffer alongside it.
    fn create_capturing_client(tokens: Vec<Tokens>) -> (TdsClient, Arc<std::sync::Mutex<Vec<u8>>>) {
        let transport = TestTransport::with_tokens(tokens);
        let sent = Arc::clone(&transport.sent);
        let transport = AnyTransport::dynamic(transport);
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        let client = TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        );
        (client, sent)
    }

    /// Like [`create_capturing_client`], but also returns a shared flag that,
    /// when set, makes the transport's next `send` fail — used to exercise the
    /// mid-stream abort path of the streamed PLP write.
    fn create_failing_capturing_client(
        tokens: Vec<Tokens>,
    ) -> (TdsClient, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let transport = TestTransport::with_tokens(tokens);
        let fail = std::sync::Arc::clone(&transport.send_should_fail);
        let transport = AnyTransport::dynamic(transport);
        let negotiated_settings =
            crate::handler::handler_factory::create_test_negotiated_settings_internal();
        let execution_context = crate::connection::execution_context::ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        let client = TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        );
        (client, fail)
    }

    fn done_no_more() -> Tokens {
        Tokens::Done(DoneToken {
            status: DoneStatus::FINAL,
            cur_cmd: CurrentCommand::Insert,
            row_count: 0,
        })
    }

    /// A DONE token carrying the `DONE_COUNT` flag (a DML row count). `more`
    /// sets `DONE_MORE` so it is treated as a non-terminal result in a batch.
    fn done_count(cmd: CurrentCommand, rows: u64, more: bool) -> Tokens {
        let status = if more {
            DoneStatus::COUNT | DoneStatus::MORE
        } else {
            DoneStatus::COUNT
        };
        Tokens::Done(DoneToken {
            status,
            cur_cmd: cmd,
            row_count: rows,
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
    /// them the way `advance_to_rows()` does.
    #[tokio::test]
    async fn execute_exposes_each_norow_statement() {
        // Batch: PRINT N'one'; RAISERROR(N'two', 10, 1);
        let mut client = create_test_client_with_tokens(vec![
            info_token(0, 0, "print one"),
            done_more(),
            info_token(50000, 10, "raiserror two"),
            done_no_more(),
        ]);

        // First statement surfaces as its own no-row result.
        let r1 = client
            .execute(
                "PRINT N'one'; RAISERROR(N'two', 10, 1) WITH NOWAIT;".to_string(),
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            r1,
            StatementResult::NoRows {
                rows_affected: None
            }
        );
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
        let r2 = client.advance().await.unwrap();
        assert_eq!(
            r2,
            StatementResult::NoRows {
                rows_affected: None
            }
        );
        let info2 = client.take_info_messages();
        assert!(
            info2.iter().any(|m| m.message == "raiserror two"),
            "second statement's RAISERROR should surface on its own step: {info2:?}"
        );

        // No more statements.
        let r3 = client.advance().await.unwrap();
        assert_eq!(r3, StatementResult::End);
    }

    /// A single no-row statement is exposed once, then the batch ends.
    #[tokio::test]
    async fn execute_single_norow_then_end() {
        let mut client =
            create_test_client_with_tokens(vec![info_token(0, 0, "just a print"), done_no_more()]);

        let r1 = client
            .execute("PRINT N'just a print';".to_string(), ())
            .await
            .unwrap();
        assert_eq!(
            r1,
            StatementResult::NoRows {
                rows_affected: None
            }
        );

        let r2 = client.advance().await.unwrap();
        assert_eq!(r2, StatementResult::End);
    }

    /// Statement-wise navigation collapses pure no-op statements (no row count,
    /// no messages — e.g. `CREATE TABLE`) but surfaces a DML statement's row
    /// count, matching msodbcsql (`CREATE; INSERT; SELECT` exposes the INSERT
    /// count and the SELECT, not the bare CREATE).
    #[tokio::test]
    async fn execute_collapses_noop_surfaces_rowcount() {
        let mut client = create_test_client_with_tokens(vec![
            done_more(),             // pure no-op (CREATE) - collapsed
            done_more_with_count(5), // DML with a row count - surfaced
            done_no_more(),          // trailing no-op - collapsed -> End
        ]);

        let r1 = client
            .execute(
                "CREATE TABLE #t(i int); INSERT INTO #t VALUES(1);".to_string(),
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            r1,
            StatementResult::NoRows {
                rows_affected: Some(5)
            }
        );

        let r2 = client.advance().await.unwrap();
        assert_eq!(r2, StatementResult::End);
    }

    #[tokio::test]
    async fn advance_end_when_no_open_batch() {
        let mut client = create_test_client();
        assert_eq!(client.advance().await.unwrap(), StatementResult::End);
    }

    /// Regression test for a hang: after positioning on the batch's final row
    /// set, calling `advance` without reading its rows must drain
    /// them, observe the terminal DONE (which closes the batch), and return
    /// `End` — instead of issuing another token read that would block forever on
    /// an already-finished batch. The whole exchange is bounded by a timeout so a
    /// regression surfaces as a test failure rather than a hung suite.
    #[tokio::test]
    async fn move_to_next_statement_end_after_draining_final_rowset() {
        // A single row-returning statement: COLMETADATA then a terminal DONE.
        let mut client = create_test_client_with_tokens(vec![empty_col_metadata(), done_no_more()]);

        let first = tokio::time::timeout(
            Duration::from_secs(5),
            client.execute("SELECT 1;".to_string(), ()),
        )
        .await
        .expect("execute should not hang")
        .unwrap();
        assert_eq!(first, StatementResult::Rows);

        // Advance without fetching any rows: the drain consumes the terminal
        // DONE and the call must report end-of-batch rather than block.
        let next = tokio::time::timeout(Duration::from_secs(5), client.advance())
            .await
            .expect("advance must not hang after draining the final row set")
            .unwrap();
        assert_eq!(next, StatementResult::End);
    }

    #[test]
    fn caller_zero_timeout_becomes_explicit_infinite_budget() {
        let budget = TdsClient::deduct_timeout(Some(0), Duration::ZERO);
        assert_eq!(budget, CommandTimeoutBudget::None);
        let resolved = budget.into_timeout().unwrap();
        assert_eq!(resolved.seconds(), None);
        assert_eq!(resolved.duration(), None);
    }

    // ── PLP streaming lifecycle contract tests ──

    #[tokio::test]
    async fn plp_read_bytes_no_active_stream_errors() {
        let mut client = create_test_client();
        let mut buf = [0u8; 4];

        // With no PLP stream positioned this is a sequencing error, not EOF:
        // an `Ok(0)` here would be indistinguishable from an exhausted stream.
        let err = client
            .read_active_plp_bytes(&mut buf)
            .await
            .expect_err("read with no active PLP stream must error");

        assert!(
            matches!(err, UsageError(_)),
            "expected UsageError, got {err:?}"
        );
    }

    #[test]
    fn plp_reached_end_returns_true_when_no_stream_active() {
        let client = create_test_client();

        assert!(client.active_plp_reached_end());
    }

    #[tokio::test]
    async fn read_row_column_carries_stream_collation_on_plp_variant() {
        // When resuming to a PLP column, the fixed stream collation must ride on
        // the `CursorColumn::PlpStreaming` variant so callers get it atomically
        // with the "this is a PLP column" signal, without a separate lookup.
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let metadata = crate::query::metadata::ColumnMetadata {
            user_type: 0,
            flags: 0,
            type_info: crate::datatypes::sqldatatypes::TypeInfo::partial_len(
                crate::datatypes::sqldatatypes::TdsDataType::BigVarChar,
                0xFFFF,
                Some(collation),
            )
            .unwrap(),
            data_type: crate::datatypes::sqldatatypes::TdsDataType::BigVarChar,
            column_name: "c1".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        };
        let mut reader = TestTransport::with_packet_data((-2_i64).to_le_bytes().to_vec());
        let plp_stream = crate::datatypes::decoder::PlpColumnStream::begin(&metadata, &mut reader)
            .await
            .unwrap()
            .unwrap();
        let row_metadata = Arc::new(ColMetadataToken {
            column_count: 1,
            columns: vec![metadata.clone()],
            cek_table: vec![],
        });
        let inner_pause_state = RowPauseState {
            next_column_index: 1,
            metadata: Arc::clone(&row_metadata),
            nbc_null_bitmap: None,
            decryptor: None,
        };

        // The mock replays this `PlpPaused` result from `resume_row_into`, so the
        // cursor takes the branch that builds `PlpStreaming { collation }`.
        let mut transport = TestTransport::new();
        transport
            .resume_results
            .push_back(RowReadResult::PlpPaused(PlpPauseState {
                row_pause_state: inner_pause_state,
                plp_stream,
            }));

        let mut client = create_test_client_with_transport(transport);
        client.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(RowPauseState {
            next_column_index: 0,
            metadata: row_metadata,
            nbc_null_bitmap: None,
            decryptor: None,
        }));

        let outcome = client.read_row_column(0).await.unwrap();
        assert_eq!(
            outcome,
            CursorColumn::PlpStreaming {
                collation: Some(collation)
            },
            "PLP stream collation must ride on the variant"
        );
    }

    #[test]
    fn plp_helpers_treat_non_plp_row_pause_as_no_active_stream() {
        let mut client = create_test_client();
        client.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(RowPauseState {
            next_column_index: 1,
            metadata: Arc::new(ColMetadataToken::default()),
            nbc_null_bitmap: None,
            decryptor: None,
        }));

        assert!(client.active_plp_reached_end());
    }

    #[tokio::test]
    async fn next_row_into_rejects_active_pull_cursor_row() {
        // A row parked by the pull cursor (`next_row_cursor`) must not be
        // silently drained by the push path. Mixing the two would discard the
        // parked row and hand back the *next* one, so the earlier
        // `next_row_cursor() == true` would map to a row the caller never sees.
        let mut client = create_test_client();
        client.current_metadata = Some(stale_metadata());
        client.current_result_set_has_been_read_till_end = false;
        client.active_row_read_state = ActiveRowReadState::RowPaused(Box::new(RowPauseState {
            next_column_index: 1,
            metadata: Arc::new(ColMetadataToken::default()),
            nbc_null_bitmap: None,
            decryptor: None,
        }));

        let mut sink = DiscardRowWriter;
        let err = client
            .next_row_into(&mut sink)
            .await
            .expect_err("push path must reject a parked pull-cursor row");
        assert!(
            matches!(err, UsageError(_)),
            "expected UsageError, got {err:?}"
        );

        // The guard must leave the parked row in place (not consume the wire),
        // so the caller can still finish it with `next_row_cursor`.
        assert!(
            matches!(
                client.active_row_read_state,
                ActiveRowReadState::RowPaused(_)
            ),
            "guard must not disturb the parked row state"
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
    async fn next_row_into_is_idempotent_after_end_without_metadata() {
        let mut client = create_test_client();
        client.current_metadata = None;
        client.current_result_set_has_been_read_till_end = true;
        let mut sink = DiscardRowWriter;

        assert!(
            !ResultSet::next_row_into(&mut client, &mut sink)
                .await
                .unwrap()
        );
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

    #[tokio::test]
    async fn consume_done_token_takes_single_counted_done() {
        // A normal (non-distributed) engine reports the bulk-load row count in a
        // single DONE_COUNT token.
        let mut client =
            create_test_client_with_tokens(vec![done_count(CurrentCommand::Insert, 5000, false)]);

        let rows_affected = client.consume_done_token().await.unwrap();

        assert_eq!(rows_affected, 5000);
    }

    #[tokio::test]
    async fn consume_done_token_does_not_double_count_distributed_dones() {
        // Regression for #209: a distributed engine (Fabric Warehouse) acknowledges
        // one bulk load with two DONE_COUNT tokens, each carrying the full count.
        // Summing them would report 2x; the authoritative value is the count itself.
        let mut client = create_test_client_with_tokens(vec![
            done_count(CurrentCommand::Insert, 5000, true),
            done_count(CurrentCommand::Insert, 5000, false),
        ]);

        let rows_affected = client.consume_done_token().await.unwrap();

        assert_eq!(rows_affected, 5000);
    }

    #[tokio::test]
    async fn consume_done_token_ignores_uncounted_dones() {
        // A DONE without the COUNT flag carries a meaningless row_count that must
        // not contribute to the total. This uncounted token deliberately carries a
        // non-zero row_count (7) so the pre-fix summing behavior would report 3007;
        // reporting 3000 proves the has_count() guard, not an incidental zero.
        let mut client = create_test_client_with_tokens(vec![
            Tokens::Done(DoneToken {
                status: DoneStatus::MORE,
                cur_cmd: CurrentCommand::Insert,
                row_count: 7,
            }),
            done_count(CurrentCommand::Insert, 3000, false),
        ]);

        let rows_affected = client.consume_done_token().await.unwrap();

        assert_eq!(rows_affected, 3000);
    }

    /// A single-INT-column row used to drive the streaming bulk-load path.
    struct IntRow(i32);

    #[async_trait]
    impl BulkLoadRow for IntRow {
        async fn write_to_packet(
            &self,
            writer: &mut StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            use crate::datatypes::column_values::ColumnValues;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.0))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn bulk_load_reports_client_rows_not_server_done_count() {
        // Regression for #209: the count reported to callers must be the number
        // of rows the client streamed to the wire — matching
        // `SqlBulkCopy.RowsCopied` and ODBC bcp (`llRowsCopiedInLastBCP`), both
        // of which count outgoing rows client-side — and must never be derived
        // from the server's DONE_COUNT. Distributed engines (Fabric Warehouse,
        // EngineEdition 11) acknowledge one bulk load with more than one
        // DONE_COUNT token, so trusting the server count double-counted.
        //
        // The mock server here returns a deliberately wrong count (999) in each
        // DONE_COUNT token; the client wrote 3 rows and must report exactly 3,
        // which is only possible if the reported value is the client-side count.
        use crate::datatypes::bulk_copy_metadata::{SqlDbType, TypeLength};
        use crate::datatypes::sqldatatypes::TdsDataType;

        let mut client = create_test_client_with_tokens(vec![
            // STEP 2: response to the INSERT BULK preamble command.
            done_no_more(),
            // STEP 5: distributed bulk-load acknowledgement — two DONE_COUNT
            // tokens, each carrying a bogus (non-client) count.
            done_count(CurrentCommand::Insert, 999, true),
            done_count(CurrentCommand::BulkInsert, 999, false),
        ]);

        let column_metadata = vec![
            BulkCopyColumnMetadata::new("id", SqlDbType::Int, TdsDataType::Int4 as u8)
                .with_length(4, TypeLength::Fixed(4)),
        ];

        let rows = vec![IntRow(10), IntRow(20), IntRow(30)];

        let reported = client
            .execute_bulk_load_streaming_zerocopy(
                "#t".to_string(),
                column_metadata,
                BulkCopyOptions::default(),
                None,
                None,
                rows.into_iter(),
                &[],
            )
            .await
            .expect("bulk load should succeed against the mock transport");

        assert_eq!(
            reported, 3,
            "must report the 3 client-written rows, not the server DONE_COUNT (999)"
        );
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

        let statement_id = client.register_prepared_handle_for_test(1);
        client
            .execute_sp_unprepare_for_test(statement_id, ())
            .await
            .unwrap();

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
    async fn check_and_reconnect_is_noop_after_close_connection() {
        let mut client = create_test_client();
        client.recovery_context.session_recovery_negotiated = true;

        client.close_connection().await.unwrap();
        assert!(!client.recovery_context.session_recovery_negotiated);

        // A dead transport must not be resurrected once the caller closed it.
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
        client.transport.close_transport().await.unwrap();
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
        client.transport.close_transport().await.unwrap();
        // The closed TestTransport returns is_connection_dead() = true,
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
        assert_eq!(
            result,
            CommandTimeoutBudget::Remaining(NonZeroU32::new(18).unwrap())
        );
        let resolved = result.into_timeout().unwrap();
        assert_eq!(resolved.seconds(), Some(18));
        assert_eq!(resolved.duration(), Some(Duration::from_secs(18)));
    }

    #[test]
    fn fully_consumed_positive_budget_errors_before_timeout_conversion() {
        let result = TdsClient::deduct_timeout(Some(5), Duration::from_secs(10));
        assert_eq!(result, CommandTimeoutBudget::Exhausted);
        assert!(matches!(
            result.into_timeout(),
            Err(crate::error::Error::TimeoutError(_))
        ));
    }

    #[test]
    fn exact_budget_boundary_is_exhausted_not_infinite() {
        // elapsed == timeout: remaining is exactly 0, which must be Exhausted,
        // never the `Some(0) == infinite` widening the old helper applied.
        let result = TdsClient::deduct_timeout(Some(10), Duration::from_secs(10));
        assert_eq!(result, CommandTimeoutBudget::Exhausted);
        assert!(matches!(
            result.into_timeout(),
            Err(crate::error::Error::TimeoutError(_))
        ));
    }

    #[test]
    fn deduct_timeout_passes_through_none() {
        let result = TdsClient::deduct_timeout(None, Duration::from_secs(10));
        assert_eq!(result, CommandTimeoutBudget::None);
    }

    #[test]
    fn deduct_timeout_zero_elapsed() {
        let result = TdsClient::deduct_timeout(Some(30), Duration::ZERO);
        assert_eq!(
            result,
            CommandTimeoutBudget::Remaining(NonZeroU32::new(30).unwrap())
        );
    }

    #[test]
    fn deduct_timeout_rounds_up_sub_second() {
        // 1.9 seconds elapsed should round up to 2 seconds deducted
        let result = TdsClient::deduct_timeout(Some(30), Duration::from_millis(1900));
        assert_eq!(
            result,
            CommandTimeoutBudget::Remaining(NonZeroU32::new(28).unwrap())
        );
    }

    /// A reconnect that consumes the whole command budget must fail the call
    /// before any bytes reach the wire — not fall through with an unbounded
    /// timeout. This exercises the real `execute` entry point, which the
    /// helper-only `deduct_timeout` tests do not.
    #[tokio::test]
    async fn execute_exhausted_reconnect_budget_writes_no_request() {
        let transport = TestTransport::new();
        let sent = Arc::clone(&transport.sent);
        let mut client = create_test_client_with_transport(transport);
        client.reconnect_elapsed_for_test = Some(Duration::from_secs(1));

        let error = client
            .execute(
                "SELECT 1".to_string(),
                ExecuteOptions::new().timeout_secs(1),
            )
            .await
            .expect_err("execute should fail before serialization");

        assert!(matches!(
            &error,
            crate::error::Error::TimeoutError(crate::error::TimeoutErrorType::String(message))
                if message == "command timeout exhausted"
        ));
        assert!(sent.lock().unwrap().is_empty());
    }

    /// Same guarantee for the cursor RPC family: one representative call-site
    /// covers the shared `into_timeout()?` gate across all cursor operations.
    #[tokio::test]
    async fn cursor_open_exhausted_reconnect_budget_writes_no_request() {
        use crate::connection::cursor_ops::CursorClient;

        let transport = TestTransport::new();
        let sent = Arc::clone(&transport.sent);
        let mut client = create_test_client_with_transport(transport);
        client.reconnect_elapsed_for_test = Some(Duration::from_secs(1));

        let error = client
            .cursor_open(
                "SELECT 1",
                crate::cursor::CursorScrollOption::FORWARD_ONLY,
                crate::cursor::CursorConcurrency::READONLY,
                0,
                Some(1),
                None,
            )
            .await
            .expect_err("cursor_open should fail before serialization");

        assert!(matches!(
            &error,
            crate::error::Error::TimeoutError(crate::error::TimeoutErrorType::String(message))
                if message == "command timeout exhausted"
        ));
        assert!(sent.lock().unwrap().is_empty());
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

    // ── execute_prepared / unprepare: StatementId-keyed handle map ──

    fn sid(raw: u64) -> StatementId {
        StatementId::from_raw_for_test(raw)
    }

    #[tokio::test]
    async fn execute_prepared_open_batch_guard_precedes_recovery_and_map_lookup() {
        let mut client = create_test_client();
        client.execution_context.set_has_open_batch(true);
        let mut statement = PreparedStatement::materialized_for_test("SELECT 1", sid(1));
        client.prepared_handles.insert(sid(1), 55);
        let mut orphaned = Some(sid(9));

        let result = client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await;

        // Re-entering with an open batch is a local usage error reported before
        // any recovery I/O or handle-map lookup, so nothing is consumed.
        assert!(matches!(result, Err(UsageError(_))));
        assert_eq!(orphaned, Some(sid(9)));
        assert_eq!(statement.id(), Some(sid(1)));
        assert_eq!(client.prepared_handles.get(&sid(1)), Some(&55));
        assert_eq!(client.connection_recovery_count(), 0);
    }

    // A piggyback needs a prepare to ride on, so an orphan handed in alongside a
    // still-live statement stays with the caller rather than being dropped.
    #[tokio::test]
    async fn execute_prepared_leaves_orphan_untouched_on_the_execute_path() {
        let mut client = create_test_client();
        let mut statement = PreparedStatement::materialized_for_test("SELECT 1", sid(1));
        client.prepared_handles.insert(sid(1), 55);
        client.prepared_handles.insert(sid(9), 77);
        let mut orphaned = Some(sid(9));

        let result = client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await;

        assert!(result.is_err(), "no server behind the test transport");
        assert_eq!(orphaned, Some(sid(9)));
        assert_eq!(
            client.prepared_handles.get(&sid(9)),
            Some(&77),
            "the orphan is still releasable via unprepare"
        );
    }

    // ── execute_prepared: reuse-vs-reprepare branch ──
    //
    // The RPC name serializes as PROC_ID_SWITCH (0xFFFF) followed by the proc id
    // as i16 LE, so sp_prepexec (13) is `FF FF 0D 00` and sp_execute (12) is
    // `FF FF 0C 00`. Asserting on those bytes pins which branch actually reached
    // the wire, which the returned `StatementResult` alone cannot show.

    #[tokio::test]
    async fn execute_prepared_materializes_an_unmaterialized_statement() {
        let (mut client, sent) = create_capturing_client(vec![
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ]);
        let mut statement = PreparedStatement::new("SELECT 1");
        let mut orphaned = None;

        client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await
            .expect("sp_prepexec should succeed against the queued tokens");

        let statement_id = statement
            .id()
            .expect("the first execute must materialize the statement");
        assert_eq!(client.prepared_handles.get(&statement_id).copied(), Some(9));
        assert!(
            sent.lock()
                .unwrap()
                .windows(4)
                .any(|w| w == [0xFF, 0xFF, 0x0D, 0x00]),
            "a statement with no identity must go out as sp_prepexec"
        );
    }

    #[tokio::test]
    async fn execute_prepared_reprepares_when_the_id_has_no_live_handle() {
        // The state a reconnect leaves behind: the caller's statement keeps its
        // id but the map that resolved it is gone. That id must not address the
        // new session's plan.
        let (mut client, sent) = create_capturing_client(vec![
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ]);
        let stale = sid(99);
        let mut statement = PreparedStatement::materialized_for_test("SELECT 1", stale);
        let mut orphaned = None;

        client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await
            .expect("sp_prepexec should succeed against the queued tokens");

        let statement_id = statement.id().expect("re-prepare must re-materialize");
        assert_ne!(
            statement_id, stale,
            "a re-prepare must issue a fresh identity, not revive the dead one"
        );
        assert_eq!(client.prepared_handles.get(&statement_id).copied(), Some(9));
        assert!(!client.prepared_handles.contains_key(&stale));
        assert!(
            sent.lock()
                .unwrap()
                .windows(4)
                .any(|w| w == [0xFF, 0xFF, 0x0D, 0x00]),
            "an id with no live handle must re-prepare via sp_prepexec"
        );
    }

    #[tokio::test]
    async fn execute_prepared_reuses_a_live_handle_via_sp_execute() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        let statement_id = sid(1);
        let mut statement = PreparedStatement::materialized_for_test("SELECT 1", statement_id);
        client.prepared_handles.insert(statement_id, 55);
        let mut orphaned = None;

        client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await
            .expect("sp_execute should succeed against the queued DONE token");

        assert_eq!(
            statement.id(),
            Some(statement_id),
            "reuse must keep the statement's identity"
        );
        let bytes = sent.lock().unwrap().clone();
        assert!(
            bytes.windows(4).any(|w| w == [0xFF, 0xFF, 0x0C, 0x00]),
            "a live handle must be reused via sp_execute, not re-prepared"
        );
        // The @handle positional: name length 0x00, status 0x00 (NONE — a
        // re-prepare would send 0x01/BY_REF_VALUE here), INTN 0x26, max size
        // 0x04, value length 0x04, then the handle (55) little-endian.
        let expected = [0x00, 0x00, 0x26, 0x04, 0x04, 0x37, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "sp_execute must address the cached handle on the wire"
        );
    }

    // The server can report an error and still return a `@handle` for the plan
    // it allocated. The drain records that handle, so the statement must carry
    // the id naming it or the entry — and the server-side plan — is unreachable.
    #[tokio::test]
    async fn execute_prepared_keeps_the_handle_reachable_when_the_batch_errors() {
        use crate::token::tokens::ErrorToken;

        let mut client = create_test_client_with_tokens(vec![
            Tokens::Error(ErrorToken {
                number: 50000,
                state: 1,
                severity: 16,
                message: "prepexec failed".to_string(),
                server_name: String::new(),
                proc_name: String::new(),
                line_number: 1,
            }),
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ]);
        let mut statement = PreparedStatement::new("SELECT 1");
        let mut orphaned = None;

        let result = client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await;

        assert!(result.is_err(), "the batch reported an error");
        let statement_id = statement
            .id()
            .expect("the statement must carry the id even though the execute failed");
        assert_eq!(
            client.prepared_handles.get(&statement_id).copied(),
            Some(9),
            "the captured handle must stay addressable so unprepare can release it"
        );

        client.close_query().await.ok();
        client.unprepare(statement_id, ()).await.ok();
        assert!(
            !client.prepared_handles.contains_key(&statement_id),
            "the caller can release the plan the failed prepare left behind"
        );
    }

    // A compile failure (bad syntax) returns no `@handle`, so nothing was
    // materialized and the statement must not claim an identity.
    #[tokio::test]
    async fn execute_prepared_leaves_the_statement_unmaterialized_when_no_handle_returns() {
        use crate::token::tokens::ErrorToken;

        let mut client = create_test_client_with_tokens(vec![
            Tokens::Error(ErrorToken {
                number: 102,
                state: 1,
                severity: 15,
                message: "Incorrect syntax near 'SELCT'".to_string(),
                server_name: String::new(),
                proc_name: String::new(),
                line_number: 1,
            }),
            done_no_more(),
        ]);
        let mut statement = PreparedStatement::new("SELCT 1");
        let mut orphaned = None;

        let result = client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await;

        assert!(result.is_err());
        assert_eq!(
            statement.id(),
            None,
            "no handle came back, so the statement was never materialized"
        );
        assert!(client.prepared_handles.is_empty());
    }

    // A multi-statement batch whose *later* statement fails: `execute_prepared`
    // returns Ok on the first result, and the error surfaces on a subsequent
    // drain. The `@handle` trails the whole batch, so capture must survive that
    // error — the drain reads on past it and only the terminal DONE aborts.
    #[tokio::test]
    async fn handle_is_captured_when_a_later_statement_in_the_batch_errors() {
        use crate::token::tokens::ErrorToken;

        let mut client = create_capturing_client(vec![
            done_count(CurrentCommand::Insert, 1, true),
            Tokens::Error(ErrorToken {
                number: 2627,
                state: 1,
                severity: 14,
                message: "Violation of PRIMARY KEY constraint".to_string(),
                server_name: String::new(),
                proc_name: String::new(),
                line_number: 2,
            }),
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ])
        .0;
        let mut statement = PreparedStatement::new("INSERT INTO a ...; INSERT INTO b ...;");
        let mut orphaned = None;

        client
            .execute_prepared(&mut statement, Vec::new(), &mut orphaned, ())
            .await
            .expect("the first statement's result is reached before the error");
        let statement_id = statement.id().expect("materialized on the first result");

        // The error only surfaces here, after execute_prepared already returned.
        assert!(client.advance().await.is_err());

        assert_eq!(
            client.prepared_handles.get(&statement_id).copied(),
            Some(9),
            "the trailing @handle must be captured while draining past the error"
        );
    }

    // ── PreparedStatement lifecycle & the StatementId handle map ──

    #[test]
    fn take_id_clears_and_returns() {
        let mut stmt = PreparedStatement::materialized_for_test("SELECT 1", sid(7));
        assert_eq!(stmt.take_id(), Some(sid(7)));
        assert!(stmt.id().is_none());
        // Second take yields nothing — the identity is gone.
        assert!(stmt.take_id().is_none());
    }

    #[tokio::test]
    async fn unprepare_skips_absent_handle_without_rpc() {
        // The client holds no handle for this id (never materialized, or a
        // reconnect cleared the map): unprepare skips sp_unprepare. The transport
        // has no queued tokens, so a drain would error — reaching Ok proves no RPC
        // was sent.
        let mut client = create_test_client();

        client.unprepare(sid(5), ()).await.unwrap();
    }

    #[tokio::test]
    async fn unprepare_releases_live_handle() {
        // A handle the client still holds is released via sp_unprepare. Asserting
        // on the captured wire bytes proves the RPC was actually sent — a wrongly
        // skipped release would also return Ok, leaving the DONE unread.
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        client.prepared_handles.insert(sid(5), 5);

        client.unprepare(sid(5), ()).await.unwrap();

        // The sp_unprepare @handle positional parameter serializes as: name length
        // 0x00, status 0x00 (NONE), INTN type 0x26, max size 0x04, value length
        // 0x04, then the handle id (5) little-endian.
        let bytes = sent.lock().unwrap().clone();
        let expected = [0x00, 0x00, 0x26, 0x04, 0x04, 0x05, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "a live handle must be released by sending sp_unprepare with its id on the wire"
        );
        // The map entry is evicted once the release is sent.
        assert!(!client.prepared_handles.contains_key(&sid(5)));
    }

    #[tokio::test]
    async fn unprepare_open_batch_returns_usage_error_before_io() {
        // Parity with execute_prepared: the open-batch guard is a local check
        // reported before any recovery I/O.
        let mut client = create_test_client();
        client.execution_context.set_has_open_batch(true);

        let result = client.unprepare(sid(5), ()).await;

        assert!(matches!(result, Err(UsageError(_))));
    }

    #[tokio::test]
    async fn unprepare_propagates_reconnect_error_when_unrecoverable() {
        // Recover-first parity with msodbcsql: a dead, unrecoverable connection
        // (open transaction) fails recovery, so unprepare surfaces that error
        // instead of sending sp_unprepare on a dead socket. The ODBC layer
        // swallows it best-effort.
        let mut client = create_test_client();
        client.prepared_handles.insert(sid(5), 5);
        client.recovery_context.session_recovery_negotiated = true;
        client.execution_context.set_transaction_descriptor(42);

        let result = client.unprepare(sid(5), ()).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unprepare_drops_both_map_entries_even_when_the_rpc_fails() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

        // The transport has no queued tokens, so the sp_unprepare drain fails.
        // `unprepare` consumed the identity, so nothing can reach either entry
        // again — both must be gone rather than leaving the AE entry behind.
        let mut client = create_test_client();
        client.prepared_handles.insert(sid(5), 5);
        client
            .prepared_param_encryption
            .insert(sid(5), Arc::new(DescribeParameterEncryptionResult::new()));

        assert!(client.unprepare(sid(5), ()).await.is_err());

        assert!(!client.prepared_handles.contains_key(&sid(5)));
        assert!(!client.prepared_param_encryption.contains_key(&sid(5)));
    }

    #[test]
    fn push_return_value_routes_handle_into_map_under_pending_id() {
        let mut client = create_test_client();
        client.pending_capture = Some(sid(3));

        client.push_return_value(int_return_value(0, 9));

        assert_eq!(client.prepared_handles.get(&sid(3)).copied(), Some(9));
        // The capture is one-shot.
        assert!(client.pending_capture.is_none());
    }

    #[test]
    fn push_return_value_pins_pending_encryption_metadata() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

        let mut client = create_test_client();
        client.pending_capture = Some(sid(1));
        let describe = Arc::new(DescribeParameterEncryptionResult::new());
        client.pending_prepared_param_encryption = Some(Arc::clone(&describe));

        client.push_return_value(int_return_value(0, 9));

        // Metadata pinning is protocol-owned and completes in the token funnel,
        // keyed by the same StatementId as the handle.
        assert!(Arc::ptr_eq(
            client.prepared_param_encryption.get(&sid(1)).unwrap(),
            &describe
        ));
        assert!(client.pending_prepared_param_encryption.is_none());
    }

    #[test]
    fn reused_server_handle_id_cannot_collide_across_statements() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

        // The server may hand out the same integer handle again after a plan is
        // dropped. Keyed by StatementId, the replacement lands in its own slot
        // instead of overwriting (or being overwritten by) the prior statement's
        // metadata — the aliasing hazard an i32-keyed map required an ordered
        // evict-before-insert to avoid.
        let mut client = create_test_client();
        let dropped = Arc::new(DescribeParameterEncryptionResult::new());
        client.prepared_param_encryption.insert(sid(1), dropped);
        client.prepared_handles.insert(sid(1), 7);

        client.pending_capture = Some(sid(2));
        let replacement = Arc::new(DescribeParameterEncryptionResult::new());
        client.pending_prepared_param_encryption = Some(Arc::clone(&replacement));

        client.push_return_value(int_return_value(0, 7));

        assert!(Arc::ptr_eq(
            client.prepared_param_encryption.get(&sid(2)).unwrap(),
            &replacement
        ));
        assert_eq!(client.prepared_handles.get(&sid(2)).copied(), Some(7));
    }

    #[tokio::test]
    async fn drain_error_after_returned_handle_retains_prepared_metadata() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;
        use crate::token::tokens::ErrorToken;

        let mut client = create_test_client_with_tokens(vec![
            Tokens::Error(ErrorToken {
                number: 50000,
                state: 1,
                severity: 16,
                message: "prepexec failed".to_string(),
                server_name: String::new(),
                proc_name: String::new(),
                line_number: 1,
            }),
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ]);
        client.pending_capture = Some(sid(1));
        let replacement = Arc::new(DescribeParameterEncryptionResult::new());
        client.pending_prepared_param_encryption = Some(Arc::clone(&replacement));

        client.execution_context.set_has_open_batch(true);
        client.current_result_set_has_been_read_till_end = true;
        let result = client.advance().await;

        assert!(result.is_err());
        assert_eq!(client.prepared_handles.get(&sid(1)).copied(), Some(9));
        assert!(Arc::ptr_eq(
            client.prepared_param_encryption.get(&sid(1)).unwrap(),
            &replacement
        ));
        assert!(client.pending_prepared_param_encryption.is_none());
    }

    #[tokio::test]
    async fn returned_handle_is_available_after_terminal_done() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

        let mut client = create_test_client_with_tokens(vec![
            Tokens::ReturnValue(ae_return_value_token("@handle", ColumnValues::Int(9), None)),
            done_no_more(),
        ]);
        client.execution_context.set_has_open_batch(true);
        client.current_result_set_has_been_read_till_end = true;
        client.pending_capture = Some(sid(1));
        let replacement = Arc::new(DescribeParameterEncryptionResult::new());
        client.pending_prepared_param_encryption = Some(Arc::clone(&replacement));

        assert!(matches!(client.advance().await, Ok(StatementResult::End)));

        assert_eq!(client.prepared_handles.get(&sid(1)).copied(), Some(9));
        assert!(Arc::ptr_eq(
            client.prepared_param_encryption.get(&sid(1)).unwrap(),
            &replacement
        ));
    }

    #[tokio::test]
    async fn close_query_error_aborts_pending_prepare_capture() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;

        let mut client = create_test_client();
        client.execution_context.set_has_open_batch(true);
        client.pending_capture = Some(sid(1));
        client.pending_prepared_param_encryption =
            Some(Arc::new(DescribeParameterEncryptionResult::new()));

        assert!(client.close_query().await.is_err());
        assert!(client.pending_capture.is_none());
        assert!(client.pending_prepared_param_encryption.is_none());

        client.push_return_value(int_return_value(0, 9));
        assert!(!client.prepared_handles.contains_key(&sid(1)));
        assert!(!client.prepared_param_encryption.contains_key(&sid(1)));
    }

    #[tokio::test]
    async fn advance_error_aborts_pending_prepare_capture() {
        let mut client = create_test_client();
        client.execution_context.set_has_open_batch(true);
        client.current_result_set_has_been_read_till_end = true;
        client.pending_capture = Some(sid(1));

        assert!(client.advance().await.is_err());
        assert!(client.pending_capture.is_none());
    }

    #[tokio::test]
    async fn next_row_error_aborts_pending_prepare_capture() {
        let mut client = create_test_client();
        client.current_result_set_has_been_read_till_end = false;
        client.pending_capture = Some(sid(1));

        assert!(ResultSet::next_row(&mut client).await.is_err());
        assert!(client.pending_capture.is_none());
    }

    #[tokio::test]
    async fn next_row_into_error_aborts_pending_prepare_capture() {
        let mut client = create_test_client();
        client.current_result_set_has_been_read_till_end = false;
        client.pending_capture = Some(sid(1));
        let mut writer = DiscardRowWriter;

        assert!(
            ResultSet::next_row_into(&mut client, &mut writer)
                .await
                .is_err()
        );
        assert!(client.pending_capture.is_none());
    }

    #[test]
    fn push_return_value_without_pending_capture_leaves_map_untouched() {
        // A reuse (sp_execute) issues no handle and arms no pending capture, so a
        // stray Int return value is surfaced as an output param, not routed into
        // the handle map.
        let mut client = create_test_client();

        client.push_return_value(int_return_value(0, 9));

        assert!(client.prepared_handles.is_empty());
        assert_eq!(
            client.retrieve_output_params().unwrap().map(|v| v.len()),
            Some(1)
        );
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
    async fn assert_no_result_set_clears_metadata<F, T>(invoke: F)
    where
        F: AsyncFnOnce(&mut TdsClient) -> TdsResult<T>,
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
            c.execute("INSERT INTO t VALUES (1)".to_string(), ()).await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_replaces_stale_metadata_when_result_set_returned() {
        let mut client = create_test_client_with_tokens(vec![empty_col_metadata(), done_no_more()]);
        let stale = stale_metadata();
        client.current_metadata = Some(Arc::clone(&stale));

        client
            .execute("SELECT 1".to_string(), ())
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

    // ── SQLRowCount plumbing: last_rows_affected capture / reset ──

    #[tokio::test]
    async fn execute_captures_dml_affected_row_count() {
        let mut client =
            create_test_client_with_tokens(vec![done_count(CurrentCommand::Update, 5, false)]);
        client
            .execute("UPDATE t SET x = 1".to_string(), ())
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), 5);
    }

    #[tokio::test]
    async fn execute_reports_no_row_count_for_ddl() {
        // A DONE without the COUNT flag (DDL / SET NOCOUNT ON) leaves it at -1.
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client
            .execute("CREATE TABLE t(i int)".to_string(), ())
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), -1);
    }

    #[tokio::test]
    async fn execute_reports_no_row_count_for_select() {
        // Landing on COLMETADATA (a forward-only result set) reports -1.
        let mut client = create_test_client_with_tokens(vec![empty_col_metadata(), done_no_more()]);
        client.execute("SELECT 1".to_string(), ()).await.unwrap();
        assert_eq!(client.last_rows_affected(), -1);
    }

    #[tokio::test]
    async fn dml_then_select_batch_reports_no_row_count_for_select() {
        // UPDATE (counted, has_more) then SELECT. Statement-wise, `execute`
        // lands on the UPDATE (count 7); advancing onto the SELECT's COLMETADATA
        // must clear that count so SQLRowCount reports -1 for the forward-only
        // SELECT, not the DML count. (Copilot review AB thread.)
        let mut client = create_test_client_with_tokens(vec![
            done_count(CurrentCommand::Update, 7, true),
            empty_col_metadata(),
            done_no_more(),
        ]);
        client
            .execute("UPDATE t SET x = 1; SELECT * FROM t".to_string(), ())
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), 7);
        // Advance onto the SELECT: COLMETADATA clears the DML count.
        client.advance().await.unwrap();
        assert_eq!(client.last_rows_affected(), -1);
    }

    #[tokio::test]
    async fn row_count_resets_between_commands() {
        // A DML count from one command must not leak into the next command that
        // reports none. begin_command (on every execute* path) resets it.
        let mut client = create_test_client_with_tokens(vec![
            done_count(CurrentCommand::Delete, 4, false),
            done_no_more(),
        ]);
        client
            .execute("DELETE FROM t".to_string(), ())
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), 4);
        // The second command is a DDL (no count) and must start fresh at -1.
        client
            .execute("CREATE TABLE u(i int)".to_string(), ())
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), -1);
    }

    #[tokio::test]
    async fn multi_dml_batch_captures_each_statement_count() {
        // A pure-DML batch surfaces one count per statement, in order. Statement-
        // wise, each count is captured as `execute`/`advance` positions on the
        // next DML boundary.
        let mut client = create_test_client_with_tokens(vec![
            done_count(CurrentCommand::Update, 3, true),
            done_count(CurrentCommand::Delete, 2, true),
            done_count(CurrentCommand::Insert, 1, false),
        ]);
        client
            .execute(
                "UPDATE t SET x=1; DELETE FROM t; INSERT INTO t VALUES (1)".to_string(),
                (),
            )
            .await
            .unwrap();
        assert_eq!(client.last_rows_affected(), 3);
        client.advance().await.unwrap();
        assert_eq!(client.last_rows_affected(), 2);
        client.advance().await.unwrap();
        assert_eq!(client.last_rows_affected(), 1);
        assert_eq!(client.take_dml_result_counts(), vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn select_clears_preceding_dml_counts() {
        // UPDATE (counted, has_more) then SELECT: advancing onto COLMETADATA
        // clears the buffered DML counts so they are not surfaced for the SELECT.
        let mut client = create_test_client_with_tokens(vec![
            done_count(CurrentCommand::Update, 7, true),
            empty_col_metadata(),
            done_no_more(),
        ]);
        client
            .execute("UPDATE t SET x=1; SELECT * FROM t".to_string(), ())
            .await
            .unwrap();
        // Statement-wise: execute stops on the UPDATE; advancing onto the
        // SELECT's COLMETADATA clears the buffered DML counts.
        client.advance().await.unwrap();
        assert!(client.take_dml_result_counts().is_empty());
    }

    #[tokio::test]
    async fn execute_stored_procedure_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_stored_procedure("dbo.do_work".to_string(), None, None, ())
                .await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_sp_executesql_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            c.execute_sp_executesql("UPDATE t SET v = 1".to_string(), Vec::new(), ())
                .await
        })
        .await;
    }

    #[tokio::test]
    async fn execute_sp_prepexec_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            let mut orphan = None;
            c.execute_sp_prepexec_for_test(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                &mut orphan,
                (),
            )
            .await
            .map(|(_, result)| result)
        })
        .await;
    }

    // The `@handle` positional parameter of sp_prepexec serializes as:
    //   0x00  positional name length
    //   0x01  status flags = BY_REF_VALUE
    //   0x26  TYPE_INFO type byte = INTN
    //   0x04  TYPE_INFO max size = 4
    //   value: length byte then little-endian bytes (length 0x00 for NULL).
    // These tests pin the byte the current selection controls: the orphan's
    // live handle becomes the input value of that by-reference `@handle`.

    #[tokio::test]
    async fn execute_sp_prepexec_sends_orphan_handle_as_byref_handle_input() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        let orphan_id = client.issue_statement_id();
        client.prepared_handles.insert(orphan_id, 0x5152_5354);
        let mut orphan = Some(orphan_id);
        client
            .execute_sp_prepexec_for_test(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                &mut orphan,
                (),
            )
            .await
            .expect("sp_prepexec should succeed against the queued DONE token");

        let bytes = sent.lock().unwrap().clone();
        let expected = [0x00, 0x01, 0x26, 0x04, 0x04, 0x54, 0x53, 0x52, 0x51];
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected),
            "the orphan's handle must be sent as the by-reference @handle input so the server \
             drops the prior prepared statement"
        );
    }

    #[tokio::test]
    async fn execute_sp_prepexec_sends_null_handle_when_no_orphan() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        let mut orphan = None;
        client
            .execute_sp_prepexec_for_test(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                &mut orphan,
                (),
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

    // A reconnect clears `prepared_handles`, so an orphan the caller still holds
    // may name a statement with no live handle. That drop is a no-op server-side.
    #[tokio::test]
    async fn execute_sp_prepexec_sends_null_handle_when_orphan_has_no_live_handle() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);
        let mut orphan = Some(client.issue_statement_id());
        client
            .execute_sp_prepexec_for_test(
                "UPDATE t SET v = 1".to_string(),
                Vec::new(),
                &mut orphan,
                (),
            )
            .await
            .expect("sp_prepexec should succeed against the queued DONE token");

        let bytes = sent.lock().unwrap().clone();
        let expected_null = [0x00, 0x01, 0x26, 0x04, 0x00];
        assert!(
            bytes
                .windows(expected_null.len())
                .any(|w| w == expected_null),
            "an orphan with no live handle must send a NULL @handle input"
        );
        assert_eq!(orphan, None, "the orphan is released either way");
    }

    #[tokio::test]
    async fn execute_sp_prepexec_preserves_orphan_on_pre_send_error() {
        let mut client = create_test_client();
        let orphan_id = client.issue_statement_id();
        client.prepared_handles.insert(orphan_id, 7);
        client.execution_context.set_has_open_batch(true);
        let mut orphan = Some(orphan_id);

        let result = client
            .execute_sp_prepexec_for_test("SELECT 1".to_string(), Vec::new(), &mut orphan, ())
            .await;

        assert!(result.is_err());
        assert_eq!(orphan, Some(orphan_id));
        assert_eq!(
            client.prepared_handles.get(&orphan_id),
            Some(&7),
            "a pre-send failure must leave the orphan releasable"
        );
    }

    #[tokio::test]
    async fn execute_sp_prepexec_consumes_orphan_after_send_begins() {
        let mut client = create_test_client();
        let orphan_id = client.issue_statement_id();
        client.prepared_handles.insert(orphan_id, 7);
        let mut orphan = Some(orphan_id);

        let result = client
            .execute_sp_prepexec_for_test("SELECT 1".to_string(), Vec::new(), &mut orphan, ())
            .await;

        assert!(result.is_err());
        assert_eq!(orphan, None);
        assert!(
            !client.prepared_handles.contains_key(&orphan_id),
            "once the drop crosses the send boundary the orphan's entry is dead"
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
        client.pending_capture = Some(sid(1));

        // First value = the sp_prepexec @handle: on the managed path it is
        // diverted into the handle map and NOT surfaced as a user output
        // parameter — mirroring msodbcsql, which routes it to hPrepCurrent.
        client.push_return_value(int_return_value(0, 0x0102_0304));

        assert_eq!(
            client.prepared_handles.get(&sid(1)).copied(),
            Some(0x0102_0304)
        );
        assert!(client.pending_capture.is_none(), "capture is one-shot");
        assert!(
            client.return_values.is_empty(),
            "the diverted handle must not appear in return_values"
        );
        assert!(client.get_return_values().is_empty());

        // Subsequent values are genuine output params and must be surfaced.
        client.push_return_value(int_return_value(1, 7));
        assert_eq!(client.return_values.len(), 1);
        assert!(matches!(
            client.return_values[0].value,
            ColumnValues::Int(7)
        ));
    }

    #[tokio::test]
    async fn execute_sp_execute_clears_stale_metadata_when_no_result_set() {
        assert_no_result_set_clears_metadata(async |c: &mut TdsClient| {
            let statement_id = c.register_prepared_handle_for_test(42);
            c.execute_sp_execute_for_test(statement_id, None, None, ())
                .await
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

    // ── Raw TDS token byte builders (little-endian, matching the real parsers) ──

    fn message_token_bytes(
        token: crate::token::tokens::TokenType,
        number: u32,
        message: &str,
    ) -> Vec<u8> {
        use crate::token::parsers::common::test_utils::MockReader;
        let mut b = vec![token as u8];
        b.extend_from_slice(&0u16.to_le_bytes()); // token length (ignored by parser)
        b.extend_from_slice(&number.to_le_bytes());
        b.push(1); // state
        b.push(16); // severity
        let msg = MockReader::encode_utf16(message);
        b.extend_from_slice(&((msg.len() / 2) as u16).to_le_bytes()); // US_VARCHAR char count
        b.extend_from_slice(&msg);
        b.push(0); // server_name (B_VARCHAR, 0 chars)
        b.push(0); // proc_name (B_VARCHAR, 0 chars)
        b.extend_from_slice(&1u32.to_le_bytes()); // line number
        b
    }

    fn error_token_bytes(number: u32, _severity: u8, message: &str) -> Vec<u8> {
        message_token_bytes(crate::token::tokens::TokenType::Error, number, message)
    }

    fn info_token_bytes(number: u32, message: &str) -> Vec<u8> {
        message_token_bytes(crate::token::tokens::TokenType::Info, number, message)
    }

    fn done_bytes(status: u16) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::Done as u8];
        b.extend_from_slice(&status.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // cur_cmd
        b.extend_from_slice(&0u64.to_le_bytes()); // row_count
        b
    }

    fn int_column_bytes(name: &str) -> Vec<u8> {
        use crate::token::parsers::common::test_utils::MockReader;
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes()); // user_type
        b.extend_from_slice(&0u16.to_le_bytes()); // flags (not nullable)
        b.push(crate::datatypes::sqldatatypes::TdsDataType::Int4 as u8);
        let name_bytes = MockReader::encode_utf16(name);
        b.push((name_bytes.len() / 2) as u8); // name length in chars
        b.extend_from_slice(&name_bytes);
        b
    }

    fn colmetadata_single_int_bytes(name: &str) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::ColMetadata as u8];
        b.extend_from_slice(&1u16.to_le_bytes()); // column count
        b.extend(int_column_bytes(name));
        b
    }

    /// A single-Int COLMETADATA as it appears on the wire when Always Encrypted
    /// is negotiated: an (empty) CEK table — a `u16` count of 0 — sits between
    /// the column count and the first column definition. The column itself is
    /// not encrypted (flags = 0), so it carries no per-column crypto metadata.
    /// Parsing these bytes without column-encryption awareness misreads the
    /// CEK-table prefix as column data and desynchronizes the stream.
    fn colmetadata_single_int_ae_bytes(name: &str) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::ColMetadata as u8];
        b.extend_from_slice(&1u16.to_le_bytes()); // column count
        b.extend_from_slice(&0u16.to_le_bytes()); // empty CEK table
        b.extend(int_column_bytes(name));
        b
    }

    /// A single COLMETADATA under Always Encrypted whose one column is
    /// `FLAG_ENCRYPTED` and carries `CryptoMetadata`, while the CEK table is
    /// empty. This server anomaly makes `resolve_cell_decryptor` fail fast
    /// (encrypted column, no keys to resolve). The outer (ciphertext) wire type
    /// is modeled as Int4 so the row payload is a fixed 4 bytes; on the drain
    /// path the column must be decoded as raw ciphertext without resolving any
    /// decryptor, so this failure is never triggered.
    fn colmetadata_single_encrypted_int_ae_bytes(name: &str) -> Vec<u8> {
        use crate::token::parsers::common::test_utils::MockReader;
        const FLAG_ENCRYPTED: u16 = 0x0800;
        let int4 = crate::datatypes::sqldatatypes::TdsDataType::Int4 as u8;

        let mut b = vec![crate::token::tokens::TokenType::ColMetadata as u8];
        b.extend_from_slice(&1u16.to_le_bytes()); // column count
        b.extend_from_slice(&0u16.to_le_bytes()); // empty CEK table

        // Encrypted column definition.
        b.extend_from_slice(&0u32.to_le_bytes()); // user_type
        b.extend_from_slice(&FLAG_ENCRYPTED.to_le_bytes()); // flags
        b.push(int4); // ciphertext wire type
        // CryptoMetadata (has_cek_table = true, since AE is negotiated).
        b.extend_from_slice(&0u16.to_le_bytes()); // cek_table_ordinal
        b.extend_from_slice(&0u32.to_le_bytes()); // base user_type
        b.push(int4); // base data type
        b.push(2); // cipher_algorithm_id (non-custom: AEAD_AES_256_CBC_HMAC_SHA256)
        b.push(1); // encryption_type (deterministic)
        b.push(1); // normalization_rule_version
        let name_bytes = MockReader::encode_utf16(name);
        b.push((name_bytes.len() / 2) as u8);
        b.extend_from_slice(&name_bytes);
        b
    }

    fn colmetadata_two_int_bytes(first: &str, second: &str) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::ColMetadata as u8];
        b.extend_from_slice(&2u16.to_le_bytes()); // column count
        b.extend(int_column_bytes(first));
        b.extend(int_column_bytes(second));
        b
    }

    fn row_int_bytes(value: i32) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::Row as u8];
        b.extend_from_slice(&value.to_le_bytes()); // non-nullable Int4: 4 raw LE bytes
        b
    }

    fn row_two_int_bytes(a: i32, b_val: i32) -> Vec<u8> {
        let mut b = vec![crate::token::tokens::TokenType::Row as u8];
        b.extend_from_slice(&a.to_le_bytes());
        b.extend_from_slice(&b_val.to_le_bytes());
        b
    }

    /// NBCROW (0xD2) for a single Int4 column whose only value is NULL. The
    /// 1-byte null bitmap has bit 0 set, so no value bytes follow.
    fn nbcrow_single_null_bytes() -> Vec<u8> {
        vec![crate::token::tokens::TokenType::NbcRow as u8, 0b0000_0001]
    }

    const DONE_MORE_ERROR: u16 = 0x0003; // DONE_MORE | DONE_ERROR
    const DONE_FINAL: u16 = 0x0000;

    fn expect_sql_error(err: crate::error::Error) -> crate::error::SqlServerDiagnostics {
        match err {
            crate::error::Error::SqlServerError { diagnostics } => diagnostics,
            other => panic!("expected a SqlServerError, got: {other:?}"),
        }
    }

    /// Parity with go-mssqldb #410: a statement-scoped error (`RAISERROR`)
    /// followed by a row-returning statement (`SELECT ...`) in the same batch.
    /// On the ERROR token, `advance_to_result_boundary` drains the remainder of
    /// the batch via `drain_stream`. The drain must carry the trailing result
    /// set's COLMETADATA into the row-decoding path so the ROW tokens are fully
    /// consumed, letting the real SQL error surface instead of a parse failure.
    #[tokio::test]
    async fn drain_on_error_consumes_trailing_rowset_and_surfaces_sql_error() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom")); // RAISERROR('boom', 16, 1)
        stream.extend(done_bytes(DONE_MORE_ERROR)); // stmt 1 done, batch continues
        stream.extend(colmetadata_single_int_bytes("n")); // SELECT n ...
        stream.extend(row_int_bytes(1)); // one row
        stream.extend(done_bytes(DONE_FINAL)); // end of batch

        let mut client = client_over_bytes(stream);
        let err = client
            .advance_to_result_boundary()
            .await
            .expect_err("a statement error must surface as an error");

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors.len(), 1);
        assert_eq!(diagnostics.errors[0].number, 1222);
        assert_eq!(diagnostics.errors[0].message, "boom");
    }

    /// After the error path drains the batch to its terminal DONE, the client
    /// must be left idle: `advance_to_result_boundary`'s `Error` branch resets
    /// the result-set state so a caller that calls `next_row` after `execute`
    /// returned `Err` does not pass the `maybe_has_unread_rows` guard and read a
    /// stream that is already fully consumed.
    #[tokio::test]
    async fn drain_on_error_leaves_client_idle_for_next_row() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("n"));
        stream.extend(row_int_bytes(1));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        client
            .advance_to_result_boundary()
            .await
            .expect_err("a statement error must surface as an error");

        assert!(
            !client.maybe_has_unread_rows(),
            "drained error path must clear the unread-rows guard"
        );
        assert!(!client.on_rows(), "drained error path must clear metadata");
        assert!(
            !client.has_open_batch(),
            "drained error path must close the batch"
        );
    }

    /// When the trailing result set is truncated mid-drain (the stream ends
    /// before the terminal DONE), the drain fails — but the original SQL error
    /// must still surface as the primary error and the client must be left idle
    /// so the failed drain does not leave the batch marked open. Covers the
    /// `Err` arm of the drain in `advance_to_result_boundary`'s `Error` branch.
    #[tokio::test]
    async fn drain_on_error_truncated_stream_still_surfaces_sql_error() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("n"));
        // Stream ends here: no ROW, no terminal DONE — the drain read fails.

        let mut client = client_over_bytes(stream);
        let err = client
            .advance_to_result_boundary()
            .await
            .expect_err("a statement error must surface as an error");

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
        assert_eq!(diagnostics.errors[0].message, "boom");
        assert!(
            !client.maybe_has_unread_rows(),
            "a failed drain must still clear the unread-rows guard"
        );
        assert!(
            !client.has_open_batch(),
            "a failed drain must still close the batch"
        );
    }

    /// Control: the same statement error followed by a *no-row* statement drains
    /// cleanly and surfaces the real SQL error.
    #[tokio::test]
    async fn drain_on_error_surfaces_sql_error_without_trailing_rowset() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client
            .advance_to_result_boundary()
            .await
            .expect_err("a statement error must surface as an error");

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
    }

    /// The trailing result set can carry multiple multi-column rows; every ROW
    /// must be consumed before the terminal DONE is reached.
    #[tokio::test]
    async fn drain_on_error_consumes_multi_row_multi_column_rowset() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1205, 16, "deadlock"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_two_int_bytes("a", "b"));
        stream.extend(row_two_int_bytes(1, 2));
        stream.extend(row_two_int_bytes(3, 4));
        stream.extend(row_two_int_bytes(5, 6));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1205);
    }

    /// NBCROW rows (null-bitmap-compressed) in the trailing result set are
    /// consumed through the same row-decoding path.
    #[tokio::test]
    async fn drain_on_error_consumes_nbcrow_rows() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("n"));
        stream.extend(nbcrow_single_null_bytes());
        stream.extend(row_int_bytes(7));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
    }

    /// Multiple trailing result sets after the error (each COLMETADATA / ROWs /
    /// DONE-with-more) are drained one after another until the final DONE.
    #[tokio::test]
    async fn drain_on_error_consumes_multiple_trailing_rowsets() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("a"));
        stream.extend(row_int_bytes(1));
        stream.extend(done_bytes(0x0001)); // DONE_MORE, result set 1 ends
        stream.extend(colmetadata_single_int_bytes("b"));
        stream.extend(row_int_bytes(2));
        stream.extend(row_int_bytes(3));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
    }

    /// An ERROR token that appears *inside* the trailing result set (after its
    /// rows) is collected alongside the first error, and INFO tokens are
    /// captured as informational messages.
    #[tokio::test]
    async fn drain_collects_secondary_error_and_info_tokens() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "first"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("n"));
        stream.extend(row_int_bytes(1));
        stream.extend(info_token_bytes(50000, "just fyi"));
        stream.extend(error_token_bytes(50001, 16, "second"));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        let numbers: Vec<u32> = diagnostics.errors.iter().map(|e| e.number).collect();
        assert_eq!(numbers, vec![1222, 50001]);
    }

    /// A COLMETADATA arriving before the current result set's DONE is a protocol
    /// violation, so the drain aborts rather than silently mis-parsing it. The
    /// abort is *contained*: the original statement error stays primary and the
    /// client is left idle, so the caller sees the real SQL error (1222) instead
    /// of the internal protocol detail, and a failed drain never leaves the
    /// batch marked open.
    #[tokio::test]
    async fn drain_rejects_colmetadata_before_result_set_done() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "boom"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_bytes("a"));
        stream.extend(row_int_bytes(1));
        stream.extend(colmetadata_single_int_bytes("b")); // no DONE before new metadata
        stream.extend(row_int_bytes(2));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
        assert!(
            !client.maybe_has_unread_rows(),
            "a contained drain failure must still clear the unread-rows guard"
        );
        assert!(
            !client.has_open_batch(),
            "a contained drain failure must still close the batch"
        );
    }

    /// Regression guard for the drain's Always Encrypted awareness: when column
    /// encryption is negotiated, a trailing result set's COLMETADATA carries a
    /// CEK-table prefix (empty here). The drain's top-level token read must use
    /// the same ColumnEncryption context as the normal read path, or those two
    /// prefix bytes are misread as column data and the stream desynchronizes —
    /// exactly the corruption this drain exists to prevent. Reading with
    /// `ParserContext::None` here surfaces a parse error instead of the SQL
    /// error, so this test fails without the fix.
    #[tokio::test]
    async fn drain_parses_trailing_rowset_metadata_under_column_encryption() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "lock timeout"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_int_ae_bytes("a"));
        stream.extend(row_int_bytes(7));
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes_with_ae(stream);
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
    }

    /// Blocking-review regression guard: the drain must not resolve a cell
    /// decryptor for a discarded result set. Here the trailing result set has a
    /// `FLAG_ENCRYPTED` column whose CEK table is empty — an anomaly that makes
    /// `resolve_cell_decryptor` fail fast. Resolving a decryptor on the drain
    /// path would surface that `Err` and mask the real SQL error. The fix
    /// decodes the encrypted column as raw ciphertext instead, so error 1222
    /// still surfaces and the batch is fully drained.
    #[tokio::test]
    async fn drain_does_not_resolve_decryptor_for_discarded_encrypted_rowset() {
        let mut stream = Vec::new();
        stream.extend(error_token_bytes(1222, 16, "lock timeout"));
        stream.extend(done_bytes(DONE_MORE_ERROR));
        stream.extend(colmetadata_single_encrypted_int_ae_bytes("secret"));
        stream.extend(row_int_bytes(0x1122_3344)); // 4 ciphertext bytes, discarded
        stream.extend(done_bytes(DONE_FINAL));

        let mut client = client_over_bytes_with_ae(stream);
        // Enable column encryption for this command so the pre-fix drain would
        // actually resolve a decryptor (and fail fast on the empty CEK table).
        // With the fix, the drain ignores this and decodes raw ciphertext.
        client.current_command_ce_setting =
            crate::connection::client_context::ExecutionColumnEncryptionSetting::Enabled;
        let err = client.advance_to_result_boundary().await.unwrap_err();

        let diagnostics = expect_sql_error(err);
        assert_eq!(diagnostics.errors[0].number, 1222);
    }

    /// The drain's top-level loop must apply the side effects of the control
    /// tokens it walks past — collecting ERROR diagnostics, recording INFO
    /// messages, applying ENVCHANGE, session-state and return-value/status
    /// tokens — before the terminal DONE ends the batch. This exercises the
    /// `apply_drain_side_effect` dispatch reached from `drain_stream` directly
    /// (no trailing row set), complementing the byte-level row-drain tests.
    #[tokio::test]
    async fn drain_applies_side_effects_of_control_tokens() {
        use crate::token::tokens::{
            EnvChangeContainer, EnvChangeToken, EnvChangeTokenSubType, ErrorToken, OrderToken,
            ReturnStatusToken, SessionStateToken,
        };

        let tokens = vec![
            Tokens::Error(ErrorToken {
                number: 1205,
                state: 51,
                severity: 13,
                message: "deadlock victim".to_string(),
                server_name: "test-server".to_string(),
                proc_name: String::new(),
                line_number: 1,
            }),
            info_token(50_000, 10, "printed message"),
            Tokens::EnvChange(EnvChangeToken {
                sub_type: EnvChangeTokenSubType::ResetConnection,
                change_type: EnvChangeContainer::from((0u32, 0u32)),
            }),
            Tokens::SessionState(SessionStateToken {
                sequence_number: u32::MAX,
                status: 0,
                states: Vec::new(),
            }),
            Tokens::ReturnValue(ae_return_value_token("@out", ColumnValues::Int(7), None)),
            Tokens::ReturnStatus(ReturnStatusToken { value: 3 }),
            // A token the drain neither steers on nor records: exercised only to
            // prove it is skipped without aborting the drain.
            Tokens::Order(OrderToken {
                _order_columns: Vec::new(),
            }),
            done_no_more(),
        ];

        let mut client = create_test_client_with_tokens(tokens);
        let errors = client.drain_stream().await.unwrap();

        assert_eq!(errors.len(), 1, "the ERROR token must be collected");
        assert_eq!(errors[0].number, 1205);
        assert!(matches!(
            client.last_return_status,
            ReturnStatus::Received(3)
        ));
        assert_eq!(
            client.return_values.len(),
            1,
            "the RETURNVALUE must be surfaced as an output parameter"
        );
    }

    // ── Streamed (data-at-execution) PLP parameter write ──

    /// Reassembles the TDS packet stream captured by the mock transport into the
    /// original contiguous request payload, stripping each 8-byte packet header.
    fn reassemble_sent(sent: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0;
        while off < sent.len() {
            let packet_len = u16::from_be_bytes([sent[off + 2], sent[off + 3]]) as usize;
            out.extend_from_slice(&sent[off + 8..off + packet_len]);
            off += packet_len;
        }
        out
    }

    /// The 8-byte little-endian `PLP_UNKNOWN_LEN` sentinel that opens every
    /// unknown-length PLP value on the wire.
    const PLP_UNKNOWN_LEN_BYTES: [u8; 8] = [0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    /// The 4-byte PLP terminator (a zero-length chunk header) that closes a value.
    const PLP_TERMINATOR_BYTES: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
    /// The 8-byte little-endian `PLP_NULL` sentinel: a MAX-type value that is SQL
    /// NULL. No chunks or terminator follow it.
    const PLP_NULL_BYTES: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

    /// Index of the last occurrence of `needle` in `haystack`, if any.
    fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        (0..=haystack.len() - needle.len())
            .rev()
            .find(|&i| &haystack[i..i + needle.len()] == needle)
    }

    /// A data-at-execution `varbinary(max)` parameter template named `name`.
    /// varbinary keeps chunk bytes raw (no encoding), so tests can assert the
    /// exact wire bytes they streamed.
    fn streamed_varbinary(name: &str) -> RpcParameter {
        RpcParameter::data_at_exec(
            Some(name.to_string()),
            StatusFlags::NONE,
            StreamedSqlType::VarBinaryMax,
        )
    }

    /// A single streamed chunk is framed as `[u32 len][bytes]` and the value is
    /// then closed with the PLP terminator.
    #[tokio::test]
    async fn streamed_write_single_chunk_frames_value_and_terminator() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        let status = client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@v")
        );

        let chunk = [0xAAu8; 6];
        client.write_streamed_chunk(&chunk).await.unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));

        let payload = reassemble_sent(&sent.lock().unwrap());
        let pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).expect("value opener present");
        let after = &payload[pos + PLP_UNKNOWN_LEN_BYTES.len()..];

        let mut expected = Vec::new();
        expected.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        expected.extend_from_slice(&chunk);
        expected.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(after, expected.as_slice());

        // The lifecycle is complete: no streamed write remains parked.
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
    }

    /// A streamed parameter marked NULL (before any chunk) is closed with
    /// `PLP_NULL` — no unknown-length opener, no chunks, no terminator. Mirrors
    /// msodbcsql's `VARMAX_LENGTH_NULL` for a data-at-execution NULL.
    #[tokio::test]
    async fn streamed_write_null_frames_plp_null() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        client.write_streamed_null().unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));

        let payload = reassemble_sent(&sent.lock().unwrap());
        // The streamed @v value's length field is PLP_NULL and nothing follows it
        // (no chunks, no terminator), so it is the final value on the wire. Note
        // the positional @statement/@params args are themselves nvarchar(max) and
        // legitimately use PLP_UNKNOWN_LEN openers, so we assert on @v's PLP_NULL
        // being last rather than the absence of any opener in the whole payload.
        assert!(
            payload.ends_with(&PLP_NULL_BYTES),
            "a NULL streamed value must end the message with PLP_NULL and no terminator"
        );
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
    }

    /// Writing a chunk after marking the parameter NULL is a usage error, and the
    /// stream stays usable (the message is not corrupted — nothing was written).
    #[tokio::test]
    async fn streamed_write_chunk_after_null_errors() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        client.write_streamed_null().unwrap();
        let err = client
            .write_streamed_chunk(&[0x01])
            .await
            .expect_err("cannot write data after NULL");
        assert!(matches!(err, UsageError(_)));
        // Still active: end can still close it as NULL.
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Active(_)
        ));
        assert!(matches!(
            client.end_streamed_param().await.unwrap(),
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));
    }

    /// An *empty* chunk after marking the parameter NULL must still be a usage
    /// error, not a silent no-op: the empty-chunk short-circuit runs only after
    /// the NULL-sequencing check, so it cannot mask this caller mistake.
    #[tokio::test]
    async fn streamed_write_empty_chunk_after_null_errors() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        client.write_streamed_null().unwrap();
        let err = client
            .write_streamed_chunk(&[])
            .await
            .expect_err("an empty chunk after NULL must still error");
        assert!(matches!(err, UsageError(_)));
        assert!(matches!(
            client.end_streamed_param().await.unwrap(),
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));
    }

    /// Marking a parameter NULL after chunks were already written is a usage
    /// error: a value that has begun streaming cannot become NULL.
    #[tokio::test]
    async fn streamed_null_after_chunk_errors() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        client.write_streamed_chunk(&[0x01, 0x02]).await.unwrap();
        let err = client
            .write_streamed_null()
            .expect_err("cannot mark NULL after chunks were written");
        assert!(matches!(err, UsageError(_)));
    }

    /// `write_streamed_null` with no active streamed parameter is a usage error.
    #[tokio::test]
    async fn streamed_null_without_active_stream_errors() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
        let err = client
            .write_streamed_null()
            .expect_err("no active streamed parameter");
        assert!(matches!(err, UsageError(_)));
    }

    /// A NULL streamed parameter followed by a normal streamed parameter: the
    /// first closes with `PLP_NULL`, the second frames its value normally, and the
    /// lifecycle advances NeedData -> NeedData -> Done.
    #[tokio::test]
    async fn streamed_write_null_then_value_param() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(a, b) VALUES (@a, @b)".to_string(),
                vec![streamed_varbinary("@a"), streamed_varbinary("@b")],
                (),
            )
            .await
            .unwrap();

        // @a is NULL.
        client.write_streamed_null().unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@b")
        );

        // @b carries a value.
        let value = [0x7Au8; 5];
        client.write_streamed_chunk(&value).await.unwrap();
        assert!(matches!(
            client.end_streamed_param().await.unwrap(),
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));

        let payload = reassemble_sent(&sent.lock().unwrap());
        // @b's value is framed after an unknown-length opener; @a contributed a
        // PLP_NULL and no opener.
        let b_pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let mut expected_b = Vec::new();
        expected_b.extend_from_slice(&(value.len() as u32).to_le_bytes());
        expected_b.extend_from_slice(&value);
        expected_b.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(
            &payload[b_pos + PLP_UNKNOWN_LEN_BYTES.len()..],
            expected_b.as_slice()
        );
        assert!(
            find_last(&payload[..b_pos], &PLP_NULL_BYTES).is_some(),
            "@a must have emitted PLP_NULL before @b's value"
        );
    }

    /// Multiple chunks are each length-prefixed independently and the value is
    /// closed by exactly one terminator — the incremental multi-chunk case.
    #[tokio::test]
    async fn streamed_write_multiple_chunks_each_length_prefixed_single_terminator() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        let chunks: [&[u8]; 3] = [&[0x01, 0x02], &[0x03, 0x04, 0x05], &[0x06]];
        for c in chunks {
            client.write_streamed_chunk(c).await.unwrap();
        }
        client.end_streamed_param().await.unwrap();

        let payload = reassemble_sent(&sent.lock().unwrap());
        let pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let after = &payload[pos + PLP_UNKNOWN_LEN_BYTES.len()..];

        let mut expected = Vec::new();
        for c in chunks {
            expected.extend_from_slice(&(c.len() as u32).to_le_bytes());
            expected.extend_from_slice(c);
        }
        expected.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(after, expected.as_slice());
    }

    /// An empty chunk must be ignored, not written: a zero-length chunk header
    /// IS the terminator, so emitting one mid-value would truncate the value on
    /// the server. Protocol-safety analogue of the read-side early-terminator
    /// test.
    #[tokio::test]
    async fn streamed_write_empty_chunk_is_skipped_not_terminator() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        client.write_streamed_chunk(&[0x11, 0x22]).await.unwrap();
        client.write_streamed_chunk(&[]).await.unwrap(); // must be a no-op
        client.end_streamed_param().await.unwrap();

        let payload = reassemble_sent(&sent.lock().unwrap());
        let pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let after = &payload[pos + PLP_UNKNOWN_LEN_BYTES.len()..];

        // Exactly one framed chunk followed by exactly one terminator; the empty
        // chunk contributed nothing.
        let mut expected = Vec::new();
        expected.extend_from_slice(&2u32.to_le_bytes());
        expected.extend_from_slice(&[0x11, 0x22]);
        expected.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(after, expected.as_slice());
    }

    /// Two data-at-execution parameters advance NeedData(@a) -> NeedData(@b) ->
    /// Done, and both values are framed in order on the wire.
    #[tokio::test]
    async fn streamed_write_two_params_advances_need_data_then_done() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        let status = client
            .begin_sp_executesql(
                "INSERT INTO t(a, b) VALUES (@a, @b)".to_string(),
                vec![streamed_varbinary("@a"), streamed_varbinary("@b")],
                (),
            )
            .await
            .unwrap();
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@a")
        );

        let a_value = [0xA1u8; 4];
        client.write_streamed_chunk(&a_value).await.unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@b")
        );

        let b_value = [0xB2u8; 5];
        client.write_streamed_chunk(&b_value).await.unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));

        // @b is the last streamed param: its opener is the last sentinel, and the
        // bytes after it are exactly @b's framed value + terminator.
        let payload = reassemble_sent(&sent.lock().unwrap());
        let b_pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let mut expected_b = Vec::new();
        expected_b.extend_from_slice(&(b_value.len() as u32).to_le_bytes());
        expected_b.extend_from_slice(&b_value);
        expected_b.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(
            &payload[b_pos + PLP_UNKNOWN_LEN_BYTES.len()..],
            expected_b.as_slice()
        );

        // @a's opener precedes @b's, and its framed value + terminator sits right
        // after it (followed by @b's parameter header).
        let a_pos = find_last(&payload[..b_pos], &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let mut expected_a = Vec::new();
        expected_a.extend_from_slice(&(a_value.len() as u32).to_le_bytes());
        expected_a.extend_from_slice(&a_value);
        expected_a.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert!(
            payload[a_pos + PLP_UNKNOWN_LEN_BYTES.len()..].starts_with(&expected_a),
            "@a's framed value + terminator must immediately follow its opener"
        );
        assert!(a_pos < b_pos, "@a must be serialized before @b");
    }

    /// With no data-at-execution parameter, `begin_sp_executesql` delegates to
    /// the atomic path: it sends the RPC, consumes the response, and returns
    /// Complete without parking any streamed state.
    #[tokio::test]
    async fn begin_without_data_at_exec_delegates_and_returns_complete() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        let status = client
            .begin_sp_executesql(
                "INSERT INTO t(id) VALUES (@id)".to_string(),
                vec![RpcParameter::new(
                    Some("@id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(7)),
                )],
                (),
            )
            .await
            .unwrap();

        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
        assert!(
            !sent.lock().unwrap().is_empty(),
            "the atomic RPC should have been sent"
        );
    }

    /// Always Encrypted does not block the atomic fallback when there are no
    /// data-at-execution parameters.
    #[tokio::test]
    async fn begin_without_data_at_exec_allows_always_encrypted_atomic_fallback() {
        let mut client = client_over_bytes_with_ae(done_bytes(DONE_FINAL));

        let status = client
            .begin_sp_executesql("SELECT 1".to_string(), Vec::new(), ())
            .await
            .unwrap();

        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
    }

    /// A mid-value send failure aborts the streamed write: the error is
    /// surfaced, the parked message is dropped (state returns to `Idle`, not left
    /// `Active`/resumable), and the transport is closed so the incomplete RPC
    /// cannot desynchronize a later command.
    #[tokio::test]
    async fn streamed_write_chunk_send_failure_aborts_stream() {
        let (mut client, fail) = create_failing_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Active(_)
        ));

        // Fail the next wire send; a chunk large enough to overflow the packet
        // payload buffer forces a flush (and thus a `send`) inside
        // write_streamed_chunk.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let big = vec![0xABu8; 10_000];
        let _err = client
            .write_streamed_chunk(&big)
            .await
            .expect_err("a send failure must surface as an error");

        // The streamed write is aborted, not left resumable.
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
        assert!(
            client.is_connection_dead(),
            "an incomplete RPC must make the connection non-reusable"
        );

        // Further streamed calls now fail cleanly (no active stream) rather than
        // appending to the corrupt message.
        let followup = client
            .write_streamed_chunk(&[0x00])
            .await
            .expect_err("no active streamed parameter after abort");
        assert!(matches!(followup, UsageError(_)));
        let end = client
            .end_streamed_param()
            .await
            .expect_err("no active streamed parameter after abort");
        assert!(matches!(end, UsageError(_)));
    }

    /// A send failure while finalizing the last streamed parameter (flushing the
    /// terminator + message) aborts the same way: error surfaced, state `Idle`,
    /// transport closed.
    #[tokio::test]
    async fn end_streamed_param_send_failure_aborts_stream() {
        let (mut client, fail) = create_failing_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        // A small chunk stays buffered (fits one packet, no send yet).
        client
            .write_streamed_chunk(&[0x01, 0x02, 0x03])
            .await
            .unwrap();

        // finalize() inside end_streamed_param flushes the message -> send fails.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let _err = client
            .end_streamed_param()
            .await
            .expect_err("finalize send failure must surface");

        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
        assert!(
            client.is_connection_dead(),
            "a failed final send must make the connection non-reusable"
        );
    }

    /// Beginning a streamed execution while one is already active is rejected.
    #[tokio::test]
    async fn begin_while_stream_active_errors() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        let err = client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .expect_err("cannot begin a second streamed execution while one is active");
        assert!(matches!(err, UsageError(_)));
    }

    /// An unnamed data-at-execution parameter is rejected: streamed values must
    /// be named so `sp_executesql` can match them by name.
    #[tokio::test]
    async fn begin_rejects_unnamed_data_at_exec_param() {
        let mut client = create_test_client_with_tokens(vec![]);
        let bad =
            RpcParameter::data_at_exec(None, StatusFlags::NONE, StreamedSqlType::VarBinaryMax);

        let err = client
            .begin_sp_executesql("INSERT INTO t(v) VALUES (@v)".to_string(), vec![bad], ())
            .await
            .expect_err("unnamed data-at-exec parameter must be rejected");
        assert!(matches!(err, UsageError(_)));
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
    }

    /// All non-streaming execute entry points must reject data-at-exec params
    /// with a usage error instead of panicking or writing malformed RPC values.
    #[tokio::test]
    async fn execute_sp_executesql_rejects_data_at_exec_params() {
        let mut client = create_test_client_with_tokens(vec![]);
        let err = client
            .execute_sp_executesql("SELECT @v".to_string(), vec![streamed_varbinary("@v")], ())
            .await
            .expect_err("streamed params are only valid via begin_sp_executesql");
        assert!(matches!(err, UsageError(_)));
    }

    #[tokio::test]
    async fn execute_stored_procedure_rejects_data_at_exec_params() {
        let mut client = create_test_client_with_tokens(vec![]);
        let err = client
            .execute_stored_procedure(
                "dbo.p".to_string(),
                None,
                Some(vec![streamed_varbinary("@v")]),
                (),
            )
            .await
            .expect_err("streamed params are only valid via begin_sp_executesql");
        assert!(matches!(err, UsageError(_)));
    }

    #[tokio::test]
    async fn execute_prepared_rejects_data_at_exec_params() {
        let mut client = create_test_client_with_tokens(vec![]);
        let mut statement = PreparedStatement::new("SELECT @v");
        let mut orphaned = None;
        let err = client
            .execute_prepared(
                &mut statement,
                vec![streamed_varbinary("@v")],
                &mut orphaned,
                (),
            )
            .await
            .expect_err("streamed params are only valid via begin_sp_executesql");
        assert!(matches!(err, UsageError(_)));
    }

    /// Cancelling while idle must be a no-op that keeps the connection reusable.
    #[tokio::test]
    async fn cancel_streamed_write_on_idle_client_is_noop() {
        let mut client = create_test_client_with_tokens(vec![]);
        client.cancel_streamed_write().await;
        assert!(
            !client.is_connection_dead(),
            "cancelling while idle must not kill a reusable connection"
        );
        assert!(matches!(
            client.streamed_write_state,
            StreamedWriteState::Idle
        ));
    }

    /// Cancelling an active streamed write must discard parked state and mark
    /// the connection dead so it cannot be reused by a pool checkout.
    #[tokio::test]
    async fn cancel_streamed_write_aborts_active_stream_and_marks_connection_dead() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
        let status = client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .expect("begin must park for streamed value");
        assert!(matches!(
            status,
            StreamedParamStatus::NeedData { param_name: _ }
        ));

        client.cancel_streamed_write().await;

        assert!(
            client.is_connection_dead(),
            "an aborted streamed write must not hand a live-looking connection back to the pool"
        );
        assert!(matches!(
            client.write_streamed_chunk(&[0x01]).await,
            Err(UsageError(_))
        ));
    }

    /// `write_streamed_chunk` with no active streamed parameter is a usage error.
    #[tokio::test]
    async fn write_streamed_chunk_without_active_stream_errors() {
        let mut client = create_test_client_with_tokens(vec![]);
        let err = client
            .write_streamed_chunk(&[0x01])
            .await
            .expect_err("no active streamed parameter");
        assert!(matches!(err, UsageError(_)));
    }

    /// `end_streamed_param` with no active streamed parameter is a usage error.
    #[tokio::test]
    async fn end_streamed_param_without_active_stream_errors() {
        let mut client = create_test_client_with_tokens(vec![]);
        let err = client
            .end_streamed_param()
            .await
            .expect_err("no active streamed parameter");
        assert!(matches!(err, UsageError(_)));
    }

    #[tokio::test]
    async fn write_streamed_chunk_empty_while_idle_is_usage_error() {
        let mut client = create_test_client_with_tokens(vec![]);
        let err = client
            .write_streamed_chunk(&[])
            .await
            .expect_err("empty chunk while Idle must be a usage error");
        assert!(matches!(err, UsageError(_)));
    }

    #[tokio::test]
    async fn normal_command_blocked_while_streamed_write_active() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();
        let err = client
            .execute_sp_executesql("SELECT 1".to_string(), vec![], ())
            .await
            .expect_err("normal command must be blocked while streaming");
        assert!(matches!(err, UsageError(_)));
        client.write_streamed_null().unwrap();
        let status = client.end_streamed_param().await.unwrap();
        assert!(matches!(
            status,
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));
    }

    #[tokio::test]
    async fn write_streamed_chunk_resets_timeout_to_configured_budget() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                ExecuteOptions::new().timeout_secs(30),
            )
            .await
            .unwrap();
        client.remaining_request_timeout = Some(Duration::from_secs(1));
        client.write_streamed_chunk(&[0x01]).await.unwrap();
        assert_eq!(
            client.remaining_request_timeout,
            Some(Duration::from_secs(30))
        );
        client.remaining_request_timeout = Some(Duration::from_secs(1));
        client.end_streamed_param().await.unwrap();
    }
    /// Little-endian UTF-16 encoding of `s`, matching how parameter names are
    /// written on the wire (length-prefixed unicode).
    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A materialized (send-now) named parameter and a data-at-execution one may
    /// be mixed: the materialized value is sent atomically in the RPC prefix and
    /// the streamed value follows. Offline analogue of the sparse non-PLP + PLP
    /// e2e test. The materialized name precedes the streamed value opener, and
    /// the streamed value frames correctly.
    #[tokio::test]
    async fn streamed_write_mixes_materialized_and_data_at_exec() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        let materialized = RpcParameter::new(
            Some("@id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(7)),
        );

        let status = client
            .begin_sp_executesql(
                "INSERT INTO t(id, v) VALUES (@id, @v)".to_string(),
                vec![materialized, streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@v")
        );

        let value = [0xCDu8; 8];
        client.write_streamed_chunk(&value).await.unwrap();
        assert!(matches!(
            client.end_streamed_param().await.unwrap(),
            StreamedParamStatus::Complete(
                StatementResult::NoRows { .. } | StatementResult::Rows | StatementResult::End
            )
        ));

        let payload = reassemble_sent(&sent.lock().unwrap());

        // The streamed value opener is the last unknown-length sentinel, and the
        // materialized parameter's name is serialized before it.
        let v_pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();
        let id_name = utf16le("@id");
        let id_pos = find_last(&payload[..v_pos], &id_name)
            .expect("materialized parameter name must be serialized before the streamed value");
        assert!(id_pos < v_pos, "@id must precede the streamed @v value");

        let mut expected = Vec::new();
        expected.extend_from_slice(&(value.len() as u32).to_le_bytes());
        expected.extend_from_slice(&value);
        expected.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(
            &payload[v_pos + PLP_UNKNOWN_LEN_BYTES.len()..],
            expected.as_slice()
        );
    }

    /// After `begin_sp_executesql` parks the message, the streamed state must
    /// retain the un-emitted parameters and the collation used to write their
    /// TYPE_INFO. Analogue of the read side's pause-state-preserves-collation
    /// test.
    #[tokio::test]
    async fn streamed_write_pause_state_preserves_pending_and_collation() {
        let (mut client, _sent) = create_capturing_client(vec![done_no_more()]);
        let expected_collation = client.negotiated_settings.database_collation;

        client
            .begin_sp_executesql(
                "INSERT INTO t(a, b, c) VALUES (@a, @b, @c)".to_string(),
                vec![
                    streamed_varbinary("@a"),
                    streamed_varbinary("@b"),
                    streamed_varbinary("@c"),
                ],
                (),
            )
            .await
            .unwrap();

        match &client.streamed_write_state {
            StreamedWriteState::Active(ctx) => {
                // @a is open for data; @b and @c remain pending, in order.
                assert_eq!(ctx.pending.len(), 2);
                assert_eq!(ctx.pending[0].name.as_deref(), Some("@b"));
                assert_eq!(ctx.pending[1].name.as_deref(), Some("@c"));
                assert_eq!(ctx.db_collation, expected_collation);
            }
            StreamedWriteState::Idle => panic!("streamed write should be Active after begin"),
        }
    }

    /// A single chunk larger than the TDS packet payload is split across multiple
    /// packets on the wire yet reassembles into one contiguous length-prefixed
    /// value. Analogue of multi-packet incremental reads.
    #[tokio::test]
    async fn streamed_write_large_chunk_spans_multiple_packets() {
        let (mut client, sent) = create_capturing_client(vec![done_no_more()]);

        client
            .begin_sp_executesql(
                "INSERT INTO t(v) VALUES (@v)".to_string(),
                vec![streamed_varbinary("@v")],
                (),
            )
            .await
            .unwrap();

        // Larger than the mock transport's 4096-byte packet so the value must
        // span several TDS packets.
        let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        client.write_streamed_chunk(&big).await.unwrap();
        client.end_streamed_param().await.unwrap();

        // The captured stream is more than one packet.
        assert!(
            sent.lock().unwrap().len() > 4096,
            "a 10 KB value must produce multiple TDS packets"
        );

        let payload = reassemble_sent(&sent.lock().unwrap());
        let pos = find_last(&payload, &PLP_UNKNOWN_LEN_BYTES).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&(big.len() as u32).to_le_bytes());
        expected.extend_from_slice(&big);
        expected.extend_from_slice(&PLP_TERMINATOR_BYTES);
        assert_eq!(
            &payload[pos + PLP_UNKNOWN_LEN_BYTES.len()..],
            expected.as_slice()
        );
    }

    // ── ResetConnection ENVCHANGE: full client-side state transition (A1/A3) ──

    fn error_token_with_severity(severity: u8) -> crate::token::tokens::ErrorToken {
        crate::token::tokens::ErrorToken {
            number: 4060,
            state: 1,
            severity,
            message: "test error".to_string(),
            server_name: "test-server".to_string(),
            proc_name: String::new(),
            line_number: 1,
        }
    }

    /// A server-acknowledged connection reset must return the client to its
    /// login baseline: session-bound caches emptied and negotiated
    /// database/language/collation restored to their login values, so a
    /// previous borrower's `USE otherdb` / `SET LANGUAGE` and prepared handles
    /// do not leak to the next borrower.
    #[tokio::test]
    async fn reset_connection_ack_clears_caches_and_restores_login_defaults() {
        use crate::security::describe_parameter_encryption::DescribeParameterEncryptionResult;
        use crate::token::tokens::{
            EnvChangeContainer, EnvChangeToken, EnvChangeTokenSubType, SqlCollation,
        };

        let mut client = create_test_client_with_tokens(vec![
            Tokens::EnvChange(EnvChangeToken {
                sub_type: EnvChangeTokenSubType::ResetConnection,
                change_type: EnvChangeContainer::from((0u32, 0u32)),
            }),
            done_no_more(),
        ]);

        // Session-bound state a borrower accumulated before the reset.
        client.prepared_handles.insert(sid(1), 55);
        client
            .prepared_param_encryption
            .insert(sid(1), Arc::new(DescribeParameterEncryptionResult::new()));
        client.pending_capture = Some(sid(2));
        client.pending_prepared_param_encryption =
            Some(Arc::new(DescribeParameterEncryptionResult::new()));
        client.negotiated_settings.database = "otherdb".to_string();
        client.negotiated_settings.language = "français".to_string();
        client.negotiated_settings.database_collation = SqlCollation {
            info: 0,
            lcid_language_id: 0x0000,
            col_flags: 0,
            sort_id: 0,
        };

        client.drain_stream().await.unwrap();

        assert!(
            client.prepared_handles.is_empty(),
            "prepared handles belong to the reset session"
        );
        assert!(
            client.prepared_param_encryption.is_empty(),
            "cached AE describe metadata must be dropped"
        );
        assert!(client.pending_capture.is_none());
        assert!(client.pending_prepared_param_encryption.is_none());
        assert_eq!(client.database(), "master", "database reverts to login");
        assert_eq!(client.language(), "us_english", "language reverts to login");
        assert_eq!(
            client.get_collation().lcid_language_id,
            0x0409,
            "collation reverts to login"
        );
    }

    /// Arming the reset drops the session-bound caches immediately, without a
    /// round trip, and leaves the acknowledgement outstanding until the server
    /// confirms it on the request that carries the bit.
    #[tokio::test]
    async fn prepare_reset_connection_clears_caches_and_arms_bit() {
        use crate::message::messages::PacketStatusFlags;
        use crate::token::tokens::{EnvChangeContainer, EnvChangeToken, EnvChangeTokenSubType};

        let (mut client, sent) = create_capturing_client(vec![
            Tokens::EnvChange(EnvChangeToken {
                sub_type: EnvChangeTokenSubType::ResetConnection,
                change_type: EnvChangeContainer::from((0u32, 0u32)),
            }),
            done_no_more(),
        ]);
        client.prepared_handles.insert(sid(1), 55);

        client.prepare_reset_connection(false);

        assert!(
            client.prepared_handles.is_empty(),
            "arming must drop session-bound caches without waiting for the ack"
        );
        assert!(
            client.reset_pending(),
            "the acknowledgement is still outstanding until the server confirms"
        );
        assert!(
            sent.lock().unwrap().is_empty(),
            "arming must not send anything on its own"
        );

        // The next request carries the bit and collects the acknowledgement.
        client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect("the carrying request should succeed");
        client.close_query().await.expect("drain the ack");

        let sent = sent.lock().unwrap();
        assert_eq!(
            sent[1] & PacketStatusFlags::ResetConnection as u8,
            PacketStatusFlags::ResetConnection as u8,
            "the next request must carry the RESETCONNECTION bit in its first packet"
        );
        assert!(
            !client.reset_pending(),
            "the ResetConnection ack must clear the pending flag"
        );
    }

    /// The fail-at-checkout guarantee: a request that carried the
    /// RESETCONNECTION bit and completed without the server's ENVCHANGE ran
    /// against a session that was never reset. The client fails that request and
    /// marks itself dead so a pool discards the connection instead of handing a
    /// dirty session to the next borrower.
    #[tokio::test]
    async fn unacknowledged_reset_fails_the_carrying_request_and_marks_it_dead() {
        // No ResetConnection ENVCHANGE: the batch just completes.
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client.prepare_reset_connection(false);

        let err = client
            .execute(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED".to_string(),
                (),
            )
            .await
            .expect_err("an unacknowledged reset must fail the carrying request");

        assert!(
            matches!(err, crate::error::Error::ConnectionResetNotAcknowledged),
            "expected ConnectionResetNotAcknowledged, got {err:?}"
        );
        assert!(
            client.is_connection_dead(),
            "the connection must be marked dead so the pool discards it"
        );
        assert!(
            !client.reset_pending(),
            "the failure is reported once; the drain that follows must not repeat it"
        );
    }

    /// A row-returning carrier fails at its COLMETADATA rather than waiting for
    /// the rows to be read: the reset ENVCHANGE precedes everything the request
    /// produces, so the first result-set token already proves it never arrived.
    #[tokio::test]
    async fn unacknowledged_reset_fails_a_row_returning_carrier_at_its_metadata() {
        let mut client = create_test_client_with_tokens(vec![empty_col_metadata(), done_no_more()]);
        client.prepare_reset_connection(false);

        let err = client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect_err("an unacknowledged reset must fail the carrying request");

        assert!(
            matches!(err, crate::error::Error::ConnectionResetNotAcknowledged),
            "expected ConnectionResetNotAcknowledged, got {err:?}"
        );
        assert!(client.is_connection_dead());
    }

    /// The server's own diagnostic must win over the reset verdict: a request
    /// that fails outright tells the caller something more actionable, and the
    /// unacknowledged reset is still caught on the next token.
    #[tokio::test]
    async fn server_error_on_the_carrier_is_not_masked_by_the_reset_verdict() {
        let mut client = create_test_client_with_tokens(vec![
            Tokens::Error(error_token_with_severity(16)),
            done_no_more(),
        ]);
        client.prepare_reset_connection(false);

        let err = client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect_err("the server error must surface");

        assert!(
            matches!(err, crate::error::Error::SqlServerError { .. }),
            "the server diagnostic must reach the caller, got {err:?}"
        );
        assert!(
            client.is_connection_dead(),
            "the unacknowledged reset is not lost: the drain that follows the error \
             still condemns the connection"
        );
    }

    /// The bit is armed on the connection but consumed by the packet writer, so
    /// an arm that never reached the wire owes nothing. This is the shape a
    /// transparent reconnect leaves behind — the armed bit died with the old
    /// transport — and it must never condemn the responses that follow.
    #[tokio::test]
    async fn armed_reset_that_never_reached_the_wire_condemns_nothing() {
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client.prepare_reset_connection(false);

        client
            .drain_stream()
            .await
            .expect("a response no reset bit was sent on must not be condemned");

        assert!(
            !client.is_connection_dead(),
            "nothing was ever owed, so nothing may be failed"
        );
        assert!(
            client.reset_pending(),
            "the reset is still armed and will ride the next request"
        );
    }

    /// Regression: a carrying request whose response is abandoned before any
    /// token is read — the cancellation/timeout shape, where
    /// `wait_for_attention_ack` drains to the attention DONE and discards the
    /// `ResetConnection` ENVCHANGE along with everything else — must not leave
    /// its suspicion behind for the *next* request to answer for. Doing so would
    /// mark a healthy connection dead on a token that had nothing to do with the
    /// reset.
    #[tokio::test]
    async fn abandoned_carrier_does_not_condemn_the_next_request() {
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client.prepare_reset_connection(false);

        // Send the carrier but never read its response, exactly as the
        // attention drain leaves things: the bit is on the wire and no token
        // was ever observed.
        client
            .send_query_batch(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED".to_string(),
                ().into(),
            )
            .await
            .expect("the carrier is sent");
        assert!(
            client.reset_pending(),
            "precondition: the reset is still outstanding when the response is abandoned"
        );

        // The next, unrelated request must complete normally.
        client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect("an unrelated request must not answer for an abandoned reset");

        assert!(
            !client.is_connection_dead(),
            "the next request must not be charged for a reset it never carried"
        );
        assert!(
            !client.reset_pending(),
            "the abandoned reset is settled, not left outstanding forever"
        );
    }

    /// The other abandonment shape: the response *was* partially read — a
    /// non-verdict token advanced tracking to `AwaitingAck` — and then ended
    /// before any token could settle it. Cancellation landing mid-response
    /// produces this, and like the never-read case it must not be carried into
    /// the next request.
    #[tokio::test]
    async fn abandoned_carrier_that_reached_awaiting_ack_is_settled_too() {
        // One INFO token, then the stream ends: INFO is not evidence the
        // request ran, so tracking advances to `AwaitingAck` and stops there.
        let mut client = create_test_client_with_tokens(vec![info_token(0, 0, "mid-response")]);
        client.prepare_reset_connection(false);

        client
            .execute(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED".to_string(),
                (),
            )
            .await
            .expect_err("the scripted stream ends before the batch completes");
        assert!(
            !client.is_connection_dead(),
            "an inconclusive token must not condemn the carrier itself"
        );
        assert!(
            client.reset_pending(),
            "precondition: the acknowledgement is still outstanding when the response ends"
        );

        client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect_err("the scripted transport has no tokens left");

        assert!(
            !client.is_connection_dead(),
            "the next request must not be charged for the abandoned reset"
        );
        assert!(
            !client.reset_pending(),
            "the abandoned reset is settled at the start of the next command"
        );
    }

    /// The settlement above must not fire on the ordinary armed-but-unsent
    /// reset: starting a command while a reset is armed is the normal path, and
    /// that command is the one that carries the bit.
    #[tokio::test]

    async fn starting_a_command_leaves_an_unsent_armed_reset_alone() {
        let mut client = create_test_client_with_tokens(vec![
            Tokens::EnvChange(crate::token::tokens::EnvChangeToken {
                sub_type: EnvChangeTokenSubType::ResetConnection,
                change_type: crate::token::tokens::EnvChangeContainer::from((0u32, 0u32)),
            }),
            done_no_more(),
        ]);
        client.prepare_reset_connection(false);

        client
            .execute("SELECT 1".to_string(), ())
            .await
            .expect("the armed reset must still ride this request and be acknowledged");

        assert!(!client.is_connection_dead());
        assert!(
            !client.reset_pending(),
            "the acknowledgement arrived on the carrying request"
        );
    }

    /// Session recovery supersedes a pending reset: the new login is already at
    /// the session defaults and the old bit died with the old transport, so no
    /// acknowledgement is coming. Adopting the recovered session must clear the
    /// pending reset, or the first response on a healthy new connection would
    /// report it as unacknowledged and throw the connection away.
    #[tokio::test]
    async fn adopting_a_recovered_session_clears_a_pending_reset() {
        // A server error is inconclusive about the reset, and the stream ends
        // there — the shape of a session that errored and then dropped — so the
        // reset is still outstanding when recovery kicks in.
        let mut client =
            create_test_client_with_tokens(vec![Tokens::Error(error_token_with_severity(16))]);
        client.prepare_reset_connection(false);
        let _ = client.execute("SELECT 1".to_string(), ()).await;
        assert!(client.reset_pending());

        client
            .adopt_recovered_session(
                AnyTransport::dynamic(TestTransport::with_tokens(vec![done_no_more()])),
                crate::handler::handler_factory::create_test_negotiated_settings_internal(),
                ExecutionContext::new(),
                Vec::new(),
                &[],
            )
            .expect("adopting a recovered session must succeed");

        assert!(
            !client.reset_pending(),
            "a fresh login supersedes the reset it can no longer acknowledge"
        );
        client
            .drain_stream()
            .await
            .expect("the recovered session must serve responses normally");
    }

    /// Regression: the cursor RPCs consume their responses via `drain_stream`
    /// but never call `begin_command`, so settling only there left a cursor op
    /// issued after an abandoned carrier to be condemned on its first token —
    /// the exact false positive the settlement exists to prevent. Every cursor
    /// entry point does call `check_and_reconnect`, so the settle lives there
    /// too. Driven through `check_and_reconnect` directly, which is the shared
    /// request boundary the cursor path relies on.
    #[tokio::test]
    async fn cursor_path_request_boundary_settles_an_abandoned_carrier() {
        let mut client = create_test_client_with_tokens(vec![done_no_more()]);
        client.prepare_reset_connection(false);

        // Send a carrier but never read its response — the shape the attention
        // drain leaves behind on cancellation/timeout.
        client
            .send_query_batch(
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED".to_string(),
                ().into(),
            )
            .await
            .expect("the carrier is sent");
        assert!(
            client.reset_pending(),
            "precondition: the acknowledgement is outstanding when the response is abandoned"
        );

        // A cursor op's boundary: no `begin_command`, only `check_and_reconnect`.
        client
            .check_and_reconnect(None, None)
            .await
            .expect("the boundary check must not fail");

        assert!(
            !client.reset_pending(),
            "the cursor path's request boundary must settle the abandoned reset"
        );
        assert!(
            !client.is_connection_dead(),
            "a cursor op must not be charged for a reset it never carried"
        );
    }

    /// D3: a server fatal-error token (severity/class ≥ 20) marks the transport
    /// known-dead even on a still-open socket, so a pool checkout discards it.
    #[test]
    fn fatal_error_token_marks_connection_known_dead() {
        let mut client = create_test_client();
        assert!(!client.is_connection_dead());

        let info = client.record_error_token(&error_token_with_severity(FATAL_ERROR_SEVERITY));

        assert_eq!(info.class, FATAL_ERROR_SEVERITY as i32);
        assert!(
            client.is_connection_dead(),
            "a class >= 20 error must mark the connection dead"
        );
    }

    /// D3 boundary: an error just below the fatal threshold (e.g. a plain
    /// severity-16 user error) leaves the connection reusable.
    #[test]
    fn nonfatal_error_token_leaves_connection_alive() {
        let mut client = create_test_client();

        client.record_error_token(&error_token_with_severity(FATAL_ERROR_SEVERITY - 1));

        assert!(
            !client.is_connection_dead(),
            "a class < 20 error must not mark the connection dead"
        );
    }
}
