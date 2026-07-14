// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::str::FromStr;

use mssql_tds::{
    datatypes::{
        column_values::{
            ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
            SqlSmallDateTime, SqlSmallMoney, SqlTime,
        },
        decoder::DecimalParts,
        sql_string::{EncodingType, SqlString},
        sqldatatypes::TdsDataType,
        sqltypes::SqlType,
    },
    query::{metadata::ColumnMetadata, result::ReturnValue},
    token::tokens::SqlCollation,
};
use napi::{
    Error,
    bindgen_prelude::{Buffer, Null},
};
use uuid::Uuid;

use crate::{
    connection::RowDataType,
    datatypes::datetime::{NapiSqlDateTime, NapiSqlDateTime2, NapiSqlDateTimeOffset, NapiSqlTime},
};

#[napi(object)]
#[derive(Debug, Clone)]
pub struct CollationMetadata {
    pub info: u32,
    pub lcid_language_id: i32,
    pub col_flags: u8,
    pub sort_id: u8,
    pub is_utf8: bool,
}

impl From<SqlCollation> for CollationMetadata {
    fn from(collation: SqlCollation) -> Self {
        CollationMetadata {
            info: collation.info,
            lcid_language_id: collation.lcid_language_id(),
            col_flags: collation.col_flags,
            sort_id: collation.sort_id(),
            is_utf8: collation.utf8(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
#[napi]
pub enum SqlDataTypes {
    Void = 0x1F,
    Image = 0x22,
    Text = 0x23,
    Guid = 0x24,
    VarBinary = 0x25,
    VarChar = 0x27,
    Date,
    Time,
    DateTime2 = 0x2A,
    DateTimeOffset = 0x2B,
    Binary = 0x2D,
    Char = 0x2F,
    Int1 = 0x30,
    Bit = 0x32,
    Int2 = 0x34,
    Decimal = 0x37,
    Int4 = 0x38,
    SmallDateTime = 0x3A,
    Flt4 = 0x3B,
    Money = 0x3C,
    DateTime = 0x3D,
    Flt8 = 0x3E,
    Numeric = 0x3F,
    SsVariant = 0x62,
    NText = 0x63,
    FltN = 0x6D,
    Money4 = 0x7A,
    Int8 = 0x7F,
    BigVarBinary = 0xA5,
    BigVarChar = 0xA7,
    BigBinary = 0xAD,
    BigChar = 0xAF,
    NVarChar = 0xE7,
    NChar = 0xEF,
    Udt = 0xF0,
    Xml = 0xF1,
    Json = 0xF4,
}

impl TryFrom<TdsDataType> for SqlDataTypes {
    fn try_from(value: TdsDataType) -> Result<Self, Self::Error> {
        match value {
            TdsDataType::Void => Ok(SqlDataTypes::Void),
            TdsDataType::Image => Ok(SqlDataTypes::Image),
            TdsDataType::Text => Ok(SqlDataTypes::Text),
            TdsDataType::Guid => Ok(SqlDataTypes::Guid),
            TdsDataType::VarBinary => Ok(SqlDataTypes::VarBinary),
            TdsDataType::VarChar => Ok(SqlDataTypes::VarChar),
            TdsDataType::Binary => Ok(SqlDataTypes::Binary),
            TdsDataType::Char => Ok(SqlDataTypes::Char),
            TdsDataType::Int1 => Ok(SqlDataTypes::Int1),
            TdsDataType::Bit => Ok(SqlDataTypes::Bit),
            TdsDataType::Int2 => Ok(SqlDataTypes::Int2),
            TdsDataType::Decimal => Ok(SqlDataTypes::Decimal),
            TdsDataType::Int4 => Ok(SqlDataTypes::Int4),
            TdsDataType::Flt4 => Ok(SqlDataTypes::Flt4),
            TdsDataType::Money => Ok(SqlDataTypes::Money),
            TdsDataType::DateTime => Ok(SqlDataTypes::DateTime),
            TdsDataType::Flt8 => Ok(SqlDataTypes::Flt8),
            TdsDataType::Numeric => Ok(SqlDataTypes::Numeric),
            TdsDataType::SsVariant => Ok(SqlDataTypes::SsVariant),
            TdsDataType::NText => Ok(SqlDataTypes::NText),
            TdsDataType::Money4 => Ok(SqlDataTypes::Money4),
            TdsDataType::Int8 => Ok(SqlDataTypes::Int8),
            TdsDataType::BigVarBinary => Ok(SqlDataTypes::BigVarBinary),
            TdsDataType::BigVarChar => Ok(SqlDataTypes::BigVarChar),
            TdsDataType::BigBinary => Ok(SqlDataTypes::BigBinary),
            TdsDataType::BigChar => Ok(SqlDataTypes::BigChar),
            TdsDataType::NVarChar => Ok(SqlDataTypes::NVarChar),
            TdsDataType::NChar => Ok(SqlDataTypes::NChar),
            TdsDataType::Udt => Ok(SqlDataTypes::Udt),
            TdsDataType::Xml => Ok(SqlDataTypes::Xml),
            TdsDataType::Json => Ok(SqlDataTypes::Json),
            TdsDataType::DateTim4 => Ok(SqlDataTypes::SmallDateTime),
            TdsDataType::IntN => Err(()),
            TdsDataType::DateN => Err(()),
            TdsDataType::TimeN => Err(()),
            TdsDataType::DateTime2N => Err(()),
            TdsDataType::DateTimeOffsetN => Err(()),
            TdsDataType::DateTimeN => Err(()),
            TdsDataType::FltN => Err(()),
            TdsDataType::BitN => Err(()),
            TdsDataType::DecimalN => Err(()),
            TdsDataType::NumericN => Err(()),
            TdsDataType::MoneyN => Err(()),
            TdsDataType::None => Err(()),
            // TODO(Phase 3): Add Vector to SqlDataTypes enum and implement proper conversion
            TdsDataType::Vector => Err(()),
            // Table-valued parameters are not representable as a scalar JS data type.
            TdsDataType::SqlTable => Err(()),
        }
    }

    type Error = ();
}

#[napi(object)]
#[derive(Debug)]
pub struct NapiSqlMoney {
    pub lsb_part: i32, // LSB
    pub msb_part: i32, // MSB - Only populated for Money, 0 for SmallMoney
}

impl From<SqlMoney> for NapiSqlMoney {
    fn from(value: SqlMoney) -> Self {
        NapiSqlMoney {
            lsb_part: value.lsb_part,
            msb_part: value.msb_part,
        }
    }
}

impl From<NapiSqlMoney> for SqlMoney {
    fn from(napi_sql_money: NapiSqlMoney) -> Self {
        SqlMoney {
            lsb_part: napi_sql_money.lsb_part,
            msb_part: napi_sql_money.msb_part,
        }
    }
}

#[napi(object)]
#[derive(Debug)]
pub struct NapiDecimalParts {
    pub is_positive: bool,
    pub scale: u8,
    pub precision: u8,
    pub int_parts: Vec<i32>,
}

impl From<DecimalParts> for NapiDecimalParts {
    fn from(decimal_parts: DecimalParts) -> Self {
        NapiDecimalParts {
            is_positive: decimal_parts.is_positive,
            scale: decimal_parts.scale,
            precision: decimal_parts.precision,
            int_parts: decimal_parts.int_parts,
        }
    }
}

impl From<NapiDecimalParts> for DecimalParts {
    fn from(napi_decimal_parts: NapiDecimalParts) -> Self {
        DecimalParts {
            is_positive: napi_decimal_parts.is_positive,
            scale: napi_decimal_parts.scale,
            precision: napi_decimal_parts.precision,
            int_parts: napi_decimal_parts.int_parts,
        }
    }
}

#[napi(object)]
pub struct OutputParams {
    pub ordinal: u16,
    pub name: String,
    pub value: RowDataType,
    pub metadata: Metadata,
}

impl TryFrom<ReturnValue> for OutputParams {
    type Error = String;

    fn try_from(return_value: ReturnValue) -> Result<Self, Self::Error> {
        Ok(OutputParams {
            ordinal: return_value.param_ordinal,
            name: return_value.param_name,
            value: transform_col(return_value.value),
            metadata: Metadata::try_from(return_value.column_metadata.as_ref())?,
        })
    }
}

#[napi(object)]
pub struct Metadata {
    pub name: String,
    pub data_type: SqlDataTypes,
    pub encoding: Option<CollationMetadata>,
}

impl Clone for Metadata {
    fn clone(&self) -> Self {
        Metadata {
            name: self.name.clone(),
            data_type: self.data_type,
            encoding: self.encoding.clone(),
        }
    }
}

impl TryFrom<&ColumnMetadata> for Metadata {
    type Error = String;

    fn try_from(column_metadata: &ColumnMetadata) -> Result<Self, Self::Error> {
        let sql_type = {
            let tried_type = column_metadata.data_type.try_into();
            match tried_type {
                Ok(sql_type) => sql_type,
                Err(_) => match column_metadata.data_type {
                    TdsDataType::IntN => match column_metadata.type_info.length {
                        1 => SqlDataTypes::Int1,
                        2 => SqlDataTypes::Int2,
                        4 => SqlDataTypes::Int4,
                        8 => SqlDataTypes::Int8,
                        len => return Err(format!("Unsupported IntN length from server: {len}")),
                    },
                    TdsDataType::DateN => SqlDataTypes::Date,
                    TdsDataType::TimeN => SqlDataTypes::Time,
                    TdsDataType::DateTimeN => match column_metadata.type_info.length {
                        4 => SqlDataTypes::SmallDateTime,
                        8 => SqlDataTypes::DateTime,
                        _ => {
                            return Err(format!(
                                "Unsupported DateTimeN length from server: {}",
                                column_metadata.type_info.length
                            ));
                        }
                    },
                    TdsDataType::DateTime2N => SqlDataTypes::DateTime2,
                    TdsDataType::DateTimeOffsetN => SqlDataTypes::DateTimeOffset,
                    TdsDataType::BitN => SqlDataTypes::Bit,
                    TdsDataType::DecimalN => SqlDataTypes::Decimal,
                    TdsDataType::NumericN => SqlDataTypes::Numeric,
                    TdsDataType::MoneyN => match column_metadata.type_info.length {
                        4 => SqlDataTypes::Money4,
                        8 => SqlDataTypes::Money,
                        _ => {
                            return Err(format!(
                                "Unsupported MoneyN length from server: {}",
                                column_metadata.type_info.length
                            ));
                        }
                    },
                    TdsDataType::FltN => match column_metadata.type_info.length {
                        4 => SqlDataTypes::Flt4,
                        8 => SqlDataTypes::Flt8,
                        _ => {
                            return Err(format!(
                                "Unsupported FltN length from server: {}",
                                column_metadata.type_info.length
                            ));
                        }
                    },
                    TdsDataType::Flt4 => SqlDataTypes::Flt4,
                    TdsDataType::Flt8 => SqlDataTypes::Flt8,
                    TdsDataType::None => {
                        return Err("Received TdsDataType::None from server - invalid data type"
                            .to_string());
                    }
                    _ => {
                        return Err(format!(
                            "Unsupported SQL data type from server: {:?}",
                            column_metadata.data_type
                        ));
                    }
                },
            }
        };

        Ok(Metadata {
            name: column_metadata.column_name.clone(),
            data_type: sql_type,
            encoding: column_metadata.get_collation().map(Into::into),
        })
    }
}

#[napi(object)]
pub struct Parameter {
    pub name: String,
    pub data_type: SqlDataTypes,
    pub value: RowDataType,
    // Applicable to Varchar, NVarChar, VarBinary, NVarBinary, and similar types
    pub length: Option<u32>,
    pub direction: ParameterDirection,
}

#[napi]
#[derive(Debug)]
pub enum ParameterDirection {
    Input,
    Output,
}

/// Values are converted from Parameter to SqlType to be sent over the wire.
impl TryFrom<Parameter> for SqlType {
    fn try_from(param: Parameter) -> Result<SqlType, Error> {
        match param.value {
            RowDataType::A(_) => Err(Error::from_reason(format!(
                "Napi F64 is deprecated. Use Buffers for real / float, received datatype {:?}",
                param.data_type
            ))),
            RowDataType::B(v) => {
                if !matches!(
                    param.data_type,
                    SqlDataTypes::Int1
                        | SqlDataTypes::Int2
                        | SqlDataTypes::Int4
                        | SqlDataTypes::Int8
                        | SqlDataTypes::Date
                        | SqlDataTypes::Money4
                ) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for number: {:?}. DataType Value {:?}",
                        param.data_type, v
                    )));
                }
                match param.data_type {
                    SqlDataTypes::Int1 => {
                        if v < u8::MIN as i32 || v > u8::MAX as i32 {
                            return Err(Error::from_reason(format!(
                                "Value {v} out of range for Int1"
                            )));
                        }
                        Ok(SqlType::TinyInt(Some(v as u8)))
                    }
                    SqlDataTypes::Int2 => {
                        if v < i16::MIN as i32 || v > i16::MAX as i32 {
                            return Err(Error::from_reason(format!(
                                "Value {v} out of range for Int2"
                            )));
                        }
                        Ok(SqlType::SmallInt(Some(v as i16)))
                    }
                    SqlDataTypes::Int4 => Ok(SqlType::Int(Some(v))),
                    SqlDataTypes::Int8 => Ok(SqlType::BigInt(Some(v as i64))),
                    SqlDataTypes::Date => {
                        // Conversion to u32 is safe
                        let sql_date = SqlDate::create(v as u32).map_err(|e| {
                            Error::from_reason(format!(
                                "Failed to create SqlDate from u32 value: {e}"
                            ))
                        })?;
                        Ok(SqlType::Date(Some(sql_date)))
                    }
                    SqlDataTypes::Money4 => {
                        Ok(SqlType::SmallMoney(Some(SqlSmallMoney { int_val: v })))
                    }
                    _ => Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::A: {:?}. Only Int1, Int2, Int4, Int8 are allowed.",
                        param.data_type
                    ))),
                }
            }

            RowDataType::C(bigint) => {
                let (i64val, is_lossless) = bigint.get_i64();
                if !is_lossless {
                    return Err(Error::from_reason(format!(
                        "BigInt value {i64val} is not lossless. A value out of range of i64 was provided."
                    )));
                }
                if !matches!(param.data_type, SqlDataTypes::Int8) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::B: {:?}. Only Int8 is allowed.",
                        param.data_type
                    )));
                }
                Ok(SqlType::BigInt(Some(i64val)))
            }
            RowDataType::D(bit_val) => {
                if !matches!(param.data_type, SqlDataTypes::Bit) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::C: {:?}. Only Bit is allowed.",
                        param.data_type
                    )));
                }
                Ok(SqlType::Bit(Some(bit_val)))
            }
            RowDataType::E(buffer) => {
                match param.data_type {
                    SqlDataTypes::VarChar => {
                        let bytes: Vec<u8> = buffer.to_vec();
                        match param.length {
                            Some(len) => Ok(SqlType::Varchar(
                                Some(SqlString::new(bytes, EncodingType::DelayedSet)),
                                len.try_into().unwrap(),
                            )),
                            None => Ok(SqlType::VarcharMax(Some(SqlString::new(
                                bytes,
                                EncodingType::DelayedSet,
                            )))),
                        }
                    }
                    SqlDataTypes::NVarChar => {
                        let bytes: Vec<u8> = buffer.to_vec();
                        match param.length {
                            Some(len) => Ok(SqlType::NVarchar(
                                Some(SqlString::new(bytes, EncodingType::DelayedSet)),
                                len.try_into().unwrap(),
                            )),
                            None => Ok(SqlType::NVarcharMax(Some(SqlString::new(
                                bytes,
                                EncodingType::DelayedSet,
                            )))),
                        }
                    }
                    SqlDataTypes::BigVarBinary => {
                        let bytes: Vec<u8> = buffer.to_vec();
                        match param.length {
                            Some(len) => {
                                Ok(SqlType::VarBinary(Some(bytes), len.try_into().unwrap()))
                            }
                            None => Ok(SqlType::VarBinaryMax(Some(bytes))),
                        }
                    }
                    SqlDataTypes::Flt4 => {
                        let real_value = f32::from_le_bytes(buffer.to_vec().as_slice().try_into().map_err(|_| {
                        Error::from_reason(format!(
                            "Failed to convert Vec<u8> to [u8; 4] for Flt4 (f32), got {} bytes",
                            buffer.len()
                        ))
                    })?);
                        Ok(SqlType::Real(Some(real_value)))
                    }
                    SqlDataTypes::Flt8 => {
                        let float_value = f64::from_le_bytes(buffer.to_vec().as_slice().try_into().map_err(|_| {
                        Error::from_reason(format!(
                            "Failed to convert Vec<u8> to [u8; 8] for Flt8 (f64), got {} bytes",
                            buffer.len()
                        ))
                    })?);
                        Ok(SqlType::Float(Some(float_value)))
                    }
                    _ => Err(Error::from_reason(format!(
                        "Buffer conversion not implemented for data_type: {:?}",
                        param.data_type
                    ))),
                }
            }
            RowDataType::F(_) => get_null_sql_type(&param),
            RowDataType::G(napi_sql_date_time) => match param.data_type {
                SqlDataTypes::DateTime => {
                    let sql_datetime: SqlDateTime = napi_sql_date_time.into();
                    Ok(SqlType::DateTime(Some(sql_datetime)))
                }
                SqlDataTypes::SmallDateTime => {
                    let sql_small_datetime: SqlSmallDateTime = napi_sql_date_time.try_into()?;
                    Ok(SqlType::SmallDateTime(Some(sql_small_datetime)))
                }
                _ => Err(Error::from_reason(format!(
                    "Invalid data_type for RowDataType::F: {:?}. Only DateTime is allowed.",
                    param.data_type
                ))),
            },
            RowDataType::H(v) => {
                // Check if the data_type is date
                if matches!(param.data_type, SqlDataTypes::Date) {
                    // Convert the u32 value to SqlDate
                    let sql_date = SqlDate::create(v).map_err(|e| {
                        Error::from_reason(format!("Failed to create SqlDate from u32 value: {e}"))
                    })?;
                    return Ok(SqlType::Date(Some(sql_date)));
                }
                Err(Error::from_reason(format!(
                    "u32 value {} conversion for data_type {:?} not supported.",
                    v, param.data_type
                )))
            }
            RowDataType::I(napi_sql_time) => {
                if !matches!(param.data_type, SqlDataTypes::Time) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::H: {:?}. Only Time is allowed.",
                        param.data_type
                    )));
                }
                let sql_time = SqlTime::try_from(napi_sql_time)?;
                Ok(SqlType::Time(Some(sql_time)))
            }
            RowDataType::J(napi_sql_datetime2) => match param.data_type {
                SqlDataTypes::DateTime2 => {
                    let sql_datetime2: SqlDateTime2 = napi_sql_datetime2.try_into()?;
                    Ok(SqlType::DateTime2(Some(sql_datetime2)))
                }
                _ => Err(Error::from_reason(format!(
                    "Invalid data_type for RowDataType::J: {:?}. Only DateTime2 is allowed.",
                    param.data_type
                ))),
            },
            RowDataType::K(napi_date_time_offset) => match param.data_type {
                SqlDataTypes::DateTimeOffset => {
                    let sql_datetime_offset: SqlDateTimeOffset =
                        napi_date_time_offset.try_into()?;
                    Ok(SqlType::DateTimeOffset(Some(sql_datetime_offset)))
                }
                _ => Err(Error::from_reason(format!(
                    "Invalid data_type for RowDataType::K: {:?}. Only DateTimeOffset is allowed.",
                    param.data_type
                ))),
            },
            RowDataType::L(napi_sql_money) => {
                if !matches!(param.data_type, SqlDataTypes::Money | SqlDataTypes::Money4) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::L: {:?}. Only Money and Money4 are allowed.",
                        param.data_type
                    )));
                }
                Ok(SqlType::Money(Some(napi_sql_money.into())))
            }
            RowDataType::M(decimal_parts) => {
                if !matches!(
                    param.data_type,
                    SqlDataTypes::Decimal | SqlDataTypes::Numeric
                ) {
                    return Err(Error::from_reason(format!(
                        "Invalid data_type for RowDataType::L: {:?}. Only Decimal and Numeric are allowed.",
                        param.data_type
                    )));
                }
                Ok(SqlType::Decimal(Some(decimal_parts.into())))
            }
            RowDataType::N(uuid) => match param.data_type {
                SqlDataTypes::Guid => {
                    let uuid = Uuid::from_str(&uuid).map_err(|uuid| {
                        Error::from_reason(format!("Failed to convert String to Uuid: {uuid}"))
                    })?;
                    Ok(SqlType::Uuid(Some(uuid)))
                }
                _ => Err(Error::from_reason(format!(
                    "Invalid data_type for RowDataType::N: {:?}. Only Guid is allowed.",
                    param.data_type
                ))),
            },
        }
    }

    type Error = napi::Error;
}

fn get_null_sql_type(param: &Parameter) -> Result<SqlType, Error> {
    match param.data_type {
        SqlDataTypes::Int1 | SqlDataTypes::Int2 | SqlDataTypes::Int4 | SqlDataTypes::Int8 => {
            Ok(SqlType::Int(None))
        }
        SqlDataTypes::Bit => Ok(SqlType::Bit(None)),
        SqlDataTypes::Decimal | SqlDataTypes::Numeric => Ok(SqlType::Decimal(None)),
        SqlDataTypes::Money | SqlDataTypes::Money4 => Ok(SqlType::Money(None)),
        SqlDataTypes::Void => Err(Error::from_reason("Void type not implemented")),
        SqlDataTypes::Image => Err(Error::from_reason("Image conversion not implemented")),
        SqlDataTypes::Text => Ok(SqlType::Text(None)),
        SqlDataTypes::Guid => Ok(SqlType::Uuid(None)),
        SqlDataTypes::VarBinary => Ok(SqlType::VarBinary(None, 0)),
        SqlDataTypes::VarChar => Ok(SqlType::VarcharMax(None)),
        SqlDataTypes::Date => Ok(SqlType::Date(None)),
        SqlDataTypes::Time => Ok(SqlType::Time(None)),
        SqlDataTypes::DateTime2 => Ok(SqlType::DateTime2(None)),
        SqlDataTypes::DateTimeOffset => Ok(SqlType::DateTimeOffset(None)),
        SqlDataTypes::Binary => Ok(SqlType::Binary(None, 0)),
        SqlDataTypes::Char => Ok(SqlType::Char(None, 0)),
        SqlDataTypes::SmallDateTime => Ok(SqlType::SmallDateTime(None)),
        SqlDataTypes::Flt4 => Ok(SqlType::Real(None)),
        SqlDataTypes::DateTime => Ok(SqlType::DateTime(None)),
        SqlDataTypes::Flt8 => Ok(SqlType::Float(None)),
        SqlDataTypes::SsVariant => Err(Error::from_reason("SSVariant conversion not implemented")),
        SqlDataTypes::NText => Ok(SqlType::NText(None)),
        SqlDataTypes::FltN => Ok(SqlType::Float(None)),
        SqlDataTypes::BigVarBinary => Ok(SqlType::VarBinaryMax(None)),
        SqlDataTypes::BigVarChar => Ok(SqlType::Varchar(None, 0)),
        SqlDataTypes::BigBinary => Ok(SqlType::Binary(None, 0)),
        SqlDataTypes::BigChar => Ok(SqlType::Char(None, 0)),
        SqlDataTypes::NVarChar => Ok(SqlType::NVarcharMax(None)),
        SqlDataTypes::NChar => Ok(SqlType::NChar(None, 0)),
        SqlDataTypes::Udt => Err(Error::from_reason("Udt conversion not implemented")),
        SqlDataTypes::Xml => Ok(SqlType::Xml(None)),
        SqlDataTypes::Json => Ok(SqlType::Json(None)),
    }
}

pub fn transform_col(col: ColumnValues) -> RowDataType {
    match col {
        ColumnValues::Int(v) => RowDataType::B(v),
        ColumnValues::Uuid(uuid) => RowDataType::N(uuid.to_string()),
        ColumnValues::Bit(v) => RowDataType::D(v),
        ColumnValues::BigInt(v) => RowDataType::C(v.into()),
        ColumnValues::TinyInt(v) => RowDataType::H(v.into()),
        ColumnValues::SmallInt(v) => RowDataType::B(v.into()),
        ColumnValues::Real(real) => RowDataType::E(Buffer::from(real.to_le_bytes().as_slice())),
        ColumnValues::Float(float) => RowDataType::E(Buffer::from(float.to_le_bytes().as_slice())),
        ColumnValues::Decimal(decimal_parts) => RowDataType::M(decimal_parts.into()),
        ColumnValues::Numeric(decimal_parts) => RowDataType::M(decimal_parts.into()),
        ColumnValues::String(sql_string) => RowDataType::E(Buffer::from(sql_string.bytes)),
        ColumnValues::DateTime(sql_date_time) => RowDataType::G(NapiSqlDateTime {
            days: sql_date_time.days,
            time: sql_date_time.time,
        }),
        ColumnValues::Date(sql_date) => RowDataType::H(sql_date.get_days()),
        ColumnValues::Time(_time) => RowDataType::I(NapiSqlTime::from(_time)),
        ColumnValues::DateTime2(_date_time2) => RowDataType::J(NapiSqlDateTime2::from(_date_time2)),
        ColumnValues::DateTimeOffset(date_time_offset) => {
            RowDataType::K(NapiSqlDateTimeOffset::from(date_time_offset))
        }
        ColumnValues::SmallDateTime(sql_small_date_time) => RowDataType::G(NapiSqlDateTime {
            days: sql_small_date_time.days.into(),
            time: sql_small_date_time.time.into(),
        }),
        ColumnValues::SmallMoney(sql_small_money) => RowDataType::B(sql_small_money.int_val),
        ColumnValues::Money(sql_money) => RowDataType::L(sql_money.into()),
        ColumnValues::Bytes(items) => RowDataType::E(Buffer::from(items)),
        ColumnValues::Xml(sql_xml) => RowDataType::E(Buffer::from(sql_xml.bytes)),
        ColumnValues::Null => RowDataType::F(Null),
        ColumnValues::Json(sql_json) => RowDataType::E(Buffer::from(sql_json.bytes)),
        // TODO(Phase 3): Implement Vector to JavaScript conversion
        ColumnValues::Vector(_) => {
            // Temporary placeholder - return null for now
            RowDataType::F(Null)
        }
    }
}
