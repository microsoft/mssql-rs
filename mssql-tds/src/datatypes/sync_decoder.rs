// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Column-atomic synchronous decode of non-PLP row cells.
//!
//! The row path suspends only at column boundaries. Within a column this module
//! runs entirely over an in-memory [`PacketBuffer`]: [`column_wire_len`] peeks
//! the length prefix (never consuming) to compute the total wire width, and once
//! the whole cell is buffered [`decode_column_body`] consumes it with infallible
//! `take_*` reads and performs exactly one `write_*`/`write_null`.
//!
//! Because the length computation only peeks, a `NeedBytes` shortfall lets the
//! driver re-run from the column start with nothing consumed and nothing
//! written — the atomicity that lets `RowPauseState.next_column_index` stay the
//! sole resume granularity, with no new pause/step machine.

use crate::core::TdsResult;
use crate::datatypes::column_values::{
    SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
    SqlSmallMoney, SqlTime,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::row_writer::RowWriter;
use crate::datatypes::sql_string::{SqlString, get_encoding_type};
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfoVariant};
use crate::error::Error;
use crate::io::packet_buffer::{NeedBytes, PacketBuffer};
use crate::query::metadata::ColumnMetadata;

/// Returns `true` when `meta` names a non-PLP type whose cell decode is owned by
/// this synchronous path. Callers must route PLP cells and any unsupported type
/// to the legacy async decoder instead.
///
/// The set grows as families are ported; it is the single gate that keeps the
/// sync driver and [`decode_column_body`] in lockstep.
pub(crate) fn is_supported(meta: &ColumnMetadata) -> bool {
    if meta.is_plp() {
        return false;
    }
    matches!(
        meta.data_type,
        TdsDataType::Int1
            | TdsDataType::Int2
            | TdsDataType::Int4
            | TdsDataType::Int8
            | TdsDataType::IntN
            | TdsDataType::Flt4
            | TdsDataType::Flt8
            | TdsDataType::FltN
            | TdsDataType::Bit
            | TdsDataType::BitN
            | TdsDataType::Money4
            | TdsDataType::Money
            | TdsDataType::MoneyN
            | TdsDataType::DecimalN
            | TdsDataType::NumericN
            | TdsDataType::DateTime
            | TdsDataType::DateTim4
            | TdsDataType::DateTimeN
            | TdsDataType::DateN
            | TdsDataType::TimeN
            | TdsDataType::DateTime2N
            | TdsDataType::DateTimeOffsetN
            | TdsDataType::Guid
            // Variable-length non-PLP strings (USHORT length prefix, 0xFFFF = NULL).
            // Long-length LOB types (Text/NText) keep the legacy async path.
            | TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::NChar
            | TdsDataType::NVarChar
            // Variable-length non-PLP binary (USHORT length prefix, 0xFFFF = NULL).
            | TdsDataType::BigBinary
            | TdsDataType::BigVarBinary
    )
}

/// Total wire byte width of the cell for `meta`, including any length prefix.
///
/// Peeks (never consumes) the 1-byte length prefix of `N`/scale types; fixed
/// types have no prefix. Returns `NeedBytes` when the prefix itself is not yet
/// buffered, so the caller refills and re-drives from the column start.
///
/// Only valid for types where [`is_supported`] returns `true`; other types hit
/// the `unreachable!` guard, which indicates a routing bug at the call site.
pub(crate) fn column_wire_len(
    buf: &PacketBuffer,
    meta: &ColumnMetadata,
) -> Result<usize, NeedBytes> {
    let fixed = match meta.data_type {
        TdsDataType::Int1 | TdsDataType::Bit => Some(1),
        TdsDataType::Int2 => Some(2),
        TdsDataType::Int4 | TdsDataType::Flt4 | TdsDataType::Money4 | TdsDataType::DateTim4 => {
            Some(4)
        }
        TdsDataType::Int8 | TdsDataType::Flt8 | TdsDataType::Money | TdsDataType::DateTime => {
            Some(8)
        }
        _ => None,
    };
    if let Some(width) = fixed {
        return Ok(width);
    }

    // Variable-length non-PLP strings and binary are 2-byte (USHORT) length
    // prefixed; 0xFFFF is the NULL marker (no body). The prefix width is the
    // absolute byte count needed to compute the total, so a shortfall re-drives
    // ensure() for the whole prefix.
    if matches!(
        meta.data_type,
        TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigBinary
            | TdsDataType::BigVarBinary
    ) {
        return match buf.peek_bytes(2) {
            Some(prefix) => {
                let length = u16::from_le_bytes([prefix[0], prefix[1]]);
                if length == 0xFFFF {
                    Ok(2)
                } else {
                    Ok(2 + length as usize)
                }
            }
            None => Err(NeedBytes { shortfall: 2 }),
        };
    }

    // Every remaining supported type is 1-byte length-prefixed; the prefix value
    // is the body width, so the total is 1 + prefix.
    match buf.peek_bytes(1) {
        Some(prefix) => Ok(1 + prefix[0] as usize),
        None => Err(NeedBytes { shortfall: 1 }),
    }
}

/// Decodes the whole cell for `meta` from `buf`, which is guaranteed to hold at
/// least [`column_wire_len`] bytes. Every `take_*` is therefore infallible for
/// buffer reasons; the only errors returned are genuine protocol violations
/// (bad `IntN`/`MoneyN`/`GUID` lengths, missing scale metadata, etc.). Performs
/// exactly one `write_*`/`write_null`.
pub(crate) fn decode_column_body(
    buf: &mut PacketBuffer,
    meta: &ColumnMetadata,
    col: usize,
    writer: &mut (dyn RowWriter + Send),
) -> TdsResult<()> {
    match meta.data_type {
        // === Fixed-length integer types ===
        TdsDataType::Int1 => writer.write_u8(col, buf.take_u8()?),
        TdsDataType::Int2 => writer.write_i16(col, buf.take_i16_le()?),
        TdsDataType::Int4 => writer.write_i32(col, buf.take_i32_le()?),
        TdsDataType::Int8 => writer.write_i64(col, buf.take_i64_le()?),
        TdsDataType::IntN => match buf.take_u8()? {
            0 => writer.write_null(col),
            1 => writer.write_u8(col, buf.take_u8()?),
            2 => writer.write_i16(col, buf.take_i16_le()?),
            4 => writer.write_i32(col, buf.take_i32_le()?),
            8 => writer.write_i64(col, buf.take_i64_le()?),
            other => {
                return Err(Error::ProtocolError(format!(
                    "Invalid IntN length - {other}"
                )));
            }
        },

        // === Fixed-length float types ===
        TdsDataType::Flt4 => writer.write_f32(col, buf.take_f32_le()?),
        TdsDataType::Flt8 => writer.write_f64(col, buf.take_f64_le()?),
        TdsDataType::FltN => match buf.take_u8()? {
            0 => writer.write_null(col),
            4 => writer.write_f32(col, buf.take_f32_le()?),
            _ => writer.write_f64(col, buf.take_f64_le()?),
        },

        // === Bit types ===
        TdsDataType::Bit => writer.write_bool(col, buf.take_u8()? == 1),
        TdsDataType::BitN => {
            if buf.take_u8()? > 0 {
                writer.write_bool(col, buf.take_u8()? == 1);
            } else {
                writer.write_null(col);
            }
        }

        // === Money types ===
        TdsDataType::Money4 => writer.write_smallmoney(col, take_money4(buf)?),
        TdsDataType::Money => writer.write_money(col, take_money8(buf)?),
        TdsDataType::MoneyN => match buf.take_u8()? {
            0 => writer.write_null(col),
            4 => writer.write_smallmoney(col, take_money4(buf)?),
            8 => writer.write_money(col, take_money8(buf)?),
            other => {
                return Err(Error::ProtocolError(format!(
                    "Invalid MoneyN length - {other}"
                )));
            }
        },

        // === Decimal / Numeric ===
        TdsDataType::DecimalN => match take_decimal(buf, meta)? {
            Some(val) => writer.write_decimal(col, val),
            None => writer.write_null(col),
        },
        TdsDataType::NumericN => match take_decimal(buf, meta)? {
            Some(val) => writer.write_numeric(col, val),
            None => writer.write_null(col),
        },

        // === DateTime types ===
        TdsDataType::DateTime => writer.write_datetime(col, take_datetime(buf)?),
        TdsDataType::DateTim4 => writer.write_smalldatetime(col, take_small_datetime(buf)?),
        TdsDataType::DateTimeN => match buf.take_u8()? {
            0 => writer.write_null(col),
            4 => writer.write_smalldatetime(col, take_small_datetime(buf)?),
            _ => writer.write_datetime(col, take_datetime(buf)?),
        },
        TdsDataType::DateN => {
            if buf.take_u8()? == 0 {
                writer.write_null(col);
            } else {
                writer.write_date(col, take_date(buf)?);
            }
        }
        TdsDataType::TimeN => {
            let length = buf.take_u8()?;
            if length == 0 {
                writer.write_null(col);
            } else {
                writer.write_time(col, take_time(buf, length, scale_of(meta, "TimeN")?)?);
            }
        }
        TdsDataType::DateTime2N => {
            let length = buf.take_u8()?;
            if length == 0 {
                writer.write_null(col);
            } else {
                writer.write_datetime2(
                    col,
                    take_datetime2(buf, length, scale_of(meta, "DateTime2N")?)?,
                );
            }
        }
        TdsDataType::DateTimeOffsetN => {
            let length = buf.take_u8()?;
            if length == 0 {
                writer.write_null(col);
            } else {
                writer.write_datetimeoffset(
                    col,
                    take_datetime_offset(buf, length, scale_of(meta, "DateTimeOffsetN")?)?,
                );
            }
        }

        // === GUID ===
        TdsDataType::Guid => {
            let length = buf.take_u8()?;
            if length == 0 {
                writer.write_null(col);
            } else if length == 16 {
                let mut bytes = [0u8; 16];
                for slot in bytes.iter_mut() {
                    *slot = buf.take_u8()?;
                }
                let uuid = uuid::Uuid::from_slice_le(&bytes)
                    .map_err(|e| Error::ProtocolError(format!("Failed to parse UUID: {e}")))?;
                writer.write_uuid(col, uuid);
            } else {
                return Err(Error::ProtocolError(format!(
                    "Invalid GUID length: expected 16 bytes, got {length}"
                )));
            }
        }

        // === Variable-length non-PLP strings (USHORT prefix, 0xFFFF = NULL) ===
        TdsDataType::Char
        | TdsDataType::VarChar
        | TdsDataType::BigChar
        | TdsDataType::BigVarChar
        | TdsDataType::NChar
        | TdsDataType::NVarChar => {
            let length = buf.take_u16_le()?;
            if length == 0xFFFF {
                writer.write_null(col);
            } else {
                let bytes = take_bytes(buf, length as usize);
                writer.write_string(col, SqlString::new(bytes, get_encoding_type(meta)));
            }
        }

        // === Variable-length non-PLP binary (USHORT prefix, 0xFFFF = NULL) ===
        TdsDataType::BigBinary | TdsDataType::BigVarBinary => {
            let length = buf.take_u16_le()?;
            if length == 0xFFFF {
                writer.write_null(col);
            } else {
                writer.write_bytes(col, take_bytes(buf, length as usize));
            }
        }

        other => {
            unreachable!("decode_column_body called for unsupported type {other:?}");
        }
    }
    Ok(())
}

fn scale_of(meta: &ColumnMetadata, type_name: &str) -> TdsResult<u8> {
    meta.get_scale()
        .ok_or_else(|| Error::ImplementationError(format!("{type_name} type should have scale")))
}

/// Consumes exactly `n` bytes from `buf`. The column driver guarantees the whole
/// cell is buffered before calling into the body, so the copy always fills.
fn take_bytes(buf: &mut PacketBuffer, n: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; n];
    let copied = buf.copy_out(&mut bytes);
    debug_assert_eq!(
        copied, n,
        "cell not fully buffered before decode_column_body"
    );
    bytes
}

fn take_money4(buf: &mut PacketBuffer) -> TdsResult<SqlSmallMoney> {
    Ok(buf.take_i32_le()?.into())
}

fn take_money8(buf: &mut PacketBuffer) -> TdsResult<SqlMoney> {
    let msb = buf.take_i32_le()?;
    let lsb = buf.take_i32_le()?;
    Ok(SqlMoney {
        lsb_part: lsb,
        msb_part: msb,
    })
}

fn take_datetime(buf: &mut PacketBuffer) -> TdsResult<SqlDateTime> {
    let days = buf.take_i32_le()?;
    let ticks = buf.take_u32_le()?;
    Ok(SqlDateTime { days, time: ticks })
}

fn take_small_datetime(buf: &mut PacketBuffer) -> TdsResult<SqlSmallDateTime> {
    let days = buf.take_u16_le()?;
    let minutes = buf.take_u16_le()?;
    Ok(SqlSmallDateTime {
        days,
        time: minutes,
    })
}

fn take_date(buf: &mut PacketBuffer) -> TdsResult<SqlDate> {
    Ok(SqlDate::unchecked_create(buf.take_u24_le()?))
}

fn take_time(buf: &mut PacketBuffer, byte_len: u8, scale: u8) -> TdsResult<SqlTime> {
    let scaled_value = match byte_len {
        3 => buf.take_u24_le()? as u64,
        4 => buf.take_u32_le()? as u64,
        _ => buf.take_uint40_le()?,
    };
    let time_nanoseconds = match scale {
        0 => scaled_value * 10_000_000,
        1 => scaled_value * 1_000_000,
        2 => scaled_value * 100_000,
        3 => scaled_value * 10_000,
        4 => scaled_value * 1_000,
        5 => scaled_value * 100,
        6 => scaled_value * 10,
        _ => scaled_value,
    };
    Ok(SqlTime {
        time_nanoseconds,
        scale,
    })
}

fn take_datetime2(buf: &mut PacketBuffer, byte_len: u8, scale: u8) -> TdsResult<SqlDateTime2> {
    let time_byte_len = byte_len.checked_sub(3).ok_or_else(|| {
        Error::ProtocolError(format!(
            "Invalid DateTime2 byte length: {byte_len}. Expected at least 3 bytes for date component."
        ))
    })?;
    let time = take_time(buf, time_byte_len, scale)?;
    let date = take_date(buf)?;
    Ok(SqlDateTime2 {
        days: date.get_days(),
        time,
    })
}

fn take_datetime_offset(
    buf: &mut PacketBuffer,
    byte_len: u8,
    scale: u8,
) -> TdsResult<SqlDateTimeOffset> {
    let datetime2_byte_len = byte_len.checked_sub(2).ok_or_else(|| {
        Error::ProtocolError(format!(
            "Invalid DateTimeOffset byte length: {byte_len}. Expected at least 2 bytes for offset component."
        ))
    })?;
    let datetime2 = take_datetime2(buf, datetime2_byte_len, scale)?;
    let offset = buf.take_i16_le()?;
    Ok(SqlDateTimeOffset { datetime2, offset })
}

fn take_decimal(buf: &mut PacketBuffer, meta: &ColumnMetadata) -> TdsResult<Option<DecimalParts>> {
    let length = buf.take_u8()?;
    let TypeInfoVariant::VarLenPrecisionScale(_, _, precision, scale) =
        meta.type_info.type_info_variant
    else {
        return Err(Error::ProtocolError(format!(
            "Invalid type info variant for Decimal/Numeric: expected VarLenPrecisionScale, got: {:?}",
            meta.type_info.type_info_variant
        )));
    };
    if length == 0 {
        return Ok(None);
    }
    let sign = buf.take_u8()?;
    let is_positive = sign == 1;
    let number_of_int_parts = (length - 1) >> 2;

    #[cfg(fuzzing)]
    const MAX_DECIMAL_INT_PARTS: u8 = 10;
    #[cfg(not(fuzzing))]
    const MAX_DECIMAL_INT_PARTS: u8 = 64;

    if number_of_int_parts > MAX_DECIMAL_INT_PARTS {
        return Err(Error::ProtocolError(format!(
            "Decimal int parts {number_of_int_parts} exceeds maximum allowed {MAX_DECIMAL_INT_PARTS} (length was {length})"
        )));
    }

    let mut int_parts = vec![0i32; number_of_int_parts as usize];
    for part in int_parts.iter_mut() {
        *part = buf.take_i32_le()?;
    }

    Ok(Some(DecimalParts {
        is_positive,
        scale,
        precision,
        int_parts,
    }))
}
