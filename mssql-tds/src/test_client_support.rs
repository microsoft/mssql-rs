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
use crate::connection::transport::network_transport::TransportSslHandler;
use crate::connection::transport::tds_transport::TdsTransport;
use crate::core::{CancelHandle, NegotiatedEncryptionSetting, TdsResult};
use crate::datatypes::row_writer::RowWriter;
use crate::handler::handler_factory::create_test_negotiated_settings_internal;
use crate::io::reader_writer::{NetworkReader, NetworkWriter};
use crate::io::token_stream::{ParserContext, RowReadResult, TdsTokenStreamReader};
use crate::message::messages::ResetConnectionMode;
use crate::token::tokens::{
    ColMetadataToken, CurrentCommand, DoneStatus, DoneToken, InfoToken, Tokens,
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
}

impl TokenReplayTransport {
    fn new(tokens: Vec<Tokens>) -> Self {
        Self {
            pending_tokens: VecDeque::from(tokens),
            reset_mode: ResetConnectionMode::None,
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
        _writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        if let Some(tok) = self.pending_tokens.pop_front() {
            return Ok(RowReadResult::Token(tok));
        }
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
    }
    fn take_reset_mode(&mut self) -> ResetConnectionMode {
        std::mem::replace(&mut self.reset_mode, ResetConnectionMode::None)
    }
}

#[async_trait]
impl NetworkReader for TokenReplayTransport {
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        buffer.fill(0);
        Ok(0)
    }
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
}

/// Builds a [`TdsClient`] whose transport replays `tokens`. Combine with the
/// public statement-wise navigation API
/// ([`TdsClient::execute_multi_statement`](crate::connection::tds_client::TdsClient::execute_multi_statement)
/// / [`move_to_next_statement`](crate::connection::tds_client::TdsClient::move_to_next_statement))
/// to position the client on a scripted result before handing it to a
/// consumer under test.
pub fn tds_client_from_tokens(tokens: Vec<ScriptedToken>) -> TdsClient {
    let tokens: Vec<Tokens> = tokens.into_iter().map(|t| t.0).collect();
    let transport = Box::new(TokenReplayTransport::new(tokens));
    let negotiated_settings = create_test_negotiated_settings_internal();
    let execution_context = ExecutionContext::new();
    let client_context = ClientContext::with_data_source("tcp:localhost,1433");
    TdsClient::new(
        transport,
        negotiated_settings,
        execution_context,
        client_context,
    )
}

/// An empty COLMETADATA token — a row-returning result set with zero columns.
pub fn col_metadata_empty() -> ScriptedToken {
    ScriptedToken(Tokens::ColMetadata(ColMetadataToken::default()))
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
