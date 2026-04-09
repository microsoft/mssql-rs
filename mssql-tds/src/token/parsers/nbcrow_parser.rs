// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{io::Error, vec};

use async_trait::async_trait;
use tracing::trace;

use super::super::tokens::{RowToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    datatypes::{column_values::ColumnValues, decoder::SqlTypeDecode},
    io::token_stream::ParserContext,
};

pub(crate) struct NbcRowTokenParser<T>
where
    T: SqlTypeDecode,
{
    // fields omitted
    decoder: T,
}

impl<T: SqlTypeDecode + Default> Default for NbcRowTokenParser<T> {
    fn default() -> Self {
        Self {
            decoder: T::default(),
        }
    }
}

fn is_null_value_in_column(null_bitmap: &[u8], index: usize) -> bool {
    let byte_index: usize = index / 8;
    let bit_index = index % 8;
    (null_bitmap[byte_index] & (1 << bit_index)) != 0
}

#[async_trait]
impl<T: SqlTypeDecode + Sync, P: TdsPacketReader + Send + Sync> TokenParser<P>
    for NbcRowTokenParser<T>
{
    async fn parse(&self, reader: &mut P, context: &ParserContext) -> TdsResult<Tokens> {
        let column_metadata_token = match context {
            ParserContext::ColumnMetadata(metadata) => {
                trace!("Metadata during Row Parsing: {:?}", metadata);
                metadata
            }
            _ => {
                return Err(crate::error::Error::from(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Expected ColumnMetadata in context",
                )));
            }
        };

        let all_metadata = &column_metadata_token.columns;
        let mut all_values: Vec<ColumnValues> =
            Vec::with_capacity(column_metadata_token.column_count as usize);
        let col_count = all_metadata.len();

        let bitmap_length = col_count.div_ceil(8);
        let mut bitmap: Vec<u8> = vec![0; bitmap_length as usize];
        reader.read_bytes(bitmap.as_mut_slice()).await?;
        // let mut index = 0;

        for (index, metadata) in all_metadata.iter().enumerate() {
            trace!("Metadata: {:?}", metadata);
            let is_null = is_null_value_in_column(&bitmap, index);

            if is_null {
                all_values.push(ColumnValues::Null);
            } else {
                let column_value = self.decoder.decode(reader, metadata).await?;
                all_values.push(column_value);
            }
        }
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
            Ok(ColumnValues::Int(99))
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

    // --- is_null_value_in_column tests ---

    #[test]
    fn test_null_bitmap_first_bit_set() {
        assert!(is_null_value_in_column(&[0b0000_0001], 0));
    }

    #[test]
    fn test_null_bitmap_first_bit_unset() {
        assert!(!is_null_value_in_column(&[0b0000_0000], 0));
    }

    #[test]
    fn test_null_bitmap_various_positions() {
        let bitmap = [0b1010_0101u8]; // bits 0,2,5,7 set
        assert!(is_null_value_in_column(&bitmap, 0));
        assert!(!is_null_value_in_column(&bitmap, 1));
        assert!(is_null_value_in_column(&bitmap, 2));
        assert!(!is_null_value_in_column(&bitmap, 3));
        assert!(!is_null_value_in_column(&bitmap, 4));
        assert!(is_null_value_in_column(&bitmap, 5));
        assert!(!is_null_value_in_column(&bitmap, 6));
        assert!(is_null_value_in_column(&bitmap, 7));
    }

    #[test]
    fn test_null_bitmap_multi_byte() {
        let bitmap = [0x00, 0xFF]; // second byte: all null
        assert!(!is_null_value_in_column(&bitmap, 0));
        assert!(!is_null_value_in_column(&bitmap, 7));
        assert!(is_null_value_in_column(&bitmap, 8));
        assert!(is_null_value_in_column(&bitmap, 15));
    }

    // --- NbcRowTokenParser tests ---

    #[tokio::test]
    async fn test_parse_no_metadata_context() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
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
    async fn test_parse_all_null() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
        // 3 columns, bitmap = 0b0000_0111 (all 3 null)
        let mut reader = MockReader::new(vec![0b0000_0111]);
        let context = make_context(vec![
            make_int_column("a"),
            make_int_column("b"),
            make_int_column("c"),
        ]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => {
                assert_eq!(row.all_values.len(), 3);
                assert!(row.all_values.iter().all(|v| *v == ColumnValues::Null));
            }
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_no_nulls() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
        // 2 columns, bitmap = 0x00 (no nulls)
        let mut reader = MockReader::new(vec![0x00]);
        let context = make_context(vec![make_int_column("a"), make_int_column("b")]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => {
                assert_eq!(row.all_values.len(), 2);
                assert!(row.all_values.iter().all(|v| *v == ColumnValues::Int(99)));
            }
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_mixed_nulls() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
        // 3 columns, bitmap = 0b0000_0010 (column 1 is null, 0 and 2 are not)
        let mut reader = MockReader::new(vec![0b0000_0010]);
        let context = make_context(vec![
            make_int_column("a"),
            make_int_column("b"),
            make_int_column("c"),
        ]);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => {
                assert_eq!(row.all_values.len(), 3);
                assert_eq!(row.all_values[0], ColumnValues::Int(99));
                assert_eq!(row.all_values[1], ColumnValues::Null);
                assert_eq!(row.all_values[2], ColumnValues::Int(99));
            }
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_columns_spanning_two_bitmap_bytes() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
        // 9 columns, need 2 bitmap bytes
        // byte 0 = 0xFF (columns 0-7 all null), byte 1 = 0x00 (column 8 not null)
        let mut reader = MockReader::new(vec![0xFF, 0x00]);
        let columns: Vec<ColumnMetadata> =
            (0..9).map(|i| make_int_column(&format!("c{i}"))).collect();
        let context = make_context(columns);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::Row(row) => {
                assert_eq!(row.all_values.len(), 9);
                for v in &row.all_values[..8] {
                    assert_eq!(*v, ColumnValues::Null);
                }
                assert_eq!(row.all_values[8], ColumnValues::Int(99));
            }
            _ => panic!("Expected Row token"),
        }
    }

    #[tokio::test]
    async fn test_parse_empty_reader_errors() {
        let parser = NbcRowTokenParser::<MockDecoder>::default();
        // 1 column needs 1 bitmap byte, but reader is empty
        let mut reader = MockReader::new(vec![]);
        let context = make_context(vec![make_int_column("a")]);

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
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
        let parser = NbcRowTokenParser::<FailingDecoder>::default();
        // 1 column, bitmap = 0x00 (not null, so decoder will be called)
        let mut reader = MockReader::new(vec![0x00]);
        let context = make_context(vec![make_int_column("a")]);

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("decode failure"));
    }
}
