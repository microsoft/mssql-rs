// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::vec;

use async_trait::async_trait;

use super::super::tokens::Tokens;
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{io::token_stream::ParserContext, token::tokens::OrderToken};

#[derive(Default)]
pub(crate) struct OrderTokenParser {}

#[async_trait]
impl<T> TokenParser<T> for OrderTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let length = reader.read_uint16().await?;

        let col_count = length / 2;
        let mut columns = vec![];
        for _ in 0..col_count {
            columns.push(reader.read_uint16().await?);
        }
        Ok(Tokens::from(OrderToken {
            _order_columns: columns,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::transport::network_transport::tests::create_network_transport_with_data;
    use crate::io::packet_reader::tests::TestPacketBuilder;
    use crate::message::messages::PacketType;

    #[tokio::test]
    async fn test_parse_order_single_column() {
        // Test parsing ORDER token with single column
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_u16(2); // length: 1 column * 2 bytes
        builder.append_u16(1); // column index 1

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = OrderTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Order(token) => {
                assert_eq!(token._order_columns.len(), 1);
                assert_eq!(token._order_columns[0], 1);
            }
            _ => panic!("Expected Order token"),
        }
    }

    #[tokio::test]
    async fn test_parse_order_multiple_columns() {
        // Test parsing ORDER token with multiple columns
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_u16(6); // length: 3 columns * 2 bytes
        builder.append_u16(1); // column index 1
        builder.append_u16(3); // column index 3
        builder.append_u16(2); // column index 2

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = OrderTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Order(token) => {
                assert_eq!(token._order_columns.len(), 3);
                assert_eq!(token._order_columns[0], 1);
                assert_eq!(token._order_columns[1], 3);
                assert_eq!(token._order_columns[2], 2);
            }
            _ => panic!("Expected Order token"),
        }
    }

    #[tokio::test]
    async fn test_parse_order_empty() {
        // Test parsing ORDER token with no columns
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_u16(0); // length: 0 columns

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = OrderTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Order(token) => {
                assert_eq!(token._order_columns.len(), 0);
            }
            _ => panic!("Expected Order token"),
        }
    }

    #[tokio::test]
    async fn test_parse_order_many_columns() {
        // Test parsing ORDER token with many columns
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        let column_count = 10;
        builder.append_u16(column_count * 2); // length

        for i in 0..column_count {
            builder.append_u16(i);
        }

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = OrderTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Order(token) => {
                assert_eq!(token._order_columns.len(), column_count as usize);
                for (idx, &col) in token._order_columns.iter().enumerate() {
                    assert_eq!(col, idx as u16);
                }
            }
            _ => panic!("Expected Order token"),
        }
    }
}
