// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::decoder::GenericDecoder;
use crate::datatypes::row_writer::RowWriter;
use crate::io::packet_reader::TdsPacketReader;
use crate::query::metadata::ColumnMetadata;
use crate::token::parsers::TokenParser;
use crate::token::parsers::{
    ColMetadataTokenParser, DoneInProcTokenParser, DoneProcTokenParser, DoneTokenParser,
    EnvChangeTokenParser, ErrorTokenParser, FeatureExtAckTokenParser, FedAuthInfoTokenParser,
    InfoTokenParser, LoginAckTokenParser, NbcRowTokenParser, OrderTokenParser,
    ReturnStatusTokenParser, ReturnValueTokenParser, RowTokenParser, SspiTokenParser,
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
}

#[cfg(fuzzing)]
pub enum RowReadResult {
    RowWritten,
    Token(Tokens),
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

/// `ParserContext` is used to add additional context, which can be leveraged by the token parsers.
/// One of the usecase is passing the metadata for the columns, to the row parser and to the
/// NBC row token parser.
/// The consumer of the TokenStreamReader is supposed to set/reset this context.
/// Incorrectly managing this context, can lead to bad context being used for subsequent operations.
#[derive(Debug)]
#[cfg(not(fuzzing))]
pub(crate) enum ParserContext {
    ColumnMetadata(Arc<ColMetadataToken>),
    None(()),
}

#[derive(Debug)]
#[cfg(fuzzing)]
#[allow(private_interfaces)]
pub enum ParserContext {
    ColumnMetadata(Arc<ColMetadataToken>),
    None(()),
}

impl Default for ParserContext {
    fn default() -> Self {
        ParserContext::None(())
    }
}

fn extract_column_metadata(context: &ParserContext) -> TdsResult<&[ColumnMetadata]> {
    match context {
        ParserContext::ColumnMetadata(metadata) => Ok(&metadata.columns),
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
            let columns = extract_column_metadata(context)?;
            let decoder = GenericDecoder::default();
            for (col, meta) in columns.iter().enumerate() {
                decoder.decode_into(reader, meta, col, writer).await?;
            }
            Ok(RowReadResult::RowWritten)
        }
        TokenType::NbcRow => {
            let columns = extract_column_metadata(context)?;
            let bitmap_len = columns.len().div_ceil(8);
            let mut bitmap = vec![0u8; bitmap_len];
            reader.read_bytes(&mut bitmap).await?;
            let decoder = GenericDecoder::default();
            for (col, meta) in columns.iter().enumerate() {
                if bitmap[col / 8] & (1 << (col % 8)) != 0 {
                    writer.write_null(col);
                } else {
                    decoder.decode_into(reader, meta, col, writer).await?;
                }
            }
            Ok(RowReadResult::RowWritten)
        }
        _ => {
            let token = dispatch_token(reader, registry, token_type, context).await?;
            Ok(RowReadResult::Token(token))
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
            TokenParsers::from(ColMetadataTokenParser::default()),
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
    SspiTokenParser => Sspi
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::tokens::TokenType;
    use std::collections::HashMap;

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
        let unsupported_type = TokenType::TabName; // This token type is not registered in the default registry
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
