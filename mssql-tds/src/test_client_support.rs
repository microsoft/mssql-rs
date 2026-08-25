// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only helpers for driving a [`TdsClient`] against a scripted sequence of
//! TDS tokens, without a live server. Gated behind the `test-util` feature so
//! downstream crates (e.g. `mssql-odbc`) can unit-test the code paths that
//! require a positioned client — statement-wise navigation, no-row results,
//! end-of-batch — which otherwise are only reachable through end-to-end tests.
//!
//! The transport replays the queued tokens for both `receive_token` (result
//! boundaries) and `receive_row_into` (row draining, where every queued token
//! is surfaced as a control token, so a `DONE` terminates the current result
//! set exactly as it does on the wire). It has no row bytes, so it cannot yield
//! a materialized row (`RowReadResult::RowWritten`); tests that need to observe
//! end-of-rowset drive a terminal `DONE` instead.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;

use crate::connection::client_context::ClientContext;
use crate::connection::execution_context::ExecutionContext;
use crate::connection::tds_client::TdsClient;
use crate::connection::transport::any_transport::AnyTransport;
use crate::connection::transport::network_transport::TransportSslHandler;
use crate::connection::transport::tds_transport::TdsTransport;
use crate::core::{CancelHandle, NegotiatedEncryptionSetting, TdsResult};
use crate::datatypes::row_writer::RowWriter;
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
use crate::handler::handler_factory::create_test_negotiated_settings_internal;
use crate::io::reader_writer::{NetworkReader, NetworkWriter};
use crate::io::token_stream::{
    ColumnPolicy, ParserContext, PlpPauseState, RowHeader, RowPauseState, RowReadResult,
    TdsTokenStreamReader,
};
use crate::message::messages::ResetConnectionMode;
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::{
    ColMetadataToken, CurrentCommand, DoneStatus, DoneToken, EnvChangeContainer, EnvChangeToken,
    EnvChangeTokenSubType, InfoToken, Tokens,
};

/// An opaque, scripted TDS token produced by the constructor helpers in this
/// module and consumed by [`tds_client_from_tokens`]. It wraps the crate's
/// internal token representation so the token type itself stays sealed.
pub struct ScriptedToken(Tokens);

/// A transport that replays a fixed queue of [`Tokens`] and discards anything
/// written to the wire. Running out of queued tokens is reported as a closed
/// connection.
#[derive(Debug)]
struct TokenReplayTransport {
    pending_tokens: VecDeque<Tokens>,
    reset_mode: ResetConnectionMode,
    reset_dispatched: bool,
    known_dead: bool,
}

impl TokenReplayTransport {
    fn new(tokens: Vec<Tokens>) -> Self {
        Self {
            pending_tokens: VecDeque::from(tokens),
            reset_mode: ResetConnectionMode::None,
            reset_dispatched: false,
            known_dead: false,
        }
    }
}

#[async_trait]
impl TdsTokenStreamReader for TokenReplayTransport {
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
        if let Some(tok) = self.pending_tokens.pop_front() {
            return Ok(RowReadResult::Token(tok));
        }
        Err(crate::error::Error::ConnectionClosed("test".to_string()))
    }

    // The scripted transport surfaces every queued token as a control token and
    // has no row bytes, so `receive_row_header` likewise only ever yields a
    // `RowHeader::Token`, never `Positioned`.
    async fn receive_row_header(
        &mut self,
        _context: &ParserContext,
        _remaining_request_timeout: Option<Duration>,
        _cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        if let Some(tok) = self.pending_tokens.pop_front() {
            return Ok(RowHeader::Token(tok));
        }
        Err(crate::error::Error::ConnectionClosed("test".to_string()))
    }

    // The scripted transport surfaces every queued token as a control token and
    // has no row bytes, so it never produces a `RowPaused` / `PlpPaused` result;
    // these resume paths are therefore unreachable for it.
    async fn resume_row_into(
        &mut self,
        _pause_state: RowPauseState,
        _remaining_request_timeout: Option<Duration>,
        _cancel_handle: Option<&CancelHandle>,
        _plan: ColumnPolicy,
        _writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
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
impl TransportSslHandler for TokenReplayTransport {
    async fn enable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }
    async fn disable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }
}

#[async_trait]
impl NetworkWriter for TokenReplayTransport {
    async fn send(&mut self, _data: &[u8]) -> TdsResult<()> {
        Ok(())
    }
    fn packet_size(&self) -> u32 {
        4096
    }
    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
        NegotiatedEncryptionSetting::NoEncryption
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
impl NetworkReader for TokenReplayTransport {
    fn packet_size(&self) -> u32 {
        4096
    }
}

#[async_trait]
impl TdsTransport for TokenReplayTransport {
    fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        self
    }
    fn reset_reader(&mut self) {}
    fn packet_size(&self) -> u32 {
        4096
    }
    async fn close_transport(&mut self) -> TdsResult<()> {
        Ok(())
    }
    async fn send_attention_with_timeout(&mut self, _timeout: Duration) -> TdsResult<bool> {
        Ok(false)
    }
    fn is_connection_dead(&self) -> bool {
        true
    }
    fn connection_known_dead(&self) -> bool {
        self.known_dead
    }
    fn mark_known_dead(&mut self) {
        self.known_dead = true;
    }
}

/// Builds a [`TdsClient`] whose transport replays `tokens`. Combine with the
/// public statement-wise navigation API
/// ([`TdsClient::execute`](crate::connection::tds_client::TdsClient::execute)
/// / [`advance`](crate::connection::tds_client::TdsClient::advance))
/// to position the client on a scripted result before handing it to a
/// consumer under test.
pub fn tds_client_from_tokens(tokens: Vec<ScriptedToken>) -> TdsClient {
    let tokens: Vec<Tokens> = tokens.into_iter().map(|t| t.0).collect();
    let transport = AnyTransport::dynamic(TokenReplayTransport::new(tokens));
    let negotiated_settings = create_test_negotiated_settings_internal();
    let execution_context = ExecutionContext::new();
    let client_context = ClientContext::with_data_source("tcp:localhost,1433");
    TdsClient::new(
        transport,
        negotiated_settings,
        execution_context,
        client_context,
        Vec::new(),
    )
}

/// Like [`tds_client_from_tokens`], but the returned client already reports an
/// active local transaction with `descriptor`, as though the server had sent a
/// `BeginTransaction` ENVCHANGE.
///
/// Consumer tests use this to reach paths guarded by `has_active_transaction()`
/// — notably the connection-pool reset's rollback-before-reset branch — which
/// otherwise cannot be entered, because the scripted transport only surfaces
/// tokens during a round trip that happens after the guard is evaluated.
pub fn tds_client_from_tokens_in_transaction(
    tokens: Vec<ScriptedToken>,
    descriptor: u64,
) -> TdsClient {
    let tokens: Vec<Tokens> = tokens.into_iter().map(|t| t.0).collect();
    let transport = AnyTransport::dynamic(TokenReplayTransport::new(tokens));
    let negotiated_settings = create_test_negotiated_settings_internal();
    let mut execution_context = ExecutionContext::new();
    execution_context.set_transaction_descriptor(descriptor);
    let client_context = ClientContext::with_data_source("tcp:localhost,1433");
    TdsClient::new(
        transport,
        negotiated_settings,
        execution_context,
        client_context,
        Vec::new(),
    )
}

/// An empty COLMETADATA token — a row-returning result set with zero columns.
pub fn col_metadata_empty() -> ScriptedToken {
    ScriptedToken(Tokens::ColMetadata(ColMetadataToken::default()))
}

/// A `Vec<ColumnMetadata>` of `n` nullable `int` columns named `c1..=cn`.
///
/// For consumer-side tests (e.g. the ODBC `SQLGetData` column-range and
/// forward-only guards) that only need a result set with a given column count;
/// the type detail is irrelevant to those checks.
pub fn int_columns(n: usize) -> Vec<ColumnMetadata> {
    (1..=n)
        .map(|i| ColumnMetadata {
            user_type: 0,
            flags: 0x01, // nullable
            type_info: TypeInfo::fixed_len(TdsDataType::Int4).expect("Int4 is a fixed-length type"),
            data_type: TdsDataType::Int4,
            column_name: format!("c{i}"),
            multi_part_name: None,
            crypto_metadata: None,
        })
        .collect()
}

/// A DONE token with the MORE flag set (more results follow in the batch).
pub fn done_more() -> ScriptedToken {
    ScriptedToken(Tokens::Done(DoneToken {
        status: DoneStatus::MORE,
        cur_cmd: CurrentCommand::Insert,
        row_count: 0,
    }))
}

/// A terminal DONE token (no more results — end of batch).
pub fn done_no_more() -> ScriptedToken {
    ScriptedToken(Tokens::Done(DoneToken {
        status: DoneStatus::FINAL,
        cur_cmd: CurrentCommand::Insert,
        row_count: 0,
    }))
}

/// A `RollbackTransaction` ENVCHANGE token — the acknowledgement the server
/// emits for a Transaction Manager rollback request, clearing the client's
/// transaction descriptor.
pub fn env_change_rollback_transaction() -> ScriptedToken {
    ScriptedToken(Tokens::EnvChange(EnvChangeToken {
        sub_type: EnvChangeTokenSubType::RollbackTransaction,
        change_type: EnvChangeContainer::from((0u64, 0u64)),
    }))
}

/// A `ResetConnection` ENVCHANGE token — the acknowledgement the server emits
/// when it processes a RESETCONNECTION request. Script it ahead of a terminal
/// DONE to drive [`TdsClient::reset_connection`](crate::connection::tds_client::TdsClient::reset_connection)
/// to completion in a consumer test.
pub fn env_change_reset_connection() -> ScriptedToken {
    ScriptedToken(Tokens::EnvChange(EnvChangeToken {
        sub_type: EnvChangeTokenSubType::ResetConnection,
        change_type: EnvChangeContainer::from((0u32, 0u32)),
    }))
}

/// A DONE token carrying a row count and the MORE flag (e.g. a DML statement
/// followed by more statements in the batch).
pub fn done_more_with_count(row_count: u64) -> ScriptedToken {
    ScriptedToken(Tokens::Done(DoneToken {
        status: DoneStatus::MORE | DoneStatus::COUNT,
        cur_cmd: CurrentCommand::Insert,
        row_count,
    }))
}

/// An INFO token (surfaces as a diagnostic message, e.g. from PRINT / low
/// severity RAISERROR).
pub fn info(number: u32, severity: u8, message: &str) -> ScriptedToken {
    ScriptedToken(Tokens::Info(InfoToken {
        number,
        state: 1,
        severity,
        message: message.to_string(),
        server_name: "test-server".to_string(),
        proc_name: String::new(),
        line_number: 1,
    }))
}

// ── Byte-level replay harness (test-only) ──────────────────────────────────
//
// Unlike [`TokenReplayTransport`], which replays pre-parsed [`Tokens`], this
// transport runs raw TDS bytes through the real token parsers so `drain_stream`
// exercises actual ROW decoding. It depends on the `cfg(test)`-only
// `MockReader`, so it is gated behind `cfg(test)` and is unavailable to the
// downstream `test-util` feature build.
#[cfg(test)]
pub(crate) mod byte_stream {
    use super::*;
    use crate::token::parsers::common::test_utils::MockReader;

    struct ByteStreamTransport {
        reader: MockReader,
        registry: crate::io::token_stream::GenericTokenParserRegistry,
        nbc_bitmap_scratch: Option<std::sync::Arc<[u8]>>,
        known_dead: bool,
    }

    impl ByteStreamTransport {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                reader: MockReader::new(bytes),
                registry: crate::io::token_stream::GenericTokenParserRegistry::default(),
                nbc_bitmap_scratch: None,
                known_dead: false,
            }
        }
    }

    impl std::fmt::Debug for ByteStreamTransport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ByteStreamTransport").finish()
        }
    }

    #[async_trait]
    impl TdsTokenStreamReader for ByteStreamTransport {
        async fn receive_token(
            &mut self,
            context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
        ) -> TdsResult<Tokens> {
            crate::io::token_stream::receive_token_internal(
                &mut self.reader,
                &self.registry,
                context,
            )
            .await
        }

        async fn receive_row_into(
            &mut self,
            context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
            plan: ColumnPolicy,
            writer: &mut (dyn RowWriter + Send),
        ) -> TdsResult<RowReadResult> {
            crate::io::token_stream::receive_row_into_internal(
                &mut self.reader,
                &self.registry,
                context,
                plan,
                writer,
                &mut self.nbc_bitmap_scratch,
            )
            .await
        }

        // Scripted byte streams drive the drain path, which reads whole rows or
        // control tokens; positioning-only reads are unused, so this simply
        // decodes the next token and surfaces it as a control-token header.
        async fn receive_row_header(
            &mut self,
            context: &ParserContext,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
        ) -> TdsResult<RowHeader> {
            let token = crate::io::token_stream::receive_token_internal(
                &mut self.reader,
                &self.registry,
                context,
            )
            .await?;
            Ok(RowHeader::Token(token))
        }

        async fn resume_row_into(
            &mut self,
            _pause_state: RowPauseState,
            _remaining_request_timeout: Option<Duration>,
            _cancel_handle: Option<&CancelHandle>,
            _plan: ColumnPolicy,
            _writer: &mut (dyn RowWriter + Send),
        ) -> TdsResult<RowReadResult> {
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
    impl TransportSslHandler for ByteStreamTransport {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
        async fn disable_ssl(&mut self) -> TdsResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl NetworkWriter for ByteStreamTransport {
        async fn send(&mut self, _data: &[u8]) -> TdsResult<()> {
            Ok(())
        }
        fn packet_size(&self) -> u32 {
            4096
        }
        fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
            NegotiatedEncryptionSetting::NoEncryption
        }
        fn set_reset_mode(&mut self, _mode: ResetConnectionMode) {}
        fn take_reset_mode(&mut self) -> ResetConnectionMode {
            ResetConnectionMode::None
        }
        // This transport never carries a reset, so there is nothing to record.
        fn note_reset_dispatched(&mut self) {}
        fn take_reset_dispatched(&mut self) -> bool {
            false
        }
    }

    #[async_trait]
    impl NetworkReader for ByteStreamTransport {
        fn packet_size(&self) -> u32 {
            4096
        }
    }

    #[async_trait]
    impl TdsTransport for ByteStreamTransport {
        fn as_writer(&mut self) -> &mut dyn NetworkWriter {
            self
        }
        fn reset_reader(&mut self) {}
        fn packet_size(&self) -> u32 {
            4096
        }
        async fn close_transport(&mut self) -> TdsResult<()> {
            Ok(())
        }
        async fn send_attention_with_timeout(&mut self, _timeout: Duration) -> TdsResult<bool> {
            Ok(false)
        }
        fn is_connection_dead(&self) -> bool {
            false
        }
        fn connection_known_dead(&self) -> bool {
            self.known_dead
        }
        fn mark_known_dead(&mut self) {
            self.known_dead = true;
        }
    }

    /// Builds a [`TdsClient`] whose transport replays raw TDS `bytes` through the
    /// real token parsers.
    pub(crate) fn tds_client_over_raw_bytes(bytes: Vec<u8>) -> TdsClient {
        let transport = AnyTransport::dynamic(ByteStreamTransport::new(bytes));
        let negotiated_settings = create_test_negotiated_settings_internal();
        let execution_context = ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        )
    }

    /// Same as [`tds_client_over_raw_bytes`] but with Always Encrypted negotiated,
    /// so COLMETADATA reads expect the CEK-table prefix. Used to prove the drain
    /// parses trailing result sets with the same encryption awareness as the
    /// normal read path.
    pub(crate) fn tds_client_over_raw_bytes_with_column_encryption(bytes: Vec<u8>) -> TdsClient {
        use crate::message::features::always_encrypted::AlwaysEncryptedFeature;
        use crate::message::login::Feature;

        let transport = AnyTransport::dynamic(ByteStreamTransport::new(bytes));
        let mut negotiated_settings = create_test_negotiated_settings_internal();
        let mut feature = AlwaysEncryptedFeature::default();
        feature.set_acknowledged(true);
        negotiated_settings
            .session_settings
            .supported_features
            .push(Box::new(feature));
        let execution_context = ExecutionContext::new();
        let client_context = ClientContext::with_data_source("tcp:localhost,1433");
        TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            client_context,
            Vec::new(),
        )
    }
}
