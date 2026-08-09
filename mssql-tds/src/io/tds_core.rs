// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-synchronous row-fetch parse body (sans-I/O core).
//!
//! [`TdsCore::step_row`] owns the single production framing loop for a ROW /
//! NBCROW token: it consumes the row header atomically, decodes every
//! synchronously-supported non-PLP cell in place over the [`PacketBuffer`], and
//! carries its whole-column cursor in the existing [`RowPauseState`]. It never
//! performs I/O — on buffer underflow it returns [`RowStep::NeedBytes`], and for
//! any cell whose byte pull still lives in an async seam (eager PLP, PLP
//! streaming pause, legacy text-pointer LOBs, rare fallback types, or the
//! Always-Encrypted fallback) it returns [`RowStep::AsyncColumn`].
//!
//! The async driver ([`crate::io::token_stream::drive_row_over_buffer`]) is the
//! only place that awaits: it refills the buffer on `NeedBytes` and services an
//! `AsyncColumn` through the existing L4 async seam, then re-drives `step_row`
//! from the shared cursor. The async and sync (future P4c) drivers differ only
//! in that refill step; the parse body here is shared.
//!
//! [`TdsCore::step_token`] is the non-row analog (P4b): it decodes the bounded,
//! length-delimited non-row tokens (DONE family, RETURNSTATUS, ORDER, ERROR,
//! INFO, ENVCHANGE) in place over the [`PacketBuffer`] via the pure-sync leaf in
//! [`crate::io::sync_token`], returning [`TokenStep::NeedBytes`] on underflow at
//! token entry (nothing consumed, restartable from the token byte). Tokens whose
//! bodies are unbounded or carry embedded values/PLP — COLMETADATA, RETURNVALUE,
//! SESSIONSTATE, FEATUREEXTACK — plus the login/handshake tokens are handed back
//! as [`TokenStep::AsyncToken`] and serviced by the existing async parser on the
//! seam. Their pure-sync inversion is **deferred** to a later, parent-gated layer
//! that would introduce a sanctioned mid-token `TokenPauseState` cursor; after
//! P4b the receive path is sync-parse for its bounded tokens and async-seamed for
//! the unbounded ones, not fully sync.

use crate::core::TdsResult;
use crate::datatypes::row_writer::RowWriter;
use crate::datatypes::sync_decoder;
use crate::io::packet_buffer::{NeedBytes, PacketBuffer};
use crate::io::sync_token;
use crate::io::token_stream::{ParserContext, RowPauseState, extract_row_context};
use crate::token::tokens::{TokenType, Tokens};

/// Zero-sized owner of the synchronous row-fetch parse body.
pub(crate) struct TdsCore;

/// Outcome of one synchronous [`TdsCore::step_row`] call.
pub(crate) enum RowStep {
    /// The whole row was decoded into the writer.
    RowWritten,
    /// The header byte named a non-row token; the async driver dispatches it.
    Token(TokenType),
    /// Decoding paused after a column (`RowWriter::pause_after_column`); resume
    /// from the carried [`RowPauseState`].
    RowPaused(RowPauseState),
    /// The cell at `col` must be decoded through the async seam (eager PLP,
    /// p7 PLP streaming pause, p4d legacy LOBs, rare fallback types, or the AE
    /// fallback). The driver services it and re-drives `step_row` at `col + 1`.
    ///
    /// The two yield reasons are named for *why* they yield:
    /// - [`RowStep::NeedBytes`]`{shortfall}` = this cell is **BOUNDED**; give it
    ///   `N` more bytes and the whole cell fits, so the sync core will finish it.
    /// - `AsyncColumn` = this column is **UNBOUNDED**; the driver owns its
    ///   chunked I/O and streams it chunk-at-a-time through the shared
    ///   `plp_collect_step` / `collect_plp_bytes` leaf.
    ///
    /// Do NOT collapse into NeedBytes — yielding the column preserves bounded
    /// residency; forcing PLP through `ensure(full-len)` would materialize the
    /// whole multi-GB VARCHAR(MAX) and reintroduce the footgun, and would need a
    /// new mid-value resumable machine. This is an intentional sans-I/O shape,
    /// not an unfinished inversion: it is what keeps L4b residency bounded to one
    /// chunk while a LOB larger than the buffer streams past.
    AsyncColumn { col: usize },
    /// The buffer underflowed; the driver refills and re-drives with the same
    /// (unchanged) cursor. Nothing was consumed on this step.
    NeedBytes(NeedBytes),
}

/// Outcome of one synchronous [`TdsCore::step_token`] call.
///
/// The non-row analog of [`RowStep`]. Bounded tokens are parsed whole in place
/// and returned as [`TokenStep::Parsed`]; unbounded / value-carrying tokens are
/// yielded to the async driver as [`TokenStep::AsyncToken`] (see the module-level
/// note on the deferred sync inversion of those tokens).
pub(crate) enum TokenStep {
    /// The whole token body was decoded in place; the driver returns it.
    Parsed(Tokens),
    /// The token is COLMETADATA / RETURNVALUE / SESSIONSTATE / FEATUREEXTACK, a
    /// login/handshake token, or an unrecognized-but-dispatchable token: its
    /// body is not pure-sync yet, so the driver services it through the existing
    /// async parser on the seam. A genuine yield-to-driver, never a hidden
    /// `block_on` — this is the [`RowStep::AsyncColumn`] discipline for tokens,
    /// leaving the seam shaped for a future P4c sync leaf/cursor underneath it.
    AsyncToken(TokenType),
    /// The buffer underflowed at token entry; nothing was consumed, so the
    /// driver refills and re-drives, re-peeking the same token byte.
    NeedBytes(NeedBytes),
}

/// Result of consuming the row header at row entry.
enum HeaderStep {
    Started(RowPauseState),
    Token(TokenType),
    NeedBytes(NeedBytes),
}

impl TdsCore {
    /// Advances one row-decode step over `buf` without performing I/O.
    ///
    /// `resume` is the driver-owned whole-column cursor. On `None` the header is
    /// consumed atomically and `resume` is set to the fresh cursor; on `Some` the
    /// header is skipped and decoding continues from `next_column_index`. This
    /// unifies the fresh-row and resume-from-pause paths into one body.
    pub(crate) fn step_row(
        buf: &mut PacketBuffer,
        resume: &mut Option<RowPauseState>,
        context: &ParserContext,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowStep> {
        if resume.is_none() {
            match Self::begin_row_header(buf, context)? {
                HeaderStep::Started(state) => *resume = Some(state),
                HeaderStep::Token(token_type) => return Ok(RowStep::Token(token_type)),
                HeaderStep::NeedBytes(need) => return Ok(RowStep::NeedBytes(need)),
            }
        }

        let state = resume.as_mut().expect("row cursor set after header");
        let len = state.columns.len();
        loop {
            let col = state.next_column_index;
            if col >= len {
                return Ok(RowStep::RowWritten);
            }

            let is_null = state
                .nbc_null_bitmap
                .as_ref()
                .map(|bitmap| bitmap[col / 8] & (1 << (col % 8)) != 0)
                .unwrap_or(false);

            if is_null {
                writer.write_null(col);
            } else {
                let meta = &state.columns[col];
                if meta.crypto_metadata.is_none() && sync_decoder::is_supported(meta) {
                    match sync_decoder::column_wire_len(buf, meta) {
                        Ok(total) => {
                            if let Err(need) = buf.ensure(total) {
                                return Ok(RowStep::NeedBytes(need));
                            }
                            sync_decoder::decode_column_body(buf, meta, col, writer)?;
                        }
                        Err(need) => return Ok(RowStep::NeedBytes(need)),
                    }
                } else {
                    return Ok(RowStep::AsyncColumn { col });
                }
            }

            if writer.pause_after_column(col) && col + 1 < len {
                return Ok(RowStep::RowPaused(state.resume_at(col + 1)));
            }
            state.next_column_index = col + 1;
        }
    }

    /// Consumes the ROW / NBCROW header as one atomic step.
    ///
    /// The header (token byte, plus the fixed-width NBCROW null bitmap) is
    /// ensured resident before any byte is taken, so a `NeedBytes` shortfall
    /// leaves the buffer position unchanged and a re-drive re-peeks the same
    /// token byte. This is what lets the whole-column [`RowPauseState`] cursor be
    /// the sole resume granularity: the "token consumed, bitmap pending" state
    /// never exists, so no new resumable machine is needed for the header.
    fn begin_row_header(buf: &mut PacketBuffer, context: &ParserContext) -> TdsResult<HeaderStep> {
        let first = match buf.peek_bytes(1) {
            Some(bytes) => bytes[0],
            None => return Ok(HeaderStep::NeedBytes(NeedBytes { shortfall: 1 })),
        };
        let token_type: TokenType = first.try_into()?;

        match token_type {
            TokenType::Row => {
                buf.take_u8()?;
                let (columns, decryptor) = extract_row_context(context)?;
                Ok(HeaderStep::Started(RowPauseState {
                    next_column_index: 0,
                    columns: columns.to_vec(),
                    nbc_null_bitmap: None,
                    decryptor: decryptor.cloned(),
                }))
            }
            TokenType::NbcRow => {
                let (columns, decryptor) = extract_row_context(context)?;
                let bitmap_len = columns.len().div_ceil(8);
                if let Err(need) = buf.ensure(1 + bitmap_len) {
                    return Ok(HeaderStep::NeedBytes(need));
                }
                buf.take_u8()?;
                let bitmap = buf.take_bytes(bitmap_len)?;
                Ok(HeaderStep::Started(RowPauseState {
                    next_column_index: 0,
                    columns: columns.to_vec(),
                    nbc_null_bitmap: Some(bitmap),
                    decryptor: decryptor.cloned(),
                }))
            }
            _ => {
                buf.take_u8()?;
                Ok(HeaderStep::Token(token_type))
            }
        }
    }

    /// Advances one non-row token-decode step over `buf` without performing I/O.
    ///
    /// Peeks the token byte and classifies it:
    /// - Bounded tokens (DONE family, RETURNSTATUS, ORDER, ERROR, INFO,
    ///   ENVCHANGE — see [`sync_token::is_sync_token`]) are decoded whole in
    ///   place. The token byte is not consumed until the entire length-delimited
    ///   body is resident, so a shortfall returns [`TokenStep::NeedBytes`] with
    ///   the buffer position unchanged and the step is restartable from the token
    ///   byte — the token-atomic analog of [`Self::begin_row_header`].
    /// - Every other token (the value-carrying (b) tokens, login/handshake
    ///   tokens, and any dispatchable token this core does not sync-parse) has
    ///   its token byte consumed and is returned as [`TokenStep::AsyncToken`] for
    ///   the driver to service through the existing async parser.
    ///
    /// A malformed token byte propagates the same `TryFrom` error the async path
    /// raised; nothing is consumed on that error (only the byte was peeked).
    pub(crate) fn step_token(
        buf: &mut PacketBuffer,
        context: &ParserContext,
    ) -> TdsResult<TokenStep> {
        let first = match buf.peek_bytes(1) {
            Some(bytes) => bytes[0],
            None => return Ok(TokenStep::NeedBytes(NeedBytes { shortfall: 1 })),
        };
        let token_type: TokenType = first.try_into()?;

        if !sync_token::is_sync_token(&token_type) {
            buf.take_u8()?;
            return Ok(TokenStep::AsyncToken(token_type));
        }

        // Total on-wire size including the token byte. Fixed-width tokens are
        // known outright; length-prefixed tokens carry a u16 body length at
        // body offset 0 (just past the token byte), so the whole token is
        // `1 + 2 + len`.
        let total = match sync_token::fixed_body_len(&token_type) {
            Some(body) => 1 + body,
            None => {
                if let Err(need) = buf.ensure(3) {
                    return Ok(TokenStep::NeedBytes(need));
                }
                let prefix = buf.peek_bytes(3).expect("ensured 3 bytes");
                1 + 2 + u16::from_le_bytes([prefix[1], prefix[2]]) as usize
            }
        };

        if let Err(need) = buf.ensure(total) {
            return Ok(TokenStep::NeedBytes(need));
        }

        buf.take_u8()?;
        Ok(TokenStep::Parsed(sync_token::parse_token_body(
            buf, token_type, context,
        )?))
    }
}
