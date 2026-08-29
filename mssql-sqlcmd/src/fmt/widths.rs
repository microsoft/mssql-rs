// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Column display widths.
//!
//! Every number here was read off the shipped binary by selecting one column of
//! each type and measuring the dashed underline, not taken from documentation.

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;

use crate::compat::Compat;

/// How a column's text sits inside its field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Which of the two width caps applies to a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cap {
    /// Capped by `-Y` / `SQLCMDMAXFIXEDTYPEWIDTH`: the sized character and
    /// binary types.
    Fixed,
    /// Capped by `-y` / `SQLCMDMAXVARTYPEWIDTH`: the large object types, whose
    /// declared size carries no useful bound.
    Large,
    /// `sql_variant`, which takes a flat width that neither switch affects.
    Variant,
    /// Numeric and date types, which neither switch affects.
    None,
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnLayout {
    pub width: usize,
    pub align: Align,
}

/// A width of zero means "as wide as the value": neither padded nor truncated.
pub const NATURAL_WIDTH: usize = 0;

/// Widths that depend only on the type, not on its declared size.
///
/// The nullable type codes (`IntN`, `FltN`, `MoneyN`, `DateTimeN`, `BitN`) are
/// deliberately absent: one code covers several concrete types and only the
/// wire size tells them apart, so they go through [`width_from_size`].
fn intrinsic_width(data_type: TdsDataType) -> Option<usize> {
    use TdsDataType::*;
    Some(match data_type {
        Bit => 1,
        Int1 => 3,
        Int2 => 6,
        Int4 => 11,
        Int8 => 20,
        Flt4 => 14,
        Flt8 => 24,
        Money => 21,
        Money4 => 12,
        DateN => 16,
        TimeN => 22,
        DateTime => 23,
        DateTim4 => 19,
        DateTime2N => 38,
        DateTimeOffsetN => 45,
        Guid => 36,
        _ => return Option::None,
    })
}

/// `IntN`, `FltN`, `MoneyN` and `DateTimeN` are the nullable forms; the wire
/// size tells us which concrete type is underneath.
fn width_from_size(data_type: TdsDataType, size: usize) -> Option<usize> {
    use TdsDataType::*;
    Some(match (data_type, size) {
        (BitN, _) => 1,
        (IntN, 1) => 3,
        (IntN, 2) => 6,
        (IntN, 4) => 11,
        (IntN, 8) => 20,
        (FltN, 4) => 14,
        (FltN, 8) => 24,
        (MoneyN, 4) => 12,
        (MoneyN, 8) => 21,
        // `smalldatetime` is the four-byte form of `datetime`.
        (DateTimeN, 4) => 19,
        (DateTimeN, 8) => 23,
        _ => return Option::None,
    })
}

fn is_numeric_or_temporal(data_type: TdsDataType) -> bool {
    use TdsDataType::*;
    matches!(
        data_type,
        Bit | BitN
            | Int1
            | Int2
            | Int4
            | Int8
            | IntN
            | Flt4
            | Flt8
            | FltN
            | Money
            | Money4
            | MoneyN
            | Decimal
            | DecimalN
            | Numeric
            | NumericN
            | DateN
            | TimeN
            | DateTime
            | DateTimeN
            | DateTim4
            | DateTime2N
            | DateTimeOffsetN
    )
}

/// `varchar(max)` and friends share a type code with their sized forms and are
/// told apart only by this length sentinel.
const MAX_SIZE_SENTINEL: usize = 0xFFFF;

/// The width both references give a `sql_variant` column.
///
/// A variant's declared length describes the widest value it could hold, not
/// the one it does, and the base type is known only per value. Rather than size
/// the column from either, the reference prints a flat 8000 — and unlike the
/// other variable-width types, does not let `-y` cap it.
const VARIANT_WIDTH: usize = 8000;

fn classify(data_type: TdsDataType, size: usize) -> Cap {
    use TdsDataType::*;
    match data_type {
        SsVariant => Cap::Variant,
        Text | NText | Image | Xml | Json | Udt | Vector => Cap::Large,
        Char | NChar | VarChar | NVarChar | BigChar | BigVarChar | Binary | BigBinary
        | VarBinary | BigVarBinary => {
            if size == MAX_SIZE_SENTINEL {
                Cap::Large
            } else {
                Cap::Fixed
            }
        }
        _ => Cap::None,
    }
}

/// `varchar(max)` and friends arrive with a sentinel size rather than a real one.
/// Characters occupy one column each; binary renders as `0x` plus two hex
/// digits per byte.
fn declared_width(data_type: TdsDataType, size: usize, precision: Option<u8>) -> usize {
    use TdsDataType::*;
    match data_type {
        Decimal | DecimalN | Numeric | NumericN => precision.unwrap_or(18) as usize + 2,
        NChar | NVarChar => size / 2,
        Binary | BigBinary | VarBinary | BigVarBinary => size * 2 + 2,
        _ => size,
    }
}

fn type_size(column: &ColumnMetadata) -> usize {
    // `TypeInfo::length` is the wire size; `max` types carry a sentinel.
    column.type_info.length
}

/// Widths go-sqlcmd uses where they differ from ODBC's.
///
/// go-sqlcmd asks the Go driver for each column's declared length rather than
/// carrying ODBC's own table, so several types come out differently: `bigint`
/// and `money` are a digit wider, and `time` and the binary types are sized
/// from the declared length without allowing for the `0x` prefix and two hex
/// digits per byte that the value actually needs.
fn go_width(data_type: TdsDataType, size: usize) -> Option<usize> {
    use TdsDataType::*;
    Some(match data_type {
        Int8 => 21,
        IntN if size == 8 => 21,
        Money => 24,
        MoneyN if size == 8 => 24,
        Money4 => 14,
        MoneyN if size == 4 => 14,
        TimeN => 16,
        Binary | BigBinary | VarBinary | BigVarBinary if size != MAX_SIZE_SENTINEL => size,
        _ => return Option::None,
    })
}

/// Whether a column's values line up on the right.
pub fn is_right_justified(column: &ColumnMetadata) -> bool {
    is_numeric_or_temporal(column.data_type)
}

/// Computes the field width and alignment for one column.
///
/// `max_var` is `-y` (0 meaning unlimited) and `max_fixed` is `-Y` (0 meaning
/// unlimited). The column name is a floor: a narrow type under a long heading
/// widens to fit the heading.
pub fn layout(
    column: &ColumnMetadata,
    max_var: usize,
    max_fixed: usize,
    compat: Compat,
) -> ColumnLayout {
    let data_type = column.data_type;
    let size = type_size(column);

    let mut width = compat
        .is_go()
        .then(|| go_width(data_type, size))
        .flatten()
        .or_else(|| intrinsic_width(data_type))
        .or_else(|| width_from_size(data_type, size))
        .unwrap_or_else(|| declared_width(data_type, size, column.get_precision()));

    match classify(data_type, size) {
        Cap::Large => {
            // The declared size of a large object says nothing useful about how
            // wide to print it. `-y 0` asks for no limit at all, which the
            // reference renders as the value's own length.
            if max_var == 0 {
                return ColumnLayout {
                    width: NATURAL_WIDTH,
                    align: Align::Left,
                };
            }
            width = max_var;
        }
        Cap::Fixed => {
            if max_fixed > 0 {
                width = width.min(max_fixed);
            }
        }
        Cap::Variant => width = VARIANT_WIDTH,
        Cap::None => {}
    }

    let align = if is_numeric_or_temporal(data_type) {
        Align::Right
    } else {
        Align::Left
    };

    ColumnLayout {
        width: width.max(column.column_name.chars().count()),
        align,
    }
}
