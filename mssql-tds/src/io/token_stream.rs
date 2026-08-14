// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::decoder::{GenericDecoder, PlpColumnStream, decrypt_encrypted_column};
use crate::datatypes::row_writer::{DiscardRowWriter, RowWriter, write_column_value};
use crate::io::packet_reader::TdsPacketReader;
use crate::query::metadata::ColumnMetadata;
use crate::security::cell_decryptor::CellDecryptor;
use crate::token::parsers::TokenParser;
use crate::token::parsers::{
    ColInfoTokenParser, ColMetadataTokenParser, DoneInProcTokenParser, DoneProcTokenParser,
    DoneTokenParser, EnvChangeTokenParser, ErrorTokenParser, FeatureExtAckTokenParser,
    FedAuthInfoTokenParser, InfoTokenParser, LoginAckTokenParser, NbcRowTokenParser,
    OrderTokenParser, ReturnStatusTokenParser, ReturnValueTokenParser, RowTokenParser,
    SessionStateTokenParser, SspiTokenParser, TabNameTokenParser,
};
use crate::token::tokens::{ColMetadataToken, TokenType, Tokens};
use async_trait::async_trait;
use core::convert::From;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

#[cfg(fuzzing)]
use crate::error::Error::{OperationCancelledError, TimeoutError};
#[cfg(fuzzing)]
use crate::error::TimeoutErrorType;
#[cfg(fuzzing)]
use crate::token::tokens::DoneStatus;
#[cfg(fuzzing)]
use tokio::time::timeout;

/// Explicit, caller-supplied decode intent for a row.
///
/// Replaces the old `RowWriter::pause_before_first_column` /
/// `pause_after_column` predicates: the decision of *how far to decode* now
/// travels as an argument instead of being polled off the sink. The push
/// consumers (Arrow / N-API / bulk) always use [`ColumnPolicy::DecodeAll`]; the ODBC
/// pull cursor drives the other variants.
///
/// This enum is uniformly *column-level*: every variant answers "what should
/// happen to each column of this row". The row-level "just position, decode
/// nothing" decision lives separately in [`receive_row_header`] /
/// [`RowHeader`], so it no longer has to be smuggled through here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(not(fuzzing))]
pub(crate) enum ColumnPolicy {
    /// Decode every column into the writer, never pause (push sinks).
    DecodeAll,
    /// Skip columns `< target`, decode `target`, then pause after it.
    DecodeOne(usize),
    /// Skip every remaining column, allocating nothing (drain the current row).
    SkipAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(fuzzing)]
pub enum ColumnPolicy {
    DecodeAll,
    DecodeOne(usize),
    SkipAll,
}

/// Result of attempting to read a row directly into a [`RowWriter`].
#[cfg(not(fuzzing))]
#[derive(Debug)]
pub(crate) enum RowReadResult {
    /// A row was decoded directly into the writer via `decode_into`,
    /// bypassing the intermediate `RowToken { all_values: Vec<ColumnValues> }`.
    RowWritten,
    /// A non-row token was received and needs normal handling.
    Token(Tokens),
    /// Row decoding paused after `paused_at_column`; call `resume_row_into` to
    /// continue from the next column.
    ///
    RowPaused(RowPauseState),
    /// Row decoding paused at a PLP column before consuming payload bytes.
    /// Use `read_active_plp_bytes` to stream chunks and then `resume_row_into`
    /// with `plp_state.row_pause_state`.
    ///
    PlpPaused(PlpPauseState),
}

#[cfg(fuzzing)]
#[derive(Debug)]
pub enum RowReadResult {
    RowWritten,
    Token(Tokens),
    RowPaused(RowPauseState),
    PlpPaused(PlpPauseState),
}

/// Outcome of reading only a row *header* — the ROW/NBCROW token byte plus any
/// NBCROW null bitmap — without decoding columns.
///
/// This is the row-level counterpart to the column-level [`ColumnPolicy`]. Splitting
/// it out keeps `ColumnPolicy` uniformly column-level and gives the pull cursor
/// ([`next_row_cursor`](crate::connection::tds_client::TdsClient::next_row_cursor))
/// exactly the two outcomes it can act on, instead of a four-variant
/// [`RowReadResult`] whose row-decode arms are unreachable when only the header
/// is read.
#[cfg(not(fuzzing))]
pub(crate) enum RowHeader {
    /// Positioned on a row before column 0; its columns are still on the wire,
    /// to be pulled with `resume_row_into`.
    Positioned(RowPauseState),
    /// A non-row token was received instead (e.g. the terminating DONE), which
    /// the caller handles as a result-set boundary.
    Token(Tokens),
}

#[cfg(fuzzing)]
pub enum RowHeader {
    Positioned(RowPauseState),
    Token(Tokens),
}

/// Carry-over state when [`RowWriter::pause_after_column`] returns `true`.
///
/// Passed back to [`TdsTokenStreamReader::resume_row_into`] to continue
/// decoding the rest of the row from where it paused.
#[derive(Debug)]
#[cfg(not(fuzzing))]
pub(crate) struct RowPauseState {
    /// Index of the first column that has not yet been decoded.
    pub(crate) next_column_index: usize,
    /// Column metadata for the row, shared with the ParserContext.
    pub(crate) metadata: Arc<ColMetadataToken>,
    /// NBCROW null-bitmap (one bit per column, LSB-first).  `None` for plain ROW.
    pub(crate) nbc_null_bitmap: Option<Arc<[u8]>>,
    /// Optional AE decryptor needed to continue decrypting encrypted columns
    /// after a row pause/resume boundary.
    pub(crate) decryptor: Option<Arc<dyn CellDecryptor>>,
}

#[derive(Debug)]
#[cfg(fuzzing)]
#[allow(private_interfaces)]
pub struct RowPauseState {
    pub next_column_index: usize,
    pub metadata: Arc<ColMetadataToken>,
    pub nbc_null_bitmap: Option<Arc<[u8]>>,
    pub decryptor: Option<Arc<dyn CellDecryptor>>,
}

impl RowPauseState {
    /// Borrows just the column layout so callers outside this module don't have
    /// to reach through the shared token and its CEK table.
    pub(crate) fn columns(&self) -> &[ColumnMetadata] {
        &self.metadata.columns
    }
}

/// Active PLP stream state captured when row decoding is paused at a PLP column.
#[derive(Debug)]
#[cfg(not(fuzzing))]
pub(crate) struct PlpPauseState {
    pub(crate) row_pause_state: RowPauseState,
    pub(crate) plp_stream: PlpColumnStream,
}

#[derive(Debug)]
#[cfg(fuzzing)]
pub struct PlpPauseState {
    pub row_pause_state: RowPauseState,
    pub plp_stream: PlpColumnStream,
}

impl PlpPauseState {
    pub(crate) fn reached_end(&self) -> bool {
        self.plp_stream.reached_end()
    }

    /// Declared total length of the whole PLP value in wire bytes when the
    /// server sent a known-length header; `None` for unknown-length PLP.
    pub(crate) fn known_len(&self) -> Option<u64> {
        self.plp_stream.known_len()
    }

    /// Cumulative wire bytes consumed from this PLP value across all chunks.
    pub(crate) fn total_read(&self) -> usize {
        self.plp_stream.total_read()
    }

    pub(crate) fn collation(&self) -> Option<crate::token::tokens::SqlCollation> {
        self.plp_stream.collation()
    }
}

#[async_trait]
#[cfg(not(fuzzing))]
pub(crate) trait TdsTokenStreamReader {
    async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens>;

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    /// Reads only the next row *header* — the ROW/NBCROW token byte and any
    /// NBCROW null bitmap — pausing before column 0 without decoding columns.
    /// Non-row tokens are returned as [`RowHeader::Token`]. Used by the pull
    /// cursor to position on a row (`SQLFetch`) before pulling columns.
    async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader>;

    /// Resume a paused row decode from `pause_state.next_column_index`, applying
    /// `plan` to the remaining columns.
    ///
    /// The caller is responsible for passing back the exact [`RowPauseState`]
    /// that was returned inside `RowReadResult::RowPaused`.
    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    /// Reads bytes from an active PLP stream captured by
    /// [`RowReadResult::PlpPaused`].
    async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize>;
}

#[async_trait]
#[cfg(fuzzing)]
pub trait TdsTokenStreamReader {
    async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens>;

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader>;

    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize>;
}

#[cfg(fuzzing)]
pub struct TokenStreamReader<T, R>
where
    T: TdsPacketReader + Send + Sync,
    R: TokenParserRegistry + Send + Sync,
{
    pub packet_reader: T,
    pub parser_registry: Box<R>,
    pub nbc_bitmap_scratch: Option<Arc<[u8]>>,
}

/// Column metadata plus the optional cell decryptor needed to decode a row.
///
/// Returned by [`extract_row_context`] so the ROW/NBCROW decode paths can both
/// access the column layout and the Always Encrypted decryptor (if any).
type RowDecodeContext<'a> = (
    &'a Arc<ColMetadataToken>,
    Option<&'a Arc<dyn CellDecryptor>>,
);

/// `ParserContext` is used to add additional context, which can be leveraged by the token parsers.
/// One of the usecase is passing the metadata for the columns, to the row parser and to the
/// NBC row token parser.
/// The consumer of the TokenStreamReader is supposed to set/reset this context.
/// Incorrectly managing this context, can lead to bad context being used for subsequent operations.
#[derive(Debug)]
#[cfg(not(fuzzing))]
pub(crate) enum ParserContext {
    /// Column metadata for the current result set, paired with an optional
    /// [`CellDecryptor`] used to decrypt Always Encrypted columns while decoding
    /// rows. The decryptor is `None` when the result set has no encrypted
    /// columns or column encryption is not enabled.
    ColumnMetadata(Arc<ColMetadataToken>, Option<Arc<dyn CellDecryptor>>),
    /// Carries whether Always Encrypted (TCE) was negotiated for the connection.
    /// Consumed by the COLMETADATA parser to decide whether to parse the CEK
    /// table and per-column crypto metadata.
    ColumnEncryption(bool),
    None(()),
}

#[derive(Debug)]
#[cfg(fuzzing)]
#[allow(private_interfaces)]
pub enum ParserContext {
    ColumnMetadata(Arc<ColMetadataToken>, Option<Arc<dyn CellDecryptor>>),
    /// Carries whether Always Encrypted (TCE) was negotiated for the connection.
    /// Consumed by the COLMETADATA parser to decide whether to parse the CEK
    /// table and per-column crypto metadata.
    ColumnEncryption(bool),
    None(()),
}

impl Default for ParserContext {
    fn default() -> Self {
        ParserContext::None(())
    }
}

impl ParserContext {
    /// Returns `true` when this context indicates Always Encrypted was
    /// negotiated, instructing the COLMETADATA parser to parse encryption
    /// metadata.
    pub(crate) fn is_column_encryption_supported(&self) -> bool {
        matches!(self, ParserContext::ColumnEncryption(true))
    }
}

fn extract_row_context(context: &ParserContext) -> TdsResult<RowDecodeContext<'_>> {
    match context {
        ParserContext::ColumnMetadata(metadata, decryptor) => Ok((metadata, decryptor.as_ref())),
        _ => Err(crate::error::Error::ProtocolError(
            "Expected ColumnMetadata in context for row decoding".to_string(),
        )),
    }
}

pub(crate) async fn dispatch_token<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    registry: &impl TokenParserRegistry,
    token_type: TokenType,
    context: &ParserContext,
) -> TdsResult<Tokens> {
    let parser = match registry.get_parser(&token_type) {
        Some(parser) => parser,
        None => {
            return Err(crate::error::Error::ProtocolError(format!(
                "No parser implemented for token type: {token_type:?}. This token type is not supported yet."
            )));
        }
    };

    debug!("Parsing token type: {:?}", &token_type);

    match parser {
        TokenParsers::EnvChange(parser) => parser.parse(reader, context).await,
        TokenParsers::LoginAck(parser) => parser.parse(reader, context).await,
        TokenParsers::Done(parser) => parser.parse(reader, context).await,
        TokenParsers::DoneInProc(parser) => parser.parse(reader, context).await,
        TokenParsers::DoneProc(parser) => parser.parse(reader, context).await,
        TokenParsers::Info(parser) => parser.parse(reader, context).await,
        TokenParsers::Error(parser) => parser.parse(reader, context).await,
        TokenParsers::FedAuthInfo(parser) => parser.parse(reader, context).await,
        TokenParsers::FeatureExtAck(parser) => parser.parse(reader, context).await,
        TokenParsers::ColMetadata(parser) => parser.parse(reader, context).await,
        TokenParsers::Row(parser) => parser.parse(reader, context).await,
        TokenParsers::Order(parser) => parser.parse(reader, context).await,
        TokenParsers::ReturnStatus(parser) => parser.parse(reader, context).await,
        TokenParsers::NbcRow(parser) => parser.parse(reader, context).await,
        TokenParsers::ReturnValue(parser) => parser.parse(reader, context).await,
        TokenParsers::SessionState(parser) => parser.parse(reader, context).await,
        TokenParsers::TabName(parser) => parser.parse(reader, context).await,
        TokenParsers::ColInfo(parser) => parser.parse(reader, context).await,
        TokenParsers::Sspi(parser) => parser.parse(reader, context).await,
    }
}

pub(crate) async fn receive_token_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    registry: &impl TokenParserRegistry,
    context: &ParserContext,
) -> TdsResult<Tokens> {
    let token_type_byte = reader.read_byte().await?;
    let token_type: TokenType = token_type_byte.try_into()?;
    debug!(
        "Received token type: {:?} ({})",
        token_type, token_type_byte
    );
    dispatch_token(reader, registry, token_type, context).await
}

/// Reads and discards one column's wire bytes without materializing a value.
///
/// PLP columns use the alloc-free chunk skip; fixed/short columns decode into a
/// [`DiscardRowWriter`], which drops any transient value instead of retaining it
/// in a row `Vec`. Encrypted columns are skipped as their raw ciphertext
/// varbinary — no decryption is performed for a skipped column.
async fn skip_column<R: TdsPacketReader + Send + Sync>(
    decoder: &GenericDecoder,
    reader: &mut R,
    meta: &ColumnMetadata,
    col: usize,
) -> TdsResult<()> {
    if meta.is_plp() {
        if let Some(mut stream) = PlpColumnStream::begin(meta, reader).await? {
            stream.skip_to_end(reader).await?;
        }
        Ok(())
    } else {
        // TODO(#47154): This materializes and heap-allocates the skipped column's
        // value (SqlString/Vec/...) only to discard it. For column-wise
        // SQLGetData that skips leading columns, this is wasteful — especially
        // when several columns are skipped per row. Skip the bytes at the
        // PacketReader level instead (advance past the column's length without
        // decoding). Larger change tracked as work item 47154.
        let mut sink = DiscardRowWriter;
        decoder.decode_into(reader, meta, col, &mut sink).await
    }
}

/// Builds the pause result after column `col` was decoded/skipped: either
/// [`RowReadResult::RowPaused`] positioned on `col + 1`, or
/// [`RowReadResult::RowWritten`] if `col` was the last column.
fn pause_after_column(
    col: usize,
    metadata: &Arc<ColMetadataToken>,
    bitmap: Option<&Arc<[u8]>>,
    decryptor: Option<&Arc<dyn CellDecryptor>>,
) -> RowReadResult {
    if col + 1 < metadata.columns.len() {
        RowReadResult::RowPaused(RowPauseState {
            next_column_index: col + 1,
            metadata: Arc::clone(metadata),
            nbc_null_bitmap: bitmap.cloned(),
            decryptor: decryptor.cloned(),
        })
    } else {
        RowReadResult::RowWritten
    }
}

/// Unified per-column decode driver for ROW and NBCROW tokens.
///
/// `bitmap` is `Some` for NBCROW (LSB-first null bits), `None` for plain ROW.
/// `plan` decides, per column, whether to decode it into `writer`, skip its
/// bytes, or pause. This single loop replaces the former
/// `decode_row_columns` / `decode_nbcrow_columns` pair and their
/// `writer.pause_*` polling.
async fn drive_row_columns<R: TdsPacketReader + Send + Sync, W: RowWriter + Send + ?Sized>(
    reader: &mut R,
    metadata: &Arc<ColMetadataToken>,
    decryptor: Option<&Arc<dyn CellDecryptor>>,
    bitmap: Option<&Arc<[u8]>>,
    start_col: usize,
    plan: ColumnPolicy,
    writer: &mut W,
) -> TdsResult<RowReadResult> {
    let decoder = GenericDecoder::default();
    let columns = &metadata.columns;
    for (col, meta) in columns.iter().enumerate().skip(start_col) {
        let stop_here = matches!(plan, ColumnPolicy::DecodeOne(target) if target == col);
        let skip = match plan {
            ColumnPolicy::SkipAll => true,
            ColumnPolicy::DecodeOne(target) => col < target,
            ColumnPolicy::DecodeAll => false,
        };

        let is_null = bitmap.is_some_and(|bm| bm[col / 8] & (1 << (col % 8)) != 0);

        if is_null {
            // NBCROW null column: no payload bytes on the wire.
            if !skip {
                writer.write_null(col);
            }
            if stop_here {
                return Ok(pause_after_column(col, metadata, bitmap, decryptor));
            }
            continue;
        }

        if skip {
            skip_column(&decoder, reader, meta, col).await?;
            continue;
        }

        // At the cursor's target PLP column, pause before payload so the caller
        // can stream chunks via `read_active_plp_bytes`.
        if stop_here && meta.is_plp() {
            // TODO: Add AE-aware PLP streaming path for paused row reads.
            // Until then, fail fast to avoid streaming ciphertext bytes to callers.
            if meta.crypto_metadata.is_some() {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "Always Encrypted paused PLP streaming".to_string(),
                    context: format!(
                        "Encrypted PLP column '{}' cannot be streamed via read_active_plp_bytes yet.",
                        meta.column_name
                    ),
                });
            }
            match PlpColumnStream::begin(meta, reader).await? {
                None => {
                    writer.write_null(col);
                    return Ok(pause_after_column(col, metadata, bitmap, decryptor));
                }
                Some(plp_stream) => {
                    // `pause_after_column` reports `RowWritten` when `col` is the
                    // last column, so construct the pause state directly instead
                    // of destructuring its `RowPaused`. Either way the row is
                    // complete once this trailing PLP payload is drained;
                    // `next_column_index == columns.len()` (when `col` is last)
                    // makes that later drain a no-op.
                    return Ok(RowReadResult::PlpPaused(PlpPauseState {
                        row_pause_state: RowPauseState {
                            next_column_index: col + 1,
                            metadata: Arc::clone(metadata),
                            nbc_null_bitmap: bitmap.cloned(),
                            decryptor: decryptor.cloned(),
                        },
                        plp_stream,
                    }));
                }
            }
        }

        decode_or_decrypt_column(&decoder, reader, meta, decryptor, col, writer).await?;

        if stop_here {
            return Ok(pause_after_column(col, metadata, bitmap, decryptor));
        }
    }
    Ok(RowReadResult::RowWritten)
}

async fn decode_or_decrypt_column<
    R: TdsPacketReader + Send + Sync,
    W: RowWriter + Send + ?Sized,
>(
    decoder: &GenericDecoder,
    reader: &mut R,
    meta: &ColumnMetadata,
    decryptor: Option<&Arc<dyn CellDecryptor>>,
    col: usize,
    writer: &mut W,
) -> TdsResult<()> {
    match (meta.crypto_metadata.is_some(), decryptor) {
        (true, Some(dec)) => {
            let value = decrypt_encrypted_column(decoder, reader, meta, dec).await?;
            write_column_value(writer, col, value);
        }
        (true, None) => {
            tracing::debug!(
                column = %meta.column_name,
                "Encrypted column has no column-encryption decryptor available \
                 (Always Encrypted disabled for this command, or no key-store \
                 provider registered); returning the raw ciphertext varbinary"
            );
            decoder.decode_into(reader, meta, col, writer).await?;
        }
        (false, _) => {
            decoder.decode_into(reader, meta, col, writer).await?;
        }
    }
    Ok(())
}

/// Allocates a zeroed null bitmap in a single allocation.
///
/// `Arc<[u8]>` stores its refcounts inline with the data, so `Arc::from(Vec<u8>)`
/// cannot reuse the `Vec`'s buffer — it allocates again and memcpies. Collecting
/// from a `TrustedLen` iterator writes straight into the `Arc`'s allocation.
fn zeroed_bitmap(bitmap_len: usize) -> Arc<[u8]> {
    std::iter::repeat_n(0u8, bitmap_len).collect()
}

/// Fills `buffer` with exactly `buffer.len()` bitmap bytes.
///
/// [`TdsPacketReader::read_bytes`] reports a count and does not contract a full
/// fill. That matters more here than elsewhere: because the buffer is reused, a
/// short read would leave the *previous* row's bits in the tail and silently
/// yield wrong NULL flags for this row, with the wrongness depending on whatever
/// row came before. A short bitmap read is a protocol violation, so treat it as
/// one rather than decoding from half-stale state.
async fn fill_bitmap<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    buffer: &mut [u8],
) -> TdsResult<()> {
    let expected = buffer.len();
    let read = reader.read_bytes(buffer).await?;
    if read != expected {
        return Err(crate::error::Error::ProtocolError(format!(
            "NBCROW null bitmap truncated: expected {expected} bytes, read {read}"
        )));
    }
    Ok(())
}

/// Reads an NBCROW null bitmap, refilling `scratch`'s allocation in place when
/// nothing else still holds it.
///
/// `bitmap_len` is constant for a result set and paused rows are drained before
/// the next row header is read, so after the first NBCROW row of a result set
/// this allocates nothing.
///
/// The [`Arc::get_mut`] check is load-bearing for correctness, not just an
/// optimization guard: a consumer holding a [`RowPauseState`] past end-of-row
/// keeps the cached bitmap alive, and refilling it would corrupt a row that is
/// still being read. Falling through to a fresh allocation is the only safe
/// response, so the reuse and allocate paths are kept separate — each takes the
/// single [`Arc::get_mut`] it needs, and the one that must not fail is the one
/// operating on an `Arc` allocated a line earlier.
async fn read_nbc_bitmap<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    bitmap_len: usize,
    scratch: &mut Option<Arc<[u8]>>,
) -> TdsResult<Arc<[u8]>> {
    if let Some(mut cached) = scratch.take()
        && cached.len() == bitmap_len
        && let Some(buffer) = Arc::get_mut(&mut cached)
    {
        fill_bitmap(reader, buffer).await?;
        *scratch = Some(Arc::clone(&cached));
        return Ok(cached);
    }

    let mut bitmap = zeroed_bitmap(bitmap_len);
    let buffer = Arc::get_mut(&mut bitmap).expect("a freshly allocated Arc is unique");
    fill_bitmap(reader, buffer).await?;

    *scratch = Some(Arc::clone(&bitmap));
    Ok(bitmap)
}

pub(crate) async fn receive_row_into_internal<
    R: TdsPacketReader + Send + Sync,
    W: RowWriter + Send + ?Sized,
>(
    reader: &mut R,
    registry: &impl TokenParserRegistry,
    context: &ParserContext,
    plan: ColumnPolicy,
    writer: &mut W,
    nbc_bitmap_scratch: &mut Option<Arc<[u8]>>,
) -> TdsResult<RowReadResult> {
    let token_type_byte = reader.read_byte().await?;
    let token_type: TokenType = token_type_byte.try_into()?;
    debug!("Parsing token type: {:?}", &token_type);

    match token_type {
        TokenType::Row => {
            let (metadata, decryptor) = extract_row_context(context)?;
            drive_row_columns(reader, metadata, decryptor, None, 0, plan, writer).await
        }
        TokenType::NbcRow => {
            let (metadata, decryptor) = extract_row_context(context)?;
            let bitmap_len = metadata.columns.len().div_ceil(8);
            let bitmap = read_nbc_bitmap(reader, bitmap_len, nbc_bitmap_scratch).await?;
            drive_row_columns(reader, metadata, decryptor, Some(&bitmap), 0, plan, writer).await
        }
        _ => {
            let token = dispatch_token(reader, registry, token_type, context).await?;
            Ok(RowReadResult::Token(token))
        }
    }
}

/// Reads only the next row *header* — the ROW/NBCROW token byte plus any NBCROW
/// null bitmap — and pauses before column 0 without decoding any column. A
/// non-row token is returned as [`RowHeader::Token`]. This is the row-level
/// split-out of `receive_row_into` used by the pull cursor to position on a row.
pub(crate) async fn receive_row_header_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    registry: &impl TokenParserRegistry,
    context: &ParserContext,
    nbc_bitmap_scratch: &mut Option<Arc<[u8]>>,
) -> TdsResult<RowHeader> {
    let token_type_byte = reader.read_byte().await?;
    let token_type: TokenType = token_type_byte.try_into()?;
    debug!("Parsing row header token type: {:?}", &token_type);

    match token_type {
        TokenType::Row => {
            let (metadata, decryptor) = extract_row_context(context)?;
            Ok(RowHeader::Positioned(RowPauseState {
                next_column_index: 0,
                metadata: Arc::clone(metadata),
                nbc_null_bitmap: None,
                decryptor: decryptor.cloned(),
            }))
        }
        TokenType::NbcRow => {
            let (metadata, decryptor) = extract_row_context(context)?;
            let bitmap_len = metadata.columns.len().div_ceil(8);
            let bitmap = read_nbc_bitmap(reader, bitmap_len, nbc_bitmap_scratch).await?;
            Ok(RowHeader::Positioned(RowPauseState {
                next_column_index: 0,
                metadata: Arc::clone(metadata),
                nbc_null_bitmap: Some(bitmap),
                decryptor: decryptor.cloned(),
            }))
        }
        _ => {
            let token = dispatch_token(reader, registry, token_type, context).await?;
            Ok(RowHeader::Token(token))
        }
    }
}

/// Resumes a paused row decode from `pause_state.next_column_index`, applying
/// `plan` to the remaining columns.
///
/// Does not read a token-type byte — the token has already been consumed.
pub(crate) async fn resume_row_into_internal<
    R: TdsPacketReader + Send + Sync,
    W: RowWriter + Send + ?Sized,
>(
    reader: &mut R,
    pause_state: RowPauseState,
    plan: ColumnPolicy,
    writer: &mut W,
) -> TdsResult<RowReadResult> {
    let RowPauseState {
        next_column_index,
        metadata,
        nbc_null_bitmap,
        decryptor,
    } = pause_state;

    drive_row_columns(
        reader,
        &metadata,
        decryptor.as_ref(),
        nbc_null_bitmap.as_ref(),
        next_column_index,
        plan,
        writer,
    )
    .await
}

pub(crate) async fn read_active_plp_bytes_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    plp_state: &mut PlpPauseState,
    out: &mut [u8],
) -> TdsResult<usize> {
    plp_state.plp_stream.read_into(reader, out).await
}

#[cfg(fuzzing)]
impl<T, R> TokenStreamReader<T, R>
where
    T: TdsPacketReader + Send + Sync,
    R: TokenParserRegistry + Send + Sync,
{
    pub fn new(packet_reader: T, parser_registry: Box<R>) -> TokenStreamReader<T, R> {
        TokenStreamReader {
            packet_reader,
            parser_registry,
            nbc_bitmap_scratch: None,
        }
    }

    async fn cancel_read_stream_and_wait(&mut self) -> TdsResult<()> {
        self.packet_reader.cancel_read_stream().await?;
        let dummy_context = ParserContext::None(());
        while let Ok(token) = receive_token_internal(
            &mut self.packet_reader,
            &*self.parser_registry,
            &dummy_context,
        )
        .await
        {
            if let Tokens::Done(done_token) = token
                && done_token.status.contains(DoneStatus::ATTN)
            {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(fuzzing)]
#[async_trait]
impl<T, R> TdsTokenStreamReader for TokenStreamReader<T, R>
where
    T: TdsPacketReader + Send + Sync,
    R: TokenParserRegistry + Send + Sync,
{
    async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens> {
        let cancellable_receive_token = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_token_internal(&mut self.packet_reader, &*self.parser_registry, context),
        );
        let token_result = match remaining_request_timeout.as_ref() {
            Some(remaining_request_timeout) => {
                match timeout(*remaining_request_timeout, cancellable_receive_token).await {
                    Ok(result) => result,
                    Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
                }
            }
            None => cancellable_receive_token.await,
        };

        match &token_result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        token_result
    }

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_row_into_internal(
                &mut self.packet_reader,
                &*self.parser_registry,
                context,
                plan,
                writer,
                &mut self.nbc_bitmap_scratch,
            ),
        );
        let result = match remaining_request_timeout.as_ref() {
            Some(t) => match timeout(*t, cancellable).await {
                Ok(r) => r,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => cancellable.await,
        };

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        result
    }

    async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_row_header_internal(
                &mut self.packet_reader,
                &*self.parser_registry,
                context,
                &mut self.nbc_bitmap_scratch,
            ),
        );
        let result = match remaining_request_timeout.as_ref() {
            Some(t) => match timeout(*t, cancellable).await {
                Ok(r) => r,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => cancellable.await,
        };

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        result
    }

    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            resume_row_into_internal(&mut self.packet_reader, pause_state, plan, writer),
        );
        let result = match remaining_request_timeout.as_ref() {
            Some(t) => match timeout(*t, cancellable).await {
                Ok(r) => r,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => cancellable.await,
        };

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        result
    }

    async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            read_active_plp_bytes_internal(&mut self.packet_reader, plp_state, out),
        );
        let result = match remaining_request_timeout.as_ref() {
            Some(t) => match timeout(*t, cancellable).await {
                Ok(r) => r,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => cancellable.await,
        };

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        result
    }
}
#[cfg(not(fuzzing))]
pub(crate) trait TokenParserRegistry: Send + Sync {
    fn get_parser(&self, token_type: &TokenType) -> Option<&TokenParsers>;
}

#[cfg(fuzzing)]
pub trait TokenParserRegistry: Send + Sync {
    fn get_parser(&self, token_type: &TokenType) -> Option<&TokenParsers>;
}

#[cfg(not(fuzzing))]
pub(crate) struct GenericTokenParserRegistry {
    parsers: HashMap<TokenType, TokenParsers>,
}

#[cfg(fuzzing)]
pub struct GenericTokenParserRegistry {
    parsers: HashMap<TokenType, TokenParsers>,
}

impl Default for GenericTokenParserRegistry {
    fn default() -> Self {
        let mut internal_registry: HashMap<TokenType, TokenParsers> = HashMap::new();
        internal_registry.insert(
            TokenType::EnvChange,
            TokenParsers::from(EnvChangeTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::LoginAck,
            TokenParsers::from(LoginAckTokenParser::default()),
        );
        internal_registry.insert(TokenType::Done, TokenParsers::from(DoneTokenParser {}));
        internal_registry.insert(
            TokenType::DoneInProc,
            TokenParsers::from(DoneInProcTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::DoneProc,
            TokenParsers::from(DoneProcTokenParser::default()),
        );
        internal_registry.insert(TokenType::Info, TokenParsers::from(InfoTokenParser {}));
        internal_registry.insert(TokenType::Error, TokenParsers::from(ErrorTokenParser {}));
        internal_registry.insert(
            TokenType::FeatureExtAck,
            TokenParsers::from(FeatureExtAckTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::FedAuthInfo,
            TokenParsers::from(FedAuthInfoTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::ColMetadata,
            TokenParsers::from(ColMetadataTokenParser),
        );
        internal_registry.insert(
            TokenType::Row,
            TokenParsers::from(RowTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::Order,
            TokenParsers::from(OrderTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::ReturnStatus,
            TokenParsers::from(ReturnStatusTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::NbcRow,
            TokenParsers::from(NbcRowTokenParser::default()),
        );
        internal_registry.insert(
            TokenType::ReturnValue,
            TokenParsers::from(ReturnValueTokenParser::default()),
        );
        internal_registry.insert(TokenType::SSPI, TokenParsers::from(SspiTokenParser));
        internal_registry.insert(
            TokenType::SessionState,
            TokenParsers::from(SessionStateTokenParser),
        );
        internal_registry.insert(TokenType::TabName, TokenParsers::from(TabNameTokenParser));
        internal_registry.insert(TokenType::ColInfo, TokenParsers::from(ColInfoTokenParser));
        Self {
            parsers: internal_registry,
        }
    }
}

impl TokenParserRegistry for GenericTokenParserRegistry {
    fn get_parser(&self, token_type: &TokenType) -> Option<&TokenParsers> {
        self.parsers.get(token_type)
    }
}

#[allow(private_interfaces)]
pub enum TokenParsers {
    EnvChange(EnvChangeTokenParser),
    LoginAck(LoginAckTokenParser),
    Done(DoneTokenParser),
    DoneInProc(DoneInProcTokenParser),
    DoneProc(DoneProcTokenParser),
    Info(InfoTokenParser),
    Error(ErrorTokenParser),
    FedAuthInfo(FedAuthInfoTokenParser),
    FeatureExtAck(FeatureExtAckTokenParser),
    ColMetadata(ColMetadataTokenParser),
    Row(RowTokenParser<GenericDecoder>),
    Order(OrderTokenParser),
    ReturnStatus(ReturnStatusTokenParser),
    NbcRow(NbcRowTokenParser<GenericDecoder>),
    ReturnValue(ReturnValueTokenParser<GenericDecoder>),
    SessionState(SessionStateTokenParser),
    TabName(TabNameTokenParser),
    ColInfo(ColInfoTokenParser),
    Sspi(SspiTokenParser),
}

macro_rules! impl_from_token_parser {
    ($($parser:ty => $variant:ident),*) => {
        $(
            impl From<$parser> for TokenParsers {
                fn from(parser: $parser) -> Self {
                    TokenParsers::$variant(parser)
                }
            }
        )*
    };
}

impl_from_token_parser!(
    EnvChangeTokenParser => EnvChange,
    LoginAckTokenParser => LoginAck,
    DoneTokenParser => Done,
    DoneInProcTokenParser => DoneInProc,
    DoneProcTokenParser => DoneProc,
    InfoTokenParser => Info,
    ErrorTokenParser => Error,
    FedAuthInfoTokenParser => FedAuthInfo,
    FeatureExtAckTokenParser => FeatureExtAck,
    ColMetadataTokenParser => ColMetadata,
    RowTokenParser<GenericDecoder> => Row,
    OrderTokenParser => Order,
    ReturnStatusTokenParser => ReturnStatus,
    NbcRowTokenParser<GenericDecoder> => NbcRow,
    ReturnValueTokenParser<GenericDecoder> => ReturnValue,
    SessionStateTokenParser => SessionState,
    TabNameTokenParser => TabName,
    ColInfoTokenParser => ColInfo,
    SspiTokenParser => Sspi
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
    use crate::io::packet_reader::TdsPacketReader;
    use crate::token::tokens::{SqlCollation, TokenType};

    use std::collections::HashMap;
    use std::sync::Arc;

    /// Companion to `row_fetch_futures_stay_small` (#225) for the decode chain below
    /// `Box<dyn TdsTransport>`. That guard measures futures built on `TdsClient`, which
    /// re-boxes at the transport boundary, so it cannot observe anything in this file:
    /// its four futures are byte-identical before and after this chain roughly doubled.
    #[test]
    fn row_decode_futures_stay_small() {
        const MAX: usize = 4096;

        let metadata = Arc::new(ColMetadataToken {
            column_count: 0,
            columns: vec![],
            cek_table: vec![],
        });
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);
        let registry = GenericTokenParserRegistry::default();
        let mut reader = TestByteReader::new(vec![TokenType::Row as u8]);
        let mut sink = DiscardRowWriter;
        let mut nbc_bitmap_scratch = None;

        // Constructing an async fn's future runs none of its body, so these are free to
        // build and drop unpolled. Each borrow ends with its statement.
        //
        // Both instantiations are measured: `dyn` is what production reaches today, and
        // the monomorphic one is what a concrete writer gets once the transport boundary
        // stops erasing it (#265).
        let receive_dyn = size_of_val(&receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeAll,
            &mut sink as &mut (dyn RowWriter + Send),
            &mut nbc_bitmap_scratch,
        ));
        let receive_mono = size_of_val(&receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeAll,
            &mut sink,
            &mut nbc_bitmap_scratch,
        ));
        let drive_dyn = size_of_val(&drive_row_columns(
            &mut reader,
            &metadata,
            None,
            None,
            0,
            ColumnPolicy::DecodeAll,
            &mut sink as &mut (dyn RowWriter + Send),
        ));
        let drive_mono = size_of_val(&drive_row_columns(
            &mut reader,
            &metadata,
            None,
            None,
            0,
            ColumnPolicy::DecodeAll,
            &mut sink,
        ));
        let resume_dyn = size_of_val(&resume_row_into_internal(
            &mut reader,
            RowPauseState {
                next_column_index: 0,
                metadata: Arc::clone(&metadata),
                nbc_null_bitmap: None,
                decryptor: None,
            },
            ColumnPolicy::DecodeAll,
            &mut sink as &mut (dyn RowWriter + Send),
        ));

        for (name, size) in [
            ("receive_row_into_internal (dyn)", receive_dyn),
            ("receive_row_into_internal (mono)", receive_mono),
            ("drive_row_columns (dyn)", drive_dyn),
            ("drive_row_columns (mono)", drive_mono),
            ("resume_row_into_internal (dyn)", resume_dyn),
        ] {
            assert!(
                size <= MAX,
                "{name} future is {size} B, expected <= {MAX} B"
            );
        }
    }

    #[test]
    fn test_parser_context_default() {
        let context = ParserContext::default();
        match context {
            ParserContext::None(_) => {}
            _ => panic!("Default ParserContext should be None variant"),
        }
    }

    #[test]
    fn test_generic_token_parser_registry_has_all_parsers() {
        let registry = GenericTokenParserRegistry::default();

        assert!(registry.get_parser(&TokenType::EnvChange).is_some());
        assert!(registry.get_parser(&TokenType::LoginAck).is_some());
        assert!(registry.get_parser(&TokenType::Done).is_some());
        assert!(registry.get_parser(&TokenType::DoneInProc).is_some());
        assert!(registry.get_parser(&TokenType::DoneProc).is_some());
        assert!(registry.get_parser(&TokenType::Info).is_some());
        assert!(registry.get_parser(&TokenType::Error).is_some());
        assert!(registry.get_parser(&TokenType::FeatureExtAck).is_some());
        assert!(registry.get_parser(&TokenType::FedAuthInfo).is_some());
        assert!(registry.get_parser(&TokenType::ColMetadata).is_some());
        assert!(registry.get_parser(&TokenType::Row).is_some());
        assert!(registry.get_parser(&TokenType::Order).is_some());
        assert!(registry.get_parser(&TokenType::ReturnStatus).is_some());
        assert!(registry.get_parser(&TokenType::NbcRow).is_some());
        assert!(registry.get_parser(&TokenType::ReturnValue).is_some());
        assert!(registry.get_parser(&TokenType::SessionState).is_some());
        assert!(registry.get_parser(&TokenType::TabName).is_some());
        assert!(registry.get_parser(&TokenType::ColInfo).is_some());
    }

    #[test]
    fn test_generic_token_parser_registry_get_parser() {
        let registry = GenericTokenParserRegistry::default();

        // Test that we can get parsers for supported token types
        assert!(registry.get_parser(&TokenType::EnvChange).is_some());
        assert!(registry.get_parser(&TokenType::Done).is_some());
        assert!(registry.get_parser(&TokenType::Info).is_some());
    }

    #[test]
    fn test_generic_token_parser_registry_unsupported_token() {
        let registry = GenericTokenParserRegistry::default();

        // Test with an unsupported token type (using a type that's not registered)
        // This tests the negative case
        let unsupported_type = TokenType::AltMetadata; // This token type is not registered in the default registry
        assert!(registry.get_parser(&unsupported_type).is_none());
    }

    #[test]
    fn test_token_parsers_from_conversions() {
        // Test that all From implementations work correctly
        let env_change_parser = EnvChangeTokenParser::default();
        let _: TokenParsers = env_change_parser.into();

        let login_ack_parser = LoginAckTokenParser::default();
        let _: TokenParsers = login_ack_parser.into();

        let done_parser = DoneTokenParser {};
        let _: TokenParsers = done_parser.into();

        let done_in_proc_parser = DoneInProcTokenParser::default();
        let _: TokenParsers = done_in_proc_parser.into();

        let done_proc_parser = DoneProcTokenParser::default();
        let _: TokenParsers = done_proc_parser.into();

        let info_parser = InfoTokenParser {};
        let _: TokenParsers = info_parser.into();

        let error_parser = ErrorTokenParser {};
        let _: TokenParsers = error_parser.into();
    }

    #[test]
    fn test_parser_context_variants() {
        // Test None variant
        let context_none = ParserContext::None(());
        match context_none {
            ParserContext::None(_) => {}
            _ => panic!("Expected ParserContext::None"),
        }

        // Test ColumnMetadata variant (would need actual ColMetadataToken to construct)
        // This tests that the variant exists and can be pattern matched
    }

    struct TestByteReader {
        data: Vec<u8>,
        pos: usize,
        /// When set, the next `read_bytes` fills only this many bytes and
        /// reports that count, modelling a reader that does not fully fill.
        short_read: Option<usize>,
    }

    impl TestByteReader {
        fn new(data: Vec<u8>) -> Self {
            Self {
                data,
                pos: 0,
                short_read: None,
            }
        }

        fn with_short_read(mut self, filled: usize) -> Self {
            self.short_read = Some(filled);
            self
        }

        fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
            if self.pos + n > self.data.len() {
                return Err(crate::error::Error::ProtocolError(
                    "unexpected end of test buffer".to_string(),
                ));
            }
            let slice = &self.data[self.pos..self.pos + n];
            self.pos += n;
            Ok(slice)
        }
    }

    impl TdsPacketReader for TestByteReader {
        async fn read_byte(&mut self) -> TdsResult<u8> {
            Ok(self.take(1)?[0])
        }

        async fn read_int16(&mut self) -> TdsResult<i16> {
            unimplemented!("unused in test")
        }

        async fn read_uint16(&mut self) -> TdsResult<u16> {
            unimplemented!("unused in test")
        }

        async fn read_int32(&mut self) -> TdsResult<i32> {
            let raw = self.take(4)?;
            Ok(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        }

        async fn read_uint32(&mut self) -> TdsResult<u32> {
            let raw = self.take(4)?;
            Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        }

        async fn read_int64(&mut self) -> TdsResult<i64> {
            let raw = self.take(8)?;
            Ok(i64::from_le_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]))
        }

        async fn read_uint64(&mut self) -> TdsResult<u64> {
            unimplemented!("unused in test")
        }

        async fn read_float32(&mut self) -> TdsResult<f32> {
            unimplemented!("unused in test")
        }

        async fn read_float64(&mut self) -> TdsResult<f64> {
            unimplemented!("unused in test")
        }

        async fn read_uint24(&mut self) -> TdsResult<u32> {
            unimplemented!("unused in test")
        }

        async fn read_uint40(&mut self) -> TdsResult<u64> {
            unimplemented!("unused in test")
        }

        async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
            if let Some(filled) = self.short_read.take() {
                let raw = self.take(filled)?;
                buffer[..filled].copy_from_slice(raw);
                return Ok(filled);
            }
            let raw = self.take(buffer.len())?;
            buffer.copy_from_slice(raw);
            Ok(buffer.len())
        }

        async fn skip_bytes(&mut self, count: usize) -> TdsResult<()> {
            self.take(count)?;
            Ok(())
        }

        async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
            unimplemented!("unused in test")
        }

        async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
            unimplemented!("unused in test")
        }

        async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
            unimplemented!("unused in test")
        }

        async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
            unimplemented!("unused in test")
        }

        async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            unimplemented!("unused in test")
        }

        async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            unimplemented!("unused in test")
        }

        async fn read_unicode(&mut self, _len: usize) -> TdsResult<String> {
            unimplemented!("unused in test")
        }

        async fn read_unicode_with_byte_length(&mut self, _len: usize) -> TdsResult<String> {
            unimplemented!("unused in test")
        }

        async fn cancel_read_stream(&mut self) -> TdsResult<()> {
            unimplemented!("unused in test")
        }

        fn reset_reader(&mut self) {
            self.pos = 0;
        }
    }

    fn plp_varbinary_metadata(
        column_name: &str,
        crypto_metadata: Option<crate::query::metadata::CryptoMetadata>,
    ) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags: if crypto_metadata.is_some() { 0x0800 } else { 0 },
            data_type: TdsDataType::BigVarBinary,
            type_info: TypeInfo::partial_len(TdsDataType::BigVarBinary, 0xFFFF, None).unwrap(),
            column_name: column_name.to_string(),
            multi_part_name: None,
            crypto_metadata,
        }
    }

    fn ae_crypto_metadata() -> crate::query::metadata::CryptoMetadata {
        crate::query::metadata::CryptoMetadata {
            cek_table_ordinal: 0,
            base_data_type: TdsDataType::BigVarBinary,
            base_type_info: TypeInfo::partial_len(TdsDataType::BigVarBinary, 0xFFFF, None).unwrap(),
            cipher_algorithm_id: 2,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        }
    }

    #[tokio::test]
    async fn plp_paused_state_preserves_collation_for_active_stream() {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let metadata = ColumnMetadata {
            user_type: 0,
            flags: 0,
            type_info: TypeInfo::partial_len(TdsDataType::BigVarChar, 0xFFFF, Some(collation))
                .unwrap(),
            data_type: TdsDataType::BigVarChar,
            column_name: "c1".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        };
        let metadata = Arc::new(ColMetadataToken {
            column_count: 1,
            columns: vec![metadata],
            cek_table: vec![],
        });
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);

        let mut packet = vec![TokenType::Row as u8];
        packet.extend_from_slice(&(-2_i64).to_le_bytes());
        let mut reader = TestByteReader::new(packet);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let result = receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeOne(0),
            &mut writer,
            &mut None,
        )
        .await
        .unwrap();

        match result {
            RowReadResult::PlpPaused(plp_state) => {
                assert_eq!(plp_state.collation(), Some(collation));
                assert!(!plp_state.reached_end());
            }
            _ => panic!("expected PlpPaused"),
        }
    }

    fn int4_metadata(column_name: &str) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags: 0,
            data_type: TdsDataType::Int4,
            type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
            column_name: column_name.to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        }
    }

    fn two_int4_metadata() -> Arc<ColMetadataToken> {
        Arc::new(ColMetadataToken {
            column_count: 2,
            columns: vec![int4_metadata("c1"), int4_metadata("c2")],
            cek_table: vec![],
        })
    }

    // The pause states below must borrow the ParserContext's metadata rather than
    // deep-copy it: on the SQLGetData path a row pauses after every column pull,
    // so a clone here is O(N) metadata allocations per pull.
    #[tokio::test]
    async fn row_pause_shares_result_metadata_arc() {
        let metadata = two_int4_metadata();
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);

        let mut packet = vec![TokenType::Row as u8];
        packet.extend_from_slice(&1_i32.to_le_bytes());
        let mut reader = TestByteReader::new(packet);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let result = receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeOne(0),
            &mut writer,
            &mut None,
        )
        .await
        .unwrap();

        match result {
            RowReadResult::RowPaused(state) => {
                assert!(Arc::ptr_eq(&metadata, &state.metadata));
                assert_eq!(state.next_column_index, 1);
            }
            other => panic!("expected RowPaused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn row_header_shares_result_metadata_arc() {
        let metadata = two_int4_metadata();
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);

        let mut reader = TestByteReader::new(vec![TokenType::Row as u8]);
        let registry = GenericTokenParserRegistry::default();

        match receive_row_header_internal(&mut reader, &registry, &context, &mut None)
            .await
            .unwrap()
        {
            RowHeader::Positioned(state) => {
                assert!(Arc::ptr_eq(&metadata, &state.metadata));
                assert_eq!(state.next_column_index, 0);
                assert!(state.nbc_null_bitmap.is_none());
            }
            RowHeader::Token(_) => panic!("expected Positioned, got Token"),
        }
    }

    #[tokio::test]
    async fn nbcrow_pause_shares_header_bitmap_arc() {
        let metadata = two_int4_metadata();
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);

        let mut reader = TestByteReader::new(vec![TokenType::NbcRow as u8, 0b0000_0001]);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let RowHeader::Positioned(header_state) =
            receive_row_header_internal(&mut reader, &registry, &context, &mut None)
                .await
                .unwrap()
        else {
            panic!("expected Positioned");
        };
        let bitmap = header_state.nbc_null_bitmap.as_ref().unwrap().clone();

        match resume_row_into_internal(
            &mut reader,
            header_state,
            ColumnPolicy::DecodeOne(0),
            &mut writer,
        )
        .await
        .unwrap()
        {
            RowReadResult::RowPaused(state) => {
                assert!(Arc::ptr_eq(&metadata, &state.metadata));
                assert_eq!(state.next_column_index, 1);
                assert!(Arc::ptr_eq(
                    &bitmap,
                    state.nbc_null_bitmap.as_ref().unwrap()
                ));
            }
            other => panic!("expected RowPaused, got {other:?}"),
        }
    }

    /// Reads the next NBCROW header and yields just its null bitmap, dropping
    /// the rest of the pause state so the bitmap's only holders are the caller
    /// and `scratch`.
    async fn next_nbc_bitmap(
        reader: &mut TestByteReader,
        registry: &GenericTokenParserRegistry,
        context: &ParserContext,
        scratch: &mut Option<Arc<[u8]>>,
    ) -> Arc<[u8]> {
        let RowHeader::Positioned(state) =
            receive_row_header_internal(reader, registry, context, scratch)
                .await
                .unwrap()
        else {
            panic!("expected Positioned");
        };
        state.nbc_null_bitmap.expect("NBCROW carries a null bitmap")
    }

    // NBCROW rows are the hot path for wide result sets, so the bitmap
    // allocation must be refilled rather than reallocated once no paused row
    // still holds it.
    #[tokio::test]
    async fn nbcrow_bitmap_allocation_is_reused_across_rows() {
        let context = ParserContext::ColumnMetadata(two_int4_metadata(), None);

        let mut reader = TestByteReader::new(vec![
            TokenType::NbcRow as u8,
            0b0000_0001,
            TokenType::NbcRow as u8,
            0b0000_0010,
        ]);
        let registry = GenericTokenParserRegistry::default();
        let mut scratch = None;

        let first = next_nbc_bitmap(&mut reader, &registry, &context, &mut scratch).await;
        assert_eq!(*first, [0b0000_0001]);
        let first_allocation = first.as_ptr();
        drop(first);

        let second = next_nbc_bitmap(&mut reader, &registry, &context, &mut scratch).await;
        assert_eq!(*second, [0b0000_0010]);
        assert_eq!(second.as_ptr(), first_allocation);
    }

    // A consumer holding a `RowPauseState` past end-of-row keeps its bitmap
    // alive, so refilling in place would corrupt a row still being read.
    #[tokio::test]
    async fn nbcrow_bitmap_is_not_reused_while_a_paused_row_holds_it() {
        let context = ParserContext::ColumnMetadata(two_int4_metadata(), None);

        let mut reader = TestByteReader::new(vec![
            TokenType::NbcRow as u8,
            0b0000_0001,
            TokenType::NbcRow as u8,
            0b0000_0010,
        ]);
        let registry = GenericTokenParserRegistry::default();
        let mut scratch = None;

        let held = next_nbc_bitmap(&mut reader, &registry, &context, &mut scratch).await;
        let next = next_nbc_bitmap(&mut reader, &registry, &context, &mut scratch).await;

        assert_ne!(next.as_ptr(), held.as_ptr());
        assert_eq!(*held, [0b0000_0001]);
        assert_eq!(*next, [0b0000_0010]);
    }

    // A new result set can change the column count, so a cached bitmap of the
    // wrong length must not be refilled and handed back at the wrong size.
    #[tokio::test]
    async fn nbcrow_bitmap_is_not_reused_across_a_column_count_change() {
        let narrow = ParserContext::ColumnMetadata(two_int4_metadata(), None);
        let wide = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 9,
                columns: (0..9).map(|i| int4_metadata(&format!("c{i}"))).collect(),
                cek_table: vec![],
            }),
            None,
        );

        let mut reader = TestByteReader::new(vec![
            TokenType::NbcRow as u8,
            0b0000_0001,
            TokenType::NbcRow as u8,
            0b0000_0011,
            0b0000_0001,
        ]);
        let registry = GenericTokenParserRegistry::default();
        let mut scratch = None;

        let first = next_nbc_bitmap(&mut reader, &registry, &narrow, &mut scratch).await;
        assert_eq!(*first, [0b0000_0001]);
        drop(first);

        let second = next_nbc_bitmap(&mut reader, &registry, &wide, &mut scratch).await;
        assert_eq!(*second, [0b0000_0011, 0b0000_0001]);
    }

    // `read_bytes` reports a count and contracts no full fill. Because the
    // buffer is reused, a partial fill would leave the previous row's bits in
    // the tail, so it has to be rejected rather than decoded.
    #[tokio::test]
    async fn nbcrow_short_bitmap_read_is_rejected() {
        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 9,
                columns: (0..9).map(|i| int4_metadata(&format!("c{i}"))).collect(),
                cek_table: vec![],
            }),
            None,
        );

        let mut reader =
            TestByteReader::new(vec![TokenType::NbcRow as u8, 0b0000_0011, 0b0000_0001])
                .with_short_read(1);
        let registry = GenericTokenParserRegistry::default();
        let mut scratch = None;

        let Err(err) =
            receive_row_header_internal(&mut reader, &registry, &context, &mut scratch).await
        else {
            panic!("a truncated null bitmap must be rejected");
        };

        assert!(
            err.to_string().contains("NBCROW null bitmap truncated"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn plp_pause_shares_result_metadata_arc() {
        let metadata = Arc::new(ColMetadataToken {
            column_count: 1,
            columns: vec![plp_varbinary_metadata("c1", None)],
            cek_table: vec![],
        });
        let context = ParserContext::ColumnMetadata(Arc::clone(&metadata), None);

        let mut packet = vec![TokenType::Row as u8];
        packet.extend_from_slice(&(-2_i64).to_le_bytes());
        let mut reader = TestByteReader::new(packet);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let result = receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeOne(0),
            &mut writer,
            &mut None,
        )
        .await
        .unwrap();

        match result {
            RowReadResult::PlpPaused(plp_state) => {
                assert!(Arc::ptr_eq(&metadata, &plp_state.row_pause_state.metadata));
            }
            _ => panic!("expected PlpPaused"),
        }
    }

    #[test]
    fn row_pause_state_debug_redacts_cek_secrets() {
        let encrypted_key = vec![0x2A; 4];
        let metadata = Arc::new(ColMetadataToken {
            column_count: 1,
            columns: vec![int4_metadata("c1")],
            cek_table: vec![crate::query::metadata::CekTableEntry {
                database_id: 1,
                cek_id: 2,
                cek_version: 3,
                cek_md_version: [0u8; 8],
                encrypted_cek_values: vec![crate::query::metadata::EncryptedCekValue {
                    encrypted_key: encrypted_key.clone(),
                    key_store_name: "AZURE_KEY_VAULT".to_string(),
                    key_path: "https://vault.example/keys/cmk".to_string(),
                    algorithm_name: "RSA_OAEP".to_string(),
                }],
            }],
        });

        let rendered = format!(
            "{:?}",
            RowPauseState {
                next_column_index: 0,
                metadata,
                nbc_null_bitmap: None,
                decryptor: None,
            }
        );

        assert!(!rendered.contains(&format!("{encrypted_key:?}")));
        assert!(!rendered.contains("vault.example"));
        assert!(!rendered.contains("AZURE_KEY_VAULT"));
        assert!(rendered.contains("c1"));
    }

    #[tokio::test]
    async fn nbcrow_pause_and_plp_resume_path_is_exercised() {
        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 2,
                columns: vec![
                    ColumnMetadata {
                        user_type: 0,
                        flags: 0,
                        data_type: TdsDataType::Int4,
                        type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                        column_name: "c1".to_string(),
                        multi_part_name: None,
                        crypto_metadata: None,
                    },
                    plp_varbinary_metadata("c2", None),
                ],
                cek_table: vec![],
            }),
            None,
        );

        let mut packet = vec![TokenType::NbcRow as u8, 0b0000_0001];
        packet.extend_from_slice(&(-2_i64).to_le_bytes());
        let mut reader = TestByteReader::new(packet);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let RowHeader::Positioned(header_state) =
            receive_row_header_internal(&mut reader, &registry, &context, &mut None)
                .await
                .unwrap()
        else {
            panic!("expected Positioned");
        };
        let bitmap = header_state.nbc_null_bitmap.as_ref().unwrap().clone();
        let result = resume_row_into_internal(
            &mut reader,
            header_state,
            ColumnPolicy::DecodeOne(1),
            &mut writer,
        )
        .await
        .unwrap();

        match result {
            RowReadResult::PlpPaused(plp_state) => {
                assert!(plp_state.collation().is_none());
                assert!(!plp_state.reached_end());
                assert!(Arc::ptr_eq(
                    &bitmap,
                    plp_state.row_pause_state.nbc_null_bitmap.as_ref().unwrap()
                ));
            }
            _ => panic!("expected PlpPaused"),
        }
    }

    #[tokio::test]
    async fn ae_paused_plp_streaming_fails_fast() {
        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 1,
                columns: vec![plp_varbinary_metadata("c1", Some(ae_crypto_metadata()))],
                cek_table: vec![],
            }),
            None,
        );

        let mut reader = TestByteReader::new(vec![TokenType::Row as u8]);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DiscardRowWriter;

        let result = receive_row_into_internal(
            &mut reader,
            &registry,
            &context,
            ColumnPolicy::DecodeOne(0),
            &mut writer,
            &mut None,
        )
        .await;

        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, context }) => {
                assert_eq!(feature, "Always Encrypted paused PLP streaming");
                assert!(context.contains("Encrypted PLP column 'c1' cannot be streamed"));
                assert!(context.contains("read_active_plp_bytes"));
            }
            Err(err) => panic!("expected UnimplementedFeature, got: {err:?}"),
            Ok(_) => panic!("expected AE paused PLP streaming to fail"),
        }
    }

    struct MockTokenParserRegistry {
        parsers: HashMap<TokenType, TokenParsers>,
    }

    impl MockTokenParserRegistry {
        fn new() -> Self {
            Self {
                parsers: HashMap::new(),
            }
        }

        fn add_parser(&mut self, token_type: TokenType, parser: TokenParsers) {
            self.parsers.insert(token_type, parser);
        }
    }

    impl TokenParserRegistry for MockTokenParserRegistry {
        fn get_parser(&self, token_type: &TokenType) -> Option<&TokenParsers> {
            self.parsers.get(token_type)
        }
    }

    #[test]
    fn test_custom_token_parser_registry() {
        let mut registry = MockTokenParserRegistry::new();

        assert!(registry.get_parser(&TokenType::Done).is_none());

        registry.add_parser(TokenType::Done, TokenParsers::from(DoneTokenParser {}));

        assert!(registry.get_parser(&TokenType::Done).is_some());
    }

    #[test]
    fn test_parser_registry_count() {
        let registry = GenericTokenParserRegistry::default();
        let expected_count = 15; // Number of token types registered in default()

        let token_types = [
            TokenType::EnvChange,
            TokenType::LoginAck,
            TokenType::Done,
            TokenType::DoneInProc,
            TokenType::DoneProc,
            TokenType::Info,
            TokenType::Error,
            TokenType::FeatureExtAck,
            TokenType::FedAuthInfo,
            TokenType::ColMetadata,
            TokenType::Row,
            TokenType::Order,
            TokenType::ReturnStatus,
            TokenType::NbcRow,
            TokenType::ReturnValue,
        ];

        let count = token_types
            .iter()
            .filter(|tt| registry.get_parser(tt).is_some())
            .count();
        assert_eq!(count, expected_count);
    }
}
