// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # ROW Token Parser
//!
//! Parses ROW tokens (0xD1) which contain actual data rows from a query result set.
//! Each ROW token represents one row of data, with values for all columns defined
//! in the preceding COLMETADATA token.
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                  ROW Token (variable length)                    │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  Column 1 Value  │  Column 2 Value  │  ...  │  Column N Value   │
//! │   (variable)     │   (variable)     │       │   (variable)      │
//! └─────────────────────────────────────────────────────────────────┘
//!      0 ... M         M+1 ... P                  Q ... R
//!
//! Each column value format depends on its data type (from COLMETADATA):
//!
//! Fixed-length types (INT, BIGINT, etc.):
//!   ┌──────────────┐
//!   │  Value bytes │
//!   └──────────────┘
//!
//! Variable-length types (VARCHAR, VARBINARY):
//!   ┌────────┬──────────────┐
//!   │ Length │  Value bytes │
//!   │(1 or 2)│  (N bytes)   │
//!   └────────┴──────────────┘
//!
//! NULL values:
//!   - Fixed-length: No special marker (determined by type info)
//!   - Variable-length: Length = 0xFFFF (2 bytes) or 0xFF (1 byte)
//!
//! LOB types (TEXT, IMAGE, XML):
//!   ┌──────────┬─────────────┬──────────────┐
//!   │ TextPtr  │  Timestamp  │  Value bytes │
//!   │(16 bytes)│  (8 bytes)  │  (variable)  │
//!   └──────────┴─────────────┴──────────────┘
//! ```
//!
//! ## Token Flow Example
//!
//! ```text
//! Query: SELECT Id, Name, Age FROM Users
//!
//! Server response:
//!   1. COLMETADATA ← Defines 3 columns (Id:INT, Name:NVARCHAR, Age:INT)
//!   2. ROW         ← First row:  [1, "Alice", 30]
//!   3. ROW         ← Second row: [2, "Bob", 25]
//!   4. ROW         ← Third row:  [3, "Carol", NULL]
//!   5. DONE        ← End of result set (RowCount=3)
//! ```
//!
//! ## Parsing Dependencies
//!
//! ROW parsing requires the COLMETADATA from context:
//! - Column count (how many values to read)
//! - Data types (how to interpret each value's bytes)
//! - Type info (precision, scale, max length, etc.)
//! - Nullability (whether NULL values are allowed)
//!
//! Without COLMETADATA, ROW tokens cannot be parsed correctly.
//!
//! ## Related Tokens
//!
//! - **COLMETADATA (0x81)**: Must precede ROW tokens, defines structure
//! - **NBCROW (0xD2)**: Null-bitmap compressed row (more efficient for sparse data)
//! - **DONE (0xFD)**: Follows all ROW tokens, indicates completion

use std::io::Error;

use async_trait::async_trait;
use tracing::trace;

use super::super::tokens::{RowToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    datatypes::{column_values::ColumnValues, decoder::SqlTypeDecode},
    io::token_stream::ParserContext,
};

/// Parser for ROW token (0xD1) - contains actual query result data
///
/// This parser requires column metadata from context to correctly
/// decode each column value according to its data type.
pub(crate) struct RowTokenParser<T: SqlTypeDecode> {
    // fields omitted
    decoder: T,
}

impl<T: SqlTypeDecode + Default> Default for RowTokenParser<T> {
    fn default() -> Self {
        Self {
            decoder: T::default(),
        }
    }
}

#[async_trait]
impl<D: SqlTypeDecode + Default + Send + Sync, P: TdsPacketReader + Send + Sync> TokenParser<P>
    for RowTokenParser<D>
{
    async fn parse(&self, reader: &mut P, context: &ParserContext) -> TdsResult<Tokens> {
        // Extract column metadata from parser context
        // This metadata was set when COLMETADATA token was parsed
        let column_metadata_token = match context {
            ParserContext::ColumnMetadata(metadata) => {
                trace!("Metadata during Row Parsing: {:?}", metadata);
                metadata
            }
            _ => {
                // ROW tokens MUST be preceded by COLMETADATA
                // Without metadata, we don't know how to parse the values
                return Err(crate::error::Error::from(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Expected ColumnMetadata in context",
                )));
            }
        };

        // Get metadata for all columns in the result set
        let all_metadata = &column_metadata_token.columns;

        // Pre-allocate vector for column values
        let mut all_values: Vec<ColumnValues> =
            Vec::with_capacity(column_metadata_token.column_count as usize);

        // Parse each column value in order
        // The decoder knows how to read each SQL type based on its metadata
        for metadata in all_metadata {
            trace!("Metadata: {:?}", metadata);

            // Decode the value according to its data type
            // This handles:
            // - NULL values
            // - Fixed vs variable length types
            // - Type-specific encoding (collation, precision, scale, etc.)
            let column_value = self.decoder.decode(reader, metadata).await?;

            all_values.push(column_value);
        }

        // Construct the complete row token with all column values
        Ok(Tokens::from(RowToken::new(all_values)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::datatypes::sqldatatypes::{
        FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant,
    };
    use crate::io::token_stream::ParserContext;
    use crate::query::metadata::ColumnMetadata;
    use crate::token::parsers::common::test_utils::MockReader;
    use crate::token::tokens::ColMetadataToken;

    #[derive(Default)]
    struct MockDecoder;

    #[async_trait]
    impl SqlTypeDecode for MockDecoder {
        async fn decode<T>(
            &self,
            _reader: &mut T,
            _metadata: &ColumnMetadata,
        ) -> TdsResult<ColumnValues>
        where
            T: TdsPacketReader + Send + Sync,
        {
            Ok(ColumnValues::Int(42))
        }
    }

    fn make_int_column(name: &str) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags: 0,
            type_info: TypeInfo {
                tds_type: TdsDataType::Int4,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            data_type: TdsDataType::Int4,
            column_name: name.to_string(),
            multi_part_name: None,
        }
    }

    fn make_context(columns: Vec<ColumnMetadata>) -> ParserContext {
        ParserContext::ColumnMetadata(Arc::new(ColMetadataToken {
            column_count: columns.len() as u16,
            columns,
        }))
    }

    #[tokio::test]
    async fn test_parse_no_metadata_context() {
        let parser = RowTokenParser::<MockDecoder>::default();
        let mut reader = MockReader::new(vec![]);
        let context = ParserContext::None(());
        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Expected ColumnMetadata in context")
        );
    }

    #[tokio::test]
    async fn test_parse_single_column() {
        let parser = RowTokenParser::<MockDecoder>::default();
        let mut reader = MockReader::new(vec![]);
        let context = make_context(vec![make_int_column("id")]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => {
                assert_eq!(row.all_values.len(), 1);
                assert_eq!(row.all_values[0], ColumnValues::Int(42));
            }
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_multiple_columns() {
        let parser = RowTokenParser::<MockDecoder>::default();
        let mut reader = MockReader::new(vec![]);
        let context = make_context(vec![
            make_int_column("a"),
            make_int_column("b"),
            make_int_column("c"),
        ]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => assert_eq!(row.all_values.len(), 3),
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_zero_columns() {
        let parser = RowTokenParser::<MockDecoder>::default();
        let mut reader = MockReader::new(vec![]);
        let context = make_context(vec![]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => assert!(row.all_values.is_empty()),
            _ => panic!("Expected Row token"),
        }
    }

    #[derive(Default)]
    struct FailingDecoder;

    #[async_trait]
    impl SqlTypeDecode for FailingDecoder {
        async fn decode<T>(
            &self,
            _reader: &mut T,
            _metadata: &ColumnMetadata,
        ) -> TdsResult<ColumnValues>
        where
            T: TdsPacketReader + Send + Sync,
        {
            Err(crate::error::Error::ProtocolError(
                "decode failure".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_parse_decoder_error_propagates() {
        let parser = RowTokenParser::<FailingDecoder>::default();
        let mut reader = MockReader::new(vec![]);
        let context = make_context(vec![make_int_column("a")]);

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("decode failure"));
    }
}
