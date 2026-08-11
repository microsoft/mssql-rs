// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # ERROR Token Parser
//!
//! Parses ERROR tokens (0xAA) which report SQL Server errors to the client.
//! These tokens are sent when a statement execution fails or encounters an error.
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                      ERROR Token (variable length)                   │
//! ├─────────┬──────────┬───────┬──────────┬──────────────┬──────────────┤
//! │ Length  │  Number  │ State │ Severity │   Message    │ Server Name  │
//! │(2 bytes)│ (4 bytes)│(1 byte│ (1 byte) │ (2+N chars)  │ (1+N chars)  │
//! │ UINT16  │  UINT32  │  BYTE │   BYTE   │US_VARCHAR(2) │B_VARCHAR(1)  │
//! └─────────┴──────────┴───────┴──────────┴──────────────┴──────────────┘
//!     0-1       2-5        6        7        8-9+N       10+N-11+M
//!
//! ┌──────────────┬──────────────┐
//! │   Proc Name  │ Line Number  │
//! │ (1+N chars)  │  (4 bytes)   │
//! │ B_VARCHAR(1) │   UINT32     │
//! └──────────────┴──────────────┘
//!   12+M-13+P      14+P-17+P
//!
//! Fields:
//!   Length    - Total token length excluding this field
//!   Number    - SQL Server error number (e.g., 208 = object not found)
//!   State     - State code (internal error state machine position)
//!   Severity  - Error severity (0-25, where >16 requires immediate attention)
//!                 11-16: User errors (can be corrected by user)
//!                 17-19: Software/hardware errors
//!                 20-25: Fatal errors (connection terminated)
//!   Message   - Human-readable error message (UTF-16 LE)
//!   ServerName- Name of SQL Server instance (UTF-16 LE)
//!   ProcName  - Stored procedure name if error occurred in a proc (UTF-16 LE)
//!   LineNumber- Line number in batch/proc where error occurred
//! ```
//!
//! ## Common Error Numbers
//!
//! - **208**: Invalid object name (table/view doesn't exist)
//! - **515**: Cannot insert NULL into non-nullable column
//! - **547**: Foreign key constraint violation
//! - **2601**: Duplicate key violation
//! - **18456**: Login failed
//!
//! ## Example
//!
//! ```text
//! // After executing "SELECT * FROM NonExistentTable"
//! // Server sends ERROR token:
//! //   Number   = 208
//! //   State    = 1
//! //   Class    = 16 (user error)
//! //   Message  = "Invalid object name 'NonExistentTable'."
//! //   LineNumber = 1
//! ```

use async_trait::async_trait;
use tracing::error;

use super::super::tokens::{ErrorToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{io::token_stream::ParserContext, token::tokens::TokenType};

/// Parser for ERROR token (0xAA) - reports SQL Server errors
/// Parser for ERROR token (0xAA) - reports SQL Server errors
#[derive(Default)]
pub(crate) struct ErrorTokenParser {
    // fields omitted
}

#[async_trait]
impl<T> TokenParser<T> for ErrorTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        error!(
            "Parsing Error token with type: 0x{:02X}",
            TokenType::Error as u8
        );

        // Read token length (2 bytes) - total length excluding this field
        let _ = reader.read_uint16().await?;

        // Read error number (4 bytes) - SQL Server error code
        let number = reader.read_uint32().await?;

        // Read state (1 byte) - internal state code
        let state = reader.read_byte().await?;

        // Read severity (1 byte) - error severity level (0-25)
        let severity = reader.read_byte().await?;

        // Read error message (US_VARCHAR with 2-byte length prefix)
        // Message is in UTF-16 LE format
        let message = reader.read_varchar_u16_length().await?.unwrap_or_default();
        error!("Error message: {:?}", message);

        // Read server name (B_VARCHAR with 1-byte length prefix)
        let server_name = reader.read_varchar_u8_length().await?;

        // Read procedure name (B_VARCHAR with 1-byte length prefix)
        // Empty if error not in a stored procedure
        let proc_name = reader.read_varchar_u8_length().await?;

        // Read line number (4 bytes) - line where error occurred
        let line_number = reader.read_uint32().await?;

        Ok(Tokens::from(ErrorToken {
            number,
            state,
            severity,
            message,
            server_name,
            proc_name,
            line_number,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_reader::tests::{TestPacketBuilder, create_network_transport_with_data};
    use crate::message::messages::PacketType;

    fn encode_utf16_string(s: &str) -> Vec<u8> {
        let utf16_units: Vec<u16> = s.encode_utf16().collect();
        let mut bytes = Vec::with_capacity(utf16_units.len() * 2);
        for unit in utf16_units {
            bytes.push((unit & 0xFF) as u8);
            bytes.push((unit >> 8) as u8);
        }
        bytes
    }

    #[tokio::test]
    async fn test_parse_error_token_basic() {
        // Test parsing basic ERROR token
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);

        let message = "Invalid object name";
        let server_name = "TestServer";
        let proc_name = "";

        let message_bytes = encode_utf16_string(message);
        let server_bytes = encode_utf16_string(server_name);
        let proc_bytes = encode_utf16_string(proc_name);

        // Calculate total length
        let length = 4 + 1 + 1 // number, state, severity
            + 2 + message_bytes.len() // message length + message
            + 1 + server_bytes.len() // server name length + server name
            + 1 + proc_bytes.len() // proc name length + proc name
            + 4; // line number

        builder.append_u16(length as u16); // token length
        builder.append_u32(208); // error number (object not found)
        builder.append_byte(1); // state
        builder.append_byte(16); // severity (user error)

        // Message with 2-byte length prefix (character count)
        builder.append_u16(message.len() as u16);
        builder.append_bytes(&message_bytes);

        // Server name with 1-byte length prefix (character count)
        builder.append_byte(server_name.len() as u8);
        builder.append_bytes(&server_bytes);

        // Proc name with 1-byte length prefix (character count)
        builder.append_byte(proc_name.len() as u8);
        builder.append_bytes(&proc_bytes);

        // Line number
        builder.append_u32(1);

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = ErrorTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Error(token) => {
                assert_eq!(token.number, 208);
                assert_eq!(token.state, 1);
                assert_eq!(token.severity, 16);
                assert_eq!(token.message, message);
                assert_eq!(token.server_name, server_name);
                assert_eq!(token.proc_name, proc_name);
                assert_eq!(token.line_number, 1);
            }
            _ => panic!("Expected Error token"),
        }
    }

    #[tokio::test]
    async fn test_parse_error_token_with_proc() {
        // Test parsing ERROR token from stored procedure
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);

        let message = "Error in stored procedure";
        let server_name = "ProdServer";
        let proc_name = "sp_GetUserData";

        let message_bytes = encode_utf16_string(message);
        let server_bytes = encode_utf16_string(server_name);
        let proc_bytes = encode_utf16_string(proc_name);

        let length =
            4 + 1 + 1 + 2 + message_bytes.len() + 1 + server_bytes.len() + 1 + proc_bytes.len() + 4;

        builder.append_u16(length as u16);
        builder.append_u32(50000); // custom error number
        builder.append_byte(2);
        builder.append_byte(17); // severity 17 (resource/hardware error)

        builder.append_u16(message.len() as u16);
        builder.append_bytes(&message_bytes);

        builder.append_byte(server_name.len() as u8);
        builder.append_bytes(&server_bytes);

        builder.append_byte(proc_name.len() as u8);
        builder.append_bytes(&proc_bytes);

        builder.append_u32(123); // line number in proc

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = ErrorTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Error(token) => {
                assert_eq!(token.number, 50000);
                assert_eq!(token.state, 2);
                assert_eq!(token.severity, 17);
                assert_eq!(token.message, message);
                assert_eq!(token.server_name, server_name);
                assert_eq!(token.proc_name, proc_name);
                assert_eq!(token.line_number, 123);
            }
            _ => panic!("Expected Error token"),
        }
    }

    #[tokio::test]
    async fn test_parse_error_token_constraint_violation() {
        // Test parsing ERROR token for constraint violation
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);

        let message = "Violation of PRIMARY KEY constraint";
        let server_name = "DB1";
        let proc_name = "";

        let message_bytes = encode_utf16_string(message);
        let server_bytes = encode_utf16_string(server_name);
        let proc_bytes = encode_utf16_string(proc_name);

        let length =
            4 + 1 + 1 + 2 + message_bytes.len() + 1 + server_bytes.len() + 1 + proc_bytes.len() + 4;

        builder.append_u16(length as u16);
        builder.append_u32(2627); // primary key violation
        builder.append_byte(1);
        builder.append_byte(14);

        builder.append_u16(message.len() as u16);
        builder.append_bytes(&message_bytes);

        builder.append_byte(server_name.len() as u8);
        builder.append_bytes(&server_bytes);

        builder.append_byte(0); // no proc name
        builder.append_u32(5);

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = ErrorTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Error(token) => {
                assert_eq!(token.number, 2627);
                assert_eq!(token.severity, 14);
                assert_eq!(token.message, message);
                assert_eq!(token.proc_name, "");
            }
            _ => panic!("Expected Error token"),
        }
    }

    #[tokio::test]
    async fn test_parse_error_token_fatal() {
        // Test parsing ERROR token with fatal severity
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);

        let message = "Fatal error";
        let server_name = "SQL";
        let proc_name = "";

        let message_bytes = encode_utf16_string(message);
        let server_bytes = encode_utf16_string(server_name);
        let proc_bytes = encode_utf16_string(proc_name);

        let length =
            4 + 1 + 1 + 2 + message_bytes.len() + 1 + server_bytes.len() + 1 + proc_bytes.len() + 4;

        builder.append_u16(length as u16);
        builder.append_u32(9999);
        builder.append_byte(1);
        builder.append_byte(25); // fatal severity

        builder.append_u16(message.len() as u16);
        builder.append_bytes(&message_bytes);

        builder.append_byte(server_name.len() as u8);
        builder.append_bytes(&server_bytes);

        builder.append_byte(0);
        builder.append_u32(0);

        let mut packet_reader = create_network_transport_with_data(&builder.build()).await;

        let parser = ErrorTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut packet_reader, &context).await.unwrap();

        match result {
            Tokens::Error(token) => {
                assert_eq!(token.severity, 25);
                assert_eq!(token.message, message);
            }
            _ => panic!("Expected Error token"),
        }
    }
}
