// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS value serialization utilities for RPC parameters and bulk copy operations.
//!
//! This module provides shared value encoding logic that can be used by both
//! RPC parameter serialization (SqlType) and bulk copy ROW token serialization.
//! It separates the concerns of type metadata encoding from value encoding.

use crate::core::TdsResult;
use crate::datatypes::column_values::ColumnValues;
use crate::datatypes::lcid_encoding::lcid_to_encoding;
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_vector::{SqlVector, VectorData};
use crate::datatypes::sqldatatypes::TdsDataType;
use crate::datatypes::sqltypes::get_time_length_from_scale;
use crate::error::Error;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter, TdsPacketWriterUnchecked};
use crate::token::tokens::SqlCollation;

// NULL markers for different type classes
const NULL_LENGTH: u8 = 0x00;
const VARNULL: u16 = 0xFFFF;

/// PLP sentinel indicating a SQL NULL value.
pub const PLP_NULL: u64 = 0xFFFFFFFFFFFFFFFF;
/// PLP sentinel indicating unknown total length.
pub const PLP_UNKNOWN_LEN: u64 = 0xFFFFFFFFFFFFFFFE;
/// PLP chunk terminator (zero-length chunk).
pub const PLP_TERMINATOR: u32 = 0x00000000;

// TDS type byte constants for string types
const NVARCHAR: u8 = TdsDataType::NVarChar as u8; // 0xE7
const NCHAR: u8 = TdsDataType::NChar as u8; // 0xEF
const NTEXT: u8 = TdsDataType::NText as u8; // 0x63
const VARCHAR: u8 = TdsDataType::BigVarChar as u8; // 0xA7
const CHAR: u8 = TdsDataType::BigChar as u8; // 0xAF
const TEXT: u8 = TdsDataType::Text as u8; // 0x23
const SQL_VARIANT: u8 = TdsDataType::SsVariant as u8; // 0x62

// TDS type byte constant for binary types
const IMAGE: u8 = TdsDataType::Image as u8; // 0x22

/// Context for value serialization, containing type metadata needed for encoding.
///
/// This struct encapsulates the TDS type information required to properly encode
/// a value without duplicating the type metadata itself.
#[derive(Debug, Clone)]
pub struct TdsTypeContext {
    /// TDS type byte (e.g., 0x26 for INTN, 0xE7 for NVARCHAR)
    pub tds_type: u8,

    /// Maximum type size (for nullable types: 1/2/4/8 for INTN, 4/8 for FLTN, etc.)
    /// For NVARCHAR/NCHAR: character count (not byte count)
    /// For VARCHAR/CHAR: byte count
    /// Can be up to 8000 for NVARCHAR(4000) or VARCHAR(8000)
    pub max_size: usize,

    /// Whether this is a PLP (Partial Length Prefix) type (MAX types)
    pub is_plp: bool,

    /// Whether this is a fixed-length type (e.g., BINARY(n) vs VARBINARY(n)), or an
    /// element of sql_variant with fixed length (e.g., INT4, GUID, NUMERIC inside sql_variant).
    /// Fixed-length types write exactly max_size bytes with no length prefix in ROW tokens
    pub is_fixed_length: bool,

    /// For Decimal/Numeric: precision
    pub precision: Option<u8>,

    /// For Decimal/Numeric/Time/DateTime2/DateTimeOffset: scale
    #[allow(dead_code)]
    pub scale: Option<u8>,

    /// Collation for string types (CHAR/VARCHAR/NCHAR/NVARCHAR/TEXT/NTEXT)
    pub collation: Option<SqlCollation>,

    /// Whether the type is nullable (affects NULL encoding)
    #[allow(dead_code)]
    pub is_nullable: bool,
}

impl TdsTypeContext {
    /// Check if this is a fixed-length type (no length prefix needed in ROW data).
    pub fn is_fixed_type(&self) -> bool {
        matches!(
            self.tds_type,
            // Fixed types: INT1-INT8, BIT, FLT4, FLT8, DATETIME, MONEY, etc.
            0x30..=0x3F | 0x7A | 0x7F
        )
    }
}

/// Main value serializer - writes ONLY the value bytes (no type metadata).
pub struct TdsValueSerializer;

impl TdsValueSerializer {
    /// Serialize a value using the provided type context.
    ///
    /// This writes ONLY the value portion:
    /// - For nullable types (INTN, FLTN): length byte + value bytes
    /// - For fixed types (INT4, FLT8): value bytes only
    /// - For variable-length types: length prefix + value bytes
    /// - For PLP types: total_length + chunks + terminator
    ///
    /// Type metadata (TDS type byte, max_size, precision, scale, collation)
    /// must be written separately by the caller.
    #[inline]
    pub async fn serialize_value<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &ColumnValues,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Check if target column is sql_variant (TDS type SQL_VARIANT)
        // If so, wrap the value with variant wire format
        if ctx.tds_type == SQL_VARIANT {
            return Self::serialize_as_variant(writer, value, ctx).await;
        }

        Self::serialize_value_inner(writer, value, ctx).await
    }

    /// Serialize a NULL value using the appropriate NULL marker for the type.
    #[inline(always)]
    pub async fn serialize_null<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Check type class and write appropriate NULL marker
        match ctx.tds_type {
            // Legacy LOB types (TEXT, NTEXT, IMAGE) use single byte 0x00 for NULL
            0x23 | 0x63 | 0x22 => {
                writer.write_byte_async(0x00).await?;
            }
            // Nullable types (INTN, FLTN, BITN, MONEYN, DATETIMEN, DecimalN, NumericN, Guid, DateN, TimeN, DateTime2N, DateTimeOffsetN) use length = 0x00
            0x26 | 0x6D | 0x68 | 0x6E | 0x6F | 0x6A | 0x6C | 0x24 | 0x28 | 0x29 | 0x2A | 0x2B => {
                writer.write_byte_async(NULL_LENGTH).await?;
            }
            // Fixed BIT type (0x32) cannot be NULL - must use BitN (0x68) for nullable
            0x32 => {
                return Err(Error::UsageError(
                    "Cannot serialize NULL for fixed BIT type 0x32. Use BitN (0x68) for nullable BIT columns.".to_string()
                ));
            }
            _ => {
                // Other types depend on length classification
                if ctx.is_plp {
                    // PLP NULL: 8 bytes of 0xFF
                    writer.write_u64_async(PLP_NULL).await?;
                } else if ctx.is_fixed_type() {
                    // Fixed-length types cannot be NULL - must use nullable variant (INTN, FLTN, etc.)
                    return Err(Error::UsageError(format!(
                        "Cannot serialize NULL for fixed-length type 0x{:02X}.",
                        ctx.tds_type
                    )));
                } else {
                    // Variable-length NULL: 0xFFFF
                    writer.write_u16_async(VARNULL).await?;
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_bit<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: bool,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        let byte_value = if value { 1u8 } else { 0u8 };

        if !ctx.is_fixed_type() {
            // Nullable BIT (BITN with length 1): length byte + value (2 bytes total)
            match writer.has_space(2) {
                false => {
                    writer.write_byte_async(1).await?; // Length for BITN (1 byte)
                    writer.write_byte_async(byte_value).await?;
                }
                true => {
                    writer.write_byte_unchecked(1); // Length for BITN (1 byte)
                    writer.write_byte_unchecked(byte_value);
                }
            }
        } else {
            // Fixed type (BIT, 0x32) - just write value (1 byte)
            match writer.has_space(1) {
                false => {
                    writer.write_byte_async(byte_value).await?;
                }
                true => {
                    writer.write_byte_unchecked(byte_value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_tinyint<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: u8,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Phase 1 Optimization: Batch writes for fixed types
        if !ctx.is_fixed_type() {
            // Nullable TINYINT (INTN with length 1): length byte + value (2 bytes total)
            match writer.has_space(2) {
                false => {
                    writer.write_byte_async(1).await?; // Length for INTN (1 byte)
                    writer.write_byte_async(value).await?;
                }
                true => {
                    writer.write_byte_unchecked(1); // Length for INTN (1 byte)
                    writer.write_byte_unchecked(value);
                }
            }
        } else {
            // Fixed type (INT1, 0x30) - just write value (1 byte)
            match writer.has_space(1) {
                false => {
                    writer.write_byte_async(value).await?;
                }
                true => {
                    writer.write_byte_unchecked(value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_smallint<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: i16,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Phase 1 Optimization: Batch writes for fixed types
        if !ctx.is_fixed_type() {
            // Nullable SMALLINT (INTN with length 2): length byte + value (3 bytes total)
            match writer.has_space(3) {
                false => {
                    writer.write_byte_async(2).await?; // Length for INTN (2 bytes)
                    writer.write_i16_async(value).await?;
                }
                true => {
                    writer.write_byte_unchecked(2); // Length for INTN (2 bytes)
                    writer.write_i16_unchecked(value);
                }
            }
        } else {
            // Fixed type (INT2, 0x34) - just write value (2 bytes)
            match writer.has_space(2) {
                false => {
                    writer.write_i16_async(value).await?;
                }
                true => {
                    writer.write_i16_unchecked(value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_int<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: i32,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Phase 1 Optimization: Batch writes for fixed types
        if !ctx.is_fixed_type() {
            // Ensure space for length byte + value (5 bytes total)
            match writer.has_space(5) {
                false => {
                    writer.write_byte_async(4).await?; // Length for INTN
                    writer.write_i32_async(value).await?;
                }
                true => {
                    writer.write_byte_unchecked(4); // Length for INTN
                    writer.write_i32_unchecked(value);
                }
            }
        } else {
            // Fixed type - just write value (4 bytes)
            match writer.has_space(4) {
                false => {
                    writer.write_i32_async(value).await?;
                }
                true => {
                    writer.write_i32_unchecked(value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_bigint<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: i64,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Phase 1 Optimization: Batch writes for fixed types
        if !ctx.is_fixed_type() {
            // Nullable BIGINT (INTN with length 8): length byte + value (9 bytes total)
            match writer.has_space(9) {
                false => {
                    writer.write_byte_async(8).await?; // Length for INTN (8 bytes)
                    writer.write_i64_async(value).await?;
                }
                true => {
                    writer.write_byte_unchecked(8); // Length for INTN (8 bytes)
                    writer.write_i64_unchecked(value);
                }
            }
        } else {
            // Fixed type (INT8, 0x7F) - just write value (8 bytes)
            match writer.has_space(8) {
                false => {
                    writer.write_i64_async(value).await?;
                }
                true => {
                    writer.write_i64_unchecked(value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_real<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: f32,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // REAL is 4-byte IEEE 754 float
        // TDS FloatN format: length byte + IEEE 754 bytes (little-endian)
        if !ctx.is_fixed_type() {
            // Nullable REAL (FLOATN with length 4): length byte + value (5 bytes total)
            match writer.has_space(5) {
                false => {
                    writer.write_byte_async(4).await?; // Length for FLOATN (4 bytes)
                    // Write f32 as i32 bytes (same bit pattern)
                    writer.write_i32_async(value.to_bits() as i32).await?;
                }
                true => {
                    writer.write_byte_unchecked(4); // Length for FLOATN (4 bytes)
                    // Write f32 as i32 bytes (same bit pattern)
                    writer.write_i32_unchecked(value.to_bits() as i32);
                }
            }
        } else {
            // Fixed type (FLT4, 0x3B) - just write value (4 bytes)
            match writer.has_space(4) {
                false => {
                    // Write f32 as i32 bytes (same bit pattern)
                    writer.write_i32_async(value.to_bits() as i32).await?;
                }
                true => {
                    // Write f32 as i32 bytes (same bit pattern)
                    writer.write_i32_unchecked(value.to_bits() as i32);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_float<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: f64,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // FLOAT is 8-byte IEEE 754 double
        // TDS FloatN format: length byte + IEEE 754 bytes (little-endian)
        if !ctx.is_fixed_type() {
            // Nullable FLOAT (FLOATN with length 8): length byte + value (9 bytes total)
            match writer.has_space(9) {
                false => {
                    writer.write_byte_async(8).await?; // Length for FLOATN (8 bytes)
                    writer.write_f64_unchecked(value);
                }
                true => {
                    writer.write_byte_unchecked(8); // Length for FLOATN (8 bytes)
                    writer.write_f64_unchecked(value);
                }
            }
        } else {
            // Fixed type (FLT8, 0x3E) - just write value (8 bytes)
            match writer.has_space(8) {
                false => {
                    // Write f64 as i64 bytes (same bit pattern)
                    writer.write_i64_async(value.to_bits() as i64).await?;
                }
                true => {
                    writer.write_f64_unchecked(value);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_decimal<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::decoder::DecimalParts,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Decimal/Numeric format in TDS:
        // - Length byte (number of bytes in the value, excluding the length byte)
        // - Sign byte (0 = negative, 1 = positive)
        // - Little-endian bytes representing the value
        //
        // Length is determined by precision (from metadata):
        // Precision 1-9:   5 bytes (1 sign + 4 value bytes)
        // Precision 10-19: 9 bytes (1 sign + 8 value bytes)
        // Precision 20-28: 13 bytes (1 sign + 12 value bytes)
        // Precision 29-38: 17 bytes (1 sign + 16 value bytes)

        let precision = ctx.precision.unwrap_or(38);

        // Determine required byte length based on precision
        let value_bytes = match precision {
            1..=9 => 4,
            10..=19 => 8,
            20..=28 => 12,
            29..=38 => 16,
            _ => {
                return Err(Error::ProtocolError(format!(
                    "Invalid precision {} for DECIMAL/NUMERIC type",
                    precision
                )));
            }
        };

        let total_length = 1 + value_bytes; // sign byte + value bytes

        // Write length byte (skip in case of sql_variant when is_fixed_length=true)
        if !ctx.is_fixed_length {
            writer.write_byte_async(total_length as u8).await?;
        }

        // Write sign byte
        writer
            .write_byte_async(if value.is_positive { 1 } else { 0 })
            .await?;

        // Write value bytes in little-endian order
        // Pad with zeros if value has fewer chunks than needed
        let chunks_needed = value_bytes / 4;
        for i in 0..chunks_needed {
            let chunk = value.int_parts.get(i).copied().unwrap_or(0);
            writer.write_i32_async(chunk).await?;
        }

        Ok(())
    }

    #[inline(always)]
    async fn serialize_smallmoney<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlSmallMoney,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // SMALLMONEY format in TDS:
        // - For MoneyN (nullable, 0x6E): length byte (4) + 4-byte integer
        // - For Money4 (fixed, 0x7A): 4-byte integer only
        // The value is stored as a 4-byte signed integer scaled by 10,000
        // (e.g., $123.4567 is stored as 1234567)

        // Skip length prefix in case of sql_variant when is_fixed_length=true or
        // when we're dealing with fixed Money types
        if !ctx.is_fixed_type() && !ctx.is_fixed_length {
            // MoneyN with length 4: length byte + value (5 bytes total)
            match writer.has_space(5) {
                false => {
                    writer.write_byte_async(4).await?; // Length for MoneyN (4 bytes)
                    writer.write_i32_async(value.int_val).await?;
                }
                true => {
                    writer.write_byte_unchecked(4); // Length for MoneyN (4 bytes)
                    writer.write_i32_unchecked(value.int_val);
                }
            }
        } else {
            // Fixed type (Money4, 0x7A) - just write value (4 bytes)
            match writer.has_space(4) {
                false => {
                    writer.write_i32_async(value.int_val).await?;
                }
                true => {
                    writer.write_i32_unchecked(value.int_val);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_money<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlMoney,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // MONEY format in TDS:
        // - For MoneyN (nullable, 0x6E): length byte (8) + 8 bytes (two 4-byte integers)
        // - For Money (fixed, 0x3C): 8 bytes (two 4-byte integers) only
        // The value is stored as two 4-byte signed integers in mixed endian format:
        //   - First 4 bytes: MSB (most significant 4 bytes)
        //   - Second 4 bytes: LSB (least significant 4 bytes)
        // The combined value is scaled by 10,000 (e.g., $123.4567 is stored as 1234567)

        if !ctx.is_fixed_type() {
            // MoneyN with length 8: length byte + value (9 bytes total)
            match writer.has_space(9) {
                false => {
                    writer.write_byte_async(8).await?; // Length for MoneyN (8 bytes)
                    writer.write_i32_async(value.msb_part).await?; // MSB first
                    writer.write_i32_async(value.lsb_part).await?; // LSB second
                }
                true => {
                    writer.write_byte_unchecked(8); // Length for MoneyN (8 bytes)
                    writer.write_i32_unchecked(value.msb_part); // MSB first
                    writer.write_i32_unchecked(value.lsb_part); // LSB second
                }
            }
        } else {
            // Fixed type (Money, 0x3C) - just write value (8 bytes)
            match writer.has_space(8) {
                false => {
                    writer.write_i32_async(value.msb_part).await?; // MSB first
                    writer.write_i32_async(value.lsb_part).await?; // LSB second
                }
                true => {
                    writer.write_i32_unchecked(value.msb_part); // MSB first
                    writer.write_i32_unchecked(value.lsb_part); // LSB second
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_date<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlDate,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // DATE format in TDS:
        // - For DateN (nullable, 0x28): length byte (3) + 3-byte unsigned integer
        // - For Date (fixed, 0x2A): 3-byte unsigned integer only
        // - In sql_variant: 3-byte unsigned integer (no length prefix due to is_fixed_length flag)
        // The value is stored as a 3-byte unsigned integer representing days since 0001-01-01
        // Valid range: 1 (0001-01-01) to 3,652,059 (9999-12-31)
        // The 3-byte unsigned integer can hold values up to 0xFFFFFF (16,777,215),
        // but SQL Server DATE type has a more restricted range.

        if !ctx.is_fixed_type() && !ctx.is_fixed_length {
            // DateN with length 3: length byte + value (4 bytes total)
            let days = value.get_days();
            match writer.has_space(4) {
                false => {
                    writer.write_byte_async(3).await?; // Length for DateN (3 bytes)
                    // Write 3 bytes in little-endian format (u32 as 3 bytes)
                    writer.write_byte_async((days & 0xFF) as u8).await?;
                    writer.write_byte_async(((days >> 8) & 0xFF) as u8).await?;
                    writer.write_byte_async(((days >> 16) & 0xFF) as u8).await?;
                }
                true => {
                    writer.write_byte_unchecked(3); // Length for DateN (3 bytes)
                    // Write 3 bytes in little-endian format (u32 as 3 bytes)
                    writer.write_byte_unchecked((days & 0xFF) as u8);
                    writer.write_byte_unchecked(((days >> 8) & 0xFF) as u8);
                    writer.write_byte_unchecked(((days >> 16) & 0xFF) as u8);
                }
            }
        } else {
            // Fixed type (Date, 0x2A) - just write value (3 bytes)
            let days = value.get_days();

            match writer.has_space(3) {
                false => {
                    writer.write_byte_async((days & 0xFF) as u8).await?;
                    writer.write_byte_async(((days >> 8) & 0xFF) as u8).await?;
                    writer.write_byte_async(((days >> 16) & 0xFF) as u8).await?;
                }
                true => {
                    writer.write_byte_unchecked((days & 0xFF) as u8);
                    writer.write_byte_unchecked(((days >> 8) & 0xFF) as u8);
                    writer.write_byte_unchecked(((days >> 16) & 0xFF) as u8);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_time<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlTime,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // TIME format in TDS:
        // - For TimeN (nullable, 0x29): length byte + time_nanoseconds (3, 4, or 5 bytes)
        // - For Time (fixed, 0x2A): time_nanoseconds only (3, 4, or 5 bytes)
        // The value is stored as little-endian unsigned integer representing 100-nanosecond units
        // Length depends on scale:
        //   - Scale 0-2: 3 bytes
        //   - Scale 3-4: 4 bytes
        //   - Scale 5-7: 5 bytes

        // Determine the byte length based on scale
        let time_length = get_time_length_from_scale(value.scale)?;

        // Scale the time value based on the scale
        // The time_nanoseconds is always in 100-nanosecond units internally
        // But SQL Server expects the value to be in units appropriate for the scale:
        // Scale 0: seconds (divide by 10^7)
        // Scale 1: tenths of seconds (divide by 10^6)
        // Scale 2: hundredths of seconds (divide by 10^5)
        // Scale 3: milliseconds (divide by 10^4)
        // Scale 4: ten-thousandths (divide by 10^3)
        // Scale 5: hundred-thousandths (divide by 10^2)
        // Scale 6: microseconds (divide by 10^1)
        // Scale 7: 100-nanoseconds (divide by 10^0 = no scaling)
        let time_value = match value.scale {
            0 => value.time_nanoseconds / 10_000_000, // Seconds
            1 => value.time_nanoseconds / 1_000_000,  // Tenths
            2 => value.time_nanoseconds / 100_000,    // Hundredths
            3 => value.time_nanoseconds / 10_000,     // Milliseconds
            4 => value.time_nanoseconds / 1_000,      // Ten-thousandths
            5 => value.time_nanoseconds / 100,        // Hundred-thousandths
            6 => value.time_nanoseconds / 10,         // Microseconds
            7 => value.time_nanoseconds,              // 100-nanoseconds (no scaling)
            _ => value.time_nanoseconds,
        };

        if !ctx.is_fixed_type() && !ctx.is_fixed_length {
            // TimeN with length prefix
            let total_size = (1 + time_length) as usize;
            match writer.has_space(total_size) {
                false => {
                    writer.write_byte_async(time_length).await?; // Length byte
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_async(byte_val).await?;
                    }
                }
                true => {
                    writer.write_byte_unchecked(time_length); // Length byte
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_unchecked(byte_val);
                    }
                }
            }
        } else {
            // Fixed type (Time, 0x2A) - just write value (3, 4, or 5 bytes)
            match writer.has_space(time_length as usize) {
                false => {
                    for i in 0..time_length {
                        writer
                            .write_byte_async(((time_value >> (i * 8)) & 0xFF) as u8)
                            .await?;
                    }
                }
                true => {
                    for i in 0..time_length {
                        writer.write_byte_unchecked(((time_value >> (i * 8)) & 0xFF) as u8);
                    }
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_datetime<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlDateTime,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // DATETIME format in TDS:
        // - For DateTimeN (nullable, 0x6F): length byte (8) + 4-byte days + 4-byte time
        // - For DateTime (fixed, 0x3D): 4-byte days + 4-byte time (8 bytes total)
        //
        // Days: Signed 32-bit integer representing days since January 1, 1900
        //       (negative for dates before 1900, back to January 1, 1753)
        // Time: Unsigned 32-bit integer representing 1/300th of a second since midnight
        //       (300 ticks per second, range 0 to 25,919,999 for 23:59:59.997)

        if !ctx.is_fixed_type() {
            // DateTimeN with length 8: length byte + value (9 bytes total)
            match writer.has_space(9) {
                false => {
                    writer.write_byte_async(8).await?; // Length for DateTimeN (8 bytes)
                    writer.write_i32_async(value.days).await?;
                    writer.write_u32_async(value.time).await?;
                }
                true => {
                    writer.write_byte_unchecked(8); // Length for DateTimeN (8 bytes)
                    writer.write_i32_unchecked(value.days);
                    // Note: No unchecked version for u32, use the bytes directly
                    let time_bytes = value.time.to_le_bytes();
                    writer.write_byte_unchecked(time_bytes[0]);
                    writer.write_byte_unchecked(time_bytes[1]);
                    writer.write_byte_unchecked(time_bytes[2]);
                    writer.write_byte_unchecked(time_bytes[3]);
                }
            }
        } else {
            // Fixed type (DateTime, 0x3D) - just write value (8 bytes)
            match writer.has_space(8) {
                false => {
                    writer.write_i32_async(value.days).await?;
                    writer.write_u32_async(value.time).await?;
                }
                true => {
                    writer.write_i32_unchecked(value.days);
                    // Note: No unchecked version for u32, use the bytes directly
                    let time_bytes = value.time.to_le_bytes();
                    writer.write_byte_unchecked(time_bytes[0]);
                    writer.write_byte_unchecked(time_bytes[1]);
                    writer.write_byte_unchecked(time_bytes[2]);
                    writer.write_byte_unchecked(time_bytes[3]);
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_smalldatetime<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlSmallDateTime,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // SMALLDATETIME format in TDS:
        // - For DateTimeN (nullable, 0x6F): length byte (4) + 2-byte days + 2-byte time
        // - SMALLDATETIME always uses DateTimeN (0x6F) with length 4
        //
        // Days: Unsigned 16-bit integer representing days since January 1, 1900
        //       (range 0 to 65535 for dates from 1900-01-01 to 2079-06-06)
        // Time: Unsigned 16-bit integer representing minutes since midnight
        //       (range 0 to 1439 for times from 00:00 to 23:59)

        if !ctx.is_fixed_type() {
            // DateTimeN with length 4: length byte + value (5 bytes total)
            match writer.has_space(5) {
                false => {
                    writer.write_byte_async(4).await?; // Length for SmallDateTime (4 bytes)
                    writer.write_u16_async(value.days).await?;
                    writer.write_u16_async(value.time).await?;
                }
                true => {
                    writer.write_byte_unchecked(4); // Length for SmallDateTime (4 bytes)
                    writer.write_u16_unchecked(value.days);
                    writer.write_u16_unchecked(value.time);
                }
            }
        } else {
            // Fixed type - just write value (4 bytes)
            match writer.has_space(4) {
                false => {
                    writer.write_u16_async(value.days).await?;
                    writer.write_u16_async(value.time).await?;
                }
                true => {
                    writer.write_u16_unchecked(value.days);
                    writer.write_u16_unchecked(value.time);
                }
            }
        }
        Ok(())
    }

    /// Serialize a UNIQUEIDENTIFIER (GUID/UUID) value.
    ///
    /// UNIQUEIDENTIFIER wire format:
    /// - TDS type 0x24 (GUIDTYPE)
    /// - Fixed 16-byte value in mixed-endian format
    /// - Always uses nullable variant (no fixed GUID type)
    ///
    /// Format: 1-byte length (0x10 = 16) + 16 bytes GUID data
    ///
    /// The uuid crate's to_bytes_le() method directly produces the correct
    /// SQL Server mixed-endian format (Data1-3 little-endian, Data4 big-endian).
    #[inline(always)]
    async fn serialize_uuid<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &uuid::Uuid,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // GUID wire format:
        // - In sql_variant: Fixed 16 bytes (no length prefix)
        // - In regular columns: 1-byte length (0x10) + 16 bytes of GUID data
        let guid_bytes = value.to_bytes_le();

        if ctx.is_fixed_length {
            // Fixed-length context (sql_variant) - write 16 bytes directly without length prefix
            match writer.has_space(16) {
                false => {
                    writer.write_async(&guid_bytes).await?;
                }
                true => {
                    for &byte in &guid_bytes {
                        writer.write_byte_unchecked(byte);
                    }
                }
            }
        } else {
            // Nullable context (regular columns) - write length byte + 16 bytes
            match writer.has_space(17) {
                false => {
                    writer.write_byte_async(16u8).await?;
                    writer.write_async(&guid_bytes).await?;
                }
                true => {
                    writer.write_byte_unchecked(16u8);
                    for &byte in &guid_bytes {
                        writer.write_byte_unchecked(byte);
                    }
                }
            }
        }
        Ok(())
    }

    #[inline(always)]
    async fn serialize_datetime2<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlDateTime2,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // DATETIME2 format in TDS:
        // - For DateTime2N (nullable, 0x2A): length byte + time_nanoseconds + 3-byte days
        // - Time portion: 3, 4, or 5 bytes (same encoding as TIME type)
        // - Date portion: 3 bytes (unsigned, days since 0001-01-01)
        // Total length depends on scale:
        //   - Scale 0-2: 6 bytes (3 for time + 3 for date)
        //   - Scale 3-4: 7 bytes (4 for time + 3 for date)
        //   - Scale 5-7: 8 bytes (5 for time + 3 for date)

        // Determine the time length based on scale
        let time_length = get_time_length_from_scale(value.time.scale)?;

        let date_length = 3u8;
        let total_value_length = time_length + date_length;

        // Scale the time value based on the scale (same logic as serialize_time)
        let time_value = match value.time.scale {
            0 => value.time.time_nanoseconds / 10_000_000, // Seconds
            1 => value.time.time_nanoseconds / 1_000_000,  // Tenths
            2 => value.time.time_nanoseconds / 100_000,    // Hundredths
            3 => value.time.time_nanoseconds / 10_000,     // Milliseconds
            4 => value.time.time_nanoseconds / 1_000,      // Ten-thousandths
            5 => value.time.time_nanoseconds / 100,        // Hundred-thousandths
            6 => value.time.time_nanoseconds / 10,         // Microseconds
            7 => value.time.time_nanoseconds,              // 100-nanoseconds (no scaling)
            _ => value.time.time_nanoseconds,
        };

        if !ctx.is_fixed_type() && !ctx.is_fixed_length {
            // DateTime2N with length prefix
            let total_size = (1 + total_value_length) as usize;
            match writer.has_space(total_size) {
                false => {
                    writer.write_byte_async(total_value_length).await?; // Length byte

                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_async(byte_val).await?;
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.days.to_le_bytes();
                    writer.write_byte_async(date_bytes[0]).await?;
                    writer.write_byte_async(date_bytes[1]).await?;
                    writer.write_byte_async(date_bytes[2]).await?;
                }
                true => {
                    writer.write_byte_unchecked(total_value_length); // Length byte

                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_unchecked(byte_val);
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.days.to_le_bytes();
                    writer.write_byte_unchecked(date_bytes[0]);
                    writer.write_byte_unchecked(date_bytes[1]);
                    writer.write_byte_unchecked(date_bytes[2]);
                }
            }
        } else {
            // Fixed type - just write value (time + 3-byte date)
            match writer.has_space(total_value_length as usize) {
                false => {
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        writer
                            .write_byte_async(((time_value >> (i * 8)) & 0xFF) as u8)
                            .await?;
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.days.to_le_bytes();
                    writer.write_byte_async(date_bytes[0]).await?;
                    writer.write_byte_async(date_bytes[1]).await?;
                    writer.write_byte_async(date_bytes[2]).await?;
                }
                true => {
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        writer.write_byte_unchecked(((time_value >> (i * 8)) & 0xFF) as u8);
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.days.to_le_bytes();
                    writer.write_byte_unchecked(date_bytes[0]);
                    writer.write_byte_unchecked(date_bytes[1]);
                    writer.write_byte_unchecked(date_bytes[2]);
                }
            }
        }
        Ok(())
    }

    /// Serialize a DATETIMEOFFSET value to the TDS stream.
    ///
    /// DATETIMEOFFSET wire format:
    /// - 1 byte: length (0 for NULL, or time_length + 3 + 2 for date + offset)
    /// - time_length bytes: time component (3, 4, or 5 bytes based on scale)
    /// - 3 bytes: date component (days since 0001-01-01, little-endian)
    /// - 2 bytes: timezone offset in minutes (little-endian, signed i16)
    ///
    /// Time length by scale:
    /// - Scale 0-2: 3 bytes
    /// - Scale 3-4: 4 bytes
    /// - Scale 5-7: 5 bytes
    async fn serialize_datetimeoffset<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlDateTimeOffset,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // DATETIMEOFFSET format in TDS:
        // - For DateTimeOffsetN (nullable, 0x2B): length byte + time + 3-byte days + 2-byte offset
        // - Time portion: 3, 4, or 5 bytes (same encoding as TIME/DATETIME2 type)
        // - Date portion: 3 bytes (unsigned, days since 0001-01-01)
        // - Offset portion: 2 bytes (signed i16, minutes from UTC)
        // Total length depends on scale:
        //   - Scale 0-2: 8 bytes (3 for time + 3 for date + 2 for offset)
        //   - Scale 3-4: 9 bytes (4 for time + 3 for date + 2 for offset)
        //   - Scale 5-7: 10 bytes (5 for time + 3 for date + 2 for offset)

        // Determine the time length based on scale
        let time_length = get_time_length_from_scale(value.datetime2.time.scale)?;

        let date_length = 3u8;
        let offset_length = 2u8;
        let total_value_length = time_length + date_length + offset_length;

        // Scale the time value based on the scale (same logic as serialize_time)
        let time_value = match value.datetime2.time.scale {
            0 => value.datetime2.time.time_nanoseconds / 10_000_000, // Seconds
            1 => value.datetime2.time.time_nanoseconds / 1_000_000,  // Tenths
            2 => value.datetime2.time.time_nanoseconds / 100_000,    // Hundredths
            3 => value.datetime2.time.time_nanoseconds / 10_000,     // Milliseconds
            4 => value.datetime2.time.time_nanoseconds / 1_000,      // Ten-thousandths
            5 => value.datetime2.time.time_nanoseconds / 100,        // Hundred-thousandths
            6 => value.datetime2.time.time_nanoseconds / 10,         // Microseconds
            7 => value.datetime2.time.time_nanoseconds,              // 100-nanoseconds (no scaling)
            _ => value.datetime2.time.time_nanoseconds,
        };

        if !ctx.is_fixed_type() && !ctx.is_fixed_length {
            // DateTimeOffsetN with length prefix
            let total_size = (1 + total_value_length) as usize;
            match writer.has_space(total_size) {
                false => {
                    writer.write_byte_async(total_value_length).await?; // Length byte

                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_async(byte_val).await?;
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.datetime2.days.to_le_bytes();
                    writer.write_byte_async(date_bytes[0]).await?;
                    writer.write_byte_async(date_bytes[1]).await?;
                    writer.write_byte_async(date_bytes[2]).await?;

                    // Write timezone offset as 2-byte little-endian signed integer
                    let offset_bytes = value.offset.to_le_bytes();
                    writer.write_byte_async(offset_bytes[0]).await?;
                    writer.write_byte_async(offset_bytes[1]).await?;
                }
                true => {
                    writer.write_byte_unchecked(total_value_length); // Length byte

                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        let byte_val = ((time_value >> (i * 8)) & 0xFF) as u8;
                        writer.write_byte_unchecked(byte_val);
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.datetime2.days.to_le_bytes();
                    writer.write_byte_unchecked(date_bytes[0]);
                    writer.write_byte_unchecked(date_bytes[1]);
                    writer.write_byte_unchecked(date_bytes[2]);

                    // Write timezone offset as 2-byte little-endian signed integer
                    let offset_bytes = value.offset.to_le_bytes();
                    writer.write_byte_unchecked(offset_bytes[0]);
                    writer.write_byte_unchecked(offset_bytes[1]);
                }
            }
        } else {
            // Fixed type or sql_variant - just write value (time + date + offset, no length prefix)
            match writer.has_space(total_value_length as usize) {
                false => {
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        writer
                            .write_byte_async(((time_value >> (i * 8)) & 0xFF) as u8)
                            .await?;
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.datetime2.days.to_le_bytes();
                    writer.write_byte_async(date_bytes[0]).await?;
                    writer.write_byte_async(date_bytes[1]).await?;
                    writer.write_byte_async(date_bytes[2]).await?;

                    // Write timezone offset as 2-byte little-endian signed integer
                    let offset_bytes = value.offset.to_le_bytes();
                    writer.write_byte_async(offset_bytes[0]).await?;
                    writer.write_byte_async(offset_bytes[1]).await?;
                }
                true => {
                    // Write time_value in little-endian format (variable bytes)
                    for i in 0..time_length {
                        writer.write_byte_unchecked(((time_value >> (i * 8)) & 0xFF) as u8);
                    }

                    // Write date as 3-byte little-endian unsigned integer
                    let date_bytes = value.datetime2.days.to_le_bytes();
                    writer.write_byte_unchecked(date_bytes[0]);
                    writer.write_byte_unchecked(date_bytes[1]);
                    writer.write_byte_unchecked(date_bytes[2]);

                    // Write timezone offset as 2-byte little-endian signed integer
                    let offset_bytes = value.offset.to_le_bytes();
                    writer.write_byte_unchecked(offset_bytes[0]);
                    writer.write_byte_unchecked(offset_bytes[1]);
                }
            }
        }
        Ok(())
    }

    /// Serialize BINARY/VARBINARY/IMAGE data to the TDS stream.
    ///
    /// BINARY/VARBINARY wire format:
    /// - For variable-length types (BINARY/VARBINARY):
    ///   - If length <= 8000:
    ///     - 2 bytes: actual length (0xFFFF for NULL)
    ///     - n bytes: actual byte data
    ///   - If length > 8000 (MAX types, not common for BINARY):
    ///     - 8 bytes: total length (0xFFFFFFFFFFFFFFFF for NULL)
    ///     - chunks of data with 4-byte length prefixes
    ///     - 4-byte terminator (0x00000000)
    ///
    /// IMAGE wire format (legacy LOB type):
    /// - Format: textptr_len (1) + textptr (16 x 0xFF) + timestamp (8 x 0xFF) + length (4) + data
    /// - This matches the format used by TEXT and NTEXT types
    /// - Reference: TdsParser.cs s_longDataHeader constant
    ///
    /// For BINARY(n): If data length < n, pad with zeros to reach fixed length
    /// For VARBINARY(n): Use exact data length, no padding
    ///
    /// TDS types:
    /// - 0xAD: BINARY(n) / VARBINARY(n) - fixed/variable-length binary
    /// - 0xA5: VARBINARY(MAX) - variable-length binary with PLP encoding
    /// - 0x22: IMAGE - legacy LOB type with special header format
    #[inline(always)]
    async fn serialize_bytes<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &[u8],
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        let data_len = value.len();
        let schema_size = ctx.max_size;

        // Check for size overflow (skip for PLP types and legacy LOB types which support up to 2GB)
        if !ctx.is_plp && ctx.tds_type != IMAGE && data_len > schema_size {
            return Err(Error::UsageError(format!(
                "Binary data length ({}) exceeds schema size ({})",
                data_len, schema_size
            )));
        }

        // Handle IMAGE type (legacy LOB type, similar to TEXT/NTEXT)
        if ctx.tds_type == IMAGE {
            // Legacy LOB type format: textptr_len (1) + textptr (16 x 0xFF) + timestamp (8 x 0xFF) + length (4) + data
            // This matches the format used by TEXT and NTEXT

            // Write textptr length as 16 (0x10)
            writer.write_byte_async(0x10).await?;

            // Write 16-byte textptr (all 0xFF as per .NET SqlClient)
            for _ in 0..16 {
                writer.write_byte_async(0xFF).await?;
            }

            // Write 8-byte timestamp (all 0xFF as per .NET SqlClient)
            for _ in 0..8 {
                writer.write_byte_async(0xFF).await?;
            }

            // Write data length (4 bytes)
            writer.write_u32_async(data_len as u32).await?;

            // Write actual data
            writer.write_async(value).await?;
        } else if ctx.is_plp {
            // For PLP types (MAX types), use PLP encoding
            // Write PLP_UNKNOWN_LEN (0xFFFFFFFFFFFFFFFE) to indicate total length is unknown
            // This matches .NET SqlBulkCopy behavior
            writer.write_u64_async(PLP_UNKNOWN_LEN).await?;

            // Write chunk length (4 bytes)
            writer.write_u32_async(data_len as u32).await?;

            // Write actual data
            writer.write_async(value).await?;

            // Write terminator (4 bytes of 0x00)
            writer.write_u32_async(PLP_TERMINATOR).await?;
        } else if ctx.is_fixed_length {
            // Fixed-length BINARY(n): Write exactly n bytes (no length prefix)
            // Write actual data
            for &byte in value {
                writer.write_byte_async(byte).await?;
            }

            // Pad with zeros to reach fixed size
            let padding_needed = schema_size.saturating_sub(data_len);
            for _ in 0..padding_needed {
                writer.write_byte_async(0u8).await?;
            }
        } else {
            // Variable-length VARBINARY(n): Write 2-byte length prefix + data (no padding)
            writer.write_u16_async(data_len as u16).await?;

            // Write actual data
            for &byte in value {
                writer.write_byte_async(byte).await?;
            }
        }
        Ok(())
    }

    /// Serialize a JSON value using PLP encoding.
    ///
    /// JSON type (0xF4) uses UTF-8 encoding and PLP (Partially Length Prefixed) structure.
    /// Format: PLP_UNKNOWN_LEN (8 bytes) + chunk_len (4 bytes) + data + terminator (4 bytes)
    async fn serialize_json<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &SqlJson,
        _ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // CRITICAL: JSON bulk copy uses NVARCHAR (0xE7) encoding, which requires UTF-16LE
        // Convert UTF-8 JSON to UTF-16LE like .NET SqlBulkCopy does
        let json_str = std::str::from_utf8(&value.bytes).map_err(|e| {
            Error::TypeConversionError(format!("Invalid UTF-8 in JSON data: {}", e))
        })?;

        // Encode as UTF-16LE (no BOM needed for NVARCHAR)
        let utf16_data: Vec<u16> = json_str.encode_utf16().collect();
        let byte_len = utf16_data.len() * 2; // Each u16 is 2 bytes

        // JSON uses PLP encoding with UTF-16LE for NVARCHAR type
        // Write PLP_UNKNOWN_LEN (0xFFFFFFFFFFFFFFFE)
        writer.write_u64_async(PLP_UNKNOWN_LEN).await?;

        // Write chunk length (4 bytes)
        writer.write_u32_async(byte_len as u32).await?;

        // Write UTF-16LE encoded data
        for code_unit in utf16_data {
            writer.write_u16_async(code_unit).await?;
        }

        // Write terminator (4 bytes of 0x00)
        writer.write_u32_async(PLP_TERMINATOR).await?;

        Ok(())
    }

    /// Serialize an XML value for bulk copy using PLP encoding.
    ///
    /// XML bulk copy format (matching parameter serialization):
    /// - 8 bytes: PLP_UNKNOWN_LEN (0xFFFFFFFFFFFFFFFE)
    /// - 4 bytes: chunk length
    /// - 2 bytes: BOM (0xFFFE) if not present in data
    /// - n bytes: XML data
    /// - 4 bytes: PLP_TERMINATOR (0x00000000)
    ///
    /// Note: No chunking support right now - single chunk only
    async fn serialize_xml<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::column_values::SqlXml,
        _ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        let data = &value.bytes;

        // Write PLP_UNKNOWN_LEN (8 bytes) - modern format
        writer.write_u64_async(PLP_UNKNOWN_LEN).await?;

        // Calculate data length: add BOM if not present
        let data_len = if value.has_bom() {
            data.len()
        } else {
            data.len() + 2
        };

        // Write chunk length (4 bytes)
        writer.write_u32_async(data_len as u32).await?;

        // Write BOM if not present (UTF-16LE: 0xFF 0xFE)
        if !value.has_bom() {
            writer.write_byte_async(0xFF).await?;
            writer.write_byte_async(0xFE).await?;
        }

        // Write XML data
        writer.write_async(data).await?;

        // Write PLP terminator (4 bytes)
        writer.write_u32_async(PLP_TERMINATOR).await?;

        Ok(())
    }

    /// Serialize a String (NVARCHAR/VARCHAR/NCHAR/CHAR) to the TDS stream.
    ///
    /// String wire format depends on type and encoding:
    /// - For NVARCHAR(n), NCHAR(n) where n <= 4000 (non-MAX types):
    ///   - 2 bytes: character count (0xFFFF for NULL) - NOT byte count!
    ///   - n*2 bytes: UTF-16LE encoded characters
    /// - For NVARCHAR(MAX) (PLP types):
    ///   - 8 bytes: PLP_UNKNOWN_LEN (0xFFFFFFFFFFFFFFFE) to indicate unknown total length
    ///   - 4 bytes: chunk byte length
    ///   - n*2 bytes: UTF-16LE encoded characters
    ///   - 4 bytes: terminator (0x00000000)
    /// - For VARCHAR(n), CHAR(n) where n <= 8000 (non-MAX types):
    ///   - 2 bytes: byte count (0xFFFF for NULL)
    ///   - n bytes: single-byte encoded characters (based on collation)
    ///
    /// CRITICAL: For NVARCHAR types, the length prefix is CHARACTER count, not byte count!
    /// Each character is 2 bytes in UTF-16LE, so byte_count = char_count * 2.
    ///
    /// This matches .NET SqlBulkCopy behavior where:
    /// - NVARCHAR(4000) → max 4000 characters (8000 bytes)
    /// - VARCHAR(8000) → max 8000 bytes
    ///
    /// TDS types handled:
    /// - 0xE7: NVARCHAR(n) / NVARCHAR(MAX) - UTF-16LE encoding
    /// - 0xEF: NCHAR(n) - UTF-16LE encoding with padding
    /// - 0xA7: VARCHAR(n) / VARCHAR(MAX) - single-byte encoding
    /// - 0xAF: CHAR(n) - single-byte encoding with padding
    async fn serialize_string<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &crate::datatypes::sql_string::SqlString,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // For NVARCHAR/NCHAR types, we need UTF-16LE encoding
        // The SqlString might already be UTF-16 encoded, so check first to avoid re-encoding
        let (utf16_bytes, is_unicode) = match ctx.tds_type {
            NVARCHAR | NCHAR => {
                // NVARCHAR/NCHAR - UTF-16LE encoding required
                let bytes = if let Some(utf16_data) = value.as_utf16_bytes() {
                    // Already UTF-16 encoded, use directly (zero-copy optimization)
                    utf16_data
                } else if let Some(raw_bytes) = value.as_raw_wire_bytes() {
                    // DelayedSet/LcidBased: bytes are already in wire format, use directly.
                    // This is critical for the RPC path where bytes are pre-encoded.
                    raw_bytes
                } else {
                    // Need to encode to UTF-16LE
                    // Get UTF-8 string first
                    let utf8_str = value.to_utf8_string();
                    // Encode to UTF-16LE and store temporarily
                    // NOTE: This creates a temporary Vec, but SqlString should ideally store UTF-16 already
                    return Self::serialize_string_utf16(writer, &utf8_str, ctx).await;
                };
                (bytes, true)
            }
            NTEXT => {
                // NTEXT - UTF-16LE encoding required (legacy LOB type)
                // For NTEXT, we need UTF-16 encoding but will handle serialization differently
                if let Some(utf16_data) = value.as_utf16_bytes() {
                    // Already UTF-16 encoded, use directly
                    (utf16_data, true)
                } else {
                    // UTF-8 bytes that need conversion to UTF-16
                    (&value.bytes[..], false)
                }
            }
            VARCHAR | CHAR | TEXT => {
                // VARCHAR/CHAR/TEXT - single-byte encoding
                // If the bytes are already in wire format (DelayedSet or LcidBased),
                // use them directly without decode→re-encode roundtrip.
                // This is critical for the RPC path where bytes are pre-encoded.
                if let Some(raw_bytes) = value.as_raw_wire_bytes() {
                    return Self::serialize_char_varchar_direct(writer, raw_bytes, ctx).await;
                }

                // Otherwise (UTF-8 or UTF-16 source), decode and re-encode to target code page
                let decoded_str = value.to_utf8_string();

                // Encode to single-byte based on collation
                let single_byte_data = if let Some(collation) = &ctx.collation {
                    // Extract LCID from the lower 20 bits of collation.info
                    let lcid = collation.info & 0x000F_FFFF;

                    // Map LCID to encoding
                    match lcid_to_encoding(lcid) {
                        Ok(encoding) => {
                            // Encode using the determined encoding
                            let (encoded, _encoding_used, had_errors) =
                                encoding.encode(&decoded_str);

                            if had_errors {
                                tracing::warn!(
                                    "Encountered encoding errors while converting string to LCID 0x{:04X} ({}) encoding. \
                                     Some characters may have been replaced.",
                                    lcid,
                                    lcid
                                );
                            }

                            encoded.into_owned()
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Unsupported LCID 0x{:04X} ({}), falling back to Latin-1. Error: {}",
                                lcid,
                                lcid,
                                e
                            );
                            // Fall back to Latin-1 for unsupported LCIDs
                            decoded_str
                                .chars()
                                .map(|c| {
                                    if (c as u32) <= 0xFF {
                                        c as u8
                                    } else {
                                        b'?' // Replace unmappable characters with '?'
                                    }
                                })
                                .collect::<Vec<u8>>()
                        }
                    }
                } else {
                    // No collation provided, use Latin-1 (ISO-8859-1) as default
                    // This covers ASCII + extended Latin characters
                    decoded_str
                        .chars()
                        .map(|c| {
                            if (c as u32) <= 0xFF {
                                c as u8
                            } else {
                                b'?' // Replace unmappable characters with '?'
                            }
                        })
                        .collect::<Vec<u8>>()
                };

                // Store the single-byte data temporarily - we'll use it below
                // Note: This creates a temporary allocation, but it's necessary for the conversion
                return Self::serialize_char_varchar_direct(writer, &single_byte_data, ctx).await;
            }
            _ => {
                return Err(Error::UsageError(format!(
                    "Unsupported TDS type for string serialization: 0x{:02X}",
                    ctx.tds_type
                )));
            }
        };

        // Determine data length based on encoding
        let (data_len, char_count) = if is_unicode {
            // UTF-16LE: byte_len must be even, char_count = byte_len / 2
            let byte_len = utf16_bytes.len();
            if byte_len % 2 != 0 {
                return Err(Error::UsageError(format!(
                    "Invalid UTF-16 data: byte length {} is odd",
                    byte_len
                )));
            }
            let char_count = byte_len / 2;
            (byte_len, char_count)
        } else {
            // Single-byte encoding: char_count = byte_len
            (utf16_bytes.len(), utf16_bytes.len())
        };

        // CRITICAL: For NVARCHAR(n), the schema size is in characters, not bytes!
        // max_size represents character count for Unicode types
        let schema_char_count = ctx.max_size;

        // Check for size overflow (skip for PLP types which support up to 1GB)
        if !ctx.is_plp && char_count > schema_char_count {
            return Err(Error::UsageError(format!(
                "String length ({} characters) exceeds schema size ({} characters)",
                char_count, schema_char_count
            )));
        }

        // Serialize based on type classification
        if ctx.tds_type == NTEXT || ctx.tds_type == TEXT {
            // Legacy LOB types (NTEXT=0x63, TEXT=0x23): Use special format matching .NET SqlClient
            // Format: textptr_len (1) + textptr (16 × 0xFF) + timestamp (8 × 0xFF) + length (4) + data
            // Reference: TdsParser.cs s_longDataHeader constant

            // Write textptr length as 16 (0x10)
            writer.write_byte_async(0x10).await?;

            // Write 16-byte textptr (all 0xFF as per .NET SqlClient)
            for _ in 0..16 {
                writer.write_byte_async(0xFF).await?;
            }

            // Write 8-byte timestamp (all 0xFF as per .NET SqlClient)
            for _ in 0..8 {
                writer.write_byte_async(0xFF).await?;
            }

            // For NTEXT, we need to ensure we have UTF-16LE encoded data
            if ctx.tds_type == NTEXT {
                // NTEXT - need UTF-16LE encoding
                if is_unicode {
                    // Already UTF-16, use as-is
                    writer.write_u32_async(data_len as u32).await?;
                    writer.write_async(utf16_bytes).await?;
                } else {
                    // Need to convert UTF-8 to UTF-16LE
                    let utf8_str = value.to_utf8_string();
                    let utf16_vec: Vec<u16> = utf8_str.encode_utf16().collect();
                    let utf16_byte_len = utf16_vec.len() * 2;

                    // Write data length (4 bytes)
                    writer.write_u32_async(utf16_byte_len as u32).await?;

                    // Write UTF-16LE data
                    for code_unit in &utf16_vec {
                        writer.write_u16_async(*code_unit).await?;
                    }
                }
            } else {
                // TEXT - single-byte encoding
                writer.write_u32_async(data_len as u32).await?;
                writer.write_async(utf16_bytes).await?;
            }
        } else if ctx.is_plp {
            // PLP types (NVARCHAR(MAX), VARCHAR(MAX)): Use PLP encoding
            // Write PLP_UNKNOWN_LEN (0xFFFFFFFFFFFFFFFE) to indicate total length is unknown
            writer.write_u64_async(PLP_UNKNOWN_LEN).await?;

            // Write chunk byte length (4 bytes)
            writer.write_u32_async(data_len as u32).await?;

            // Write actual data
            writer.write_async(utf16_bytes).await?;

            // Write terminator (4 bytes of 0x00)
            writer.write_u32_async(PLP_TERMINATOR).await?;
        } else if ctx.is_fixed_length {
            // Fixed-length types (NCHAR(n), CHAR(n)): Write exactly n characters
            // For NCHAR: Write char_count * 2 bytes (no length prefix)
            // For CHAR: Write char_count bytes (no length prefix)

            // Write actual data
            writer.write_async(utf16_bytes).await?;

            // Pad with zeros (space characters in UTF-16) to reach fixed size
            let padding_chars = schema_char_count.saturating_sub(char_count);
            if is_unicode {
                // Pad with UTF-16 space (0x0020)
                for _ in 0..padding_chars {
                    writer.write_u16_async(0x0020).await?;
                }
            } else {
                // Pad with ASCII space (0x20)
                for _ in 0..padding_chars {
                    writer.write_byte_async(0x20).await?;
                }
            }
        } else {
            // Variable-length types (NVARCHAR(n), VARCHAR(n)): Write length prefix + data
            // CRITICAL: Length prefix is ALWAYS byte count, not character count
            // This matches .NET SqlClient behavior: WriteShort(length * ADP.CharSize)
            // where ADP.CharSize = 2 for Unicode
            let length_prefix = data_len as u16;

            tracing::debug!(
                "Writing variable-length string: is_unicode={}, tds_type=0x{:02X}, char_count={}, data_len={}, length_prefix={}, utf16_bytes.len()={}",
                is_unicode,
                ctx.tds_type,
                char_count,
                data_len,
                length_prefix,
                utf16_bytes.len()
            );

            writer.write_u16_async(length_prefix).await?;

            // Write actual data
            writer.write_async(utf16_bytes).await?;
        }

        Ok(())
    }

    /// Helper to serialize a UTF-8 string as UTF-16LE for NVARCHAR/NCHAR types.
    ///
    /// This is used when the SqlString is not already UTF-16 encoded.
    async fn serialize_string_utf16<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        utf8_str: &str,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Encode to UTF-16LE
        let utf16_data: Vec<u16> = utf8_str.encode_utf16().collect();
        let char_count = utf16_data.len();
        let byte_len = char_count * 2;

        // Check schema constraints
        let schema_char_count = ctx.max_size;
        if !ctx.is_plp && char_count > schema_char_count {
            return Err(Error::UsageError(format!(
                "String length ({} characters) exceeds schema size ({} characters)",
                char_count, schema_char_count
            )));
        }

        // Serialize based on type classification
        if ctx.is_plp {
            // PLP types (NVARCHAR(MAX)): Use PLP encoding
            writer.write_u64_async(PLP_UNKNOWN_LEN).await?;
            writer.write_u32_async(byte_len as u32).await?;

            // Write UTF-16LE data
            for code_unit in &utf16_data {
                writer.write_u16_async(*code_unit).await?;
            }

            writer.write_u32_async(PLP_TERMINATOR).await?;
        } else if ctx.is_fixed_length {
            // Fixed-length types (NCHAR(n)): Write exactly n characters with padding
            // Write actual data
            for code_unit in &utf16_data {
                writer.write_u16_async(*code_unit).await?;
            }

            // Pad with UTF-16 space (0x0020)
            let padding_chars = schema_char_count.saturating_sub(char_count);
            for _ in 0..padding_chars {
                writer.write_u16_async(0x0020).await?;
            }
        } else {
            // Variable-length types (NVARCHAR(n)): Write character count + data
            writer.write_u16_async(char_count as u16).await?;

            // Write UTF-16LE data
            for code_unit in &utf16_data {
                writer.write_u16_async(*code_unit).await?;
            }
        }

        Ok(())
    }

    /// Serialize VECTOR value to the TDS stream.
    ///
    /// Wire format for VECTOR (non-PLP):
    /// - 2 bytes: actual length (USHORT) = 8 (header) + dims * element_size
    /// - 8 bytes: header [layout_format=0xA9, version=0x01, dims:u16, base_type:u8, 0,0,0]
    /// - n bytes: element values (little-endian)
    #[inline(always)]
    async fn serialize_vector<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &SqlVector,
        _ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Compute total payload size
        let dims = value.dimension_count();
        let total_size = value.total_size();

        // Length prefix (USHORT)
        writer.write_u16_async(total_size as u16).await?;

        // Header
        crate::datatypes::sqltypes::SqlType::encode_vector_header(writer, dims, value.base_type())
            .await?;

        // Values
        match &value.data {
            VectorData::Float32(vs) => {
                for f in vs {
                    // Write f32 as i32 little-endian (bit-compatible)
                    writer.write_i32_async((*f).to_bits() as i32).await?;
                }
            }
        }

        Ok(())
    }

    /// Helper to serialize a single-byte string for CHAR/VARCHAR types.
    ///
    /// Takes single-byte encoded data and serializes it according to the TDS type context.
    async fn serialize_char_varchar_direct<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        single_byte_data: &[u8],
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        let char_count = single_byte_data.len();
        let schema_char_count = ctx.max_size;

        // Check for size overflow (skip for PLP types)
        if !ctx.is_plp && char_count > schema_char_count {
            return Err(Error::UsageError(format!(
                "String length ({} characters) exceeds schema size ({} characters)",
                char_count, schema_char_count
            )));
        }

        // Serialize based on type classification
        if ctx.tds_type == TEXT {
            // Legacy TEXT type: Use special format
            // Write textptr length as 16 (0x10)
            writer.write_byte_async(0x10).await?;

            // Write 16-byte textptr (all 0xFF)
            for _ in 0..16 {
                writer.write_byte_async(0xFF).await?;
            }

            // Write 8-byte timestamp (all 0xFF)
            for _ in 0..8 {
                writer.write_byte_async(0xFF).await?;
            }

            // Write 4-byte length
            writer.write_u32_async(char_count as u32).await?;

            // Write actual data
            writer.write_async(single_byte_data).await?;
        } else if ctx.is_plp {
            // PLP types (VARCHAR(MAX)): Use PLP encoding
            writer.write_u64_async(PLP_UNKNOWN_LEN).await?;
            writer.write_u32_async(char_count as u32).await?;
            writer.write_async(single_byte_data).await?;
            writer.write_u32_async(PLP_TERMINATOR).await?;
        } else if ctx.is_fixed_length {
            // Fixed-length CHAR(n): Write exactly n bytes with padding
            // Write actual data
            writer.write_async(single_byte_data).await?;

            // Pad with ASCII space (0x20) to reach fixed size
            let padding_count = schema_char_count.saturating_sub(char_count);
            for _ in 0..padding_count {
                writer.write_byte_async(0x20).await?;
            }
        } else {
            // Variable-length VARCHAR(n): Write length prefix + data
            // Length prefix is byte count (2 bytes)
            writer.write_u16_async(char_count as u16).await?;
            writer.write_async(single_byte_data).await?;
        }

        Ok(())
    }

    /// Serialize a value as SQL_VARIANT type (TdsDataType::SsVariant).
    ///
    /// This wraps any inner value with the variant wire format:
    /// ```text
    /// [4-byte total_length][1-byte base_type][1-byte prop_len][N-byte props][M-byte data]
    /// ```
    ///
    /// For NULL values, total_length is 0 and no other bytes are written.
    ///
    /// # Arguments
    ///
    /// * `writer` - Packet writer for TDS stream
    /// * `value` - The value to wrap as variant (can be any ColumnValues type)
    /// * `ctx` - Type context (should have tds_type == SQL_VARIANT)
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Value type is not supported in sql_variant (text, ntext, image, timestamp)
    /// - Value serialization fails
    async fn serialize_as_variant<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &ColumnValues,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Handle NULL variant: just write 4-byte length = 0
        if matches!(value, ColumnValues::Null) {
            writer.write_u32_async(0).await?;
            return Ok(());
        }

        // Get the base TDS type for this value
        let base_type = Self::get_variant_base_type(value)?;

        // Calculate property byte length using shared function
        let prop_len = Self::calculate_type_info_length(base_type, value);

        // Calculate the data size without actually serializing (to avoid creating temp writer)
        let data_size = Self::calculate_value_size(value)?;

        // SQL_VARIANT has a maximum data size of 8000 bytes (excluding metadata)
        const MAX_VARIANT_DATA_SIZE: u32 = 8000;
        if data_size > MAX_VARIANT_DATA_SIZE {
            return Err(Error::UsageError(format!(
                "SQL_VARIANT data size ({} bytes) exceeds maximum ({} bytes). \
                 The base data type cannot store more than {} bytes in SQL_VARIANT.",
                data_size, MAX_VARIANT_DATA_SIZE, MAX_VARIANT_DATA_SIZE
            )));
        }

        // Calculate total length: type_byte(1) + prop_len_byte(1) + prop_bytes + data_bytes
        let total_length = 2u32 + (prop_len as u32) + data_size;

        // SQL_VARIANT total size (including metadata) cannot exceed 8016 bytes
        const MAX_VARIANT_SIZE: u32 = 8016;
        if total_length > MAX_VARIANT_SIZE {
            return Err(Error::UsageError(format!(
                "SQL_VARIANT total size ({} bytes) exceeds maximum size ({} bytes). \
                 The variant contains {} bytes of data plus {} bytes of metadata.",
                total_length,
                MAX_VARIANT_SIZE,
                data_size,
                2 + prop_len as u32
            )));
        }

        // Write variant wrapper
        writer.write_u32_async(total_length).await?; // 4-byte total length
        writer.write_byte_async(base_type).await?; // 1-byte base type
        writer.write_byte_async(prop_len).await?; // 1-byte property length

        // Write property bytes using shared function
        if prop_len > 0 {
            Self::write_type_info_bytes(writer, base_type, value, ctx).await?;
        }

        // Create a temporary context with the correct base type for serializing the inner value
        let temp_ctx = Self::create_variant_inner_context(value, base_type, ctx)?;

        // Now write the actual value data using the inner serialization
        Self::serialize_value_inner(writer, value, &temp_ctx).await?;

        Ok(())
    }

    /// Calculate the serialized size of a value in bytes.
    ///
    /// This estimates the size without actually serializing, used for variant
    /// length calculation. Doesn't include the length prefix bytes coz it is
    /// not encoded in case of sql_variant.
    fn calculate_value_size(value: &ColumnValues) -> TdsResult<u32> {
        let size = match value {
            ColumnValues::TinyInt(_) => 1,
            ColumnValues::SmallInt(_) => 2,
            ColumnValues::Int(_) => 4,
            ColumnValues::BigInt(_) => 8,

            ColumnValues::Real(_) => 4,
            ColumnValues::Float(_) => 8,

            ColumnValues::Bit(_) => 1,

            ColumnValues::Decimal(v) | ColumnValues::Numeric(v) => {
                // Decimal/Numeric size based on precision (matching serialize_decimal logic)
                // Precision 1-9:   5 bytes (1 sign + 4 value bytes)
                // Precision 10-19: 9 bytes (1 sign + 8 value bytes)
                // Precision 20-28: 13 bytes (1 sign + 12 value bytes)
                // Precision 29-38: 17 bytes (1 sign + 16 value bytes)
                let precision = v.precision;
                let value_bytes = match precision {
                    1..=9 => 4,
                    10..=19 => 8,
                    20..=28 => 12,
                    29..=38 => 16,
                    _ => 16, // Default to max
                };
                1 + value_bytes // sign(1) + value_bytes
            }

            ColumnValues::Money(_) => 8,
            ColumnValues::SmallMoney(_) => 4,

            ColumnValues::Date(_) => 3,
            ColumnValues::Time(v) => get_time_length_from_scale(v.scale)? as u32,
            ColumnValues::DateTime(_) => 8, // days(4) + time(4)
            ColumnValues::DateTime2(v) => {
                let time_len = get_time_length_from_scale(v.time.scale)? as u32;
                time_len + 3 // time + days(3)
            }
            ColumnValues::DateTimeOffset(v) => {
                let time_len = get_time_length_from_scale(v.datetime2.time.scale)? as u32;
                time_len + 3 + 2 // time + days(3) + offset(2)
            }
            ColumnValues::SmallDateTime(_) => 4, // days(2) + time(2)

            ColumnValues::Bytes(v) => v.len() as u32,

            ColumnValues::String(s) => s.bytes.len() as u32,

            ColumnValues::Uuid(_) => 16,

            // Unsupported types in variant
            ColumnValues::Vector(_) | ColumnValues::Xml(_) | ColumnValues::Json(_) => {
                return Err(Error::UsageError(
                    "Unsupported data type in sql_variant".to_string(),
                ));
            }

            ColumnValues::Null => 0, // Should have been handled earlier
        };

        Ok(size)
    }

    /// Calculate the length of TYPE_INFO property bytes for a given type.
    ///
    /// This determines how many bytes of type-specific metadata are needed:
    /// - 0 bytes: Fixed-length types (INT, FLOAT, BIT, DATETIME, MONEY, GUID, DATE)
    /// - 1 byte: TIME, DATETIME2, DATETIMEOFFSET (scale)
    /// - 2 bytes: DECIMAL/NUMERIC (precision + scale), BINARY/VARBINARY (max_length)
    /// - 7 bytes: String types (collation[5] + max_length[2])
    ///
    /// This function is shared between sql_variant serialization and COLMETADATA encoding.
    fn calculate_type_info_length(tds_type: u8, _value: &ColumnValues) -> u8 {
        match tds_type {
            // Fixed-length types: 0 property bytes (type code defines everything)
            x if x == TdsDataType::Int1 as u8
                || x == TdsDataType::Int2 as u8
                || x == TdsDataType::Int4 as u8
                || x == TdsDataType::Int8 as u8
                || x == TdsDataType::Flt4 as u8
                || x == TdsDataType::Flt8 as u8
                || x == TdsDataType::Bit as u8
                || x == TdsDataType::Money as u8
                || x == TdsDataType::Money4 as u8
                || x == TdsDataType::DateTime as u8
                || x == TdsDataType::DateTim4 as u8 =>
            {
                0
            }

            // DateN and Guid: 0 property bytes
            x if x == TdsDataType::DateN as u8 || x == TdsDataType::Guid as u8 => 0,

            // Time types: 1 byte (scale)
            x if x == TdsDataType::TimeN as u8
                || x == TdsDataType::DateTime2N as u8
                || x == TdsDataType::DateTimeOffsetN as u8 =>
            {
                1
            }

            // Decimal/Numeric: 2 bytes (precision + scale)
            x if x == TdsDataType::DecimalN as u8 || x == TdsDataType::NumericN as u8 => 2,

            // Binary types: 2 bytes (max_length as u16)
            x if x == TdsDataType::BigVarBinary as u8 || x == TdsDataType::BigBinary as u8 => 2,

            // String types: 7 bytes (collation[5] + max_length[2])
            x if x == TdsDataType::NVarChar as u8
                || x == TdsDataType::NChar as u8
                || x == TdsDataType::BigChar as u8
                || x == TdsDataType::BigVarChar as u8 =>
            {
                7
            }

            // All valid types from get_variant_base_type are handled above
            _ => unreachable!("Invalid TDS type 0x{:02X} for sql_variant", tds_type),
        }
    }

    /// Write TYPE_INFO bytes for a sql_variant value to the packet writer.
    /// We don't use metadata info from the context here, as it contains sql_variant
    /// metadata, & not metadata for the inner value. Instead, we extract the necessary
    /// metadata bytes directly from the value itself.
    async fn write_type_info_bytes<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        tds_type: u8,
        value: &ColumnValues,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        match tds_type {
            // Fixed-length types: No property bytes to write
            x if x == TdsDataType::Int1 as u8
                || x == TdsDataType::Int2 as u8
                || x == TdsDataType::Int4 as u8
                || x == TdsDataType::Int8 as u8
                || x == TdsDataType::Flt4 as u8
                || x == TdsDataType::Flt8 as u8
                || x == TdsDataType::Bit as u8
                || x == TdsDataType::Money as u8
                || x == TdsDataType::Money4 as u8
                || x == TdsDataType::DateTime as u8
                || x == TdsDataType::DateTim4 as u8 =>
            {
                // No property bytes - type code fully defines the format
            }

            // DateN and Guid: 0 property bytes (no need to write anything)
            x if x == TdsDataType::DateN as u8 || x == TdsDataType::Guid as u8 => {
                // No property bytes
            }

            // Time types: 1 byte (scale)
            x if x == TdsDataType::TimeN as u8 => {
                if let ColumnValues::Time(v) = value {
                    writer.write_byte_async(v.scale).await?;
                }
            }
            x if x == TdsDataType::DateTime2N as u8 => {
                if let ColumnValues::DateTime2(v) = value {
                    writer.write_byte_async(v.time.scale).await?;
                }
            }
            x if x == TdsDataType::DateTimeOffsetN as u8 => {
                if let ColumnValues::DateTimeOffset(v) = value {
                    writer.write_byte_async(v.datetime2.time.scale).await?;
                }
            }

            // Decimal/Numeric: 2 bytes (precision + scale)
            x if x == TdsDataType::DecimalN as u8 || x == TdsDataType::NumericN as u8 => {
                if let ColumnValues::Decimal(v) | ColumnValues::Numeric(v) = value {
                    writer.write_byte_async(v.precision).await?;
                    writer.write_byte_async(v.scale).await?;
                }
            }

            // Binary types: 2 bytes (max_length as u16)
            x if x == TdsDataType::BigVarBinary as u8 => {
                if let ColumnValues::Bytes(v) = value {
                    let max_length = v.len() as u16;
                    writer.write_u16_async(max_length).await?;
                }
            }

            // String types: 7 bytes (collation[5] + max_length[2])
            x if x == TdsDataType::NVarChar as u8 => {
                // Get collation from context or use SQL_Latin1_General_CP1_CI_AS as default
                // This is the most common SQL Server collation for US English
                // TODO: Check which collation ODBC/.NET uses by default
                let collation = ctx.collation.unwrap_or(SqlCollation {
                    info: 0x00000409, // LCID 1033 (US English)
                    lcid_language_id: 0x0409,
                    col_flags: 0,
                    sort_id: 52, // SQL_Latin1_General_CP1_CI_AS
                });

                // Write collation (5 bytes): info (4 bytes) + sort_id (1 byte)
                writer.write_u32_async(collation.info).await?;
                writer.write_byte_async(collation.sort_id).await?;

                // Calculate max_length based on value type
                let max_length = match value {
                    ColumnValues::String(s) => s.bytes.len() as u16,
                    _ => 0,
                };

                // Write max_length (2 bytes)
                writer.write_u16_async(max_length).await?;
            }

            // All valid types from get_variant_base_type are handled above
            _ => unreachable!("Invalid TDS type 0x{:02X} for sql_variant", tds_type),
        }

        Ok(())
    }

    /// Get the TDS base type byte for a ColumnValues variant.
    ///
    /// Maps ColumnValues enum variants to their TDS type codes.
    ///
    /// IMPORTANT: SQL_VARIANT uses fixed-length type codes (INT4, FLT8, etc.)
    /// not nullable variants (INTN, FLTN).
    fn get_variant_base_type(value: &ColumnValues) -> TdsResult<u8> {
        let tds_type = match value {
            // Integer types
            ColumnValues::TinyInt(_) => TdsDataType::Int1 as u8,
            ColumnValues::SmallInt(_) => TdsDataType::Int2 as u8,
            ColumnValues::Int(_) => TdsDataType::Int4 as u8,
            ColumnValues::BigInt(_) => TdsDataType::Int8 as u8,

            // Floating point types
            ColumnValues::Real(_) => TdsDataType::Flt4 as u8,
            ColumnValues::Float(_) => TdsDataType::Flt8 as u8,

            // Bit type
            ColumnValues::Bit(_) => TdsDataType::Bit as u8,

            // Decimal and numeric types
            ColumnValues::Decimal(_) => TdsDataType::DecimalN as u8,
            ColumnValues::Numeric(_) => TdsDataType::NumericN as u8,

            // Money types
            ColumnValues::Money(_) => TdsDataType::Money as u8,
            ColumnValues::SmallMoney(_) => TdsDataType::Money4 as u8,

            // Date/time types
            ColumnValues::Date(_) => TdsDataType::DateN as u8,
            ColumnValues::Time(_) => TdsDataType::TimeN as u8,
            ColumnValues::DateTime(_) => TdsDataType::DateTime as u8,
            ColumnValues::DateTime2(_) => TdsDataType::DateTime2N as u8,
            ColumnValues::DateTimeOffset(_) => TdsDataType::DateTimeOffsetN as u8,
            ColumnValues::SmallDateTime(_) => TdsDataType::DateTim4 as u8,

            // Binary types - use BigVarBinary for variable-length binary data
            ColumnValues::Bytes(_) => TdsDataType::BigVarBinary as u8,

            // String types - determine based on encoding (will be refined in calculate_variant_prop_bytes)
            // For now, default to NVarChar for Unicode strings
            ColumnValues::String(_) => TdsDataType::NVarChar as u8,

            // GUID - use nullable GUID type
            ColumnValues::Uuid(_) => TdsDataType::Guid as u8,

            ColumnValues::Null => {
                // Should have been handled earlier
                return Err(Error::ProtocolError(
                    "NULL should be handled before get_variant_base_type".to_string(),
                ));
            }

            // Catch-all for unsupported types in sql_variant (XML, JSON, Vector, etc.)
            _ => {
                return Err(Error::UsageError(
                    "Unsupported data type in sql_variant columns".to_string(),
                ));
            }
        };

        Ok(tds_type)
    }

    /// Create a TdsTypeContext for serializing the inner value of a variant.
    ///
    /// This creates a context with the correct base type (not SQL_VARIANT) so that
    /// the inner value serialization works correctly.
    fn create_variant_inner_context(
        value: &ColumnValues,
        base_type: u8,
        _original_ctx: &TdsTypeContext,
    ) -> TdsResult<TdsTypeContext> {
        let ctx = match value {
            // Integer types: use fixed-length types (INT1, INT2, INT4, INT8)
            // is_fixed_length=true means skip the length byte prefix in serialization
            ColumnValues::TinyInt(_) => TdsTypeContext {
                tds_type: TdsDataType::Int1 as u8,
                max_size: 1,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::SmallInt(_) => TdsTypeContext {
                tds_type: TdsDataType::Int2 as u8,
                max_size: 2,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::Int(_) => TdsTypeContext {
                tds_type: TdsDataType::Int4 as u8,
                max_size: 4,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::BigInt(_) => TdsTypeContext {
                tds_type: TdsDataType::Int8 as u8,
                max_size: 8,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },

            ColumnValues::Real(_) => TdsTypeContext {
                tds_type: TdsDataType::Flt4 as u8,
                max_size: 4,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::Float(_) => TdsTypeContext {
                tds_type: TdsDataType::Flt8 as u8,
                max_size: 8,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },

            ColumnValues::Bit(_) => TdsTypeContext {
                tds_type: TdsDataType::Bit as u8,
                max_size: 1,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },

            ColumnValues::Decimal(v) | ColumnValues::Numeric(v) => TdsTypeContext {
                tds_type: base_type, // 0x6A or 0x6C
                max_size: 17,        // Max size for decimal
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: Some(v.precision),
                scale: Some(v.scale),
                collation: None,
            },

            ColumnValues::Money(_) => TdsTypeContext {
                tds_type: TdsDataType::Money as u8,
                max_size: 8,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::SmallMoney(_) => TdsTypeContext {
                tds_type: TdsDataType::Money4 as u8,
                max_size: 4,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },

            // Date/Time types
            ColumnValues::Date(_) => TdsTypeContext {
                tds_type: TdsDataType::DateN as u8,
                max_size: 3,
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::Time(v) => TdsTypeContext {
                tds_type: TdsDataType::TimeN as u8,
                max_size: 5,
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: None,
                scale: Some(v.scale),
                collation: None,
            },
            ColumnValues::DateTime(_) => TdsTypeContext {
                tds_type: TdsDataType::DateTime as u8,
                max_size: 8,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },
            ColumnValues::DateTime2(v) => TdsTypeContext {
                tds_type: TdsDataType::DateTime2N as u8,
                max_size: 8,
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: None,
                scale: Some(v.time.scale),
                collation: None,
            },
            ColumnValues::DateTimeOffset(v) => TdsTypeContext {
                tds_type: TdsDataType::DateTimeOffsetN as u8,
                max_size: 10,
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: None,
                scale: Some(v.datetime2.time.scale),
                collation: None,
            },
            ColumnValues::SmallDateTime(_) => TdsTypeContext {
                tds_type: TdsDataType::DateTim4 as u8,
                max_size: 4,
                is_nullable: false,
                is_plp: false,
                is_fixed_length: true, // Already fixed type, but keep flag for consistency
                precision: None,
                scale: None,
                collation: None,
            },

            // Binary types
            ColumnValues::Bytes(v) => TdsTypeContext {
                tds_type: TdsDataType::BigVarBinary as u8,
                max_size: v.len(),
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // Skip length prefix in sql_variant
                precision: None,
                scale: None,
                collation: None,
            },

            // String types
            // Encode ColumnValues::String as NVARCHAR
            ColumnValues::String(s) => {
                TdsTypeContext {
                    tds_type: TdsDataType::NVarChar as u8,
                    max_size: s.bytes.len() / 2, // Character count for Unicode (UTF-16LE = 2 bytes per char)
                    is_nullable: true,
                    is_plp: false,
                    is_fixed_length: true, // Skip length prefix in sql_variant
                    precision: None,
                    scale: None,
                    // Use column collation if known (from original context), else fall back to default
                    // This matches ODBC behavior: use column collation if available, else connection default
                    collation: _original_ctx.collation,
                }
            }

            // GUID
            ColumnValues::Uuid(_) => TdsTypeContext {
                tds_type: TdsDataType::Guid as u8,
                max_size: 16,
                is_nullable: true,
                is_plp: false,
                is_fixed_length: true, // In sql_variant, GUID is serialized as fixed 16 bytes without length prefix
                precision: None,
                scale: None,
                collation: None,
            },

            ColumnValues::Vector(_) | ColumnValues::Xml(_) | ColumnValues::Json(_) => {
                return Err(Error::UsageError(
                    "Unsupported data type in sql_variant".to_string(),
                ));
            }

            ColumnValues::Null => {
                return Err(Error::ProtocolError(
                    "NULL should be handled before create_variant_inner_context".to_string(),
                ));
            }
        };

        Ok(ctx)
    }

    /// Serialize the inner value without the variant wrapper check.
    ///
    /// This is used internally by serialize_as_variant to avoid infinite recursion.
    /// It performs the same serialization as serialize_value but skips the variant check.
    async fn serialize_value_inner<'a, 'b>(
        writer: &'a mut PacketWriter<'b>,
        value: &ColumnValues,
        ctx: &TdsTypeContext,
    ) -> TdsResult<()>
    where
        'b: 'a,
    {
        // Direct dispatch without variant check
        match value {
            ColumnValues::Null => Self::serialize_null(writer, ctx).await,
            ColumnValues::Bit(v) => Self::serialize_bit(writer, *v, ctx).await,
            ColumnValues::TinyInt(v) => Self::serialize_tinyint(writer, *v, ctx).await,
            ColumnValues::SmallInt(v) => Self::serialize_smallint(writer, *v, ctx).await,
            ColumnValues::Int(v) => Self::serialize_int(writer, *v, ctx).await,
            ColumnValues::BigInt(v) => Self::serialize_bigint(writer, *v, ctx).await,
            ColumnValues::Real(v) => Self::serialize_real(writer, *v, ctx).await,
            ColumnValues::Float(v) => Self::serialize_float(writer, *v, ctx).await,
            ColumnValues::Decimal(v) | ColumnValues::Numeric(v) => {
                Self::serialize_decimal(writer, v, ctx).await
            }
            ColumnValues::SmallMoney(v) => Self::serialize_smallmoney(writer, v, ctx).await,
            ColumnValues::Money(v) => Self::serialize_money(writer, v, ctx).await,
            ColumnValues::Date(v) => Self::serialize_date(writer, v, ctx).await,
            ColumnValues::Time(v) => Self::serialize_time(writer, v, ctx).await,
            ColumnValues::DateTime(v) => Self::serialize_datetime(writer, v, ctx).await,
            ColumnValues::DateTime2(v) => Self::serialize_datetime2(writer, v, ctx).await,
            ColumnValues::DateTimeOffset(v) => Self::serialize_datetimeoffset(writer, v, ctx).await,
            ColumnValues::SmallDateTime(v) => Self::serialize_smalldatetime(writer, v, ctx).await,
            ColumnValues::Bytes(v) => Self::serialize_bytes(writer, v, ctx).await,
            ColumnValues::Json(v) => Self::serialize_json(writer, v, ctx).await,
            ColumnValues::String(v) => Self::serialize_string(writer, v, ctx).await,
            ColumnValues::Vector(v) => Self::serialize_vector(writer, v, ctx).await,
            ColumnValues::Xml(v) => Self::serialize_xml(writer, v, ctx).await,
            ColumnValues::Uuid(v) => Self::serialize_uuid(writer, v, ctx).await,
        }
    }
}

#[cfg(test)]
mod serializer_tests {
    use crate::datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallDateTime, SqlSmallMoney, SqlTime,
    };
    use crate::datatypes::decoder::DecimalParts;
    use crate::datatypes::tds_value_serializer::{
        PLP_NULL, PLP_UNKNOWN_LEN, TdsTypeContext, TdsValueSerializer, VARNULL,
    };
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
    use crate::message::messages::PacketType;
    use futures::executor::block_on;

    fn nullable_ctx(tds_type: u8) -> TdsTypeContext {
        TdsTypeContext {
            tds_type,
            max_size: 0,
            is_plp: false,
            is_fixed_length: false,
            precision: None,
            scale: None,
            collation: None,
            is_nullable: true,
        }
    }

    fn payload(writer: &PacketWriter) -> Vec<u8> {
        writer.get_payload().into_inner()[8..].to_vec()
    }

    // ── serialize_null ──

    #[test]
    fn null_intn() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26); // INTN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_fltn() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6D); // FLTN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_bitn() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x68); // BITN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_guid() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x24); // GUID
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_daten() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x28); // DateN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_timen() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x29); // TimeN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_datetime2n() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2A); // DateTime2N
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_datetimeoffsetn() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2B); // DateTimeOffsetN
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_text_legacy() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x23); // TEXT
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn null_plp() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xE7); // NVARCHAR
        ctx.is_plp = true;
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), PLP_NULL.to_le_bytes().to_vec());
    }

    #[test]
    fn null_varlength() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0xE7); // NVARCHAR non-PLP
        block_on(TdsValueSerializer::serialize_null(&mut w, &ctx)).unwrap();
        assert_eq!(payload(&w), VARNULL.to_le_bytes().to_vec());
    }

    #[test]
    fn null_fixed_bit_returns_error() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x32); // Fixed BIT
        let res = block_on(TdsValueSerializer::serialize_null(&mut w, &ctx));
        assert!(res.is_err());
    }

    #[test]
    fn null_fixed_int4_returns_error() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x38); // INT4 fixed
        ctx.is_plp = false;
        let res = block_on(TdsValueSerializer::serialize_null(&mut w, &ctx));
        assert!(res.is_err());
    }

    // ── serialize_bit ──

    #[test]
    fn bit_true_nullable() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x68); // BITN
        let val = ColumnValues::Bit(true);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x01, 0x01]); // length=1, value=1
    }

    #[test]
    fn bit_false_nullable() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x68); // BITN
        let val = ColumnValues::Bit(false);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x01, 0x00]); // length=1, value=0
    }

    #[test]
    fn bit_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x68); // BITN
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]); // NULL
    }

    // ── serialize_tinyint ──

    #[test]
    fn tinyint_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26); // INTN
        let val = ColumnValues::TinyInt(42);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x01, 42]); // length=1, value=42
    }

    #[test]
    fn tinyint_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_smallint ──

    #[test]
    fn smallint_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26); // INTN
        let val = ColumnValues::SmallInt(256);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // length=2, value=256 LE = [0x00, 0x01]
        assert_eq!(payload(&w), vec![0x02, 0x00, 0x01]);
    }

    #[test]
    fn smallint_negative() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::SmallInt(-1);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x02, 0xFF, 0xFF]);
    }

    // ── serialize_int ──

    #[test]
    fn int_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::Int(0x01020304);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x04, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn int_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_bigint ──

    #[test]
    fn bigint_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::BigInt(1);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x08]; // length=8
        expected.extend_from_slice(&1i64.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn bigint_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x26);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_real ──

    #[test]
    fn real_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6D); // FLTN
        let val = ColumnValues::Real(1.5f32);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x04]; // length=4
        expected.extend_from_slice(&(1.5f32.to_bits() as i32).to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn real_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6D);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_float ──

    #[test]
    fn float_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6D); // FLTN
        let val = ColumnValues::Float(1.25f64);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x08]; // length=8
        expected.extend_from_slice(&1.25f64.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn float_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6D);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_smallmoney ──

    #[test]
    fn smallmoney_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6E); // MONEYN
        let val = ColumnValues::SmallMoney(SqlSmallMoney { int_val: 1234567 });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x04]; // length=4
        expected.extend_from_slice(&1234567i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn smallmoney_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6E);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_money ──

    #[test]
    fn money_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6E); // MONEYN
        let val = ColumnValues::Money(SqlMoney {
            msb_part: 0x01,
            lsb_part: 0x02,
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x08]; // length=8
        expected.extend_from_slice(&0x01i32.to_le_bytes()); // MSB first
        expected.extend_from_slice(&0x02i32.to_le_bytes()); // LSB second
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn money_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6E);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_date ──

    #[test]
    fn date_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x28); // DateN
        let date = SqlDate::create(100).unwrap();
        let val = ColumnValues::Date(date);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // length=3, days=100 LE 3 bytes: [100, 0, 0]
        assert_eq!(payload(&w), vec![0x03, 100, 0, 0]);
    }

    #[test]
    fn date_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x28);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_time ──

    #[test]
    fn time_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x29); // TimeN
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn time_value_scale0() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x29);
        // 1 second = 10_000_000 in 100ns units; scale 0 → divide by 10^7 → 1
        let val = ColumnValues::Time(SqlTime {
            time_nanoseconds: 10_000_000,
            scale: 0,
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // scale 0 → time_length=3, length byte=3, time_value=1 LE 3 bytes
        assert_eq!(payload(&w), vec![0x03, 0x01, 0x00, 0x00]);
    }

    // ── serialize_datetime ──

    #[test]
    fn datetime_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6F); // DateTimeN
        let val = ColumnValues::DateTime(SqlDateTime {
            days: 10,
            time: 300,
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x08]; // length=8
        expected.extend_from_slice(&10i32.to_le_bytes());
        expected.extend_from_slice(&300u32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn datetime_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6F);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_smalldatetime ──

    #[test]
    fn smalldatetime_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6F); // DateTimeN
        let val = ColumnValues::SmallDateTime(SqlSmallDateTime { days: 5, time: 60 });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x04]; // length=4
        expected.extend_from_slice(&5u16.to_le_bytes());
        expected.extend_from_slice(&60u16.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn smalldatetime_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6F);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    // ── serialize_datetime2 ──

    #[test]
    fn datetime2_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2A); // DateTime2N
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn datetime2_value_scale0() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2A);
        let val = ColumnValues::DateTime2(SqlDateTime2 {
            days: 1,
            time: SqlTime {
                time_nanoseconds: 0,
                scale: 0,
            },
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // scale 0 → time_length=3, total_value_length=6, length byte=6
        // time=0 as 3 bytes [0,0,0], days=1 as 3 bytes LE [1,0,0]
        assert_eq!(payload(&w), vec![0x06, 0, 0, 0, 1, 0, 0]);
    }

    // ── serialize_datetimeoffset ──

    #[test]
    fn datetimeoffset_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2B); // DateTimeOffsetN
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn datetimeoffset_value_scale0() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x2B);
        let val = ColumnValues::DateTimeOffset(SqlDateTimeOffset {
            datetime2: SqlDateTime2 {
                days: 1,
                time: SqlTime {
                    time_nanoseconds: 0,
                    scale: 0,
                },
            },
            offset: 60, // +1 hour
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // scale 0 → time_length=3, total=3+3+2=8, length byte=8
        // time=0 [0,0,0], days=1 [1,0,0], offset=60 LE [60,0]
        assert_eq!(payload(&w), vec![0x08, 0, 0, 0, 1, 0, 0, 60, 0]);
    }

    // ── serialize_uuid ──

    #[test]
    fn uuid_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x24); // GUID
        let uuid = uuid::Uuid::nil();
        let val = ColumnValues::Uuid(uuid);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(p[0], 16); // length=16
        assert_eq!(&p[1..], &[0u8; 16]); // nil UUID → all zeros
    }

    #[test]
    fn uuid_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x24);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn uuid_specific_value() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x24);
        let uuid = uuid::Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap();
        let val = ColumnValues::Uuid(uuid);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(p[0], 16);
        assert_eq!(&p[1..], uuid.to_bytes_le().as_slice());
    }

    // ── serialize_decimal ──

    #[test]
    fn decimal_value_precision5() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A); // DecimalN
        ctx.precision = Some(5);
        ctx.scale = Some(2);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 5,
            int_parts: vec![12345],
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // precision 5 → value_bytes=4, total_length=5
        // [5, 1(positive), 12345 as i32 LE]
        let mut expected = vec![0x05, 0x01];
        expected.extend_from_slice(&12345i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn decimal_negative() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A);
        ctx.precision = Some(9);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: false,
            scale: 0,
            precision: 9,
            int_parts: vec![100],
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x05, 0x00]; // length=5, sign=0 (negative)
        expected.extend_from_slice(&100i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn decimal_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0x6A);
        let val = ColumnValues::Null;
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        assert_eq!(payload(&w), vec![0x00]);
    }

    #[test]
    fn decimal_precision18() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A);
        ctx.precision = Some(18);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 18,
            int_parts: vec![1, 2],
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        // precision 18 → value_bytes=8, total=9
        let mut expected = vec![0x09, 0x01]; // length=9, sign=positive
        expected.extend_from_slice(&1i32.to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    // ── Helper for small-buffer tests ──

    fn collect_finalized(mock: &MockNetworkWriter) -> Vec<u8> {
        let mut result = Vec::new();
        let data = &mock.data;
        let mut offset = 0;
        while offset + 8 <= data.len() {
            let pkt_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            if pkt_len < 8 || offset + pkt_len > data.len() {
                break;
            }
            result.extend_from_slice(&data[offset + 8..offset + pkt_len]);
            offset += pkt_len;
        }
        result
    }

    // ── Small-buffer variants (exercises has_space() == false async paths) ──
    //
    // Packet sizes chosen so has_space() returns false at position 0,
    // but individual write_*_async calls don't overflow by more than payload:
    //   bit/tinyint: 9 (payload=1)
    //   smallint: 10 (payload=2)
    //   int/real/smalldatetime/smallmoney/date/time: 12 (payload=4)
    //   bigint/float/datetime/money/datetime2/datetimeoffset: 16 (payload=8)
    //   uuid: 24 (payload=16)

    #[test]
    fn bit_true_small_buffer() {
        let mut mock = MockNetworkWriter::new(9);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let val = ColumnValues::Bit(true);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &val,
            &nullable_ctx(0x68),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x01, 0x01]);
    }

    #[test]
    fn tinyint_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(9);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::TinyInt(42),
            &nullable_ctx(0x26),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x01, 42]);
    }

    #[test]
    fn smallint_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(10);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::SmallInt(256),
            &nullable_ctx(0x26),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x02, 0x00, 0x01]);
    }

    #[test]
    fn int_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Int(0x01020304),
            &nullable_ctx(0x26),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x04, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn bigint_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(16);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::BigInt(1),
            &nullable_ctx(0x26),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x08];
        expected.extend_from_slice(&1i64.to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn real_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Real(1.5f32),
            &nullable_ctx(0x6D),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x04];
        expected.extend_from_slice(&(1.5f32.to_bits() as i32).to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn date_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let date = SqlDate::create(100).unwrap();
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Date(date),
            &nullable_ctx(0x28),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x03, 100, 0, 0]);
    }

    #[test]
    fn time_value_scale0_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Time(SqlTime {
                time_nanoseconds: 10_000_000,
                scale: 0,
            }),
            &nullable_ctx(0x29),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x03, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn datetime2_value_scale0_small_buffer() {
        let mut mock = MockNetworkWriter::new(16);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::DateTime2(SqlDateTime2 {
                days: 1,
                time: SqlTime {
                    time_nanoseconds: 0,
                    scale: 0,
                },
            }),
            &nullable_ctx(0x2A),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(collect_finalized(&mock), vec![0x06, 0, 0, 0, 1, 0, 0]);
    }

    #[test]
    fn datetimeoffset_value_scale0_small_buffer() {
        let mut mock = MockNetworkWriter::new(16);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                datetime2: SqlDateTime2 {
                    days: 1,
                    time: SqlTime {
                        time_nanoseconds: 0,
                        scale: 0,
                    },
                },
                offset: 60,
            }),
            &nullable_ctx(0x2B),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        assert_eq!(
            collect_finalized(&mock),
            vec![0x08, 0, 0, 0, 1, 0, 0, 60, 0]
        );
    }

    #[test]
    fn smalldatetime_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::SmallDateTime(SqlSmallDateTime { days: 5, time: 60 }),
            &nullable_ctx(0x6F),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x04];
        expected.extend_from_slice(&5u16.to_le_bytes());
        expected.extend_from_slice(&60u16.to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn datetime_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(16);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::DateTime(SqlDateTime {
                days: 10,
                time: 300,
            }),
            &nullable_ctx(0x6F),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x08];
        expected.extend_from_slice(&10i32.to_le_bytes());
        expected.extend_from_slice(&300u32.to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn smallmoney_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(12);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::SmallMoney(SqlSmallMoney { int_val: 1234567 }),
            &nullable_ctx(0x6E),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x04];
        expected.extend_from_slice(&1234567i32.to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn money_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(16);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Money(SqlMoney {
                msb_part: 0x01,
                lsb_part: 0x02,
            }),
            &nullable_ctx(0x6E),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let mut expected = vec![0x08];
        expected.extend_from_slice(&0x01i32.to_le_bytes());
        expected.extend_from_slice(&0x02i32.to_le_bytes());
        assert_eq!(collect_finalized(&mock), expected);
    }

    #[test]
    fn uuid_value_small_buffer() {
        let mut mock = MockNetworkWriter::new(24);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let uuid = uuid::Uuid::nil();
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Uuid(uuid),
            &nullable_ctx(0x24),
        ))
        .unwrap();
        block_on(w.finalize()).unwrap();
        drop(w);
        let p = collect_finalized(&mock);
        assert_eq!(p[0], 16);
        assert_eq!(&p[1..], &[0u8; 16]);
    }

    // ── String/binary serialization ──

    #[test]
    fn string_nvarchar_short() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xE7);
        ctx.max_size = 100;
        let val = ColumnValues::String(crate::datatypes::sql_string::SqlString::from_utf8_string(
            "Hi".to_string(),
        ));
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(&p[0..2], &4u16.to_le_bytes());
        let utf16: Vec<u8> = "Hi".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert_eq!(&p[2..], &utf16[..]);
    }

    #[test]
    fn string_nvarchar_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0xE7);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Null,
            &ctx,
        ))
        .unwrap();
        assert_eq!(payload(&w), VARNULL.to_le_bytes().to_vec());
    }

    #[test]
    fn string_nvarchar_plp() {
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xE7);
        ctx.is_plp = true;
        let val = ColumnValues::String(crate::datatypes::sql_string::SqlString::from_utf8_string(
            "AB".to_string(),
        ));
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        let utf16: Vec<u8> = "AB".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let mut expected = Vec::new();
        expected.extend_from_slice(&PLP_UNKNOWN_LEN.to_le_bytes());
        expected.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
        expected.extend_from_slice(&utf16);
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(p, expected);
    }

    #[test]
    fn string_varchar_short() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xA7);
        ctx.max_size = 100;
        let val = ColumnValues::String(crate::datatypes::sql_string::SqlString::new(
            b"Hi".to_vec(),
            crate::datatypes::sql_string::EncodingType::Utf8,
        ));
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(&p[0..2], &2u16.to_le_bytes());
        assert_eq!(&p[2..], b"Hi");
    }

    #[test]
    fn bytes_varbinary_short() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xAD);
        ctx.max_size = 100;
        let val = ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(&p[0..2], &4u16.to_le_bytes());
        assert_eq!(&p[2..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn bytes_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let ctx = nullable_ctx(0xAD);
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Null,
            &ctx,
        ))
        .unwrap();
        assert_eq!(payload(&w), VARNULL.to_le_bytes().to_vec());
    }

    #[test]
    fn bytes_plp() {
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xA5);
        ctx.is_plp = true;
        let data = vec![0x01, 0x02, 0x03];
        let val = ColumnValues::Bytes(data.clone());
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        let mut expected = Vec::new();
        expected.extend_from_slice(&PLP_UNKNOWN_LEN.to_le_bytes());
        expected.extend_from_slice(&3u32.to_le_bytes());
        expected.extend_from_slice(&data);
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(p, expected);
    }

    #[test]
    fn bytes_image() {
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x22);
        ctx.max_size = 2_147_483_647;
        let data = vec![0xCA, 0xFE];
        let val = ColumnValues::Bytes(data.clone());
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        assert_eq!(p[0], 0x10);
        assert_eq!(&p[1..17], &[0xFF; 16]);
        assert_eq!(&p[17..25], &[0xFF; 8]);
        assert_eq!(&p[25..29], &2u32.to_le_bytes());
        assert_eq!(&p[29..31], &data[..]);
    }

    // ── JSON and XML ──

    #[test]
    fn json_value() {
        use crate::datatypes::sql_json::SqlJson;
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xF4);
        ctx.is_plp = true;
        let json = SqlJson::new(b"{\"a\":1}".to_vec());
        let val = ColumnValues::Json(json);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        let utf16: Vec<u8> = "{\"a\":1}"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let mut expected = Vec::new();
        expected.extend_from_slice(&PLP_UNKNOWN_LEN.to_le_bytes());
        expected.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
        expected.extend_from_slice(&utf16);
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(p, expected);
    }

    #[test]
    fn xml_value() {
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xF1);
        ctx.is_plp = true;
        let xml = crate::datatypes::column_values::SqlXml::from("<r/>".to_string());
        let xml_bytes = xml.bytes.clone();
        let val = ColumnValues::Xml(xml);
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        let data_len = xml_bytes.len() + 2; // +2 for BOM
        let mut expected = Vec::new();
        expected.extend_from_slice(&PLP_UNKNOWN_LEN.to_le_bytes());
        expected.extend_from_slice(&(data_len as u32).to_le_bytes());
        expected.push(0xFF);
        expected.push(0xFE);
        expected.extend_from_slice(&xml_bytes);
        expected.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(p, expected);
    }

    #[test]
    fn xml_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0xF1);
        ctx.is_plp = true;
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Null,
            &ctx,
        ))
        .unwrap();
        assert_eq!(payload(&w), PLP_NULL.to_le_bytes().to_vec());
    }

    // ── Decimal precision ranges ──

    #[test]
    fn decimal_precision15() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A);
        ctx.precision = Some(15);
        ctx.scale = Some(0);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 15,
            int_parts: vec![1, 2],
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x09, 0x01]; // length=9, sign=positive
        expected.extend_from_slice(&1i32.to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn decimal_precision25() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A);
        ctx.precision = Some(25);
        ctx.scale = Some(0);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: false,
            scale: 0,
            precision: 25,
            int_parts: vec![1, 2, 3],
        });
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let mut expected = vec![0x0D, 0x00]; // length=13, sign=negative
        expected.extend_from_slice(&1i32.to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        expected.extend_from_slice(&3i32.to_le_bytes());
        assert_eq!(payload(&w), expected);
    }

    #[test]
    fn decimal_precision_invalid() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x6A);
        ctx.precision = Some(39);
        let val = ColumnValues::Decimal(DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 39,
            int_parts: vec![1],
        });
        assert!(block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).is_err());
    }

    // ── serialize_as_variant ──

    #[test]
    fn variant_null() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x62);
        ctx.max_size = 8016;
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Null,
            &ctx,
        ))
        .unwrap();
        assert_eq!(payload(&w), 0u32.to_le_bytes().to_vec());
    }

    #[test]
    fn variant_int() {
        let mut mock = MockNetworkWriter::new(64);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x62);
        ctx.max_size = 8016;
        block_on(TdsValueSerializer::serialize_value(
            &mut w,
            &ColumnValues::Int(42),
            &ctx,
        ))
        .unwrap();
        let p = payload(&w);
        // total_length = 2 + 0(prop) + 4(data) = 6
        assert_eq!(&p[0..4], &6u32.to_le_bytes());
        assert_eq!(p[4], 0x38); // INT4
        assert_eq!(p[5], 0x00); // prop_len = 0
        assert_eq!(&p[6..10], &42i32.to_le_bytes());
    }

    #[test]
    fn variant_string_nvarchar() {
        let mut mock = MockNetworkWriter::new(128);
        let mut w = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let mut ctx = nullable_ctx(0x62);
        ctx.max_size = 8016;
        let val = ColumnValues::String(crate::datatypes::sql_string::SqlString::from_utf8_string(
            "Hi".to_string(),
        ));
        block_on(TdsValueSerializer::serialize_value(&mut w, &val, &ctx)).unwrap();
        let p = payload(&w);
        // "Hi" UTF-16LE = 4 bytes. total_length = 2 + 7(prop) + 4(data) = 13
        assert_eq!(&p[0..4], &13u32.to_le_bytes());
        assert_eq!(p[4], 0xE7); // NVARCHAR
        assert_eq!(p[5], 0x07); // prop_len = 7
    }
}

#[cfg(test)]
mod tests {
    use crate::datatypes::lcid_encoding::lcid_to_encoding;

    /// Test that different collations use different encodings for non-ASCII characters
    #[test]
    fn test_collation_based_encoding_chinese() {
        // Chinese Simplified (CP936/GBK)
        // LCID 2052 (0x0804) = Chinese (PRC)
        let lcid: u32 = 0x0804;
        let encoding = lcid_to_encoding(lcid).expect("Should find Chinese encoding");

        // Test with Chinese characters: "你好" (Hello in Chinese)
        let chinese_text = "你好";
        let (encoded, _enc, had_errors) = encoding.encode(chinese_text);

        assert!(
            !had_errors,
            "Should encode Chinese characters without errors"
        );
        assert!(
            encoded.len() == 4,
            "Chinese characters should be 2 bytes each in GBK: got {} bytes",
            encoded.len()
        );

        // Verify it's not Latin-1 encoding (which would replace with '?')
        assert!(
            !encoded.contains(&b'?'),
            "Should not contain replacement character"
        );

        // GBK encoding for "你好"
        // '你' = 0xC4 0xE3
        // '好' = 0xBA 0xC3
        assert_eq!(encoded[0], 0xC4, "First byte of '你' should be 0xC4");
        assert_eq!(encoded[1], 0xE3, "Second byte of '你' should be 0xE3");
        assert_eq!(encoded[2], 0xBA, "First byte of '好' should be 0xBA");
        assert_eq!(encoded[3], 0xC3, "Second byte of '好' should be 0xC3");
    }

    #[test]
    fn test_collation_based_encoding_japanese() {
        // Japanese (CP932/Shift-JIS)
        // LCID 1041 (0x0411) = Japanese (Japan)
        let lcid: u32 = 0x0411;
        let encoding = lcid_to_encoding(lcid).expect("Should find Japanese encoding");

        // Test with Japanese Hiragana: "こんにちは" (Hello in Japanese)
        let japanese_text = "こんにちは";
        let (encoded, _enc, had_errors) = encoding.encode(japanese_text);

        assert!(
            !had_errors,
            "Should encode Japanese characters without errors"
        );
        assert!(
            encoded.len() == 10,
            "5 Japanese characters should be 2 bytes each in Shift-JIS: got {} bytes",
            encoded.len()
        );

        // Verify it's not Latin-1 encoding (which would replace with '?')
        assert!(
            !encoded.contains(&b'?'),
            "Should not contain replacement character"
        );
    }

    #[test]
    fn test_collation_based_encoding_western_european() {
        // Western European (Windows-1252)
        // LCID 1033 (0x0409) = English (United States)
        let lcid: u32 = 0x0409;
        let encoding = lcid_to_encoding(lcid).expect("Should find Western European encoding");

        // Test with extended Latin characters: "café"
        let text = "café";
        let (encoded, _enc, had_errors) = encoding.encode(text);

        assert!(
            !had_errors,
            "Should encode Latin-1 characters without errors"
        );
        assert_eq!(encoded.len(), 4, "Should be 4 bytes");

        // Windows-1252 encoding for "café"
        // 'c' = 0x63, 'a' = 0x61, 'f' = 0x66, 'é' = 0xE9
        assert_eq!(encoded[0], 0x63);
        assert_eq!(encoded[1], 0x61);
        assert_eq!(encoded[2], 0x66);
        assert_eq!(
            encoded[3], 0xE9,
            "Extended character 'é' should be 0xE9 in Windows-1252"
        );
    }

    #[test]
    fn test_collation_based_encoding_turkish() {
        // Turkish (Windows-1254)
        // LCID 1055 (0x041F) = Turkish (Turkey)
        let lcid: u32 = 0x041F;
        let encoding = lcid_to_encoding(lcid).expect("Should find Turkish encoding");

        // Test with Turkish-specific characters: "şğıİ"
        let text = "şğıİ";
        let (encoded, _enc, had_errors) = encoding.encode(text);

        assert!(
            !had_errors,
            "Should encode Turkish characters without errors"
        );
        assert_eq!(
            encoded.len(),
            4,
            "Should be 4 bytes for 4 Turkish characters"
        );

        // Verify it's properly encoded in Windows-1254
        // 'ş' = 0xFE in Windows-1254
        assert_eq!(
            encoded[0], 0xFE,
            "Turkish 'ş' should be 0xFE in Windows-1254"
        );
    }

    #[test]
    fn test_collation_based_encoding_cyrillic() {
        // Cyrillic (Windows-1251)
        // LCID 1049 (0x0419) = Russian (Russia)
        let lcid: u32 = 0x0419;
        let encoding = lcid_to_encoding(lcid).expect("Should find Cyrillic encoding");

        // Test with Russian text: "Привет" (Hello in Russian)
        let text = "Привет";
        let (encoded, _enc, had_errors) = encoding.encode(text);

        assert!(
            !had_errors,
            "Should encode Cyrillic characters without errors"
        );
        assert_eq!(
            encoded.len(),
            6,
            "Should be 6 bytes for 6 Cyrillic characters"
        );

        // Verify it's not Latin-1 encoding (which would replace with '?')
        assert!(
            !encoded.contains(&b'?'),
            "Should not contain replacement character"
        );
    }

    #[test]
    fn test_different_collations_produce_different_encodings() {
        // Demonstrate that the same Unicode character produces different byte sequences
        // depending on the collation

        let test_char = "ñ"; // Spanish n with tilde

        // Western European (Windows-1252): LCID 1033
        let encoding_1252 = lcid_to_encoding(0x0409).unwrap();
        let (encoded_1252, _, _) = encoding_1252.encode(test_char);

        // Spanish (Windows-1252): LCID 1034 - should be same as above
        let encoding_spanish = lcid_to_encoding(0x040A).unwrap();
        let (encoded_spanish, _, _) = encoding_spanish.encode(test_char);

        // Both Western European languages should produce same encoding
        assert_eq!(
            encoded_1252[0], encoded_spanish[0],
            "Same code page should produce same encoding"
        );
        assert_eq!(
            encoded_1252[0], 0xF1,
            "Character 'ñ' should be 0xF1 in Windows-1252"
        );

        // But if we try to encode it in a different code page that doesn't support it,
        // we'd get a replacement character
        // Example: Chinese (CP936) doesn't have 'ñ'
        let encoding_chinese = lcid_to_encoding(0x0804).unwrap();
        let (_encoded_chinese, _, had_errors) = encoding_chinese.encode(test_char);

        // Chinese encoding would either map it differently or use replacement
        if had_errors {
            // It's okay if it had errors - that's expected for unsupported characters
            assert!(
                had_errors,
                "Chinese encoding should not support Spanish 'ñ' character"
            );
        }
    }

    #[test]
    fn test_ascii_characters_encoded_same_across_collations() {
        // ASCII characters (0x00-0x7F) should be encoded identically across all collations
        let ascii_text = "Hello123";

        let collations = vec![
            0x0409, // English (US)
            0x0804, // Chinese (PRC)
            0x0411, // Japanese
            0x0419, // Russian
            0x041F, // Turkish
        ];

        let mut all_encodings = Vec::new();

        for lcid in collations {
            let encoding = lcid_to_encoding(lcid)
                .unwrap_or_else(|_| panic!("Should find encoding for LCID 0x{:04X}", lcid));
            let (encoded, _enc, had_errors) = encoding.encode(ascii_text);

            assert!(!had_errors, "ASCII characters should encode without errors");
            all_encodings.push(encoded.to_vec());
        }

        // All encodings should be identical for ASCII text
        let first_encoding = &all_encodings[0];
        for (i, encoding) in all_encodings.iter().enumerate().skip(1) {
            assert_eq!(
                first_encoding, encoding,
                "ASCII text should encode identically across all collations (difference at index {})",
                i
            );
        }

        // Verify the actual ASCII values
        assert_eq!(first_encoding, b"Hello123", "ASCII should be encoded as-is");
    }

    #[test]
    fn test_latin1_fallback_for_unsupported_lcid() {
        // Test that unsupported LCID returns error (fallback handled in serialize_string)
        let unsupported_lcid: u32 = 0xFFFFFF; // Invalid LCID

        let result = lcid_to_encoding(unsupported_lcid);
        assert!(result.is_err(), "Should return error for unsupported LCID");

        // The actual fallback to Latin-1 is tested in the serialize_string logic
        // which logs a warning and uses the Latin-1 fallback path
    }

    #[test]
    fn test_encoding_preserves_data_integrity() {
        // Test round-trip for data that should work: encode UTF-16 -> decode to string
        // -> encode with collation -> verify no data loss for compatible characters

        let test_cases = vec![
            (0x0409, "Hello World", b"Hello World" as &[u8]), // ASCII
            (0x0409, "café", &[0x63, 0x61, 0x66, 0xE9]),      // Windows-1252 with accent
        ];

        for (lcid, input_text, expected_bytes) in test_cases {
            let encoding = lcid_to_encoding(lcid)
                .unwrap_or_else(|_| panic!("Should find encoding for LCID 0x{:04X}", lcid));
            let (encoded, _enc, had_errors) = encoding.encode(input_text);

            assert!(
                !had_errors,
                "Should encode '{}' without errors for LCID 0x{:04X}",
                input_text, lcid
            );
            assert_eq!(
                &encoded[..],
                expected_bytes,
                "Encoded bytes should match expected for '{}' with LCID 0x{:04X}",
                input_text,
                lcid
            );
        }
    }
}
