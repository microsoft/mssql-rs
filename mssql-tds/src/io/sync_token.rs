// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-synchronous non-row token parse leaf (sans-I/O core).
//!
//! These are the **production** parse bodies for the bounded, length-delimited
//! non-row tokens driven by [`TdsCore::step_token`](crate::io::tds_core::TdsCore::step_token):
//! DONE / DONEINPROC / DONEPROC, RETURNSTATUS, ORDER, ERROR, INFO, and ENVCHANGE.
//! Each reads its whole body in place over a [`PacketBuffer`] that the driver has
//! already ensured is fully resident, so no accessor here can underflow — a
//! shortfall is handled one layer up, at token entry, by re-driving `step_token`.
//!
//! The byte-for-byte semantics mirror the corresponding `async fn parse` in
//! `token/parsers/*`, which are retained as `#[cfg(any(test, fuzzing))]`
//! reference oracles; the split-at-every-offset differential tests in
//! `token_stream.rs` prove the two decode identically.
//!
//! The value-carrying / unbounded tokens (COLMETADATA, RETURNVALUE,
//! SESSIONSTATE, FEATUREEXTACK) and the login/handshake tokens are **not** here:
//! [`is_sync_token`] returns `false` for them and the driver keeps servicing
//! them on the async seam. Their pure-sync inversion is deferred to a later,
//! parent-gated layer that would add a sanctioned mid-token cursor.

use byteorder::{ByteOrder, LittleEndian};

use crate::core::TdsResult;
use crate::io::packet_buffer::{NeedBytes, PacketBuffer};
use crate::io::packet_reader::PacketReader;
use crate::io::token_stream::ParserContext;
use crate::message::login::RoutingInfo;
use crate::token::tokens::{
    CurrentCommand, DoneStatus, DoneToken, EnvChangeContainer, EnvChangeToken,
    EnvChangeTokenSubType, ErrorToken, InfoToken, OrderToken, ReturnStatusToken, SqlCollation,
    TokenType, Tokens,
};

/// Whether `token_type` is decoded by the synchronous leaf in this module.
///
/// The bounded, length-delimited hot-path tokens are sync; everything else
/// (the value-carrying (b) tokens and the login/handshake tokens) stays on the
/// async seam via [`TokenStep::AsyncToken`](crate::io::tds_core::TokenStep).
pub(crate) fn is_sync_token(token_type: &TokenType) -> bool {
    matches!(
        token_type,
        TokenType::Done
            | TokenType::DoneInProc
            | TokenType::DoneProc
            | TokenType::ReturnStatus
            | TokenType::Order
            | TokenType::Error
            | TokenType::Info
            | TokenType::EnvChange
    )
}

/// Fixed on-wire body length (excluding the token byte) for the fixed-width sync
/// tokens, or `None` for the length-prefixed ones whose body is `2 + len`.
pub(crate) fn fixed_body_len(token_type: &TokenType) -> Option<usize> {
    match token_type {
        // status(2) + cur_cmd(2) + row_count(8)
        TokenType::Done | TokenType::DoneInProc | TokenType::DoneProc => Some(12),
        // value(4)
        TokenType::ReturnStatus => Some(4),
        _ => None,
    }
}

/// Total body length (excluding the token byte) for a sync token whose token
/// byte has already been consumed, i.e. `buf` is positioned at the body.
///
/// Fixed-width tokens return immediately; length-prefixed tokens read the u16
/// body-length prefix at body offset 0 and return `2 + len`. A shortfall while
/// reading that prefix is reported as [`NeedBytes`] so the caller can refill.
pub(crate) fn body_len(buf: &PacketBuffer, token_type: &TokenType) -> Result<usize, NeedBytes> {
    if let Some(body) = fixed_body_len(token_type) {
        return Ok(body);
    }
    match buf.peek_bytes(2) {
        Some(prefix) => Ok(2 + u16::from_le_bytes([prefix[0], prefix[1]]) as usize),
        None => Err(NeedBytes {
            shortfall: 2 - buf.available(),
        }),
    }
}

/// Parses one bounded non-row token body in place.
///
/// The caller ([`TdsCore::step_token`](crate::io::tds_core::TdsCore::step_token)
/// or the row driver's terminal-token handoff) guarantees the token byte is
/// already consumed and the entire body is resident in `buf`, so every `take_*`
/// here succeeds.
pub(crate) fn parse_token_body(
    buf: &mut PacketBuffer,
    token_type: TokenType,
    _context: &ParserContext,
) -> TdsResult<Tokens> {
    match token_type {
        TokenType::Done => Ok(Tokens::Done(parse_done_body(buf)?)),
        TokenType::DoneInProc => Ok(Tokens::DoneInProc(parse_done_body(buf)?)),
        TokenType::DoneProc => Ok(Tokens::DoneProc(parse_done_body(buf)?)),
        TokenType::ReturnStatus => {
            let value = buf.take_i32_le()?;
            Ok(Tokens::from(ReturnStatusToken { value }))
        }
        TokenType::Order => parse_order_body(buf),
        TokenType::Error => parse_error_body(buf),
        TokenType::Info => parse_info_body(buf),
        TokenType::EnvChange => parse_envchange_body(buf),
        other => Err(crate::error::Error::ProtocolError(format!(
            "sync_token::parse_token_body called with non-sync token {other:?}"
        ))),
    }
}

fn parse_done_body(buf: &mut PacketBuffer) -> TdsResult<DoneToken> {
    let status = buf.take_u16_le()?;
    let done_status = DoneStatus::from(status);
    let current_command_value = buf.take_u16_le()?;
    let current_command =
        CurrentCommand::try_from(current_command_value).unwrap_or(CurrentCommand::None);
    let row_count = buf.take_u64_le()?;
    Ok(DoneToken {
        status: done_status,
        cur_cmd: current_command,
        row_count,
    })
}

fn parse_order_body(buf: &mut PacketBuffer) -> TdsResult<Tokens> {
    let length = buf.take_u16_le()?;
    let col_count = length / 2;
    let mut columns = Vec::new();
    for _ in 0..col_count {
        columns.push(buf.take_u16_le()?);
    }
    Ok(Tokens::from(OrderToken {
        _order_columns: columns,
    }))
}

fn parse_error_body(buf: &mut PacketBuffer) -> TdsResult<Tokens> {
    // Token length (u16) - total length excluding this field; already used by
    // the driver to bound the body, so discarded here.
    let _ = buf.take_u16_le()?;
    let number = buf.take_u32_le()?;
    let state = buf.take_u8()?;
    let severity = buf.take_u8()?;
    let message = read_varchar_u16(buf)?.unwrap_or_default();
    let server_name = read_varchar_u8(buf)?;
    let proc_name = read_varchar_u8(buf)?;
    let line_number = buf.take_u32_le()?;
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

fn parse_info_body(buf: &mut PacketBuffer) -> TdsResult<Tokens> {
    let _length = buf.take_u16_le()?;
    let number = buf.take_u32_le()?;
    let state = buf.take_u8()?;
    let severity = buf.take_u8()?;
    let message = read_varchar_u16(buf)?;
    let server_name = read_varchar_u8(buf)?;
    let proc_name = read_varchar_u8(buf)?;
    let line_number = buf.take_u32_le()?;
    Ok(Tokens::from(InfoToken {
        number,
        state,
        severity,
        message: message.unwrap_or_default(),
        server_name,
        proc_name,
        line_number,
    }))
}

fn parse_envchange_body(buf: &mut PacketBuffer) -> TdsResult<Tokens> {
    use std::io::Error;

    let _token_length = buf.take_u16_le()?;
    let sub_type = buf.take_u8()?;
    let token_sub_type: EnvChangeTokenSubType = sub_type.try_into()?;

    let token_value_change: EnvChangeContainer = match token_sub_type {
        EnvChangeTokenSubType::Database => {
            let new_value = read_varchar_u8(buf)?;
            let old_value = read_varchar_u8(buf)?;
            EnvChangeContainer::from((old_value, new_value))
        }
        EnvChangeTokenSubType::Language => {
            let new_value = read_varchar_u8(buf)?;
            let old_value = read_varchar_u8(buf)?;
            EnvChangeContainer::from((old_value, new_value))
        }
        EnvChangeTokenSubType::CharacterSet => {
            let new_value = read_varchar_u8(buf)?;
            let old_value = read_varchar_u8(buf)?;
            EnvChangeContainer::from((old_value, new_value))
        }
        EnvChangeTokenSubType::PacketSize => {
            let new_value_string = read_varchar_u8(buf)?;
            let old_value_string = read_varchar_u8(buf)?;
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
            let new_bytes = read_u8_varbyte(buf)?;
            let old_bytes = read_u8_varbyte(buf)?;
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
        EnvChangeTokenSubType::BeginTransaction | EnvChangeTokenSubType::EnlistDtcTransaction => {
            let new_value = read_u8_varbyte(buf)?;
            let new_descriptor = match new_value.len() {
                8 => Ok(LittleEndian::read_u64(&new_value)),
                _ => Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid new transaction descriptor",
                )),
            }?;
            let old_value = read_u8_varbyte(buf)?;
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
            let new_value = read_u8_varbyte(buf)?;
            let new_descriptor: u64 = match new_value.len() {
                0 => Ok(0u64),
                _ => Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid new transaction descriptor",
                )),
            }?;
            let old_value = read_u8_varbyte(buf)?;
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
            let new_value = read_u8_varbyte(buf)?;
            let new_descriptor: u64 = match new_value.len() {
                0 => Ok(0u64),
                _ => Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid new transaction descriptor",
                )),
            }?;
            let old_value = read_u8_varbyte(buf)?;
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
            let new_value = read_u8_varbyte(buf)?;
            let new_descriptor = match new_value.len() {
                8 => Ok(LittleEndian::read_u64(&new_value)),
                _ => Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid new transaction descriptor",
                )),
            }?;
            let old_value = read_u8_varbyte(buf)?;
            let old_descriptor = match old_value.len() {
                0 => Ok(0u64),
                _ => Err(crate::error::Error::ProtocolError(
                    "Invalid old transaction descriptor".to_string(),
                )),
            }?;
            EnvChangeContainer::from((old_descriptor, new_descriptor))
        }
        EnvChangeTokenSubType::DatabaseMirroringPartner => {
            let new_value = read_varchar_u8(buf)?;
            let old_value = read_varchar_u8(buf)?;
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
            let new_value = read_u8_varbyte(buf)?;
            let old_value = read_u8_varbyte(buf)?;
            EnvChangeContainer::from((old_value, new_value))
        }
        EnvChangeTokenSubType::UserInstanceName => {
            return Err(crate::error::Error::UnimplementedFeature {
                feature: "UserInstanceName".to_string(),
                context: "EnvChange token parsing not yet implemented".to_string(),
            });
        }
        EnvChangeTokenSubType::Routing => {
            let _length = buf.take_u16_le()?;
            let protocol = buf.take_u8()?;
            let port = buf.take_u16_le()?;
            let server = read_varchar_u16(buf)?;
            let routing_info = Some(RoutingInfo {
                protocol,
                port,
                server: server.unwrap_or_default(),
            });

            let mut old_routing_info: Option<RoutingInfo> = None;

            let old_length = buf.take_u16_le()?;
            if old_length > 0 {
                let old_protocol = buf.take_u8()?;
                let old_port = buf.take_u16_le()?;
                let old_server = read_varchar_u16(buf)?;

                old_routing_info = Some(RoutingInfo {
                    protocol: old_protocol,
                    port: old_port,
                    server: old_server.unwrap_or_default(),
                });
            }
            EnvChangeContainer::from((old_routing_info, routing_info))
        }
        EnvChangeTokenSubType::Unknown(_value) => {
            let new_value = read_varchar_u8(buf)?;
            let old_value = read_varchar_u8(buf)?;
            EnvChangeContainer::from((old_value, new_value))
        }
    };
    Ok(Tokens::from(EnvChangeToken {
        sub_type: token_sub_type,
        change_type: token_value_change,
    }))
}

/// Synchronous mirror of `TdsPacketReader::read_varchar_u16_length`: a u16
/// char-count prefix (`0xFFFF` == null) followed by that many UTF-16LE units.
fn read_varchar_u16(buf: &mut PacketBuffer) -> TdsResult<Option<String>> {
    let length: u16 = buf.take_u16_le()?;
    if length == PacketReader::LENGTHNULL {
        return Ok(None);
    }
    let string = read_unicode_with_byte_length(buf, (length << 1) as usize)?;
    Ok(Some(string))
}

/// Synchronous mirror of `TdsPacketReader::read_varchar_u8_length`: a u8
/// char-count prefix followed by that many UTF-16LE units.
fn read_varchar_u8(buf: &mut PacketBuffer) -> TdsResult<String> {
    let length: u8 = buf.take_u8()?;
    read_unicode_with_byte_length(buf, (length << 1) as usize)
}

/// Synchronous mirror of `TdsPacketReader::read_u8_varbyte`: a u8 length prefix
/// followed by that many raw bytes.
fn read_u8_varbyte(buf: &mut PacketBuffer) -> TdsResult<Vec<u8>> {
    let length: u8 = buf.take_u8()?;
    buf.take_bytes(length as usize)
}

/// Synchronous mirror of `TdsPacketReader::read_unicode_with_byte_length`.
fn read_unicode_with_byte_length(buf: &mut PacketBuffer, byte_length: usize) -> TdsResult<String> {
    const MAX_STRING_BYTE_LENGTH: usize = u8::MAX as usize * 2;
    if byte_length > MAX_STRING_BYTE_LENGTH {
        return Err(crate::error::Error::UsageError(format!(
            "Unicode string byte length {byte_length} exceeds maximum allowed size of {MAX_STRING_BYTE_LENGTH} bytes"
        )));
    }

    let byte_buffer = buf.take_bytes(byte_length)?;
    let mut u16_buffer = Vec::with_capacity(byte_buffer.len() / 2);
    for chunk in byte_buffer.chunks(2) {
        let value = u16::from_le_bytes([chunk[0], chunk[1]]);
        u16_buffer.push(value);
    }
    let string = String::from_utf16(&u16_buffer)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(string)
}
