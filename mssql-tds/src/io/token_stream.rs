// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::decoder::{
    GenericDecoder, PlpColumnStream, decrypt_cipher_value, decrypt_encrypted_column,
};
use crate::datatypes::row_writer::{DefaultRowWriter, RowWriter, write_column_value};
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

fn extract_row_context(context: &ParserContext) -> TdsResult<RowDecodeContext<'_>> {
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
/// Shared by both the initial ROW path and the resume-from-pause path.
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
            let mut bitmap = vec![0u8; bitmap_len];
            reader.read_bytes(&mut bitmap).await?;
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
