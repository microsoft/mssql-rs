// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use core::fmt;
use std::{fmt::Debug, io::Error, vec};

use super::{
    sql_string::{SqlString, get_encoding_type},
    sqldatatypes::{TdsDataType, TypeInfoVariant},
};
use crate::datatypes::sqldatatypes::TypeInfo;
use crate::{
    core::TdsResult,
    datatypes::{sql_json::SqlJson, sql_string::EncodingType, sqldatatypes::FixedLengthTypes},
};
use crate::{
    datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
    },
    io::packet_reader::TdsPacketReader,
};
use crate::{query::metadata::ColumnMetadata, token::tokens::SqlCollation};

use super::row_writer::{RowWriter, write_column_value};

// Maximum reasonable allocation size for a single value (100MB)
// This prevents fuzzer-induced capacity overflow panics
#[cfg(fuzzing)]
const MAX_ALLOC_SIZE: usize = 64 * 1024; // 64KB for fuzzing
#[cfg(not(fuzzing))]
const MAX_ALLOC_SIZE: usize = 100 * 1024 * 1024;

// Maximum allocation size for PLP (Partial Length Pointer) types
// SQL Server supports PLP types up to 2GB (i32::MAX is approximately 2.1GB)
#[cfg(fuzzing)]
const MAX_PLP_SIZE: usize = 64 * 1024; // 64KB for fuzzing
#[cfg(not(fuzzing))]
const MAX_PLP_SIZE: usize = i32::MAX as usize;

// Helper function to validate allocation size before allocating
#[inline]
fn validate_alloc_size(size: usize, context: &str) -> TdsResult<()> {
    if size > MAX_ALLOC_SIZE {
        #[cfg(fuzzing)]
        {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "[ALLOC-REJECT] {} requesting {} bytes (max {})",
                context,
                size,
                MAX_ALLOC_SIZE
            );
        }

        return Err(crate::error::Error::ProtocolError(format!(
            "{context}: allocation size {size} exceeds maximum allowed {MAX_ALLOC_SIZE} bytes"
        )));
    }
    #[cfg(fuzzing)]
    {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "[ALLOC-OK] {} requesting {} bytes",
            context,
            size
        );
    }
    Ok(())
}

// Macro to create validated Vec allocations
#[cfg(fuzzing)]
macro_rules! safe_vec {
    ($elem:expr; $size:expr, $context:expr) => {{
        let size = $size;
        validate_alloc_size(size, $context)?;
        vec![$elem; size]
    }};
}

#[cfg(not(fuzzing))]
macro_rules! safe_vec {
    ($elem:expr; $size:expr, $context:expr) => {{
        let size = $size;
        validate_alloc_size(size, $context)?;
        vec![$elem; size]
    }};
}

#[async_trait]
pub(crate) trait SqlTypeDecode {
    async fn decode<T>(&self, reader: &mut T, metadata: &ColumnMetadata) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync;
}

impl From<u8> for ColumnValues {
    fn from(value: u8) -> Self {
        ColumnValues::TinyInt(value)
    }
}

impl From<i32> for ColumnValues {
    fn from(value: i32) -> Self {
        ColumnValues::Int(value)
    }
}

#[derive(Debug, Default)]
pub(crate) struct GenericDecoder {
    string_decoder: StringDecoder,
}

impl GenericDecoder {
    #[cfg(test)]
    const SHORTLEN_MAXVALUE: usize = 65535;
    const SQL_PLP_NULL: usize = 0xffffffffffffffff;
    const SQL_PLP_UNKNOWNLEN: usize = 0xfffffffffffffffe;

    // Reads a SQL_VARIANT type from the TDS stream.
    async fn read_sql_variant<T>(&self, reader: &mut T) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let length = reader.read_uint32().await?;
        let variant_base_type = reader.read_byte().await?;
        let tds_type = TdsDataType::try_from(variant_base_type)?;
        let variant_prop_bytes = reader.read_byte().await?;
        let bytes_for_type_and_properties_byte = 2;

        // Use checked arithmetic to prevent integer underflow
        let data_length = length
            .checked_sub(bytes_for_type_and_properties_byte)
            .and_then(|v| v.checked_sub(variant_prop_bytes as u32))
            .ok_or_else(|| {
                crate::error::Error::ProtocolError(format!(
                    "SQL_VARIANT data length calculation underflow: length={length}, prop_bytes={variant_prop_bytes}"
                ))
            })?;

        let col_value = match variant_prop_bytes {
            0 => {
                self.decode_zero_propbyte_variant(reader, tds_type, data_length)
                    .await?
            }
            1 => {
                // TIMENTYPE, DATETIME2NTYPE, DATETIMEOFFSETNTYPE
                self.decode_one_byte_variant(reader, tds_type, data_length)
                    .await?
            }
            2 => {
                decode_two_propbyte_variant(reader, variant_base_type, tds_type, data_length)
                    .await?
            }
            7 => {
                // BIGVARCHARTYPE, BIGCHARTYPE, NVARCHARTYPE, NCHARTYPE
                decode_seven_propbyte_variant(reader, tds_type, data_length).await?
            }
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Unexpected SQL variant properties length: {variant_prop_bytes}. Expected 0, 1, 2, or 7. This indicates malformed or invalid data."
                )));
            }
        };
        Ok(col_value)
    }

    async fn decode_zero_propbyte_variant<T>(
        &self,
        reader: &mut T,
        tds_type: TdsDataType,
        data_length: u32,
    ) -> Result<ColumnValues, crate::error::Error>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let fixed_length_type_result = FixedLengthTypes::try_from(tds_type);

        // The type may be a fixed length type, or it may be a variable length type like Guid/DateN
        match fixed_length_type_result {
            Ok(fixed_length_type) => {
                let type_info = TypeInfo {
                    tds_type,
                    length: data_length as usize,
                    type_info_variant: TypeInfoVariant::FixedLen(fixed_length_type),
                };
                let variant_actual_type_md = ColumnMetadata {
                    user_type: 0,
                    flags: 0,
                    type_info,
                    data_type: tds_type,
                    column_name: "".to_string(),
                    multi_part_name: None,
                };
                self.decode(reader, &variant_actual_type_md).await
            }
            _ => {
                // If the type is not a fixed length type, we should not reach here.
                match tds_type {
                    TdsDataType::Guid => Self::read_guid(reader, data_length as u8).await,
                    TdsDataType::DateN => Self::read_daten(reader, data_length as u8).await,
                    _ => Err(crate::error::Error::ProtocolError(format!(
                        "For 0 byte property, only Guid and DateN are expected, but got: {tds_type:?}"
                    ))),
                }
            }
        }
    }

    async fn decode_one_byte_variant<T>(
        &self,
        reader: &mut T,
        tds_type: TdsDataType,
        data_length: u32,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let scale = reader.read_byte().await?;
        Ok(match tds_type {
            TdsDataType::TimeN => {
                let time_nanos = self.read_time(reader, data_length as u8, scale).await?;
                ColumnValues::Time(time_nanos)
            }
            TdsDataType::DateTime2N => {
                self.read_datetime2(reader, data_length as u8, scale)
                    .await?
            }
            TdsDataType::DateTimeOffsetN => {
                self.read_datetime_offset(reader, data_length as u8, scale)
                    .await?
            }
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid SQL_VARIANT: 1-byte property is only valid for TimeN, DateTime2N, and DateTimeOffsetN types, but got: {tds_type:?}"
                )));
            }
        })
    }

    async fn read_decimal<T>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
    ) -> TdsResult<Option<DecimalParts>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        // Decimal/numeric data type has 1 byte length.
        let length = reader.read_byte().await?;
        let TypeInfoVariant::VarLenPrecisionScale(_, _, precision, scale) =
            metadata.type_info.type_info_variant
        else {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid type info variant for Decimal/Numeric: expected VarLenPrecisionScale, got: {:?}",
                metadata.type_info.type_info_variant
            )));
        };
        GenericDecoder::read_decimal_data(reader, length, precision, scale).await
    }

    async fn read_decimal_data<T>(
        reader: &mut T,
        length: u8,
        precision: u8,
        scale: u8,
    ) -> TdsResult<Option<DecimalParts>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        // If length is 0, then it is NULL.
        if length == 0 {
            return Ok(None);
        }
        let sign = reader.read_byte().await?;
        let is_positive = sign == 1;

        let number_of_int_parts = (length - 1) >> 2;

        // Limit decimal parts allocation for fuzzing
        #[cfg(fuzzing)]
        const MAX_DECIMAL_INT_PARTS: u8 = 10; // Maximum 10 int parts = 40 bytes
        #[cfg(not(fuzzing))]
        const MAX_DECIMAL_INT_PARTS: u8 = 64; // SQL Server max precision is 38, which needs max ~17 int parts

        if number_of_int_parts > MAX_DECIMAL_INT_PARTS {
            return Err(crate::error::Error::ProtocolError(format!(
                "Decimal int parts {number_of_int_parts} exceeds maximum allowed {MAX_DECIMAL_INT_PARTS} (length was {length})"
            )));
        }

        let int_parts_len = number_of_int_parts as usize;
        validate_alloc_size(int_parts_len * 4, "read_decimal int_parts")?;
        let mut int_parts = vec![0i32; int_parts_len];
        for part_index in 0..number_of_int_parts {
            int_parts[part_index as usize] = reader.read_int32().await?;
        }

        Ok(Some(DecimalParts {
            is_positive,
            scale,
            precision,
            int_parts,
        }))
    }

    async fn read_datetime<T>(&self, reader: &mut T) -> TdsResult<SqlDateTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = reader.read_int32().await?;
        let ticks = reader.read_uint32().await?;

        Ok(SqlDateTime { days, time: ticks })
    }

    async fn read_small_datetime<T>(&self, reader: &mut T) -> TdsResult<SqlSmallDateTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = reader.read_uint16().await?;
        let minutes = reader.read_uint16().await?;
        Ok(SqlSmallDateTime {
            days,
            time: minutes,
        })
    }

    async fn read_date<T>(reader: &mut T) -> TdsResult<SqlDate>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let days = reader.read_uint24().await?;
        Ok(SqlDate::unchecked_create(days))
    }

    async fn read_time<T>(&self, reader: &mut T, byte_len: u8, scale: u8) -> TdsResult<SqlTime>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let scaled_value = match byte_len {
            3 => reader.read_uint24().await? as u64,
            4 => reader.read_uint32().await? as u64,
            _ => reader.read_uint40().await?,
        };

        // The value from SQL Server is in scaled units based on the scale:
        // Scale 0: seconds (need to multiply by 10^7)
        // Scale 1: tenths of seconds (multiply by 10^6)
        // Scale 2: hundredths (multiply by 10^5)
        // Scale 3: milliseconds (multiply by 10^4)
        // Scale 4: ten-thousandths (multiply by 10^3)
        // Scale 5: hundred-thousandths (multiply by 10^2)
        // Scale 6: microseconds (multiply by 10^1)
        // Scale 7: 100-nanoseconds (multiply by 10^0 = no scaling)
        // We need to convert to 100-nanosecond units for consistency
        let time_nanoseconds = match scale {
            0 => scaled_value * 10_000_000, // Seconds to 100ns
            1 => scaled_value * 1_000_000,  // Tenths to 100ns
            2 => scaled_value * 100_000,    // Hundredths to 100ns
            3 => scaled_value * 10_000,     // Milliseconds to 100ns
            4 => scaled_value * 1_000,      // Ten-thousandths to 100ns
            5 => scaled_value * 100,        // Hundred-thousandths to 100ns
            6 => scaled_value * 10,         // Microseconds to 100ns
            7 => scaled_value,              // Already in 100ns
            _ => scaled_value,              // Fallback
        };

        Ok(SqlTime {
            time_nanoseconds,
            scale,
        })
    }

    async fn read_datetime2<T>(
        &self,
        reader: &mut T,
        byte_len: u8,
        scale: u8,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let time_byte_len = byte_len.checked_sub(3).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Invalid DateTime2 byte length: {byte_len}. Expected at least 3 bytes for date component."
            ))
        })?;
        let time_nanos = self.read_time(reader, time_byte_len, scale).await?;
        let sql_date = Self::read_date(reader).await?;
        let datetime2 = SqlDateTime2 {
            days: sql_date.get_days(),
            time: time_nanos,
        };
        Ok(ColumnValues::DateTime2(datetime2))
    }

    async fn read_datetime_offset<T>(
        &self,
        reader: &mut T,
        byte_len: u8,
        scale: u8,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let datetime2_byte_len = byte_len.checked_sub(2).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Invalid DateTimeOffset byte length: {byte_len}. Expected at least 2 bytes for offset component."
            ))
        })?;
        let datetime2 = self
            .read_datetime2(reader, datetime2_byte_len, scale)
            .await?;
        let datetime2 = match datetime2 {
            ColumnValues::DateTime2(dt2) => dt2,
            _ => {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Internal error: read_datetime2 returned unexpected type: {datetime2:?}"
                )));
            }
        };
        let offset = reader.read_int16().await?;
        let datetime_offset = SqlDateTimeOffset { datetime2, offset };
        Ok(ColumnValues::DateTimeOffset(datetime_offset))
    }

    async fn read_intn<T>(&self, reader: &mut T, byte_len: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let value: ColumnValues = match byte_len {
            1 => ColumnValues::TinyInt(reader.read_byte().await?), // Some(reader.read_byte().await? as i64),
            2 => ColumnValues::SmallInt(reader.read_int16().await?), // Some(reader.read_int16().await? as i64),
            4 => ColumnValues::Int(reader.read_int32().await?),
            8 => ColumnValues::BigInt(reader.read_int64().await?),
            0 => ColumnValues::Null,
            _ => {
                return Err(crate::error::Error::from(Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Invalid IntN length",
                )));
            }
        };
        Ok(value)
    }

    async fn read_money4<T>(&self, reader: &mut T) -> TdsResult<SqlSmallMoney>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let small_money_val = reader.read_int32().await?;
        Ok(small_money_val.into())
    }

    // Reads the TDS 8-byte money value. It is represented in TDS as two 4-byte integers (mixed endian).
    // See comments in MoneyParts definition for more details.
    async fn read_money8<T>(&self, reader: &mut T) -> TdsResult<SqlMoney>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let msb = reader.read_int32().await?;
        let lsb = reader.read_int32().await?;
        Ok(SqlMoney {
            lsb_part: lsb,
            msb_part: msb,
        })
    }

    async fn read_daten<T>(reader: &mut T, length: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        if length == 0 {
            Ok(ColumnValues::Null)
        } else {
            // length == 3.
            Ok(ColumnValues::Date(Self::read_date(reader).await?))
        }
    }

    async fn read_guid<T>(reader: &mut T, length: u8) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        if length > 0 {
            // UUID must be exactly 16 bytes
            if length != 16 {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid GUID length: expected 16 bytes, got {length}"
                )));
            }
            let mut bytes = safe_vec![0u8; length as usize, "read_guid"];
            reader.read_bytes(&mut bytes).await?;
            let unique_id = uuid::Uuid::from_slice_le(&bytes).map_err(|e| {
                crate::error::Error::ProtocolError(format!("Failed to parse UUID: {e}"))
            })?;
            Ok(ColumnValues::Uuid(unique_id))
        } else {
            Ok(ColumnValues::Null)
        }
    }

    async fn decode_vector<T>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
    ) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        use crate::datatypes::sql_vector::SqlVector;
        use crate::datatypes::sqldatatypes::{
            VECTOR_HEADER_SIZE, VECTOR_MAX_SIZE, VectorBaseType, VectorLayoutFormat,
            VectorLayoutVersion,
        };

        // Read length prefix (USHORTLEN format)
        let length_prefix_value = reader.read_uint16().await? as usize;

        // Handle NULL (length = 0xFFFF)
        if length_prefix_value == 0xFFFF {
            return Ok(ColumnValues::Null);
        }

        // Validate length
        if length_prefix_value > VECTOR_MAX_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length {} exceeds maximum of {} bytes",
                length_prefix_value, VECTOR_MAX_SIZE
            )));
        }

        // Must have at least header
        if length_prefix_value < VECTOR_HEADER_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length {} is less than minimum header size of {} bytes",
                length_prefix_value, VECTOR_HEADER_SIZE
            )));
        }

        // Read 8-byte header
        let layout_format_byte = reader.read_byte().await?;
        let layout_version_byte = reader.read_byte().await?;
        let dimension_count = reader.read_uint16().await?;
        let base_type_byte = reader.read_byte().await?;
        let _reserved1 = reader.read_byte().await?; // Reserved
        let _reserved2 = reader.read_byte().await?; // Reserved
        let _reserved3 = reader.read_byte().await?; // Reserved

        // Validate header using enum conversions
        let _layout_format = VectorLayoutFormat::try_from(layout_format_byte)?;
        let _layout_version = VectorLayoutVersion::try_from(layout_version_byte)?;

        // Get base type from metadata's TypeInfoVariant scale field
        let base_type_in_metadata = match &metadata.type_info.type_info_variant {
            TypeInfoVariant::VarLenScale(_, scale) => *scale,
            _ => {
                return Err(crate::error::Error::ProtocolError(
                    "Vector metadata missing scale (base type)".to_string(),
                ));
            }
        };

        if base_type_byte != base_type_in_metadata {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector base type mismatch: metadata has 0x{:02X}, vector header has 0x{:02X}",
                base_type_in_metadata, base_type_byte
            )));
        }

        // Validate base type using enum conversion
        let base_type = VectorBaseType::try_from(base_type_byte)?;

        let length_in_metadata = metadata.type_info.length;
        // Calculate data length based on vector header info
        let element_size = base_type.element_size_bytes();
        let length_from_vector_header =
            VECTOR_HEADER_SIZE + (dimension_count as usize * element_size);
        if length_prefix_value != length_from_vector_header
            || length_prefix_value != length_in_metadata
        {
            return Err(crate::error::Error::ProtocolError(format!(
                "Vector length mismatch: length in prefix {} bytes, length from vector header {} bytes, length in metadata {} bytes, for {} dimensions (element size: {} bytes)",
                length_prefix_value,
                length_from_vector_header,
                length_in_metadata,
                dimension_count,
                element_size
            )));
        }

        // Read raw element bytes (let SqlVector parse based on base_type)
        let element_bytes = length_prefix_value - VECTOR_HEADER_SIZE;
        let mut raw_bytes = vec![0u8; element_bytes];
        reader.read_bytes(&mut raw_bytes).await?;

        // Create SqlVector - try_from_raw validates header, parses bytes by type, and validates dimensions
        let vector = SqlVector::try_from_raw(
            layout_format_byte,
            layout_version_byte,
            base_type_byte,
            raw_bytes,
        )?;

        Ok(ColumnValues::Vector(vector))
    }

    async fn read_plp_bytes<T>(reader: &mut T) -> TdsResult<Option<Vec<u8>>>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let long_len_i64 = reader.read_int64().await?;
        let long_len = long_len_i64 as u64;

        // If the length is SQL_PLP_NULL, it means the value is NULL.
        if long_len as usize == Self::SQL_PLP_NULL {
            Ok(None)
        } else {
            // If the length is SQL_PLP_UNKNOWNLEN, it means the length is unknown and we have to
            // gather all the chunks until we reach the end of the PLP data which is a zero length
            // chunk.
            let mut vector_capacity = if long_len as usize != Self::SQL_PLP_UNKNOWNLEN {
                let capacity = long_len as usize;
                // Check for overflow or excessively large values
                // If long_len_i64 was negative, casting to u64 then usize can produce huge values
                if long_len_i64 < 0 || capacity > MAX_PLP_SIZE {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "PLP length {capacity} (raw i64: {long_len_i64}) exceeds maximum allowed size of {MAX_PLP_SIZE} bytes"
                    )));
                }
                capacity
            } else {
                0
            };
            let mut plp_buffer = vec![0u8; vector_capacity];
            let mut chunk_len = reader.read_uint32().await? as usize;
            let mut offset: usize = 0;
            let mut chunk_count = 0u32;

            #[cfg(fuzzing)]
            const MAX_PLP_CHUNKS: u32 = 1000;
            #[cfg(not(fuzzing))]
            const MAX_PLP_CHUNKS: u32 = 100000;

            #[cfg(fuzzing)]
            const MAX_CHUNK_SIZE: usize = 8 * 1024; // 8KB per chunk for fuzzing
            #[cfg(not(fuzzing))]
            const MAX_CHUNK_SIZE: usize = 16 * 1024 * 1024; // 16MB per chunk normally

            while chunk_len > 0 {
                chunk_count += 1;

                #[cfg(fuzzing)]
                {
                    eprintln!(
                        "[ALLOC] read_plp_bytes: chunk #{chunk_count}, chunk_len={chunk_len}, total_capacity={vector_capacity}"
                    );
                }

                if chunk_count > MAX_PLP_CHUNKS {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "Too many PLP chunks: {chunk_count} (max {MAX_PLP_CHUNKS})"
                    )));
                }

                // Limit individual chunk size
                if chunk_len > MAX_CHUNK_SIZE {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "PLP chunk size {chunk_len} exceeds maximum allowed chunk size of {MAX_CHUNK_SIZE} bytes"
                    )));
                }

                if long_len as usize == Self::SQL_PLP_UNKNOWNLEN {
                    // Use checked_add to prevent capacity overflow
                    vector_capacity = vector_capacity.checked_add(chunk_len).ok_or_else(|| {
                        crate::error::Error::ProtocolError(format!(
                            "PLP chunk accumulation would overflow capacity: {vector_capacity} + {chunk_len}"
                        ))
                    })?;
                    // Validate against MAX_PLP_SIZE after accumulation
                    if vector_capacity > MAX_PLP_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "PLP accumulated size {vector_capacity} exceeds maximum allowed size of {MAX_PLP_SIZE} bytes (SQL Server limit: 2GB)"
                        )));
                    }
                    plp_buffer.resize(vector_capacity, 0);
                } else {
                    // For known length, validate that chunk fits within the allocated buffer
                    let end_offset = offset.checked_add(chunk_len).ok_or_else(|| {
                        crate::error::Error::ProtocolError(format!(
                            "PLP chunk offset would overflow: {offset} + {chunk_len}"
                        ))
                    })?;
                    if end_offset > plp_buffer.len() {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "PLP chunk exceeds declared length: offset={offset}, chunk_len={chunk_len}, buffer_len={}, declared_len={long_len}",
                            plp_buffer.len()
                        )));
                    }
                }
                let chunk_size_read = reader
                    .read_bytes(&mut plp_buffer[offset..offset + chunk_len])
                    .await?;
                offset += chunk_size_read;
                chunk_len = reader.read_uint32().await? as usize;
            }
            Ok(Some(plp_buffer))
        }
    }

    /// Decodes a column value from the wire and writes it directly into a
    /// [`RowWriter`], bypassing the intermediate `ColumnValues` enum for
    /// common types. Rare types (XML, JSON, Vector, Image, UDT, SsVariant)
    /// fall back to `decode()` + `write_column_value()`.
    pub(crate) async fn decode_into<T, W>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
        col: usize,
        writer: &mut W,
    ) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
        W: RowWriter + ?Sized,
    {
        match metadata.data_type {
            // === Fixed-length integer types ===
            TdsDataType::Int1 => {
                writer.write_u8(col, reader.read_byte().await?);
            }
            TdsDataType::Int2 => {
                writer.write_i16(col, reader.read_int16().await?);
            }
            TdsDataType::Int4 => {
                writer.write_i32(col, reader.read_int32().await?);
            }
            TdsDataType::Int8 => {
                writer.write_i64(col, reader.read_int64().await?);
            }
            TdsDataType::IntN => {
                let byte_len = reader.read_byte().await?;
                match byte_len {
                    1 => writer.write_u8(col, reader.read_byte().await?),
                    2 => writer.write_i16(col, reader.read_int16().await?),
                    4 => writer.write_i32(col, reader.read_int32().await?),
                    8 => writer.write_i64(col, reader.read_int64().await?),
                    0 => writer.write_null(col),
                    _ => {
                        return Err(crate::error::Error::from(Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Invalid IntN length",
                        )));
                    }
                }
            }

            // === Fixed-length float types ===
            TdsDataType::Flt4 => {
                writer.write_f32(col, reader.read_float32().await?);
            }
            TdsDataType::Flt8 => {
                writer.write_f64(col, reader.read_float64().await?);
            }
            TdsDataType::FltN => {
                let length = reader.read_byte().await?;
                match length {
                    0 => writer.write_null(col),
                    4 => writer.write_f32(col, reader.read_float32().await?),
                    _ => writer.write_f64(col, reader.read_float64().await?),
                }
            }

            // === Bit types ===
            TdsDataType::Bit => {
                writer.write_bool(col, reader.read_byte().await? == 1);
            }
            TdsDataType::BitN => {
                let byte_len = reader.read_byte().await?;
                if byte_len > 0 {
                    writer.write_bool(col, reader.read_byte().await? == 1);
                } else {
                    writer.write_null(col);
                }
            }

            // === Money types ===
            TdsDataType::Money4 => {
                writer.write_smallmoney(col, self.read_money4(reader).await?);
            }
            TdsDataType::Money => {
                writer.write_money(col, self.read_money8(reader).await?);
            }
            TdsDataType::MoneyN => {
                let byte_len = reader.read_byte().await?;
                match byte_len {
                    4 => writer.write_smallmoney(col, self.read_money4(reader).await?),
                    8 => writer.write_money(col, self.read_money8(reader).await?),
                    0 => writer.write_null(col),
                    _ => {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid MoneyN length - {byte_len}"
                        )));
                    }
                }
            }

            // === Decimal / Numeric ===
            TdsDataType::DecimalN => match self.read_decimal(reader, metadata).await? {
                Some(val) => writer.write_decimal(col, val),
                None => writer.write_null(col),
            },
            TdsDataType::NumericN => match self.read_decimal(reader, metadata).await? {
                Some(val) => writer.write_numeric(col, val),
                None => writer.write_null(col),
            },

            // === String types — delegate to StringDecoder ===
            TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::NText
            | TdsDataType::Text => {
                self.string_decoder
                    .decode_string_into(reader, metadata, col, writer)
                    .await?;
            }

            // === Binary types ===
            TdsDataType::BigBinary => {
                let length = reader.read_uint16().await?;
                if length as usize > MAX_ALLOC_SIZE {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "BigBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                    )));
                }
                let mut bytes = vec![0u8; length as usize];
                reader.read_bytes(&mut bytes).await?;
                writer.write_bytes(col, bytes);
            }
            TdsDataType::BigVarBinary => {
                if metadata.is_plp() {
                    match GenericDecoder::read_plp_bytes(reader).await? {
                        Some(bytes) => writer.write_bytes(col, bytes),
                        None => writer.write_null(col),
                    }
                } else {
                    let length = reader.read_uint16().await?;
                    if length as usize > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "BigVarBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut bytes = vec![0u8; length as usize];
                    reader.read_bytes(&mut bytes).await?;
                    writer.write_bytes(col, bytes);
                }
            }

            // === DateTime types ===
            TdsDataType::DateTime => {
                writer.write_datetime(col, self.read_datetime(reader).await?);
            }
            TdsDataType::DateTim4 => {
                let daypart = reader.read_uint16().await?;
                let timepart = reader.read_uint16().await?;
                writer.write_smalldatetime(
                    col,
                    SqlSmallDateTime {
                        days: daypart,
                        time: timepart,
                    },
                );
            }
            TdsDataType::DateTimeN => {
                let length = reader.read_byte().await?;
                match length {
                    0 => writer.write_null(col),
                    4 => writer.write_smalldatetime(col, self.read_small_datetime(reader).await?),
                    _ => writer.write_datetime(col, self.read_datetime(reader).await?),
                }
            }
            TdsDataType::DateN => {
                let length = reader.read_byte().await?;
                if length == 0 {
                    writer.write_null(col);
                } else {
                    writer.write_date(col, Self::read_date(reader).await?);
                }
            }
            TdsDataType::TimeN => {
                let length = reader.read_byte().await?;
                if length == 0 {
                    writer.write_null(col);
                } else {
                    writer.write_time(
                        col,
                        self.read_time(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "TimeN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?,
                    );
                }
            }
            TdsDataType::DateTime2N => {
                let length = reader.read_byte().await?;
                if length == 0 {
                    writer.write_null(col);
                } else {
                    let cv = self
                        .read_datetime2(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTime2N type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?;
                    if let ColumnValues::DateTime2(dt2) = cv {
                        writer.write_datetime2(col, dt2);
                    }
                }
            }
            TdsDataType::DateTimeOffsetN => {
                let length = reader.read_byte().await?;
                if length == 0 {
                    writer.write_null(col);
                } else {
                    let cv = self
                        .read_datetime_offset(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTimeOffsetN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await?;
                    if let ColumnValues::DateTimeOffset(dto) = cv {
                        writer.write_datetimeoffset(col, dto);
                    }
                }
            }

            // === GUID ===
            TdsDataType::Guid => {
                let length = reader.read_byte().await?;
                if length == 0 {
                    writer.write_null(col);
                } else {
                    if length != 16 {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid GUID length: expected 16 bytes, got {length}"
                        )));
                    }
                    let mut bytes = [0u8; 16];
                    reader.read_bytes(&mut bytes).await?;
                    let uuid = uuid::Uuid::from_slice_le(&bytes).map_err(|e| {
                        crate::error::Error::ProtocolError(format!("Failed to parse UUID: {e}"))
                    })?;
                    writer.write_uuid(col, uuid);
                }
            }

            // === Fallback: rare types go through decode() → write_column_value() ===
            _ => {
                let value = self.decode(reader, metadata).await?;
                write_column_value(writer, col, value);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SqlTypeDecode for GenericDecoder {
    async fn decode<T>(&self, reader: &mut T, metadata: &ColumnMetadata) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let result = match metadata.data_type {
            TdsDataType::Int1 => {
                let value = reader.read_byte().await?;
                ColumnValues::from(value)
            }
            TdsDataType::Int2 => {
                let value = reader.read_int16().await?;
                ColumnValues::SmallInt(value)
            }
            TdsDataType::Int4 => {
                let value = reader.read_int32().await?;
                ColumnValues::from(value)
            }
            TdsDataType::Int8 => {
                let value = reader.read_int64().await?;
                ColumnValues::BigInt(value)
            }
            TdsDataType::Flt4 => {
                let value = reader.read_float32().await?;
                ColumnValues::Real(value)
            }
            TdsDataType::Flt8 => {
                let value = reader.read_float64().await?;
                ColumnValues::Float(value)
            }
            TdsDataType::Money4 => ColumnValues::SmallMoney(self.read_money4(reader).await?),
            TdsDataType::Money => ColumnValues::Money(self.read_money8(reader).await?),
            TdsDataType::MoneyN => {
                let byte_len = reader.read_byte().await?;
                match byte_len {
                    4 => ColumnValues::SmallMoney(self.read_money4(reader).await?),
                    8 => ColumnValues::Money(self.read_money8(reader).await?),
                    0 => ColumnValues::Null,
                    _ => {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Invalid MoneyN length - {byte_len}"
                        )));
                    }
                }
            }
            TdsDataType::DecimalN => {
                let value = self.read_decimal(reader, metadata).await?;
                match value {
                    Some(value) => ColumnValues::Decimal(value),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::NumericN => {
                let value = self.read_decimal(reader, metadata).await?;
                match value {
                    Some(value) => ColumnValues::Numeric(value),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Bit => {
                let value = reader.read_byte().await?;
                ColumnValues::Bit(value == 1)
            }
            TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::Char
            | TdsDataType::VarChar
            | TdsDataType::NText
            | TdsDataType::Text => self.string_decoder.decode(reader, metadata).await?,
            TdsDataType::DateTime => {
                let value = self.read_datetime(reader).await?;
                ColumnValues::DateTime(value)
            }
            TdsDataType::IntN => {
                let byte_len = reader.read_byte().await?;
                self.read_intn(reader, byte_len).await?
            }
            TdsDataType::BigBinary => {
                let length = reader.read_uint16().await?;
                if length as usize > MAX_ALLOC_SIZE {
                    return Err(crate::error::Error::ProtocolError(format!(
                        "BigBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                    )));
                }
                let mut bytes = vec![0u8; length as usize];
                reader.read_bytes(&mut bytes).await?;
                ColumnValues::Bytes(bytes)
            }
            TdsDataType::BigVarBinary => {
                if metadata.is_plp() {
                    let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                    match some_bytes {
                        Some(bytes) => ColumnValues::Bytes(bytes),
                        None => ColumnValues::Null,
                    }
                } else {
                    let length = reader.read_uint16().await?;
                    if length as usize > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "BigVarBinary length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut bytes = vec![0u8; length as usize];
                    reader.read_bytes(&mut bytes).await?;
                    ColumnValues::Bytes(bytes)
                }
            }
            TdsDataType::Xml => {
                assert!(metadata.is_plp());
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Xml(SqlXml { bytes }),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Json => {
                assert!(metadata.is_plp());
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Json(SqlJson::new(bytes)),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::Vector => self.decode_vector(reader, metadata).await?,
            TdsDataType::BitN => {
                let byte_len = reader.read_byte().await?;
                if byte_len > 0 {
                    let value = reader.read_byte().await?;
                    ColumnValues::Bit(value == 1)
                } else {
                    ColumnValues::Null
                }
            }
            TdsDataType::Guid => {
                let length = reader.read_byte().await?;
                Self::read_guid(reader, length).await?
            }
            TdsDataType::FltN => {
                // This is variable length float, hence the length needs to be read first
                let length = reader.read_byte().await?;
                if length == 0 {
                    return Ok(ColumnValues::Null);
                }
                if length == 4 {
                    let value = reader.read_float32().await?;
                    ColumnValues::Real(value)
                } else {
                    let value = reader.read_float64().await?;
                    ColumnValues::Float(value)
                }
            }
            TdsDataType::DateTimeN => {
                let length = reader.read_byte().await?;
                // If length is 0, then it is NULL
                if length == 0 {
                    return Ok(ColumnValues::Null);
                } else if length == 4 {
                    // SmallDateTime
                    let smalldatetime = self.read_small_datetime(reader).await?;
                    return Ok(ColumnValues::SmallDateTime(smalldatetime));
                } else {
                    // DateTime
                    return Ok(ColumnValues::DateTime(self.read_datetime(reader).await?));
                }
            }
            TdsDataType::DateN => {
                let length = reader.read_byte().await?;
                return Self::read_daten(reader, length).await;
            }
            TdsDataType::TimeN => {
                let length = reader.read_byte().await?;
                match length {
                    0 => return Ok(ColumnValues::Null),
                    _ => {
                        return Ok(ColumnValues::Time(
                            self.read_time(
                                reader,
                                length,
                                metadata.get_scale().ok_or_else(|| {
                                    crate::error::Error::ImplementationError(
                                        "TimeN type should have scale".to_string(),
                                    )
                                })?,
                            )
                            .await?,
                        ));
                    }
                }
            }
            TdsDataType::DateTime2N => {
                let length = reader.read_byte().await?;
                match length {
                    0 => Ok(ColumnValues::Null),
                    _ => {
                        self.read_datetime2(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTime2N type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await
                    }
                }
            }?,
            TdsDataType::DateTimeOffsetN => {
                let length = reader.read_byte().await?;
                match length {
                    0 => Ok(ColumnValues::Null),
                    _ => {
                        self.read_datetime_offset(
                            reader,
                            length,
                            metadata.get_scale().ok_or_else(|| {
                                crate::error::Error::ImplementationError(
                                    "DateTimeOffsetN type should have scale".to_string(),
                                )
                            })?,
                        )
                        .await
                    }
                }
            }?,
            TdsDataType::Image => {
                let text_ptr_len = reader.read_byte().await? as usize;

                let length = if text_ptr_len > 0 {
                    const TIMESTAMP_BYTE_COUNT: usize = 8;
                    reader.skip_bytes(text_ptr_len).await?;
                    reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
                    reader.read_uint32().await? as usize
                } else {
                    0
                };

                if length == 0 {
                    ColumnValues::Null
                } else {
                    if length > MAX_ALLOC_SIZE {
                        return Err(crate::error::Error::ProtocolError(format!(
                            "Image length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                        )));
                    }
                    let mut buffer = vec![0u8; length];
                    reader.read_bytes(&mut buffer).await?;
                    ColumnValues::Bytes(buffer)
                }
            }
            TdsDataType::Udt => {
                assert!(metadata.is_plp());
                let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
                match some_bytes {
                    Some(bytes) => ColumnValues::Bytes(bytes),
                    None => ColumnValues::Null,
                }
            }
            TdsDataType::SsVariant => self.read_sql_variant(reader).await?,
            TdsDataType::DateTim4 => {
                let daypart = reader.read_uint16().await?;
                let timepart = reader.read_uint16().await?;
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: daypart,
                    time: timepart,
                })
            }
            TdsDataType::Decimal => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "Fixed-length Decimal type".to_string(),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not implemented. Use DecimalN instead.",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
            TdsDataType::Numeric => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: "Fixed-length Numeric type".to_string(),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not implemented. Use NumericN instead.",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
            _ => {
                return Err(crate::error::Error::UnimplementedFeature {
                    feature: format!("Data type {:?}", metadata.data_type),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not yet supported in the decoder",
                        metadata.data_type, metadata.data_type as u8
                    ),
                });
            }
        };
        Ok(result)
    }
}

#[derive(Debug, Default)]
struct StringDecoder;

impl StringDecoder {
    fn is_long_len_type(data_type: TdsDataType) -> bool {
        matches!(data_type, TdsDataType::NText | TdsDataType::Text)
    }

    async fn decode_string_into<T, W>(
        &self,
        reader: &mut T,
        metadata: &ColumnMetadata,
        col: usize,
        writer: &mut W,
    ) -> TdsResult<()>
    where
        T: TdsPacketReader + Send + Sync,
        W: RowWriter + ?Sized,
    {
        let encoding_type = get_encoding_type(metadata);

        if metadata.is_plp() {
            match GenericDecoder::read_plp_bytes(reader).await? {
                Some(bytes) => writer.write_string(col, SqlString::new(bytes, encoding_type)),
                None => writer.write_null(col),
            }
        } else if Self::is_long_len_type(metadata.data_type) {
            let text_ptr_len = reader.read_byte().await? as usize;

            if text_ptr_len == 0 {
                writer.write_null(col);
                return Ok(());
            }

            const TIMESTAMP_BYTE_COUNT: usize = 8;
            reader.skip_bytes(text_ptr_len).await?;
            reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
            let length = reader.read_uint32().await? as usize;

            if length > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Text data length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }

            let sql_string = if length == 0 {
                SqlString::new(Vec::new(), encoding_type)
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                SqlString::new(buffer, encoding_type)
            };
            writer.write_string(col, sql_string);
        } else {
            let length = reader.read_uint16().await? as usize;
            if length == 0xFFFF {
                writer.write_null(col);
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                writer.write_string(col, SqlString::new(buffer, encoding_type));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SqlTypeDecode for StringDecoder {
    async fn decode<T>(&self, reader: &mut T, metadata: &ColumnMetadata) -> TdsResult<ColumnValues>
    where
        T: TdsPacketReader + Send + Sync,
    {
        let encoding_type = get_encoding_type(metadata);

        // If Plp Column. (BIGVARCHARTYPE, BIGVARBINARYTYPE, NVARCHARTYPE with md.length == ushort.max)
        if metadata.is_plp() {
            let some_bytes = GenericDecoder::read_plp_bytes(reader).await?;
            match some_bytes {
                Some(bytes) => Ok(ColumnValues::String(SqlString::new(bytes, encoding_type))),
                None => Ok(ColumnValues::Null),
            }
        } else if Self::is_long_len_type(metadata.data_type) {
            // Legacy LOB types (TEXT/NTEXT/IMAGE) reading implementation
            //
            // WIRE FORMAT (from .NET TdsParser.cs:6517-6600):
            // 1. textptr_len (1 byte): Length of text pointer
            //    - 0x00 = NULL value
            //    - 0x10 (16) = Valid pointer (typical)
            // 2. textptr (textptr_len bytes): Text pointer (usually 16 bytes)
            //    - Server-managed pointer, client treats as opaque
            // 3. timestamp (8 bytes): Row timestamp
            //    - Used for optimistic concurrency
            // 4. data_length (4 bytes, uint32): Actual data length in bytes
            //    - For NTEXT: byte count (divide by 2 for char count)
            //    - For TEXT: byte count in the collation's encoding
            // 5. data (data_length bytes): The actual string data
            //    - For NTEXT: UTF-16LE encoded
            //    - For TEXT: encoded per collation (LCID-based)
            //
            // CURRENT IMPLEMENTATION STATUS:
            // Reads textptr_len (1 byte)
            // Skips textptr (16 bytes) and timestamp (8 bytes)
            // Reads data_length (4 bytes, uint32)
            // Allocates buffer and reads data
            // Creates SqlString with appropriate encoding type
            // NULL handling works (textptr_len = 0)
            // LCID-based decoding implemented (see sql_string.rs)
            let text_ptr_len = reader.read_byte().await? as usize;

            let length = if text_ptr_len > 0 {
                const TIMESTAMP_BYTE_COUNT: usize = 8;
                reader.skip_bytes(text_ptr_len).await?;
                reader.skip_bytes(TIMESTAMP_BYTE_COUNT).await?;
                reader.read_uint32().await? as usize
            } else {
                // text_ptr_len == 0 means NULL value
                return Ok(ColumnValues::Null);
            };

            // Empty string (length == 0 but textptr_len > 0) is valid - return empty string, not NULL
            if length > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Text data length {length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }

            let sql_string = if length == 0 {
                // Create empty SqlString with appropriate encoding
                SqlString::new(Vec::new(), encoding_type)
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;
                SqlString::new(buffer, encoding_type)
            };
            Ok(ColumnValues::String(sql_string))
        } else {
            let length = reader.read_uint16().await? as usize;
            if length == 0xFFFF {
                return Ok(ColumnValues::Null);
            } else {
                let mut buffer = vec![0u8; length];
                reader.read_bytes(&mut buffer).await?;

                let sql_string = SqlString::new(buffer, encoding_type);

                Ok(ColumnValues::String(sql_string))
            }
        }
    }
}

/// TDS representation of Decimal and Numeric types.
#[derive(Clone)]
pub struct DecimalParts {
    /// `true` for non-negative values.
    pub is_positive: bool,
    /// Number of digits after the decimal point.
    pub scale: u8,
    /// Total number of significant digits.
    pub precision: u8,
    /// 32-bit integer parts of the value (little-endian order).
    pub int_parts: Vec<i32>,
}

impl fmt::Display for DecimalParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_decimal_string())
    }
}

impl DecimalParts {
    /// Create a DecimalParts from a decimal string using BigDecimal.
    ///
    /// Supports SQL Server's full 38-digit precision using arbitrary-precision arithmetic.
    /// More efficient and robust than manual parsing.
    ///
    /// # Arguments
    /// * `s` - String like "123.45", "-0.01", "99999999999999999999999999999999999999"
    /// * `precision` - Total number of digits (1-38 for SQL Server)
    /// * `scale` - Number of digits after decimal point (0-precision)
    ///
    /// # Returns
    /// DecimalParts or Error if parsing fails or precision/scale validation fails
    pub fn from_string(s: &str, precision: u8, scale: u8) -> TdsResult<Self> {
        use bigdecimal::num_bigint::{BigInt, Sign};
        use bigdecimal::{BigDecimal, Zero};
        use std::str::FromStr;

        let trimmed = s.trim();

        // Check for negative zero in the original string before parsing
        // BigDecimal normalizes -0 to 0, so we need to detect it early
        let has_negative_sign = trimmed.starts_with('-');

        // Parse the string into a BigDecimal
        let decimal = BigDecimal::from_str(trimmed).map_err(|e| {
            crate::error::Error::TypeConversionError(format!(
                "Invalid decimal string '{}': {}",
                s, e
            ))
        })?;

        // Check if input has more fractional digits than target scale
        // Get the scale of the input decimal
        let input_scale = decimal.fractional_digit_count();
        if input_scale > scale as i64 {
            return Err(crate::error::Error::TypeConversionError(format!(
                "Input decimal scale {} exceeds target scale {}",
                input_scale, scale
            )));
        }

        // Handle zero case (but preserve sign from original string)
        if decimal.is_zero() {
            return Ok(DecimalParts {
                is_positive: !has_negative_sign,
                scale,
                precision,
                int_parts: vec![0],
            });
        }

        // Extract sign for non-zero values
        let is_positive = decimal.sign() != Sign::Minus;
        let abs_decimal = decimal.abs();

        // Scale the decimal: multiply by 10^scale to shift to integer representation
        let scale_factor = BigDecimal::from(10u64).powi(scale as i64);
        let scaled = abs_decimal * scale_factor;

        // Round to nearest integer
        let rounded = scaled.round(0);

        // Extract as BigInt, handling any remaining exponent
        let (bigint, exponent) = rounded.into_bigint_and_exponent();
        let final_bigint = if exponent > 0 {
            bigint * BigInt::from(10u64).pow(exponent as u32)
        } else if exponent < 0 {
            bigint / BigInt::from(10u64).pow((-exponent) as u32)
        } else {
            bigint
        };

        // Validate precision
        let digits_str = final_bigint.to_string();
        if digits_str.len() > precision as usize {
            return Err(crate::error::Error::TypeConversionError(format!(
                "Decimal value has {} digits, exceeds target precision {}",
                digits_str.len(),
                precision
            )));
        }

        // Convert BigInt to Vec<i32> for TDS wire format (little-endian 32-bit chunks)
        let bytes = final_bigint.to_signed_bytes_le();
        let mut int_parts = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let mut part: i32 = 0;
            for j in 0..4 {
                if i + j < bytes.len() {
                    part |= (bytes[i + j] as i32) << (j * 8);
                }
            }
            int_parts.push(part);
            i += 4;
        }

        if int_parts.is_empty() {
            int_parts.push(0);
        }

        Ok(DecimalParts {
            is_positive,
            scale,
            precision,
            int_parts,
        })
    }

    /// Create a DecimalParts from an i64 value.
    pub fn from_i64(value: i64, precision: u8, scale: u8) -> TdsResult<Self> {
        let is_positive = value >= 0;
        let abs_value = value.unsigned_abs();

        // Scale the value by multiplying by 10^scale
        let scaled_value = abs_value as u128 * 10u128.pow(scale as u32);

        // Convert to int_parts
        let mut int_parts = Vec::new();
        let mut remaining = scaled_value;
        while remaining > 0 || int_parts.is_empty() {
            int_parts.push((remaining & 0xFFFFFFFF) as i32);
            remaining >>= 32;
        }

        Ok(DecimalParts {
            is_positive,
            scale,
            precision,
            int_parts,
        })
    }

    /// Create a DecimalParts from an f64 value.
    pub fn from_f64(value: f64, precision: u8, scale: u8) -> TdsResult<Self> {
        // Convert f64 to string with appropriate precision
        let s = format!("{:.prec$}", value, prec = scale as usize);
        Self::from_string(&s, precision, scale)
    }

    /// Convert DecimalParts to a string representation suitable for Python Decimal.
    /// Returns a string like "123.45", "-0.01", etc.
    fn to_decimal_string(&self) -> String {
        // Convert int_parts to u128
        // int_parts[0] is the least significant, int_parts[n-1] is most significant
        let u128_value = self
            .int_parts
            .iter()
            .enumerate()
            .fold(0u128, |acc, (i, &part)| {
                acc + ((part as u32 as u128) << (i * 32))
            });

        let value_str = u128_value.to_string();

        // Insert decimal point at the correct position
        let result = if self.scale == 0 {
            value_str
        } else {
            let scale_pos = self.scale as usize;
            if value_str.len() <= scale_pos {
                // Need to pad with leading zeros
                format!("0.{}{}", "0".repeat(scale_pos - value_str.len()), value_str)
            } else {
                let split_pos = value_str.len() - scale_pos;
                format!("{}.{}", &value_str[..split_pos], &value_str[split_pos..])
            }
        };

        if self.is_positive {
            result
        } else {
            format!("-{}", result)
        }
    }

    fn to_f64(&self) -> f64 {
        let u128_value = self
            .int_parts
            .iter()
            .enumerate()
            .fold(0u128, |acc, (i, &part)| {
                acc + ((part as u32 as u128) << (i * 32))
            });

        let mut d_ret: f64 = u128_value as f64;

        d_ret /= 10.0_f64.powi(self.scale as i32);

        if self.is_positive { d_ret } else { -d_ret }
    }
}

impl PartialEq for DecimalParts {
    fn eq(&self, other: &Self) -> bool {
        let min_len = self.int_parts.len().min(other.int_parts.len());
        for i in 0..min_len {
            if self.int_parts[i] != other.int_parts[i] {
                return false;
            }
        }
        if self.int_parts.len() > other.int_parts.len() {
            if self.int_parts[min_len..].iter().any(|&x| x != 0) {
                return false;
            }
        } else if other.int_parts.len() > self.int_parts.len()
            && other.int_parts[min_len..].iter().any(|&x| x != 0)
        {
            return false;
        }
        self.is_positive == other.is_positive
            && self.scale == other.scale
            && self.precision == other.precision
    }
}

impl Debug for DecimalParts {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Decimal: {}{} Precision {} Scale {} F64 value: {}",
            if self.is_positive { "" } else { "-" },
            self.int_parts
                .iter()
                .map(|part| part.to_string())
                .collect::<Vec<String>>()
                .join(" "),
            self.precision,
            self.scale,
            self.to_f64()
        )
    }
}

async fn decode_two_propbyte_variant<T>(
    reader: &mut T,
    variant_base_type: u8,
    tds_type: TdsDataType,
    data_length: u32,
) -> TdsResult<ColumnValues>
where
    T: TdsPacketReader + Send + Sync,
{
    Ok(match tds_type {
        // BIGVARBINARYTYPE, BIGBINARYTYPE
        TdsDataType::BigVarBinary | TdsDataType::BigBinary => {
            let _max_length: u16 = reader.read_uint16().await?;
            if data_length as usize > MAX_ALLOC_SIZE {
                return Err(crate::error::Error::ProtocolError(format!(
                    "SQL Variant binary data length {data_length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
                )));
            }
            let mut buffer = vec![0u8; data_length as usize];
            reader.read_bytes(&mut buffer).await?;
            ColumnValues::Bytes(buffer)
        }
        TdsDataType::NumericN | TdsDataType::DecimalN => {
            let precision = reader.read_byte().await?;
            let scale = reader.read_byte().await?;
            let decimal_parts =
                GenericDecoder::read_decimal_data(reader, data_length as u8, precision, scale)
                    .await?;

            if matches!(tds_type, TdsDataType::NumericN) {
                match decimal_parts {
                    Some(value) => ColumnValues::Numeric(value),
                    None => ColumnValues::Null,
                }
            } else {
                match decimal_parts {
                    Some(value) => ColumnValues::Decimal(value),
                    None => ColumnValues::Null,
                }
            }
        }
        _ => {
            return Err(crate::error::Error::ProtocolError(format!(
                "Unexpected SQL variant base type for len(2) prop bytes: {variant_base_type:#04X}. Expected binary or numeric types."
            )));
        }
    })
}

async fn decode_seven_propbyte_variant<T>(
    reader: &mut T,
    tds_type: TdsDataType,
    data_length: u32,
) -> TdsResult<ColumnValues>
where
    T: TdsPacketReader + Send + Sync,
{
    assert!(matches!(
        tds_type,
        TdsDataType::BigVarChar | TdsDataType::BigChar | TdsDataType::NVarChar | TdsDataType::NChar
    ));
    let mut collation_bytes = vec![0u8; 5];
    reader.read_bytes(&mut collation_bytes).await?;
    let _max_length = reader.read_uint16().await? as usize;
    let collation: SqlCollation = collation_bytes.as_slice().try_into()?;
    if data_length as usize > MAX_ALLOC_SIZE {
        return Err(crate::error::Error::ProtocolError(format!(
            "SQL Variant string data length {data_length} exceeds maximum allowed size of {MAX_ALLOC_SIZE} bytes"
        )));
    }
    let mut buffer = vec![0u8; data_length as usize];
    reader.read_bytes(&mut buffer).await?;
    let encoding = if matches!(tds_type, TdsDataType::NVarChar | TdsDataType::NChar) {
        EncodingType::Utf16
    } else if collation.utf8() {
        EncodingType::Utf8
    } else {
        EncodingType::LcidBased(collation)
    };
    let sql_string = SqlString::new(buffer, encoding);
    Ok(ColumnValues::String(sql_string))
}

#[cfg(test)]
mod test {
    use crate::datatypes::{
        column_values::ColumnValues,
        decoder::{
            DecimalParts, GenericDecoder, MAX_ALLOC_SIZE, StringDecoder, validate_alloc_size,
        },
        sqldatatypes::TdsDataType,
    };

    #[test]
    fn test_f64_conversion() {
        let expected: f64 = 123456.322;

        // Represents 123456.322 as observed over TDS wire.
        let int_parts = vec![-539269688, 2];
        let parts = DecimalParts {
            is_positive: true,
            scale: 5,
            precision: 18,
            int_parts,
        };

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_negative() {
        let expected: f64 = -123456.322;

        // Represents -123456.322 as observed over TDS wire.
        let int_parts = vec![-539269688, 2];
        let parts = DecimalParts {
            is_positive: false,
            scale: 5,
            precision: 18,
            int_parts,
        };

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_zero() {
        let expected: f64 = 0.0;

        let int_parts = vec![0];
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts,
        };

        assert_eq!(expected, parts.to_f64());
    }

    #[test]
    fn test_f64_conversion_large_number() {
        // Test conversion with larger numbers
        let int_parts = vec![100000, 0];
        let parts = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 7,
            int_parts,
        };

        let result = parts.to_f64();
        // With scale of 2, int value 100000 should become 1000.00
        assert!((result - 1000.0).abs() < 0.01);
    }

    #[test]
    fn test_decimal_parts_with_multiple_int_parts() {
        // Test with multiple integer parts to ensure full conversion
        let int_parts = vec![1000000000, 1];
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 19,
            int_parts,
        };

        // Should successfully convert to f64
        let result = parts.to_f64();
        assert!(result > 0.0);
    }

    #[test]
    fn test_u8_to_column_values() {
        let value: u8 = 123;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 123),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_i32_to_column_values() {
        let value: i32 = 12345;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, 12345),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_i32_negative_to_column_values() {
        let value: i32 = -12345;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, -12345),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_generic_decoder_default() {
        let _decoder = GenericDecoder::default();
    }

    #[test]
    fn test_string_decoder_default() {
        let _decoder = StringDecoder;
    }

    #[test]
    fn test_decimal_parts_debug() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![123, 456],
        };
        let debug_str = format!("{parts:?}");
        // Just verify the debug trait works - don't assert on exact format
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn test_generic_decoder_constants() {
        assert_eq!(GenericDecoder::SHORTLEN_MAXVALUE, 65535);
        assert_eq!(GenericDecoder::SQL_PLP_NULL, 0xffffffffffffffff);
        assert_eq!(GenericDecoder::SQL_PLP_UNKNOWNLEN, 0xfffffffffffffffe);
    }

    #[test]
    fn test_decimal_parts_scale_precision() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 5,
            precision: 18,
            int_parts: vec![100000],
        };

        // Test that scale affects the decimal conversion
        let result = parts.to_f64();
        // With scale of 5, int value 100000 should become 1.00000
        assert!((result - 1.0).abs() < 0.00001);
    }

    #[test]
    fn test_decimal_parts_empty_int_parts() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![],
        };

        let result = parts.to_f64();
        assert_eq!(result, 0.0);
    }

    #[test]
    fn test_decimal_parts_single_int_part() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 5,
            int_parts: vec![12345],
        };

        let result = parts.to_f64();
        assert_eq!(result, 12345.0);
    }

    #[test]
    fn test_column_values_from_u8_zero() {
        let value: u8 = 0;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 0),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_column_values_from_u8_max() {
        let value: u8 = 255;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::TinyInt(v) => assert_eq!(v, 255),
            _ => panic!("Expected TinyInt variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_zero() {
        let value: i32 = 0;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, 0),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_max() {
        let value: i32 = i32::MAX;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, i32::MAX),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_column_values_from_i32_min() {
        let value: i32 = i32::MIN;
        let col_val: ColumnValues = value.into();
        match col_val {
            ColumnValues::Int(v) => assert_eq!(v, i32::MIN),
            _ => panic!("Expected Int variant"),
        }
    }

    #[test]
    fn test_validate_alloc_size_within_limit() {
        let result = validate_alloc_size(1024, "test_allocation");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_alloc_size_at_limit() {
        let result = validate_alloc_size(MAX_ALLOC_SIZE, "test_at_limit");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_alloc_size_exceeds_limit() {
        let result = validate_alloc_size(MAX_ALLOC_SIZE + 1, "test_exceeds");
        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = format!("{e:?}");
            assert!(error_msg.contains("exceeds maximum allowed"));
        }
    }

    #[test]
    fn test_validate_alloc_size_zero() {
        let result = validate_alloc_size(0, "test_zero");
        assert!(result.is_ok());
    }

    #[test]
    fn test_decimal_parts_equality_same() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100, 200],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100, 200],
        };
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_sign() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        let parts2 = DecimalParts {
            is_positive: false,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_scale() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 3,
            precision: 10,
            int_parts: vec![100],
        };
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_precision() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 12,
            int_parts: vec![100],
        };
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_length_with_zeros() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100, 0, 0],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_different_length_with_nonzeros() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100, 200],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_debug_format_positive() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![12345],
        };
        let debug_str = format!("{parts:?}");
        assert!(debug_str.contains("Decimal:"));
        assert!(debug_str.contains("12345"));
        assert!(debug_str.contains("Precision 10"));
        assert!(debug_str.contains("Scale 2"));
        assert!(!debug_str.starts_with("Decimal: -"));
    }

    #[test]
    fn test_decimal_parts_debug_format_negative() {
        let parts = DecimalParts {
            is_positive: false,
            scale: 3,
            precision: 15,
            int_parts: vec![54321],
        };
        let debug_str = format!("{parts:?}");
        assert!(debug_str.contains("Decimal: -"));
        assert!(debug_str.contains("54321"));
        assert!(debug_str.contains("Precision 15"));
        assert!(debug_str.contains("Scale 3"));
    }

    #[test]
    fn test_decimal_parts_debug_format_multiple_parts() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 20,
            int_parts: vec![100, 200, 300],
        };
        let debug_str = format!("{parts:?}");
        assert!(debug_str.contains("100"));
        assert!(debug_str.contains("200"));
        assert!(debug_str.contains("300"));
    }

    #[test]
    fn test_f64_conversion_high_scale() {
        let int_parts = vec![12345];
        let parts = DecimalParts {
            is_positive: true,
            scale: 10,
            precision: 15,
            int_parts,
        };
        let result = parts.to_f64();
        // With scale of 10, 12345 should become 0.0000012345
        assert!((result - 0.0000012345).abs() < 0.0000000001);
    }

    #[test]
    fn test_f64_conversion_single_zero() {
        let parts = DecimalParts {
            is_positive: true,
            scale: 5,
            precision: 10,
            int_parts: vec![0],
        };
        let result = parts.to_f64();
        assert_eq!(result, 0.0);
    }

    #[test]
    fn test_f64_conversion_negative_zero() {
        let parts = DecimalParts {
            is_positive: false,
            scale: 0,
            precision: 1,
            int_parts: vec![0],
        };
        let result = parts.to_f64();
        assert_eq!(result, -0.0);
    }

    #[test]
    fn test_string_decoder_is_long_len_type_ntext() {
        assert!(StringDecoder::is_long_len_type(TdsDataType::NText));
    }

    #[test]
    fn test_string_decoder_is_long_len_type_text() {
        assert!(StringDecoder::is_long_len_type(TdsDataType::Text));
    }

    #[test]
    fn test_string_decoder_is_long_len_type_not_long() {
        assert!(!StringDecoder::is_long_len_type(TdsDataType::NVarChar));
        assert!(!StringDecoder::is_long_len_type(TdsDataType::BigVarChar));
        assert!(!StringDecoder::is_long_len_type(TdsDataType::Int4));
    }

    #[test]
    fn test_decimal_parts_f64_conversion_with_many_int_parts() {
        // Test with 3 int parts
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 30,
            int_parts: vec![1, 2, 3],
        };
        let result = parts.to_f64();
        // Just verify it doesn't panic and produces a value
        assert!(result > 0.0);
    }

    #[test]
    fn test_decimal_parts_equality_reversed_order() {
        // Test that order matters for trailing zeros
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![0, 100],
        };
        // These should not be equal as trailing zeros are in different positions
        assert_ne!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_equality_both_empty() {
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![],
        };
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_validate_alloc_size_mid_range() {
        // Test allocation in the middle range
        let result = validate_alloc_size(MAX_ALLOC_SIZE / 2, "test_mid_range");
        assert!(result.is_ok());
    }

    #[test]
    fn test_decimal_parts_f64_negative_with_scale() {
        // Test negative number with scale
        let parts = DecimalParts {
            is_positive: false,
            scale: 3,
            precision: 10,
            int_parts: vec![123456],
        };
        let result = parts.to_f64();
        assert!((result + 123.456).abs() < 0.001);
    }

    #[test]
    fn test_decimal_parts_equality_one_empty_one_zero() {
        // Test equality between empty vec and vec with zero
        let parts1 = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![],
        };
        let parts2 = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![0],
        };
        // Empty should equal to [0]
        assert_eq!(parts1, parts2);
    }

    #[test]
    fn test_decimal_parts_debug_with_zero() {
        // Test Debug formatting with zero value
        let parts = DecimalParts {
            is_positive: true,
            scale: 0,
            precision: 1,
            int_parts: vec![0],
        };
        let debug_str = format!("{parts:?}");
        assert!(debug_str.contains("0"));
        assert!(debug_str.contains("F64 value: 0"));
    }

    // Tests for DecimalParts::from_string
    #[test]
    fn test_from_string_positive_decimal() {
        let result = DecimalParts::from_string("123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.scale, 2);
        assert_eq!(parts.precision, 10);
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_negative_decimal() {
        let result = DecimalParts::from_string("-123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(!parts.is_positive);
        assert_eq!(parts.scale, 2);
        assert_eq!(parts.precision, 10);
        assert_eq!(parts.to_decimal_string(), "-123.45");
    }

    #[test]
    fn test_from_string_integer_no_decimal_point() {
        let result = DecimalParts::from_string("12345", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.scale, 0);
        assert_eq!(parts.to_decimal_string(), "12345");
    }

    #[test]
    fn test_from_string_with_leading_zeros() {
        let result = DecimalParts::from_string("00123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_small_fractional_value() {
        let result = DecimalParts::from_string("0.01", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.01");
    }

    #[test]
    fn test_from_string_zero() {
        let result = DecimalParts::from_string("0", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0");
    }

    #[test]
    fn test_from_string_zero_with_scale() {
        let result = DecimalParts::from_string("0.00", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.00");
    }

    #[test]
    fn test_from_string_fractional_padding() {
        // "1.5" with scale 3 should be treated as "1.500"
        let result = DecimalParts::from_string("1.5", 10, 3);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "1.500");
    }

    #[test]
    fn test_from_string_max_precision_38_digits() {
        let value = "12345678901234567890123456789012345678";
        let result = DecimalParts::from_string(value, 38, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), value);
    }

    #[test]
    fn test_from_string_high_scale() {
        let result = DecimalParts::from_string("123.456789", 10, 6);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.456789");
    }

    #[test]
    fn test_from_string_leading_zeros_precision_check() {
        // "00001.00" should have precision of 3 (1 significant digit + 2 scale)
        let result = DecimalParts::from_string("00001.00", 5, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "1.00");
    }

    #[test]
    fn test_from_string_with_positive_sign() {
        let result = DecimalParts::from_string("+123.45", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(parts.is_positive);
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    // Error cases
    #[test]
    fn test_from_string_invalid_characters() {
        let result = DecimalParts::from_string("not_a_number", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_multiple_decimal_points() {
        let result = DecimalParts::from_string("123.45.67", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("Invalid decimal string"));
    }

    #[test]
    fn test_from_string_scale_exceeded() {
        // Trying to parse "123.456" with scale 2 should fail
        let result = DecimalParts::from_string("123.456", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("scale") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_precision_exceeded() {
        // "12345" has 5 significant digits, should fail with precision 4
        let result = DecimalParts::from_string("12345", 4, 0);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("precision") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_precision_exceeded_with_decimal() {
        // "123.45" has 5 significant digits total, should fail with precision 4
        let result = DecimalParts::from_string("123.45", 4, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("precision") && error_msg.contains("exceeds"));
    }

    #[test]
    fn test_from_string_invalid_digit_in_integer_part() {
        let result = DecimalParts::from_string("12a34", 10, 0);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_invalid_digit_in_fractional_part() {
        let result = DecimalParts::from_string("123.4x5", 10, 2);
        assert!(result.is_err());
        let error_msg = format!("{:?}", result.unwrap_err());
        assert!(error_msg.contains("invalid digit"));
    }

    #[test]
    fn test_from_string_leading_zeros_not_counted_in_precision() {
        // "0000123" should be treated as precision 3, not 7
        let result = DecimalParts::from_string("0000123", 3, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123");
    }

    #[test]
    fn test_from_string_whitespace_trimmed() {
        let result = DecimalParts::from_string("  123.45  ", 10, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    #[test]
    fn test_from_string_negative_zero() {
        let result = DecimalParts::from_string("-0", 10, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert!(!parts.is_positive);
        assert_eq!(parts.to_decimal_string(), "-0");
    }

    #[test]
    fn test_from_string_only_zeros_with_decimal() {
        let result = DecimalParts::from_string("0.0", 10, 1);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "0.0");
    }

    #[test]
    fn test_from_string_exact_precision_match() {
        // Test when actual precision exactly matches target precision
        let result = DecimalParts::from_string("12345", 5, 0);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "12345");
    }

    #[test]
    fn test_from_string_exact_scale_match() {
        // Test when fractional digits exactly match scale
        let result = DecimalParts::from_string("123.45", 5, 2);
        assert!(result.is_ok());
        let parts = result.unwrap();
        assert_eq!(parts.to_decimal_string(), "123.45");
    }

    // Vector deserialization tests
    mod vector_tests {
        use super::*;
        use crate::datatypes::{
            sql_vector::SqlVector,
            sqldatatypes::{
                VECTOR_MAX_DIMENSIONS, VectorBaseType, VectorLayoutFormat, VectorLayoutVersion,
            },
        };

        #[test]
        fn test_vector_creation_and_validation() {
            // Test that SqlVector::try_from_f32 creates valid vectors
            let dimensions = vec![1.0, 2.0, 3.0];
            let vector = SqlVector::try_from_f32(dimensions.clone()).unwrap();

            // Only check semantic data - TDS header fields are not stored
            assert_eq!(vector.as_f32(), Some(dimensions.as_slice()));
            assert_eq!(vector.dimension_count(), 3);
        }

        #[test]
        fn test_vector_single_dimension() {
            let vector = SqlVector::try_from_f32(vec![42.5]).unwrap();
            assert_eq!(vector.as_f32(), Some(&[42.5][..]));
            assert_eq!(vector.dimension_count(), 1);
        }

        #[test]
        fn test_vector_max_dimensions() {
            let dimensions: Vec<f32> = (0..VECTOR_MAX_DIMENSIONS).map(|i| i as f32).collect();
            let vector = SqlVector::try_from_f32(dimensions).unwrap();
            assert_eq!(vector.dimension_count(), VECTOR_MAX_DIMENSIONS);
        }

        #[test]
        fn test_vector_from_raw_valid() {
            let values = vec![1.0_f32, 2.0, 3.0];
            // Convert f32 values to raw bytes
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let vector = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                VectorLayoutVersion::V1 as u8,
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            // try_from_raw validates during construction
            assert!(vector.is_ok());
            let vector = vector.unwrap();
            assert_eq!(vector.as_f32(), Some(values.as_slice()));
        }

        #[test]
        fn test_vector_from_raw_invalid_layout_format() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                0x00, // Invalid format
                VectorLayoutVersion::V1 as u8,
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("layout format"));
        }

        #[test]
        fn test_vector_from_raw_invalid_layout_version() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                0x99, // Invalid version
                VectorBaseType::Float32 as u8,
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("layout version"));
        }

        #[test]
        fn test_vector_from_raw_invalid_base_type() {
            let values = vec![1.0_f32, 2.0];
            let mut raw_bytes = Vec::new();
            for val in &values {
                raw_bytes.extend_from_slice(&val.to_le_bytes());
            }

            let result = SqlVector::try_from_raw(
                VectorLayoutFormat::V1 as u8,
                VectorLayoutVersion::V1 as u8,
                0x99, // Invalid base type
                raw_bytes,
            );

            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("base type"));
        }

        #[test]
        fn test_vector_empty_dimensions() {
            let result = SqlVector::try_from_f32(vec![]);
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("at least one dimension")
            );
        }

        #[test]
        fn test_vector_too_many_dimensions() {
            let dimensions: Vec<f32> = (0..(VECTOR_MAX_DIMENSIONS + 1)).map(|i| i as f32).collect();
            let result = SqlVector::try_from_f32(dimensions);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
        }

        #[test]
        fn test_vector_total_size() {
            let vector = SqlVector::try_from_f32(vec![1.0, 2.0, 3.0]).unwrap();
            assert_eq!(vector.total_size(), 8 + 3 * 4); // 8 byte header + 3 floats * 4 bytes
        }

        #[test]
        fn test_column_values_vector_variant() {
            let vector = SqlVector::try_from_f32(vec![1.0, 2.0, 3.0]).unwrap();
            let col_val = ColumnValues::Vector(vector);

            match col_val {
                ColumnValues::Vector(v) => {
                    assert_eq!(v.dimension_count(), 3);
                    assert_eq!(v.as_f32(), Some(&[1.0, 2.0, 3.0][..]));
                }
                _ => panic!("Expected Vector variant"),
            }
        }
    }

    mod decode_into_tests {
        use async_trait::async_trait;
        use byteorder::{ByteOrder, LittleEndian};

        use crate::core::TdsResult;
        use crate::datatypes::column_values::{ColumnValues, SqlDateTime, SqlSmallDateTime};
        use crate::datatypes::decoder::{GenericDecoder, SqlTypeDecode};
        use crate::datatypes::row_writer::DefaultRowWriter;
        use crate::datatypes::sqldatatypes::VariableLengthTypes;
        use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo, TypeInfoVariant};
        use crate::io::packet_reader::TdsPacketReader;
        use crate::query::metadata::ColumnMetadata;

        /// Byte-buffer backed mock implementing every `TdsPacketReader` method
        /// used by the decoder.
        struct ByteReader {
            data: Vec<u8>,
            pos: usize,
        }

        impl ByteReader {
            fn new(data: Vec<u8>) -> Self {
                Self { data, pos: 0 }
            }

            fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
                if self.pos + n > self.data.len() {
                    return Err(crate::error::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "End of data",
                    )));
                }
                let slice = &self.data[self.pos..self.pos + n];
                self.pos += n;
                Ok(slice)
            }
        }

        #[async_trait]
        impl TdsPacketReader for ByteReader {
            async fn read_byte(&mut self) -> TdsResult<u8> {
                Ok(self.take(1)?[0])
            }
            async fn read_int16(&mut self) -> TdsResult<i16> {
                Ok(LittleEndian::read_i16(self.take(2)?))
            }
            async fn read_uint16(&mut self) -> TdsResult<u16> {
                Ok(LittleEndian::read_u16(self.take(2)?))
            }
            async fn read_int32(&mut self) -> TdsResult<i32> {
                Ok(LittleEndian::read_i32(self.take(4)?))
            }
            async fn read_uint32(&mut self) -> TdsResult<u32> {
                Ok(LittleEndian::read_u32(self.take(4)?))
            }
            async fn read_int64(&mut self) -> TdsResult<i64> {
                Ok(LittleEndian::read_i64(self.take(8)?))
            }
            async fn read_uint64(&mut self) -> TdsResult<u64> {
                Ok(LittleEndian::read_u64(self.take(8)?))
            }
            async fn read_float32(&mut self) -> TdsResult<f32> {
                Ok(LittleEndian::read_f32(self.take(4)?))
            }
            async fn read_float64(&mut self) -> TdsResult<f64> {
                Ok(LittleEndian::read_f64(self.take(8)?))
            }
            async fn read_uint24(&mut self) -> TdsResult<u32> {
                let b = self.take(3)?;
                Ok(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
            }
            async fn read_uint40(&mut self) -> TdsResult<u64> {
                let b = self.take(5)?;
                Ok(b[0] as u64
                    | (b[1] as u64) << 8
                    | (b[2] as u64) << 16
                    | (b[3] as u64) << 24
                    | (b[4] as u64) << 32)
            }
            async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
                let slice = self.take(buffer.len())?;
                buffer.copy_from_slice(slice);
                Ok(buffer.len())
            }
            async fn skip_bytes(&mut self, count: usize) -> TdsResult<()> {
                self.take(count)?;
                Ok(())
            }
            async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
                unimplemented!()
            }
            async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
                unimplemented!()
            }
            async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
                unimplemented!()
            }
            async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
                unimplemented!()
            }
            async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
                unimplemented!()
            }
            async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
                unimplemented!()
            }
            async fn read_unicode(&mut self, _len: usize) -> TdsResult<String> {
                unimplemented!()
            }
            async fn read_unicode_with_byte_length(&mut self, _len: usize) -> TdsResult<String> {
                unimplemented!()
            }
            async fn cancel_read_stream(&mut self) -> TdsResult<()> {
                unimplemented!()
            }
            fn reset_reader(&mut self) {
                self.pos = 0;
            }
        }

        fn fixed_metadata(data_type: TdsDataType, length: usize) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::FixedLen(
                        crate::datatypes::sqldatatypes::FixedLengthTypes::try_from(data_type)
                            .unwrap_or(crate::datatypes::sqldatatypes::FixedLengthTypes::Int4),
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            }
        }

        fn varlen_metadata(data_type: TdsDataType, length: usize) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::IntN),
                        length,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            }
        }

        /// Runs both decode() and decode_into() on the same bytes and asserts
        /// that decode_into via DefaultRowWriter produces the same ColumnValues
        /// as decode().
        async fn assert_decode_equivalence(
            bytes: Vec<u8>,
            metadata: &ColumnMetadata,
        ) -> ColumnValues {
            let decoder = GenericDecoder::default();

            // Run decode()
            let mut reader1 = ByteReader::new(bytes.clone());
            let expected = decoder.decode(&mut reader1, metadata).await.unwrap();

            // Run decode_into()
            let mut reader2 = ByteReader::new(bytes);
            let mut writer = DefaultRowWriter::new(1);
            decoder
                .decode_into(&mut reader2, metadata, 0, &mut writer)
                .await
                .unwrap();
            let row = writer.take_row();
            assert_eq!(row.len(), 1);
            assert_eq!(
                row[0], expected,
                "decode_into mismatch for {:?}",
                metadata.data_type
            );
            expected
        }

        #[tokio::test]
        async fn decode_into_int1() {
            let md = fixed_metadata(TdsDataType::Int1, 1);
            let val = assert_decode_equivalence(vec![42], &md).await;
            assert_eq!(val, ColumnValues::TinyInt(42));
        }

        #[tokio::test]
        async fn decode_into_int2() {
            let md = fixed_metadata(TdsDataType::Int2, 2);
            let mut buf = [0u8; 2];
            LittleEndian::write_i16(&mut buf, -1234);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::SmallInt(-1234));
        }

        #[tokio::test]
        async fn decode_into_int4() {
            let md = fixed_metadata(TdsDataType::Int4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_i32(&mut buf, 99999);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Int(99999));
        }

        #[tokio::test]
        async fn decode_into_int8() {
            let md = fixed_metadata(TdsDataType::Int8, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_i64(&mut buf, i64::MAX);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::BigInt(i64::MAX));
        }

        #[tokio::test]
        async fn decode_into_intn_null() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            // length byte = 0 → null
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_intn_i32() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            let mut buf = vec![4u8]; // length = 4
            let mut i32_buf = [0u8; 4];
            LittleEndian::write_i32(&mut i32_buf, 777);
            buf.extend_from_slice(&i32_buf);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Int(777));
        }

        #[tokio::test]
        async fn decode_into_flt4() {
            let md = fixed_metadata(TdsDataType::Flt4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_f32(&mut buf, 1.5);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Real(1.5));
        }

        #[tokio::test]
        async fn decode_into_flt8() {
            let md = fixed_metadata(TdsDataType::Flt8, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_f64(&mut buf, 99.25);
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert_eq!(val, ColumnValues::Float(99.25));
        }

        #[tokio::test]
        async fn decode_into_fltn_null() {
            let md = varlen_metadata(TdsDataType::FltN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_fltn_f32() {
            let md = varlen_metadata(TdsDataType::FltN, 4);
            let mut buf = vec![4u8];
            let mut f32_buf = [0u8; 4];
            LittleEndian::write_f32(&mut f32_buf, 2.5);
            buf.extend_from_slice(&f32_buf);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Real(2.5));
        }

        #[tokio::test]
        async fn decode_into_bit() {
            let md = fixed_metadata(TdsDataType::Bit, 1);
            let val = assert_decode_equivalence(vec![1], &md).await;
            assert_eq!(val, ColumnValues::Bit(true));
        }

        #[tokio::test]
        async fn decode_into_bitn_null() {
            let md = varlen_metadata(TdsDataType::BitN, 1);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_bitn_true() {
            let md = varlen_metadata(TdsDataType::BitN, 1);
            let val = assert_decode_equivalence(vec![1, 1], &md).await;
            assert_eq!(val, ColumnValues::Bit(true));
        }

        #[tokio::test]
        async fn decode_into_money4() {
            let md = fixed_metadata(TdsDataType::Money4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_i32(&mut buf, 10000); // $1.00
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(val, ColumnValues::SmallMoney(_)));
        }

        #[tokio::test]
        async fn decode_into_money8() {
            let md = fixed_metadata(TdsDataType::Money, 8);
            let mut buf = [0u8; 8];
            // money is stored as msb + lsb i32 pair
            LittleEndian::write_i32(&mut buf[0..4], 0); // msb
            LittleEndian::write_i32(&mut buf[4..8], 10000); // lsb
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(val, ColumnValues::Money(_)));
        }

        #[tokio::test]
        async fn decode_into_moneyn_null() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_datetime() {
            let md = fixed_metadata(TdsDataType::DateTime, 8);
            let mut buf = [0u8; 8];
            LittleEndian::write_i32(&mut buf[0..4], 43000); // days
            LittleEndian::write_u32(&mut buf[4..8], 100); // ticks
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(
                val,
                ColumnValues::DateTime(SqlDateTime {
                    days: 43000,
                    time: 100
                })
            ));
        }

        #[tokio::test]
        async fn decode_into_smalldatetime() {
            let md = fixed_metadata(TdsDataType::DateTim4, 4);
            let mut buf = [0u8; 4];
            LittleEndian::write_u16(&mut buf[0..2], 1000); // days
            LittleEndian::write_u16(&mut buf[2..4], 60); // minutes
            let val = assert_decode_equivalence(buf.to_vec(), &md).await;
            assert!(matches!(
                val,
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: 1000,
                    time: 60
                })
            ));
        }

        #[tokio::test]
        async fn decode_into_datetimen_null() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_daten() {
            let md = varlen_metadata(TdsDataType::DateN, 3);
            // length=3, then 3 bytes for date (uint24 days)
            let val = assert_decode_equivalence(vec![3, 0x01, 0x00, 0x00], &md).await;
            assert!(matches!(val, ColumnValues::Date(_)));
        }

        #[tokio::test]
        async fn decode_into_daten_null() {
            let md = varlen_metadata(TdsDataType::DateN, 3);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_guid() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            let mut buf = vec![16u8]; // length
            buf.extend_from_slice(&[1u8; 16]); // 16 bytes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Uuid(_)));
        }

        #[tokio::test]
        async fn decode_into_guid_null() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_bigbinary() {
            let md = varlen_metadata(TdsDataType::BigBinary, 4);
            let mut buf = Vec::new();
            // u16 length = 4
            buf.extend_from_slice(&[4, 0]);
            buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        }

        #[tokio::test]
        async fn decode_into_nvarchar() {
            // non-PLP, non-LOB: u16 length + bytes
            let md = varlen_metadata(TdsDataType::NVarChar, 100);
            let text_bytes = b"hi";
            let mut buf = Vec::new();
            // u16 length = 2
            buf.extend_from_slice(&[2, 0]);
            buf.extend_from_slice(text_bytes);
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::String(_)));
        }

        #[tokio::test]
        async fn decode_into_nvarchar_null() {
            let md = varlen_metadata(TdsDataType::NVarChar, 100);
            // 0xFFFF = NULL
            let val = assert_decode_equivalence(vec![0xFF, 0xFF], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_decimaln_null() {
            let md = ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::DecimalN,
                type_info: TypeInfo {
                    tds_type: TdsDataType::DecimalN,
                    length: 9,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::DecimalN,
                        9,
                        18,
                        5,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            };
            // length byte = 0 → NULL
            let val = assert_decode_equivalence(vec![0], &md).await;
            assert_eq!(val, ColumnValues::Null);
        }

        #[tokio::test]
        async fn decode_into_decimaln_value() {
            let md = ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::DecimalN,
                type_info: TypeInfo {
                    tds_type: TdsDataType::DecimalN,
                    length: 9,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::DecimalN,
                        9,
                        18,
                        2,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            };
            // length=5, sign=1 (positive), one i32 part = 12345
            let mut buf = vec![5u8, 1u8];
            let mut part = [0u8; 4];
            LittleEndian::write_i32(&mut part, 12345);
            buf.extend_from_slice(&part);
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Decimal(_)));
        }

        // ── helpers for new tests ──────────────────────────────────────

        fn precision_scale_metadata(
            data_type: TdsDataType,
            length: usize,
            precision: u8,
            scale: u8,
        ) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::DecimalN),
                        length,
                        precision,
                        scale,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            }
        }

        fn scale_metadata(data_type: TdsDataType, length: usize, scale: u8) -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type,
                type_info: TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(
                        VariableLengthTypes::try_from(data_type)
                            .unwrap_or(VariableLengthTypes::TimeN),
                        scale,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            }
        }

        fn ssvariant_metadata() -> ColumnMetadata {
            ColumnMetadata {
                user_type: 0,
                flags: 0,
                data_type: TdsDataType::SsVariant,
                type_info: TypeInfo {
                    tds_type: TdsDataType::SsVariant,
                    length: 8009,
                    type_info_variant: TypeInfoVariant::VarLen(
                        VariableLengthTypes::SsVariant,
                        8009,
                    ),
                },
                column_name: String::new(),
                multi_part_name: None,
            }
        }

        async fn assert_decode_err(bytes: Vec<u8>, metadata: &ColumnMetadata) {
            let decoder = GenericDecoder::default();
            let mut reader = ByteReader::new(bytes);
            assert!(decoder.decode(&mut reader, metadata).await.is_err());
        }

        // ── SQL_VARIANT tests ──────────────────────────────────────────

        #[tokio::test]
        async fn ssvariant_length_underflow() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // length=1, so checked_sub(2) on remaining after base_type+prop fails
            LittleEndian::write_u32(&mut [0u8; 4], 1);
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.push(TdsDataType::Int4 as u8); // base_type
            buf.push(0); // prop_bytes
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_unexpected_prop_bytes() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            buf.extend_from_slice(&10u32.to_le_bytes()); // length=10
            buf.push(TdsDataType::Int4 as u8); // base_type
            buf.push(5); // prop_bytes=5 (invalid)
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_zero_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // IntN is variable-length, not fixed-length, so FixedLengthTypes::try_from fails.
            // Then it falls through to the match on Guid/DateN, and IntN doesn't match → error.
            buf.extend_from_slice(&6u32.to_le_bytes()); // length=6 (2 header + 0 props + 4 data)
            buf.push(TdsDataType::IntN as u8); // base_type
            buf.push(0); // prop_bytes=0
            // provide 4 bytes of data that will never be read
            buf.extend_from_slice(&[0; 4]);
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_one_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            // Guid has 0 prop bytes, not 1 → error in decode_one_byte_variant
            buf.extend_from_slice(&20u32.to_le_bytes()); // length
            buf.push(TdsDataType::Guid as u8); // base_type
            buf.push(1); // prop_bytes=1
            buf.push(0); // scale byte (prop data)
            buf.extend_from_slice(&[0; 16]); // data
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn ssvariant_two_prop_unexpected_type() {
            let md = ssvariant_metadata();
            let mut buf = Vec::new();
            buf.extend_from_slice(&6u32.to_le_bytes()); // length
            buf.push(TdsDataType::IntN as u8); // not binary/decimal → error
            buf.push(2); // prop_bytes=2
            buf.extend_from_slice(&[0; 4]); // prop data + value data
            assert_decode_err(buf, &md).await;
        }

        // ── Decimal error paths ────────────────────────────────────────

        #[tokio::test]
        async fn decimal_wrong_type_info_variant() {
            // Use VarLen instead of VarLenPrecisionScale
            let md = varlen_metadata(TdsDataType::DecimalN, 9);
            // length byte = 5, sign = 1, one i32 part
            let mut buf = vec![5u8, 1u8];
            buf.extend_from_slice(&42i32.to_le_bytes());
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn decimal_large_valid() {
            let md = precision_scale_metadata(TdsDataType::DecimalN, 17, 38, 0);
            // length=17 → (17-1)>>2 = 4 int parts, which is well under the 64 limit
            let mut buf = vec![17u8, 1u8]; // length=17, sign=positive
            for _ in 0..4 {
                buf.extend_from_slice(&1i32.to_le_bytes());
            }
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Decimal(_)));
        }

        // ── Time scale branches ────────────────────────────────────────

        #[tokio::test]
        async fn time_scale_0() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 0);
            // length=3 → read_uint24 → value=1 (1 second in scale-0 units)
            // time_nanoseconds = 1 * 10_000_000
            let mut buf = vec![3u8]; // length byte
            buf.extend_from_slice(&[1, 0, 0]); // uint24 = 1
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 10_000_000);
                    assert_eq!(t.scale, 0);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_1() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 1);
            let mut buf = vec![3u8];
            buf.extend_from_slice(&[1, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 1_000_000);
                    assert_eq!(t.scale, 1);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_2() {
            let md = scale_metadata(TdsDataType::TimeN, 3, 2);
            let mut buf = vec![3u8];
            buf.extend_from_slice(&[1, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 100_000);
                    assert_eq!(t.scale, 2);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_4() {
            let md = scale_metadata(TdsDataType::TimeN, 4, 4);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&1u32.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 1_000);
                    assert_eq!(t.scale, 4);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_5() {
            let md = scale_metadata(TdsDataType::TimeN, 5, 5);
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[1, 0, 0, 0, 0]); // uint40 = 1
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 100);
                    assert_eq!(t.scale, 5);
                }
                _ => panic!("expected Time"),
            }
        }

        #[tokio::test]
        async fn time_scale_6() {
            let md = scale_metadata(TdsDataType::TimeN, 5, 6);
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[1, 0, 0, 0, 0]);
            let val = assert_decode_equivalence(buf, &md).await;
            match val {
                ColumnValues::Time(t) => {
                    assert_eq!(t.time_nanoseconds, 10);
                    assert_eq!(t.scale, 6);
                }
                _ => panic!("expected Time"),
            }
        }

        // ── DateTime2 / DateTimeOffset error paths ─────────────────────

        #[tokio::test]
        async fn datetime2_byte_len_underflow() {
            let md = scale_metadata(TdsDataType::DateTime2N, 2, 7);
            // length=2 → byte_len < 3 → checked_sub error
            let buf = vec![2u8, 0, 0];
            assert_decode_err(buf, &md).await;
        }

        #[tokio::test]
        async fn datetimeoffset_byte_len_underflow() {
            let md = scale_metadata(TdsDataType::DateTimeOffsetN, 1, 7);
            // length=1 → byte_len < 2 → checked_sub error in read_datetime_offset
            let buf = vec![1u8, 0];
            assert_decode_err(buf, &md).await;
        }

        // ── IntN unusual lengths ───────────────────────────────────────

        #[tokio::test]
        async fn intn_tinyint() {
            let md = varlen_metadata(TdsDataType::IntN, 1);
            let val = assert_decode_equivalence(vec![1u8, 42u8], &md).await;
            assert_eq!(val, ColumnValues::TinyInt(42));
        }

        #[tokio::test]
        async fn intn_smallint() {
            let md = varlen_metadata(TdsDataType::IntN, 2);
            let mut buf = vec![2u8];
            buf.extend_from_slice(&(-100i16).to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::SmallInt(-100));
        }

        #[tokio::test]
        async fn intn_invalid_length() {
            let md = varlen_metadata(TdsDataType::IntN, 4);
            let buf = vec![3u8, 0, 0, 0]; // byte_len=3 → invalid
            assert_decode_err(buf, &md).await;
        }

        // ── GUID error paths ──────────────────────────────────────────

        #[tokio::test]
        async fn guid_invalid_length() {
            let md = varlen_metadata(TdsDataType::Guid, 16);
            // length=5 (not 0 or 16) → error
            let mut buf = vec![5u8];
            buf.extend_from_slice(&[0; 5]);
            assert_decode_err(buf, &md).await;
        }

        // ── FltN paths ────────────────────────────────────────────────

        #[tokio::test]
        async fn fltn_f64() {
            let md = varlen_metadata(TdsDataType::FltN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&99.5f64.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert_eq!(val, ColumnValues::Float(99.5));
        }

        // ── MoneyN paths ──────────────────────────────────────────────

        #[tokio::test]
        async fn moneyn_smallmoney() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&10000i32.to_le_bytes());
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::SmallMoney(_)));
        }

        #[tokio::test]
        async fn moneyn_money() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&0i32.to_le_bytes()); // msb
            buf.extend_from_slice(&10000i32.to_le_bytes()); // lsb
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::Money(_)));
        }

        #[tokio::test]
        async fn moneyn_invalid_length() {
            let md = varlen_metadata(TdsDataType::MoneyN, 8);
            let buf = vec![3u8, 0, 0, 0];
            assert_decode_err(buf, &md).await;
        }

        // ── DateTimeN paths ───────────────────────────────────────────

        #[tokio::test]
        async fn datetimen_smalldatetime() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let mut buf = vec![4u8];
            buf.extend_from_slice(&1000u16.to_le_bytes()); // days
            buf.extend_from_slice(&60u16.to_le_bytes()); // minutes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(
                val,
                ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: 1000,
                    time: 60
                })
            ));
        }

        #[tokio::test]
        async fn datetimen_datetime() {
            let md = varlen_metadata(TdsDataType::DateTimeN, 8);
            let mut buf = vec![8u8];
            buf.extend_from_slice(&43000i32.to_le_bytes()); // days
            buf.extend_from_slice(&100u32.to_le_bytes()); // ticks
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(
                val,
                ColumnValues::DateTime(SqlDateTime {
                    days: 43000,
                    time: 100
                })
            ));
        }

        // ── BigBinary empty ────────────────────────────────────────────

        #[tokio::test]
        async fn bigbinary_empty() {
            let md = varlen_metadata(TdsDataType::BigBinary, 100);
            let val = assert_decode_equivalence(vec![0x00, 0x00], &md).await;
            assert_eq!(val, ColumnValues::Bytes(vec![]));
        }

        // ── DateTime2N valid decode ───────────────────────────────────

        #[tokio::test]
        async fn datetime2n_value() {
            let md = scale_metadata(TdsDataType::DateTime2N, 8, 7);
            // length=8 → time_byte_len=5, date=3 bytes
            let mut buf = vec![8u8];
            buf.extend_from_slice(&[0, 0, 0, 0, 0]); // time (uint40 = 0)
            buf.extend_from_slice(&[1, 0, 0]); // date (uint24 = 1)
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::DateTime2(_)));
        }

        // ── DateTimeOffsetN valid decode ──────────────────────────────

        #[tokio::test]
        async fn datetimeoffsetn_value() {
            let md = scale_metadata(TdsDataType::DateTimeOffsetN, 10, 7);
            // length=10 → datetime2_byte_len=8, offset=2 bytes
            let mut buf = vec![10u8];
            buf.extend_from_slice(&[0, 0, 0, 0, 0]); // time (uint40)
            buf.extend_from_slice(&[1, 0, 0]); // date (uint24)
            buf.extend_from_slice(&60i16.to_le_bytes()); // offset minutes
            let val = assert_decode_equivalence(buf, &md).await;
            assert!(matches!(val, ColumnValues::DateTimeOffset(_)));
        }
    }
}
