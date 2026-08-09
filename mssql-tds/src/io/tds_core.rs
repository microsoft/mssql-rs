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

use crate::core::TdsResult;
use crate::datatypes::row_writer::RowWriter;
use crate::datatypes::sync_decoder;
use crate::io::packet_buffer::{NeedBytes, PacketBuffer};
use crate::io::token_stream::{ParserContext, RowPauseState, extract_row_context};
use crate::token::tokens::TokenType;

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
    /// fallback). Its refill stays in the L4b/L4-async seam by design: hoisting
    /// mid-value LOB state into the sync core would require a new resumable
    /// machine. The driver services it and re-drives `step_row` at `col + 1`.
    AsyncColumn { col: usize },
    /// The buffer underflowed; the driver refills and re-drives with the same
    /// (unchanged) cursor. Nothing was consumed on this step.
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
}
