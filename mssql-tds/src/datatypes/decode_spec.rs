// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-column decode plans derived once from `COLMETADATA`.
//!
//! Decoding used to re-derive, for every cell, facts that are fixed for the whole
//! result set: the string encoding, whether the column is partially
//! length-prefixed, whether it is a legacy LOB, and the decimal/datetime scale. A
//! wide result set turns that into millions of classifications that all return the
//! same answer.
//!
//! A [`ColumnSpec`] answers both *how many bytes the value occupies* and *how to
//! interpret them*, and carries the payloads (encoding, precision, scale) the
//! decoder would otherwise recompute. That lets the per-cell path be a single
//! dispatch with no classification left in it, and it is why the enum has no
//! fallback variant: an escape hatch would force the type switch it replaces to
//! survive alongside it.
//!
//! Encryption is deliberately *not* a spec variant. The spec describes the bytes on
//! the wire, and an encrypted column still arrives shaped by its wire type. Folding
//! the flag in would make an encrypted PLP column stop comparing equal to
//! [`ColumnDecodeSpec::Plp`], silently disabling the guard that refuses to stream
//! ciphertext to PLP readers.

use crate::datatypes::sql_string::{EncodingType, get_encoding_type};
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfoVariant};
use crate::error::Error;
use crate::query::metadata::ColumnMetadata;
use crate::token::tokens::SqlCollation;

/// A column's wire shape plus the flags the row driver needs alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnSpec {
    /// How to read and interpret one cell of this column.
    pub(crate) decode: ColumnDecodeSpec,
    /// Always Encrypted protects this column; the wire bytes are ciphertext.
    pub(crate) encrypted: bool,
}

/// How a single cell of a column is read from the wire and interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnDecodeSpec {
    /// FIXEDLEN: no length prefix, width implied by the kind.
    Fixed(FixedKind),
    /// BYTELEN: one length byte, `0` meaning NULL.
    VarLenU8(VarU8Kind),
    /// USHORTLEN: a `u16` length, `0xFFFF` (`CHARBIN_NULL`) meaning NULL.
    VarLenU16(VarU16Kind),
    /// LONGLEN: text pointer, timestamp, then a `u32` length.
    LongLen(LongLenKind),
    /// PARTLEN: chunked, length-prefixed body.
    Plp(PlpKind),
    /// `sql_variant`: a `u32` length followed by an inline TYPE_INFO.
    Variant,
    /// Terminal: this column can never be decoded. Not a fallback — nothing
    /// downstream re-inspects the metadata, it only formats the error.
    Unsupported {
        data_type: TdsDataType,
        reason: UnsupportedReason,
    },
}

/// Fixed-width types, which carry no length prefix at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FixedKind {
    /// `tinyint`, 1 byte unsigned.
    U8,
    /// `smallint`, 2 bytes.
    I16,
    /// `int`, 4 bytes.
    I32,
    /// `bigint`, 8 bytes.
    I64,
    /// `real`, 4 bytes.
    F32,
    /// `float`, 8 bytes.
    F64,
    /// `bit`, 1 byte.
    Bit,
    /// `smallmoney`, 4 bytes.
    Money4,
    /// `money`, 8 bytes.
    Money8,
    /// `datetime`, 8 bytes.
    DateTime,
    /// `smalldatetime`, 4 bytes.
    SmallDateTime,
}

/// BYTELEN types. The length byte itself stays a per-cell read: it selects the
/// payload width (`IntN` is 1/2/4/8) and signals NULL, so it is wire data rather
/// than metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarU8Kind {
    /// Nullable integer, width from the length byte.
    IntN,
    /// Nullable float, width from the length byte.
    FltN,
    /// Nullable bit.
    BitN,
    /// Nullable money, width from the length byte.
    MoneyN,
    /// Nullable `datetime`/`smalldatetime`, width from the length byte.
    DateTimeN,
    /// Nullable `date`.
    DateN,
    /// `uniqueidentifier`. BYTELEN, not fixed-width: a length byte precedes the
    /// 16 payload bytes and `0` means NULL.
    Guid,
    /// `time(n)`, with the declared scale.
    Time(u8),
    /// `datetime2(n)`, with the declared scale.
    DateTime2(u8),
    /// `datetimeoffset(n)`, with the declared scale.
    DateTimeOffset(u8),
    /// `decimal(p,s)`.
    Decimal { precision: u8, scale: u8 },
    /// `numeric(p,s)`.
    Numeric { precision: u8, scale: u8 },
}

/// USHORTLEN types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarU16Kind {
    /// Opaque bytes (`binary`, non-PLP `varbinary`).
    Bytes,
    /// Character data in the column's resolved encoding.
    String(StringEncoding),
    /// `vector`, validated against the base type and length declared in TYPE_INFO.
    Vector { base_type: u8, declared_len: u16 },
}

/// LONGLEN (legacy LOB) types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LongLenKind {
    /// `image`.
    Bytes,
    /// `text`/`ntext` in the column's resolved encoding.
    String(StringEncoding),
}

/// PARTLEN types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlpKind {
    /// `varbinary(max)`, UDTs.
    Bytes,
    /// `varchar(max)`/`nvarchar(max)` in the column's resolved encoding.
    String(StringEncoding),
    /// `xml`.
    Xml,
    /// `json`.
    Json,
}

/// Which encoding a character column decodes to.
///
/// Deliberately *not* [`EncodingType`], which inlines a [`SqlCollation`] and so
/// costs 16 bytes. Because an enum is as wide as its widest variant, embedding it
/// would make every spec — including `int` — carry that payload into each cell's
/// future. This keeps the classification hoisted while leaving the collation in
/// the metadata the decoder already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StringEncoding {
    /// UTF-8, from a collation with the UTF-8 flag set.
    Utf8,
    /// UTF-16LE, for the Unicode types.
    Utf16,
    /// Codepage implied by the column's collation.
    Lcid,
    /// Collation not yet known; resolved from the connection later.
    Delayed,
}

impl StringEncoding {
    /// Classifies a character column. This is the work being hoisted out of the
    /// per-cell path.
    fn for_column(metadata: &ColumnMetadata) -> Self {
        match get_encoding_type(metadata) {
            EncodingType::Utf8 => StringEncoding::Utf8,
            EncodingType::Utf16 => StringEncoding::Utf16,
            EncodingType::LcidBased(_) => StringEncoding::Lcid,
            EncodingType::DelayedSet => StringEncoding::Delayed,
        }
    }

    /// Rebuilds the [`EncodingType`] a [`SqlString`](crate::datatypes::sql_string::SqlString)
    /// needs. Only the `Lcid` arm touches metadata, and only to copy out a
    /// collation that was already parsed — no reclassification.
    pub(crate) fn materialize(self, metadata: &ColumnMetadata) -> EncodingType {
        match self {
            StringEncoding::Utf8 => EncodingType::Utf8,
            StringEncoding::Utf16 => EncodingType::Utf16,
            StringEncoding::Delayed => EncodingType::DelayedSet,
            StringEncoding::Lcid => EncodingType::LcidBased(collation_of(metadata)),
        }
    }
}

/// The column's collation, defaulted when TYPE_INFO carries none.
fn collation_of(metadata: &ColumnMetadata) -> SqlCollation {
    match metadata.type_info.type_info_variant {
        TypeInfoVariant::PartialLen(_, _, collation, _, _)
        | TypeInfoVariant::VarLenString(_, _, collation) => collation.unwrap_or_default(),
        _ => SqlCollation::default(),
    }
}

/// Why a column can never be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnsupportedReason {
    /// The decoder has no implementation for this wire type.
    NotImplemented,
    /// Fixed-length `decimal`/`numeric`; only the nullable forms are implemented.
    FixedDecimal,
    /// `xml`/`json`/UDT arrived without a partially length-prefixed TYPE_INFO.
    RequiresPlp,
    /// A `time`/`datetime2`/`datetimeoffset` column carried no scale.
    MissingScale,
    /// A `decimal`/`numeric` column carried no precision/scale.
    MissingPrecisionScale,
    /// A `vector` column carried no base type.
    MissingVectorBaseType,
}

impl ColumnSpec {
    /// Derives the decode plan for one column.
    pub(crate) fn for_column(metadata: &ColumnMetadata) -> Self {
        ColumnSpec {
            decode: ColumnDecodeSpec::for_column(metadata),
            // Keyed on the parsed crypto metadata rather than the `fFlags`
            // encrypted bit: decryption needs the CEK/algorithm details, and the
            // parser only records them when column encryption was negotiated.
            encrypted: metadata.crypto_metadata.is_some(),
        }
    }

    /// True when the column's wire bytes are partially length-prefixed.
    ///
    /// Equivalent to [`ColumnMetadata::is_plp`], and deliberately independent of
    /// [`ColumnSpec::encrypted`] so the ciphertext-streaming guard still fires for
    /// encrypted PLP columns.
    pub(crate) fn is_plp(&self) -> bool {
        matches!(self.decode, ColumnDecodeSpec::Plp(_))
    }
}

impl ColumnDecodeSpec {
    pub(crate) fn for_column(metadata: &ColumnMetadata) -> Self {
        let data_type = metadata.data_type;

        // PLP is decided by TYPE_INFO, not by the type code, and it wins over the
        // type's default shape. Keeping this first preserves the ordering the
        // decoder used and keeps `is_plp()` and the `Plp` variant in agreement.
        if metadata.is_plp() {
            return ColumnDecodeSpec::Plp(match data_type {
                TdsDataType::Xml => PlpKind::Xml,
                TdsDataType::Json => PlpKind::Json,
                TdsDataType::Udt | TdsDataType::BigVarBinary => PlpKind::Bytes,
                TdsDataType::BigVarChar | TdsDataType::NVarChar => {
                    PlpKind::String(StringEncoding::for_column(metadata))
                }
                _ => PlpKind::Bytes,
            });
        }

        match data_type {
            TdsDataType::Int1 => ColumnDecodeSpec::Fixed(FixedKind::U8),
            TdsDataType::Int2 => ColumnDecodeSpec::Fixed(FixedKind::I16),
            TdsDataType::Int4 => ColumnDecodeSpec::Fixed(FixedKind::I32),
            TdsDataType::Int8 => ColumnDecodeSpec::Fixed(FixedKind::I64),
            TdsDataType::Flt4 => ColumnDecodeSpec::Fixed(FixedKind::F32),
            TdsDataType::Flt8 => ColumnDecodeSpec::Fixed(FixedKind::F64),
            TdsDataType::Bit => ColumnDecodeSpec::Fixed(FixedKind::Bit),
            TdsDataType::Money4 => ColumnDecodeSpec::Fixed(FixedKind::Money4),
            TdsDataType::Money => ColumnDecodeSpec::Fixed(FixedKind::Money8),
            TdsDataType::DateTime => ColumnDecodeSpec::Fixed(FixedKind::DateTime),
            TdsDataType::DateTim4 => ColumnDecodeSpec::Fixed(FixedKind::SmallDateTime),

            TdsDataType::IntN => ColumnDecodeSpec::VarLenU8(VarU8Kind::IntN),
            TdsDataType::FltN => ColumnDecodeSpec::VarLenU8(VarU8Kind::FltN),
            TdsDataType::BitN => ColumnDecodeSpec::VarLenU8(VarU8Kind::BitN),
            TdsDataType::MoneyN => ColumnDecodeSpec::VarLenU8(VarU8Kind::MoneyN),
            TdsDataType::DateTimeN => ColumnDecodeSpec::VarLenU8(VarU8Kind::DateTimeN),
            TdsDataType::DateN => ColumnDecodeSpec::VarLenU8(VarU8Kind::DateN),
            TdsDataType::Guid => ColumnDecodeSpec::VarLenU8(VarU8Kind::Guid),

            TdsDataType::TimeN => Self::scaled(metadata, VarU8Kind::Time),
            TdsDataType::DateTime2N => Self::scaled(metadata, VarU8Kind::DateTime2),
            TdsDataType::DateTimeOffsetN => Self::scaled(metadata, VarU8Kind::DateTimeOffset),

            TdsDataType::DecimalN => Self::precision_scaled(metadata, |precision, scale| {
                VarU8Kind::Decimal { precision, scale }
            }),
            TdsDataType::NumericN => Self::precision_scaled(metadata, |precision, scale| {
                VarU8Kind::Numeric { precision, scale }
            }),

            TdsDataType::BigBinary | TdsDataType::BigVarBinary => {
                ColumnDecodeSpec::VarLenU16(VarU16Kind::Bytes)
            }
            TdsDataType::NChar
            | TdsDataType::NVarChar
            | TdsDataType::BigChar
            | TdsDataType::BigVarChar
            | TdsDataType::Char
            | TdsDataType::VarChar => ColumnDecodeSpec::VarLenU16(VarU16Kind::String(
                StringEncoding::for_column(metadata),
            )),
            TdsDataType::Vector => Self::vector(metadata),

            TdsDataType::Text | TdsDataType::NText => {
                ColumnDecodeSpec::LongLen(LongLenKind::String(StringEncoding::for_column(metadata)))
            }
            TdsDataType::Image => ColumnDecodeSpec::LongLen(LongLenKind::Bytes),

            TdsDataType::SsVariant => ColumnDecodeSpec::Variant,

            TdsDataType::Xml | TdsDataType::Json | TdsDataType::Udt => {
                Self::unsupported(data_type, UnsupportedReason::RequiresPlp)
            }
            TdsDataType::Decimal | TdsDataType::Numeric => {
                Self::unsupported(data_type, UnsupportedReason::FixedDecimal)
            }
            TdsDataType::Void
            | TdsDataType::VarBinary
            | TdsDataType::Binary
            | TdsDataType::SqlTable
            | TdsDataType::None => Self::unsupported(data_type, UnsupportedReason::NotImplemented),
        }
    }

    fn scaled(metadata: &ColumnMetadata, kind: fn(u8) -> VarU8Kind) -> Self {
        match metadata.get_scale() {
            Some(scale) => ColumnDecodeSpec::VarLenU8(kind(scale)),
            None => Self::unsupported(metadata.data_type, UnsupportedReason::MissingScale),
        }
    }

    fn precision_scaled(metadata: &ColumnMetadata, kind: fn(u8, u8) -> VarU8Kind) -> Self {
        match metadata.type_info.type_info_variant {
            TypeInfoVariant::VarLenPrecisionScale(_, _, precision, scale) => {
                ColumnDecodeSpec::VarLenU8(kind(precision, scale))
            }
            _ => Self::unsupported(metadata.data_type, UnsupportedReason::MissingPrecisionScale),
        }
    }

    fn vector(metadata: &ColumnMetadata) -> Self {
        match metadata.type_info.type_info_variant {
            TypeInfoVariant::VarLenScale(_, base_type) => {
                ColumnDecodeSpec::VarLenU16(VarU16Kind::Vector {
                    base_type,
                    // Saturating is safe: the wire length is a `u16` and `0xFFFF`
                    // is the NULL marker, so an over-long declaration can never
                    // compare equal to a real payload length.
                    declared_len: u16::try_from(metadata.type_info.length).unwrap_or(u16::MAX),
                })
            }
            _ => Self::unsupported(metadata.data_type, UnsupportedReason::MissingVectorBaseType),
        }
    }

    fn unsupported(data_type: TdsDataType, reason: UnsupportedReason) -> Self {
        ColumnDecodeSpec::Unsupported { data_type, reason }
    }
}

/// Resolves the plan to use for `columns`, re-deriving it when the cached plan
/// does not cover them.
///
/// Checked once per row rather than once per cell, so the per-cell path is a
/// bare index. Re-deriving costs time; substituting a default spec would silently
/// change how a cell is decoded, so the plan is treated as a cache and never as
/// authority.
pub(crate) fn resolve_plan<'a>(
    cached: &'a [ColumnSpec],
    columns: &[ColumnMetadata],
    rederived: &'a mut Vec<ColumnSpec>,
) -> &'a [ColumnSpec] {
    if cached.len() == columns.len() {
        return cached;
    }
    rederived.extend(columns.iter().map(ColumnSpec::for_column));
    rederived
}

/// Keeps the spec small enough to stay cheap in the per-cell decode future.
///
/// An oversized spec is not a correctness bug, so this is a size assertion rather
/// than a test: it fails the build, where a regression here would otherwise be
/// invisible until someone re-ran the row benchmark.
const _: () = assert!(size_of::<ColumnSpec>() <= 8);

impl UnsupportedReason {
    /// Builds the error a column with this reason produces when decoded.
    pub(crate) fn into_error(self, data_type: TdsDataType) -> Error {
        match self {
            UnsupportedReason::NotImplemented => Error::UnimplementedFeature {
                feature: format!("Data type {data_type:?}"),
                context: format!(
                    "Data type {:?} (0x{:02X}) is not yet supported in the decoder",
                    data_type, data_type as u8
                ),
            },
            UnsupportedReason::FixedDecimal => {
                let nullable = if data_type == TdsDataType::Numeric {
                    "NumericN"
                } else {
                    "DecimalN"
                };
                Error::UnimplementedFeature {
                    feature: format!("Fixed-length {data_type:?} type"),
                    context: format!(
                        "Data type {:?} (0x{:02X}) is not implemented. Use {} instead.",
                        data_type, data_type as u8, nullable
                    ),
                }
            }
            UnsupportedReason::RequiresPlp => {
                let label = match data_type {
                    TdsDataType::Xml => "XML",
                    TdsDataType::Json => "JSON",
                    _ => "UDT",
                };
                Error::ProtocolError(format!(
                    "{label} column metadata is not partially-length-prefixed"
                ))
            }
            UnsupportedReason::MissingScale => {
                Error::ImplementationError(format!("{data_type:?} type should have scale"))
            }
            UnsupportedReason::MissingPrecisionScale => Error::ProtocolError(
                "Invalid type info variant for Decimal/Numeric: expected VarLenPrecisionScale"
                    .to_string(),
            ),
            UnsupportedReason::MissingVectorBaseType => {
                Error::ProtocolError("Vector metadata missing scale (base type)".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sqldatatypes::{
        FixedLengthTypes, PartialLengthType, TypeInfo, VariableLengthTypes,
    };
    use crate::query::metadata::{ColumnMetadata, CryptoMetadata};

    fn column(data_type: TdsDataType, type_info_variant: TypeInfoVariant) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags: 0,
            type_info: TypeInfo {
                tds_type: data_type,
                length: 8,
                type_info_variant,
            },
            data_type,
            column_name: "col".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        }
    }

    fn fixed(data_type: TdsDataType) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::FixedLen(
                FixedLengthTypes::try_from(data_type).unwrap_or(FixedLengthTypes::Int4),
            ),
        )
    }

    fn varlen(data_type: TdsDataType) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::VarLen(
                VariableLengthTypes::try_from(data_type).unwrap_or(VariableLengthTypes::IntN),
                8,
            ),
        )
    }

    fn string(data_type: TdsDataType) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::VarLenString(
                VariableLengthTypes::try_from(data_type).unwrap_or(VariableLengthTypes::BigVarChar),
                8,
                None,
            ),
        )
    }

    fn scale(data_type: TdsDataType, scale: u8) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::VarLenScale(
                VariableLengthTypes::try_from(data_type).unwrap_or(VariableLengthTypes::TimeN),
                scale,
            ),
        )
    }

    fn precision_scale(data_type: TdsDataType, precision: u8, scale: u8) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::VarLenPrecisionScale(
                VariableLengthTypes::try_from(data_type).unwrap_or(VariableLengthTypes::DecimalN),
                8,
                precision,
                scale,
            ),
        )
    }

    fn plp(data_type: TdsDataType) -> ColumnMetadata {
        column(
            data_type,
            TypeInfoVariant::PartialLen(
                PartialLengthType::try_from(data_type).unwrap_or(PartialLengthType::BigVarBinary),
                Some(0xFFFF),
                None,
                None,
                None,
            ),
        )
    }

    fn spec(metadata: &ColumnMetadata) -> ColumnDecodeSpec {
        ColumnDecodeSpec::for_column(metadata)
    }

    /// Every `TdsDataType` maps to a concrete spec. Written as an exhaustive
    /// `match` so adding a wire type fails to compile until it is classified —
    /// the enum has no fallback variant to absorb it.
    #[test]
    fn every_data_type_is_classified() {
        fn expected(data_type: TdsDataType) -> ColumnDecodeSpec {
            let lcid = StringEncoding::Lcid;
            match data_type {
                TdsDataType::Int1 => ColumnDecodeSpec::Fixed(FixedKind::U8),
                TdsDataType::Int2 => ColumnDecodeSpec::Fixed(FixedKind::I16),
                TdsDataType::Int4 => ColumnDecodeSpec::Fixed(FixedKind::I32),
                TdsDataType::Int8 => ColumnDecodeSpec::Fixed(FixedKind::I64),
                TdsDataType::Flt4 => ColumnDecodeSpec::Fixed(FixedKind::F32),
                TdsDataType::Flt8 => ColumnDecodeSpec::Fixed(FixedKind::F64),
                TdsDataType::Bit => ColumnDecodeSpec::Fixed(FixedKind::Bit),
                TdsDataType::Money4 => ColumnDecodeSpec::Fixed(FixedKind::Money4),
                TdsDataType::Money => ColumnDecodeSpec::Fixed(FixedKind::Money8),
                TdsDataType::DateTime => ColumnDecodeSpec::Fixed(FixedKind::DateTime),
                TdsDataType::DateTim4 => ColumnDecodeSpec::Fixed(FixedKind::SmallDateTime),

                TdsDataType::IntN => ColumnDecodeSpec::VarLenU8(VarU8Kind::IntN),
                TdsDataType::FltN => ColumnDecodeSpec::VarLenU8(VarU8Kind::FltN),
                TdsDataType::BitN => ColumnDecodeSpec::VarLenU8(VarU8Kind::BitN),
                TdsDataType::MoneyN => ColumnDecodeSpec::VarLenU8(VarU8Kind::MoneyN),
                TdsDataType::DateTimeN => ColumnDecodeSpec::VarLenU8(VarU8Kind::DateTimeN),
                TdsDataType::DateN => ColumnDecodeSpec::VarLenU8(VarU8Kind::DateN),
                TdsDataType::Guid => ColumnDecodeSpec::VarLenU8(VarU8Kind::Guid),
                TdsDataType::TimeN => ColumnDecodeSpec::VarLenU8(VarU8Kind::Time(7)),
                TdsDataType::DateTime2N => ColumnDecodeSpec::VarLenU8(VarU8Kind::DateTime2(7)),
                TdsDataType::DateTimeOffsetN => {
                    ColumnDecodeSpec::VarLenU8(VarU8Kind::DateTimeOffset(7))
                }
                TdsDataType::DecimalN => ColumnDecodeSpec::VarLenU8(VarU8Kind::Decimal {
                    precision: 18,
                    scale: 4,
                }),
                TdsDataType::NumericN => ColumnDecodeSpec::VarLenU8(VarU8Kind::Numeric {
                    precision: 18,
                    scale: 4,
                }),

                TdsDataType::BigBinary | TdsDataType::BigVarBinary => {
                    ColumnDecodeSpec::VarLenU16(VarU16Kind::Bytes)
                }
                TdsDataType::NChar | TdsDataType::NVarChar => {
                    ColumnDecodeSpec::VarLenU16(VarU16Kind::String(StringEncoding::Utf16))
                }
                TdsDataType::BigChar
                | TdsDataType::BigVarChar
                | TdsDataType::Char
                | TdsDataType::VarChar => ColumnDecodeSpec::VarLenU16(VarU16Kind::String(lcid)),
                TdsDataType::Vector => ColumnDecodeSpec::VarLenU16(VarU16Kind::Vector {
                    base_type: 0,
                    declared_len: 8,
                }),

                TdsDataType::Text => ColumnDecodeSpec::LongLen(LongLenKind::String(lcid)),
                TdsDataType::NText => {
                    ColumnDecodeSpec::LongLen(LongLenKind::String(StringEncoding::Utf16))
                }
                TdsDataType::Image => ColumnDecodeSpec::LongLen(LongLenKind::Bytes),

                TdsDataType::SsVariant => ColumnDecodeSpec::Variant,

                TdsDataType::Xml | TdsDataType::Json | TdsDataType::Udt => {
                    ColumnDecodeSpec::Unsupported {
                        data_type,
                        reason: UnsupportedReason::RequiresPlp,
                    }
                }
                TdsDataType::Decimal | TdsDataType::Numeric => ColumnDecodeSpec::Unsupported {
                    data_type,
                    reason: UnsupportedReason::FixedDecimal,
                },
                TdsDataType::Void
                | TdsDataType::VarBinary
                | TdsDataType::Binary
                | TdsDataType::SqlTable
                | TdsDataType::None => ColumnDecodeSpec::Unsupported {
                    data_type,
                    reason: UnsupportedReason::NotImplemented,
                },
            }
        }

        for data_type in ALL_DATA_TYPES {
            let metadata = match data_type {
                TdsDataType::TimeN | TdsDataType::DateTime2N | TdsDataType::DateTimeOffsetN => {
                    scale(data_type, 7)
                }
                TdsDataType::Vector => scale(data_type, 0),
                TdsDataType::DecimalN | TdsDataType::NumericN => precision_scale(data_type, 18, 4),
                TdsDataType::NChar
                | TdsDataType::NVarChar
                | TdsDataType::BigChar
                | TdsDataType::BigVarChar
                | TdsDataType::Char
                | TdsDataType::VarChar
                | TdsDataType::Text
                | TdsDataType::NText => string(data_type),
                TdsDataType::Int1
                | TdsDataType::Int2
                | TdsDataType::Int4
                | TdsDataType::Int8
                | TdsDataType::Flt4
                | TdsDataType::Flt8
                | TdsDataType::Bit
                | TdsDataType::Money
                | TdsDataType::Money4
                | TdsDataType::DateTime
                | TdsDataType::DateTim4 => fixed(data_type),
                _ => varlen(data_type),
            };
            assert_eq!(
                spec(&metadata),
                expected(data_type),
                "unexpected spec for {data_type:?}"
            );
        }
    }

    const ALL_DATA_TYPES: [TdsDataType; 46] = [
        TdsDataType::Void,
        TdsDataType::Image,
        TdsDataType::Text,
        TdsDataType::Guid,
        TdsDataType::VarBinary,
        TdsDataType::IntN,
        TdsDataType::VarChar,
        TdsDataType::DateN,
        TdsDataType::TimeN,
        TdsDataType::DateTime2N,
        TdsDataType::DateTimeOffsetN,
        TdsDataType::Binary,
        TdsDataType::Char,
        TdsDataType::Int1,
        TdsDataType::Bit,
        TdsDataType::Int2,
        TdsDataType::Decimal,
        TdsDataType::Int4,
        TdsDataType::DateTim4,
        TdsDataType::Flt4,
        TdsDataType::Money,
        TdsDataType::DateTime,
        TdsDataType::Flt8,
        TdsDataType::Numeric,
        TdsDataType::SsVariant,
        TdsDataType::NText,
        TdsDataType::BitN,
        TdsDataType::DecimalN,
        TdsDataType::NumericN,
        TdsDataType::FltN,
        TdsDataType::MoneyN,
        TdsDataType::DateTimeN,
        TdsDataType::Money4,
        TdsDataType::Int8,
        TdsDataType::BigVarBinary,
        TdsDataType::BigVarChar,
        TdsDataType::BigBinary,
        TdsDataType::BigChar,
        TdsDataType::NVarChar,
        TdsDataType::NChar,
        TdsDataType::Udt,
        TdsDataType::Xml,
        TdsDataType::SqlTable,
        TdsDataType::Json,
        TdsDataType::Vector,
        TdsDataType::None,
    ];

    #[test]
    fn plp_type_info_wins_over_the_type_code() {
        assert_eq!(
            spec(&plp(TdsDataType::BigVarBinary)),
            ColumnDecodeSpec::Plp(PlpKind::Bytes)
        );
        assert_eq!(
            spec(&plp(TdsDataType::Xml)),
            ColumnDecodeSpec::Plp(PlpKind::Xml)
        );
        assert_eq!(
            spec(&plp(TdsDataType::Json)),
            ColumnDecodeSpec::Plp(PlpKind::Json)
        );
        assert_eq!(
            spec(&plp(TdsDataType::Udt)),
            ColumnDecodeSpec::Plp(PlpKind::Bytes)
        );
        assert_eq!(
            spec(&plp(TdsDataType::NVarChar)),
            ColumnDecodeSpec::Plp(PlpKind::String(StringEncoding::Utf16))
        );
        assert_eq!(
            spec(&plp(TdsDataType::BigVarChar)),
            ColumnDecodeSpec::Plp(PlpKind::String(StringEncoding::Lcid))
        );
    }

    /// Regression guard for the bug that closed #254: folding encryption into the
    /// spec made encrypted PLP columns stop matching `Plp`, silently bypassing the
    /// guard that refuses to stream ciphertext to PLP readers.
    #[test]
    fn encrypted_plp_column_still_classifies_as_plp() {
        let mut metadata = plp(TdsDataType::BigVarBinary);
        metadata.flags |= 0x0800;
        metadata.crypto_metadata = Some(CryptoMetadata {
            cek_table_ordinal: 0,
            base_data_type: TdsDataType::Int4,
            base_type_info: TypeInfo {
                tds_type: TdsDataType::Int4,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            cipher_algorithm_id: 2,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        });

        let spec = ColumnSpec::for_column(&metadata);
        assert_eq!(spec.decode, ColumnDecodeSpec::Plp(PlpKind::Bytes));
        assert!(spec.encrypted);
        assert!(spec.is_plp());
    }

    #[test]
    fn encryption_is_keyed_on_crypto_metadata_not_the_flag() {
        let mut metadata = varlen(TdsDataType::IntN);
        metadata.flags |= 0x0800;
        assert!(metadata.is_encrypted());
        assert!(!ColumnSpec::for_column(&metadata).encrypted);
    }

    #[test]
    fn missing_scale_and_precision_are_terminal_not_defaulted() {
        for data_type in [
            TdsDataType::TimeN,
            TdsDataType::DateTime2N,
            TdsDataType::DateTimeOffsetN,
        ] {
            assert_eq!(
                spec(&varlen(data_type)),
                ColumnDecodeSpec::Unsupported {
                    data_type,
                    reason: UnsupportedReason::MissingScale,
                }
            );
        }
        for data_type in [TdsDataType::DecimalN, TdsDataType::NumericN] {
            assert_eq!(
                spec(&varlen(data_type)),
                ColumnDecodeSpec::Unsupported {
                    data_type,
                    reason: UnsupportedReason::MissingPrecisionScale,
                }
            );
        }
        assert_eq!(
            spec(&varlen(TdsDataType::Vector)),
            ColumnDecodeSpec::Unsupported {
                data_type: TdsDataType::Vector,
                reason: UnsupportedReason::MissingVectorBaseType,
            }
        );
    }

    /// A `vector` whose declared length exceeds `u16` saturates rather than
    /// wrapping: `0xFFFF` is the NULL marker, so a saturated length can never
    /// compare equal to a real payload length and the column still errors.
    #[test]
    fn oversized_vector_length_saturates() {
        let mut metadata = scale(TdsDataType::Vector, 1);
        metadata.type_info.length = usize::MAX;
        assert_eq!(
            spec(&metadata),
            ColumnDecodeSpec::VarLenU16(VarU16Kind::Vector {
                base_type: 1,
                declared_len: u16::MAX,
            })
        );
    }

    #[test]
    fn resolve_plan_rederives_when_the_plan_is_short() {
        let columns = vec![fixed(TdsDataType::Int4), fixed(TdsDataType::Flt8)];
        let cached: Vec<_> = columns.iter().map(ColumnSpec::for_column).collect();

        let mut scratch = Vec::new();
        assert_eq!(resolve_plan(&cached, &columns, &mut scratch), &cached[..]);
        assert!(scratch.is_empty(), "a matching plan must be used as-is");

        let mut scratch = Vec::new();
        assert_eq!(resolve_plan(&[], &columns, &mut scratch), &cached[..]);

        let mut scratch = Vec::new();
        assert_eq!(
            resolve_plan(&cached[..1], &columns, &mut scratch),
            &cached[..]
        );
    }

    #[test]
    fn unsupported_reasons_produce_distinct_errors() {
        let cases = [
            (TdsDataType::Xml, UnsupportedReason::RequiresPlp, "XML"),
            (TdsDataType::Json, UnsupportedReason::RequiresPlp, "JSON"),
            (TdsDataType::Udt, UnsupportedReason::RequiresPlp, "UDT"),
        ];
        for (data_type, reason, label) in cases {
            let message = reason.into_error(data_type).to_string();
            assert!(
                message.contains(&format!(
                    "{label} column metadata is not partially-length-prefixed"
                )),
                "unexpected message for {data_type:?}: {message}"
            );
        }

        assert!(
            UnsupportedReason::FixedDecimal
                .into_error(TdsDataType::Numeric)
                .to_string()
                .contains("NumericN")
        );
        assert!(
            UnsupportedReason::FixedDecimal
                .into_error(TdsDataType::Decimal)
                .to_string()
                .contains("DecimalN")
        );
        assert!(
            UnsupportedReason::MissingScale
                .into_error(TdsDataType::TimeN)
                .to_string()
                .contains("should have scale")
        );
        assert!(
            UnsupportedReason::NotImplemented
                .into_error(TdsDataType::SqlTable)
                .to_string()
                .contains("not yet supported")
        );
        assert!(
            UnsupportedReason::MissingVectorBaseType
                .into_error(TdsDataType::Vector)
                .to_string()
                .contains("base type")
        );
    }

    /// The plan is one entry per column and is copied per cell, so a bloated spec
    /// would eat the win it exists to deliver.
    #[test]
    fn spec_stays_small() {
        assert!(
            size_of::<ColumnSpec>() <= 32,
            "ColumnSpec grew to {} bytes",
            size_of::<ColumnSpec>()
        );
    }
}
