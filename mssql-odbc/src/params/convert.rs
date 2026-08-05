// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion from a bound application parameter buffer (`BoundParam`) to a
//! TDS RPC parameter (`RpcParameter`).
//!
//! The buffer is first decoded into a normalized [`CValue`] by
//! [`crate::params::cvalue`], then coerced to the TDS type implied by the
//! application's `ParameterType` (`SQL_*`). Data-at-execution and default
//! parameters are still rejected with `HYC00`; an invalid negative
//! `StrLen_or_Ind` is rejected with `HY090`.

use mssql_tds::datatypes::column_values::{SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use uuid::Uuid;

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DATE,
    SQL_C_DEFAULT, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID, SQL_C_LONG, SQL_C_NUMERIC, SQL_C_SBIGINT,
    SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_STINYINT, SQL_C_TIME, SQL_C_TIMESTAMP, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME,
    SQL_C_TYPE_TIMESTAMP, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR,
    SQL_CHAR, SQL_DATA_AT_EXEC, SQL_DECIMAL, SQL_DEFAULT_PARAM, SQL_DOUBLE, SQL_FLOAT, SQL_GUID,
    SQL_INTEGER, SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NTS,
    SQL_NULL_DATA, SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET,
    SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR,
    SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlDateStruct, SqlGuid, SqlLen, SqlNumericStruct,
    SqlSmallInt, SqlSsTime2Struct, SqlSsTimestampoffsetStruct, SqlTimeStruct, SqlTimestampStruct,
};
use crate::api::sqlstate::ERR_INVALID_STRING_OR_BUFFER_LENGTH;
use crate::params::BoundParam;
use crate::params::cvalue::{CValue, read_c_value};

/// Days from 0001-01-01 to 1970-01-01.
const DAYS_YEAR_ONE_TO_EPOCH: i64 = 719_162;
/// SQL Server `datetime2`/`time` default scale.
const DEFAULT_TIME_SCALE: u8 = 7;
/// Widest `decimal`/`numeric` precision SQL Server accepts.
const MAX_DECIMAL_PRECISION: u8 = 38;

/// Why a bound parameter could not be converted.
///
/// The "not yet implemented" variants post `HYC00` via [`message`]; a bad
/// indicator posts the canonical `HY090` diagnostic
/// (`ERR_INVALID_STRING_OR_BUFFER_LENGTH`).
///
/// [`message`]: ParamConvError::message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamConvError {
    /// The application's C type is not a recognized `SQL_C_*` value.
    UnsupportedCType(SqlSmallInt),
    /// The C value cannot be represented in the requested SQL type.
    UnsupportedConversion(SqlSmallInt, SqlSmallInt),
    /// The parameter uses data-at-execution (`SQLPutData`).
    DataAtExecUnsupported,
    /// The parameter requested its default value.
    DefaultParamUnsupported,
    /// `StrLen_or_Ind` is a negative value that is not a valid input length.
    InvalidLength(SqlLen),
}

impl ParamConvError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::UnsupportedCType(_) => "Parameter C type not yet implemented",
            Self::UnsupportedConversion(..) => "Restricted data type attribute violation",
            Self::DataAtExecUnsupported => "Data-at-execution parameters not yet implemented",
            Self::DefaultParamUnsupported => "Default parameters not yet implemented",
            Self::InvalidLength(_) => ERR_INVALID_STRING_OR_BUFFER_LENGTH.text,
        }
    }
}

/// Converts a bound parameter into a named RPC parameter, optionally taking the
/// value from data streamed via `SQLPutData` instead of the application buffer.
///
/// # Safety
/// See [`bound_param_to_value`].
pub(crate) unsafe fn bound_param_to_rpc_with_data(
    name: String,
    param: &BoundParam,
    dae: Option<Option<&[u8]>>,
) -> Result<RpcParameter, ParamConvError> {
    let value = match dae {
        Some(Some(bytes)) => {
            let c_type = effective_c_type(param.c_type, param.sql_type);
            let cvalue = unsafe {
                read_c_value(
                    c_type,
                    bytes.as_ptr(),
                    bytes.len() as SqlLen,
                    param.buffer_length,
                )
            }
            .ok_or(ParamConvError::UnsupportedCType(param.c_type))?;
            to_sql_type(&cvalue, param).ok_or(ParamConvError::UnsupportedConversion(
                param.c_type,
                param.sql_type,
            ))?
        }
        Some(None) => null_value(
            param.sql_type,
            effective_c_type(param.c_type, param.sql_type),
        ),
        None => unsafe { bound_param_to_value(param) }?,
    };
    Ok(RpcParameter::new(Some(name), StatusFlags::NONE, value))
}

/// Reports whether an indicator value requests data-at-execution.
pub(crate) fn is_data_at_exec(indicator: SqlLen) -> bool {
    indicator == SQL_DATA_AT_EXEC || indicator <= SQL_LEN_DATA_AT_EXEC_OFFSET
}

/// Byte stride between consecutive elements of a column-wise parameter array.
///
/// ODBC derives the stride from the C type for fixed-length buffers and from
/// `BufferLength` for character/binary buffers. Applications routinely pass
/// `BufferLength = 0` for fixed-length types, so the natural width wins there.
pub(crate) fn c_type_stride(
    c_type: SqlSmallInt,
    sql_type: SqlSmallInt,
    buffer_length: SqlLen,
) -> usize {
    let fixed = match effective_c_type(c_type, sql_type) {
        SQL_C_BIT | SQL_C_STINYINT | SQL_C_UTINYINT | SQL_C_TINYINT => 1,
        SQL_C_SSHORT | SQL_C_USHORT | SQL_C_SHORT => 2,
        SQL_C_SLONG | SQL_C_ULONG | SQL_C_LONG => 4,
        SQL_C_SBIGINT | SQL_C_UBIGINT => 8,
        SQL_C_FLOAT => 4,
        SQL_C_DOUBLE => 8,
        SQL_C_TYPE_DATE | SQL_C_DATE => size_of::<SqlDateStruct>(),
        SQL_C_TYPE_TIME | SQL_C_TIME => size_of::<SqlTimeStruct>(),
        SQL_C_TYPE_TIMESTAMP | SQL_C_TIMESTAMP => size_of::<SqlTimestampStruct>(),
        SQL_C_SS_TIME2 => size_of::<SqlSsTime2Struct>(),
        SQL_C_SS_TIMESTAMPOFFSET => size_of::<SqlSsTimestampoffsetStruct>(),
        SQL_C_GUID => size_of::<SqlGuid>(),
        SQL_C_NUMERIC => size_of::<SqlNumericStruct>(),
        _ => 0,
    };
    if fixed > 0 {
        fixed
    } else if buffer_length > 0 {
        buffer_length as usize
    } else {
        1
    }
}

/// Reads the application's value buffer and produces the corresponding
/// [`SqlType`].
///
/// # Safety
/// `param.parameter_value_ptr` and `param.strlen_or_ind_ptr` must satisfy the
/// ODBC binding contract: the value buffer is readable for the indicated
/// length and the indicator pointer, if non-null, points to one valid `SqlLen`.
pub(crate) unsafe fn bound_param_to_value(param: &BoundParam) -> Result<SqlType, ParamConvError> {
    let indicator = if param.strlen_or_ind_ptr.is_null() {
        None
    } else {
        Some(unsafe { *param.strlen_or_ind_ptr })
    };

    let c_type = effective_c_type(param.c_type, param.sql_type);

    if let Some(ind) = indicator {
        if ind == SQL_NULL_DATA {
            return Ok(null_value(param.sql_type, c_type));
        }
        if ind == SQL_DEFAULT_PARAM {
            return Err(ParamConvError::DefaultParamUnsupported);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamConvError::DataAtExecUnsupported);
        }
        // Any remaining negative indicator is invalid for an input parameter.
        if ind < 0 && ind != SQL_NTS as SqlLen {
            return Err(ParamConvError::InvalidLength(ind));
        }
    }

    // For string C types a null indicator pointer means "null-terminated".
    let len_spec = indicator.unwrap_or(SQL_NTS as SqlLen);

    let cvalue = unsafe {
        read_c_value(
            c_type,
            param.parameter_value_ptr as *const u8,
            len_spec,
            param.buffer_length,
        )
    }
    .ok_or(ParamConvError::UnsupportedCType(param.c_type))?;

    to_sql_type(&cvalue, param).ok_or(ParamConvError::UnsupportedConversion(
        param.c_type,
        param.sql_type,
    ))
}

/// Resolves `SQL_C_DEFAULT` to the C type implied by the SQL type, per the ODBC
/// default-mapping table.
fn effective_c_type(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> SqlSmallInt {
    if c_type != SQL_C_DEFAULT {
        return c_type;
    }
    match sql_type {
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => SQL_C_WCHAR,
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => SQL_C_BINARY,
        SQL_BIT => SQL_C_BIT,
        SQL_TINYINT => SQL_C_TINYINT,
        SQL_SMALLINT => SQL_C_SSHORT,
        SQL_INTEGER => SQL_C_SLONG,
        SQL_BIGINT => SQL_C_SBIGINT,
        SQL_REAL => SQL_C_FLOAT,
        SQL_FLOAT | SQL_DOUBLE => SQL_C_DOUBLE,
        SQL_GUID => SQL_C_GUID,
        SQL_TYPE_DATE => SQL_C_TYPE_DATE,
        SQL_TYPE_TIME => SQL_C_TYPE_TIME,
        SQL_SS_TIME2 => SQL_C_SS_TIME2,
        SQL_TYPE_TIMESTAMP => SQL_C_TYPE_TIMESTAMP,
        SQL_SS_TIMESTAMPOFFSET => SQL_C_SS_TIMESTAMPOFFSET,
        _ => SQL_C_CHAR,
    }
}

/// Typed NULL for the requested SQL type.
fn null_value(sql_type: SqlSmallInt, c_type: SqlSmallInt) -> SqlType {
    match sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR => SqlType::VarcharMax(None),
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => SqlType::NVarcharMax(None),
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => SqlType::VarBinaryMax(None),
        SQL_BIT => SqlType::Bit(None),
        SQL_TINYINT => SqlType::TinyInt(None),
        SQL_SMALLINT => SqlType::SmallInt(None),
        SQL_INTEGER => SqlType::Int(None),
        SQL_BIGINT => SqlType::BigInt(None),
        SQL_REAL => SqlType::Real(None),
        SQL_FLOAT | SQL_DOUBLE => SqlType::Float(None),
        SQL_DECIMAL | SQL_NUMERIC => SqlType::Decimal(None),
        SQL_GUID => SqlType::Uuid(None),
        SQL_TYPE_DATE => SqlType::Date(None),
        SQL_TYPE_TIME | SQL_SS_TIME2 => SqlType::Time(None),
        SQL_TYPE_TIMESTAMP => SqlType::DateTime2(None),
        SQL_SS_TIMESTAMPOFFSET => SqlType::DateTimeOffset(None),
        // No parameter type was supplied: fall back to the C type's family so
        // the server still receives a typed NULL.
        _ => match c_type {
            SQL_C_WCHAR => SqlType::NVarcharMax(None),
            SQL_C_BINARY => SqlType::VarBinaryMax(None),
            _ => SqlType::VarcharMax(None),
        },
    }
}

fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Coerces a decoded C value into the TDS type named by the application's
/// `ParameterType`. Returns `None` when the pairing is not convertible.
fn to_sql_type(cvalue: &CValue, param: &BoundParam) -> Option<SqlType> {
    let value = match param.sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR => {
            SqlType::VarcharMax(Some(SqlString::from_utf8_string(cvalue.to_text())))
        }
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => SqlType::NVarcharMax(Some(SqlString::new(
            utf16_bytes(&cvalue.to_text()),
            EncodingType::Utf16,
        ))),
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => {
            SqlType::VarBinaryMax(Some(to_bytes(cvalue)?))
        }
        SQL_BIT => SqlType::Bit(Some(to_i64(cvalue)? != 0)),
        SQL_TINYINT => SqlType::TinyInt(Some(u8::try_from(to_i64(cvalue)?).ok()?)),
        SQL_SMALLINT => SqlType::SmallInt(Some(i16::try_from(to_i64(cvalue)?).ok()?)),
        SQL_INTEGER => SqlType::Int(Some(i32::try_from(to_i64(cvalue)?).ok()?)),
        SQL_BIGINT => SqlType::BigInt(Some(to_i64(cvalue)?)),
        SQL_REAL => SqlType::Real(Some(to_f64(cvalue)? as f32)),
        SQL_FLOAT | SQL_DOUBLE => SqlType::Float(Some(to_f64(cvalue)?)),
        SQL_DECIMAL | SQL_NUMERIC => SqlType::Decimal(Some(to_decimal(cvalue, param)?)),
        SQL_GUID => SqlType::Uuid(Some(to_uuid(cvalue)?)),
        SQL_TYPE_DATE => SqlType::Date(Some(to_date(cvalue)?)),
        SQL_TYPE_TIME | SQL_SS_TIME2 => SqlType::Time(Some(to_time(cvalue, param)?)),
        SQL_TYPE_TIMESTAMP => SqlType::DateTime2(Some(to_datetime2(cvalue, param)?)),
        SQL_SS_TIMESTAMPOFFSET => SqlType::DateTimeOffset(Some(to_datetimeoffset(cvalue, param)?)),
        // Unknown parameter type: send the value in its natural family.
        _ => natural_sql_type(cvalue),
    };
    Some(value)
}

/// The TDS type a C value maps to when the application supplied no usable
/// `ParameterType`.
fn natural_sql_type(cvalue: &CValue) -> SqlType {
    match cvalue {
        CValue::Text { text, wide: false } => {
            SqlType::VarcharMax(Some(SqlString::from_utf8_string(text.clone())))
        }
        CValue::Bytes(b) => SqlType::VarBinaryMax(Some(b.clone())),
        CValue::Int(v) => SqlType::BigInt(Some(*v)),
        CValue::Bool(v) => SqlType::Bit(Some(*v)),
        CValue::Float(v) => SqlType::Float(Some(*v)),
        other => SqlType::NVarcharMax(Some(SqlString::new(
            utf16_bytes(&other.to_text()),
            EncodingType::Utf16,
        ))),
    }
}

fn to_bytes(cvalue: &CValue) -> Option<Vec<u8>> {
    match cvalue {
        CValue::Bytes(b) => Some(b.clone()),
        CValue::Text { text, wide } => Some(if *wide {
            utf16_bytes(text)
        } else {
            text.clone().into_bytes()
        }),
        CValue::Guid(_) => to_uuid(cvalue).map(|u| u.as_bytes().to_vec()),
        _ => None,
    }
}

fn to_i64(cvalue: &CValue) -> Option<i64> {
    match cvalue {
        CValue::Int(v) => Some(*v),
        CValue::UInt(v) => i64::try_from(*v).ok(),
        CValue::Bool(v) => Some(i64::from(*v)),
        CValue::Float(v) => Some(v.round() as i64),
        CValue::Numeric(_) => to_f64(cvalue).map(|f| f.round() as i64),
        CValue::Text { text, .. } => {
            let t = text.trim();
            t.parse::<i64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f.round() as i64))
        }
        _ => None,
    }
}

fn to_f64(cvalue: &CValue) -> Option<f64> {
    match cvalue {
        CValue::Float(v) => Some(*v),
        CValue::Int(v) => Some(*v as f64),
        CValue::UInt(v) => Some(*v as f64),
        CValue::Bool(v) => Some(f64::from(u8::from(*v))),
        CValue::Numeric(_) | CValue::Text { .. } => cvalue.to_text().trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn to_decimal(cvalue: &CValue, param: &BoundParam) -> Option<DecimalParts> {
    let text = match cvalue {
        CValue::Text { .. } | CValue::Numeric(_) | CValue::Int(_) | CValue::UInt(_) => {
            cvalue.to_text()
        }
        CValue::Bool(v) => u8::from(*v).to_string(),
        CValue::Float(v) => format!("{v}"),
        _ => return None,
    };
    let text = text.trim();
    let (precision, scale) = decimal_precision_scale(text, cvalue, param);
    DecimalParts::from_string(text, precision, scale).ok()
}

/// Picks the precision/scale to encode with, honouring the application's
/// `ColumnSize`/`DecimalDigits` when they are usable and otherwise deriving
/// them from the literal so no digits are lost.
fn decimal_precision_scale(text: &str, cvalue: &CValue, param: &BoundParam) -> (u8, u8) {
    if let CValue::Numeric(n) = cvalue
        && n.precision > 0
        && n.precision <= MAX_DECIMAL_PRECISION
        && n.scale >= 0
    {
        return (n.precision, n.scale as u8);
    }
    let literal_scale = text
        .split_once('.')
        .map_or(0usize, |(_, frac)| frac.trim_end_matches('0').len());
    let literal_digits = text.chars().filter(char::is_ascii_digit).count();

    let app_scale = u8::try_from(param.decimal_digits).unwrap_or(0);
    let app_precision = u8::try_from(param.column_size).unwrap_or(0);

    let scale = app_scale.max(u8::try_from(literal_scale).unwrap_or(0));
    let precision = app_precision
        .max(u8::try_from(literal_digits).unwrap_or(0))
        .max(scale.saturating_add(1))
        .min(MAX_DECIMAL_PRECISION);
    (precision, scale.min(precision))
}

fn to_uuid(cvalue: &CValue) -> Option<Uuid> {
    match cvalue {
        CValue::Guid(g) => Some(Uuid::from_fields(g.data1, g.data2, g.data3, &g.data4)),
        CValue::Text { text, .. } => Uuid::parse_str(text.trim()).ok(),
        CValue::Bytes(b) => <[u8; 16]>::try_from(b.as_slice())
            .ok()
            .map(Uuid::from_bytes),
        _ => None,
    }
}

/// Howard Hinnant's `days_from_civil`: days relative to 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if month > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn to_date(cvalue: &CValue) -> Option<SqlDate> {
    let (y, m, d) = date_parts(cvalue)?;
    let days = days_from_civil(y, m, d) + DAYS_YEAR_ONE_TO_EPOCH;
    SqlDate::create(u32::try_from(days).ok()?).ok()
}

fn date_parts(cvalue: &CValue) -> Option<(i64, u32, u32)> {
    match cvalue {
        CValue::Date(d) => Some((i64::from(d.year), u32::from(d.month), u32::from(d.day))),
        CValue::Timestamp(t) => Some((i64::from(t.year), u32::from(t.month), u32::from(t.day))),
        CValue::TimestampOffset(t) => {
            Some((i64::from(t.year), u32::from(t.month), u32::from(t.day)))
        }
        CValue::Text { text, .. } => parse_date_text(text.trim()),
        _ => None,
    }
}

fn parse_date_text(text: &str) -> Option<(i64, u32, u32)> {
    let head = text.split([' ', 'T']).next()?;
    let mut parts = head.split('-');
    let y = parts.next()?.parse::<i64>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    Some((y, m, d))
}

fn time_nanos(cvalue: &CValue) -> Option<u64> {
    let (h, mi, s, n) = match cvalue {
        CValue::Time {
            hour,
            minute,
            second,
            nanos,
        } => (
            u64::from(*hour),
            u64::from(*minute),
            u64::from(*second),
            u64::from(*nanos),
        ),
        CValue::Timestamp(t) => (
            u64::from(t.hour),
            u64::from(t.minute),
            u64::from(t.second),
            u64::from(t.fraction),
        ),
        CValue::TimestampOffset(t) => (
            u64::from(t.hour),
            u64::from(t.minute),
            u64::from(t.second),
            u64::from(t.fraction),
        ),
        CValue::Text { text, .. } => return parse_time_text(text.trim()),
        _ => return None,
    };
    Some(((h * 3600 + mi * 60 + s) * 1_000_000_000) + n)
}

fn parse_time_text(text: &str) -> Option<u64> {
    let tail = text.split([' ', 'T']).next_back()?;
    let tail = tail.split('+').next()?;
    let mut parts = tail.split(':');
    let h = parts.next()?.parse::<u64>().ok()?;
    let mi = parts.next()?.parse::<u64>().ok()?;
    let (sec_text, frac_text) = match parts.next() {
        Some(rest) => match rest.split_once('.') {
            Some((s, f)) => (s, Some(f)),
            None => (rest, None),
        },
        None => ("0", None),
    };
    let s = sec_text.parse::<u64>().ok()?;
    let nanos = match frac_text {
        Some(f) => {
            let digits: String = f.chars().filter(char::is_ascii_digit).take(9).collect();
            format!("{digits:0<9}").parse::<u64>().ok()?
        }
        None => 0,
    };
    Some(((h * 3600 + mi * 60 + s) * 1_000_000_000) + nanos)
}

/// Uses the application's `DecimalDigits` as the fractional-second scale,
/// falling back to `datetime2`'s default of 7.
fn time_scale(param: &BoundParam) -> u8 {
    match u8::try_from(param.decimal_digits) {
        Ok(s) if s <= 7 => s,
        _ => DEFAULT_TIME_SCALE,
    }
}

fn to_time(cvalue: &CValue, param: &BoundParam) -> Option<SqlTime> {
    Some(SqlTime {
        time_nanoseconds: time_nanos(cvalue)?,
        scale: time_scale(param),
    })
}

fn to_datetime2(cvalue: &CValue, param: &BoundParam) -> Option<SqlDateTime2> {
    let (y, m, d) = date_parts(cvalue)?;
    let days = days_from_civil(y, m, d) + DAYS_YEAR_ONE_TO_EPOCH;
    Some(SqlDateTime2 {
        days: u32::try_from(days).ok()?,
        time: SqlTime {
            time_nanoseconds: time_nanos(cvalue).unwrap_or(0),
            scale: time_scale(param),
        },
    })
}

fn to_datetimeoffset(cvalue: &CValue, param: &BoundParam) -> Option<SqlDateTimeOffset> {
    let offset = match cvalue {
        CValue::TimestampOffset(t) => t.timezone_hour * 60 + t.timezone_minute,
        _ => 0,
    };
    Some(SqlDateTimeOffset {
        datetime2: to_datetime2(cvalue, param)?,
        offset,
    })
}

/// Known ODBC SQL data type identifiers (plus SQL Server extensions) accepted
/// at bind time. Conversion support is checked separately.
pub(crate) fn is_valid_sql_type(sql_type: SqlSmallInt) -> bool {
    matches!(
        sql_type,
        SQL_CHAR
            | SQL_VARCHAR
            | SQL_LONGVARCHAR
            | SQL_WCHAR
            | SQL_WVARCHAR
            | SQL_WLONGVARCHAR
            | SQL_BINARY
            | SQL_VARBINARY
            | SQL_LONGVARBINARY
            | SQL_DECIMAL
            | SQL_NUMERIC
            | SQL_SMALLINT
            | SQL_INTEGER
            | SQL_BIGINT
            | SQL_TINYINT
            | SQL_BIT
            | SQL_REAL
            | SQL_FLOAT
            | SQL_DOUBLE
            | SQL_GUID
            | SQL_TYPE_DATE
            | SQL_TYPE_TIME
            | SQL_TYPE_TIMESTAMP
            | SQL_SS_TIME2
            | SQL_SS_TIMESTAMPOFFSET
    )
}

/// Known ODBC C type identifiers accepted at bind time.
pub(crate) fn is_valid_c_type(c_type: SqlSmallInt) -> bool {
    c_family(c_type).is_some() || c_type == SQL_C_DEFAULT
}

/// C-type family, used to decide which conversions `SQLBindParameter` accepts.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Family {
    Char,
    Binary,
    Number,
    Guid,
    Date,
    Time,
    Timestamp,
}

fn c_family(c_type: SqlSmallInt) -> Option<Family> {
    let family = match c_type {
        SQL_C_CHAR | SQL_C_WCHAR => Family::Char,
        SQL_C_BINARY => Family::Binary,
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_STINYINT | SQL_C_UTINYINT | SQL_C_SHORT
        | SQL_C_SSHORT | SQL_C_USHORT | SQL_C_LONG | SQL_C_SLONG | SQL_C_ULONG | SQL_C_SBIGINT
        | SQL_C_UBIGINT | SQL_C_FLOAT | SQL_C_DOUBLE | SQL_C_NUMERIC => Family::Number,
        SQL_C_GUID => Family::Guid,
        SQL_C_DATE | SQL_C_TYPE_DATE => Family::Date,
        SQL_C_TIME | SQL_C_TYPE_TIME | SQL_C_SS_TIME2 => Family::Time,
        SQL_C_TIMESTAMP | SQL_C_TYPE_TIMESTAMP | SQL_C_SS_TIMESTAMPOFFSET => Family::Timestamp,
        _ => return None,
    };
    Some(family)
}

fn sql_family(sql_type: SqlSmallInt) -> Option<Family> {
    let family = match sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR | SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => {
            Family::Char
        }
        SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => Family::Binary,
        SQL_DECIMAL | SQL_NUMERIC | SQL_SMALLINT | SQL_INTEGER | SQL_BIGINT | SQL_TINYINT
        | SQL_BIT | SQL_REAL | SQL_FLOAT | SQL_DOUBLE => Family::Number,
        SQL_GUID => Family::Guid,
        SQL_TYPE_DATE => Family::Date,
        SQL_TYPE_TIME | SQL_SS_TIME2 => Family::Time,
        SQL_TYPE_TIMESTAMP | SQL_SS_TIMESTAMPOFFSET => Family::Timestamp,
        _ => return None,
    };
    Some(family)
}

/// Whether the C type → SQL type conversion is supported. Mirrors the ODBC
/// conversion matrix: character C types convert to everything, and every C type
/// converts to a character SQL type. The other families only convert within
/// themselves, except that a date or time widens into a timestamp, a timestamp
/// narrows back to either, and GUIDs interchange with binary.
pub(crate) fn is_valid_conversion(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> bool {
    if c_type == SQL_C_DEFAULT {
        return is_valid_sql_type(sql_type);
    }
    let (Some(from), Some(to)) = (c_family(c_type), sql_family(sql_type)) else {
        return false;
    };
    if from == Family::Char || to == Family::Char || from == to {
        return true;
    }
    matches!(
        (from, to),
        (Family::Binary, Family::Guid)
            | (Family::Guid, Family::Binary)
            | (Family::Date, Family::Timestamp)
            | (Family::Time, Family::Timestamp)
            | (Family::Timestamp, Family::Date)
            | (Family::Timestamp, Family::Time)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_NO_TOTAL, SQL_PARAM_INPUT, SqlDateStruct, SqlGuid, SqlNumericStruct, SqlTimestampStruct,
    };
    use std::ffi::c_void;

    fn param(c_type: SqlSmallInt, ptr: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type,
            sql_type: 0,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: ptr,
            buffer_length: 0,
            strlen_or_ind_ptr: ind,
        }
    }

    fn typed(
        c_type: SqlSmallInt,
        sql_type: SqlSmallInt,
        ptr: *mut c_void,
        ind: *mut SqlLen,
    ) -> BoundParam {
        let mut p = param(c_type, ptr, ind);
        p.sql_type = sql_type;
        p
    }

    #[test]
    fn char_nts_becomes_varchar() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let p = typed(
            SQL_C_CHAR,
            SQL_VARCHAR,
            buf.as_mut_ptr() as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn wchar_explicit_length_becomes_nvarchar() {
        let mut buf: Vec<u16> = "hi".encode_utf16().collect();
        let mut ind: SqlLen = (buf.len() * 2) as SqlLen;
        let p = typed(
            SQL_C_WCHAR,
            SQL_WVARCHAR,
            buf.as_mut_ptr() as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::NVarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hi"),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn null_indicator_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = typed(SQL_C_CHAR, SQL_VARCHAR, std::ptr::null_mut(), &mut ind);
        assert!(matches!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::VarcharMax(None)
        ));
    }

    #[test]
    fn null_indicator_for_integer_yields_int_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = typed(SQL_C_SLONG, SQL_INTEGER, std::ptr::null_mut(), &mut ind);
        assert!(matches!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::Int(None)
        ));
    }

    #[test]
    fn tinyint_c_type_binds_to_tinyint() {
        let mut v: i8 = 42;
        let mut ind: SqlLen = 1;
        let p = typed(
            SQL_C_TINYINT,
            SQL_TINYINT,
            &mut v as *mut i8 as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::TinyInt(Some(42))
        );
    }

    #[test]
    fn bigint_c_type_binds_to_bigint() {
        let mut v: i64 = -9_000_000_000;
        let mut ind: SqlLen = 8;
        let p = typed(
            SQL_C_SBIGINT,
            SQL_BIGINT,
            &mut v as *mut i64 as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::BigInt(Some(-9_000_000_000))
        );
    }

    #[test]
    fn double_binds_to_float() {
        let mut v: f64 = 2.5;
        let mut ind: SqlLen = 8;
        let p = typed(
            SQL_C_DOUBLE,
            SQL_DOUBLE,
            &mut v as *mut f64 as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::Float(Some(2.5))
        );
    }

    #[test]
    fn bit_binds_to_bit() {
        let mut v: u8 = 1;
        let mut ind: SqlLen = 1;
        let p = typed(
            SQL_C_BIT,
            SQL_BIT,
            &mut v as *mut u8 as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::Bit(Some(true))
        );
    }

    #[test]
    fn binary_binds_to_varbinary() {
        let mut buf = [1u8, 2, 3];
        let mut ind: SqlLen = 3;
        let p = typed(
            SQL_C_BINARY,
            SQL_VARBINARY,
            buf.as_mut_ptr() as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::VarBinaryMax(Some(vec![1, 2, 3]))
        );
    }

    #[test]
    fn integer_widens_from_narrow_c_type() {
        let mut v: i16 = 7;
        let mut ind: SqlLen = 2;
        let p = typed(
            SQL_C_SSHORT,
            SQL_INTEGER,
            &mut v as *mut i16 as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::Int(Some(7))
        );
    }

    #[test]
    fn char_converts_to_integer() {
        let mut buf: Vec<u8> = b"123\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let p = typed(
            SQL_C_CHAR,
            SQL_INTEGER,
            buf.as_mut_ptr() as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::Int(Some(123))
        );
    }

    #[test]
    fn integer_converts_to_varchar() {
        let mut v: i32 = -17;
        let mut ind: SqlLen = 4;
        let p = typed(
            SQL_C_SLONG,
            SQL_VARCHAR,
            &mut v as *mut i32 as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "-17"),
            other => panic!("expected VarcharMax, got {other:?}"),
        }
    }

    #[test]
    fn numeric_struct_binds_to_decimal() {
        let mut n = SqlNumericStruct {
            precision: 6,
            scale: 2,
            sign: 1,
            val: [0; 16],
        };
        n.val[0] = 0xD2;
        n.val[1] = 0x04;
        let mut ind: SqlLen = 19;
        let p = typed(
            SQL_C_NUMERIC,
            SQL_DECIMAL,
            &mut n as *mut SqlNumericStruct as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::Decimal(Some(d)) => assert_eq!(d.to_decimal_string(), "12.34"),
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn char_binds_to_decimal() {
        let mut buf: Vec<u8> = b"-3.50\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = typed(
            SQL_C_CHAR,
            SQL_DECIMAL,
            buf.as_mut_ptr() as *mut c_void,
            &mut ind,
        );
        p.column_size = 10;
        p.decimal_digits = 2;
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::Decimal(Some(d)) => assert_eq!(d.to_decimal_string(), "-3.50"),
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_binds_to_datetime2() {
        let mut ts = SqlTimestampStruct {
            year: 2024,
            month: 3,
            day: 15,
            hour: 12,
            minute: 30,
            second: 45,
            fraction: 500_000_000,
        };
        let mut ind: SqlLen = 16;
        let mut p = typed(
            SQL_C_TYPE_TIMESTAMP,
            SQL_TYPE_TIMESTAMP,
            &mut ts as *mut SqlTimestampStruct as *mut c_void,
            &mut ind,
        );
        p.decimal_digits = 7;
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::DateTime2(Some(dt)) => {
                assert_eq!(dt.days, (days_from_civil(2024, 3, 15) + 719_162) as u32);
                assert_eq!(
                    dt.time.time_nanoseconds,
                    (12 * 3600 + 30 * 60 + 45) * 1_000_000_000 + 500_000_000
                );
            }
            other => panic!("expected DateTime2, got {other:?}"),
        }
    }

    #[test]
    fn date_struct_binds_to_date() {
        let mut d = SqlDateStruct {
            year: 1970,
            month: 1,
            day: 1,
        };
        let mut ind: SqlLen = 6;
        let p = typed(
            SQL_C_TYPE_DATE,
            SQL_TYPE_DATE,
            &mut d as *mut SqlDateStruct as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::Date(Some(date)) => assert_eq!(date.get_days(), 719_162),
            other => panic!("expected Date, got {other:?}"),
        }
    }

    #[test]
    fn guid_binds_to_uuid() {
        let mut g = SqlGuid {
            data1: 0x0123_4567,
            data2: 0x89AB,
            data3: 0xCDEF,
            data4: [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF],
        };
        let mut ind: SqlLen = 16;
        let p = typed(
            SQL_C_GUID,
            SQL_GUID,
            &mut g as *mut SqlGuid as *mut c_void,
            &mut ind,
        );
        match unsafe { bound_param_to_value(&p) }.unwrap() {
            SqlType::Uuid(Some(u)) => {
                assert_eq!(u.to_string(), "01234567-89ab-cdef-0123-456789abcdef");
            }
            other => panic!("expected Uuid, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: i32 = 7;
        let p = param(12345, &mut val as *mut i32 as *mut c_void, &mut ind);
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap_err(),
            ParamConvError::UnsupportedCType(12345)
        );
    }

    #[test]
    fn incompatible_conversion_is_rejected() {
        let mut d = SqlDateStruct {
            year: 2020,
            month: 1,
            day: 1,
        };
        let mut ind: SqlLen = 6;
        let p = typed(
            SQL_C_TYPE_DATE,
            SQL_INTEGER,
            &mut d as *mut SqlDateStruct as *mut c_void,
            &mut ind,
        );
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap_err(),
            ParamConvError::UnsupportedConversion(SQL_C_TYPE_DATE, SQL_INTEGER)
        );
    }

    #[test]
    fn data_at_exec_is_rejected() {
        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap_err(),
            ParamConvError::DataAtExecUnsupported
        );
    }

    #[test]
    fn invalid_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_NO_TOTAL;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap_err(),
            ParamConvError::InvalidLength(SQL_NO_TOTAL)
        );
    }

    #[test]
    fn conversion_matrix_allows_char_and_same_family() {
        assert!(is_valid_conversion(SQL_C_CHAR, SQL_VARCHAR));
        assert!(is_valid_conversion(SQL_C_WCHAR, SQL_WVARCHAR));
        // Character C types reach every SQL family, and vice versa.
        assert!(is_valid_conversion(SQL_C_CHAR, SQL_WVARCHAR));
        assert!(is_valid_conversion(SQL_C_WCHAR, SQL_VARCHAR));
        assert!(is_valid_conversion(SQL_C_CHAR, SQL_INTEGER));
        assert!(is_valid_conversion(SQL_C_SLONG, SQL_VARCHAR));
        // Same numeric family.
        assert!(is_valid_conversion(SQL_C_TINYINT, SQL_TINYINT));
        assert!(is_valid_conversion(SQL_C_DOUBLE, SQL_DECIMAL));
        // Date widens to timestamp; timestamp narrows to date.
        assert!(is_valid_conversion(SQL_C_TYPE_DATE, SQL_TYPE_TIMESTAMP));
        assert!(is_valid_conversion(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_DATE));
        // Cross-family pairings that ODBC does not define.
        assert!(!is_valid_conversion(SQL_C_TYPE_DATE, SQL_INTEGER));
        assert!(!is_valid_conversion(SQL_C_SLONG, SQL_TYPE_TIMESTAMP));
        assert!(!is_valid_conversion(SQL_C_SLONG, SQL_GUID));
    }

    #[test]
    fn default_param_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_DEFAULT_PARAM;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        assert_eq!(
            unsafe { bound_param_to_value(&p) }.unwrap_err(),
            ParamConvError::DefaultParamUnsupported
        );
    }

    #[test]
    fn null_indicator_wchar_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = typed(SQL_C_WCHAR, SQL_WVARCHAR, std::ptr::null_mut(), &mut ind);
        assert!(matches!(
            unsafe { bound_param_to_value(&p) }.unwrap(),
            SqlType::NVarcharMax(None)
        ));
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1, 1, 1) + DAYS_YEAR_ONE_TO_EPOCH, 0);
        assert_eq!(days_from_civil(1900, 1, 1) + 25_567, 0);
    }
}
