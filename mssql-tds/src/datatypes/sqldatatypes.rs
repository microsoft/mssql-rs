// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::core::TdsResult;
use crate::datatypes::sqltypes::get_time_length_from_scale;
use crate::error::Error;
use crate::io::packet_reader::TdsPacketReader;
use crate::token::tokens::SqlCollation;
use std::fmt::format;
use tracing::trace;

/// Wire-level data type identifiers defined by the TDS protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum TdsDataType {
    /// Placeholder for absent data (`0x1F`).
    Void = 0x1F,
    /// Legacy large binary object (`0x22`).
    Image = 0x22,
    /// Legacy variable-length non-Unicode string (`0x23`).
    Text = 0x23,
    /// 16-byte GUID (`0x24`).
    Guid = 0x24,
    /// Legacy variable-length binary (`0x25`).
    VarBinary = 0x25,
    /// Nullable integer of 1/2/4/8 bytes (`0x26`).
    IntN = 0x26,
    /// Legacy variable-length non-Unicode string (`0x27`).
    VarChar = 0x27,
    /// Nullable date (`0x28`).
    DateN = 0x28,
    /// Nullable time with fractional seconds (`0x29`).
    TimeN = 0x29,
    /// Nullable datetime2 with fractional seconds (`0x2A`).
    DateTime2N = 0x2A,
    /// Nullable datetimeoffset (`0x2B`).
    DateTimeOffsetN = 0x2B,
    /// Legacy fixed-length binary (`0x2D`).
    Binary = 0x2D,
    /// Legacy fixed-length non-Unicode string (`0x2F`).
    Char = 0x2F,
    /// 1-byte unsigned integer (`0x30`).
    Int1 = 0x30,
    /// Fixed-width bit (`0x32`).
    Bit = 0x32,
    /// 2-byte signed integer (`0x34`).
    Int2 = 0x34,
    /// Fixed decimal (`0x37`).
    Decimal = 0x37,
    /// 4-byte signed integer (`0x38`).
    Int4 = 0x38,
    /// 4-byte small datetime (`0x3A`).
    DateTim4 = 0x3A,
    /// 4-byte IEEE float (`0x3B`).
    Flt4 = 0x3B,
    /// 8-byte currency (`0x3C`).
    Money = 0x3C,
    /// 8-byte datetime (`0x3D`).
    DateTime = 0x3D,
    /// 8-byte IEEE double (`0x3E`).
    Flt8 = 0x3E,
    /// Fixed numeric (`0x3F`).
    Numeric = 0x3F,
    /// Sql_variant container (`0x62`).
    SsVariant = 0x62,
    /// Legacy variable-length Unicode string (`0x63`).
    NText = 0x63,
    /// Nullable bit (`0x68`).
    BitN = 0x68,
    /// Nullable decimal (`0x6A`).
    DecimalN = 0x6A,
    /// Nullable numeric (`0x6C`).
    NumericN = 0x6C,
    /// Nullable float of 4/8 bytes (`0x6D`).
    FltN = 0x6D,
    /// Nullable money of 4/8 bytes (`0x6E`).
    MoneyN = 0x6E,
    /// Nullable datetime of 4/8 bytes (`0x6F`).
    DateTimeN = 0x6F,
    /// 4-byte currency (`0x7A`).
    Money4 = 0x7A,
    /// 8-byte signed integer (`0x7F`).
    Int8 = 0x7F,
    /// Variable-length binary up to 8000 bytes (`0xA5`).
    BigVarBinary = 0xA5,
    /// Variable-length non-Unicode string up to 8000 bytes (`0xA7`).
    BigVarChar = 0xA7,
    /// Fixed-length binary up to 8000 bytes (`0xAD`).
    BigBinary = 0xAD,
    /// Fixed-length non-Unicode string up to 8000 bytes (`0xAF`).
    BigChar = 0xAF,
    /// Variable-length Unicode string up to 4000 chars (`0xE7`).
    NVarChar = 0xE7,
    /// Fixed-length Unicode string up to 4000 chars (`0xEF`).
    NChar = 0xEF,
    /// CLR user-defined type (`0xF0`).
    Udt = 0xF0,
    /// XML data (`0xF1`).
    Xml = 0xF1,
    /// JSON data (`0xF4`).
    Json = 0xF4,
    /// Fixed-dimension float vector (`0xF5`).
    Vector = 0xF5,

    /// Sentinel for uninitialized type.
    #[default]
    None = 0x00,
}

impl TdsDataType {
    /// Returns the T-SQL type name (e.g. `"bigint"`, `"nvarchar"`) used in
    /// stored-procedure `@parameters` metadata.
    pub fn get_meta_type_name(&self) -> &'static str {
        match self {
            TdsDataType::Int8 => "bigint",
            TdsDataType::Flt8 => "float",
            TdsDataType::Flt4 => "real",
            TdsDataType::BigBinary => "binary",
            TdsDataType::BigVarBinary => "varbinary",
            TdsDataType::Image => "image",
            TdsDataType::Bit => "bit",
            TdsDataType::Int1 => "tinyint",
            TdsDataType::Int2 => "smallint",
            TdsDataType::Int4 => "int",
            TdsDataType::BigChar => "char",
            TdsDataType::BigVarChar => "varchar",
            TdsDataType::Text => "text",
            TdsDataType::NChar => "nchar",
            TdsDataType::NVarChar => "nvarchar",
            TdsDataType::NText => "ntext",
            TdsDataType::DecimalN => "decimal",
            TdsDataType::Xml => "xml",
            TdsDataType::DateTime => "datetime",
            TdsDataType::DateTim4 => "smalldatetime",
            TdsDataType::Money => "money",
            TdsDataType::Money4 => "smallmoney",
            TdsDataType::Guid => "uniqueidentifier",
            TdsDataType::SsVariant => "sql_variant",
            TdsDataType::Udt => "udt",
            TdsDataType::Json => "json",
            TdsDataType::Vector => "vector",
            TdsDataType::DateN => "date",
            TdsDataType::TimeN => "time",
            TdsDataType::DateTime2N => "datetime2",
            TdsDataType::DateTimeOffsetN => "datetimeoffset",
            TdsDataType::VarChar => "varchar",
            TdsDataType::VarBinary => "varbinary",
            TdsDataType::Void => todo!(),
            TdsDataType::IntN => todo!(),
            TdsDataType::Binary => todo!(),
            TdsDataType::Char => todo!(),
            TdsDataType::Decimal => "decimal",
            TdsDataType::Numeric => "numeric",
            TdsDataType::BitN => todo!(),
            TdsDataType::NumericN => todo!(),
            TdsDataType::FltN => todo!(),
            TdsDataType::MoneyN => todo!(),
            TdsDataType::DateTimeN => todo!(),
            TdsDataType::None => todo!(),
        }
    }
}

impl TryFrom<u8> for TdsDataType {
    type Error = Error;

    fn try_from(value: u8) -> TdsResult<Self> {
        match value {
            0x1F => Ok(TdsDataType::Void),
            0x22 => Ok(TdsDataType::Image),
            0x23 => Ok(TdsDataType::Text),
            0x24 => Ok(TdsDataType::Guid),
            0x25 => Ok(TdsDataType::VarBinary),
            0x26 => Ok(TdsDataType::IntN),
            0x27 => Ok(TdsDataType::VarChar),
            0x28 => Ok(TdsDataType::DateN),
            0x29 => Ok(TdsDataType::TimeN),
            0x2A => Ok(TdsDataType::DateTime2N),
            0x2B => Ok(TdsDataType::DateTimeOffsetN),
            0x2D => Ok(TdsDataType::Binary),
            0x2F => Ok(TdsDataType::Char),
            0x30 => Ok(TdsDataType::Int1),
            0x32 => Ok(TdsDataType::Bit),
            0x34 => Ok(TdsDataType::Int2),
            0x37 => Ok(TdsDataType::Decimal),
            0x38 => Ok(TdsDataType::Int4),
            0x3A => Ok(TdsDataType::DateTim4),
            0x3B => Ok(TdsDataType::Flt4),
            0x3C => Ok(TdsDataType::Money),
            0x3D => Ok(TdsDataType::DateTime),
            0x3E => Ok(TdsDataType::Flt8),
            0x3F => Ok(TdsDataType::Numeric),
            0x62 => Ok(TdsDataType::SsVariant),
            0x63 => Ok(TdsDataType::NText),
            0x68 => Ok(TdsDataType::BitN),
            0x6A => Ok(TdsDataType::DecimalN),
            0x6C => Ok(TdsDataType::NumericN),
            0x6D => Ok(TdsDataType::FltN),
            0x6E => Ok(TdsDataType::MoneyN),
            0x6F => Ok(TdsDataType::DateTimeN),
            0x7A => Ok(TdsDataType::Money4),
            0x7F => Ok(TdsDataType::Int8),
            0xA5 => Ok(TdsDataType::BigVarBinary),
            0xA7 => Ok(TdsDataType::BigVarChar),
            0xAD => Ok(TdsDataType::BigBinary),
            0xAF => Ok(TdsDataType::BigChar),
            0xE7 => Ok(TdsDataType::NVarChar),
            0xEF => Ok(TdsDataType::NChar),
            0xF0 => Ok(TdsDataType::Udt),
            0xF1 => Ok(TdsDataType::Xml),
            0xF4 => Ok(TdsDataType::Json),
            0xF5 => Ok(TdsDataType::Vector),
            _ => Err(Error::ProtocolError(format(format_args!(
                "Invalid TDS Type {value:?}"
            )))),
        }
    }
}

// Macro to generate TryFrom implementation for a enum from TdsDataType
macro_rules! impl_try_from_tdstypes {
    (
        $(#[doc = $doc:literal])*
        #[repr(u8)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum $enum_name:ident {
            $($variant:ident = $tds_type:expr_2021),* $(,)?
        }
    ) => {
        #[repr(u8)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum $enum_name {
            $($variant = $tds_type),*
        }

        impl TryFrom<TdsDataType> for $enum_name {
            type Error = Error;

            fn try_from(value: TdsDataType) -> TdsResult<Self> {
                match value {
                    $(TdsDataType::$variant => Ok($enum_name::$variant),)*
                    _ => Err(Error::ProtocolError(format(format_args!("Invalid TDS Type {:?}", value)))),
                }
            }
        }
    };
}

// Vector data type binary format definitions
// These types define the structure of the Vector type as specified in TDS protocol

/// Vector layout format identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VectorLayoutFormat {
    /// Version 1 layout format (0xA9)
    V1 = 0xA9,
}

impl TryFrom<u8> for VectorLayoutFormat {
    type Error = Error;

    fn try_from(value: u8) -> TdsResult<Self> {
        match value {
            0xA9 => Ok(VectorLayoutFormat::V1),
            _ => Err(Error::ProtocolError(format!(
                "Invalid Vector layout format: 0x{:02X}",
                value
            ))),
        }
    }
}

/// Vector layout version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VectorLayoutVersion {
    /// Version 1 (0x01)
    V1 = 0x01,
}

impl TryFrom<u8> for VectorLayoutVersion {
    type Error = Error;

    fn try_from(value: u8) -> TdsResult<Self> {
        match value {
            0x01 => Ok(VectorLayoutVersion::V1),
            _ => Err(Error::ProtocolError(format!(
                "Unsupported Vector layout version: 0x{:02X}",
                value
            ))),
        }
    }
}

/// Vector base type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VectorBaseType {
    /// 32-bit floating point (0x00)
    Float32 = 0x00,
}

impl VectorBaseType {
    /// Returns the size in bytes of each element for this base type.
    pub const fn element_size_bytes(self) -> usize {
        match self {
            VectorBaseType::Float32 => 4,
        }
    }
}

impl TryFrom<u8> for VectorBaseType {
    type Error = Error;

    fn try_from(value: u8) -> TdsResult<Self> {
        match value {
            0x00 => Ok(VectorBaseType::Float32),
            _ => Err(Error::ProtocolError(format!(
                "Unsupported Vector base type: 0x{:02X}",
                value
            ))),
        }
    }
}

/// Maximum number of dimensions in a TDS vector.
pub(crate) const VECTOR_MAX_DIMENSIONS: u16 = 1998;
/// Size of the vector header in bytes.
pub const VECTOR_HEADER_SIZE: usize = 8;
/// Maximum total vector payload size in bytes.
pub(crate) const VECTOR_MAX_SIZE: usize = 8000;

impl_try_from_tdstypes!(
    /// The subset of TdsDataTypes which are categorized as Fixed Length Types.
    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FixedLengthTypes {
        Int1 = TdsDataType::Int1 as u8,
        Bit = TdsDataType::Bit as u8,
        Int2 = TdsDataType::Int2 as u8,
        Int4 = TdsDataType::Int4 as u8,
        DateTim4 = TdsDataType::DateTim4 as u8,
        Flt4 = TdsDataType::Flt4 as u8,
        Money4 = TdsDataType::Money4 as u8,
        Money = TdsDataType::Money as u8,
        DateTime = TdsDataType::DateTime as u8,
        Flt8 = TdsDataType::Flt8 as u8,
        Int8 = TdsDataType::Int8 as u8,
    }
);

impl FixedLengthTypes {
    /// Returns the number of bytes required to store the length of the data type.
    pub fn get_len(&self) -> usize {
        match self {
            FixedLengthTypes::Int1 | FixedLengthTypes::Bit => size_of::<u8>(),
            FixedLengthTypes::Int2 => size_of::<u16>(),
            FixedLengthTypes::Int4
            | FixedLengthTypes::DateTim4
            | FixedLengthTypes::Flt4
            | FixedLengthTypes::Money4 => size_of::<u32>(),

            FixedLengthTypes::Money
            | FixedLengthTypes::DateTime
            | FixedLengthTypes::Flt8
            | FixedLengthTypes::Int8 => size_of::<u64>(),
        }
    }
}

impl_try_from_tdstypes!(
    /// The subset of TdsDataTypes which are categorized as Variable Length Types.
    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum VariableLengthTypes {
        Guid = TdsDataType::Guid as u8,
        IntN = TdsDataType::IntN as u8,
        BitN = TdsDataType::BitN as u8,
        Decimal = TdsDataType::Decimal as u8,
        Numeric = TdsDataType::Numeric as u8,
        DecimalN = TdsDataType::DecimalN as u8,
        NumericN = TdsDataType::NumericN as u8,
        FltN = TdsDataType::FltN as u8,
        MoneyN = TdsDataType::MoneyN as u8,
        DateTimeN = TdsDataType::DateTimeN as u8,
        DateN = TdsDataType::DateN as u8,
        TimeN = TdsDataType::TimeN as u8,
        DateTime2N = TdsDataType::DateTime2N as u8,
        DateTimeOffsetN = TdsDataType::DateTimeOffsetN as u8,
        Char = TdsDataType::Char as u8,
        VarChar = TdsDataType::VarChar as u8,
        Binary = TdsDataType::Binary as u8,
        VarBinary = TdsDataType::VarBinary as u8,
        BigVarBinary = TdsDataType::BigVarBinary as u8,
        BigVarChar = TdsDataType::BigVarChar as u8,
        BigBinary = TdsDataType::BigBinary as u8,
        BigChar = TdsDataType::BigChar as u8,
        NVarChar = TdsDataType::NVarChar as u8,
        NChar = TdsDataType::NChar as u8,
        Text = TdsDataType::Text as u8,
        Image = TdsDataType::Image as u8,
        NText = TdsDataType::NText as u8,
        SsVariant = TdsDataType::SsVariant as u8,
        Vector = TdsDataType::Vector as u8,
    }
);

impl VariableLengthTypes {
    /// Returns the number of bytes that need to be read off the wire, to determine the length
    /// of the data type. This either 1, 2 or 4 bytes, depending on if this variable length type is a TDS
    /// BYTELEN_TYPE, USHORTLEN_TYPE or LONGLEN_TYPE.
    pub fn get_len_byte_count(&self) -> usize {
        match self {
            VariableLengthTypes::BigVarBinary
            | VariableLengthTypes::BigVarChar
            | VariableLengthTypes::BigBinary
            | VariableLengthTypes::BigChar
            | VariableLengthTypes::NVarChar
            | VariableLengthTypes::NChar
            | VariableLengthTypes::Vector => size_of::<u16>(),

            VariableLengthTypes::Guid
            | VariableLengthTypes::IntN
            | VariableLengthTypes::Decimal
            | VariableLengthTypes::Numeric
            | VariableLengthTypes::BitN
            | VariableLengthTypes::DecimalN
            | VariableLengthTypes::NumericN
            | VariableLengthTypes::FltN
            | VariableLengthTypes::MoneyN
            | VariableLengthTypes::DateTimeN
            | VariableLengthTypes::DateN
            | VariableLengthTypes::TimeN
            | VariableLengthTypes::DateTime2N
            | VariableLengthTypes::DateTimeOffsetN
            | VariableLengthTypes::Char
            | VariableLengthTypes::VarChar
            | VariableLengthTypes::Binary
            | VariableLengthTypes::VarBinary => size_of::<u8>(),

            VariableLengthTypes::Image
            | VariableLengthTypes::NText
            | VariableLengthTypes::SsVariant
            | VariableLengthTypes::Text => size_of::<u32>(),
        }
    }
}

impl_try_from_tdstypes!(
    /// Partial Length types (chunked data types). They do not require the full data length
    /// to be specified before the actual data is streamed out.
    #[repr(u8)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum PartialLengthType {
        Xml = TdsDataType::Xml as u8,
        BigVarChar = TdsDataType::BigVarChar as u8,
        BigVarBinary = TdsDataType::BigVarBinary as u8,
        NVarChar = TdsDataType::NVarChar as u8,
        Udt = TdsDataType::Udt as u8,
        Json = TdsDataType::Json as u8,
    }
);

/// Represents the TYPE_INFO in the tds spec
#[derive(Debug, Clone)]
pub struct TypeInfo {
    /// Wire-level data type identifier.
    pub tds_type: TdsDataType,
    /// Byte length of the data on the wire.
    pub length: Length,
    /// Category-specific metadata (fixed, variable, partial-length, etc.).
    pub(crate) type_info_variant: TypeInfoVariant,
}

type Precision = u8;
type Scale = u8;
type Length = usize;

/// Represents one of the variants of the TypeInfo from TDS Spec.
#[derive(Debug, Clone)]
pub(crate) enum TypeInfoVariant {
    FixedLen(FixedLengthTypes),
    VarLenString(VariableLengthTypes, Length, Option<SqlCollation>),
    VarLenPrecisionScale(VariableLengthTypes, Length, Precision, Scale),
    VarLenScale(VariableLengthTypes, Scale),
    VarLen(VariableLengthTypes, Length),
    PartialLen(
        PartialLengthType,
        Option<Length>,
        Option<SqlCollation>,
        Option<XmlInfo>,
        Option<UdtInfo>,
    ),
}

/// XML schema metadata parsed from the TYPE_INFO token.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields parsed from TDS wire protocol for completeness
pub(crate) struct XmlInfo {
    schema_present: u8,
    db_name: Option<String>,
    owning_schema: Option<String>,
    xml_schema_collection: Option<String>,
}

/// UDT metadata received in COLMETADATA tokens.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields parsed from TDS wire protocol for completeness
pub(crate) struct UdtInfoInColMetadata {
    max_byte_size: u16,
    db_name: String,
    schema_name: String,
    type_name: String,
    assembly_qualified_name: UdtMetadata,
}

type UdtMetadata = String;

/// UDT metadata sent in RPC parameter descriptions.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields parsed from TDS wire protocol for completeness
pub(crate) struct UdtInfoInRpc {
    db_name: String,
    schema_name: String,
    type_name: String,
}

/// UDT type information, varying by token context.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum UdtInfo {
    InColMetadata(UdtInfoInColMetadata),
    InRpc(UdtInfoInRpc),
}

pub(crate) async fn read_type_info<T>(reader: &mut T, data_type: TdsDataType) -> TdsResult<TypeInfo>
where
    T: TdsPacketReader + Send + Sync,
{
    // Handle the special Void type which represents no data/null column
    if data_type == TdsDataType::Void {
        return Err(Error::ProtocolError(
            "Void data type (0x1F) is not supported in column metadata. This typically indicates malformed or invalid token stream.".to_string()
        ));
    }

    let fixed_length_type = FixedLengthTypes::try_from(data_type);
    if let Ok(fdt) = fixed_length_type {
        return Ok(TypeInfo {
            tds_type: data_type,
            length: fdt.get_len(),
            type_info_variant: TypeInfoVariant::FixedLen(fdt),
        });
    }

    let var_len_type = VariableLengthTypes::try_from(data_type);

    if let Ok(vdt) = var_len_type {
        let type_info = match vdt {
            VariableLengthTypes::TimeN => {
                let scale = reader.read_byte().await?;
                let length = get_time_length_from_scale(scale)? as usize;
                trace!(
                    "Parsing TimeN: scale={}, calculated length={}",
                    scale, length
                );
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(vdt, scale),
                }
            }
            VariableLengthTypes::DateTime2N => {
                let scale = reader.read_byte().await?;
                let time_length = get_time_length_from_scale(scale)? as usize;
                let length = time_length + 3; // time + 3 bytes for date
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(vdt, scale),
                }
            }
            VariableLengthTypes::DateTimeOffsetN => {
                let scale = reader.read_byte().await?;
                let time_length = get_time_length_from_scale(scale)? as usize;
                let length = time_length + 3 + 2; // time + 3 bytes for date + 2 bytes for offset
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(vdt, scale),
                }
            }
            VariableLengthTypes::Vector => {
                // Vector uses USHORTLEN (u16) for max length and SCALE byte for base type
                let length = reader.read_uint16().await? as usize;
                let base_type_byte = reader.read_byte().await?;

                // Validate base type
                let _base_type = VectorBaseType::try_from(base_type_byte)?;

                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenScale(vdt, base_type_byte),
                }
            }
            VariableLengthTypes::MoneyN
            | VariableLengthTypes::DateTimeN
            | VariableLengthTypes::IntN
            | VariableLengthTypes::FltN
            | VariableLengthTypes::Guid
            | VariableLengthTypes::BitN => {
                let length: usize = reader.read_byte().await? as usize;
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(var_len_type?, length),
                }
            }
            VariableLengthTypes::DateN => TypeInfo {
                tds_type: data_type,
                length: 3, // DATE is always 3 bytes
                type_info_variant: TypeInfoVariant::VarLen(var_len_type?, 3),
            },
            VariableLengthTypes::DecimalN
            | VariableLengthTypes::NumericN
            | VariableLengthTypes::Decimal
            | VariableLengthTypes::Numeric => {
                let len_byte_count = vdt.get_len_byte_count();
                let length = match len_byte_count {
                    1 => reader.read_byte().await? as usize,
                    2 => reader.read_uint16().await? as usize,
                    4 => reader.read_int32().await? as usize,
                    _ => {
                        unreachable!(
                            "Invalid tds length {:?} for type: {:?}",
                            len_byte_count, data_type
                        )
                    }
                };
                let precision = reader.read_byte().await?;
                let scale = reader.read_byte().await?;
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenPrecisionScale(
                        var_len_type.unwrap(),
                        length,
                        precision,
                        scale,
                    ),
                }
            }
            VariableLengthTypes::BigChar
            | VariableLengthTypes::BigVarChar
            | VariableLengthTypes::Text
            | VariableLengthTypes::NText
            | VariableLengthTypes::NChar
            | VariableLengthTypes::NVarChar => {
                let length = get_variable_length(reader, &vdt).await?;

                // Collation is only applicable to BIGCHARTYPE, BIGVARCHARTYPE, TEXTTYPE, NTEXTTYPE,
                // NCHARTYPE, or NVARCHARTYPE
                let collation = {
                    let mut collation_bytes: [u8; 5] = [0; 5];
                    let _ = reader.read_bytes(&mut collation_bytes).await?;

                    if collation_bytes.is_empty() {
                        None
                    } else {
                        collation_bytes.as_slice().try_into().ok()
                    }
                };

                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLenString(
                        var_len_type.unwrap(),
                        length,
                        collation,
                    ),
                }
            }
            VariableLengthTypes::Image => {
                let length = get_variable_length(reader, &vdt).await?;
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(var_len_type.unwrap(), length),
                }
            }
            VariableLengthTypes::BigVarBinary | VariableLengthTypes::BigBinary => {
                let length = get_variable_length(reader, &vdt).await?;

                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(var_len_type.unwrap(), length),
                }
            }
            VariableLengthTypes::SsVariant => {
                let length = get_variable_length(reader, &vdt).await?;
                TypeInfo {
                    tds_type: data_type,
                    length,
                    type_info_variant: TypeInfoVariant::VarLen(var_len_type.unwrap(), length),
                }
            }
            ty => {
                return Err(Error::ProtocolError(format!(
                    "Unsupported TDS type for TypeInfo::read(): {ty:?}. This type is not yet implemented."
                )));
            }
        };

        // At this point, it is possible that we have a data type which could be PLP
        // Check if the data type matches the PLP types, and if so, convert it to PLP
        match data_type {
            TdsDataType::BigVarChar | TdsDataType::BigVarBinary | TdsDataType::NVarChar => {
                let plp_type = PartialLengthType::try_from(data_type);
                // Only if the length from earlier metadata is unknown (0xFFFF), then
                // we can convert it to a PLP type.
                if type_info.length == 0xFFFF {
                    let info = match type_info.type_info_variant {
                        TypeInfoVariant::VarLenString(_, _, collation) => Ok(TypeInfo {
                            tds_type: data_type,
                            length: type_info.length,
                            type_info_variant: TypeInfoVariant::PartialLen(
                                plp_type.unwrap(),
                                Some(type_info.length),
                                collation,
                                None,
                                None,
                            ),
                        }),
                        TypeInfoVariant::VarLen(_, _) => Ok(TypeInfo {
                            tds_type: data_type,
                            length: type_info.length,
                            type_info_variant: TypeInfoVariant::PartialLen(
                                plp_type.unwrap(),
                                Some(type_info.length),
                                None,
                                None,
                                None,
                            ),
                        }),
                        _ => {
                            unreachable!("Other PLP types apart from strings are not handled.");
                        }
                    };
                    return info;
                } else {
                    return Ok(type_info);
                }
            }
            _ => return Ok(type_info),
        }
    }

    let plp_type = PartialLengthType::try_from(data_type);

    if let Ok(pt) = plp_type {
        let type_info = match pt {
            PartialLengthType::Udt => {
                let len = reader.read_uint16().await? as usize;
                let db_name = reader.read_varchar_u8_length().await?;
                let schema_name = reader.read_varchar_u8_length().await?;
                let type_name = reader.read_varchar_u8_length().await?;
                // let assembly_qualified_name_length = reader.read_uint16().await? as usize;
                let assembly_qualified_name = reader.read_varchar_u16_length().await?;
                let assembly_qualified_name: String = match assembly_qualified_name {
                    Some(name) => name,
                    None => {
                        return Err(Error::ProtocolError(
                            "Missing UDT assembly qualified name".to_string(),
                        ));
                    }
                };
                TypeInfo {
                    tds_type: data_type,
                    length: len,
                    type_info_variant: TypeInfoVariant::PartialLen(
                        pt,
                        Some(len),
                        None,
                        None,
                        Some(UdtInfo::InColMetadata(UdtInfoInColMetadata {
                            max_byte_size: len as u16,
                            db_name,
                            schema_name,
                            type_name,
                            assembly_qualified_name,
                        })),
                    ),
                }
            }
            PartialLengthType::Json => TypeInfo {
                tds_type: data_type,
                length: 0xffff,
                type_info_variant: TypeInfoVariant::PartialLen(pt, None, None, None, None),
            },
            PartialLengthType::Xml => {
                let schema_present = reader.read_byte().await?;
                let db_name = if schema_present == 0x01 {
                    Some(reader.read_varchar_u8_length().await?)
                } else {
                    None
                };

                let owning_schema = if schema_present == 0x01 {
                    Some(reader.read_varchar_u8_length().await?)
                } else {
                    None
                };

                let xml_schema_collection = if schema_present == 0x01 {
                    reader.read_varchar_u16_length().await?
                } else {
                    None
                };

                let xml_info = Some(XmlInfo {
                    schema_present,
                    db_name,
                    owning_schema,
                    xml_schema_collection,
                });

                TypeInfo {
                    tds_type: data_type,
                    length: 0xffff,
                    type_info_variant: TypeInfoVariant::PartialLen(pt, None, None, xml_info, None),
                }
            }
            _ => unreachable!("We shouldn't have reached here with a PLP type that is not UDT"),
        };
        return Ok(type_info);
    }

    unimplemented!(
        "Couldnt find the Variable length equivalent of data_type.
        Is this UDT: {:?}",
        data_type
    )
}

/// Returns `true` if the TDS data type uses Unicode encoding (NVarChar, NChar, NText).
pub(crate) fn is_unicode_type(data_type: TdsDataType) -> bool {
    matches!(
        data_type,
        TdsDataType::NVarChar | TdsDataType::NChar | TdsDataType::NText
    )
}

// Reads the variable length data type from the reader and returns the length of the data.
pub(crate) async fn get_variable_length<T>(
    reader: &mut T,
    data_type: &VariableLengthTypes,
) -> TdsResult<usize>
where
    T: TdsPacketReader + Send + Sync,
{
    let len_byte_count = data_type.get_len_byte_count();
    let length = match len_byte_count {
        1 => reader.read_byte().await? as usize,
        2 => reader.read_uint16().await? as usize,
        4 => {
            let len_i32 = reader.read_int32().await?;
            // Negative values indicate invalid protocol data and should error out
            // to prevent capacity overflow from casting negative i32 to huge usize values
            if len_i32 < 0 {
                return Err(Error::ProtocolError(format!(
                    "Invalid length value {len_i32} for data type {data_type:?}. Length cannot be negative."
                )));
            }

            len_i32 as usize
        }
        _ => {
            return Err(Error::ProtocolError(format!(
                "Invalid TDS length byte count {len_byte_count} for data type {data_type:?}. Expected 1, 2, or 4 bytes."
            )));
        }
    };
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tds_data_type_get_meta_type_name() {
        assert_eq!(TdsDataType::Int8.get_meta_type_name(), "bigint");
        assert_eq!(TdsDataType::Flt8.get_meta_type_name(), "float");
        assert_eq!(TdsDataType::Flt4.get_meta_type_name(), "real");
        assert_eq!(TdsDataType::BigBinary.get_meta_type_name(), "binary");
        assert_eq!(TdsDataType::BigVarBinary.get_meta_type_name(), "varbinary");
        assert_eq!(TdsDataType::Image.get_meta_type_name(), "image");
        assert_eq!(TdsDataType::Bit.get_meta_type_name(), "bit");
        assert_eq!(TdsDataType::Int1.get_meta_type_name(), "tinyint");
        assert_eq!(TdsDataType::Int2.get_meta_type_name(), "smallint");
        assert_eq!(TdsDataType::Int4.get_meta_type_name(), "int");
        assert_eq!(TdsDataType::BigChar.get_meta_type_name(), "char");
        assert_eq!(TdsDataType::BigVarChar.get_meta_type_name(), "varchar");
        assert_eq!(TdsDataType::Text.get_meta_type_name(), "text");
        assert_eq!(TdsDataType::NChar.get_meta_type_name(), "nchar");
        assert_eq!(TdsDataType::NVarChar.get_meta_type_name(), "nvarchar");
        assert_eq!(TdsDataType::NText.get_meta_type_name(), "ntext");
        assert_eq!(TdsDataType::DecimalN.get_meta_type_name(), "decimal");
        assert_eq!(TdsDataType::Xml.get_meta_type_name(), "xml");
        assert_eq!(TdsDataType::DateTime.get_meta_type_name(), "datetime");
        assert_eq!(TdsDataType::DateTim4.get_meta_type_name(), "smalldatetime");
        assert_eq!(TdsDataType::Money.get_meta_type_name(), "money");
        assert_eq!(TdsDataType::Money4.get_meta_type_name(), "smallmoney");
        assert_eq!(TdsDataType::Guid.get_meta_type_name(), "uniqueidentifier");
        assert_eq!(TdsDataType::SsVariant.get_meta_type_name(), "sql_variant");
        assert_eq!(TdsDataType::Udt.get_meta_type_name(), "udt");
        assert_eq!(TdsDataType::Json.get_meta_type_name(), "json");
        assert_eq!(TdsDataType::DateN.get_meta_type_name(), "date");
        assert_eq!(TdsDataType::TimeN.get_meta_type_name(), "time");
        assert_eq!(TdsDataType::DateTime2N.get_meta_type_name(), "datetime2");
        assert_eq!(
            TdsDataType::DateTimeOffsetN.get_meta_type_name(),
            "datetimeoffset"
        );
        assert_eq!(TdsDataType::VarChar.get_meta_type_name(), "varchar");
        assert_eq!(TdsDataType::VarBinary.get_meta_type_name(), "varbinary");
        assert_eq!(TdsDataType::Decimal.get_meta_type_name(), "decimal");
        assert_eq!(TdsDataType::Numeric.get_meta_type_name(), "numeric");
    }

    #[test]
    fn test_tds_data_type_try_from_u8() {
        assert_eq!(TdsDataType::try_from(0x1F).unwrap(), TdsDataType::Void);
        assert_eq!(TdsDataType::try_from(0x22).unwrap(), TdsDataType::Image);
        assert_eq!(TdsDataType::try_from(0x23).unwrap(), TdsDataType::Text);
        assert_eq!(TdsDataType::try_from(0x24).unwrap(), TdsDataType::Guid);
        assert_eq!(TdsDataType::try_from(0x30).unwrap(), TdsDataType::Int1);
        assert_eq!(TdsDataType::try_from(0x32).unwrap(), TdsDataType::Bit);
        assert_eq!(TdsDataType::try_from(0x34).unwrap(), TdsDataType::Int2);
        assert_eq!(TdsDataType::try_from(0x38).unwrap(), TdsDataType::Int4);
        assert_eq!(TdsDataType::try_from(0x7F).unwrap(), TdsDataType::Int8);
        assert_eq!(TdsDataType::try_from(0xE7).unwrap(), TdsDataType::NVarChar);
        assert_eq!(TdsDataType::try_from(0xF1).unwrap(), TdsDataType::Xml);
        assert_eq!(TdsDataType::try_from(0xF4).unwrap(), TdsDataType::Json);
    }

    #[test]
    fn test_tds_data_type_try_from_u8_invalid() {
        assert!(TdsDataType::try_from(0xFF).is_err());
        assert!(TdsDataType::try_from(0x00).is_err());
        assert!(TdsDataType::try_from(0x99).is_err());
    }

    #[test]
    fn test_fixed_length_types_try_from() {
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Int1).unwrap(),
            FixedLengthTypes::Int1
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Bit).unwrap(),
            FixedLengthTypes::Bit
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Int2).unwrap(),
            FixedLengthTypes::Int2
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Int4).unwrap(),
            FixedLengthTypes::Int4
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Int8).unwrap(),
            FixedLengthTypes::Int8
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Flt4).unwrap(),
            FixedLengthTypes::Flt4
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Flt8).unwrap(),
            FixedLengthTypes::Flt8
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Money).unwrap(),
            FixedLengthTypes::Money
        );
        assert_eq!(
            FixedLengthTypes::try_from(TdsDataType::Money4).unwrap(),
            FixedLengthTypes::Money4
        );
    }

    #[test]
    fn test_fixed_length_types_try_from_invalid() {
        assert!(FixedLengthTypes::try_from(TdsDataType::NVarChar).is_err());
        assert!(FixedLengthTypes::try_from(TdsDataType::Xml).is_err());
    }

    #[test]
    fn test_fixed_length_types_get_len() {
        assert_eq!(FixedLengthTypes::Int1.get_len(), 1);
        assert_eq!(FixedLengthTypes::Bit.get_len(), 1);
        assert_eq!(FixedLengthTypes::Int2.get_len(), 2);
        assert_eq!(FixedLengthTypes::Int4.get_len(), 4);
        assert_eq!(FixedLengthTypes::Flt4.get_len(), 4);
        assert_eq!(FixedLengthTypes::Money4.get_len(), 4);
        assert_eq!(FixedLengthTypes::DateTim4.get_len(), 4);
        assert_eq!(FixedLengthTypes::Int8.get_len(), 8);
        assert_eq!(FixedLengthTypes::Flt8.get_len(), 8);
        assert_eq!(FixedLengthTypes::Money.get_len(), 8);
        assert_eq!(FixedLengthTypes::DateTime.get_len(), 8);
    }

    #[test]
    fn test_variable_length_types_try_from() {
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::Guid).unwrap(),
            VariableLengthTypes::Guid
        );
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::IntN).unwrap(),
            VariableLengthTypes::IntN
        );
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::NVarChar).unwrap(),
            VariableLengthTypes::NVarChar
        );
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::BigVarChar).unwrap(),
            VariableLengthTypes::BigVarChar
        );
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::Text).unwrap(),
            VariableLengthTypes::Text
        );
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::Image).unwrap(),
            VariableLengthTypes::Image
        );
    }

    #[test]
    fn test_variable_length_types_try_from_invalid() {
        assert!(VariableLengthTypes::try_from(TdsDataType::Int4).is_err());
        assert!(VariableLengthTypes::try_from(TdsDataType::Flt8).is_err());
    }

    #[test]
    fn test_variable_length_types_get_len_byte_count() {
        assert_eq!(VariableLengthTypes::BigVarBinary.get_len_byte_count(), 2);
        assert_eq!(VariableLengthTypes::BigVarChar.get_len_byte_count(), 2);
        assert_eq!(VariableLengthTypes::NVarChar.get_len_byte_count(), 2);
        assert_eq!(VariableLengthTypes::NChar.get_len_byte_count(), 2);
        assert_eq!(VariableLengthTypes::Guid.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::IntN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::DateN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::TimeN.get_len_byte_count(), 1);
    }

    #[test]
    fn test_partial_length_type_try_from() {
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::Xml).unwrap(),
            PartialLengthType::Xml
        );
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::Udt).unwrap(),
            PartialLengthType::Udt
        );
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::Json).unwrap(),
            PartialLengthType::Json
        );
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::BigVarChar).unwrap(),
            PartialLengthType::BigVarChar
        );
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::BigVarBinary).unwrap(),
            PartialLengthType::BigVarBinary
        );
        assert_eq!(
            PartialLengthType::try_from(TdsDataType::NVarChar).unwrap(),
            PartialLengthType::NVarChar
        );
    }

    #[test]
    fn test_partial_length_type_try_from_invalid() {
        assert!(PartialLengthType::try_from(TdsDataType::Int4).is_err());
        assert!(PartialLengthType::try_from(TdsDataType::Bit).is_err());
        assert!(PartialLengthType::try_from(TdsDataType::Text).is_err());
        assert!(PartialLengthType::try_from(TdsDataType::NText).is_err());
        assert!(PartialLengthType::try_from(TdsDataType::Image).is_err());
    }

    #[test]
    fn test_tds_data_type_equality() {
        assert_eq!(TdsDataType::Int4, TdsDataType::Int4);
        assert_ne!(TdsDataType::Int4, TdsDataType::Int8);
    }

    #[test]
    fn test_tds_data_type_clone() {
        let dt = TdsDataType::NVarChar;
        let cloned = dt;
        assert_eq!(dt, cloned);
    }

    #[test]
    fn test_tds_data_type_default() {
        assert_eq!(TdsDataType::default(), TdsDataType::None);
    }

    #[test]
    fn test_fixed_length_types_equality() {
        assert_eq!(FixedLengthTypes::Int4, FixedLengthTypes::Int4);
        assert_ne!(FixedLengthTypes::Int4, FixedLengthTypes::Int8);
    }

    #[test]
    fn test_variable_length_types_equality() {
        assert_eq!(VariableLengthTypes::NVarChar, VariableLengthTypes::NVarChar);
        assert_ne!(VariableLengthTypes::NVarChar, VariableLengthTypes::Text);
    }

    // Vector-specific tests
    #[test]
    fn test_vector_type_meta_name() {
        assert_eq!(TdsDataType::Vector.get_meta_type_name(), "vector");
    }

    #[test]
    fn test_vector_try_from_u8() {
        assert_eq!(TdsDataType::try_from(0xF5).unwrap(), TdsDataType::Vector);
    }

    #[test]
    fn test_vector_in_variable_length_types() {
        assert_eq!(
            VariableLengthTypes::try_from(TdsDataType::Vector).unwrap(),
            VariableLengthTypes::Vector
        );
    }

    #[test]
    fn test_vector_len_byte_count() {
        assert_eq!(
            VariableLengthTypes::Vector.get_len_byte_count(),
            size_of::<u16>()
        );
    }

    #[test]
    fn vector_layout_format_try_from_valid() {
        assert_eq!(
            VectorLayoutFormat::try_from(0xA9).unwrap(),
            VectorLayoutFormat::V1
        );
    }

    #[test]
    fn vector_layout_format_try_from_invalid() {
        assert!(VectorLayoutFormat::try_from(0x00).is_err());
        assert!(VectorLayoutFormat::try_from(0xFF).is_err());
    }

    #[test]
    fn vector_layout_version_try_from_valid() {
        assert_eq!(
            VectorLayoutVersion::try_from(0x01).unwrap(),
            VectorLayoutVersion::V1
        );
    }

    #[test]
    fn vector_layout_version_try_from_invalid() {
        assert!(VectorLayoutVersion::try_from(0x00).is_err());
        assert!(VectorLayoutVersion::try_from(0x02).is_err());
    }

    #[test]
    fn vector_base_type_try_from_valid() {
        let bt = VectorBaseType::try_from(0x00).unwrap();
        assert_eq!(bt, VectorBaseType::Float32);
        assert_eq!(bt.element_size_bytes(), 4);
    }

    #[test]
    fn vector_base_type_try_from_invalid() {
        assert!(VectorBaseType::try_from(0x01).is_err());
        assert!(VectorBaseType::try_from(0xFF).is_err());
    }

    #[test]
    fn tds_data_type_try_from_all_valid() {
        let expected: Vec<(u8, TdsDataType)> = vec![
            (0x1F, TdsDataType::Void),
            (0x22, TdsDataType::Image),
            (0x23, TdsDataType::Text),
            (0x24, TdsDataType::Guid),
            (0x25, TdsDataType::VarBinary),
            (0x26, TdsDataType::IntN),
            (0x27, TdsDataType::VarChar),
            (0x28, TdsDataType::DateN),
            (0x29, TdsDataType::TimeN),
            (0x2A, TdsDataType::DateTime2N),
            (0x2B, TdsDataType::DateTimeOffsetN),
            (0x2D, TdsDataType::Binary),
            (0x2F, TdsDataType::Char),
            (0x30, TdsDataType::Int1),
            (0x32, TdsDataType::Bit),
            (0x34, TdsDataType::Int2),
            (0x37, TdsDataType::Decimal),
            (0x38, TdsDataType::Int4),
            (0x3A, TdsDataType::DateTim4),
            (0x3B, TdsDataType::Flt4),
            (0x3C, TdsDataType::Money),
            (0x3D, TdsDataType::DateTime),
            (0x3E, TdsDataType::Flt8),
            (0x3F, TdsDataType::Numeric),
            (0x62, TdsDataType::SsVariant),
            (0x63, TdsDataType::NText),
            (0x68, TdsDataType::BitN),
            (0x6A, TdsDataType::DecimalN),
            (0x6C, TdsDataType::NumericN),
            (0x6D, TdsDataType::FltN),
            (0x6E, TdsDataType::MoneyN),
            (0x6F, TdsDataType::DateTimeN),
            (0x7A, TdsDataType::Money4),
            (0x7F, TdsDataType::Int8),
            (0xA5, TdsDataType::BigVarBinary),
            (0xA7, TdsDataType::BigVarChar),
            (0xAD, TdsDataType::BigBinary),
            (0xAF, TdsDataType::BigChar),
            (0xE7, TdsDataType::NVarChar),
            (0xEF, TdsDataType::NChar),
            (0xF0, TdsDataType::Udt),
            (0xF1, TdsDataType::Xml),
            (0xF4, TdsDataType::Json),
            (0xF5, TdsDataType::Vector),
        ];
        for (byte, expected_type) in expected {
            assert_eq!(
                TdsDataType::try_from(byte).unwrap(),
                expected_type,
                "byte 0x{byte:02X}"
            );
        }
    }

    #[test]
    fn variable_length_types_get_len_byte_count_all_categories() {
        // USHORTLEN_TYPE = 2 bytes
        assert_eq!(VariableLengthTypes::BigBinary.get_len_byte_count(), 2);
        assert_eq!(VariableLengthTypes::BigChar.get_len_byte_count(), 2);

        // BYTELEN_TYPE = 1 byte
        assert_eq!(VariableLengthTypes::BitN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::DecimalN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::NumericN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::FltN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::MoneyN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::DateTimeN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::DateTime2N.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::DateTimeOffsetN.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::Char.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::VarChar.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::Binary.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::VarBinary.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::Decimal.get_len_byte_count(), 1);
        assert_eq!(VariableLengthTypes::Numeric.get_len_byte_count(), 1);

        // LONGLEN_TYPE = 4 bytes
        assert_eq!(VariableLengthTypes::Image.get_len_byte_count(), 4);
        assert_eq!(VariableLengthTypes::NText.get_len_byte_count(), 4);
        assert_eq!(VariableLengthTypes::SsVariant.get_len_byte_count(), 4);
        assert_eq!(VariableLengthTypes::Text.get_len_byte_count(), 4);
    }

    #[test]
    fn variable_length_types_try_from_all() {
        let cases: Vec<(TdsDataType, VariableLengthTypes)> = vec![
            (TdsDataType::Guid, VariableLengthTypes::Guid),
            (TdsDataType::IntN, VariableLengthTypes::IntN),
            (TdsDataType::BitN, VariableLengthTypes::BitN),
            (TdsDataType::Decimal, VariableLengthTypes::Decimal),
            (TdsDataType::Numeric, VariableLengthTypes::Numeric),
            (TdsDataType::DecimalN, VariableLengthTypes::DecimalN),
            (TdsDataType::NumericN, VariableLengthTypes::NumericN),
            (TdsDataType::FltN, VariableLengthTypes::FltN),
            (TdsDataType::MoneyN, VariableLengthTypes::MoneyN),
            (TdsDataType::DateTimeN, VariableLengthTypes::DateTimeN),
            (TdsDataType::DateN, VariableLengthTypes::DateN),
            (TdsDataType::TimeN, VariableLengthTypes::TimeN),
            (TdsDataType::DateTime2N, VariableLengthTypes::DateTime2N),
            (
                TdsDataType::DateTimeOffsetN,
                VariableLengthTypes::DateTimeOffsetN,
            ),
            (TdsDataType::Char, VariableLengthTypes::Char),
            (TdsDataType::VarChar, VariableLengthTypes::VarChar),
            (TdsDataType::Binary, VariableLengthTypes::Binary),
            (TdsDataType::VarBinary, VariableLengthTypes::VarBinary),
            (TdsDataType::BigVarBinary, VariableLengthTypes::BigVarBinary),
            (TdsDataType::BigVarChar, VariableLengthTypes::BigVarChar),
            (TdsDataType::BigBinary, VariableLengthTypes::BigBinary),
            (TdsDataType::BigChar, VariableLengthTypes::BigChar),
            (TdsDataType::NVarChar, VariableLengthTypes::NVarChar),
            (TdsDataType::NChar, VariableLengthTypes::NChar),
            (TdsDataType::Text, VariableLengthTypes::Text),
            (TdsDataType::Image, VariableLengthTypes::Image),
            (TdsDataType::NText, VariableLengthTypes::NText),
            (TdsDataType::SsVariant, VariableLengthTypes::SsVariant),
            (TdsDataType::Vector, VariableLengthTypes::Vector),
        ];
        for (tds_type, expected) in cases {
            assert_eq!(
                VariableLengthTypes::try_from(tds_type).unwrap(),
                expected,
                "{tds_type:?}"
            );
        }
    }
}
