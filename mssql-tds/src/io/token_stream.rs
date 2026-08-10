// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::decoder::{
    GenericDecoder, PlpChunkStreamReader, PlpColumnStream, decrypt_cipher_value,
    decrypt_encrypted_column,
};
use crate::datatypes::row_writer::{DefaultRowWriter, RowWriter, write_column_value};
use crate::datatypes::sql_string::{SqlString, get_encoding_type};
use crate::datatypes::sqldatatypes::TdsDataType;
use crate::datatypes::sync_decoder::{PlpProgress, plp_collect_step};
use crate::io::packet_buffer::PacketBuffer;
use crate::io::packet_reader::TdsPacketReader;
use crate::io::sync_token;
use crate::io::tds_core::{RowStep, TdsCore, TokenStep};
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

/// Result of attempting to read a row directly into a [`RowWriter`].
#[cfg(not(fuzzing))]
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
pub enum RowReadResult {
    RowWritten,
    Token(Tokens),
    RowPaused(RowPauseState),
    PlpPaused(PlpPauseState),
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
    /// Full column metadata for the row (shared with the ParserContext).
    pub(crate) columns: Vec<ColumnMetadata>,
    /// NBCROW null-bitmap (one bit per column, LSB-first).  `None` for plain ROW.
    pub(crate) nbc_null_bitmap: Option<Vec<u8>>,
    /// Optional AE decryptor needed to continue decrypting encrypted columns
    /// after a row pause/resume boundary.
    pub(crate) decryptor: Option<Arc<dyn CellDecryptor>>,
}

#[derive(Debug)]
#[cfg(fuzzing)]
pub struct RowPauseState {
    pub next_column_index: usize,
    pub columns: Vec<ColumnMetadata>,
    pub nbc_null_bitmap: Option<Vec<u8>>,
    pub decryptor: Option<Arc<dyn CellDecryptor>>,
}

impl RowPauseState {
    /// Clones this cursor advanced to `next_column_index`, carrying the column
    /// metadata, NBCROW bitmap, and AE decryptor forward. Mirrors the pause-state
    /// construction in the async oracle so the resume cursor is byte-identical.
    pub(crate) fn resume_at(&self, next_column_index: usize) -> RowPauseState {
        RowPauseState {
            next_column_index,
            columns: self.columns.clone(),
            nbc_null_bitmap: self.nbc_null_bitmap.clone(),
            decryptor: self.decryptor.clone(),
        }
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
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    /// Resume a paused row decode from the column after the one that triggered
    /// [`pause_after_column`](RowWriter::pause_after_column).
    ///
    /// The caller is responsible for passing back the exact [`RowPauseState`]
    /// that was returned inside `RowReadResult::RowPaused`.
    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
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
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult>;

    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
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
}

/// Column metadata plus the optional cell decryptor needed to decode a row.
///
/// Returned by [`extract_row_context`] so the ROW/NBCROW decode paths can both
/// access the column layout and the Always Encrypted decryptor (if any).
type RowDecodeContext<'a> = (&'a [ColumnMetadata], Option<&'a Arc<dyn CellDecryptor>>);

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

pub(crate) fn extract_row_context(context: &ParserContext) -> TdsResult<RowDecodeContext<'_>> {
    match context {
        ParserContext::ColumnMetadata(metadata, decryptor) => {
            Ok((&metadata.columns, decryptor.as_ref()))
        }
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

/// Test/fuzzing-only reference oracle for the non-row token receive path.
///
/// Production token consumption runs through the synchronous
/// [`TdsCore::step_token`] body driven by [`drive_token_over_buffer`], which
/// sync-parses the bounded category-(a) tokens in place and seams the
/// value-carrying / login tokens through [`dispatch_token`]. This async
/// whole-token read is retained solely as a differential byte-identity oracle
/// for the refill-boundary tests; no production path reaches it.
#[cfg(any(test, fuzzing))]
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

/// Decodes columns starting at `start_col` for a plain ROW token.
///
/// Test/fuzzing-only reference oracle. Production row decoding runs through the
/// synchronous [`TdsCore::step_row`] body driven by [`drive_row_over_buffer`];
/// this async framing is retained only to differentially cross-check that body
/// (via the buffer-owning `PacketReader` and the bufferless `TestByteReader`).
#[cfg(any(test, fuzzing))]
async fn decode_row_columns<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    columns: &[ColumnMetadata],
    decryptor: Option<&Arc<dyn CellDecryptor>>,
    start_col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult> {
    let decoder = GenericDecoder::default();
    for (col, meta) in columns.iter().enumerate().skip(start_col) {
        // For PLP target columns, pause before payload consumption so callers
        // can stream SQLGetData-style chunks from wire.
        if meta.is_plp() && writer.pause_after_column(col) {
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
                    if col + 1 < columns.len() {
                        return Ok(RowReadResult::RowPaused(RowPauseState {
                            next_column_index: col + 1,
                            columns: columns.to_vec(),
                            nbc_null_bitmap: None,
                            decryptor: decryptor.cloned(),
                        }));
                    }
                    return Ok(RowReadResult::RowWritten);
                }
                Some(plp_stream) => {
                    return Ok(RowReadResult::PlpPaused(PlpPauseState {
                        row_pause_state: RowPauseState {
                            next_column_index: col + 1,
                            columns: columns.to_vec(),
                            nbc_null_bitmap: None,
                            decryptor: decryptor.cloned(),
                        },
                        plp_stream,
                    }));
                }
            }
        }

        decode_or_decrypt_column(&decoder, reader, meta, decryptor, col, writer).await?;
        if writer.pause_after_column(col) && col + 1 < columns.len() {
            return Ok(RowReadResult::RowPaused(RowPauseState {
                next_column_index: col + 1,
                columns: columns.to_vec(),
                nbc_null_bitmap: None,
                decryptor: decryptor.cloned(),
            }));
        }
    }
    Ok(RowReadResult::RowWritten)
}

/// Decodes columns starting at `start_col` for an NBCROW token.
///
/// Test/fuzzing-only reference oracle (see [`decode_row_columns`]).
#[cfg(any(test, fuzzing))]
async fn decode_nbcrow_columns<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    columns: &[ColumnMetadata],
    decryptor: Option<&Arc<dyn CellDecryptor>>,
    bitmap: &[u8],
    start_col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult> {
    let decoder = GenericDecoder::default();
    for (col, meta) in columns.iter().enumerate().skip(start_col) {
        if bitmap[col / 8] & (1 << (col % 8)) != 0 {
            writer.write_null(col);
        } else {
            if meta.is_plp() && writer.pause_after_column(col) {
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
                        if col + 1 < columns.len() {
                            return Ok(RowReadResult::RowPaused(RowPauseState {
                                next_column_index: col + 1,
                                columns: columns.to_vec(),
                                nbc_null_bitmap: Some(bitmap.to_vec()),
                                decryptor: decryptor.cloned(),
                            }));
                        }
                        return Ok(RowReadResult::RowWritten);
                    }
                    Some(plp_stream) => {
                        return Ok(RowReadResult::PlpPaused(PlpPauseState {
                            row_pause_state: RowPauseState {
                                next_column_index: col + 1,
                                columns: columns.to_vec(),
                                nbc_null_bitmap: Some(bitmap.to_vec()),
                                decryptor: decryptor.cloned(),
                            },
                            plp_stream,
                        }));
                    }
                }
            }

            decode_or_decrypt_column(&decoder, reader, meta, decryptor, col, writer).await?;
        }
        if writer.pause_after_column(col) && col + 1 < columns.len() {
            return Ok(RowReadResult::RowPaused(RowPauseState {
                next_column_index: col + 1,
                columns: columns.to_vec(),
                nbc_null_bitmap: Some(bitmap.to_vec()),
                decryptor: decryptor.cloned(),
            }));
        }
    }
    Ok(RowReadResult::RowWritten)
}

async fn decode_or_decrypt_column<R: TdsPacketReader + Send + Sync>(
    decoder: &GenericDecoder,
    reader: &mut R,
    meta: &ColumnMetadata,
    decryptor: Option<&Arc<dyn CellDecryptor>>,
    col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<()> {
    match (meta.crypto_metadata.is_some(), decryptor) {
        (true, Some(dec)) => {
            if crate::datatypes::sync_decoder::is_supported(meta) {
                // Non-PLP ciphertext: buffer + decode the cipher cell atomically
                // via the shared sync step, then run the synchronous cell
                // decryptor and write the plaintext.
                let mut cipher_cell = DefaultRowWriter::new(1);
                reader.decode_column_into(meta, 0, &mut cipher_cell).await?;
                let cipher = cipher_cell
                    .take_row()
                    .pop()
                    .unwrap_or(crate::datatypes::column_values::ColumnValues::Null);
                let value = decrypt_cipher_value(meta, dec, cipher)?;
                write_column_value(writer, col, value);
            } else if meta.is_plp() {
                // PLP ciphertext (varbinary(max)): collect the whole ciphertext
                // via the sync PLP core, then run the synchronous decryptor.
                // Whole-value materialization before decrypt is required (AE
                // needs the full cipher block); only the byte pull is inverted
                // to the sync buffer, mirroring the non-PLP AE fold above.
                let cipher = match reader.collect_plp_bytes().await? {
                    Some(bytes) => crate::datatypes::column_values::ColumnValues::Bytes(bytes),
                    None => crate::datatypes::column_values::ColumnValues::Null,
                };
                let value = decrypt_cipher_value(meta, dec, cipher)?;
                write_column_value(writer, col, value);
            } else {
                let value = decrypt_encrypted_column(decoder, reader, meta, dec).await?;
                write_column_value(writer, col, value);
            }
        }
        (true, None) => {
            tracing::info!(
                column = %meta.column_name,
                "Encrypted column has no column-encryption decryptor available \
                 (Always Encrypted disabled for this command, or no key-store \
                 provider registered); returning the raw ciphertext varbinary"
            );
            decode_non_plp_column(decoder, reader, meta, col, writer).await?;
        }
        (false, _) => {
            decode_non_plp_column(decoder, reader, meta, col, writer).await?;
        }
    }
    Ok(())
}

/// Decodes a single non-encrypted, non-PLP-or-PLP column.
///
/// Routes cells whose type is owned by the synchronous column-atomic path
/// (`sync_decoder::is_supported`) through `reader.decode_column_into`, which
/// lifts the `.await` out of the middle of the cell to the column boundary.
/// PLP cells and not-yet-ported types fall back to the legacy async
/// `decode_into`, preserving byte-identical decoding.
async fn decode_non_plp_column<R: TdsPacketReader + Send + Sync>(
    decoder: &GenericDecoder,
    reader: &mut R,
    meta: &ColumnMetadata,
    col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<()> {
    if crate::datatypes::sync_decoder::is_supported(meta) {
        reader.decode_column_into(meta, col, writer).await
    } else {
        decoder.decode_into(reader, meta, col, writer).await
    }
}

/// Test/fuzzing-only reference oracle for a fresh row read.
///
/// Production reads run through [`drive_row_over_buffer`] over the synchronous
/// [`TdsCore::step_row`] body. This async framing is retained to differentially
/// cross-check that body: it is exercised by the buffer-owning `PacketReader`
/// tests and the bufferless `TestByteReader` async-default tests, and by the
/// `fuzzing` token stream reader.
#[cfg(any(test, fuzzing))]
pub(crate) async fn receive_row_into_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    registry: &impl TokenParserRegistry,
    context: &ParserContext,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult> {
    let token_type_byte = reader.read_byte().await?;
    let token_type: TokenType = token_type_byte.try_into()?;
    debug!("Parsing token type: {:?}", &token_type);

    match token_type {
        TokenType::Row => {
            let (columns, decryptor) = extract_row_context(context)?;
            decode_row_columns(reader, columns, decryptor, 0, writer).await
        }
        TokenType::NbcRow => {
            let (columns, decryptor) = extract_row_context(context)?;
            let bitmap_len = columns.len().div_ceil(8);
            let bitmap = reader.read_null_bitmap(bitmap_len).await?;
            decode_nbcrow_columns(reader, columns, decryptor, &bitmap, 0, writer).await
        }
        _ => {
            let token = dispatch_token(reader, registry, token_type, context).await?;
            Ok(RowReadResult::Token(token))
        }
    }
}

/// Resumes a paused row decode from `pause_state.next_column_index`.
///
/// Does not read a token-type byte — the token has already been consumed.
///
/// Test/fuzzing-only reference oracle (see [`receive_row_into_internal`]); the
/// production resume path drives [`TdsCore::step_row`] with a `Some` cursor.
#[cfg(any(test, fuzzing))]
pub(crate) async fn resume_row_into_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    pause_state: RowPauseState,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult> {
    let RowPauseState {
        next_column_index,
        columns,
        nbc_null_bitmap,
        decryptor,
    } = pause_state;

    match nbc_null_bitmap {
        None => {
            decode_row_columns(
                reader,
                &columns,
                decryptor.as_ref(),
                next_column_index,
                writer,
            )
            .await
        }
        Some(bitmap) => {
            decode_nbcrow_columns(
                reader,
                &columns,
                decryptor.as_ref(),
                &bitmap,
                next_column_index,
                writer,
            )
            .await
        }
    }
}

pub(crate) async fn read_active_plp_bytes_internal<R: TdsPacketReader + Send + Sync>(
    reader: &mut R,
    plp_state: &mut PlpPauseState,
    out: &mut [u8],
) -> TdsResult<usize> {
    plp_state.plp_stream.read_into(reader, out).await
}

/// A [`TdsPacketReader`] that owns its reassembly [`PacketBuffer`], so the sync
/// [`TdsCore::step_row`] body can decode cells in place and the driver can lift
/// the sole refill `.await` out to the column boundary.
#[async_trait]
pub(crate) trait BufferedRowReader: TdsPacketReader + Send + Sync {
    /// The owned reassembly buffer that `step_row` decodes over.
    fn row_buffer_mut(&mut self) -> &mut PacketBuffer;

    /// Pulls one more TDS packet into the buffer, erroring (rather than
    /// spinning) if the refill exposes no new bytes.
    async fn refill_row_buffer(&mut self) -> TdsResult<()>;
}

/// Outcome of servicing one async-seam column via [`decode_async_row_column`].
enum AsyncColumnOutcome {
    /// The column was decoded eagerly; continue the row at `col + 1`.
    Continue,
    /// The row read terminated (paused or completed) at this column.
    Terminal(RowReadResult),
}

/// Decodes one non-null column whose byte pull lives in an async seam.
///
/// This reproduces exactly one iteration of the [`decode_row_columns`] /
/// [`decode_nbcrow_columns`] non-null body: the pre-payload PLP streaming-pause
/// block (p7) and the eager [`decode_or_decrypt_column`] call (eager PLP, p4d
/// legacy LOBs, rare fallback types, AE fallback), plus the post-column pause
/// check. Calling `decode_or_decrypt_column` verbatim keeps the decoded value
/// and the resume cursor byte-identical to the reference oracle.
async fn decode_async_row_column<R: BufferedRowReader>(
    reader: &mut R,
    state: &RowPauseState,
    col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<AsyncColumnOutcome> {
    let decoder = GenericDecoder::default();
    let columns = &state.columns;
    let meta = &columns[col];
    let decryptor = state.decryptor.as_ref();
    let len = columns.len();

    if meta.is_plp() && writer.pause_after_column(col) {
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
                if col + 1 < len {
                    return Ok(AsyncColumnOutcome::Terminal(RowReadResult::RowPaused(
                        state.resume_at(col + 1),
                    )));
                }
                return Ok(AsyncColumnOutcome::Terminal(RowReadResult::RowWritten));
            }
            Some(plp_stream) => {
                return Ok(AsyncColumnOutcome::Terminal(RowReadResult::PlpPaused(
                    PlpPauseState {
                        row_pause_state: state.resume_at(col + 1),
                        plp_stream,
                    },
                )));
            }
        }
    }

    decode_or_decrypt_column(&decoder, reader, meta, decryptor, col, writer).await?;
    if writer.pause_after_column(col) && col + 1 < len {
        return Ok(AsyncColumnOutcome::Terminal(RowReadResult::RowPaused(
            state.resume_at(col + 1),
        )));
    }
    Ok(AsyncColumnOutcome::Continue)
}

/// Production row-fetch driver: the single async shell over [`TdsCore::step_row`].
///
/// It owns the only `.await` on the row path — refilling the buffer on
/// [`RowStep::NeedBytes`] and servicing an [`RowStep::AsyncColumn`] through the
/// existing async seam — then re-drives the sync body from the shared cursor.
/// `initial_resume` is `None` for a fresh row (the header is consumed) and
/// `Some` for a resume-from-pause (the header is skipped); `registry` and
/// `context` are inert on the resume path.
pub(crate) async fn drive_row_over_buffer<R, Reg>(
    reader: &mut R,
    registry: &Reg,
    context: &ParserContext,
    initial_resume: Option<RowPauseState>,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult>
where
    R: BufferedRowReader,
    Reg: TokenParserRegistry,
{
    let mut resume = initial_resume;
    loop {
        let step = TdsCore::step_row(reader.row_buffer_mut(), &mut resume, context, writer)?;
        match step {
            RowStep::RowWritten => return Ok(RowReadResult::RowWritten),
            RowStep::RowPaused(state) => return Ok(RowReadResult::RowPaused(state)),
            RowStep::Token(token_type) => {
                let token = resolve_header_token(reader, registry, token_type, context).await?;
                return Ok(RowReadResult::Token(token));
            }
            RowStep::NeedBytes(need) => {
                tracing::trace!(
                    shortfall = need.shortfall,
                    "refilling row buffer for step_row"
                );
                reader.refill_row_buffer().await?;
            }
            RowStep::AsyncColumn { col } => {
                let state = resume.as_ref().expect("row cursor set before AsyncColumn");
                match decode_async_row_column(reader, state, col, writer).await? {
                    AsyncColumnOutcome::Continue => {
                        resume
                            .as_mut()
                            .expect("row cursor set before AsyncColumn")
                            .next_column_index = col + 1;
                    }
                    AsyncColumnOutcome::Terminal(result) => return Ok(result),
                }
            }
        }
    }
}

/// Resolves a non-row token whose token byte has already been consumed.
///
/// This is the shared terminal-token seam used by both the top-level non-row
/// receive driver ([`drive_token_over_buffer`]) and the row driver's
/// [`RowStep::Token`] arm: [`TdsCore::begin_row_header`] consumes the token byte
/// then hands the classification back, and this helper decodes it.
///
/// Category-(a) bounded tokens are parsed in place by the pure-sync
/// [`sync_token::parse_token_body`] body — refilling the buffer until the whole
/// length-bounded token is resident, so the parse runs over a complete slice and
/// never sees a partial buffer. Every other token (the value-carrying (b)
/// tokens plus login/handshake tokens) stays on the async seam via the existing
/// [`dispatch_token`] parser. This is the token-atomic analog of
/// [`decode_async_row_column`] for the row path's `AsyncColumn`.
async fn resolve_header_token<R, Reg>(
    reader: &mut R,
    registry: &Reg,
    token_type: TokenType,
    context: &ParserContext,
) -> TdsResult<Tokens>
where
    R: BufferedRowReader,
    Reg: TokenParserRegistry,
{
    if !sync_token::is_sync_token(&token_type) {
        return dispatch_token(reader, registry, token_type, context).await;
    }
    loop {
        let total = match sync_token::body_len(reader.row_buffer_mut(), &token_type) {
            Ok(total) => total,
            Err(_) => {
                reader.refill_row_buffer().await?;
                continue;
            }
        };
        if reader.row_buffer_mut().ensure(total).is_err() {
            reader.refill_row_buffer().await?;
            continue;
        }
        return sync_token::parse_token_body(reader.row_buffer_mut(), token_type, context);
    }
}

/// Production non-row receive driver: the single async shell over
/// [`TdsCore::step_token`].
///
/// It owns the only `.await` on the non-row token path — refilling the buffer on
/// [`TokenStep::NeedBytes`] and servicing a [`TokenStep::AsyncToken`] through the
/// existing async [`dispatch_token`] seam — then re-drives the sync body from the
/// shared cursor. This is the token-consume analog of [`drive_row_over_buffer`].
pub(crate) async fn drive_token_over_buffer<R, Reg>(
    reader: &mut R,
    registry: &Reg,
    context: &ParserContext,
) -> TdsResult<Tokens>
where
    R: BufferedRowReader,
    Reg: TokenParserRegistry,
{
    loop {
        match TdsCore::step_token(reader.row_buffer_mut(), context)? {
            TokenStep::Parsed(token) => return Ok(token),
            TokenStep::AsyncToken(token_type) => {
                return dispatch_token(reader, registry, token_type, context).await;
            }
            TokenStep::NeedBytes(need) => {
                tracing::trace!(
                    shortfall = need.shortfall,
                    "refilling token buffer for step_token"
                );
                reader.refill_row_buffer().await?;
            }
        }
    }
}

/// The blocking sibling of [`BufferedRowReader`]: the sole refill seam the
/// *synchronous* row driver pulls through.
///
/// A [`BufferedRowReader`] minus the `async` — `row_buffer_mut` is identical, and
/// `refill_row_buffer_blocking` blocks the calling thread for one more packet
/// instead of awaiting it. The owned [`PacketBuffer`] and the parse body
/// ([`TdsCore::step_row`]) are shared verbatim; only this edge differs.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait BlockingRowReader {
    /// The owned reassembly buffer that `step_row` decodes over.
    fn row_buffer_mut(&mut self) -> &mut PacketBuffer;

    /// Blocks until one more TDS packet is resident, erroring (rather than
    /// spinning) if the refill exposes no new bytes.
    fn refill_row_buffer_blocking(&mut self) -> TdsResult<()>;
}

/// Blocking analog of [`crate::io::packet_reader::PacketReader::ensure`]: loops
/// [`PacketBuffer::ensure`], blocking-refilling until `byte_count` bytes are
/// readable. The forward-progress guard lives in `refill_row_buffer_blocking`.
#[cfg_attr(not(test), allow(dead_code))]
fn ensure_blocking<R: BlockingRowReader>(reader: &mut R, byte_count: usize) -> TdsResult<()> {
    loop {
        if reader.row_buffer_mut().ensure(byte_count).is_ok() {
            return Ok(());
        }
        reader.refill_row_buffer_blocking()?;
    }
}

/// Blocking sibling of [`crate::io::packet_reader::PacketReader::collect_plp_bytes`].
///
/// Classifies the 8-byte PLP header, then drives the shared [`plp_collect_step`]
/// leaf one chunk header / body slice at a time — refill lifted to
/// [`ensure_blocking`]. Residency stays bounded to ~one packet: the whole
/// (possibly multi-GB) value is never ensured into the buffer. The framing is the
/// exact leaf the async collect uses; only the refill edge is blocking.
#[cfg_attr(not(test), allow(dead_code))]
fn collect_plp_bytes_blocking<R: BlockingRowReader>(reader: &mut R) -> TdsResult<Option<Vec<u8>>> {
    ensure_blocking(reader, 8)?;
    let raw = reader.row_buffer_mut().take_i64_le()?;
    let mut plp = match PlpChunkStreamReader::classify_length(raw)? {
        None => return Ok(None),
        Some(len) => PlpChunkStreamReader::new(len),
    };

    let mut out = Vec::new();
    loop {
        match plp_collect_step(&mut plp, reader.row_buffer_mut(), &mut out)? {
            PlpProgress::Done => return Ok(Some(out)),
            PlpProgress::NeedMore(n) => ensure_blocking(reader, n)?,
        }
    }
}

/// Blocking sibling of [`resolve_header_token`].
///
/// Category-(a) bounded tokens are parsed in place by the pure-sync
/// [`sync_token::parse_token_body`] body, blocking-refilling until the whole
/// length-bounded token is resident. Value-carrying / login tokens live on the
/// async [`dispatch_token`] seam, which the blocking L3 path cannot service yet;
/// they surface as [`crate::error::Error::UnimplementedFeature`] (a genuine
/// yield-to-driver refusal, never reached by the row-only differential corpus,
/// which terminates in a bounded DONE token). L4 supplies the sync token seam.
#[cfg_attr(not(test), allow(dead_code))]
fn resolve_header_token_blocking<R: BlockingRowReader>(
    reader: &mut R,
    token_type: TokenType,
    context: &ParserContext,
) -> TdsResult<Tokens> {
    if !sync_token::is_sync_token(&token_type) {
        return Err(crate::error::Error::UnimplementedFeature {
            feature: "blocking sync resolution of a value-carrying token".to_string(),
            context: format!(
                "token {token_type:?} is serviced by the async parser seam; the L3 blocking \
                 driver has no sync token leaf for it yet"
            ),
        });
    }
    loop {
        let total = match sync_token::body_len(reader.row_buffer_mut(), &token_type) {
            Ok(total) => total,
            Err(_) => {
                reader.refill_row_buffer_blocking()?;
                continue;
            }
        };
        if reader.row_buffer_mut().ensure(total).is_err() {
            reader.refill_row_buffer_blocking()?;
            continue;
        }
        return sync_token::parse_token_body(reader.row_buffer_mut(), token_type, context);
    }
}

/// Blocking sibling of [`decode_async_row_column`], restricted to the L3 scope.
///
/// Reproduces the eager PLP arm of [`decode_async_row_column`] /
/// `GenericDecoder::decode_into` for non-encrypted PLP (`max`) cells:
/// `varbinary(max)` collects chunk-streamed bytes via
/// [`collect_plp_bytes_blocking`] then `write_bytes`, while `varchar(max)` /
/// `nvarchar(max)` collect the same way and `write_string` through
/// [`SqlString::new`] with the column's [`get_encoding_type`], mirroring
/// `StringDecoder::decode_string_into`'s PLP arm. Every remaining async-seam
/// reason (encrypted cells, non-PLP Text/NText LOBs, rare fallback types) stays
/// out of the L3 blocking scope and refuses via
/// [`crate::error::Error::UnimplementedFeature`].
#[cfg_attr(not(test), allow(dead_code))]
fn decode_blocking_async_column<R: BlockingRowReader>(
    reader: &mut R,
    state: &RowPauseState,
    col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<AsyncColumnOutcome> {
    let columns = &state.columns;
    let meta = &columns[col];
    let len = columns.len();

    let plp_string = meta.is_plp()
        && matches!(
            meta.data_type,
            TdsDataType::NVarChar
                | TdsDataType::BigVarChar
                | TdsDataType::NChar
                | TdsDataType::BigChar
        );

    if meta.crypto_metadata.is_some()
        || (meta.data_type != TdsDataType::BigVarBinary && !plp_string)
    {
        return Err(crate::error::Error::UnimplementedFeature {
            feature: "blocking sync decode of an async-seam column".to_string(),
            context: format!(
                "column '{}' ({:?}) is not in the L3 blocking scope (non-encrypted \
                 varbinary(max) / varchar(max) / nvarchar(max) only)",
                meta.column_name, meta.data_type
            ),
        });
    }

    match collect_plp_bytes_blocking(reader)? {
        Some(bytes) if plp_string => {
            writer.write_string(col, SqlString::new(bytes, get_encoding_type(meta)))
        }
        Some(bytes) => writer.write_bytes(col, bytes),
        None => writer.write_null(col),
    }

    if writer.pause_after_column(col) && col + 1 < len {
        return Ok(AsyncColumnOutcome::Terminal(RowReadResult::RowPaused(
            state.resume_at(col + 1),
        )));
    }
    Ok(AsyncColumnOutcome::Continue)
}

/// Blocking row-fetch driver: the single *synchronous* shell over
/// [`TdsCore::step_row`].
///
/// This is [`drive_row_over_buffer`] with every `.await` removed. It calls the
/// identical `TdsCore::step_row` parse body and re-drives from the shared cursor;
/// only the refill/AsyncColumn/Token edges block instead of awaiting. No parse
/// machine is duplicated — the sole difference from the async driver is the edge.
/// It returns the same [`RowReadResult`], so a differential test can feed one wire
/// corpus to both drivers and assert byte-identical rows and identical underflow
/// behavior. (No registry parameter: the blocking driver resolves only bounded
/// sync tokens; value-carrying tokens refuse, so it never needs the async
/// [`dispatch_token`] registry.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn drive_row_over_buffer_blocking<R: BlockingRowReader>(
    reader: &mut R,
    context: &ParserContext,
    initial_resume: Option<RowPauseState>,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<RowReadResult> {
    let mut resume = initial_resume;
    loop {
        let step = TdsCore::step_row(reader.row_buffer_mut(), &mut resume, context, writer)?;
        match step {
            RowStep::RowWritten => return Ok(RowReadResult::RowWritten),
            RowStep::RowPaused(state) => return Ok(RowReadResult::RowPaused(state)),
            RowStep::Token(token_type) => {
                let token = resolve_header_token_blocking(reader, token_type, context)?;
                return Ok(RowReadResult::Token(token));
            }
            RowStep::NeedBytes(need) => {
                tracing::trace!(
                    shortfall = need.shortfall,
                    "blocking refill of row buffer for step_row"
                );
                reader.refill_row_buffer_blocking()?;
            }
            RowStep::AsyncColumn { col } => {
                let state = resume.as_ref().expect("row cursor set before AsyncColumn");
                match decode_blocking_async_column(reader, state, col, writer)? {
                    AsyncColumnOutcome::Continue => {
                        resume
                            .as_mut()
                            .expect("row cursor set before AsyncColumn")
                            .next_column_index = col + 1;
                    }
                    AsyncColumnOutcome::Terminal(result) => return Ok(result),
                }
            }
        }
    }
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
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_row_into_internal(
                &mut self.packet_reader,
                &*self.parser_registry,
                context,
                writer,
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
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            resume_row_into_internal(&mut self.packet_reader, pause_state, writer),
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
    use crate::datatypes::column_values::{
        SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
        SqlSmallMoney, SqlTime, SqlXml,
    };
    use crate::datatypes::decoder::DecimalParts;
    use crate::datatypes::row_writer::RowWriter;
    use crate::datatypes::sql_json::SqlJson;
    use crate::datatypes::sql_string::SqlString;
    use crate::datatypes::sql_vector::SqlVector;
    use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
    use crate::io::packet_reader::TdsPacketReader;
    use crate::token::tokens::{SqlCollation, TokenType};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;

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
    }

    impl TestByteReader {
        fn new(data: Vec<u8>) -> Self {
            Self { data, pos: 0 }
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

    #[async_trait]
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
            unimplemented!("unused in test")
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

    struct PauseAtColumnWriter {
        pause_at: usize,
    }

    impl RowWriter for PauseAtColumnWriter {
        fn pause_after_column(&self, col: usize) -> bool {
            col == self.pause_at
        }

        fn write_null(&mut self, _col: usize) {}
        fn write_bool(&mut self, _col: usize, _val: bool) {}
        fn write_u8(&mut self, _col: usize, _val: u8) {}
        fn write_i16(&mut self, _col: usize, _val: i16) {}
        fn write_i32(&mut self, _col: usize, _val: i32) {}
        fn write_i64(&mut self, _col: usize, _val: i64) {}
        fn write_f32(&mut self, _col: usize, _val: f32) {}
        fn write_f64(&mut self, _col: usize, _val: f64) {}
        fn write_string(&mut self, _col: usize, _val: SqlString) {}
        fn write_bytes(&mut self, _col: usize, _val: Vec<u8>) {}
        fn write_decimal(&mut self, _col: usize, _val: DecimalParts) {}
        fn write_numeric(&mut self, _col: usize, _val: DecimalParts) {}
        fn write_date(&mut self, _col: usize, _val: SqlDate) {}
        fn write_time(&mut self, _col: usize, _val: SqlTime) {}
        fn write_datetime(&mut self, _col: usize, _val: SqlDateTime) {}
        fn write_smalldatetime(&mut self, _col: usize, _val: SqlSmallDateTime) {}
        fn write_datetime2(&mut self, _col: usize, _val: SqlDateTime2) {}
        fn write_datetimeoffset(&mut self, _col: usize, _val: SqlDateTimeOffset) {}
        fn write_money(&mut self, _col: usize, _val: SqlMoney) {}
        fn write_smallmoney(&mut self, _col: usize, _val: SqlSmallMoney) {}
        fn write_uuid(&mut self, _col: usize, _val: uuid::Uuid) {}
        fn write_xml(&mut self, _col: usize, _val: SqlXml) {}
        fn write_json(&mut self, _col: usize, _val: SqlJson) {}
        fn write_vector(&mut self, _col: usize, _val: SqlVector) {}
        fn end_row(&mut self) {}
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
        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 1,
                columns: vec![metadata],
                cek_table: vec![],
            }),
            None,
        );

        let mut packet = vec![TokenType::Row as u8];
        packet.extend_from_slice(&(-2_i64).to_le_bytes());
        let mut reader = TestByteReader::new(packet);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = PauseAtColumnWriter { pause_at: 0 };

        let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
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
        let mut writer = PauseAtColumnWriter { pause_at: 1 };

        let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
            .await
            .unwrap();

        match result {
            RowReadResult::PlpPaused(plp_state) => {
                assert!(plp_state.collation().is_none());
                assert!(!plp_state.reached_end());
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
        let mut writer = PauseAtColumnWriter { pause_at: 0 };

        let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer).await;

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

    /// Mandatory blocking test (L4a test A): a non-PLP column followed by a PLP
    /// column decoded through the buffer-owning `PacketReader` — the reader whose
    /// sync driver L4a inverts. The refill boundary is swept across every byte
    /// offset (including the exact non-PLP -> PLP transition and mid-cell splits)
    /// and every split must decode byte-identically to the single-packet
    /// baseline. This proves the inverted sync step consumes the non-PLP cell
    /// atomically and hands off to the async PLP path with a coherent shared
    /// cursor, regardless of where the packet boundary lands.
    #[tokio::test]
    async fn nonplp_to_plp_transition_is_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::NVarChar,
                type_info: TypeInfo::partial_len(TdsDataType::NVarChar, 0xFFFF, Some(collation))
                    .unwrap(),
                column_name: "s".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
        ];

        // ROW payload: [Row token][int4 = 42][nvarchar(max) PLP = "Hi"].
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&42_i32.to_le_bytes());
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // SQL_PLP_UNKNOWNLEN
        let hi_utf16 = [0x48u8, 0x00, 0x69, 0x00]; // "Hi"
        payload.extend_from_slice(&(hi_utf16.len() as u32).to_le_bytes());
        payload.extend_from_slice(&hi_utf16);
        payload.extend_from_slice(&0u32.to_le_bytes()); // PLP zero-length terminator

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            // Clear EOM on the first packet so the buffer keeps reading into the second.
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let wire = one_packet(&payload);
        let baseline = decode(wire, &columns).await;
        assert_eq!(baseline.len(), 2);
        assert_eq!(baseline[0], ColumnValues::Int(42));
        assert_ne!(baseline[1], ColumnValues::Null);

        // Sweep the refill boundary across every interior byte offset. Offset 5
        // is the exact non-PLP -> PLP transition (token + int4); others land
        // mid-int4 and mid-PLP-chunk. All must match the single-packet decode.
        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "row decode diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// Mandatory blocking test (L4a hybrid mid-row seam): a row that MIXES two
    /// inverted non-PLP cells (fixed-width `int4` + variable-length `varchar`,
    /// both driven by the new sync `step()`) followed by a not-yet-inverted PLP
    /// `nvarchar(max)` cell (legacy async `decode_into`), all in ONE row. The
    /// refill boundary is swept across every interior byte offset — including
    /// the `int4 -> varchar` seam, inside the varchar's 2-byte USHORT length
    /// prefix (peek-only re-drive), and the higher-risk `varchar -> PLP` seam
    /// where an inverted cell hands off to the async path. Every split must
    /// decode byte-identically to the single-packet baseline, proving the
    /// inverted step and the async PLP path share ONE coherent row cursor
    /// (`next_column_index`) no matter where the packet boundary lands.
    /// Shared fixture for the hybrid non-PLP -> PLP mid-row seam tests: a row
    /// `[int4 (fixed inverted)][varchar(64) (var-length inverted)][nvarchar(max) PLP (legacy async)]`.
    fn mixed_seam_columns() -> Vec<ColumnMetadata> {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::NVarChar,
                type_info: TypeInfo::partial_len(TdsDataType::NVarChar, 0xFFFF, Some(collation))
                    .unwrap(),
                column_name: "s".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
        ]
    }

    /// ROW payload `[Row token][int4 = 42][varchar "ab"][nvarchar(max) PLP "Hi"]`.
    ///
    /// Returns `(payload, varchar_cell_range, nonplp_to_plp_transition_offset)`.
    /// `varchar_cell_range` is the interior byte range of the inverted varchar cell
    /// (USHORT prefix + data); `transition_offset` is the first byte of the PLP column —
    /// the exact whole-column non-PLP -> PLP boundary governed by `next_column_index`.
    fn mixed_seam_payload() -> (Vec<u8>, std::ops::Range<usize>, usize) {
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&42_i32.to_le_bytes());
        let varchar_start = payload.len();
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes()); // USHORT length prefix
        payload.extend_from_slice(&ab);
        let transition = payload.len(); // first byte of the PLP column
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // SQL_PLP_UNKNOWNLEN
        let hi_utf16 = [0x48u8, 0x00, 0x69, 0x00]; // "Hi"
        payload.extend_from_slice(&(hi_utf16.len() as u32).to_le_bytes());
        payload.extend_from_slice(&hi_utf16);
        payload.extend_from_slice(&0u32.to_le_bytes()); // PLP zero-length terminator
        (payload, varchar_start..transition, transition)
    }

    async fn decode_mixed_seam(
        read_data: Vec<u8>,
    ) -> Vec<crate::datatypes::column_values::ColumnValues> {
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::MockNetworkReaderWriter;

        let columns = mixed_seam_columns();
        let mut mock = MockNetworkReaderWriter::new(read_data, 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await.unwrap();
        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: columns.len() as u16,
                columns: columns.clone(),
                cek_table: vec![],
            }),
            None,
        );
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DefaultRowWriter::new(columns.len());
        let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
            .await
            .unwrap();
        assert!(matches!(result, RowReadResult::RowWritten));
        writer.take_row()
    }

    fn seam_one_packet(payload: &[u8]) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_bytes(payload).build()
    }

    fn seam_two_packets(payload: &[u8], split: usize) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let mut first = first_builder.append_bytes(&payload[..split]).build();
        first[1] = 0x00; // clear EOM so the buffer keeps reading into the second packet
        let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let second = second_builder.append_bytes(&payload[split..]).build();
        [first, second].concat()
    }

    #[tokio::test]
    async fn mixed_row_inverted_then_plp_is_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;

        let (payload, _varchar, _transition) = mixed_seam_payload();
        let baseline = decode_mixed_seam(seam_one_packet(&payload)).await;
        assert_eq!(baseline.len(), 3);
        assert_eq!(baseline[0], ColumnValues::Int(42));
        assert_ne!(baseline[1], ColumnValues::Null); // varchar "ab"
        assert_ne!(baseline[2], ColumnValues::Null); // nvarchar(max) "Hi"

        for split in 1..payload.len() {
            let got = decode_mixed_seam(seam_two_packets(&payload, split)).await;
            assert_eq!(
                got, baseline,
                "mixed row decode diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// Mandatory blocking test (L4a sharpened): the refill boundary lands EXACTLY at the
    /// whole-column non-PLP -> PLP transition. The inverted step fully consumes the last
    /// non-PLP cell (varchar) inside packet 1; the PLP column's first `ensure()` then
    /// returns `NeedBytes` -> refill -> the legacy async PLP path resumes at
    /// `next_column_index`. Proves the whole-column seam (governed by `RowPauseState`'s
    /// column cursor, not a mid-value pause) hands off cleanly and byte-identically.
    #[tokio::test]
    async fn refill_boundary_at_nonplp_to_plp_column_transition_resumes_into_async_plp() {
        use crate::datatypes::column_values::ColumnValues;

        let (payload, _varchar, transition) = mixed_seam_payload();
        let baseline = decode_mixed_seam(seam_one_packet(&payload)).await;
        assert_eq!(baseline.len(), 3);
        assert_eq!(baseline[0], ColumnValues::Int(42));
        assert_ne!(baseline[2], ColumnValues::Null); // PLP nvarchar(max) "Hi"

        let got = decode_mixed_seam(seam_two_packets(&payload, transition)).await;
        assert_eq!(
            got, baseline,
            "resume into the async PLP path diverged when the refill boundary landed exactly \
             at the non-PLP -> PLP column transition (offset {transition})"
        );
    }

    /// Cheap companion: the refill boundary lands inside the inverted var-length varchar
    /// cell — both inside its 2-byte USHORT length prefix (peek-only re-drive) and
    /// mid-data. Confirms the inverted step's OWN resume is coherent, independent of the
    /// downstream PLP handoff.
    #[tokio::test]
    async fn refill_boundary_within_inverted_varchar_cell_resumes_coherently() {
        let (payload, varchar, _transition) = mixed_seam_payload();
        let baseline = decode_mixed_seam(seam_one_packet(&payload)).await;

        for split in varchar {
            let got = decode_mixed_seam(seam_two_packets(&payload, split)).await;
            assert_eq!(
                got, baseline,
                "inverted varchar cell resume diverged when the refill boundary landed at \
                 offset {split}"
            );
        }
    }

    /// L4b blocking test 1: a single-chunk PLP `varbinary(max)` value decoded
    /// through the buffer-owning `PacketReader` — the reader whose sync PLP
    /// driver L4b inverts. The refill boundary is swept across EVERY interior
    /// byte offset, including inside the 8-byte PLP length header and inside the
    /// 4-byte chunk-length prefix, so mid-header, mid-prefix, and mid-chunk-body
    /// resume are all exercised. Every split must decode byte-identically to the
    /// single-packet baseline, proving the sync driver resumes exactly from the
    /// resumable `PlpChunkStreamReader` state after each packet-boundary refill.
    #[tokio::test]
    async fn plp_single_chunk_mid_chunk_resume_is_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let columns = vec![plp_varbinary_metadata("v", None)];

        // ROW payload: [Row token][varbinary(max) PLP: UNKNOWNLEN, one 12-byte
        // chunk, zero-length terminator].
        let body: Vec<u8> = (0u8..12).collect();
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // SQL_PLP_UNKNOWNLEN
        payload.extend_from_slice(&(body.len() as u32).to_le_bytes());
        payload.extend_from_slice(&body);
        payload.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00; // clear EOM so the buffer keeps reading into the second packet
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 1);
        assert_ne!(baseline[0], ColumnValues::Null);

        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "single-chunk PLP decode diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// L4b blocking test 2: a MULTI-chunk PLP `varbinary(max)` value spanning
    /// MULTIPLE refills, terminating on the zero-length chunk. Two parts:
    ///
    /// (a) A small multi-chunk value swept across every interior refill offset —
    /// boundaries land both BETWEEN chunks and MID-chunk-body — each byte-
    /// identical to the single-packet baseline.
    ///
    /// (b) Residency (req-1): a LARGE multi-chunk value whose total wire size
    /// exceeds the `PacketReader`'s 2-packet buffer capacity (8192 B) decodes
    /// byte-identically to the async-default baseline when fed as many packet-
    /// sized frames. This proves BY CONSTRUCTION that the sync driver keeps
    /// `PacketBuffer` residency chunk-bounded and never ensures the whole value:
    /// a whole-value `ensure` on a >8192 B value could never be satisfied by the
    /// 8192 B buffer and would trip the forward-progress guard, so a successful
    /// byte-identical decode is only possible with chunk-at-a-time `ensure`.
    #[tokio::test]
    async fn plp_multi_chunk_across_refills_is_byte_identical_with_bounded_residency() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let columns = vec![plp_varbinary_metadata("v", None)];

        fn plp_value(chunk_sizes: &[usize]) -> Vec<u8> {
            let mut plp = Vec::new();
            plp.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
            let mut counter = 0u8;
            for &n in chunk_sizes {
                plp.extend_from_slice(&(n as u32).to_le_bytes());
                for _ in 0..n {
                    plp.push(counter);
                    counter = counter.wrapping_add(1);
                }
            }
            plp.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator
            plp
        }

        // Row from a flat byte stream via the async-default seam (TestByteReader),
        // exercising the trait's default `collect_plp_bytes` — the reference the
        // sync driver must match byte-for-byte.
        async fn decode_async_default(plp: &[u8], columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut flat = vec![TokenType::Row as u8];
            flat.extend_from_slice(plp);
            let mut reader = TestByteReader::new(flat);
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        // Row through the buffer-owning PacketReader (sync PLP driver) over a
        // wire framed into `piece`-sized packets; EOM is set only on the last.
        async fn decode_framed(
            plp: &[u8],
            columns: &[ColumnMetadata],
            piece: usize,
        ) -> Vec<ColumnValues> {
            let mut payload = vec![TokenType::Row as u8];
            payload.extend_from_slice(plp);
            let mut wire = Vec::new();
            let mut offset = 0;
            while offset < payload.len() {
                let end = (offset + piece).min(payload.len());
                let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
                let mut packet = builder.append_bytes(&payload[offset..end]).build();
                if end < payload.len() {
                    packet[1] = 0x00; // clear EOM: more packets follow
                }
                wire.extend_from_slice(&packet);
                offset = end;
            }
            let mut mock = MockNetworkReaderWriter::new(wire, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }
        async fn decode_pr(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        // (a) Small multi-chunk value: three chunks, boundaries swept everywhere.
        let small = plp_value(&[5, 7, 3]);
        let mut small_payload = vec![TokenType::Row as u8];
        small_payload.extend_from_slice(&small);
        let baseline = decode_pr(one_packet(&small_payload), &columns).await;
        assert_eq!(baseline.len(), 1);
        assert_ne!(baseline[0], ColumnValues::Null);
        assert_eq!(baseline, decode_async_default(&small, &columns).await);
        for split in 1..small_payload.len() {
            let got = decode_pr(two_packets(&small_payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "multi-chunk PLP decode diverged when the refill boundary landed at offset {split}"
            );
        }

        // (b) Large multi-chunk value: total wire far exceeds the 8192 B buffer
        // capacity, so it can never be whole-resident. Fed as 2000-byte packet
        // frames; must equal the async-default decode. Success proves the driver
        // ensures one chunk at a time (bounded residency), never the whole value.
        let large = plp_value(&[3000, 3000, 3000, 2500]);
        assert!(
            large.len() > 8192,
            "residency value must exceed buffer capacity"
        );
        let large_ref = decode_async_default(&large, &columns).await;
        assert_ne!(large_ref[0], ColumnValues::Null);
        let large_framed = decode_framed(&large, &columns, 2000).await;
        assert_eq!(
            large_framed, large_ref,
            "large multi-chunk PLP value must decode byte-identically under packet-sized refills"
        );
    }

    /// L4b blocking test 3: a fully-synchronous mixed row
    /// `[int4][varchar][varbinary(max) PLP]` decoded through the buffer-owning
    /// `PacketReader`. After L4b every cell runs on the sync core: `int4` and
    /// `varchar` via the L4a non-PLP step, and the `varbinary(max)` PLP cell via
    /// the L4b sync PLP driver — no async decode remnant in the row. The refill
    /// boundary is swept across every interior offset (including the
    /// `varchar -> PLP` seam and inside the PLP header/chunk-prefix), and the
    /// exact decoded values are asserted against the single-packet baseline,
    /// proving the whole row is byte-identical no matter where the packet
    /// boundary lands.
    #[tokio::test]
    async fn fully_sync_mixed_row_with_plp_is_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            plp_varbinary_metadata("b", None),
        ];

        // ROW: [Row][int4 = 7][varchar "ab"][varbinary(max) PLP two chunks].
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&7_i32.to_le_bytes());
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        payload.extend_from_slice(&ab);
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
        let c0: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let c1: [u8; 3] = [0x01, 0x02, 0x03];
        payload.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c0);
        payload.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c1);
        payload.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 3);
        assert_eq!(baseline[0], ColumnValues::Int(7));
        assert_ne!(baseline[1], ColumnValues::Null);
        assert_eq!(
            baseline[2],
            ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        );

        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "fully-sync mixed row diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// Mandatory blocking test (L4c A): the NBCROW fixed-width null-bitmap read
    /// inverted to the sync `PacketBuffer` core. Nine columns force a two-byte
    /// bitmap (`bitmap_len = 2`), so sweeping the refill boundary across every
    /// interior offset lands it INSIDE the multi-byte bitmap (offset 2 splits the
    /// two bitmap bytes). Every split must decode byte-identically to the
    /// single-packet baseline, proving `read_null_bitmap` ensures/refills
    /// mid-bitmap and takes the whole bitmap atomically before decoding columns.
    #[tokio::test]
    async fn nbcrow_bitmap_read_resumes_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let columns: Vec<ColumnMetadata> = (0..9)
            .map(|i| ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: format!("c{i}"),
                multi_part_name: None,
                crypto_metadata: None,
            })
            .collect();

        // Columns 0, 3, 8 are NULL via the bitmap (bit set == NULL); the other
        // six carry an int4 value in column order. bitmap_len = ceil(9/8) = 2.
        let null_cols = [0usize, 3, 8];
        let present: [(usize, i32); 6] = [(1, 11), (2, 22), (4, 44), (5, 55), (6, 66), (7, 77)];
        let mut bitmap = [0u8; 2];
        for &c in &null_cols {
            bitmap[c / 8] |= 1 << (c % 8);
        }

        let mut payload = vec![TokenType::NbcRow as u8];
        payload.extend_from_slice(&bitmap);
        for &(_, v) in &present {
            payload.extend_from_slice(&v.to_le_bytes());
        }

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00; // clear EOM so the buffer reads into the second packet
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 9);
        for &c in &null_cols {
            assert_eq!(baseline[c], ColumnValues::Null, "column {c} should be NULL");
        }
        for &(c, v) in &present {
            assert_eq!(baseline[c], ColumnValues::Int(v), "column {c} value");
        }

        // Sweep the refill boundary across every interior offset. Offset 2 splits
        // the two-byte bitmap; later offsets land between columns and mid-int4.
        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "NBCROW decode diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// Mandatory blocking test (L4c B): a fully-sync NBCROW row mixing every cell
    /// class — a column NULL'd via the bitmap, non-PLP inverted cells (`int4` +
    /// `varchar(64)`, L4a sync step), and a PLP `varbinary(max)` (L4b sync collect)
    /// — decoded through the buffer-owning `PacketReader`. After L4c the whole
    /// NBCROW eager row is sync: bitmap (this layer) + non-PLP (L4a) + PLP (L4b).
    /// The refill boundary is swept across every interior offset, including the
    /// bitmap end, the column transitions, and inside the PLP chunk; every split
    /// must decode byte-identically to the single-packet baseline.
    #[tokio::test]
    async fn fully_sync_nbcrow_mixed_row_is_byte_identical_across_refill_boundary() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        // [int4 present][int4 NULL via bitmap][varchar(64) present][varbinary(max) PLP present]
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "z".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            plp_varbinary_metadata("b", None),
        ];

        // NBCROW: bitmap NULLs column 1; then int4=7, varchar "ab", varbinary(max)
        // PLP two chunks. bitmap_len = ceil(4/8) = 1.
        let mut payload = vec![TokenType::NbcRow as u8, 0b0000_0010];
        payload.extend_from_slice(&7_i32.to_le_bytes());
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        payload.extend_from_slice(&ab);
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
        let c0: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let c1: [u8; 3] = [0x01, 0x02, 0x03];
        payload.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c0);
        payload.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c1);
        payload.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 4);
        assert_eq!(baseline[0], ColumnValues::Int(7));
        assert_eq!(baseline[1], ColumnValues::Null);
        assert_ne!(baseline[2], ColumnValues::Null); // varchar "ab"
        assert_eq!(
            baseline[3],
            ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        );

        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "fully-sync NBCROW row diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// P4a blocking test 1: a fully-synchronous non-PLP ROW decoded through the
    /// production `TdsCore::step_row` driver (`drive_row_over_buffer`) over the
    /// buffer-owning `PacketReader`. Sweeping the refill boundary across every
    /// interior offset (including the token byte, mid-length-prefix, and mid-cell)
    /// must decode byte-identically to the single-packet baseline, proving the
    /// sync core re-drives from an unchanged buffer position on every `NeedBytes`.
    #[tokio::test]
    async fn tdscore_step_row_refill_boundary_swept_all_offsets_is_byte_identical() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        // [int4][varchar(64)][int2] — all non-PLP, all `is_supported`, so every
        // cell is decoded inline by `step_row` with no `AsyncColumn` yield.
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int2,
                type_info: TypeInfo::fixed_len(TdsDataType::Int2).unwrap(),
                column_name: "s".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
        ];

        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&7_i32.to_le_bytes());
        let abc = [0x61u8, 0x62, 0x63]; // "abc"
        payload.extend_from_slice(&(abc.len() as u16).to_le_bytes());
        payload.extend_from_slice(&abc);
        payload.extend_from_slice(&0x1234_i16.to_le_bytes());

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 3);
        assert_eq!(baseline[0], ColumnValues::Int(7));
        assert_ne!(baseline[1], ColumnValues::Null);
        assert_eq!(baseline[2], ColumnValues::SmallInt(0x1234));

        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "TdsCore driver diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// P4a blocking test 2: an NBCROW decoded through the production `step_row`
    /// driver, with nine columns forcing a two-byte null bitmap. Sweeping the
    /// refill boundary lands it INSIDE the bitmap (offset 2 splits the two bitmap
    /// bytes, offset 1 splits token-from-bitmap). This exercises the atomic
    /// row-header step: a `NeedBytes` at the bitmap leaves the token unconsumed,
    /// so the re-drive re-peeks the same token byte — proving the binding
    /// condition holds with zero new resumable state.
    #[tokio::test]
    async fn tdscore_step_row_nbcrow_bitmap_split_across_refill_is_byte_identical() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let columns: Vec<ColumnMetadata> = (0..9)
            .map(|i| ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: format!("c{i}"),
                multi_part_name: None,
                crypto_metadata: None,
            })
            .collect();

        let null_cols = [0usize, 3, 8];
        let present: [(usize, i32); 6] = [(1, 11), (2, 22), (4, 44), (5, 55), (6, 66), (7, 77)];
        let mut bitmap = [0u8; 2];
        for &c in &null_cols {
            bitmap[c / 8] |= 1 << (c % 8);
        }

        let mut payload = vec![TokenType::NbcRow as u8];
        payload.extend_from_slice(&bitmap);
        for &(_, v) in &present {
            payload.extend_from_slice(&v.to_le_bytes());
        }

        async fn decode(read_data: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 9);
        for &c in &null_cols {
            assert_eq!(baseline[c], ColumnValues::Null, "column {c} should be NULL");
        }
        for &(c, v) in &present {
            assert_eq!(baseline[c], ColumnValues::Int(v), "column {c} value");
        }

        for split in 1..payload.len() {
            let got = decode(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "TdsCore NBCROW driver diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// P4a blocking test 3 (Fork A): a ROW mixing an inline sync cell and an eager
    /// PLP `varbinary(max)` cell, decoded through the production `step_row` driver.
    /// The PLP cell is not `is_supported`, so `step_row` yields `AsyncColumn` and
    /// the driver services it via the existing `collect_plp_bytes().await` seam,
    /// then re-drives at the next column. Sweeping the refill boundary (including
    /// inside the PLP chunk stream) must equal both the single-packet baseline and
    /// the async reference oracle, proving the yield/resume seam is byte-identical.
    #[tokio::test]
    async fn tdscore_step_row_eager_plp_yields_and_resumes_at_next_column() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            plp_varbinary_metadata("b", None),
        ];

        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&7_i32.to_le_bytes());
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        payload.extend_from_slice(&ab);
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
        let c0: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let c1: [u8; 3] = [0x01, 0x02, 0x03];
        payload.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c0);
        payload.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c1);
        payload.extend_from_slice(&0u32.to_le_bytes());

        async fn decode_driver(
            read_data: Vec<u8>,
            columns: &[ColumnMetadata],
        ) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        async fn decode_oracle(
            read_data: Vec<u8>,
            columns: &[ColumnMetadata],
        ) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        let baseline = decode_driver(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 3);
        assert_eq!(baseline[0], ColumnValues::Int(7));
        assert_ne!(baseline[1], ColumnValues::Null);
        assert_eq!(
            baseline[2],
            ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        );
        // The driver's yield/resume must match the async reference oracle exactly.
        assert_eq!(
            baseline,
            decode_oracle(one_packet(&payload), &columns).await
        );

        for split in 1..payload.len() {
            let got = decode_driver(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "TdsCore eager-PLP driver diverged when the refill boundary landed at offset {split}"
            );
        }
    }

    /// P4b hardening (AD-2): a STRICT residency-CEILING assertion on the
    /// PRODUCTION driver for an eager-PLP LOB. P4a's own eager-PLP residency
    /// guarantee is asserted here, one layer up, rather than in #198, to keep the
    /// frozen P4a tip stable.
    ///
    /// A single `varbinary(max)` PLP column carrying many small chunks whose total
    /// wire EXCEEDS the 8192 B buffer capacity is decoded through
    /// `drive_row_over_buffer` (the production driver), framed into sub-buffer
    /// packets. The driver services the unbounded column via the `AsyncColumn`
    /// seam chunk-at-a-time, so peak `PacketBuffer` residency must stay bounded
    /// (≲ one buffer/chunk) and never approach the whole-LOB size.
    ///
    /// This is a CEILING, not a "decode still matches" check: if anyone ever
    /// collapses `AsyncColumn` into a collect-whole / `NeedBytes(full-len)` path,
    /// the driver would drive `length` toward the full LOB size and
    /// `peak_length` would reach `>= large.len()`, FAILING the ceiling — robust
    /// even if the buffer capacity later becomes growable.
    #[tokio::test]
    async fn tdscore_step_row_eager_plp_residency_ceiling_on_production_driver() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let columns = vec![plp_varbinary_metadata("b", None)];

        // UNKNOWNLEN PLP value with many small chunks; total wire far exceeds the
        // buffer capacity, so it can never be whole-resident.
        fn plp_value(chunk: usize, count: usize) -> Vec<u8> {
            let mut plp = Vec::new();
            plp.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
            let mut counter = 0u8;
            for _ in 0..count {
                plp.extend_from_slice(&(chunk as u32).to_le_bytes());
                for _ in 0..chunk {
                    plp.push(counter);
                    counter = counter.wrapping_add(1);
                }
            }
            plp.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator
            plp
        }

        // Frame the payload into `piece`-sized packets (EOM only on the last), so
        // each refill exposes at most one small packet.
        fn frame_into(payload: &[u8], piece: usize) -> Vec<u8> {
            let mut wire = Vec::new();
            let mut offset = 0;
            while offset < payload.len() {
                let end = (offset + piece).min(payload.len());
                let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
                let mut packet = builder.append_bytes(&payload[offset..end]).build();
                if end < payload.len() {
                    packet[1] = 0x00; // clear EOM: more packets follow
                }
                wire.extend_from_slice(&packet);
                offset = end;
            }
            wire
        }

        async fn decode_oracle(
            read_data: Vec<u8>,
            columns: &[ColumnMetadata],
        ) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = ParserContext::ColumnMetadata(
                Arc::new(ColMetadataToken {
                    column_count: columns.len() as u16,
                    columns: columns.to_vec(),
                    cek_table: vec![],
                }),
                None,
            );
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = receive_row_into_internal(&mut reader, &registry, &context, &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        // 24 x 500 B chunks = 12000 B of payload; comfortably over the buffer cap.
        let plp = plp_value(500, 24);
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&plp);

        let reference = decode_oracle(frame_into(&payload, 2048), &columns).await;
        assert_ne!(reference[0], ColumnValues::Null);

        // Drive the production driver over 512 B packet frames and capture peak
        // residency from the shared buffer after a successful decode.
        let wire = frame_into(&payload, 512);
        let mut mock = MockNetworkReaderWriter::new(wire, 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await.unwrap();

        // Bind the residency ceiling to the ACTUAL in-test buffer capacity (the
        // 2 x negotiated-packet working buffer), not a magic number.
        let buffer_capacity = reader.row_buffer_mut().working_buffer().len();
        assert!(
            payload.len() > buffer_capacity,
            "residency value ({}) must exceed the actual buffer capacity ({buffer_capacity})",
            payload.len()
        );

        let context = ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: columns.len() as u16,
                columns: columns.to_vec(),
                cek_table: vec![],
            }),
            None,
        );
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DefaultRowWriter::new(columns.len());
        let result = drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
            .await
            .unwrap();
        assert!(matches!(result, RowReadResult::RowWritten));
        let decoded = writer.take_row();
        assert_eq!(
            decoded, reference,
            "eager-PLP LOB must decode byte-identically through the production driver"
        );

        // The load-bearing CEILING: peak resident bytes must stay within one
        // 2 x packet buffer. A collect-whole / NeedBytes(full-len) collapse would
        // drive `length` toward the whole-LOB size across successive strip_header
        // appends: on a growable buffer `peak` would exceed `buffer_capacity`
        // (this assertion fails); on the fixed-cap buffer the forward-progress
        // guard trips and the decode errors (the `.unwrap()` above panics). Either
        // way the footgun regresses this test.
        let peak = reader.row_buffer_mut().peak_length();
        assert!(
            peak <= buffer_capacity,
            "peak residency {peak} exceeded the buffer capacity {buffer_capacity}: \
             the driver held more than one buffer/chunk resident"
        );
        assert!(
            peak < payload.len(),
            "peak residency {peak} reached the whole-LOB size {}: AsyncColumn was \
             collapsed into a collect-whole / NeedBytes(full-len) path, \
             reintroducing whole-LOB residency",
            payload.len()
        );
    }

    /// P4a blocking test 4: the resume-from-pause path. A `RowPauseState` positioned
    /// mid-row is driven both through the production `step_row` driver (with a
    /// `Some` cursor, which skips the header and decodes from `next_column_index`)
    /// and through the `resume_row_into_internal` reference oracle. Sweeping the
    /// refill boundary across the resumed cells must produce byte-identical rows,
    /// proving the unified resume path matches the oracle and keeping that oracle
    /// exercised under `cargo test`.
    #[tokio::test]
    async fn tdscore_resume_from_pause_is_byte_identical_and_matches_oracle() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::{MockNetworkReaderWriter, TestPacketBuilder};
        use crate::message::messages::PacketType;

        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "m".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
        ];

        // Resume at column 1: the wire holds only columns 1 (varchar) and 2 (int4);
        // no token byte and no bitmap, exactly as the transport resume path feeds it.
        let mut payload = Vec::new();
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        payload.extend_from_slice(&ab);
        payload.extend_from_slice(&99_i32.to_le_bytes());

        fn pause_state(columns: &[ColumnMetadata]) -> RowPauseState {
            RowPauseState {
                next_column_index: 1,
                columns: columns.to_vec(),
                nbc_null_bitmap: None,
                decryptor: None,
            }
        }

        async fn decode_driver(
            read_data: Vec<u8>,
            columns: &[ColumnMetadata],
        ) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let registry = GenericTokenParserRegistry::default();
            let context = ParserContext::None(());
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer(
                &mut reader,
                &registry,
                &context,
                Some(pause_state(columns)),
                &mut writer,
            )
            .await
            .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        async fn decode_oracle(
            read_data: Vec<u8>,
            columns: &[ColumnMetadata],
        ) -> Vec<ColumnValues> {
            let mut mock = MockNetworkReaderWriter::new(read_data, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = resume_row_into_internal(&mut reader, pause_state(columns), &mut writer)
                .await
                .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn one_packet(payload: &[u8]) -> Vec<u8> {
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            builder.append_bytes(payload).build()
        }
        fn two_packets(payload: &[u8], split: usize) -> Vec<u8> {
            let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut first = first_builder.append_bytes(&payload[..split]).build();
            first[1] = 0x00;
            let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
            let second = second_builder.append_bytes(&payload[split..]).build();
            [first, second].concat()
        }

        // Resuming at column 1 decodes only columns 1 (varchar) and 2 (int4);
        // column 0 was written before the pause, so the resumed row holds 2 cells.
        let baseline = decode_driver(one_packet(&payload), &columns).await;
        assert_eq!(baseline.len(), 2);
        assert_ne!(baseline[0], ColumnValues::Null);
        assert_eq!(baseline[1], ColumnValues::Int(99));
        assert_eq!(
            baseline,
            decode_oracle(one_packet(&payload), &columns).await
        );

        for split in 1..payload.len() {
            let got = decode_driver(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, baseline,
                "TdsCore resume driver diverged when the refill boundary landed at offset {split}"
            );
            let oracle = decode_oracle(two_packets(&payload, split), &columns).await;
            assert_eq!(
                got, oracle,
                "resume driver vs oracle diverged at offset {split}"
            );
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

    // ---- P4b: non-row token driver refill-boundary differential tests ----
    //
    // Each test feeds a canned token byte stream through the production
    // `drive_token_over_buffer` (sync `TdsCore::step_token` body + the single
    // refill `.await`) and the `#[cfg(any(test, fuzzing))]` `receive_token_internal`
    // async oracle, sweeping the refill boundary across every interior byte. The
    // driver result must be byte-identical to the oracle at every split, proving
    // the sync inversion parses the bounded category-(a) tokens (DONE/DONEINPROC/
    // DONEPROC, ERROR, INFO, ORDER, ENVCHANGE) identically and that the
    // `AsyncToken` seam hands the value-carrying tokens (COLMETADATA) to the async
    // parser across a refill without corruption.

    fn token_one_packet(payload: &[u8]) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_bytes(payload).build()
    }

    fn token_two_packets(payload: &[u8], split: usize) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let mut first = first_builder.append_bytes(&payload[..split]).build();
        first[1] = 0x00; // clear EOM: more packets follow
        let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let second = second_builder.append_bytes(&payload[split..]).build();
        [first, second].concat()
    }

    async fn decode_token_via_driver(wire: Vec<u8>, context: &ParserContext) -> Tokens {
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::MockNetworkReaderWriter;
        let mut mock = MockNetworkReaderWriter::new(wire, 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await.unwrap();
        let registry = GenericTokenParserRegistry::default();
        drive_token_over_buffer(&mut reader, &registry, context)
            .await
            .unwrap()
    }

    async fn decode_token_via_oracle(wire: Vec<u8>, context: &ParserContext) -> Tokens {
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::MockNetworkReaderWriter;
        let mut mock = MockNetworkReaderWriter::new(wire, 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await.unwrap();
        let registry = GenericTokenParserRegistry::default();
        receive_token_internal(&mut reader, &registry, context)
            .await
            .unwrap()
    }

    /// Drives `token_bytes` through the production driver and the async oracle at
    /// the whole-token baseline and at every interior split, asserting the driver
    /// is byte-identical to the oracle (and both to the canonical decode) at each.
    async fn assert_token_matches_oracle_at_every_split(
        token_bytes: &[u8],
        context: &ParserContext,
    ) {
        let canonical = format!(
            "{:?}",
            decode_token_via_oracle(token_one_packet(token_bytes), context).await
        );
        assert_eq!(
            canonical,
            format!(
                "{:?}",
                decode_token_via_driver(token_one_packet(token_bytes), context).await
            ),
            "driver whole-token decode diverged from the oracle"
        );
        for split in 1..token_bytes.len() {
            let driver = format!(
                "{:?}",
                decode_token_via_driver(token_two_packets(token_bytes, split), context).await
            );
            let oracle = format!(
                "{:?}",
                decode_token_via_oracle(token_two_packets(token_bytes, split), context).await
            );
            assert_eq!(
                driver, canonical,
                "driver decode at split {split} diverged from canonical"
            );
            assert_eq!(
                oracle, canonical,
                "oracle decode at split {split} diverged from canonical"
            );
        }
    }

    fn done_like_token(token_byte: u8) -> Vec<u8> {
        let mut v = vec![token_byte];
        v.extend_from_slice(&0x0010_u16.to_le_bytes()); // status: DONE_COUNT
        v.extend_from_slice(&0x00C1_u16.to_le_bytes()); // cur_cmd
        v.extend_from_slice(&42_u64.to_le_bytes()); // row_count
        v
    }

    fn ascii_utf16(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len() * 2);
        for b in s.bytes() {
            out.push(b);
            out.push(0);
        }
        out
    }

    /// Category-(a) fixed-body DONE family split across a refill boundary.
    #[tokio::test]
    async fn drive_token_done_family_split_matches_oracle() {
        let context = ParserContext::None(());
        for token_byte in [
            TokenType::Done as u8,
            TokenType::DoneInProc as u8,
            TokenType::DoneProc as u8,
        ] {
            assert_token_matches_oracle_at_every_split(&done_like_token(token_byte), &context)
                .await;
        }
    }

    /// Category-(a) length-prefixed ERROR and INFO split across a refill boundary.
    #[tokio::test]
    async fn drive_token_error_info_split_matches_oracle() {
        let context = ParserContext::None(());
        for token_byte in [TokenType::Error as u8, TokenType::Info as u8] {
            let mut body = Vec::new();
            body.extend_from_slice(&14081_u32.to_le_bytes()); // number
            body.push(1); // state
            body.push(16); // severity
            let message = ascii_utf16("hi there");
            body.extend_from_slice(&((message.len() / 2) as u16).to_le_bytes());
            body.extend_from_slice(&message);
            let server = ascii_utf16("srv");
            body.push((server.len() / 2) as u8);
            body.extend_from_slice(&server);
            body.push(0); // empty proc name
            body.extend_from_slice(&7_u32.to_le_bytes()); // line number

            let mut token = vec![token_byte];
            token.extend_from_slice(&(body.len() as u16).to_le_bytes());
            token.extend_from_slice(&body);
            assert_token_matches_oracle_at_every_split(&token, &context).await;
        }
    }

    /// Category-(a) ORDER split across a refill boundary.
    #[tokio::test]
    async fn drive_token_order_split_matches_oracle() {
        let context = ParserContext::None(());
        let cols = [1_u16, 2, 3];
        let mut body = Vec::new();
        for c in cols {
            body.extend_from_slice(&c.to_le_bytes());
        }
        let mut token = vec![TokenType::Order as u8];
        token.extend_from_slice(&(body.len() as u16).to_le_bytes());
        token.extend_from_slice(&body);
        assert_token_matches_oracle_at_every_split(&token, &context).await;
    }

    /// Category-(a) ENVCHANGE (Database subtype) split across a refill boundary.
    #[tokio::test]
    async fn drive_token_envchange_split_matches_oracle() {
        let context = ParserContext::None(());
        let new_value = ascii_utf16("db_new");
        let old_value = ascii_utf16("db_old");
        let mut body = vec![0x01]; // subtype: Database
        body.push((new_value.len() / 2) as u8);
        body.extend_from_slice(&new_value);
        body.push((old_value.len() / 2) as u8);
        body.extend_from_slice(&old_value);
        let mut token = vec![TokenType::EnvChange as u8];
        token.extend_from_slice(&(body.len() as u16).to_le_bytes());
        token.extend_from_slice(&body);
        assert_token_matches_oracle_at_every_split(&token, &context).await;
    }

    fn single_int_colmetadata_token() -> Vec<u8> {
        let mut token = vec![TokenType::ColMetadata as u8];
        token.extend_from_slice(&1_u16.to_le_bytes()); // column count
        token.extend_from_slice(&0_u32.to_le_bytes()); // user type
        token.extend_from_slice(&0_u16.to_le_bytes()); // flags
        token.push(TdsDataType::Int4 as u8); // data type (no type info)
        let name = ascii_utf16("id");
        token.push((name.len() / 2) as u8);
        token.extend_from_slice(&name);
        token
    }

    /// COLMETADATA is a value-carrying category-(b) token that stays on the
    /// `AsyncToken` seam. Splitting it across a refill boundary proves the seam
    /// hands a token spanning a refill to the async parser byte-identically.
    #[tokio::test]
    async fn drive_token_colmetadata_split_matches_oracle() {
        let context = ParserContext::default();
        assert_token_matches_oracle_at_every_split(&single_int_colmetadata_token(), &context).await;
    }

    /// The seam boundary is new surface: after the driver yields COLMETADATA via
    /// the `AsyncToken` seam, the next step must hand off to the row driver over
    /// the same shared buffer and decode the following ROW.
    #[tokio::test]
    async fn drive_token_colmetadata_then_row_handoff() {
        use crate::datatypes::column_values::ColumnValues;
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::MockNetworkReaderWriter;

        let mut payload = single_int_colmetadata_token();
        payload.push(TokenType::Row as u8);
        payload.extend_from_slice(&7_i32.to_le_bytes());

        let mut mock = MockNetworkReaderWriter::new(token_one_packet(&payload), 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await.unwrap();
        let registry = GenericTokenParserRegistry::default();

        let meta_context = ParserContext::default();
        let token = drive_token_over_buffer(&mut reader, &registry, &meta_context)
            .await
            .unwrap();
        let colmeta = match token {
            Tokens::ColMetadata(token) => token,
            other => panic!("expected ColMetadata from the seam, got {other:?}"),
        };
        assert_eq!(colmeta.column_count, 1);

        let row_context = ParserContext::ColumnMetadata(Arc::new(colmeta), None);
        let mut writer = DefaultRowWriter::new(1);
        let result = drive_row_over_buffer(&mut reader, &registry, &row_context, None, &mut writer)
            .await
            .unwrap();
        assert!(matches!(result, RowReadResult::RowWritten));
        assert_eq!(writer.take_row(), vec![ColumnValues::Int(7)]);
    }

    // ---- L3: blocking sync row driver — differential vs async oracle + residency ----
    //
    // These tests feed ONE wire corpus to BOTH the async production driver
    // (`drive_row_over_buffer`, the oracle) and the new sync
    // `drive_row_over_buffer_blocking`, and assert byte-identical decoded rows and
    // identical underflow behavior. The sync driver reuses the ONE parse body
    // (`TdsCore::step_row`) and the ONE PLP leaf (`plp_collect_step`) verbatim;
    // only the refill edge blocks instead of awaiting, so any divergence would be
    // a bug in the edge, not the parse.

    use crate::datatypes::column_values::ColumnValues;
    use crate::io::blocking_reader::BlockingPacketReader;
    use crate::io::byte_source::BlockingByteSource;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// In-memory [`BlockingByteSource`] corpus feeder: hands back the pre-framed
    /// wire in bounded `chunk`-sized slices so a single logical packet arrives
    /// split across several `receive` calls, exercising the split-header /
    /// coalesced-surplus paths of `assemble_tds_packet_blocking` at every offset.
    /// The shared atomic `cancel` flag models the R1 between-slices cancel-check;
    /// a set flag turns the next `receive` into a cooperative cancellation.
    struct ChunkedBlockingSource {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
        cancel: std::sync::Arc<AtomicBool>,
    }

    impl ChunkedBlockingSource {
        fn new(data: Vec<u8>, chunk: usize) -> Self {
            Self {
                data,
                position: 0,
                chunk: chunk.max(1),
                cancel: std::sync::Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl BlockingByteSource for ChunkedBlockingSource {
        fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(crate::error::Error::ProtocolError(
                    "blocking receive cancelled between slices".to_string(),
                ));
            }
            let remaining = self.data.len() - self.position;
            let to_read = buffer.len().min(self.chunk).min(remaining);
            buffer[..to_read].copy_from_slice(&self.data[self.position..self.position + to_read]);
            self.position += to_read;
            Ok(to_read)
        }
    }

    fn l3_one_packet(payload: &[u8]) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_bytes(payload).build()
    }

    fn l3_two_packets(payload: &[u8], split: usize) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut first_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let mut first = first_builder.append_bytes(&payload[..split]).build();
        first[1] = 0x00; // clear EOM: a second packet follows
        let mut second_builder = TestPacketBuilder::new(PacketType::PreLogin);
        let second = second_builder.append_bytes(&payload[split..]).build();
        [first, second].concat()
    }

    fn l3_frame_into(payload: &[u8], piece: usize) -> Vec<u8> {
        use crate::io::packet_reader::tests::TestPacketBuilder;
        use crate::message::messages::PacketType;
        let mut wire = Vec::new();
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + piece).min(payload.len());
            let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
            let mut packet = builder.append_bytes(&payload[offset..end]).build();
            if end < payload.len() {
                packet[1] = 0x00; // clear EOM: more packets follow
            }
            wire.extend_from_slice(&packet);
            offset = end;
        }
        wire
    }

    fn l3_row_context(columns: &[ColumnMetadata]) -> ParserContext {
        ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: columns.len() as u16,
                columns: columns.to_vec(),
                cek_table: vec![],
            }),
            None,
        )
    }

    /// Decode one row through the ASYNC production driver (the oracle).
    async fn l3_decode_async(
        wire: Vec<u8>,
        columns: &[ColumnMetadata],
    ) -> TdsResult<Vec<ColumnValues>> {
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::io::packet_reader::PacketReader;
        use crate::io::packet_reader::tests::MockNetworkReaderWriter;
        let mut mock = MockNetworkReaderWriter::new(wire, 0);
        let mut reader = PacketReader::new(&mut mock);
        reader.read_tds_packet_for_test().await?;
        let context = l3_row_context(columns);
        let registry = GenericTokenParserRegistry::default();
        let mut writer = DefaultRowWriter::new(columns.len());
        let result =
            drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer).await?;
        assert!(matches!(result, RowReadResult::RowWritten));
        Ok(writer.take_row())
    }

    /// Decode one row through the SYNC blocking driver over a chunked in-memory
    /// source. No priming: the driver's first `step_row` underflows and pulls the
    /// first packet through `refill_row_buffer_blocking`, mirroring the async path.
    fn l3_decode_blocking(
        wire: Vec<u8>,
        columns: &[ColumnMetadata],
        chunk: usize,
    ) -> TdsResult<Vec<ColumnValues>> {
        use crate::datatypes::row_writer::DefaultRowWriter;
        let source = ChunkedBlockingSource::new(wire, chunk);
        let mut reader = BlockingPacketReader::new(source, 4096);
        let context = l3_row_context(columns);
        let mut writer = DefaultRowWriter::new(columns.len());
        let result = drive_row_over_buffer_blocking(&mut reader, &context, None, &mut writer)?;
        assert!(matches!(result, RowReadResult::RowWritten));
        Ok(writer.take_row())
    }

    /// Gate 3 (differential, byte-identical): a mixed row — inline `int4`, inline
    /// `varchar`, and a multi-chunk `varbinary(max)` PLP cell — decoded via BOTH
    /// the async oracle and the blocking driver. The refill boundary is swept
    /// across EVERY payload offset (including inside the PLP chunk stream); at each
    /// split the blocking driver must equal the single-packet baseline and the
    /// async oracle byte-for-byte, proving the sync refill edge preserves the
    /// shared parse body.
    #[tokio::test]
    async fn blocking_driver_matches_async_oracle_across_refill_boundary() {
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0x0409,
            col_flags: 0,
            sort_id: 52,
        };
        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::BigVarChar,
                type_info: TypeInfo::var_len_string(TdsDataType::BigVarChar, 64, Some(collation))
                    .unwrap(),
                column_name: "v".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            plp_varbinary_metadata("b", None),
        ];

        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&7_i32.to_le_bytes());
        let ab = [0x61u8, 0x62]; // "ab"
        payload.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        payload.extend_from_slice(&ab);
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // PLP UNKNOWNLEN
        let c0: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let c1: [u8; 3] = [0x01, 0x02, 0x03];
        payload.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c0);
        payload.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        payload.extend_from_slice(&c1);
        payload.extend_from_slice(&0u32.to_le_bytes()); // PLP terminator

        let baseline = l3_decode_async(l3_one_packet(&payload), &columns)
            .await
            .unwrap();
        assert_eq!(baseline[0], ColumnValues::Int(7));
        assert_ne!(baseline[1], ColumnValues::Null);
        assert_eq!(
            baseline[2],
            ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        );

        // The blocking single-packet decode must equal the async baseline.
        assert_eq!(
            l3_decode_blocking(l3_one_packet(&payload), &columns, 7).unwrap(),
            baseline
        );

        for split in 1..payload.len() {
            let wire = l3_two_packets(&payload, split);
            let async_got = l3_decode_async(wire.clone(), &columns).await.unwrap();
            let blocking_got = l3_decode_blocking(wire, &columns, 5).unwrap();
            assert_eq!(
                async_got, baseline,
                "async oracle diverged at refill boundary offset {split}"
            );
            assert_eq!(
                blocking_got, baseline,
                "blocking driver diverged from oracle at refill boundary offset {split}"
            );
        }
    }

    /// Gate 3 (differential, NBCROW): an NBCROW with nine columns (forcing a
    /// two-byte null bitmap) decoded via BOTH drivers, with the refill boundary
    /// swept across every offset — landing inside the bitmap split (offset 1
    /// token-from-bitmap, offset 2 between the two bitmap bytes). The blocking
    /// driver's bitmap handling must stay byte-identical to the async oracle.
    #[tokio::test]
    async fn blocking_driver_nbcrow_matches_async_oracle_across_refill_boundary() {
        let columns: Vec<ColumnMetadata> = (0..9)
            .map(|i| ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: format!("c{i}"),
                multi_part_name: None,
                crypto_metadata: None,
            })
            .collect();

        let null_cols = [0usize, 3, 8];
        let present: [(usize, i32); 6] = [(1, 11), (2, 22), (4, 44), (5, 55), (6, 66), (7, 77)];
        let mut bitmap = [0u8; 2];
        for &c in &null_cols {
            bitmap[c / 8] |= 1 << (c % 8);
        }

        let mut payload = vec![TokenType::NbcRow as u8];
        payload.extend_from_slice(&bitmap);
        for &(_, v) in &present {
            payload.extend_from_slice(&v.to_le_bytes());
        }

        let baseline = l3_decode_async(l3_one_packet(&payload), &columns)
            .await
            .unwrap();
        for &c in &null_cols {
            assert_eq!(baseline[c], ColumnValues::Null, "column {c} should be NULL");
        }
        for &(c, v) in &present {
            assert_eq!(baseline[c], ColumnValues::Int(v), "column {c} value");
        }
        assert_eq!(
            l3_decode_blocking(l3_one_packet(&payload), &columns, 3).unwrap(),
            baseline
        );

        for split in 1..payload.len() {
            let wire = l3_two_packets(&payload, split);
            let async_got = l3_decode_async(wire.clone(), &columns).await.unwrap();
            let blocking_got = l3_decode_blocking(wire, &columns, 3).unwrap();
            assert_eq!(
                async_got, baseline,
                "async oracle NBCROW diverged at bitmap-split offset {split}"
            );
            assert_eq!(
                blocking_got, baseline,
                "blocking NBCROW driver diverged from oracle at bitmap-split offset {split}"
            );
        }
    }

    /// Gate 3 (differential, truncation/underflow parity): a corpus whose trailing
    /// bytes are cut mid-row so the row decode underflows during refill. BOTH
    /// drivers must fail, and with the IDENTICAL error (same `ConnectionClosed`
    /// message from the shared assembly body), proving the blocking refill edge
    /// reports underflow exactly as the async edge does.
    #[tokio::test]
    async fn blocking_driver_truncation_errors_identically_to_async_oracle() {
        let columns = vec![plp_varbinary_metadata("b", None)];

        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // PLP UNKNOWNLEN
        let chunk: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        payload.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        payload.extend_from_slice(&chunk);
        payload.extend_from_slice(&0u32.to_le_bytes()); // terminator

        // Split mid-PLP so the row decode must refill packet 2, then cut packet 2
        // short so that refill underflows (EOF before the declared packet length).
        let split = payload.len() - 6;
        let mut wire = l3_two_packets(&payload, split);
        wire.truncate(wire.len() - 4);

        let async_err = l3_decode_async(wire.clone(), &columns)
            .await
            .expect_err("async oracle must error on a truncated row");
        let blocking_err = l3_decode_blocking(wire, &columns, 5)
            .expect_err("blocking driver must error on a truncated row");

        assert_eq!(
            format!("{async_err}"),
            format!("{blocking_err}"),
            "blocking driver reported a different error than the async oracle on truncation"
        );
    }

    /// Gate 4 (R-c residency, LOAD-BEARING): a multi-chunk `varbinary(max)` LOB
    /// whose total wire far exceeds `2 x max_packet` (>16384 B) is decoded through
    /// the blocking driver over small packet frames. It asserts BOTH (a)
    /// byte-identical output vs the async oracle AND (b) a bounded residency
    /// ceiling: peak `PacketBuffer` residency stays within one `2 x packet` buffer
    /// and never approaches the whole-LOB size. A blocking path that passed
    /// byte-identity but collect-whole'd the LOB would drive `peak_length` toward
    /// the full LOB and FAIL this ceiling — the assertion is required, not
    /// optional, to protect the L4b bounded-residency guarantee.
    #[tokio::test]
    async fn blocking_driver_multichunk_plp_residency_ceiling() {
        let columns = vec![plp_varbinary_metadata("b", None)];

        fn plp_value(chunk: usize, count: usize) -> Vec<u8> {
            let mut plp = Vec::new();
            plp.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
            let mut counter = 0u8;
            for _ in 0..count {
                plp.extend_from_slice(&(chunk as u32).to_le_bytes());
                for _ in 0..chunk {
                    plp.push(counter);
                    counter = counter.wrapping_add(1);
                }
            }
            plp.extend_from_slice(&0u32.to_le_bytes()); // zero-length terminator
            plp
        }

        // 40 x 500 B chunks = 20000 B of value: > 2 x max_packet (8192) and
        // > 16384, so it can never be whole-resident in the fixed 2 x packet buffer.
        let plp = plp_value(500, 40);
        let mut payload = vec![TokenType::Row as u8];
        payload.extend_from_slice(&plp);

        // Async reference over 2048 B frames.
        let reference = l3_decode_async(l3_frame_into(&payload, 2048), &columns)
            .await
            .unwrap();
        assert_ne!(reference[0], ColumnValues::Null);

        // Blocking driver over 512 B packet frames, source dribbling 256 B/slice.
        let source = ChunkedBlockingSource::new(l3_frame_into(&payload, 512), 256);
        let mut reader = BlockingPacketReader::new(source, 4096);

        // The residency ceiling is bound to the ACTUAL 2 x packet buffer capacity,
        // allocated at construction, not a magic number.
        let buffer_capacity = reader.row_buffer_mut().working_buffer().len();
        assert!(
            payload.len() > buffer_capacity,
            "residency value ({}) must exceed the buffer capacity ({buffer_capacity})",
            payload.len()
        );
        assert!(
            payload.len() > 16384,
            "the LOB must exceed 2 x max_packet (>16384 B) to prove bounded residency"
        );

        let context = l3_row_context(&columns);
        let mut writer = crate::datatypes::row_writer::DefaultRowWriter::new(columns.len());
        let result =
            drive_row_over_buffer_blocking(&mut reader, &context, None, &mut writer).unwrap();
        assert!(matches!(result, RowReadResult::RowWritten));
        assert_eq!(
            writer.take_row(),
            reference,
            "blocking multi-chunk PLP LOB must decode byte-identically to the async oracle"
        );

        let peak = reader.row_buffer_mut().peak_length();
        assert!(
            peak <= buffer_capacity,
            "peak residency {peak} exceeded the buffer capacity {buffer_capacity}: \
             the blocking driver held more than one buffer/chunk resident"
        );
        assert!(
            peak < payload.len(),
            "peak residency {peak} reached the whole-LOB size {}: the blocking PLP path \
             collect-whole'd the LOB, regressing bounded residency",
            payload.len()
        );
    }

    /// Gate 3 (differential, non-row TOKEN boundary): the row driver is handed a
    /// wire that opens with a bounded DONE token instead of a row. `step_row`
    /// consumes the token byte and returns `RowStep::Token`, so BOTH drivers must
    /// resolve it through their header-token seam (`resolve_header_token` vs
    /// `resolve_header_token_blocking`) and exit with an identical
    /// `RowReadResult::Token(Tokens::Done(..))`. The refill boundary is swept over
    /// every offset so the blocking seam's `body_len`-underflow -> refill loop is
    /// exercised at each interior split.
    #[tokio::test]
    async fn blocking_driver_done_token_boundary_matches_async_oracle() {
        use crate::datatypes::row_writer::DefaultRowWriter;

        let columns = vec![ColumnMetadata {
            user_type: 0,
            flags: 0,
            data_type: TdsDataType::Int4,
            type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
            column_name: "n".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        }];
        let payload = done_like_token(TokenType::Done as u8);

        fn done_debug(result: RowReadResult) -> String {
            match result {
                RowReadResult::Token(token) => {
                    assert!(
                        matches!(token, Tokens::Done(_)),
                        "row driver exited on a non-DONE token: {token:?}"
                    );
                    format!("{token:?}")
                }
                _ => panic!("expected RowReadResult::Token(Done) from the row driver"),
            }
        }

        async fn drive_async(wire: Vec<u8>, columns: &[ColumnMetadata]) -> RowReadResult {
            use crate::io::packet_reader::PacketReader;
            use crate::io::packet_reader::tests::MockNetworkReaderWriter;
            let mut mock = MockNetworkReaderWriter::new(wire, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = l3_row_context(columns);
            let registry = GenericTokenParserRegistry::default();
            let mut writer = DefaultRowWriter::new(columns.len());
            drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
                .await
                .unwrap()
        }

        fn drive_blocking(
            wire: Vec<u8>,
            columns: &[ColumnMetadata],
            chunk: usize,
        ) -> RowReadResult {
            let source = ChunkedBlockingSource::new(wire, chunk);
            let mut reader = BlockingPacketReader::new(source, 4096);
            let context = l3_row_context(columns);
            let mut writer = DefaultRowWriter::new(columns.len());
            drive_row_over_buffer_blocking(&mut reader, &context, None, &mut writer).unwrap()
        }

        let expected = done_debug(drive_async(l3_one_packet(&payload), &columns).await);
        assert_eq!(
            done_debug(drive_blocking(l3_one_packet(&payload), &columns, 3)),
            expected,
            "blocking DONE-token exit diverged from the async oracle at the baseline"
        );

        for split in 1..payload.len() {
            let async_exit =
                done_debug(drive_async(l3_two_packets(&payload, split), &columns).await);
            let blocking_exit =
                done_debug(drive_blocking(l3_two_packets(&payload, split), &columns, 3));
            assert_eq!(
                async_exit, expected,
                "async DONE exit diverged at split {split}"
            );
            assert_eq!(
                blocking_exit, expected,
                "blocking DONE exit diverged at split {split}"
            );
        }
    }

    /// Gate 3 (differential, RowPaused yield + eager-PLP resume): proves the
    /// blocking driver's pause *and* resume edges match the async oracle. Phase A
    /// drives a full `[int4, varbinary(max)]` row with a writer that pauses after
    /// the inline column; BOTH drivers must take the shared `RowStep::RowPaused`
    /// arm and yield `RowReadResult::RowPaused` at the same `next_column_index`.
    /// Phase B resumes a hand-built cursor positioned at the PLP column, feeding
    /// only the resumed cell bytes, and asserts the eager multi-chunk PLP decode
    /// is byte-identical between the async oracle and the blocking driver across
    /// every refill split — covering the yield/resume cycle the plain
    /// `RowWritten` tests never reach.
    #[tokio::test]
    async fn blocking_driver_rowpaused_yield_and_plp_resume_match_async_oracle() {
        use crate::datatypes::row_writer::DefaultRowWriter;

        let columns = vec![
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::Int4,
                type_info: TypeInfo::fixed_len(TdsDataType::Int4).unwrap(),
                column_name: "n".to_string(),
                multi_part_name: None,
                crypto_metadata: None,
            },
            plp_varbinary_metadata("b", None),
        ];

        // ---- Phase A: pause-after-inline yields RowPaused in both drivers ----
        let mut row = vec![TokenType::Row as u8];
        row.extend_from_slice(&7_i32.to_le_bytes()); // col0 int4
        row.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // col1 PLP (never consumed)
        row.extend_from_slice(&2u32.to_le_bytes());
        row.extend_from_slice(&[0xAA, 0xBB]);
        row.extend_from_slice(&0u32.to_le_bytes());

        let paused_index_async = {
            use crate::io::packet_reader::PacketReader;
            use crate::io::packet_reader::tests::MockNetworkReaderWriter;
            let mut mock = MockNetworkReaderWriter::new(l3_one_packet(&row), 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let context = l3_row_context(&columns);
            let registry = GenericTokenParserRegistry::default();
            let mut writer = PauseAtColumnWriter { pause_at: 0 };
            match drive_row_over_buffer(&mut reader, &registry, &context, None, &mut writer)
                .await
                .unwrap()
            {
                RowReadResult::RowPaused(state) => state.next_column_index,
                _ => panic!("async oracle did not yield RowPaused after the inline column"),
            }
        };

        let paused_index_blocking = {
            let source = ChunkedBlockingSource::new(l3_one_packet(&row), 4);
            let mut reader = BlockingPacketReader::new(source, 4096);
            let context = l3_row_context(&columns);
            let mut writer = PauseAtColumnWriter { pause_at: 0 };
            match drive_row_over_buffer_blocking(&mut reader, &context, None, &mut writer).unwrap()
            {
                RowReadResult::RowPaused(state) => state.next_column_index,
                _ => panic!("blocking driver did not yield RowPaused after the inline column"),
            }
        };
        assert_eq!(paused_index_async, 1, "pause must land at the PLP column");
        assert_eq!(
            paused_index_blocking, paused_index_async,
            "blocking pause column diverged from the async oracle"
        );

        // ---- Phase B: resume the PLP cursor; eager multi-chunk PLP byte-identity ----
        // The resumed wire holds ONLY the PLP cell (no token byte, no bitmap),
        // exactly as the transport resume path feeds a `RowPauseState` cursor.
        let mut cell = Vec::new();
        cell.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFE_u64.to_le_bytes()); // UNKNOWNLEN
        let c0: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        let c1: [u8; 3] = [0x01, 0x02, 0x03];
        cell.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        cell.extend_from_slice(&c0);
        cell.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        cell.extend_from_slice(&c1);
        cell.extend_from_slice(&0u32.to_le_bytes()); // terminator

        fn plp_pause_state(columns: &[ColumnMetadata]) -> RowPauseState {
            RowPauseState {
                next_column_index: 1,
                columns: columns.to_vec(),
                nbc_null_bitmap: None,
                decryptor: None,
            }
        }

        async fn resume_async(wire: Vec<u8>, columns: &[ColumnMetadata]) -> Vec<ColumnValues> {
            use crate::io::packet_reader::PacketReader;
            use crate::io::packet_reader::tests::MockNetworkReaderWriter;
            let mut mock = MockNetworkReaderWriter::new(wire, 0);
            let mut reader = PacketReader::new(&mut mock);
            reader.read_tds_packet_for_test().await.unwrap();
            let registry = GenericTokenParserRegistry::default();
            let context = ParserContext::None(());
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer(
                &mut reader,
                &registry,
                &context,
                Some(plp_pause_state(columns)),
                &mut writer,
            )
            .await
            .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        fn resume_blocking(
            wire: Vec<u8>,
            columns: &[ColumnMetadata],
            chunk: usize,
        ) -> Vec<ColumnValues> {
            let source = ChunkedBlockingSource::new(wire, chunk);
            let mut reader = BlockingPacketReader::new(source, 4096);
            let context = ParserContext::None(());
            let mut writer = DefaultRowWriter::new(columns.len());
            let result = drive_row_over_buffer_blocking(
                &mut reader,
                &context,
                Some(plp_pause_state(columns)),
                &mut writer,
            )
            .unwrap();
            assert!(matches!(result, RowReadResult::RowWritten));
            writer.take_row()
        }

        let baseline = resume_async(l3_one_packet(&cell), &columns).await;
        assert_eq!(baseline.len(), 1);
        assert_eq!(
            baseline[0],
            ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03])
        );
        assert_eq!(
            resume_blocking(l3_one_packet(&cell), &columns, 5),
            baseline,
            "blocking eager-PLP resume diverged from the async oracle at the baseline"
        );

        for split in 1..cell.len() {
            let async_row = resume_async(l3_two_packets(&cell, split), &columns).await;
            let blocking_row = resume_blocking(l3_two_packets(&cell, split), &columns, 5);
            assert_eq!(
                async_row, baseline,
                "async resume diverged at split {split}"
            );
            assert_eq!(
                blocking_row, baseline,
                "blocking resume diverged at split {split}"
            );
        }
    }
}
