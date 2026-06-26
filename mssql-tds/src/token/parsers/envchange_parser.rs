// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Error;

use async_trait::async_trait;
use byteorder::{ByteOrder, LittleEndian};
use tracing::event;

use super::super::tokens::{EnvChangeToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    io::token_stream::ParserContext,
    message::login::RoutingInfo,
    token::tokens::{EnvChangeContainer, EnvChangeTokenSubType, SqlCollation, TokenType},
};

#[cfg(not(fuzzing))]
#[derive(Default)]
pub(crate) struct EnvChangeTokenParser {
    // fields omitted
}

#[cfg(fuzzing)]
#[derive(Debug, Default)]
pub struct EnvChangeTokenParser {
    // fields omitted
}

#[async_trait]
impl<T> TokenParser<T> for EnvChangeTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let _token_length = reader.read_uint16().await?;
        let sub_type = reader.read_byte().await?;
        let token_sub_type: EnvChangeTokenSubType = sub_type.try_into()?;
        event!(
            tracing::Level::DEBUG,
            "Parsing {:?} token with type and subtype {:?}",
            TokenType::EnvChange,
            token_sub_type
        );

        let token_value_change: EnvChangeContainer = match token_sub_type {
            EnvChangeTokenSubType::Database => {
                let new_value = reader.read_varchar_u8_length().await?;
                let old_value = reader.read_varchar_u8_length().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::Language => {
                let new_value = reader.read_varchar_u8_length().await?;
                let old_value = reader.read_varchar_u8_length().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::CharacterSet => {
                let new_value = reader.read_varchar_u8_length().await?;
                let old_value = reader.read_varchar_u8_length().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::PacketSize => {
                let new_value_string = reader.read_varchar_u8_length().await?;
                let old_value_string = reader.read_varchar_u8_length().await?;
                let new_value = new_value_string.parse::<u32>().map_err(|_| {
                    Error::new(std::io::ErrorKind::InvalidData, "Invalid new packet size")
                })?;
                let old_value = old_value_string.parse::<u32>().map_err(|_| {
                    Error::new(std::io::ErrorKind::InvalidData, "Invalid old packet size")
                })?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::UnicodeDataSortingLocalId => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "UnicodeDataSortingLocalId".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::UnicodeDataSortingComparisonFlags => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "UnicodeDataSortingComparisonFlags".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::SqlCollation => {
                let new_bytes = reader.read_u8_varbyte().await?;
                let old_bytes = reader.read_u8_varbyte().await?;
                let old_collation: Option<SqlCollation> = match old_bytes.len() {
                    5 => old_bytes.as_slice().try_into().ok(),
                    _ => None,
                };

                let new_collation: Option<SqlCollation> = match new_bytes.len() {
                    5 => new_bytes.as_slice().try_into().ok(),
                    _ => None,
                };
                EnvChangeContainer::from((old_collation, new_collation))
            }
            EnvChangeTokenSubType::BeginTransaction
            | EnvChangeTokenSubType::EnlistDtcTransaction => {
                let new_value = reader.read_u8_varbyte().await?;
                let new_descriptor = match new_value.len() {
                    8 => Ok(LittleEndian::read_u64(&new_value)),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid new transaction descriptor",
                    )),
                }?;
                let old_value = reader.read_u8_varbyte().await?;
                let old_descriptor = match old_value.len() {
                    0 => Ok(0u64),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid old transaction descriptor",
                    )),
                }?;
                EnvChangeContainer::from((old_descriptor, new_descriptor))
            }
            EnvChangeTokenSubType::CommitTransaction => {
                let new_value = reader.read_u8_varbyte().await?;
                let new_descriptor: u64 = match new_value.len() {
                    0 => Ok(0u64),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid new transaction descriptor",
                    )),
                }?;
                let old_value = reader.read_u8_varbyte().await?;
                let old_descriptor = match old_value.len() {
                    8 => Ok(LittleEndian::read_u64(&old_value)),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid old transaction descriptor",
                    )),
                }?;
                EnvChangeContainer::from((old_descriptor, new_descriptor))
            }
            EnvChangeTokenSubType::RollbackTransaction => {
                let new_value = reader.read_u8_varbyte().await?;
                let new_descriptor: u64 = match new_value.len() {
                    0 => Ok(0u64),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid new transaction descriptor",
                    )),
                }?;
                let old_value = reader.read_u8_varbyte().await?;
                let old_descriptor = match old_value.len() {
                    8 => Ok(LittleEndian::read_u64(&old_value)),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid old transaction descriptor",
                    )),
                }?;
                EnvChangeContainer::from((old_descriptor, new_descriptor))
            }
            EnvChangeTokenSubType::DefectTransaction => {
                let new_value = reader.read_u8_varbyte().await?;
                let new_descriptor = match new_value.len() {
                    8 => Ok(LittleEndian::read_u64(&new_value)),
                    _ => Err(Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid new transaction descriptor",
                    )),
                }?;
                let old_value = reader.read_u8_varbyte().await?;
                let old_descriptor = match old_value.len() {
                    0 => Ok(0u64),
                    _ => Err(crate::error::Error::ProtocolError(
                        "Invalid old transaction descriptor".to_string(),
                    )),
                }?;
                EnvChangeContainer::from((old_descriptor, new_descriptor))
            }
            EnvChangeTokenSubType::DatabaseMirroringPartner => {
                // MS-TDS 2.2.7.9: the new value carries the mirroring/failover
                // partner server name as a B_VARCHAR; the old value is empty.
                let new_value = reader.read_varchar_u8_length().await?;
                let old_value = reader.read_varchar_u8_length().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::PromoteTransaction => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "PromoteTransaction".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::TransactionManagerAddress => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "TransactionManagerAddress".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::TransactionEnded => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "TransactionEnded".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::ResetConnection => {
                let new_value = reader.read_u8_varbyte().await?;
                let old_value = reader.read_u8_varbyte().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
            EnvChangeTokenSubType::UserInstanceName => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "UserInstanceName".to_string(),
                    context: "EnvChange token parsing not yet implemented".to_string(),
                });
            }
            EnvChangeTokenSubType::Routing => {
                let _length = reader.read_uint16().await?;
                let protocol = reader.read_byte().await?;
                let port = reader.read_uint16().await?;
                let server = reader.read_varchar_u16_length().await?;
                let routing_info = Some(RoutingInfo {
                    protocol,
                    port,
                    server: server.unwrap_or_default(),
                });

                let mut old_routing_info: Option<RoutingInfo> = None;

                let old_length = reader.read_uint16().await?;
                if old_length > 0 {
                    let old_protocol = reader.read_byte().await?;
                    let old_port = reader.read_uint16().await?;
                    let old_server = reader.read_varchar_u16_length().await?;

                    old_routing_info = Some(RoutingInfo {
                        protocol: old_protocol,
                        port: old_port,
                        server: old_server.unwrap_or_default(),
                    });
                }
                EnvChangeContainer::from((old_routing_info, routing_info))
            }
            EnvChangeTokenSubType::Unknown(_value) => {
                // For unknown environment change subtypes, try to read as generic string change
                let new_value = reader.read_varchar_u8_length().await?;
                let old_value = reader.read_varchar_u8_length().await?;
                EnvChangeContainer::from((old_value, new_value))
            }
        };
        Ok(Tokens::from(EnvChangeToken {
            sub_type: token_sub_type,
            change_type: token_value_change,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::test_utils::MockReader;
    use super::*;
    use crate::token::tokens::{EnvChangeContainer, EnvChangeTokenSubType};
    use byteorder::{ByteOrder, LittleEndian};

    /// Helper to build token data with token length, subtype, and custom body
    fn build_envchange_token(sub_type: u8, body: Vec<u8>) -> Vec<u8> {
        let mut data = Vec::new();
        // Token length (u16): 1 byte for subtype + body length
        let token_length = 1u16 + body.len() as u16;
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, token_length);
        data.extend_from_slice(&buf);
        // Subtype
        data.push(sub_type);
        // Body
        data.extend_from_slice(&body);
        data
    }

    /// Helper to encode a varchar with u8 length (TDS B_VARCHAR)
    fn encode_varchar_u8(s: &str) -> Vec<u8> {
        let utf16_units: Vec<u16> = s.encode_utf16().collect();
        let mut data = Vec::new();
        data.push(utf16_units.len() as u8); // Length in characters
        for unit in utf16_units {
            data.push((unit & 0xFF) as u8);
            data.push((unit >> 8) as u8);
        }
        data
    }

    /// Helper to encode a varchar with u16 length (TDS US_VARCHAR)
    fn encode_varchar_u16(s: &str) -> Vec<u8> {
        let utf16_units: Vec<u16> = s.encode_utf16().collect();
        let mut data = Vec::new();
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, utf16_units.len() as u16);
        data.extend_from_slice(&buf);
        for unit in utf16_units {
            data.push((unit & 0xFF) as u8);
            data.push((unit >> 8) as u8);
        }
        data
    }

    /// Helper to encode u8 varbyte
    fn encode_u8_varbyte(bytes: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.push(bytes.len() as u8);
        data.extend_from_slice(bytes);
        data
    }

    #[tokio::test]
    async fn test_parse_database_change() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("newdb"));
        body.extend_from_slice(&encode_varchar_u8("olddb"));
        let data = build_envchange_token(EnvChangeTokenSubType::Database.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::Database);
                match token.change_type {
                    EnvChangeContainer::String(value_pair) => {
                        assert_eq!(value_pair.new_value(), "newdb");
                        assert_eq!(value_pair.old_value(), "olddb");
                    }
                    _ => panic!("Expected String container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_language_change() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("English"));
        body.extend_from_slice(&encode_varchar_u8("Spanish"));
        let data = build_envchange_token(EnvChangeTokenSubType::Language.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::Language);
                match token.change_type {
                    EnvChangeContainer::String(value_pair) => {
                        assert_eq!(value_pair.new_value(), "English");
                        assert_eq!(value_pair.old_value(), "Spanish");
                    }
                    _ => panic!("Expected String container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_charset_change() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("UTF8"));
        body.extend_from_slice(&encode_varchar_u8("ASCII"));
        let data = build_envchange_token(EnvChangeTokenSubType::CharacterSet.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::CharacterSet);
                match token.change_type {
                    EnvChangeContainer::String(value_pair) => {
                        assert_eq!(value_pair.new_value(), "UTF8");
                        assert_eq!(value_pair.old_value(), "ASCII");
                    }
                    _ => panic!("Expected String container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_database_mirroring_partner_change() {
        // Sent during login for mirrored/AlwaysOn databases: new value carries the
        // partner server name, old value is empty. See issue microsoft/mssql-python#600.
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("partner.contoso.com"));
        body.extend_from_slice(&encode_varchar_u8(""));
        let data = build_envchange_token(
            EnvChangeTokenSubType::DatabaseMirroringPartner.as_u8(),
            body,
        );

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(
                    token.sub_type,
                    EnvChangeTokenSubType::DatabaseMirroringPartner
                );
                match token.change_type {
                    EnvChangeContainer::String(value_pair) => {
                        assert_eq!(value_pair.new_value(), "partner.contoso.com");
                        assert_eq!(value_pair.old_value(), "");
                    }
                    _ => panic!("Expected String container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_packet_size_change() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("8192"));
        body.extend_from_slice(&encode_varchar_u8("4096"));
        let data = build_envchange_token(EnvChangeTokenSubType::PacketSize.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::PacketSize);
                match token.change_type {
                    EnvChangeContainer::UInt32(value_pair) => {
                        assert_eq!(*value_pair.new_value(), 8192);
                        assert_eq!(*value_pair.old_value(), 4096);
                    }
                    _ => panic!("Expected UInt32 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_packet_size_invalid_new_value() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("invalid"));
        body.extend_from_slice(&encode_varchar_u8("4096"));
        let data = build_envchange_token(EnvChangeTokenSubType::PacketSize.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_packet_size_invalid_old_value() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("8192"));
        body.extend_from_slice(&encode_varchar_u8("bad"));
        let data = build_envchange_token(EnvChangeTokenSubType::PacketSize.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_sql_collation_change() {
        let mut body = Vec::new();
        // Valid 5-byte collation
        let new_collation_bytes = vec![0x09, 0x04, 0x00, 0x00, 0x34];
        body.extend_from_slice(&encode_u8_varbyte(&new_collation_bytes));
        let old_collation_bytes = vec![0x09, 0x04, 0x00, 0x00, 0x33];
        body.extend_from_slice(&encode_u8_varbyte(&old_collation_bytes));
        let data = build_envchange_token(EnvChangeTokenSubType::SqlCollation.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::SqlCollation);
                match token.change_type {
                    EnvChangeContainer::SqlCollation(value_pair) => {
                        assert!(value_pair.new_value().is_some());
                        assert!(value_pair.old_value().is_some());
                        let new = value_pair.new_value().unwrap();
                        let old = value_pair.old_value().unwrap();
                        assert_eq!(new.sort_id, 0x34);
                        assert_eq!(old.sort_id, 0x33);
                    }
                    _ => panic!("Expected SqlCollation container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_sql_collation_invalid_length() {
        let mut body = Vec::new();
        // Invalid collation with wrong length
        let new_collation_bytes = vec![0x09, 0x04, 0x00]; // Only 3 bytes
        body.extend_from_slice(&encode_u8_varbyte(&new_collation_bytes));
        let old_collation_bytes = vec![0x09, 0x04, 0x00, 0x00, 0x33];
        body.extend_from_slice(&encode_u8_varbyte(&old_collation_bytes));
        let data = build_envchange_token(EnvChangeTokenSubType::SqlCollation.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                match token.change_type {
                    EnvChangeContainer::SqlCollation(value_pair) => {
                        // Invalid length should result in None
                        assert!(value_pair.new_value().is_none());
                        assert!(value_pair.old_value().is_some());
                    }
                    _ => panic!("Expected SqlCollation container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_begin_transaction() {
        let mut body = Vec::new();
        // New transaction descriptor (8 bytes)
        let new_descriptor = 12345678u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, new_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        // Old transaction descriptor (0 bytes for begin)
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        let data = build_envchange_token(EnvChangeTokenSubType::BeginTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::BeginTransaction);
                match token.change_type {
                    EnvChangeContainer::UInt64(value_pair) => {
                        assert_eq!(*value_pair.new_value(), new_descriptor);
                        assert_eq!(*value_pair.old_value(), 0);
                    }
                    _ => panic!("Expected UInt64 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_begin_transaction_invalid_new_descriptor() {
        let mut body = Vec::new();
        // Invalid new descriptor (wrong length)
        body.extend_from_slice(&encode_u8_varbyte(&[0x01, 0x02]));
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        let data = build_envchange_token(EnvChangeTokenSubType::BeginTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_begin_transaction_invalid_old_descriptor() {
        let mut body = Vec::new();
        let new_descriptor = 12345678u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, new_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        // Invalid old descriptor (should be 0 bytes, but has data)
        body.extend_from_slice(&encode_u8_varbyte(&[0x01]));
        let data = build_envchange_token(EnvChangeTokenSubType::BeginTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_commit_transaction() {
        let mut body = Vec::new();
        // New transaction descriptor (0 bytes for commit)
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        // Old transaction descriptor (8 bytes)
        let old_descriptor = 87654321u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, old_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        let data = build_envchange_token(EnvChangeTokenSubType::CommitTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::CommitTransaction);
                match token.change_type {
                    EnvChangeContainer::UInt64(value_pair) => {
                        assert_eq!(*value_pair.new_value(), 0);
                        assert_eq!(*value_pair.old_value(), old_descriptor);
                    }
                    _ => panic!("Expected UInt64 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_rollback_transaction() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        let old_descriptor = 99999999u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, old_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        let data = build_envchange_token(EnvChangeTokenSubType::RollbackTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::RollbackTransaction);
                match token.change_type {
                    EnvChangeContainer::UInt64(value_pair) => {
                        assert_eq!(*value_pair.new_value(), 0);
                        assert_eq!(*value_pair.old_value(), old_descriptor);
                    }
                    _ => panic!("Expected UInt64 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_enlist_dtc_transaction() {
        let mut body = Vec::new();
        let new_descriptor = 11223344u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, new_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        let data = build_envchange_token(EnvChangeTokenSubType::EnlistDtcTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::EnlistDtcTransaction);
                match token.change_type {
                    EnvChangeContainer::UInt64(value_pair) => {
                        assert_eq!(*value_pair.new_value(), new_descriptor);
                        assert_eq!(*value_pair.old_value(), 0);
                    }
                    _ => panic!("Expected UInt64 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_defect_transaction() {
        let mut body = Vec::new();
        let new_descriptor = 55667788u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, new_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        body.extend_from_slice(&encode_u8_varbyte(&[]));
        let data = build_envchange_token(EnvChangeTokenSubType::DefectTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::DefectTransaction);
                match token.change_type {
                    EnvChangeContainer::UInt64(value_pair) => {
                        assert_eq!(*value_pair.new_value(), new_descriptor);
                        assert_eq!(*value_pair.old_value(), 0);
                    }
                    _ => panic!("Expected UInt64 container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_defect_transaction_invalid_old_descriptor() {
        let mut body = Vec::new();
        let new_descriptor = 55667788u64;
        let mut buf = [0u8; 8];
        LittleEndian::write_u64(&mut buf, new_descriptor);
        body.extend_from_slice(&encode_u8_varbyte(&buf));
        // Invalid: old descriptor should be empty
        body.extend_from_slice(&encode_u8_varbyte(&[0x01]));
        let data = build_envchange_token(EnvChangeTokenSubType::DefectTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_reset_connection() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_u8_varbyte(&[0x01, 0x02]));
        body.extend_from_slice(&encode_u8_varbyte(&[0x03, 0x04]));
        let data = build_envchange_token(EnvChangeTokenSubType::ResetConnection.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::ResetConnection);
                match token.change_type {
                    EnvChangeContainer::BytesType(value_pair) => {
                        assert_eq!(value_pair.new_value(), &vec![0x01, 0x02]);
                        assert_eq!(value_pair.old_value(), &vec![0x03, 0x04]);
                    }
                    _ => panic!("Expected BytesType container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_routing() {
        let mut body = Vec::new();
        // New routing info
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, 10); // length (protocol + port + server)
        body.extend_from_slice(&buf);
        body.push(0x01); // protocol
        LittleEndian::write_u16(&mut buf, 1433); // port
        body.extend_from_slice(&buf);
        body.extend_from_slice(&encode_varchar_u16("newserver"));

        // Old routing info (empty)
        LittleEndian::write_u16(&mut buf, 0); // length
        body.extend_from_slice(&buf);

        let data = build_envchange_token(EnvChangeTokenSubType::Routing.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::Routing);
                match token.change_type {
                    EnvChangeContainer::RoutingType(value_pair) => {
                        assert!(value_pair.new_value().is_some());
                        let new_routing = value_pair.new_value().as_ref().unwrap();
                        assert_eq!(new_routing.protocol, 0x01);
                        assert_eq!(new_routing.port, 1433);
                        assert_eq!(new_routing.server, "newserver");
                        assert!(value_pair.old_value().is_none());
                    }
                    _ => panic!("Expected RoutingType container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_routing_with_old_value() {
        let mut body = Vec::new();
        // New routing info
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, 10);
        body.extend_from_slice(&buf);
        body.push(0x02);
        LittleEndian::write_u16(&mut buf, 1434);
        body.extend_from_slice(&buf);
        body.extend_from_slice(&encode_varchar_u16("newserver"));

        // Old routing info
        LittleEndian::write_u16(&mut buf, 10);
        body.extend_from_slice(&buf);
        body.push(0x01);
        LittleEndian::write_u16(&mut buf, 1433);
        body.extend_from_slice(&buf);
        body.extend_from_slice(&encode_varchar_u16("oldserver"));

        let data = build_envchange_token(EnvChangeTokenSubType::Routing.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => match token.change_type {
                EnvChangeContainer::RoutingType(value_pair) => {
                    assert!(value_pair.new_value().is_some());
                    let new_routing = value_pair.new_value().as_ref().unwrap();
                    assert_eq!(new_routing.protocol, 0x02);
                    assert_eq!(new_routing.port, 1434);
                    assert_eq!(new_routing.server, "newserver");

                    assert!(value_pair.old_value().is_some());
                    let old_routing = value_pair.old_value().as_ref().unwrap();
                    assert_eq!(old_routing.protocol, 0x01);
                    assert_eq!(old_routing.port, 1433);
                    assert_eq!(old_routing.server, "oldserver");
                }
                _ => panic!("Expected RoutingType container"),
            },
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_unknown_subtype() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8("newval"));
        body.extend_from_slice(&encode_varchar_u8("oldval"));
        let data = build_envchange_token(99, body); // Unknown subtype

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => {
                assert_eq!(token.sub_type, EnvChangeTokenSubType::Unknown(99));
                match token.change_type {
                    EnvChangeContainer::String(value_pair) => {
                        assert_eq!(value_pair.new_value(), "newval");
                        assert_eq!(value_pair.old_value(), "oldval");
                    }
                    _ => panic!("Expected String container"),
                }
            }
            _ => panic!("Expected EnvChange token"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_unicode_data_sorting_local_id() {
        let body = Vec::new();
        let data = build_envchange_token(
            EnvChangeTokenSubType::UnicodeDataSortingLocalId.as_u8(),
            body,
        );

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "UnicodeDataSortingLocalId");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_unicode_data_sorting_comparison_flags() {
        let body = Vec::new();
        let data = build_envchange_token(
            EnvChangeTokenSubType::UnicodeDataSortingComparisonFlags.as_u8(),
            body,
        );

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "UnicodeDataSortingComparisonFlags");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_promote_transaction() {
        let body = Vec::new();
        let data = build_envchange_token(EnvChangeTokenSubType::PromoteTransaction.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "PromoteTransaction");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_transaction_manager_address() {
        let body = Vec::new();
        let data = build_envchange_token(
            EnvChangeTokenSubType::TransactionManagerAddress.as_u8(),
            body,
        );

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "TransactionManagerAddress");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_transaction_ended() {
        let body = Vec::new();
        let data = build_envchange_token(EnvChangeTokenSubType::TransactionEnded.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "TransactionEnded");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_unimplemented_user_instance_name() {
        let body = Vec::new();
        let data = build_envchange_token(EnvChangeTokenSubType::UserInstanceName.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await;

        assert!(result.is_err());
        match result {
            Err(crate::error::Error::UnimplementedFeature { feature, .. }) => {
                assert_eq!(feature, "UserInstanceName");
            }
            _ => panic!("Expected UnimplementedFeature error"),
        }
    }

    #[tokio::test]
    async fn test_parse_empty_string_values() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_varchar_u8(""));
        body.extend_from_slice(&encode_varchar_u8(""));
        let data = build_envchange_token(EnvChangeTokenSubType::Database.as_u8(), body);

        let mut reader = MockReader::new(data);
        let parser = EnvChangeTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::EnvChange(token) => match token.change_type {
                EnvChangeContainer::String(value_pair) => {
                    assert_eq!(value_pair.new_value(), "");
                    assert_eq!(value_pair.old_value(), "");
                }
                _ => panic!("Expected String container"),
            },
            _ => panic!("Expected EnvChange token"),
        }
    }
}
