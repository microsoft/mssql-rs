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

/// msodbcsql `MAX_ALTERNATE_SERVER_LENGTH_IN_CHAR` (`tds.h`).
const MAX_ALTERNATE_SERVER_CHARS: usize = 1024;
/// [MS-TDS] 2.2.7.9: AlternateDatabase must not exceed 128 characters.
const MAX_ALTERNATE_DATABASE_CHARS: usize = 128;

fn malformed_routing(detail: &str) -> crate::error::Error {
    crate::error::Error::ProtocolError(format!("Malformed Routing ENVCHANGE: {detail}"))
}

/// Splits a non-empty US_VARCHAR of at most `max_chars` UTF-16 code units off
/// the front of `data`.
fn split_us_varchar<'a>(
    data: &'a [u8],
    field: &str,
    max_chars: usize,
) -> TdsResult<(String, &'a [u8])> {
    let (length, rest) = data
        .split_first_chunk::<2>()
        .ok_or_else(|| malformed_routing(&format!("truncated {field} length")))?;
    let chars = usize::from(u16::from_le_bytes(*length));
    if chars == 0 || chars > max_chars {
        return Err(malformed_routing(&format!(
            "{field} length {chars} is outside 1..={max_chars}"
        )));
    }
    let (value, rest) = rest
        .split_at_checked(chars * 2)
        .ok_or_else(|| malformed_routing(&format!("truncated {field}")))?;
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect();
    let value = String::from_utf16(&units)
        .map_err(|_| malformed_routing(&format!("{field} is not valid UTF-16")))?;
    Ok((value, rest))
}

/// msodbcsql applies the same bounds to both routing types (`Parse.cpp`).
fn parse_routing_data(data: &[u8], enhanced: bool) -> TdsResult<RoutingInfo> {
    let (&[protocol, port_low, port_high], rest) = data
        .split_first_chunk::<3>()
        .ok_or_else(|| malformed_routing("truncated protocol and port"))?;
    let (server, rest) = split_us_varchar(rest, "alternate server", MAX_ALTERNATE_SERVER_CHARS)?;
    let (database, rest) = if enhanced {
        let (database, rest) =
            split_us_varchar(rest, "alternate database", MAX_ALTERNATE_DATABASE_CHARS)?;
        (Some(database), rest)
    } else {
        (None, rest)
    };
    if !rest.is_empty() {
        return Err(malformed_routing("trailing routing data"));
    }
    if protocol != 0 {
        return Err(malformed_routing(&format!(
            "protocol {protocol} is not TCP"
        )));
    }
    let port = u16::from_le_bytes([port_low, port_high]);
    if port == 0 {
        return Err(malformed_routing("port 0"));
    }
    Ok(RoutingInfo {
        protocol,
        port,
        server,
        database,
    })
}

#[async_trait]
impl<T> TokenParser<T> for EnvChangeTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let token_length = reader.read_uint16().await?;
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
            EnvChangeTokenSubType::Routing | EnvChangeTokenSubType::EnhancedRouting => {
                if token_length < 5 {
                    return Err(malformed_routing("truncated routing length fields"));
                }
                // Read exactly the declared routing data so a malformed value cannot
                // desynchronize the token stream.
                let routing_data_length = reader.read_uint16().await?;
                // Type, RoutingDataValueLength, RoutingDataValue and the empty OLDVALUE.
                let expected_token_length = 1 + 2 + usize::from(routing_data_length) + 2;
                if usize::from(token_length) != expected_token_length {
                    return Err(malformed_routing(&format!(
                        "token length {token_length} does not match routing data length \
                         {routing_data_length}"
                    )));
                }
                let mut routing_data = vec![0u8; usize::from(routing_data_length)];
                if reader.read_bytes(&mut routing_data).await? != routing_data.len() {
                    return Err(malformed_routing("truncated routing data"));
                }
                if reader.read_uint16().await? != 0 {
                    return Err(malformed_routing("non-empty old value"));
                }
                let routing_info = parse_routing_data(
                    &routing_data,
                    token_sub_type == EnvChangeTokenSubType::EnhancedRouting,
                )?;
                EnvChangeContainer::from((None, Some(routing_info)))
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

    fn routing_data(protocol: u8, port: u16, server: &str, database: Option<&str>) -> Vec<u8> {
        let mut data = vec![protocol];
        data.extend_from_slice(&port.to_le_bytes());
        data.extend_from_slice(&encode_varchar_u16(server));
        if let Some(database) = database {
            data.extend_from_slice(&encode_varchar_u16(database));
        }
        data
    }

    fn routing_body(routing_data: &[u8]) -> Vec<u8> {
        let mut body = (routing_data.len() as u16).to_le_bytes().to_vec();
        body.extend_from_slice(routing_data);
        body.extend_from_slice(&0u16.to_le_bytes());
        body
    }

    /// Parses `data` and reports how many bytes the parser left unread.
    async fn parse_envchange(data: Vec<u8>) -> (TdsResult<Tokens>, usize) {
        let mut reader = MockReader::new(data);
        let result = EnvChangeTokenParser::default()
            .parse(&mut reader, &ParserContext::default())
            .await;
        (result, reader.remaining())
    }

    #[tokio::test]
    async fn test_parse_routing_types() {
        for (sub_type, database) in [
            (EnvChangeTokenSubType::Routing, None),
            (EnvChangeTokenSubType::EnhancedRouting, Some("sales")),
        ] {
            let mut data = build_envchange_token(
                sub_type.as_u8(),
                routing_body(&routing_data(0, 11000, "replica.sql.net", database)),
            );
            data.push(0xFD);

            let (result, remaining) = parse_envchange(data).await;
            assert_eq!(remaining, 1, "must not consume the following DONE");
            let Ok(Tokens::EnvChange(token)) = result else {
                panic!("Expected EnvChange token, got {result:?}");
            };
            assert_eq!(token.sub_type, sub_type);
            let EnvChangeContainer::RoutingType(value_pair) = token.change_type else {
                panic!("Expected RoutingType container");
            };
            assert_eq!(value_pair.old_value(), &None);
            assert_eq!(
                value_pair.new_value().as_deref(),
                Some(&RoutingInfo {
                    protocol: 0,
                    port: 11000,
                    server: "replica.sql.net".to_string(),
                    database: database.map(str::to_string),
                })
            );
        }
    }

    #[tokio::test]
    async fn test_parse_routing_accepts_limit_lengths() {
        let server = "s".repeat(MAX_ALTERNATE_SERVER_CHARS);
        let database = "d".repeat(MAX_ALTERNATE_DATABASE_CHARS);
        for (sub_type, database) in [
            (EnvChangeTokenSubType::Routing, None),
            (
                EnvChangeTokenSubType::EnhancedRouting,
                Some(database.as_str()),
            ),
        ] {
            let data = build_envchange_token(
                sub_type.as_u8(),
                routing_body(&routing_data(0, 1433, &server, database)),
            );
            let (result, remaining) = parse_envchange(data).await;
            assert!(result.is_ok(), "{sub_type:?}: {result:?}");
            assert_eq!(remaining, 0);
        }
    }

    #[tokio::test]
    async fn test_parse_routing_rejects_malformed_payloads() {
        let server_too_long = "s".repeat(MAX_ALTERNATE_SERVER_CHARS + 1);
        for (sub_type, database) in [
            (EnvChangeTokenSubType::Routing, None),
            (EnvChangeTokenSubType::EnhancedRouting, Some("sales")),
        ] {
            let valid = routing_data(0, 11000, "replica.sql.net", database);
            let mut trailing = valid.clone();
            trailing.push(0);
            let mut invalid_utf16 = routing_data(0, 11000, "s", database);
            invalid_utf16[5..7].copy_from_slice(&0xD800u16.to_le_bytes());
            let mut server_overrun = valid.clone();
            server_overrun[3..5].copy_from_slice(&1024u16.to_le_bytes());
            let mut old_value = routing_body(&valid);
            let old_value_length = old_value.len() - 2;
            old_value[old_value_length] = 1;

            let cases = [
                (
                    "protocol 1 is not TCP",
                    routing_body(&routing_data(1, 11000, "replica.sql.net", database)),
                ),
                (
                    "port 0",
                    routing_body(&routing_data(0, 0, "replica.sql.net", database)),
                ),
                (
                    "alternate server length 0",
                    routing_body(&routing_data(0, 11000, "", database)),
                ),
                (
                    "alternate server length 1025",
                    routing_body(&routing_data(0, 11000, &server_too_long, database)),
                ),
                (
                    "alternate server is not valid UTF-16",
                    routing_body(&invalid_utf16),
                ),
                ("trailing routing data", routing_body(&trailing)),
                ("truncated alternate server", routing_body(&server_overrun)),
                (
                    "truncated alternate server length",
                    routing_body(&[0, 1, 0, 1]),
                ),
                ("truncated protocol and port", routing_body(&[0, 1])),
                ("non-empty old value", old_value),
            ];
            for (expected, body) in cases {
                let mut data = build_envchange_token(sub_type.as_u8(), body);
                data.push(0xFD);
                let (result, remaining) = parse_envchange(data).await;
                assert!(
                    matches!(result, Err(crate::error::Error::ProtocolError(ref message)) if message.contains(expected)),
                    "{sub_type:?} / {expected}: {result:?}"
                );
                assert_eq!(remaining, 1, "must not consume the following DONE");
            }
        }
    }

    #[tokio::test]
    async fn test_parse_enhanced_routing_rejects_malformed_database() {
        let valid = routing_data(0, 11000, "replica.sql.net", Some("sales"));
        let database_too_long = "d".repeat(MAX_ALTERNATE_DATABASE_CHARS + 1);
        let cases = [
            (
                "alternate database length 0",
                routing_body(&routing_data(0, 11000, "replica.sql.net", Some(""))),
            ),
            (
                "alternate database length 129",
                routing_body(&routing_data(
                    0,
                    11000,
                    "replica.sql.net",
                    Some(&database_too_long),
                )),
            ),
            (
                "truncated alternate database",
                routing_body(&valid[..valid.len() - 2]),
            ),
            (
                "truncated alternate database length",
                routing_body(&routing_data(0, 11000, "replica.sql.net", None)),
            ),
        ];
        for (expected, body) in cases {
            let data = build_envchange_token(EnvChangeTokenSubType::EnhancedRouting.as_u8(), body);
            match parse_envchange(data).await.0 {
                Err(crate::error::Error::ProtocolError(message)) => {
                    assert!(message.contains(expected), "{expected}: {message}")
                }
                Err(other) => panic!("{expected}: expected a protocol error, got {other:?}"),
                Ok(_) => panic!("{expected}: malformed payload was accepted"),
            }
        }
    }

    #[tokio::test]
    async fn test_parse_routing_rejects_length_mismatches() {
        for (sub_type, database) in [
            (EnvChangeTokenSubType::Routing, None),
            (EnvChangeTokenSubType::EnhancedRouting, Some("sales")),
        ] {
            let valid = build_envchange_token(
                sub_type.as_u8(),
                routing_body(&routing_data(0, 11000, "replica.sql.net", database)),
            );
            for offset in [0, 3] {
                for delta in [-1, 1] {
                    let mut data = valid.clone();
                    let declared = u16::from_le_bytes([data[offset], data[offset + 1]])
                        .checked_add_signed(delta)
                        .unwrap();
                    data[offset..offset + 2].copy_from_slice(&declared.to_le_bytes());
                    let result = parse_envchange(data).await.0;
                    assert!(
                        matches!(result, Err(crate::error::Error::ProtocolError(ref message)) if message.contains("token length")),
                        "{sub_type:?} / offset {offset} / delta {delta}: {result:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn test_parse_routing_rejects_short_length_fields() {
        for sub_type in [
            EnvChangeTokenSubType::Routing,
            EnvChangeTokenSubType::EnhancedRouting,
        ] {
            for token_length in 0u16..5 {
                let mut data = token_length.to_le_bytes().to_vec();
                data.extend_from_slice(&[sub_type.as_u8(), 0xFD]);
                let (result, remaining) = parse_envchange(data).await;
                assert!(
                    matches!(result, Err(crate::error::Error::ProtocolError(_))),
                    "{sub_type:?} / token length {token_length}: {result:?}"
                );
                assert_eq!(remaining, 1, "must not consume the following DONE");
            }
        }
    }

    #[tokio::test]
    async fn test_parse_routing_rejects_truncated_data() {
        for (sub_type, database) in [
            (EnvChangeTokenSubType::Routing, None),
            (EnvChangeTokenSubType::EnhancedRouting, Some("sales")),
        ] {
            let mut data = build_envchange_token(
                sub_type.as_u8(),
                routing_body(&routing_data(0, 11000, "replica.sql.net", database)),
            );
            data.truncate(data.len() - 4);
            let result = parse_envchange(data).await.0;
            assert!(
                matches!(result, Err(crate::error::Error::ProtocolError(ref message)) if message.contains("truncated routing data")),
                "{sub_type:?}: {result:?}"
            );
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
