// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use uuid::Uuid;

use crate::datatypes::column_values::{
    ColumnValues, DEFAULT_VARTIME_SCALE, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset,
    SqlMoney, SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use crate::datatypes::sql_json::SqlJson;
use crate::datatypes::sql_vector::SqlVector;
use crate::datatypes::tds_value_serializer::{TdsTypeContext, TdsValueSerializer};
use crate::{
    core::TdsResult,
    datatypes::{
        decoder::DecimalParts,
        sql_string::SqlString,
        sqldatatypes::{
            FixedLengthTypes, TdsDataType, VECTOR_HEADER_SIZE, VECTOR_MAX_DIMENSIONS,
            VectorBaseType, VectorLayoutFormat, VectorLayoutVersion,
        },
    },
    error::Error,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
    token::tokens::SqlCollation,
};

/// Input parameter type for RPC calls.
///
/// Each variant wraps an `Option` to support SQL `NULL`. The inner types
/// carry the Rust-side value; serialization into TDS wire format is handled
/// by [`TdsValueSerializer`].
#[derive(Debug, PartialEq, Clone)]
pub enum SqlType {
    /// Boolean value (`bit`).
    Bit(Option<bool>),
    /// Unsigned 8-bit integer (`tinyint`).
    TinyInt(Option<u8>),
    /// Signed 16-bit integer (`smallint`).
    SmallInt(Option<i16>),
    /// Signed 32-bit integer (`int`).
    Int(Option<i32>),
    /// Signed 64-bit integer (`bigint`).
    BigInt(Option<i64>),
    /// 32-bit IEEE float (`real`).
    Real(Option<f32>),
    /// 64-bit IEEE float (`float`).
    Float(Option<f64>),
    /// Exact numeric with configurable precision/scale (`decimal`).
    Decimal(Option<DecimalParts>),
    /// Exact numeric with configurable precision/scale (`numeric`).
    Numeric(Option<DecimalParts>),
    /// 8-byte currency (`money`).
    Money(Option<SqlMoney>),
    /// 4-byte currency (`smallmoney`).
    SmallMoney(Option<SqlSmallMoney>),

    /// Time-of-day with configurable fractional-second precision.
    Time(Option<SqlTime>),
    /// Date and time with configurable fractional-second precision.
    DateTime2(Option<SqlDateTime2>),
    /// Date, time, and UTC offset.
    DateTimeOffset(Option<SqlDateTimeOffset>),
    /// Date-time with 1-minute accuracy.
    SmallDateTime(Option<SqlSmallDateTime>),
    /// Date-time with 1/300-second accuracy.
    DateTime(Option<SqlDateTime>),
    /// Calendar date only.
    Date(Option<SqlDate>),

    /// Represents a Varchar with a specifiied length.
    NVarchar(Option<SqlString>, u16),

    /// Represents a Varchar with MAX length.
    NVarcharMax(Option<SqlString>),

    /// Variable-length non-Unicode string with specified max length.
    Varchar(Option<SqlString>, u16),
    /// Variable-length non-Unicode string with MAX length.
    VarcharMax(Option<SqlString>),

    /// Variable-length binary with specified max length.
    VarBinary(Option<Vec<u8>>, u16),
    /// Variable-length binary with MAX length.
    VarBinaryMax(Option<Vec<u8>>),

    /// Fixed-length binary.
    Binary(Option<Vec<u8>>, u16),
    /// Fixed-length non-Unicode string.
    Char(Option<SqlString>, u16),
    /// Fixed-length Unicode string.
    NChar(Option<SqlString>, u16),

    /// Legacy variable-length non-Unicode string (`text`).
    Text(Option<SqlString>),
    /// Legacy variable-length Unicode string (`ntext`).
    NText(Option<SqlString>),

    /// JSON document.
    Json(Option<SqlJson>),

    /// XML document.
    Xml(Option<SqlXml>),
    /// 16-byte GUID (`uniqueidentifier`).
    Uuid(Option<Uuid>),

    /// Parameters: (data, dimensions, base_type)
    /// Although SqlVector has dimension & base type information, we also pass it separately so
    /// that we can serialize NULL vector parameters (where SqlVector=None) with correct metadata.
    Vector(Option<SqlVector>, u16, VectorBaseType),
    // To be added in future
    // Variant
    // TVP
}

type NullableTdsType = TdsDataType;

// The maximum length of a variable length type in TDS is 8000 bytes.
pub(crate) const VAR_TDS_MAX_LENGTH: u16 = 8000u16;

// The length of a NULL value in TDS is 65535 bytes for variable length types.
pub(crate) const MAX_U16_LENGTH: u16 = 65535u16;

// The fixed size for Decimal in TDS is 17 bytes.
pub(crate) const DECIMAL_FIXED_SIZE: u8 = 17;

pub(crate) const PLP_UNKNOWN_LENGTH: u64 = 0xFFFF_FFFF_FFFF_FFFE;

pub(crate) const PLP_NULL: u64 = 0xFFFF_FFFF_FFFF_FFFF;

pub(crate) const NO_XML_SCHEMA: u8 = 0x00;

impl SqlType {
    fn get_nullable_type(&self) -> NullableTdsType {
        match self {
            SqlType::Bit(_)
            | SqlType::TinyInt(_)
            | SqlType::SmallInt(_)
            | SqlType::Int(_)
            | SqlType::BigInt(_) => TdsDataType::IntN,
            SqlType::Real(_) | SqlType::Float(_) => TdsDataType::FltN,
            SqlType::Decimal(_) => TdsDataType::NumericN,
            SqlType::Numeric(_) => TdsDataType::NumericN,
            SqlType::NVarchar(_, _) => TdsDataType::NVarChar,
            SqlType::VarBinary(_items, _size) => TdsDataType::BigVarBinary,
            SqlType::Binary(_items, _) => TdsDataType::BigBinary,
            SqlType::Char(_, _) => TdsDataType::BigChar,
            SqlType::NChar(_, _) => TdsDataType::NChar,
            SqlType::Text(_) => TdsDataType::Text,
            SqlType::NText(_) => TdsDataType::NText,
            SqlType::Json(_) => TdsDataType::Json,

            SqlType::Time(_) => TdsDataType::TimeN,
            SqlType::DateTime2(_) => TdsDataType::DateTime2N,
            SqlType::DateTimeOffset(_) => TdsDataType::DateTimeOffsetN,
            SqlType::DateTime(_) => TdsDataType::DateTimeN,
            SqlType::Date(_) => TdsDataType::DateN,
            SqlType::SmallDateTime(_) => TdsDataType::DateTimeN,
            SqlType::NVarcharMax(_) => TdsDataType::NVarChar,
            SqlType::Varchar(_, _) => TdsDataType::BigVarChar,
            SqlType::VarcharMax(_) => TdsDataType::BigVarChar,
            SqlType::VarBinaryMax(_) => TdsDataType::BigVarBinary,
            SqlType::Xml(_) => TdsDataType::Xml,
            SqlType::Uuid(_) => TdsDataType::Guid,
            SqlType::Money(_) => TdsDataType::MoneyN,
            SqlType::SmallMoney(_) => TdsDataType::MoneyN,
            SqlType::Vector(_, _, _) => TdsDataType::Vector,
        }
    }

    fn get_fixed_length_size(&self) -> usize {
        let fixed_length_type = FixedLengthTypes::try_from(self);
        assert!(
            fixed_length_type.is_ok(),
            "SqlType is not a fixed length type."
        );
        fixed_length_type.unwrap().get_len()
    }

    /// Convert this SqlType to a ColumnValues (for value serialization) and a TdsTypeContext.
    /// Returns (ColumnValues, TdsTypeContext).
    fn to_column_value_and_context(
        &self,
        db_collation: &SqlCollation,
    ) -> (ColumnValues, TdsTypeContext) {
        let nullable_type = self.get_nullable_type();
        let tds_type = nullable_type as u8;

        // Common context for nullable RPC types: always nullable, never fixed-length
        // (RPC value data always has length prefixes)
        let base_ctx = TdsTypeContext {
            tds_type,
            max_size: 0,
            is_plp: false,
            is_fixed_length: false,
            precision: None,
            scale: None,
            collation: None,
            is_nullable: true,
        };

        match self {
            // Fixed-size integer/float types
            SqlType::Bit(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Bit(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 1,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::TinyInt(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::TinyInt(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 1,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::SmallInt(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::SmallInt(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 2,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::Int(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Int(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 4,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::BigInt(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::BigInt(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 8,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::Real(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Real(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 4,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::Float(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Float(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 8,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Decimal/Numeric - RPC always sends 17 bytes, so use precision >= 29
            SqlType::Decimal(opt) | SqlType::Numeric(opt) => {
                let cv = match opt {
                    Some(v) => {
                        if matches!(self, SqlType::Numeric(_)) {
                            ColumnValues::Numeric(v.clone())
                        } else {
                            ColumnValues::Decimal(v.clone())
                        }
                    }
                    None => ColumnValues::Null,
                };
                // Use precision 38 to ensure TdsValueSerializer writes 17 bytes (matching RPC behavior)
                let ctx = TdsTypeContext {
                    max_size: DECIMAL_FIXED_SIZE as usize,
                    precision: Some(38),
                    scale: Some(0),
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Money types
            SqlType::Money(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Money(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 8,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::SmallMoney(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::SmallMoney(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 4,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Date/Time types
            SqlType::Date(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Date(v.clone()),
                    None => ColumnValues::Null,
                };
                (cv, base_ctx)
            }
            SqlType::Time(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Time(v.clone()),
                    None => ColumnValues::Null,
                };
                (cv, base_ctx)
            }
            SqlType::DateTime(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::DateTime(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 8,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::SmallDateTime(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::SmallDateTime(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 4,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::DateTime2(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::DateTime2(v.clone()),
                    None => ColumnValues::Null,
                };
                (cv, base_ctx)
            }
            SqlType::DateTimeOffset(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::DateTimeOffset(v.clone()),
                    None => ColumnValues::Null,
                };
                (cv, base_ctx)
            }

            // UUID
            SqlType::Uuid(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Uuid(*v),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: 16,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // String types - NVarchar
            SqlType::NVarchar(opt, param_len) => {
                let max_size = 4000u16;
                let param_len = if *param_len > max_size {
                    MAX_U16_LENGTH
                } else {
                    *param_len * 2
                };
                let is_plp = param_len == MAX_U16_LENGTH;
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::NVarChar as u8,
                    max_size: if is_plp {
                        usize::MAX
                    } else {
                        (param_len / 2) as usize
                    },
                    is_plp,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::NVarcharMax(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::NVarChar as u8,
                    max_size: usize::MAX,
                    is_plp: true,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }

            // String types - Varchar
            SqlType::Varchar(opt, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    MAX_U16_LENGTH
                } else {
                    *param_len
                };
                let is_plp = param_len == MAX_U16_LENGTH;
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::BigVarChar as u8,
                    max_size: if is_plp {
                        usize::MAX
                    } else {
                        param_len as usize
                    },
                    is_plp,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::VarcharMax(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::BigVarChar as u8,
                    max_size: usize::MAX,
                    is_plp: true,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Char/NChar
            SqlType::Char(opt, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    MAX_U16_LENGTH
                } else {
                    *param_len
                };
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::BigChar as u8,
                    max_size: param_len as usize,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::NChar(opt, param_len) => {
                let param_len = if *param_len > 4000 {
                    MAX_U16_LENGTH
                } else {
                    *param_len * 2
                };
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::NChar as u8,
                    max_size: (param_len / 2) as usize,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Text/NText (legacy LOB types)
            SqlType::Text(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::Text as u8,
                    max_size: usize::MAX,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::NText(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::String(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    tds_type: TdsDataType::NText as u8,
                    max_size: usize::MAX,
                    collation: Some(*db_collation),
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Binary types
            SqlType::Binary(opt, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    u16::MAX
                } else {
                    *param_len
                };
                let cv = match opt {
                    Some(v) => ColumnValues::Bytes(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: param_len as usize,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::VarBinary(opt, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    u16::MAX
                } else {
                    *param_len
                };
                let is_plp = param_len == u16::MAX;
                let cv = match opt {
                    Some(v) => ColumnValues::Bytes(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: param_len as usize,
                    is_plp,
                    ..base_ctx
                };
                (cv, ctx)
            }
            SqlType::VarBinaryMax(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Bytes(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: usize::MAX,
                    is_plp: true,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // XML
            SqlType::Xml(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Xml(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: usize::MAX,
                    is_plp: true,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // JSON
            SqlType::Json(opt) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Json(v.clone()),
                    None => ColumnValues::Null,
                };
                let ctx = TdsTypeContext {
                    max_size: usize::MAX,
                    is_plp: true,
                    ..base_ctx
                };
                (cv, ctx)
            }

            // Vector
            SqlType::Vector(opt, dimensions, base_type) => {
                let cv = match opt {
                    Some(v) => ColumnValues::Vector(v.clone()),
                    None => ColumnValues::Null,
                };
                let element_size = base_type.element_size_bytes() as u16;
                let exact_size = (VECTOR_HEADER_SIZE as u16) + (*dimensions * element_size);
                let ctx = TdsTypeContext {
                    max_size: exact_size as usize,
                    ..base_ctx
                };
                (cv, ctx)
            }
        }
    }

    /// Write RPC type metadata preamble, then delegate value serialization to TdsValueSerializer.
    pub(crate) async fn serialize(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        db_collation: &SqlCollation,
    ) -> TdsResult<()> {
        // JSON needs special handling: TdsValueSerializer converts to UTF-16LE for bulk copy,
        // but RPC sends raw UTF-8 bytes with TDS type 0xF4.
        if let SqlType::Json(json) = self {
            return self.serialize_json(packet_writer, json).await;
        }

        // Step 1: Write the RPC type metadata preamble
        self.write_rpc_type_metadata(packet_writer, db_collation)
            .await?;

        // Step 2: Convert to ColumnValues + TdsTypeContext and serialize value
        let (column_value, ctx) = self.to_column_value_and_context(db_collation);
        TdsValueSerializer::serialize_value(packet_writer, &column_value, &ctx).await?;

        Ok(())
    }

    /// Write the RPC type metadata preamble (type byte, max_size, precision/scale/collation).
    /// This is RPC-specific and must stay exactly as-is for protocol compatibility.
    async fn write_rpc_type_metadata(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        db_collation: &SqlCollation,
    ) -> TdsResult<()> {
        let nullable_type = self.get_nullable_type();

        match self {
            // Fixed-size integer types: type byte + max_size byte
            SqlType::Bit(_)
            | SqlType::TinyInt(_)
            | SqlType::SmallInt(_)
            | SqlType::Int(_)
            | SqlType::BigInt(_)
            | SqlType::Real(_)
            | SqlType::Float(_) => {
                let type_size = self.get_fixed_length_size();
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(type_size as u8).await?;
            }

            // Decimal/Numeric: type byte + 17 + precision + scale
            SqlType::Decimal(opt) | SqlType::Numeric(opt) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(DECIMAL_FIXED_SIZE).await?;
                match opt {
                    Some(v) => {
                        packet_writer.write_byte_async(v.precision).await?;
                        packet_writer.write_byte_async(v.scale).await?;
                    }
                    None => {
                        packet_writer.write_byte_async(1).await?;
                        packet_writer.write_byte_async(0).await?;
                    }
                }
            }

            // Money types: type byte + size byte
            SqlType::Money(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(8u8).await?;
            }
            SqlType::SmallMoney(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(4u8).await?;
            }

            // UUID: type byte + 16
            SqlType::Uuid(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(16u8).await?;
            }

            // DateTime: type byte + 8
            SqlType::DateTime(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(8u8).await?;
            }

            // SmallDateTime: type byte + 4
            SqlType::SmallDateTime(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(4u8).await?;
            }

            // Date: type byte only (no size byte)
            SqlType::Date(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
            }

            // Time: type byte + scale
            SqlType::Time(opt) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                let scale = match opt {
                    Some(t) => t.get_scale(),
                    None => DEFAULT_VARTIME_SCALE,
                };
                packet_writer.write_byte_async(scale).await?;
            }

            // DateTime2: type byte + scale
            SqlType::DateTime2(opt) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                let scale = match opt {
                    Some(dt2) => dt2.time.get_scale(),
                    None => DEFAULT_VARTIME_SCALE,
                };
                packet_writer.write_byte_async(scale).await?;
            }

            // DateTimeOffset: type byte + scale
            SqlType::DateTimeOffset(opt) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                let scale = match opt {
                    Some(dto) => dto.datetime2.time.get_scale(),
                    None => DEFAULT_VARTIME_SCALE,
                };
                packet_writer.write_byte_async(scale).await?;
            }

            // NVarchar: type byte + param_len(u16) + collation(5 bytes)
            SqlType::NVarchar(_, param_len) => {
                let max_size = 4000u16;
                let param_len = if *param_len > max_size {
                    MAX_U16_LENGTH
                } else {
                    *param_len * 2
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }
            SqlType::NVarcharMax(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(MAX_U16_LENGTH).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }

            // Varchar: type byte + param_len(u16) + collation(5 bytes)
            SqlType::Varchar(_, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    MAX_U16_LENGTH
                } else {
                    *param_len
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }
            SqlType::VarcharMax(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(MAX_U16_LENGTH).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }

            // Char: type byte + param_len(u16) + collation(5 bytes)
            SqlType::Char(_, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    MAX_U16_LENGTH
                } else {
                    *param_len
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }
            SqlType::NChar(_, param_len) => {
                let param_len = if *param_len > 4000 {
                    MAX_U16_LENGTH
                } else {
                    *param_len * 2
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
            }

            // Text/NText: type byte + u32 max_size + collation(5 bytes) + table name parts
            SqlType::Text(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u32_async(0x7FFFFFFF).await?; // max size
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
                // No table name parts for RPC parameters
                packet_writer.write_byte_async(0).await?; // num parts = 0
            }
            SqlType::NText(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u32_async(0x7FFFFFFF).await?; // max size
                packet_writer.write_u32_async(db_collation.info).await?;
                packet_writer.write_byte_async(db_collation.sort_id).await?;
                // No table name parts for RPC parameters
                packet_writer.write_byte_async(0).await?; // num parts = 0
            }

            // Binary types: type byte + param_len(u16)
            SqlType::Binary(_, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    u16::MAX
                } else {
                    *param_len
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
            }
            SqlType::VarBinary(_, param_len) => {
                let param_len = if *param_len > VAR_TDS_MAX_LENGTH {
                    u16::MAX
                } else {
                    *param_len
                };
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(param_len).await?;
            }
            SqlType::VarBinaryMax(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_u16_async(u16::MAX).await?;
            }

            // XML: type byte + no_schema byte
            SqlType::Xml(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
                packet_writer.write_byte_async(NO_XML_SCHEMA).await?;
            }

            // JSON: type byte only
            SqlType::Json(_) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;
            }

            // Vector: type byte + exact_size(u16) + base_type byte
            SqlType::Vector(sql_vector, dimensions, base_type) => {
                packet_writer.write_byte_async(nullable_type as u8).await?;

                if *dimensions > VECTOR_MAX_DIMENSIONS {
                    return Err(Error::UsageError(format!(
                        "Vector dimensions {} exceeds maximum supported dimensions {}",
                        dimensions, VECTOR_MAX_DIMENSIONS
                    )));
                }

                if let Some(vector) = sql_vector {
                    let actual_base_type = vector.base_type();
                    if actual_base_type != *base_type {
                        return Err(Error::TypeConversionError(format!(
                            "Vector base type mismatch: declared {:?}, but vector has {:?}",
                            base_type, actual_base_type
                        )));
                    }
                    let actual_dimensions = vector.dimension_count();
                    if actual_dimensions != *dimensions {
                        return Err(Error::TypeConversionError(format!(
                            "Vector dimension mismatch: declared {}, but vector has {}",
                            dimensions, actual_dimensions
                        )));
                    }
                }

                let element_size = base_type.element_size_bytes() as u16;
                let exact_size = (VECTOR_HEADER_SIZE as u16) + (*dimensions * element_size);
                packet_writer.write_u16_async(exact_size).await?;
                packet_writer.write_byte_async(*base_type as u8).await?;
            }
        }

        Ok(())
    }

    async fn serialize_json(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        json: &Option<SqlJson>,
    ) -> TdsResult<()> {
        let nullable_type: NullableTdsType = self.get_nullable_type();
        packet_writer.write_byte_async(nullable_type as u8).await?;
        let optional_sqljson = match json {
            Some(binary) => Some(binary),
            None => None,
        };

        match optional_sqljson {
            Some(sqljson) => {
                let data = &sqljson.bytes;

                // Write unknown length for PLP.
                packet_writer.write_u64_async(PLP_UNKNOWN_LENGTH).await?;

                let data_len = data.len();

                // Write the data chunk length, which is the same as PLP length.
                packet_writer.write_u32_async(data_len as u32).await?;

                packet_writer.write_async(data).await?;

                // Write a zero-length PLP chunk terminator to signal the end of the PLP stream.
                packet_writer.write_u32_async(0).await?;
            }
            None => {
                packet_writer.write_u64_async(PLP_NULL).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn encode_vector_header(
        packet_writer: &mut PacketWriter<'_>,
        dimension_count: u16,
        base_type: VectorBaseType,
    ) -> TdsResult<()> {
        packet_writer
            .write_byte_async(VectorLayoutFormat::V1 as u8)
            .await?;
        packet_writer
            .write_byte_async(VectorLayoutVersion::V1 as u8)
            .await?;
        packet_writer.write_u16_async(dimension_count).await?;
        packet_writer.write_byte_async(base_type as u8).await?;
        packet_writer.write_byte_async(0x00).await?; // reserved
        packet_writer.write_byte_async(0x00).await?; // reserved
        packet_writer.write_byte_async(0x00).await?; // reserved
        Ok(())
    }
}

/// Calculate the byte length for time-based types based on scale value.
///
/// This mapping is defined in the TDS protocol documentation:
/// - Scale 0-2: 3 bytes
/// - Scale 3-4: 4 bytes  
/// - Scale 5-7: 5 bytes
pub(crate) fn get_time_length_from_scale(scale: u8) -> TdsResult<u8> {
    match scale {
        0..=2 => Ok(0x03),
        3 | 4 => Ok(0x04),
        5..=7 => Ok(0x05),
        _ => Err(Error::UsageError(format!(
            "Invalid scale for Time type: {scale}"
        ))),
    }
}

impl TryFrom<&SqlType> for FixedLengthTypes {
    type Error = Error;

    fn try_from(value: &SqlType) -> TdsResult<FixedLengthTypes> {
        match value {
            SqlType::Bit(_) => Ok(FixedLengthTypes::Int1),
            SqlType::TinyInt(_) => Ok(FixedLengthTypes::Int1),
            SqlType::SmallInt(_) => Ok(FixedLengthTypes::Int2),
            SqlType::Int(_) => Ok(FixedLengthTypes::Int4),
            SqlType::BigInt(_) => Ok(FixedLengthTypes::Int8),
            SqlType::Real(_) => Ok(FixedLengthTypes::Flt4),
            SqlType::Float(_) => Ok(FixedLengthTypes::Flt8),
            _ => Err(Error::UsageError(
                "SqlType is not a fixed length type.".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod json_tests {
    use std::io::Cursor;

    use bytes::Buf;

    use crate::{
        datatypes::{
            sql_json::SqlJson,
            sqldatatypes::TdsDataType,
            sqltypes::{PLP_NULL, PLP_UNKNOWN_LENGTH, SqlType},
        },
        io::{
            packet_reader::tests::MockNetworkReaderWriter,
            packet_writer::{PacketWriter, TdsPacketWriter},
        },
        message::messages::PacketType,
    };

    #[tokio::test]
    async fn test_write_json() {
        let json_str = "[\"abc\",\"ghi\",\"def\"]";
        let sqljson: SqlJson = json_str.to_string().into();

        let mut copied_bytes = Vec::new();

        copied_bytes.extend_from_slice(sqljson.bytes.as_slice());

        let byte_len = sqljson.bytes.len();

        let val = Some(sqljson);
        let sqltypejson = SqlType::Json(val.clone());

        let mut mock_reader_writer = MockNetworkReaderWriter::default();

        let mut packet_writer = PacketWriter::new(
            PacketType::TabularResult,
            &mut mock_reader_writer,
            None,
            None,
        );

        sqltypejson
            .serialize_json(&mut packet_writer, &val)
            .await
            .unwrap();
        packet_writer.finalize().await.unwrap();

        let payload = mock_reader_writer.get_written_data();
        let mut test_cursor = Cursor::new(payload);
        test_cursor.set_position(PacketWriter::PACKET_HEADER_SIZE as u64);
        assert_eq!(test_cursor.get_u8(), TdsDataType::Json as u8); // Valdate tds type
        assert_eq!(test_cursor.get_u64_le(), PLP_UNKNOWN_LENGTH);
        assert_eq!(test_cursor.get_u32_le(), byte_len as u32); // Chunk len
        let mut written_bytes = vec![0u8; byte_len];
        test_cursor.copy_to_slice(&mut written_bytes);
        assert_eq!(written_bytes, copied_bytes);
    }

    #[tokio::test]
    async fn test_write_null_json() {
        let sqltypejson = SqlType::Json(None);

        let mut mock_reader_writer = MockNetworkReaderWriter::default();

        let mut packet_writer = PacketWriter::new(
            PacketType::TabularResult,
            &mut mock_reader_writer,
            None,
            None,
        );

        sqltypejson
            .serialize_json(&mut packet_writer, &None)
            .await
            .unwrap();
        packet_writer.finalize().await.unwrap();

        let payload = mock_reader_writer.get_written_data();
        let mut test_cursor = Cursor::new(payload);
        test_cursor.set_position(PacketWriter::PACKET_HEADER_SIZE as u64);
        assert_eq!(test_cursor.get_u8(), TdsDataType::Json as u8); // Valdate tds type
        assert_eq!(test_cursor.get_u64_le(), PLP_NULL);
    }
}

#[cfg(test)]
mod coverage_tests {
    use crate::datatypes::{
        column_values::{
            ColumnValues, SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime,
            SqlSmallMoney, SqlTime,
        },
        decoder::DecimalParts,
        sqldatatypes::{FixedLengthTypes, TdsDataType},
        sqltypes::{SqlType, get_time_length_from_scale},
    };
    use crate::token::tokens::SqlCollation;

    fn default_collation() -> SqlCollation {
        SqlCollation {
            info: 0,
            lcid_language_id: 0,
            col_flags: 0,
            sort_id: 0,
        }
    }

    // -- get_time_length_from_scale --

    #[test]
    fn time_length_scale_0_to_2() {
        for s in 0..=2 {
            assert_eq!(get_time_length_from_scale(s).unwrap(), 3);
        }
    }

    #[test]
    fn time_length_scale_3_4() {
        assert_eq!(get_time_length_from_scale(3).unwrap(), 4);
        assert_eq!(get_time_length_from_scale(4).unwrap(), 4);
    }

    #[test]
    fn time_length_scale_5_to_7() {
        for s in 5..=7 {
            assert_eq!(get_time_length_from_scale(s).unwrap(), 5);
        }
    }

    #[test]
    fn time_length_invalid_scale() {
        assert!(get_time_length_from_scale(8).is_err());
        assert!(get_time_length_from_scale(255).is_err());
    }

    // -- TryFrom<&SqlType> for FixedLengthTypes --

    #[test]
    fn fixed_length_valid_conversions() {
        let cases: Vec<(SqlType, FixedLengthTypes)> = vec![
            (SqlType::Bit(None), FixedLengthTypes::Int1),
            (SqlType::TinyInt(None), FixedLengthTypes::Int1),
            (SqlType::SmallInt(None), FixedLengthTypes::Int2),
            (SqlType::Int(None), FixedLengthTypes::Int4),
            (SqlType::BigInt(None), FixedLengthTypes::Int8),
            (SqlType::Real(None), FixedLengthTypes::Flt4),
            (SqlType::Float(None), FixedLengthTypes::Flt8),
        ];
        for (sql_type, expected) in cases {
            assert_eq!(FixedLengthTypes::try_from(&sql_type).unwrap(), expected);
        }
    }

    #[test]
    fn fixed_length_invalid_conversion() {
        let non_fixed = SqlType::NVarchar(None, 100);
        assert!(FixedLengthTypes::try_from(&non_fixed).is_err());
    }

    // -- get_nullable_type --

    #[test]
    fn nullable_type_numeric() {
        let dp = DecimalParts {
            is_positive: true,
            scale: 2,
            precision: 10,
            int_parts: vec![100],
        };
        assert_eq!(
            SqlType::Numeric(Some(dp)).get_nullable_type(),
            TdsDataType::NumericN
        );
    }

    #[test]
    fn nullable_type_char_nchar() {
        assert_eq!(
            SqlType::Char(None, 10).get_nullable_type(),
            TdsDataType::BigChar
        );
        assert_eq!(
            SqlType::NChar(None, 10).get_nullable_type(),
            TdsDataType::NChar
        );
    }

    #[test]
    fn nullable_type_text_ntext() {
        assert_eq!(SqlType::Text(None).get_nullable_type(), TdsDataType::Text);
        assert_eq!(SqlType::NText(None).get_nullable_type(), TdsDataType::NText);
    }

    #[test]
    fn nullable_type_time_datetime2_smalldatetime() {
        assert_eq!(SqlType::Time(None).get_nullable_type(), TdsDataType::TimeN);
        assert_eq!(
            SqlType::DateTime2(None).get_nullable_type(),
            TdsDataType::DateTime2N
        );
        assert_eq!(
            SqlType::SmallDateTime(None).get_nullable_type(),
            TdsDataType::DateTimeN
        );
    }

    // -- get_fixed_length_size --

    #[test]
    fn fixed_length_sizes() {
        assert_eq!(SqlType::Bit(None).get_fixed_length_size(), 1);
        assert_eq!(SqlType::TinyInt(None).get_fixed_length_size(), 1);
        assert_eq!(SqlType::SmallInt(None).get_fixed_length_size(), 2);
        assert_eq!(SqlType::Int(None).get_fixed_length_size(), 4);
        assert_eq!(SqlType::BigInt(None).get_fixed_length_size(), 8);
        assert_eq!(SqlType::Real(None).get_fixed_length_size(), 4);
        assert_eq!(SqlType::Float(None).get_fixed_length_size(), 8);
    }

    #[test]
    #[should_panic(expected = "SqlType is not a fixed length type")]
    fn fixed_length_size_non_fixed_panics() {
        SqlType::NVarchar(None, 50).get_fixed_length_size();
    }

    // -- to_column_value_and_context: null branches --

    #[test]
    fn to_cv_smallint_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::SmallInt(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 2);
        assert!(ctx.is_nullable);
    }

    #[test]
    fn to_cv_numeric_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Numeric(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 17);
        assert_eq!(ctx.precision, Some(38));
        assert_eq!(ctx.scale, Some(0));
    }

    #[test]
    fn to_cv_numeric_some() {
        let col = &default_collation();
        let dp = DecimalParts {
            is_positive: true,
            scale: 4,
            precision: 18,
            int_parts: vec![42],
        };
        let (cv, _) = SqlType::Numeric(Some(dp.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Numeric(dp));
    }

    #[test]
    fn to_cv_date_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Date(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 0);
    }

    #[test]
    fn to_cv_time_null() {
        let col = &default_collation();
        let (cv, _) = SqlType::Time(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
    }

    #[test]
    fn to_cv_smalldatetime_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::SmallDateTime(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 4);
    }

    #[test]
    fn to_cv_datetime2_null() {
        let col = &default_collation();
        let (cv, _) = SqlType::DateTime2(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
    }

    #[test]
    fn to_cv_datetimeoffset_null() {
        let col = &default_collation();
        let (cv, _) = SqlType::DateTimeOffset(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
    }

    #[test]
    fn to_cv_money_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Money(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 8);
    }

    #[test]
    fn to_cv_smallmoney_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::SmallMoney(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 4);
    }

    #[test]
    fn to_cv_char_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Char(None, 50).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 50);
        assert_eq!(ctx.tds_type, TdsDataType::BigChar as u8);
        assert!(ctx.collation.is_some());
    }

    #[test]
    fn to_cv_char_exceeds_max() {
        let col = &default_collation();
        let (_, ctx) = SqlType::Char(None, 9000).to_column_value_and_context(col);
        assert_eq!(ctx.max_size, 65535);
    }

    #[test]
    fn to_cv_nchar_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::NChar(None, 100).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.tds_type, TdsDataType::NChar as u8);
        assert_eq!(ctx.max_size, 100);
    }

    #[test]
    fn to_cv_nchar_exceeds_max() {
        let col = &default_collation();
        let (_, ctx) = SqlType::NChar(None, 5000).to_column_value_and_context(col);
        assert_eq!(ctx.max_size, (65535 / 2) as usize);
    }

    #[test]
    fn to_cv_text_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Text(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, usize::MAX);
        assert_eq!(ctx.tds_type, TdsDataType::Text as u8);
    }

    #[test]
    fn to_cv_ntext_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::NText(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, usize::MAX);
        assert_eq!(ctx.tds_type, TdsDataType::NText as u8);
    }

    #[test]
    fn to_cv_date_some() {
        let col = &default_collation();
        let d = SqlDate::create(100).unwrap();
        let (cv, _) = SqlType::Date(Some(d.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Date(d));
    }

    #[test]
    fn to_cv_time_some() {
        let col = &default_collation();
        let t = SqlTime {
            time_nanoseconds: 1_000_000,
            scale: 3,
        };
        let (cv, _) = SqlType::Time(Some(t.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Time(t));
    }

    #[test]
    fn to_cv_smalldatetime_some() {
        let col = &default_collation();
        let sdt = SqlSmallDateTime {
            days: 100,
            time: 60,
        };
        let (cv, ctx) = SqlType::SmallDateTime(Some(sdt.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::SmallDateTime(sdt));
        assert_eq!(ctx.max_size, 4);
    }

    #[test]
    fn to_cv_datetime2_some() {
        let col = &default_collation();
        let dt2 = SqlDateTime2 {
            days: 100,
            time: SqlTime {
                time_nanoseconds: 0,
                scale: 7,
            },
        };
        let (cv, _) = SqlType::DateTime2(Some(dt2.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::DateTime2(dt2));
    }

    #[test]
    fn to_cv_datetimeoffset_some() {
        let col = &default_collation();
        let dto = SqlDateTimeOffset {
            datetime2: SqlDateTime2 {
                days: 100,
                time: SqlTime {
                    time_nanoseconds: 0,
                    scale: 7,
                },
            },
            offset: -300,
        };
        let (cv, _) = SqlType::DateTimeOffset(Some(dto.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::DateTimeOffset(dto));
    }

    #[test]
    fn to_cv_money_some() {
        let col = &default_collation();
        let m = SqlMoney {
            lsb_part: 100,
            msb_part: 0,
        };
        let (cv, _) = SqlType::Money(Some(m.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Money(m));
    }

    #[test]
    fn to_cv_smallmoney_some() {
        let col = &default_collation();
        let sm = SqlSmallMoney { int_val: 50 };
        let (cv, _) = SqlType::SmallMoney(Some(sm.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::SmallMoney(sm));
    }

    #[test]
    fn to_cv_nvarchar_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::NVarchar(None, 100).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.tds_type, TdsDataType::NVarChar as u8);
        assert!(!ctx.is_plp);
        assert!(ctx.collation.is_some());
    }

    #[test]
    fn to_cv_nvarchar_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("hello".to_string());
        let (cv, ctx) = SqlType::NVarchar(Some(val.clone()), 100).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
        assert_eq!(ctx.max_size, 100);
    }

    #[test]
    fn to_cv_nvarchar_exceeds_max_becomes_plp() {
        let col = &default_collation();
        let (_, ctx) = SqlType::NVarchar(None, 5000).to_column_value_and_context(col);
        assert!(ctx.is_plp);
        assert_eq!(ctx.max_size, usize::MAX);
    }

    #[test]
    fn to_cv_nvarcharmax_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::NVarcharMax(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert!(ctx.is_plp);
        assert_eq!(ctx.max_size, usize::MAX);
    }

    #[test]
    fn to_cv_nvarcharmax_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("test".to_string());
        let (cv, _) = SqlType::NVarcharMax(Some(val.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }

    #[test]
    fn to_cv_varchar_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Varchar(None, 200).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.tds_type, TdsDataType::BigVarChar as u8);
        assert!(!ctx.is_plp);
    }

    #[test]
    fn to_cv_varchar_exceeds_max_becomes_plp() {
        let col = &default_collation();
        let (_, ctx) = SqlType::Varchar(None, 9000).to_column_value_and_context(col);
        assert!(ctx.is_plp);
    }

    #[test]
    fn to_cv_varcharmax_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::VarcharMax(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert!(ctx.is_plp);
    }

    #[test]
    fn to_cv_varcharmax_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("hello".to_string());
        let (cv, _) = SqlType::VarcharMax(Some(val.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }

    #[test]
    fn to_cv_text_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("text data".to_string());
        let (cv, _) = SqlType::Text(Some(val.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }

    #[test]
    fn to_cv_ntext_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("ntext data".to_string());
        let (cv, _) = SqlType::NText(Some(val.clone())).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }

    #[test]
    fn to_cv_binary_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Binary(None, 16).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 16);
    }

    #[test]
    fn to_cv_binary_some() {
        let col = &default_collation();
        let data = vec![1, 2, 3];
        let (cv, _) = SqlType::Binary(Some(data.clone()), 16).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Bytes(data));
    }

    #[test]
    fn to_cv_varbinary_null() {
        let col = &default_collation();
        let (cv, _) = SqlType::VarBinary(None, 100).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
    }

    #[test]
    fn to_cv_varbinarymax_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::VarBinaryMax(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert!(ctx.is_plp);
    }

    #[test]
    fn to_cv_xml_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Xml(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert!(ctx.is_plp);
    }

    #[test]
    fn to_cv_json_null() {
        let col = &default_collation();
        let (cv, ctx) = SqlType::Json(None).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert!(ctx.is_plp);
    }

    #[test]
    fn to_cv_vector_null() {
        use crate::datatypes::sqldatatypes::VectorBaseType;
        let col = &default_collation();
        let (cv, ctx) =
            SqlType::Vector(None, 3, VectorBaseType::Float32).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::Null);
        assert_eq!(ctx.max_size, 8 + 3 * 4); // header + 3 floats
    }

    #[test]
    fn to_cv_char_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("hi".to_string());
        let (cv, _) = SqlType::Char(Some(val.clone()), 10).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }

    #[test]
    fn to_cv_nchar_some() {
        use crate::datatypes::sql_string::SqlString;
        let col = &default_collation();
        let val = SqlString::from_utf8_string("hello".to_string());
        let (cv, _) = SqlType::NChar(Some(val.clone()), 50).to_column_value_and_context(col);
        assert_eq!(cv, ColumnValues::String(val));
    }
}
