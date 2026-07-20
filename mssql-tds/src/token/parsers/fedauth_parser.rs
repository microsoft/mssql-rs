// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{io::Error, vec};

use async_trait::async_trait;
use tracing::debug;

use super::super::fed_auth_info::FedAuthInfoToken;
use super::super::tokens::Tokens;
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{io::token_stream::ParserContext, token::fed_auth_info::FedAuthInfoId};

#[derive(Default)]
pub(crate) struct FedAuthInfoTokenParser {
    // fields omitted
}

impl FedAuthInfoTokenParser {
    const FEDAUTH_OPTIONS_SIZE: u32 = 9;
}

#[async_trait]
impl<T> TokenParser<T> for FedAuthInfoTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        let length = reader.read_int32().await?;

        let options_count = reader.read_uint32().await?;

        // Use checked_sub to prevent integer overflow
        let data_left = length
            .checked_sub(size_of::<u32>() as i32)
            .ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "Invalid FedAuthInfo token length: {length} (would overflow when subtracting header size)"
                ))
            })?;

        // Validate data_left to prevent capacity overflow attacks
        const MAX_TOKEN_DATA_SIZE: i32 = 1024 * 1024; // 1MB reasonable limit
        if !(0..=MAX_TOKEN_DATA_SIZE).contains(&data_left) {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid FedAuthInfo token data size: {data_left} bytes (length: {length}, options_count: {options_count}). Must be between 0 and {MAX_TOKEN_DATA_SIZE} bytes."
            )));
        }

        // Validate that we have enough data for the options_count
        // Each option requires FEDAUTH_OPTIONS_SIZE bytes
        let required_size = options_count
            .checked_mul(Self::FEDAUTH_OPTIONS_SIZE)
            .ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "FedAuthInfo options_count overflow: {options_count} * {} would overflow",
                    Self::FEDAUTH_OPTIONS_SIZE
                ))
            })?;

        if required_size as i32 > data_left {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid FedAuthInfo token: options_count ({options_count}) requires {required_size} bytes, but only {data_left} bytes available"
            )));
        }

        let mut token_data: Vec<u8> = vec![0; data_left as usize];
        reader.read_bytes(&mut token_data[0..]).await?;

        // Validate that token_data has enough space for the option headers
        // This prevents accessing out of bounds if options_count is set but data is insufficient
        if (token_data.len() as u32) < required_size {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid FedAuthInfo token: token data size ({}) is less than required size ({}) for {} options",
                token_data.len(),
                required_size,
                options_count
            )));
        }

        let mut sts_url = String::new();
        let mut spn = String::new();
        for i in 0..options_count {
            let current_options_offset = i * Self::FEDAUTH_OPTIONS_SIZE;
            let option_id = token_data[current_options_offset as usize];

            // Validate slice bounds before conversion
            let length_slice = token_data
                .get((current_options_offset + 1) as usize..(current_options_offset + 5) as usize)
                .ok_or_else(|| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FedAuth option data length out of bounds",
                    )
                })?;
            let option_data_length = u32::from_le_bytes(length_slice.try_into().map_err(|_| {
                Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid FedAuth option data length",
                )
            })?);

            let offset_slice = token_data
                .get((current_options_offset + 5) as usize..(current_options_offset + 9) as usize)
                .ok_or_else(|| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FedAuth option offset out of bounds",
                    )
                })?;
            let option_data_offset_raw =
                u32::from_le_bytes(offset_slice.try_into().map_err(|_| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid FedAuth option offset",
                    )
                })?);

            // Check for underflow before subtraction
            let option_data_offset = option_data_offset_raw
                .checked_sub(size_of::<u32>() as u32)
                .ok_or_else(|| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FedAuth option offset too small (underflow)",
                    )
                })?;

            // Compute the end offset in usize to avoid u32 overflow on malformed input.
            let string_end = (option_data_offset as usize)
                .checked_add(option_data_length as usize)
                .ok_or_else(|| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FedAuth string data length overflow",
                    )
                })?;
            let string_bytes: &[u8] = token_data
                .get(option_data_offset as usize..string_end)
                .ok_or_else(|| {
                    Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FedAuth string data out of bounds",
                    )
                })?;
            let u16_slice: Vec<u16> = string_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            let value = String::from_utf16(&u16_slice).map_err(|_| {
                Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-16 sequence")
            })?;

            debug!(
                "FedAuth option: {:?} with value: {:?}",
                option_id,
                value.clone()
            );

            match Into::<FedAuthInfoId>::into(option_id) {
                FedAuthInfoId::STSUrl => {
                    sts_url = value;
                }
                FedAuthInfoId::SPN => {
                    spn = value;
                }
                FedAuthInfoId::Unknown(id) => {
                    tracing::debug!(
                        "Ignoring unknown FedAuthInfoId: 0x{:02X} with value: {}",
                        id,
                        value
                    );
                }
            }
        }

        Ok(Tokens::from(FedAuthInfoToken { spn, sts_url }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::test_utils::MockReader;
    use super::*;
    use crate::token::fed_auth_info::FedAuthInfoId;
    use byteorder::{ByteOrder, LittleEndian};

    /// Helper to build FedAuth token data
    fn build_fedauth_token(options: &[(u8, &str)]) -> Vec<u8> {
        let mut data = Vec::new();

        // Calculate total size
        let options_size = (options.len() as u32) * FedAuthInfoTokenParser::FEDAUTH_OPTIONS_SIZE;
        let mut string_data = Vec::new();

        for (_, value) in options {
            let utf16_bytes = MockReader::encode_utf16(value);
            string_data.push(utf16_bytes);
        }

        let total_string_size: usize = string_data.iter().map(|d| d.len()).sum();
        let total_size = options_size + total_string_size as u32;

        // Write length (total size + size of options_count field)
        let mut buf = [0u8; 4];
        LittleEndian::write_i32(&mut buf, (total_size + 4) as i32);
        data.extend_from_slice(&buf);

        // Write options_count
        LittleEndian::write_u32(&mut buf, options.len() as u32);
        data.extend_from_slice(&buf);

        // Write option headers
        let mut current_offset = options_size + 4; // +4 for options_count
        for (i, (option_id, _)) in options.iter().enumerate() {
            data.push(*option_id);

            let string_len = string_data[i].len() as u32;
            LittleEndian::write_u32(&mut buf, string_len);
            data.extend_from_slice(&buf);

            LittleEndian::write_u32(&mut buf, current_offset);
            data.extend_from_slice(&buf);

            current_offset += string_len;
        }

        // Write string data
        for utf16_bytes in string_data {
            data.extend_from_slice(&utf16_bytes);
        }

        data
    }

    #[tokio::test]
    async fn test_parse_fedauth_with_spn_and_sts_url() {
        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), "https://sts.example.com"),
            (FedAuthInfoId::SPN.as_u8(), "MSSQLSvc/server.example.com"),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "https://sts.example.com");
                assert_eq!(token.spn, "MSSQLSvc/server.example.com");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_with_only_sts_url() {
        let options = vec![(
            FedAuthInfoId::STSUrl.as_u8(),
            "https://login.microsoftonline.com",
        )];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "https://login.microsoftonline.com");
                assert_eq!(token.spn, "");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_with_only_spn() {
        let options = vec![(FedAuthInfoId::SPN.as_u8(), "MSSQLSvc/myserver:1433")];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.spn, "MSSQLSvc/myserver:1433");
                assert_eq!(token.sts_url, "");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_with_unknown_option() {
        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), "https://sts.example.com"),
            (0xFF, "unknown_value"),
            (FedAuthInfoId::SPN.as_u8(), "MSSQLSvc/server"),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "https://sts.example.com");
                assert_eq!(token.spn, "MSSQLSvc/server");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_empty_options() {
        let options: Vec<(u8, &str)> = vec![];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "");
                assert_eq!(token.spn, "");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_length_overflow() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write a length that would underflow when subtracting size_of::<u32>()
        LittleEndian::write_i32(&mut buf, 2);
        data.extend_from_slice(&buf);

        LittleEndian::write_u32(&mut buf, 0);
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        if let Err(e) = result {
            // Verify error contains message about invalid data size (negative values cause overflow)
            assert!(
                e.to_string()
                    .contains("Invalid FedAuthInfo token data size")
            );
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_negative_length() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write negative length
        LittleEndian::write_i32(&mut buf, -1);
        data.extend_from_slice(&buf);

        LittleEndian::write_u32(&mut buf, 0);
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_fedauth_excessive_length() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write length exceeding MAX_TOKEN_DATA_SIZE
        LittleEndian::write_i32(&mut buf, 2 * 1024 * 1024); // 2MB
        data.extend_from_slice(&buf);

        LittleEndian::write_u32(&mut buf, 0);
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid FedAuthInfo token data size")
        );
    }

    #[tokio::test]
    async fn test_parse_fedauth_options_count_overflow() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write valid length
        LittleEndian::write_i32(&mut buf, 100);
        data.extend_from_slice(&buf);

        // Write options_count that would overflow when multiplied
        LittleEndian::write_u32(&mut buf, u32::MAX / 8);
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[tokio::test]
    async fn test_parse_fedauth_options_exceed_available_data() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write length for 20 bytes of data (after header)
        LittleEndian::write_i32(&mut buf, 24);
        data.extend_from_slice(&buf);

        // Write options_count that requires more than available data
        LittleEndian::write_u32(&mut buf, 10); // 10 options * 9 bytes = 90 bytes, but only 20 available
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("requires"));
    }

    #[tokio::test]
    async fn test_parse_fedauth_invalid_option_data_bounds() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write length
        LittleEndian::write_i32(&mut buf, 50);
        data.extend_from_slice(&buf);

        // Write options_count
        LittleEndian::write_u32(&mut buf, 1);
        data.extend_from_slice(&buf);

        // Write option header with invalid offset/length
        data.push(FedAuthInfoId::STSUrl.as_u8());

        // Data length pointing beyond available data
        LittleEndian::write_u32(&mut buf, 1000);
        data.extend_from_slice(&buf);

        // Offset
        LittleEndian::write_u32(&mut buf, 100);
        data.extend_from_slice(&buf);

        // Add some filler data
        data.extend_from_slice(&[0u8; 37]);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_fedauth_invalid_utf16() {
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write length
        LittleEndian::write_i32(&mut buf, 17);
        data.extend_from_slice(&buf);

        // Write options_count
        LittleEndian::write_u32(&mut buf, 1);
        data.extend_from_slice(&buf);

        // Write option header
        data.push(FedAuthInfoId::STSUrl.as_u8());

        // Data length (4 bytes for invalid UTF-16)
        LittleEndian::write_u32(&mut buf, 4);
        data.extend_from_slice(&buf);

        // Offset (after options header)
        LittleEndian::write_u32(&mut buf, 13);
        data.extend_from_slice(&buf);

        // Add invalid UTF-16 sequence (unpaired surrogate)
        data.extend_from_slice(&[0x00, 0xD8, 0x00, 0x00]); // High surrogate without low surrogate

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_parse_fedauth_empty_strings() {
        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), ""),
            (FedAuthInfoId::SPN.as_u8(), ""),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "");
                assert_eq!(token.spn, "");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_long_strings() {
        let long_url = "https://".to_string() + &"a".repeat(1000) + ".example.com";
        let long_spn = "MSSQLSvc/".to_string() + &"server".repeat(100);

        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), long_url.as_str()),
            (FedAuthInfoId::SPN.as_u8(), long_spn.as_str()),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, long_url);
                assert_eq!(token.spn, long_spn);
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_unicode_strings() {
        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), "https://例え.com/認証"),
            (FedAuthInfoId::SPN.as_u8(), "MSSQLSvc/сервер:1433"),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "https://例え.com/認証");
                assert_eq!(token.spn, "MSSQLSvc/сервер:1433");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_multiple_same_options() {
        // Test that last value wins for duplicate options
        let options = vec![
            (FedAuthInfoId::STSUrl.as_u8(), "https://first.com"),
            (FedAuthInfoId::STSUrl.as_u8(), "https://second.com"),
            (FedAuthInfoId::STSUrl.as_u8(), "https://third.com"),
        ];
        let data = build_fedauth_token(&options);
        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::FedAuthInfo(token) => {
                assert_eq!(token.sts_url, "https://third.com");
            }
            _ => panic!("Expected FedAuthInfo token"),
        }
    }

    #[tokio::test]
    async fn test_parse_fedauth_insufficient_token_data_for_options() {
        // Regression test for fuzz crash: options_count > 0 but token_data is too small
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write a small length that leaves little data after header
        LittleEndian::write_i32(&mut buf, 13); // Just enough for header (4) + option header (9)
        data.extend_from_slice(&buf);

        // Write options_count = 1 (requires 9 bytes for header)
        LittleEndian::write_u32(&mut buf, 1);
        data.extend_from_slice(&buf);

        // Add 9 bytes for the option header but with invalid offset/length
        // This creates a scenario where we have the option header but not the string data
        data.push(FedAuthInfoId::STSUrl.as_u8());
        LittleEndian::write_u32(&mut buf, 10); // String length pointing beyond data
        data.extend_from_slice(&buf);
        LittleEndian::write_u32(&mut buf, 13); // Offset pointing beyond data
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err(), "Should error on insufficient token data");
    }

    #[tokio::test]
    async fn test_parse_fedauth_zero_length_with_nonzero_options() {
        // Edge case: data_left is 0 but options_count is non-zero
        let mut data = Vec::new();
        let mut buf = [0u8; 4];

        // Write length that makes data_left = 0
        LittleEndian::write_i32(&mut buf, 4);
        data.extend_from_slice(&buf);

        // Write options_count > 0
        LittleEndian::write_u32(&mut buf, 1);
        data.extend_from_slice(&buf);

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(
            result.is_err(),
            "Should error when options_count requires more data than available"
        );
    }

    /// Regression test for a fuzzer-discovered crash (crash-1a0718529807d9d6b4ae25c65ac23c42c1b742aa).
    /// A malformed option header whose data offset and length are both large u32 values caused
    /// `offset + length` to overflow when computing the string slice bounds, aborting the process.
    /// The parser must return an error instead of panicking.
    #[tokio::test]
    async fn test_parse_fedauth_option_offset_length_overflow() {
        // Crash input without the leading token-type byte (consumed by the token stream reader).
        let data = vec![
            0x0e, 0x00, 0x00, 0x00, // length = 14
            0x01, 0x00, 0x00, 0x00, // options_count = 1
            0x00, // option_id
            0x00, 0x04, 0x27, 0x81, // option_data_length = 0x81270400
            0xb9, 0xdf, 0x0b, 0xfe, // option_data_offset = 0xfe0bdfb9
            0x3b, // filler
        ];

        let mut reader = MockReader::new(data);
        let parser = FedAuthInfoTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(
            result.is_err(),
            "Overflowing option offset/length must produce an error, not a panic"
        );
    }
}
