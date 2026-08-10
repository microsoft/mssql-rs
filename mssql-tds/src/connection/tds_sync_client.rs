// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synchronous, reactor-free row-fetch client over the blocking TDS edge.
//!
//! [`TdsSyncClient`] is the first surfaced consumer of the L3 blocking row
//! driver. It mirrors the per-row *fetch* surface of the async
//! [`TdsClient`](crate::connection::tds_client::TdsClient) — the methods a
//! `block_on(next_row())` consumer calls in its hot loop — but drives them with
//! no tokio reactor: it reads TDS packets straight off an owned blocking socket
//! and re-uses [`TdsCore::step_row`](crate::io::tds_core::TdsCore) verbatim via
//! [`drive_row_over_buffer_blocking`]. The parse body is identical to the async
//! path; only the byte-pull edge blocks instead of awaiting.
//!
//! # OWNING (into_sync/into_async), NOT borrowed
//!
//! > OWNING (into_sync/into_async), NOT borrowed: a SyncRowFetcher<'a> borrowing
//! > dbc.client cannot persist across SQLFetch FFI calls (storing it beside the
//! > client it borrows = self-referential, illegal in safe Rust) ⇒ re-created
//! > every SQLFetch ⇒ fd into_std/revert flip PER FETCH. On our base
//! > fetch.rs:133 = block_on(next_row()) PER ROW ⇒ borrowed flips PER ROW
//! > (millions/1M-row set). Owning stores TdsSyncClient by value ⇒ fd stays
//! > blocking across the whole result set (~2 flips). Prototype ii revert PROVEN
//! > sound @13.2µs. Borrowed is DEAD; do not reintroduce.
//!
//! # Reversible, terminal drop
//!
//! The flip is reversible: [`TdsSyncClient::into_async`] hands the socket back to
//! a tokio stream and returns the original [`TdsClient`], so control-plane work
//! (execute/advance/close) stays async. Abandoning a `TdsSyncClient` without
//! reverting performs a **terminal clean-close** — the owned `std::net::TcpStream`
//! closes on drop (its natural RAII), which is why there is deliberately no
//! explicit `Drop` impl. There is **never** a Drop-driven fd-revert-to-async;
//! that is the dead borrowed RAII per-fetch anti-pattern.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;

use crate::connection::tds_client::{ReturnStatus, TdsClient, TokenOutcome};
use crate::core::TdsResult;
use crate::datatypes::column_values::ColumnValues;
use crate::datatypes::row_writer::{DefaultRowWriter, RowWriter};
use crate::error::{Error, SqlErrorInfo, SqlInfoMessage};
use crate::io::blocking_reader::BlockingPacketReader;
use crate::io::std_byte_source::StdTcpByteSource;
use crate::io::token_stream::{
    ParserContext, PlpPauseState, RowPauseState, RowReadResult, drive_row_over_buffer_blocking,
};
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::{SqlCollation, Tokens};

/// Outcome of [`TdsClient::into_sync`]. The flip is opportunistic: only a raw TCP
/// transport can be handed to the blocking edge.
///
/// The variant payloads are the established clients themselves, so this enum is
/// intentionally large; boxing would alter the frozen public surface
/// (`Converted(TdsSyncClient)` / `NotEligible(TdsClient)`), so the size lint is
/// suppressed rather than changing the shape.
#[allow(clippy::large_enum_variant)]
pub enum SyncConversion {
    /// The connection was a raw TCP socket and is now driven synchronously.
    Converted(TdsSyncClient),
    /// The transport could not be flipped (e.g. TLS). The async client is
    /// returned **unchanged** so the caller keeps using `block_on` — not an error.
    NotEligible(TdsClient),
    /// The flip was attempted but constructing the blocking edge failed; the
    /// connection is unusable.
    Failed(Error),
}

/// Mirror of [`ActiveRowReadState`](crate::connection::tds_client) for the sync
/// edge: the pause cursor carried between fetch calls.
enum SyncRowState {
    Idle,
    RowPaused(Box<RowPauseState>),
    PlpPaused(Box<PlpPauseState>),
}

/// A synchronous, reactor-free client exposing the TDS row-fetch surface over an
/// owned blocking socket. Built by [`TdsClient::into_sync`]; reverted by
/// [`TdsSyncClient::into_async`]. See the [module docs](self) for the
/// owning-vs-borrowed rationale and drop semantics.
///
/// All connection/result-set state (metadata, INFO buffer, read-till-end flag,
/// `count_map`, return values) lives on the owned [`TdsClient`] `inner`; token
/// side-effects flow through [`TdsClient::apply_row_read_token`], the *same*
/// handler the async path uses, so the two clients cannot drift. This wrapper
/// adds only the blocking edge and the sync-side pause cursor.
pub struct TdsSyncClient {
    /// The async client, gutted of its socket + read residual (both now owned by
    /// `reader`). It remains the single source of truth for all connection state
    /// so token side-effects mutate the same fields as the async path, and so
    /// [`into_async`](Self::into_async) can hand the socket back.
    inner: TdsClient,
    reader: BlockingPacketReader<StdTcpByteSource>,
    /// Empty-slice fallback for [`get_metadata`](Self::get_metadata) when the
    /// connection has no current metadata.
    empty_metadata: Vec<ColumnMetadata>,
    active: SyncRowState,
    /// Per-request timeout applied as a read deadline before each fetch.
    request_timeout: Option<Duration>,
    /// Runtime handle captured at conversion, needed to re-register the socket
    /// with the reactor in [`into_async`](Self::into_async).
    runtime_handle: Option<Handle>,
}

impl TdsSyncClient {
    /// Builds a sync client from an established (raw-TCP) connection. Called only
    /// by [`TdsClient::into_sync`], which has already extracted the socket and
    /// seeded the blocking reader with the transport's residual bytes.
    pub(crate) fn from_established(
        inner: TdsClient,
        reader: BlockingPacketReader<StdTcpByteSource>,
        runtime_handle: Option<Handle>,
        request_timeout: Option<Duration>,
    ) -> Self {
        Self {
            inner,
            reader,
            empty_metadata: Vec::new(),
            active: SyncRowState::Idle,
            request_timeout,
            runtime_handle,
        }
    }

    /// Reverts to the async [`TdsClient`], re-registering the owned socket with
    /// the tokio reactor and handing back any unconsumed bytes so the async path
    /// resumes byte-identically.
    ///
    /// Errors (fd known-dead / poisoned) if the runtime handle is missing or the
    /// socket cannot be re-registered; the connection is then consumed and closes.
    pub fn into_async(mut self) -> TdsResult<TdsClient> {
        let handle = self.runtime_handle.take().ok_or_else(|| {
            Error::UsageError(
                "into_async requires the tokio runtime handle captured at into_sync; \
                 the connection was created outside a runtime context"
                    .to_string(),
            )
        })?;

        let residual = self.reader.take_residual();
        let std_stream = self.reader.into_source().into_stream();
        // Re-arm non-blocking mode before re-registering with the reactor.
        std_stream.set_nonblocking(true)?;

        let _guard = handle.enter();
        // `from_std` touches the reactor, so it must run inside `handle.enter()`.
        let tokio_stream = tokio::net::TcpStream::from_std(std_stream)?;

        let mut inner = self.inner;
        inner
            .transport
            .restore_blocking_parts(tokio_stream, residual)?;
        Ok(inner)
    }

    /// Fetches up to `max_rows` rows into `out`, recycling row buffers from
    /// `spare` to amortize per-row allocation. Stops at `max_rows`, at the
    /// result-set boundary, or when the set is exhausted; returns the count
    /// fetched. Authored fresh as a thin loop over the reactor-free
    /// [`next_row_into`](Self::next_row_into); correctness derives from that
    /// method's differential parity with the async oracle.
    pub fn fetch_rows_batch(
        &mut self,
        out: &mut Vec<Vec<ColumnValues>>,
        mut spare: Vec<Vec<ColumnValues>>,
        max_rows: usize,
    ) -> TdsResult<usize> {
        let col_count = self
            .inner
            .current_metadata
            .as_ref()
            .map_or(0, |m| m.columns.len());
        let mut fetched = 0usize;
        while fetched < max_rows {
            if !self.maybe_has_unread_rows() {
                break;
            }
            let recycled = spare.pop().unwrap_or_else(|| Vec::with_capacity(col_count));
            let mut writer = DefaultRowWriter::from_recycled(recycled);
            if self.get_next_row_into(&mut writer)? {
                out.push(writer.take_row());
                fetched += 1;
            } else {
                break;
            }
        }
        Ok(fetched)
    }

    /// Fetches the next row, or `None` at the end of the result set. Sync twin of
    /// [`ResultSet::next_row`](crate::connection::tds_client::ResultSet::next_row).
    pub fn next_row(&mut self) -> TdsResult<Option<Vec<ColumnValues>>> {
        if !self.maybe_has_unread_rows() {
            return Ok(None);
        }
        let col_count = self
            .inner
            .current_metadata
            .as_ref()
            .map_or(0, |m| m.columns.len());
        let mut writer = DefaultRowWriter::new(col_count);
        if self.get_next_row_into(&mut writer)? {
            Ok(Some(writer.take_row()))
        } else {
            Ok(None)
        }
    }

    /// Decodes the next row directly into `writer`, returning `true` if a row was
    /// written or `false` at the end of the result set. Sync twin of
    /// [`ResultSet::next_row_into`](crate::connection::tds_client::ResultSet::next_row_into).
    pub fn next_row_into(&mut self, writer: &mut (dyn RowWriter + Send)) -> TdsResult<bool> {
        if !self.maybe_has_unread_rows() {
            return Ok(false);
        }
        self.get_next_row_into(writer)
    }

    /// Drains and returns the informational (INFO/PRINT) messages captured so
    /// far. Sync twin of
    /// [`TdsClient::take_info_messages`](crate::connection::tds_client::TdsClient::take_info_messages).
    pub fn take_info_messages(&mut self) -> Vec<SqlInfoMessage> {
        self.inner.take_info_messages()
    }

    /// The current result set's column metadata, or an empty slice when none is
    /// available. Sync twin of
    /// [`ResultSet::get_metadata`](crate::connection::tds_client::ResultSet::get_metadata).
    pub fn get_metadata(&self) -> &Vec<ColumnMetadata> {
        self.inner
            .current_metadata
            .as_ref()
            .map(|m| &m.columns)
            .unwrap_or(&self.empty_metadata)
    }

    /// Whether more rows may remain in the current result set. Sync twin of
    /// [`ResultSet::maybe_has_unread_rows`](crate::connection::tds_client::ResultSet::maybe_has_unread_rows).
    pub fn maybe_has_unread_rows(&self) -> bool {
        !self.inner.current_result_set_has_been_read_till_end
    }

    /// Whether the active PLP stream (if any) has reached its end. Sync twin of
    /// [`ResultSet::active_plp_reached_end`](crate::connection::tds_client::ResultSet::active_plp_reached_end).
    pub fn active_plp_reached_end(&self) -> bool {
        match &self.active {
            SyncRowState::PlpPaused(plp_state) => plp_state.reached_end(),
            _ => true,
        }
    }

    /// The collation of the active PLP stream, if any. Sync twin of
    /// [`ResultSet::active_plp_collation`](crate::connection::tds_client::ResultSet::active_plp_collation).
    pub fn active_plp_collation(&self) -> Option<SqlCollation> {
        match &self.active {
            SyncRowState::PlpPaused(plp_state) => plp_state.collation(),
            _ => None,
        }
    }

    /// The sync mirror of `TdsClient::get_next_row_into`: drives the L3 blocking
    /// row driver over the owned buffer, resuming a paused row first and handling
    /// non-row tokens to the result-set boundary.
    fn get_next_row_into(&mut self, writer: &mut (dyn RowWriter + Send)) -> TdsResult<bool> {
        let metadata = match &self.inner.current_metadata {
            Some(metadata) => Arc::clone(metadata),
            None => {
                return Err(Error::UsageError(
                    "No metadata found while fetching the next row. Have you executed a \
                     row-returning query on the async client before converting?"
                        .to_string(),
                ));
            }
        };
        // Always Encrypted decryption is not wired through the sync fetch path in
        // v1: a non-empty CEK table means encrypted columns.
        if !metadata.cek_table.is_empty() {
            return Err(Error::UnimplementedFeature {
                feature: "Always Encrypted columns over the synchronous fetch path".to_string(),
                context: "convert back with into_async() and fetch encrypted result sets \
                          on the async client"
                    .to_string(),
            });
        }

        let mut resume = match std::mem::replace(&mut self.active, SyncRowState::Idle) {
            SyncRowState::Idle => None,
            SyncRowState::RowPaused(pause_state) => Some(*pause_state),
            SyncRowState::PlpPaused(plp_state) => {
                // Resuming a PLP-paused row needs the chunked drain
                // (`read_active_plp_bytes`), which is deferred from the v1 sync
                // surface. Preserve the pause so the accessors stay accurate.
                self.active = SyncRowState::PlpPaused(plp_state);
                return Err(Error::UnimplementedFeature {
                    feature: "resuming a PLP-paused row over the synchronous fetch path"
                        .to_string(),
                    context: "chunked PLP reads (read_active_plp_bytes) are deferred; \
                              fetch large-object rows on the async client"
                        .to_string(),
                });
            }
        };

        self.arm_deadline();
        let context = ParserContext::ColumnMetadata(metadata, None);
        loop {
            let result =
                drive_row_over_buffer_blocking(&mut self.reader, &context, resume.take(), writer)?;
            match result {
                RowReadResult::RowWritten => {
                    writer.end_row();
                    return Ok(true);
                }
                RowReadResult::RowPaused(pause_state) => {
                    self.active = SyncRowState::RowPaused(Box::new(pause_state));
                    return Ok(true);
                }
                RowReadResult::PlpPaused(plp_state) => {
                    self.active = SyncRowState::PlpPaused(Box::new(plp_state));
                    return Ok(true);
                }
                RowReadResult::Token(token) => {
                    if let Some(has_row) = self.handle_row_read_token(token)? {
                        return Ok(has_row);
                    }
                }
            }
        }
    }

    /// The sync fetch shell's non-row-token handler. Delegates every side-effect
    /// to the shared [`TdsClient::apply_row_read_token`] (identical to the async
    /// path), differing only in the ERROR drain: the rest of the batch is drained
    /// to its terminal DONE over the blocking edge before
    /// [`TdsClient::finalize_row_error`] builds the surfaced error.
    fn handle_row_read_token(&mut self, token: Tokens) -> TdsResult<Option<bool>> {
        match self.inner.apply_row_read_token(token)? {
            TokenOutcome::Continue => Ok(None),
            TokenOutcome::Terminal => Ok(Some(false)),
            TokenOutcome::DrainThenError(mut all_errors) => {
                all_errors.extend(self.drain_stream()?);
                Err(self.inner.finalize_row_error(all_errors))
            }
        }
    }

    /// The sync mirror of `TdsClient::drain_stream`: after an ERROR token, read
    /// the rest of the batch to its terminal DONE, collecting further ERROR
    /// tokens so the surfaced error carries the full diagnostic chain.
    ///
    /// Every non-terminal side-effect (`Info`/`EnvChange`/`SessionState`/
    /// `ReturnValue`) is routed through the shared
    /// [`TdsClient::apply_row_read_token`] so it cannot drift from the async
    /// drain; only the terminal DONE, collected ERRORs, and the `ReturnStatus`
    /// capture are handled inline (mirroring the async arms exactly).
    fn drain_stream(&mut self) -> TdsResult<Vec<SqlErrorInfo>> {
        let mut collected_errors: Vec<SqlErrorInfo> = Vec::new();
        let mut scratch = DefaultRowWriter::new(0);
        let context = ParserContext::None(());
        loop {
            match drive_row_over_buffer_blocking(&mut self.reader, &context, None, &mut scratch)? {
                RowReadResult::Token(token) => match token {
                    Tokens::Done(done) | Tokens::DoneProc(done) | Tokens::DoneInProc(done)
                        if !done.has_more() =>
                    {
                        break;
                    }
                    Tokens::Error(error_token) => {
                        collected_errors.push(SqlErrorInfo::from(&error_token));
                    }
                    Tokens::ReturnStatus(return_status) => {
                        self.inner.last_return_status = ReturnStatus::Received(return_status.value);
                    }
                    token @ (Tokens::Info(_)
                    | Tokens::EnvChange(_)
                    | Tokens::SessionState(_)
                    | Tokens::ReturnValue(_)) => {
                        self.inner.apply_row_read_token(token)?;
                    }
                    _ => {}
                },
                // No metadata context here, so no rows are decoded; anything but a
                // token during drain is a protocol violation.
                _ => {
                    return Err(Error::ProtocolError(
                        "Unexpected row payload while draining the token stream after an error"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(collected_errors)
    }

    /// Refreshes the byte source's read deadline from the per-request timeout,
    /// so cancel/deadline checks ride the same receive edge as the async path.
    fn arm_deadline(&mut self) {
        let deadline = self.request_timeout.map(|d| Instant::now() + d);
        self.reader.source_mut().set_deadline(deadline);
    }
}
