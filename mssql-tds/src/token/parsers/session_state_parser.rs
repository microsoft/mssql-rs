// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # SESSIONSTATE Token Parser
//!
//! Parses SESSIONSTATE tokens (0xE4) sent by the server to communicate session
//! state changes for idle connection resiliency (session recovery).
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────┐
//! │                    SESSIONSTATE Token (variable)                     │
//! ├──────────────┬──────────┬────────────────────────────────────────────┤
//! │  Total Length │ SeqNum  │  State entries (repeated)                  │
//! │ (4 bytes)    │(4 bytes) │  [state_id(1) + len(1or5) + data(N)]     │
//! │ UINT32       │ UINT32   │                                           │
//! └──────────────┴──────────┴────────────────────────────────────────────┘
//!
//! If SeqNum == 0xFFFFFFFF → master recovery disabled
//! Status byte: bit 0 = fRecoverable
//!
//! State entry length:
//!   - If first byte < 0xFF → length is that byte (1 byte encoding)
//!   - If first byte == 0xFF → next 4 bytes are the length (5 byte encoding)
//! ```

use async_trait::async_trait;

use super::common::TokenParser;
use crate::core::TdsResult;
use crate::io::packet_reader::TdsPacketReader;
use crate::io::token_stream::ParserContext;
use crate::token::tokens::{SessionStateEntry, SessionStateToken, Tokens};

/// Maximum allowed total SESSIONSTATE token data size (DoS protection).
/// Session state for 256 state IDs with reasonable data sizes should not
/// exceed this. Matches the conservative bound used by featureext_parser.
const MAX_SESSION_STATE_TOKEN_BYTES: u32 = 1024 * 1024; // 1 MiB

/// Parser for SESSIONSTATE token (0xE4) — session state changes for recovery.
#[derive(Debug, Default)]
pub(crate) struct SessionStateTokenParser;

#[async_trait]
impl<T> TokenParser<T> for SessionStateTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        // Total token length (u32) — bounds how much data follows.
        let total_length = reader.read_uint32().await?;

        if total_length > MAX_SESSION_STATE_TOKEN_BYTES {
            return Err(crate::error::Error::ProtocolError(format!(
                "SESSIONSTATE token length too large: {total_length} bytes \
                 (max: {MAX_SESSION_STATE_TOKEN_BYTES} bytes). Possible DoS attack."
            )));
        }

        // We need at least 5 bytes: sequence_number(4) + status(1)
        if total_length < 5 {
            return Err(crate::error::Error::ProtocolError(format!(
                "SESSIONSTATE token too short: {total_length} bytes (minimum 5)"
            )));
        }

        // Sequence number (u32) — if u32::MAX, signals master recovery disabled.
        let sequence_number = reader.read_uint32().await?;

        // Status byte — bit 0 = fRecoverable
        let status = reader.read_byte().await?;

        let mut bytes_read: u32 = 5; // sequence_number(4) + status(1)
        let mut states = Vec::new();

        while bytes_read < total_length {
            // state_id (u8)
            if bytes_read + 1 > total_length {
                return Err(crate::error::Error::ProtocolError(
                    "SESSIONSTATE: unexpected end of token reading state_id".to_string(),
                ));
            }
            let state_id = reader.read_byte().await?;
            bytes_read += 1;

            // state_len: if first byte < 0xFF → that's the length.
            // If first byte == 0xFF → next 4 bytes are the actual length.
            if bytes_read + 1 > total_length {
                return Err(crate::error::Error::ProtocolError(
                    "SESSIONSTATE: unexpected end of token reading state_len".to_string(),
                ));
            }
            let len_byte = reader.read_byte().await?;
            bytes_read += 1;

            let state_len: u32 = if len_byte == 0xFF {
                if bytes_read + 4 > total_length {
                    return Err(crate::error::Error::ProtocolError(
                        "SESSIONSTATE: unexpected end of token reading extended state_len"
                            .to_string(),
                    ));
                }
                let extended_len = reader.read_uint32().await?;
                bytes_read += 4;
                extended_len
            } else {
                len_byte as u32
            };

            // Validate state_len against remaining token data. Compare via subtraction
            // because `bytes_read + state_len` overflows u32 when state_len is large
            // (state_len is attacker-controlled, up to u32::MAX via the 0xFF extended
            // encoding). `bytes_read < total_length` is guaranteed by the loop guard.
            let remaining = total_length - bytes_read;
            if state_len > remaining {
                return Err(crate::error::Error::ProtocolError(format!(
                    "SESSIONSTATE: state data length {state_len} exceeds remaining \
                     token bytes ({remaining} remaining)"
                )));
            }

            let mut data = vec![0u8; state_len as usize];
            if state_len > 0 {
                reader.read_bytes(&mut data).await?;
            }
            bytes_read += state_len;

            states.push(SessionStateEntry {
                state_id,
                recoverable: (status & 0x01) != 0,
                data,
            });
        }

        Ok(Tokens::from(SessionStateToken {
            sequence_number,
            status,
            states,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::parsers::common::test_utils::MockReader;

    fn build_session_state_bytes(
        sequence_number: u32,
        status: u8,
        state_entries: &[(u8, &[u8])],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&sequence_number.to_le_bytes());
        payload.push(status);
        for &(state_id, data) in state_entries {
            payload.push(state_id);
            if data.len() < 0xFF {
                payload.push(data.len() as u8);
            } else {
                payload.push(0xFF);
                payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
            }
            payload.extend_from_slice(data);
        }
        // Prepend total length
        let total_len = payload.len() as u32;
        let mut result = Vec::new();
        result.extend_from_slice(&total_len.to_le_bytes());
        result.extend_from_slice(&payload);
        result
    }

    #[tokio::test]
    async fn parse_single_recoverable_state() {
        let bytes = build_session_state_bytes(1, 0x01, &[(5, &[0xAA, 0xBB])]);
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let token = parser.parse(&mut reader, &context).await.unwrap();
        match token {
            Tokens::SessionState(t) => {
                assert_eq!(t.sequence_number, 1);
                assert_eq!(t.status, 0x01);
                assert_eq!(t.states.len(), 1);
                assert_eq!(t.states[0].state_id, 5);
                assert!(t.states[0].recoverable);
                assert_eq!(t.states[0].data, vec![0xAA, 0xBB]);
            }
            _ => panic!("Expected SessionState token"),
        }
    }

    #[tokio::test]
    async fn parse_unrecoverable_state() {
        let bytes = build_session_state_bytes(2, 0x00, &[(10, &[0x01])]);
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let token = parser.parse(&mut reader, &context).await.unwrap();
        match token {
            Tokens::SessionState(t) => {
                assert!(!t.states[0].recoverable);
            }
            _ => panic!("Expected SessionState token"),
        }
    }

    #[tokio::test]
    async fn parse_multiple_states() {
        let bytes =
            build_session_state_bytes(3, 0x01, &[(0, &[0x01]), (1, &[0x02, 0x03]), (255, &[])]);
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let token = parser.parse(&mut reader, &context).await.unwrap();
        match token {
            Tokens::SessionState(t) => {
                assert_eq!(t.states.len(), 3);
                assert_eq!(t.states[0].state_id, 0);
                assert_eq!(t.states[0].data, vec![0x01]);
                assert_eq!(t.states[1].state_id, 1);
                assert_eq!(t.states[1].data, vec![0x02, 0x03]);
                assert_eq!(t.states[2].state_id, 255);
                assert!(t.states[2].data.is_empty());
            }
            _ => panic!("Expected SessionState token"),
        }
    }

    #[tokio::test]
    async fn parse_master_disabled_sequence() {
        let bytes = build_session_state_bytes(u32::MAX, 0x00, &[(0, &[0x01])]);
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let token = parser.parse(&mut reader, &context).await.unwrap();
        match token {
            Tokens::SessionState(t) => {
                assert_eq!(t.sequence_number, u32::MAX);
            }
            _ => panic!("Expected SessionState token"),
        }
    }

    #[tokio::test]
    async fn parse_extended_length_state() {
        // Create data larger than 0xFE bytes to trigger 0xFF + u32 length encoding
        let large_data = vec![0x42u8; 300];
        let bytes = build_session_state_bytes(1, 0x01, &[(7, &large_data)]);
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let token = parser.parse(&mut reader, &context).await.unwrap();
        match token {
            Tokens::SessionState(t) => {
                assert_eq!(t.states.len(), 1);
                assert_eq!(t.states[0].state_id, 7);
                assert_eq!(t.states[0].data.len(), 300);
            }
            _ => panic!("Expected SessionState token"),
        }
    }

    #[tokio::test]
    async fn reject_oversized_token() {
        // Construct a token claiming to be larger than MAX_SESSION_STATE_TOKEN_BYTES
        let mut bytes = Vec::new();
        let huge_len: u32 = MAX_SESSION_STATE_TOKEN_BYTES + 1;
        bytes.extend_from_slice(&huge_len.to_le_bytes());
        // Don't need actual data — parser should reject based on length alone
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("too large"));
    }

    #[tokio::test]
    async fn reject_token_too_short() {
        // Total length = 3 (less than minimum 5)
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0x00, 0x00, 0x00]); // 3 bytes of junk
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("too short"));
    }

    #[tokio::test]
    async fn reject_extended_length_overflow() {
        // Regression: a 0xFF-encoded state_len near u32::MAX previously triggered
        // `bytes_read + state_len` overflow in the bounds check.
        let mut bytes = Vec::new();
        let total_len: u32 = 11; // bytes after total_length: seq(4) + status(1) + state_id(1) + len_byte(1) + extended_len(4)
        bytes.extend_from_slice(&total_len.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // sequence_number
        bytes.push(0x00); // status
        bytes.push(0x00); // state_id
        bytes.push(0xFF); // len_byte marker; followed by u32 length
        bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // extended length = u32::MAX
        let mut reader = MockReader::new(bytes);
        let parser = SessionStateTokenParser;
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("exceeds remaining"));
    }
}
