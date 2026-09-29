// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # DONE Token Parser
//!
//! Parses DONE, DONEPROC, and DONEINPROC tokens which indicate completion
//! of SQL statements, stored procedures, and nested procedures respectively.
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                      DONE Token (12 bytes)                      │
//! ├──────────┬──────────┬──────────────────┬──────────────────────┤
//! │  Status  │ CurCmd   │                  │      Row Count       │
//! │ (2 bytes)│ (2 bytes)│    Reserved      │     (8 bytes)        │
//! │ UINT16   │ UINT16   │    (unused)      │      UINT64          │
//! └──────────┴──────────┴──────────────────┴──────────────────────┘
//!     0-1        2-3                            4-11
//!
//! Status flags (bitmask):
//!   0x00 = DONE_FINAL   - Final DONE in response
//!   0x01 = DONE_MORE    - More results coming
//!   0x02 = DONE_ERROR   - Error occurred
//!   0x10 = DONE_COUNT   - Row count is valid
//!   0x20 = DONE_ATTN    - Attention acknowledgment
//!   0x100= DONE_SRVERROR- Server error occurred
//!
//! CurCmd values:
//!   0x00 = None
//!   0xC1 = SELECT
//!   0xC2 = INSERT
//!   0xC3 = DELETE
//!   0xC4 = UPDATE
//!   ... (see CurrentCommand enum)
//! ```
//!
//! ## Token Variants
//!
//! - **DONE (0xFD)**: Indicates completion of a SQL statement
//! - **DONEPROC (0xFE)**: Indicates completion of a stored procedure
//! - **DONEINPROC (0xFF)**: Indicates completion of a statement within a procedure
//!
//! ## Example
//!
//! ```text
//! // After executing "SELECT * FROM users"
//! // Server sends:
//! //   - ColMetadata token (column definitions)
//! //   - Row tokens (data rows)
//! //   - DONE token (Status=0x10, CurCmd=0xC1, RowCount=5)
//! ```

use async_trait::async_trait;

use super::super::tokens::{DoneToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    io::token_stream::ParserContext,
    token::tokens::{CurrentCommand, DoneStatus},
};

pub(crate) const DONE_PAYLOAD_LEN: usize = 12;

pub(crate) async fn read_done_token<T: TdsPacketReader + Send + Sync>(
    reader: &mut T,
) -> TdsResult<DoneToken> {
    if let Some(bytes) = reader.try_read_slice(DONE_PAYLOAD_LEN) {
        return buffered_done_token(bytes).ok_or_else(|| {
            crate::error::Error::ProtocolError("Buffered DONE payload is incomplete".to_string())
        });
    }
    let status = reader.read_uint16().await?;
    let command = reader.read_uint16().await?;
    let row_count = reader.read_uint64().await?;
    Ok(DoneToken {
        status: DoneStatus::from(status),
        cur_cmd: CurrentCommand::try_from(command).unwrap_or(CurrentCommand::None),
        row_count,
    })
}

pub(crate) fn buffered_done_token(bytes: &[u8]) -> Option<DoneToken> {
    let bytes: &[u8; DONE_PAYLOAD_LEN] = bytes.get(..DONE_PAYLOAD_LEN)?.try_into().ok()?;
    Some(DoneToken {
        status: DoneStatus::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        cur_cmd: CurrentCommand::try_from(u16::from_le_bytes([bytes[2], bytes[3]]))
            .unwrap_or(CurrentCommand::None),
        row_count: u64::from_le_bytes([
            bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
        ]),
    })
}

/// Parser for DONE token (0xFD) - signals completion of a SQL statement
#[cfg(not(fuzzing))]
#[derive(Default)]
pub(crate) struct DoneTokenParser {
    // fields omitted
}

#[cfg(fuzzing)]
pub struct DoneTokenParser {
    // fields omitted
}

#[async_trait]
impl<T> TokenParser<T> for DoneTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        read_done_token(reader).await.map(Tokens::Done)
    }
}

/// Parser for DONEINPROC token (0xFF) - signals completion within a stored procedure
/// Parser for DONEINPROC token (0xFF) - signals completion within a stored procedure
#[derive(Debug, Default)]
pub(crate) struct DoneInProcTokenParser {
    // fields omitted
}

#[async_trait]
impl<T> TokenParser<T> for DoneInProcTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        read_done_token(reader).await.map(Tokens::DoneInProc)
    }
}

/// Parser for DONEPROC token (0xFE) - signals completion of a stored procedure
#[derive(Debug, Default)]
pub(crate) struct DoneProcTokenParser {
    // fields omitted
}

#[async_trait]
impl<T> TokenParser<T> for DoneProcTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        read_done_token(reader).await.map(Tokens::DoneProc)
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::test_utils::MockReader;
    use super::*;
    use crate::token::tokens::{CurrentCommand, DoneStatus};
    use byteorder::{ByteOrder, LittleEndian};

    fn build_done_token_data(status: u16, cur_cmd: u16, row_count: u64) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, status);
        data.extend_from_slice(&buf);
        LittleEndian::write_u16(&mut buf, cur_cmd);
        data.extend_from_slice(&buf);
        let mut buf8 = [0u8; 8];
        LittleEndian::write_u64(&mut buf8, row_count);
        data.extend_from_slice(&buf8);
        data
    }

    #[tokio::test]
    async fn test_parse_done_token_success() {
        let data =
            build_done_token_data(DoneStatus::FINAL.bits(), CurrentCommand::Select as u16, 5);
        let mut reader = MockReader::new(data);
        let parser = DoneTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::Done(token) => {
                assert!(token.status.contains(DoneStatus::FINAL));
                assert_eq!(token.cur_cmd, CurrentCommand::Select);
                assert_eq!(token.row_count, 5);
            }
            _ => panic!("Expected Done token"),
        }
    }

    #[tokio::test]
    async fn test_parse_done_token_with_count() {
        let data = build_done_token_data(
            (DoneStatus::COUNT | DoneStatus::FINAL).bits(),
            CurrentCommand::Insert as u16,
            10,
        );
        let mut reader = MockReader::new(data);
        let parser = DoneTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::Done(token) => {
                assert!(token.status.contains(DoneStatus::COUNT));
                assert_eq!(token.cur_cmd, CurrentCommand::Insert);
                assert_eq!(token.row_count, 10);
            }
            _ => panic!("Expected Done token"),
        }
    }

    #[tokio::test]
    async fn test_parse_done_token_with_error() {
        let data = build_done_token_data(
            (DoneStatus::ERROR | DoneStatus::FINAL).bits(),
            CurrentCommand::None as u16,
            0,
        );
        let mut reader = MockReader::new(data);
        let parser = DoneTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::Done(token) => {
                assert!(token.status.contains(DoneStatus::ERROR));
                assert_eq!(token.cur_cmd, CurrentCommand::None);
                assert_eq!(token.row_count, 0);
            }
            _ => panic!("Expected Done token"),
        }
    }

    #[tokio::test]
    async fn test_parse_done_token_with_more() {
        let data =
            build_done_token_data(DoneStatus::MORE.bits(), CurrentCommand::Select as u16, 100);
        let mut reader = MockReader::new(data);
        let parser = DoneTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::Done(token) => {
                assert!(token.status.contains(DoneStatus::MORE));
                assert_eq!(token.cur_cmd, CurrentCommand::Select);
                assert_eq!(token.row_count, 100);
            }
            _ => panic!("Expected Done token"),
        }
    }

    #[tokio::test]
    async fn test_parse_doneinproc_token() {
        let data =
            build_done_token_data(DoneStatus::FINAL.bits(), CurrentCommand::Update as u16, 3);
        let mut reader = MockReader::new(data);
        let parser = DoneInProcTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::DoneInProc(token) => {
                assert!(token.status.contains(DoneStatus::FINAL));
                assert_eq!(token.cur_cmd, CurrentCommand::Update);
                assert_eq!(token.row_count, 3);
            }
            _ => panic!("Expected DoneInProc token"),
        }
    }

    #[tokio::test]
    async fn test_parse_doneproc_token() {
        let data = build_done_token_data(
            (DoneStatus::COUNT | DoneStatus::FINAL).bits(),
            CurrentCommand::Delete as u16,
            7,
        );
        let mut reader = MockReader::new(data);
        let parser = DoneProcTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::DoneProc(token) => {
                assert!(token.status.contains(DoneStatus::COUNT));
                assert_eq!(token.cur_cmd, CurrentCommand::Delete);
                assert_eq!(token.row_count, 7);
            }
            _ => panic!("Expected DoneProc token"),
        }
    }

    #[tokio::test]
    async fn test_parse_done_token_large_rowcount() {
        let data = build_done_token_data(
            (DoneStatus::COUNT | DoneStatus::FINAL).bits(),
            CurrentCommand::Select as u16,
            999999999,
        );
        let mut reader = MockReader::new(data);
        let parser = DoneTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::Done(token) => {
                assert_eq!(token.row_count, 999999999);
            }
            _ => panic!("Expected Done token"),
        }
    }
}
