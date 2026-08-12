// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parser for the COLINFO TDS token (0xA5).
//!
//! COLINFO carries per-column browse-mode information for cursor result sets.
//! The payload is length-prefixed (u16) and can be safely skipped
//! because browse-mode column info is not required for result-set processing.

use async_trait::async_trait;

use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{io::token_stream::ParserContext, token::tokens::Tokens};

/// Reads the COLINFO token's u16 length prefix and skips the body.
#[derive(Default)]
pub(crate) struct ColInfoTokenParser;

#[async_trait]
impl<T> TokenParser<T> for ColInfoTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let length = reader.read_uint16().await? as usize;
        reader.skip_bytes(length).await?;
        Ok(Tokens::ColInfo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::messages::PacketType;
    use crate::test_packet_support::{TestPacketBuilder, create_network_transport_with_data};

    #[tokio::test]
    async fn test_col_info_empty_body() {
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_u16(0);
        let mut reader = create_network_transport_with_data(&builder.build());

        let parser = ColInfoTokenParser;
        let token = parser
            .parse(&mut reader, &ParserContext::default())
            .await
            .unwrap();
        assert!(matches!(token, Tokens::ColInfo));
    }

    #[tokio::test]
    async fn test_col_info_with_body() {
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_u16(4);
        builder.append_bytes(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let mut reader = create_network_transport_with_data(&builder.build());

        let parser = ColInfoTokenParser;
        let token = parser
            .parse(&mut reader, &ParserContext::default())
            .await
            .unwrap();
        assert!(matches!(token, Tokens::ColInfo));
    }
}
